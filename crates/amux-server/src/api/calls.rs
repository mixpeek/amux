//! Outbound phone calls placed for a worker (docs/design/phone-calls.md).
//!
//! A worker asks for a call with a goal and context. amux builds the voice
//! model's instructions itself, so the AI disclosure is always the first
//! sentence. A number on `AMUX_CALL_ALLOWLIST` is dialed at once; any other
//! number is `held` until the owner approves that exact call in the Calls tab,
//! the same rule as a worker's external email. amux hands the call to a bridge
//! (`AMUX_CALL_BRIDGE_URL`) that places it and talks to the voice model, and the
//! bridge reports back with a per-call token amux stores only as a sha256.
//!
//! - `POST /api/calls`: request a call (`to`, `goal`, `context`, `max_minutes`).
//! - `GET  /api/calls[?worker=&status=&limit=]`, `GET /api/calls/{id}`.
//! - `POST /api/calls/{id}/approve`, `/decline`: the owner only
//!   (`land_approvals::owner_only`, the vault's predicate). A worker can never
//!   approve, its own call or another's.
//! - `POST /api/calls/{id}/events`, `/result`: the bridge, bearer result_token.
//!
//! A finished call (any terminal status) writes `~/.amux/calls/<id>.md`, logs a
//! CRM interaction of type `call` when the number belongs to a contact, emits
//! `call.completed`, and tells the requesting worker the outcome and the path.
//!
//! Verdicts: call_requested, call_held, call_approved, call_declined,
//! call_dialed, call_answered, call_completed, call_failed, call_closed,
//! call_token_rejected, call_worker_told, call_worker_not_told.

use super::calendar::query_rows_json;
use super::AppState;
use crate::config::now_f64;
use crate::db::WriteOutcome;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/calls", get(list_route).post(request_route))
        .route("/api/calls/{id}", get(get_route))
        .route("/api/calls/{id}/approve", post(approve_route))
        .route("/api/calls/{id}/decline", post(decline_route))
        .route("/api/calls/{id}/events", post(events_route))
        .route("/api/calls/{id}/result", post(result_route))
}

/// Statuses a call ends in. Anything else is still in flight.
pub const TERMINAL: &[&str] = &["completed", "no_answer", "busy", "failed", "canceled", "declined"];

/// What a bridge may report as a result's status.
const RESULT_STATUSES: &[&str] = &["completed", "no_answer", "busy", "failed", "canceled"];

const DEFAULT_MAX_MINUTES: i64 = 10;
const MAX_MAX_MINUTES: i64 = 60;

fn bad(code: StatusCode, error: &str) -> Response {
    (code, Json(json!({"ok": false, "error": error}))).into_response()
}

