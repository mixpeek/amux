//! CRM companies, deals and lookalikes (docs/design/crm.md, phase 1; owner
//! 2026-11-23: one CRM for Mixpeek, amux and personal, with contact info, ICP
//! grade and lookalikes, to replace Lightfield and Instantly over time).
//!
//!   GET    /api/crm/accounts[?q=&grade=&status=]   companies, with people and open deal counts
//!   POST   /api/crm/accounts                       201 {id}; `name` required; 409 on a live domain
//!   GET    /api/crm/accounts/{id}                  company + people + deals + lookalikes
//!   PATCH  /api/crm/accounts/{id}                  whitelisted fields
//!   DELETE /api/crm/accounts/{id}                  soft delete
//!   GET    /api/crm/deals[?stage=&account_id=]     deals, newest first
//!   POST   /api/crm/deals                          201 {id}; `title` required
//!   GET    /api/crm/deals/{id}                     one deal, with its company and people
//!   PATCH  /api/crm/deals/{id}, DELETE (soft)
//!   POST   /api/crm/accounts/{id}/interactions     activity on the company itself {type, notes, subject?, date?, direction?, message_id?}
//!   POST   /api/crm/lookalikes                     {kind, from_id, to_id, score, reason}
//!   DELETE /api/crm/lookalikes                     {kind, from_id, to_id}
//!   GET    /api/crm/summary                        counts for the tab's header
//!
//! Every create records `source`: what the body says, else `worker:<name>` for
//! a worker's request, else `owner`. The audit found 14 brand names and an
//! RB2B batch in the address book with nothing saying where they came from.

use super::calendar::query_rows_json;
use super::{internal, AppState};
use crate::db::board_store::next_issue_id;
use crate::db::{PendingEvent, WriteOutcome};
use amux_core::revision::{EntityType, MutationKind};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/accounts", get(list_accounts).post(create_account))
        .route("/accounts/{id}", get(get_account).patch(patch_account).delete(delete_account))
        .route("/deals", get(list_deals).post(create_deal))
        .route("/deals/{id}", get(get_deal).patch(patch_deal).delete(delete_deal))
        .route("/accounts/{id}/interactions", axum::routing::post(add_account_interaction))
        .route("/lookalikes", axum::routing::post(add_lookalike).delete(delete_lookalike))
        .route("/summary", get(summary))
}

/// Lightfield's stages, which the GTM scripts already use.
pub const STAGES: &[&str] = &["lead", "qualification", "demo", "trial", "proposal", "won", "lost"];
pub const GRADES: &[&str] = &["A", "B", "C", "D"];

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn ev(kind: &str, id: &str, m: MutationKind) -> PendingEvent {
    PendingEvent { entity_type: EntityType::Other(kind.into()), entity_id: id.to_string(), mutation: m, payload: None }
}

/// Who created a record: the body's `source`, a worker, or the owner.
pub(crate) fn source_of(body: &Value, headers: &HeaderMap) -> String {
    let given = body.get("source").and_then(Value::as_str).map(str::trim).unwrap_or("");
    if !given.is_empty() {
        return given.chars().take(120).collect();
    }
    match headers.get("x-amux-session").and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty()) {
        Some(w) => format!("worker:{w}"),
        None => "owner".into(),
    }
}

/// A grade of A to D, or empty; anything else is refused.
pub(crate) fn grade_of(v: &Value) -> Result<String, String> {
    let g = v.as_str().unwrap_or("").trim().to_uppercase();
    if g.is_empty() || GRADES.contains(&g.as_str()) {
        Ok(g)
    } else {
        Err(format!("icp_grade must be one of A, B, C, D or empty, not {g:?}"))
    }
}

/// A bare domain: no scheme, path or www, lowercased.
pub(crate) fn norm_domain(s: &str) -> String {
    let s = s.trim().to_lowercase();
    let s = s.split("://").last().unwrap_or("");
    let s = s.split(['/', '?', '#']).next().unwrap_or("");
    s.trim_start_matches("www.").to_string()
}

fn text(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Text(String::new()),
        Value::String(s) => SqlValue::Text(s.clone()),
        other => SqlValue::Text(other.to_string()),
    }
}

/// UPDATE `table` SET each present whitelisted field; returns the rows changed.
fn patch_row(conn: &rusqlite::Connection, table: &str, id: &str, body: &Value, fields: &[&str], extra: &[(&str, SqlValue)]) -> rusqlite::Result<usize> {
    let mut sets = Vec::new();
    let mut params: Vec<SqlValue> = Vec::new();
    for f in fields {
        if let Some(v) = body.get(*f) {
            params.push(text(v));
            sets.push(format!("{f}=?{}", params.len()));
        }
    }
    for (f, v) in extra {
        params.push(v.clone());
        sets.push(format!("{f}=?{}", params.len()));
    }
    if sets.is_empty() {
        return Ok(0);
    }
    params.push(SqlValue::Integer(now_secs()));
    let u = params.len();
    params.push(SqlValue::Text(id.to_string()));
    let i = params.len();
    conn.execute(
        &format!("UPDATE {table} SET {}, updated=?{u} WHERE id=?{i} AND deleted IS NULL", sets.join(", ")),
        rusqlite::params_from_iter(params.iter()),
    )
}

// ---- companies ---------------------------------------------------------------

const ACCOUNT_FIELDS: &[&str] = &[
    "name", "domain", "industry", "size", "location", "website", "linkedin", "notes", "status", "icp_reasons", "external",
];

