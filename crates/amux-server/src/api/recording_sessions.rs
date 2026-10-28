//! Recording sessions (MSG-98487, docs/design/recorder.md): a recording
//! arrives as numbered chunks while it is being made, instead of one upload
//! after it ends.
//!
//! The one-shot `POST /api/recordings/upload` could not finish a long
//! recording: the whole file in one request, a 300 s client abort that
//! retried forever, up to 1 GB buffered in RAM here. A session takes each
//! chunk as it is written, so at stop only the last few seconds are left to
//! send, and nothing the device captured depends on one request succeeding.
//!
//! A session is an id, a start, one or more TRACKS (`mic`, `system`) and per
//! track a sequence of CHUNKS:
//! - `bytes`: fragments of one continuous stream (the browser's
//!   MediaRecorder); assembly joins them in order.
//! - `segment`: independently decodable files (the Mac app's 10 s AAC
//!   segments); assembly concatenates them with ffmpeg, filling every gap
//!   with silence so the tracks stay on one timeline.
//!
//! Every chunk lands in `<folder>/.sessions/<id>/<track>/<seq>.chunk` by
//! temp file and rename, recorded in `manifest.json` with its sha256, start
//! and duration. A missing sequence number is a gap by construction, so
//! nothing is skipped silently: `finish` refuses (409, with the missing
//! list) until the client sends them or declares them lost, and the sidecar
//! of the finished recording lists every gap.
//!
//! Assembly runs in the `recordings-transcribe` job tick (and at once on
//! finish), so a crash mid-assembly resumes on the next tick. A session
//! nobody finished within `AMUX_RECORDING_ABANDON_H` hours (default 6) is
//! assembled from what arrived and filed as `partial`. Never discarded.
//!
//! Verdicts: recording_session_opened, recording_session_finishing,
//! recording_session_filed, recording_session_abandoned,
//! recording_session_assembly_failed, recording_chunk_refused.

use super::recordings::{self, RecordingMeta};
use super::AppState;
use axum::extract::{Path as AxPath, Query, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use sha2::Digest;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const SESSIONS_DIR: &str = ".sessions";
/// The prefix of the assembly error for a session with no audio at all.
const EMPTY: &str = "empty";
const MANIFEST: &str = "manifest.json";
const MAX_CHUNK_BYTES: usize = 64 << 20;
const MAX_TRACKS: usize = 4;
/// A start later than the previous chunk's end by more than this is a gap.
const GAP_SLACK_MS: i64 = 250;
const ENV_ABANDON_H: &str = "AMUX_RECORDING_ABANDON_H";
/// An assembly lock older than this belongs to a process that died.
const ASSEMBLY_STALE_S: u64 = 30 * 60;

/// Manifest reads and writes, separate from the folder lock that filing a
/// finished recording holds while ffmpeg runs.
static MANIFEST_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    MANIFEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/sessions", get(list_route).post(open_route))
        .route("/sessions/{id}", get(get_route))
        .route(
            "/sessions/{id}/chunks/{track}/{seq}",
            put(chunk_route).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/sessions/{id}/finish", post(finish_route))
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({"ok": false, "error": msg.into()}))).into_response()
}

fn now_ms() -> i64 {
    (crate::config::now_f64() * 1000.0) as i64
}