fn env(home: &FsPath, key: &str) -> Option<String> {
    crate::api::settings::effective_env(home, key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

// ── Pure pieces ──

/// A dialable number in E.164, or why not. Accepts `+<country><number>` with
/// common punctuation, a bare 10-digit North American number, or 11 digits
/// starting with 1.
pub fn normalize_e164(raw: &str) -> Result<String, String> {
    let t = raw.trim();
    let plus = t.starts_with('+');
    let mut digits = String::new();
    for c in t.chars() {
        match c {
            '0'..='9' => digits.push(c),
            '+' | ' ' | '-' | '(' | ')' | '.' => {}
            _ => return Err(format!("'{raw}' is not a phone number")),
        }
    }
    if t.matches('+').count() > usize::from(plus) {
        return Err(format!("'{raw}' is not a phone number"));
    }
    let e164 = if plus {
        digits
    } else if digits.len() == 10 && !digits.starts_with(['0', '1']) {
        // A North American area code never starts with 0 or 1.
        format!("1{digits}")
    } else if digits.len() == 11 && digits.starts_with('1') && !digits[1..].starts_with(['0', '1']) {
        digits
    } else {
        return Err(format!("'{raw}' needs a country code, e.g. +15551234567"));
    };
    if !(8..=15).contains(&e164.len()) || e164.starts_with('0') {
        return Err(format!("'{raw}' is not a valid E.164 number"));
    }
    Ok(format!("+{e164}"))
}

/// Numbers that skip the owner's approval, from `AMUX_CALL_ALLOWLIST`.
pub fn allowlist(home: &FsPath) -> Vec<String> {
    env(home, "AMUX_CALL_ALLOWLIST")
        .unwrap_or_default()
        .split(',')
        .filter_map(|n| normalize_e164(n).ok())
        .collect()
}

fn owner_name(home: &FsPath) -> String {
    env(home, "AMUX_OWNER_NAME").unwrap_or_else(|| "Ethan Steininger".to_string())
}

/// The voice model's instructions. Built here, never by the worker, so the
/// disclosure is always the first sentence and the limits are always present.
pub fn instructions(owner: &str, goal: &str, context: &str, max_minutes: i64) -> String {
    let context = if context.trim().is_empty() { "(none given)" } else { context.trim() };
    format!(
        "You are an AI assistant calling on behalf of {owner}. Open the call by saying exactly that: \
         you are an AI assistant calling on behalf of {owner}.\n\n\
         Goal: {goal}\n\n\
         Context:\n{context}\n\n\
         Limits:\n\
         - Do not agree to any payment, purchase, contract or charge.\n\
         - Do not share personal data beyond what the context above names.\n\
         - Keep the call under {max_minutes} minutes. When the time is up, wrap up and hang up.\n\n\
         If you cannot complete the goal, say you will follow up, and end politely.",
        goal = goal.trim(),
    )
}

fn is_loopback_url(url: &str) -> bool {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let host = if host.starts_with('[') {
        host.split(']').next().unwrap_or("").trim_start_matches('[')
    } else {
        host.split(':').next().unwrap_or("")
    };
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Where the bridge reports back: `AMUX_CALL_RESULT_BASE`, else this amux's
/// public address. A bridge on this machine (a loopback URL, as in a test or a
/// local dev bridge) can use the server's own local address from
/// `endpoint.json`; a remote bridge never gets a loopback address it cannot reach.
pub fn result_base(home: &FsPath, bridge: &str) -> Option<String> {
    if let Some(b) = env(home, "AMUX_CALL_RESULT_BASE") {
        return Some(b);
    }
    if let Some(u) = super::static_files::public_links::public_url(home) {
        return Some(u);
    }
    if !is_loopback_url(bridge) {
        return None;
    }
    std::fs::read_to_string(home.join("endpoint.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["canonical_url"].as_str().map(str::to_string))
}

pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn mint_token() -> Result<String, String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| format!("no OS random source for the call's token: {e}"))?;
    Ok(hex::encode(b))
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn transcript_path(home: &FsPath, id: &str) -> PathBuf {
    home.join("calls").join(format!("{id}.md"))
}

fn valid_id(id: &str) -> bool {
    id.starts_with("CALL-") && id.len() <= 40 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The transcript as markdown: what was asked, what came of it, and every turn.
pub fn transcript_markdown(call: &Value, transcript: &[Value], recording_url: Option<&str>) -> String {
    let s = |k: &str| call.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let who = call.get("contact_name").and_then(Value::as_str).filter(|n| !n.is_empty()).map(str::to_string);
    let mut out = format!("# Call {}\n\n", s("id"));
    out.push_str(&format!("- To: {}{}\n", s("to_number"), who.map(|n| format!(" ({n})")).unwrap_or_default()));
    out.push_str(&format!("- Worker: {}\n", s("worker")));
    out.push_str(&format!("- Status: {}\n", s("status")));
    if let Some(d) = call.get("duration_s").and_then(Value::as_i64) {
        out.push_str(&format!("- Duration: {d}s\n"));
    }
    if let Some(u) = recording_url.filter(|u| !u.is_empty()) {
        out.push_str(&format!("- Recording: {u}\n"));
    }
    out.push_str(&format!("\n## Goal\n\n{}\n", s("goal")));
    out.push_str(&format!("\n## Outcome\n\n{}\n", if s("outcome").is_empty() { "(none reported)".into() } else { s("outcome") }));
    if !s("summary").is_empty() {
        out.push_str(&format!("\n## Summary\n\n{}\n", s("summary")));
    }
    out.push_str("\n## Transcript\n\n");
    if transcript.is_empty() {
        out.push_str("(no turns reported)\n");
    }
    for turn in transcript {
        let who = turn.get("who").and_then(Value::as_str).unwrap_or("?");
        let text = turn.get("text").and_then(Value::as_str).unwrap_or("");
        let t = turn.get("t").and_then(Value::as_f64).map(|t| format!(" [{t:.0}s]")).unwrap_or_default();
        out.push_str(&format!("**{who}**{t}: {text}\n\n"));
    }
    out
}

// ── Store helpers ──

const SELECT: &str = "SELECT c.id, c.worker, c.to_number, c.contact, COALESCE(p.name, '') AS contact_name, c.goal, \
     c.context, c.max_minutes, c.status, c.outcome, c.summary, c.duration_s, c.provider_sid, c.approved_by, \
     c.requested_at, c.approved_at, c.started_at, c.ended_at, c.error \
     FROM calls c LEFT JOIN crm_contacts p ON p.id = c.contact AND p.deleted IS NULL";

async fn exec(state: &AppState, sql: &'static str, params: Vec<SqlValue>) -> anyhow::Result<usize> {
    let slot = Arc::new(Mutex::new(0usize));
    let w = slot.clone();
    state
        .store
        .write_async(move |conn| {
            let n = conn.execute(sql, rusqlite::params_from_iter(params.iter()))?;
            *w.lock().unwrap_or_else(|p| p.into_inner()) = n;
            Ok(WriteOutcome { applied: n > 0, events: vec![] })
        })
        .await?;
    let n = *slot.lock().unwrap_or_else(|p| p.into_inner());
    Ok(n)
}

async fn load(state: &AppState, id: &str, with_transcript: bool) -> Option<Value> {
    let id = id.to_string();
    state
        .store
        .read_async(move |conn| {
            let sql = format!("{SELECT} WHERE c.id = ?1");
            let mut rows = query_rows_json(conn, &sql, &[&id as &dyn rusqlite::ToSql])?;
            let Some(mut row) = rows.pop() else { return Ok(None) };
            if with_transcript {
                let raw: String = conn.query_row("SELECT transcript FROM calls WHERE id = ?1", [&id], |r| r.get(0))?;
                let t: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!([]));
                row["transcript"] = t;
                let instr: String = conn.query_row("SELECT instructions FROM calls WHERE id = ?1", [&id], |r| r.get(0))?;
                row["instructions"] = json!(instr);
            }
            Ok(Some(row))
        })
        .await
        .ok()
        .flatten()
}

/// The CRM contact whose phone is this number, if any.
async fn contact_for(state: &AppState, number: &str) -> Option<String> {
    let number = number.to_string();
    state
        .store
        .read_async(move |conn| {
            let mut st = conn.prepare("SELECT id, phone FROM crm_contacts WHERE deleted IS NULL AND phone <> ''")?;
            let rows: Vec<(String, String)> =
                st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
            Ok(rows.into_iter().find(|(_, p)| normalize_e164(p).ok().as_deref() == Some(number.as_str())).map(|(id, _)| id))
        })
        .await
        .ok()
        .flatten()
}

async fn emit(state: &AppState, call: &Value, etype: &str) {
    let s = |k: &str| call.get(k).cloned().unwrap_or(Value::Null);
    let id = call["id"].as_str().unwrap_or("");
    let worker = call["worker"].as_str().filter(|w| !w.is_empty()).unwrap_or("owner");
    let data = json!({
        "call_id": id, "worker": s("worker"), "to": s("to_number"), "contact": s("contact"),
        "contact_name": s("contact_name"), "status": s("status"), "outcome": s("outcome"),
        "goal": s("goal"), "duration_s": s("duration_s"),
    });
    crate::api::session_verbs::emit_event_store(
        &state.store,
        worker,
        etype,
        Some(data),
        Some(format!("call:{id}:{etype}")),
        "calls",
    )
    .await;
}

// ── Routes ──

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct RequestBody {
    to: String,
    goal: String,
    context: String,
    max_minutes: Option<i64>,
}

async fn request_route(State(state): State<AppState>, headers: HeaderMap, uri: Uri, Json(body): Json<RequestBody>) -> Response {
    let home = crate::config::amux_home();
    let to = match normalize_e164(&body.to) {
        Ok(n) => n,
        Err(e) => return bad(StatusCode::BAD_REQUEST, &e),
    };
    if body.goal.trim().is_empty() {
        return bad(StatusCode::BAD_REQUEST, "a call needs a goal: what should the call achieve?");
    }
    let worker = crate::api::email::hdr_lane(&headers);
    // The owner placing a call needs no approval from themselves. A request
    // with no worker header is the owner only when it proves it (the owner
    // bearer and no member identity); otherwise it is an unattributed caller
    // and is held like any worker's call.
    let by_owner = worker.is_none()
        && super::land_approvals::owner_only(&state, &headers, &uri, "call_request_unproven_owner", "place a call without approval").is_ok();
    let worker = worker.unwrap_or_default();
    let max_minutes = body.max_minutes.unwrap_or(DEFAULT_MAX_MINUTES).clamp(1, MAX_MAX_MINUTES);
    let allowlisted = allowlist(&home).contains(&to);
    let contact = contact_for(&state, &to).await;
    let id = format!("CALL-{}", ulid::Ulid::new());
    let instr = instructions(&owner_name(&home), &body.goal, &body.context, max_minutes);
    let now = now_f64();
    let (status, approved_by) = if by_owner {
        ("approved", "owner")
    } else if allowlisted {
        ("approved", "allowlist")
    } else {
        ("held", "")
    };
    let ins = exec(
        &state,
        "INSERT INTO calls (id, worker, to_number, contact, goal, context, instructions, max_minutes, status, \
         approved_by, requested_at, approved_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        vec![
            SqlValue::Text(id.clone()),
            SqlValue::Text(worker.clone()),
            SqlValue::Text(to.clone()),
            contact.clone().map(SqlValue::Text).unwrap_or(SqlValue::Null),
            SqlValue::Text(body.goal.trim().to_string()),
            SqlValue::Text(body.context.trim().to_string()),
            SqlValue::Text(instr),
            SqlValue::Integer(max_minutes),
            SqlValue::Text(status.to_string()),
            SqlValue::Text(approved_by.to_string()),
            SqlValue::Real(now),
            if status == "approved" { SqlValue::Real(now) } else { SqlValue::Null },
        ],
    )
    .await;
    if let Err(e) = ins {
        tracing::error!(target: "amux::calls", error = %e, verdict = "call_store_failed", measured = true, n_considered = 1,
            "a call request could not be stored");
        return bad(StatusCode::INTERNAL_SERVER_ERROR, "the call could not be stored");
    }
    tracing::info!(target: "amux::calls", call = %id, worker = %worker, to = %to, allowlisted, by_owner,
        verdict = "call_requested", measured = true, n_considered = 1, "a call was requested");
    let Some(call) = load(&state, &id, false).await else {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, "the call was stored but could not be read back");
    };
    emit(&state, &call, "call.requested").await;
    if status == "held" {
        tracing::info!(target: "amux::calls", call = %id, worker = %worker, to = %to,
            verdict = "call_held", measured = true, n_considered = 1,
            "a call to a number outside the allowlist is held for the owner's approval");
        emit(&state, &call, "call.held").await;
        return (
            StatusCode::ACCEPTED,
            Json(json!({"ok": true, "id": id, "status": "held",
                "message": "held for the owner's approval in the Calls tab; you get a message when the call ends",
                "call": call})),
        )
            .into_response();
    }
    tracing::info!(target: "amux::calls", call = %id, approved_by, verdict = "call_approved", measured = true,
        n_considered = 1, "a call was approved without a hold");
    dial(&state, &id).await;
    let call = load(&state, &id, false).await.unwrap_or(call);
    let code = if call["status"] == "failed" { StatusCode::BAD_GATEWAY } else { StatusCode::CREATED };
    (code, Json(json!({"ok": code.is_success(), "id": id, "status": call["status"], "error": call["error"], "call": call})))
        .into_response()
}

async fn list_route(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let home = crate::config::amux_home();
    let worker = q.get("worker").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let status = q.get("status").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let limit = q.get("limit").and_then(|l| l.parse::<i64>().ok()).unwrap_or(100).clamp(1, 500);
    let rows = state
        .store
        .read_async(move |conn| {
            let mut sql = format!("{SELECT} WHERE 1=1");
            let mut params: Vec<SqlValue> = vec![];
            if let Some(w) = worker {
                params.push(SqlValue::Text(w));
                sql.push_str(&format!(" AND c.worker = ?{}", params.len()));
            }
            if let Some(s) = status {
                params.push(SqlValue::Text(s));
                sql.push_str(&format!(" AND c.status = ?{}", params.len()));
            }
            params.push(SqlValue::Integer(limit));
            sql.push_str(&format!(" ORDER BY c.requested_at DESC LIMIT ?{}", params.len()));
            let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
            let rows = query_rows_json(conn, &sql, &refs)?;
            let total: i64 = conn.query_row("SELECT COUNT(*) FROM calls", [], |r| r.get(0))?;
            Ok((rows, total))
        })
        .await;
    match rows {
        Ok((rows, total)) => Json(json!({
            "calls": rows, "count": rows.len(), "measured": true, "n_considered": total,
            "bridge_configured": env(&home, "AMUX_CALL_BRIDGE_URL").is_some(),
            "allowlist_count": allowlist(&home).len(),
        }))
        .into_response(),
        Err(e) => {
            tracing::warn!(target: "amux::calls", error = %e, verdict = "calls_unreadable", measured = false,
                n_considered = 0, "the calls table could not be read");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"calls": [], "measured": false, "why_unmeasured": e.to_string()})))
                .into_response()
        }
    }
}

