//! Journal API: `/api/journal` over the live `journal_entries` /
//! `journal_media` tables, route- and field-compatible with the Python
//! handlers so the dashboard's journal tab works unchanged.
//!
//! Python parity decisions, recorded so they are not "fixed" later:
//! - Entry ids are `JRN-N` from the SHARED `issue_counters` table
//!   (`_next_issue_id("JRN")`); media ids are `secrets.token_urlsafe(10)`
//!   shaped (10 random bytes, base64url, no padding — same generator as
//!   crm.rs interaction ids).
//! - Media UPLOAD is **JSON + base64**, exactly like Python — NOT multipart
//!   and NOT the raw-body shape files.rs uses, because the SPA sends
//!   `{"data": "<dataURL or base64>", "name": "photo.jpg"}`:
//!   `curl -sk -X POST -H 'Content-Type: application/json' \
//!      -d '{"data":"data:image/png;base64,iVBORw0K...","name":"p.png"}' \
//!      $URL/api/journal/JRN-1/media`
//!   Bytes live on DISK under `<amux home>/journal-media/<id><ext>` (ext from
//!   magic bytes, not the client), only metadata in `journal_media`. Serving
//!   probes `.jpeg/.jpg/.png/.webp/.gif` on disk and answers with
//!   `Cache-Control: public, max-age=86400` (Python `_raw(..., cache=True)`).
//! - Entry DELETE is soft (`deleted=now`); media DELETE is hard (row + file)
//!   — that asymmetry is Python's.
//! - PATCH/DELETE answer `{"ok": true}` without existence checks, and a
//!   PATCH with no allowed fields skips the UPDATE entirely (no `updated`
//!   bump) — Python's `if fields:` guard.
//! - An id that does not match Python's `[A-Z]+-\d+` route regex falls
//!   through to the same `{"error": "journal route not found"}` 404.
//! - Undecodable base64 answers 400 here where Python answers 500 — the one
//!   deliberate deviation (an input error is the client's, not the
//!   server's); nothing in the SPA branches on that status.
//! - Python's in-process `_journal_version` counter has no consumer in the
//!   Rust server; writes publish StateEvents instead.

use super::calendar::query_rows_json;
use super::settings::amux_home;
use super::AppState;
use crate::db::board_store::next_issue_id;
use crate::db::{PendingEvent, WriteOutcome};
use amux_core::revision::{EntityType, MutationKind};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_entries).post(create_entry))
        .route("/tags", get(tag_counts))
        .route("/config", get(get_config).post(set_config))
        .route("/import", axum::routing::post(import_entries))
        .route("/media/{id}", get(serve_media).delete(delete_media))
        .route("/{id}", get(get_entry).patch(patch_entry).delete(delete_entry))
        .route("/{id}/media", axum::routing::post(upload_media))
        // Python's trailing `{"error": "journal route not found"}` for
        // anything else under /api/journal/.
        .fallback(|| async { journal_not_found() })
}

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

use super::internal;

fn journal_not_found() -> Response {
    err(StatusCode::NOT_FOUND, json!({ "error": "journal route not found" }))
}

fn ev(entity: &str, id: &str, mutation: MutationKind) -> PendingEvent {
    PendingEvent {
        entity_type: EntityType::Other(entity.into()),
        entity_id: id.to_string(),
        mutation,
        payload: None,
    }
}

/// Python's `CC_JOURNAL_MEDIA = CC_HOME / "journal-media"`, resolved
/// per-request so tests can point AMUX_HOME at a temp dir.
fn media_dir() -> PathBuf {
    amux_home().join("journal-media")
}

