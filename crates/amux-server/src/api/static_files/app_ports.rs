//! Each app inside amux on an origin of its own: the apps origin's host on a
//! port only that app answers on, with the app at `/`.
//!
//! Under `/app/<id>/` an app works only as far as amux can rewrite it. The
//! relay routes the `src`/`href` it can see in HTML back under the prefix, but
//! nothing it cannot see: a module's `import "/src/App.tsx"`, a stylesheet's
//! `url(/fonts/x.woff2)`, a `fetch("/api")`, and a router that reads
//! `/app/<id>/` as its page. A Vite dev server through the prefix painted its
//! server-rendered HTML with 53 requests 404ing, so nothing ran, the fonts and
//! the hero video were missing (owner, 2026-11-08: "a lot of issues with the
//! app thing"). On its own port the app sees the paths it was written for and
//! nothing needs rewriting. Its WebSocket (a dev server's live reload) is
//! relayed too.
//!
//! The ports come from `AMUX_APP_PORTS` (`<first>-<last>`; default the 32
//! after the apps port; `0` turns this off). An app keeps its port across
//! restarts (`~/.amux/app-origins.json`). Where the apps origin is one fixed
//! address (`AMUX_APPS_URL`, an amux Cloud server behind Caddy), apps stay
//! under `/app/<id>/` on it, as does any app whose port cannot be bound.
use super::app_relay;
use axum::body::Body;
use axum::extract::ws::{Message as AxMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use futures::{SinkExt, StreamExt};
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as TMsg;

const FILE: &str = "app-origins.json";
const DEFAULT_COUNT: u16 = 32;

static TLS: OnceLock<(IpAddr, axum_server::tls_rustls::RustlsConfig)> = OnceLock::new();
/// Apps this process is serving on a port of their own: id -> port.
static LISTENING: Mutex<Option<HashMap<String, u16>>> = Mutex::new(None);

/// Called once the apps origin listens, with its address and certificate.
pub(crate) fn set_tls(ip: IpAddr, cfg: axum_server::tls_rustls::RustlsConfig) {
    if TLS.set((ip, cfg)).is_err() {
        tracing::warn!(measured = true, n_considered = 1, verdict = "app_port_tls_already_set",
            "the apps' certificate was set twice; the first one stays");
    }
}

/// The ports apps may get, or None when apps stay under `/app/<id>/`.
fn range() -> Option<(u16, u16)> {
    let home = crate::config::amux_home();
    if crate::api::settings::effective_env(&home, "AMUX_APPS_URL").is_some_and(|u| !u.trim().is_empty()) {
        return None;
    }
    let raw = crate::api::settings::effective_env(&home, "AMUX_APP_PORTS").unwrap_or_default();
    parse_range(raw.trim(), super::apps_origin::apps_port(crate::config::canonical_port()))
}

fn parse_range(raw: &str, apps_port: Option<u16>) -> Option<(u16, u16)> {
    match raw {
        "0" | "off" | "false" => None,
        "" => {
            let first = apps_port?.checked_add(1)?;
            Some((first, first.checked_add(DEFAULT_COUNT - 1)?))
        }
        r => {
            let (a, b) = r.split_once('-')?;
            let (a, b) = (a.trim().parse::<u16>().ok()?, b.trim().parse::<u16>().ok()?);
            (a > 0 && a <= b).then_some((a, b))
        }
    }
}

fn file(home: &Path) -> PathBuf {
    home.join(FILE)
}

fn load(home: &Path) -> BTreeMap<String, u16> {
    std::fs::read_to_string(file(home))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| serde_json::from_value(v["ports"].clone()).ok())
        .unwrap_or_default()
}

fn save(home: &Path, ports: &BTreeMap<String, u16>) {
    let body = serde_json::json!({ "ports": ports }).to_string();
    let tmp = file(home).with_extension(format!("tmp.{}", ulid::Ulid::new()));
    if let Err(error) = std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, file(home))) {
        let leftover = std::fs::remove_file(&tmp).err().map(|e| e.to_string()).unwrap_or_default();
        tracing::warn!(%error, %leftover, measured = true, n_considered = ports.len(), verdict = "app_port_save_failed",
            "the apps' ports could not be saved; an app may get a different port after a restart");
    }
}