async fn list_accounts(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        let mut sql = String::from(
            "SELECT a.id,a.name,a.domain,a.industry,a.size,a.location,a.website,a.linkedin,a.status,a.icp_grade,a.icp_reasons,a.source,a.tags,a.updated,\
             (SELECT COUNT(*) FROM crm_contacts c WHERE c.account_id=a.id AND c.deleted IS NULL) AS people,\
             (SELECT COUNT(*) FROM crm_deals d WHERE d.account_id=a.id AND d.deleted IS NULL AND d.stage NOT IN ('won','lost')) AS open_deals,\
             MAX(COALESCE((SELECT MAX(i.date) FROM crm_interactions i JOIN crm_contacts c ON c.id=i.contact_id WHERE c.account_id=a.id AND c.deleted IS NULL),''), \
                 COALESCE((SELECT MAX(x.date) FROM crm_account_activity x WHERE x.account_id=a.id),'')) AS last_date \
             FROM crm_accounts a WHERE a.deleted IS NULL",
        );
        let mut args: Vec<String> = Vec::new();
        if let Some(n) = q.get("q").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            args.push(format!("%{n}%"));
            let k = args.len();
            sql.push_str(&format!(" AND (a.name LIKE ?{k} OR a.domain LIKE ?{k} OR a.industry LIKE ?{k})"));
        }
        for (key, col) in [("grade", "a.icp_grade"), ("status", "a.status")] {
            if let Some(v) = q.get(key).map(|s| s.trim()).filter(|s| !s.is_empty()) {
                args.push(v.to_string());
                sql.push_str(&format!(" AND {col}=?{}", args.len()));
            }
        }
        sql.push_str(" ORDER BY CASE a.icp_grade WHEN 'A' THEN 0 WHEN 'B' THEN 1 WHEN 'C' THEN 2 WHEN 'D' THEN 3 ELSE 4 END, a.updated DESC");
        sql.push_str(&super::crm::page_of(&q));
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

async fn create_account(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let name = body.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "name required" }));
    }
    let grade = match grade_of(body.get("icp_grade").unwrap_or(&Value::Null)) {
        Ok(g) => g,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    let domain = norm_domain(body.get("domain").and_then(Value::as_str).unwrap_or(""));
    let source = source_of(&body, &headers);
    let slot: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
    let w = slot.clone();
    let res = state
        .store
        .write_async(move |conn| {
            if !domain.is_empty() {
                let dup: Option<String> = conn
                    .query_row("SELECT id FROM crm_accounts WHERE lower(domain)=?1 AND deleted IS NULL", [&domain], |r| r.get(0))
                    .ok();
                if let Some(d) = dup {
                    *w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Err(d));
                    return Ok(WriteOutcome { applied: false, events: vec![] });
                }
            }
            let id = next_issue_id(conn, "ACCT")?;
            let now = now_secs();
            let s = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let tags = body.get("tags").filter(|t| t.is_array()).map(Value::to_string).unwrap_or_else(|| "[]".into());
            conn.execute(
                "INSERT INTO crm_accounts (id,name,domain,industry,size,location,website,linkedin,notes,tags,status,icp_grade,icp_reasons,icp_scored_at,source,external,created,updated) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?17)",
                rusqlite::params![&id, &name, &domain, s("industry"), s("size"), s("location"), s("website"), s("linkedin"), s("notes"),
                    tags, s("status"), &grade, s("icp_reasons"), if grade.is_empty() { None } else { Some(now) }, &source,
                    body.get("external").filter(|e| e.is_object()).map(Value::to_string).unwrap_or_else(|| "{}".into()), now],
            )?;
            *w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Ok(id.clone()));
            Ok(WriteOutcome { applied: true, events: vec![ev("crm_account", &id, MutationKind::Created)] })
        })
        .await;
    if let Err(e) = res {
        return internal(e);
    }
    let taken = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
    match taken {
        Some(Ok(id)) => (StatusCode::CREATED, Json(json!({ "id": id, "ok": true }))).into_response(),
        Some(Err(dup)) => {
            tracing::info!(target: "amux::crm", existing = %dup, measured = true, n_considered = 1, verdict = "crm_account_duplicate_domain", "a company with this domain already exists");
            err(StatusCode::CONFLICT, json!({ "error": "a company with this domain already exists", "code": "duplicate", "id": dup }))
        }
        None => internal(anyhow::anyhow!("create_account wrote nothing")),
    }
}

