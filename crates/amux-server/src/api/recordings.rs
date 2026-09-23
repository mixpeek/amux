//! Recordings (AMUX-4624). The dashboard's Record tab keeps audio on the
//! device until it is online, then uploads it here. This module files it in
//! the folder the person chose, and the `recordings-transcribe` job turns it
//! into text with a local model.
//!
//! THE FOLDER IS THE STORE. One recording is up to four files sharing a base
//! name, `<YYYY-MM-DD_HH-MM-SS>_<id>`, in the recording's own local time:
//!
//! - `<base>.<ext>`: the audio. When ffmpeg is present it is remuxed (no
//!   re-encode) with the recording datetime in its container metadata
//!   (`creation_time`, `date`, `title`, ISO 6709 `location`). In every case
//!   the file's times are set to the moment recording started, the way a
//!   camera stamps a photo.
//! - `<base>.json`: the sidecar. Everything the device knew (start, end,
//!   timezone, location, device, the uploaded bytes' sha256) plus the
//!   transcript state. It is what makes a repeated upload idempotent.
//! - `<base>.md` and `<base>.transcript.json`: the transcript, front matter
//!   first, carrying the same datetime.
//!
//! There is no table. A person can move, back up or sync the folder with any
//! tool, and amux holds no second copy of the truth to disagree with it.
//!
//! Folder: `AMUX_RECORDINGS_DIR` (server.env) wins, then the `recordings_dir`
//! pref the tab writes, then `~/Recordings`.

use super::fs::{expanduser, is_dangerous_write, is_path_allowed};
use super::AppState;
use crate::db::{PendingEvent, WriteOutcome};
use crate::runtime_jobs::registry;
use amux_core::revision::{EntityType, MutationKind};
use axum::extract::{Path as AxPath, Query, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use sha2::Digest;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};

const ENV_DIR: &str = "AMUX_RECORDINGS_DIR";
const ENV_MODEL: &str = "AMUX_RECORDINGS_WHISPER_MODEL";
const PREF_DIR: &str = "recordings_dir";
const DEFAULT_DIR: &str = "~/Recordings";
/// ggml-base.en transcribed 11 s of speech in 4.6 s on this Intel host;
/// small.en took 13.6 s, slower than real time. Override with the env var.
const DEFAULT_MODEL: &str = "base.en";
const SIDECAR_KIND: &str = "amux-recording";
/// A day of 32 kbps speech is about 350 MB. This bounds a mistake, not a use.
const MAX_UPLOAD_BYTES: usize = 1 << 30;
const MAX_TRANSCRIBE_ATTEMPTS: i64 = 3;
/// A `running` transcript older than this belongs to a process that died.
const RUNNING_STALE_MS: i64 = 30 * 60 * 1000;
/// One job tick stops STARTING new transcriptions after this long, so a
/// backlog drains across ticks instead of pinning one tick for hours.
const TICK_BUDGET: Duration = Duration::from_secs(10 * 60);

/// Serializes writes into the folder: an upload's check-then-write and the
/// job's read-modify-write of a sidecar. Never held across a transcription.
static FOLDER_LOCK: Mutex<()> = Mutex::new(());

fn folder_lock() -> std::sync::MutexGuard<'static, ()> {
    FOLDER_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/config", get(get_config).post(post_config))
        // Body limit off: the handler reads up to MAX_UPLOAD_BYTES itself and
        // must be the one to answer, like /api/dictate.
        .route(
            "/upload",
            post(upload).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/{id}", get(get_one))
        .route("/{id}/transcribe", post(retranscribe))
}

// ---- folder ----------------------------------------------------------------

fn env_dir() -> Option<String> {
    std::env::var(ENV_DIR)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn pref_dir(conn: &rusqlite::Connection) -> Option<String> {
    let raw: String = conn
        .query_row("SELECT value FROM prefs WHERE key=?1", [PREF_DIR], |r| {
            r.get(0)
        })
        .ok()?;
    // Tolerate a JSON-quoted value, which is how the generic prefs API stores strings.
    let v = serde_json::from_str::<String>(&raw).unwrap_or(raw);
    Some(v.trim().to_string()).filter(|s| !s.is_empty())
}

/// The folder and which setting chose it: `env`, `pref` or `default`.
pub(crate) fn resolve_dir(conn: &rusqlite::Connection) -> (PathBuf, &'static str) {
    if let Some(d) = env_dir() {
        return (expanduser(&d), "env");
    }
    if let Some(d) = pref_dir(conn) {
        return (expanduser(&d), "pref");
    }
    (expanduser(DEFAULT_DIR), "default")
}

async fn current_dir(state: &AppState) -> anyhow::Result<(PathBuf, &'static str)> {
    let store = state.store.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<(PathBuf, &'static str)> {
        let conn = store.read()?;
        Ok(resolve_dir(&conn))
    })
    .await?
}

/// Where recordings may be written. The Files API's own path policy, so the
/// tab cannot aim audio at `~/.ssh` or a launch-agent folder.
pub(crate) fn validate_dir(raw: &str) -> Result<PathBuf, (StatusCode, String)> {
    let t = raw.trim();
    if t.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "dir required".into()));
    }
    let p = expanduser(t);
    if !p.is_absolute() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("use an absolute path or one starting with ~/: {t}"),
        ));
    }
    if !is_path_allowed(&p) || is_dangerous_write(&p) {
        return Err((
            StatusCode::FORBIDDEN,
            format!("amux will not write recordings into {t}"),
        ));
    }
    if p.exists() && !p.is_dir() {
        return Err((StatusCode::BAD_REQUEST, format!("not a folder: {t}")));
    }
    Ok(p)
}

// ---- what the device sent ----------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Location {
    pub lat: f64,
    pub lon: f64,
    pub accuracy_m: Option<f64>,
}

#[derive(Clone, Debug)]
pub(crate) struct RecordingMeta {
    pub id: String,
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    pub dur_ms: i64,
    /// Minutes EAST of UTC on the device when recording started (EDT is -240).
    pub tz_offset_min: i32,
    pub mime: String,
    pub ext: &'static str,
    pub location: Option<Location>,
    pub device: String,
    pub recovered: bool,
}

#[derive(serde::Deserialize, Default)]
pub struct UploadQuery {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    ended_at: Option<String>,
    #[serde(default)]
    dur_ms: Option<String>,
    #[serde(default)]
    tz_offset_min: Option<String>,
    #[serde(default)]
    mime: Option<String>,
    #[serde(default)]
    lat: Option<String>,
    #[serde(default)]
    lon: Option<String>,
    #[serde(default)]
    accuracy_m: Option<String>,
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    recovered: Option<String>,
}

