//! Chunked file upload API — the dashboard's peek drag-and-drop and attach
//! button hit this protocol (app.js `uploadAndAttach`):
//!
//!   POST /api/upload/start        → {id, chunks}
//!   PUT  /api/upload/:id/chunk/:n → {ok: true, chunk: n}
//!   POST /api/upload/:id/finish   → {path, name, url}
//!
//! Replaces the Python proxy (py_proxy → amux-server.py:68577) with a native
//! Rust implementation. Uploads land in `$AMUX_HOME/uploads/` (same directory
//! Python used) so the served `/api/uploads/:name` path stays the same.

use super::AppState;
use axum::body::Bytes;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const STALE_SECS: u64 = 3600;
const PURGE_AGE_SECS: u64 = 86400;
/// How many swept dirs the sweep names individually. The aggregate counts stay
/// authoritative and `elided` says how many were not named, so a large backlog
/// cannot flood the log while the claim stays auditable (the AF-179 shape).
const SWEEP_LOG_DIRS: usize = 12;

struct InFlight {
    filename: String,
    chunks: usize,
    received: BTreeSet<usize>,
    tmpdir: PathBuf,
    ts: u64,
}

type UploadState = Arc<Mutex<std::collections::HashMap<String, InFlight>>>;

fn uploads_dir() -> PathBuf {
    let home = std::env::var("AMUX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".amux")
        });
    home.join("uploads")
}

/// Remove `.chunked-*` staging dirs that no in-flight upload claims and that
/// are older than `STALE_SECS`.
///
/// Keyed on the DIRECTORY NAME rather than on `tmpdir`, so a dir belonging to a
/// live upload is never removed even if `uploads_dir()` changed under us. The
/// age gate is what keeps this safe against a concurrent `start()` on another
/// worker: a dir created seconds ago is never swept, whether or not this
/// process's map happens to hold it yet.
///
/// `dir` and `max_age` are parameters rather than reads of `uploads_dir()` and
/// `STALE_SECS` so the two rules can be discriminated in a test without
/// backdating an mtime, which std cannot portably do. A test that could only
/// drive this through the globals would be reduced to asserting "nothing was
/// deleted", which passes just as well when the function does nothing at all.
fn sweep_orphan_chunk_dirs(
    dir: &std::path::Path,
    live: &std::collections::HashMap<String, InFlight>,
    max_age: std::time::Duration,
    dry_run: bool,
) -> Vec<SweptDir> {
    let cutoff = max_age;
    let mut n: Vec<SweptDir> = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return n };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        let Some(uid) = name.strip_prefix(".chunked-") else { continue };
        if live.contains_key(uid) {
            continue;
        }
        if !ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let age = ent
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().unwrap_or_default())
            .unwrap_or_default();
        if age <= cutoff {
            continue;
        }
        // Measure BEFORE deleting, and record it before the bytes are gone.
        let (chunks, bytes) = std::fs::read_dir(ent.path())
            .map(|rd| {
                rd.flatten().fold((0u64, 0u64), |(c, b), f| {
                    (c + 1, b + f.metadata().map(|m| m.len()).unwrap_or(0))
                })
            })
            .unwrap_or((0, 0));
        let swept = SweptDir { name: name.clone(), age_s: age.as_secs(), chunks, bytes };
        // A dry run records without deleting; a real run records only what it
        // actually managed to remove, so the log never claims a dir that is
        // still on disk.
        if !dry_run && std::fs::remove_dir_all(ent.path()).is_err() {
            continue;
        }
        n.push(swept);
    }
    n
}

/// What one swept staging dir WAS, captured before it is removed.
///
/// A COUNT IS NOT AN AUDIT TRAIL (AF-238, caught by the amux lane reviewing
/// AF-235). The first version of this logged only how many dirs went, which
/// makes the sweep unauditable AFTER the fact as well as before: `filename`
/// lives only in the in-memory map, so nobody can say what a reaped dir held —
/// and with a bare count, nobody can even say which ones they were.
///
/// This is the identical defect AF-179 fixed in the observed-edits hook
/// ("LOG WHAT WAS CLAIMED, NOT ONLY HOW MANY"), made again one file over by the
/// same author who fixed it there. Ethos rule 7's note that verification habits
/// do not transfer between operands, demonstrated rather than read.
#[derive(Debug, Clone)]
struct SweptDir {
    name: String,
    age_s: u64,
    chunks: u64,
    bytes: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

pub fn routes() -> Router<AppState> {
    let state: UploadState = Arc::new(Mutex::new(std::collections::HashMap::new()));
    Router::new()
        .route("/start", post({
            let s = state.clone();
            move |body| start(s, body)
        }))
        .route("/{id}/chunk/{n}", put({
            let s = state.clone();
            move |path, body| chunk(s, path, body)
        }).layer(
            // The SPA uploads 5MB chunks (app.js:8459 CHUNK_SIZE); axum's
            // default 2MB body cap 413'd EVERY chunk 0 after the client had
            // already streamed it (27s wasted per attempt — live incident
            // 2026-08-09, found via /api/logs/analyze in one call). 8MB =
            // chunk + protocol slack; anything larger is a client bug and
            // the 413 is then honest.
            axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024),
        ))
        .route("/{id}/finish", post({
            let s = state.clone();
            move |path| finish(s, path)
        }))
}

