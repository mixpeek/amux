//! Live transcription, meeting notes and meeting understanding for
//! recordings (owner, 2026-10-28).
//!
//! **Live transcription** (on by default, `recordings_live`). While a
//! recording session is open, the `recordings-live` job transcribes each
//! track's newly arrived audio in windows of about `LIVE_WINDOW_MS` (15 s), with the
//! same engine the finished transcript uses. Each window is appended to
//! `<session>/live.json` and the readable `<session>/live.md` before the next
//! one starts, so if the app, the device or this server dies mid-meeting, the
//! words up to that point are already on disk. When the session is filed the
//! live pieces ride in its sidecar (`live`), and the finished transcript is
//! those pieces plus only the tail nobody transcribed yet: the work is done
//! progressively, not all at once at the end. With two tracks the lines are
//! labelled You (microphone) and Them (the computer's sound).
//!
//! **Meeting notes** (opt-in, `recordings_notes`). Once a transcript is done,
//! the helper model (`AMUX_HELPER_MODEL`, `lookup::helper_answer`) merges it
//! with the notes typed into the Record tab during the meeting into
//! `<base>.notes.md`: a title, a summary, decisions and action items, then the
//! person's own notes verbatim.
//!
//! **Meeting understanding** (opt-in, `recordings_organize`). The same call is
//! given the fleet (every worker's name, groups and description) and says
//! which workers and groups the meeting concerns, choosing only from that
//! list. The recording is filed with them (`about` in its sidecar, shown in
//! the Record tab), and a short summary is posted in each matched group's
//! Messages channel as the Recorder, with no @mention, so nobody is woken.
//!
//! Verdicts: recording_live_transcribed, recording_live_failed,
//! recording_transcript_from_live, recording_notes_written,
//! recording_notes_failed, recording_filed_with_workers.

use super::recordings::{self, Plan, Segment, Transcriber};
use super::AppState;
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) const PREF_LIVE: &str = "recordings_live";
pub(crate) const PREF_NOTES: &str = "recordings_notes";
pub(crate) const PREF_ORGANIZE: &str = "recordings_organize";
/// A track's new audio is transcribed once this much has arrived. 15 s: the
/// words land about 20 s after they are said (a 30 s window had the first line
/// 44 s in, after a 33 s meeting had ended).
const LIVE_WINDOW_MS: i64 = 15_000;
/// A shorter run is transcribed anyway once nothing new arrived for this long
/// (a pause, a stop), so the live log never trails the audio by more.
const LIVE_IDLE_MS: i64 = 12_000;
/// One window never takes more than this much audio, so a backlog (the
/// server was down mid-meeting) drains in bounded steps.
const LIVE_MAX_MS: i64 = 120_000;
/// A failed window waits this long before it is tried again.
const LIVE_RETRY_MS: i64 = 60_000;
const LIVE_FILE: &str = "live.json";
const LIVE_MD: &str = "live.md";
const NOTES_FILE: &str = "notes.md";
const SEND_TO_FILE: &str = "send_to.json";
const MAX_NOTES_ATTEMPTS: i64 = 3;
const MAX_USER_NOTES: usize = 200_000;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/options", get(get_options).post(post_options))
        .route("/sessions/{id}/live", get(get_live))
        .route("/sessions/{id}/notes", put(put_notes))
        .route("/sessions/{id}/send-to", put(put_session_send_to))
        .route("/{id}/notes", post(write_notes_now))
        .route("/{id}/send", post(send_now))
        .route("/{id}/edit", patch(edit_recording))
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({"ok": false, "error": msg.into()}))).into_response()
}

fn now_ms() -> i64 {
    (crate::config::now_f64() * 1000.0) as i64
}

// ---- options ------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Options {
    pub live: bool,
    pub notes: bool,
    pub organize: bool,
}

fn pref_bool(conn: &rusqlite::Connection, key: &str, default: bool) -> bool {
    conn.query_row("SELECT value FROM prefs WHERE key=?1", rusqlite::params![key], |r| r.get::<_, String>(0))
        .ok()
        .map(|v| matches!(v.trim(), "1" | "true" | "on" | "yes"))
        .unwrap_or(default)
}

pub(crate) fn options(conn: &rusqlite::Connection) -> Options {
    Options {
        live: pref_bool(conn, PREF_LIVE, true),
        notes: pref_bool(conn, PREF_NOTES, false),
        organize: pref_bool(conn, PREF_ORGANIZE, false),
    }
}

fn options_json(o: Options) -> Value {
    // Who a recording can be sent to: every worker, isolated ones included
    // (the owner's send reaches them), with what each does.
    let workers: Vec<Value> = super::conversations::fleet_directory_all().into_iter()
        .map(|(n, g, d, iso)| json!({"name": n, "groups": g, "desc": d, "isolated": iso})).collect();
    json!({
        "live": o.live, "notes": o.notes, "organize": o.organize, "workers": workers,
        "defaults": {"live": true, "notes": false, "organize": false},
        "helper": "AMUX_HELPER_MODEL (Settings, helper model) writes notes and files meetings",
    })
}

