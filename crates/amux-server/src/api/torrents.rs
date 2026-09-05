//! Torrents API (SPA long-tail port): `/api/torrents/*` driving the aria2c
//! JSON-RPC daemon, route- and shape-compatible with the Python
//! `_handle_torrents` handlers.
//!
//! Parity decisions, recorded so they are not "fixed" later:
//! - Python's `_aria2_ensure` SPAWNS aria2c when it is not running. This
//!   server does not manage a child daemon; instead every RPC-touching route
//!   degrades to an honest 503 `{"error": "aria2c not running", "start":
//!   "<the exact command Python launches>"}` so a caller (or the SPA) can
//!   start it and retry — a clear way out, not 500 soup.
//! - aria2 returns every number as a STRING ("12345"); Python `int()`s them.
//!   [`as_i64`] does the same so the list shape carries real integers.
//! - `config` GET/POST are local state (the download dir) + a best-effort
//!   `aria2.changeGlobalOption` whose failure is swallowed exactly like
//!   Python's `try/except pass`. They do not 503: the dir is answerable
//!   whether or not aria2 is up (Python gates them behind _aria2_ensure only
//!   because the gate wraps the whole handler).
//! - The file route enforces "within the download dir" via canonicalized
//!   paths (Python `_path_is_within(resolve())`), streams in 1MB chunks, and
//!   honors single-range `Range:` headers with 206/Content-Range for video
//!   scrubbing (AMUX-1820). One edge differs: a NONEXISTENT path outside the
//!   dir answers 404 here (canonicalize fails first) where Python answers
//!   403 — nothing is disclosed either way.
//! - All RPC goes through the [`Aria2Rpc`] trait so tests script the wire
//!   and never need a live daemon.

use axum::body::Body;
use axum::extract::{Path as AxPath, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::AppState;

// ---- constants (ported from amux-server.py) --------------------------------

pub const ARIA2_RPC_PORT: u16 = 6800;
pub const ARIA2_RPC_SECRET: &str = "amux";

pub fn default_download_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
        .join("Downloads")
        .join("amux-torrents")
}

/// The exact daemon invocation Python's `_aria2_ensure` performs, as one
/// shell line — served in the 503 body so the way out is copy-pasteable.
pub fn aria2_start_command(download_dir: &std::path::Path) -> String {
    format!(
        "aria2c --enable-rpc --rpc-listen-port {ARIA2_RPC_PORT} --rpc-secret {ARIA2_RPC_SECRET} \
         --dir {} --seed-time=0 --bt-enable-lpd=true --enable-dht=true \
         --enable-peer-exchange=true --max-concurrent-downloads=5 --file-allocation=none \
         --daemon=false --quiet=true",
        download_dir.display()
    )
}

// ---- RPC seam --------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum RpcError {
    /// TCP-level failure: nothing is listening on the RPC port. Every route
    /// maps this to the honest 503.
    Unreachable(String),
    /// aria2 answered with a JSON-RPC error (bad gid, bad uri, ...).
    Rpc(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Unreachable(e) => write!(f, "aria2 unreachable: {e}"),
            RpcError::Rpc(e) => write!(f, "{e}"),
        }
    }
}

/// `params` is the method's parameter array WITHOUT the secret token — the
/// transport prepends `token:<secret>` (Python `_aria2_rpc` parity).
#[async_trait::async_trait]
pub trait Aria2Rpc: Send + Sync {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError>;
}

/// Production transport against `http://localhost:6800/jsonrpc` (Python's
/// urlopen timeout=5 kept).
pub struct HttpAria2 {
    client: reqwest::Client,
    url: String,
    secret: String,
}

impl HttpAria2 {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
            url: format!("http://localhost:{ARIA2_RPC_PORT}/jsonrpc"),
            secret: ARIA2_RPC_SECRET.to_string(),
        }
    }
}

impl Default for HttpAria2 {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Aria2Rpc for HttpAria2 {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let mut full = vec![json!(format!("token:{}", self.secret))];
        if let Value::Array(a) = params {
            full.extend(a);
        }
        let payload = json!({ "jsonrpc": "2.0", "id": "amux", "method": method, "params": full });
        let res = self
            .client
            .post(&self.url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| RpcError::Unreachable(e.to_string()))?;
        let body: Value = res.json().await.map_err(|e| RpcError::Rpc(e.to_string()))?;
        if let Some(err) = body.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| err.to_string());
            return Err(RpcError::Rpc(msg));
        }
        body.get("result").cloned().ok_or_else(|| RpcError::Rpc("no result in RPC reply".into()))
    }
}

