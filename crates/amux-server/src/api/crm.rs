//! CRM API: contacts, tags, interactions and follow-ups (AMUX-2929).
//!
//! **This is a PORT OF A LIVE CONTRACT, not a new feature.** The Python server
//! served `/api/crm/*` until it was deleted at 792ce1f; the SCHEMA came across
//! in `0001_baseline.sql` and the DATA is still here — 308 contacts, 72 tags,
//! 67 interactions on this machine — but the routes never did. So the tables
//! were being maintained by migrations while nothing could read them.
//!
//! Two things kept claiming otherwise, which is why this went unnoticed:
//!
//!   * global `~/.claude/CLAUDE.md` documents `amux crm add` and
//!     `POST /api/crm/contacts` as working primitives, to EVERY session in
//!     every project;
//!   * app.js's AMUX-2590 comment, which removed the People view, says
//!     "the /api/crm endpoints still exist for agents".
//!
//! Both were false: `POST /api/crm/contacts` answered 405 (the GET-only SPA
//! catch-all taking a non-GET — the tell CLAUDE.md's own observability table
//! describes) and `GET /api/crm` answered 404, with no crm route mounted at
//! all. Deleting the documentation instead would have been the cheaper fix and
//! the wrong one: it abandons 308 rows of somebody's real address book on the
//! strength of a missing route (ethos rule 8 — whose data is this?).
//!
//! CONTRACT FIDELITY. Shapes are matched to the Python implementation
//! (recovered from `792ce1f^:amux-server.py`) rather than redesigned, because
//! the callers — the bash CLI's `amux crm` verbs and any agent following
//! CLAUDE.md — were written against it:
//!
//!   GET    /api/crm/contacts[?q=]        list + LIKE search over name/company/role
//!   POST   /api/crm/contacts             201 {id, ok}; `name` required
//!   GET    /api/crm/contacts/{id}        contact + tags + interactions
//!   PATCH  /api/crm/contacts/{id}        whitelisted fields + full tag replace
//!   DELETE /api/crm/contacts/{id}        SOFT delete (sets `deleted`)
//!   POST   /api/crm/contacts/{id}/interactions   201 {id, ok}
//!   PATCH  /api/crm/interactions/{id}    whitelisted fields
//!   DELETE /api/crm/interactions/{id}    HARD delete (matches Python)
//!   GET    /api/crm/followups            pending follow-ups, soonest first
//!
//! The delete asymmetry (soft for contacts, hard for interactions) is Python's
//! and is preserved deliberately: `crm_contacts.deleted` exists and every read
//! filters on it, while `crm_interactions` has no such column. Changing either
//! would silently alter what the existing 308 rows mean.
//!
//! NOT PORTED: `_crm_sync_external`, which mirrored writes to a configured
//! external CRM. It reached a third-party API on every create and on notes
//! changes; porting it blind — without knowing which provider is configured or
//! whether those credentials are still live — would mean this server starts
//! making outbound calls nobody asked for. Left out, and named here so its
//! absence is a recorded decision rather than an oversight.

use super::calendar::query_rows_json;
use super::{internal, AppState};
use crate::db::board_store::next_issue_id;
use crate::db::{PendingEvent, WriteOutcome};
use amux_core::revision::{EntityType, MutationKind};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/contacts", get(list_contacts).post(create_contact))
        .route(
            "/contacts/{id}",
            get(get_contact).patch(patch_contact).delete(delete_contact),
        )
        .route("/contacts/{id}/interactions", post(add_interaction))
        .route(
            "/interactions/{id}",
            axum::routing::patch(patch_interaction).delete(delete_interaction),
        )
        .route("/followups", get(list_followups))
}

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn ev(id: &str, kind: MutationKind) -> PendingEvent {
    PendingEvent {
        entity_type: EntityType::Other("crm_contact".into()),
        entity_id: id.to_string(),
        mutation: kind,
        payload: None,
    }
}

/// Fields a client may write on a contact. Python's `allowed` set, verbatim —
/// notably NOT `id`, `created` or `deleted`, so a PATCH can neither re-key a
/// row nor resurrect a soft-deleted one by accident.
const CONTACT_FIELDS: &[&str] = &[
    "name", "company", "role", "email", "linkedin", "twitter", "phone", "notes",
    // CRM phase 1 (docs/design/crm.md): the company link, status, ICP reasons,
    // location and other systems' ids. icp_grade and the stop timestamps go
    // through their own checks in patch_contact.
    "account_id", "status", "icp_reasons", "location", "external",
];