async fn get_options(State(state): State<AppState>) -> Response {
    match state.store.read() {
        Ok(c) => Json(options_json(options(&c))).into_response(),
        Err(e) => err(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
    }
}

async fn post_options(State(state): State<AppState>, Json(b): Json<Value>) -> Response {
    let mut sets: Vec<(&'static str, String)> = Vec::new();
    for (field, key) in [("live", PREF_LIVE), ("notes", PREF_NOTES), ("organize", PREF_ORGANIZE)] {
        if let Some(v) = b.get(field) {
            let Some(on) = v.as_bool() else {
                return err(StatusCode::BAD_REQUEST, format!("{field} is true or false"));
            };
            sets.push((key, if on { "1" } else { "0" }.to_string()));
        }
    }
    if sets.is_empty() {
        return err(StatusCode::BAD_REQUEST, "name live, notes or organize");
    }
    let log = sets.clone();
    let w = state
        .store
        .write_async(move |c| {
            for (k, v) in &sets {
                c.execute(
                    "INSERT INTO prefs (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    rusqlite::params![k, v],
                )?;
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    if let Err(e) = w {
        return err(StatusCode::SERVICE_UNAVAILABLE, e.to_string());
    }
    tracing::info!(set = ?log, "recordings: options changed");
    get_options(State(state)).await
}

// ---- clip transcription ----------------------------------------------------------

/// The engine a clip is transcribed with: the one the finished transcript
/// would use on this host.
pub(crate) enum ClipEngine {
    Local { ffmpeg: PathBuf, tr: Transcriber },
    Gemini { ffmpeg: Option<PathBuf>, key: String, vocab: String },
}

impl ClipEngine {
    pub(crate) fn from_plan(plan: &Plan) -> Result<ClipEngine, String> {
        use super::transcription::Engine;
        let (engine, local) = plan.engine();
        match engine {
            Engine::Unavailable(why) => Err(why),
            Engine::Local => local.map(|(ffmpeg, tr)| ClipEngine::Local { ffmpeg, tr }),
            Engine::Gemini => {
                let (key, vocab) = plan.gemini.clone().ok_or("no Gemini key")?;
                Ok(ClipEngine::Gemini { ffmpeg: super::file_viewer::find_bin("ffmpeg"), key, vocab })
            }
        }
    }
    fn name(&self) -> (&'static str, String) {
        match self {
            ClipEngine::Local { tr, .. } => (
                "whisper.cpp",
                tr.model.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            ),
            ClipEngine::Gemini { .. } => ("gemini", super::dictation::dictation_model()),
        }
    }
    fn ffmpeg(&self) -> Option<&Path> {
        match self {
            ClipEngine::Local { ffmpeg, .. } => Some(ffmpeg),
            ClipEngine::Gemini { ffmpeg, .. } => ffmpeg.as_deref(),
        }
    }
}

/// Transcribe `input` from `from_s` (seconds into it) to the end. Segment
/// times are milliseconds from `from_s`.
pub(crate) fn transcribe_clip(eng: &ClipEngine, input: &Path, from_s: f64, work: &Path) -> Result<Vec<Segment>, String> {
    let ff = eng.ffmpeg().ok_or("ffmpeg is needed for live transcription")?;
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y", "-i"].iter().map(|s| s.to_string()).collect();
    a.push(input.display().to_string());
    if from_s > 0.0 {
        // After -i: accurate on streams with no index (a WebM prefix).
        a.push("-ss".into());
        a.push(format!("{from_s:.3}"));
    }
    match eng {
        ClipEngine::Local { tr, .. } => {
            let wav = work.join("clip.wav");
            a.extend(["-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"].iter().map(|s| s.to_string()));
            a.push(wav.display().to_string());
            recordings::run_bounded(ff, &a, Duration::from_secs(300))?;
            let lang = if tr.model.file_stem().is_some_and(|s| s.to_string_lossy().ends_with(".en")) { "en" } else { "auto" };
            let out = work.join("clip");
            let w: Vec<String> = vec![
                "-m".into(), tr.model.display().to_string(), "-f".into(), wav.display().to_string(),
                "--threads".into(), tr.threads.to_string(), "-l".into(), lang.into(),
                "-np".into(), "-oj".into(), "-of".into(), out.display().to_string(),
            ];
            recordings::run_bounded(&tr.bin, &w, Duration::from_secs(300))?;
            let raw = std::fs::read(work.join("clip.json")).map_err(|e| format!("whisper-cli wrote no JSON: {e}"))?;
            let v: Value = serde_json::from_slice(&raw).map_err(|e| format!("whisper-cli JSON is unreadable: {e}"))?;
            Ok(recordings::parse_whisper_json(&v))
        }
        ClipEngine::Gemini { key, vocab, .. } => {
            let mp3 = work.join("clip.mp3");
            a.extend(["-ac", "1", "-ar", "16000", "-b:a", "32k"].iter().map(|s| s.to_string()));
            a.push(mp3.display().to_string());
            recordings::run_bounded(ff, &a, Duration::from_secs(300))?;
            let bytes = std::fs::read(&mp3).map_err(|e| e.to_string())?;
            let dur_ms = (bytes.len() as i64 * 8 * 1000) / 32_000;
            let mut prompt = String::from(
                "Transcribe this audio verbatim, in the language spoken. Output only the transcript text: \
                 no commentary, headings or timestamps. If nobody speaks, output nothing.",
            );
            if !vocab.is_empty() {
                prompt.push('\n');
                prompt.push_str(vocab);
            }
            let text = recordings::block_on(super::transcription::gemini_transcribe(key, "audio/mpeg", &bytes, &prompt, 120))?;
            let text = text.trim().to_string();
            Ok(if text.is_empty() { vec![] } else { vec![Segment { from_ms: 0, to_ms: dur_ms, text }] })
        }
    }
}

// ---- live transcription ----------------------------------------------------------

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

fn write_atomic(p: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = p.with_file_name(format!(".{}.tmp", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, p)
}

fn seg_json(s: &Segment) -> Value {
    json!({"from_ms": s.from_ms, "to_ms": s.to_ms, "text": s.text})
}

fn segs_of(v: &Value) -> Vec<Segment> {
    v.as_array().map(|a| a.iter().filter_map(|s| Some(Segment {
        from_ms: s["from_ms"].as_i64()?, to_ms: s["to_ms"].as_i64().unwrap_or(0), text: s["text"].as_str()?.to_string(),
    })).collect()).unwrap_or_default()
}

fn speaker(track: &str) -> &'static str {
    if track == "system" { "Them" } else { "You" }
}

/// Every track's segments on one timeline; labelled by speaker when there is
/// more than one track.
pub(crate) fn merged(tracks: &Map<String, Value>, field: &str) -> Vec<Segment> {
    let labelled = tracks.len() > 1;
    let mut all: Vec<Segment> = tracks.iter().flat_map(|(t, v)| {
        segs_of(&v[field]).into_iter().map(move |mut s| {
            if labelled {
                s.text = format!("{}: {}", speaker(t), s.text);
            }
            s
        })
    }).collect();
    all.sort_by_key(|s| s.from_ms);
    all
}

fn render_live(m: &Value, live: &Value) -> String {
    let mut s = format!("# Live transcript\n\nRecording {} (started {}). Updated as the meeting goes; the finished transcript replaces it.\n\n",
        m["id"].as_str().unwrap_or(""), m["started_at"].as_i64().unwrap_or(0));
    let tracks = live["tracks"].as_object().cloned().unwrap_or_default();
    let segs = merged(&tracks, "segments");
    if segs.is_empty() {
        s.push_str("(nothing transcribed yet)\n");
    }
    for seg in segs {
        let sec = seg.from_ms / 1000;
        s.push_str(&format!("[{}:{:02}:{:02}] {}\n", sec / 3600, sec / 60 % 60, sec % 60, seg.text));
    }
    s
}

/// The live state of a session folder, or an empty one.
pub(crate) fn read_live(sdir: &Path) -> Value {
    read_json(&sdir.join(LIVE_FILE)).unwrap_or_else(|| json!({"tracks": {}}))
}

/// Chunks of one track, by seq: (seq, start_ms, dur_ms).
fn track_chunks(m: &Value, track: &str) -> Vec<(u64, i64, i64)> {
    let mut v: Vec<(u64, i64, i64)> = m["chunks"][track].as_object().map(|o| o.iter().filter_map(|(k, c)| {
        Some((k.parse().ok()?, c["start_ms"].as_i64()?, c["dur_ms"].as_i64().unwrap_or(0)))
    }).collect()).unwrap_or_default();
    v.sort_by_key(|c| c.0);
    v
}

/// One live pass over every open session in `dir`. Returns windows transcribed.
pub(crate) fn live_tick_in(dir: &Path, eng: &ClipEngine, now: i64, budget: Duration) -> usize {
    let t0 = Instant::now();
    let root = dir.join(".sessions");
    let Ok(rd) = std::fs::read_dir(&root) else { return 0 };
    let mut done = 0;
    for e in rd.flatten() {
        if t0.elapsed() > budget {
            break;
        }
        let sdir = e.path();
        let Some(m) = super::recording_sessions::read_manifest_pub(&sdir) else { continue };
        if m["state"] != "open" {
            continue;
        }
        let tracks: Vec<String> = m["tracks"].as_array().cloned().unwrap_or_default().iter()
            .filter_map(|t| t.as_str().map(str::to_string)).collect();
        let started = m["started_at"].as_i64().unwrap_or(0);
        let mut live = read_live(&sdir);
        let mut changed = false;
        for t in &tracks {
            let st = live["tracks"][t].clone();
            if st["retry_at"].as_i64().is_some_and(|r| r > now) {
                continue;
            }
            let next = st["next_seq"].as_u64().unwrap_or(0);
            let chunks = track_chunks(&m, t);
            // The contiguous run from `next`: a hole stops it until filled.
            let mut run: Vec<(u64, i64, i64)> = Vec::new();
            let mut total = 0;
            for (want, c) in (next..).zip(chunks.iter().filter(|c| c.0 >= next)) {
                if c.0 != want || total >= LIVE_MAX_MS {
                    break;
                }
                total += c.2.max(0);
                run.push(*c);
            }
            let Some(last) = run.last().copied() else { continue };
            let last_end = last.1 + last.2;
            if total < LIVE_WINDOW_MS && now - last_end < LIVE_IDLE_MS {
                continue;
            }
            let work = match tempfile::tempdir() { Ok(w) => w, Err(_) => continue };
            let (input, from_s, offset_ms) = if m["chunk_kind"] == "segment" {
                // Independently decodable pieces: the window is their bytes, joined.
                let p = work.path().join("window.aac");
                let mut buf = Vec::new();
                for c in &run {
                    if let Ok(b) = std::fs::read(sdir.join(t).join(format!("{:08}.chunk", c.0))) { buf.extend(b); }
                }
                if std::fs::write(&p, &buf).is_err() { continue; }
                (p, 0.0, run[0].1 - started)
            } else {
                // One continuous stream: only the first fragment has the header,
                // so the window is cut out of the whole prefix.
                let p = work.path().join(format!("prefix.{}", if m["mime"].as_str().unwrap_or("").contains("mp4") { "mp4" } else { "webm" }));
                let mut buf = Vec::new();
                for c in chunks.iter().filter(|c| c.0 <= last.0) {
                    if let Ok(b) = std::fs::read(sdir.join(t).join(format!("{:08}.chunk", c.0))) { buf.extend(b); }
                }
                if std::fs::write(&p, &buf).is_err() { continue; }
                let from = (run[0].1 - started).max(0);
                (p, from as f64 / 1000.0, from)
            };
            let tw = Instant::now();
            match transcribe_clip(eng, &input, from_s, work.path()) {
                Ok(segs) => {
                    let mut all = live["tracks"][t]["segments"].as_array().cloned().unwrap_or_default();
                    let n = segs.len();
                    for s in segs {
                        all.push(seg_json(&Segment { from_ms: s.from_ms + offset_ms, to_ms: s.to_ms + offset_ms, text: s.text }));
                    }
                    live["tracks"][t] = json!({"next_seq": last.0 + 1, "through_ms": last_end - started, "segments": all,
                        "windows": st["windows"].as_i64().unwrap_or(0) + 1});
                    changed = true;
                    done += 1;
                    tracing::info!(id = m["id"].as_str().unwrap_or(""), track = %t, window_ms = total, segments = n,
                        secs = tw.elapsed().as_secs_f64(), measured = true, n_considered = run.len(),
                        verdict = "recording_live_transcribed", "recordings: live transcript advanced");
                }
                Err(e) => {
                    live["tracks"][t] = json!({"next_seq": next, "through_ms": st["through_ms"], "segments": st["segments"],
                        "windows": st["windows"], "error": e, "retry_at": now + LIVE_RETRY_MS});
                    changed = true;
                    tracing::warn!(id = m["id"].as_str().unwrap_or(""), track = %t, error = %live["tracks"][t]["error"],
                        measured = true, n_considered = run.len(), verdict = "recording_live_failed",
                        "recordings: a live window could not be transcribed; it is retried, and the finished transcript covers it either way");
                }
            }
        }
        if changed {
            live["updated_at"] = json!(now_ms());
            live["engine"] = json!(eng.name().0);
            if write_atomic(&sdir.join(LIVE_FILE), &serde_json::to_vec_pretty(&live).unwrap_or_default()).is_ok() {
                let _ = write_atomic(&sdir.join(LIVE_MD), render_live(&m, &live).as_bytes());
            }
        }
    }
    done
}

static LIVE_UNAVAILABLE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// The `recordings-live` job's tick.
pub async fn live_pending(state: AppState) {
    let Ok(dir) = recordings::folder_path(&state).await else { return };
    let store = state.store.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let (plan, opts) = match store.read() {
            Ok(c) => (Plan::read(&c), options(&c)),
            Err(_) => return 0,
        };
        if !opts.live {
            return 0;
        }
        match ClipEngine::from_plan(&plan) {
            Ok(eng) => {
                LIVE_UNAVAILABLE.lock().unwrap_or_else(|p| p.into_inner()).clear();
                live_tick_in(&dir, &eng, now_ms(), Duration::from_secs(30))
            }
            Err(why) => {
                // Said once per reason, not every tick: an open recording is
                // then transcribed only when it is finished.
                let mut last = LIVE_UNAVAILABLE.lock().unwrap_or_else(|p| p.into_inner());
                if *last != why {
                    tracing::warn!(why = %why, measured = true, n_considered = 0, verdict = "recording_live_unavailable",
                        "recordings: live transcription is on but nothing can transcribe here; recordings are transcribed when they finish, if at all");
                    *last = why;
                }
                0
            }
        }
    })
    .await;
}

/// What a filed recording carries from its session: the live transcript and
/// the notes typed during it.
pub(crate) fn session_extras(sdir: &Path) -> Map<String, Value> {
    let mut out = Map::new();
    // Workers tagged during the recording, and any @worker typed in the notes.
    let mut add: Vec<(String, String)> = read_json(&sdir.join(SEND_TO_FILE)).and_then(|v| v.as_array().cloned()).unwrap_or_default()
        .iter().filter_map(|w| Some((w["worker"].as_str()?.to_string(), w["instructions"].as_str().unwrap_or("").to_string()))).collect();
    if let Ok(n) = std::fs::read_to_string(sdir.join(NOTES_FILE)) {
        let names: Vec<String> = super::conversations::fleet_directory_all().into_iter().map(|f| f.0).collect();
        for w in mentioned_workers(&n, &names) {
            if !add.iter().any(|(x, _)| *x == w) { add.push((w, String::new())); }
        }
    }
    if !add.is_empty() {
        out.insert("send_to".into(), add_send_to(&Value::Null, &add));
    }
    let live = read_live(sdir);
    if live["tracks"].as_object().is_some_and(|t| !t.is_empty()) {
        out.insert("live".into(), live);
    }
    if let Ok(n) = std::fs::read_to_string(sdir.join(NOTES_FILE)) {
        if !n.trim().is_empty() {
            out.insert("user_notes".into(), json!(n));
        }
    }
    out
}

/// The finished transcript of a recording that was transcribed live: the
/// live pieces plus each track's untranscribed tail. None when it has none.
pub(crate) fn transcribe_with_live(dir: &Path, sidecar: &Value, eng: &ClipEngine) -> Option<Result<Value, String>> {
    let live_tracks = sidecar["live"]["tracks"].as_object()?.clone();
    if !live_tracks.values().any(|v| v["segments"].as_array().is_some_and(|a| !a.is_empty()) || v["through_ms"].as_i64().unwrap_or(0) > 0) {
        return None;
    }
    let t0 = Instant::now();
    let dur = sidecar["dur_ms"].as_i64().unwrap_or(0);
    let track_files = sidecar["session"]["track_files"].as_object().cloned().unwrap_or_default();
    let mut tracks = live_tracks.clone();
    let mut tails = 0;
    for (t, v) in live_tracks.iter() {
        let through = v["through_ms"].as_i64().unwrap_or(0);
        if dur - through <= 1000 {
            continue;
        }
        let file = track_files.get(t).and_then(Value::as_str).or(sidecar["file"].as_str());
        let Some(file) = file else { continue };
        let work = match tempfile::tempdir() { Ok(w) => w, Err(e) => return Some(Err(e.to_string())) };
        match transcribe_clip(eng, &dir.join(file), through as f64 / 1000.0, work.path()) {
            Ok(segs) => {
                let mut all = v["segments"].as_array().cloned().unwrap_or_default();
                all.extend(segs.iter().map(|s| seg_json(&Segment { from_ms: s.from_ms + through, to_ms: s.to_ms + through, text: s.text.clone() })));
                tracks.insert(t.clone(), json!({"segments": all, "through_ms": dur}));
                tails += 1;
            }
            Err(e) => return Some(Err(format!("the {t} tail after the live transcript: {e}"))),
        }
    }
    let segs = merged(&tracks, "segments");
    let (engine, model) = eng.name();
    let t = recordings::write_transcript_pub(dir, sidecar, &segs, engine, &model, Value::Null, t0);
    if t.is_ok() {
        tracing::info!(id = sidecar["id"].as_str().unwrap_or(""), segments = segs.len(), tails, measured = true,
            n_considered = live_tracks.len(), verdict = "recording_transcript_from_live",
            "recordings: the finished transcript is the live pieces plus the untranscribed tail");
    }
    Some(t.map(|mut v| { v["live"] = json!(true); v }))
}

// ---- routes for the Record tab -----------------------------------------------------

async fn get_live(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    let dir = match recordings::folder(&state).await { Ok(d) => d, Err(r) => return r };
    let sdir = super::recording_sessions::session_dir(&dir, &id);
    if sdir.is_dir() {
        let live = read_live(&sdir);
        let tracks = live["tracks"].as_object().cloned().unwrap_or_default();
        let segs: Vec<Value> = merged(&tracks, "segments").iter().map(seg_json).collect();
        let errors: Vec<Value> = tracks.iter().filter_map(|(t, v)| v["error"].as_str().map(|e| json!({"track": t, "error": e}))).collect();
        return Json(json!({"id": id, "state": "recording", "segments": segs, "errors": errors,
            "through_ms": tracks.values().filter_map(|v| v["through_ms"].as_i64()).min(), "updated_at": live["updated_at"],
            "measured": true, "n_considered": segs.len()})).into_response();
    }
    // Filed already: the finished transcript if there is one, else what was live.
    match recordings::find_sidecar_pub(&dir, &id) {
        Some((_, v)) => {
            let tracks = v["live"]["tracks"].as_object().cloned().unwrap_or_default();
            let segs: Vec<Value> = merged(&tracks, "segments").iter().map(seg_json).collect();
            Json(json!({"id": id, "state": "filed", "segments": segs, "transcript": v["transcript"], "measured": true,
                "n_considered": segs.len()})).into_response()
        }
        None => err(StatusCode::NOT_FOUND, "no recording or session with that id"),
    }
}

/// The notes typed in the Record tab during a meeting; replaced whole.
async fn put_notes(State(state): State<AppState>, AxPath(id): AxPath<String>, body: axum::body::Bytes) -> Response {
    if body.len() > MAX_USER_NOTES {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "notes are at most 200 KB");
    }
    let dir = match recordings::folder(&state).await { Ok(d) => d, Err(r) => return r };
    let sdir = super::recording_sessions::session_dir(&dir, &id);
    if !sdir.join("manifest.json").is_file() {
        // Filed already: the notes go on the sidecar, and owed notes are rewritten with them.
        let text = String::from_utf8_lossy(&body).to_string();
        return match recordings::find_sidecar_pub(&dir, &id) {
            Some((p, _)) => match recordings::update_sidecar(&p, |v| {
                v["user_notes"] = json!(text);
                if v["notes"]["status"] == "done" { v["notes"]["stale"] = json!(true); }
            }) {
                Ok(_) => Json(json!({"ok": true, "id": id, "on": "recording"})).into_response(),
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            },
            None => err(StatusCode::NOT_FOUND, "no recording or session with that id"),
        };
    }
    match write_atomic(&sdir.join(NOTES_FILE), &body) {
        Ok(()) => Json(json!({"ok": true, "id": id, "on": "session", "bytes": body.len()})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn parse_workers(b: &Value) -> Result<Vec<(String, String)>, String> {
    let names: Vec<String> = super::conversations::fleet_directory_all().into_iter().map(|f| f.0).collect();
    let mut out = Vec::new();
    for w in b["workers"].as_array().cloned().unwrap_or_default() {
        let (n, ins) = match &w {
            Value::String(n) => (n.clone(), String::new()),
            o => (o["worker"].as_str().unwrap_or("").to_string(), o["instructions"].as_str().unwrap_or("").to_string()),
        };
        let n = n.trim().trim_start_matches('@').to_string();
        if !names.contains(&n) {
            return Err(format!("no worker named {n}"));
        }
        out.push((n, ins.chars().take(4000).collect()));
    }
    if out.is_empty() {
        return Err("name at least one worker".into());
    }
    Ok(out)
}

/// Tag workers on a recording in progress: they receive it once it is transcribed.
async fn put_session_send_to(State(state): State<AppState>, AxPath(id): AxPath<String>, Json(b): Json<Value>) -> Response {
    let dir = match recordings::folder(&state).await { Ok(d) => d, Err(r) => return r };
    let sdir = super::recording_sessions::session_dir(&dir, &id);
    let list = if b["workers"].as_array().is_some_and(|a| a.is_empty()) { Ok(vec![]) } else { parse_workers(&b) };
    let list = match list { Ok(l) => l, Err(e) => return err(StatusCode::BAD_REQUEST, e) };
    if !sdir.join("manifest.json").is_file() {
        return err(StatusCode::NOT_FOUND, "no recording in progress with that id; use POST /api/recordings/<id>/send");
    }
    let v = json!(list.iter().map(|(w, i)| json!({"worker": w, "instructions": i})).collect::<Vec<_>>());
    match write_atomic(&sdir.join(SEND_TO_FILE), &serde_json::to_vec(&v).unwrap_or_default()) {
        Ok(()) => Json(json!({"ok": true, "id": id, "send_to": v})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Send a filed recording to workers (now, or once it is transcribed).
async fn send_now(State(state): State<AppState>, AxPath(id): AxPath<String>, Json(b): Json<Value>) -> Response {
    let list = match parse_workers(&b) { Ok(l) => l, Err(e) => return err(StatusCode::BAD_REQUEST, e) };
    let dir = match recordings::folder(&state).await { Ok(d) => d, Err(r) => return r };
    let Some((p, _)) = recordings::find_sidecar_pub(&dir, &id) else { return err(StatusCode::NOT_FOUND, "no recording with that id") };
    if let Err(e) = recordings::update_sidecar(&p, |s| { s["send_to"] = add_send_to(&s["send_to"], &list); }) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    send_pass(&state, &dir).await;
    match recordings::find_sidecar_pub(&dir, &id) {
        Some((_, v)) => Json(json!({"ok": true, "send_to": v["send_to"],
            "waiting": v["transcript"]["status"] != "done"})).into_response(),
        None => err(StatusCode::NOT_FOUND, "the recording went away"),
    }
}

/// Rename a recording, or tick an action item.
async fn edit_recording(State(state): State<AppState>, AxPath(id): AxPath<String>, Json(b): Json<Value>) -> Response {
    let dir = match recordings::folder(&state).await { Ok(d) => d, Err(r) => return r };
    let Some((p, _)) = recordings::find_sidecar_pub(&dir, &id) else { return err(StatusCode::NOT_FOUND, "no recording with that id") };
    let title = b["title"].as_str().map(|t| t.trim().chars().take(200).collect::<String>());
    let done = b["action_done"].as_object().and_then(|o| Some((o.get("index")?.as_u64()? as usize, o.get("done")?.as_bool()?)));
    if title.is_none() && done.is_none() {
        return err(StatusCode::BAD_REQUEST, "send title, or action_done {index, done}");
    }
    match recordings::update_sidecar(&p, |s| {
        if let Some(t) = &title { s["title"] = json!(t); }
        if let Some((i, d)) = done { if s["notes"]["action_items"][i].is_object() { s["notes"]["action_items"][i]["done"] = json!(d); } }
    }) {
        Ok(v) => Json(json!({"ok": true, "title": v["title"], "notes": v["notes"]})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Write (or rewrite) a recording's notes now, whatever the option says.
async fn write_notes_now(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    let dir = match recordings::folder(&state).await { Ok(d) => d, Err(r) => return r };
    let Some((p, v)) = recordings::find_sidecar_pub(&dir, &id) else { return err(StatusCode::NOT_FOUND, "no recording with that id") };
    if v["transcript"]["status"] != "done" {
        return err(StatusCode::CONFLICT, "the transcript is not done yet; notes follow it");
    }
    let organize = state.store.read().map(|c| options(&c).organize).unwrap_or(false);
    let _ = recordings::update_sidecar(&p, |s| { s["notes"] = json!({"status": "pending", "organize": organize, "asked": true}); });
    notes_pass(&state, &dir).await;
    match recordings::find_sidecar_pub(&dir, &id) {
        Some((_, v)) => Json(json!({"ok": v["notes"]["status"] == "done", "notes": v["notes"], "about": v["about"]})).into_response(),
        None => err(StatusCode::NOT_FOUND, "the recording went away"),
    }
}

// ---- notes, understanding, the calendar, sending to workers ----------------------

/// Called when a transcript finishes: notes are owed when an option asks, and
/// a recording somebody tagged a worker on is owed to that worker.
pub(crate) fn owe_notes(sidecar: &mut Value, opts: Options) {
    if (opts.notes || opts.organize) && sidecar["notes"].is_null() {
        sidecar["notes"] = json!({"status": "pending", "organize": opts.organize, "write": opts.notes});
    }
}

fn notes_owed(v: &Value) -> bool {
    match v["notes"]["status"].as_str() {
        Some("pending") => true,
        Some("failed") => v["notes"]["attempts"].as_i64().unwrap_or(0) < MAX_NOTES_ATTEMPTS,
        _ => false,
    }
}

fn transcript_segments(dir: &Path, v: &Value) -> Vec<Segment> {
    v["transcript"]["segments_file"].as_str()
        .and_then(|f| read_json(&dir.join(f)))
        .map(|j| segs_of(&j["segments"]))
        .unwrap_or_default()
}

fn transcript_text(dir: &Path, v: &Value) -> String {
    transcript_segments(dir, v).iter().map(|s| {
        let sec = s.from_ms / 1000;
        format!("[{}:{:02}] {}", sec / 60, sec % 60, s.text)
    }).collect::<Vec<_>>().join("\n")
}

/// `@name` words in the typed notes that name a worker in `names`.
pub(crate) fn mentioned_workers(text: &str, names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in text.split(|c: char| c.is_whitespace() || ",;()".contains(c)) {
        if let Some(n) = w.strip_prefix('@') {
            let n = n.trim_end_matches(|c: char| ".:!?".contains(c));
            if names.iter().any(|x| x == n) && !out.iter().any(|x| x == n) {
                out.push(n.to_string());
            }
        }
    }
    out
}

fn prompt_for(transcript: &str, user_notes: &str, meeting: &Value, fleet: &[(String, Vec<String>, String)]) -> String {
    let mut p = String::from(
        "You turn a meeting recording into notes. Below is the transcript (lines may start with You: or Them:, \
         the two sides of a call), the calendar event it matched if any, and the notes the person typed during \
         it. Write notes the way a careful colleague would: what the meeting was about, what was decided, who \
         will do what. Use the typed notes as the spine and fill them in from the transcript; never invent \
         anything the transcript does not say.\n\nAnswer with ONE JSON object and nothing else:\n\
         {\"title\": \"<a short title for the meeting>\", \"summary\": \"<two to four sentences>\", \
         \"key_points\": [\"...\"], \"notes_md\": \"<the notes, markdown, with headings>\", \"decisions\": [\"...\"], \
         \"action_items\": [{\"text\": \"...\", \"owner\": \"<a person's name, or empty>\"}], \
         \"workers\": [\"<worker name>\"], \"groups\": [\"<group name>\"], \"why\": \"<one sentence>\"}\n\n\
         For workers and groups, choose ONLY names from the fleet list below, and only those this meeting is \
         actually about (they are suggested to the person, who decides); an empty list is a fine answer.\n\n\
         Fleet (name | groups | what it does):\n",
    );
    for (n, g, d) in fleet {
        p.push_str(&format!("- {n} | {} | {}\n", g.join(","), d.chars().take(200).collect::<String>()));
    }
    if meeting.is_object() {
        p.push_str(&format!("\nCalendar event: {}\n", meeting));
    }
    p.push_str("\nTyped notes:\n");
    p.push_str(if user_notes.trim().is_empty() { "(none)" } else { user_notes });
    p.push_str("\n\nTranscript:\n");
    p.push_str(transcript);
    p
}

/// The first JSON object in a model's answer.
fn parse_answer(s: &str) -> Option<Value> {
    let a = s.find('{')?;
    let b = s.rfind('}')?;
    serde_json::from_str(&s[a..=b]).ok()
}

/// Action items as {text, owner, done}, from either shape a model answers in.
fn action_items(a: &Value) -> Vec<Value> {
    a["action_items"].as_array().map(|x| x.iter().filter_map(|i| match i {
        Value::String(t) => Some(json!({"text": t, "owner": "", "done": false})),
        Value::Object(o) => o.get("text").and_then(Value::as_str).map(|t| json!({"text": t,
            "owner": o.get("owner").and_then(Value::as_str).unwrap_or(""), "done": false})),
        _ => None,
    }).collect()).unwrap_or_default()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|x| x.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

fn notes_md(sidecar: &Value, title: &str, a: &Value, items: &[Value], user_notes: &str) -> String {
    let mut s = format!("# {title}\n\n");
    s.push_str(&format!("Recorded {} · {}\n", sidecar["recorded_at_local"].as_str().unwrap_or(""), sidecar["file"].as_str().unwrap_or("")));
    let people = strings(&sidecar["meeting"]["attendees"]);
    if !people.is_empty() {
        s.push_str(&format!("Participants: {}\n", people.join(", ")));
    }
    s.push('\n');
    if let Some(sum) = a["summary"].as_str() { s.push_str(&format!("{sum}\n\n")); }
    let kp = strings(&a["key_points"]);
    if !kp.is_empty() {
        s.push_str("## Key points\n\n");
        for k in kp { s.push_str(&format!("- {k}\n")); }
        s.push('\n');
    }
    if let Some(n) = a["notes_md"].as_str() { s.push_str(n.trim()); s.push_str("\n\n"); }
    let dec = strings(&a["decisions"]);
    if !dec.is_empty() {
        s.push_str("## Decisions\n\n");
        for d in dec { s.push_str(&format!("- {d}\n")); }
        s.push('\n');
    }
    if !items.is_empty() {
        s.push_str("## Action items\n\n");
        for i in items {
            let o = i["owner"].as_str().unwrap_or("");
            s.push_str(&format!("- [ ] {}{}\n", i["text"].as_str().unwrap_or(""), if o.is_empty() { String::new() } else { format!(" ({o})") }));
        }
        s.push('\n');
    }
    if !user_notes.trim().is_empty() {
        s.push_str("## Your notes, as typed\n\n");
        s.push_str(user_notes.trim());
        s.push('\n');
    }
    s
}

/// The calendar event a recording overlaps most: amux's own events and every
/// connected Google account. At least half the recording, or ten minutes, must
/// fall inside it.
pub(crate) async fn meeting_for(state: &AppState, start_ms: i64, end_ms: i64) -> Option<Value> {
    use chrono::{DateTime, TimeZone, Utc};
    let to_dt = |ms: i64| Utc.timestamp_millis_opt(ms).single();
    let (s, e) = (to_dt(start_ms - 30 * 60_000)?, to_dt(end_ms + 30 * 60_000)?);
    let parse = |t: &str| -> Option<i64> {
        DateTime::parse_from_rfc3339(t).map(|d| d.timestamp_millis()).ok()
            .or_else(|| chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S").ok()
                .or_else(|| chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M").ok())
                .and_then(|n| chrono::Local.from_local_datetime(&n).single()).map(|d| d.timestamp_millis()))
    };
    let mut cands: Vec<Value> = Vec::new();
    if let Ok(c) = state.store.read() {
        if let Ok(mut q) = c.prepare("SELECT id, title, start, end, location, description FROM cal_events WHERE deleted IS NULL AND all_day=0") {
            let rows = q.query_map([], |r| Ok(json!({"source": "amux", "id": r.get::<_, String>(0)?, "title": r.get::<_, String>(1)?,
                "start": r.get::<_, String>(2)?, "end": r.get::<_, Option<String>>(3)?, "location": r.get::<_, Option<String>>(4)?,
                "description": r.get::<_, Option<String>>(5)?, "attendees": []})));
            if let Ok(rows) = rows { cands.extend(rows.flatten()); }
        }
    }
    let (g, _) = super::gcal::events_between(state, &s.to_rfc3339(), &e.to_rfc3339()).await;
    for ev in g {
        if ev["all_day"] == true || ev["status"] == "cancelled" { continue; }
        let attendees: Vec<String> = ev["attendees"].as_array().map(|a| a.iter().filter_map(|x| {
            x.as_str().map(str::to_string).or_else(|| x["display_name"].as_str().or(x["email"].as_str()).map(str::to_string))
        }).collect()).unwrap_or_default();
        cands.push(json!({"source": "google", "id": ev["event_id"], "title": ev["title"], "start": ev["start_time"], "end": ev["end_time"],
            "location": ev["location"], "meeting_url": ev["meeting_url"], "organizer": ev["organizer"], "attendees": attendees,
            "calendar": ev["calendar_name"], "html_link": ev["html_link"]}));
    }
    let dur = (end_ms - start_ms).max(1);
    let mut best: Option<(i64, Value)> = None;
    for c in cands {
        let (Some(cs), Some(ce)) = (c["start"].as_str().and_then(parse), c["end"].as_str().and_then(parse)) else { continue };
        let overlap = end_ms.min(ce) - start_ms.max(cs);
        if overlap <= 0 || (overlap * 2 < dur && overlap < 10 * 60_000) { continue; }
        if best.as_ref().is_none_or(|(o, _)| overlap > *o) {
            best = Some((overlap, c));
        }
    }
    best.map(|(_, c)| c)
}

/// Write the owed notes of every recording in `dir`: the calendar event, the
/// notes, the suggested workers, and (organize on) the group channel posts.
pub async fn notes_pass(state: &AppState, dir: &Path) -> usize {
    let d = dir.to_path_buf();
    let owed: Vec<(PathBuf, Value)> = tokio::task::spawn_blocking(move || {
        recordings::read_sidecars(&d).into_iter().filter(|(_, v)| v["transcript"]["status"] == "done" && notes_owed(v)).collect()
    }).await.unwrap_or_default();
    let mut done = 0;
    if !owed.is_empty() {
        let fleet = super::conversations::fleet_directory();
        for (path, v) in owed {
            if write_notes_one(state, dir, &path, &v, &fleet).await {
                done += 1;
            }
        }
    }
    send_pass(state, dir).await;
    done
}

async fn write_notes_one(state: &AppState, dir: &Path, path: &Path, v: &Value, fleet: &[(String, Vec<String>, String)]) -> bool {
    let id = v["id"].as_str().unwrap_or("").to_string();
    let attempts = v["notes"]["attempts"].as_i64().unwrap_or(0) + 1;
    let organize = v["notes"]["organize"].as_bool().unwrap_or(false);
    let user_notes = v["user_notes"].as_str().unwrap_or("").to_string();
    let transcript = transcript_text(dir, v);
    // The calendar first: its title and people name the meeting in the notes.
    let start = v["started_at"].as_i64().unwrap_or(0);
    let meeting = if v["meeting"].is_object() { v["meeting"].clone() } else {
        meeting_for(state, start, start + v["dur_ms"].as_i64().unwrap_or(0)).await.unwrap_or(Value::Null)
    };
    if transcript.trim().is_empty() && user_notes.trim().is_empty() {
        let _ = recordings::update_sidecar(path, |s| {
            s["notes"] = json!({"status": "skipped", "why": "nothing was said and nothing was typed"});
            if meeting.is_object() { s["meeting"] = meeting.clone(); }
        });
        return false;
    }
    let t0 = Instant::now();
    let answer = super::lookup::helper_answer(&prompt_for(&transcript, &user_notes, &meeting, fleet)).await;
    let parsed = match answer {
        Ok((model, text)) => parse_answer(&text).map(|a| (model, a)).ok_or_else(|| format!("the helper's answer was not JSON: {}", text.chars().take(200).collect::<String>())),
        Err((_, e)) => Err(e),
    };
    let (model, a) = match parsed {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!(id, attempts, error = %e, measured = true, n_considered = 1, verdict = "recording_notes_failed",
                "recordings: meeting notes could not be written; retried on the next pass");
            let _ = recordings::update_sidecar(path, |s| {
                s["notes"]["status"] = json!("failed"); s["notes"]["error"] = json!(e); s["notes"]["attempts"] = json!(attempts);
                if meeting.is_object() { s["meeting"] = meeting.clone(); }
            });
            return false;
        }
    };
    let title = v["title"].as_str().filter(|t| !t.is_empty()).map(str::to_string)
        .or_else(|| meeting["title"].as_str().filter(|t| !t.is_empty()).map(str::to_string))
        .or_else(|| a["title"].as_str().map(str::to_string)).unwrap_or_else(|| "Meeting".into());
    let items = action_items(&a);
    let mut sc = v.clone();
    if meeting.is_object() { sc["meeting"] = meeting.clone(); }
    let base = v["file"].as_str().and_then(|f| f.rsplit_once('.')).map(|(b, _)| b.to_string()).unwrap_or_else(|| id.clone());
    let md_name = format!("{base}.notes.md");
    if let Err(e) = write_atomic(&dir.join(&md_name), notes_md(&sc, &title, &a, &items, &user_notes).as_bytes()) {
        let _ = recordings::update_sidecar(path, |s| { s["notes"]["status"] = json!("failed"); s["notes"]["error"] = json!(e.to_string()); s["notes"]["attempts"] = json!(attempts); });
        return false;
    }
    // Only real names survive: a model's guess at a worker that does not exist files nothing.
    let names: Vec<&str> = fleet.iter().map(|f| f.0.as_str()).collect();
    let groups: Vec<String> = fleet.iter().flat_map(|f| f.1.clone()).collect();
    let workers: Vec<String> = strings(&a["workers"]).into_iter().filter(|w| names.contains(&w.as_str())).collect();
    let mut gs: Vec<String> = strings(&a["groups"]).into_iter().map(|g| g.to_lowercase()).filter(|g| groups.contains(g)).collect();
    gs.sort();
    gs.dedup();
    let summary = a["summary"].as_str().unwrap_or("").to_string();
    let about = json!({"title": title, "summary": summary, "workers": workers, "groups": gs, "why": a["why"], "filed": organize});
    let _ = recordings::update_sidecar(path, |s| {
        s["notes"] = json!({"status": "done", "md": md_name, "title": title, "summary": summary, "key_points": strings(&a["key_points"]),
            "decisions": strings(&a["decisions"]), "action_items": items, "model": model, "attempts": attempts,
            "secs": (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0, "finished_at": now_ms()});
        s["about"] = about.clone();
        if meeting.is_object() { s["meeting"] = meeting.clone(); }
    });
    tracing::info!(id, model = %model, calendar = meeting.is_object(), secs = t0.elapsed().as_secs_f64(), measured = true, n_considered = 1,
        verdict = "recording_notes_written", "recordings: meeting notes written");
    if organize && (!workers.is_empty() || !gs.is_empty()) {
        let mut posted = 0;
        for g in &gs {
            let body = format!("Meeting filed here: **{title}**\n\n{summary}\n\nWorkers it concerns: {}. Notes: {md_name}",
                if workers.is_empty() { "none named".to_string() } else { workers.join(", ") });
            if super::conversations::recorder_post(state, g, &body).await.is_ok() {
                posted += 1;
            }
        }
        tracing::info!(id, workers = ?workers, groups = ?gs, posted, measured = true, n_considered = fleet.len(),
            verdict = "recording_filed_with_workers", "recordings: a meeting was filed with the workers and groups it concerns");
    }
    true
}

/// What a tagged worker receives: what the meeting was and where its files
/// are, and to handle it as fits the worker's own role.
fn send_text(dir: &Path, v: &Value, instructions: &str) -> String {
    let abs = |f: Option<&str>| f.map(|f| dir.join(f).display().to_string());
    let title = v["title"].as_str().or(v["notes"]["title"].as_str()).or(v["meeting"]["title"].as_str()).unwrap_or("A recording");
    let dur = v["dur_ms"].as_i64().unwrap_or(0) / 1000;
    let mut s = format!("A recording for you, sent from the Record tab: {title}\n\n");
    s.push_str(&format!("When: {} ({}m {:02}s)\n", v["recorded_at_local"].as_str().unwrap_or(""), dur / 60, dur % 60));
    let people = strings(&v["meeting"]["attendees"]);
    s.push_str(&format!("Participants: {}\n", if people.is_empty() { "not known (no calendar event matched)".to_string() } else { people.join(", ") }));
    if let Some(m) = v["meeting"]["title"].as_str() {
        s.push_str(&format!("Calendar event: {m}{}\n", v["meeting"]["meeting_url"].as_str().map(|u| format!(" ({u})")).unwrap_or_default()));
    }
    if let Some(p) = abs(v["file"].as_str()) { s.push_str(&format!("Audio: {p}\n")); }
    for (t, f) in v["session"]["track_files"].as_object().cloned().unwrap_or_default() {
        if let Some(p) = abs(f.as_str()) { s.push_str(&format!("Audio, {} only: {p}\n", if t == "system" { "the other side" } else { "the microphone" })); }
    }
    if let Some(p) = abs(v["transcript"]["md"].as_str()) { s.push_str(&format!("Transcript: {p}\n")); }
    if let Some(p) = abs(v["notes"]["md"].as_str()) { s.push_str(&format!("Meeting notes: {p}\n")); }
    if let Some(sum) = v["notes"]["summary"].as_str() { s.push_str(&format!("\nSummary: {sum}\n")); }
    if !instructions.trim().is_empty() {
        s.push_str(&format!("\nThe owner's instructions for you: {}\n", instructions.trim()));
    }
    s.push_str("\nHandle it as fits your role: for example, catalogue the meeting in your own directory with its transcript, \
        date and participants, and follow up on anything in it that is yours. Reply here with what you did.");
    s
}

/// Deliver every recording that is owed to a worker: after its transcript,
/// and after its notes when notes are being written (not forever: a failed
/// or skipped note does not hold it).
pub(crate) async fn send_pass(state: &AppState, dir: &Path) -> usize {
    let d = dir.to_path_buf();
    let owed: Vec<(PathBuf, Value)> = tokio::task::spawn_blocking(move || {
        recordings::read_sidecars(&d).into_iter().filter(|(_, v)| {
            v["transcript"]["status"] == "done" && v["notes"]["status"] != "pending"
                && !(v["notes"]["status"] == "failed" && notes_owed(v))
                && v["send_to"].as_array().is_some_and(|a| a.iter().any(|w| w["state"].is_null()))
        }).collect()
    }).await.unwrap_or_default();
    let mut sent = 0;
    let routable: Vec<String> = super::conversations::fleet_directory().into_iter().map(|f| f.0).collect();
    for (path, v) in owed {
        let id = v["id"].as_str().unwrap_or("").to_string();
        let list = v["send_to"].as_array().cloned().unwrap_or_default();
        let mut out = Vec::new();
        for w in list {
            if !w["state"].is_null() { out.push(w); continue; }
            let name = w["worker"].as_str().unwrap_or("").to_string();
            let text = send_text(dir, &v, w["instructions"].as_str().unwrap_or(""));
            // A Messages worker gets it in its thread, as the owner (two-way: it
            // answers there); an isolated worker by a direct owner send.
            let res = if routable.contains(&name) {
                super::conversations::owner_post_to_worker(state, &name, &text).await
            } else {
                super::conversations::owner_send_direct(state, &name, &text).await
            };
            let (st, err) = match res { Ok(s) => (s, Value::Null), Err(e) => ("failed".to_string(), json!(e)) };
            if st == "failed" {
                tracing::warn!(id, worker = %name, error = %err, measured = true, n_considered = 1, verdict = "recording_send_failed",
                    "recordings: a recording could not be sent to the worker it was tagged for");
            } else {
                sent += 1;
                tracing::info!(id, worker = %name, state = %st, measured = true, n_considered = 1, verdict = "recording_sent_to_worker",
                    "recordings: a recording was sent to the worker it was tagged for");
            }
            let mut w2 = w.clone();
            w2["state"] = json!(st);
            w2["at"] = json!(now_ms());
            if !err.is_null() { w2["error"] = err; }
            out.push(w2);
        }
        let _ = recordings::update_sidecar(&path, |s| { s["send_to"] = json!(out); });
    }
    sent
}

/// Add workers to a recording's send list (existing entries keep their state).
pub(crate) fn add_send_to(list: &Value, add: &[(String, String)]) -> Value {
    let mut out: Vec<Value> = list.as_array().cloned().unwrap_or_default();
    for (w, ins) in add {
        match out.iter_mut().find(|x| x["worker"] == w.as_str()) {
            Some(x) if x["state"].is_null() => { if !ins.is_empty() { x["instructions"] = json!(ins); } }
            Some(x) => { // sent already: a second send is a new entry
                if !ins.is_empty() && x["instructions"] != json!(ins) {
                    out.push(json!({"worker": w, "instructions": ins}));
                }
            }
            None => out.push(json!({"worker": w, "instructions": ins})),
        }
    }
    json!(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_tracks_merge_on_one_timeline_labelled_by_side() {
        let tracks = json!({
            "mic": {"segments": [{"from_ms": 0, "to_ms": 2000, "text": "hello"}, {"from_ms": 9000, "to_ms": 9500, "text": "bye"}]},
            "system": {"segments": [{"from_ms": 4000, "to_ms": 6000, "text": "hi there"}]},
        });
        let m = merged(tracks.as_object().unwrap(), "segments");
        let texts: Vec<&str> = m.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["You: hello", "Them: hi there", "You: bye"]);
        let one = json!({"mic": {"segments": [{"from_ms": 0, "to_ms": 1, "text": "solo"}]}});
        assert_eq!(merged(one.as_object().unwrap(), "segments")[0].text, "solo", "one track has no labels");
    }

    #[test]
    fn a_model_answer_is_read_from_its_json_even_with_words_around_it() {
        let a = parse_answer("Sure! ```json\n{\"title\": \"Q4 plan\", \"workers\": [\"amux\"]}\n```").unwrap();
        assert_eq!(a["title"], "Q4 plan");
        assert!(parse_answer("no json here").is_none());
    }

    #[test]
    fn notes_are_owed_only_when_an_option_asks_and_only_once() {
        let mut v = json!({});
        owe_notes(&mut v, Options { live: true, notes: false, organize: false });
        assert!(v["notes"].is_null(), "both options off: nothing is owed");
        owe_notes(&mut v, Options { live: true, notes: true, organize: false });
        assert!(notes_owed(&v));
        v["notes"]["status"] = json!("done");
        owe_notes(&mut v, Options { live: true, notes: true, organize: true });
        assert_eq!(v["notes"]["status"], "done", "done notes are not owed again");
        assert!(!notes_owed(&v));
    }

    #[test]
    fn the_prompt_lists_only_the_fleet_and_says_to_choose_from_it() {
        let fleet = vec![("amux".to_string(), vec!["amux".to_string()], "Builds amux".to_string())];
        let p = prompt_for("[0:00] You: we need the board fixed", "", &json!({"title": "Sync", "attendees": ["Ann"]}), &fleet);
        assert!(p.contains("- amux | amux | Builds amux") && p.contains("choose ONLY names from the fleet list"));
        assert!(p.contains("Calendar event:") && p.contains("Ann"));
        assert!(!prompt_for("x", "", &Value::Null, &fleet).contains("Calendar event:"));
    }

    #[test]
    fn at_names_in_typed_notes_pick_real_workers_only_once() {
        let names = vec!["mxp-presales".to_string(), "amux".to_string()];
        assert_eq!(mentioned_workers("send to @mxp-presales, and @mxp-presales. also @nobody @amux:", &names), ["mxp-presales", "amux"]);
        assert!(mentioned_workers("email me at a@b.com", &names).is_empty());
    }

    #[test]
    fn a_send_list_keeps_sent_entries_and_adds_new_ones() {
        let l = add_send_to(&Value::Null, &[("a".into(), "catalogue it".into())]);
        assert_eq!(l[0]["instructions"], "catalogue it");
        let mut sent = l.clone();
        sent[0]["state"] = json!("posted");
        let again = add_send_to(&sent, &[("a".into(), String::new()), ("b".into(), String::new())]);
        let arr = again.as_array().unwrap();
        assert_eq!(arr.len(), 2, "a repeat with no new instructions is not sent twice: {again}");
        assert_eq!(arr[0]["state"], "posted");
    }

    #[test]
    fn action_items_read_in_either_shape() {
        let a = json!({"action_items": ["call Ann", {"text": "ship it", "owner": "Ethan"}, 3]});
        let i = action_items(&a);
        assert_eq!(i.len(), 2);
        assert_eq!(i[1]["owner"], "Ethan");
        assert_eq!(i[0]["done"], false);
    }
}
