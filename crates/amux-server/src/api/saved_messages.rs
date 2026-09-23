//! /api/saved-messages — the peek composer's reusable snippets (AMUX-2871).
//!
//! Python contract (amux-server.py:67646 at 792ce1f^): per-session rows in the
//! `saved_messages` table, `?session=` scopes the list and omitting it returns
//! every worker's. GET/POST/DELETE/PATCH.
//!
//! The table has been carrying rows the SPA could not read: 3 saved messages
//! existed on this machine while `_smRefresh` fell back to its IndexedDB cache
//! and, failing that, rendered "No saved messages for this worker yet." A
//! stored row and an unroutable read are indistinguishable from that message.

use super::AppState;
use crate::api::fs::{parse_qs, qs_get};
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete as delete_route, get};
use axum::{Json, Router};
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/{id}", delete_route(remove).patch(patch))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

async fn list(State(state): State<AppState>, RawQuery(q): RawQuery) -> Response {
    let params = parse_qs(q.as_deref().unwrap_or(""));
    // Presence, not emptiness: `?session=` with a blank value is the peek pane
    // asking for the unnamed scope, which is NOT the same question as omitting
    // the param (every worker). Python distinguished them with `is not None`.
    let session = qs_get(&params, "session");
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };
    let sql = if session.is_some() {
        "SELECT id, session, label, text, created FROM saved_messages WHERE session=?1 ORDER BY pos, id"
    } else {
        "SELECT id, session, label, text, created FROM saved_messages ORDER BY pos, id"
    };
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Json(Value::Array(vec![])).into_response();
    };
    let map = |r: &rusqlite::Row| -> rusqlite::Result<Value> {
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "session": r.get::<_, String>(1)?,
            "label": r.get::<_, String>(2)?,
            "text": r.get::<_, String>(3)?,
            "created": r.get::<_, i64>(4)?,
        }))
    };
    let rows: Vec<Value> = match &session {
        Some(s) => stmt
            .query_map(rusqlite::params![s], map)
            .map(|it| it.flatten().collect())
            .unwrap_or_default(),
        None => stmt
            .query_map([], map)
            .map(|it| it.flatten().collect())
            .unwrap_or_default(),
    };
    Json(Value::Array(rows)).into_response()
}

async fn create(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let text = body["text"].as_str().unwrap_or("").trim().to_string();
    if text.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "text required"})),
        )
            .into_response();
    }
    let label = body["label"].as_str().unwrap_or("").trim().to_string();
    let session = body["session"].as_str().unwrap_or("").trim().to_string();
    let ts = now();
    let (t, l, s) = (text.clone(), label.clone(), session.clone());
    match state
        .store
        .write_async(move |conn| {
            // py:67668 — `pos` seeded from the timestamp so new rows land last
            // under `ORDER BY pos, id` without a separate ordering write.
            conn.execute(
                "INSERT INTO saved_messages (session, label, text, created, pos) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![s, l, t, ts, ts as f64],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await
    {
        Ok(_) => {
            let id = state
                .store
                .read()
                .ok()
                .and_then(|c| {
                    c.query_row(
                        "SELECT id FROM saved_messages WHERE session=?1 AND text=?2 ORDER BY id DESC LIMIT 1",
                        rusqlite::params![session, text],
                        |r| r.get::<_, i64>(0),
                    )
                    .ok()
                })
                .unwrap_or(0);
            (
                StatusCode::CREATED,
                Json(json!({"ok": true, "id": id, "session": session,
                            "label": label, "text": text, "created": ts})),
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
            .into_response(),
    }
}

async fn remove(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match state
        .store
        .write_async(move |conn| {
            let n = conn.execute("DELETE FROM saved_messages WHERE id=?1", [id])?;
            Ok(crate::db::WriteOutcome {
                applied: n > 0,
                events: vec![],
            })
        })
        .await
    {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn patch(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<Value>,
) -> Response {
    // Absent vs present-and-empty are different edits: a missing "label" leaves
    // it alone, `"label": ""` clears it. `"text": ""` is refused outright,
    // because a snippet with no text cannot be recalled and cannot be deleted
    // from the list (the row renders as a blank strip).
    let text = match body.get("text") {
        Some(v) => {
            let t = v.as_str().unwrap_or("").trim().to_string();
            if t.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "text required"})),
                )
                    .into_response();
            }
            Some(t)
        }
        None => None,
    };
    let label = body
        .get("label")
        .map(|v| v.as_str().unwrap_or("").trim().to_string());
    if text.is_none() && label.is_none() {
        return Json(json!({"ok": true, "changed": false})).into_response();
    }
    match state
        .store
        .write_async(move |conn| {
            if let Some(t) = &text {
                conn.execute(
                    "UPDATE saved_messages SET text=?1 WHERE id=?2",
                    rusqlite::params![t, id],
                )?;
            }
            if let Some(l) = &label {
                conn.execute(
                    "UPDATE saved_messages SET label=?1 WHERE id=?2",
                    rusqlite::params![l, id],
                )?;
            }
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await
    {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