/// Serve uploaded files at `/api/uploads/:filename`.
pub fn serve_routes() -> Router<AppState> {
    Router::new().route("/{filename}", get(serve_uploaded))
}

#[derive(Deserialize)]
struct StartReq {
    name: Option<String>,
    #[serde(default = "default_size")]
    #[allow(dead_code)]
    size: u64,
    #[serde(default = "default_chunks")]
    chunks: usize,
}

fn default_size() -> u64 { 0 }
fn default_chunks() -> usize { 1 }

async fn start(state: UploadState, Json(body): Json<StartReq>) -> Response {
    let raw_name = body.name.unwrap_or_else(|| "upload".into());
    let filename: String = raw_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>()
        .chars()
        .take(120)
        .collect();
    let total_chunks = body.chunks.max(1);
    let uid = format!("{:012x}", rand_id());

    let dir = uploads_dir().join(format!(".chunked-{uid}"));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()}));
    }

    let mut map = state.lock().unwrap();
    // Purge stale uploads
    let cutoff = now_secs().saturating_sub(STALE_SECS);
    let stale: Vec<String> = map.iter()
        .filter(|(_, v)| v.ts < cutoff)
        .map(|(k, _)| k.clone())
        .collect();
    for k in stale {
        if let Some(entry) = map.remove(&k) {
            let _ = std::fs::remove_dir_all(&entry.tmpdir);
        }
    }
    // ...AND THE ONES THE MAP CANNOT ACCOUNT FOR (AF-235). The purge above
    // iterates the IN-MEMORY map, so it can only ever free directories this
    // process still knows about. The map does not survive a restart — and this
    // binary self-adopts on every commit that touches crates/ — so each restart
    // orphaned its tmpdirs with no record left to purge them by. They then sat
    // in ~/.amux/uploads forever, invisible to the only cleanup path there was.
    //
    // Four EMPTY `.chunked-*` dirs are what let @Dygreens prove the client-side
    // half of this bug on #124: /api/upload/start had succeeded and chunk 0
    // never wrote. The evidence was only readable because nothing had cleaned
    // it up, which is the one upside of a leak, and not a reason to keep it.
    // DRY RUN: count and describe, delete nothing. There is no undo for this and
    // no way to say afterwards what a reaped dir held, so the operator gets a way
    // to LOOK first. Set AMUX_UPLOAD_SWEEP_DRYRUN=1 in ~/.amux/server.env and
    // restart; every line below is emitted with `dry_run=true` and the bytes stay
    // where they are.
    let dry_run = std::env::var("AMUX_UPLOAD_SWEEP_DRYRUN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let swept = sweep_orphan_chunk_dirs(
        &uploads_dir(),
        &map,
        std::time::Duration::from_secs(STALE_SECS),
        dry_run,
    );
    if !swept.is_empty() {
        // Loud on purpose, and SPECIFIC. A silent sweep would make the next
        // instance of the leak look like a healthy day; a sweep that logs only a
        // count leaves nobody able to say WHICH dirs went, which is the same
        // defect one level in (AF-238).
        let total_bytes: u64 = swept.iter().map(|d| d.bytes).sum();
        let with_data = swept.iter().filter(|d| d.chunks > 0).count();
        let oldest = swept.iter().map(|d| d.age_s).max().unwrap_or(0);
        // Named individually, newest-last, capped so a 100-dir backlog cannot
        // flood the log while the counts above stay authoritative.
        let mut named: Vec<&SweptDir> = swept.iter().collect();
        named.sort_by_key(|d| std::cmp::Reverse(d.age_s));
        for d in named.iter().take(SWEEP_LOG_DIRS) {
            tracing::warn!(dir = %d.name, age_s = d.age_s, chunks = d.chunks, bytes = d.bytes,
                           dry_run, "upload: orphaned staging dir");
        }
        let elided = swept.len().saturating_sub(SWEEP_LOG_DIRS);
        tracing::warn!(
            count = swept.len(), with_data, total_bytes, oldest_age_s = oldest, elided, dry_run,
            "upload: {}{} orphaned .chunked-* dir(s), {with_data} holding data, {total_bytes} \
             bytes, oldest {oldest}s. Expected after a restart (the in-flight map does not \
             survive one); a rising count means transfers are being abandoned before finish.{}",
            if dry_run { "DRY RUN — would remove " } else { "removed " },
            swept.len(),
            if elided > 0 { format!(" {elided} more not named above.") } else { String::new() },
        );
    }

    map.insert(uid.clone(), InFlight {
        filename,
        chunks: total_chunks,
        received: BTreeSet::new(),
        tmpdir: dir,
        ts: now_secs(),
    });

    Json(json!({"id": uid, "chunks": total_chunks})).into_response()
}

