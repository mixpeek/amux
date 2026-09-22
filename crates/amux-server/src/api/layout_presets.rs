//! /api/layout-presets — save/load/delete tab layout configurations.
//!
//! Python contract (amux-server.py:73415): GET returns all presets,
//! POST upserts by name, DELETE /:name removes one. The `layout_presets`
//! table is created by Python's migration; Rust reads/writes it directly.

use super::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", axum::routing::get(list).post(upsert))
        .route("/{name}", axum::routing::delete(delete))
}

async fn list(State(state): State<AppState>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    let mut stmt = match conn.prepare(
        "SELECT name, hidden, tab_order, created_at FROM layout_presets ORDER BY created_at DESC",
    ) {
        Ok(s) => s,
        Err(_) => return Json(json!([])).into_response(),
    };
    let rows = stmt
        .query_map([], |r| {
            let name: String = r.get(0)?;
            let hidden: String = r.get(1)?;
            let tab_order: String = r.get(2)?;
            let created_at: i64 = r.get(3)?;
            Ok((name, hidden, tab_order, created_at))
        })
        .ok();
    let mut out = Vec::new();
    if let Some(rows) = rows {
        for row in rows.flatten() {
            let hidden: Value = serde_json::from_str(&row.1).unwrap_or_else(|_| json!([]));
            let tab_order: Value = serde_json::from_str(&row.2).unwrap_or_else(|_| json!([]));
            out.push(json!({
                "name": row.0,
                "hidden": hidden,
                "tab_order": tab_order,
                "created_at": row.3,
            }));
        }
    }
    Json(Value::Array(out)).into_response()
}

#[derive(Deserialize)]
struct UpsertBody {
    #[serde(default)]
    name: String,
    #[serde(default)]
    hidden: Vec<String>,
    #[serde(default)]
    tab_order: Vec<String>,
}

async fn upsert(State(state): State<AppState>, Json(body): Json<UpsertBody>) -> Response {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "name is required"})),
        )
            .into_response();
    }
    let hidden = serde_json::to_string(&body.hidden).unwrap_or_else(|_| "[]".into());
    let tab_order = serde_json::to_string(&body.tab_order).unwrap_or_else(|_| "[]".into());
    let name_clone = name.clone();
    match state
        .store
        .write_async(move |conn| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            conn.execute(
                "INSERT OR REPLACE INTO layout_presets (name, hidden, tab_order, created_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![name_clone, hidden, tab_order, now],
            )?;
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await
    {
        Ok(_) => (StatusCode::CREATED, Json(json!({"ok": true, "name": name}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn delete(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    match state
        .store
        .write_async(move |conn| {
            conn.execute("DELETE FROM layout_presets WHERE name = ?1", [&name])?;
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