async fn get_account(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let conn = store.read()?;
        let p = [&id as &dyn rusqlite::ToSql];
        let mut rows = query_rows_json(&conn, "SELECT * FROM crm_accounts WHERE id=?1 AND deleted IS NULL", &p)?;
        if rows.is_empty() {
            return Ok(None);
        }
        let mut a = rows.remove(0);
        let people = query_rows_json(&conn,
            "SELECT id,name,role,email,phone,linkedin,status,icp_grade FROM crm_contacts WHERE account_id=?1 AND deleted IS NULL ORDER BY name", &p)?;
        let deals = query_rows_json(&conn,
            "SELECT * FROM crm_deals WHERE account_id=?1 AND deleted IS NULL ORDER BY updated DESC", &p)?;
        let mut activity = query_rows_json(&conn,
            "SELECT i.*, c.name AS contact_name FROM crm_interactions i JOIN crm_contacts c ON c.id=i.contact_id \
             WHERE c.account_id=?1 AND c.deleted IS NULL ORDER BY i.date DESC, i.created DESC LIMIT 50", &p)?;
        // The company's own activity (competitor changes, company-level
        // events, a watched company channel) beside its people's.
        activity.extend(query_rows_json(&conn,
            "SELECT x.*, '' AS contact_id, '' AS contact_name FROM crm_account_activity x WHERE x.account_id=?1 ORDER BY x.date DESC, x.created DESC LIMIT 50", &p)?);
        activity.sort_by(|a, b| {
            let k = |v: &Value| (v["date"].as_str().unwrap_or("").to_string(), v["created"].as_i64().unwrap_or(0));
            k(b).cmp(&k(a))
        });
        activity.truncate(50);
        let looks = lookalikes_of(&conn, "account", &id)?;
        if let Some(o) = a.as_object_mut() {
            o.insert("people".into(), Value::Array(people));
            o.insert("deals".into(), Value::Array(deals));
            o.insert("activity".into(), Value::Array(activity));
            o.insert("lookalikes".into(), Value::Array(looks));
        }
        Ok(Some(a))
    })
    .await;
    match joined {
        Ok(Ok(Some(a))) => Json(a).into_response(),
        Ok(Ok(None)) => err(StatusCode::NOT_FOUND, json!({ "error": "not found" })),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

/// The companies (or people) like `id`, both directions, best first, each
/// with the other side's name.
pub(crate) fn lookalikes_of(conn: &rusqlite::Connection, kind: &str, id: &str) -> rusqlite::Result<Vec<Value>> {
    let (table, cols) = if kind == "account" { ("crm_accounts", "o.name, o.domain, o.icp_grade") } else { ("crm_contacts", "o.name, o.email AS domain, o.icp_grade") };
    query_rows_json(
        conn,
        &format!(
            "SELECT o.id, {cols}, l.score, l.reason, l.found_by, l.found_at FROM crm_lookalikes l \
             JOIN {table} o ON o.id = CASE WHEN l.from_id=?2 THEN l.to_id ELSE l.from_id END AND o.deleted IS NULL \
             WHERE l.kind=?1 AND (l.from_id=?2 OR l.to_id=?2) ORDER BY l.score DESC"
        ),
        &[&kind as &dyn rusqlite::ToSql, &id],
    )
}

async fn patch_account(State(state): State<AppState>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let mut extra: Vec<(&str, SqlValue)> = Vec::new();
    if let Some(g) = body.get("icp_grade") {
        match grade_of(g) {
            Ok(g) => {
                extra.push(("icp_scored_at", if g.is_empty() { SqlValue::Null } else { SqlValue::Integer(now_secs()) }));
                extra.push(("icp_grade", SqlValue::Text(g)));
            }
            Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
        }
    }
    if let Some(t) = body.get("tags").filter(|t| t.is_array()) {
        extra.push(("tags", SqlValue::Text(t.to_string())));
    }
    let mut body = body;
    if let Some(d) = body.get("domain").and_then(Value::as_str).map(norm_domain) {
        body["domain"] = Value::String(d);
    }
    let res = state
        .store
        .write_async(move |conn| {
            let n = patch_row(conn, "crm_accounts", &id, &body, ACCOUNT_FIELDS, &extra)?;
            Ok(WriteOutcome { applied: n > 0, events: if n > 0 { vec![ev("crm_account", &id, MutationKind::Updated)] } else { vec![] } })
        })
        .await;
    match res {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) if e.to_string().contains("UNIQUE") => err(StatusCode::CONFLICT, json!({ "error": "a company with this domain already exists", "code": "duplicate" })),
        Err(e) => internal(e),
    }
}

async fn delete_account(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    soft_delete(state, "crm_accounts", "crm_account", id).await
}

