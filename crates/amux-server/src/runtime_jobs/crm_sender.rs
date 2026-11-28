//! The CRM's sender: approved cold email goes out from the sending pool, and
//! the pool's inboxes warm each other up (owner, 2026-11-27: "setup the email
//! sequencing using the alternative emails ... all the capabilities as
//! instantly that are important to land in inbox and get people to see them
//! without sacrificing our own domain").
//!
//! Every minute (AMUX_CRM_SENDER_SECS, default 60; 0 stops it; off in an
//! isolated server):
//!
//! 1. DNS. Each pool domain's MX, SPF, DKIM, DMARC and Spamhaus DBL state is
//!    checked once a day (api/crm_mailboxes.rs `dns_check`) and kept on the
//!    inbox; a missing record or a listing makes the inbox not ready.
//! 2. Cold email. An email touch the owner approved in a campaign with
//!    mailboxes is sent by the server, byte for byte as approved, from its
//!    picked inbox: in the send window (AMUX_CRM_SEND_HOURS, default 8-17
//!    local; weekdays unless AMUX_CRM_SEND_WEEKENDS=1), at most one per inbox
//!    per pass, at least AMUX_CRM_SEND_GAP_MIN (9) minutes plus a per-touch
//!    jitter of up to as many again after that inbox's last send, and under the
//!    inbox's ramped cap for today. A first email's `Subject:` line is its
//!    subject; a follow-up without one goes in the same Gmail thread as "Re:".
//!    Each carries `List-Unsubscribe: <mailto:inbox?subject=unsubscribe>`.
//!    The person is re-checked first (unsubscribed, bounced, do_not_contact),
//!    and the touch is then recorded done exactly as a worker's `/done` would
//!    (crm.touch.done, the next step planned). A failed send is retried on
//!    later passes and marked failed after 3 attempts.
//! 3. Warm-up (AMUX_CRM_WARMUP=0 stops it). Each connected inbox in the pool
//!    (not paused) mails another inbox of the pool on a different domain:
//!    AMUX_CRM_WARMUP_START (2) a day, one more a day since its warm-up began,
//!    up to AMUX_CRM_WARMUP_MAX (10), in the send window, 20 minutes apart.
//!    Five minutes on, the receiver's copy is found by its reference: in Spam
//!    it is moved to the inbox and marked important (`rescued`), in the inbox it
//!    is marked important and read, and one in three is answered in thread.
//!    Where it landed is kept (`crm_warmup.placement`): the inbox's
//!    `placement_7d` in GET /api/crm/mailboxes is its inbox-placement rate.
//!
//! What workers do: draft, and the owner approves. A worker never sends a
//! pool email itself: /api/email/send refuses a worker send from a pool inbox
//! (crm_pool_send_by_crm), so one approval is one send.
//!
//! Verdicts: crm_sender_sent, crm_sender_send_failed, crm_sender_gave_up,
//! crm_sender_held (with why), crm_sender_dns_checked, crm_warmup_sent,
//! crm_warmup_checked, crm_warmup_failed, crm_sender_write_failed.

use super::registry::ids;
use crate::api::AppState;
use crate::db::WriteOutcome;
use serde_json::{json, Value};

const JOB: &str = ids::CRM_SENDER;
const DEFAULT_SECS: i64 = 60;
const SENDER_SESSION: &str = "crm-sender";

fn env_n(k: &str, d: i64) -> i64 {
    crate::config::env_i64(k, d)
}

/// The send window's hours, local time: [start, end).
pub(crate) fn hours() -> (u32, u32) {
    let raw = std::env::var("AMUX_CRM_SEND_HOURS").unwrap_or_default();
    let mut it = raw.split('-').map(|p| p.trim().parse::<u32>().ok());
    match (it.next().flatten(), it.next().flatten()) {
        (Some(a), Some(b)) if a < b && b <= 24 => (a, b),
        _ => (8, 17),
    }
}

/// Whether `t` (local) is inside the send window.
pub(crate) fn in_window(t: chrono::DateTime<chrono::Local>) -> bool {
    use chrono::{Datelike, Timelike};
    let (a, b) = hours();
    let weekend = matches!(t.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun);
    if weekend && std::env::var("AMUX_CRM_SEND_WEEKENDS").map(|v| v != "1").unwrap_or(true) {
        return false;
    }
    (a..b).contains(&t.hour())
}