/// The port app `id` is served on at `/`, binding it on first use. None means
/// the app opens under `/app/<id>/` instead, and the reason is logged.
pub(crate) fn port_for(id: &str) -> Option<u16> {
    let (lo, hi) = range()?;
    let Some((ip, tls)) = TLS.get().cloned() else {
        tracing::warn!(id, measured = true, n_considered = 1, verdict = "app_port_no_listener",
            "an app opened before the apps origin listened; it opens under /app/<id>/");
        return None;
    };
    let mut g = LISTENING.lock().unwrap_or_else(|e| e.into_inner());
    let live = g.get_or_insert_with(HashMap::new);
    if let Some(p) = live.get(id) {
        return Some(*p);
    }
    let home = crate::config::amux_home();
    let mut ports = load(&home);
    // Ports of apps no longer in Apps are free again.
    ports.retain(|k, _| k == id || super::public_links::approved_internal(k).is_some());
    let mut tried = Vec::new();
    let candidates: Vec<u16> = match ports.get(id) {
        Some(p) => vec![*p],
        None => (lo..=hi).filter(|p| !ports.values().any(|q| q == p)).collect(),
    };
    for p in candidates {
        match bind(ip, p) {
            Ok(l) => {
                ports.insert(id.to_string(), p);
                save(&home, &ports);
                live.insert(id.to_string(), p);
                spawn(id.to_string(), l, tls);
                tracing::info!(id, port = p, measured = true, n_considered = 1, verdict = "app_port_listening",
                    "an app inside amux is served at / on a port of its own");
                return Some(p);
            }
            Err(e) => tried.push(format!("{p}: {e}")),
        }
    }
    tracing::warn!(id, range = %format!("{lo}-{hi}"), tried = %tried.join("; "), measured = true, n_considered = tried.len(),
        verdict = "app_port_bind_failed", "no port of its own for an app; it opens under /app/<id>/, where some of it may not load");
    None
}

/// App `old` is now called `new`: it keeps its port, under the new name.
pub(crate) fn renamed(old: &str, new: &str) {
    let mut g = LISTENING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = g.get_or_insert_with(HashMap::new).remove(old) {
        g.get_or_insert_with(HashMap::new).insert(new.to_string(), p);
    }
    let home = crate::config::amux_home();
    let mut ports = load(&home);
    if let Some(p) = ports.remove(old) {
        ports.insert(new.to_string(), p);
        save(&home, &ports);
    }
}

/// At boot, serve again every app that had a port, so a tab left open on it
/// keeps working across a restart.
pub(crate) fn resume() {
    if range().is_none() {
        return;
    }
    let ids: Vec<String> = load(&crate::config::amux_home()).into_keys().collect();
    for id in ids {
        if super::public_links::approved_internal(&id).is_some() {
            // port_for logs a port it could not bind.
            port_for(&id);
        }
    }
}

fn bind(ip: IpAddr, port: u16) -> std::io::Result<std::net::TcpListener> {
    let l = std::net::TcpListener::bind(SocketAddr::new(ip, port))?;
    l.set_nonblocking(true)?;
    Ok(l)
}

fn spawn(id: String, l: std::net::TcpListener, tls: axum_server::tls_rustls::RustlsConfig) {
    let port = l.local_addr().map(|a| a.port()).unwrap_or(0);
    let app = Router::new().fallback(move |req: Request| {
        let id = id.clone();
        async move { super::apps_origin::serve_at_root(&id, req).await }
    });
    tokio::spawn(async move {
        if let Err(error) = axum_server::from_tcp_rustls(l, tls).serve(app.into_make_service()).await {
            tracing::warn!(port, %error, measured = true, n_considered = 1, verdict = "app_port_stopped",
                "an app's own port stopped listening");
        }
    });
}

