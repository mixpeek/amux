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
//! field, or a field whose name, id, autocomplete or label reads like a
//! secret (password, token, one-time code, card number, CVC, SSN, PIN), is
//! recorded as "[redacted]". Keys are recorded only for Enter, Escape and Tab.

use super::AppState;
use crate::integrations::browser as chrome;
use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Steps travel as console.debug messages with this prefix, which every page
/// has natively. A CDP binding (Runtime.addBinding) was tried first: after a
/// click took the tab to another origin's renderer, the tab stopped answering
/// for about 40 seconds while a watch held it.
const MARK: &str = "__amuxwatch:";
const MAX_STEPS: usize = 1000;
const T: Duration = Duration::from_secs(8);

/// The recorder, run in every document of the watched tab.
pub const RECORDER: &str = r#"(() => {
  if (window.__amuxWatchOn) return; window.__amuxWatchOn = true;
  const SECRET = /pass|secret|token|otp|one.?time|cvc|cvv|card.?num|cc-?(num|csc)|ssn|\bpin\b|auth.?code|security.?code/i;
  const send = (o) => { if (window.__amuxWatchOff) return;
    try { console.debug('__amuxwatch:' + JSON.stringify(Object.assign({ t: Date.now(), url: location.href, title: document.title }, o))); } catch (e) {} };
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
    r.name = (label(el) || text).slice(0, 80);
    if (el.id) r.id = el.id;
    if (el.getAttribute('name')) r.field = el.getAttribute('name');
    if (el.type) r.type = el.type;
    if (el.href) r.href = el.href;
    return r;
  };
  const secret = (el) => !!el && (el.type === 'password'
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
    send({ kind: 'fill', target: desc(el), value: box ? String(el.checked) : hide ? '[redacted]' : String(el.value || '').slice(0, 200), redacted: hide });
  };
  const typed = (el) => el && /^(input|textarea)$/i.test(el.tagName) && !/^(checkbox|radio|submit|button|file)$/i.test(el.type || '');
  addEventListener('focusin', (e) => { if (typed(e.target)) e.target.__amuxStart = e.target.value; }, true);
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

struct Watch {
    steps: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    started_at: i64,
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

fn session_of(
    body: Option<&Value>,
    q: Option<&HashMap<String, String>>,
    headers: &HeaderMap,
) -> String {
    body.and_then(|b| b.get("session"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| q.and_then(|q| q.get("session").cloned()))
        .or_else(|| {
            headers
                .get("x-amux-session")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "amux".into())
}

fn snapshot(session: &str) -> Value {
    let all = WATCHES.lock().expect("watch registry poisoned");
    match all.get(session) {
        None => {
            json!({ "session": session, "watching": false, "steps": [], "measured": true, "n_considered": 0 })
        }
        Some(w) => {
            let steps = w.steps.lock().expect("watch steps poisoned").clone();
            json!({ "session": session, "watching": !w.stop.load(Ordering::Relaxed), "started_at": w.started_at,
                    "error": w.error.lock().expect("watch error poisoned").clone(),
                    "steps": steps, "measured": true, "n_considered": steps.len(),
                    "redacted": steps.iter().filter(|s| s.get("redacted") == Some(&json!(true))).count() })
        }
    }
}

/// The recorder's steps among CDP events, appended in arrival order.
fn collect(events: Vec<Value>, steps: &Mutex<Vec<Value>>) {
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
            .and_then(|p| serde_json::from_str::<Value>(p).ok())
        {
            let mut st = steps.lock().expect("watch steps poisoned");
            if st.len() < MAX_STEPS {
                st.push(step);
            }
        }
    }
}

/// Installs the binding, the new-document script and the recorder on one
/// connection. Returns the script id and any steps that arrived meanwhile.
async fn arm(cdp: &mut chrome::CdpClient) -> anyhow::Result<(String, Vec<Value>)> {
    cdp.call("Runtime.enable", json!({}), T).await?;
    let added = cdp
        .call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": RECORDER }),
            T,
        )
        .await?;
    // The current page's first step arrives while this call waits: keep it.
    let mut first = Vec::new();
    cdp.call_in(
        None,
        "Runtime.evaluate",
        json!({ "expression": RECORDER }),
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

async fn start(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(v)| v);
    let session = session_of(body.as_ref(), None, &headers);
    if WATCHES
        .lock()
        .expect("watch registry poisoned")
        .get(&session)
        .is_some_and(|w| !w.stop.load(Ordering::Relaxed))
    {
        return Json(json!({ "ok": true, "already": true, "watch": snapshot(&session) }))
            .into_response();
    }
    let (page, mut cdp) = match super::browser::connect_session(&session, None).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let (mut script_id, first) = match arm(&mut cdp).await {
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
        error: Arc::new(Mutex::new(None)),
    };
    collect(first, &w.steps);
    let (steps, stopflag, error) = (w.steps.clone(), w.stop.clone(), w.error.clone());
    WATCHES
        .lock()
        .expect("watch registry poisoned")
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
            collect(events, &steps);
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
                        let _ = cdp
                            .call_in(
                                None,
                                "Runtime.evaluate",
                                json!({ "expression": RECORDER }),
                                T,
                                Some(&mut ev),
                            )
                            .await;
                        collect(ev, &steps);
                        tracing::info!(
                            "[browser-watch] verdict=watch_rearmed session={s2} times={rearmed}"
                        );
                    }
                }
                Err(e) => {
                    errors += 1;
                    if errors >= 3 {
                        reconnects += 1;
                        let back = match super::browser::connect_session(&s2, None).await {
                            Ok((_, mut c)) => match arm(&mut c).await {
                                Ok((id, ev)) => {
                                    collect(ev, &steps);
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
                            *error.lock().expect("watch error poisoned") =
                                Some(format!("the tab went away: {e}"));
                            stopflag.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        // Leave the page as it was: no recorder in the next document, none listening now.
        let _ = cdp
            .call(
                "Page.removeScriptToEvaluateOnNewDocument",
                json!({ "identifier": script_id }),
                T,
            )
            .await;
        let _ = cdp
            .call(
                "Runtime.evaluate",
                json!({ "expression": "window.__amuxWatchOff = true" }),
                T,
            )
            .await;
    });
    Json(json!({ "ok": true, "watch": snapshot(&session),
                 "next": "do the task in the browser; GET /api/browser/watch for the steps, POST /api/browser/watch/stop when done" }))
        .into_response()
}

async fn show(headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    Json(snapshot(&session_of(None, Some(&q), &headers))).into_response()
}

async fn stop(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(v)| v);
    let session = session_of(body.as_ref(), None, &headers);
    if let Some(w) = WATCHES
        .lock()
        .expect("watch registry poisoned")
        .get(&session)
    {
        w.stop.store(true, Ordering::Relaxed);
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
        let r = super::RECORDER;
        assert!(r.contains("type === 'password'") && r.contains("'[redacted]'"));
        assert!(r.contains("e.key === 'Enter' || e.key === 'Escape' || e.key === 'Tab'"));
        assert!(r.contains(super::MARK));
    }
}
