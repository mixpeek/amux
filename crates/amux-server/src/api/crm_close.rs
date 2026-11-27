//! Closing the loop in the CRM (docs/design/crm-outbound-acceptance.md,
//! sections 10 to 16; owner 2026-11-27: "it barely covers closing, and
//! closing is the point ... all 34 rows could read Pass and a deal would still
//! never move past lead").
//!
//! Stages. Every stage change goes through [`move_stage`]: one row in
//! `crm_deal_stages` (from, to, when, who, why) and a `crm.deal.stage` event,
//! so the funnel counts a demo that later moved on or was lost. The rules,
//! checked by [`check_stage`] wherever a stage is set:
//!   - only the owner moves a deal to or from `proposal` or `won` (the human
//!     boundary: the agents drive to booked and nurtured; the owner prices and
//!     signs), refused as `crm_deal_owner_stage`;
//!   - `won` needs `amount` (the signed value) and `close_date` (`crm_deal_won_incomplete`);
//!     `mrr_usd` is optional, for a deal that really is a subscription;
//!   - `lost` needs a `lost_reason` from [`LOST_REASONS`] (`crm_deal_lost_reason`).
//!
//!   GET  /api/crm/deals/{id}/stages   the deal's stage history, oldest first
//!
//! Replies. A worker reads each reply and records its intent:
//!   POST /api/crm/replies/{interaction_id}/intent
//!        {intent, why, follow_up_date?, do_not_contact?, lost_reason?, referral?: {name, email?, linkedin?, role?}}
//! with intent one of [`INTENTS`]. Reading it is the worker's judgement; the
//! server stores it on the interaction, makes the move it implies (section 11
//! of the plan) and emits `crm.reply.classified`.
//!
//! Measurement. GET /api/crm/funnel serves the `crm.funnel.v1` contract that
//! mixpeek's gtm-engine report reads: enrolled, contacted, replied, booked,
//! demoed, won and lost per sequence or cohort, each counted once per person,
//! with conversion per step and MRR. The names and their meanings stay fixed;
//! a change ships as v2 beside v1. `crm.funnel.changed` names what moved, so a
//! consumer re-reads instead of polling.
//!
//! Address check. Before the first cold email, [`check_email`]: the syntax, a
//! role address, a known disposable domain, and the domain's MX records over
//! the system resolver. Free; a paid mailbox-level check is the owner's
//! decision. POST /api/crm/contacts/{id}/check-email runs it on demand.

use super::calendar::query_rows_json;
use super::crm_accounts::{ev, now_secs, source_of, STAGES};
use super::{internal, AppState};
use crate::db::board_store::next_issue_id;
use crate::db::WriteOutcome;
use amux_core::revision::MutationKind;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/replies/{id}/intent", post(intent_route))
        .route("/funnel", get(funnel_route))
        .route("/deals/{id}/stages", get(stages_route))
        .route("/contacts/{id}/check-email", post(check_email_route))
}

/// Stages only the owner moves a deal to or from.
pub const OWNER_STAGES: &[&str] = &["proposal", "won"];
pub const LOST_REASONS: &[&str] = &["no_budget", "no_need", "timing", "competitor", "no_response", "other"];
pub const INTENTS: &[&str] = &["interested", "book_later", "objection", "not_interested", "referral"];
/// Actions that reach nobody's inbox: not counted as "contacted" by the funnel,
/// whatever AMUX_CRM_AUTO_ACTIONS says, so the contract's meaning stays fixed.
const SILENT_ACTIONS: &[&str] = &["note", "follow", "like", "watch"];
pub const FUNNEL_CONTRACT: &str = "crm.funnel.v1";

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn slot<T>(s: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    s.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn rank(stage: &str) -> usize {
    STAGES.iter().position(|s| *s == stage).unwrap_or(0)
}

/// Whether a deal may move from `from` to `to` with these fields, and who is asking.
/// `deal` is the deal as it will be after the write (its current row with the
/// request's fields laid over it). Err is (verdict, message).
pub(crate) fn check_stage(from: &str, to: &str, deal: &Value, owner: bool) -> Result<(), (&'static str, String)> {
    if from == to {
        return Ok(());
    }
    if !owner && (OWNER_STAGES.contains(&to) || OWNER_STAGES.contains(&from)) {
        return Err(("crm_deal_owner_stage", format!(
            "only the owner moves a deal to or from {}: the agents drive to a booked meeting and keep it warm; the owner prices and signs", OWNER_STAGES.join(" or "))));
    }
    let has = |k: &str| match deal.get(k) {
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x > 0.0),
        Some(Value::String(s)) => !s.trim().is_empty(),
        _ => false,
    };
    if to == "won" {
        // The signed value and the day. A contract is not a subscription, so
        // mrr_usd is optional: set it on a deal that really is monthly.
        let missing: Vec<&str> = ["amount", "close_date"].into_iter().filter(|k| !has(k)).collect();
        if !missing.is_empty() {
            return Err(("crm_deal_won_incomplete", format!("a won deal needs {}", missing.join(", "))));
        }
    }
    if to == "lost" {
        let r = deal.get("lost_reason").and_then(Value::as_str).unwrap_or("");
        if !LOST_REASONS.contains(&r) {
            return Err(("crm_deal_lost_reason", format!("a lost deal needs lost_reason, one of {}", LOST_REASONS.join(", "))));
        }
    }
    Ok(())
}

/// Record a deal's first stage (a new deal), so its history starts where it does.
pub(crate) fn first_stage(conn: &rusqlite::Connection, deal: &str, stage: &str, by: &str, why: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO crm_deal_stages (deal_id, from_stage, to_stage, at, by, why) VALUES (?1,'',?2,?3,?4,?5)",
        rusqlite::params![deal, stage, now_secs(), by, why],
    )?;
    Ok(())
}