async fn get_route(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if !valid_id(&id) {
        return bad(StatusCode::NOT_FOUND, "no such call");
    }
    match load(&state, &id, true).await {
        Some(mut c) => {
            let p = transcript_path(&crate::config::amux_home(), &id);
            if p.exists() {
                c["transcript_path"] = json!(p.to_string_lossy());
            }
            Json(c).into_response()
        }
        None => bad(StatusCode::NOT_FOUND, "no such call"),
    }
}

async fn approve_route(State(state): State<AppState>, headers: HeaderMap, uri: Uri, Path(id): Path<String>) -> Response {
    if let Err(r) = super::land_approvals::owner_only(&state, &headers, &uri, "call_approve_refused", "approve a call") {
        return *r;
    }
    if !valid_id(&id) {
        return bad(StatusCode::NOT_FOUND, "no such call");
    }
    let n = exec(
        &state,
        "UPDATE calls SET status = 'approved', approved_at = ?2, approved_by = 'owner' WHERE id = ?1 AND status = 'held'",
        vec![SqlValue::Text(id.clone()), SqlValue::Real(now_f64())],
    )
    .await
    .unwrap_or(0);
    if n == 0 {
        return match load(&state, &id, false).await {
            Some(c) => bad(StatusCode::CONFLICT, &format!("this call is {}, not held", c["status"].as_str().unwrap_or("?"))),
            None => bad(StatusCode::NOT_FOUND, "no such call"),
        };
    }
    tracing::info!(target: "amux::calls", call = %id, verdict = "call_approved", measured = true, n_considered = 1,
        "the owner approved a held call");
    dial(&state, &id).await;
    let call = load(&state, &id, false).await.unwrap_or(Value::Null);
    Json(json!({"ok": call["status"] != "failed", "call": call})).into_response()
}