/// Statuses a person can have; empty is "not set".
const STATUSES: &[&str] = &["lead", "active", "customer", "partner", "personal", "do_not_contact"];

/// Same, for an interaction.
const INTERACTION_FIELDS: &[&str] = &["date", "type", "notes", "follow_up_date", "follow_up_note", "direction", "subject", "deal_id"];

fn str_field(body: &Value, key: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Tags for one contact, as a JSON array of strings.
fn tags_for(conn: &rusqlite::Connection, id: &str) -> rusqlite::Result<Vec<Value>> {
    let mut st = conn.prepare("SELECT tag FROM crm_tags WHERE contact_id=?1 ORDER BY tag")?;
    let rows = st.query_map([id], |r| r.get::<_, String>(0))?;
    Ok(rows.filter_map(Result::ok).map(Value::from).collect())
}

/// Replace a contact's whole tag set. Python deletes then re-inserts, ignoring
/// duplicate-key errors; `INSERT OR IGNORE` expresses that without the
/// swallowed exception.
fn replace_tags(conn: &rusqlite::Connection, id: &str, tags: &[Value]) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM crm_tags WHERE contact_id=?1", [id])?;
    for t in tags {
        let t = t.as_str().unwrap_or("").trim();
        if !t.is_empty() {
            conn.execute(
                "INSERT OR IGNORE INTO crm_tags (contact_id, tag) VALUES (?1, ?2)",
                [id, t],
            )?;
        }
    }
    Ok(())
}

/// Emails to this address held for the owner's approval (gtm-engine,
/// 2026-11-25: apr_b92a3a8dd1ad259b sat 44 hours and the person's record could
/// not see it). Read from the approval store itself, so it can never disagree
/// with the Needs-you list: id, subject, the worker that drafted it, age.
pub(crate) fn held_emails_for(email: &str) -> Vec<Value> {
    let email = email.trim().to_lowercase();
    if !email.contains('@') {
        return vec![];
    }
    let home = crate::config::amux_home();
    super::email_approval::list_pending(&home)
        .into_iter()
        .filter(|d| {
            ["/preview/to", "/payload/to", "/preview/cc", "/payload/cc"].iter().any(|p| {
                d.pointer(p).and_then(Value::as_str).is_some_and(|v| {
                    v.split([',', ';']).any(|a| a.rsplit('<').next().unwrap_or(a).trim_end_matches('>').trim().eq_ignore_ascii_case(&email))
                })
            })
        })
        .map(|d| json!({
            "id": d["id"], "subject": d.pointer("/preview/subject").or_else(|| d.pointer("/payload/subject")).cloned().unwrap_or(Value::Null),
            "session": d["session"], "endpoint": d["endpoint"], "created": d["created"], "age_s": d["age_s"],
        }))
        .collect()
}

// ---- GET /api/crm/contacts -------------------------------------------------

/// The list view, with each contact's last-contacted date and soonest pending
/// follow-up. Python's ORDER BY is preserved exactly, including its oddity:
/// `CASE WHEN last_date IS NULL THEN 0 ELSE 1 END DESC, last_date ASC` puts
/// contacts you HAVE spoken to first, oldest first — i.e. "who is overdue" —
/// and never-contacted rows last. It reads like a bug and is the intended
/// order, so it is transcribed rather than tidied.
/// " LIMIT n OFFSET m" from `limit` and `offset`, or nothing (the whole list,
/// as callers before paging expect).
pub(crate) fn page_of(q: &HashMap<String, String>) -> String {
    match q.get("limit").and_then(|v| v.parse::<u64>().ok()).filter(|n| *n > 0) {
        Some(n) => format!(" LIMIT {} OFFSET {}", n.min(PAGE_MAX), q.get("offset").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)),
        None => String::new(),
    }
}

/// The most one page holds. A larger `limit` is cut to it, and the list says so.
pub(crate) const PAGE_MAX: u64 = 5000;