fn ext_for_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => Some("m4a"),
        "audio/webm" | "video/webm" => Some("webm"),
        "audio/ogg" | "application/ogg" => Some("ogg"),
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        "audio/wav" | "audio/x-wav" | "audio/wave" => Some("wav"),
        "audio/aac" => Some("aac"),
        _ => None,
    }
}

/// Containers ffmpeg can write these tags into. Raw AAC and WAV cannot carry
/// them, so for those the datetime lives on the file and in the sidecar.
fn embeddable(ext: &str) -> bool {
    matches!(ext, "m4a" | "webm" | "ogg" | "mp3")
}

fn valid_id(id: &str) -> bool {
    (6..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn num<T: std::str::FromStr>(v: &Option<String>) -> Option<T> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
}

pub(crate) fn parse_meta(
    q: &UploadQuery,
    content_type: &str,
    now: i64,
) -> Result<RecordingMeta, (StatusCode, String)> {
    let bad = |m: String| (StatusCode::BAD_REQUEST, m);
    let id = q.id.as_deref().unwrap_or("").trim().to_string();
    if !valid_id(&id) {
        return Err(bad("id must be 6 to 64 letters, digits, - or _".into()));
    }
    let started: i64 =
        num(&q.started_at).ok_or_else(|| bad("started_at (epoch ms) required".into()))?;
    // 2000-01-01 up to a day ahead: a clock further off would misfile the recording.
    if !(946_684_800_000..=now + 86_400_000).contains(&started) {
        return Err(bad(format!(
            "started_at {started} is not a plausible recording time"
        )));
    }
    let dur_q: Option<i64> = num::<i64>(&q.dur_ms).filter(|d| *d >= 0);
    let ended = num::<i64>(&q.ended_at)
        .filter(|e| *e >= started)
        .or(dur_q.map(|d| started + d))
        .unwrap_or(started);
    let dur = dur_q.unwrap_or(ended - started);
    let tz: i32 = num(&q.tz_offset_min).unwrap_or(0);
    if !(-840..=840).contains(&tz) {
        return Err(bad(format!("tz_offset_min {tz} is out of range")));
    }
    let mime_raw = q
        .mime
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(content_type);
    let mime = mime_raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let ext = ext_for_mime(&mime).ok_or_else(|| {
        (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("unsupported audio type {mime:?}"),
        )
    })?;
    let location = match (num::<f64>(&q.lat), num::<f64>(&q.lon)) {
        (Some(lat), Some(lon))
            if lat.is_finite() && lon.is_finite() && lat.abs() <= 90.0 && lon.abs() <= 180.0 =>
        {
            Some(Location {
                lat,
                lon,
                accuracy_m: num::<f64>(&q.accuracy_m).filter(|a| a.is_finite() && *a >= 0.0),
            })
        }
        _ => None,
    };
    let device: String = q
        .device
        .as_deref()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    let recovered = matches!(
        q.recovered.as_deref().map(str::trim),
        Some("1" | "true" | "yes")
    );
    Ok(RecordingMeta {
        id,
        started_at_ms: started,
        ended_at_ms: ended,
        dur_ms: dur,
        tz_offset_min: tz,
        mime,
        ext,
        location,
        device,
        recovered,
    })
}

// ---- time and names ----------------------------------------------------------

fn utc_of(ms: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).unwrap_or_default()
}

fn local_of(ms: i64, tz_offset_min: i32) -> chrono::DateTime<chrono::FixedOffset> {
    let off = chrono::FixedOffset::east_opt(tz_offset_min * 60)
        .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).expect("zero offset"));
    utc_of(ms).with_timezone(&off)
}

pub(crate) fn base_name(meta: &RecordingMeta) -> String {
    format!(
        "{}_{}",
        local_of(meta.started_at_ms, meta.tz_offset_min).format("%Y-%m-%d_%H-%M-%S"),
        meta.id
    )
}

/// ISO 6709, the form QuickTime and ffmpeg's mp4 `location` tag use.
pub(crate) fn iso6709(lat: f64, lon: f64) -> String {
    format!("{lat:+08.4}{lon:+09.4}/")
}

/// Stamp a file with the moment recording started: modified and accessed
/// time everywhere, creation time on macOS, so Finder and `ls -lt` order a
/// recording by when it was made rather than when it synced.
pub(crate) fn stamp_times(path: &Path, ms: i64) -> std::io::Result<()> {
    let t = UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64);
    let f = std::fs::OpenOptions::new().write(true).open(path)?;
    #[allow(unused_mut)]
    let mut times = std::fs::FileTimes::new().set_modified(t).set_accessed(t);
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::FileTimesExt;
        times = times.set_created(t);
    }
    f.set_times(times)
}

/// The ffmpeg remux that embeds the recording's datetime (and location, when
/// the device sent one) without re-encoding the audio.
pub(crate) fn metadata_args(meta: &RecordingMeta, input: &Path, output: &Path) -> Vec<String> {
    let local = local_of(meta.started_at_ms, meta.tz_offset_min);
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y", "-i"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    a.push(input.display().to_string());
    a.extend(
        ["-map", "0:a", "-c", "copy", "-map_metadata", "-1"]
            .iter()
            .map(|s| s.to_string()),
    );
    let mut tags = vec![
        (
            "creation_time",
            utc_of(meta.started_at_ms)
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string(),
        ),
        ("date", local.format("%Y-%m-%d").to_string()),
        (
            "title",
            format!("Recording {}", local.format("%Y-%m-%d %H:%M")),
        ),
        (
            "comment",
            format!("amux recording {} started {}", meta.id, local.to_rfc3339()),
        ),
    ];
    if let Some(l) = &meta.location {
        tags.push(("location", iso6709(l.lat, l.lon)));
    }
    for (k, v) in tags {
        a.push("-metadata".into());
        a.push(format!("{k}={v}"));
    }
    if meta.ext == "m4a" {
        a.push("-movflags".into());
        a.push("+faststart+use_metadata_tags".into());
    }
    a.push(output.display().to_string());
    a
}

/// Run a tool to completion under a deadline. The error names the tool and
/// carries the tail of its stderr, which goes to a file so a chatty tool can
/// never fill a pipe and hang.
fn run_bounded(bin: &Path, args: &[String], timeout: Duration) -> Result<(), String> {
    use std::io::{Read, Seek};
    let name = bin
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut errf = tempfile::tempfile().map_err(|e| format!("{name}: {e}"))?;
    let child_err = errf.try_clone().map_err(|e| format!("{name}: {e}"))?;
    let mut child = std::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(child_err)
        .spawn()
        .map_err(|e| format!("{name}: {e}"))?;
    let t0 = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if t0.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{name} timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("{name}: {e}")),
        }
    };
    if status.success() {
        return Ok(());
    }
    let mut stderr = String::new();
    let _ = errf.seek(std::io::SeekFrom::Start(0));
    let _ = errf.read_to_string(&mut stderr);
    let tail: Vec<char> = stderr.trim().chars().collect();
    let tail: String = tail[tail.len().saturating_sub(300)..].iter().collect();
    Err(format!("{name} exited with {status}: {tail}"))
}