/// Move a deal to `to`: the stage, `next_step` when given, one history row.
/// Returns the `crm.deal.stage` payload, or None when the deal is missing or
/// already there (then only `next_step` is written). The rules are the
/// caller's ([`check_stage`]); this writes what was allowed.
pub(crate) fn move_stage(conn: &rusqlite::Connection, deal: &str, to: &str, by: &str, why: &str, next_step: Option<&str>) -> rusqlite::Result<Option<Value>> {
    let cur: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT d.stage, d.title, d.account_id, COALESCE(a.name,'') FROM crm_deals d LEFT JOIN crm_accounts a ON a.id=d.account_id WHERE d.id=?1 AND d.deleted IS NULL",
            [deal],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok();
    let Some((from, title, account, account_name)) = cur else { return Ok(None) };
    let now = now_secs();
    if let Some(ns) = next_step {
        conn.execute("UPDATE crm_deals SET next_step=?2, updated=?3 WHERE id=?1", rusqlite::params![deal, ns, now])?;
    }
    if from == to {
        return Ok(None);
    }
    conn.execute("UPDATE crm_deals SET stage=?2, updated=?3 WHERE id=?1", rusqlite::params![deal, to, now])?;
    conn.execute(
        "INSERT INTO crm_deal_stages (deal_id, from_stage, to_stage, at, by, why) VALUES (?1,?2,?3,?4,?5,?6)",
        rusqlite::params![deal, &from, to, now, by, why],
    )?;
    let next: String = conn.query_row("SELECT next_step FROM crm_deals WHERE id=?1", [deal], |r| r.get(0)).unwrap_or_default();
    tracing::info!(target: "amux::crm", deal = %deal, from = %from, to = %to, by = %by, measured = true, n_considered = 1,
        verdict = "crm_deal_stage_moved", "a deal moved stage: {why}");
    Ok(Some(json!({ "deal_id": deal, "title": title, "account_id": account, "account_name": account_name,
        "from": from, "to": to, "by": by, "why": why, "next_step": next })))
}