async fn decline_route(State(state): State<AppState>, headers: HeaderMap, uri: Uri, Path(id): Path<String>) -> Response {
    if let Err(r) = super::land_approvals::owner_only(&state, &headers, &uri, "call_decline_refused", "decline a call") {
        return *r;
    }
    if !valid_id(&id) {
        return bad(StatusCode::NOT_FOUND, "no such call");
    }
    let n = exec(
        &state,
        "UPDATE calls SET status = 'declined', ended_at = ?2, error = 'declined by the owner' WHERE id = ?1 AND status = 'held'",
        vec![SqlValue::Text(id.clone()), SqlValue::Real(now_f64())],
    )
    .await
    .unwrap_or(0);
    if n == 0 {
        return match load(&state, &id, false).await {
            Some(c) => bad(StatusCode::CONFLICT, &format!("this call is {}, not held", c["status"].as_str().unwrap_or("?"))),
            None => bad(StatusCode::NOT_FOUND, "no such call"),
        };
    }
    tracing::info!(target: "amux::calls", call = %id, verdict = "call_declined", measured = true, n_considered = 1,
        "the owner declined a held call");
    finish(&state, &id, &[], None).await;
    let call = load(&state, &id, false).await.unwrap_or(Value::Null);
    Json(json!({"ok": true, "call": call})).into_response()
}

