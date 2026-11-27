//! The agent side of the CRM (docs/design/crm.md; owner 2026-11-23: "a very
//! agentic-oriented CRM ... fields that allow the agent to take action on ...
//! multi-touch ... LinkedIn, Twitter ... real time ... subscribe to a user ...
//! websites ... career posting ... the event subscription, and then obviously
//! we're using the amux harness for all of this").
//!
//! Channels: where a person or company can be reached and watched.
//!   GET    /api/crm/channels[?owner_kind=&owner_id=&watch=1]
//!   POST   /api/crm/channels            {owner_kind, owner_id, kind, handle?, url?, watch?, check_every_min?, how?}
//!   PATCH  /api/crm/channels/{id}, DELETE (soft)
//!   GET    /api/crm/channels/due        watched channels a worker checks (how=browser), due now
//!   POST   /api/crm/channels/{id}/observed   {items: [{title, url?}]}: what a check saw
//! Touches: one planned action on one channel.
//!   GET    /api/crm/touches[?status=&assignee=&contact_id=&account_id=]
//!   POST   /api/crm/touches             {contact_id|account_id, channel, action, body?, due_date?, assignee?, sequence?, step?}
//!   PATCH  /api/crm/touches/{id}        body, due date, assignee, result, status (not approved)
//!   POST   /api/crm/touches/{id}/approve   owner only
//!   POST   /api/crm/touches/{id}/done      {result?, result_url?, failed?}
//!
//! Activity: the CRM events themselves, newest first, as a worker's subscription sees them.
//!   GET    /api/crm/activity[?limit=&type=]
//!
//! Real time is amux's event subscriptions (api/event_subs.rs). The server
//! emits, as source `crm`, so any worker's subscription can match:
//!   crm.channel.changed   a watched channel shows something new (items added)
//!   crm.touch.approved    the owner approved a touch; its assignee does it
//!   crm.touch.done        a touch was done (or failed)
//!   crm.contact.replied   mail or a LinkedIn message from a person in the CRM
//!   crm.touch.due         an open touch's day has come (api/crm_outbound.rs)
//!   crm.signup            an amux Cloud order arrived or changed status (api/crm_signups.rs)
//!
//! Websites and careers pages are fetched by the server (runtime job crm-watch);
//! a social profile is checked by a worker through a signed-in browser route,
//! which reports through /observed, because those sites need a login and limit
//! automation. Mail and LinkedIn messages are captured into each person's
//! activity by the same job, so nobody types them in.
//!
//! Anything a person outside the company reads waits for the owner's approve
//! (the owner's standing rule: "Draft it and show me. I send it."). A worker can
//! plan, draft and record; only the owner bearer approves.

use super::calendar::query_rows_json;
use super::crm_accounts::{ev, now_secs, source_of};
use super::{internal, AppState};
use crate::db::board_store::next_issue_id;
use crate::db::WriteOutcome;
use amux_core::revision::MutationKind;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/channels", get(list_channels).post(create_channel))
        .route("/channels/due", get(channels_due))
        .route("/channels/{id}", axum::routing::patch(patch_channel).delete(delete_channel))
        .route("/channels/{id}/observed", post(observed))
        .route("/touches", get(list_touches).post(create_touch))
        .route("/touches/{id}", axum::routing::patch(patch_touch))
        .route("/touches/{id}/approve", post(approve_touch))
        .route("/touches/{id}/done", post(done_touch))
        .route("/activity", get(activity))
}

// Online and offline touch points (owner 2026-11-24: "it should support multiple
// touch points too offline and online"): a sequence can mix an email, a LinkedIn
// DM, a call, a letter by post and a meeting at an event.
pub const KINDS: &[&str] = &["linkedin", "x", "instagram", "website", "careers", "email", "phone", "youtube", "tiktok", "github",
    "sms", "whatsapp", "slack", "discord", "community", "post", "event", "in_person", "other"];
pub const ACTIONS: &[&str] = &["dm", "comment", "connect", "email", "followup", "follow", "like", "call", "note",
    "text", "voicemail", "letter", "gift", "meet", "invite", "intro", "visit", "watch"];
/// Touch points that happen off a screen: the owner or a worker records them done.
pub const OFFLINE: &[&str] = &["post", "event", "in_person"];
const STATUSES: &[&str] = &["planned", "drafted", "approved", "done", "skipped", "failed"];
/// Items kept from a check, to tell the next one what is new.
const KEEP_ITEMS: usize = 400;

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn slot<T>(s: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    s.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Who checks a channel by default: the server fetches public pages; a worker
/// checks anything behind a login.
pub fn default_how(kind: &str) -> &'static str {
    match kind {
        "website" | "careers" | "github" => "fetch",
        _ => "browser",
    }
}

// ---- channels ------------------------------------------------------------------

async fn list_channels(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        let mut sql = String::from(
            "SELECT ch.*, COALESCE(c.name, a.name, '') AS owner_name FROM crm_channels ch \
             LEFT JOIN crm_contacts c ON ch.owner_kind='contact' AND c.id=ch.owner_id \
             LEFT JOIN crm_accounts a ON ch.owner_kind='account' AND a.id=ch.owner_id \
             WHERE ch.deleted IS NULL",
        );
        let mut args: Vec<String> = Vec::new();
        for (k, col) in [("owner_kind", "ch.owner_kind"), ("owner_id", "ch.owner_id"), ("kind", "ch.kind")] {
            if let Some(v) = q.get(k).map(|s| s.trim()).filter(|s| !s.is_empty()) {
                args.push(v.to_string());
                sql.push_str(&format!(" AND {col}=?{}", args.len()));
            }
        }
        if q.get("watch").map(String::as_str) == Some("1") {
            sql.push_str(" AND ch.watch=1");
        }
        sql.push_str(" ORDER BY ch.owner_id, ch.kind");
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a as &dyn rusqlite::ToSql).collect();
        let mut rows = query_rows_json(&conn, &sql, &refs)?;
        for r in rows.iter_mut() {
            if let Some(o) = r.as_object_mut() {
                o.remove("last_items");
            }
        }
        Ok(rows)
    })
    .await;
    match joined {
        Ok(Ok(rows)) => Json(Value::Array(rows)).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

async fn create_channel(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let s = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let (owner_kind, owner_id, kind) = (s("owner_kind"), s("owner_id"), s("kind").to_lowercase());
    if !matches!(owner_kind.as_str(), "contact" | "account") || owner_id.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "send owner_kind (contact or account) and owner_id" }));
    }
    if !KINDS.contains(&kind.as_str()) {
        return err(StatusCode::BAD_REQUEST, json!({ "error": format!("kind must be one of {}", KINDS.join(", ")) }));
    }
    let (handle, url) = (s("handle"), s("url"));
    if handle.is_empty() && url.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "send a handle or a url" }));
    }
    let watch = body.get("watch").and_then(Value::as_bool).unwrap_or(false);
    if watch && url.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "a watched channel needs a url to check" }));
    }
    let how = match s("how").as_str() {
        "" => default_how(&kind).to_string(),
        h @ ("fetch" | "browser") => h.to_string(),
        _ => return err(StatusCode::BAD_REQUEST, json!({ "error": "how is fetch or browser" })),
    };
    let every = body.get("check_every_min").and_then(Value::as_i64).unwrap_or(360).clamp(15, 10_080);
    let how_stored = how.clone();
    let out: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let w = out.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let id = next_issue_id(conn, "CH")?;
            let now = now_secs();
            conn.execute(
                "INSERT INTO crm_channels (id,owner_kind,owner_id,kind,handle,url,watch,check_every_min,how,created,updated) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10)",
                rusqlite::params![&id, &owner_kind, &owner_id, &kind, &handle, &url, watch as i64, every, &how, now],
            )?;
            *slot(&w) = Some(id.clone());
            Ok(WriteOutcome { applied: true, events: vec![ev(if owner_kind == "account" { "crm_account" } else { "crm_contact" }, &owner_id, MutationKind::Updated)] })
        })
        .await;
    match res {
        Ok(_) => (StatusCode::CREATED, Json(json!({ "id": slot(&out).take(), "ok": true, "how": how_stored }))).into_response(),
        Err(e) => internal(e),
    }
}