/// The person's open deal (theirs, or their company's), oldest first.
pub(crate) fn open_deal_for(conn: &rusqlite::Connection, contact: &str) -> Option<(String, String)> {
    let account: String = conn.query_row("SELECT account_id FROM crm_contacts WHERE id=?1", [contact], |r| r.get(0)).unwrap_or_default();
    conn.query_row(
        "SELECT id, stage FROM crm_deals WHERE deleted IS NULL AND stage NOT IN ('won','lost') \
         AND (contact_ids LIKE '%\"' || ?1 || '\"%' OR (?2<>'' AND account_id=?2)) ORDER BY created LIMIT 1",
        [contact, account.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .ok()
}

/// A new deal for a person, at `stage`, with its first history row.
pub(crate) fn open_deal(conn: &rusqlite::Connection, contact: &str, stage: &str, next_step: &str, source: &str, notes: &str) -> rusqlite::Result<String> {
    let (name, account, company): (String, String, String) = conn
        .query_row(
            "SELECT c.name, c.account_id, COALESCE(NULLIF(a.name,''), c.company) FROM crm_contacts c LEFT JOIN crm_accounts a ON a.id=c.account_id WHERE c.id=?1",
            [contact],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap_or_default();
    let id = next_issue_id(conn, "DEAL")?;
    let title = if company.trim().is_empty() { name } else { company };
    let now = now_secs();
    conn.execute(
        "INSERT INTO crm_deals (id,title,account_id,contact_ids,stage,next_step,notes,source,external,created,updated) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'{}',?9,?9)",
        rusqlite::params![&id, title, account, json!([contact]).to_string(), stage, next_step, notes, source, now],
    )?;
    first_stage(conn, &id, stage, source, notes)?;
    Ok(id)
}

/// A booked meeting (row 35): the person's deal moves to `demo` (opened there
/// when they had none) with "prep the demo" as its next step, and a prep
/// touch is planned for whoever worked them. Returns (stage event, prep touch).
pub(crate) fn on_booking_deal(conn: &rusqlite::Connection, contact: &str) -> rusqlite::Result<(Option<Value>, Option<String>)> {
    let next = "prep the demo: read the brief, open the app, write the questions to ask";
    let stage = match open_deal_for(conn, contact) {
        Some((deal, stage)) if rank(&stage) < rank("demo") => move_stage(conn, &deal, "demo", "booking", "they booked a meeting", Some(next))?,
        Some(_) => None,
        None => {
            let deal = open_deal(conn, contact, "demo", next, "booking", "they booked a meeting")?;
            Some(json!({ "deal_id": deal, "from": "", "to": "demo", "by": "booking", "why": "they booked a meeting", "next_step": next }))
        }
    };
    // Whoever last worked this person prepares the demo.
    let assignee: String = conn
        .query_row("SELECT assignee FROM crm_touches WHERE contact_id=?1 AND assignee<>'' AND deleted IS NULL ORDER BY updated DESC LIMIT 1", [contact], |r| r.get(0))
        .unwrap_or_default();
    let open: bool = conn
        .query_row("SELECT 1 FROM crm_touches WHERE contact_id=?1 AND source='booking' AND status IN ('planned','drafted','approved') AND deleted IS NULL", [contact], |_| Ok(true))
        .unwrap_or(false);
    let prep = if open { None } else {
        let id = next_issue_id(conn, "TCH")?;
        let now = now_secs();
        conn.execute(
            "INSERT INTO crm_touches (id,contact_id,channel,action,body,due_date,assignee,status,sequence,step,source,created,updated) \
             VALUES (?1,?2,'other','note','',?3,?4,'planned','',0,'booking',?5,?5)",
            rusqlite::params![&id, contact, chrono::Local::now().format("%Y-%m-%d").to_string(), assignee, now],
        )?;
        Some(id)
    };
    Ok((stage, prep))
}

// ---- stage history ---------------------------------------------------------------------

async fn stages_route(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        Ok(query_rows_json(&*store.read()?, "SELECT from_stage, to_stage, at, by, why FROM crm_deal_stages WHERE deal_id=?1 ORDER BY at, id", &[&id as &dyn rusqlite::ToSql])?)
    })
    .await
    {
        Ok(Ok(rows)) => Json(json!({ "stages": rows })).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(anyhow::anyhow!(e)),
    }
}

// ---- reply intent ----------------------------------------------------------------------

/// What an intent write did, or the status and body that refused it.
type IntentWrite = Result<Value, (StatusCode, Value)>;

async fn intent_route(State(state): State<AppState>, headers: HeaderMap, Path(ix): Path<String>, Json(body): Json<Value>) -> Response {
    let s = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let intent = s("intent");
    if !INTENTS.contains(&intent.as_str()) {
        return err(StatusCode::BAD_REQUEST, json!({ "error": format!("intent is one of {}", INTENTS.join(", ")), "code": "crm_intent_unknown" }));
    }
    let referral = body.get("referral").cloned().unwrap_or(Value::Null);
    if intent == "referral" && referral.get("name").and_then(Value::as_str).unwrap_or("").trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "a referral names the person: referral: {name, email?, linkedin?, role?}", "code": "crm_intent_referral_unnamed" }));
    }
    let by = source_of(&json!({}), &headers);
    let owner = super::standing_approvals::is_owner_request(&headers);
    let (why, follow, lost_reason) = (s("why"), s("follow_up_date"), s("lost_reason"));
    let dnc = body.get("do_not_contact").and_then(Value::as_bool) == Some(true);
    let booking_url = crate::config::env_string("AMUX_CRM_BOOKING_URL", "");
    let out: Arc<Mutex<Option<IntentWrite>>> = Arc::new(Mutex::new(None));
    let w = out.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let row: Option<(String, String)> = conn
                .query_row("SELECT contact_id, direction FROM crm_interactions WHERE id=?1", [&ix], |r| Ok((r.get(0)?, r.get(1)?)))
                .ok();
            let Some((contact, _direction)) = row else {
                *slot(&w) = Some(Err((StatusCode::NOT_FOUND, json!({ "error": "no interaction with that id", "code": "not_found" }))));
                return Ok(WriteOutcome { applied: false, events: vec![] });
            };
            conn.execute("UPDATE crm_interactions SET intent=?2, intent_why=?3, updated=?4 WHERE id=?1", rusqlite::params![&ix, &intent, &why, now_secs()])?;
            let name: String = conn.query_row("SELECT name FROM crm_contacts WHERE id=?1", [&contact], |r| r.get(0)).unwrap_or_default();
            let mut stage_ev: Option<Value> = None;
            let mut referral_id: Option<String> = None;
            let mut note = String::new();
            let forward = |conn: &rusqlite::Connection, next: &str| -> rusqlite::Result<Option<Value>> {
                match open_deal_for(conn, &contact) {
                    Some((deal, stage)) if rank(&stage) < rank("qualification") => move_stage(conn, &deal, "qualification", &by, &format!("their reply reads {intent}"), Some(next)),
                    Some((deal, _)) => {
                        conn.execute("UPDATE crm_deals SET next_step=?2, updated=?3 WHERE id=?1", rusqlite::params![&deal, next, now_secs()])?;
                        Ok(None)
                    }
                    None => {
                        let deal = open_deal(conn, &contact, "qualification", next, &by, &format!("their reply reads {intent}"))?;
                        Ok(Some(json!({ "deal_id": deal, "from": "", "to": "qualification", "by": by, "why": format!("their reply reads {intent}"), "next_step": next })))
                    }
                }
            };
            match intent.as_str() {
                "interested" => {
                    let next = if booking_url.is_empty() { "push for a booked meeting".to_string() } else { format!("push for a booked meeting: {booking_url}") };
                    stage_ev = forward(conn, &next)?;
                }
                "book_later" => {
                    let date = if follow.is_empty() { (chrono::Local::now().date_naive() + chrono::Duration::days(14)).format("%Y-%m-%d").to_string() } else { follow.clone() };
                    conn.execute("UPDATE crm_interactions SET follow_up_date=?2, follow_up_note=?3 WHERE id=?1",
                        rusqlite::params![&ix, &date, format!("they asked to talk later: {why}")])?;
                    stage_ev = forward(conn, &format!("follow up on {date}"))?;
                }
                "objection" => {
                    stage_ev = forward(conn, &format!("answer their objection: {why}"))?;
                }
                "not_interested" => {
                    let now = now_secs();
                    conn.execute("UPDATE crm_enrollments SET status='stopped', updated=?2 WHERE contact_id=?1 AND status='active'", rusqlite::params![&contact, now])?;
                    conn.execute("UPDATE crm_touches SET status='skipped', result='stopped: not interested', updated=?2 WHERE contact_id=?1 AND deleted IS NULL AND status IN ('planned','drafted','approved')",
                        rusqlite::params![&contact, now])?;
                    if dnc {
                        conn.execute("UPDATE crm_contacts SET status='do_not_contact', updated=?2 WHERE id=?1", rusqlite::params![&contact, now])?;
                    }
                    if let Some((deal, stage)) = open_deal_for(conn, &contact) {
                        let reason = if LOST_REASONS.contains(&lost_reason.as_str()) { lost_reason.clone() } else { "no_need".to_string() };
                        let deal_row = json!({ "lost_reason": reason });
                        match check_stage(&stage, "lost", &deal_row, owner) {
                            Ok(()) => {
                                conn.execute("UPDATE crm_deals SET lost_reason=?2 WHERE id=?1", rusqlite::params![&deal, &reason])?;
                                stage_ev = move_stage(conn, &deal, "lost", &by, &format!("not interested: {why}"), Some("none"))?;
                            }
                            Err((_, m)) => note = format!("the deal stays at {stage}: {m}"),
                        }
                    }
                }
                _ => {
                    // A referral: the person they named, at the same company, credited to them.
                    let r = |k: &str| referral.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
                    let (account, company): (String, String) = conn
                        .query_row("SELECT account_id, company FROM crm_contacts WHERE id=?1", [&contact], |x| Ok((x.get(0)?, x.get(1)?)))
                        .unwrap_or_default();
                    let id = next_issue_id(conn, "PPL")?;
                    let now = now_secs();
                    conn.execute(
                        "INSERT INTO crm_contacts (id,name,email,linkedin,role,company,account_id,source,notes,created,updated) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10)",
                        rusqlite::params![&id, r("name"), r("email").to_lowercase(), r("linkedin"), r("role"), company, account,
                            format!("referral:{contact}"), format!("referred by {name}: {why}"), now],
                    )?;
                    referral_id = Some(id);
                }
            }
            let mut events = vec![ev("crm_contact", &contact, MutationKind::Updated)];
            if stage_ev.is_some() {
                events.push(ev("crm_deal", "", MutationKind::Updated));
            }
            if let Some(id) = &referral_id {
                events.push(ev("crm_contact", id, MutationKind::Created));
            }
            *slot(&w) = Some(Ok(json!({
                "interaction_id": ix, "contact_id": contact, "name": name, "intent": intent, "why": why,
                "deal": stage_ev, "referral_contact_id": referral_id, "note": note,
            })));
            Ok(WriteOutcome { applied: true, events })
        })
        .await;
    if let Err(e) = res {
        return internal(e);
    }
    let taken = slot(&out).take();
    match taken {
        Some(Ok(v)) => {
            if let Some(d) = v.get("deal").filter(|d| !d.is_null()) {
                super::crm_agent::emit(&state, "crm.deal.stage", d.clone()).await;
            }
            super::crm_agent::emit(&state, "crm.reply.classified", v.clone()).await;
            emit_funnel_changed(&state, "reply_classified", v["contact_id"].as_str().unwrap_or("")).await;
            tracing::info!(target: "amux::crm", intent = %v["intent"].as_str().unwrap_or(""), measured = true, n_considered = 1,
                verdict = "crm_reply_classified", "a worker recorded what a reply means");
            Json(v).into_response()
        }
        Some(Err((st, b))) => err(st, b),
        None => internal(anyhow::anyhow!("intent wrote nothing")),
    }
}

// ---- the funnel ------------------------------------------------------------------------

