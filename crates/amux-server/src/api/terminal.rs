//! /api/terminal — the Workspace tab's web-terminal panes (AMUX-2885).
//!
//! A PORT of a live contract: the SPA ships an xterm.js terminal per Workspace
//! pane (`_wsTerm*` in app.js) and has 404'd on every keystroke since the
//! python retirement. Contract derived from the SPA's OWN calls — the actual
//! contract, the way graph/speedtest were ported — not the deleted python:
//!
//!   POST   /api/terminal/create            {cols,rows} -> {id} | {error}
//!   POST   /api/terminal/{id}/input        {data: base64}      -> {ok}
//!   POST   /api/terminal/{id}/resize       {cols,rows}         -> {ok}
//!   GET    /api/terminal/{id}/output[?wait=N]  -> {alive, data: base64}
//!   DELETE /api/terminal/{id}                                  -> {ok}
//!
//! `output` is a long-poll: with `?wait=N` it blocks up to N seconds for new
//! bytes, then drains and returns them base64. The SPA re-polls immediately
//! (`wait=25`), so this is a chunk-by-chunk stream over plain HTTP. Without
//! `wait` it returns the buffered bytes at once — the reconnect path, so a
//! reloaded pane repaints scrollback.
//!
//! LOCAL SHELL ONLY. The SPA sends `{cols,rows}` and never a `host`, so this
//! spawns `$SHELL`/bash in a PTY on the machine the server runs on. The python
//! had an ssh-to-arbitrary-host branch; it is DELIBERATELY not ported — an
//! ssh-out surface is a real escalation and nothing in the UI asks for it.
//! This is no new capability: the same bearer/ui-token auth guards it, and an
//! agent that can reach this endpoint can already run shell on this box.
//!
//! Every blocking PTY op runs on `spawn_blocking` — portable-pty is sync and
//! the long-poll wait would otherwise stall the async executor (the exact
//! class the sessions single-flight fix was about).

use super::AppState;
use axum::extract::{Path as AxPath, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/create", post(create))
        .route("/{id}/input", post(input))
        .route("/{id}/resize", post(resize))
        .route("/{id}/output", get(output))
        .route("/{id}", delete(kill))
}

/// One live PTY: the master (for writes + resize), the child (for kill), and a
/// buffer a reader thread fills. `alive` flips false on EOF/exit; `buf` +
/// `cv` are the long-poll's wait/notify pair.
struct Session {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    shared: Arc<Shared>,
    last_activity: Instant,
}

struct Shared {
    buf: Mutex<Vec<u8>>,
    cv: Condvar,
    alive: AtomicBool,
}

fn store() -> &'static Mutex<HashMap<String, Session>> {
    static S: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Monotonic id counter — the id only needs to be unique + unguessable-enough
/// for a same-origin authed API. `pty-<n>` matches the python id shape.
fn next_id() -> String {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(1);
    format!("pty-{}", N.fetch_add(1, Ordering::Relaxed))
}

