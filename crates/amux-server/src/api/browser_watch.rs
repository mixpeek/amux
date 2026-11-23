//! `/api/browser/watch`: learn a web task by watching the owner do it once
//! (owner, 2026-11-22, from research/athena-mac-app-teardown.md: Athena's
//! page subscriptions record the person's clicks with secret fields redacted).
//!
//! - `POST /start {session?}`  start recording the session's tab
//! - `GET  /?session=`         the steps so far: page, click, fill, key, submit
//! - `POST /stop {session?}`   stop, and return the steps
//!
//! The owner drives the amux browser (its window, or the dashboard's Browser
//! tab); a worker reads the steps and repeats or adapts them with its own
//! model (A2: amux records, it does not turn steps into a script).
//!
//! How: one CDP connection per watch. The recorder reports each step as a
//! marked `console.debug`, which arrives on that connection as a
//! `Runtime.consoleAPICalled` event, and `Page.addScriptToEvaluateOnNewDocument` installs the recorder
//! in every page the tab loads, so a click that navigates is still recorded
//! (the event is queued on the socket before the page unloads). The loop
//! collects events every 300 ms.
//!
//! Redaction happens IN THE PAGE, before anything leaves it: a password
//! field (or one that was a password field at any point while focused), or a
//! field whose name, id, autocomplete or label reads like a secret (password,
//! token, code, MFA, API key, credential, card number, CVC, SSN, PIN), is
//! recorded as "[redacted]". A value or click text that looks like a token or
//! a card number is redacted too. Page and link URLs keep their path, drop a
//! fragment that carries data, and redact query values whose key reads like
//! a secret or whose value looks like a token (callback codes, magic links).
//! Keys are recorded only for Enter, Escape and Tab.
//!
//! Who: a worker watches its own tab, the one its X-Amux-Session header names
//! (the identity middleware checks that header against the worker's token).
//! A body or query `session` naming another worker is refused. Without a
//! worker header only the owner may watch, and may name any session (the
//! default, "amux", is the owner's own tab). amux-security, 2026-11-23: the
//! body used to win over the header and the default was the owner's tab, so
//! any worker could start, read or stop a watch on any tab.