/// `crm.funnel.changed`: what moved, and for whom. The numbers come from
/// GET /api/crm/funnel; the event tells a consumer to read them again.
pub(crate) async fn emit_funnel_changed(state: &AppState, reason: &str, contact: &str) {
    let store = state.store.clone();
    let c = contact.to_string();
    let seqs: Vec<String> = tokio::task::spawn_blocking(move || -> Vec<String> {
        let Ok(conn) = store.read() else { return vec![] };
        let Ok(mut q) = conn.prepare("SELECT sequence_id FROM crm_enrollments WHERE contact_id=?1") else { return vec![] };
        q.query_map([&c], |r| r.get::<_, String>(0)).map(|it| it.filter_map(Result::ok).collect()).unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    super::crm_agent::emit(state, "crm.funnel.changed", json!({ "contract": FUNNEL_CONTRACT, "reason": reason, "contact_id": contact, "sequences": seqs })).await;
}

#[derive(Default)]
struct Group {
    name: String,
    people: BTreeMap<String, [bool; 7]>,
    won_deals: HashSet<String>,
}

const STEPS: [&str; 7] = ["enrolled", "contacted", "replied", "booked", "demoed", "won", "lost"];

pub(crate) fn funnel(conn: &rusqlite::Connection, q: &HashMap<String, String>) -> rusqlite::Result<Value> {
    let by_cohort = q.get("group").map(String::as_str) == Some("cohort");
    let since_day = q.get("since").cloned().unwrap_or_default();
    let since_ts = chrono::NaiveDate::parse_from_str(&since_day, "%Y-%m-%d").ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0)).map(|t| t.and_utc().timestamp()).unwrap_or(0);
    let seq_f = q.get("sequence").cloned().unwrap_or_default();
    let cohort_f = q.get("cohort").cloned().unwrap_or_default();
    let rows: Vec<(String, String, String, String, i64, String)> = conn
        .prepare(
            "SELECT e.sequence_id, COALESCE(s.name,''), e.contact_id, e.cohort, e.created, e.status FROM crm_enrollments e \
             LEFT JOIN crm_sequences s ON s.id=e.sequence_id WHERE e.created>=?1 AND (?2='' OR e.sequence_id=?2) AND (?3='' OR e.cohort=?3)",
        )?
        .query_map(rusqlite::params![since_ts, &seq_f, &cohort_f], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let silent = SILENT_ACTIONS.iter().map(|a| format!("'{a}'")).collect::<Vec<_>>().join(",");
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for (seq, seq_name, contact, cohort, at, status) in &rows {
        let one = |sql: &str| -> bool { conn.query_row(sql, rusqlite::params![contact, at], |_| Ok(true)).unwrap_or(false) };
        let contacted = one(&format!("SELECT 1 FROM crm_touches WHERE contact_id=?1 AND deleted IS NULL AND status='done' AND COALESCE(done_at,updated)>=?2 AND action NOT IN ({silent}) LIMIT 1"));
        let replied = status == "replied" || one("SELECT 1 FROM crm_interactions WHERE contact_id=?1 AND direction='in' AND created>=?2 LIMIT 1");
        let booked = status == "booked" || one("SELECT 1 FROM crm_interactions WHERE contact_id=?1 AND type IN ('meeting','booking') AND created>=?2 LIMIT 1");
        let account: String = conn.query_row("SELECT account_id FROM crm_contacts WHERE id=?1", [contact], |r| r.get(0)).unwrap_or_default();
        let deals: Vec<(String, String)> = conn
            .prepare("SELECT id, stage FROM crm_deals WHERE deleted IS NULL AND (contact_ids LIKE '%\"' || ?1 || '\"%' OR (?2<>'' AND account_id=?2))")?
            .query_map([contact.as_str(), account.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let reached = |deal: &str, stages: &str| -> bool {
            conn.query_row(&format!("SELECT 1 FROM crm_deal_stages WHERE deal_id=?1 AND at>=?2 AND to_stage IN ({stages}) LIMIT 1"), rusqlite::params![deal, at], |_| Ok(true))
                .unwrap_or(false)
        };
        let demoed = deals.iter().any(|(d, _)| reached(d, "'demo','trial','proposal','won'"));
        let won: Vec<&String> = deals.iter().filter(|(d, _)| reached(d, "'won'")).map(|(d, _)| d).collect();
        let lost = deals.iter().any(|(d, st)| st == "lost" && reached(d, "'lost'"));
        let key = if by_cohort { if cohort.is_empty() { "(none)".to_string() } else { cohort.clone() } } else { seq.clone() };
        let g = groups.entry(key).or_default();
        if !by_cohort {
            g.name = seq_name.clone();
        }
        let flags = [true, contacted, replied, booked, demoed, !won.is_empty(), lost];
        let cur = g.people.entry(contact.clone()).or_insert([false; 7]);
        for (i, f) in flags.iter().enumerate() {
            cur[i] |= *f;
        }
        g.won_deals.extend(won.into_iter().cloned());
    }
    let mut out = Vec::new();
    for (key, g) in &groups {
        let count = |i: usize| g.people.values().filter(|f| f[i]).count() as i64;
        let c: Vec<i64> = (0..7).map(count).collect();
        let ratio = |a: i64, b: i64| if b == 0 { Value::Null } else { json!(((a as f64 / b as f64) * 10_000.0).round() / 10_000.0) };
        let (mut mrr, mut amount) = (0.0f64, 0.0f64);
        for d in &g.won_deals {
            let (m, a, cur): (Option<f64>, Option<f64>, String) = conn
                .query_row("SELECT mrr_usd, amount, currency FROM crm_deals WHERE id=?1", [d], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap_or((None, None, String::new()));
            mrr += m.unwrap_or(0.0);
            if cur.eq_ignore_ascii_case("USD") || cur.is_empty() {
                amount += a.unwrap_or(0.0);
            }
        }
        let mut row = json!({ "group": if by_cohort { "cohort" } else { "sequence" }, "key": key, "name": if by_cohort { key.clone() } else { g.name.clone() } });
        for (i, step) in STEPS.iter().enumerate() {
            row[*step] = json!(c[i]);
        }
        row["conversion"] = json!({
            "contacted_per_enrolled": ratio(c[1], c[0]), "replied_per_contacted": ratio(c[2], c[1]), "booked_per_replied": ratio(c[3], c[2]),
            "demoed_per_booked": ratio(c[4], c[3]), "won_per_demoed": ratio(c[5], c[4]), "won_per_enrolled": ratio(c[5], c[0]),
        });
        row["mrr_usd"] = json!(mrr);
        row["won_amount_usd"] = json!(amount);
        out.push(row);
    }
    Ok(json!({
        "contract": FUNNEL_CONTRACT, "measured": true, "n_considered": rows.len(), "why_unmeasured": Value::Null,
        "since": since_day, "as_of": chrono::Utc::now().to_rfc3339(), "groups": out,
    }))
}

async fn funnel_route(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<Value> { Ok(funnel(&*store.read()?, &q)?) }).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "amux::crm", measured = false, verdict = "crm_funnel_failed", "the funnel could not be read: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "contract": FUNNEL_CONTRACT, "measured": false, "n_considered": 0,
                "why_unmeasured": e.to_string(), "groups": [] }))).into_response()
        }
        Err(e) => internal(anyhow::anyhow!(e)),
    }
}