fn valid_track(t: &str) -> bool {
    (1..=16).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub(crate) fn session_dir(dir: &Path, id: &str) -> PathBuf {
    dir.join(SESSIONS_DIR).join(id)
}

/// A session's manifest under the manifest lock (recording_live.rs).
pub(crate) fn read_manifest_pub(sdir: &Path) -> Option<Value> {
    let _g = lock();
    read_manifest(sdir)
}

fn read_manifest(sdir: &Path) -> Option<Value> {
    std::fs::read(sdir.join(MANIFEST)).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn write_manifest(sdir: &Path, m: &Value) -> std::io::Result<()> {
    let tmp = sdir.join(format!(".{MANIFEST}.{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(m).unwrap_or_default())?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, sdir.join(MANIFEST))
}

// ---- open --------------------------------------------------------------------

/// The session's metadata, checked by the same rules as a one-shot upload.
fn meta_of(m: &Value, ended_at: Option<i64>, now: i64) -> Result<RecordingMeta, (StatusCode, String)> {
    let s = |k: &str| m.get(k).and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    let q = recordings::UploadQuery::from_parts(
        s("id"), s("started_at"), ended_at.map(|e| e.to_string()), None, s("tz_offset_min"),
        s("mime"), s("lat"), s("lon"), s("accuracy_m"), s("device"),
        Some(if m["recovered"].as_bool().unwrap_or(false) { "1".into() } else { "0".into() }),
    );
    recordings::parse_meta(&q, "", now)
}

async fn open_route(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let now = now_ms();
    if let Err((s, m)) = meta_of(&body, None, now) {
        return err(s, m);
    }
    let id = body["id"].as_str().unwrap_or("").to_string();
    let kind: &'static str = match body["chunk_kind"].as_str().unwrap_or("bytes") {
        "bytes" => "bytes",
        "segment" => "segment",
        _ => "",
    };
    if kind.is_empty() {
        return err(StatusCode::BAD_REQUEST, "chunk_kind is bytes or segment");
    }
    let tracks: Vec<String> = match body.get("tracks").and_then(Value::as_array) {
        Some(a) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        None => vec!["mic".into()],
    };
    if tracks.is_empty() || tracks.len() > MAX_TRACKS || !tracks.iter().all(|t| valid_track(t)) {
        return err(StatusCode::BAD_REQUEST, "tracks: 1 to 4 lowercase names like mic or system");
    }
    if kind == "bytes" && tracks.len() > 1 {
        return err(StatusCode::BAD_REQUEST, "a bytes session has one track");
    }
    let dir = match recordings::folder(&state).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let body2 = body.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<(Value, bool), String> {
        let _g = lock();
        if let Some((_, sidecar)) = recordings::find_sidecar_pub(&dir, &id) {
            return Ok((json!({"state": "finished", "id": id, "recording": sidecar}), false));
        }
        let sdir = session_dir(&dir, &id);
        if let Some(m) = read_manifest(&sdir) {
            return Ok((summary(&m), false));
        }
        std::fs::create_dir_all(&sdir).map_err(|e| e.to_string())?;
        let mut m = Map::new();
        for k in ["id", "started_at", "tz_offset_min", "mime", "device", "lat", "lon", "accuracy_m"] {
            if let Some(v) = body2.get(k) {
                m.insert(k.into(), v.clone());
            }
        }
        m.insert("kind".into(), json!("amux-recording-session"));
        m.insert("schema".into(), json!(1));
        m.insert("chunk_kind".into(), json!(kind));
        m.insert("tracks".into(), json!(tracks));
        m.insert("state".into(), json!("open"));
        m.insert("opened_at".into(), json!(now));
        m.insert("updated_at".into(), json!(now));
        m.insert("chunks".into(), json!({}));
        m.insert("gaps".into(), json!([]));
        let m = Value::Object(m);
        write_manifest(&sdir, &m).map_err(|e| e.to_string())?;
        Ok((summary(&m), true))
    })
    .await;
    match out {
        Ok(Ok((v, created))) => {
            if created {
                tracing::info!(id = %v["id"], kind, measured = true, n_considered = 1,
                    verdict = "recording_session_opened", "recordings: a session opened");
            }
            Json(v).into_response()
        }
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---- chunks ------------------------------------------------------------------

#[derive(serde::Deserialize, Default)]
struct ChunkQuery {
    #[serde(default)]
    start_ms: Option<i64>,
    #[serde(default)]
    dur_ms: Option<i64>,
    #[serde(default)]
    sha256: Option<String>,
}

async fn chunk_route(
    State(state): State<AppState>,
    AxPath((id, track, seq)): AxPath<(String, String, u64)>,
    Query(q): Query<ChunkQuery>,
    req: Request,
) -> Response {
    let body = match axum::body::to_bytes(req.into_body(), MAX_CHUNK_BYTES).await {
        Ok(b) => b,
        Err(e) => return err(StatusCode::PAYLOAD_TOO_LARGE, format!("chunk over {} MB: {e}", MAX_CHUNK_BYTES >> 20)),
    };
    if body.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty chunk");
    }
    let sha = hex::encode(sha2::Sha256::digest(&body));
    if let Some(claimed) = q.sha256.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if !claimed.eq_ignore_ascii_case(&sha) {
            tracing::warn!(id, track, seq, measured = true, n_considered = 1, verdict = "recording_chunk_refused",
                "recordings: a chunk arrived damaged (sha mismatch); the client resends it");
            return err(StatusCode::UNPROCESSABLE_ENTITY, format!("sha256 mismatch: got {sha}"));
        }
    }
    let dir = match recordings::folder(&state).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let out = tokio::task::spawn_blocking(move || store_chunk(&dir, &id, &track, seq, &body, &sha, q.start_ms, q.dur_ms)).await;
    match out {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err((s, m))) => err(s, m),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn store_chunk(
    dir: &Path,
    id: &str,
    track: &str,
    seq: u64,
    bytes: &[u8],
    sha: &str,
    start_ms: Option<i64>,
    dur_ms: Option<i64>,
) -> Result<Value, (StatusCode, String)> {
    if seq > MAX_SEQ {
        return Err((StatusCode::BAD_REQUEST, format!("seq is over {MAX_SEQ}")));
    }
    let sdir = session_dir(dir, id);
    let io = |e: std::io::Error| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let _g = lock();
    let Some(mut m) = read_manifest(&sdir) else {
        if recordings::find_sidecar_pub(dir, id).is_some() {
            return Ok(json!({"ok": true, "state": "finished", "duplicate": true}));
        }
        return Err((StatusCode::NOT_FOUND, "no open session with that id; open it first".into()));
    };
    if !m["tracks"].as_array().is_some_and(|a| a.iter().any(|t| t == track)) {
        return Err((StatusCode::BAD_REQUEST, format!("track {track} is not in this session")));
    }
    let key = seq.to_string();
    if let Some(have) = m["chunks"][track].get(&key) {
        if have["sha256"] == sha {
            return Ok(json!({"ok": true, "seq": seq, "duplicate": true}));
        }
        return Err((StatusCode::CONFLICT, format!("chunk {track}/{seq} already holds different bytes")));
    }
    if m["state"] != "open" {
        return Err((StatusCode::CONFLICT, format!("the session is {}; it takes no more chunks", m["state"])));
    }
    let tdir = sdir.join(track);
    std::fs::create_dir_all(&tdir).map_err(io)?;
    let path = tdir.join(format!("{seq:08}.chunk"));
    let tmp = tdir.join(format!(".{seq:08}.chunk.tmp"));
    {
        let mut f = std::fs::File::create(&tmp).map_err(io)?;
        f.write_all(bytes).map_err(io)?;
        f.sync_all().map_err(io)?;
    }
    std::fs::rename(&tmp, &path).map_err(io)?;
    let entry = json!({"sha256": sha, "bytes": bytes.len(), "start_ms": start_ms, "dur_ms": dur_ms, "at": now_ms()});
    if !m["chunks"].get(track).is_some_and(Value::is_object) {
        m["chunks"][track] = json!({});
    }
    m["chunks"][track][key] = entry;
    m["updated_at"] = json!(now_ms());
    write_manifest(&sdir, &m).map_err(io)?;
    Ok(json!({"ok": true, "seq": seq, "sha256": sha}))
}

// ---- status --------------------------------------------------------------------

fn seqs(m: &Value, track: &str) -> BTreeSet<u64> {
    m["chunks"][track].as_object().map(|o| o.keys().filter_map(|k| k.parse().ok()).collect()).unwrap_or_default()
}

/// Seq numbers below the highest received that are absent: the gaps a
/// finish must either fill or declare lost.
fn missing(have: &BTreeSet<u64>, through: Option<u64>) -> Vec<u64> {
    let Some(top) = through.or_else(|| have.iter().next_back().copied()) else { return vec![] };
    (0..=top.min(MAX_SEQ)).filter(|s| !have.contains(s)).collect()
}

/// The highest seq a session may name: 10 s pieces for 30 days. A finish's
/// last_seq above it is refused before anything is walked under the lock.
const MAX_SEQ: u64 = 260_000;

fn summary(m: &Value) -> Value {
    let mut tracks = Map::new();
    for t in m["tracks"].as_array().cloned().unwrap_or_default() {
        let t = t.as_str().unwrap_or("").to_string();
        let have = seqs(m, &t);
        let bytes: u64 = m["chunks"][&t].as_object().map(|o| o.values().filter_map(|v| v["bytes"].as_u64()).sum()).unwrap_or(0);
        tracks.insert(t.clone(), json!({
            "received": have.len(), "bytes": bytes, "last_seq": have.iter().next_back(),
            "missing": missing(&have, None),
        }));
    }
    json!({"id": m["id"], "state": m["state"], "chunk_kind": m["chunk_kind"], "tracks": tracks,
           "opened_at": m["opened_at"], "updated_at": m["updated_at"], "gaps": m["gaps"], "error": m.get("error")})
}

async fn get_route(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    let dir = match recordings::folder(&state).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let out = tokio::task::spawn_blocking(move || {
        let _g = lock();
        if let Some(m) = read_manifest(&session_dir(&dir, &id)) {
            return Some(summary(&m));
        }
        recordings::find_sidecar_pub(&dir, &id).map(|(_, s)| json!({"id": id, "state": "finished", "recording": s}))
    })
    .await
    .ok()
    .flatten();
    match out {
        Some(v) => Json(v).into_response(),
        None => err(StatusCode::NOT_FOUND, "no session or recording with that id"),
    }
}

async fn list_route(State(state): State<AppState>) -> Response {
    let dir = match recordings::folder(&state).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let out = tokio::task::spawn_blocking(move || {
        let _g = lock();
        let mut v: Vec<Value> = std::fs::read_dir(dir.join(SESSIONS_DIR)).into_iter().flatten().flatten()
            .filter_map(|e| read_manifest(&e.path())).map(|m| summary(&m)).collect();
        v.sort_by_key(|s| -s["updated_at"].as_i64().unwrap_or(0));
        v
    })
    .await
    .unwrap_or_default();
    Json(json!({"sessions": out, "measured": true, "n_considered": out.len()})).into_response()
}

// ---- finish --------------------------------------------------------------------

async fn finish_route(State(state): State<AppState>, AxPath(id): AxPath<String>, Json(body): Json<Value>) -> Response {
    let dir = match recordings::folder(&state).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let d2 = dir.clone();
    let id2 = id.clone();
    let marked = tokio::task::spawn_blocking(move || mark_finishing(&d2, &id2, &body, false)).await;
    match marked {
        Ok(Ok(v)) if v["state"] == "finishing" => {
            tracing::info!(id = %id, measured = true, n_considered = 1, verdict = "recording_session_finishing",
                "recordings: a session finished capturing; assembling it");
            // Assemble now; the job tick is the recovery if this process dies.
            let id3 = id.clone();
            let assembled = tokio::task::spawn_blocking(move || {
                let ffmpeg = super::file_viewer::find_bin("ffmpeg");
                assemble(&dir, &id3, ffmpeg.as_deref())
            })
            .await;
            match assembled {
                Ok(Ok(sidecar)) => {
                    let triggered = crate::runtime_jobs::registry::trigger(crate::runtime_jobs::registry::ids::RECORDINGS_TRANSCRIBE);
                    Json(json!({"ok": true, "id": id, "state": "finished", "recording": sidecar, "transcribe_triggered": triggered})).into_response()
                }
                Ok(Err(e)) if e.starts_with(EMPTY) => Json(json!({"ok": true, "id": id, "state": "empty", "error": e})).into_response(),
                Ok(Err(e)) => (StatusCode::ACCEPTED, Json(json!({"ok": true, "id": id, "state": "finishing",
                    "error": e, "retry": "the recordings job retries the assembly on its next tick"}))).into_response(),
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            }
        }
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err((s, v))) => (s, Json(v)).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Close a session to new chunks. Refuses (409, with the missing seqs) while
/// any track has a hole the client neither sent nor declared lost, unless
/// `abandoned`, which declares every hole lost.
pub(crate) fn mark_finishing(dir: &Path, id: &str, body: &Value, abandoned: bool) -> Result<Value, (StatusCode, Value)> {
    let _g = lock();
    let sdir = session_dir(dir, id);
    let Some(mut m) = read_manifest(&sdir) else {
        if let Some((_, s)) = recordings::find_sidecar_pub(dir, id) {
            return Ok(json!({"ok": true, "id": id, "state": "finished", "recording": s}));
        }
        return Err((StatusCode::NOT_FOUND, json!({"ok": false, "error": "no session with that id"})));
    };
    if m["state"] != "open" {
        return Ok(summary(&m));
    }
    let mut holes = Map::new();
    let mut lost_all = Map::new();
    for t in m["tracks"].as_array().cloned().unwrap_or_default() {
        let t = t.as_str().unwrap_or("").to_string();
        let have = seqs(&m, &t);
        let through = body["last_seq"].get(&t).and_then(Value::as_u64);
        if through.is_some_and(|n| n > MAX_SEQ) {
            return Err((StatusCode::BAD_REQUEST, json!({"ok": false, "error": format!("last_seq for {t} is over {MAX_SEQ}")})));
        }
        let declared: BTreeSet<u64> = body["lost"].get(&t).and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
        let miss = missing(&have, through);
        let open: Vec<u64> = miss.iter().copied().filter(|s| !declared.contains(s)).collect();
        if !open.is_empty() && !abandoned {
            holes.insert(t.clone(), json!(open));
        }
        if !miss.is_empty() {
            lost_all.insert(t, json!(miss));
        }
    }
    if !holes.is_empty() {
        return Err((StatusCode::CONFLICT, json!({"ok": false, "error": "chunks are missing; send them, or declare them in `lost`",
            "missing": holes})));
    }
    let ended = body["ended_at"].as_i64().unwrap_or_else(|| m["updated_at"].as_i64().unwrap_or_else(now_ms));
    m["state"] = json!("finishing");
    m["finish"] = json!({"ended_at": ended, "lost": lost_all, "abandoned": abandoned, "at": now_ms()});
    if let Some(g) = body["gaps"].as_array() {
        let mut gaps = m["gaps"].as_array().cloned().unwrap_or_default();
        gaps.extend(g.iter().take(10_000).cloned());
        m["gaps"] = json!(gaps);
    }
    if abandoned || body["recovered"].as_bool().unwrap_or(false) {
        m["recovered"] = json!(true);
    }
    write_manifest(&sdir, &m).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"ok": false, "error": e.to_string()})))?;
    Ok(summary(&m))
}