/// Hand an approved call to the bridge. Claims it (`approved` -> `dialing`)
/// before the request, so a result that arrives before the bridge answers is
/// never overwritten, and a second dial of the same call does nothing.
async fn dial(state: &AppState, id: &str) {
    let home = crate::config::amux_home();
    let fail = |why: String| async move {
        let n = exec(
            state,
            "UPDATE calls SET status = 'failed', error = ?2, ended_at = ?3 WHERE id = ?1 AND status IN ('approved', 'dialing')",
            vec![SqlValue::Text(id.to_string()), SqlValue::Text(why.clone()), SqlValue::Real(now_f64())],
        )
        .await
        .unwrap_or(0);
        tracing::warn!(target: "amux::calls", call = %id, error = %why, verdict = "call_failed", measured = true,
            n_considered = 1, "a call could not be placed");
        if n > 0 {
            finish(state, id, &[], None).await;
        }
    };
    let Some(bridge) = env(&home, "AMUX_CALL_BRIDGE_URL") else {
        return fail("no call bridge configured (AMUX_CALL_BRIDGE_URL)".to_string()).await;
    };
    let Some(base) = result_base(&home, &bridge) else {
        return fail("this amux's public address is unknown, so the bridge could not report back \
                     (set AMUX_CALL_RESULT_BASE or AMUX_PUBLIC_URL)"
            .to_string())
        .await;
    };
    let token = match mint_token() {
        Ok(t) => t,
        Err(e) => return fail(e).await,
    };
    let claimed = exec(
        state,
        "UPDATE calls SET status = 'dialing', result_token_hash = ?2 WHERE id = ?1 AND status = 'approved'",
        vec![SqlValue::Text(id.to_string()), SqlValue::Text(token_hash(&token))],
    )
    .await
    .unwrap_or(0);
    if claimed == 0 {
        return;
    }
    let Some(call) = load(state, id, true).await else { return };
    let base = base.trim_end_matches('/');
    let payload = json!({
        "call_id": id,
        "to": call["to_number"],
        "from": env(&home, "TWILIO_FROM").unwrap_or_default(),
        "instructions": call["instructions"],
        "voice": env(&home, "AMUX_CALL_VOICE").unwrap_or_else(|| "Ara".to_string()),
        "max_minutes": call["max_minutes"],
        "result_url": format!("{base}/api/calls/{id}/result"),
        "events_url": format!("{base}/api/calls/{id}/events"),
        "result_token": token,
    });
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).build();
    let resp = match client {
        Ok(c) => c.post(format!("{}/calls", bridge.trim_end_matches('/'))).json(&payload).send().await,
        Err(e) => return fail(format!("http client: {e}")).await,
    };
    let resp = match resp {
        Ok(r) => r,
        Err(e) => return fail(format!("the call bridge did not answer: {e}")).await,
    };
    let code = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !code.is_success() {
        let why = body["error"].as_str().map(str::to_string).unwrap_or_else(|| format!("HTTP {code}"));
        return fail(format!("the call bridge refused the call: {why}")).await;
    }
    let sid = body["provider_sid"].as_str().unwrap_or("").to_string();
    if let Err(e) = exec(
        state,
        "UPDATE calls SET provider_sid = ?2 WHERE id = ?1",
        vec![SqlValue::Text(id.to_string()), SqlValue::Text(sid.clone())],
    )
    .await
    {
        tracing::warn!(target: "amux::calls", call = %id, error = %e, verdict = "call_sid_unstored", measured = true,
            n_considered = 1, "the bridge's call id could not be stored");
    }
    tracing::info!(target: "amux::calls", call = %id, provider_sid = %sid, verdict = "call_dialed", measured = true,
        n_considered = 1, "the call bridge accepted the call");
    if let Some(c) = load(state, id, false).await {
        emit(state, &c, "call.started").await;
    }
}