// ---- address check ---------------------------------------------------------------------

const ROLE_LOCALS: &[&str] = &["info", "sales", "support", "hello", "contact", "admin", "office", "team", "help", "billing", "jobs", "careers",
    "marketing", "press", "noreply", "no-reply", "webmaster", "postmaster", "abuse"];
const DISPOSABLE: &[&str] = &["mailinator.com", "guerrillamail.com", "10minutemail.com", "tempmail.com", "temp-mail.org", "yopmail.com",
    "trashmail.com", "sharklasers.com", "getnada.com", "dispostable.com", "maildrop.cc", "throwawaymail.com"];

/// The free check of an address before a cold send. `mx` is the domain's MX
/// lookup: Some(true) found, Some(false) the domain has none, None the
/// lookup could not run (then `measured` is false and nothing is refused).
pub(crate) fn email_verdict(addr: &str, mx: Option<bool>) -> Value {
    let a = addr.trim().to_lowercase();
    let mut parts = a.splitn(2, '@');
    let (local, domain) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let syntax = !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.') && !a.contains(' ') && !domain.contains('@');
    let role = ROLE_LOCALS.contains(&local);
    let disposable = DISPOSABLE.contains(&domain);
    let (ok, why) = if !syntax { (false, "not an email address") }
        else if disposable { (false, "a disposable mail domain") }
        else if mx == Some(false) { (false, "the domain takes no mail (no MX record)") }
        else { (true, "") };
    json!({ "address": a, "ok": ok, "why": why, "syntax": syntax, "role": role, "disposable": disposable, "mx": mx,
        "measured": mx.is_some() || !syntax || disposable, "checked_at": now_secs() })
}

/// The domain's MX records, asked of the first nameserver in /etc/resolv.conf
/// over UDP. None when no nameserver is configured or the lookup failed.
#[cfg(not(test))]
pub(crate) fn has_mx(domain: &str) -> Option<bool> {
    let ns = std::fs::read_to_string("/etc/resolv.conf").ok()?
        .lines().find_map(|l| l.trim().strip_prefix("nameserver").map(|s| s.trim().to_string()))?;
    let addr: std::net::IpAddr = ns.parse().ok()?;
    let sock = std::net::UdpSocket::bind(if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).ok()?;
    sock.set_read_timeout(Some(std::time::Duration::from_secs(3))).ok()?;
    let id: u16 = (now_secs() & 0xffff) as u16;
    sock.send_to(&mx_query(id, domain), (addr, 53)).ok()?;
    let mut buf = [0u8; 1500];
    let (n, _) = sock.recv_from(&mut buf).ok()?;
    mx_answer(&buf[..n], id)
}

/// Tests never touch the network: reserved names (RFC 2606) take no mail, the rest do.
#[cfg(test)]
pub(crate) fn has_mx(domain: &str) -> Option<bool> {
    Some(!(domain.ends_with(".example") || domain.ends_with(".invalid")))
}

fn mx_query(id: u16, domain: &str) -> Vec<u8> {
    let mut q = Vec::with_capacity(32 + domain.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]); // recursion desired; one question
    for label in domain.trim_end_matches('.').split('.') {
        q.push(label.len().min(63) as u8);
        q.extend_from_slice(&label.as_bytes()[..label.len().min(63)]);
    }
    q.extend_from_slice(&[0, 0, 15, 0, 1]); // root, QTYPE MX, QCLASS IN
    q
}

/// Some(true) when the answer names a mail server, Some(false) for NXDOMAIN,
/// an empty answer, or only a "null MX" (RFC 7505: preference 0 and the root
/// as exchange, a domain saying it takes no mail), None for a reply that is
/// not ours or a server failure.
fn mx_answer(b: &[u8], id: u16) -> Option<bool> {
    if b.len() < 12 || u16::from_be_bytes([b[0], b[1]]) != id || b[2] & 0x80 == 0 {
        return None;
    }
    let rcode = b[3] & 0x0f;
    let (qd, an) = (u16::from_be_bytes([b[4], b[5]]), u16::from_be_bytes([b[6], b[7]]));
    match rcode {
        0 => {}
        3 => return Some(false),
        _ => return None,
    }
    if an == 0 {
        return Some(false);
    }
    // Skip a name: labels to a zero byte, or a 2-byte compression pointer.
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
    let mut real = false;
    for _ in 0..an {
        i = skip_name(b, i)?;
        let ty = u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]);
        let len = u16::from_be_bytes([*b.get(i + 8)?, *b.get(i + 9)?]) as usize;
        let rd = i + 10;
        if ty == 15 && len >= 3 {
            // preference (2 bytes), then the exchange; a lone zero byte is the root.
            let null = len == 3 && b.get(rd + 2) == Some(&0);
            real |= !null;
        }
        i = rd + len;
    }
    Some(real)
}

/// Check a person's address and store the result on them (`email_check`).
pub(crate) async fn check_and_store(state: &AppState, contact: &str) -> Option<Value> {
    let store = state.store.clone();
    let c = contact.to_string();
    let email: String = tokio::task::spawn_blocking(move || -> Option<String> {
        store.read().ok()?.query_row("SELECT email FROM crm_contacts WHERE id=?1 AND deleted IS NULL", [&c], |r| r.get(0)).ok()
    })
    .await
    .ok()
    .flatten()?;
    if email.trim().is_empty() {
        return Some(json!({ "ok": false, "why": "no email address", "measured": true }));
    }
    let domain = email.rsplit('@').next().unwrap_or("").to_lowercase();
    // AMUX_CRM_MX_CHECK=0 turns the lookup off (the e2e servers, whose made-up
    // domains have no mail); the check then says it did not look.
    let mx = if crate::config::env_string("AMUX_CRM_MX_CHECK", "1") == "0" { None } else {
        tokio::task::spawn_blocking(move || has_mx(&domain)).await.ok().flatten()
    };
    let v = email_verdict(&email, mx);
    let (c2, body) = (contact.to_string(), v.to_string());
    let _ = state
        .store
        .write_async(move |conn| {
            conn.execute("UPDATE crm_contacts SET email_check=?2 WHERE id=?1", rusqlite::params![&c2, &body])?;
            Ok(WriteOutcome { applied: true, events: vec![ev("crm_contact", &c2, MutationKind::Updated)] })
        })
        .await;
    tracing::info!(target: "amux::crm", contact = %contact, ok = v["ok"].as_bool().unwrap_or(false), measured = v["measured"].as_bool().unwrap_or(false),
        n_considered = 1, verdict = "crm_email_checked", "an address was checked before a cold send");
    Some(v)
}

