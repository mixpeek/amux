//! The sending pool (owner, 2026-11-27: "Do it", on a cold-email setup of
//! Google inboxes only, 2 inboxes per domain, 15 sends a day per inbox, a 14
//! day warm up and 20% of inboxes in reserve; and "use this as an alternative
//! to Instantly ... across different domains using different emails that
//! leverages the existing amux connectors").
//!
//! A mailbox is one sending inbox: a connected Google account
//! (`/api/gmail/auth?account=`), its status, daily cap and warm-up. A
//! campaign names the mailboxes its email goes out from (crm_campaigns.rs);
//! naming one there registers it here as warming from that day.
//!
//! When the owner approves a cold email in a campaign that has mailboxes,
//! [`pick`] gives it the ready inbox with the fewest sends today, and the touch
//! carries it (`mailbox`) so its worker sends `from` it. Ready means: active (or
//! warming with its warm-up days done), not in reserve or paused, connected,
//! and under today's cap. When none is, the approval is refused as
//! `crm_mailbox_cap` and says why, never sent from a tired inbox.
//!
//!   GET    /api/crm/mailboxes            each inbox with today's sends, 7-day sends, bounce and reply rates, and the pool's health
//!   POST   /api/crm/mailboxes            {address, status?, daily_cap?, warmup_started?, warmup_days?, notes?, force?}
//!   PATCH  /api/crm/mailboxes/{address}, DELETE
//!
//! Defaults: AMUX_CRM_MAILBOX_DAILY_CAP (15), AMUX_CRM_MAILBOX_WARMUP_DAYS (14),
//! AMUX_CRM_MAILBOX_PER_DOMAIN (2: a third live inbox on a domain is refused as
//! `crm_mailbox_domain_full` unless `force`), AMUX_CRM_MAILBOX_RESERVE_PCT (20:
//! the pool's health says when fewer are held in reserve).
//!
//! Warm-up is the clock and the traffic: a warming inbox carries no cold email
//! until its days are done, and meanwhile the pool's own inboxes mail each
//! other (runtime_jobs/crm_sender.rs), each receiver takes its copy out of
//! Spam, marks it important and sometimes replies; where each copy landed is
//! the inbox's `placement_7d` (inbox share of the warm-up mail it sent).
//!
//! What else keeps cold mail landing (owner, 2026-11-27: "all the capabilities
//! as instantly that are important to land in inbox ... without sacrificing our
//! own domain"):
//!
//! - The CRM sends an approved email itself (crm_sender.rs): from the picked
//!   inbox, exactly as approved, in the send window (AMUX_CRM_SEND_HOURS,
//!   default 8-17 local, weekdays unless AMUX_CRM_SEND_WEEKENDS=1), at least
//!   AMUX_CRM_SEND_GAP_MIN (default 9) minutes after that inbox's last send
//!   plus a jitter of up to as many again, with a `List-Unsubscribe` mailto
//!   header (a reply or that mail saying unsubscribe opts them out). A first
//!   email starts with a `Subject:` line ([`subject_refusal`]); a follow-up
//!   without one goes in the same thread as "Re:". No open or click tracking:
//!   a pixel and rewritten links are spam signals.
//! - Ramp ([`cap_today`]): after warm-up an inbox starts at
//!   AMUX_CRM_MAILBOX_RAMP_START (5) a day and adds AMUX_CRM_MAILBOX_RAMP_STEP
//!   (2) a day up to its cap.
//! - Bounces ([`bounce_of`], [`record_bounce`]): a delivery failure in a pool
//!   inbox marks the person bounced (no more mail to them) and pauses the
//!   inbox when its 7-day bounce rate is over AMUX_CRM_MAILBOX_MAX_BOUNCE_PCT
//!   (3) with at least 20 sends and 2 bounces, emitting crm.mailbox.paused.
//! - DNS ([`dns_check`]): each sending domain's MX, SPF, DKIM (Google's
//!   selector), DMARC and Spamhaus DBL listing, checked daily. Missing SPF,
//!   DKIM or MX, or a listing, makes the inbox not ready, and says which.
//!   The owner's own domain stays out of cold mail by being out of the pool.

use super::calendar::query_rows_json;
use super::crm_accounts::{ev, now_secs};
use super::{internal, AppState};
use crate::db::WriteOutcome;
use amux_core::revision::MutationKind;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch};
use axum::{Json, Router};
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/mailboxes", get(list_route).post(create_route))
        .route("/mailboxes/{address}", patch(patch_route).delete(delete_route))
}

pub const STATUSES: &[&str] = &["warming", "active", "reserve", "paused"];

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn cap_default() -> i64 { crate::config::env_i64("AMUX_CRM_MAILBOX_DAILY_CAP", 15) }
fn warmup_default() -> i64 { crate::config::env_i64("AMUX_CRM_MAILBOX_WARMUP_DAYS", 14) }
fn per_domain() -> i64 { crate::config::env_i64("AMUX_CRM_MAILBOX_PER_DOMAIN", 2) }
fn reserve_pct() -> i64 { crate::config::env_i64("AMUX_CRM_MAILBOX_RESERVE_PCT", 20) }

fn today() -> chrono::NaiveDate {
    chrono::Local::now().date_naive()
}

/// Seconds at the start of today, local time: "sends today" counts from here.
pub(crate) fn start_of_today() -> i64 {
    today().and_hms_opt(0, 0, 0).and_then(|t| t.and_local_timezone(chrono::Local).single()).map(|t| t.timestamp()).unwrap_or(0)
}

pub(crate) fn domain_of(addr: &str) -> String {
    addr.rsplit('@').next().unwrap_or("").to_string()
}

/// Whether the address is a connected Google account (a token file the Gmail
/// connector wrote). Tests never read the real home: an address starting
/// "unconnected" reads as not connected, any other as connected.
#[cfg(not(test))]
pub(crate) fn connected(addr: &str) -> bool {
    crate::config::amux_home().join("gmail-tokens").join(format!("{addr}.json")).exists()
}
#[cfg(test)]
pub(crate) fn connected(addr: &str) -> bool {
    !addr.starts_with("unconnected")
}