async fn patch_channel(State(state): State<AppState>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let mut sets: Vec<(&'static str, SqlValue)> = Vec::new();
    for f in ["handle", "url"] {
        if let Some(v) = body.get(f).and_then(Value::as_str) {
            sets.push((f, SqlValue::Text(v.trim().to_string())));
        }
    }
    if let Some(b) = body.get("watch").and_then(Value::as_bool) {
        sets.push(("watch", SqlValue::Integer(b as i64)));
    }
    if let Some(n) = body.get("check_every_min").and_then(Value::as_i64) {
        sets.push(("check_every_min", SqlValue::Integer(n.clamp(15, 10_080))));
    }
    if let Some(h) = body.get("how").and_then(Value::as_str) {
        if !matches!(h, "fetch" | "browser") {
            return err(StatusCode::BAD_REQUEST, json!({ "error": "how is fetch or browser" }));
        }
        sets.push(("how", SqlValue::Text(h.into())));
    }
    if sets.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "nothing to change" }));
    }
    let res = state
        .store
        .write_async(move |conn| {
            let mut params: Vec<SqlValue> = sets.iter().map(|(_, v)| v.clone()).collect();
            let set_sql: Vec<String> = sets.iter().enumerate().map(|(i, (f, _))| format!("{f}=?{}", i + 1)).collect();
            params.push(SqlValue::Integer(now_secs()));
            params.push(SqlValue::Text(id.clone()));
            let n = conn.execute(
                &format!("UPDATE crm_channels SET {}, updated=?{} WHERE id=?{} AND deleted IS NULL", set_sql.join(", "), params.len() - 1, params.len()),
                rusqlite::params_from_iter(params.iter()),
            )?;
            Ok(WriteOutcome { applied: n > 0, events: if n > 0 { vec![ev("crm_channel", &id, MutationKind::Updated)] } else { vec![] } })
        })
        .await;
    match res {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

async fn delete_channel(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let res = state
        .store
        .write_async(move |conn| {
            let n = conn.execute("UPDATE crm_channels SET deleted=?1 WHERE id=?2 AND deleted IS NULL", rusqlite::params![now_secs(), &id])?;
            Ok(WriteOutcome { applied: n > 0, events: if n > 0 { vec![ev("crm_channel", &id, MutationKind::Deleted)] } else { vec![] } })
        })
        .await;
    match res {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

/// Watched channels whose check is due, for the checker `how` (fetch or browser).
pub(crate) fn due_channels(conn: &rusqlite::Connection, how: &str, now: i64) -> rusqlite::Result<Vec<Value>> {
    query_rows_json(
        conn,
        "SELECT ch.id, ch.owner_kind, ch.owner_id, ch.kind, ch.handle, ch.url, ch.check_every_min, ch.last_checked_at, \
                COALESCE(c.name, a.name, '') AS owner_name FROM crm_channels ch \
         LEFT JOIN crm_contacts c ON ch.owner_kind='contact' AND c.id=ch.owner_id \
         LEFT JOIN crm_accounts a ON ch.owner_kind='account' AND a.id=ch.owner_id \
         WHERE ch.deleted IS NULL AND ch.watch=1 AND ch.url<>'' AND ch.how=?1 \
           AND (ch.last_checked_at IS NULL OR ch.last_checked_at + ch.check_every_min*60 <= ?2) \
         ORDER BY COALESCE(ch.last_checked_at, 0) LIMIT 50",
        &[&how as &dyn rusqlite::ToSql, &now],
    )
}

async fn channels_due(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let how = q.get("how").cloned().unwrap_or_else(|| "browser".into());
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> { Ok(due_channels(&*store.read()?, &how, now_secs())?) }).await;
    match joined {
        Ok(Ok(rows)) => Json(json!({ "due": rows, "measured": true, "n_considered": rows.len(),
            "next": "check each url, then POST /api/crm/channels/<id>/observed {items:[{title,url}]}" })).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

/// One check of a channel: what it showed. Returns the event to emit when
/// something is new (never on the first check, which only records).
pub(crate) fn record_observation(conn: &rusqlite::Connection, id: &str, items: &[Value], error: &str, now: i64) -> rusqlite::Result<Option<Value>> {
    let row: Option<(String, String, String, String, String, String)> = conn
        .query_row(
            "SELECT ch.owner_kind, ch.owner_id, ch.kind, ch.url, ch.last_items, COALESCE(c.name, a.name, '') FROM crm_channels ch \
             LEFT JOIN crm_contacts c ON ch.owner_kind='contact' AND c.id=ch.owner_id \
             LEFT JOIN crm_accounts a ON ch.owner_kind='account' AND a.id=ch.owner_id \
             WHERE ch.id=?1 AND ch.deleted IS NULL",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .ok();
    let Some((owner_kind, owner_id, kind, url, last, owner_name)) = row else { return Ok(None) };
    if !error.is_empty() {
        conn.execute("UPDATE crm_channels SET last_checked_at=?1, last_error=?2 WHERE id=?3", rusqlite::params![now, error, id])?;
        return Ok(None);
    }
    let key = |v: &Value| -> String {
        v.get("url").and_then(Value::as_str).filter(|u| !u.is_empty())
            .or_else(|| v.get("title").and_then(Value::as_str))
            .unwrap_or("").trim().to_string()
    };
    let items: Vec<Value> = items.iter().filter(|v| !key(v).is_empty()).take(KEEP_ITEMS).cloned().collect();
    let prev: Vec<Value> = serde_json::from_str(&last).unwrap_or_default();
    let first = conn.query_row("SELECT last_checked_at IS NULL FROM crm_channels WHERE id=?1", [id], |r| r.get::<_, bool>(0))?;
    let seen: HashSet<String> = prev.iter().map(key).collect();
    let added: Vec<Value> = items.iter().filter(|v| !seen.contains(&key(v))).cloned().collect();
    let hash = {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(Value::Array(items.clone()).to_string().as_bytes()))
    };
    conn.execute(
        "UPDATE crm_channels SET last_checked_at=?1, last_error='', last_hash=?2, last_items=?3, \
         last_changed_at=CASE WHEN ?4 THEN ?1 ELSE last_changed_at END WHERE id=?5",
        rusqlite::params![now, hash, Value::Array(items.clone()).to_string(), !first && !added.is_empty(), id],
    )?;
    if first || added.is_empty() {
        return Ok(None);
    }
    let summary: Vec<String> = added.iter().take(5).map(|v| v.get("title").and_then(Value::as_str).unwrap_or_else(|| v.get("url").and_then(Value::as_str).unwrap_or("")).chars().take(160).collect()).collect();
    // The person's (or company's) timeline shows it too.
    if owner_kind == "account" {
        conn.execute(
            "INSERT OR IGNORE INTO crm_account_activity (id,account_id,date,type,direction,subject,notes,message_id,source,created) VALUES (?1,?2,?3,'signal','in',?4,?5,?6,'crm-watch',?7)",
            rusqlite::params![ulid::Ulid::new().to_string(), &owner_id, chrono::Local::now().format("%Y-%m-%d").to_string(),
                format!("{kind}: {} new", added.len()), summary.join("\n"), format!("signal:{id}:{hash}"), now],
        )?;
    }
    if owner_kind == "contact" {
        conn.execute(
            "INSERT OR IGNORE INTO crm_interactions (id,contact_id,date,type,notes,created,updated,direction,subject,message_id) VALUES (?1,?2,?3,'signal',?4,?5,?5,'in',?6,?7)",
            rusqlite::params![ulid::Ulid::new().to_string(), &owner_id, chrono::Local::now().format("%Y-%m-%d").to_string(),
                summary.join("\n"), now, format!("{kind}: {} new", added.len()), format!("signal:{id}:{hash}")],
        )?;
    }
    Ok(Some(json!({
        "channel_id": id, "kind": kind, "owner_kind": owner_kind, "owner_id": owner_id, "owner_name": owner_name,
        "url": url, "added": added.iter().take(20).collect::<Vec<_>>(), "n_added": added.len(), "summary": summary.join(" | "),
    })))
}

async fn observed(State(state): State<AppState>, Path(id): Path<String>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let items: Vec<Value> = body.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let error = body.get("error").and_then(Value::as_str).unwrap_or("").to_string();
    let by = source_of(&json!({}), &headers);
    let out: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let w = out.clone();
    let id2 = id.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let ev_data = record_observation(conn, &id2, &items, &error, now_secs())?;
            let changed = ev_data.is_some();
            *slot(&w) = ev_data;
            Ok(WriteOutcome { applied: true, events: if changed { vec![ev("crm_channel", &id2, MutationKind::Updated)] } else { vec![] } })
        })
        .await;
    if let Err(e) = res {
        return internal(e);
    }
    let ev_data = slot(&out).take();
    if let Some(mut d) = ev_data.clone() {
        d["checked_by"] = json!(by);
        emit(&state, "crm.channel.changed", d).await;
    }
    Json(json!({ "ok": true, "changed": ev_data.is_some(), "n_added": ev_data.as_ref().and_then(|d| d.get("n_added")).cloned().unwrap_or(json!(0)) })).into_response()
}

/// A crm.* event, emitted by the server as source `crm` so any worker's
/// subscription can match it.
pub(crate) async fn emit(state: &AppState, etype: &str, data: Value) {
    let idem = format!("{etype}:{}", ulid::Ulid::new());
    crate::api::session_verbs::emit_event_store(&state.store, "crm", etype, Some(data), Some(idem), "crm").await;
    tracing::info!(target: "amux::crm", etype, measured = true, n_considered = 1, verdict = "crm_event_emitted", "a CRM event was emitted");
}

// ---- touches -------------------------------------------------------------------------

async fn list_touches(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        let mut sql = String::from(
            "SELECT t.*, COALESCE(c.name,'') AS contact_name, COALESCE(a.name, ca.name, c.company, '') AS account_name, \
             COALESCE(c.icp_grade, a.icp_grade, '') AS icp_grade, COALESCE(c.email,'') AS contact_email, \
             c.unsubscribed_at AS contact_unsubscribed_at, c.bounced_at AS contact_bounced_at, COALESCE(c.status,'') AS contact_status, COALESCE(c.email_check,'') AS contact_email_check FROM crm_touches t \
             LEFT JOIN crm_contacts c ON c.id=t.contact_id LEFT JOIN crm_accounts a ON a.id=t.account_id \
             LEFT JOIN crm_accounts ca ON ca.id=c.account_id WHERE t.deleted IS NULL",
        );
        let mut args: Vec<String> = Vec::new();
        for (k, col) in [("status", "t.status"), ("assignee", "t.assignee"), ("contact_id", "t.contact_id"), ("account_id", "t.account_id"), ("channel", "t.channel")] {
            if let Some(v) = q.get(k).map(|s| s.trim()).filter(|s| !s.is_empty()) {
                args.push(v.to_string());
                sql.push_str(&format!(" AND {col}=?{}", args.len()));
            }
        }
        if q.get("open").map(String::as_str) == Some("1") {
            sql.push_str(" AND t.status IN ('planned','drafted','approved')");
        }
        sql.push_str(" ORDER BY CASE t.status WHEN 'drafted' THEN 0 WHEN 'approved' THEN 1 WHEN 'planned' THEN 2 ELSE 3 END, CASE WHEN t.due_date='' THEN '9999' ELSE t.due_date END, t.created");
        // A cap only when asked: the person and company views want every touch.
        if let Some(n) = q.get("limit").and_then(|v| v.parse::<i64>().ok()).filter(|n| *n > 0) {
            sql.push_str(&format!(" LIMIT {}", n.min(2000)));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a as &dyn rusqlite::ToSql).collect();
        Ok(query_rows_json(&conn, &sql, &refs)?)
    })
    .await;
    match joined {
        Ok(Ok(rows)) => Json(Value::Array(rows)).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

async fn create_touch(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let s = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let (contact, account, channel, action) = (s("contact_id"), s("account_id"), s("channel").to_lowercase(), s("action").to_lowercase());
    if contact.is_empty() && account.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "send contact_id or account_id" }));
    }
    if !KINDS.contains(&channel.as_str()) || !ACTIONS.contains(&action.as_str()) {
        return err(StatusCode::BAD_REQUEST, json!({ "error": format!("channel is one of {}; action one of {}", KINDS.join(", "), ACTIONS.join(", ")) }));
    }
    let source = source_of(&body, &headers);
    // A worker that makes a touch is its assignee unless it names another.
    let assignee = { let a = s("assignee"); if a.is_empty() { source.strip_prefix("worker:").unwrap_or("").to_string() } else { a } };
    let (text, target, due, sequence) = (s("body"), s("target_url"), s("due_date"), s("sequence"));
    let step = body.get("step").and_then(Value::as_i64).unwrap_or(0);
    let status = if text.is_empty() { "planned" } else { "drafted" };
    let force = body.get("force").and_then(Value::as_bool) == Some(true);
    // The voice (api/crm_voice.rs): a text is checked when it is written.
    let voice_check = if text.is_empty() { None } else {
        let mut c = super::crm_voice::check_touch(&state, &s("voice"), &channel, &action, step.max(1), &text).await;
        c["slop"] = json!(body.get("slop").and_then(Value::as_bool) == Some(true));
        if let Some(r) = super::crm_voice::refusal(&mut c, super::standing_approvals::is_owner_request(&headers), &s("voice_override"), "new") {
            return r;
        }
        Some(c.to_string())
    };
    let out: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let w = out.clone();
    let refused: Arc<Mutex<Option<super::crm_outbound::Refusal>>> = Arc::new(Mutex::new(None));
    let wr = refused.clone();
    let res = state
        .store
        .write_async(move |conn| {
            // Effective and safe outbound (api/crm_outbound.rs): nobody who
            // opted out, no second open touch on a channel, no pile-up.
            if let Some(g) = super::crm_outbound::guard(conn, &contact, &channel, &action, &sequence, force) {
                *slot(&wr) = Some(g);
                return Ok(WriteOutcome { applied: false, events: vec![] });
            }
            let id = next_issue_id(conn, "TCH")?;
            let now = now_secs();
            conn.execute(
                "INSERT INTO crm_touches (id,contact_id,account_id,channel,action,body,target_url,due_date,assignee,status,sequence,step,source,created,updated,voice_check) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?14,?15)",
                rusqlite::params![&id, &contact, &account, &channel, &action, &text, &target, &due, &assignee, status,
                    &sequence, step, &source, now, &voice_check],
            )?;
            *slot(&w) = Some(id.clone());
            let owner = if contact.is_empty() { ("crm_account", account.clone()) } else { ("crm_contact", contact.clone()) };
            Ok(WriteOutcome { applied: true, events: vec![ev("crm_touch", &id, MutationKind::Created), ev(owner.0, &owner.1, MutationKind::Updated)] })
        })
        .await;
    if let Err(e) = res {
        return internal(e);
    }
    if let Some((verdict, why, existing)) = slot(&refused).take() {
        tracing::info!(target: "amux::crm", existing = %existing, measured = true, n_considered = 1, verdict, "an outward touch was refused: {why}");
        return err(StatusCode::CONFLICT, json!({ "error": why, "code": verdict, "existing": existing }));
    }
    (StatusCode::CREATED, Json(json!({ "id": slot(&out).take(), "ok": true, "status": status }))).into_response()
}