// ---- context + router ------------------------------------------------------

pub struct TorrentsCtx {
    pub rpc: Arc<dyn Aria2Rpc>,
    /// Python's mutable global `_ARIA2_DOWNLOAD_DIR`.
    pub download_dir: Mutex<PathBuf>,
}

pub fn routes() -> Router<AppState> {
    routes_with(Arc::new(TorrentsCtx {
        rpc: Arc::new(HttpAria2::new()),
        download_dir: Mutex::new(default_download_dir()),
    }))
}

pub fn routes_with(ctx: Arc<TorrentsCtx>) -> Router<AppState> {
    Router::new()
        .route("/", get(list_torrents).post(add_torrent))
        .route("/config", get(get_config).post(set_config))
        .route("/{gid}", axum::routing::delete(delete_torrent))
        .route("/{gid}/file", get(serve_file))
        .route("/{gid}/{action}", axum::routing::post(torrent_action))
        // Python's trailing `{"error": "torrent route not found"}`.
        .fallback(|| async { route_not_found() })
        .layer(Extension(ctx))
}

// ---- shared helpers --------------------------------------------------------

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn route_not_found() -> Response {
    err(StatusCode::NOT_FOUND, json!({ "error": "torrent route not found" }))
}

/// The honest degradation every RPC-touching route promises.
fn aria2_down(ctx: &TorrentsCtx) -> Response {
    let dir = ctx.download_dir.lock().expect("download_dir").clone();
    err(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({ "error": "aria2c not running", "start": aria2_start_command(&dir) }),
    )
}

/// Python's `int(t.get(k, 0))` — aria2 serializes numbers as strings.
fn as_i64(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// Python route regex: `[a-f0-9]+`. A miss falls to the module 404.
fn valid_gid(gid: &str) -> bool {
    !gid.is_empty() && gid.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn body_str(body: &Value, k: &str) -> String {
    body.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

fn expanduser(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~") {
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            return PathBuf::from(format!("{home}{rest}"));
        }
    }
    PathBuf::from(p)
}

// ---- GET /api/torrents -----------------------------------------------------

/// Python `_aria2_list_all`: merge tellActive + tellWaiting + tellStopped
/// into the SPA's shape. Per-method RPC errors are swallowed (parity), but
/// an unreachable daemon is the 503.
pub async fn list_torrents(Extension(ctx): Extension<Arc<TorrentsCtx>>) -> Response {
    let mut raw: Vec<Value> = Vec::new();
    for (method, params) in [
        ("aria2.tellActive", json!([])),
        ("aria2.tellWaiting", json!([0, 100])),
        ("aria2.tellStopped", json!([0, 100])),
    ] {
        match ctx.rpc.call(method, params).await {
            Ok(Value::Array(items)) => raw.extend(items),
            Ok(_) => {}
            Err(RpcError::Unreachable(_)) => return aria2_down(&ctx),
            Err(RpcError::Rpc(_)) => {} // Python: per-method except/pass
        }
    }
    let result: Vec<Value> = raw
        .iter()
        .map(|t| {
            let mut name = t
                .pointer("/bittorrent/info/name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let files_in = t.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
            if name.is_empty() {
                if let Some(f0) = files_in.first() {
                    name = f0
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .rsplit('/')
                        .next()
                        .unwrap_or("")
                        .to_string();
                }
            }
            let files: Vec<Value> = files_in
                .iter()
                .filter_map(|f| {
                    let fp = f.get("path").and_then(Value::as_str).unwrap_or("");
                    if fp.is_empty() {
                        return None;
                    }
                    let fsize = as_i64(f.get("length"));
                    let complete = as_i64(f.get("completedLength")) >= fsize && fsize > 0;
                    Some(json!({ "path": fp, "size": fsize, "complete": complete }))
                })
                .collect();
            json!({
                "gid": t.get("gid").and_then(Value::as_str).unwrap_or(""),
                "name": name,
                "status": t.get("status").and_then(Value::as_str).unwrap_or("unknown"),
                "total": as_i64(t.get("totalLength")),
                "completed": as_i64(t.get("completedLength")),
                "speed": as_i64(t.get("downloadSpeed")),
                "files": files,
            })
        })
        .collect();
    Json(Value::Array(result)).into_response()
}

// ---- POST /api/torrents ----------------------------------------------------

pub async fn add_torrent(
    Extension(ctx): Extension<Arc<TorrentsCtx>>,
    Json(body): Json<Value>,
) -> Response {
    let uri = body_str(&body, "uri");
    let torrent_path = body_str(&body, "file");
    if uri.is_empty() && torrent_path.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "uri or file required" }));
    }
    let call = if !torrent_path.is_empty() && std::path::Path::new(&torrent_path).is_file() {
        match std::fs::read(&torrent_path) {
            Ok(bytes) => {
                // Python base64.b64encode: standard alphabet, padded.
                let b64 = crate::integrations::email::base64_std(&bytes);
                ctx.rpc.call("aria2.addTorrent", json!([b64])).await
            }
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })),
        }
    } else {
        // magnet: and plain URLs take the same call (Python's two branches
        // are identical).
        ctx.rpc.call("aria2.addUri", json!([[uri]])).await
    };
    match call {
        Ok(gid) => Json(json!({ "gid": gid })).into_response(),
        Err(RpcError::Unreachable(_)) => aria2_down(&ctx),
        Err(RpcError::Rpc(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e })),
    }
}