/// When a warming inbox's warm-up ends, as YYYY-MM-DD ("" when it has no start).
fn warmup_ends(started: &str, days: i64) -> String {
    chrono::NaiveDate::parse_from_str(started, "%Y-%m-%d").ok()
        .map(|d| (d + chrono::Duration::days(days.max(0))).format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn env_n(k: &str, d: i64) -> i64 { crate::config::env_i64(k, d) }

/// Today's sending cap for an inbox: warming sends none; an inbox past its
/// warm-up ramps from RAMP_START a day by RAMP_STEP a day to its cap. An
/// active inbox with no warm-up date (an old, already-warm inbox) gets its cap.
pub(crate) fn cap_today(status: &str, cap: i64, started: &str, days: i64) -> i64 {
    if status != "active" && status != "warming" {
        return 0;
    }
    let Ok(d) = chrono::NaiveDate::parse_from_str(started, "%Y-%m-%d") else {
        return if status == "active" { cap } else { 0 };
    };
    let since_ready = (today() - (d + chrono::Duration::days(days.max(0)))).num_days();
    if since_ready < 0 {
        // The owner set it active by hand: it is warm already.
        return if status == "active" { cap } else { 0 };
    }
    (env_n("AMUX_CRM_MAILBOX_RAMP_START", 5) + env_n("AMUX_CRM_MAILBOX_RAMP_STEP", 2) * since_ready).clamp(0, cap)
}

/// An email body's subject: a first line `Subject: ...`, and the rest after it.
pub(crate) fn split_subject(body: &str) -> (Option<String>, String) {
    let t = body.trim_start();
    let first = t.lines().next().unwrap_or("");
    if let Some(rest) = first.strip_prefix("Subject:").or_else(|| first.strip_prefix("subject:")) {
        let subject = rest.trim().to_string();
        let after = t[first.len()..].trim_start_matches(['\r', '\n']).to_string();
        if !subject.is_empty() {
            return (Some(subject), after);
        }
    }
    (None, body.to_string())
}

/// The thread a follow-up email goes in: the newest earlier email the CRM sent
/// this person from the same inbox, as (thread id, subject).
pub(crate) fn thread_for(conn: &rusqlite::Connection, contact: &str, mailbox: &str, not: &str) -> Option<(String, String)> {
    conn.query_row(
        "SELECT sent_thread, sent_subject FROM crm_touches WHERE contact_id=?1 AND mailbox=?2 AND id<>?3 AND channel='email' AND status='done' \
         AND sent_thread<>'' AND deleted IS NULL ORDER BY done_at DESC LIMIT 1",
        [contact, mailbox, not], |r| Ok((r.get(0)?, r.get(1)?))).ok()
}

/// Why an email the CRM will send cannot be approved as written: a first email
/// (nothing earlier to this person from the pool to thread under) without a
/// `Subject:` line.
pub(crate) fn subject_refusal(conn: &rusqlite::Connection, touch: &str) -> Option<String> {
    let (body, contact, mailbox): (String, String, String) = conn.query_row("SELECT body, contact_id, mailbox FROM crm_touches WHERE id=?1", [touch], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).ok()?;
    if split_subject(&body).0.is_some() {
        return None;
    }
    // The same test the sender threads by: an earlier email from this inbox
    // (land review F1: any inbox here let a follow-up be approved that the
    // sender, finding no thread in its own inbox, then held forever).
    if thread_for(conn, &contact, &mailbox, touch).is_some() {
        return None;
    }
    Some("the CRM sends this email itself, exactly as approved: start the body with a line `Subject: ...` (a follow-up without one goes in the same thread as Re:)".into())
}

/// The address a delivery-failure notice says it could not reach, or None
/// when the mail is not a bounce.
pub(crate) fn bounce_of(d: &Value) -> Option<String> {
    let from = d.get("from_address").and_then(Value::as_str).unwrap_or("").to_lowercase();
    let subject = d.get("subject").and_then(Value::as_str).unwrap_or("").to_lowercase();
    let daemon = from.starts_with("mailer-daemon@") || from.starts_with("postmaster@");
    let notice = ["delivery status notification", "undeliverable", "delivery failure", "returned mail", "undelivered mail", "failure notice"].iter().any(|k| subject.contains(k));
    if !daemon || !notice {
        return None;
    }
    let text = super::crm_agent::unescape_html(d.get("snippet").and_then(Value::as_str).unwrap_or(""));
    let re = regex::Regex::new(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}").ok()?;
    let account = d.get("account").and_then(Value::as_str).unwrap_or("").to_lowercase();
    let found = re.find_iter(&text).map(|m| m.as_str().trim_end_matches('.').to_lowercase()).find(|a| *a != account && !a.starts_with("mailer-daemon@"));
    found
}

/// A bounce reached `inbox` for `failed`: the person reads bounced, and the
/// inbox is paused when it bounces too much. Some(the pause) when it paused.
pub(crate) fn record_bounce(conn: &rusqlite::Connection, inbox: &str, failed: &str) -> rusqlite::Result<Option<Value>> {
    let now = now_secs();
    let n = conn.execute(
        "UPDATE crm_contacts SET bounced_at=?2, email_check=?3, updated=?2 WHERE lower(email)=?1 AND deleted IS NULL AND bounced_at IS NULL",
        rusqlite::params![failed, now, json!({ "address": failed, "ok": false, "why": "a send to it bounced", "measured": true, "checked_at": now }).to_string()],
    )?;
    tracing::info!(target: "amux::crm", inbox, marked = n, measured = true, n_considered = 1, verdict = "crm_bounce_recorded", "a cold email bounced; the person reads bounced");
    let row: Option<(String, String)> = conn.query_row("SELECT status, address FROM crm_mailboxes WHERE address=?1", [inbox], |r| Ok((r.get(0)?, r.get(1)?))).ok();
    let Some((status, addr)) = row else { return Ok(None) };
    if status == "paused" {
        return Ok(None);
    }
    let week = now - 7 * 86_400;
    let sent: i64 = conn.query_row("SELECT COUNT(*) FROM crm_touches WHERE mailbox=?1 AND status='done' AND done_at>=?2 AND deleted IS NULL", rusqlite::params![&addr, week], |r| r.get(0))?;
    let bounced: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT t.contact_id) FROM crm_touches t JOIN crm_contacts c ON c.id=t.contact_id WHERE t.mailbox=?1 AND t.status='done' AND t.done_at>=?2 \
         AND c.bounced_at IS NOT NULL AND c.bounced_at>=t.done_at", rusqlite::params![&addr, week], |r| r.get(0))?;
    let max = env_n("AMUX_CRM_MAILBOX_MAX_BOUNCE_PCT", 3);
    // Enough sends to judge, and more than one bounce: one bad address in a
    // small batch is the list's fault, which marking the person handles.
    if sent < 20 || bounced < 2 || bounced * 100 <= sent * max {
        return Ok(None);
    }
    let why = format!("{bounced} of {sent} sends bounced in 7 days, over {max}%: clean the list before it sends again");
    conn.execute("UPDATE crm_mailboxes SET status='paused', paused_reason=?2, updated=?3 WHERE address=?1", rusqlite::params![&addr, &why, now])?;
    Ok(Some(json!({ "address": addr, "why": why, "bounced_7d": bounced, "sent_7d": sent, "bounce_rate": (bounced as f64 / sent as f64 * 1000.0).round() / 1000.0 })))
}

// ---- DNS ----------------------------------------------------------------------------------

/// One DNS question to the first nameserver in /etc/resolv.conf over UDP:
/// (rcode, the answers' rdata of type `qtype`). None when it could not ask.
#[cfg(not(test))]
fn dns_ask(name: &str, qtype: u16) -> Option<(u8, Vec<Vec<u8>>)> {
    let ns = std::fs::read_to_string("/etc/resolv.conf").ok()?
        .lines().find_map(|l| l.trim().strip_prefix("nameserver").map(|s| s.trim().to_string()))?;
    let addr: std::net::IpAddr = ns.parse().ok()?;
    let sock = std::net::UdpSocket::bind(if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).ok()?;
    sock.set_read_timeout(Some(std::time::Duration::from_secs(3))).ok()?;
    let id: u16 = (now_secs() as u16) ^ qtype;
    sock.send_to(&dns_query(id, name, qtype), (addr, 53)).ok()?;
    let mut buf = [0u8; 4096];
    let (n, _) = sock.recv_from(&mut buf).ok()?;
    dns_parse(&buf[..n], id, qtype)
}

/// Tests never touch the network: `good.test` is fully set up, `bare.test`
/// has mail and nothing else, `listed.test` is on the DBL.
#[cfg(test)]
fn dns_ask(name: &str, qtype: u16) -> Option<(u8, Vec<Vec<u8>>)> {
    let txt = |s: &str| { let mut v = vec![s.len() as u8]; v.extend_from_slice(s.as_bytes()); v };
    let good = name.ends_with("good.test") || name.ends_with("listed.test");
    Some(match (name, qtype) {
        (n, 15) if n.ends_with(".test") && !n.ends_with("dbl.spamhaus.org") => (0, vec![vec![0, 1, 2, b'm', b'x', 0]]),
        (n, 16) if good && n.starts_with("_dmarc.") => (0, vec![txt("v=DMARC1; p=none")]),
        (n, 16) if good && n.starts_with("google._domainkey.") => (0, vec![txt("v=DKIM1; k=rsa; p=MII")]),
        (_, 16) if good => (0, vec![txt("v=spf1 include:_spf.google.com ~all")]),
        ("listed.test.dbl.spamhaus.org", 1) => (0, vec![vec![127, 0, 1, 2]]),
        (_, _) => (3, vec![]),
    })
}

#[cfg_attr(test, allow(dead_code))]
fn dns_query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
    let mut q = Vec::with_capacity(32 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.trim_end_matches('.').split('.') {
        q.push(label.len().min(63) as u8);
        q.extend_from_slice(&label.as_bytes()[..label.len().min(63)]);
    }
    q.push(0); // the root
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&[0, 1]);
    q
}