// ---- assembly ------------------------------------------------------------------

struct AssemblyLock(PathBuf);
impl Drop for AssemblyLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn take_assembly_lock(sdir: &Path) -> Option<AssemblyLock> {
    let p = sdir.join(".assembling");
    if let Ok(md) = std::fs::metadata(&p) {
        let age = md.modified().ok().and_then(|t| t.elapsed().ok()).map(|d| d.as_secs()).unwrap_or(0);
        if age < ASSEMBLY_STALE_S {
            return None;
        }
        let _ = std::fs::remove_file(&p);
    }
    std::fs::OpenOptions::new().write(true).create_new(true).open(&p).ok().map(|_| AssemblyLock(p))
}

fn ffmpeg_run(ff: &Path, args: &[String], secs: u64) -> Result<(), String> {
    let mut all: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y"].iter().map(|s| s.to_string()).collect();
    all.extend_from_slice(args);
    recordings::run_bounded(ff, &all, Duration::from_secs(secs))
}

/// The silence that stands in for a gap, so a segment track stays on the
/// recording's timeline. Same stream shape as the Mac app's segments (ADTS
/// AAC, 48 kHz mono), so the concat demuxer joins them without a mismatch.
fn silence(ff: &Path, path: &Path, ms: i64) -> Result<(), String> {
    // The length is the source's own `d=` (not `-t`, which reads as a tmux target to tmux_target_audit).
    ffmpeg_run(ff, &["-f".into(), "lavfi".into(), "-i".into(), format!("anullsrc=r=48000:cl=mono:d={:.3}", ms as f64 / 1000.0),
        "-c:a".into(), "aac".into(), "-b:a".into(), "64k".into(),
        "-f".into(), "adts".into(), path.display().to_string()], 60)
}