// ---- POST /api/torrents/{gid}/{pause|resume|remove} ------------------------

pub async fn torrent_action(
    Extension(ctx): Extension<Arc<TorrentsCtx>>,
    AxPath((gid, action)): AxPath<(String, String)>,
) -> Response {
    if !valid_gid(&gid) || !matches!(action.as_str(), "pause" | "resume" | "remove") {
        return route_not_found();
    }
    let outcome = match action.as_str() {
        "pause" => ctx.rpc.call("aria2.forcePause", json!([gid])).await.map(|_| ()),
        "resume" => ctx.rpc.call("aria2.unpause", json!([gid])).await.map(|_| ()),
        _ => match ctx.rpc.call("aria2.forceRemove", json!([gid.clone()])).await {
            Ok(_) => Ok(()),
            Err(RpcError::Unreachable(e)) => Err(RpcError::Unreachable(e)),
            // Python: a failed forceRemove (already-finished download) falls
            // back to clearing the download result.
            Err(RpcError::Rpc(_)) => {
                ctx.rpc.call("aria2.removeDownloadResult", json!([gid])).await.map(|_| ())
            }
        },
    };
    match outcome {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(RpcError::Unreachable(_)) => aria2_down(&ctx),
        Err(RpcError::Rpc(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e })),
    }
}

// ---- DELETE /api/torrents/{gid} --------------------------------------------

pub async fn delete_torrent(
    Extension(ctx): Extension<Arc<TorrentsCtx>>,
    AxPath(gid): AxPath<String>,
) -> Response {
    if !valid_gid(&gid) {
        return route_not_found();
    }
    match ctx.rpc.call("aria2.forceRemove", json!([gid.clone()])).await {
        Ok(_) => {}
        Err(RpcError::Unreachable(_)) => return aria2_down(&ctx),
        Err(RpcError::Rpc(_)) => {
            // Python swallows BOTH failures and answers ok — deleting an
            // already-gone torrent is not an error the SPA can act on.
            if let Err(RpcError::Unreachable(_)) =
                ctx.rpc.call("aria2.removeDownloadResult", json!([gid])).await
            {
                return aria2_down(&ctx);
            }
        }
    }
    Json(json!({ "ok": true })).into_response()
}

// ---- GET/POST /api/torrents/config -----------------------------------------

pub async fn get_config(Extension(ctx): Extension<Arc<TorrentsCtx>>) -> Response {
    let dir = ctx.download_dir.lock().expect("download_dir").clone();
    Json(json!({ "download_dir": dir.display().to_string() })).into_response()
}

