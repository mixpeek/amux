//! CDP only when a route cannot do it, and never unseen.
//!
//! Ethan (2026-10-27): "i prefer using amux browser and only cdp if we cannot
//! accomplish". `amux browser route` is the way a worker drives a browser, and
//! every route verb is audited. A raw Chrome debugging port bypassed all of
//! it: a worker that read `cdp_port` from `/api/browser/status` and connected
//! a websocket could navigate, evaluate and type with no record anywhere.
//!
//! So `cdp_port` is withheld unless the caller says why the route cannot do
//! the job (`cdp_reason`), and what it then receives is the port of a proxy,
//! not Chrome's. The proxy forwards everything (the `/json` discovery
//! endpoints with their URLs rewritten, and the devtools websockets), so
//! Playwright's `connectOverCDP` and the chrome-cdp skill work unchanged. It
//! records, into the same `session_events` ledger as route verbs:
//! - `browser.cdp.granted`: who, which profile, and the stated reason;
//! - `browser.cdp.command`: each command that acts on a page (navigate,
//!   evaluate, input, cookies, targets, downloads), values redacted the same
//!   way route actions are (lengths, never typed text or code);
//! - `browser.cdp.session`: on disconnect, the count of every method sent.
//!
//! One proxy per (worker, Chrome port), reused across grants.

use super::AppState;
use axum::body::Body;
use axum::extract::ws::{Message as AxMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use tokio_tungstenite::tungstenite::Message as TMsg;

/// Shortest reason accepted. Long enough that "x" or "need it" does not pass
/// as an explanation; the reason is recorded, not judged.
pub const MIN_REASON_CHARS: usize = 12;

pub const WITHHELD_NOTE: &str = "cdp_port is withheld: drive the browser with `amux browser route` (state, shot, action). \
Only if a route cannot do the job, ask again with cdp_reason=<why the route cannot do it>; you will get a logged CDP proxy port.";

#[derive(Clone)]
struct Proxy {
    state: AppState,
    session: String,
    profile: String,
    upstream: u16,
    port: u16,
}

fn proxies() -> &'static Mutex<HashMap<(String, u16), u16>> {
    static P: OnceLock<Mutex<HashMap<(String, u16), u16>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

fn is_proxy_port(port: u16) -> bool {
    proxies().lock().map(|m| m.values().any(|p| *p == port)).unwrap_or(false)
}

/// A usable reason, trimmed and capped, or None.
pub fn reason(raw: Option<&str>) -> Option<String> {
    let r = raw.map(str::trim).unwrap_or("");
    (r.chars().count() >= MIN_REASON_CHARS).then(|| super::session_verbs::redact_secrets(r).chars().take(300).collect())
}

/// The proxy port for `session` driving Chrome on `upstream`, starting one if
/// needed, and the grant recorded. None when no listener could be bound.
pub async fn grant(state: &AppState, session: &str, profile: &str, upstream: u16, why: &str) -> Option<u16> {
    let existing = proxies().lock().ok().and_then(|m| m.get(&(session.to_string(), upstream)).copied());
    let port = match existing {
        Some(p) => p,
        None => {
            let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(target: "amux::browser_audit", session, upstream, error = %e, verdict = "cdp_proxy_unavailable",
                        "could not start a CDP proxy; the raw port stays withheld");
                    return None;
                }
            };
            let port = listener.local_addr().ok()?.port();
            let p = Proxy { state: state.clone(), session: session.to_string(), profile: profile.to_string(), upstream, port };
            let app = Router::new()
                .route("/devtools/{*rest}", get(ws_upgrade))
                .fallback(discovery)
                .with_state(p);
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            if let Ok(mut m) = proxies().lock() {
                m.insert((session.to_string(), upstream), port);
            }
            port
        }
    };
    super::browser::record_browser_event(state, Some(session), session, "cdp.granted",
        json!({"profile": profile, "reason": why, "proxy_port": port, "sensitive_values_recorded": false})).await;
    tracing::info!(target: "amux::browser_audit", session, profile, proxy_port = port, verdict = "cdp_granted",
        "a worker was given a logged CDP proxy port");
    Some(port)
}

