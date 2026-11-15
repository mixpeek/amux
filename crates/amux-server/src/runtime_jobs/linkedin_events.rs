//! New LinkedIn messages as `linkedin.message_received` events, so a
//! subscription can wake a worker when someone messages the owner on LinkedIn
//! (Ethan, 2026-11-15: "can you add linkedin messages to the event
//! subscription"). LinkedIn sends no notification email to any connected inbox,
//! so `email.received` cannot carry them.
//!
//! About every 45 minutes (AMUX_LINKEDIN_EVENTS_SECS, default 2700; LinkedIn
//! flags rapid automated activity), a browser route on the `ethan` profile
//! opens https://www.linkedin.com/messaging/thread/new/, reads the
//! conversation list, and stops the route. That page shows the list beside an
//! empty compose pane, so no conversation is opened: `/messaging/` itself
//! opens the newest thread, which marks it read. The job never clicks, types
//! or opens a thread.
//!
//! An unread row is one event, keyed by participant names and snippet, so one
//! new message fires once and the next message in the same conversation fires
//! again. The first read only records what is there: messages already in the
//! list are history, not news. The list carries no thread or profile link
//! (rows are click targets), so `conversation_url` is the inbox and the
//! event has no profile url; getting either would mean opening the thread.
//!
//! The last read time is kept on disk, so a server restart (the builder swaps
//! the binary on every deploy) does not read LinkedIn again early.
//!
//! Off with AMUX_LINKEDIN_EVENTS_SECS=0, and in an isolated (e2e) server.
//! Verdicts: linkedin_events_emitted, linkedin_events_seeded,
//! linkedin_events_read_failed, linkedin_events_signed_out, linkedin_events_empty_list,
//! linkedin_events_state_unsaved.

use super::registry::ids;
use crate::api::AppState;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const JOB: &str = ids::LINKEDIN_EVENTS;
/// The route's session name: what the browser audit events are attributed to.
const ROUTE_SESSION: &str = "linkedin-events";
const PROFILE: &str = "ethan";
const LIST_URL: &str = "https://www.linkedin.com/messaging/thread/new/";
const INBOX_URL: &str = "https://www.linkedin.com/messaging/";
const DEFAULT_SECS: i64 = 2700;
/// How often the loop wakes to check whether a read is due.
const WAKE_SECS: u64 = 300;
const SEEN_CAP: usize = 2000;

/// Reads the conversation list. Text only, no clicks.
const LIST_JS: &str = r#"(() => {
  const t = (e, s) => { const x = e.querySelector(s); return x ? x.textContent.replace(/\s+/g, ' ').trim() : ''; };
  const rows = [...document.querySelectorAll('li[class*=conversation-listitem]')].map(li => {
    const all = li.textContent.replace(/\s+/g, ' ');
    return {
      name: t(li, '.msg-conversation-listitem__participant-names'),
      snippet: t(li, '.msg-conversation-card__message-snippet'),
      time: t(li, 'time'),
      unread: !!li.querySelector('.msg-conversation-card__message-snippet--unread, .notification-badge--show'),
      inmail: /\bInMail\b/.test(all),
      sponsored: /\bSponsored\b/.test(all),
    };
  });
  return JSON.stringify({ href: location.href, rows });
})()"#;

fn state_path(home: &Path) -> PathBuf {
    home.join("linkedin-events.json")
}

#[derive(Default)]
struct Watch {
    seeded: bool,
    last_read: i64,
    seen: Vec<String>,
}

fn load(home: &Path) -> Watch {
    let v: Value = std::fs::read_to_string(state_path(home))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    Watch {
        seeded: v["seeded"].as_bool().unwrap_or(false),
        last_read: v["last_read"].as_i64().unwrap_or(0),
        seen: v["seen"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default(),
    }
}

fn save(home: &Path, w: &Watch) -> Result<(), String> {
    let tmp = state_path(home).with_extension("json.tmp");
    let body = json!({"seeded": w.seeded, "last_read": w.last_read, "seen": w.seen});
    std::fs::write(&tmp, body.to_string()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, state_path(home)).map_err(|e| e.to_string())
}

/// One conversation's latest message: names plus snippet.
pub(crate) fn row_key(row: &Value) -> String {
    use sha2::{Digest, Sha256};
    let text = format!("{}\u{1f}{}", row["name"].as_str().unwrap_or(""), row["snippet"].as_str().unwrap_or(""));
    format!("{:x}", Sha256::digest(text.as_bytes()))[..24].to_string()
}

/// The rows to announce: unread, from someone else, and not seen before.
/// Pure, so the cut is tested without LinkedIn.
pub(crate) fn fresh<'a>(rows: &'a [Value], seen: &[String]) -> Vec<&'a Value> {
    rows.iter()
        .filter(|r| r["unread"].as_bool().unwrap_or(false))
        .filter(|r| !r["name"].as_str().unwrap_or("").is_empty())
        .filter(|r| !r["snippet"].as_str().unwrap_or("").starts_with("You:"))
        .filter(|r| !seen.contains(&row_key(r)))
        .collect()
}