/// Seconds to wait after an inbox's last send before this touch goes: the
/// base gap plus a jitter from the touch's id, so sends do not tick like a clock.
pub(crate) fn gap_secs(touch: &str) -> i64 {
    use sha2::{Digest, Sha256};
    let base = env_n("AMUX_CRM_SEND_GAP_MIN", 9).max(0) * 60;
    let h = Sha256::digest(touch.as_bytes());
    let jitter = if base == 0 { 0 } else { (u64::from_be_bytes(h[..8].try_into().unwrap_or([0; 8])) % base as u64) as i64 };
    base + jitter
}

/// A write the sender could not make: logged, never dropped (A4).
fn logged<T>(what: &str, r: anyhow::Result<T>) {
    if let Err(e) = r {
        tracing::warn!(target: "amux::crm", what, error = %e, measured = true, n_considered = 1, verdict = "crm_sender_write_failed", "the CRM sender could not record a step");
    }
}

/// One send the sender will make this pass.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outgoing {
    pub touch: String,
    pub mailbox: String,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub thread: String,
}

/// The sends due now: per inbox, its oldest approved email, when the inbox may
/// send now; and for each held inbox, why. Pure over the database so the
/// rules are tested without Gmail.
pub(crate) fn due(conn: &rusqlite::Connection, now: i64, window_open: bool) -> (Vec<Outgoing>, Vec<(String, String)>) {
    let mut sends = Vec::new();
    let mut held = Vec::new();
    let rows: Vec<(String, String, String, i64)> = conn
        .prepare(
            "SELECT t.id, t.mailbox, t.contact_id, COALESCE(t.approved_at,0) FROM crm_touches t WHERE t.status='approved' AND t.channel='email' \
             AND t.mailbox<>'' AND t.deleted IS NULL ORDER BY t.approved_at, t.id",
        )
        .and_then(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect())
        .unwrap_or_default();
    let mut seen = std::collections::BTreeSet::new();
    for (id, mailbox, contact, _) in rows {
        if !seen.insert(mailbox.clone()) {
            continue;
        }
        if !window_open {
            held.push((mailbox, "outside the send window".into()));
            continue;
        }
        let mb: Option<(String, i64, String, i64)> = conn
            .query_row("SELECT status, daily_cap, warmup_started, warmup_days FROM crm_mailboxes WHERE address=?1", [&mailbox], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .ok();
        let Some((status, cap, started, days)) = mb else {
            held.push((mailbox, "not in the pool".into()));
            continue;
        };
        if status == "paused" || status == "reserve" {
            held.push((mailbox.clone(), format!("{mailbox} is {status}")));
            continue;
        }
        if !crate::api::crm_mailboxes::connected(&mailbox) {
            held.push((mailbox.clone(), format!("{mailbox} is not a connected Google account")));
            continue;
        }
        let dns: String = conn.query_row("SELECT dns FROM crm_mailboxes WHERE address=?1", [&mailbox], |r| r.get(0)).unwrap_or_default();
        let dnsv: Value = serde_json::from_str(&dns).unwrap_or(Value::Null);
        if dnsv.is_null() {
            held.push((mailbox, "its domain's DNS has not been checked yet".into()));
            continue;
        }
        if dnsv["measured"] == json!(true) && dnsv["ok"] != json!(true) {
            held.push((mailbox, format!("DNS: {}", dnsv["problems"])));
            continue;
        }
        let cap_today = crate::api::crm_mailboxes::cap_today(&status, cap, &started, days);
        let day = crate::api::crm_mailboxes::start_of_today();
        let (sent_today, last): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*) FILTER (WHERE done_at>=?2), COALESCE(MAX(done_at),0) FROM crm_touches WHERE mailbox=?1 AND status='done' AND channel='email' AND deleted IS NULL",
                rusqlite::params![&mailbox, day], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap_or((0, 0));
        if sent_today >= cap_today {
            held.push((mailbox.clone(), format!("{mailbox} sent {sent_today} of {cap_today} today")));
            continue;
        }
        let wait = last + gap_secs(&id) - now;
        if last > 0 && wait > 0 {
            held.push((mailbox.clone(), format!("spacing: next send in {} min", (wait + 59) / 60)));
            continue;
        }
        let row: Option<(String, String)> = conn
            .query_row("SELECT t.body, COALESCE(c.email,'') FROM crm_touches t LEFT JOIN crm_contacts c ON c.id=t.contact_id WHERE t.id=?1", [&id], |r| Ok((r.get(0)?, r.get(1)?)))
            .ok();
        let Some((text, to)) = row else { continue };
        let (subject, body) = crate::api::crm_mailboxes::split_subject(&text);
        let (thread, subject) = match (subject, crate::api::crm_mailboxes::thread_for(conn, &contact, &mailbox, &id)) {
            (Some(s), _) => (String::new(), s),
            (None, Some((t, s))) => (t, if s.to_lowercase().starts_with("re:") { s } else { format!("Re: {s}") }),
            (None, None) => {
                held.push((mailbox, format!("{id} has no Subject: line and nothing earlier to thread under")));
                continue;
            }
        };
        sends.push(Outgoing { touch: id, mailbox, to: to.trim().to_string(), subject, body, thread });
    }
    (sends, held)
}