/// Whether a request asks to become a WebSocket.
pub(crate) fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.get(header::UPGRADE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Relay a WebSocket to the app's loopback port: the app's own subprotocol
/// (Vite's `vite-hmr`) and its non-amux cookies go through, amux's do not.
pub(crate) async fn relay_ws(local: &str, port: u16, tail: &str, req: Request) -> Response {
    let (mut parts, _) = req.into_parts();
    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(ws) => ws,
        Err(e) => return e.into_response(),
    };
    let Ok(mut up_req) = format!("ws://{local}:{port}{tail}").into_client_request() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let h = up_req.headers_mut();
    for k in [header::SEC_WEBSOCKET_PROTOCOL, header::ORIGIN, header::USER_AGENT] {
        if let Some(v) = parts.headers.get(&k) {
            h.insert(k, v.clone());
        }
    }
    if let Some(c) = app_relay::strip_amux_cookies(&parts.headers).and_then(|c| c.parse().ok()) {
        h.insert(header::COOKIE, c);
    }
    let (upstream, resp) = match tokio_tungstenite::connect_async(up_req).await {
        Ok(x) => x,
        Err(error) => {
            tracing::warn!(port, %error, measured = true, n_considered = 1, verdict = "app_ws_relay_failed",
                "an app's WebSocket could not reach the app");
            return (StatusCode::BAD_GATEWAY, Body::from("The app did not accept the WebSocket.\n")).into_response();
        }
    };
    let chosen = resp.headers().get(header::SEC_WEBSOCKET_PROTOCOL).and_then(|v| v.to_str().ok()).map(str::to_string);
    let ws = match chosen {
        Some(p) => ws.protocols([p]),
        None => ws,
    };
    ws.max_message_size(64 << 20).on_upgrade(move |client| pump(client, upstream))
}

async fn pump(client: WebSocket, upstream: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>) {
    let (mut up_tx, mut up_rx) = upstream.split();
    let (mut cl_tx, mut cl_rx) = client.split();
    let to_app = async {
        while let Some(Ok(msg)) = cl_rx.next().await {
            let out = match msg {
                AxMsg::Text(t) => TMsg::Text(t.as_str().to_string()),
                AxMsg::Binary(b) => TMsg::Binary(b.to_vec()),
                AxMsg::Ping(b) => TMsg::Ping(b.to_vec()),
                AxMsg::Pong(b) => TMsg::Pong(b.to_vec()),
                AxMsg::Close(_) => break,
            };
            if up_tx.send(out).await.is_err() {
                break;
            }
        }
    };
    let to_browser = async {
        while let Some(Ok(msg)) = up_rx.next().await {
            let out = match msg {
                TMsg::Text(t) => AxMsg::Text(t.into()),
                TMsg::Binary(b) => AxMsg::Binary(b.into()),
                TMsg::Ping(b) => AxMsg::Ping(b.into()),
                TMsg::Pong(b) => AxMsg::Pong(b.into()),
                TMsg::Close(_) => break,
                TMsg::Frame(_) => continue,
            };
            if cl_tx.send(out).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = to_app => {},
        _ = to_browser => {},
    }
}

/// `https://host:8825` with the port replaced.
pub(crate) fn on_port(base: &str, port: u16) -> String {
    match base.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) && !h.ends_with('/') && h.contains("://") => format!("{h}:{port}"),
        _ => format!("{}:{port}", base.trim_end_matches('/')),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_range_defaults_after_the_apps_port_and_can_be_named_or_turned_off() {
        assert_eq!(parse_range("", Some(8825)), Some((8826, 8857)));
        assert_eq!(parse_range("9100-9110", Some(8825)), Some((9100, 9110)));
        assert_eq!(parse_range("0", Some(8825)), None);
        assert_eq!(parse_range("", None), None, "no apps origin, no app ports");
        assert_eq!(parse_range("9110-9100", Some(8825)), None);
        assert_eq!(parse_range("x", Some(8825)), None);
    }

    #[test]
    fn the_handoff_keeps_the_host_and_takes_the_apps_port() {
        assert_eq!(on_port("https://desktop.tail5ce8f5.ts.net:8825", 8826), "https://desktop.tail5ce8f5.ts.net:8826");
        assert_eq!(on_port("https://[::1]:8825", 8830), "https://[::1]:8830");
        assert_eq!(on_port("https://localhost", 8830), "https://localhost:8830");
    }
}