pub async fn set_config(
    Extension(ctx): Extension<Arc<TorrentsCtx>>,
    Json(body): Json<Value>,
) -> Response {
    let new_dir = body_str(&body, "download_dir");
    if new_dir.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "download_dir required" }));
    }
    let expanded = expanduser(&new_dir);
    if let Err(e) = std::fs::create_dir_all(&expanded) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() }));
    }
    *ctx.download_dir.lock().expect("download_dir") = expanded.clone();
    // Best-effort, exactly Python's try/except pass — a dead daemon picks
    // the dir up from the start command in the 503 instead.
    let _ = ctx
        .rpc
        .call(
            "aria2.changeGlobalOption",
            json!([{ "dir": expanded.display().to_string() }]),
        )
        .await;
    Json(json!({ "download_dir": expanded.display().to_string() })).into_response()
}

// ---- GET /api/torrents/{gid}/file?path= ------------------------------------

#[derive(serde::Deserialize)]
pub struct FileParams {
    #[serde(default)]
    path: Option<String>,
}

const CHUNK: u64 = 1024 * 1024;

fn content_type_for(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "zip" => "application/zip",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

pub async fn serve_file(
    Extension(ctx): Extension<Arc<TorrentsCtx>>,
    AxPath(gid): AxPath<String>,
    Query(p): Query<FileParams>,
    headers: HeaderMap,
) -> Response {
    if !valid_gid(&gid) {
        return route_not_found();
    }
    let Some(file_path) = p.path.filter(|s| !s.is_empty()) else {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "path required" }));
    };
    let download_dir = ctx.download_dir.lock().expect("download_dir").clone();
    // Security: canonicalize both sides before the containment check
    // (Python `_path_is_within(resolve())`) so ../ and symlinks cannot
    // escape the download dir.
    let Ok(real) = expanduser(&file_path).canonicalize() else {
        return err(StatusCode::NOT_FOUND, json!({ "error": "file not found" }));
    };
    let Ok(root) = download_dir.canonicalize() else {
        return err(StatusCode::FORBIDDEN, json!({ "error": "forbidden" }));
    };
    if !real.starts_with(&root) {
        return err(StatusCode::FORBIDDEN, json!({ "error": "forbidden" }));
    }
    if !real.is_file() {
        return err(StatusCode::NOT_FOUND, json!({ "error": "file not found" }));
    }
    let ct = content_type_for(&real);
    let fsize = match std::fs::metadata(&real) {
        Ok(m) => m.len(),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })),
    };

    // Single-range support (Python: `bytes=(\d+)-(\d*)`).
    let range = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| {
            let (a, b) = r.split_once('-')?;
            let start: u64 = a.parse().ok()?;
            let end: u64 = if b.is_empty() { fsize.saturating_sub(1) } else { b.parse().ok()? };
            Some((start, end.min(fsize.saturating_sub(1))))
        });

    let (status, start, length, content_range) = match range {
        Some((start, end)) if start <= end && start < fsize => {
            (StatusCode::PARTIAL_CONTENT, start, end - start + 1, Some(format!("bytes {start}-{end}/{fsize}")))
        }
        _ => (StatusCode::OK, 0, fsize, None),
    };

    let mut file = match tokio::fs::File::open(&real).await {
        Ok(f) => f,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })),
    };
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    if start > 0 {
        if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
            return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() }));
        }
    }
    // 1MB-chunk stream (Python `_stream_file_body`): player aborts drop the
    // stream without buffering the whole file.
    let stream = futures::stream::unfold((file, length), |(mut f, remaining)| async move {
        if remaining == 0 {
            return None;
        }
        let cap = remaining.min(CHUNK) as usize;
        let mut buf = vec![0u8; cap];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(buf)), (f, remaining - n as u64)))
            }
            Err(e) => Some((Err(e), (f, 0))),
        }
    });

    let mut builder = Response::builder()
        .status(status)
        .header("Content-Type", ct)
        .header("Content-Length", length.to_string())
        .header("Accept-Ranges", "bytes");
    if let Some(cr) = content_range {
        builder = builder.header("Content-Range", cr);
    } else {
        builder = builder
            .header("Content-Disposition", format!("inline; filename=\"{}\"", real.file_name().and_then(|n| n.to_str()).unwrap_or("file")));
    }
    builder.body(Body::from_stream(stream)).unwrap_or_else(|e| {
        err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() }))
    })
}