/// Headers that say how much of the list this response holds (the body is a
/// bare array, like the board's): `x-amux-total` rows match, `x-amux-returned`
/// came back, `x-amux-truncated: 1` when more remain past this page, and
/// `x-amux-limit-capped: 1` when the asked-for limit was over [`PAGE_MAX`]
/// (gtm-engine, 2026-11-25: limit=6000 returned 5,000 of 5,688 and said nothing).
pub(crate) fn page_headers(q: &HashMap<String, String>, total: usize, returned: usize) -> axum::http::HeaderMap {
    let mut h = axum::http::HeaderMap::new();
    let offset = q.get("offset").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    let asked = q.get("limit").and_then(|v| v.parse::<u64>().ok()).filter(|n| *n > 0);
    let capped = asked.is_some_and(|n| n > PAGE_MAX);
    let truncated = offset + returned < total;
    for (k, v) in [("x-amux-total", total.to_string()), ("x-amux-returned", returned.to_string()),
        ("x-amux-truncated", (truncated as u8).to_string()), ("x-amux-limit-capped", (capped as u8).to_string())] {
        if let Ok(v) = axum::http::HeaderValue::from_str(&v) {
            h.insert(k, v);
        }
    }
    if capped || truncated {
        tracing::info!(target: "amux::crm", total, returned, offset, capped, measured = true, n_considered = total,
            verdict = "crm_list_truncated", "the contact list returned one page of a longer list; the headers say how much");
    }
    h
}