/// One gap a reader can act on: which track, where, how long, and why.
fn gap(track: &str, at_ms: i64, dur_ms: i64, why: &str) -> Value {
    json!({"track": track, "at_ms": at_ms, "dur_ms": dur_ms, "why": why})
}

/// Join one track. Returns (file, computed gaps).
fn assemble_track(sdir: &Path, m: &Value, track: &str, ext: &str, ff: Option<&Path>) -> Result<(PathBuf, Vec<Value>), String> {
    let chunks: BTreeMap<u64, Value> = m["chunks"][track].as_object()
        .map(|o| o.iter().filter_map(|(k, v)| k.parse().ok().map(|s| (s, v.clone()))).collect()).unwrap_or_default();
    let lost: BTreeSet<u64> = m["finish"]["lost"][track].as_array()
        .map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
    let mut gaps = Vec::new();
    let started = m["started_at"].as_i64().unwrap_or(0);
    if chunks.is_empty() {
        return Err(format!("track {track} has no chunks"));
    }
    if m["chunk_kind"] == "bytes" {
        let out = sdir.join(format!("{track}.{ext}"));
        let mut f = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        let mut prev_end: Option<i64> = None;
        for (seq, c) in &chunks {
            let bytes = std::fs::read(sdir.join(track).join(format!("{seq:08}.chunk"))).map_err(|e| e.to_string())?;
            f.write_all(&bytes).map_err(|e| e.to_string())?;
            if let (Some(pe), Some(s)) = (prev_end, c["start_ms"].as_i64()) {
                if s - pe > GAP_SLACK_MS {
                    gaps.push(gap(track, pe - started, s - pe, "time"));
                }
            }
            prev_end = c["start_ms"].as_i64().zip(c["dur_ms"].as_i64()).map(|(s, d)| s + d).or(prev_end);
        }
        for s in &lost {
            gaps.push(gap(track, chunks.range(..s).next_back().and_then(|(_, c)| c["start_ms"].as_i64()).unwrap_or(started) - started, 0, "lost_chunk"));
        }
        f.sync_all().map_err(|e| e.to_string())?;
        return Ok((out, gaps));
    }
    // Segments: ffmpeg concat with silence wherever the timeline has a hole.
    let ff = ff.ok_or("ffmpeg is needed to join segment recordings")?;
    let list = sdir.join(format!("{track}.concat.txt"));
    let mut lines = String::new();
    let mut cursor = started;
    let mut n_sil = 0;
    for (seq, c) in &chunks {
        let s = c["start_ms"].as_i64().unwrap_or(cursor);
        if s - cursor > GAP_SLACK_MS {
            let why = if lost.iter().any(|l| l < seq) { "lost_chunk" } else { "time" };
            gaps.push(gap(track, cursor - started, s - cursor, why));
            let sp = sdir.join(format!("{track}.sil{n_sil}.aac"));
            silence(ff, &sp, s - cursor)?;
            lines.push_str(&format!("file '{}'\nduration {:.3}\n", sp.display().to_string().replace('\'', "'\\''"), (s - cursor) as f64 / 1000.0));
            n_sil += 1;
        }
        let cp = sdir.join(track).join(format!("{seq:08}.chunk"));
        // The manifest's duration, never ffmpeg's: a raw ADTS piece has no
        // duration, so the concat demuxer estimates one from the bitrate
        // (18.5 s for a 10 s piece) and a 2 h recording came out 3 h long.
        let d = c["dur_ms"].as_i64().unwrap_or(0);
        lines.push_str(&format!("file '{}'\n", cp.display().to_string().replace('\'', "'\\''")));
        if d > 0 {
            lines.push_str(&format!("duration {:.3}\n", d as f64 / 1000.0));
        }
        cursor = s + d;
    }
    std::fs::write(&list, lines).map_err(|e| e.to_string())?;
    let out = sdir.join(format!("{track}.m4a"));
    ffmpeg_run(ff, &["-f".into(), "concat".into(), "-safe".into(), "0".into(), "-i".into(), list.display().to_string(),
        // Each piece decodes a little long (the encoder's priming and padding),
        // so pieces overlap their neighbour by a few ms; resampling to the
        // timestamps absorbs that instead of drifting or dropping speech.
        "-af".into(), "aresample=async=1000:first_pts=0".into(),
        "-c:a".into(), "aac".into(), "-b:a".into(), "96k".into(), out.display().to_string()], 3600)?;
    Ok((out, gaps))
}