use super::AppState;
use crate::integrations::browser as chrome;
use axum::extract::{OriginalUri, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Steps travel as console.debug messages with this prefix plus a per-watch
/// nonce, which every page has natively. A CDP binding (Runtime.addBinding) was tried first: after a
/// click took the tab to another origin's renderer, the tab stopped answering
/// for about 40 seconds while a watch held it.
const MARK: &str = "__amuxwatch:";
const MAX_STEPS: usize = 1000;
const T: Duration = Duration::from_secs(8);

/// Finished watches kept for reading back, counted from when they stopped,
/// before they are evicted.
const KEEP_FINISHED_S: i64 = 3600;
/// Stands for the watch's nonce in [`RECORDER`]; see [`recorder`].
const NONCE: &str = "__AMUX_WATCH_NONCE__";

/// The recorder, run in every document of the watched tab. `console.debug` is
/// captured when the recorder installs, which on a new document is before any
/// page script runs, so a page cannot read the nonce by wrapping it later.
pub const RECORDER: &str = r#"(() => {
  if (window.__amuxWatchOn) return; window.__amuxWatchOn = true;
  const SECRET = /pass|pwd|secret|token|otp|one.?time|(^|[^a-z])(code|pin|mfa|2fa|totp)([^a-z]|$)|api.?key|access.?key|private.?key|credential|auth(?!or)|verif|cvc|cvv|card.?num|cc-?(num|csc|exp)|ssn|security.?code/i;
  const QKEY = /code|token|key|secret|pass|pwd|auth|sig|ticket|session|sid|jwt|nonce|state|otp|magic|login|reset|invite|credential/i;
  const TOKENISH = (v) => /^[A-Za-z0-9_\-.~+\/=%]{20,}$/.test(v) && /[0-9]/.test(v) && /[A-Za-z]/.test(v);
  const CARDISH = (v) => /^[0-9][0-9 -]{11,22}$/.test(v) && v.replace(/\D/g, '').length >= 12;
  const scrub = (v) => { const s = String(v || ''); return (TOKENISH(s.trim()) || CARDISH(s.trim())) ? '[redacted]' : s; };
  const cleanUrl = (u) => { try { const x = new URL(u, location.href); let hit = false;
      for (const [k, v] of [...x.searchParams]) if (QKEY.test(k) || TOKENISH(v)) { x.searchParams.set(k, '[redacted]'); hit = true; }
      if (x.hash && (/[=&]/.test(x.hash) || x.hash.length > 40)) { x.hash = '#[redacted]'; hit = true; }
      return hit ? x.href.replace(/%5Bredacted%5D/g, '[redacted]') : x.href; } catch (e) { return '[unparsed url]'; } };
  const dbg = console.debug.bind(console);
  const send = (o) => { if (window.__amuxWatchOff) return;
    try { dbg('__amuxwatch:__AMUX_WATCH_NONCE__:' + JSON.stringify(Object.assign({ t: Date.now(), url: cleanUrl(location.href), title: document.title }, o))); } catch (e) {} };
  const sel = (el) => {
    if (el.id) return '#' + CSS.escape(el.id);
    const parts = []; let e = el;
    while (e && e.nodeType === 1 && parts.length < 4) {
      let s = e.tagName.toLowerCase();
      if (e.getAttribute('name')) s += '[name="' + e.getAttribute('name') + '"]';
      else { const sib = [...((e.parentNode && e.parentNode.children) || [])].filter(x => x.tagName === e.tagName);
             if (sib.length > 1) s += ':nth-of-type(' + (sib.indexOf(e) + 1) + ')'; }
      parts.unshift(s);
      e = e.parentElement;
      if (e && e.id) { parts.unshift('#' + CSS.escape(e.id)); break; }
    }
    return parts.join(' > ');
  };
  const label = (el) => (el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.getAttribute('title')
    || (el.labels && el.labels[0] && el.labels[0].innerText) || '').trim();
  const desc = (el) => {
    if (!el || !el.tagName) return {};
    const r = { tag: el.tagName.toLowerCase(), selector: sel(el) };
    const role = el.getAttribute('role'); if (role) r.role = role;
    const isField = /^(input|textarea|select)$/i.test(el.tagName);
    const text = isField ? '' : (el.innerText || '').trim().replace(/\s+/g, ' ');
    r.name = scrub((label(el) || text).slice(0, 80));
    if (el.id) r.id = el.id;
    if (el.getAttribute('name')) r.field = el.getAttribute('name');
    if (el.type) r.type = el.type;
    if (el.href) r.href = cleanUrl(el.href);
    return r;
  };
  const secret = (el) => !!el && (el.type === 'password' || el.__amuxWasSecret === true
    || SECRET.test([el.name, el.id, el.autocomplete, el.getAttribute && el.getAttribute('aria-label'), label(el)].join(' ')));
  addEventListener('click', (e) => {
    const el = (e.target.closest && e.target.closest('a,button,input,select,textarea,summary,label,[role]')) || e.target;
    send({ kind: 'click', target: desc(el) });
  }, true);
  // A fill is recorded when the field is left with a value it did not have
  // when it was entered (however it got there: typing, paste, autofill), and
  // a checkbox, radio or select when it changes.
  const fill = (el) => {
    const box = el.type === 'checkbox' || el.type === 'radio';
    const hide = !box && secret(el);
    const value = box ? String(el.checked) : hide ? '[redacted]' : scrub(String(el.value || '').slice(0, 200));
    send({ kind: 'fill', target: desc(el), value, redacted: hide || value === '[redacted]' });
  };
  const typed = (el) => el && /^(input|textarea)$/i.test(el.tagName) && !/^(checkbox|radio|submit|button|file)$/i.test(el.type || '');
  // A field that is a password field at any point while it is being filled
  // stays secret, so a "show password" toggle before leaving it reveals nothing.
  const mark = (el) => { if (el && el.type === 'password') el.__amuxWasSecret = true; };
  addEventListener('focusin', (e) => { mark(e.target); if (typed(e.target)) e.target.__amuxStart = e.target.value; }, true);
  addEventListener('input', (e) => mark(e.target), true);
  addEventListener('focusout', (e) => {
    const el = e.target;
    if (typed(el) && el.value !== (el.__amuxStart === undefined ? '' : el.__amuxStart)) { el.__amuxStart = el.value; fill(el); }
  }, true);
  addEventListener('change', (e) => {
    const el = e.target; if (!el || !el.tagName || typed(el)) return;
    fill(el);
  }, true);
  addEventListener('submit', (e) => send({ kind: 'submit', target: desc(e.target) }), true);
  addEventListener('keydown', (e) => {
    if (e.key === 'Enter' || e.key === 'Escape' || e.key === 'Tab') send({ kind: 'key', key: e.key, target: desc(e.target) });
  }, true);
  send({ kind: 'page' });
})();"#;

