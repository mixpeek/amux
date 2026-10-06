//! `/api/sessions/{name}/typing` — who else is typing to this worker right now.
//!
//! Multiplayer presence for the composer (AC-474): when two people share an
//! amux (invited members on a dedicated host), each one sees "elliot is
//! typing…" on the worker the other is writing to, so they do not send
//! crossing instructions.
//!
//! EPHEMERAL ON PURPOSE. Typing state lives in process memory with a short
//! TTL and is never written to the store: a keystroke stream through the
//! single writer would be thousands of rows a day that mean nothing a few
//! seconds later (ethos rule 5). It reaches clients on the SSE stream as
//! `{"type":"typing",...}` through its own broadcast channel, beside the
//! revisioned store events (sse.rs selects on both).
//!
//! No polling fallback, deliberately: a client that has fallen back to
//! polling simply shows no indicator. The client also expires every entry
//! after [`TTL`] unless it is refreshed, so a missed "stopped" event can
//! leave an indicator up for at most that long, never indefinitely.
//!
//! Identity is the server's, not the client's: a verified invited member is
//! `member:<email>`; otherwise the caller's lane header, otherwise `owner`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::AppState;

/// How long one "typing" ping stays true. Clients refresh every few seconds
/// while keys are moving, so this only bounds a vanished client.
pub const TTL: Duration = Duration::from_secs(8);

type Registry = HashMap<String, HashMap<String, Instant>>;

fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The broadcast every SSE connection subscribes to. Payloads are complete
/// JSON event bodies.
pub fn channel() -> &'static tokio::sync::broadcast::Sender<String> {
    static TX: OnceLock<tokio::sync::broadcast::Sender<String>> = OnceLock::new();
    TX.get_or_init(|| tokio::sync::broadcast::channel(256).0)
}

