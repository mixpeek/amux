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
//! Warm-up here is the clock: a warming inbox carries no cold email until its
//! days are done. The warm-up traffic itself (mail exchanged with other
//! inboxes to build reputation) needs a warm-up network, which is the owner's
//! decision.

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
fn start_of_today() -> i64 {
    today().and_hms_opt(0, 0, 0).and_then(|t| t.and_local_timezone(chrono::Local).single()).map(|t| t.timestamp()).unwrap_or(0)
}

fn domain_of(addr: &str) -> String {
    addr.rsplit('@').next().unwrap_or("").to_string()
}

/// Whether the address is a connected Google account (a token file the Gmail
/// connector wrote). Tests never read the real home: an address starting
/// "unconnected" reads as not connected, any other as connected.
#[cfg(not(test))]
fn connected(addr: &str) -> bool {
    crate::config::amux_home().join("gmail-tokens").join(format!("{addr}.json")).exists()
}
#[cfg(test)]
fn connected(addr: &str) -> bool {
    !addr.starts_with("unconnected")
}

/// When a warming inbox's warm-up ends, as YYYY-MM-DD ("" when it has no start).
fn warmup_ends(started: &str, days: i64) -> String {
    chrono::NaiveDate::parse_from_str(started, "%Y-%m-%d").ok()
        .map(|d| (d + chrono::Duration::days(days.max(0))).format("%Y-%m-%d").to_string())
        .unwrap_or_default()
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
fn not_ready(conn: &rusqlite::Connection, row: &(String, String, i64, String, i64)) -> Option<String> {
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
    let sent = sends_since(conn, addr, start_of_today());
    if sent >= *cap {
        return Some(format!("{addr} sent {sent} of {cap} today"));
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
        if why.is_none() {
            ready += 1;
            capacity += cap;
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
            "rules": { "daily_cap": cap_default(), "warmup_days": warmup_default(), "per_domain": per_domain(), "reserve_pct": reserve_pct() } },
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
        Ok(Some(s)) => sets.push(("status", SqlValue::Text(s))),
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
        let (_, s) = call(&app, "POST", "/sequences", &W, Some(json!({"name": "s", "campaign_id": cid, "steps": [{"channel": "email", "action": "email", "template": "hi"}]}))).await;
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