pub async fn list_contacts(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let needle = q.get("q").map(|s| s.trim().to_string()).unwrap_or_default();
    // Filters the CRM tab sends: each an exact match on its column.
    let filters: Vec<(&'static str, String)> = [("status", "c.status"), ("grade", "c.icp_grade"), ("account_id", "c.account_id"), ("source", "c.source")]
        .into_iter()
        .filter_map(|(k, col)| q.get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).map(|v| (col, v)))
        .collect();
    let tag = q.get("tag").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let store = state.store.clone();
    let q2 = q.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<Value>, usize)> {
        let q = q2;
        let conn = store.read()?;
        let base = "SELECT c.id,c.name,c.company,c.role,c.email,c.linkedin,c.twitter,c.phone,\
             c.account_id,c.status,c.icp_grade,c.location,c.source,c.unsubscribed_at,c.bounced_at,\
             (SELECT name FROM crm_accounts a WHERE a.id=c.account_id AND a.deleted IS NULL) AS account_name,\
             (SELECT date FROM crm_interactions WHERE contact_id=c.id ORDER BY date DESC LIMIT 1) AS last_date,\
             (SELECT follow_up_date FROM crm_interactions WHERE contact_id=c.id AND follow_up_date IS NOT NULL ORDER BY follow_up_date ASC LIMIT 1) AS next_followup,\
             (SELECT follow_up_note FROM crm_interactions WHERE contact_id=c.id AND follow_up_date IS NOT NULL ORDER BY follow_up_date ASC LIMIT 1) AS next_followup_note \
             FROM crm_contacts c WHERE c.deleted IS NULL";
        let order = " ORDER BY CASE WHEN last_date IS NULL THEN 0 ELSE 1 END DESC, last_date ASC";
        let mut args: Vec<String> = Vec::new();
        let mut cond = String::new();
        if !needle.is_empty() {
            args.push(format!("%{needle}%"));
            cond.push_str(" AND (c.name LIKE ?1 OR c.company LIKE ?1 OR c.role LIKE ?1 OR c.email LIKE ?1)");
        }
        for (col, v) in &filters {
            args.push(v.clone());
            cond.push_str(&format!(" AND {col}=?{}", args.len()));
        }
        if let Some(t) = &tag {
            args.push(t.clone());
            cond.push_str(&format!(" AND EXISTS (SELECT 1 FROM crm_tags t WHERE t.contact_id=c.id AND t.tag=?{})", args.len()));
        }
        // A page, when asked for: the CRM tab reads 200 at a time (gtm-engine
        // imported 4,292 LinkedIn connections on 2026-11-23, 1.4 MB as one list).
        let page = page_of(&q);
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a as &dyn rusqlite::ToSql).collect();
        let total: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM crm_contacts c WHERE c.deleted IS NULL{cond}"), refs.as_slice(), |r| r.get(0))?;
        let mut rows = query_rows_json(&conn, &format!("{base}{cond}{order}{page}"), &refs)?;
        for r in rows.iter_mut() {
            let id = r.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            if let Some(o) = r.as_object_mut() {
                o.insert("tags".into(), Value::Array(tags_for(&conn, &id)?));
            }
        }
        Ok((rows, total as usize))
    })
    .await;
    match joined {
        Ok(Ok((rows, total))) => (page_headers(&q, total, rows.len()), Json(Value::Array(rows))).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- POST /api/crm/contacts ------------------------------------------------

pub async fn create_contact(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let name = str_field(&body, "name").trim().to_string();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "name required" }));
    }
    let grade = match super::crm_accounts::grade_of(body.get("icp_grade").unwrap_or(&Value::Null)) {
        Ok(g) => g,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    let status = str_field(&body, "status").trim().to_string();
    if !status.is_empty() && !STATUSES.contains(&status.as_str()) {
        return err(StatusCode::BAD_REQUEST, json!({ "error": format!("status must be one of {}", STATUSES.join(", ")) }));
    }
    let source = super::crm_accounts::source_of(&body, &headers);
    let email = str_field(&body, "email").trim().to_string();
    let allow_dup = body.get("allow_duplicate").and_then(Value::as_bool) == Some(true);
    // Ok(new id) or Err(the live contact that already has this email).
    let slot: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let write = state
        .store
        .write_async(move |conn| {
            // One person per email: the audit found the same people entered
            // two and three times by different workers.
            if !email.is_empty() && !allow_dup {
                let dup: Option<String> = conn
                    .query_row("SELECT id FROM crm_contacts WHERE lower(email)=lower(?1) AND deleted IS NULL", [&email], |r| r.get(0))
                    .ok();
                if let Some(d) = dup {
                    *slot_w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Err(d));
                    return Ok(WriteOutcome { applied: false, events: vec![] });
                }
            }
            let now = now_secs();
            let cid = next_issue_id(conn, "PPL")?;
            conn.execute(
                "INSERT INTO crm_contacts (id,name,company,role,email,linkedin,twitter,phone,notes,created,updated,\
                 account_id,status,icp_grade,icp_reasons,icp_scored_at,location,source,external) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
                rusqlite::params![
                    &cid,
                    &name,
                    &str_field(&body, "company"),
                    &str_field(&body, "role"),
                    &email,
                    &str_field(&body, "linkedin"),
                    &str_field(&body, "twitter"),
                    &str_field(&body, "phone"),
                    &str_field(&body, "notes"),
                    now,
                    &str_field(&body, "account_id"),
                    &status,
                    &grade,
                    &str_field(&body, "icp_reasons"),
                    if grade.is_empty() { None } else { Some(now) },
                    &str_field(&body, "location"),
                    &source,
                    body.get("external").filter(|e| e.is_object()).map(Value::to_string).unwrap_or_else(|| "{}".into()),
                ],
            )?;
            if let Some(tags) = body.get("tags").and_then(Value::as_array) {
                replace_tags(conn, &cid, tags)?;
            }
            *slot_w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Ok(cid.clone()));
            Ok(WriteOutcome { applied: true, events: vec![ev(&cid, MutationKind::Created)] })
        })
        .await;
    if let Err(e) = write {
        return internal(e);
    }
    let taken = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
    match taken {
        Some(Ok(id)) => (StatusCode::CREATED, Json(json!({ "id": id, "ok": true }))).into_response(),
        Some(Err(dup)) => {
            tracing::info!(target: "amux::crm", existing = %dup, measured = true, n_considered = 1, verdict = "crm_contact_duplicate_email",
                "a contact with this email already exists; nothing was created");
            err(StatusCode::CONFLICT, json!({ "error": "a contact with this email already exists", "code": "duplicate", "id": dup,
                "next": "PATCH /api/crm/contacts/<id> to update it, or send allow_duplicate: true" }))
        }
        None => internal(anyhow::anyhow!("create_contact wrote nothing")),
    }
}

// ---- GET /api/crm/contacts/{id} --------------------------------------------