async fn read<T: Send + 'static>(state: &AppState, f: impl FnOnce(&rusqlite::Connection) -> T + Send + 'static) -> Option<T> {
    let store = state.store.clone();
    tokio::task::spawn_blocking(move || store.read().ok().map(|c| f(&c))).await.ok().flatten()
}

async fn refresh_dns(state: &AppState) {
    let stale = read(state, |c| -> Vec<(String, String)> {
        c.prepare("SELECT address, domain FROM crm_mailboxes WHERE dns_checked_at IS NULL OR dns_checked_at<?1")
            .and_then(|mut st| st.query_map([crate::api::crm_accounts::now_secs() - 86_400], |r| Ok((r.get(0)?, r.get(1)?)))?.collect())
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    for (addr, domain) in stale {
        let d = domain.clone();
        let v = tokio::task::spawn_blocking(move || crate::api::crm_mailboxes::dns_check(&d)).await.unwrap_or(Value::Null);
        if v.is_null() {
            continue;
        }
        tracing::info!(target: "amux::crm", domain = %domain, ok = v["ok"].as_bool().unwrap_or(false), measured = v["measured"].as_bool().unwrap_or(false),
            n_considered = 1, problems = %v["problems"], verdict = "crm_sender_dns_checked", "a sending domain's DNS was checked");
        let text = v.to_string();
        let r = state
            .store
            .write_async(move |c| {
                c.execute("UPDATE crm_mailboxes SET dns=?2, dns_checked_at=?3 WHERE address=?1", rusqlite::params![&addr, &text, crate::api::crm_accounts::now_secs()])?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await;
        logged("dns", r);
    }
}

async fn send_cold(state: &AppState, gmail: &crate::integrations::email::GmailClient) {
    let now = crate::api::crm_accounts::now_secs();
    let open = in_window(chrono::Local::now());
    let (sends, held) = read(state, move |c| due(c, now, open)).await.unwrap_or_default();
    for (mb, why) in &held {
        tracing::debug!(target: "amux::crm", mailbox = %mb, why = %why, verdict = "crm_sender_held", "a pool inbox has approved email waiting");
    }
    for s in sends {
        // The person may have opted out, bounced or been marked do-not-contact since approval.
        let t = s.touch.clone();
        let refusal = read(state, move |c| {
            let contact: String = c.query_row("SELECT contact_id FROM crm_touches WHERE id=?1", [&t], |r| r.get(0)).unwrap_or_default();
            crate::api::crm_outbound::guard(c, &contact, "email", "email", "", true).map(|g| g.1)
        })
        .await
        .flatten();
        if let Some(why) = refusal {
            finish(state, &s, true, &format!("not sent: {why}"), "", "").await;
            continue;
        }
        if s.to.is_empty() {
            finish(state, &s, true, "not sent: the person has no email address", "", "").await;
            continue;
        }
        let headers = vec![("List-Unsubscribe".to_string(), format!("<mailto:{}?subject=unsubscribe>", s.mailbox))];
        match gmail.compose_send_with_headers(&s.mailbox, &s.to, &s.subject, &s.body, "", "", "", &s.thread, true, &[], &headers).await {
            Ok(r) => {
                let gid = r["id"].as_str().unwrap_or("").to_string();
                let tid = r["thread_id"].as_str().unwrap_or("").to_string();
                crate::integrations::email::email_log(gmail.home(), json!({ "endpoint": "crm-sender", "via": "gmail", "from": s.mailbox, "to": s.to,
                    "subject": s.subject, "gmail_id": gid, "thread_id": tid, "crm_touch": s.touch, "session": SENDER_SESSION, "approved_by": "owner" }));
                tracing::info!(target: "amux::crm", touch = %s.touch, mailbox = %s.mailbox, measured = true, n_considered = 1, verdict = "crm_sender_sent",
                    "an approved cold email was sent from the pool");
                finish(state, &s, false, &format!("sent from {}", s.mailbox), &format!("gmail:{gid}"), &tid).await;
            }
            Err(e) => {
                let id = s.touch.clone();
                let r = state
                    .store
                    .write_async(move |c| {
                        c.execute("UPDATE crm_touches SET send_attempts=send_attempts+1 WHERE id=?1", [&id])?;
                        Ok(WriteOutcome { applied: true, events: vec![] })
                    })
                    .await;
                logged("send_attempts", r);
                let n = read(state, { let id = s.touch.clone(); move |c| c.query_row("SELECT send_attempts FROM crm_touches WHERE id=?1", [&id], |r| r.get::<_, i64>(0)).unwrap_or(0) }).await.unwrap_or(0);
                tracing::warn!(target: "amux::crm", touch = %s.touch, mailbox = %s.mailbox, error = %e, attempts = n, measured = true, n_considered = 1,
                    verdict = "crm_sender_send_failed", "an approved cold email did not send; it is retried");
                if n >= 3 {
                    tracing::warn!(target: "amux::crm", touch = %s.touch, verdict = "crm_sender_gave_up", "three sends failed; the touch is marked failed");
                    finish(state, &s, true, &format!("send failed 3 times: {e}"), "", "").await;
                }
            }
        }
    }
}

async fn finish(state: &AppState, s: &Outgoing, failed: bool, result: &str, url: &str, thread: &str) {
    let (id, result, url, thread, subject) = (s.touch.clone(), result.to_string(), url.to_string(), thread.to_string(), s.subject.clone());
    let out = std::sync::Arc::new(std::sync::Mutex::new(None));
    let w = out.clone();
    let r = state
        .store
        .write_async(move |c| {
            if !failed {
                c.execute("UPDATE crm_touches SET sent_thread=?2, sent_subject=?3 WHERE id=?1", rusqlite::params![&id, &thread, &subject])?;
            }
            let r = crate::api::crm_agent::record_done(c, &id, failed, &result, &url)?;
            let applied = r.is_ok();
            // The same refreshes as the /done route: the touch, and the person
            // (or the company, for a company touch) it belongs to (land review F2).
            let events = match &r {
                Ok(t) => {
                    use amux_core::revision::MutationKind::Updated;
                    let (contact, account) = (t["contact_id"].as_str().unwrap_or(""), t["account_id"].as_str().unwrap_or(""));
                    let owner = if contact.is_empty() { crate::api::crm_accounts::ev("crm_account", account, Updated) } else { crate::api::crm_accounts::ev("crm_contact", contact, Updated) };
                    vec![crate::api::crm_accounts::ev("crm_touch", &id, Updated), owner]
                }
                Err(_) => vec![],
            };
            *w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = r.ok();
            Ok(WriteOutcome { applied, events })
        })
        .await;
    logged("done", r);
    let taken = out.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
    if let Some(mut t) = taken {
        t["done_by"] = json!(SENDER_SESSION);
        crate::api::crm_agent::emit(state, "crm.touch.done", t).await;
    }
}

// ---- warm-up ------------------------------------------------------------------------------

const OPENERS: &[&str] = &["Quick one", "Following up", "Thanks again", "Small update", "Checking in", "Notes from today", "Next week", "Re the plan", "One more thing", "Draft for review"];
const LINES: &[&str] = &[
    "I moved the review to Thursday so we have the numbers first.",
    "The draft is in the shared folder; the second section still needs a pass.",
    "Can you look at the timeline and tell me if the dates work on your side?",
    "We kept the scope small for now and will add the rest after the pilot.",
    "Thanks for the notes yesterday, they made the summary much clearer.",
    "I will send the updated list once the last two names are confirmed.",
    "The call went well and they asked for a short write-up by Friday.",
    "Let me know if mornings are easier for you next week.",
    "I added a few questions at the end; no rush on those.",
    "The invoice went out this morning, so we should be set for the month.",
    "We can skip the sync this week unless something comes up.",
    "I think the simpler option is fine and saves us a week.",
];

/// A warm-up note: varied, plain, with a reference only the sender knows.
pub(crate) fn warmup_note(seed: &str) -> (String, String, String) {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(seed.as_bytes());
    let pick = |i: usize, n: usize| (h[i] as usize) % n;
    let token = format!("{:02x}{:02x}{:02x}{:02x}{:02x}", h[20], h[21], h[22], h[23], h[24]);
    let subject = OPENERS[pick(0, OPENERS.len())].to_string();
    let (a, mut b) = (pick(1, LINES.len()), pick(2, LINES.len()));
    if b == a {
        b = (b + 1) % LINES.len();
    }
    let body = format!("Hi,\n\n{} {}\n\nRef {token}\n\nThanks", LINES[a], LINES[b]);
    (subject, body, token)
}

/// Warm-up mail an inbox should send today: START plus one a day since its
/// warm-up began, up to MAX.
pub(crate) fn warmup_target(started: &str) -> i64 {
    let since = chrono::NaiveDate::parse_from_str(started, "%Y-%m-%d").map(|d| (chrono::Local::now().date_naive() - d).num_days().max(0)).unwrap_or(0);
    (env_n("AMUX_CRM_WARMUP_START", 2) + since).min(env_n("AMUX_CRM_WARMUP_MAX", 10))
}

/// Pairs (from, to) to warm now: each eligible inbox under today's target and
/// 20 minutes past its last warm-up mails the inbox on another domain that
/// received the fewest today.
pub(crate) fn warmup_pairs(conn: &rusqlite::Connection, now: i64) -> Vec<(String, String)> {
    let pool: Vec<(String, String, String)> = conn
        .prepare("SELECT address, domain, warmup_started FROM crm_mailboxes WHERE status IN ('warming','active','reserve') ORDER BY address")
        .and_then(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect())
        .unwrap_or_default();
    let pool: Vec<_> = pool.into_iter().filter(|(a, _, _)| crate::api::crm_mailboxes::connected(a)).collect();
    let day = crate::api::crm_mailboxes::start_of_today();
    let count = |sql: &str, a: &str| conn.query_row(sql, rusqlite::params![a, day], |r| r.get::<_, i64>(0)).unwrap_or(0);
    let mut out = Vec::new();
    for (a, d, started) in &pool {
        let sent = count("SELECT COUNT(*) FROM crm_warmup WHERE from_addr=?1 AND sent_at>=?2", a);
        let last: i64 = conn.query_row("SELECT COALESCE(MAX(sent_at),0) FROM crm_warmup WHERE from_addr=?1", [a], |r| r.get(0)).unwrap_or(0);
        if sent >= warmup_target(started) || now - last < 20 * 60 {
            continue;
        }
        let to = pool.iter().filter(|(b, e, _)| b != a && e != d)
            .min_by_key(|(b, _, _)| (count("SELECT COUNT(*) FROM crm_warmup WHERE to_addr=?1 AND sent_at>=?2", b), b.clone()));
        if let Some((b, _, _)) = to {
            out.push((a.clone(), b.clone()));
        }
    }
    out
}

async fn warmup(state: &AppState, gmail: &crate::integrations::email::GmailClient) {
    if std::env::var("AMUX_CRM_WARMUP").is_ok_and(|v| v == "0") {
        return;
    }
    let now = crate::api::crm_accounts::now_secs();
    if in_window(chrono::Local::now()) {
        let pairs = read(state, move |c| warmup_pairs(c, now)).await.unwrap_or_default();
        for (from, to) in pairs {
            let id = ulid::Ulid::new().to_string();
            let (subject, body, token) = warmup_note(&id);
            match gmail.compose_send(&from, &to, &subject, &body, "", "", "", "", false, &[]).await {
                Ok(r) => {
                    crate::integrations::email::email_log(gmail.home(), json!({ "endpoint": "crm-warmup", "via": "gmail", "from": from, "to": to, "subject": subject, "session": SENDER_SESSION }));
                    let (gid, tid) = (r["id"].as_str().unwrap_or("").to_string(), r["thread_id"].as_str().unwrap_or("").to_string());
                    let r = state.store.write_async(move |c| {
                        c.execute("INSERT INTO crm_warmup (id,from_addr,to_addr,subject,token,sent_at,gmail_id,thread_id) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                            rusqlite::params![&id, &from, &to, &subject, &token, now, &gid, &tid])?;
                        Ok(WriteOutcome { applied: true, events: vec![] })
                    }).await;
                    logged("warmup_sent", r);
                    tracing::info!(target: "amux::crm", measured = true, n_considered = 1, verdict = "crm_warmup_sent", "a warm-up mail went between two pool inboxes");
                }
                Err(e) => tracing::warn!(target: "amux::crm", from = %from, to = %to, error = %e, measured = true, n_considered = 1, verdict = "crm_warmup_failed", "a warm-up mail did not send"),
            }
        }
    }
    // Where each copy landed, five minutes on; one hour without it is missing.
    let open: Vec<(String, String, String, String, i64)> = read(state, move |c| {
        c.prepare("SELECT id, to_addr, token, subject, sent_at FROM crm_warmup WHERE placement='' AND sent_at<=?1 ORDER BY sent_at LIMIT 20")
            .and_then(|mut st| st.query_map([now - 300], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect())
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    for (id, to, token, subject, sent_at) in open {
        let found = match gmail.locate_token(&to, &token).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(target: "amux::crm", to = %to, error = %e, measured = false, n_considered = 0, verdict = "crm_warmup_failed", "a warm-up copy could not be looked for");
                continue;
            }
        };
        let (placement, rescued) = match &found {
            None if now - sent_at < 3600 => continue,
            None => ("missing", false),
            Some((gid, labels)) if labels.iter().any(|l| l == "SPAM") => {
                let ok = gmail.modify_labels(&to, gid, &["INBOX", "IMPORTANT"], &["SPAM"]).await.is_ok();
                ("spam", ok)
            }
            Some((gid, labels)) => {
                if let Err(e) = gmail.modify_labels(&to, gid, &["IMPORTANT"], &["UNREAD"]).await {
                    tracing::warn!(target: "amux::crm", to = %to, error = %e, measured = true, n_considered = 1, verdict = "crm_warmup_failed", "a warm-up copy could not be marked important");
                }
                (if labels.iter().any(|l| l == "INBOX") { "inbox" } else { "other" }, false)
            }
        };
        // One in three is answered, in thread, from the receiver.
        let mut replied = false;
        if found.is_some() && id.bytes().last().is_some_and(|b| b % 3 == 0) {
            let from: String = read(state, { let id = id.clone(); move |c| c.query_row("SELECT from_addr FROM crm_warmup WHERE id=?1", [&id], |r| r.get(0)).unwrap_or_default() }).await.unwrap_or_default();
            let thread: String = read(state, { let id = id.clone(); move |c| c.query_row("SELECT thread_id FROM crm_warmup WHERE id=?1", [&id], |r| r.get(0)).unwrap_or_default() }).await.unwrap_or_default();
            let (_, line, _) = warmup_note(&format!("{id}-reply"));
            let text = line.lines().nth(2).unwrap_or("Sounds good, thanks.").to_string();
            replied = gmail.compose_send(&to, &from, &format!("Re: {subject}"), &format!("{text}\n\nThanks"), "", "", "", &thread, false, &[]).await.is_ok();
        }
        tracing::info!(target: "amux::crm", to = %to, placement, rescued, replied, measured = true, n_considered = 1, verdict = "crm_warmup_checked", "where a warm-up copy landed");
        let r = state.store.write_async(move |c| {
            c.execute("UPDATE crm_warmup SET placement=?2, checked_at=?3, rescued=?4, replied_at=?5 WHERE id=?1",
                rusqlite::params![&id, placement, now, rescued as i64, if replied { Some(now) } else { None }])?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        }).await;
        logged("warmup_checked", r);
    }
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, 60, move || {
        let state = state.clone();
        async move {
            let every = crate::config::env_i64(&super::per_job_disable_var(JOB), DEFAULT_SECS);
            if every <= 0 || std::env::var("AMUX_ISOLATED").is_ok_and(|v| v == "1") {
                return;
            }
            static LAST: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
            let now = chrono::Utc::now().timestamp();
            if now - LAST.load(std::sync::atomic::Ordering::Relaxed) < every {
                return;
            }
            LAST.store(now, std::sync::atomic::Ordering::Relaxed);
            let any: bool = read(&state, |c| c.query_row("SELECT 1 FROM crm_mailboxes LIMIT 1", [], |_| Ok(true)).unwrap_or(false)).await.unwrap_or(false);
            if !any {
                return;
            }
            let gmail = crate::integrations::email::GmailClient::new_default();
            refresh_dns(&state).await;
            send_cold(&state, &gmail).await;
            warmup(&state, &gmail).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> rusqlite::Connection {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate::apply_all(&mut c).unwrap();
        c
    }

    fn add_mailbox(c: &rusqlite::Connection, addr: &str, status: &str, started: &str, dns_ok: bool) {
        let dns = json!({ "domain": addr.rsplit('@').next(), "ok": dns_ok, "measured": true, "problems": if dns_ok { json!([]) } else { json!(["no DKIM record"]) } });
        c.execute("INSERT INTO crm_mailboxes (address,domain,status,daily_cap,warmup_started,warmup_days,notes,created,updated,dns,dns_checked_at) VALUES (?1,?2,?3,15,?4,14,'',1,1,?5,1)",
            rusqlite::params![addr, addr.rsplit('@').next().unwrap(), status, started, dns.to_string()]).unwrap();
    }

    fn add_touch(c: &rusqlite::Connection, id: &str, contact: &str, mailbox: &str, body: &str, approved_at: i64) {
        c.execute("INSERT OR IGNORE INTO crm_contacts (id,name,email,created,updated) VALUES (?1,?1,?2,1,1)", rusqlite::params![contact, format!("{contact}@acme.io")]).unwrap();
        c.execute("INSERT INTO crm_touches (id,contact_id,channel,action,body,status,approved_at,mailbox,created,updated) VALUES (?1,?2,'email','email',?3,'approved',?4,?5,1,1)",
            rusqlite::params![id, contact, body, approved_at, mailbox]).unwrap();
    }

    #[test]
    fn one_send_per_inbox_in_the_window_spaced_capped_and_threaded() {
        let c = db();
        add_mailbox(&c, "a@usemixpeek.com", "active", "", true);
        add_mailbox(&c, "b@trymixpeek.com", "active", "", false);
        add_mailbox(&c, "c@getmixpeek.com", "paused", "", true);
        add_touch(&c, "T1", "ada", "a@usemixpeek.com", "Subject: Your catalog\n\nHi Ada, ...", 10);
        add_touch(&c, "T2", "bo", "a@usemixpeek.com", "Subject: Hello\n\nHi Bo", 11);
        add_touch(&c, "T3", "cy", "b@trymixpeek.com", "Subject: Hi\n\nHi Cy", 12);
        add_touch(&c, "T4", "di", "c@getmixpeek.com", "Subject: Hi\n\nHi Di", 13);
        let now = crate::api::crm_accounts::now_secs();
        let (s, held) = due(&c, now, true);
        assert_eq!(s.len(), 1, "one per inbox, and only the ready inbox: {s:?} {held:?}");
        assert_eq!((s[0].touch.as_str(), s[0].subject.as_str(), s[0].body.as_str(), s[0].to.as_str()), ("T1", "Your catalog", "Hi Ada, ...", "ada@acme.io"));
        let why: Vec<String> = held.iter().map(|h| h.1.clone()).collect();
        assert!(why.iter().any(|w| w.contains("DNS")) && why.iter().any(|w| w.contains("paused")), "{why:?}");
        assert!(due(&c, now, false).0.is_empty(), "nothing outside the window");
        // T1 sent just now: a's next waits for the gap.
        c.execute("UPDATE crm_touches SET status='done', done_at=?1, sent_thread='th1', sent_subject='Your catalog' WHERE id='T1'", [now]).unwrap();
        let (s, held) = due(&c, now + 60, true);
        assert!(s.is_empty() && held.iter().any(|h| h.1.starts_with("spacing")), "{held:?}");
        assert_eq!(due(&c, now + gap_secs("T2") + 1, true).0[0].touch, "T2");
        // A follow-up to Ada with no subject goes in her thread as Re:.
        add_touch(&c, "T5", "ada", "a@usemixpeek.com", "Just bumping this.", 1);
        let s = due(&c, now + 2 * 3600, true).0;
        let f = s.iter().find(|x| x.touch == "T5").expect("the follow-up is due before T2 (approved earlier)");
        assert_eq!((f.thread.as_str(), f.subject.as_str()), ("th1", "Re: Your catalog"));
    }

    #[test]
    fn a_ramping_inbox_sends_no_more_than_its_cap_today() {
        let c = db();
        let started = (chrono::Local::now().date_naive() - chrono::Duration::days(14)).format("%Y-%m-%d").to_string();
        add_mailbox(&c, "a@usemixpeek.com", "warming", &started, true);
        assert_eq!(crate::api::crm_mailboxes::cap_today("warming", 15, &started, 14), 5, "day one after warm-up starts at 5");
        let now = crate::api::crm_accounts::now_secs();
        for i in 0..5 {
            add_touch(&c, &format!("D{i}"), &format!("p{i}"), "a@usemixpeek.com", "Subject: x\n\ny", 1);
            c.execute("UPDATE crm_touches SET status='done', done_at=?1 WHERE id=?2", rusqlite::params![now - 3600, format!("D{i}")]).unwrap();
        }
        add_touch(&c, "N", "q", "a@usemixpeek.com", "Subject: x\n\ny", 2);
        let (s, held) = due(&c, now, true);
        assert!(s.is_empty() && held[0].1.contains("5 of 5"), "{held:?}");
    }

    #[test]
    fn the_window_gap_and_warmup_note_are_computed() {
        use chrono::TimeZone;
        let mon10 = chrono::Local.with_ymd_and_hms(2026, 11, 30, 10, 0, 0).unwrap();
        let sat10 = chrono::Local.with_ymd_and_hms(2026, 11, 28, 10, 0, 0).unwrap();
        let mon20 = chrono::Local.with_ymd_and_hms(2026, 11, 30, 20, 0, 0).unwrap();
        assert!(in_window(mon10) && !in_window(sat10) && !in_window(mon20));
        let g = gap_secs("T1");
        assert!((540..1080).contains(&g) && g != gap_secs("T2"), "9 minutes plus a jitter that differs per touch");
        let (s, b, t) = warmup_note("x");
        assert!(!s.is_empty() && b.contains(&t) && t.len() == 10);
        assert_ne!(warmup_note("y").1, b);
    }

    #[test]
    fn warmup_pairs_cross_domains_and_respect_the_daily_target() {
        let c = db();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        add_mailbox(&c, "a@usemixpeek.com", "warming", &today, true);
        add_mailbox(&c, "b@trymixpeek.com", "warming", &today, true);
        add_mailbox(&c, "a2@usemixpeek.com", "paused", &today, true);
        let now = crate::api::crm_accounts::now_secs();
        let p = warmup_pairs(&c, now);
        assert_eq!(p, vec![("a@usemixpeek.com".to_string(), "b@trymixpeek.com".to_string()), ("b@trymixpeek.com".to_string(), "a@usemixpeek.com".to_string())]);
        for i in 0..2 {
            c.execute("INSERT INTO crm_warmup (id,from_addr,to_addr,sent_at) VALUES (?1,'a@usemixpeek.com','b@trymixpeek.com',?2)", rusqlite::params![format!("w{i}"), now - 3600 * (i + 1)]).unwrap();
        }
        assert_eq!(warmup_pairs(&c, now).len(), 1, "a met its first day's target of 2");
    }
}
