//! One daily digest of pending outbound-email approvals (AMUX-5240).
//!
//! gtm-ticker's SCHED-498 drafts replies and parks each one for the owner.
//! Approvals used to expire after an hour, so each schedule fire re-requested
//! them, and the only notice the owner got was a banner he had to be looking
//! at. Four drafts expired twice in the 2026-09-26 fleet review. Approvals no
//! longer expire and a re-request reuses the pending one (see
//! `api::email_approval`), which leaves one gap: something has to tell the
//! owner the queue exists when he is not at the dashboard.
//!
//! This is that notice, sent at most once a local day, only when something is
//! pending. It reuses the owner channels the alert module already has: web
//! push (`crate::push::send_all`) and the owner inbox resolved by
//! `alerts::owner_email_destination` through `AlertChannels::email`. It never
//! pages SMS: this is a reminder, not a fire alarm.
//!
//! Kill switch: `AMUX_EMAIL_APPROVAL_DIGEST=0` (global scope; the digest is
//! addressed to the owner, not to a lane). `AMUX_EMAIL_APPROVAL_DIGEST_HOUR`
//! picks the local hour (default 9). The job also honours the per-job
//! `AMUX_EMAIL_APPROVAL_DIGEST_SECS=0` switch.

use crate::api::alerts::AlertChannels;
use crate::api::AppState;
use serde_json::{json, Value};
use std::path::Path;

const JOB: &str = super::registry::ids::EMAIL_APPROVAL_DIGEST;
const KILL_SWITCH: &str = "AMUX_EMAIL_APPROVAL_DIGEST";
/// How many drafts the body lists before it says "and N more". A digest the
/// owner cannot skim is one he stops reading; the true count is always in the
/// subject and first line.
const LIST_CAP: usize = 25;

fn tick_secs() -> u64 {
    crate::config::env_i64("AMUX_EMAIL_APPROVAL_DIGEST_TICK_S", 900).max(60) as u64
}