async fn soft_delete(state: AppState, table: &'static str, kind: &'static str, id: String) -> Response {
    let res = state
        .store
        .write_async(move |conn| {
            let n = conn.execute(&format!("UPDATE {table} SET deleted=?1 WHERE id=?2 AND deleted IS NULL"), rusqlite::params![now_secs(), &id])?;
            Ok(WriteOutcome { applied: n > 0, events: if n > 0 { vec![ev(kind, &id, MutationKind::Deleted)] } else { vec![] } })
        })
        .await;
    match res {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- deals ---------------------------------------------------------------------

const DEAL_FIELDS: &[&str] = &["title", "account_id", "currency", "close_date", "next_step", "owner", "notes", "external", "lost_reason", "asset_url"];

fn stage_of(v: &Value) -> Result<String, String> {
    let s = v.as_str().unwrap_or("").trim().to_lowercase();
    if STAGES.contains(&s.as_str()) {
        Ok(s)
    } else {
        Err(format!("stage must be one of {}", STAGES.join(", ")))
    }
}

async fn list_deals(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        let mut sql = String::from(
            "SELECT d.*, a.name AS account_name FROM crm_deals d LEFT JOIN crm_accounts a ON a.id=d.account_id WHERE d.deleted IS NULL",
        );
        let mut args: Vec<String> = Vec::new();
        for (key, col) in [("stage", "d.stage"), ("account_id", "d.account_id")] {
            if let Some(v) = q.get(key).map(|s| s.trim()).filter(|s| !s.is_empty()) {
                args.push(v.to_string());
                sql.push_str(&format!(" AND {col}=?{}", args.len()));
            }
        }
        if let Some(t) = q.get("tag").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            args.push(t.to_string());
            sql.push_str(&format!(" AND EXISTS (SELECT 1 FROM json_each(CASE WHEN json_valid(d.tags) THEN d.tags ELSE '[]' END) WHERE value=?{})", args.len()));
        }
        sql.push_str(" ORDER BY d.updated DESC");
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a as &dyn rusqlite::ToSql).collect();
        let mut rows = query_rows_json(&conn, &sql, &refs)?;
        rows.iter_mut().for_each(deal_arrays);
        Ok(rows)
    })
    .await;
    match joined {
        Ok(Ok(rows)) => Json(Value::Array(rows)).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

/// A deal row's JSON-array columns, as arrays.
fn deal_arrays(r: &mut Value) {
    if let Some(o) = r.as_object_mut() {
        for k in ["contact_ids", "tags"] {
            let v = o.get(k).and_then(Value::as_str).and_then(|s| serde_json::from_str::<Value>(s).ok()).filter(Value::is_array).unwrap_or(json!([]));
            o.insert(k.into(), v);
        }
    }
}

async fn get_deal(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let conn = store.read()?;
        let mut rows = query_rows_json(&conn,
            "SELECT d.*, a.name AS account_name FROM crm_deals d LEFT JOIN crm_accounts a ON a.id=d.account_id WHERE d.id=?1 AND d.deleted IS NULL",
            &[&id as &dyn rusqlite::ToSql])?;
        let Some(mut d) = rows.pop() else { return Ok(None) };
        deal_arrays(&mut d);
        let mut people = Vec::new();
        for cid in d["contact_ids"].as_array().cloned().unwrap_or_default().iter().filter_map(Value::as_str) {
            people.extend(query_rows_json(&conn, "SELECT id,name,role,email,status FROM crm_contacts WHERE id=?1 AND deleted IS NULL",
                &[&cid as &dyn rusqlite::ToSql])?);
        }
        d["people"] = Value::Array(people);
        Ok(Some(d))
    })
    .await;
    match joined {
        Ok(Ok(Some(d))) => Json(d).into_response(),
        Ok(Ok(None)) => err(StatusCode::NOT_FOUND, json!({ "error": "not found" })),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

/// Activity on the company itself (gtm-engine, 2026-11-25: a competitor's
/// change had nowhere to go, and the company's activity stayed empty). The
/// same message id twice for a company is one row.
async fn add_account_interaction(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let s = |k: &str| body.get(k).and_then(Value::as_str).map(str::trim).unwrap_or("").to_string();
    let ty = { let t = s("type"); if t.is_empty() { "other".to_string() } else { t } };
    let date = { let d = s("date"); if d.is_empty() { chrono::Local::now().format("%Y-%m-%d").to_string() } else { d } };
    let (notes, subject, direction, message_id) = (s("notes"), s("subject"), s("direction"), s("message_id"));
    if notes.is_empty() && subject.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "send notes or a subject" }));
    }
    let source = source_of(&body, &headers);
    let slot: Arc<Mutex<Option<Result<String, &'static str>>>> = Arc::new(Mutex::new(None));
    let w = slot.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let live: bool = conn.query_row("SELECT 1 FROM crm_accounts WHERE id=?1 AND deleted IS NULL", [&id], |_| Ok(true)).unwrap_or(false);
            if !live {
                *w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Err("not_found"));
                return Ok(WriteOutcome { applied: false, events: vec![] });
            }
            let ix = ulid::Ulid::new().to_string();
            let n = conn.execute(
                "INSERT OR IGNORE INTO crm_account_activity (id,account_id,date,type,direction,subject,notes,message_id,source,created) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                rusqlite::params![&ix, &id, &date, &ty, &direction, &subject, &notes, &message_id, &source, now_secs()],
            )?;
            if n == 0 {
                let existing: String = conn.query_row("SELECT id FROM crm_account_activity WHERE account_id=?1 AND message_id=?2",
                    [&id, &message_id], |r| r.get(0)).unwrap_or_default();
                *w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Ok(format!("dup:{existing}")));
                return Ok(WriteOutcome { applied: false, events: vec![] });
            }
            conn.execute("UPDATE crm_accounts SET updated=?2 WHERE id=?1", rusqlite::params![&id, now_secs()])?;
            *w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Ok(ix));
            Ok(WriteOutcome { applied: true, events: vec![ev("crm_account", &id, MutationKind::Updated)] })
        })
        .await;
    if let Err(e) = res {
        return internal(e);
    }
    let taken = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
    match taken {
        Some(Ok(ix)) => match ix.strip_prefix("dup:") {
            Some(existing) => (StatusCode::OK, Json(json!({ "id": existing, "ok": true, "duplicate": true }))).into_response(),
            None => (StatusCode::CREATED, Json(json!({ "id": ix, "ok": true }))).into_response(),
        },
        Some(Err(_)) => err(StatusCode::NOT_FOUND, json!({ "error": "not found" })),
        None => internal(anyhow::anyhow!("add_account_interaction wrote nothing")),
    }
}