/// Python's entry-route regex `^[A-Z]+-\d+$`: uppercase prefix, one hyphen,
/// digits. Anything else falls through to the journal 404.
fn valid_entry_id(id: &str) -> bool {
    match id.split_once('-') {
        Some((p, n)) => {
            !p.is_empty()
                && p.chars().all(|c| c.is_ascii_uppercase())
                && !n.is_empty()
                && n.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

/// `secrets.token_urlsafe(10)` shape (14 urlsafe chars): ULID's 80-bit
/// random field is exactly 10 CSPRNG bytes — same trick as crm.rs.
fn media_id() -> String {
    let bytes = ulid::Ulid::new().0.to_be_bytes();
    crate::integrations::email::base64url_nopad(&bytes[6..16])
}

/// Python `body.get(k, "")`.
fn body_str(body: &Value, k: &str) -> String {
    body.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Python truthiness for `1 if body.get("starred") else 0`.
fn starred_int(body: &Value) -> i64 {
    body.get("starred").map(super::settings::truthy).unwrap_or(false) as i64
}

/// Python's tags normalization: a list joins trimmed non-empty items with
/// ","; a string passes through; anything else is "".
fn tags_str(body: &Value, key: &str) -> String {
    match body.get(key) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// JSON -> SQLite with Python's sqlite3 adapters (True -> 1).
fn sql_value(v: &Value) -> rusqlite::types::Value {
    match v {
        Value::Null => rusqlite::types::Value::Null,
        Value::Bool(b) => rusqlite::types::Value::Integer(*b as i64),
        Value::Number(n) if n.is_i64() => rusqlite::types::Value::Integer(n.as_i64().unwrap_or(0)),
        Value::Number(n) => rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => rusqlite::types::Value::Text(s.clone()),
        other => rusqlite::types::Value::Text(other.to_string()),
    }
}

const MEDIA_EXTS: [(&str, &str); 5] = [
    (".jpeg", "image/jpeg"),
    (".jpg", "image/jpeg"),
    (".png", "image/png"),
    (".webp", "image/webp"),
    (".gif", "image/gif"),
];

/// The media projection Python attaches to every entry.
fn media_for_entry(conn: &Connection, eid: &str) -> rusqlite::Result<Vec<Value>> {
    query_rows_json(
        conn,
        "SELECT id, filename, mime, position FROM journal_media WHERE entry_id=?1 ORDER BY position",
        &[&eid],
    )
}

// ---- GET /api/journal -------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct ListParams {
    #[serde(default)]
    q: String,
    #[serde(default)]
    tag: String,
    #[serde(default, rename = "from")]
    from_d: String,
    #[serde(default, rename = "to")]
    to_d: String,
    #[serde(default)]
    has_media: String,
    #[serde(default)]
    has_location: String,
}

async fn list_entries(State(state): State<AppState>, Query(p): Query<ListParams>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        let q = p.q.trim().to_string();
        let tag = p.tag.trim().to_string();
        let mut sql = String::from("SELECT * FROM journal_entries WHERE deleted IS NULL");
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        if !q.is_empty() {
            sql.push_str(
                " AND (text LIKE ? OR place_name LIKE ? OR tags LIKE ? OR prompt1 LIKE ? OR prompt2 LIKE ? OR prompt3 LIKE ?)",
            );
            for _ in 0..6 {
                params.push(rusqlite::types::Value::Text(format!("%{q}%")));
            }
        }
        if !tag.is_empty() {
            sql.push_str(" AND (',' || tags || ',' LIKE ?)");
            params.push(rusqlite::types::Value::Text(format!("%,{tag},%")));
        }
        if !p.from_d.is_empty() {
            sql.push_str(" AND date >= ?");
            params.push(rusqlite::types::Value::Text(p.from_d.clone()));
        }
        if !p.to_d.is_empty() {
            sql.push_str(" AND date <= ?");
            params.push(rusqlite::types::Value::Text(p.to_d.clone()));
        }
        if p.has_location == "1" {
            sql.push_str(" AND lat IS NOT NULL");
        }
        if p.has_media == "1" {
            sql.push_str(" AND id IN (SELECT entry_id FROM journal_media)");
        }
        sql.push_str(" ORDER BY date DESC, created DESC");
        let refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p as &dyn rusqlite::types::ToSql).collect();
        let mut entries = query_rows_json(&conn, &sql, &refs)?;
        for e in &mut entries {
            let eid = e.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            e["media"] = Value::Array(media_for_entry(&conn, &eid)?);
        }
        Ok(entries)
    })
    .await;
    match joined {
        Ok(Ok(rows)) => Json(Value::Array(rows)).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- GET /api/journal/tags --------------------------------------------------

async fn tag_counts(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Map<String, Value>> {
        let conn = store.read()?;
        let mut stmt =
            conn.prepare("SELECT tags FROM journal_entries WHERE deleted IS NULL AND tags != ''")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut counts: Map<String, Value> = Map::new();
        for tags in rows.flatten() {
            for t in tags.split(',') {
                let t = t.trim();
                if !t.is_empty() {
                    let n = counts.get(t).and_then(Value::as_i64).unwrap_or(0);
                    counts.insert(t.to_string(), json!(n + 1));
                }
            }
        }
        Ok(counts)
    })
    .await;
    match joined {
        Ok(Ok(counts)) => Json(Value::Object(counts)).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- GET/POST /api/journal/config ------------------------------------------

async fn get_config(State(state): State<AppState>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let mut prompts = Map::new();
    for i in 1..=3 {
        let v: Option<String> = conn
            .query_row(
                "SELECT value FROM prefs WHERE key=?1",
                [format!("journal_prompt_{i}")],
                |r| r.get(0),
            )
            .ok();
        prompts.insert(format!("prompt{i}"), Value::String(v.unwrap_or_default()));
    }
    Json(Value::Object(prompts)).into_response()
}

async fn set_config(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let vals: Vec<String> = (1..=3).map(|i| body_str(&body, &format!("prompt{i}"))).collect();
    let write = state
        .store
        .write_async(move |conn| {
            for (i, val) in vals.iter().enumerate() {
                conn.execute(
                    "INSERT INTO prefs (key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2",
                    rusqlite::params![format!("journal_prompt_{}", i + 1), val],
                )?;
            }
            Ok(WriteOutcome {
                applied: true,
                events: vec![ev("journal_config", "prompts", MutationKind::Updated)],
            })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- POST /api/journal ------------------------------------------------------

async fn create_entry(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let now = chrono::Utc::now().timestamp();
    let date_val = {
        let d = body_str(&body, "date");
        if d.is_empty() { chrono::Local::now().format("%Y-%m-%d").to_string() } else { d }
    };
    let tags = tags_str(&body, "tags");
    let text = body_str(&body, "text");
    let place_name = body_str(&body, "place_name");
    let starred = starred_int(&body);
    let lat = sql_value(body.get("lat").unwrap_or(&Value::Null));
    let lng = sql_value(body.get("lng").unwrap_or(&Value::Null));
    let prompts: Vec<String> = (1..=3).map(|i| body_str(&body, &format!("prompt{i}"))).collect();

    let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let eid = next_issue_id(conn, "JRN")?;
            conn.execute(
                "INSERT INTO journal_entries (id,text,date,created,updated,lat,lng,place_name,starred,tags,prompt1,prompt2,prompt3) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                rusqlite::params![
                    eid, text, date_val, now, now, lat, lng, place_name, starred, tags,
                    prompts[0], prompts[1], prompts[2]
                ],
            )?;
            let events = vec![ev("journal_entry", &eid, MutationKind::Created)];
            *slot_w.lock().expect("slot") = Some(eid);
            Ok(WriteOutcome { applied: true, events })
        })
        .await;
    match write {
        Ok(_) => {
            let eid = slot.lock().expect("slot").take().unwrap_or_default();
            (StatusCode::CREATED, Json(json!({ "id": eid, "ok": true }))).into_response()
        }
        Err(e) => internal(e),
    }
}

// ---- POST /api/journal/import ----------------------------------------------

/// Day One bulk import. Dates: `creationDate[:10]` is the entry date;
/// `creationDate[:19]` parsed as LOCAL time (Python `time.mktime`) is
/// created/updated. An unparseable date falls back to now where Python
/// would 500 — the whole import failing on one bad row helps nobody.
async fn import_entries(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let entries = body.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
    let photos_dir = body_str(&body, "photos_dir");
    let now = chrono::Utc::now().timestamp();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mdir = media_dir();

    let slot: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let slot_w = slot.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let mut imported = 0usize;
            let mut events = Vec::new();
            for entry in &entries {
                let eid = next_issue_id(conn, "JRN")?;
                let cd = entry.get("creationDate").and_then(Value::as_str).unwrap_or("");
                let date_val =
                    if cd.is_empty() { today.clone() } else { cd.get(..10).unwrap_or(cd).to_string() };
                let created_ts = if cd.is_empty() {
                    now
                } else {
                    chrono::NaiveDateTime::parse_from_str(
                        cd.get(..19).unwrap_or(cd),
                        "%Y-%m-%dT%H:%M:%S",
                    )
                    .ok()
                    .and_then(|ndt| {
                        use chrono::TimeZone;
                        chrono::Local.from_local_datetime(&ndt).single()
                    })
                    .map(|dt| dt.timestamp())
                    .unwrap_or(now)
                };
                let empty = json!({});
                let loc = entry.get("location").unwrap_or(&empty);
                let lat = sql_value(loc.get("latitude").unwrap_or(&Value::Null));
                let lng = sql_value(loc.get("longitude").unwrap_or(&Value::Null));
                // Python: placeName or localityName, administrativeArea,
                // country — empties dropped, ", "-joined.
                let first = {
                    let p = body_str(loc, "placeName");
                    if p.is_empty() { body_str(loc, "localityName") } else { p }
                };
                let place_name = [first, body_str(loc, "administrativeArea"), body_str(loc, "country")]
                    .into_iter()
                    .filter(|p| !p.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ");
                let tags = tags_str(entry, "tags");
                let starred = starred_int(entry);
                conn.execute(
                    "INSERT INTO journal_entries (id,text,date,created,updated,lat,lng,place_name,starred,tags,prompt1,prompt2,prompt3) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'','','')",
                    rusqlite::params![
                        eid, body_str(entry, "text"), date_val, created_ts, created_ts, lat, lng,
                        place_name, starred, tags
                    ],
                )?;
                for (i, photo) in
                    entry.get("photos").and_then(Value::as_array).into_iter().flatten().enumerate()
                {
                    let mid = media_id();
                    let md5 = body_str(photo, "md5");
                    let fname = {
                        let f = body_str(photo, "filename");
                        if f.is_empty() { format!("{md5}.jpeg") } else { f }
                    };
                    let ptype = {
                        let t = body_str(photo, "type");
                        if t.is_empty() { "jpeg".to_string() } else { t }
                    };
                    conn.execute(
                        "INSERT INTO journal_media (id, entry_id, filename, mime, position, created) \
                         VALUES (?1,?2,?3,?4,?5,?6)",
                        rusqlite::params![mid, eid, fname, format!("image/{ptype}"), i as i64, created_ts],
                    )?;
                    // Copy the photo bytes when the export dir has them.
                    if !photos_dir.is_empty() {
                        let src = std::path::Path::new(&photos_dir).join(format!("{md5}.jpeg"));
                        if src.exists() {
                            let _ = std::fs::create_dir_all(&mdir);
                            let _ = std::fs::copy(&src, mdir.join(format!("{mid}.jpeg")));
                        }
                    }
                }
                events.push(ev("journal_entry", &eid, MutationKind::Created));
                imported += 1;
            }
            *slot_w.lock().expect("slot") = imported;
            Ok(WriteOutcome { applied: imported > 0, events })
        })
        .await;
    match write {
        Ok(_) => {
            let imported = *slot.lock().expect("slot");
            Json(json!({ "ok": true, "imported": imported })).into_response()
        }
        Err(e) => internal(e),
    }
}

// ---- GET /api/journal/media/{id} -------------------------------------------

async fn serve_media(AxPath(mid): AxPath<String>) -> Response {
    // Python rejects path separators in the id; axum can't route "/" here
    // but "\" would survive, and it must not reach the filesystem probe.
    if mid.contains('/') || mid.contains('\\') {
        return err(StatusCode::NOT_FOUND, json!({ "error": "not found" }));
    }
    let dir = media_dir();
    for (ext, ct) in MEDIA_EXTS {
        let fpath = dir.join(format!("{mid}{ext}"));
        if fpath.exists() {
            return match tokio::fs::read(&fpath).await {
                Ok(bytes) => (
                    [
                        (header::CONTENT_TYPE, ct.to_string()),
                        // Python _raw(..., cache=True).
                        (header::CACHE_CONTROL, "public, max-age=86400".to_string()),
                    ],
                    bytes,
                )
                    .into_response(),
                Err(e) => internal(e),
            };
        }
    }
    err(StatusCode::NOT_FOUND, json!({ "error": "not found" }))
}

// ---- DELETE /api/journal/media/{id} ----------------------------------------

async fn delete_media(State(state): State<AppState>, AxPath(mid): AxPath<String>) -> Response {
    let dir = media_dir();
    let mid_w = mid.clone();
    let write = state
        .store
        .write_async(move |conn| {
            for (ext, _) in MEDIA_EXTS {
                // Python: if exists: unlink. Errors ignored — the row delete
                // is the operation of record.
                let _ = std::fs::remove_file(dir.join(format!("{mid_w}{ext}")));
            }
            let n = conn.execute("DELETE FROM journal_media WHERE id=?1", [&mid_w])?;
            let events = if n > 0 {
                vec![ev("journal_media", &mid_w, MutationKind::Deleted)]
            } else {
                vec![]
            };
            Ok(WriteOutcome { applied: n > 0, events })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- GET/PATCH/DELETE /api/journal/{id} ------------------------------------

async fn get_entry(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    if !valid_entry_id(&id) {
        return journal_not_found();
    }
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let conn = store.read()?;
        let Some(mut e) = query_rows_json(
            &conn,
            "SELECT * FROM journal_entries WHERE id=?1 AND deleted IS NULL",
            &[&id],
        )?
        .pop() else {
            return Ok(None);
        };
        e["media"] = Value::Array(media_for_entry(&conn, &id)?);
        Ok(Some(e))
    })
    .await;
    match joined {
        Ok(Ok(Some(e))) => Json(e).into_response(),
        Ok(Ok(None)) => err(StatusCode::NOT_FOUND, json!({ "error": "not found" })),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

const PATCH_FIELDS: [&str; 10] = [
    "text", "date", "lat", "lng", "place_name", "starred", "tags", "prompt1", "prompt2", "prompt3",
];

async fn patch_entry(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(body): Json<Value>,
) -> Response {
    if !valid_entry_id(&id) {
        return journal_not_found();
    }
    let fields: Vec<(String, Value)> = PATCH_FIELDS
        .iter()
        .filter_map(|k| {
            body.get(*k).map(|v| {
                // Python: a tags LIST is joined before the UPDATE.
                if *k == "tags" && v.is_array() {
                    (k.to_string(), Value::String(tags_str(&body, "tags")))
                } else {
                    (k.to_string(), v.clone())
                }
            })
        })
        .collect();
    let id_w = id.clone();
    let write = state
        .store
        .write_async(move |conn| {
            if fields.is_empty() {
                // Python's `if fields:` guard — no UPDATE, no `updated` bump.
                return Ok(WriteOutcome { applied: false, events: vec![] });
            }
            let now = chrono::Utc::now().timestamp();
            let set_cl: Vec<String> = fields.iter().map(|(k, _)| format!("{k}=?")).collect();
            let mut params: Vec<rusqlite::types::Value> =
                fields.iter().map(|(_, v)| sql_value(v)).collect();
            params.push(rusqlite::types::Value::Integer(now));
            params.push(rusqlite::types::Value::Text(id_w.clone()));
            let n = conn.execute(
                &format!("UPDATE journal_entries SET {}, updated=? WHERE id=?", set_cl.join(", ")),
                rusqlite::params_from_iter(params),
            )?;
            let events = if n > 0 {
                vec![ev("journal_entry", &id_w, MutationKind::Updated)]
            } else {
                vec![]
            };
            Ok(WriteOutcome { applied: n > 0, events })
        })
        .await;
    match write {
        // Python answers {"ok": true} whether or not the row existed.
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

async fn delete_entry(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    if !valid_entry_id(&id) {
        return journal_not_found();
    }
    let id_w = id.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let now = chrono::Utc::now().timestamp();
            let n = conn.execute(
                "UPDATE journal_entries SET deleted=?1 WHERE id=?2",
                rusqlite::params![now, id_w],
            )?;
            let events = if n > 0 {
                vec![ev("journal_entry", &id_w, MutationKind::Deleted)]
            } else {
                vec![]
            };
            Ok(WriteOutcome { applied: n > 0, events })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- POST /api/journal/{id}/media ------------------------------------------

async fn upload_media(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(body): Json<Value>,
) -> Response {
    if !valid_entry_id(&id) {
        return journal_not_found();
    }
    let mut b64 = body_str(&body, "data");
    // Data-URL prefix: everything through the first comma goes.
    if let Some((_, rest)) = b64.split_once(',') {
        b64 = rest.to_string();
    }
    // Whitespace-tolerant strict decode (Python b64decode accepts embedded
    // newlines a data URL may carry).
    let cleaned: String = b64.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let data = match base64::engine::general_purpose::STANDARD.decode(cleaned.as_bytes()) {
        Ok(d) => d,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": format!("invalid base64 data: {e}") })),
    };
    let fname = {
        let f = body_str(&body, "name");
        if f.is_empty() { "photo.jpg".to_string() } else { f }
    };
    // Extension/mime from MAGIC BYTES, never from the client (Python parity).
    let (ext, mime) = if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        (".png", "image/png")
    } else if data.starts_with(b"\xff\xd8") {
        (".jpeg", "image/jpeg")
    } else if data.starts_with(b"RIFF") {
        (".webp", "image/webp")
    } else {
        (".jpeg", "image/jpeg")
    };
    let now = chrono::Utc::now().timestamp();
    let dir = media_dir();
    let id_w = id.clone();
    let mid = media_id();

    // File first, DB second — Python's order (a failed DB write can orphan a
    // file, never the reverse: a media row must not point at nothing).
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return internal(e);
    }
    if let Err(e) = tokio::fs::write(dir.join(format!("{mid}{ext}")), &data).await {
        return internal(e);
    }
    let mid_w = mid.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let pos: i64 = conn.query_row(
                "SELECT COUNT(*) FROM journal_media WHERE entry_id=?1",
                [&id_w],
                |r| r.get(0),
            )?;
            conn.execute(
                "INSERT INTO journal_media (id, entry_id, filename, mime, position, created) \
                 VALUES (?1,?2,?3,?4,?5,?6)",
                rusqlite::params![mid_w, id_w, fname, mime, pos, now],
            )?;
            let events = vec![ev("journal_media", &mid_w, MutationKind::Created)];
            Ok(WriteOutcome { applied: true, events })
        })
        .await;
    match write {
        Ok(_) => (StatusCode::CREATED, Json(json!({ "id": mid, "ok": true }))).into_response(),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// Tests — temp-DB stores; media tests pin AMUX_HOME to a temp dir under the
// shared env lock (settings::test_env), never the live ~/.amux.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn app() -> (axum::Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("journal-test.db")).unwrap();
        let state = AppState {
            secrets: std::sync::Arc::new(crate::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let router = Router::new().nest("/api/journal", routes()).with_state(state);
        (router, dir)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let b = Request::builder().method(method).uri(path);
        let req = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, v)
    }

    #[tokio::test]
    async fn crud_lifecycle_mints_jrn_ids() {
        let (app, _dir) = app();
        let (st, res) = send(
            &app,
            "POST",
            "/api/journal",
            Some(json!({ "text": "first entry", "tags": ["a", " b ", ""], "starred": true,
                         "lat": 40.7, "lng": -74.0, "place_name": "NYC" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{res}");
        assert_eq!(res, json!({ "id": "JRN-1", "ok": true }));

        let (st, e) = send(&app, "GET", "/api/journal/JRN-1", None).await;
        assert_eq!(st, StatusCode::OK, "{e}");
        assert_eq!(e["text"], json!("first entry"));
        assert_eq!(e["tags"], json!("a,b"), "list tags join trimmed, empties dropped");
        assert_eq!(e["starred"], json!(1), "Python truthiness -> 1");
        assert_eq!(e["lat"], json!(40.7));
        assert_eq!(e["place_name"], json!("NYC"));
        assert_eq!(e["media"], json!([]));
        assert!(e["date"].as_str().unwrap().len() == 10);

        // PATCH: allowed fields only; tags list joined; updated bumped.
        let before_updated = e["updated"].as_i64().unwrap();
        let (st, r) = send(
            &app,
            "PATCH",
            "/api/journal/JRN-1",
            Some(json!({ "text": "edited", "tags": ["x", "y"], "starred": false,
                         "id": "JRN-999", "created": 1 })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r, json!({ "ok": true }));
        let (_, e2) = send(&app, "GET", "/api/journal/JRN-1", None).await;
        assert_eq!(e2["text"], json!("edited"));
        assert_eq!(e2["tags"], json!("x,y"));
        assert_eq!(e2["starred"], json!(0));
        assert_eq!(e2["id"], json!("JRN-1"), "id is not a patchable field");
        assert!(e2["updated"].as_i64().unwrap() >= before_updated);
        assert_ne!(e2["created"], json!(1), "created is not a patchable field");

        // Empty PATCH: ok, but no UPDATE ran (Python's `if fields:` guard).
        let (st, r) = send(&app, "PATCH", "/api/journal/JRN-1", Some(json!({ "nope": 1 }))).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r, json!({ "ok": true }));

        // DELETE is soft; entry vanishes from GET and list.
        let (st, r) = send(&app, "DELETE", "/api/journal/JRN-1", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r, json!({ "ok": true }));
        let (st, _) = send(&app, "GET", "/api/journal/JRN-1", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (_, list) = send(&app, "GET", "/api/journal", None).await;
        assert_eq!(list.as_array().unwrap().len(), 0);

        // Non-Python-shaped ids fall to the journal 404, like the route regex.
        let (st, e) = send(&app, "GET", "/api/journal/jrn-1", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(e["error"], json!("journal route not found"));
    }

    #[tokio::test]
    async fn python_shaped_row_round_trips_column_by_column() {
        let (app, dir) = app();
        {
            let conn = rusqlite::Connection::open(dir.path().join("journal-test.db")).unwrap();
            conn.execute(
                "INSERT INTO journal_entries (id,text,date,created,updated,lat,lng,place_name,starred,tags,prompt1,prompt2,prompt3) \
                 VALUES ('JRN-42','walked the bridge','2026-07-04',1751600000,1751600001,40.7061,-73.9969,'Brooklyn, NY, United States',1,'walk,summer','grateful for','','')",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO journal_media (id, entry_id, filename, mime, position, created) \
                 VALUES ('tokAbc123XyzQ','JRN-42','bridge.jpeg','image/jpeg',0,1751600000)",
                [],
            )
            .unwrap();
        }
        let (st, e) = send(&app, "GET", "/api/journal/JRN-42", None).await;
        assert_eq!(st, StatusCode::OK, "{e}");
        assert_eq!(e["id"], json!("JRN-42"));
        assert_eq!(e["text"], json!("walked the bridge"));
        assert_eq!(e["date"], json!("2026-07-04"));
        assert_eq!(e["created"], json!(1751600000));
        assert_eq!(e["updated"], json!(1751600001));
        assert_eq!(e["lat"], json!(40.7061));
        assert_eq!(e["lng"], json!(-73.9969));
        assert_eq!(e["place_name"], json!("Brooklyn, NY, United States"));
        assert_eq!(e["starred"], json!(1));
        assert_eq!(e["tags"], json!("walk,summer"));
        assert_eq!(e["prompt1"], json!("grateful for"));
        assert_eq!(e["prompt2"], json!(""));
        assert_eq!(e["prompt3"], json!(""));
        assert_eq!(e["deleted"], Value::Null);
        let m = &e["media"].as_array().unwrap()[0];
        assert_eq!(m, &json!({ "id": "tokAbc123XyzQ", "filename": "bridge.jpeg",
                               "mime": "image/jpeg", "position": 0 }));

        // PATCH round-trip on the Python-shaped row.
        let (_, r) = send(&app, "PATCH", "/api/journal/JRN-42",
                          Some(json!({ "place_name": "DUMBO" }))).await;
        assert_eq!(r, json!({ "ok": true }));
        let (_, e2) = send(&app, "GET", "/api/journal/JRN-42", None).await;
        assert_eq!(e2["place_name"], json!("DUMBO"));
        assert_eq!(e2["text"], json!("walked the bridge"), "untouched");
    }

    #[tokio::test]
    async fn list_filters_match_python() {
        let (app, _dir) = app();
        for (text, tags, date, lat) in [
            ("beach day", "summer,beach", "2026-07-01", Some(1.0)),
            ("work notes", "work", "2026-08-01", None),
            ("summer plans in prompt", "", "2026-08-05", None),
        ] {
            let mut body = json!({ "text": text, "tags": tags, "date": date });
            if let Some(l) = lat {
                body["lat"] = json!(l);
                body["lng"] = json!(l);
            }
            let (st, _) = send(&app, "POST", "/api/journal", Some(body)).await;
            assert_eq!(st, StatusCode::CREATED);
        }
        // Order: date DESC.
        let (_, all) = send(&app, "GET", "/api/journal", None).await;
        let ids: Vec<&str> =
            all.as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["JRN-3", "JRN-2", "JRN-1"]);
        // q searches text (and tags — "summer" matches JRN-1 via tags too).
        let (_, hits) = send(&app, "GET", "/api/journal?q=summer", None).await;
        assert_eq!(hits.as_array().unwrap().len(), 2);
        // tag filter is EXACT within the comma list: "beach" matches, "each" does not.
        let (_, hits) = send(&app, "GET", "/api/journal?tag=beach", None).await;
        assert_eq!(hits.as_array().unwrap().len(), 1);
        assert_eq!(hits[0]["id"], json!("JRN-1"));
        let (_, hits) = send(&app, "GET", "/api/journal?tag=each", None).await;
        assert_eq!(hits.as_array().unwrap().len(), 0);
        // Date range.
        let (_, hits) = send(&app, "GET", "/api/journal?from=2026-08-01&to=2026-08-04", None).await;
        assert_eq!(hits.as_array().unwrap().len(), 1);
        assert_eq!(hits[0]["id"], json!("JRN-2"));
        // has_location.
        let (_, hits) = send(&app, "GET", "/api/journal?has_location=1", None).await;
        assert_eq!(hits.as_array().unwrap().len(), 1);
        assert_eq!(hits[0]["id"], json!("JRN-1"));
        // has_media (none yet).
        let (_, hits) = send(&app, "GET", "/api/journal?has_media=1", None).await;
        assert_eq!(hits.as_array().unwrap().len(), 0);

        // Tag counts.
        let (_, counts) = send(&app, "GET", "/api/journal/tags", None).await;
        assert_eq!(counts["summer"], json!(1));
        assert_eq!(counts["beach"], json!(1));
        assert_eq!(counts["work"], json!(1));
    }

    #[tokio::test]
    async fn config_prompts_round_trip() {
        let (app, _dir) = app();
        let (_, cfg) = send(&app, "GET", "/api/journal/config", None).await;
        assert_eq!(cfg, json!({ "prompt1": "", "prompt2": "", "prompt3": "" }));
        let (st, r) = send(&app, "POST", "/api/journal/config",
                           Some(json!({ "prompt1": "Grateful for?", "prompt3": "Tomorrow?" }))).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r, json!({ "ok": true }));
        let (_, cfg) = send(&app, "GET", "/api/journal/config", None).await;
        // Python writes ALL THREE keys — an omitted prompt is cleared to "".
        assert_eq!(cfg, json!({ "prompt1": "Grateful for?", "prompt2": "", "prompt3": "Tomorrow?" }));
    }

    #[tokio::test]
    async fn media_upload_serve_delete_on_disk() {
        let home = tempfile::tempdir().unwrap();
        let _env = crate::api::settings::test_env::set_home(home.path());
        let (app, _dir) = app();
        let (_, _) = send(&app, "POST", "/api/journal", Some(json!({ "text": "with photo" }))).await;

        // A real 1x1 PNG header + payload; magic bytes must pick .png even
        // though the client said .jpg.
        let png: Vec<u8> = [b"\x89PNG\r\n\x1a\n".as_slice(), &[0u8; 32]].concat();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let (st, up) = send(
            &app,
            "POST",
            "/api/journal/JRN-1/media",
            Some(json!({ "data": format!("data:image/png;base64,{b64}"), "name": "photo.jpg" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{up}");
        let mid = up["id"].as_str().unwrap().to_string();
        assert_eq!(mid.len(), 14, "token_urlsafe(10) shape");
        assert!(home.path().join("journal-media").join(format!("{mid}.png")).exists(),
                "bytes on disk under the temp home, ext from magic bytes");

        // Entry now carries the media row; has_media filter sees it.
        let (_, e) = send(&app, "GET", "/api/journal/JRN-1", None).await;
        let m = &e["media"].as_array().unwrap()[0];
        assert_eq!(m["filename"], json!("photo.jpg"));
        assert_eq!(m["mime"], json!("image/png"));
        assert_eq!(m["position"], json!(0));
        let (_, hits) = send(&app, "GET", "/api/journal?has_media=1", None).await;
        assert_eq!(hits.as_array().unwrap().len(), 1);

        // Serve: bytes + content-type + Python's cache header.
        let req = Request::builder()
            .method("GET")
            .uri(format!("/api/journal/media/{mid}"))
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CONTENT_TYPE], "image/png");
        assert_eq!(res.headers()[header::CACHE_CONTROL], "public, max-age=86400");
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), png.as_slice());

        // Second upload gets position 1.
        let jpg = [b"\xff\xd8".as_slice(), &[0u8; 8]].concat();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&jpg);
        let (_, up2) = send(&app, "POST", "/api/journal/JRN-1/media",
                            Some(json!({ "data": b64 }))).await;
        let (_, e) = send(&app, "GET", "/api/journal/JRN-1", None).await;
        assert_eq!(e["media"].as_array().unwrap().len(), 2);
        assert_eq!(e["media"][1]["position"], json!(1));
        assert_eq!(e["media"][1]["filename"], json!("photo.jpg"), "default name");

        // Delete removes row AND file.
        let (st, r) = send(&app, "DELETE", &format!("/api/journal/media/{mid}"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r, json!({ "ok": true }));
        assert!(!home.path().join("journal-media").join(format!("{mid}.png")).exists());
        let (_, e) = send(&app, "GET", "/api/journal/JRN-1", None).await;
        assert_eq!(e["media"].as_array().unwrap().len(), 1);

        // Serving the deleted id: Python's plain "not found".
        let (st, nf) = send(&app, "GET", &format!("/api/journal/media/{mid}"), None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(nf["error"], json!("not found"));
        // Bad base64 is a 400 (documented deviation from Python's 500).
        let (st, _) = send(&app, "POST", "/api/journal/JRN-1/media",
                           Some(json!({ "data": "!!not-base64!!" }))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (_, up3) = send(&app, "GET", "/api/journal/JRN-1", None).await;
        assert_eq!(up3["media"].as_array().unwrap().len(), 1, "failed upload wrote nothing");
        drop(up2);
    }

    #[tokio::test]
    async fn day_one_import_maps_dates_location_photos() {
        let home = tempfile::tempdir().unwrap();
        let _env = crate::api::settings::test_env::set_home(home.path());
        let photos = tempfile::tempdir().unwrap();
        std::fs::write(photos.path().join("abc123.jpeg"), b"\xff\xd8fakejpeg").unwrap();
        let (app, _dir) = app();

        let (st, r) = send(
            &app,
            "POST",
            "/api/journal/import",
            Some(json!({
                "photos_dir": photos.path().to_str().unwrap(),
                "entries": [
                    { "text": "Day One entry", "creationDate": "2024-03-05T14:30:00Z",
                      "starred": true, "tags": ["travel", "family"],
                      "location": { "placeName": "Louvre", "administrativeArea": "Paris",
                                    "country": "France", "latitude": 48.86, "longitude": 2.33 },
                      "photos": [{ "md5": "abc123", "type": "jpeg" }] },
                    { "text": "minimal" }
                ]
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r, json!({ "ok": true, "imported": 2 }));

        let (_, e) = send(&app, "GET", "/api/journal/JRN-1", None).await;
        assert_eq!(e["text"], json!("Day One entry"));
        assert_eq!(e["date"], json!("2024-03-05"));
        assert_eq!(e["place_name"], json!("Louvre, Paris, France"));
        assert_eq!(e["lat"], json!(48.86));
        assert_eq!(e["starred"], json!(1));
        assert_eq!(e["tags"], json!("travel,family"));
        assert!(e["created"].as_i64().unwrap() > 0);
        let media = e["media"].as_array().unwrap();
        assert_eq!(media.len(), 1);
        assert_eq!(media[0]["filename"], json!("abc123.jpeg"));
        assert_eq!(media[0]["mime"], json!("image/jpeg"));
        // The photo bytes were copied into the temp home's media dir.
        let mid = media[0]["id"].as_str().unwrap();
        assert!(home.path().join("journal-media").join(format!("{mid}.jpeg")).exists());

        // Second entry: today's date, no location, no media.
        let (_, e2) = send(&app, "GET", "/api/journal/JRN-2", None).await;
        assert_eq!(e2["date"].as_str().unwrap(),
                   chrono::Local::now().format("%Y-%m-%d").to_string());
        assert_eq!(e2["place_name"], json!(""));
        assert_eq!(e2["media"], json!([]));
    }

    #[tokio::test]
    async fn unmatched_journal_path_is_pythons_404() {
        let (app, _dir) = app();
        let (st, e) = send(&app, "GET", "/api/journal/JRN-1/bogus/extra", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(e["error"], json!("journal route not found"));
    }
}