/// The recorder with this watch's nonce in place.
pub fn recorder(nonce: &str) -> String {
    RECORDER.replace(NONCE, nonce)
}

struct Watch {
    steps: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    started_at: i64,
    /// When the watch stopped (unix seconds); 0 while it runs.
    stopped_at: Arc<AtomicI64>,
    error: Arc<Mutex<Option<String>>>,
}

static WATCHES: std::sync::LazyLock<Mutex<HashMap<String, Watch>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(show))
        .route("/start", post(start))
        .route("/stop", post(stop))
}

/// The session a request may watch: the worker its header names, or for the
/// owner any named session ("amux" when none is named). Anyone else is refused.
fn session_of(
    state: &AppState,
    uri: &Uri,
    body: Option<&Value>,
    q: Option<&HashMap<String, String>>,
    headers: &HeaderMap,
) -> Result<String, Box<Response>> {
    let named = body
        .and_then(|b| b.get("session"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| q.and_then(|q| q.get("session").cloned()))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let worker = headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(w) = worker {
        if let Some(n) = named.filter(|n| *n != w) {
            tracing::warn!(named = %n, caller = %w, measured = true, n_considered = 1,
                verdict = "browser_watch_as_self", "[browser-watch] refused: the request named another worker's tab");
            return Err(Box::new((
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "a worker watches its own tab: drop `session` or name yourself in X-Amux-Session", "code": "watch_as_self" })),
            )
                .into_response()));
        }
        return Ok(w);
    }
    super::land_approvals::owner_only(state, headers, uri, "browser_watch_refused", "watch a tab without a worker identity")?;
    Ok(named.unwrap_or_else(|| "amux".into()))
}

/// Drops watches that stopped more than [`KEEP_FINISHED_S`] ago.
fn evict_finished(all: &mut HashMap<String, Watch>, now: i64) {
    let before = all.len();
    all.retain(|_, w| {
        let stopped = w.stopped_at.load(Ordering::Relaxed);
        !w.stop.load(Ordering::Relaxed) || stopped == 0 || now - stopped < KEEP_FINISHED_S
    });
    if all.len() < before {
        tracing::info!(evicted = before - all.len(), measured = true, n_considered = before,
            verdict = "browser_watch_evicted", "[browser-watch] finished watches evicted");
    }
}

fn snapshot(session: &str) -> Value {
    let all = WATCHES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match all.get(session) {
        None => {
            json!({ "session": session, "watching": false, "steps": [], "measured": true, "n_considered": 0 })
        }
        Some(w) => {
            let steps = w
                .steps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            json!({ "session": session, "watching": !w.stop.load(Ordering::Relaxed), "started_at": w.started_at,
                    "error": w.error.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone(),
                    "steps": steps, "measured": true, "n_considered": steps.len(),
                    "redacted": steps.iter().filter(|s| s.get("redacted") == Some(&json!(true))).count() })
        }
    }
}