async fn check_email_route(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match check_and_store(&state, &id).await {
        Some(v) => Json(v).into_response(),
        None => err(StatusCode::NOT_FOUND, json!({ "error": "not found" })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stage_rules_hold_the_owner_boundary_and_the_closing_fields() {
        let full = json!({ "amount": 12000, "close_date": "2026-12-01" });
        assert_eq!(check_stage("demo", "won", &full, false).unwrap_err().0, "crm_deal_owner_stage", "a worker cannot sign");
        assert_eq!(check_stage("proposal", "lost", &json!({ "lost_reason": "timing" }), false).unwrap_err().0, "crm_deal_owner_stage", "a lost proposal is the owner's");
        assert!(check_stage("demo", "won", &full, true).is_ok(), "a contract needs no monthly figure");
        assert_eq!(check_stage("demo", "won", &json!({ "amount": 12000 }), true).unwrap_err().0, "crm_deal_won_incomplete");
        assert_eq!(check_stage("demo", "lost", &json!({}), false).unwrap_err().0, "crm_deal_lost_reason");
        assert!(check_stage("demo", "lost", &json!({ "lost_reason": "no_budget" }), false).is_ok());
        assert!(check_stage("lead", "demo", &json!({}), false).is_ok(), "a worker moves a deal up to demo");
    }

    #[test]
    fn an_address_fails_on_syntax_a_throwaway_domain_or_no_mx_and_a_role_is_flagged() {
        assert_eq!(email_verdict("ada@acme.io", Some(true))["ok"], true);
        assert_eq!(email_verdict("ada@acme", Some(true))["ok"], false);
        assert_eq!(email_verdict("ada@mailinator.com", Some(true))["why"], "a disposable mail domain");
        assert_eq!(email_verdict("ada@nowhere.example", Some(false))["ok"], false);
        let v = email_verdict("info@acme.io", Some(true));
        assert_eq!((v["ok"].as_bool(), v["role"].as_bool()), (Some(true), Some(true)));
        let v = email_verdict("ada@acme.io", None);
        assert_eq!((v["ok"].as_bool(), v["measured"].as_bool()), (Some(true), Some(false)), "a lookup that could not run refuses nothing and says so");
    }

    #[test]
    fn an_mx_question_and_its_answers_read_as_dns_packets() {
        let q = mx_query(0x1234, "acme.io");
        assert_eq!(&q[..2], &[0x12, 0x34]);
        assert_eq!(&q[12..], &[4, b'a', b'c', b'm', b'e', 2, b'i', b'o', 0, 0, 15, 0, 1]);
        // A reply: header, the question echoed, one MX answer pointing at the question's name.
        let answer = |exchange: &[u8]| {
            let mut a = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
            a.extend_from_slice(&q[12..]);
            a.extend_from_slice(&[0xc0, 12, 0, 15, 0, 1, 0, 0, 1, 0]);
            a.extend_from_slice(&((2 + exchange.len()) as u16).to_be_bytes());
            a.extend_from_slice(&[0, 10]);
            a.extend_from_slice(exchange);
            a
        };
        assert_eq!(mx_answer(&answer(&[2, b'm', b'x', 0xc0, 12]), 0x1234), Some(true), "a real mail server");
        assert_eq!(mx_answer(&answer(&[0]), 0x1234), Some(false), "a null MX: the domain takes no mail");
        let mut none = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 0, 0, 0, 0, 0];
        none.extend_from_slice(&q[12..]);
        assert_eq!(mx_answer(&none, 0x1234), Some(false), "no records");
        none[3] = 0x83;
        assert_eq!(mx_answer(&none, 0x1234), Some(false), "NXDOMAIN");
        assert_eq!(mx_answer(&none, 0x9999), None, "not our question");
    }
    use crate::api::standing_approvals::tests::{call, test_state};

    async fn app() -> (Router, AppState) {
        let mut state = test_state();
        state.auth_token = Some("owner-secret".into());
        let app = crate::api::crm::routes().merge(crate::api::crm_accounts::routes()).merge(crate::api::crm_agent::routes())
            .merge(crate::api::crm_outbound::routes()).merge(routes()).with_state(state.clone());
        (app, state)
    }
    const OWNER: [(&str, &str); 1] = [("authorization", "Bearer owner-secret")];
    const W: [(&str, &str); 1] = [("x-amux-session", "amux-gtm")];

    fn events(state: &AppState, ty: &str) -> Vec<Value> {
        let conn = state.store.read().unwrap();
        let mut q = conn.prepare("SELECT data FROM session_events WHERE type=?1 ORDER BY id").unwrap();
        q.query_map([ty], |r| r.get::<_, String>(0)).unwrap().filter_map(Result::ok).map(|d| serde_json::from_str(&d).unwrap()).collect()
    }

    async fn reply(state: &AppState, contact: &str, text: &str) -> String {
        let (c, t, ix) = (contact.to_string(), text.to_string(), ulid::Ulid::new().to_string());
        let i2 = ix.clone();
        state.store.write_async(move |conn| {
            conn.execute("INSERT INTO crm_interactions (id,contact_id,date,type,notes,created,updated,direction) VALUES (?1,?2,'2026-11-27','email',?3,?4,?4,'in')",
                rusqlite::params![&i2, &c, &t, now_secs()])?;
            crate::api::crm_outbound::on_reply(conn, &c, &t)?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        }).await.unwrap();
        ix
    }

    #[tokio::test]
    async fn a_deal_runs_from_reply_to_won_and_the_funnel_counts_each_step_once() {
        let (app, state) = app().await;
        let (_, a) = call(&app, "POST", "/accounts", &OWNER, Some(json!({"name": "Renderwell", "domain": "renderwell.io"}))).await;
        let acct = a["id"].as_str().unwrap().to_string();
        let (_, p) = call(&app, "POST", "/contacts", &OWNER, Some(json!({"name": "Nina Park", "email": "nina@renderwell.io", "account_id": acct}))).await;
        let nina = p["id"].as_str().unwrap().to_string();
        let (_, s) = call(&app, "POST", "/sequences", &W, Some(json!({"name": "IP check", "steps": [
            {"channel": "email", "action": "email", "template": "Hi {first_name}, the app: {asset_url}"}, {"day": 3, "channel": "linkedin", "action": "dm"}]}))).await;
        let seq = s["id"].as_str().unwrap().to_string();
        let (_, e) = call(&app, "POST", &format!("/sequences/{seq}/enroll"), &W, Some(json!({"contact_ids": [nina.clone()], "cohort": "ip-check-2026-12"}))).await;
        let t1 = e["enrolled"][0]["touch_id"].as_str().unwrap().to_string();
        call(&app, "POST", &format!("/touches/{t1}/approve"), &OWNER, None).await;
        call(&app, "POST", &format!("/touches/{t1}/done"), &W, Some(json!({"result": "sent"}))).await;

        // A reply opens the deal at lead; the worker reads it as interested and it moves to qualification.
        let ix = reply(&state, &nina, "sounds interesting, tell me more").await;
        let (_, deals) = call(&app, "GET", "/deals", &OWNER, None).await;
        let deal = deals[0]["id"].as_str().unwrap().to_string();
        assert_eq!(deals[0]["stage"], "lead", "a reply alone does not say yes");
        let (st, v) = call(&app, "POST", &format!("/replies/{ix}/intent"), &W, Some(json!({"intent": "interested", "why": "asked for more"}))).await;
        assert_eq!((st, v["deal"]["to"].as_str()), (StatusCode::OK, Some("qualification")), "{v}");
        let (st, _) = call(&app, "POST", &format!("/replies/{ix}/intent"), &W, Some(json!({"intent": "maybe"}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "an intent outside the five is refused");

        // The deal's app reaches the next draft.
        call(&app, "PATCH", &format!("/deals/{deal}"), &W, Some(json!({"asset_url": "https://apps.example/renderwell"}))).await;
        let filled = { let conn = state.store.read().unwrap(); crate::api::crm_outbound::fill(&conn, "see {asset_url} for {deal}", &nina) };
        assert_eq!(filled, "see https://apps.example/renderwell for Renderwell");

        // A booking moves it to demo and plans the prep touch.
        let (st, _) = call(&app, "POST", &format!("/contacts/{nina}/interactions"), &W, Some(json!({"type": "meeting", "notes": "calendly abc"}))).await;
        assert_eq!(st, StatusCode::CREATED);
        let (_, d) = call(&app, "GET", &format!("/deals/{deal}"), &OWNER, None).await;
        assert_eq!((d["stage"].as_str(), d["next_step"].as_str().map(|s| s.starts_with("prep the demo"))), (Some("demo"), Some(true)), "{d}");
        let (_, touches) = call(&app, "GET", &format!("/touches?contact_id={nina}"), &OWNER, None).await;
        assert!(touches.as_array().unwrap().iter().any(|t| t["source"] == "booking" && t["status"] == "planned" && t["assignee"] == "amux-gtm"), "{touches}");

        // The human boundary: a worker cannot sign; the owner needs the closing fields.
        let (st, r) = call(&app, "PATCH", &format!("/deals/{deal}"), &W, Some(json!({"stage": "won", "amount": 12000, "mrr_usd": 1000, "close_date": "2026-12-20"}))).await;
        assert_eq!((st, r["code"].as_str()), (StatusCode::CONFLICT, Some("crm_deal_owner_stage")));
        let (st, r) = call(&app, "PATCH", &format!("/deals/{deal}"), &OWNER, Some(json!({"stage": "won", "amount": 12000}))).await;
        assert_eq!((st, r["code"].as_str()), (StatusCode::CONFLICT, Some("crm_deal_won_incomplete")));
        let (_, d) = call(&app, "GET", &format!("/deals/{deal}"), &OWNER, None).await;
        assert_eq!((d["stage"].as_str(), d["amount"].as_f64()), (Some("demo"), None), "a refused move writes nothing");
        let (st, _) = call(&app, "PATCH", &format!("/deals/{deal}"), &OWNER, Some(json!({"stage": "won", "amount": 12000, "close_date": "2026-12-20"}))).await;
        assert_eq!(st, StatusCode::OK);

        let (_, h) = call(&app, "GET", &format!("/deals/{deal}/stages"), &OWNER, None).await;
        let path: Vec<&str> = h["stages"].as_array().unwrap().iter().map(|x| x["to_stage"].as_str().unwrap()).collect();
        assert_eq!(path, vec!["lead", "qualification", "demo", "won"]);
        let moves: Vec<String> = events(&state, "crm.deal.stage").iter().map(|e| e["to"].as_str().unwrap().to_string()).collect();
        assert_eq!(moves, vec!["qualification", "demo", "won"], "each move is an event");
        assert_eq!(events(&state, "crm.reply.classified").len(), 1);

        let (_, f) = call(&app, "GET", "/funnel", &OWNER, None).await;
        assert_eq!((f["contract"].as_str(), f["measured"].as_bool(), f["n_considered"].as_i64()), (Some("crm.funnel.v1"), Some(true), Some(1)));
        let g = &f["groups"][0];
        let counts: Vec<i64> = STEPS.iter().map(|k| g[*k].as_i64().unwrap()).collect();
        assert_eq!(counts, vec![1, 1, 1, 1, 1, 1, 0], "{g}");
        assert_eq!((g["key"].as_str(), g["mrr_usd"].as_f64(), g["won_amount_usd"].as_f64()), (Some(seq.as_str()), Some(0.0), Some(12000.0)));
        assert_eq!(g["conversion"]["won_per_enrolled"].as_f64(), Some(1.0));
        let (_, f) = call(&app, "GET", "/funnel?group=cohort", &OWNER, None).await;
        assert_eq!(f["groups"][0]["key"], "ip-check-2026-12");

        // Lost later: still demoed and won in the history, now counted lost.
        call(&app, "PATCH", &format!("/deals/{deal}"), &OWNER, Some(json!({"stage": "lost", "lost_reason": "no_budget"}))).await;
        let (_, f) = call(&app, "GET", "/funnel", &OWNER, None).await;
        assert_eq!((f["groups"][0]["demoed"].as_i64(), f["groups"][0]["lost"].as_i64()), (Some(1), Some(1)));
    }

    #[tokio::test]
    async fn each_intent_makes_its_move() {
        let (app, state) = app().await;
        let mut ids = vec![];
        for n in ["Jon Bell", "Maya Ortiz", "Sam Reed", "Leah Wu"] {
            let (_, p) = call(&app, "POST", "/contacts", &OWNER, Some(json!({"name": n, "email": format!("{}@x.io", n.split(' ').next().unwrap().to_lowercase())}))).await;
            ids.push(p["id"].as_str().unwrap().to_string());
        }
        let (_, s) = call(&app, "POST", "/sequences", &W, Some(json!({"name": "s", "stop_on_reply": false, "steps": [{"channel": "linkedin", "action": "dm"}, {"day": 3, "channel": "email", "action": "email"}]}))).await;
        let seq = s["id"].as_str().unwrap().to_string();
        call(&app, "POST", &format!("/sequences/{seq}/enroll"), &W, Some(json!({"contact_ids": ids.clone()}))).await;

        let ix = reply(&state, &ids[0], "not for us").await;
        let (_, v) = call(&app, "POST", &format!("/replies/{ix}/intent"), &W, Some(json!({"intent": "not_interested", "why": "they said no", "do_not_contact": true}))).await;
        assert_eq!(v["deal"]["to"], "lost", "{v}");
        let (_, p) = call(&app, "GET", &format!("/contacts/{}", ids[0]), &OWNER, None).await;
        assert_eq!(p["status"], "do_not_contact");
        let (_, t) = call(&app, "GET", &format!("/touches?contact_id={}", ids[0]), &OWNER, None).await;
        assert!(t.as_array().unwrap().iter().all(|x| x["status"] == "skipped"), "their sequence stopped: {t}");

        let ix = reply(&state, &ids[1], "ping me in january").await;
        call(&app, "POST", &format!("/replies/{ix}/intent"), &W, Some(json!({"intent": "book_later", "follow_up_date": "2027-01-10"}))).await;
        let fu: String = state.store.read().unwrap().query_row("SELECT follow_up_date FROM crm_interactions WHERE id=?1", [&ix], |r| r.get(0)).unwrap();
        assert_eq!(fu, "2027-01-10");

        let ix = reply(&state, &ids[2], "we already use another tool").await;
        let (_, v) = call(&app, "POST", &format!("/replies/{ix}/intent"), &W, Some(json!({"intent": "objection", "why": "uses another tool"}))).await;
        assert_eq!(v["deal"]["next_step"], "answer their objection: uses another tool");

        let ix = reply(&state, &ids[3], "talk to my colleague Ana").await;
        let (st, _) = call(&app, "POST", &format!("/replies/{ix}/intent"), &W, Some(json!({"intent": "referral"}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "a referral names someone");
        let (_, v) = call(&app, "POST", &format!("/replies/{ix}/intent"), &W, Some(json!({"intent": "referral", "why": "owns the media library", "referral": {"name": "Ana Silva", "email": "Ana@X.io"}}))).await;
        let rid = v["referral_contact_id"].as_str().unwrap();
        let (_, r) = call(&app, "GET", &format!("/contacts/{rid}"), &OWNER, None).await;
        assert_eq!((r["email"].as_str(), r["source"].as_str()), (Some("ana@x.io"), Some(format!("referral:{}", ids[3]).as_str())));
    }

    #[tokio::test]
    async fn cold_outreach_skips_customers_competitors_the_owners_deals_bad_addresses_and_a_busy_account() {
        let (app, state) = app().await;
        let acct = |name: &'static str, tags: Value| { let app = app.clone(); async move {
            let (_, a) = call(&app, "POST", "/accounts", &OWNER, Some(json!({"name": name, "tags": tags}))).await;
            a["id"].as_str().unwrap().to_string() } };
        let (small, rival, signed) = (acct("Small Co", json!([])).await, acct("Rival", json!(["competitor"])).await, acct("Signed", json!([])).await);
        let person = |name: &'static str, account: String| { let app = app.clone(); async move {
            let (_, p) = call(&app, "POST", "/contacts", &OWNER, Some(json!({"name": name, "email": format!("{}@small.io", name.to_lowercase()), "account_id": account}))).await;
            p["id"].as_str().unwrap().to_string() } };
        let (a, b, r, c) = (person("Ann", small.clone()).await, person("Bea", small.clone()).await, person("Rex", rival).await, person("Cal", signed.clone()).await);
        call(&app, "POST", "/deals", &OWNER, Some(json!({"title": "Signed", "account_id": signed, "stage": "won", "amount": 1, "mrr_usd": 1, "close_date": "2026-11-01"}))).await;
        let touch = |c: &str, ch: &'static str, ac: &'static str| { let app = app.clone(); let c = c.to_string(); async move {
            call(&app, "POST", "/touches", &W, Some(json!({"contact_id": c, "channel": ch, "action": ac, "body": "hi"}))).await } };
        assert_eq!(touch(&r, "linkedin", "dm").await.1["code"], "crm_touch_competitor");
        assert_eq!(touch(&c, "linkedin", "dm").await.1["code"], "crm_touch_customer");

        // Ann is reached; Bea at the same small company waits a week.
        let (st, t) = touch(&a, "linkedin", "dm").await;
        assert_eq!(st, StatusCode::CREATED, "{t}");
        let id = t["id"].as_str().unwrap().to_string();
        call(&app, "POST", &format!("/touches/{id}/approve"), &OWNER, None).await;
        call(&app, "POST", &format!("/touches/{id}/done"), &W, Some(json!({"result": "sent"}))).await;
        assert_eq!(touch(&b, "linkedin", "dm").await.1["code"], "crm_touch_account_cooldown");
        // A like is no outreach: it runs on its own and holds nothing.
        let (_, l) = call(&app, "POST", "/touches", &W, Some(json!({"contact_id": b, "channel": "linkedin", "action": "like"}))).await;
        let (st, _) = call(&app, "POST", &format!("/touches/{}/done", l["id"].as_str().unwrap()), &W, Some(json!({"result": "liked"}))).await;
        assert_eq!(st, StatusCode::OK, "a like needs no approval");

        // The owner's own deal at the company keeps workers out, even beside a
        // worker's deal there that was opened first (land review F1).
        call(&app, "POST", "/deals", &OWNER, Some(json!({"title": "A worker's", "account_id": small, "owner": "worker-1"}))).await;
        call(&app, "POST", "/deals", &OWNER, Some(json!({"title": "Owner's", "account_id": small, "owner": "owner"}))).await;
        assert_eq!(touch(&a, "x", "dm").await.1["code"], "crm_touch_owner_deal");

        // An address that failed its check holds email only.
        let (bad, _) = (person("Dee", String::new()).await, ());
        let b2 = bad.clone();
        state.store.write_async(move |conn| {
            conn.execute("UPDATE crm_contacts SET email_check=?2 WHERE id=?1", rusqlite::params![&b2, email_verdict("dee@nowhere.example", Some(false)).to_string()])?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        }).await.unwrap();
        assert_eq!(touch(&bad, "email", "email").await.1["code"], "crm_touch_address_unverified");
        assert_eq!(touch(&bad, "linkedin", "dm").await.0, StatusCode::CREATED);
    }
}
