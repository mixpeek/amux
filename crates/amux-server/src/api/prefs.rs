//! /api/prefs — key/value preferences over the live `prefs` table.
//!
//! The SPA's most-referenced missing endpoint at extraction time (24 call
//! sites). Contract matches amux-server.py:67294-67313 exactly: GET with
//! ?key= returns {key, value|null}; GET bare returns the whole map as one
//! flat object; POST {key, value} upserts and echoes {ok, key, value}.

use super::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};

pub fn routes() -> Router<AppState> {
    Router::new().route("/", axum::routing::get(get_prefs).post(set_pref))
}

#[derive(Deserialize)]
pub struct PrefsQuery {
    #[serde(default)]
    key: String,
}

async fn get_prefs(State(state): State<AppState>, Query(q): Query<PrefsQuery>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    if !q.key.is_empty() {
        let value: Option<String> = conn
            .query_row("SELECT value FROM prefs WHERE key = ?1", [&q.key], |r| {
                r.get(0)
            })
            .ok();
        return Json(json!({"key": q.key, "value": value})).into_response();
    }
    let mut stmt = match conn.prepare("SELECT key, value FROM prefs") {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)));
    let mut map = Map::new();
    if let Ok(rows) = rows {
        for row in rows.flatten() {
            map.insert(row.0, Value::String(row.1));
        }
    }
    Json(Value::Object(map)).into_response()
}

#[derive(Deserialize)]
pub struct SetPref {
    #[serde(default)]
    key: String,
    #[serde(default)]
    value: String,
}

async fn set_pref(State(state): State<AppState>, Json(body): Json<SetPref>) -> Response {
    if body.key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "key required"})),
        )
            .into_response();
    }
    let key = body.key.clone();
    let value = body.value.clone();
    let result = state
        .store
        .write_async(move |conn| {
            // Same-value upsert is a no-op: report it honestly (Invariant 37)
            // — the Python endpoint blindly rewrites, but a rev bump for an
            // unchanged pref would make every poll look like a change.
            let existing: Option<String> = conn
                .query_row("SELECT value FROM prefs WHERE key = ?1", [&key], |r| {
                    r.get(0)
                })
                .ok();
            if existing.as_deref() == Some(value.as_str()) {
                return Ok(crate::db::WriteOutcome {
                    applied: false,
                    events: vec![],
                });
            }
            conn.execute(
                "INSERT INTO prefs (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = ?2",
                rusqlite::params![key, value],
            )?;
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Other("pref".into()),
                    entity_id: key.clone(),
                    mutation: amux_core::revision::MutationKind::Updated,
                    payload: None,
                }],
            })
        })
        .await;
    match result {
        Ok(_) => Json(json!({"ok": true, "key": body.key, "value": body.value})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