async fn create_deal(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let title = body.get("title").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if title.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "title required" }));
    }
    let stage = match stage_of(body.get("stage").unwrap_or(&json!("lead"))) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    // A deal opened straight at a closing stage passes the same rules as a move there.
    if let Err((code, why)) = super::crm_close::check_stage("lead", &stage, &body, super::standing_approvals::is_owner_request(&headers)) {
        return err(StatusCode::CONFLICT, json!({ "error": why, "code": code }));
    }
    let source = source_of(&body, &headers);
    let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let w = slot.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let id = next_issue_id(conn, "DEAL")?;
            let now = now_secs();
            let s = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            conn.execute(
                "INSERT INTO crm_deals (id,title,account_id,contact_ids,stage,amount,currency,close_date,next_step,owner,notes,source,external,created,updated,tags) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?14,?15)",
                rusqlite::params![&id, &title, s("account_id"),
                    body.get("contact_ids").filter(|v| v.is_array()).map(Value::to_string).unwrap_or_else(|| "[]".into()),
                    &stage, body.get("amount").and_then(Value::as_f64), if s("currency").is_empty() { "USD".into() } else { s("currency") },
                    s("close_date"), s("next_step"), s("owner"), s("notes"), &source,
                    body.get("external").filter(|e| e.is_object()).map(Value::to_string).unwrap_or_else(|| "{}".into()), now,
                    body.get("tags").filter(|t| t.is_array()).map(Value::to_string).unwrap_or_else(|| "[]".into())],
            )?;
            conn.execute("UPDATE crm_deals SET mrr_usd=?2, lost_reason=?3, asset_url=?4 WHERE id=?1",
                rusqlite::params![&id, body.get("mrr_usd").and_then(Value::as_f64), s("lost_reason"), s("asset_url")])?;
            super::crm_close::first_stage(conn, &id, &stage, &source, "opened")?;
            *w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(id.clone());
            Ok(WriteOutcome { applied: true, events: vec![ev("crm_deal", &id, MutationKind::Created)] })
        })
        .await;
    match res {
        Ok(_) => (StatusCode::CREATED, Json(json!({ "id": slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take(), "ok": true }))).into_response(),
        Err(e) => internal(e),
    }
}

/// What a deal PATCH did: the stage move it made (if any), or the rule that refused it.
type StageWrite = Result<Option<Value>, (&'static str, String)>;

async fn patch_deal(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let mut extra: Vec<(&str, SqlValue)> = Vec::new();
    let to = match body.get("stage").map(stage_of) {
        Some(Ok(s)) => Some(s),
        Some(Err(e)) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
        None => None,
    };
    if let Some(a) = body.get("amount") {
        extra.push(("amount", a.as_f64().map(SqlValue::Real).unwrap_or(SqlValue::Null)));
    }
    if let Some(a) = body.get("mrr_usd") {
        extra.push(("mrr_usd", a.as_f64().map(SqlValue::Real).unwrap_or(SqlValue::Null)));
    }
    if let Some(c) = body.get("contact_ids").filter(|v| v.is_array()) {
        extra.push(("contact_ids", SqlValue::Text(c.to_string())));
    }
    if let Some(t) = body.get("tags").filter(|v| v.is_array()) {
        extra.push(("tags", SqlValue::Text(t.to_string())));
    }
    let owner = super::standing_approvals::is_owner_request(&headers);
    let by = source_of(&json!({}), &headers);
    let out: Arc<Mutex<Option<StageWrite>>> = Arc::new(Mutex::new(None));
    let w = out.clone();
    let res = state
        .store
        .write_async(move |conn| {
            // The rules read the deal as it will be: its row with this request laid over it.
            let mut merged = query_rows_json(conn, "SELECT * FROM crm_deals WHERE id=?1 AND deleted IS NULL", &[&id as &dyn rusqlite::ToSql])?
                .pop().unwrap_or(Value::Null);
            let from = merged["stage"].as_str().unwrap_or("").to_string();
            if let (Some(o), Some(b)) = (merged.as_object_mut(), body.as_object()) {
                for (k, v) in b {
                    o.insert(k.clone(), v.clone());
                }
            }
            if let Some(to) = &to {
                if let Err(e) = super::crm_close::check_stage(&from, to, &merged, owner) {
                    *w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Err(e));
                    return Ok(WriteOutcome { applied: false, events: vec![] });
                }
            }
            let n = patch_row(conn, "crm_deals", &id, &body, DEAL_FIELDS, &extra)?;
            let moved = match &to {
                Some(to) => super::crm_close::move_stage(conn, &id, to, &by, body.get("why").and_then(Value::as_str).unwrap_or(""), None)?,
                None => None,
            };
            let applied = n > 0 || moved.is_some();
            *w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Ok(moved));
            Ok(WriteOutcome { applied, events: if applied { vec![ev("crm_deal", &id, MutationKind::Updated)] } else { vec![] } })
        })
        .await;
    if let Err(e) = res {
        return internal(e);
    }
    let taken = out.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
    match taken {
        Some(Err((code, why))) => {
            tracing::info!(target: "amux::crm", measured = true, n_considered = 1, verdict = code, "a deal's stage change was refused: {why}");
            err(StatusCode::CONFLICT, json!({ "error": why, "code": code }))
        }
        Some(Ok(moved)) => {
            if let Some(m) = &moved {
                super::crm_agent::emit(&state, "crm.deal.stage", m.clone()).await;
                super::crm_close::emit_funnel_changed(&state, "deal_stage", "").await;
            }
            Json(json!({ "ok": true, "moved": moved })).into_response()
        }
        None => Json(json!({ "ok": true })).into_response(),
    }
}

async fn delete_deal(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    soft_delete(state, "crm_deals", "crm_deal", id).await
}

// ---- lookalikes ------------------------------------------------------------------