/// The touch as a JSON row with the person's and company's names.
fn touch_row(conn: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<Value>> {
    Ok(query_rows_json(conn,
        "SELECT t.*, COALESCE(c.name,'') AS contact_name, COALESCE(a.name,'') AS account_name FROM crm_touches t \
         LEFT JOIN crm_contacts c ON c.id=t.contact_id LEFT JOIN crm_accounts a ON a.id=t.account_id WHERE t.id=?1 AND t.deleted IS NULL",
        &[&id as &dyn rusqlite::ToSql])?.into_iter().next())
}

async fn patch_touch(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let mut sets: Vec<(&'static str, SqlValue)> = Vec::new();
    let owner = super::standing_approvals::is_owner_request(&headers);
    // A new text is checked against the voice before it is stored; the
    // owner's edit of a draft is kept as an example of the voice.
    let new_text = body.get("body").and_then(Value::as_str).filter(|b| !b.trim().is_empty()).map(str::to_string);
    let mut voice_id = String::new();
    if let Some(text) = &new_text {
        let store = state.store.clone();
        let tid = id.clone();
        let cur = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<(String, String, i64, String)>> {
            Ok(store.read()?.query_row(
                "SELECT channel, action, step, COALESCE(json_extract(voice_check,'$.voice'),'') FROM crm_touches WHERE id=?1 AND deleted IS NULL",
                [&tid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).ok())
        }).await;
        if let Ok(Ok(Some((ch, ac, step, vid)))) = cur {
            let hint = body.get("voice").and_then(Value::as_str).unwrap_or(&vid).to_string();
            let mut c = super::crm_voice::check_touch(&state, &hint, &ch, &ac, step.max(1), text).await;
            c["slop"] = json!(body.get("slop").and_then(Value::as_bool) == Some(true));
            let why = body.get("voice_override").and_then(Value::as_str).unwrap_or("").trim().to_string();
            if let Some(r) = super::crm_voice::refusal(&mut c, owner, &why, &id) {
                return r;
            }
            voice_id = c["voice"].as_str().unwrap_or("").to_string();
            c["by"] = json!(if owner { "owner" } else { "worker" });
            sets.push(("voice_check", SqlValue::Text(c.to_string())));
        }
    }
    // `result` carries why the owner declined a draft (Skip with a note), which its worker reads.
    for f in ["body", "due_date", "assignee", "target_url", "sequence", "result"] {
        if let Some(v) = body.get(f).and_then(Value::as_str) {
            sets.push((f, SqlValue::Text(v.to_string())));
        }
    }
    if let Some(st) = body.get("status").and_then(Value::as_str) {
        // Approval has its own route, owner only; done has its own too.
        if !matches!(st, "planned" | "drafted" | "skipped") || !STATUSES.contains(&st) {
            return err(StatusCode::BAD_REQUEST, json!({ "error": "status here is planned, drafted or skipped; approve and done have their own routes" }));
        }
        sets.push(("status", SqlValue::Text(st.into())));
    } else if body.get("body").and_then(Value::as_str).is_some_and(|b| !b.trim().is_empty()) {
        // A draft written to a planned touch makes it a draft; an approved one
        // whose text changes goes back for approval.
        sets.push(("status", SqlValue::Text("drafted".into())));
    }
    if sets.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "nothing to change" }));
    }
    let res = state
        .store
        .write_async(move |conn| {
            if let (true, Some(after)) = (owner, &new_text) {
                let prev: Option<(String, String, String, i64)> = conn
                    .query_row("SELECT body, status, channel, action, step FROM crm_touches WHERE id=?1 AND deleted IS NULL", [&id],
                        |r| Ok((r.get(0)?, r.get(1)?, format!("{}/{}", r.get::<_, String>(2)?, r.get::<_, String>(3)?), r.get(4)?))).ok();
                if let Some((before, _, key, step)) = prev.filter(|p| matches!(p.1.as_str(), "drafted" | "approved") && !p.0.trim().is_empty()) {
                    let (ch, ac) = key.split_once('/').unwrap_or((&key, ""));
                    super::crm_voice::keep_example(conn, &voice_id, &super::crm_voice::key_of(ch, ac, step.max(1)), "owner_edit", &before, after, &id)?;
                }
            }
            let mut params: Vec<SqlValue> = sets.iter().map(|(_, v)| v.clone()).collect();
            let set_sql: Vec<String> = sets.iter().enumerate().map(|(i, (f, _))| format!("{f}=?{}", i + 1)).collect();
            params.push(SqlValue::Integer(now_secs()));
            params.push(SqlValue::Text(id.clone()));
            let n = conn.execute(
                &format!("UPDATE crm_touches SET {}, updated=?{} WHERE id=?{} AND deleted IS NULL AND status NOT IN ('done','failed')",
                    set_sql.join(", "), params.len() - 1, params.len()),
                rusqlite::params_from_iter(params.iter()),
            )?;
            Ok(WriteOutcome { applied: n > 0, events: if n > 0 { vec![ev("crm_touch", &id, MutationKind::Updated)] } else { vec![] } })
        })
        .await;
    match res {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

async fn approve_touch(State(state): State<AppState>, OriginalUri(uri): OriginalUri, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = super::land_approvals::owner_only(&state, &headers, &uri, "crm_touch_approve_refused", "approve an outreach touch") {
        return *r;
    }
    // An email that would go to an unchecked address is checked first (plan
    // section 15); the guard below refuses it when the check failed.
    let unchecked: Option<String> = {
        let store = state.store.clone();
        let tid = id.clone();
        tokio::task::spawn_blocking(move || -> Option<String> {
            store.read().ok()?.query_row(
                "SELECT t.contact_id FROM crm_touches t JOIN crm_contacts c ON c.id=t.contact_id WHERE t.id=?1 AND t.channel='email' AND c.email_check=''",
                [&tid], |r| r.get(0)).ok()
        }).await.ok().flatten()
    };
    if let Some(c) = unchecked {
        super::crm_close::check_and_store(&state, &c).await;
    }
    let out: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let w = out.clone();
    let refused: Arc<Mutex<Option<super::crm_outbound::Refusal>>> = Arc::new(Mutex::new(None));
    let wr = refused.clone();
    let res = state
        .store
        .write_async(move |conn| {
            // The person may have opted out since the draft was written.
            let who: Option<(String, String, String)> = conn
                .query_row("SELECT contact_id, channel, action FROM crm_touches WHERE id=?1", [&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .ok();
            if let Some((c, ch, ac)) = who {
                if let Some(g) = super::crm_outbound::guard(conn, &c, &ch, &ac, "", true) {
                    *slot(&wr) = Some(g);
                    return Ok(WriteOutcome { applied: false, events: vec![] });
                }
            }
            let n = conn.execute(
                "UPDATE crm_touches SET status='approved', approved_by='owner', approved_at=?1, updated=?1 WHERE id=?2 AND deleted IS NULL AND status IN ('planned','drafted')",
                rusqlite::params![now_secs(), &id],
            )?;
            *slot(&w) = if n > 0 { touch_row(conn, &id)? } else { None };
            // What the owner approves is the voice too.
            if let Some(t) = slot(&w).as_ref() {
                let s = |k: &str| t[k].as_str().unwrap_or("").to_string();
                let vid = serde_json::from_str::<Value>(&s("voice_check")).ok()
                    .and_then(|v| v["voice"].as_str().map(String::from))
                    .or_else(|| super::crm_voice::voice_for(conn, "").map(|v| v.0)).unwrap_or_default();
                let key = super::crm_voice::key_of(&s("channel"), &s("action"), t["step"].as_i64().unwrap_or(1).max(1));
                super::crm_voice::keep_example(conn, &vid, &key, "approved", "", &s("body"), &id)?;
            }
            Ok(WriteOutcome { applied: n > 0, events: if n > 0 { vec![ev("crm_touch", &id, MutationKind::Updated)] } else { vec![] } })
        })
        .await;
    if let Err(e) = res {
        return internal(e);
    }
    if let Some((verdict, why, _)) = slot(&refused).take() {
        tracing::info!(target: "amux::crm", measured = true, n_considered = 1, verdict, "an approval was refused: {why}");
        return err(StatusCode::CONFLICT, json!({ "error": why, "code": verdict }));
    }
    let row = slot(&out).take();
    match row {
        Some(t) => {
            emit(&state, "crm.touch.approved", t.clone()).await;
            Json(json!({ "ok": true, "touch": t })).into_response()
        }
        None => err(StatusCode::CONFLICT, json!({ "error": "no planned or drafted touch with that id", "code": "not_approvable" })),
    }
}

async fn done_touch(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let failed = body.get("failed").and_then(Value::as_bool).unwrap_or(false);
    let result = body.get("result").and_then(Value::as_str).unwrap_or("").to_string();
    let url = body.get("result_url").and_then(Value::as_str).unwrap_or("").to_string();
    let by = source_of(&json!({}), &headers);
    let out: Arc<Mutex<Option<Result<Value, &'static str>>>> = Arc::new(Mutex::new(None));
    let w = out.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let cur: Option<(String, String, String, String, String, String)> = conn.query_row(
                "SELECT status, action, channel, contact_id, body, account_id FROM crm_touches WHERE id=?1 AND deleted IS NULL", [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).ok();
            let Some((status, action, channel, contact, text, account)) = cur else { *slot(&w) = Some(Err("not_found")); return Ok(WriteOutcome { applied: false, events: vec![] }) };
            // Reaching someone outside needs the owner's approval first; a note
            // or a follow is the worker's own business.
            let outward = super::crm_outbound::outward(&action);
            if !failed && outward && status != "approved" {
                *slot(&w) = Some(Err("not_approved"));
                return Ok(WriteOutcome { applied: false, events: vec![] });
            }
            let now = now_secs();
            conn.execute(
                "UPDATE crm_touches SET status=?1, done_at=?2, result=?3, result_url=?4, updated=?2 WHERE id=?5",
                rusqlite::params![if failed { "failed" } else { "done" }, now, &result, &url, &id],
            )?;
            if !failed && !contact.is_empty() {
                conn.execute(
                    "INSERT OR IGNORE INTO crm_interactions (id,contact_id,date,type,notes,created,updated,direction,subject,message_id) VALUES (?1,?2,?3,?4,?5,?6,?6,'out',?7,?8)",
                    rusqlite::params![ulid::Ulid::new().to_string(), &contact, chrono::Local::now().format("%Y-%m-%d").to_string(),
                        if channel == "email" { "email" } else { "dm" }, if result.is_empty() { text.clone() } else { format!("{text}\n\n{result}") },
                        now, format!("{channel} {action}"), format!("touch:{id}")],
                )?;
            }
            // Its sequence's next step is planned now, due its days later.
            let next = if failed { None } else { super::crm_outbound::advance(conn, &id)? };
            let mut row = touch_row(conn, &id)?.unwrap_or(json!({ "id": id }));
            row["next_touch"] = json!(next);
            *slot(&w) = Some(Ok(row));
            // The touch's owner refreshes: the person, or the company for a company touch.
            let owner = if contact.is_empty() { ev("crm_account", &account, MutationKind::Updated) } else { ev("crm_contact", &contact, MutationKind::Updated) };
            Ok(WriteOutcome { applied: true, events: vec![ev("crm_touch", &id, MutationKind::Updated), owner] })
        })
        .await;
    if let Err(e) = res {
        return internal(e);
    }
    let taken = slot(&out).take();
    match taken {
        Some(Ok(mut t)) => {
            t["done_by"] = json!(by);
            emit(&state, "crm.touch.done", t.clone()).await;
            Json(json!({ "ok": true, "touch": t })).into_response()
        }
        Some(Err("not_approved")) => {
            tracing::warn!(target: "amux::crm", measured = true, n_considered = 1, verdict = "crm_touch_done_unapproved", "a worker marked an outward touch done before the owner approved it");
            err(StatusCode::CONFLICT, json!({ "error": "this touch reaches someone outside and the owner has not approved it", "code": "not_approved",
                "next": "draft it (PATCH body), the owner approves it in the CRM tab, then do it; a crm.touch.approved event says when" }))
        }
        _ => err(StatusCode::NOT_FOUND, json!({ "error": "not found" })),
    }
}

// ---- watch and capture (runtime job crm-watch) ---------------------------------------------

/// The visible text of an HTML page, one meaningful line per item.
pub(crate) fn page_items(html: &str) -> Vec<Value> {
    let mut s = String::with_capacity(html.len());
    // ASCII lowering keeps every byte offset, so `lower[i..]` and `html[i..]`
    // stay aligned on any text (Unicode lowering changes lengths: 'İ' is 2
    // bytes, its lowercase 3).
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if lower[i..].starts_with("<script") || lower[i..].starts_with("<style") || lower[i..].starts_with("<noscript") {
            let close = if lower[i..].starts_with("<script") { "</script>" } else if lower[i..].starts_with("<style") { "</style>" } else { "</noscript>" };
            i = lower[i..].find(close).map(|j| i + j + close.len()).unwrap_or(bytes.len());
            continue;
        }
        if bytes[i] == b'<' {
            let end = html[i..].find('>').map(|j| i + j + 1).unwrap_or(bytes.len());
            let tag = &lower[i..end];
            if ["<br", "<p", "</p", "<div", "</div", "<li", "<h1", "<h2", "<h3", "<h4", "<tr", "<a ", "</a", "<td", "<section", "<article"].iter().any(|t| tag.starts_with(t)) {
                s.push('\n');
            }
            i = end;
            continue;
        }
        let ch = html[i..].chars().next().unwrap_or(' ');
        s.push(ch);
        i += ch.len_utf8();
    }
    let s = s.replace("&amp;", "&").replace("&nbsp;", " ").replace("&#39;", "'").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">");
    let mut seen = HashSet::new();
    s.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| l.chars().count() >= 4 && l.chars().count() <= 300 && seen.insert(l.clone()))
        .take(KEEP_ITEMS)
        .map(|l| json!({ "title": l }))
        .collect()
}