// ---- sidecars ----------------------------------------------------------------

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn is_sidecar_name(name: &str) -> bool {
    !name.starts_with('.') && name.ends_with(".json") && !name.ends_with(".transcript.json")
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice::<Value>(&std::fs::read(path).ok()?).ok()
}

/// Every recording sidecar in `dir`, newest recording first. Other JSON files
/// in the folder are left alone.
pub(crate) fn read_sidecars(dir: &Path) -> Vec<(PathBuf, Value)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(PathBuf, Value)> = rd
        .flatten()
        .filter(|e| is_sidecar_name(&e.file_name().to_string_lossy()))
        .filter_map(|e| {
            let v = read_json(&e.path())?;
            (v["kind"] == SIDECAR_KIND && v["id"].is_string()).then(|| (e.path(), v))
        })
        .collect();
    out.sort_by_key(|(_, v)| std::cmp::Reverse(v["started_at"].as_i64().unwrap_or(0)));
    out
}

fn find_sidecar(dir: &Path, id: &str) -> Option<(PathBuf, Value)> {
    let suffix = format!("_{id}.json");
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        if !is_sidecar_name(&name) || !name.ends_with(&suffix) {
            return None;
        }
        let v = read_json(&e.path())?;
        (v["kind"] == SIDECAR_KIND && v["id"] == id).then(|| (e.path(), v))
    })
}

/// Replace a sidecar's `transcript` object, keeping every other field exactly
/// as it is on disk.
fn set_transcript(path: &Path, transcript: Value) -> std::io::Result<Value> {
    let _g = folder_lock();
    let mut v = read_json(path).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "unreadable sidecar")
    })?;
    v["transcript"] = transcript;
    write_atomic(path, &serde_json::to_vec_pretty(&v).unwrap_or_default())?;
    if let Some(ms) = v["started_at"].as_i64() {
        let _ = stamp_times(path, ms);
    }
    Ok(v)
}

#[derive(Debug)]
pub(crate) enum StoreError {
    Conflict {
        existing_sha256: String,
        file: String,
    },
    Io(String),
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e.to_string())
    }
}

/// File one uploaded recording and return `(sidecar, deduped)`. The same id
/// with the same bytes is the device retrying and answers with what is
/// already there; the same id with DIFFERENT bytes is refused, because
/// overwriting would destroy the only other copy of someone's recording.
pub(crate) fn store_upload(
    dir: &Path,
    meta: &RecordingMeta,
    bytes: &[u8],
    ffmpeg: Option<&Path>,
) -> Result<(Value, bool), StoreError> {
    let _g = folder_lock();
    std::fs::create_dir_all(dir)?;
    let sha = hex::encode(sha2::Sha256::digest(bytes));
    if let Some((_, existing)) = find_sidecar(dir, &meta.id) {
        let have = existing["upload_sha256"].as_str().unwrap_or("").to_string();
        if have == sha {
            return Ok((existing, true));
        }
        let file = existing["file"].as_str().unwrap_or("").to_string();
        return Err(StoreError::Conflict {
            existing_sha256: have,
            file,
        });
    }
    let base = base_name(meta);
    let audio = dir.join(format!("{base}.{}", meta.ext));
    let upload_tmp = dir.join(format!(".{base}.upload.{}", meta.ext));
    std::fs::write(&upload_tmp, bytes)?;
    let (embedded, metadata_error): (bool, Option<String>) = match ffmpeg {
        Some(ff) if embeddable(meta.ext) => {
            let out_tmp = dir.join(format!(".{base}.meta.{}", meta.ext));
            let budget = Duration::from_secs(60 + (bytes.len() as u64 >> 20) * 2);
            let ran = run_bounded(ff, &metadata_args(meta, &upload_tmp, &out_tmp), budget);
            let wrote = std::fs::metadata(&out_tmp)
                .map(|m| m.len() > 0)
                .unwrap_or(false);
            if ran.is_ok() && wrote {
                std::fs::rename(&out_tmp, &audio)?;
                let _ = std::fs::remove_file(&upload_tmp);
                (true, None)
            } else {
                let _ = std::fs::remove_file(&out_tmp);
                std::fs::rename(&upload_tmp, &audio)?;
                let why = ran
                    .err()
                    .unwrap_or_else(|| "ffmpeg wrote an empty file".into());
                tracing::warn!("[recordings] metadata not embedded for {}: {why}; stored the audio as uploaded", meta.id);
                (false, Some(why))
            }
        }
        Some(_) => {
            std::fs::rename(&upload_tmp, &audio)?;
            let why = format!(
                ".{} files cannot carry these tags; the datetime is on the file and in the sidecar",
                meta.ext
            );
            (false, Some(why))
        }
        None => {
            std::fs::rename(&upload_tmp, &audio)?;
            tracing::warn!(
                "[recordings] ffmpeg not found, so {} was stored without embedded metadata",
                meta.id
            );
            (false, Some("ffmpeg not found".into()))
        }
    };
    stamp_times(&audio, meta.started_at_ms)?;
    let final_bytes = std::fs::metadata(&audio)?.len();
    let file = audio
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let sidecar = json!({
        "kind": SIDECAR_KIND,
        "schema": 1,
        "id": meta.id,
        "file": file,
        "path": audio.display().to_string(),
        "mime": meta.mime,
        "bytes": final_bytes,
        "upload_bytes": bytes.len(),
        "upload_sha256": sha,
        "started_at": meta.started_at_ms,
        "ended_at": meta.ended_at_ms,
        "dur_ms": meta.dur_ms,
        "tz_offset_min": meta.tz_offset_min,
        "recorded_at_utc": utc_of(meta.started_at_ms).to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "recorded_at_local": local_of(meta.started_at_ms, meta.tz_offset_min).to_rfc3339(),
        "location": meta.location.as_ref().map(|l| json!({
            "lat": l.lat, "lon": l.lon, "accuracy_m": l.accuracy_m, "iso6709": iso6709(l.lat, l.lon),
        })),
        "device": meta.device,
        "recovered": meta.recovered,
        "metadata_embedded": embedded,
        "metadata_error": metadata_error,
        "synced_at": now_ms(),
        "transcript": { "status": "pending", "attempts": 0 },
    });
    let sidecar_path = dir.join(format!("{base}.json"));
    write_atomic(
        &sidecar_path,
        &serde_json::to_vec_pretty(&sidecar).unwrap_or_default(),
    )?;
    let _ = stamp_times(&sidecar_path, meta.started_at_ms);
    Ok((sidecar, false))
}