#[cfg_attr(test, allow(dead_code))]
fn dns_parse(b: &[u8], id: u16, qtype: u16) -> Option<(u8, Vec<Vec<u8>>)> {
    if b.len() < 12 || u16::from_be_bytes([b[0], b[1]]) != id || b[2] & 0x80 == 0 {
        return None;
    }
    let rcode = b[3] & 0x0f;
    let (qd, an) = (u16::from_be_bytes([b[4], b[5]]), u16::from_be_bytes([b[6], b[7]]));
    fn skip_name(b: &[u8], mut i: usize) -> Option<usize> {
        loop {
            let l = *b.get(i)? as usize;
            if l == 0 { return Some(i + 1); }
            if l & 0xc0 == 0xc0 { return Some(i + 2); }
            i += 1 + l;
        }
    }
    let mut i = 12;
    for _ in 0..qd {
        i = skip_name(b, i)? + 4;
    }
    let mut out = Vec::new();
    for _ in 0..an {
        i = skip_name(b, i)?;
        let ty = u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]);
        let len = u16::from_be_bytes([*b.get(i + 8)?, *b.get(i + 9)?]) as usize;
        let rd = i + 10;
        if ty == qtype {
            out.push(b.get(rd..rd + len)?.to_vec());
        }
        i = rd + len;
    }
    Some((rcode, out))
}

/// The TXT strings of a name, each record's character-strings joined.
fn txt_records(name: &str) -> Option<Vec<String>> {
    let (rcode, rds) = dns_ask(name, 16)?;
    if rcode != 0 && rcode != 3 {
        return None;
    }
    Some(rds.iter().map(|rd| {
        let (mut i, mut s) = (0usize, String::new());
        while i < rd.len() {
            let l = rd[i] as usize;
            s.push_str(&String::from_utf8_lossy(rd.get(i + 1..(i + 1 + l).min(rd.len())).unwrap_or(&[])));
            i += 1 + l;
        }
        s
    }).collect())
}

/// A sending domain's mail setup: MX, SPF, DKIM (Google's `google` selector),
/// DMARC and whether Spamhaus' domain blocklist lists it. `ok` false names
/// what is missing in `problems`; `measured` false when the lookups could not
/// run (no answer is not a missing record).
pub(crate) fn dns_check(domain: &str) -> Value {
    let mx = dns_ask(domain, 15).map(|(rc, rds)| rc == 0 && rds.iter().any(|r| !(r.len() == 3 && r[2] == 0)));
    let spf = txt_records(domain).map(|v| v.into_iter().find(|t| t.starts_with("v=spf1")).unwrap_or_default());
    let dkim = txt_records(&format!("google._domainkey.{domain}")).map(|v| v.iter().any(|t| t.contains("v=DKIM1") || t.contains("p=")));
    let dmarc = txt_records(&format!("_dmarc.{domain}")).map(|v| v.into_iter().find(|t| t.starts_with("v=DMARC1")).unwrap_or_default());
    // DBL: an A record 127.0.1.x means listed; 127.255.255.x means Spamhaus
    // refused the resolver (public resolvers are refused), which is unmeasured.
    let dbl = dns_ask(&format!("{domain}.dbl.spamhaus.org"), 1).map(|(rc, rds)| {
        if rc == 3 { Value::Bool(false) }
        else if rds.iter().any(|r| r.len() == 4 && r[0] == 127 && r[1] == 0 && r[2] == 1) { Value::Bool(true) }
        else { Value::Null }
    });
    let measured = mx.is_some() && spf.is_some() && dkim.is_some() && dmarc.is_some();
    let mut problems = Vec::new();
    if mx == Some(false) { problems.push("no MX record: replies cannot arrive".to_string()); }
    if spf.as_deref() == Some("") { problems.push("no SPF record (v=spf1 include:_spf.google.com ~all)".into()); }
    if dkim == Some(false) { problems.push("no DKIM record at google._domainkey (turn on DKIM in the Google admin console)".into()); }
    if dmarc.as_deref() == Some("") { problems.push("no DMARC record at _dmarc (v=DMARC1; p=none to start)".into()); }
    if dbl == Some(Value::Bool(true)) { problems.push("listed on the Spamhaus domain blocklist".into()); }
    json!({ "domain": domain, "mx": mx, "spf": spf, "dkim": dkim, "dmarc": dmarc, "dbl_listed": dbl.unwrap_or(Value::Null),
        "ok": measured && problems.is_empty(), "problems": problems, "measured": measured, "checked_at": now_secs() })
}