/// The bridge's token, checked against this call's stored hash. It rides as
/// `Authorization: Bearer <token>` (the contract), or as `X-Amux-Call-Token`
/// when the bearer slot already carries the owner credential, as it must when
/// the request reaches a server whose auth middleware wants that bearer.
async fn authorize_bridge(state: &AppState, headers: &HeaderMap, id: &str) -> Result<(), Response> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty());
    let presented = header("x-amux-call-token")
        .or_else(|| header("authorization").and_then(|v| v.strip_prefix("Bearer ")).map(str::trim))
        .unwrap_or("");
    let idc = id.to_string();
    let stored: String = state
        .store
        .read_async(move |conn| {
            Ok(conn
                .query_row("SELECT result_token_hash FROM calls WHERE id = ?1", [&idc], |r| r.get::<_, String>(0))
                .unwrap_or_default())
        })
        .await
        .unwrap_or_default();
    if stored.is_empty() || presented.is_empty() || !constant_eq(&token_hash(presented), &stored) {
        tracing::warn!(target: "amux::calls", call = %id, presented = !presented.is_empty(), verdict = "call_token_rejected",
            measured = true, n_considered = 1, "a report for a call carried no valid result token");
        return Err(bad(StatusCode::UNAUTHORIZED, "this call's result token is required"));
    }
    Ok(())
}