/// One pass: fetch each due public page, and capture new mail and LinkedIn
/// messages into the people they came from.
pub(crate) async fn tick(state: &AppState) {
    let store = state.store.clone();
    let due = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> { Ok(due_channels(&*store.read()?, "fetch", now_secs())?) }).await;
    let due = match due {
        Ok(Ok(d)) => d,
        other => {
            tracing::warn!(target: "amux::crm", measured = false, n_considered = 0, verdict = "crm_watch_unreadable", "{other:?}");
            vec![]
        }
    };
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).user_agent("Mozilla/5.0 (amux CRM watch)").build();
    for ch in &due {
        let id = ch["id"].as_str().unwrap_or("").to_string();
        let url = ch["url"].as_str().unwrap_or("").to_string();
        let (items, error) = match &client {
            Ok(c) => match c.get(&url).send().await {
                Ok(r) if r.status().is_success() => match r.text().await {
                    Ok(t) => (page_items(&t.chars().take(2_000_000).collect::<String>()), String::new()),
                    Err(e) => (vec![], format!("read failed: {e}")),
                },
                Ok(r) => (vec![], format!("HTTP {}", r.status())),
                Err(e) => (vec![], format!("fetch failed: {e}")),
            },
            Err(e) => (vec![], format!("no http client: {e}")),
        };
        let (id2, err2) = (id.clone(), error.clone());
        let ev_data = state
            .store
            .write_async(move |conn| {
                let d = record_observation(conn, &id2, &items, &err2, now_secs())?;
                *slot(&CHANGED) = d;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .ok()
            .and_then(|_| slot(&CHANGED).take());
        if !error.is_empty() {
            tracing::info!(target: "amux::crm", channel = %id, url = %url, error = %error, measured = true, n_considered = 1, verdict = "crm_watch_fetch_failed", "a watched page could not be read");
        }
        if let Some(mut d) = ev_data {
            d["checked_by"] = json!("crm-watch");
            emit(state, "crm.channel.changed", d).await;
        }
    }
    capture(state).await;
    super::crm_outbound::announce_due(state).await;
    super::crm_voice::recheck_planned(state).await;
    super::crm_signups::pull(state).await;
}

static CHANGED: Mutex<Option<Value>> = Mutex::new(None);

/// New email.received and linkedin.message_received events, and new rows of
/// the sent-mail log, become activity on the person they are from or to.
async fn capture(state: &AppState) {
    let home = crate::config::amux_home();
    let sent_log = crate::integrations::email::email_log_path(&home);
    let res = state
        .store
        .write_async(move |conn| {
            let (n_in, replied) = capture_events(conn)?;
            let n_out = capture_sent(conn, &sent_log)?;
            *slot(&REPLIED) = replied;
            if n_in + n_out > 0 {
                tracing::info!(target: "amux::crm", captured_in = n_in, captured_out = n_out, measured = true, n_considered = n_in + n_out,
                    verdict = "crm_capture", "mail and messages attached to people in the CRM");
            }
            Ok(WriteOutcome { applied: n_in + n_out > 0, events: vec![] })
        })
        .await;
    if let Err(e) = res {
        tracing::warn!(target: "amux::crm", error = %e, measured = false, n_considered = 0, verdict = "crm_capture_failed", "capture could not write");
        return;
    }
    let replied = std::mem::take(&mut *slot(&REPLIED));
    for r in replied {
        emit(state, "crm.contact.replied", r).await;
    }
}

static REPLIED: Mutex<Vec<Value>> = Mutex::new(Vec::new());

fn state_get(conn: &rusqlite::Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM crm_capture_state WHERE key=?1", [key], |r| r.get(0)).ok()
}

fn state_set(conn: &rusqlite::Connection, key: &str, v: &str) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO crm_capture_state (key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [key, v])?;
    Ok(())
}