async fn chunk(
    state: UploadState,
    Path((id, n)): Path<(String, usize)>,
    body: Bytes,
) -> Response {
    let chunk_path = {
        let map = state.lock().unwrap();
        let Some(entry) = map.get(&id) else {
            return err(StatusCode::NOT_FOUND, json!({"error": "unknown upload"}));
        };
        if n >= entry.chunks {
            return err(StatusCode::BAD_REQUEST, json!({"error": "chunk index out of range"}));
        }
        entry.tmpdir.join(format!("{n:06}"))
    };

    if let Err(e) = tokio::fs::write(&chunk_path, &body).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()}));
    }

    {
        let mut map = state.lock().unwrap();
        if let Some(entry) = map.get_mut(&id) {
            entry.received.insert(n);
        }
    }

    Json(json!({"ok": true, "chunk": n})).into_response()
}

async fn finish(state: UploadState, Path(id): Path<String>) -> Response {
    let entry = {
        let map = state.lock().unwrap();
        let Some(entry) = map.get(&id) else {
            return err(StatusCode::NOT_FOUND, json!({"error": "unknown upload"}));
        };
        if entry.received.len() < entry.chunks {
            let missing = entry.chunks - entry.received.len();
            return err(StatusCode::BAD_REQUEST, json!({"error": format!("{missing} chunks missing")}));
        }
        InFlight {
            filename: entry.filename.clone(),
            chunks: entry.chunks,
            received: entry.received.clone(),
            tmpdir: entry.tmpdir.clone(),
            ts: entry.ts,
        }
    };

    let dest_dir = uploads_dir();
    if let Err(e) = std::fs::create_dir_all(&dest_dir) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()}));
    }

    let save_name = format!("{id}-{}", entry.filename);
    let save_path = dest_dir.join(&save_name);

    // Assemble chunks
    let assemble = tokio::task::spawn_blocking({
        let tmpdir = entry.tmpdir.clone();
        let chunks = entry.chunks;
        let target = save_path.clone();
        move || -> std::io::Result<()> {
            let mut out = std::fs::File::create(&target)?;
            for i in 0..chunks {
                let cp = tmpdir.join(format!("{i:06}"));
                let mut inp = std::fs::File::open(&cp)?;
                std::io::copy(&mut inp, &mut out)?;
            }
            let _ = std::fs::remove_dir_all(&tmpdir);
            Ok(())
        }
    });

    match assemble.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
    }

    // Remove from in-flight
    {
        let mut map = state.lock().unwrap();
        map.remove(&id);
    }

    // Purge old uploads (>24h)
    if let Ok(entries) = std::fs::read_dir(&dest_dir) {
        let cutoff = now_secs().saturating_sub(PURGE_AGE_SECS);
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if let Ok(meta) = e.metadata() {
                let mtime = meta.modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if mtime < cutoff {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    Json(json!({
        "path": save_path.display().to_string(),
        "name": entry.filename,
        "url": format!("/api/uploads/{save_name}"),
    })).into_response()
}

async fn serve_uploaded(Path(filename): Path<String>) -> Response {
    let path = uploads_dir().join(&filename);
    if !path.is_file() {
        return err(StatusCode::NOT_FOUND, json!({"error": "file not found"}));
    }
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let ct = content_type_for(&filename);
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, ct.to_string())],
                bytes,
            ).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
    }
}

fn content_type_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "txt" | "log" | "md" => "text/plain; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css",
        "js" | "mjs" => "application/javascript",
        "wasm" => "application/wasm",
        "zip" => "application/zip",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "webm" => "audio/webm",
        "mp4" => "video/mp4",
        "csv" => "text/csv",
        _ => "application/octet-stream",
    }
}