async fn events_route(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    if !valid_id(&id) {
        return bad(StatusCode::NOT_FOUND, "no such call");
    }
    if let Err(r) = authorize_bridge(&state, &headers, &id).await {
        return r;
    }
    let event = body.get("event").or_else(|| body.get("status")).and_then(Value::as_str).unwrap_or("").to_string();
    match event.as_str() {
        "answered" => {
            let n = exec(
                &state,
                "UPDATE calls SET status = 'in_progress', started_at = COALESCE(started_at, ?2) WHERE id = ?1 AND status = 'dialing'",
                vec![SqlValue::Text(id.clone()), SqlValue::Real(now_f64())],
            )
            .await
            .unwrap_or(0);
            tracing::info!(target: "amux::calls", call = %id, applied = n > 0, verdict = "call_answered", measured = true,
                n_considered = 1, "the callee answered");
        }
        "ringing" | "ended" => {
            tracing::info!(target: "amux::calls", call = %id, event = %event, verdict = "call_event", measured = true,
                n_considered = 1, "the call bridge reported a call event");
        }
        _ => return bad(StatusCode::BAD_REQUEST, "event must be ringing, answered or ended"),
    }
    Json(json!({"ok": true})).into_response()
}

async fn result_route(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    if !valid_id(&id) {
        return bad(StatusCode::NOT_FOUND, "no such call");
    }
    if let Err(r) = authorize_bridge(&state, &headers, &id).await {
        return r;
    }
    let status = body.get("status").and_then(Value::as_str).unwrap_or("completed");
    if !RESULT_STATUSES.contains(&status) {
        return bad(StatusCode::BAD_REQUEST, &format!("status must be one of {}", RESULT_STATUSES.join(", ")));
    }
    let transcript: Vec<Value> = body.get("transcript").and_then(Value::as_array).cloned().unwrap_or_default();
    let text = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let duration = body.get("duration_s").and_then(Value::as_f64).map(|d| d.round() as i64);
    let n = exec(
        &state,
        "UPDATE calls SET status = ?2, outcome = ?3, summary = ?4, transcript = ?5, duration_s = ?6, ended_at = ?7, \
         error = CASE WHEN ?2 = 'failed' THEN ?8 ELSE error END \
         WHERE id = ?1 AND status IN ('dialing', 'in_progress')",
        vec![
            SqlValue::Text(id.clone()),
            SqlValue::Text(status.to_string()),
            SqlValue::Text(text("outcome")),
            SqlValue::Text(text("summary")),
            SqlValue::Text(Value::Array(transcript.clone()).to_string()),
            duration.map(SqlValue::Integer).unwrap_or(SqlValue::Null),
            SqlValue::Real(now_f64()),
            SqlValue::Text(text("error")),
        ],
    )
    .await
    .unwrap_or(0);
    if n == 0 {
        let now = load(&state, &id, false).await.map(|c| c["status"].as_str().unwrap_or("").to_string()).unwrap_or_default();
        return bad(StatusCode::CONFLICT, &format!("this call is {now}; a result is only taken once, while it is in flight"));
    }
    let recording = body.get("recording_url").and_then(Value::as_str).map(str::to_string);
    let path = finish(&state, &id, &transcript, recording.as_deref()).await;
    Json(json!({"ok": true, "transcript_path": path})).into_response()
}