// ---------------------------------------------------------------------------
// Tests — scripted Aria2Rpc, no live daemon, temp dirs only.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    /// Scripted RPC: answers by method name (first match pops), records
    /// every call with its params.
    struct MockRpc {
        calls: Mutex<Vec<(String, Value)>>,
        script: Mutex<Vec<(String, Result<Value, RpcError>)>>,
    }

    impl MockRpc {
        fn new(script: Vec<(&str, Result<Value, RpcError>)>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                script: Mutex::new(
                    script.into_iter().map(|(m, r)| (m.to_string(), r)).collect(),
                ),
            })
        }
        fn calls(&self) -> Vec<(String, Value)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Aria2Rpc for MockRpc {
        async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
            self.calls.lock().unwrap().push((method.to_string(), params));
            let mut script = self.script.lock().unwrap();
            if let Some(pos) = script.iter().position(|(m, _)| m == method) {
                return script.remove(pos).1;
            }
            Err(RpcError::Rpc(format!("mock has no answer for {method}")))
        }
    }

    fn unreachable() -> Result<Value, RpcError> {
        Err(RpcError::Unreachable("connection refused".into()))
    }

    /// Torrent routes never touch the store; a throwaway temp DB keeps the
    /// Router<AppState> signature honest. The returned TempDir guard must
    /// outlive the requests.
    fn app_with(rpc: Arc<MockRpc>, dir: PathBuf) -> (axum::Router, tempfile::TempDir) {
        let ctx = Arc::new(TorrentsCtx { rpc, download_dir: Mutex::new(dir) });
        let state_dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&state_dir.path().join("t.db")).unwrap();
        let state = AppState {
            store: Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (Router::new().nest("/api/torrents", routes_with(ctx)).with_state(state), state_dir)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let req = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let hdrs = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, hdrs, bytes.to_vec())
    }

    fn as_json(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
    }

    #[tokio::test]
    async fn list_maps_aria2_string_numbers_to_python_shape() {
        // aria2's own wire shape: every number a string.
        let bt = json!({
            "gid": "2089b05ecca3d829",
            "status": "active",
            "totalLength": "104857600",
            "completedLength": "52428800",
            "downloadSpeed": "1048576",
            "bittorrent": { "info": { "name": "ubuntu.iso" } },
            "files": [
                { "path": "/dl/ubuntu.iso", "length": "104857600", "completedLength": "104857600" },
                { "path": "", "length": "1", "completedLength": "1" }
            ]
        });
        let plain = json!({
            "gid": "aaaa00001111bbbb",
            "status": "complete",
            "totalLength": "10",
            "completedLength": "10",
            "downloadSpeed": "0",
            "files": [ { "path": "/dl/sub/movie.mp4", "length": "10", "completedLength": "3" } ]
        });
        let rpc = MockRpc::new(vec![
            ("aria2.tellActive", Ok(json!([bt]))),
            ("aria2.tellWaiting", Ok(json!([]))),
            ("aria2.tellStopped", Ok(json!([plain]))),
        ]);
        let (app, _sd) = app_with(rpc.clone(), default_download_dir());
        let (st, _, body) = send(&app, "GET", "/api/torrents", None, &[]).await;
        let v = as_json(&body);
        assert_eq!(st, StatusCode::OK, "{v}");
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // Integers, not strings (Python int()s them).
        assert_eq!(arr[0]["gid"], json!("2089b05ecca3d829"));
        assert_eq!(arr[0]["name"], json!("ubuntu.iso"));
        assert_eq!(arr[0]["total"], json!(104857600));
        assert_eq!(arr[0]["completed"], json!(52428800));
        assert_eq!(arr[0]["speed"], json!(1048576));
        // Empty-path file dropped; complete = completedLength >= length.
        assert_eq!(arr[0]["files"], json!([{ "path": "/dl/ubuntu.iso", "size": 104857600, "complete": true }]));
        // Name falls back to the first file's basename.
        assert_eq!(arr[1]["name"], json!("movie.mp4"));
        assert_eq!(arr[1]["files"][0]["complete"], json!(false));
        // Python's paging params: tellActive bare, the others [0, 100].
        let calls = rpc.calls();
        assert_eq!(calls[0], ("aria2.tellActive".into(), json!([])));
        assert_eq!(calls[1], ("aria2.tellWaiting".into(), json!([0, 100])));
        assert_eq!(calls[2], ("aria2.tellStopped".into(), json!([0, 100])));
    }

    #[tokio::test]
    async fn unreachable_rpc_degrades_to_honest_503_with_start_command() {
        let rpc = MockRpc::new(vec![
            ("aria2.tellActive", unreachable()),
            ("aria2.addUri", unreachable()),
            ("aria2.forcePause", unreachable()),
        ]);
        let (app, _sd) = app_with(rpc, PathBuf::from("/tmp/dl"));
        for (method, path, body) in [
            ("GET", "/api/torrents", None),
            ("POST", "/api/torrents", Some(json!({ "uri": "magnet:?xt=urn:btih:aa" }))),
            ("POST", "/api/torrents/abc123/pause", None),
        ] {
            let (st, _, bytes) = send(&app, method, path, body, &[]).await;
            let v = as_json(&bytes);
            assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{method} {path}: {v}");
            assert_eq!(v["error"], json!("aria2c not running"));
            let start = v["start"].as_str().unwrap();
            assert!(
                start.starts_with("aria2c --enable-rpc --rpc-listen-port 6800 --rpc-secret amux"),
                "{start}"
            );
            assert!(start.contains("--dir /tmp/dl"), "{start}");
            assert!(start.contains("--seed-time=0"), "{start}");
        }
    }

    #[tokio::test]
    async fn add_magnet_uses_adduri_and_validates() {
        let rpc = MockRpc::new(vec![("aria2.addUri", Ok(json!("gidgidgid1234567")))]);
        let (app, _sd) = app_with(rpc.clone(), default_download_dir());
        let (st, _, bytes) = send(
            &app,
            "POST",
            "/api/torrents",
            Some(json!({ "uri": "magnet:?xt=urn:btih:deadbeef" })),
            &[],
        )
        .await;
        let v = as_json(&bytes);
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v, json!({ "gid": "gidgidgid1234567" }));
        // Python param shape: [[uri]] (a list of URIs for one download).
        assert_eq!(rpc.calls()[0].1, json!([["magnet:?xt=urn:btih:deadbeef"]]));

        let (st, _, bytes) = send(&app, "POST", "/api/torrents", Some(json!({})), &[]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(as_json(&bytes)["error"], json!("uri or file required"));
    }

    #[tokio::test]
    async fn actions_map_to_pythons_rpc_verbs() {
        let rpc = MockRpc::new(vec![
            ("aria2.forcePause", Ok(json!("ok"))),
            ("aria2.unpause", Ok(json!("ok"))),
        ]);
        let (app, _sd) = app_with(rpc.clone(), default_download_dir());
        let (st, _, b) = send(&app, "POST", "/api/torrents/abc123/pause", None, &[]).await;
        assert_eq!(st, StatusCode::OK, "{}", as_json(&b));
        let (st, _, _) = send(&app, "POST", "/api/torrents/abc123/resume", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        let names: Vec<String> = rpc.calls().iter().map(|(m, _)| m.clone()).collect();
        assert_eq!(names, vec!["aria2.forcePause", "aria2.unpause"]);

        // Unknown action / uppercase gid: Python's regex miss -> module 404.
        let (st, _, b) = send(&app, "POST", "/api/torrents/abc123/rewind", None, &[]).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(as_json(&b)["error"], json!("torrent route not found"));
        let (st, _, _) = send(&app, "POST", "/api/torrents/ABC123/pause", None, &[]).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn remove_falls_back_to_remove_download_result() {
        // Action remove: forceRemove fails (finished download) -> fallback.
        let rpc = MockRpc::new(vec![
            ("aria2.forceRemove", Err(RpcError::Rpc("Active Download not found".into()))),
            ("aria2.removeDownloadResult", Ok(json!("ok"))),
        ]);
        let (app, _sd) = app_with(rpc.clone(), default_download_dir());
        let (st, _, b) = send(&app, "POST", "/api/torrents/abc123/remove", None, &[]).await;
        assert_eq!(st, StatusCode::OK, "{}", as_json(&b));
        assert_eq!(as_json(&b), json!({ "ok": true }));
        let names: Vec<String> = rpc.calls().iter().map(|(m, _)| m.clone()).collect();
        assert_eq!(names, vec!["aria2.forceRemove", "aria2.removeDownloadResult"]);

        // DELETE swallows even a double failure (Python's nested pass) —
        // but still owes the ok.
        let rpc2 = MockRpc::new(vec![
            ("aria2.forceRemove", Err(RpcError::Rpc("gone".into()))),
            ("aria2.removeDownloadResult", Err(RpcError::Rpc("also gone".into()))),
        ]);
        let (app2, _sd2) = app_with(rpc2, default_download_dir());
        let (st, _, b) = send(&app2, "DELETE", "/api/torrents/abc123", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(as_json(&b), json!({ "ok": true }));
    }

    #[tokio::test]
    async fn config_round_trip_and_best_effort_global_option() {
        let dl = tempfile::tempdir().unwrap();
        let target = dl.path().join("moved-here");
        // changeGlobalOption unreachable -> still 200 (best-effort, parity).
        let rpc = MockRpc::new(vec![("aria2.changeGlobalOption", unreachable())]);
        let (app, _sd) = app_with(rpc.clone(), dl.path().to_path_buf());

        let (st, _, b) = send(&app, "GET", "/api/torrents/config", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(as_json(&b)["download_dir"], json!(dl.path().display().to_string()));

        let (st, _, b) = send(
            &app,
            "POST",
            "/api/torrents/config",
            Some(json!({ "download_dir": target.display().to_string() })),
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}", as_json(&b));
        assert_eq!(as_json(&b)["download_dir"], json!(target.display().to_string()));
        assert!(target.is_dir(), "config POST must create the dir");
        assert_eq!(
            rpc.calls()[0],
            ("aria2.changeGlobalOption".into(), json!([{ "dir": target.display().to_string() }]))
        );

        let (st, _, b) = send(&app, "POST", "/api/torrents/config", Some(json!({})), &[]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(as_json(&b)["error"], json!("download_dir required"));
    }

    #[tokio::test]
    async fn file_route_is_jailed_to_download_dir_and_supports_ranges() {
        let dl = tempfile::tempdir().unwrap();
        let inside = dl.path().join("movie.mp4");
        std::fs::write(&inside, b"0123456789ABCDEF").unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("secret.txt");
        std::fs::write(&outside, b"secret").unwrap();

        let (app, _sd) = app_with(MockRpc::new(vec![]), dl.path().to_path_buf());

        // Outside the download dir -> 403, content never served.
        let (st, _, b) = send(
            &app,
            "GET",
            &format!(
                "/api/torrents/abc123/file?path={}",
                crate::integrations::email::urlencode(&outside.display().to_string())
            ),
            None,
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{}", as_json(&b));

        // Full fetch: 200, exact bytes, media headers.
        let uri = format!(
            "/api/torrents/abc123/file?path={}",
            crate::integrations::email::urlencode(&inside.display().to_string())
        );
        let (st, h, b) = send(&app, "GET", &uri, None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(b, b"0123456789ABCDEF");
        assert_eq!(h.get("content-type").unwrap(), "video/mp4");
        assert_eq!(h.get("accept-ranges").unwrap(), "bytes");

        // Range fetch: 206 + Content-Range (the video-scrub path).
        let (st, h, b) = send(&app, "GET", &uri, None, &[("Range", "bytes=4-7")]).await;
        assert_eq!(st, StatusCode::PARTIAL_CONTENT);
        assert_eq!(b, b"4567");
        assert_eq!(h.get("content-range").unwrap(), "bytes 4-7/16");
        assert_eq!(h.get("content-length").unwrap(), "4");

        // Open-ended range.
        let (st, _, b) = send(&app, "GET", &uri, None, &[("Range", "bytes=12-")]).await;
        assert_eq!(st, StatusCode::PARTIAL_CONTENT);
        assert_eq!(b, b"CDEF");

        // Missing ?path= -> Python's 400.
        let (st, _, b) = send(&app, "GET", "/api/torrents/abc123/file", None, &[]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(as_json(&b)["error"], json!("path required"));
    }
}