/// Who is acting, from server-verified identity first.
pub(crate) fn actor(headers: &HeaderMap) -> String {
    if let Some(a) = super::org::local_member_actor(headers) {
        return if a.starts_with("member:") { a.to_string() } else { format!("member:{a}") };
    }
    for k in ["x-amux-worker", "x-amux-session"] {
        if let Some(v) = headers.get(k).and_then(|v| v.to_str().ok()).map(str::trim) {
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    "owner".to_string()
}

/// A short human label: the email's local part for members, else the actor.
fn label(actor: &str) -> String {
    let a = actor.strip_prefix("member:").unwrap_or(actor);
    a.split('@').next().unwrap_or(a).to_string()
}

fn live(session: &str, now: Instant) -> Vec<Value> {
    let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    let Some(who) = reg.get_mut(session) else { return vec![] };
    who.retain(|_, exp| *exp > now);
    let mut out: Vec<Value> = who
        .keys()
        .map(|a| json!({"actor": a, "label": label(a)}))
        .collect();
    out.sort_by(|a, b| a["actor"].as_str().cmp(&b["actor"].as_str()));
    if who.is_empty() {
        reg.remove(session);
    }
    out
}

/// Record or clear one actor's typing state. Returns whether the visible set
/// changed (logged, and asserted by the tests).
fn set(session: &str, actor: &str, active: bool, now: Instant) -> bool {
    let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    let who = reg.entry(session.to_string()).or_default();
    who.retain(|_, exp| *exp > now);
    let was = who.contains_key(actor);
    if active {
        who.insert(actor.to_string(), now + TTL);
    } else {
        who.remove(actor);
    }
    was != active
}

#[derive(Deserialize)]
struct TypingBody {
    #[serde(default = "yes")]
    active: bool,
}
fn yes() -> bool {
    true
}

fn valid_session(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

async fn post_typing(
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Option<Json<TypingBody>>,
) -> Response {
    if !valid_session(&name) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid session name"}))).into_response();
    }
    let active = body.map(|b| b.active).unwrap_or(true);
    let who_acts = actor(&headers);
    let now = Instant::now();
    let changed = set(&name, &who_acts, active, now);
    let who = live(&name, now);
    // Every ping is broadcast, refreshes included: a viewer expires an entry
    // after TTL on its own clock, so a long stretch of typing must keep
    // re-arming it. Clients throttle to one ping per few seconds, so this is
    // one small event per typing person per few seconds.
    {
        let ev = json!({
            "type": "typing",
            "session": name,
            "who": who,
            "ttl_ms": TTL.as_millis() as u64,
        });
        let receivers = channel().send(ev.to_string()).unwrap_or(0);
        tracing::debug!(session = %name, actor = %who_acts, active, changed, receivers, verdict = "typing_presence", "typing presence");
    }
    Json(json!({
        "ok": true,
        "session": name,
        "actor": who_acts,
        "active": active,
        "who": who,
        "ttl_ms": TTL.as_millis() as u64,
    }))
    .into_response()
}

async fn get_typing(Path(name): Path<String>) -> Response {
    if !valid_session(&name) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid session name"}))).into_response();
    }
    let who = live(&name, Instant::now());
    Json(json!({
        "session": name,
        "who": who,
        "measured": true,
        "n_considered": who.len(),
        "ttl_ms": TTL.as_millis() as u64,
    }))
    .into_response()
}

/// One connection's view of a typing event: the same event minus the
/// viewer's own entry. A person must never see themself typing, and the
/// client cannot filter reliably because its own ping's echo can arrive
/// before the ping's response tells it who it is (seen live 2026-10-06).
/// `None` means the payload was not a typing event and passes through.
pub(crate) fn without_viewer(payload: &str, viewer: &str) -> Option<String> {
    let mut v: Value = serde_json::from_str(payload).ok()?;
    let who = v.get_mut("who")?.as_array_mut()?;
    who.retain(|w| w.get("actor").and_then(Value::as_str) != Some(viewer));
    Some(v.to_string())
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/sessions/{name}/typing", get(get_typing).post(post_typing))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_reports_change_only_on_visible_transitions_and_expires() {
        let s = "typing-test-a";
        let t0 = Instant::now();
        assert!(set(s, "member:e@x.com", true, t0), "first start is a change");
        assert!(!set(s, "member:e@x.com", true, t0 + Duration::from_secs(1)), "refresh is not");
        assert_eq!(live(s, t0 + Duration::from_secs(2)).len(), 1);
        assert_eq!(live(s, t0 + TTL + Duration::from_secs(2)).len(), 0, "expires without a refresh");
        assert!(set(s, "member:e@x.com", true, t0 + TTL + Duration::from_secs(3)));
        assert!(set(s, "member:e@x.com", false, t0 + TTL + Duration::from_secs(4)), "stop is a change");
        assert!(live(s, t0 + TTL + Duration::from_secs(4)).is_empty());
    }

    #[test]
    fn a_viewer_never_receives_their_own_entry() {
        let ev = json!({"type": "typing", "session": "w", "who": [
            {"actor": "member:a@x.com", "label": "a"}, {"actor": "member:b@x.com", "label": "b"}]}).to_string();
        let seen: Value = serde_json::from_str(&without_viewer(&ev, "member:a@x.com").unwrap()).unwrap();
        let actors: Vec<&str> = seen["who"].as_array().unwrap().iter().map(|w| w["actor"].as_str().unwrap()).collect();
        assert_eq!(actors, vec!["member:b@x.com"]);
    }

    #[test]
    fn label_is_the_email_local_part() {
        assert_eq!(label("member:ejdaniel@wexusllc.com"), "ejdaniel");
        assert_eq!(label("owner"), "owner");
        assert_eq!(label("amux-cloud"), "amux-cloud");
    }

    #[test]
    fn rejects_path_like_session_names() {
        assert!(valid_session("ed-payment-matcher"));
        assert!(!valid_session("../x"));
        assert!(!valid_session(""));
    }
}