async fn add_lookalike(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let s = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let (kind, from, to) = (s("kind"), s("from_id"), s("to_id"));
    if !matches!(kind.as_str(), "account" | "contact") || from.is_empty() || to.is_empty() || from == to {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "send {kind: account|contact, from_id, to_id} with two different ids" }));
    }
    let score = body.get("score").and_then(Value::as_f64).unwrap_or(0.0).clamp(0.0, 1.0);
    let (reason, by) = (s("reason"), source_of(&json!({ "source": s("found_by") }), &headers));
    let res = state
        .store
        .write_async(move |conn| {
            // One row per pair, whichever way round it was found.
            let (a, b) = if from < to { (from.clone(), to.clone()) } else { (to.clone(), from.clone()) };
            conn.execute(
                "INSERT INTO crm_lookalikes (kind,from_id,to_id,score,reason,found_by,found_at) VALUES (?1,?2,?3,?4,?5,?6,?7) \
                 ON CONFLICT(kind,from_id,to_id) DO UPDATE SET score=excluded.score, reason=excluded.reason, found_by=excluded.found_by, found_at=excluded.found_at",
                rusqlite::params![&kind, a, b, score, reason, by, now_secs()],
            )?;
            Ok(WriteOutcome { applied: true, events: vec![ev(if kind == "account" { "crm_account" } else { "crm_contact" }, &from, MutationKind::Updated)] })
        })
        .await;
    match res {
        Ok(_) => (StatusCode::CREATED, Json(json!({ "ok": true }))).into_response(),
        Err(e) => internal(e),
    }
}