fn idle_secs() -> u64 {
    std::env::var("AMUX_TERMINAL_IDLE_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600)
}

/// Kill sessions untouched for longer than the idle window. Called on create
/// so a fleet of abandoned panes (laptop closed mid-session) does not leak
/// shells forever — there is no other lifecycle event a closed tab produces.
fn reap_idle(map: &mut HashMap<String, Session>) {
    let cutoff = idle_secs();
    let dead: Vec<String> = map
        .iter()
        .filter(|(_, s)| s.last_activity.elapsed().as_secs() > cutoff)
        .map(|(k, _)| k.clone())
        .collect();
    for id in dead {
        if let Some(mut s) = map.remove(&id) {
            let _ = s.child.kill();
            s.shared.alive.store(false, Ordering::SeqCst);
            tracing::info!(target: "amux::terminal", %id, "reaped idle PTY");
        }
    }
}

#[derive(serde::Deserialize)]
struct CreateBody {
    #[serde(default = "def_cols")]
    cols: u16,
    #[serde(default = "def_rows")]
    rows: u16,
}
fn def_cols() -> u16 {
    100
}
fn def_rows() -> u16 {
    30
}

async fn create(body: Option<Json<CreateBody>>) -> Response {
    let CreateBody { cols, rows } = body.map(|Json(b)| b).unwrap_or(CreateBody {
        cols: def_cols(),
        rows: def_rows(),
    });
    let res = tokio::task::spawn_blocking(move || spawn_pty(cols, rows)).await;
    match res {
        Ok(Ok(id)) => (StatusCode::OK, Json(json!({ "id": id }))).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

fn spawn_pty(cols: u16, rows: u16) -> Result<String, String> {
    let size = PtySize { rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0 };
    let pair = native_pty_system().openpty(size).map_err(|e| e.to_string())?;
    // `$SHELL` on the host, bash as the fallback the python used. Login-ish
    // interactive shell so the pane behaves like a real terminal.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let mut cmd = CommandBuilder::new(&shell);
    cmd.env("TERM", "xterm-256color");
    // `/api/settings/env` is a runtime file update; it intentionally does not
    // mutate Rust's process-global environment. Resolve the current file-backed
    // values for every new terminal so a key the UI just saved is usable from
    // the official shell as well as from a newly launched worker.
    for (key, value) in
        super::settings::runtime_provider_env(&super::settings::amux_home())
    {
        cmd.env(key, value);
    }
    if let Ok(home) = std::env::var("HOME") {
        cmd.cwd(home);
    }
    let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    // Slave handle is not needed after spawn; dropping it lets the child own
    // the tty and makes EOF arrive on the master when the shell exits.
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
    let shared = Arc::new(Shared {
        buf: Mutex::new(Vec::new()),
        cv: Condvar::new(),
        alive: AtomicBool::new(true),
    });
    // Reader thread: drain the PTY into the shared buffer and wake any waiting
    // long-poll. Ends (marking the session dead) on EOF/exit.
    let sh = shared.clone();
    std::thread::spawn(move || {
        let mut tmp = [0u8; 8192];
        loop {
            match reader.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut b) = sh.buf.lock() {
                        b.extend_from_slice(&tmp[..n]);
                    }
                    sh.cv.notify_all();
                }
                Err(_) => break,
            }
        }
        sh.alive.store(false, Ordering::SeqCst);
        sh.cv.notify_all();
    });
    let id = next_id();
    let mut map = store().lock().map_err(|_| "store poisoned".to_string())?;
    reap_idle(&mut map);
    map.insert(
        id.clone(),
        Session { master: pair.master, writer, child, shared, last_activity: Instant::now() },
    );
    Ok(id)
}

#[derive(serde::Deserialize)]
struct InputBody {
    #[serde(default)]
    data: String,
}