/// Why the domain's stored DNS check stops cold mail, or None (also None when
/// it was never checked: the sender checks it before the first send).
fn dns_refusal(dns: &str) -> Option<String> {
    let v: Value = serde_json::from_str(dns).ok()?;
    if v["measured"] != json!(true) || v["ok"] == json!(true) {
        return None;
    }
    let p: Vec<&str> = v["problems"].as_array()?.iter().filter_map(Value::as_str).collect();
    (!p.is_empty()).then(|| format!("{}: {}", v["domain"].as_str().unwrap_or(""), p.join("; ")))
}

/// The pool's warm-up placement over 7 days: (sent and checked, landed in the inbox).
fn placement(conn: &rusqlite::Connection, addr: &str) -> (i64, i64) {
    conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(placement='inbox'),0) FROM crm_warmup WHERE from_addr=?1 AND placement<>'' AND sent_at>=?2",
        rusqlite::params![addr, now_secs() - 7 * 86_400], |r| Ok((r.get(0)?, r.get(1)?))).unwrap_or((0, 0))
}

/// Sends from a mailbox since `since`: cold emails done, plus approved ones
/// about to go (an approval spends a slot as soon as it is given).
fn sends_since(conn: &rusqlite::Connection, addr: &str, since: i64) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM crm_touches WHERE mailbox=?1 AND deleted IS NULL \
         AND ((status='done' AND done_at>=?2) OR (status='approved' AND approved_at>=?2))",
        rusqlite::params![addr, since],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// Why a mailbox cannot carry a cold email now, or None when it is ready.
pub(crate) fn not_ready(conn: &rusqlite::Connection, row: &(String, String, i64, String, i64)) -> Option<String> {
    let (addr, status, cap, started, days) = row;
    match status.as_str() {
        "reserve" | "paused" => return Some(format!("{addr} is {status}")),
        "warming" => {
            let ends = warmup_ends(started, *days);
            if ends.is_empty() || ends.as_str() > today().format("%Y-%m-%d").to_string().as_str() {
                return Some(format!("{addr} is warming up until {}", if ends.is_empty() { "its start is set".into() } else { ends }));
            }
        }
        _ => {}
    }
    if !connected(addr) {
        return Some(format!("{addr} is not a connected Google account (connect it at /api/gmail/auth?account={addr})"));
    }
    let dns: String = conn.query_row("SELECT dns FROM crm_mailboxes WHERE address=?1", [addr], |r| r.get(0)).unwrap_or_default();
    if let Some(why) = dns_refusal(&dns) {
        return Some(why);
    }
    let today_cap = cap_today(status, *cap, started, *days);
    let sent = sends_since(conn, addr, start_of_today());
    if sent >= today_cap {
        return Some(if today_cap < *cap { format!("{addr} sent {sent} of {today_cap} today (ramping up to {cap})") } else { format!("{addr} sent {sent} of {cap} today") });
    }
    None
}