/// The recorder's steps among CDP events, appended in arrival order. Only a
/// message carrying this watch's nonce is a step.
fn collect(events: Vec<Value>, steps: &Mutex<Vec<Value>>, nonce: &str) {
    for e in events {
        if e.get("method").and_then(Value::as_str) != Some("Runtime.consoleAPICalled") {
            continue;
        }
        let text = e
            .pointer("/params/args/0/value")
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(step) = text
            .strip_prefix(MARK)
            .and_then(|p| p.strip_prefix(nonce))
            .and_then(|p| p.strip_prefix(':'))
            .and_then(|p| serde_json::from_str::<Value>(p).ok())
        {
            let mut st = steps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if st.len() < MAX_STEPS {
                st.push(step);
            }
        }
    }
}

/// Installs the binding, the new-document script and the recorder on one
/// connection. Returns the script id and any steps that arrived meanwhile.
async fn arm(cdp: &mut chrome::CdpClient, nonce: &str) -> anyhow::Result<(String, Vec<Value>)> {
    let source = recorder(nonce);
    cdp.call("Runtime.enable", json!({}), T).await?;
    let added = cdp
        .call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": source }),
            T,
        )
        .await?;
    // The current page's first step arrives while this call waits: keep it.
    let mut first = Vec::new();
    cdp.call_in(
        None,
        "Runtime.evaluate",
        json!({ "expression": source }),
        T,
        Some(&mut first),
    )
    .await?;
    Ok((
        added
            .get("identifier")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        first,
    ))
}

/// What each poll asks the page: is the recorder here, and can it reach us?
const PROBE: &str = "JSON.stringify({ on: !!window.__amuxWatchOn })";

