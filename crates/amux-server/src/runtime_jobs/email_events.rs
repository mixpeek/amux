//! New inbox mail as `email.received` events, so a subscription can wake a
//! worker when a particular email arrives (Ethan, 2026-11-12: "if I wanna wake
//! a worker and do work when a particular activity happens in email").
//!
//! Every 60 s, each connected Gmail account's inbox (the last day) is read
//! through the same client `/api/email/inbox` uses. A message is one event,
//! keyed `email:<account>:<gmail id>` so a re-read never fires twice. The
//! first read of an account only records when watching began: mail already in
//! the inbox is history, not news. The event carries the envelope and the
//! snippet (account, from, from_address, from_domain, to, cc, subject, snippet,
//! thread_id, message_id, labels, unread, bulk); the body stays in Gmail.
//!
//! Off with AMUX_EMAIL_EVENTS_SECS=0, and in an isolated (e2e) server.
//! Verdicts: email_events_emitted, email_events_seeded, email_events_read_failed,
//! email_events_state_unsaved.

use super::registry::ids;
use crate::api::AppState;
use crate::integrations::email::GmailClient;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const JOB: &str = ids::EMAIL_EVENTS;

fn state_path(home: &Path) -> PathBuf {
    home.join("email-events.json")
}

/// When watching began, per account, in ms since the epoch.
fn load_since(home: &Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(state_path(home))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("since_ms").and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

fn save_since(home: &Path, since: &serde_json::Map<String, Value>) -> Result<(), String> {
    let tmp = state_path(home).with_extension("json.tmp");
    std::fs::write(&tmp, json!({"since_ms": since}).to_string()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, state_path(home)).map_err(|e| e.to_string())
}

/// The messages to announce: those that arrived at or after watching began.
/// Pure, so the cut is tested without Gmail.
pub(crate) fn fresh(messages: &[Value], since_ms: i64) -> Vec<&Value> {
    messages
        .iter()
        .filter(|m| m["internal_date"].as_i64().unwrap_or(0) >= since_ms && m["gmail_id"].as_str().is_some_and(|s| !s.is_empty()))
        .collect()
}

/// The bare address and its domain from a From header ("Alan <alan@acme.com>").
pub(crate) fn sender_parts(from: &str) -> (String, String) {
    let addr = match (from.rfind('<'), from.rfind('>')) {
        (Some(a), Some(b)) if b > a => &from[a + 1..b],
        _ => from,
    }
    .trim()
    .to_ascii_lowercase();
    let domain = addr.rsplit_once('@').map(|(_, d)| d.to_string()).unwrap_or_default();
    (addr, domain)
}

/// The event body for one inbox message (a summary from `inbox_messages`).
pub(crate) fn event_data(account: &str, m: &Value) -> Value {
    let (from_address, from_domain) = sender_parts(m["from"].as_str().unwrap_or(""));
    json!({
        "account": account,
        "from": m["from"],
        "from_address": from_address,
        "from_domain": from_domain,
        "to": m["to"],
        "cc": m["cc"],
        "subject": m["subject"],
        "snippet": m["body"],
        "thread_id": m["thread_id"],
        "message_id": m["message_id"],
        "gmail_id": m["gmail_id"],
        "labels": m["labels"],
        "unread": !m["read"].as_bool().unwrap_or(false),
        "bulk": m["bulk"].as_bool().unwrap_or(false),
        "date": m["date"],
    })
}

async fn tick(state: &AppState, client: &GmailClient) {
    let home = crate::config::amux_home();
    let mut since = load_since(&home);
    let now_ms = chrono::Utc::now().timestamp_millis();
    let (mut emitted, mut seeded, mut changed) = (0usize, 0usize, false);
    for account in client.connected_accounts() {
        let Some(start) = since.get(&account).and_then(Value::as_i64) else {
            since.insert(account.clone(), json!(now_ms));
            seeded += 1;
            changed = true;
            tracing::info!(target: "amux::email_events", account = %account, measured = true, n_considered = 1,
                verdict = "email_events_seeded", "watching this inbox from now; mail already there does not fire");
            continue;
        };
        let read = tokio::time::timeout(std::time::Duration::from_secs(20), client.inbox_messages(&account, 25, "", 1.0, None)).await;
        let messages = match read {
            Ok(Ok(v)) => v["messages"].as_array().cloned().unwrap_or_default(),
            Ok(Err(e)) => {
                tracing::warn!(target: "amux::email_events", account = %account, error = %e, measured = false,
                    verdict = "email_events_read_failed", "the inbox could not be read; no email events this tick");
                continue;
            }
            Err(_) => {
                tracing::warn!(target: "amux::email_events", account = %account, error = "timed out after 20s", measured = false,
                    verdict = "email_events_read_failed", "the inbox could not be read; no email events this tick");
                continue;
            }
        };
        for m in fresh(&messages, start) {
            let id = m["gmail_id"].as_str().unwrap_or("");
            crate::api::session_verbs::emit_event_store(
                &state.store,
                "",
                "email.received",
                Some(event_data(&account, m)),
                Some(format!("email:{account}:{id}")),
                "email",
            )
            .await;
            emitted += 1;
        }
        tracing::debug!(target: "amux::email_events", account = %account, measured = true, n_considered = messages.len(),
            "inbox read");
    }
    if changed {
        if let Err(e) = save_since(&home, &since) {
            tracing::warn!(target: "amux::email_events", error = %e, verdict = "email_events_state_unsaved",
                "the watch start was not saved; the next tick seeds the account again");
        }
    }
    if emitted > 0 || seeded > 0 {
        tracing::info!(target: "amux::email_events", emitted, seeded, measured = true, n_considered = emitted,
            verdict = "email_events_emitted", "new inbox mail written as email.received events (already-seen mail is ignored by its key)");
    }
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    let secs = crate::config::env_i64(&super::per_job_disable_var(JOB), 60).max(1) as u64;
    let client = std::sync::Arc::new(GmailClient::new_default());
    super::spawn_periodic(JOB, secs, move || {
        let (state, client) = (state.clone(), client.clone());
        async move {
            let off = crate::config::env_i64(&super::per_job_disable_var(JOB), 60) <= 0
                || std::env::var("AMUX_ISOLATED").is_ok_and(|v| v == "1");
            if !off {
                tick(&state, &client).await;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mail_that_arrived_after_watching_began_is_news() {
        let msgs = vec![
            json!({"gmail_id": "old", "internal_date": 1_000}),
            json!({"gmail_id": "new", "internal_date": 2_000}),
            json!({"gmail_id": "", "internal_date": 3_000}),
        ];
        let ids: Vec<&str> = fresh(&msgs, 2_000).iter().map(|m| m["gmail_id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["new"]);
    }

    #[test]
    fn the_event_carries_the_envelope_a_filter_matches_on() {
        // The shape inbox_messages returns (integrations/email.rs), field for field.
        let m = json!({"gmail_id": "g1", "message_id": "<m1@acme.com>", "thread_id": "t1",
            "from": "Alan Smith <Alan@Acme.com>", "to": "ethan@mixpeek.com", "cc": "",
            "subject": "Invoice 42", "body": "please pay", "read": false, "date": "Thu",
            "labels": ["INBOX", "UNREAD"], "bulk": false, "internal_date": 5});
        let d = event_data("ethan@mixpeek.com", &m);
        assert_eq!(d["from_address"], "alan@acme.com");
        assert_eq!(d["from_domain"], "acme.com");
        assert_eq!(d["subject"], "Invoice 42");
        assert_eq!(d["snippet"], "please pay");
        assert_eq!(d["message_id"], "<m1@acme.com>");
        assert_eq!(d["unread"], true);
        assert_eq!(d["bulk"], false);
        assert_eq!(sender_parts("bob@x.io"), ("bob@x.io".into(), "x.io".into()));
    }
}