/// The mailbox an approved cold email goes out from: the ready inbox of the
/// campaign's pool with the fewest sends today. Ok(None) when the touch is not
/// an email in a campaign with mailboxes (it sends as before); Err(why) when
/// the pool has no ready inbox.
pub(crate) fn pick(conn: &rusqlite::Connection, touch: &str) -> Result<Option<String>, String> {
    let row: Option<(String, String, String)> = conn
        .query_row("SELECT channel, sequence, mailbox FROM crm_touches WHERE id=?1", [touch], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .ok();
    let Some((channel, seq, current)) = row else { return Ok(None) };
    if channel != "email" || seq.is_empty() {
        return Ok(None);
    }
    let pool: Vec<String> = conn
        .query_row(
            "SELECT c.mailboxes FROM crm_sequences s JOIN crm_campaigns c ON c.id=s.campaign_id WHERE s.id=?1 AND c.deleted IS NULL",
            [&seq],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|m| serde_json::from_str::<Vec<String>>(&m).ok())
        .unwrap_or_default();
    if pool.is_empty() {
        return Ok(None);
    }
    if !current.is_empty() && pool.contains(&current) {
        return Ok(Some(current));
    }
    let mut why = Vec::new();
    let mut best: Option<(i64, String)> = None;
    for addr in &pool {
        let r: Option<(String, String, i64, String, i64)> = conn
            .query_row("SELECT address, status, daily_cap, warmup_started, warmup_days FROM crm_mailboxes WHERE address=?1", [addr],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .ok();
        let Some(r) = r else { why.push(format!("{addr} is not in the pool")); continue };
        if let Some(w) = not_ready(conn, &r) {
            why.push(w);
            continue;
        }
        let sent = sends_since(conn, addr, start_of_today());
        if best.as_ref().is_none_or(|(n, _)| sent < *n) {
            best = Some((sent, addr.clone()));
        }
    }
    match best {
        Some((_, a)) => Ok(Some(a)),
        None => Err(format!("no mailbox in this campaign's pool can send now: {}", why.join("; "))),
    }
}

/// Register a campaign's mailboxes that are not in the pool yet: warming from
/// today. One over the per-domain limit is registered paused, with why.
pub(crate) fn ensure(conn: &rusqlite::Connection, addrs: &[String]) -> rusqlite::Result<usize> {
    let mut n = 0;
    for a in addrs.iter().map(|a| a.trim().to_lowercase()).filter(|a| !a.is_empty()) {
        let a = &a;
        let exists: bool = conn.query_row("SELECT 1 FROM crm_mailboxes WHERE address=?1", [a], |_| Ok(true)).unwrap_or(false);
        if exists {
            continue;
        }
        let d = domain_of(a);
        let live: i64 = conn.query_row("SELECT COUNT(*) FROM crm_mailboxes WHERE domain=?1 AND status<>'paused'", [&d], |r| r.get(0)).unwrap_or(0);
        let (status, note) = if live >= per_domain() { ("paused", format!("{d} already has {live} live inboxes")) } else { ("warming", String::new()) };
        conn.execute(
            "INSERT INTO crm_mailboxes (address,domain,status,daily_cap,warmup_started,warmup_days,notes,created,updated) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8)",
            rusqlite::params![a, d, status, cap_default(), today().format("%Y-%m-%d").to_string(), warmup_default(), note, now_secs()],
        )?;
        n += 1;
    }
    if n > 0 {
        tracing::info!(target: "amux::crm", registered = n, measured = true, n_considered = addrs.len(), verdict = "crm_mailbox_registered",
            "a campaign's mailboxes joined the sending pool, warming from today");
    }
    Ok(n)
}

/// Each mailbox with what it sent and how that went, and the pool's health.
fn pool(conn: &rusqlite::Connection) -> rusqlite::Result<Value> {
    let rows = query_rows_json(conn, "SELECT * FROM crm_mailboxes ORDER BY domain, address", &[])?;
    let (day, week) = (start_of_today(), now_secs() - 7 * 86_400);
    let mut out = Vec::new();
    let (mut ready, mut capacity, mut reserve) = (0i64, 0i64, 0i64);
    let mut per: std::collections::BTreeMap<String, i64> = Default::default();
    for mut m in rows {
        let addr = m["address"].as_str().unwrap_or("").to_string();
        let status = m["status"].as_str().unwrap_or("").to_string();
        let cap = m["daily_cap"].as_i64().unwrap_or(0);
        let started = m["warmup_started"].as_str().unwrap_or("").to_string();
        let days = m["warmup_days"].as_i64().unwrap_or(0);
        let sent_total: i64 = conn.query_row("SELECT COUNT(*) FROM crm_touches WHERE mailbox=?1 AND status='done' AND deleted IS NULL", [&addr], |r| r.get(0)).unwrap_or(0);
        let bounced: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT t.contact_id) FROM crm_touches t JOIN crm_contacts c ON c.id=t.contact_id WHERE t.mailbox=?1 AND t.status='done' \
             AND c.bounced_at IS NOT NULL AND c.bounced_at>=t.done_at", [&addr], |r| r.get(0)).unwrap_or(0);
        let replied: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT t.contact_id) FROM crm_touches t WHERE t.mailbox=?1 AND t.status='done' \
             AND EXISTS (SELECT 1 FROM crm_interactions i WHERE i.contact_id=t.contact_id AND i.direction='in' AND i.created>=t.done_at)", [&addr], |r| r.get(0)).unwrap_or(0);
        let why = not_ready(conn, &(addr.clone(), status.clone(), cap, started.clone(), days));
        let today_cap = cap_today(&status, cap, &started, days);
        if why.is_none() {
            ready += 1;
            capacity += today_cap;
        }
        if status == "reserve" {
            reserve += 1;
        }
        if status != "paused" {
            *per.entry(domain_of(&addr)).or_default() += 1;
        }
        let rate = |a: i64| if sent_total == 0 { Value::Null } else { json!((a as f64 / sent_total as f64 * 1000.0).round() / 1000.0) };
        m["sent_today"] = json!(sends_since(conn, &addr, day));
        m["sent_7d"] = json!(sends_since(conn, &addr, week));
        m["sent_total"] = json!(sent_total);
        m["bounce_rate"] = rate(bounced);
        m["reply_rate"] = rate(replied);
        m["warmup_ends"] = json!(warmup_ends(&started, days));
        m["connected"] = json!(connected(&addr));
        m["cap_today"] = json!(today_cap);
        m["dns"] = serde_json::from_str::<Value>(m["dns"].as_str().unwrap_or("")).unwrap_or(Value::Null);
        let (checked, landed) = placement(conn, &addr);
        m["warmup_checked_7d"] = json!(checked);
        m["placement_7d"] = if checked == 0 { Value::Null } else { json!((landed as f64 / checked as f64 * 1000.0).round() / 1000.0) };
        m["ready"] = json!(why.is_none());
        m["why_not_ready"] = json!(why);
        out.push(m);
    }
    let total = out.len() as i64;
    let target = (total * reserve_pct() + 99) / 100;
    let crowded: Vec<String> = per.iter().filter(|(_, n)| **n > per_domain()).map(|(d, n)| format!("{d} has {n} live inboxes; the rule is {}", per_domain())).collect();
    Ok(json!({
        "mailboxes": out, "measured": true, "n_considered": total,
        "health": { "total": total, "ready": ready, "sends_left_today": capacity - out.iter().filter(|m| m["ready"] == true).map(|m| m["sent_today"].as_i64().unwrap_or(0)).sum::<i64>(),
            "capacity_today": capacity, "reserve": reserve, "reserve_target": target, "reserve_short": (target - reserve).max(0), "crowded_domains": crowded,
            "rules": { "daily_cap": cap_default(), "warmup_days": warmup_default(), "per_domain": per_domain(), "reserve_pct": reserve_pct(),
                "ramp_start": env_n("AMUX_CRM_MAILBOX_RAMP_START", 5), "ramp_step": env_n("AMUX_CRM_MAILBOX_RAMP_STEP", 2),
                "max_bounce_pct": env_n("AMUX_CRM_MAILBOX_MAX_BOUNCE_PCT", 3), "send_hours": crate::runtime_jobs::crm_sender::hours().0.to_string() + "-" + &crate::runtime_jobs::crm_sender::hours().1.to_string(),
                "send_gap_min": env_n("AMUX_CRM_SEND_GAP_MIN", 9) } },
    }))
}

async fn list_route(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<Value> { Ok(pool(&*store.read()?)?) }).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(anyhow::anyhow!(e)),
    }
}

fn status_of(v: Option<&Value>) -> Result<Option<String>, String> {
    match v.and_then(Value::as_str).map(|s| s.trim().to_lowercase()) {
        None => Ok(None),
        Some(s) if STATUSES.contains(&s.as_str()) => Ok(Some(s)),
        Some(_) => Err(format!("status is one of {}", STATUSES.join(", "))),
    }
}