async fn start(State(state): State<AppState>, OriginalUri(uri): OriginalUri, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(v)| v);
    let session = match session_of(&state, &uri, body.as_ref(), None, &headers) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    {
        let mut all = WATCHES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        evict_finished(&mut all, chrono::Utc::now().timestamp());
        if all.get(&session).is_some_and(|w| !w.stop.load(Ordering::Relaxed)) {
            drop(all);
            return Json(json!({ "ok": true, "already": true, "watch": snapshot(&session) }))
                .into_response();
        }
    }
    let nonce = {
        let mut b = [0u8; 16];
        if let Err(e) = getrandom::fill(&mut b) {
            tracing::warn!("[browser-watch] verdict=watch_start_failed session={session}: no random source: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "no random source for the watch" }))).into_response();
        }
        hex::encode(b)
    };
    let (page, mut cdp) = match super::browser::connect_session(&session, None).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let (mut script_id, first) = match arm(&mut cdp, &nonce).await {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("[browser-watch] verdict=watch_start_failed session={session}: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("could not start watching: {e}") })),
            )
                .into_response();
        }
    };
    let w = Watch {
        steps: Arc::new(Mutex::new(Vec::new())),
        stop: Arc::new(AtomicBool::new(false)),
        started_at: chrono::Utc::now().timestamp(),
        stopped_at: Arc::new(AtomicI64::new(0)),
        error: Arc::new(Mutex::new(None)),
    };
    collect(first, &w.steps, &nonce);
    let (steps, stopflag, stopped_at, error) = (w.steps.clone(), w.stop.clone(), w.stopped_at.clone(), w.error.clone());
    WATCHES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session.clone(), w);
    tracing::info!(
        "[browser-watch] verdict=watch_started session={session} url={}",
        page.url
    );
    let s2 = session.clone();
    tokio::spawn(async move {
        // A navigation that moves the tab to a new renderer process can leave a
        // page without the recorder or its callback, and can stall this socket.
        // Each poll checks both and re-arms; repeated failures reconnect.
        let (mut errors, mut reconnects, mut rearmed) = (0u32, 0u32, 0u32);
        while !stopflag.load(Ordering::Relaxed) {
            let mut events = Vec::new();
            let r = cdp
                .call_in(
                    None,
                    "Runtime.evaluate",
                    json!({ "expression": PROBE, "returnByValue": true }),
                    Duration::from_secs(4),
                    Some(&mut events),
                )
                .await;
            collect(events, &steps, &nonce);
            match r {
                Ok(v) => {
                    errors = 0;
                    let state: Value = v
                        .pointer("/result/value")
                        .and_then(Value::as_str)
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or(Value::Null);
                    let missing = state.get("on") != Some(&json!(true));
                    if missing && !state.is_null() {
                        rearmed += 1;
                        let mut ev = Vec::new();
                        let armed = cdp
                            .call_in(
                                None,
                                "Runtime.evaluate",
                                json!({ "expression": recorder(&nonce) }),
                                T,
                                Some(&mut ev),
                            )
                            .await;
                        collect(ev, &steps, &nonce);
                        // A failed re-arm is retried on the next poll.
                        tracing::info!(
                            "[browser-watch] verdict=watch_rearmed session={s2} times={rearmed} ok={}",
                            armed.is_ok()
                        );
                    }
                }
                Err(e) => {
                    errors += 1;
                    if errors >= 3 {
                        reconnects += 1;
                        let back = match super::browser::connect_session(&s2, None).await {
                            Ok((_, mut c)) => match arm(&mut c, &nonce).await {
                                Ok((id, ev)) => {
                                    collect(ev, &steps, &nonce);
                                    script_id = id;
                                    cdp = c;
                                    true
                                }
                                Err(_) => false,
                            },
                            Err(_) => false,
                        };
                        tracing::warn!("[browser-watch] verdict=watch_reconnect session={s2} ok={back} times={reconnects}: {e}");
                        errors = 0;
                        if !back && reconnects >= 5 {
                            *error
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(format!("the tab went away: {e}"));
                            stopped_at.store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
                            stopflag.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        // Leave the page as it was: no recorder in the next document, none listening now.
        // A tab that is already gone fails these; say so rather than drop it.
        let removed = cdp
            .call(
                "Page.removeScriptToEvaluateOnNewDocument",
                json!({ "identifier": script_id }),
                T,
            )
            .await;
        let quieted = cdp
            .call(
                "Runtime.evaluate",
                json!({ "expression": "window.__amuxWatchOff = true" }),
                T,
            )
            .await;
        if let Err(e) = removed.and(quieted) {
            tracing::info!("[browser-watch] verdict=watch_cleanup_partial session={s2}: {e}");
        }
    });
    Json(json!({ "ok": true, "watch": snapshot(&session),
                 "next": "do the task in the browser; GET /api/browser/watch for the steps, POST /api/browser/watch/stop when done" }))
        .into_response()
}

async fn show(State(state): State<AppState>, OriginalUri(uri): OriginalUri, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    match session_of(&state, &uri, None, Some(&q), &headers) {
        Ok(s) => Json(snapshot(&s)).into_response(),
        Err(r) => *r,
    }
}

async fn stop(State(state): State<AppState>, OriginalUri(uri): OriginalUri, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(v)| v);
    let session = match session_of(&state, &uri, body.as_ref(), None, &headers) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    if let Some(w) = WATCHES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&session)
    {
        if !w.stop.swap(true, Ordering::Relaxed) {
            w.stopped_at.store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
        }
    }
    // One more collection pass happened or is about to; let it land.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let snap = snapshot(&session);
    tracing::info!(
        "[browser-watch] verdict=watch_stopped session={session} steps={} redacted={}",
        snap["n_considered"],
        snap.get("redacted").cloned().unwrap_or(json!(0))
    );
    Json(json!({ "ok": true, "watch": snap })).into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_recorder_redacts_in_the_page_and_records_only_named_keys() {
        let r = super::recorder("n0nce");
        assert!(r.contains("type === 'password'") && r.contains("'[redacted]'"));
        assert!(r.contains("e.key === 'Enter' || e.key === 'Escape' || e.key === 'Tab'"));
        assert!(r.contains(&format!("{}n0nce:", super::MARK)) && !r.contains(super::NONCE));
    }

    #[test]
    fn only_a_message_with_the_watch_nonce_is_a_step() {
        let ev = |text: &str| serde_json::json!({ "method": "Runtime.consoleAPICalled", "params": { "args": [{ "value": text }] } });
        let steps = std::sync::Mutex::new(Vec::new());
        super::collect(
            vec![
                ev(r#"__amuxwatch:{"kind":"click","forged":"bare prefix"}"#),
                ev(r#"__amuxwatch:other:{"kind":"click","forged":"wrong nonce"}"#),
                ev(r#"__amuxwatch:n0nce:{"kind":"click"}"#),
            ],
            &steps,
            "n0nce",
        );
        let got = steps.into_inner().unwrap();
        assert_eq!(got, vec![serde_json::json!({ "kind": "click" })]);
    }

    #[test]
    fn finished_watches_are_evicted_and_live_ones_kept() {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::AtomicI64;
        let w = |stopped_at: Option<i64>, started_at: i64| super::Watch {
            steps: Default::default(),
            stop: std::sync::Arc::new(AtomicBool::new(stopped_at.is_some())),
            started_at,
            stopped_at: std::sync::Arc::new(AtomicI64::new(stopped_at.unwrap_or(0))),
            error: Default::default(),
        };
        let now = 100_000;
        let long_ago = now - super::KEEP_FINISHED_S - 1;
        let mut all = std::collections::HashMap::new();
        all.insert("stopped-long-ago".to_string(), w(Some(long_ago), long_ago - 60));
        all.insert("old-live".to_string(), w(None, long_ago));
        // Review F1: a watch that ran longer than the keep window and just
        // stopped keeps its steps.
        all.insert("long-run-just-stopped".to_string(), w(Some(now - 10), now - 5400));
        super::evict_finished(&mut all, now);
        let mut keys: Vec<_> = all.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec!["long-run-just-stopped", "old-live"]);
    }

    /// Who may watch which tab (amux-security, 2026-11-23). Every refusal
    /// happens before the browser is touched, so no Chrome is needed.
    #[tokio::test]
    async fn a_worker_watches_only_its_own_tab_and_no_identity_needs_the_owner() {
        use crate::api::standing_approvals::tests::{call, test_state};
        use axum::http::StatusCode;
        let mut state = test_state();
        state.auth_token = Some("owner-secret".into());
        let app = super::routes().with_state(state);
        let other = serde_json::json!({ "session": "amux" });
        let (st, v) = call(&app, "POST", "/start", &[("x-amux-session", "lane-t")], Some(other.clone())).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "a worker starting a watch on another tab: {v}");
        assert_eq!(v["code"], "watch_as_self");
        let (st, _) = call(&app, "POST", "/stop", &[("x-amux-session", "lane-t")], Some(other.clone())).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "a worker stopping another tab's watch");
        let (st, _) = call(&app, "GET", "/?session=amux", &[("x-amux-session", "lane-t")], None).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "a worker reading another tab's steps");
        for path in ["/", "/?session=lane-t"] {
            let (st, _) = call(&app, "GET", path, &[], None).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "no identity and no bearer reading {path}");
        }
        let (st, _) = call(&app, "POST", "/start", &[], Some(serde_json::json!({}))).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "no identity and no bearer starting the owner's tab");
        // A worker reading its own tab, naming itself or not, is let through.
        for path in ["/", "/?session=lane-t"] {
            let (st, v) = call(&app, "GET", path, &[("x-amux-session", "lane-t")], None).await;
            assert_eq!((st, v["session"].as_str()), (StatusCode::OK, Some("lane-t")), "{path}");
        }
        // The owner reads any tab, the owner's own by default.
        let (st, v) = call(&app, "GET", "/", &[("authorization", "Bearer owner-secret")], None).await;
        assert_eq!((st, v["session"].as_str()), (StatusCode::OK, Some("amux")));
        let (st, v) = call(&app, "GET", "/?session=lane-t", &[("authorization", "Bearer owner-secret")], None).await;
        assert_eq!((st, v["session"].as_str()), (StatusCode::OK, Some("lane-t")));
    }
}