async fn delete_lookalike(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let s = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let (kind, from, to) = (s("kind"), s("from_id"), s("to_id"));
    let res = state
        .store
        .write_async(move |conn| {
            let (a, b) = if from < to { (from.clone(), to.clone()) } else { (to.clone(), from.clone()) };
            let n = conn.execute("DELETE FROM crm_lookalikes WHERE kind=?1 AND from_id=?2 AND to_id=?3", rusqlite::params![&kind, a, b])?;
            Ok(WriteOutcome { applied: n > 0, events: if n > 0 { vec![ev(if kind == "account" { "crm_account" } else { "crm_contact" }, &from, MutationKind::Updated)] } else { vec![] } })
        })
        .await;
    match res {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- summary -------------------------------------------------------------------------

async fn summary(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let conn = store.read()?;
        let n = |sql: &str| -> rusqlite::Result<i64> { conn.query_row(sql, [], |r| r.get(0)) };
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let due: i64 = conn.query_row(
            "SELECT COUNT(*) FROM crm_interactions i JOIN crm_contacts c ON c.id=i.contact_id WHERE i.follow_up_date IS NOT NULL AND i.follow_up_date<=?1 AND c.deleted IS NULL",
            [&today], |r| r.get(0))?;
        let mut stages = serde_json::Map::new();
        for s in STAGES {
            stages.insert((*s).into(), json!(conn.query_row("SELECT COUNT(*) FROM crm_deals WHERE deleted IS NULL AND stage=?1", [s], |r| r.get::<_, i64>(0))?));
        }
        let people = n("SELECT COUNT(*) FROM crm_contacts WHERE deleted IS NULL")?;
        // The owner's queue: drafted touches waiting for an approve.
        let to_approve = n("SELECT COUNT(*) FROM crm_touches WHERE deleted IS NULL AND status='drafted'").unwrap_or(0);
        // Of those, the ones that cannot go out as they stand: the voice check
        // failed with no stated reason to send anyway, or the person opted out, bounced or is do-not-contact. The
        // Outbound view's Blocked list uses the same predicate (app.js _crmBlocked).
        let blocked = n("SELECT COUNT(*) FROM crm_touches t LEFT JOIN crm_contacts c ON c.id=t.contact_id WHERE t.deleted IS NULL AND t.status='drafted' \
             AND ((json_extract(t.voice_check,'$.ok')=0 AND COALESCE(json_extract(t.voice_check,'$.override'),'')='') OR c.unsubscribed_at IS NOT NULL OR c.bounced_at IS NOT NULL OR c.status='do_not_contact' \
             OR (t.channel='email' AND json_valid(c.email_check) AND json_extract(c.email_check,'$.ok')=0))").unwrap_or(0);
        let decided = n("SELECT COUNT(*) FROM crm_touches WHERE deleted IS NULL AND status IN ('approved','done','skipped','failed')").unwrap_or(0);
        let watched = n("SELECT COUNT(*) FROM crm_channels WHERE deleted IS NULL AND watch=1").unwrap_or(0);
        Ok(json!({
            "people": people,
            "companies": n("SELECT COUNT(*) FROM crm_accounts WHERE deleted IS NULL")?,
            "deals": n("SELECT COUNT(*) FROM crm_deals WHERE deleted IS NULL")?,
            "followups_due": due,
            "to_approve": to_approve,
            "blocked": blocked,
            "decided": decided,
            "watched": watched,
            "stages": stages,
            "measured": true, "n_considered": people,
        }))
    })
    .await;
    match joined {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_are_bare_and_grades_are_a_to_d() {
        assert_eq!(norm_domain("https://www.Mixpeek.com/pricing?x=1"), "mixpeek.com");
        assert_eq!(norm_domain("acme.io"), "acme.io");
        assert_eq!(grade_of(&json!("b")).unwrap(), "B");
        assert_eq!(grade_of(&json!("")).unwrap(), "");
        assert!(grade_of(&json!("A+")).is_err());
        assert!(stage_of(&json!("Demo")).is_ok() && stage_of(&json!("closed")).is_err());
    }

    /// The CRM tab's whole round: people, companies, deals and lookalikes,
    /// with grades checked, duplicates refused, and who added what recorded.
    #[tokio::test]
    async fn people_companies_deals_and_lookalikes_round_trip() {
        use crate::api::standing_approvals::tests::{call, test_state};
        let app = crate::api::crm::routes().merge(routes()).with_state(test_state());
        let w = [("x-amux-session", "gtm-engine")];
        let (st, a) = call(&app, "POST", "/accounts", &w, Some(json!({"name": "Acme", "domain": "https://www.acme.io/x", "icp_grade": "a"}))).await;
        assert_eq!(st, StatusCode::CREATED, "{a}");
        let acme = a["id"].as_str().unwrap().to_string();
        let (st, d) = call(&app, "POST", "/accounts", &[], Some(json!({"name": "Acme again", "domain": "acme.io"}))).await;
        assert_eq!((st, d["id"].as_str()), (StatusCode::CONFLICT, Some(acme.as_str())), "one live company per domain");
        let (st, _) = call(&app, "POST", "/accounts", &[], Some(json!({"name": "Bad", "icp_grade": "A+"}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "grades are A to D");
        let (_, b) = call(&app, "POST", "/accounts", &[], Some(json!({"name": "Beta", "domain": "beta.dev"}))).await;
        let beta = b["id"].as_str().unwrap().to_string();

        let (st, p) = call(&app, "POST", "/contacts", &w, Some(json!({"name": "Ada", "email": "ada@acme.io", "account_id": acme, "icp_grade": "B", "status": "lead"}))).await;
        assert_eq!(st, StatusCode::CREATED, "{p}");
        let ada = p["id"].as_str().unwrap().to_string();
        let (st, d) = call(&app, "POST", "/contacts", &[], Some(json!({"name": "Ada L", "email": "ADA@acme.io"}))).await;
        assert_eq!((st, d["id"].as_str()), (StatusCode::CONFLICT, Some(ada.as_str())), "one person per email, any case");
        let (st, _) = call(&app, "POST", "/contacts", &[], Some(json!({"name": "X", "status": "vip"}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "statuses are a fixed set");

        let (st, _) = call(&app, "POST", "/lookalikes", &[], Some(json!({"kind": "account", "from_id": acme, "to_id": beta, "score": 0.8, "reason": "same buyer"}))).await;
        assert_eq!(st, StatusCode::CREATED);
        let (_, d) = call(&app, "POST", "/deals", &[], Some(json!({"title": "Acme pilot", "account_id": acme, "stage": "demo", "amount": 12000, "contact_ids": [ada]}))).await;
        let deal = d["id"].as_str().unwrap().to_string();
        let (st, _) = call(&app, "PATCH", &format!("/deals/{deal}"), &[], Some(json!({"stage": "closed"}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "stages are Lightfield's");

        let (_, p) = call(&app, "GET", &format!("/contacts/{ada}"), &[], None).await;
        assert_eq!((p["source"].as_str(), p["icp_grade"].as_str(), p["account"]["name"].as_str()), (Some("worker:gtm-engine"), Some("B"), Some("Acme")));
        assert!(p["icp_scored_at"].as_i64().is_some(), "a grade says when it was set");
        assert_eq!(p["deals"][0]["id"].as_str(), Some(deal.as_str()));
        assert_eq!(p["lookalikes"][0]["name"].as_str(), Some("Beta"), "a person shows their company's lookalikes");
        let (_, b) = call(&app, "GET", &format!("/accounts/{beta}"), &[], None).await;
        assert_eq!(b["lookalikes"][0]["id"].as_str(), Some(acme.as_str()), "a lookalike reads both ways");
        let (_, a) = call(&app, "GET", &format!("/accounts/{acme}"), &[], None).await;
        assert_eq!((a["domain"].as_str(), a["people"][0]["id"].as_str(), a["source"].as_str()), (Some("acme.io"), Some(ada.as_str()), Some("worker:gtm-engine")));

        let (_, l) = call(&app, "GET", "/contacts?grade=B&status=lead", &[], None).await;
        assert_eq!(l.as_array().map(|v| v.len()), Some(1));
        assert_eq!(l[0]["account_name"].as_str(), Some("Acme"));
        let (_, l) = call(&app, "GET", "/accounts?limit=1", &[], None).await;
        assert_eq!(l.as_array().map(|v| v.len()), Some(1), "a page of companies");
        let (_, l) = call(&app, "GET", "/accounts?limit=1&offset=1", &[], None).await;
        assert_eq!(l.as_array().map(|v| v.len()), Some(1), "and the next one");
        let (_, l) = call(&app, "GET", "/contacts?grade=A", &[], None).await;
        assert_eq!(l.as_array().map(|v| v.len()), Some(0));
        let (_, s) = call(&app, "GET", "/summary", &[], None).await;
        assert_eq!((s["people"].as_i64(), s["companies"].as_i64(), s["deals"].as_i64(), s["stages"]["demo"].as_i64()), (Some(1), Some(2), Some(1), Some(1)));

        // The same message logged twice is one interaction, and the second
        // answer names the first rather than a row that was never written.
        let msg = json!({"type": "email", "message_id": "<m1@acme.io>", "notes": "intro"});
        let (st, first) = call(&app, "POST", &format!("/contacts/{ada}/interactions"), &[], Some(msg.clone())).await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, again) = call(&app, "POST", &format!("/contacts/{ada}/interactions"), &[], Some(msg)).await;
        assert_eq!((st, again["id"].clone(), again["duplicate"].as_bool()), (StatusCode::OK, first["id"].clone(), Some(true)));
        let (_, p) = call(&app, "GET", &format!("/contacts/{ada}"), &[], None).await;
        assert_eq!(p["interactions"].as_array().map(|v| v.len()), Some(1));
        let (_, _) = call(&app, "PATCH", &format!("/contacts/{ada}"), &[], Some(json!({"unsubscribed": true, "icp_grade": ""}))).await;
        let (_, p) = call(&app, "GET", &format!("/contacts/{ada}"), &[], None).await;
        assert!(p["unsubscribed_at"].as_i64().is_some() && p["icp_grade"] == "" && p["icp_scored_at"].is_null());
    }

    #[test]
    fn source_names_the_worker_or_the_owner() {
        let mut h = HeaderMap::new();
        assert_eq!(source_of(&json!({}), &h), "owner");
        h.insert("x-amux-session", "gtm-engine".parse().unwrap());
        assert_eq!(source_of(&json!({}), &h), "worker:gtm-engine");
        assert_eq!(source_of(&json!({"source": "rb2b"}), &h), "rb2b");
    }

    /// gtm-engine's CRM pilot, 2026-11-25: a company had nowhere to put its
    /// own activity, a deal had no tags, a single deal could not be read, the
    /// contact list cut a large limit silently, and a held email was invisible
    /// on the person it was to.
    #[tokio::test]
    async fn company_activity_deal_tags_one_deal_page_headers_and_held_email() {
        use crate::api::standing_approvals::tests::{call, test_state};
        let home = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(home.path());
        std::fs::create_dir_all(crate::api::email_approval::approvals_dir(home.path())).unwrap();
        let app = crate::api::crm::routes().merge(routes()).with_state(test_state());
        let w = [("x-amux-session", "gtm-engine")];
        let (_, a) = call(&app, "POST", "/accounts", &w, Some(json!({"name": "Rival"}))).await;
        let acct = a["id"].as_str().unwrap().to_string();
        let (st, x) = call(&app, "POST", &format!("/accounts/{acct}/interactions"), &w,
            Some(json!({"type": "competitor", "subject": "pricing page", "notes": "new enterprise tier", "message_id": "m1"}))).await;
        assert_eq!(st, StatusCode::CREATED, "{x}");
        let (st, again) = call(&app, "POST", &format!("/accounts/{acct}/interactions"), &w, Some(json!({"notes": "x", "message_id": "m1"}))).await;
        assert_eq!((st, again["duplicate"].as_bool()), (StatusCode::OK, Some(true)));
        let (st, _) = call(&app, "POST", "/accounts/ACCT-404/interactions", &w, Some(json!({"notes": "x"}))).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (_, got) = call(&app, "GET", &format!("/accounts/{acct}"), &w, None).await;
        assert_eq!((got["activity"].as_array().map(Vec::len), got["activity"][0]["subject"].as_str()), (Some(1), Some("pricing page")), "{got}");
        let (_, list) = call(&app, "GET", "/accounts?q=Rival", &w, None).await;
        assert!(list[0]["last_date"].as_str().is_some_and(|d| !d.is_empty()), "{list}");

        let (_, p) = call(&app, "POST", "/contacts", &w, Some(json!({"name": "Ada", "email": "ada@rival.io", "account_id": acct}))).await;
        let ada = p["id"].as_str().unwrap().to_string();
        let (_, d) = call(&app, "POST", "/deals", &w, Some(json!({"title": "Rival pilot", "account_id": acct, "contact_ids": [ada], "tags": ["cohort-nov", "play-swap"]}))).await;
        let deal = d["id"].as_str().unwrap().to_string();
        let (st, one) = call(&app, "GET", &format!("/deals/{deal}"), &w, None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!((one["tags"].clone(), one["people"][0]["name"].as_str(), one["account_name"].as_str()),
            (json!(["cohort-nov", "play-swap"]), Some("Ada"), Some("Rival")), "{one}");
        call(&app, "PATCH", &format!("/deals/{deal}"), &w, Some(json!({"tags": ["cohort-dec"]}))).await;
        let (_, tagged) = call(&app, "GET", "/deals?tag=cohort-dec", &w, None).await;
        assert_eq!(tagged.as_array().map(Vec::len), Some(1));
        let (_, none) = call(&app, "GET", "/deals?tag=cohort-nov", &w, None).await;
        assert_eq!(none.as_array().map(Vec::len), Some(0));
        let (st, _) = call(&app, "GET", "/deals/DEAL-404", &w, None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        let q = |l: &str, o: &str| [("limit".to_string(), l.to_string()), ("offset".to_string(), o.to_string())].into_iter().collect::<HashMap<_, _>>();
        let h = crate::api::crm::page_headers(&q("6000", "0"), 5688, 5000);
        assert_eq!((h["x-amux-total"].to_str().unwrap(), h["x-amux-truncated"].to_str().unwrap(), h["x-amux-limit-capped"].to_str().unwrap()), ("5688", "1", "1"));
        let h = crate::api::crm::page_headers(&q("5000", "5000"), 5688, 688);
        assert_eq!((h["x-amux-truncated"].to_str().unwrap(), h["x-amux-limit-capped"].to_str().unwrap()), ("0", "0"));

        let apr = crate::api::email_approval::create_approval(home.path(), "gtm-engine", "send",
            json!({"to": "Ada <ADA@rival.io>", "subject": "Intro", "body": "hi"}), json!({"to": "ADA@rival.io", "subject": "Intro"})).unwrap();
        let (_, c) = call(&app, "GET", &format!("/contacts/{ada}"), &w, None).await;
        assert_eq!((c["held_emails"][0]["id"].as_str(), c["held_emails"][0]["subject"].as_str()), (Some(apr.as_str()), Some("Intro")), "{c}");
        assert!(crate::api::crm::held_emails_for("bo@rival.io").is_empty());
    }
}