/// Inbound: mail from a person's address, a LinkedIn message from their exact
/// name (one match only). Starts at the ledger's end the first time, so history
/// is not replayed.
pub(crate) fn capture_events(conn: &rusqlite::Connection) -> rusqlite::Result<(usize, Vec<Value>)> {
    let cursor: i64 = match state_get(conn, "events_id").and_then(|v| v.parse().ok()) {
        Some(c) => c,
        None => {
            let top: i64 = conn.query_row("SELECT COALESCE(MAX(id),0) FROM session_events", [], |r| r.get(0))?;
            state_set(conn, "events_id", &top.to_string())?;
            return Ok((0, vec![]));
        }
    };
    let mut st = conn.prepare(
        "SELECT id, type, data FROM session_events WHERE id>?1 AND type IN ('email.received','linkedin.message_received') ORDER BY id LIMIT 500",
    )?;
    let rows: Vec<(i64, String, String)> = st.query_map([cursor], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<String>>(2)?.unwrap_or_default())))?.filter_map(Result::ok).collect();
    let mut last = cursor;
    let (mut n, mut replied) = (0, Vec::new());
    for (id, ty, data) in rows {
        last = id;
        let d: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        let (contact, name, channel, subject, snippet, msg) = if ty == "email.received" {
            if d.get("bulk").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let addr = d.get("from_address").and_then(Value::as_str).unwrap_or("").to_lowercase();
            let Some((cid, name)) = conn.query_row("SELECT id,name FROM crm_contacts WHERE lower(email)=?1 AND deleted IS NULL", [&addr], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).ok() else { continue };
            // Gmail's snippet is HTML-escaped (`we&#39;re`, `&lt;ethan@...&gt;`).
            (cid, name, "email", unescape_html(d.get("subject").and_then(Value::as_str).unwrap_or("")),
                unescape_html(d.get("snippet").and_then(Value::as_str).unwrap_or("")), d.get("message_id").and_then(Value::as_str).unwrap_or("").to_string())
        } else {
            let who = d.get("from_name").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
            if who.is_empty() || d.get("sponsored").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let mut q = conn.prepare("SELECT id,name FROM crm_contacts WHERE lower(name)=?1 AND deleted IS NULL LIMIT 2")?;
            let hits: Vec<(String, String)> = q.query_map([&who], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(Result::ok).collect();
            if hits.len() != 1 {
                continue;
            }
            let (cid, name) = hits[0].clone();
            (cid, name, "linkedin", "LinkedIn message".to_string(), unescape_html(d.get("snippet").and_then(Value::as_str).unwrap_or("")),
                format!("linkedin:{}", d.get("key").and_then(Value::as_str).unwrap_or("")))
        };
        let ix = ulid::Ulid::new().to_string();
        let added = conn.execute(
            "INSERT OR IGNORE INTO crm_interactions (id,contact_id,date,type,notes,created,updated,direction,subject,message_id) VALUES (?1,?2,?3,?4,?5,?6,?6,'in',?7,?8)",
            rusqlite::params![&ix, &contact, chrono::Local::now().format("%Y-%m-%d").to_string(),
                if channel == "email" { "email" } else { "dm" }, &snippet, now_secs(), &subject, &msg],
        )?;
        if added > 0 {
            n += 1;
            // A reply stops the sequences that stop on one, and a request to stop is honoured.
            let stopped = super::crm_outbound::on_reply(conn, &contact, &format!("{subject}\n{snippet}"))?;
            let open: i64 = conn.query_row("SELECT COUNT(*) FROM crm_touches WHERE contact_id=?1 AND deleted IS NULL AND status IN ('planned','drafted','approved')", [&contact], |r| r.get(0))?;
            replied.push(json!({ "contact_id": contact, "name": name, "interaction_id": ix, "channel": channel, "subject": subject, "snippet": snippet, "open_touches": open, "stopped_touches": stopped }));
        }
    }
    state_set(conn, "events_id", &last.to_string())?;
    Ok((n, replied))
}