pub async fn get_contact(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let conn = store.read()?;
        let mut rows = query_rows_json(
            &conn,
            "SELECT * FROM crm_contacts WHERE id=?1 AND deleted IS NULL",
            &[&id as &dyn rusqlite::ToSql],
        )?;
        if rows.is_empty() {
            return Ok(None);
        }
        let mut c = rows.remove(0);
        let interactions = query_rows_json(
            &conn,
            "SELECT * FROM crm_interactions WHERE contact_id=?1 ORDER BY date DESC, created DESC",
            &[&id as &dyn rusqlite::ToSql],
        )?;
        let account_id = c.get("account_id").and_then(Value::as_str).unwrap_or("").to_string();
        let account = if account_id.is_empty() {
            Value::Null
        } else {
            query_rows_json(&conn, "SELECT id,name,domain,industry,size,location,website,icp_grade,icp_reasons FROM crm_accounts WHERE id=?1 AND deleted IS NULL",
                &[&account_id as &dyn rusqlite::ToSql])?.into_iter().next().unwrap_or(Value::Null)
        };
        let deals = query_rows_json(&conn,
            "SELECT id,title,stage,amount,currency,close_date,next_step FROM crm_deals WHERE deleted IS NULL AND (contact_ids LIKE ?1 OR (account_id<>'' AND account_id=?2)) ORDER BY updated DESC",
            &[&format!("%\"{id}\"%") as &dyn rusqlite::ToSql, &account_id])?;
        let looks = if account_id.is_empty() { vec![] } else { super::crm_accounts::lookalikes_of(&conn, "account", &account_id)? };
        if let Some(o) = c.as_object_mut() {
            o.insert("tags".into(), Value::Array(tags_for(&conn, &id)?));
            o.insert("interactions".into(), Value::Array(interactions));
            o.insert("account".into(), account);
            o.insert("deals".into(), Value::Array(deals));
            o.insert("lookalikes".into(), Value::Array(looks));
            let email = o.get("email").and_then(Value::as_str).unwrap_or("").to_string();
            o.insert("held_emails".into(), Value::Array(held_emails_for(&email)));
        }
        Ok(Some(c))
    })
    .await;
    match joined {
        Ok(Ok(Some(c))) => Json(c).into_response(),
        Ok(Ok(None)) => err(StatusCode::NOT_FOUND, json!({ "error": "not found" })),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- PATCH /api/crm/contacts/{id} ------------------------------------------

pub async fn patch_contact(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    // Checked fields: the grade (A to D, and when it was set), the status, and
    // the outbound stops, set to now by `true` and cleared by `false`.
    let mut checked: Vec<(&'static str, SqlValue)> = Vec::new();
    if let Some(g) = body.get("icp_grade") {
        match super::crm_accounts::grade_of(g) {
            Ok(g) => {
                checked.push(("icp_scored_at", if g.is_empty() { SqlValue::Null } else { SqlValue::Integer(now_secs()) }));
                checked.push(("icp_grade", SqlValue::Text(g)));
            }
            Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
        }
    }
    if let Some(s) = body.get("status").and_then(Value::as_str) {
        if !s.is_empty() && !STATUSES.contains(&s) {
            return err(StatusCode::BAD_REQUEST, json!({ "error": format!("status must be one of {}", STATUSES.join(", ")) }));
        }
    }
    for f in ["unsubscribed", "bounced"] {
        if let Some(b) = body.get(f).and_then(Value::as_bool) {
            checked.push((if f == "unsubscribed" { "unsubscribed_at" } else { "bounced_at" }, if b { SqlValue::Integer(now_secs()) } else { SqlValue::Null }));
        }
    }
    let write = state
        .store
        .write_async(move |conn| {
            let now = now_secs();
            let mut sets: Vec<String> = Vec::new();
            let mut params: Vec<SqlValue> = Vec::new();
            for (f, v) in &checked {
                params.push(v.clone());
                sets.push(format!("{f}=?{}", params.len()));
            }
            for f in CONTACT_FIELDS {
                if let Some(v) = body.get(*f) {
                    // Python assigns the raw value; a non-string lands in a TEXT
                    // column and SQLite coerces it. Stringify explicitly so a
                    // number or bool cannot become a typed cell in a column every
                    // other row holds as TEXT.
                    let s = match v {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    params.push(SqlValue::Text(s));
                    sets.push(format!("{f}=?{}", params.len()));
                }
            }
            if !sets.is_empty() {
                params.push(SqlValue::Integer(now));
                let updated_ix = params.len();
                params.push(SqlValue::Text(id.clone()));
                let id_ix = params.len();
                let sql = format!(
                    "UPDATE crm_contacts SET {}, updated=?{updated_ix} WHERE id=?{id_ix}",
                    sets.join(", ")
                );
                conn.execute(&sql, rusqlite::params_from_iter(params.iter()))?;
            }
            if let Some(tags) = body.get("tags").and_then(Value::as_array) {
                replace_tags(conn, &id, tags)?;
            }
            Ok(WriteOutcome {
                applied: true,
                events: vec![ev(&id, MutationKind::Updated)],
            })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- DELETE /api/crm/contacts/{id} -----------------------------------------

/// SOFT delete — sets `deleted` to now. Every read filters `deleted IS NULL`,
/// so the row disappears from the API while staying recoverable. Python did
/// the same; a hard delete here would destroy history the schema was built to
/// keep.
pub async fn delete_contact(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let write = state
        .store
        .write_async(move |conn| {
            let n = conn.execute(
                "UPDATE crm_contacts SET deleted=?1 WHERE id=?2",
                rusqlite::params![now_secs(), &id],
            )?;
            let events = if n > 0 {
                vec![ev(&id, MutationKind::Deleted)]
            } else {
                vec![]
            };
            Ok(WriteOutcome {
                applied: n > 0,
                events,
            })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- POST /api/crm/contacts/{id}/interactions ------------------------------

pub async fn add_interaction(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let booked: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let booked_w = booked.clone();
    let cid = id.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let now = now_secs();
            // Python: `body.get("date", today)` — the caller's local date, and
            // the column is TEXT ISO-8601, not an epoch.
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let date = match body.get("date").and_then(Value::as_str) {
                Some(d) if !d.trim().is_empty() => d.trim().to_string(),
                _ => today,
            };
            // Python used secrets.token_urlsafe(10). A ULID is equally opaque
            // to callers (the id is only ever echoed back and passed to
            // DELETE /api/crm/interactions/{id}), is already a dependency of
            // this crate, and sorts by creation time. Existing rows keep their
            // token-shaped ids; nothing parses the format.
            let ix_id: String = ulid::Ulid::new().to_string();
            let ty = match body.get("type").and_then(Value::as_str) {
                Some(t) if !t.trim().is_empty() => t.trim().to_string(),
                _ => "other".to_string(),
            };
            // NULL, not "": the list query keys "has a pending follow-up" off
            // `follow_up_date IS NOT NULL`, so an empty string would make every
            // interaction look like a pending follow-up.
            let fud = body
                .get("follow_up_date")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| SqlValue::Text(s.to_string()))
                .unwrap_or(SqlValue::Null);
            let inserted = conn.execute(
                "INSERT OR IGNORE INTO crm_interactions (id,contact_id,date,type,notes,follow_up_date,follow_up_note,created,updated,direction,subject,message_id,deal_id) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8,?9,?10,?11,?12)",
                rusqlite::params![
                    &ix_id,
                    &id,
                    &date,
                    &ty,
                    &str_field(&body, "notes"),
                    fud,
                    &str_field(&body, "follow_up_note"),
                    now,
                    &str_field(&body, "direction"),
                    &str_field(&body, "subject"),
                    &str_field(&body, "message_id"),
                    &str_field(&body, "deal_id"),
                ],
            )?;
            // The same message logged twice for a person is one interaction
            // (the index on contact and message id): say which one, never a
            // made-up id for a row that was not written.
            if inserted == 0 {
                let existing: Option<String> = conn
                    .query_row("SELECT id FROM crm_interactions WHERE contact_id=?1 AND message_id=?2",
                        rusqlite::params![&id, &str_field(&body, "message_id")], |r| r.get(0))
                    .ok();
                *slot_w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(format!("dup:{}", existing.unwrap_or_default()));
                return Ok(WriteOutcome { applied: false, events: vec![] });
            }
            conn.execute(
                "UPDATE crm_contacts SET updated=?1 WHERE id=?2",
                rusqlite::params![now, &id],
            )?;
            // A reply logged by hand (direction in, a mail or a message) is a
            // reply like a captured one: it stops the sequences that stop on
            // one, honours a request to stop, and opens the deal for its intent.
            if str_field(&body, "direction") == "in" && matches!(ty.as_str(), "email" | "dm" | "text" | "call") {
                super::crm_outbound::on_reply(conn, &id, &format!("{}\n{}", str_field(&body, "subject"), str_field(&body, "notes")))?;
            }
            // A booked meeting (gtm-engine logs a /demo booking as a `meeting`
            // with "calendly <uuid>" in its notes) ends the person's sequences.
            if matches!(ty.as_str(), "meeting" | "booking") {
                let (sequences, touches) = super::crm_outbound::on_booking(conn, &id)?;
                // It also moves their deal to demo and plans the prep (row 35).
                let (stage, prep) = super::crm_close::on_booking_deal(conn, &id)?;
                let name: String = conn.query_row("SELECT name FROM crm_contacts WHERE id=?1", [&id], |r| r.get(0)).unwrap_or_default();
                *booked_w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(json!({
                    "contact_id": id.clone(), "name": name, "interaction_id": ix_id.clone(), "type": ty.clone(), "date": date.clone(),
                    "notes": str_field(&body, "notes").chars().take(300).collect::<String>(),
                    "stopped_sequences": sequences, "skipped_touches": touches,
                    "deal": stage, "prep_touch_id": prep,
                }));
            }
            *slot_w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ix_id);
            Ok(WriteOutcome { applied: true, events: vec![ev(&id, MutationKind::Updated)] })
        })
        .await;
    let booking = booked.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
    if let Some(b) = booking {
        if let Some(d) = b.get("deal").filter(|d| !d.is_null()) {
            super::crm_agent::emit(&state, "crm.deal.stage", d.clone()).await;
        }
        super::crm_close::emit_funnel_changed(&state, "booked", &cid).await;
        super::crm_agent::emit(&state, "crm.contact.booked", b).await;
        tracing::info!(target: "amux::crm", contact = %cid, measured = true, n_considered = 1, verdict = "crm_contact_booked", "a booked meeting was recorded for a person");
    }
    match write {
        Ok(_) => {
            let ix = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take().unwrap_or_default();
            match ix.strip_prefix("dup:") {
                Some(existing) => {
                    tracing::info!(target: "amux::crm", existing, measured = true, n_considered = 1, verdict = "crm_interaction_duplicate_message",
                        "this message is already logged for this person; nothing was added");
                    (StatusCode::OK, Json(json!({ "id": existing, "ok": true, "duplicate": true }))).into_response()
                }
                None => (StatusCode::CREATED, Json(json!({ "id": ix, "ok": true }))).into_response(),
            }
        }
        Err(e) => internal(e),
    }
}

// ---- PATCH / DELETE /api/crm/interactions/{id} -----------------------------

pub async fn patch_interaction(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let write = state
        .store
        .write_async(move |conn| {
            let mut sets: Vec<String> = Vec::new();
            let mut params: Vec<SqlValue> = Vec::new();
            for f in INTERACTION_FIELDS {
                if let Some(v) = body.get(*f) {
                    // follow_up_date is nullable and MEANS something when null
                    // (see add_interaction) — clearing it must write NULL.
                    let cell = match v {
                        Value::Null => SqlValue::Null,
                        Value::String(s) if s.trim().is_empty() && *f == "follow_up_date" => {
                            SqlValue::Null
                        }
                        Value::String(s) => SqlValue::Text(s.clone()),
                        other => SqlValue::Text(other.to_string()),
                    };
                    params.push(cell);
                    sets.push(format!("{f}=?{}", params.len()));
                }
            }
            if sets.is_empty() {
                return Ok(WriteOutcome {
                    applied: false,
                    events: vec![],
                });
            }
            params.push(SqlValue::Integer(now_secs()));
            let updated_ix = params.len();
            params.push(SqlValue::Text(id.clone()));
            let id_ix = params.len();
            let sql = format!(
                "UPDATE crm_interactions SET {}, updated=?{updated_ix} WHERE id=?{id_ix}",
                sets.join(", ")
            );
            let n = conn.execute(&sql, rusqlite::params_from_iter(params.iter()))?;
            Ok(WriteOutcome {
                applied: n > 0,
                events: vec![],
            })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

/// HARD delete, matching Python — `crm_interactions` has no `deleted` column.
pub async fn delete_interaction(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let write = state
        .store
        .write_async(move |conn| {
            let n = conn.execute("DELETE FROM crm_interactions WHERE id=?1", [&id])?;
            Ok(WriteOutcome {
                applied: n > 0,
                events: vec![],
            })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- GET /api/crm/followups ------------------------------------------------

pub async fn list_followups(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        Ok(query_rows_json(
            &conn,
            "SELECT c.id,c.name,c.company,i.follow_up_date,i.follow_up_note \
             FROM crm_interactions i JOIN crm_contacts c ON c.id=i.contact_id \
             WHERE i.follow_up_date IS NOT NULL AND c.deleted IS NULL \
             ORDER BY i.follow_up_date ASC",
            &[],
        )?)
    })
    .await;
    match joined {
        Ok(Ok(rows)) => Json(Value::Array(rows)).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}