/// Rewrite every `cdp_port` at `pointers` in a response for `session`: a
/// logged proxy port when `why` is a usable reason, otherwise null, with a
/// note saying how to drive the browser instead. `profile_at` names where the
/// profile for each port sits (same index), for the grant record.
pub async fn gate(state: &AppState, session: &str, why: Option<&str>, v: &mut Value, pointers: &[(&str, &str)]) {
    let why = reason(why);
    let mut withheld = 0;
    for (port_ptr, profile_ptr) in pointers {
        let Some(up) = v.pointer(port_ptr).and_then(Value::as_u64).filter(|p| *p > 0 && *p <= u16::MAX as u64) else { continue };
        // Already one of our proxies (a route's native start passed through
        // /api/browser/start, which gated it): leave it, never proxy a proxy.
        if is_proxy_port(up as u16) {
            continue;
        }
        let profile = v.pointer(profile_ptr).and_then(Value::as_str).unwrap_or("").to_string();
        let replacement = match &why {
            Some(w) => grant(state, session, &profile, up as u16, w).await.map(Value::from).unwrap_or(Value::Null),
            None => {
                withheld += 1;
                Value::Null
            }
        };
        if let Some(slot) = v.pointer_mut(port_ptr) {
            *slot = replacement;
        }
        // Who shares a Chrome is still answerable without the port: an opaque
        // reference, equal for the same browser.
        let parent = port_ptr.rsplit_once('/').map(|(a, _)| a).unwrap_or("");
        let target = if parent.is_empty() { Some(&mut *v) } else { v.pointer_mut(parent) };
        if let Some(o) = target.and_then(Value::as_object_mut) {
            o.insert("browser_ref".into(), json!(browser_ref(up as u16)));
        }
    }
    if withheld > 0 {
        if let Some(o) = v.as_object_mut() {
            o.insert("cdp".into(), json!(WITHHELD_NOTE));
        }
        tracing::info!(target: "amux::browser_audit", session, withheld, verdict = "cdp_port_withheld",
            "a browser response withheld the raw CDP port (no cdp_reason)");
    }
}

/// `/json/*` and anything else that is not a websocket: forwarded, with
/// Chrome's own host:port rewritten to the proxy so clients reconnect here.
async fn discovery(State(p): State<Proxy>, method: Method, uri: Uri, body: axum::body::Bytes) -> Response {
    let url = format!("http://127.0.0.1:{}{}", p.upstream, uri.path_and_query().map(|x| x.as_str()).unwrap_or("/"));
    let client = reqwest::Client::new();
    let m = reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let resp = match client.request(m, &url).body(body.to_vec()).send().await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("browser not reachable through the CDP proxy: {e}")).into_response(),
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let text = resp.text().await.unwrap_or_default();
    (status, [("content-type", "application/json")], Body::from(rewrite_ports(&text, p.upstream, p.port))).into_response()
}

/// An opaque, stable name for the Chrome on `upstream`. Not a secret (the
/// port is visible to `ps` on this host); withholding is a default that
/// routes work through audited verbs, not an access control.
pub fn browser_ref(upstream: u16) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ("amux-browser", upstream).hash(&mut h);
    format!("b{:012x}", h.finish() & 0xffff_ffff_ffff)
}

pub fn rewrite_ports(text: &str, upstream: u16, proxy: u16) -> String {
    text.replace(&format!("127.0.0.1:{upstream}"), &format!("127.0.0.1:{proxy}"))
        .replace(&format!("localhost:{upstream}"), &format!("127.0.0.1:{proxy}"))
}

async fn ws_upgrade(State(p): State<Proxy>, Path(rest): Path<String>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(256 << 20).on_upgrade(move |socket| relay(p, rest, socket))
}

