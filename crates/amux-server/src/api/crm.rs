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
use axum::http::StatusCode;
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
];

/// Same, for an interaction.
const INTERACTION_FIELDS: &[&str] = &["date", "type", "notes", "follow_up_date", "follow_up_note"];

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

// ---- GET /api/crm/contacts -------------------------------------------------

/// The list view, with each contact's last-contacted date and soonest pending
/// follow-up. Python's ORDER BY is preserved exactly, including its oddity:
/// `CASE WHEN last_date IS NULL THEN 0 ELSE 1 END DESC, last_date ASC` puts
/// contacts you HAVE spoken to first, oldest first — i.e. "who is overdue" —
/// and never-contacted rows last. It reads like a bug and is the intended
/// order, so it is transcribed rather than tidied.
pub async fn list_contacts(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let needle = q.get("q").map(|s| s.trim().to_string()).unwrap_or_default();
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        let base = "SELECT c.id,c.name,c.company,c.role,c.email,c.linkedin,c.twitter,c.phone,\
             (SELECT date FROM crm_interactions WHERE contact_id=c.id ORDER BY date DESC LIMIT 1) AS last_date,\
             (SELECT follow_up_date FROM crm_interactions WHERE contact_id=c.id AND follow_up_date IS NOT NULL ORDER BY follow_up_date ASC LIMIT 1) AS next_followup,\
             (SELECT follow_up_note FROM crm_interactions WHERE contact_id=c.id AND follow_up_date IS NOT NULL ORDER BY follow_up_date ASC LIMIT 1) AS next_followup_note \
             FROM crm_contacts c WHERE c.deleted IS NULL";
        let order = " ORDER BY CASE WHEN last_date IS NULL THEN 0 ELSE 1 END DESC, last_date ASC";
        let mut rows = if needle.is_empty() {
            query_rows_json(&conn, &format!("{base}{order}"), &[])?
        } else {
            let like = format!("%{needle}%");
            query_rows_json(
                &conn,
                &format!("{base} AND (c.name LIKE ?1 OR c.company LIKE ?1 OR c.role LIKE ?1){order}"),
                &[&like as &dyn rusqlite::ToSql],
            )?
        };
        for r in rows.iter_mut() {
            let id = r.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            if let Some(o) = r.as_object_mut() {
                o.insert("tags".into(), Value::Array(tags_for(&conn, &id)?));
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

// ---- POST /api/crm/contacts ------------------------------------------------

pub async fn create_contact(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let name = str_field(&body, "name").trim().to_string();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "name required" }));
    }
    let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let now = now_secs();
            let cid = next_issue_id(conn, "PPL")?;
            conn.execute(
                "INSERT INTO crm_contacts (id,name,company,role,email,linkedin,twitter,phone,notes,created,updated) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10)",
                rusqlite::params![
                    &cid,
                    &name,
                    &str_field(&body, "company"),
                    &str_field(&body, "role"),
                    &str_field(&body, "email"),
                    &str_field(&body, "linkedin"),
                    &str_field(&body, "twitter"),
                    &str_field(&body, "phone"),
                    &str_field(&body, "notes"),
                    now,
                ],
            )?;
            if let Some(tags) = body.get("tags").and_then(Value::as_array) {
                replace_tags(conn, &cid, tags)?;
            }
            *slot_w.lock().expect("slot") = Some(cid.clone());
            Ok(WriteOutcome { applied: true, events: vec![ev(&cid, MutationKind::Created)] })
        })
        .await;
    match write {
        Ok(_) => {
            let id = slot.lock().expect("slot").take().unwrap_or_default();
            (StatusCode::CREATED, Json(json!({ "id": id, "ok": true }))).into_response()
        }
        Err(e) => internal(e),
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
        if let Some(o) = c.as_object_mut() {
            o.insert("tags".into(), Value::Array(tags_for(&conn, &id)?));
            o.insert("interactions".into(), Value::Array(interactions));
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
    let write = state
        .store
        .write_async(move |conn| {
            let now = now_secs();
            let mut sets: Vec<String> = Vec::new();
            let mut params: Vec<SqlValue> = Vec::new();
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
            conn.execute(
                "INSERT INTO crm_interactions (id,contact_id,date,type,notes,follow_up_date,follow_up_note,created,updated) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8)",
                rusqlite::params![
                    &ix_id,
                    &id,
                    &date,
                    &ty,
                    &str_field(&body, "notes"),
                    fud,
                    &str_field(&body, "follow_up_note"),
                    now,
                ],
            )?;
            conn.execute(
                "UPDATE crm_contacts SET updated=?1 WHERE id=?2",
                rusqlite::params![now, &id],
            )?;
            *slot_w.lock().expect("slot") = Some(ix_id);
            Ok(WriteOutcome { applied: true, events: vec![ev(&id, MutationKind::Updated)] })
        })
        .await;
    match write {
        Ok(_) => {
            let ix = slot.lock().expect("slot").take().unwrap_or_default();
            (StatusCode::CREATED, Json(json!({ "id": ix, "ok": true }))).into_response()
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
