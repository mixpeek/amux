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

    // The map above is IN MEMORY, so every restart orphans the .chunked-* dirs
    // on disk with no record left to purge them by — they accumulate forever
    // (four empty ones were sitting in ~/.amux/uploads when this was found).
    // Sweep the directory itself, skipping anything still tracked: a live
    // upload's dir gets an mtime bump with each chunk written into it.
    if let Ok(rd) = std::fs::read_dir(uploads_dir()) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            let Some(other_uid) = name.strip_prefix(".chunked-") else { continue };
            if other_uid == uid || map.contains_key(other_uid) {
                continue;
            }
            let is_stale = ent
                .metadata()
                .ok()
                .filter(|m| m.is_dir())
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .is_some_and(|d| d.as_secs() < cutoff);
            if is_stale {
                let _ = std::fs::remove_dir_all(ent.path());
            }
        }
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