/// Everything a call that has ended owes: the transcript file, the CRM
/// interaction, `call.completed`, and a message to the worker that asked.
/// Returns the transcript path when it was written.
async fn finish(state: &AppState, id: &str, transcript: &[Value], recording_url: Option<&str>) -> Option<String> {
    let home = crate::config::amux_home();
    let call = load(state, id, false).await?;
    let status = call["status"].as_str().unwrap_or("").to_string();
    let worker = call["worker"].as_str().unwrap_or("").to_string();
    let path = transcript_path(&home, id);
    let md = transcript_markdown(&call, transcript, recording_url);
    let written = std::fs::create_dir_all(path.parent().unwrap_or(&home)).and_then(|_| std::fs::write(&path, md));
    let path_s = path.to_string_lossy().to_string();
    if let Err(e) = &written {
        tracing::warn!(target: "amux::calls", call = %id, error = %e, verdict = "call_transcript_unwritten", measured = true,
            n_considered = 1, "the call's transcript file could not be written");
    }
    if let Some(contact) = call["contact"].as_str().filter(|c| !c.is_empty()) {
        let notes = format!(
            "Call {id} ({status}) by {worker}. Goal: {}. Outcome: {}. Transcript: {path_s}",
            call["goal"].as_str().unwrap_or(""),
            call["outcome"].as_str().filter(|o| !o.is_empty()).unwrap_or("(none)")
        );
        let r = crate::api::crm::add_interaction(
            State(state.clone()),
            Path(contact.to_string()),
            Json(json!({"type": "call", "notes": notes})),
        )
        .await;
        if !r.status().is_success() {
            tracing::warn!(target: "amux::calls", call = %id, contact, status = %r.status(), verdict = "call_crm_unlogged",
                measured = true, n_considered = 1, "the call could not be logged on its CRM contact");
        }
    }
    emit(state, &call, "call.completed").await;
    // Three kinds of ending, so a count of call_failed is a count of calls
    // that went wrong: the owner declining or a bridge reporting a cancel is
    // call_closed.
    let verdict = match status.as_str() {
        "completed" => "call_completed",
        "declined" | "canceled" => "call_closed",
        _ => "call_failed",
    };
    tracing::info!(target: "amux::calls", call = %id, status = %status, worker = %worker, verdict,
        measured = true, n_considered = 1, "a call ended");
    if !worker.is_empty() {
        let who = call["contact_name"].as_str().filter(|n| !n.is_empty()).map(|n| format!(" ({n})")).unwrap_or_default();
        let mut text = format!("[call] {id} to {}{who} ended: {status}.", call["to_number"].as_str().unwrap_or(""));
        if let Some(o) = call["outcome"].as_str().filter(|o| !o.is_empty()) {
            text.push_str(&format!(" Outcome: {o}."));
        }
        if let Some(e) = call["error"].as_str().filter(|e| !e.is_empty()) {
            text.push_str(&format!(" Why: {e}."));
        }
        if written.is_ok() {
            text.push_str(&format!(" Transcript: {path_s}"));
        }
        let out = crate::api::session_verbs::deliver_automated(state, &worker, &text, &format!("call:{id}")).await;
        if out.refused {
            tracing::warn!(target: "amux::calls", call = %id, worker = %worker, reason = %out.message,
                verdict = "call_worker_not_told", measured = true, n_considered = 1,
                "the worker that asked for the call could not be told it ended");
        } else {
            tracing::info!(target: "amux::calls", call = %id, worker = %worker, submission = out.submission,
                verdict = "call_worker_told", measured = true, n_considered = 1, "the worker was told its call ended");
        }
    }
    written.ok().map(|_| path_s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_normalize_to_e164_or_say_why_not() {
        assert_eq!(normalize_e164("+1 (555) 123-4567").unwrap(), "+15551234567");
        assert_eq!(normalize_e164("555.123.4567").unwrap(), "+15551234567");
        assert_eq!(normalize_e164("15551234567").unwrap(), "+15551234567");
        assert_eq!(normalize_e164("+44 20 7946 0958").unwrap(), "+442079460958");
        for bad in ["", "12345", "call me", "+1+5551234567", "0044207946", "+0123456789"] {
            assert!(normalize_e164(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_disclosure_is_always_the_first_sentence() {
        let i = instructions("Ethan Steininger", "book a table", "", 5);
        assert!(i.starts_with("You are an AI assistant calling on behalf of Ethan Steininger."));
        assert!(i.contains("under 5 minutes"));
        assert!(i.contains("Do not agree to any payment"));
        assert!(i.ends_with("say you will follow up, and end politely."));
    }

    #[test]
    fn only_a_local_bridge_may_be_given_the_local_address() {
        assert!(is_loopback_url("http://127.0.0.1:9000"));
        assert!(is_loopback_url("http://localhost/x"));
        assert!(is_loopback_url("http://[::1]:80"));
        assert!(!is_loopback_url("https://calls.amux.io"));
        if ["AMUX_CALL_RESULT_BASE", "AMUX_PUBLIC_URL"].iter().any(|k| std::env::var(k).is_ok_and(|v| !v.is_empty())) {
            eprintln!("SKIP result_base fallback: this process env already names a public address");
            return;
        }
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("endpoint.json"), r#"{"canonical_url":"https://localhost:8824"}"#).unwrap();
        assert_eq!(result_base(home.path(), "http://127.0.0.1:9000").as_deref(), Some("https://localhost:8824"));
        assert_eq!(result_base(home.path(), "https://calls.amux.io"), None);
    }

    #[test]
    fn a_token_matches_only_its_own_hash() {
        let a = mint_token().unwrap();
        let b = mint_token().unwrap();
        assert_ne!(a, b);
        assert!(constant_eq(&token_hash(&a), &token_hash(&a)));
        assert!(!constant_eq(&token_hash(&a), &token_hash(&b)));
    }
}