fn rand_id() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    hasher.finish() & 0xFFFF_FFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    /// AF-235. The stale purge in `start()` iterates the IN-MEMORY map, so it can
    /// only free directories this process still knows about — and the map does
    /// not survive a restart, which this binary does on every commit touching
    /// crates/. Every restart therefore orphaned its `.chunked-*` dirs with no
    /// record left to purge them by. Four empty ones are what let @Dygreens prove
    /// the client half of this bug on #124.
    ///
    /// Both rules are asserted, in the two directions that can actually fail:
    /// a LIVE upload's dir is never swept at any age, and an orphan YOUNGER than
    /// the cutoff is never swept. Testing only "the old orphan went away" would
    /// pass equally well against a function that deletes the whole directory.
    #[test]
    fn sweep_removes_orphans_but_never_live_or_young_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        for n in [".chunked-live", ".chunked-orphan", "not-a-chunk-dir"] {
            std::fs::create_dir_all(dir.join(n)).unwrap();
        }
        // A plain FILE named like a staging dir must not be removed either.
        std::fs::write(dir.join(".chunked-afile"), b"x").unwrap();

        let mut live: std::collections::HashMap<String, InFlight> = Default::default();
        live.insert("live".into(), InFlight {
            filename: "f".into(), chunks: 1, received: BTreeSet::new(),
            tmpdir: dir.join(".chunked-live"), ts: now_secs(),
        });

        // Age gate: with a 1h cutoff nothing here is old enough, so a correct
        // sweep removes NOTHING. This is the assertion that fails if the age
        // check is dropped — the orphan is deletable in every other respect.
        let n = sweep_orphan_chunk_dirs(dir, &live, std::time::Duration::from_secs(3600), false);
        assert_eq!(n.len(), 0, "nothing is older than an hour; a young orphan must survive");
        assert!(dir.join(".chunked-orphan").exists(), "young orphan was swept");

        // DRY RUN reports exactly what a real sweep would take, and takes NOTHING.
        // This is the cell that has to hold before anyone points this at 73MB of
        // a human's real partial uploads: a dry run that quietly deleted would be
        // the worst possible defect here, because it is the mode you reach for
        // precisely when you are not sure.
        let dry = sweep_orphan_chunk_dirs(dir, &live, std::time::Duration::ZERO, true);
        assert_eq!(dry.len(), 1, "dry run must REPORT the orphan");
        assert!(dir.join(".chunked-orphan").exists(), "DRY RUN MUST NOT DELETE");
        assert!(dir.join(".chunked-live").exists(), "dry run must not touch a live dir either");

        // Age gate satisfied: now the orphan goes and the LIVE one stays.
        let n = sweep_orphan_chunk_dirs(dir, &live, std::time::Duration::ZERO, false);
        assert_eq!(n.len(), 1, "exactly the one orphan directory");
        // The record is captured BEFORE deletion — a count cannot say which dir
        // went, and nothing on disk can answer it afterwards (AF-238).
        assert_eq!(n[0].name, ".chunked-orphan", "the sweep must name what it took");
        assert!(!dir.join(".chunked-orphan").exists(), "orphan should be gone");
        assert!(dir.join(".chunked-live").exists(), "a LIVE upload's dir must never be swept");
        assert!(dir.join("not-a-chunk-dir").exists(), "unrelated dirs are not this sweep's business");
        assert!(dir.join(".chunked-afile").exists(), "a file is not a staging dir");
    }

    fn test_state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        std::mem::forget(dir);
        AppState {
            store,
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        }
    }

    #[tokio::test]
    async fn full_upload_flow() {
        let tmp = tempfile::tempdir().unwrap();
        // Shared guard, not a bare set_var: AMUX_HOME is process-global and
        // other lib tests (settings, journal, history, session_verbs) set it
        // under settings::test_env::LOCK — an unguarded write here raced
        // them and the flow read another test's home mid-flight (flaked in
        // full-suite runs, 2026-08-09).
        let _home = crate::api::settings::test_env::set_home(tmp.path());

        let app: Router = Router::new()
            .nest("/api/upload", routes())
            .nest("/api/uploads", serve_routes())
            .with_state(test_state());

        // Start
        let res = app.clone()
            .oneshot(Request::builder()
                .method("POST")
                .uri("/api/upload/start")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"hello.txt","size":11,"chunks":2}"#))
                .unwrap())
            .await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["chunks"], 2);

        // Chunk 0
        let res = app.clone()
            .oneshot(Request::builder()
                .method("PUT")
                .uri(format!("/api/upload/{id}/chunk/0"))
                .header("content-type", "application/octet-stream")
                .body(Body::from("hello"))
                .unwrap())
            .await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Chunk 1
        let res = app.clone()
            .oneshot(Request::builder()
                .method("PUT")
                .uri(format!("/api/upload/{id}/chunk/1"))
                .header("content-type", "application/octet-stream")
                .body(Body::from(" world"))
                .unwrap())
            .await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Finish
        let res = app.clone()
            .oneshot(Request::builder()
                .method("POST")
                .uri(format!("/api/upload/{id}/finish"))
                .body(Body::empty())
                .unwrap())
            .await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert!(v["path"].as_str().unwrap().contains("hello.txt"));
        assert!(v["url"].as_str().unwrap().starts_with("/api/uploads/"));

        // Serve the uploaded file
        let url = v["url"].as_str().unwrap();
        let res = app
            .oneshot(Request::builder()
                .uri(url)
                .body(Body::empty())
                .unwrap())
            .await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"hello world");

    }
}
