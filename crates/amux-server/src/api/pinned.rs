//! /api/pinned — CRUD for pinned-note overlays.
//!
//! Each row is a filesystem path the user wants to float as an always-on-top
//! macOS window. Rows sharing a `group_id` launch together as tabs in one
//! overlay. The dashboard's Pinned tab manages the list; the `amux pin` CLI
//! command launches the Swift overlay.

use super::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", axum::routing::get(list).post(create))
        .route("/{id}", axum::routing::patch(update).delete(delete))
        .route("/{id}/launch", axum::routing::post(launch))
}

async fn list(State(state): State<AppState>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    let mut stmt = match conn.prepare(
        "SELECT id, file_path, title, pos_x, pos_y, width, height, opacity, active, created_at, updated_at, group_id
         FROM pinned_notes ORDER BY created_at DESC",
    ) {
        Ok(s) => s,
        Err(_) => return Json(json!([])).into_response(),
    };
    let rows = stmt
        .query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "file_path": r.get::<_, String>(1)?,
                "title": r.get::<_, String>(2)?,
                "pos_x": r.get::<_, i64>(3)?,
                "pos_y": r.get::<_, i64>(4)?,
                "width": r.get::<_, i64>(5)?,
                "height": r.get::<_, i64>(6)?,
                "opacity": r.get::<_, f64>(7)?,
                "active": r.get::<_, i64>(8)? != 0,
                "created_at": r.get::<_, i64>(9)?,
                "updated_at": r.get::<_, i64>(10)?,
                "group_id": r.get::<_, Option<String>>(11)?,
            }))
        })
        .ok();
    let mut out = Vec::new();
    if let Some(rows) = rows {
        for row in rows.flatten() {
            out.push(row);
        }
    }
    Json(Value::Array(out)).into_response()
}

#[derive(Deserialize)]
struct CreateBody {
    file_path: Option<String>,
    #[serde(default)]
    file_paths: Vec<String>,
    #[serde(default)]
    title: String,
    #[serde(default = "default_pos")]
    pos_x: i64,
    #[serde(default = "default_pos")]
    pos_y: i64,
    #[serde(default = "default_width")]
    width: i64,
    #[serde(default = "default_height")]
    height: i64,
    #[serde(default = "default_opacity")]
    opacity: f64,
    #[serde(default)]
    group_id: Option<String>,
}

fn default_pos() -> i64 { 100 }
fn default_width() -> i64 { 400 }
fn default_height() -> i64 { 500 }
fn default_opacity() -> f64 { 0.6 }