async fn input(AxPath(id): AxPath<String>, body: Option<Json<InputBody>>) -> Response {
    let data = body.map(|Json(b)| b.data).unwrap_or_default();
    let res = tokio::task::spawn_blocking(move || -> Result<bool, String> {
        let bytes = crate::integrations::email::base64url_decode(&data)
            .or_else(|_| b64_std_decode(&data))
            .map_err(|_| "input data is not valid base64".to_string())?;
        let mut map = store().lock().map_err(|_| "store poisoned".to_string())?;
        let Some(s) = map.get_mut(&id) else { return Ok(false) };
        s.writer.write_all(&bytes).map_err(|e| e.to_string())?;
        let _ = s.writer.flush();
        s.last_activity = Instant::now();
        Ok(true)
    })
    .await;
    match res {
        Ok(Ok(true)) => Json(json!({ "ok": true })).into_response(),
        Ok(Ok(false)) => (StatusCode::NOT_FOUND, Json(json!({ "error": "no such terminal" }))).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct ResizeBody {
    cols: u16,
    rows: u16,
}

async fn resize(AxPath(id): AxPath<String>, body: Option<Json<ResizeBody>>) -> Response {
    let Some(Json(ResizeBody { cols, rows })) = body else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "cols and rows required" }))).into_response();
    };
    let res = tokio::task::spawn_blocking(move || -> Result<bool, String> {
        let mut map = store().lock().map_err(|_| "store poisoned".to_string())?;
        let Some(s) = map.get_mut(&id) else { return Ok(false) };
        s.master
            .resize(PtySize { rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0 })
            .map_err(|e| e.to_string())?;
        s.last_activity = Instant::now();
        Ok(true)
    })
    .await;
    match res {
        Ok(Ok(true)) => Json(json!({ "ok": true })).into_response(),
        Ok(Ok(false)) => (StatusCode::NOT_FOUND, Json(json!({ "error": "no such terminal" }))).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct OutputQ {
    #[serde(default)]
    wait: u64,
}

async fn output(AxPath(id): AxPath<String>, Query(q): Query<OutputQ>) -> Response {
    // Cap the long-poll so a client cannot pin a blocking thread indefinitely;
    // 25s matches the SPA and stays under proxy idle timeouts.
    let wait_ms = q.wait.min(25) * 1000;
    let res = tokio::task::spawn_blocking(move || -> Option<Value> {
        // Grab the shared handle without holding the store lock across the wait
        // (that would serialize every terminal's poll behind one mutex).
        let shared = {
            let mut map = store().lock().ok()?;
            let s = map.get_mut(&id)?;
            s.last_activity = Instant::now();
            s.shared.clone()
        };
        let mut b = shared.buf.lock().ok()?;
        if b.is_empty() && wait_ms > 0 && shared.alive.load(Ordering::SeqCst) {
            let (guard, _timeout) = shared
                .cv
                .wait_timeout_while(b, std::time::Duration::from_millis(wait_ms), |b| {
                    b.is_empty() && shared.alive.load(Ordering::SeqCst)
                })
                .ok()?;
            b = guard;
        }
        let drained: Vec<u8> = std::mem::take(&mut *b);
        let alive = shared.alive.load(Ordering::SeqCst) || !drained.is_empty();
        Some(json!({
            "alive": alive,
            "data": if drained.is_empty() { String::new() } else { b64_std_encode(&drained) },
        }))
    })
    .await;
    match res {
        Ok(Some(v)) => Json(v).into_response(),
        // No such terminal -> report not-alive so the SPA clears its saved id
        // and creates a fresh pane, rather than erroring the whole tab.
        Ok(None) => Json(json!({ "alive": false, "data": "" })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

async fn kill(AxPath(id): AxPath<String>) -> Response {
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(mut map) = store().lock() {
            if let Some(mut s) = map.remove(&id) {
                let _ = s.child.kill();
                s.shared.alive.store(false, Ordering::SeqCst);
            }
        }
    })
    .await;
    Json(json!({ "ok": true })).into_response()
}

// The SPA base64s with btoa (standard alphabet, padded); the shared email
// helper is URL-safe/no-pad. Accept both on input, emit standard on output so
// atob() on the client is exact.
fn b64_std_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

fn b64_std_decode(s: &str) -> Result<Vec<u8>, String> {
    // Reuse the URL-safe decoder — it already treats '+'/'/' and '-'/'_' alike
    // and ignores '=' padding, so a standard-alphabet string decodes cleanly.
    crate::integrations::email::base64url_decode(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip_matches_standard_alphabet() {
        // btoa("hi") == "aGk=" — the exact string the SPA's atob() must reverse.
        assert_eq!(b64_std_encode(b"hi"), "aGk=");
        assert_eq!(b64_std_encode(b"echo"), "ZWNobw==");
        assert_eq!(b64_std_decode("aGk=").unwrap(), b"hi");
    }
}