fn sha_file(p: &Path) -> std::io::Result<(String, u64)> {
    let mut f = std::fs::File::open(p)?;
    let mut h = sha2::Sha256::new();
    let n = std::io::copy(&mut f, &mut h)?;
    Ok((hex::encode(h.finalize()), n))
}

/// Join a `finishing` session's tracks into one recording and file it with
/// its sidecar, then remove the session directory. Idempotent; a crash
/// anywhere leaves either the session (retried) or the filed recording.
pub(crate) fn assemble(dir: &Path, id: &str, ff: Option<&Path>) -> Result<Value, String> {
    let sdir = session_dir(dir, id);
    let Some(_held) = take_assembly_lock(&sdir) else {
        return Err("another assembly of this session is running".into());
    };
    let m = { let _g = lock(); read_manifest(&sdir) }.ok_or("the session manifest is gone")?;
    if m["state"] != "finishing" {
        return Err(format!("the session is {}, not finishing", m["state"]));
    }
    let ended = m["finish"]["ended_at"].as_i64();
    let mut mv = m.clone();
    let segment = m["chunk_kind"] == "segment";
    if segment {
        mv["mime"] = json!("audio/mp4");
    }
    let meta = meta_of(&mv, ended, now_ms()).map_err(|(_, e)| e)?;
    let tracks: Vec<String> = m["tracks"].as_array().cloned().unwrap_or_default().iter()
        .filter_map(|t| t.as_str().map(str::to_string)).collect();
    let mut gaps: Vec<Value> = m["gaps"].as_array().cloned().unwrap_or_default();
    let mut files = Vec::new();
    for t in &tracks {
        if m["chunks"][t].as_object().is_none_or(|o| o.is_empty()) {
            gaps.push(gap(t, 0, meta.dur_ms, "no_audio"));
            continue;
        }
        let (f, g) = assemble_track(&sdir, &m, t, meta.ext, ff)?;
        gaps.extend(g);
        files.push((t.clone(), f));
    }
    if files.is_empty() {
        // Nothing to file: the session is closed as empty, with the reason and
        // its manifest kept, instead of waiting in `finishing` for audio that
        // will never come (a microphone that gave nothing, 2026-10-28).
        let why = "no track has any audio";
        {
            let _g = lock();
            if let Some(mut m2) = read_manifest(&sdir) {
                m2["state"] = json!("empty");
                m2["error"] = json!(why);
                let _ = write_manifest(&sdir, &m2);
            }
        }
        tracing::warn!(id, measured = true, n_considered = tracks.len(), verdict = "recording_session_empty",
            "recordings: a session finished with no audio on any track; closed as empty, its manifest kept");
        return Err(format!("{EMPTY}: {why}"));
    }
    let main = if files.len() == 1 {
        files[0].1.clone()
    } else {
        let ff = ff.ok_or("ffmpeg is needed to mix tracks")?;
        let out = sdir.join("mix.m4a");
        let mut args = Vec::new();
        for (_, f) in &files {
            args.push("-i".to_string());
            args.push(f.display().to_string());
        }
        args.extend(["-filter_complex".into(), format!("amix=inputs={}:duration=longest:normalize=0", files.len()),
            "-c:a".into(), "aac".into(), "-b:a".into(), "128k".into(), out.display().to_string()]);
        ffmpeg_run(ff, &args, 3600)?;
        out
    };
    let lost: usize = m["finish"]["lost"].as_object().map(|o| o.values().filter_map(Value::as_array).map(Vec::len).sum()).unwrap_or(0);
    let abandoned = m["finish"]["abandoned"].as_bool().unwrap_or(false);
    let partial = abandoned || lost > 0;
    let base = recordings::base_name(&meta);
    let mut track_files = Map::new();
    if files.len() > 1 {
        for (t, f) in &files {
            let dest = dir.join(format!("{base}.{t}.m4a"));
            std::fs::rename(f, &dest).map_err(|e| e.to_string())?;
            track_files.insert(t.clone(), json!(dest.file_name().map(|n| n.to_string_lossy().into_owned())));
        }
    }
    let n_chunks: usize = tracks.iter().map(|t| seqs(&m, t).len()).sum();
    let mut extra = json!({"session": {
        "chunk_kind": m["chunk_kind"], "tracks": tracks, "track_files": track_files, "chunks": n_chunks,
        "gaps": gaps, "gap_ms": gaps.iter().filter_map(|g| g["dur_ms"].as_i64()).sum::<i64>(),
        "lost_chunks": lost, "partial": partial, "abandoned": abandoned,
    }, "partial": partial});
    // The live transcript and the notes typed during the meeting travel with
    // the recording: the session folder is removed once it is filed.
    for (k, v) in super::recording_live::session_extras(&sdir) {
        extra[k] = v;
    }
    let staged = dir.join(format!(".{base}.upload.{}", meta.ext));
    std::fs::rename(&main, &staged).map_err(|e| e.to_string())?;
    let (sha, len) = sha_file(&staged).map_err(|e| e.to_string())?;
    let filed = {
        let _f = recordings::folder_lock_pub();
        recordings::file_upload(dir, &meta, &staged, &sha, len, ff, Some(extra))
    };
    let (sidecar, _) = filed.map_err(|e| format!("{e:?}"))?;
    let _ = std::fs::remove_dir_all(&sdir);
    tracing::info!(id, file = %sidecar["file"], chunks = n_chunks, gaps = gaps.len(), lost, partial, abandoned,
        measured = true, n_considered = n_chunks, verdict = "recording_session_filed",
        "recordings: a session was assembled and filed");
    Ok(sidecar)
}