fn enabled(home: &Path) -> bool {
    !crate::api::settings::effective_env(home, KILL_SWITCH).is_some_and(|v| {
        matches!(
            v.trim().trim_matches('"').to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

fn digest_hour(home: &Path) -> u32 {
    crate::api::settings::effective_env(home, "AMUX_EMAIL_APPROVAL_DIGEST_HOUR")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|h| *h < 24)
        .unwrap_or(9)
}

/// Is today's digest due? Pure: at or after the digest hour, not already sent
/// today, and something is pending.
pub(crate) fn digest_due(
    today: &str,
    hour_now: u32,
    last_sent: Option<&str>,
    digest_hour: u32,
    pending: usize,
) -> bool {
    pending > 0 && hour_now >= digest_hour && last_sent != Some(today)
}

fn state_path(home: &Path) -> std::path::PathBuf {
    // Not an `apr_<hex>.json` name, so list_pending never reads it as a draft.
    crate::api::email_approval::approvals_dir(home).join(".digest-state.json")
}

fn last_sent(home: &Path) -> Option<String> {
    std::fs::read_to_string(state_path(home))
        .ok()
        .and_then(|r| serde_json::from_str::<Value>(&r).ok())
        .and_then(|v| {
            v.get("last_sent_date")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn stamp(home: &Path, today: &str, count: usize, channels: &Value) {
    let _ = std::fs::write(
        state_path(home),
        json!({"last_sent_date": today, "count": count, "channels": channels,
               "at": crate::config::now_f64()})
        .to_string(),
    );
}

fn hours(age_s: i64) -> String {
    if age_s < 3600 {
        format!("{}m", (age_s / 60).max(1))
    } else if age_s < 48 * 3600 {
        format!("{}h", age_s / 3600)
    } else {
        format!("{}d", age_s / 86_400)
    }
}

/// Subject and body. Every block is separated by a blank line and every item
/// starts with a marker, because mail clients reflow single newlines (the RB2B
/// digest incident in CLAUDE.md).
pub(crate) fn render(pending: &[Value]) -> (String, String) {
    let n = pending.len();
    let noun = if n == 1 { "draft" } else { "drafts" };
    let subject = format!("amux: {n} email {noun} waiting for your approval");
    let mut body = format!(
        "{n} outbound email {noun} {} waiting for your approval. They do not expire: each one \
         stays until you approve or discard it.\n\n\
         Approve, edit or discard them from the approvals banner in the amux dashboard.",
        if n == 1 { "is" } else { "are" }
    );
    for (i, p) in pending.iter().take(LIST_CAP).enumerate() {
        let pv = p.get("preview").cloned().unwrap_or(Value::Null);
        let s = |k: &str| {
            pv.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let kind = if s("endpoint") == "reply" {
            "reply"
        } else {
            "new email"
        };
        let to = if s("to").is_empty() {
            "(no recipient)".into()
        } else {
            s("to")
        };
        let subj = if s("subject").is_empty() {
            "(no subject)".into()
        } else {
            s("subject")
        };
        let age = p.get("age_s").and_then(Value::as_i64).unwrap_or(0);
        let requests = p.get("requests").and_then(Value::as_u64).unwrap_or(1);
        let session = p.get("session").and_then(Value::as_str).unwrap_or("?");
        let id = p.get("id").and_then(Value::as_str).unwrap_or("?");
        let asked = if requests > 1 {
            format!(", requested {requests} times")
        } else {
            String::new()
        };
        body.push_str(&format!(
            "\n\n{}. {session}: {kind} to {to}\n\n   Subject: {subj}\n\n   Waiting {}{asked}. Id: {id}",
            i + 1,
            hours(age)
        ));
    }
    if n > LIST_CAP {
        body.push_str(&format!(
            "\n\nand {} more, listed in the dashboard.",
            n - LIST_CAP
        ));
    }
    (subject, body)
}

/// Deliver on push and email. Returns the per-channel outcome and whether any
/// channel took it.
async fn deliver(
    channels: &dyn AlertChannels,
    state: &AppState,
    home: &Path,
    subject: &str,
    body: &str,
    count: usize,
) -> (Value, bool) {
    let push_text = format!("{subject}. Open the dashboard to review.");
    let results =
        crate::push::send_all(state, "amux", &push_text, "", "email-approvals-digest", "/").await;
    let push = crate::api::alerts::push_delivery_verdict(&results);
    let (email_ok, email_detail) = match crate::api::alerts::owner_email_destination(home) {
        Some((to, pinned)) => {
            let (ok, detail) = channels.email(&to, subject, body).await;
            (
                ok,
                format!(
                    "{}{detail} -> {to}{}",
                    if ok { "" } else { "failed: " },
                    if pinned {
                        ""
                    } else {
                        " [UNPINNED: set AMUX_OWNER_EMAIL]"
                    }
                ),
            )
        }
        None => (
            false,
            "no AMUX_OWNER_EMAIL and no connected Gmail account".into(),
        ),
    };
    let out = json!({
        "push": match &push { Ok(()) => "sent".to_string(), Err(e) => format!("failed: {e}") },
        "email": email_detail,
        "count": count,
    });
    (out, push.is_ok() || email_ok)
}

/// One pass. `now_local` is injected so the due rule is testable.
pub(crate) async fn tick_with(
    channels: &dyn AlertChannels,
    state: &AppState,
    home: &Path,
    now_local: chrono::DateTime<chrono::Local>,
) -> Option<Value> {
    if !enabled(home) {
        if crate::log_dedupe::first_this_bucket(
            "email-approval-digest-disabled",
            crate::log_dedupe::hour_bucket(crate::config::now_f64()) / 24,
        ) {
            tracing::info!(
                switch = KILL_SWITCH,
                verdict = "email_approval_digest_disabled",
                "email approval digest: kill switch is off"
            );
        }
        return None;
    }
    use chrono::Timelike;
    let today = now_local.format("%Y-%m-%d").to_string();
    let pending: Vec<_> = crate::api::email_approval::list_pending(home)
        .into_iter()
        .filter(|d| !crate::api::email_approval::is_scheduled(d))
        .collect();
    let last = last_sent(home);
    if !digest_due(
        &today,
        now_local.hour(),
        last.as_deref(),
        digest_hour(home),
        pending.len(),
    ) {
        return None;
    }
    let (subject, body) = render(&pending);
    let (channels_out, delivered) =
        deliver(channels, state, home, &subject, &body, pending.len()).await;
    if delivered {
        stamp(home, &today, pending.len(), &channels_out);
        tracing::info!(measured = true, n_considered = pending.len(), channels = %channels_out,
            verdict = "email_approval_digest_sent", "email approval digest delivered (AMUX-5240)");
    } else if crate::log_dedupe::first_this_bucket(
        "email-approval-digest-undelivered",
        crate::log_dedupe::hour_bucket(crate::config::now_f64()),
    ) {
        // Not stamped, so the next tick retries; WARNed at most hourly.
        tracing::warn!(measured = true, n_considered = pending.len(), channels = %channels_out,
            verdict = "email_approval_digest_undelivered",
            "email approval digest reached NO channel; will retry next tick");
    }
    Some(channels_out)
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, tick_secs(), move || {
        let st = state.clone();
        async move {
            let home = crate::config::amux_home();
            let _ = tick_with(
                &crate::api::alerts::RealChannels,
                &st,
                &home,
                chrono::Local::now(),
            )
            .await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Mock(Arc<Mutex<Vec<(String, String, String)>>>);
    #[async_trait::async_trait]
    impl AlertChannels for Mock {
        async fn push(&self, _: &AppState, _: &str, _: &str) -> Result<(), String> {
            Err("mock".into())
        }
        async fn sms(&self, _: &str, _: &str) -> (bool, String) {
            panic!("the digest must never page SMS")
        }
        async fn email(&self, to: &str, subject: &str, body: &str) -> (bool, String) {
            self.0
                .lock()
                .unwrap()
                .push((to.into(), subject.into(), body.into()));
            (true, "mock".into())
        }
    }

    #[test]
    fn due_once_a_day_after_the_hour_and_only_with_something_pending() {
        assert!(digest_due("2026-09-26", 9, None, 9, 4));
        assert!(!digest_due("2026-09-26", 8, None, 9, 4), "before the hour");
        assert!(
            !digest_due("2026-09-26", 15, Some("2026-09-26"), 9, 4),
            "already sent today"
        );
        assert!(
            digest_due("2026-09-27", 9, Some("2026-09-26"), 9, 4),
            "next day"
        );
        assert!(
            !digest_due("2026-09-26", 9, None, 9, 0),
            "nothing pending, nothing sent"
        );
    }

    #[test]
    fn render_lists_each_draft_in_its_own_block_and_counts_honestly() {
        let p = |i: usize| {
            json!({"id": format!("apr_{i:016x}"), "session": "gtm-ticker", "age_s": 26 * 3600,
                   "requests": 3, "preview": {"endpoint": "reply", "to": "rami@ext.com", "subject": "Pilot"}})
        };
        let (subject, body) = render(&[p(1)]);
        assert_eq!(subject, "amux: 1 email draft waiting for your approval");
        assert!(
            body.contains("1. gtm-ticker: reply to rami@ext.com\n\n   Subject: Pilot"),
            "{body}"
        );
        assert!(body.contains("Waiting 26h, requested 3 times"), "{body}");
        assert!(
            !body.contains('\u{2014}'),
            "no em-dashes in owner-facing text"
        );
        let many: Vec<Value> = (0..30).map(p).collect();
        let (subject, body) = render(&many);
        assert!(subject.starts_with("amux: 30 email drafts"));
        assert!(body.contains("and 5 more"), "the cap says what it hid");
        assert!(!body.contains("26. "));
    }

    #[tokio::test]
    async fn sends_once_per_day_to_the_owner_inbox_and_never_sms() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::write(
            home.join("server.env"),
            "AMUX_OWNER_EMAIL=owner@example.com\n",
        )
        .unwrap();
        let store = Arc::new(crate::db::Store::open(&home.join("t.db")).unwrap());
        let state = AppState {
            store,
            started: std::time::Instant::now(),
            build_hash: "t".into(),
            auth_token: None,
            reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let sent = Arc::new(Mutex::new(vec![]));
        let mock = Mock(sent.clone());
        use chrono::TimeZone;
        let at = |h: u32| {
            chrono::Local
                .with_ymd_and_hms(2026, 9, 26, h, 5, 0)
                .unwrap()
        };
        // Nothing pending: nothing sent.
        assert!(tick_with(&mock, &state, home, at(10)).await.is_none());
        crate::api::email_approval::request_approval(
            home,
            "gtm-ticker",
            "reply",
            json!({"message_id": "<a@b>", "body": "hi"}),
            json!({"endpoint": "reply", "to": "rami@ext.com", "subject": "Pilot"}),
        )
        .unwrap();
        assert!(
            tick_with(&mock, &state, home, at(8)).await.is_none(),
            "before the hour"
        );
        let out = tick_with(&mock, &state, home, at(10)).await;
        assert!(out.is_some());
        assert!(
            tick_with(&mock, &state, home, at(11)).await.is_none(),
            "once a day"
        );
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "owner@example.com");
        assert!(sent[0].2.contains("rami@ext.com"));
    }
}
