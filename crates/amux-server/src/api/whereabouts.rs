//! What each person has open in amux right now: one entry per browser tab or
//! app window, with the screen it shows (a view, a worker, a conversation).
//!
//! Ethan, 2026-10-29: "click a users avatar and know what screen(s)/window(s)
//! are active for them ... anytime i can view avatars/users". Every dashboard
//! tab reports where it is (`POST /api/presence/where`) when it moves and every
//! 30 s; clicking any avatar asks `GET /api/presence/where?key=person:<email>`.
//!
//! EPHEMERAL ON PURPOSE, like typing presence (typing.rs): process memory with
//! a TTL, never the store. A restart forgets everyone until their next report,
//! at most 30 s later.
//!
//! Who sees what: a person always sees their own windows; a person with
//! workspace-wide access sees anyone's in full; a member scoped to a group or
//! worker sees only how many windows someone has and whether one is in front,
//! because a label can name a worker outside their scope. Workers see nothing.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::RawQuery;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use super::AppState;

/// A window not heard from in this long is closed (it reports every 30 s).
pub const TTL: Duration = Duration::from_secs(90);

#[derive(Clone, Debug)]
struct Window {
    view: String,
    label: String,
    worker: Option<String>,
    focused: bool,
    device: String,
    at: Instant,
}

type Registry = HashMap<String, HashMap<String, Window>>;

fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/presence/where", get(read).post(report))
}

fn clip(v: &Value, max: usize) -> String {
    v.as_str().unwrap_or("").trim().chars().take(max).collect()
}

/// "iPhone · Safari", "Mac · Chrome": enough to tell two windows apart.
pub(crate) fn device_of(ua: &str) -> String {
    let os = if ua.contains("iPhone") {
        "iPhone"
    } else if ua.contains("iPad") {
        "iPad"
    } else if ua.contains("Android") {
        "Android"
    } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
        "Mac"
    } else if ua.contains("Windows") {
        "Windows"
    } else if ua.contains("Linux") {
        "Linux"
    } else {
        "Device"
    };
    let browser = if ua.contains("amux-desktop") || ua.contains("Electron") {
        "amux app"
    } else if ua.contains("Edg/") {
        "Edge"
    } else if ua.contains("Firefox/") {
        "Firefox"
    } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else {
        "browser"
    };
    format!("{os} · {browser}")
}

async fn report(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let Some((key, _)) = super::conversations::person_of(&headers) else {
        return (StatusCode::FORBIDDEN, Json(json!({"ok": false, "error": "only a person's window reports where it is"}))).into_response();
    };
    let tab = clip(&body["tab"], 64);
    if tab.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": "tab required"}))).into_response();
    }
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    let mine = reg.entry(key.clone()).or_default();
    if body["closed"].as_bool() == Some(true) {
        mine.remove(&tab);
    } else {
        mine.insert(tab, Window {
            view: clip(&body["view"], 40),
            label: clip(&body["label"], 160),
            worker: Some(clip(&body["worker"], 80)).filter(|w| !w.is_empty()),
            focused: body["focused"].as_bool().unwrap_or(false),
            device: device_of(ua),
            at: Instant::now(),
        });
    }
    reg.values_mut().for_each(|m| m.retain(|_, w| w.at.elapsed() < TTL));
    reg.retain(|_, m| !m.is_empty());
    Json(json!({"ok": true, "key": key})).into_response()
}

/// `person:<email>` (or `person:owner`) from a key the client holds, which may
/// be a bare email.
fn person_key(raw: &str) -> String {
    let k = raw.trim().to_ascii_lowercase();
    if k.starts_with("person:") { k } else { format!("person:{k}") }
}

async fn read(headers: HeaderMap, RawQuery(raw): RawQuery) -> Response {
    let Some((me, global)) = super::conversations::person_of(&headers) else {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "only a person can see where people are"}))).into_response();
    };
    let q: HashMap<String, String> = super::fs::parse_qs(raw.as_deref().unwrap_or("")).into_iter().collect();
    let Some(raw_key) = q.get("key").filter(|k| !k.trim().is_empty()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "key required (person:<email>)"}))).into_response();
    };
    let key = person_key(raw_key);
    let full = global || key == me;
    let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    let mut wins: Vec<&Window> = reg.get(&key).map(|m| m.values().filter(|w| w.at.elapsed() < TTL).collect()).unwrap_or_default();
    wins.sort_by_key(|w| (!w.focused, w.at.elapsed()));
    let windows: Vec<Value> = wins.iter().map(|w| if full {
        json!({"view": w.view, "label": w.label, "worker": w.worker, "focused": w.focused,
               "device": w.device, "age_s": w.at.elapsed().as_secs()})
    } else {
        json!({"focused": w.focused, "age_s": w.at.elapsed().as_secs()})
    }).collect();
    Json(json!({"key": key, "measured": true, "n_considered": windows.len(), "detail": full,
                "active": windows.iter().any(|w| w["focused"].as_bool() == Some(true)),
                "windows": windows, "ttl_s": TTL.as_secs()})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_reads_as_os_and_browser() {
        assert_eq!(device_of("Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit Version/18.0 Mobile Safari/604.1"), "iPhone · Safari");
        assert_eq!(device_of("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit Chrome/130.0 Safari/537.36"), "Mac · Chrome");
        assert_eq!(device_of("Mozilla/5.0 (Windows NT 10.0) Gecko/20100101 Firefox/131.0"), "Windows · Firefox");
        assert_eq!(device_of(""), "Device · browser");
    }

    #[test]
    fn a_bare_email_names_the_person() {
        assert_eq!(person_key("Doron@X.com"), "person:doron@x.com");
        assert_eq!(person_key("person:owner"), "person:owner");
    }
}