/// The five entities Gmail puts in a snippet, decoded (`&amp;` last, so an
/// escaped entity such as `&amp;lt;` decodes once).
pub(crate) fn unescape_html(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&#x27;", "'").replace("&amp;", "&")
}

/// Outbound: each delivered send in the sent-mail log, on each recipient in
/// the CRM. Reads from the byte offset it stopped at; starts at the end.
pub(crate) fn capture_sent(conn: &rusqlite::Connection, path: &std::path::Path) -> rusqlite::Result<usize> {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let Some(off) = state_get(conn, "sent_offset").and_then(|v| v.parse::<u64>().ok()) else {
        state_set(conn, "sent_offset", &len.to_string())?;
        return Ok(0);
    };
    let off = if off > len { 0 } else { off };
    if off == len {
        return Ok(0);
    }
    use std::io::{Read, Seek, SeekFrom};
    let mut buf = String::new();
    if let Ok(mut f) = std::fs::File::open(path) {
        if f.seek(SeekFrom::Start(off)).is_ok() {
            if let Err(e) = f.take(4_000_000).read_to_string(&mut buf) {
                // Not UTF-8 or cut mid-character: nothing is consumed, and the
                // next pass tries again from the same offset.
                tracing::warn!(target: "amux::crm", error = %e, measured = false, n_considered = 0, verdict = "crm_capture_sent_unreadable", "the sent-mail log could not be read");
                return Ok(0);
            }
        }
    }
    // Only whole lines: a line still being written is read next time.
    let upto = buf.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let mut n = 0;
    for line in buf[..upto].lines() {
        let Ok(r) = serde_json::from_str::<Value>(line) else { continue };
        if r.get("blocked").is_some() || r.get("delivered") == Some(&json!(false)) {
            continue;
        }
        let subject = r.get("subject").and_then(Value::as_str).unwrap_or("").to_string();
        let msg = r.get("message_id").and_then(Value::as_str).or_else(|| r.get("gmail_id").and_then(Value::as_str))
            .or_else(|| r.get("id").and_then(Value::as_str).filter(|_| r.get("via").and_then(Value::as_str) == Some("gmail"))).unwrap_or("").to_string();
        // The send log keeps `body_preview` (and `body_chars`), not the body:
        // reading only `body` left every captured send with an empty note.
        let note: String = r.get("body").and_then(Value::as_str).filter(|b| !b.trim().is_empty())
            .or_else(|| r.get("body_preview").and_then(Value::as_str)).unwrap_or("").chars().take(2000).collect();
        let msg = if msg.is_empty() { format!("sent:{}:{}", r.get("ts").and_then(Value::as_str).unwrap_or(""), subject) } else { msg };
        let to: String = ["to", "cc"].iter().filter_map(|k| r.get(*k).and_then(Value::as_str)).collect::<Vec<_>>().join(",");
        for addr in to.split([',', ';']).map(|a| a.trim()).filter(|a| a.contains('@')) {
            let addr = addr.rsplit('<').next().unwrap_or(addr).trim_end_matches('>').trim().to_lowercase();
            let Some(cid) = conn.query_row("SELECT id FROM crm_contacts WHERE lower(email)=?1 AND deleted IS NULL", [&addr], |r| r.get::<_, String>(0)).ok() else { continue };
            n += conn.execute(
                "INSERT OR IGNORE INTO crm_interactions (id,contact_id,date,type,notes,created,updated,direction,subject,message_id) VALUES (?1,?2,?3,'email',?4,?5,?5,'out',?6,?7)",
                rusqlite::params![ulid::Ulid::new().to_string(), &cid, chrono::Local::now().format("%Y-%m-%d").to_string(),
                    &note, now_secs(), &subject, &msg],
            )?;
        }
    }
    state_set(conn, "sent_offset", &(off + upto as u64).to_string())?;
    Ok(n)
}