async fn relay(p: Proxy, rest: String, client: WebSocket) {
    let url = format!("ws://127.0.0.1:{}/devtools/{rest}", p.upstream);
    let upstream = match tokio_tungstenite::connect_async(&url).await {
        Ok((s, _)) => s,
        Err(e) => {
            tracing::warn!(target: "amux::browser_audit", session = %p.session, error = %e, verdict = "cdp_proxy_upstream_failed",
                "the CDP proxy could not reach the browser");
            return;
        }
    };
    let started = std::time::Instant::now();
    let (mut up_tx, mut up_rx) = upstream.split();
    let (mut cl_tx, mut cl_rx) = client.split();
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let to_browser = async {
        while let Some(Ok(msg)) = cl_rx.next().await {
            let out = match msg {
                AxMsg::Text(t) => {
                    if let Some((method, ev)) = command_audit(t.as_str()) {
                        *counts.entry(method).or_default() += 1;
                        if let Some(ev) = ev {
                            super::browser::record_browser_event(&p.state, Some(&p.session), &p.session, "cdp.command",
                                json!({"profile": p.profile, "via": "cdp_proxy", "command": ev, "sensitive_values_recorded": false})).await;
                        }
                    }
                    TMsg::Text(t.as_str().to_string())
                }
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
    let to_client = async {
        while let Some(Ok(msg)) = up_rx.next().await {
            let out = match msg {
                TMsg::Text(t) => AxMsg::Text(t.as_str().to_string().into()),
                TMsg::Binary(b) => AxMsg::Binary(b.to_vec().into()),
                TMsg::Ping(b) => AxMsg::Ping(b.to_vec().into()),
                TMsg::Pong(b) => AxMsg::Pong(b.to_vec().into()),
                TMsg::Close(_) => break,
                TMsg::Frame(_) => continue,
            };
            if cl_tx.send(out).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = to_browser => {},
        _ = to_client => {},
    }
    let total: u64 = counts.values().sum();
    super::browser::record_browser_event(&p.state, Some(&p.session), &p.session, "cdp.session",
        json!({"profile": p.profile, "via": "cdp_proxy", "commands": total, "by_method": counts,
               "duration_ms": started.elapsed().as_millis() as u64, "sensitive_values_recorded": false})).await;
}

/// (method, Some(record)) for a command worth its own audit row, (method,
/// None) for one only counted, None for anything that is not a command.
/// Values are reduced to what a triage needs: URLs through the same redactor
/// as route actions, and lengths instead of typed text, code or cookie values.
pub fn command_audit(text: &str) -> Option<(String, Option<Value>)> {
    let v: Value = serde_json::from_str(text).ok()?;
    let method = v.get("method")?.as_str()?.to_string();
    let p = v.get("params").cloned().unwrap_or(Value::Null);
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("");
    let len = |k: &str| p.get(k).and_then(Value::as_str).map(|x| x.chars().count()).unwrap_or(0);
    let rec = match method.as_str() {
        "Page.navigate" | "Target.createTarget" => Some(json!({"method": method, "url": super::browser::audit_url(s("url"))})),
        "Runtime.evaluate" => Some(json!({"method": method, "expression_chars": len("expression")})),
        "Runtime.callFunctionOn" => Some(json!({"method": method, "function_chars": len("functionDeclaration")})),
        "Input.insertText" => Some(json!({"method": method, "typed_chars": len("text")})),
        "Input.dispatchKeyEvent" => {
            // A printable key is typed text: record that a key went, not which.
            let key = s("key");
            let named = key.chars().count() > 1;
            if p.get("type").and_then(Value::as_str) != Some("keyDown") && p.get("type").and_then(Value::as_str) != Some("rawKeyDown") {
                None
            } else {
                Some(json!({"method": method, "key": if named { json!(key) } else { Value::Null }}))
            }
        }
        "Input.dispatchMouseEvent" => (s("type") == "mousePressed")
            .then(|| json!({"method": method, "x": p.get("x"), "y": p.get("y"), "button": p.get("button")})),
        "Network.setCookie" | "Network.deleteCookies" => Some(json!({"method": method, "name": s("name"), "domain": s("domain")})),
        "Network.setCookies" | "Storage.setCookies" => Some(json!({"method": method,
            "count": p.get("cookies").and_then(Value::as_array).map(Vec::len).unwrap_or(0)})),
        "Network.clearBrowserCookies" | "Storage.clearCookies" | "Storage.clearDataForOrigin" | "Target.closeTarget"
        | "Page.reload" | "Browser.setDownloadBehavior" | "Page.setDownloadBehavior" | "Fetch.enable"
        | "Network.setExtraHTTPHeaders" | "Emulation.setUserAgentOverride" => Some(json!({"method": method})),
        _ => None,
    };
    Some((method, rec))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_acting_commands_are_recorded_without_their_values() {
        let nav = command_audit(r#"{"id":1,"method":"Page.navigate","params":{"url":"https://app.example/x?token=abc123secret"}}"#).unwrap();
        assert_eq!(nav.0, "Page.navigate");
        let s = nav.1.unwrap().to_string();
        assert!(s.contains("app.example") && !s.contains("abc123secret"), "{s}");

        let ev = command_audit(r#"{"id":2,"method":"Runtime.evaluate","params":{"expression":"document.cookie + 'private'"}}"#).unwrap();
        let s = ev.1.unwrap().to_string();
        assert!(!s.contains("cookie") && s.contains("\"expression_chars\":27"), "{s}");

        let typed = command_audit(r#"{"id":3,"method":"Input.insertText","params":{"text":"hunter2pw"}}"#).unwrap().1.unwrap();
        assert_eq!(typed["typed_chars"], 9);
        assert!(!typed.to_string().contains("hunter2"));

        let printable = command_audit(r#"{"id":4,"method":"Input.dispatchKeyEvent","params":{"type":"keyDown","key":"p"}}"#).unwrap().1.unwrap();
        assert!(printable["key"].is_null(), "a printable key is typed text");
        let named = command_audit(r#"{"id":5,"method":"Input.dispatchKeyEvent","params":{"type":"keyDown","key":"Enter"}}"#).unwrap().1.unwrap();
        assert_eq!(named["key"], "Enter");
        assert!(command_audit(r#"{"id":6,"method":"Input.dispatchKeyEvent","params":{"type":"keyUp","key":"Enter"}}"#).unwrap().1.is_none());

        let cookie = command_audit(r#"{"id":7,"method":"Network.setCookie","params":{"name":"sid","value":"SECRETVALUE","domain":"x.com"}}"#).unwrap().1.unwrap();
        assert!(!cookie.to_string().contains("SECRETVALUE"));

        let counted = command_audit(r#"{"id":8,"method":"DOM.getDocument","params":{}}"#).unwrap();
        assert_eq!((counted.0.as_str(), counted.1.is_none()), ("DOM.getDocument", true), "read-only commands are counted, not rowed");
        assert!(command_audit(r#"{"method":"Page.loadEventFired","params":{}}"#).is_some_and(|c| c.1.is_none()));
        assert!(command_audit("not json").is_none());
    }

    #[test]
    fn discovery_urls_point_back_at_the_proxy_and_a_reason_must_say_something() {
        let body = r#"{"webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/browser/x","devtoolsFrontendUrl":"/devtools/inspector.html?ws=localhost:9222/devtools/page/1"}"#;
        let out = rewrite_ports(body, 9222, 40001);
        assert!(!out.contains("9222") && out.matches("127.0.0.1:40001").count() == 2, "{out}");
        assert!(reason(Some("need it")).is_none());
        assert!(reason(None).is_none());
        assert!(reason(Some("  the route has no file-chooser interception for this upload  ")).is_some());
    }
}