/// The event body for one row.
pub(crate) fn event_data(row: &Value) -> Value {
    json!({
        "account": PROFILE,
        "from_name": row["name"],
        "snippet": row["snippet"],
        "time_label": row["time"],
        "is_inmail": row["inmail"].as_bool().unwrap_or(false),
        "sponsored": row["sponsored"].as_bool().unwrap_or(false),
        "conversation_url": INBOX_URL,
        "key": row_key(row),
    })
}

/// Why a read cannot be used, if it cannot: not the list, or a list that has
/// not rendered yet. The first live read (2026-11-15 09:55) evaluated the page
/// before LinkedIn filled it, found 0 rows and seeded on them, which would have
/// announced every conversation already listed as new on the next read.
pub(crate) fn unusable(href: &str, rows: &[Value]) -> Option<&'static str> {
    not_the_list(href).or(if rows.is_empty() { Some("empty list") } else { None })
}

/// Why the page is not the conversation list, if it is not.
pub(crate) fn not_the_list(href: &str) -> Option<&'static str> {
    if href.contains("/login") || href.contains("/checkpoint") || href.contains("/authwall") || href.contains("/uas/") {
        Some("signed out")
    } else if !href.contains("linkedin.com/messaging") {
        Some("not the messaging page")
    } else {
        None
    }
}