/// What adding a mailbox did, or the status and body that refused it.
type AddOutcome = Result<(), (StatusCode, Value)>;

async fn create_route(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let addr = body.get("address").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
    if !addr.contains('@') || !domain_of(&addr).contains('.') {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "address is an email address" }));
    }
    let status = match status_of(body.get("status")) {
        Ok(s) => s.unwrap_or_else(|| "warming".into()),
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    let force = body.get("force").and_then(Value::as_bool) == Some(true);
    let cap = body.get("daily_cap").and_then(Value::as_i64).unwrap_or_else(cap_default).max(0);
    let days = body.get("warmup_days").and_then(Value::as_i64).unwrap_or_else(warmup_default).max(0);
    let started = body.get("warmup_started").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| today().format("%Y-%m-%d").to_string());
    let notes = body.get("notes").and_then(Value::as_str).unwrap_or("").to_string();
    let out: Arc<Mutex<Option<AddOutcome>>> = Arc::new(Mutex::new(None));
    let w = out.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let d = domain_of(&addr);
            let live: i64 = conn.query_row("SELECT COUNT(*) FROM crm_mailboxes WHERE domain=?1 AND status<>'paused' AND address<>?2", [&d, &addr], |r| r.get(0)).unwrap_or(0);
            if status != "paused" && live >= per_domain() && !force {
                *w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Err((StatusCode::CONFLICT, json!({
                    "error": format!("{d} already has {live} live inboxes; the rule is {} per domain (send force: true to add anyway)", per_domain()),
                    "code": "crm_mailbox_domain_full" }))));
                return Ok(WriteOutcome { applied: false, events: vec![] });
            }
            conn.execute(
                "INSERT INTO crm_mailboxes (address,domain,status,daily_cap,warmup_started,warmup_days,notes,created,updated) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8) \
                 ON CONFLICT(address) DO UPDATE SET status=excluded.status, daily_cap=excluded.daily_cap, warmup_started=excluded.warmup_started, \
                 warmup_days=excluded.warmup_days, notes=excluded.notes, updated=excluded.updated",
                rusqlite::params![&addr, d, status, cap, started, days, notes, now_secs()],
            )?;
            *w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Ok(()));
            Ok(WriteOutcome { applied: true, events: vec![ev("crm_mailbox", &addr, MutationKind::Created)] })
        })
        .await;
    if let Err(e) = res {
        return internal(e);
    }
    let taken = out.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
    match taken {
        Some(Err((st, b))) => {
            tracing::info!(target: "amux::crm", measured = true, n_considered = 1, verdict = "crm_mailbox_domain_full", "a mailbox was refused: its domain is full");
            err(st, b)
        }
        _ => (StatusCode::CREATED, Json(json!({ "ok": true }))).into_response(),
    }
}

async fn patch_route(State(state): State<AppState>, Path(addr): Path<String>, Json(body): Json<Value>) -> Response {
    let addr = addr.trim().to_lowercase();
    let mut sets: Vec<(&str, SqlValue)> = Vec::new();
    match status_of(body.get("status")) {
        Ok(Some(s)) => {
            if s != "paused" {
                sets.push(("paused_reason", SqlValue::Text(String::new())));
            }
            sets.push(("status", SqlValue::Text(s)));
        }
        Ok(None) => {}
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    }
    for (k, col) in [("daily_cap", "daily_cap"), ("warmup_days", "warmup_days")] {
        if let Some(n) = body.get(k).and_then(Value::as_i64) {
            sets.push((col, SqlValue::Integer(n.max(0))));
        }
    }
    for k in ["warmup_started", "notes"] {
        if let Some(v) = body.get(k).and_then(Value::as_str) {
            sets.push((k, SqlValue::Text(v.trim().to_string())));
        }
    }
    if sets.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "nothing to change" }));
    }
    let res = state
        .store
        .write_async(move |conn| {
            let mut params: Vec<SqlValue> = sets.iter().map(|(_, v)| v.clone()).collect();
            let cols: Vec<String> = sets.iter().enumerate().map(|(i, (f, _))| format!("{f}=?{}", i + 1)).collect();
            params.push(SqlValue::Integer(now_secs()));
            params.push(SqlValue::Text(addr.clone()));
            let n = conn.execute(
                &format!("UPDATE crm_mailboxes SET {}, updated=?{} WHERE address=?{}", cols.join(", "), params.len() - 1, params.len()),
                rusqlite::params_from_iter(params.iter()),
            )?;
            Ok(WriteOutcome { applied: n > 0, events: if n > 0 { vec![ev("crm_mailbox", &addr, MutationKind::Updated)] } else { vec![] } })
        })
        .await;
    match res {
        Ok(o) if o.applied => Json(json!({ "ok": true })).into_response(),
        Ok(_) => err(StatusCode::NOT_FOUND, json!({ "error": "not in the pool" })),
        Err(e) => internal(e),
    }
}