// ---- activity ------------------------------------------------------------------------

/// The CRM's own events (`emit` above writes them as session `crm`), newest
/// first: what a worker's subscription would have matched. `total` counts every
/// one, so a capped list says how much it left out.
async fn activity(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let limit = q.get("limit").and_then(|v| v.parse::<i64>().ok()).filter(|n| *n > 0).unwrap_or(200).min(1000);
    let etype = q.get("type").map(|s| s.trim().to_string()).filter(|s| s.starts_with("crm."));
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let conn = store.read()?;
        let like = etype.clone().unwrap_or_else(|| "crm.%".into());
        let rows: Vec<Value> = conn
            .prepare("SELECT id, ts, type, data FROM session_events WHERE session='crm' AND type LIKE ?1 ORDER BY id DESC LIMIT ?2")?
            .query_map(rusqlite::params![&like, limit], |r| {
                let data: Option<String> = r.get(3)?;
                Ok(json!({ "id": r.get::<_, i64>(0)?, "ts": r.get::<_, f64>(1)?, "type": r.get::<_, String>(2)?,
                    "data": data.and_then(|d| serde_json::from_str::<Value>(&d).ok()).unwrap_or(Value::Null) }))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM session_events WHERE session='crm' AND type LIKE ?1", [&like], |r| r.get(0))?;
        let mut types = serde_json::Map::new();
        let mut st = conn.prepare("SELECT type, COUNT(*) FROM session_events WHERE session='crm' AND type LIKE 'crm.%' GROUP BY type")?;
        for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (t, n) = row?;
            types.insert(t, json!(n));
        }
        Ok(json!({ "events": rows, "total": total, "types": types, "measured": true, "n_considered": total }))
    })
    .await;
    match joined {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "amux::crm", measured = false, verdict = "crm_activity_failed", "the CRM activity could not be read: {e}");
            internal(e)
        }
        Err(e) => internal(anyhow::anyhow!(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::standing_approvals::tests::{call, test_state};

    #[test]
    fn a_page_with_text_whose_case_changes_length_reads_without_a_panic() {
        let items = page_items("<p>İstanbul ofisi</p><SCRIPT>x</SCRIPT><p>Ωmega Straße</p>");
        let t: Vec<&str> = items.iter().map(|v| v["title"].as_str().unwrap()).collect();
        assert_eq!(t, vec!["İstanbul ofisi", "Ωmega Straße"]);
    }

    #[test]
    fn a_page_reads_as_its_visible_lines() {
        let items = page_items("<html><script>var x=1</script><h1>Careers</h1><ul><li>Senior Video Engineer</li><li>ML Researcher &amp; Lead</li></ul><style>.a{}</style></html>");
        let t: Vec<&str> = items.iter().map(|v| v["title"].as_str().unwrap()).collect();
        assert_eq!(t, vec!["Careers", "Senior Video Engineer", "ML Researcher & Lead"]);
    }

    #[tokio::test]
    async fn a_watched_channel_records_first_then_reports_only_what_is_new() {
        let state = test_state();
        let app = crate::api::crm::routes().merge(crate::api::crm_accounts::routes()).merge(routes()).with_state(state.clone());
        let (_, a) = call(&app, "POST", "/accounts", &[], Some(json!({"name": "Acme", "domain": "acme.io"}))).await;
        let acme = a["id"].as_str().unwrap().to_string();
        let (st, c) = call(&app, "POST", "/channels", &[], Some(json!({"owner_kind": "account", "owner_id": acme, "kind": "careers", "url": "https://acme.io/careers", "watch": true}))).await;
        assert_eq!((st, c["how"].as_str()), (StatusCode::CREATED, Some("fetch")), "a careers page is fetched by the server");
        let (_, b) = call(&app, "POST", "/channels", &[], Some(json!({"owner_kind": "account", "owner_id": acme, "kind": "careers", "url": "https://acme.io/jobs", "how": "browser"}))).await;
        assert_eq!(b["how"].as_str(), Some("browser"), "the reply says the checker actually stored");
        let ch = c["id"].as_str().unwrap().to_string();
        let (_, r) = call(&app, "POST", &format!("/channels/{ch}/observed"), &[("x-amux-session", "gtm-engine")], Some(json!({"items": [{"title": "Video Engineer"}]}))).await;
        assert_eq!(r["changed"], false, "the first check only records");
        let (_, r) = call(&app, "POST", &format!("/channels/{ch}/observed"), &[], Some(json!({"items": [{"title": "Video Engineer"}]}))).await;
        assert_eq!(r["changed"], false, "nothing new");
        let (_, r) = call(&app, "POST", &format!("/channels/{ch}/observed"), &[], Some(json!({"items": [{"title": "Video Engineer"}, {"title": "Head of Search"}]}))).await;
        assert_eq!((r["changed"].as_bool(), r["n_added"].as_i64()), (Some(true), Some(1)));
        let ev: String = state.store.read().unwrap().query_row(
            "SELECT data FROM session_events WHERE type='crm.channel.changed' ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).unwrap();
        let ev: Value = serde_json::from_str(&ev).unwrap();
        assert_eq!((ev["owner_name"].as_str(), ev["summary"].as_str(), ev["kind"].as_str()), (Some("Acme"), Some("Head of Search"), Some("careers")));
        let (st, _) = call(&app, "POST", "/channels", &[], Some(json!({"owner_kind": "account", "owner_id": acme, "kind": "x", "handle": "@acme", "watch": true}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "a watched channel needs a url");
    }

    #[tokio::test]
    async fn an_outward_touch_waits_for_the_owner_and_approval_wakes_its_worker() {
        let mut state = test_state();
        state.auth_token = Some("owner-secret".into());
        let app = crate::api::crm::routes().merge(routes()).with_state(state.clone());
        let owner = [("authorization", "Bearer owner-secret")];
        let w = [("x-amux-session", "gtm-engine")];
        let (_, p) = call(&app, "POST", "/contacts", &owner, Some(json!({"name": "Ada", "email": "ada@acme.io"}))).await;
        let ada = p["id"].as_str().unwrap().to_string();
        let (st, t) = call(&app, "POST", "/touches", &w, Some(json!({"contact_id": ada, "channel": "linkedin", "action": "dm", "body": "Hi Ada"}))).await;
        assert_eq!((st, t["status"].as_str()), (StatusCode::CREATED, Some("drafted")));
        let id = t["id"].as_str().unwrap().to_string();
        let (st, d) = call(&app, "POST", &format!("/touches/{id}/done"), &w, Some(json!({"result": "sent"}))).await;
        assert_eq!((st, d["code"].as_str()), (StatusCode::CONFLICT, Some("not_approved")), "not done before the owner approves");
        let (st, _) = call(&app, "POST", &format!("/touches/{id}/approve"), &w, None).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "a worker cannot approve");
        let (st, a) = call(&app, "POST", &format!("/touches/{id}/approve"), &owner, None).await;
        assert_eq!((st, a["touch"]["assignee"].as_str()), (StatusCode::OK, Some("gtm-engine")), "the maker is the assignee");
        let n: i64 = state.store.read().unwrap().query_row("SELECT COUNT(*) FROM session_events WHERE type='crm.touch.approved' AND source='crm'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "approval is an event a worker can subscribe to");
        let (st, _) = call(&app, "POST", &format!("/touches/{id}/done"), &w, Some(json!({"result": "sent", "result_url": "https://linkedin.com/x"}))).await;
        assert_eq!(st, StatusCode::OK);
        let (_, p) = call(&app, "GET", &format!("/contacts/{ada}"), &owner, None).await;
        assert_eq!((p["interactions"][0]["direction"].as_str(), p["interactions"][0]["subject"].as_str()), (Some("out"), Some("linkedin dm")));
        let (st, _) = call(&app, "PATCH", &format!("/touches/{id}"), &w, Some(json!({"status": "approved"}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "approval only through its own route");
    }

    #[test]
    fn mail_and_linkedin_messages_attach_to_the_person_they_are_from() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate::apply_all(&mut conn).unwrap();
        let now = now_secs();
        conn.execute("INSERT INTO crm_contacts (id,name,email,created,updated) VALUES ('PPL-1','Ada Lovelace','ada@acme.io',?1,?1)", [now]).unwrap();
        conn.execute("INSERT INTO session_events (ts,type,data,source) VALUES (?1,'email.received','{}','email')", [now as f64]).unwrap();
        assert_eq!(capture_events(&conn).unwrap().0, 0, "the first pass starts at the ledger's end");
        for (ty, data) in [
            ("email.received", json!({"from_address": "ADA@acme.io", "subject": "Re: demo", "snippet": "Tuesday works", "message_id": "<m1>"})),
            ("email.received", json!({"from_address": "news@x.io", "bulk": true})),
            ("linkedin.message_received", json!({"from_name": "Ada Lovelace", "snippet": "hello", "key": "k1"})),
            ("email.received", json!({"from_address": "ada@acme.io", "subject": "Re: demo", "message_id": "<m1>"})),
        ] {
            conn.execute("INSERT INTO session_events (ts,type,data,source) VALUES (?1,?2,?3,'x')", rusqlite::params![now as f64, ty, data.to_string()]).unwrap();
        }
        let (n, replied) = capture_events(&conn).unwrap();
        assert_eq!(n, 2, "one mail and one message; the newsletter and the repeat are not");
        assert_eq!(replied[0]["channel"], "email");
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("email-sent.jsonl");
        std::fs::write(&log, "").unwrap();
        assert_eq!(capture_sent(&conn, &log).unwrap(), 0);
        std::fs::write(&log, format!("{}\n{}\n", json!({"to": "Ada <ada@acme.io>", "subject": "Deck", "message_id": "<s1>"}), json!({"to": "ada@acme.io", "subject": "x", "blocked": "held"}))).unwrap();
        assert_eq!(capture_sent(&conn, &log).unwrap(), 1, "a delivered send attaches; a held one does not");
        let dirs: Vec<String> = conn.prepare("SELECT direction FROM crm_interactions WHERE contact_id='PPL-1' ORDER BY direction").unwrap()
            .query_map([], |r| r.get(0)).unwrap().filter_map(Result::ok).collect();
        assert_eq!(dirs, vec!["in", "in", "out"]);

        // gtm-engine, 2026-11-25: a snippet kept `&#39;` and `&lt;`, and a send
        // logged with `body_preview` only was captured with an empty note.
        conn.execute("INSERT INTO session_events (ts,type,data,source) VALUES (?1,'email.received',?2,'x')",
            rusqlite::params![now as f64, json!({"from_address": "ada@acme.io", "subject": "Re: it&#39;s on", "snippet": "we&#39;re in &lt;hold&gt; &amp; back", "message_id": "<m2>"}).to_string()]).unwrap();
        assert_eq!(capture_events(&conn).unwrap().0, 1);
        let (subj, note): (String, String) = conn.query_row("SELECT subject, notes FROM crm_interactions WHERE message_id='<m2>'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((subj.as_str(), note.as_str()), ("Re: it's on", "we're in <hold> & back"));
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        use std::io::Write;
        writeln!(f, "{}", json!({"to": "ada@acme.io", "subject": "Re: hold", "body_preview": "Understood, thanks.", "id": "g1", "via": "gmail"})).unwrap();
        assert_eq!(capture_sent(&conn, &log).unwrap(), 1);
        let note: String = conn.query_row("SELECT notes FROM crm_interactions WHERE message_id='g1'", [], |r| r.get(0)).unwrap();
        assert_eq!(note, "Understood, thanks.");
    }
    #[tokio::test]
    async fn activity_lists_the_crm_events_and_the_outbound_counts_name_what_is_blocked() {
        let mut state = test_state();
        state.auth_token = Some("owner-secret".into());
        let app = crate::api::crm::routes().merge(crate::api::crm_accounts::routes()).merge(routes()).with_state(state.clone());
        let owner = [("authorization", "Bearer owner-secret")];
        let w = [("x-amux-session", "gtm-engine")];
        let (_, a) = call(&app, "POST", "/contacts", &owner, Some(json!({"name": "Ada", "email": "ada@acme.io", "company": "Acme"}))).await;
        let (_, b) = call(&app, "POST", "/contacts", &owner, Some(json!({"name": "Bo", "email": "bo@acme.io"}))).await;
        let (ada, bo) = (a["id"].as_str().unwrap().to_string(), b["id"].as_str().unwrap().to_string());
        let (_, t1) = call(&app, "POST", "/touches", &w, Some(json!({"contact_id": ada, "channel": "linkedin", "action": "dm", "body": "Hi Ada"}))).await;
        let (_, t2) = call(&app, "POST", "/touches", &w, Some(json!({"contact_id": bo, "channel": "linkedin", "action": "dm", "body": "Hi Bo"}))).await;
        assert_eq!(t2["status"].as_str(), Some("drafted"));
        state.store.write_async(move |c| { c.execute("UPDATE crm_contacts SET unsubscribed_at=1 WHERE id=?1", [&bo])?; Ok(WriteOutcome { applied: true, events: vec![] }) }).await.unwrap();
        let (_, s) = call(&app, "GET", "/summary", &owner, None).await;
        assert_eq!((s["to_approve"].as_i64(), s["blocked"].as_i64(), s["decided"].as_i64()), (Some(2), Some(1), Some(0)), "an opted-out person's draft is blocked");
        let (_, rows) = call(&app, "GET", "/touches?status=drafted&limit=1", &owner, None).await;
        assert_eq!(rows.as_array().map(Vec::len), Some(1), "limit caps the list when asked");
        let (_, rows) = call(&app, "GET", "/touches?status=drafted", &owner, None).await;
        let bo_row = rows.as_array().unwrap().iter().find(|r| r["contact_name"] == "Bo").unwrap();
        assert!(bo_row["contact_unsubscribed_at"].as_i64().is_some(), "a touch row says its person opted out");
        let id = t1["id"].as_str().unwrap();
        let (st, _) = call(&app, "POST", &format!("/touches/{id}/approve"), &owner, None).await;
        assert_eq!(st, StatusCode::OK);
        let (_, s) = call(&app, "GET", "/summary", &owner, None).await;
        assert_eq!(s["decided"].as_i64(), Some(1), "an approval moves into the history count");
        let id2 = t2["id"].as_str().unwrap();
        let (st, _) = call(&app, "PATCH", &format!("/touches/{id2}"), &owner, Some(json!({"status": "skipped", "result": "declined: too pushy"}))).await;
        assert_eq!(st, StatusCode::OK);
        let (_, rows) = call(&app, "GET", "/touches?status=skipped", &owner, None).await;
        assert_eq!(rows[0]["result"].as_str(), Some("declined: too pushy"), "a decline keeps its reason for the worker");
        let (st, act) = call(&app, "GET", "/activity", &owner, None).await;
        assert_eq!((st, act["measured"].as_bool()), (StatusCode::OK, Some(true)));
        let ev = &act["events"][0];
        assert_eq!((ev["type"].as_str(), ev["data"]["contact_name"].as_str()), (Some("crm.touch.approved"), Some("Ada")));
        assert_eq!(act["total"].as_i64(), Some(act["events"].as_array().unwrap().len() as i64));
        assert_eq!(act["types"]["crm.touch.approved"].as_i64(), Some(1));
        let (_, none) = call(&app, "GET", "/activity?type=crm.signup", &owner, None).await;
        assert_eq!((none["total"].as_i64(), none["measured"].as_bool()), (Some(0), Some(true)), "an empty filter still says it measured");
    }
}