/// The job tick's half of sessions: assemble every `finishing` session, and
/// close and assemble every `open` one nobody touched for the abandon window.
/// Returns (filed, failed).
pub(crate) fn finish_sessions_in(dir: &Path, ff: Option<&Path>, now: i64) -> (usize, usize) {
    let abandon_ms = std::env::var(ENV_ABANDON_H).ok().and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|h| *h > 0.0).unwrap_or(6.0) * 3_600_000.0;
    let (mut filed, mut failed) = (0, 0);
    let entries: Vec<PathBuf> = std::fs::read_dir(dir.join(SESSIONS_DIR)).into_iter().flatten().flatten().map(|e| e.path()).collect();
    for sdir in entries {
        let Some(m) = ({ let _g = lock(); read_manifest(&sdir) }) else { continue };
        let id = m["id"].as_str().unwrap_or("").to_string();
        if m["state"] == "open" && (now - m["updated_at"].as_i64().unwrap_or(now)) as f64 > abandon_ms {
            tracing::warn!(id, measured = true, n_considered = 1, verdict = "recording_session_abandoned",
                "recordings: a session stopped receiving chunks and was never finished; filing what arrived as partial");
            let _ = mark_finishing(dir, &id, &json!({}), true);
        }
        let Some(m) = ({ let _g = lock(); read_manifest(&sdir) }) else { continue };
        if m["state"] != "finishing" {
            continue;
        }
        match assemble(dir, &id, ff) {
            Ok(_) => filed += 1,
            Err(e) => {
                failed += 1;
                tracing::warn!(id, error = %e, measured = true, n_considered = 1, verdict = "recording_session_assembly_failed",
                    "recordings: a finished session could not be assembled yet; its chunks are kept and the next tick retries");
                let _g = lock();
                if let Some(mut m2) = read_manifest(&sdir) {
                    m2["error"] = json!(e);
                    let _ = write_manifest(&sdir, &m2);
                }
            }
        }
    }
    (filed, failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(dir: &Path, id: &str, kind: &str, tracks: &[&str], started: i64) {
        let sdir = session_dir(dir, id);
        std::fs::create_dir_all(&sdir).unwrap();
        write_manifest(&sdir, &json!({"kind": "amux-recording-session", "schema": 1, "id": id, "started_at": started,
            "tz_offset_min": -240, "mime": "audio/webm", "chunk_kind": kind, "tracks": tracks, "state": "open",
            "opened_at": started, "updated_at": started, "chunks": {}, "gaps": []})).unwrap();
    }

    fn put(dir: &Path, id: &str, track: &str, seq: u64, b: &[u8], start: i64, dur: i64) -> Result<Value, (StatusCode, String)> {
        store_chunk(dir, id, track, seq, b, &hex::encode(sha2::Sha256::digest(b)), Some(start), Some(dur))
    }

    #[test]
    fn chunks_are_idempotent_and_conflicting_bytes_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let t0 = 1_793_000_000_000;
        open(d.path(), "sess-001", "bytes", &["mic"], t0);
        assert_eq!(put(d.path(), "sess-001", "mic", 0, b"aa", t0, 1000).unwrap()["ok"], true);
        assert_eq!(put(d.path(), "sess-001", "mic", 0, b"aa", t0, 1000).unwrap()["duplicate"], true, "a resend is a no-op");
        assert_eq!(put(d.path(), "sess-001", "mic", 0, b"bb", t0, 1000).unwrap_err().0, StatusCode::CONFLICT);
        assert_eq!(put(d.path(), "sess-001", "system", 0, b"aa", t0, 1000).unwrap_err().0, StatusCode::BAD_REQUEST);
        assert_eq!(put(d.path(), "nope-001", "mic", 0, b"aa", t0, 1000).unwrap_err().0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn finish_refuses_a_hole_until_it_is_sent_or_declared_lost() {
        let d = tempfile::tempdir().unwrap();
        let t0 = 1_793_000_000_000;
        open(d.path(), "sess-002", "bytes", &["mic"], t0);
        for s in [0u64, 1, 3] {
            put(d.path(), "sess-002", "mic", s, format!("c{s}").as_bytes(), t0 + s as i64 * 1000, 1000).unwrap();
        }
        let (st, v) = mark_finishing(d.path(), "sess-002", &json!({"last_seq": {"mic": 4}}), false).unwrap_err();
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(v["missing"]["mic"], json!([2, 4]), "the hole and the unsent tail are both named");
        put(d.path(), "sess-002", "mic", 4, b"c4", t0 + 4000, 1000).unwrap();
        let v = mark_finishing(d.path(), "sess-002", &json!({"last_seq": {"mic": 4}, "lost": {"mic": [2]}, "ended_at": t0 + 5000}), false).unwrap();
        assert_eq!(v["state"], "finishing");
        assert!(put(d.path(), "sess-002", "mic", 5, b"late", t0, 1).is_err(), "a finishing session takes no more chunks");
        open(d.path(), "sess-007", "bytes", &["mic"], t0);
        let started = std::time::Instant::now();
        let (st, _) = mark_finishing(d.path(), "sess-007", &json!({"last_seq": {"mic": 999_999_999_999u64}}), false).unwrap_err();
        assert_eq!(st, StatusCode::BAD_REQUEST, "an absurd last_seq is refused, not walked");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(put(d.path(), "sess-007", "mic", MAX_SEQ + 1, b"x", t0, 1).unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_bytes_session_is_filed_in_order_with_its_gaps_and_marked_partial() {
        let d = tempfile::tempdir().unwrap();
        let t0 = 1_793_000_000_000;
        open(d.path(), "sess-003", "bytes", &["mic"], t0);
        // Arrive out of order; seq 2 is lost; seq 3 starts 3 s late.
        put(d.path(), "sess-003", "mic", 1, b"BB", t0 + 1000, 1000).unwrap();
        put(d.path(), "sess-003", "mic", 0, b"AA", t0, 1000).unwrap();
        put(d.path(), "sess-003", "mic", 3, b"DD", t0 + 6000, 1000).unwrap();
        mark_finishing(d.path(), "sess-003", &json!({"last_seq": {"mic": 3}, "lost": {"mic": [2]}, "ended_at": t0 + 7000,
            "gaps": [{"track": "mic", "at_ms": 2000, "dur_ms": 3000, "why": "sleep"}]}), false).unwrap();
        let sc = assemble(d.path(), "sess-003", None).unwrap();
        let audio = d.path().join(sc["file"].as_str().unwrap());
        assert_eq!(std::fs::read(&audio).unwrap(), b"AABBDD", "joined in seq order, whatever the arrival order");
        assert_eq!(sc["partial"], true, "a lost chunk makes it partial");
        let gaps = sc["session"]["gaps"].as_array().unwrap();
        let whys: Vec<&str> = gaps.iter().filter_map(|g| g["why"].as_str()).collect();
        assert!(whys.contains(&"sleep") && whys.contains(&"time") && whys.contains(&"lost_chunk"), "{gaps:?}");
        assert!(!session_dir(d.path(), "sess-003").exists(), "the session directory is gone once filed");
        // Finishing again returns the filed recording, not an error.
        let v = mark_finishing(d.path(), "sess-003", &json!({}), false).unwrap();
        assert_eq!(v["state"], "finished");
    }

    /// Real ADTS pieces, because the bug lives in how ffmpeg times them: with
    /// no duration in the stream, the concat demuxer guessed one from the
    /// bitrate and a 2 h recording was filed 3 h long (2026-10-27).
    #[test]
    fn a_segment_session_is_as_long_as_its_audio_plus_its_gaps() {
        let Some(ff) = crate::api::file_viewer::find_bin("ffmpeg") else {
            eprintln!("SKIPPED a_segment_session_is_as_long_as_its_audio_plus_its_gaps: no ffmpeg on this host");
            return;
        };
        let d = tempfile::tempdir().unwrap();
        let t0 = 1_793_000_000_000;
        open(d.path(), "sess-006", "segment", &["mic"], t0);
        // Six 10 s pieces, each opening with 2 s of silence as speech does:
        // that start is what threw ffmpeg's bitrate guess off. A 5 s hole
        // before seq 3.
        for seq in 0..6u64 {
            let p = d.path().join(format!("p{seq}.aac"));
            ffmpeg_run(&ff, &["-f".into(), "lavfi".into(), "-i".into(), "anoisesrc=r=48000:a=0.3:c=white:d=10".into(),
                "-af".into(), "volume=enable='lt(t,2)':volume=0".into(),
                "-ac".into(), "1".into(), "-c:a".into(), "aac".into(), "-b:a".into(), "64k".into(), "-f".into(), "adts".into(),
                p.display().to_string()], 60).unwrap();
            let start = t0 + seq as i64 * 10_000 + if seq >= 3 { 5_000 } else { 0 };
            put(d.path(), "sess-006", "mic", seq, &std::fs::read(&p).unwrap(), start, 10_000).unwrap();
        }
        mark_finishing(d.path(), "sess-006", &json!({"last_seq": {"mic": 5}, "ended_at": t0 + 65_000}), false).unwrap();
        let sc = assemble(d.path(), "sess-006", Some(&ff)).unwrap();
        let out = std::process::Command::new(ff.with_file_name("ffprobe"))
            .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
            .arg(d.path().join(sc["file"].as_str().unwrap())).output();
        let secs: f64 = out.ok().and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok()).unwrap_or(-1.0);
        assert!((secs - 65.0).abs() < 0.5, "60 s of audio and a 5 s gap filed as {secs} s");
        let gaps = sc["session"]["gaps"].as_array().unwrap();
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0]["dur_ms"], 5000);
    }

    #[test]
    fn an_abandoned_session_is_filed_as_partial_never_dropped() {
        let d = tempfile::tempdir().unwrap();
        let t0 = 1_793_000_000_000;
        open(d.path(), "sess-004", "bytes", &["mic"], t0);
        put(d.path(), "sess-004", "mic", 0, b"AA", t0, 1000).unwrap();
        put(d.path(), "sess-004", "mic", 2, b"CC", t0 + 2000, 1000).unwrap();
        // Not yet abandoned: nothing happens.
        assert_eq!(finish_sessions_in(d.path(), None, t0 + 60_000), (0, 0));
        assert!(session_dir(d.path(), "sess-004").exists());
        // Past the window: filed with the hole declared lost.
        let late = now_ms() + 7 * 3_600_000;
        assert_eq!(finish_sessions_in(d.path(), None, late), (1, 0));
        let (_, sc) = recordings::find_sidecar_pub(d.path(), "sess-004").expect("filed");
        assert_eq!((sc["partial"].clone(), sc["session"]["abandoned"].clone(), sc["recovered"].clone()),
            (json!(true), json!(true), json!(true)));
    }

    #[test]
    fn a_session_with_no_audio_is_closed_as_empty_not_retried_forever() {
        let d = tempfile::tempdir().unwrap();
        let t0 = 1_793_000_000_000;
        open(d.path(), "sess-008", "segment", &["mic"], t0);
        mark_finishing(d.path(), "sess-008", &json!({"last_seq": {}}), false).unwrap();
        let e = assemble(d.path(), "sess-008", None).unwrap_err();
        assert!(e.starts_with(EMPTY), "{e}");
        let m = read_manifest(&session_dir(d.path(), "sess-008")).unwrap();
        assert_eq!(m["state"], "empty", "closed, with its manifest kept as evidence");
        assert_eq!(finish_sessions_in(d.path(), None, now_ms()), (0, 0), "and not assembled again");
    }

    #[test]
    fn a_failed_assembly_keeps_the_chunks_and_says_why() {
        let d = tempfile::tempdir().unwrap();
        let t0 = 1_793_000_000_000;
        open(d.path(), "sess-005", "segment", &["mic", "system"], t0);
        put(d.path(), "sess-005", "mic", 0, b"not really aac", t0, 10_000).unwrap();
        mark_finishing(d.path(), "sess-005", &json!({"last_seq": {"mic": 0}}), false).unwrap();
        // No ffmpeg: segments cannot be joined.
        assert_eq!(finish_sessions_in(d.path(), None, now_ms()), (0, 1));
        let m = read_manifest(&session_dir(d.path(), "sess-005")).unwrap();
        assert_eq!(m["state"], "finishing", "still owed, retried next tick");
        assert!(m["error"].as_str().unwrap().contains("ffmpeg"), "{m}");
        assert!(session_dir(d.path(), "sess-005").join("mic").join("00000000.chunk").exists());
    }
}