async fn create(State(state): State<AppState>, Json(body): Json<CreateBody>) -> Response {
    let mut paths: Vec<String> = body.file_paths.iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if let Some(ref fp) = body.file_path {
        let fp = fp.trim().to_string();
        if !fp.is_empty() && !paths.contains(&fp) {
            paths.insert(0, fp);
        }
    }
    if paths.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "file_path or file_paths required"}))).into_response();
    }

    let group_id = if paths.len() > 1 || body.group_id.is_some() {
        body.group_id.unwrap_or_else(|| format!("pg_{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()))
    } else {
        String::new()
    };

    let (px, py, w, h, op) = (body.pos_x, body.pos_y, body.width, body.height, body.opacity);
    let title = body.title.clone();
    let group_id2 = group_id.clone();
    let paths2 = paths.clone();

    match state
        .store
        .write_async(move |conn| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let gid: Option<&str> = if group_id.is_empty() { None } else { Some(&group_id) };
            for fp in &paths {
                let t = if title.is_empty() {
                    std::path::Path::new(fp)
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_else(|| fp.clone())
                } else {
                    title.clone()
                };
                conn.execute(
                    "INSERT INTO pinned_notes (file_path, title, pos_x, pos_y, width, height, opacity, active, created_at, updated_at, group_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?8, ?9)",
                    rusqlite::params![fp, t, px, py, w, h, op, now, gid],
                )?;
            }
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await
    {
        Ok(_) => {
            let id = state.store.read().ok()
                .and_then(|c| c.query_row(
                    "SELECT id FROM pinned_notes WHERE file_path = ?1 ORDER BY id DESC LIMIT 1",
                    [&paths2[0]], |r| r.get::<_, i64>(0),
                ).ok())
                .unwrap_or(0);
            let gid_val: Value = if group_id2.is_empty() { Value::Null } else { Value::String(group_id2) };
            (StatusCode::CREATED, Json(json!({"ok": true, "id": id, "group_id": gid_val}))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(Deserialize)]
struct UpdateBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    pos_x: Option<i64>,
    #[serde(default)]
    pos_y: Option<i64>,
    #[serde(default)]
    width: Option<i64>,
    #[serde(default)]
    height: Option<i64>,
    #[serde(default)]
    opacity: Option<f64>,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    group_id: Option<String>,
}

async fn update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateBody>,
) -> Response {
    match state
        .store
        .write_async(move |conn| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let mut sets = vec!["updated_at = ?1".to_string()];
            let mut idx = 2u32;
            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(now)];

            macro_rules! maybe {
                ($field:expr, $col:expr) => {
                    if let Some(v) = $field {
                        sets.push(format!("{} = ?{}", $col, idx));
                        params.push(Box::new(v));
                        idx += 1;
                    }
                };
            }
            maybe!(body.title, "title");
            maybe!(body.pos_x, "pos_x");
            maybe!(body.pos_y, "pos_y");
            maybe!(body.width, "width");
            maybe!(body.height, "height");
            maybe!(body.opacity, "opacity");
            maybe!(body.active.map(|b| b as i64), "active");
            maybe!(body.group_id, "group_id");

            let sql = format!("UPDATE pinned_notes SET {} WHERE id = ?{}", sets.join(", "), idx);
            params.push(Box::new(id));
            let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
            conn.execute(&sql, refs.as_slice())?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await
    {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match state
        .store
        .write_async(move |conn| {
            conn.execute("DELETE FROM pinned_notes WHERE id = ?1", [id])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await
    {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

/// One pinned note as `launch` reads it: file path, group id, x, y, width,
/// height, opacity.
type PinRow = (String, Option<String>, i64, i64, i64, i64, f64);

async fn launch(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };

    // Fetch the target pin and its group_id.
    let note: Option<PinRow> = conn
        .query_row(
            "SELECT file_path, group_id, pos_x, pos_y, width, height, opacity FROM pinned_notes WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )
        .ok();
    let Some((fp, group_id, px, py, w, h, op)) = note else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
    };

    // Collect all file paths: if this pin is in a group, get all siblings.
    let mut file_paths = Vec::new();
    let mut group_ids_to_activate = Vec::new();
    if let Some(ref gid) = group_id {
        if let Ok(mut stmt) = conn.prepare(
            "SELECT id, file_path FROM pinned_notes WHERE group_id = ?1 ORDER BY created_at ASC"
        ) {
            if let Ok(rows) = stmt.query_map([gid], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            }) {
                for row in rows.flatten() {
                    group_ids_to_activate.push(row.0);
                    file_paths.push(row.1);
                }
            }
        }
    }
    if file_paths.is_empty() {
        file_paths.push(fp);
        group_ids_to_activate.push(id);
    }

    let script_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let overlay = script_dir.join("pinned-overlay");
    let overlay_str = if overlay.exists() {
        overlay.to_string_lossy().into_owned()
    } else {
        "pinned-overlay".to_string()
    };

    let mut cmd = std::process::Command::new(&overlay_str);
    for p in &file_paths {
        cmd.arg(p);
    }
    cmd.arg("--x").arg(px.to_string())
       .arg("--y").arg(py.to_string())
       .arg("--width").arg(w.to_string())
       .arg("--height").arg(h.to_string())
       .arg("--opacity").arg(op.to_string());

    match cmd.spawn() {
        Ok(_) => {
            let _ = state
                .store
                .write_async(move |conn| {
                    for aid in &group_ids_to_activate {
                        conn.execute("UPDATE pinned_notes SET active = 1 WHERE id = ?1", [aid])?;
                    }
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                })
                .await;
            Json(json!({"ok": true, "launched": true, "files": file_paths.len()})).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("failed to launch overlay: {e}")}))).into_response(),
    }
}