// ---- transcription -------------------------------------------------------------

pub(crate) struct Transcriber {
    pub bin: PathBuf,
    pub model: PathBuf,
    pub threads: usize,
}

fn model_path() -> PathBuf {
    let raw = std::env::var(ENV_MODEL)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    if raw.contains('/') {
        return expanduser(&raw);
    }
    let stem = raw.trim_start_matches("ggml-").trim_end_matches(".bin");
    crate::config::amux_home()
        .join("models")
        .join(format!("ggml-{stem}.bin"))
}

/// The local engine, or why there is none, in words a person can act on.
pub(crate) fn transcriber() -> Result<Transcriber, String> {
    let bin = std::env::var("AMUX_WHISPER_CLI")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| super::file_viewer::find_bin("whisper-cli"))
        .ok_or_else(|| "whisper-cli is not installed (brew install whisper-cpp)".to_string())?;
    let model = model_path();
    if !model.is_file() {
        return Err(format!(
            "no local model at {} (put a whisper.cpp ggml model there, for example ggml-base.en.bin from huggingface.co/ggerganov/whisper.cpp)",
            model.display()
        ));
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get() / 2)
        .unwrap_or(4)
        .clamp(1, 8);
    Ok(Transcriber {
        bin,
        model,
        threads,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Segment {
    pub from_ms: i64,
    pub to_ms: i64,
    pub text: String,
}

/// whisper-cli's `-oj` output as segments, blank ones dropped.
pub(crate) fn parse_whisper_json(v: &Value) -> Vec<Segment> {
    v["transcription"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| {
                    let text = s["text"].as_str()?.trim().to_string();
                    (!text.is_empty()).then(|| Segment {
                        from_ms: s["offsets"]["from"].as_i64().unwrap_or(0),
                        to_ms: s["offsets"]["to"].as_i64().unwrap_or(0),
                        text,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn clock(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

fn human_duration(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        3600.. => format!("{}h {}m {}s", s / 3600, s / 60 % 60, s % 60),
        60.. => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{s}s"),
    }
}

/// The transcript file. Front matter carries the recording's own datetime, so
/// a notes app orders it by when it was said; then one line per segment with
/// its offset into the audio.
pub(crate) fn render_transcript(
    sidecar: &Value,
    segments: &[Segment],
    model: &str,
    transcribed_at: &str,
) -> String {
    let mut s = String::from("---\n");
    let _ = writeln!(
        s,
        "recorded_at: {}",
        sidecar["recorded_at_local"].as_str().unwrap_or("")
    );
    let _ = writeln!(
        s,
        "duration: {}",
        human_duration(sidecar["dur_ms"].as_i64().unwrap_or(0))
    );
    if let (Some(lat), Some(lon)) = (
        sidecar["location"]["lat"].as_f64(),
        sidecar["location"]["lon"].as_f64(),
    ) {
        let _ = writeln!(s, "location: {lat:.5}, {lon:.5}");
    }
    let _ = writeln!(s, "audio: {}", sidecar["file"].as_str().unwrap_or(""));
    let _ = writeln!(
        s,
        "engine: whisper.cpp\nmodel: {model}\ntranscribed_at: {transcribed_at}\n---\n"
    );
    if segments.is_empty() {
        s.push_str("(no speech detected)\n");
    }
    for seg in segments {
        let _ = writeln!(s, "[{}] {}", clock(seg.from_ms), seg.text);
    }
    s
}

fn strip_front_matter(s: &str) -> &str {
    s.strip_prefix("---\n")
        .and_then(|rest| {
            rest.find("\n---\n")
                .map(|i| rest[i + 5..].trim_start_matches('\n'))
        })
        .unwrap_or(s)
}

/// Transcribe one stored recording and return the transcript state to record.
pub(crate) fn transcribe_one(
    dir: &Path,
    sidecar: &Value,
    tr: &Transcriber,
    ffmpeg: &Path,
) -> Result<Value, String> {
    let t0 = Instant::now();
    let file = sidecar["file"]
        .as_str()
        .ok_or("the sidecar names no audio file")?;
    let audio = dir.join(file);
    if !audio.is_file() {
        return Err(format!("the audio file is missing: {}", audio.display()));
    }
    let work = tempfile::tempdir().map_err(|e| e.to_string())?;
    let wav = work.path().join("audio.wav");
    let dur_s = (sidecar["dur_ms"].as_i64().unwrap_or(0) / 1000).max(0) as u64;
    let mut fargs: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y", "-i"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    fargs.push(audio.display().to_string());
    fargs.extend(
        ["-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"]
            .iter()
            .map(|s| s.to_string()),
    );
    fargs.push(wav.display().to_string());
    run_bounded(ffmpeg, &fargs, Duration::from_secs(120 + dur_s / 4))?;
    let model_name = tr
        .model
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let lang = if model_name.ends_with(".en") {
        "en"
    } else {
        "auto"
    };
    let out = work.path().join("out");
    let wargs: Vec<String> = vec![
        "-m".into(),
        tr.model.display().to_string(),
        "-f".into(),
        wav.display().to_string(),
        // The long form on purpose: tests/tmux_target_audit.rs reads every short
        // t-flag argument in server source as a tmux target, and this value is
        // a thread count (AMUX-4629).
        "--threads".into(),
        tr.threads.to_string(),
        "-l".into(),
        lang.into(),
        "-np".into(),
        "-oj".into(),
        "-of".into(),
        out.display().to_string(),
    ];
    // Three times real time plus model load. base.en ran 2.4x FASTER than real time here.
    run_bounded(&tr.bin, &wargs, Duration::from_secs(300 + dur_s * 3))?;
    let raw = std::fs::read(work.path().join("out.json"))
        .map_err(|e| format!("whisper-cli wrote no JSON: {e}"))?;
    let v: Value =
        serde_json::from_slice(&raw).map_err(|e| format!("whisper-cli JSON is unreadable: {e}"))?;
    let segments = parse_whisper_json(&v);
    let base = file.rsplit_once('.').map(|(b, _)| b).unwrap_or(file);
    let started = sidecar["started_at"].as_i64().unwrap_or(0);
    let tz = sidecar["tz_offset_min"].as_i64().unwrap_or(0) as i32;
    let md_name = format!("{base}.md");
    let md = render_transcript(
        sidecar,
        &segments,
        &model_name,
        &local_of(now_ms(), tz).to_rfc3339(),
    );
    write_atomic(&dir.join(&md_name), md.as_bytes()).map_err(|e| e.to_string())?;
    let _ = stamp_times(&dir.join(&md_name), started);
    let seg_name = format!("{base}.transcript.json");
    let seg_json = json!({
        "id": sidecar["id"],
        "engine": "whisper.cpp",
        "model": model_name,
        "language": v["result"]["language"],
        "segments": segments.iter().map(|s| json!({"from_ms": s.from_ms, "to_ms": s.to_ms, "text": s.text})).collect::<Vec<_>>(),
    });
    write_atomic(
        &dir.join(&seg_name),
        &serde_json::to_vec_pretty(&seg_json).unwrap_or_default(),
    )
    .map_err(|e| e.to_string())?;
    let _ = stamp_times(&dir.join(&seg_name), started);
    let text = segments
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    Ok(json!({
        "status": "done",
        "engine": "whisper.cpp",
        "model": model_name,
        "md": md_name,
        "segments_file": seg_name,
        "words": text.split_whitespace().count(),
        "excerpt": text.chars().take(280).collect::<String>(),
        "finished_at": now_ms(),
        "secs": (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
    }))
}

/// Does this transcript state still owe work? `unavailable` does, so a
/// recording made before a model was installed is picked up afterwards.
pub(crate) fn needs_transcription(t: &Value, now: i64) -> bool {
    match t["status"].as_str().unwrap_or("pending") {
        "pending" | "unavailable" => true,
        "failed" => t["attempts"].as_i64().unwrap_or(0) < MAX_TRANSCRIBE_ATTEMPTS,
        "running" => now - t["running_since"].as_i64().unwrap_or(0) > RUNNING_STALE_MS,
        _ => false,
    }
}

static LAST_UNAVAILABLE: Mutex<String> = Mutex::new(String::new());

/// Record why nothing can be transcribed on every recording waiting for it,
/// and say so in the log once per distinct reason.
pub(crate) fn mark_unavailable(owed: &[(PathBuf, Value)], why: &str) {
    {
        let mut last = LAST_UNAVAILABLE.lock().unwrap_or_else(|p| p.into_inner());
        if *last != why {
            tracing::warn!(
                "[recordings] transcription unavailable: {why} ({} recordings waiting)",
                owed.len()
            );
            *last = why.to_string();
        }
    }
    for (path, v) in owed {
        let t = &v["transcript"];
        if t["status"] != "unavailable" || t["error"] != why {
            let _ = set_transcript(
                path,
                json!({"status": "unavailable", "error": why, "attempts": t["attempts"]}),
            );
        }
    }
}

/// One pass over `dir`: every recording that still owes a transcript, oldest
/// first, one at a time, until the tick budget is spent. Returns how many
/// transcripts were written.
pub(crate) fn transcribe_pending_in(dir: &Path, budget: Duration) -> usize {
    let now = now_ms();
    let mut owed: Vec<(PathBuf, Value)> = read_sidecars(dir)
        .into_iter()
        .filter(|(_, v)| needs_transcription(&v["transcript"], now))
        .collect();
    if owed.is_empty() {
        return 0;
    }
    owed.reverse();
    let engine = super::file_viewer::find_bin("ffmpeg")
        .ok_or_else(|| "ffmpeg is not installed (brew install ffmpeg)".to_string())
        .and_then(|ff| transcriber().map(|t| (ff, t)));
    let (ffmpeg, tr) = match engine {
        Ok(e) => e,
        Err(why) => {
            mark_unavailable(&owed, &why);
            return 0;
        }
    };
    LAST_UNAVAILABLE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
    let t0 = Instant::now();
    let mut done = 0;
    for (path, v) in owed {
        if t0.elapsed() > budget {
            break;
        }
        let id = v["id"].as_str().unwrap_or("").to_string();
        let attempts = v["transcript"]["attempts"].as_i64().unwrap_or(0) + 1;
        if set_transcript(
            &path,
            json!({"status": "running", "running_since": now_ms(), "attempts": attempts}),
        )
        .is_err()
        {
            continue;
        }
        match transcribe_one(dir, &v, &tr, &ffmpeg) {
            Ok(mut t) => {
                t["attempts"] = json!(attempts);
                tracing::info!(
                    "[recordings] transcribed {id}: {} words in {}s",
                    t["words"],
                    t["secs"]
                );
                let _ = set_transcript(&path, t);
                done += 1;
            }
            Err(e) => {
                tracing::warn!(
                    "[recordings] transcription failed for {id} (attempt {attempts} of {MAX_TRANSCRIBE_ATTEMPTS}): {e}"
                );
                let _ = set_transcript(
                    &path,
                    json!({"status": "failed", "error": e, "attempts": attempts, "finished_at": now_ms()}),
                );
            }
        }
    }
    done
}

/// The `recordings-transcribe` job's tick.
pub async fn transcribe_pending(state: AppState) {
    let dir = match current_dir(&state).await {
        Ok((d, _)) => d,
        Err(e) => {
            tracing::warn!("[recordings] could not resolve the recordings folder: {e}");
            return;
        }
    };
    let _ = tokio::task::spawn_blocking(move || transcribe_pending_in(&dir, TICK_BUDGET)).await;
}

// ---- handlers --------------------------------------------------------------------

async fn config_json(state: &AppState) -> anyhow::Result<Value> {
    let (dir, source) = current_dir(state).await?;
    let (engine, ffmpeg) =
        tokio::task::spawn_blocking(|| (transcriber(), super::file_viewer::find_bin("ffmpeg")))
            .await?;
    let model = model_path();
    let model_name = model
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let transcriber = match (engine, ffmpeg.is_some()) {
        (Ok(t), true) => json!({
            "available": true, "engine": "whisper.cpp", "bin": t.bin.display().to_string(),
            "model": model_name, "model_path": t.model.display().to_string(), "threads": t.threads,
        }),
        (engine, _) => json!({
            "available": false, "engine": "whisper.cpp", "model": model_name, "model_path": model.display().to_string(),
            "why_unavailable": engine.err().unwrap_or_else(|| "ffmpeg is not installed (brew install ffmpeg)".into()),
        }),
    };
    Ok(json!({
        "dir": dir.display().to_string(),
        "dir_source": source,
        "dir_exists": dir.is_dir(),
        "env_var": ENV_DIR,
        "ffmpeg": ffmpeg.is_some(),
        "transcriber": transcriber,
        "job": registry::ids::RECORDINGS_TRANSCRIBE,
        // False when fleet isolation registered the job without its loop: then
        // nothing transcribes on this server, and the tab should say so.
        "job_running": registry::is_triggerable(registry::ids::RECORDINGS_TRANSCRIBE),
    }))
}

async fn get_config(State(state): State<AppState>) -> Response {
    match config_json(&state).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn post_config(State(state): State<AppState>, body: axum::body::Bytes) -> Response {
    if let Some(v) = env_dir() {
        return err(
            StatusCode::CONFLICT,
            format!("{ENV_DIR} is set to {v} in the server environment and wins over this setting; change it there"),
        );
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let dir = match validate_dir(body["dir"].as_str().unwrap_or("")) {
        Ok(p) => p,
        Err((s, m)) => return err(s, m),
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return err(
            StatusCode::BAD_REQUEST,
            format!("could not create {}: {e}", dir.display()),
        );
    }
    let value = dir.display().to_string();
    let write = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO prefs (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                rusqlite::params![PREF_DIR, value],
            )?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![PendingEvent {
                    entity_type: EntityType::Other("pref".into()),
                    entity_id: PREF_DIR.into(),
                    mutation: MutationKind::Updated,
                    payload: None,
                }],
            })
        })
        .await;
    if let Err(e) = write {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    tracing::info!("[recordings] folder set to {}", dir.display());
    get_config(State(state)).await
}

#[derive(serde::Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    limit: Option<String>,
}

async fn list(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let (dir, source) = match current_dir(&state).await {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let limit = q
        .limit
        .as_deref()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 1000);
    let d = dir.clone();
    let rows = tokio::task::spawn_blocking(move || read_sidecars(&d))
        .await
        .unwrap_or_default();
    Json(json!({
        "dir": dir.display().to_string(),
        "dir_source": source,
        "dir_exists": dir.is_dir(),
        "n": rows.len(),
        "recordings": rows.into_iter().take(limit).map(|(_, v)| v).collect::<Vec<_>>(),
    }))
    .into_response()
}

async fn get_one(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    if !valid_id(&id) {
        return err(StatusCode::NOT_FOUND, "no such recording");
    }
    let dir = match current_dir(&state).await {
        Ok((d, _)) => d,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let found = tokio::task::spawn_blocking(move || {
        let (_, mut v) = find_sidecar(&dir, &id)?;
        if let Some(md) = v["transcript"]["md"].as_str().map(str::to_string) {
            if let Ok(text) = std::fs::read_to_string(dir.join(md)) {
                v["transcript"]["text"] = json!(strip_front_matter(&text));
            }
        }
        Some(v)
    })
    .await
    .ok()
    .flatten();
    match found {
        Some(v) => Json(v).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such recording"),
    }
}

async fn retranscribe(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    if !valid_id(&id) {
        return err(StatusCode::NOT_FOUND, "no such recording");
    }
    let dir = match current_dir(&state).await {
        Ok((d, _)) => d,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let key = id.clone();
    let reset = tokio::task::spawn_blocking(move || {
        let (path, _) = find_sidecar(&dir, &key)?;
        set_transcript(&path, json!({"status": "pending", "attempts": 0})).ok()
    })
    .await
    .ok()
    .flatten();
    let Some(v) = reset else {
        return err(StatusCode::NOT_FOUND, "no such recording");
    };
    let triggered = registry::trigger(registry::ids::RECORDINGS_TRANSCRIBE);
    Json(json!({"ok": true, "id": id, "transcript": v["transcript"], "triggered": triggered}))
        .into_response()
}

async fn upload(
    State(state): State<AppState>,
    Query(q): Query<UploadQuery>,
    req: Request,
) -> Response {
    let ctype = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let meta = match parse_meta(&q, &ctype, now_ms()) {
        Ok(m) => m,
        Err((s, m)) => return err(s, m),
    };
    let body = match axum::body::to_bytes(req.into_body(), MAX_UPLOAD_BYTES).await {
        Ok(b) => b,
        Err(e) => {
            return err(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "could not read the audio (limit {} MB): {e}",
                    MAX_UPLOAD_BYTES >> 20
                ),
            )
        }
    };
    if body.is_empty() {
        return err(StatusCode::BAD_REQUEST, "the recording is empty");
    }
    let (dir, source) = match current_dir(&state).await {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if let Err((s, m)) = validate_dir(&dir.display().to_string()) {
        return err(
            s,
            format!("the recordings folder ({source}) is refused: {m}"),
        );
    }
    let id = meta.id.clone();
    let stored = tokio::task::spawn_blocking(move || {
        let ffmpeg = super::file_viewer::find_bin("ffmpeg");
        store_upload(&dir, &meta, &body, ffmpeg.as_deref())
    })
    .await;
    match stored {
        Ok(Ok((sidecar, deduped))) => {
            let triggered = !deduped && registry::trigger(registry::ids::RECORDINGS_TRANSCRIBE);
            if !deduped {
                tracing::info!(
                    "[recordings] stored {} ({} bytes, metadata_embedded={}, transcription_triggered={triggered})",
                    sidecar["file"],
                    sidecar["bytes"],
                    sidecar["metadata_embedded"]
                );
            }
            let mut out = sidecar;
            out["ok"] = json!(true);
            out["deduped"] = json!(deduped);
            out["transcribe_triggered"] = json!(triggered);
            Json(out).into_response()
        }
        Ok(Err(StoreError::Conflict {
            existing_sha256,
            file,
        })) => {
            tracing::warn!("[recordings] upload refused: {id} is already stored as {file} with different bytes");
            (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!("recording {id} is already stored as {file} with different audio; nothing was overwritten"),
                    "kind": "id_conflict",
                    "existing_sha256": existing_sha256,
                    "file": file,
                })),
            )
                .into_response()
        }
        Ok(Err(StoreError::Io(e))) => {
            tracing::warn!("[recordings] could not store {id}: {e}");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not store the recording: {e}"),
            )
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    /// 2026-09-14T22:51:03Z, which is 18:51:03 in New York (EDT, -240).
    const T0: i64 = 1_789_426_263_000;

    fn meta(id: &str) -> RecordingMeta {
        RecordingMeta {
            id: id.into(),
            started_at_ms: T0,
            ended_at_ms: T0 + 12_000,
            dur_ms: 12_000,
            tz_offset_min: -240,
            mime: "audio/webm".into(),
            ext: "webm",
            location: None,
            device: "test".into(),
            recovered: false,
        }
    }

    fn mtime_ms(p: &Path) -> i64 {
        std::fs::metadata(p)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    /// Parsed by the same extractor the handler uses.
    fn q(pairs: &[(&str, &str)]) -> UploadQuery {
        let s = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let uri: axum::http::Uri = format!("/upload?{s}").parse().unwrap();
        Query::<UploadQuery>::try_from_uri(&uri).unwrap().0
    }

    #[test]
    fn an_upload_is_named_by_its_local_start_time_and_the_file_carries_that_time() {
        let dir = tempfile::tempdir().unwrap();
        let (sc, deduped) =
            store_upload(dir.path(), &meta("rTest01"), b"not really audio", None).unwrap();
        assert!(!deduped);
        assert_eq!(sc["file"], "2026-09-14_18-51-03_rTest01.webm");
        assert_eq!(sc["recorded_at_local"], "2026-09-14T18:51:03-04:00");
        assert_eq!(sc["recorded_at_utc"], "2026-09-14T22:51:03.000Z");
        assert_eq!(
            sc["upload_sha256"],
            hex::encode(sha2::Sha256::digest(b"not really audio"))
        );
        assert_eq!(sc["transcript"]["status"], "pending");
        assert_eq!(
            sc["metadata_embedded"], false,
            "no ffmpeg was passed, so nothing was embedded: {sc}"
        );
        let audio = dir.path().join("2026-09-14_18-51-03_rTest01.webm");
        assert_eq!(
            mtime_ms(&audio),
            T0,
            "the audio file's modified time is the recording start"
        );
        assert_eq!(std::fs::read(&audio).unwrap(), b"not really audio");
        let on_disk = read_json(&dir.path().join("2026-09-14_18-51-03_rTest01.json")).unwrap();
        assert_eq!(on_disk["id"], "rTest01");
    }

    #[test]
    fn the_same_recording_uploaded_twice_is_stored_once() {
        let dir = tempfile::tempdir().unwrap();
        store_upload(dir.path(), &meta("rTwice1"), b"abc", None).unwrap();
        let (_, deduped) = store_upload(dir.path(), &meta("rTwice1"), b"abc", None).unwrap();
        assert!(deduped);
        let audio: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".webm"))
            .collect();
        assert_eq!(audio.len(), 1);
    }

    #[test]
    fn different_audio_under_an_existing_id_is_refused_and_the_original_survives() {
        let dir = tempfile::tempdir().unwrap();
        store_upload(dir.path(), &meta("rClash1"), b"first", None).unwrap();
        match store_upload(dir.path(), &meta("rClash1"), b"second", None) {
            Err(StoreError::Conflict {
                existing_sha256, ..
            }) => {
                assert_eq!(existing_sha256, hex::encode(sha2::Sha256::digest(b"first")))
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(dir.path().join("2026-09-14_18-51-03_rClash1.webm")).unwrap(),
            b"first"
        );
    }

    #[test]
    fn upload_parameters_are_validated_and_location_is_optional() {
        let now = T0;
        let ok = parse_meta(
            &q(&[
                ("id", "rParse1"),
                ("started_at", "1789426263000"),
                ("dur_ms", "5000"),
                ("tz_offset_min", "-240"),
                ("lat", "40.7128"),
                ("lon", "-74.006"),
                ("accuracy_m", "12"),
            ]),
            "audio/mp4",
            now,
        )
        .unwrap();
        assert_eq!(
            (ok.ext, ok.ended_at_ms, ok.tz_offset_min),
            ("m4a", T0 + 5000, -240)
        );
        assert_eq!(
            ok.location,
            Some(Location {
                lat: 40.7128,
                lon: -74.006,
                accuracy_m: Some(12.0)
            })
        );
        let no_loc = parse_meta(
            &q(&[
                ("id", "rParse2"),
                ("started_at", "1789426263000"),
                ("lat", "40"),
            ]),
            "audio/webm;codecs=opus",
            now,
        )
        .unwrap();
        assert_eq!(
            (no_loc.ext, no_loc.location),
            ("webm", None),
            "a latitude without a longitude is no location"
        );
        let code = |pairs: &[(&str, &str)], ct: &str| parse_meta(&q(pairs), ct, now).unwrap_err().0;
        assert_eq!(
            code(
                &[("id", "../x"), ("started_at", "1789426263000")],
                "audio/mp4"
            ),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            code(&[("id", "rParse3")], "audio/mp4"),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            code(
                &[("id", "rParse4"), ("started_at", "1789426263000")],
                "text/plain"
            ),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        assert_eq!(
            code(&[("id", "rParse5"), ("started_at", "12")], "audio/mp4"),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn the_remux_embeds_the_utc_creation_time_local_date_and_iso6709_location() {
        let mut m = meta("rArgs01");
        m.ext = "m4a";
        let args = metadata_args(&m, Path::new("/in.m4a"), Path::new("/out.m4a"));
        let joined = args.join(" ");
        assert!(
            args.contains(&"creation_time=2026-09-14T22:51:03.000Z".to_string()),
            "{joined}"
        );
        assert!(args.contains(&"date=2026-09-14".to_string()), "{joined}");
        assert!(joined.contains("-c copy"), "never re-encode: {joined}");
        assert!(
            !joined.contains("location="),
            "no location was recorded: {joined}"
        );
        m.location = Some(Location {
            lat: 40.7128,
            lon: -74.006,
            accuracy_m: None,
        });
        let args = metadata_args(&m, Path::new("/in.m4a"), Path::new("/out.m4a"));
        assert!(
            args.contains(&"location=+40.7128-074.0060/".to_string()),
            "{}",
            args.join(" ")
        );
        assert_eq!(iso6709(-5.1, 7.25), "-05.1000+007.2500/");
    }

    /// The real tool, when this machine has it: ffprobe must read the
    /// recording's datetime back out of the stored file.
    #[test]
    fn with_ffmpeg_present_the_stored_file_reports_the_recording_time() {
        let (Some(ff), Some(fp)) = (
            super::super::file_viewer::find_bin("ffmpeg"),
            super::super::file_viewer::find_bin("ffprobe"),
        ) else {
            eprintln!("SKIPPED: ffmpeg/ffprobe not installed, embedding was not exercised");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.webm");
        let made = std::process::Command::new(&ff)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:a",
                "libopus",
            ])
            .arg(&src)
            .status()
            .unwrap();
        assert!(made.success());
        let store = dir.path().join("store");
        let mut m = meta("rProbe1");
        m.location = Some(Location {
            lat: 40.7128,
            lon: -74.006,
            accuracy_m: None,
        });
        let (sc, _) = store_upload(&store, &m, &std::fs::read(&src).unwrap(), Some(&ff)).unwrap();
        assert_eq!(sc["metadata_embedded"], true, "{sc}");
        let audio = store.join(sc["file"].as_str().unwrap());
        let out = std::process::Command::new(fp)
            .args(["-v", "error", "-show_entries", "format_tags", "-of", "json"])
            .arg(&audio)
            .output()
            .unwrap();
        let tags: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            tags["format"]["tags"]["creation_time"], "2026-09-14T22:51:03.000000Z",
            "{tags}"
        );
        assert_eq!(
            tags["format"]["tags"]["LOCATION"], "+40.7128-074.0060/",
            "{tags}"
        );
        assert_eq!(mtime_ms(&audio), T0);
    }

    #[test]
    fn a_transcript_carries_the_recording_datetime_and_segment_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = meta("rText01");
        m.location = Some(Location {
            lat: 40.7128,
            lon: -74.006,
            accuracy_m: None,
        });
        let (sc, _) = store_upload(dir.path(), &m, b"x", None).unwrap();
        let whisper = json!({"result": {"language": "en"}, "transcription": [
            {"offsets": {"from": 0, "to": 4000}, "text": " Pick up the van keys."},
            {"offsets": {"from": 4000, "to": 5000}, "text": "  "},
            {"offsets": {"from": 65000, "to": 70000}, "text": " Then call the landlord."},
        ]});
        let segs = parse_whisper_json(&whisper);
        assert_eq!(segs.len(), 2, "blank segments are dropped");
        let md = render_transcript(&sc, &segs, "ggml-base.en", "2026-09-14T19:00:00-04:00");
        assert!(md.starts_with("---\nrecorded_at: 2026-09-14T18:51:03-04:00\nduration: 12s\nlocation: 40.71280, -74.00600\n"), "{md}");
        assert!(
            md.contains("\n[00:00] Pick up the van keys.\n[01:05] Then call the landlord.\n"),
            "{md}"
        );
        assert_eq!(
            strip_front_matter(&md),
            "[00:00] Pick up the van keys.\n[01:05] Then call the landlord.\n"
        );
    }

    #[test]
    fn work_that_cannot_run_is_marked_unavailable_with_the_reason_and_retried_later() {
        let dir = tempfile::tempdir().unwrap();
        store_upload(dir.path(), &meta("rWait01"), b"x", None).unwrap();
        let owed = read_sidecars(dir.path());
        mark_unavailable(&owed, "no local model at /nowhere/ggml-base.en.bin");
        let (_, v) = find_sidecar(dir.path(), "rWait01").unwrap();
        assert_eq!(v["transcript"]["status"], "unavailable");
        assert_eq!(
            v["transcript"]["error"],
            "no local model at /nowhere/ggml-base.en.bin"
        );
        assert_eq!(v["started_at"], T0, "the rest of the sidecar is untouched");
        let now = now_ms();
        assert!(
            needs_transcription(&v["transcript"], now),
            "picked up again once a model exists"
        );
        assert!(!needs_transcription(&json!({"status": "done"}), now));
        assert!(!needs_transcription(
            &json!({"status": "failed", "attempts": 3}),
            now
        ));
        assert!(needs_transcription(
            &json!({"status": "failed", "attempts": 1}),
            now
        ));
        assert!(!needs_transcription(
            &json!({"status": "running", "running_since": now}),
            now
        ));
        assert!(needs_transcription(
            &json!({"status": "running", "running_since": now - RUNNING_STALE_MS - 1}),
            now
        ));
    }

    #[test]
    fn the_folder_setting_refuses_secret_and_executable_locations() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(validate_dir("~/.ssh").unwrap_err().0, StatusCode::FORBIDDEN);
        assert_eq!(
            validate_dir("~/Library/LaunchAgents").unwrap_err().0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            validate_dir("relative/dir").unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(validate_dir("").unwrap_err().0, StatusCode::BAD_REQUEST);
        assert_eq!(
            validate_dir("~/Recordings").unwrap(),
            PathBuf::from(home).join("Recordings")
        );
    }

    fn app() -> (Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("recordings-test.db")).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (
            Router::new()
                .nest("/api/recordings", routes())
                .with_state(state),
            dir,
        )
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        ctype: &str,
        body: Vec<u8>,
    ) -> (StatusCode, Value) {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", ctype)
            .body(Body::from(body))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// The shipped routes end to end: choose a folder, upload twice, list, read one.
    #[tokio::test]
    async fn a_device_upload_lands_in_the_chosen_folder_once_and_is_listed() {
        if env_dir().is_some() {
            eprintln!(
                "SKIPPED: {ENV_DIR} is set in this environment and would win over the test folder"
            );
            return;
        }
        let (app, tmp) = app();
        let folder = tmp.path().join("Recordings");
        let body = json!({"dir": folder.display().to_string()})
            .to_string()
            .into_bytes();
        let (st, cfg) = call(
            &app,
            "POST",
            "/api/recordings/config",
            "application/json",
            body,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{cfg}");
        assert_eq!(cfg["dir_source"], "pref");
        assert!(folder.is_dir(), "saving the setting creates the folder");
        let (st, bad) = call(
            &app,
            "POST",
            "/api/recordings/config",
            "application/json",
            br#"{"dir":"~/.ssh"}"#.to_vec(),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{bad}");

        let uri = "/api/recordings/upload?id=rRoute1&started_at=1789426263000&dur_ms=3000&tz_offset_min=-240";
        let (st, up) = call(&app, "POST", uri, "audio/mp4", b"fake m4a bytes".to_vec()).await;
        assert_eq!(st, StatusCode::OK, "{up}");
        assert_eq!(
            (up["deduped"].as_bool(), up["file"].as_str()),
            (Some(false), Some("2026-09-14_18-51-03_rRoute1.m4a"))
        );
        assert!(folder.join("2026-09-14_18-51-03_rRoute1.m4a").is_file());
        let (st, again) = call(&app, "POST", uri, "audio/mp4", b"fake m4a bytes".to_vec()).await;
        assert_eq!(
            (st, again["deduped"].as_bool()),
            (StatusCode::OK, Some(true)),
            "{again}"
        );
        let (st, clash) = call(&app, "POST", uri, "audio/mp4", b"other bytes".to_vec()).await;
        assert_eq!(
            (st, clash["kind"].as_str()),
            (StatusCode::CONFLICT, Some("id_conflict")),
            "{clash}"
        );

        let (st, listed) = call(&app, "GET", "/api/recordings", "application/json", vec![]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(listed["n"], 1, "{listed}");
        assert_eq!(listed["recordings"][0]["id"], "rRoute1");
        let (st, one) = call(
            &app,
            "GET",
            "/api/recordings/rRoute1",
            "application/json",
            vec![],
        )
        .await;
        assert_eq!(
            (st, one["recorded_at_local"].as_str()),
            (StatusCode::OK, Some("2026-09-14T18:51:03-04:00")),
            "{one}"
        );
        let (st, _) = call(
            &app,
            "GET",
            "/api/recordings/rMissing",
            "application/json",
            vec![],
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }
}