/// Opens the list, reads it, and always stops the route.
async fn read_list(state: &AppState) -> Result<(String, Vec<Value>), String> {
    use crate::api::browser_routing::run_verb;
    let (code, started) =
        run_verb(state, ROUTE_SESSION, "start", json!({"profile": PROFILE, "url": LIST_URL})).await;
    let read = if code >= 300 || started["ok"].as_bool() == Some(false) {
        Err(format!("start answered {code}: {}", started["error"].as_str().unwrap_or("no error text")))
    } else {
        // The list renders after the page loads: read until it has rows, the
        // page is not the list, or 20 s pass.
        let mut last = Err("the list was never read".to_string());
        for _ in 0..20 {
            let (code, v) = run_verb(state, ROUTE_SESSION, "action", json!({"action": "eval", "script": LIST_JS})).await;
            last = match v.pointer("/data/result").and_then(Value::as_str).and_then(|s| serde_json::from_str::<Value>(s).ok()) {
                Some(page) => Ok((
                    page["href"].as_str().unwrap_or("").to_string(),
                    page["rows"].as_array().cloned().unwrap_or_default(),
                )),
                None => Err(format!("eval answered {code} with no list: {}", v["error"].as_str().unwrap_or(""))),
            };
            if matches!(&last, Ok((href, rows)) if !rows.is_empty() || not_the_list(href).is_some()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        last
    };
    let (code, stopped) = run_verb(state, ROUTE_SESSION, "stop", json!({})).await;
    if code >= 300 {
        tracing::warn!(target: "amux::linkedin_events", code, error = stopped["error"].as_str().unwrap_or(""),
            verdict = "linkedin_events_route_not_stopped", "the LinkedIn read's browser route did not stop; the reaper closes it when idle");
    }
    read
}

async fn tick(state: &AppState, home: &Path, every: i64) {
    let mut w = load(home);
    let now = chrono::Utc::now().timestamp();
    if now - w.last_read < every {
        return;
    }
    w.last_read = now;
    let outcome = read_list(state).await;
    let (href, rows) = match outcome {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(target: "amux::linkedin_events", error = %e, measured = false,
                verdict = "linkedin_events_read_failed", "the LinkedIn message list could not be read; no events this time");
            persist(home, &w);
            return;
        }
    };
    if let Some(why) = unusable(&href, &rows) {
        let verdict = if why == "empty list" { "linkedin_events_empty_list" } else { "linkedin_events_signed_out" };
        tracing::warn!(target: "amux::linkedin_events", href = %href, why, measured = false, n_considered = 0, verdict,
            "the LinkedIn read did not reach a filled message list; nothing seeded or announced this time");
        persist(home, &w);
        return;
    }
    let mut emitted = 0usize;
    if !w.seeded {
        w.seen = rows.iter().map(row_key).collect();
        w.seeded = true;
        tracing::info!(target: "amux::linkedin_events", measured = true, n_considered = rows.len(),
            verdict = "linkedin_events_seeded", "watching LinkedIn messages from now; the conversations already listed do not fire");
    } else {
        for row in fresh(&rows, &w.seen) {
            let data = event_data(row);
            let key = data["key"].as_str().unwrap_or("").to_string();
            crate::api::session_verbs::emit_event_store(
                &state.store, "", "linkedin.message_received", Some(data), Some(format!("linkedin:{key}")), "linkedin",
            )
            .await;
            emitted += 1;
        }
        for row in &rows {
            let k = row_key(row);
            if !w.seen.contains(&k) {
                w.seen.push(k);
            }
        }
        let over = w.seen.len().saturating_sub(SEEN_CAP);
        w.seen.drain(..over);
        tracing::info!(target: "amux::linkedin_events", emitted, measured = true, n_considered = rows.len(),
            verdict = "linkedin_events_emitted", "LinkedIn message list read; each new unread message is one linkedin.message_received event");
    }
    persist(home, &w);
}

fn persist(home: &Path, w: &Watch) {
    if let Err(e) = save(home, w) {
        tracing::warn!(target: "amux::linkedin_events", error = %e, verdict = "linkedin_events_state_unsaved",
            "the LinkedIn watch state was not saved; the next read may come early or seed again");
    }
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, WAKE_SECS, move || {
        let state = state.clone();
        async move {
            let every = crate::config::env_i64(&super::per_job_disable_var(JOB), DEFAULT_SECS);
            if every <= 0 || std::env::var("AMUX_ISOLATED").is_ok_and(|v| v == "1") {
                return;
            }
            tick(&state, &crate::config::amux_home(), every).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, snippet: &str, unread: bool) -> Value {
        json!({"name": name, "snippet": snippet, "time": "Nov 14", "unread": unread, "inmail": false, "sponsored": false})
    }

    #[test]
    fn only_an_unseen_unread_message_from_someone_else_is_news() {
        let rows = vec![
            row("Raj Das", "Thanks, talk Monday", true),
            row("Ben Walsh", "Sounds good", false),
            row("Daria Arzy", "You: see you then", true),
            row("Tanin", "Quick question", true),
        ];
        let seen = vec![row_key(&rows[3])];
        let names: Vec<&str> = fresh(&rows, &seen).iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["Raj Das"]);
    }

    #[test]
    fn a_new_message_in_the_same_conversation_is_a_new_key() {
        assert_ne!(row_key(&row("Raj Das", "one", true)), row_key(&row("Raj Das", "two", true)));
        assert_eq!(row_key(&row("Raj Das", "one", true)), row_key(&row("Raj Das", "one", false)));
    }

    #[test]
    fn a_list_that_has_not_rendered_is_unmeasured_not_an_empty_inbox() {
        let list = "https://www.linkedin.com/messaging/thread/new/";
        assert_eq!(unusable(list, &[]), Some("empty list"));
        assert_eq!(unusable(list, &[row("a", "b", false)]), None);
        assert_eq!(unusable("https://www.linkedin.com/login", &[row("a", "b", false)]), Some("signed out"));
    }

    #[test]
    fn a_sign_in_page_is_not_read_as_an_empty_inbox() {
        assert_eq!(not_the_list("https://www.linkedin.com/login?session_redirect=x"), Some("signed out"));
        assert_eq!(not_the_list("https://www.linkedin.com/checkpoint/challenge/x"), Some("signed out"));
        assert_eq!(not_the_list("https://www.linkedin.com/feed/"), Some("not the messaging page"));
        assert_eq!(not_the_list("https://www.linkedin.com/messaging/thread/new/"), None);
    }

    #[test]
    fn the_catalog_declares_exactly_the_fields_the_event_carries() {
        let d = event_data(&row("a", "b", true));
        let mut carried: Vec<&str> = d.as_object().unwrap().keys().map(String::as_str).collect();
        let mut declared: Vec<&str> = crate::api::event_subs::DECLARED_FIELDS
            .iter()
            .find(|(t, _)| *t == "linkedin.message_received")
            .map(|(_, f)| f.to_vec())
            .unwrap();
        carried.sort();
        declared.sort();
        assert_eq!(carried, declared, "a filter on a field the event lacks never matches");
    }

    #[test]
    fn the_list_page_opens_no_conversation() {
        // /messaging/ opens the newest thread, which marks it read.
        assert!(LIST_URL.ends_with("/messaging/thread/new/"));
        assert!(!LIST_JS.contains(".click(") && !LIST_JS.contains("dispatchEvent"), "the read never clicks");
    }
}