async fn delete_route(State(state): State<AppState>, Path(addr): Path<String>) -> Response {
    let addr = addr.trim().to_lowercase();
    let res = state
        .store
        .write_async(move |conn| {
            let n = conn.execute("DELETE FROM crm_mailboxes WHERE address=?1", [&addr])?;
            Ok(WriteOutcome { applied: n > 0, events: if n > 0 { vec![ev("crm_mailbox", &addr, MutationKind::Deleted)] } else { vec![] } })
        })
        .await;
    match res {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::standing_approvals::tests::{call, test_state};

    const OWNER: [(&str, &str); 1] = [("authorization", "Bearer owner-secret")];
    const W: [(&str, &str); 1] = [("x-amux-session", "amux-gtm")];

    #[tokio::test]
    async fn the_pool_sends_from_the_freshest_ready_inbox_keeps_caps_and_warm_ups_and_says_when_none_can() {
        let mut state = test_state();
        state.auth_token = Some("owner-secret".into());
        let app = crate::api::crm::routes().merge(crate::api::crm_accounts::routes()).merge(crate::api::crm_agent::routes())
            .merge(crate::api::crm_outbound::routes()).merge(crate::api::crm_close::routes()).merge(crate::api::crm_campaigns::routes())
            .merge(routes()).with_state(state.clone());
        let (_, c) = call(&app, "POST", "/campaigns", &OWNER, Some(json!({"name": "Cold", "mailboxes": ["a@try.io", "b@try.io", "c@try.io"]}))).await;
        // Addresses are one case everywhere (land review F2): registered mixed-case, changed lower-case.
        state.store.write_async(|conn| { ensure(conn, &["D@Other.IO".into()])?; Ok(WriteOutcome { applied: true, events: vec![] }) }).await.unwrap();
        let (st, _) = call(&app, "PATCH", "/mailboxes/d@other.io", &OWNER, Some(json!({"status": "reserve"}))).await;
        assert_eq!(st, StatusCode::OK);
        let cid = c["id"].as_str().unwrap().to_string();
        let (_, p) = call(&app, "GET", "/mailboxes", &OWNER, None).await;
        let by = |p: &Value, a: &str| p["mailboxes"].as_array().unwrap().iter().find(|m| m["address"] == a).cloned().unwrap();
        assert_eq!((by(&p, "a@try.io")["status"].as_str(), by(&p, "c@try.io")["status"].as_str()), (Some("warming"), Some("paused")),
            "named in a campaign they join warming; a third on one domain is held");
        assert_eq!(p["health"]["ready"], 0, "nothing sends while warming");
        let (st, r) = call(&app, "POST", "/mailboxes", &OWNER, Some(json!({"address": "c@try.io", "status": "active"}))).await;
        assert_eq!((st, r["code"].as_str()), (StatusCode::CONFLICT, Some("crm_mailbox_domain_full")));

        // A cold email while the pool is warming is refused, with why.
        let (_, s) = call(&app, "POST", "/sequences", &W, Some(json!({"name": "s", "campaign_id": cid, "steps": [{"channel": "email", "action": "email", "template": "Subject: Hello\n\nhi"}]}))).await;
        let seq = s["id"].as_str().unwrap().to_string();
        let mut people = vec![];
        for n in 0..4 {
            let (_, x) = call(&app, "POST", "/contacts", &OWNER, Some(json!({"name": format!("P{n} Q"), "email": format!("p{n}@corp.io")}))).await;
            people.push(x["id"].as_str().unwrap().to_string());
        }
        let (_, e) = call(&app, "POST", &format!("/sequences/{seq}/enroll"), &W, Some(json!({"contact_ids": people}))).await;
        let touches: Vec<String> = e["enrolled"].as_array().unwrap().iter().map(|x| x["touch_id"].as_str().unwrap().to_string()).collect();
        let (st, r) = call(&app, "POST", &format!("/touches/{}/approve", touches[0]), &OWNER, None).await;
        assert_eq!((st, r["code"].as_str()), (StatusCode::CONFLICT, Some("crm_mailbox_cap")));
        assert!(r["error"].as_str().unwrap().contains("warming up until"), "{r}");

        // Warmed: a has a cap of 1, b of 2. Sends spread to the inbox with fewest today, and stop at the caps.
        call(&app, "PATCH", "/mailboxes/a@try.io", &OWNER, Some(json!({"status": "active", "daily_cap": 1}))).await;
        call(&app, "PATCH", "/mailboxes/b@try.io", &OWNER, Some(json!({"status": "active", "daily_cap": 2}))).await;
        // The CRM sends a pool email as approved, so a first email needs its subject.
        call(&app, "PATCH", &format!("/touches/{}", touches[0]), &W, Some(json!({"body": "hi, no subject line"}))).await;
        let (st, r) = call(&app, "POST", &format!("/touches/{}/approve", touches[0]), &OWNER, None).await;
        assert_eq!((st, r["code"].as_str()), (StatusCode::CONFLICT, Some("crm_email_no_subject")), "{r}");
        call(&app, "PATCH", &format!("/touches/{}", touches[0]), &W, Some(json!({"body": "Subject: Hello\n\nhi"}))).await;
        let mut used = vec![];
        for t in &touches[..3] {
            let (st, r) = call(&app, "POST", &format!("/touches/{t}/approve"), &OWNER, None).await;
            assert_eq!(st, StatusCode::OK, "{r}");
            used.push(r["touch"]["mailbox"].as_str().unwrap().to_string());
        }
        used.sort();
        assert_eq!(used, vec!["a@try.io", "b@try.io", "b@try.io"], "never past a cap");
        let (st, r) = call(&app, "POST", &format!("/touches/{}/approve", touches[3]), &OWNER, None).await;
        assert_eq!((st, r["code"].as_str()), (StatusCode::CONFLICT, Some("crm_mailbox_cap")), "every inbox is at today's cap");
        let (_, p) = call(&app, "GET", "/mailboxes", &OWNER, None).await;
        assert_eq!((by(&p, "b@try.io")["sent_today"].as_i64(), p["health"]["sends_left_today"].as_i64(), p["measured"].as_bool()), (Some(2), Some(0), Some(true)));
        assert_eq!((p["health"]["reserve_target"].as_i64(), p["health"]["reserve_short"].as_i64()), (Some(1), Some(0)), "20% of 4 inboxes is 1 in reserve, and d@other.io is");
    }

    #[test]
    fn a_domain_is_ready_only_with_mx_spf_dkim_dmarc_and_no_listing() {
        let good = dns_check("good.test");
        assert_eq!((good["ok"].as_bool(), good["measured"].as_bool(), good["dbl_listed"].as_bool()), (Some(true), Some(true), Some(false)), "{good}");
        let bare = dns_check("bare.test");
        let p = bare["problems"].to_string();
        assert!(bare["ok"] == false && p.contains("SPF") && p.contains("DKIM") && p.contains("DMARC"), "{bare}");
        assert!(dns_refusal(&bare.to_string()).unwrap().starts_with("bare.test: no SPF"));
        let listed = dns_check("listed.test");
        assert!(listed["ok"] == false && listed["problems"].to_string().contains("Spamhaus"), "{listed}");
        assert_eq!(dns_refusal(&good.to_string()), None);
        assert_eq!(dns_refusal(""), None, "never checked is not a refusal; the sender checks before sending");
    }

    #[test]
    fn a_follow_up_without_a_subject_needs_an_earlier_email_from_the_same_inbox() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate::apply_all(&mut conn).unwrap();
        conn.execute("INSERT INTO crm_touches (id,contact_id,channel,action,status,mailbox,sent_thread,sent_subject,done_at,created,updated) VALUES ('t1','c','email','email','done','a@try.io','th','Hi',5,1,1)", []).unwrap();
        for (id, mb) in [("t2", "a@try.io"), ("t3", "b@try.io")] {
            conn.execute("INSERT INTO crm_touches (id,contact_id,channel,action,body,status,mailbox,created,updated) VALUES (?1,'c','email','email','bumping this','drafted',?2,1,1)", [id, mb]).unwrap();
        }
        assert_eq!(subject_refusal(&conn, "t2"), None, "threads under t1 in the same inbox");
        assert!(subject_refusal(&conn, "t3").is_some(), "another inbox has no thread to answer in (land review F1)");
    }

    #[test]
    fn a_txt_answer_parses_from_the_wire() {
        let q = dns_query(7, "x.io", 16);
        let mut r = q.clone();
        r[2] = 0x81; r[3] = 0x80; r[7] = 1; // a response, one answer
        let txt = b"v=spf1 -all";
        r.extend_from_slice(&[0xc0, 12, 0, 16, 0, 1, 0, 0, 0, 60, 0, (txt.len() + 1) as u8, txt.len() as u8]);
        r.extend_from_slice(txt);
        let (rc, rds) = dns_parse(&r, 7, 16).unwrap();
        assert_eq!((rc, rds.len(), &rds[0][1..]), (0, 1, &txt[..]));
        assert!(dns_parse(&r, 8, 16).is_none(), "not our id");
    }

    #[test]
    fn a_bounce_names_the_address_and_a_bouncing_inbox_pauses() {
        let d = json!({"account": "a@try.io", "from_address": "mailer-daemon@googlemail.com", "subject": "Delivery Status Notification (Failure)",
            "snippet": "Address not found Your message wasn&#39;t delivered to Bob@Corp.io because the address couldn&#39;t be found."});
        assert_eq!(bounce_of(&d).as_deref(), Some("bob@corp.io"));
        assert_eq!(bounce_of(&json!({"from_address": "bob@corp.io", "subject": "Delivery Status Notification"})), None, "a person is not a bounce");
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate::apply_all(&mut conn).unwrap();
        conn.execute("INSERT INTO crm_mailboxes (address,domain,status,created,updated) VALUES ('a@try.io','try.io','active',1,1)", []).unwrap();
        let now = now_secs();
        for i in 0..20 {
            conn.execute("INSERT INTO crm_contacts (id,name,email,created,updated) VALUES (?1,?1,?2,1,1)", rusqlite::params![format!("c{i}"), format!("p{i}@corp.io")]).unwrap();
            conn.execute("INSERT INTO crm_touches (id,contact_id,channel,action,status,done_at,mailbox,created,updated) VALUES (?1,?2,'email','email','done',?3,'a@try.io',1,1)",
                rusqlite::params![format!("t{i}"), format!("c{i}"), now - 60]).unwrap();
        }
        assert!(record_bounce(&conn, "a@try.io", "p0@corp.io").unwrap().is_none(), "one bounce marks the person and pauses nothing");
        let bounced: Option<i64> = conn.query_row("SELECT bounced_at FROM crm_contacts WHERE id='c0'", [], |r| r.get(0)).unwrap();
        assert!(bounced.is_some());
        let p = record_bounce(&conn, "a@try.io", "p1@corp.io").unwrap().expect("2 of 20 is 10%, over 3%: paused");
        assert_eq!((p["bounced_7d"].as_i64(), p["sent_7d"].as_i64()), (Some(2), Some(20)));
        let (st, why): (String, String) = conn.query_row("SELECT status, paused_reason FROM crm_mailboxes WHERE address='a@try.io'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert!(st == "paused" && why.contains("2 of 20"), "{st} {why}");
        assert!(record_bounce(&conn, "a@try.io", "p2@corp.io").unwrap().is_none(), "already paused: no second event");
    }

    #[tokio::test]
    async fn a_linkedin_connect_reads_accepted_when_the_worker_records_it_or_they_write_and_the_funnel_counts_it() {
        let mut state = test_state();
        state.auth_token = Some("owner-secret".into());
        let app = crate::api::crm::routes().merge(crate::api::crm_agent::routes()).merge(crate::api::crm_outbound::routes())
            .merge(crate::api::crm_close::routes()).with_state(state.clone());
        let (_, s) = call(&app, "POST", "/sequences", &W, Some(json!({"name": "li", "steps": [{"channel": "linkedin", "action": "connect", "template": "hi"}, {"day": 2, "channel": "linkedin", "action": "dm"}]}))).await;
        let seq = s["id"].as_str().unwrap().to_string();
        let mut ids = vec![];
        for n in ["Ann Li", "Bea Li"] {
            let (_, p) = call(&app, "POST", "/contacts", &OWNER, Some(json!({"name": n, "linkedin": format!("linkedin.com/in/{}", n.replace(' ', ""))}))).await;
            ids.push(p["id"].as_str().unwrap().to_string());
        }
        let (_, e) = call(&app, "POST", &format!("/sequences/{seq}/enroll"), &W, Some(json!({"contact_ids": ids}))).await;
        for x in e["enrolled"].as_array().unwrap() {
            let t = x["touch_id"].as_str().unwrap();
            call(&app, "POST", &format!("/touches/{t}/approve"), &OWNER, None).await;
            call(&app, "POST", &format!("/touches/{t}/done"), &W, Some(json!({"result": "sent"}))).await;
        }
        let t0 = e["enrolled"][0]["touch_id"].as_str().unwrap().to_string();
        let (st, _) = call(&app, "POST", &format!("/touches/{t0}/accepted"), &W, None).await;
        assert_eq!(st, StatusCode::OK);
        let (_, again) = call(&app, "POST", &format!("/touches/{t0}/accepted"), &W, None).await;
        assert_eq!(again["already"], true, "a repeat is not news (land review F1)");
        let (_, f) = call(&app, "GET", "/funnel", &OWNER, None).await;
        let g = &f["groups"][0];
        assert_eq!((g["connects"].as_i64(), g["accepted"].as_i64(), g["conversion"]["accepted_per_connect"].as_f64()), (Some(2), Some(1), Some(0.5)));
        // Bea writes on LinkedIn: connected, so her connect reads accepted.
        let bea = ids[1].clone();
        state.store.write_async(move |c| { crate::api::crm_agent::mark_accepted(c, &bea)?; Ok(WriteOutcome { applied: true, events: vec![] }) }).await.unwrap();
        let (_, f) = call(&app, "GET", "/funnel", &OWNER, None).await;
        assert_eq!(f["groups"][0]["accepted"].as_i64(), Some(2));
        let n: i64 = state.store.read().unwrap().query_row("SELECT COUNT(*) FROM session_events WHERE type='crm.touch.accepted'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "the worker's record is one event, however often it is sent");
        let (_, r) = call(&app, "POST", &format!("/touches/{}/accepted", "TCH-nope"), &W, None).await;
        assert_eq!(r["code"], "not_found");
    }
}
