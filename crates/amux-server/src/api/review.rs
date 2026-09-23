//! GET /api/review/week?days=N — weekly trends data engine.
//! GET /api/review/digest?file= — weekly digest markdown.

use super::AppState;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/week", axum::routing::get(week))
        .route("/digest", axum::routing::get(digest))
}

#[derive(Deserialize)]
struct WeekQuery {
    #[serde(default = "default_days")]
    days: u32,
}

fn default_days() -> u32 {
    7
}

async fn week(State(state): State<AppState>, Query(q): Query<WeekQuery>) -> Response {
    let days = q.days.clamp(1, 90);
    let store = state.store.clone();

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let conn = store.read()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let since = now - (days as i64) * 86400;
        let since_ms = since * 1000;

        // Per-session aggregation
        let mut per: BTreeMap<String, serde_json::Map<String, serde_json::Value>> = BTreeMap::new();

        fn new_slot(sess: &str) -> serde_json::Map<String, serde_json::Value> {
            let mut m = serde_json::Map::new();
            m.insert("session".into(), json!(sess));
            m.insert("messages".into(), json!(0));
            m.insert("cards_created".into(), json!(0));
            m.insert("cards_done".into(), json!(0));
            m.insert("cards_verified".into(), json!(0));
            m.insert("tokens".into(), json!(0));
            m.insert("cost_usd".into(), json!(0.0));
            m.insert("samples".into(), json!([]));
            m
        }

        macro_rules! slot {
            ($per:expr, $sess:expr) => {{
                let key = $sess.to_string();
                if !$per.contains_key(&key) {
                    $per.insert(key.clone(), new_slot(&key));
                }
                $per.get_mut(&key).unwrap()
            }};
        }

        // Messages from cmd_history
        {
            let mut stmt = conn.prepare(
                "SELECT session, text FROM cmd_history WHERE ts >= ?1
                 AND (type IS NULL OR type NOT IN ('worker','schedule','system'))
                 ORDER BY ts",
            )?;
            let rows = stmt.query_map([since_ms], |r| {
                Ok((
                    r.get::<_, String>(0).unwrap_or_default(),
                    r.get::<_, String>(1).unwrap_or_default(),
                ))
            })?;
            for row in rows.flatten() {
                let (sess, text) = row;
                let s = slot!(per, sess);
                let msgs = s["messages"].as_i64().unwrap_or(0);
                s.insert("messages".into(), json!(msgs + 1));
                if let Some(arr) = s.get_mut("samples").and_then(|v: &mut serde_json::Value| v.as_array_mut()) {
                    if arr.len() < 5 && !text.is_empty() {
                        let clean = text
                            .trim_start_matches('[')
                            .split_once(']')
                            .map(|(_, rest)| rest.trim())
                            .unwrap_or(&text);
                        let truncated: String = clean.chars().take(140).collect();
                        arr.push(json!(truncated));
                    }
                }
            }
        }

        // Cards from issues
        {
            let mut stmt = conn.prepare(
                "SELECT session, status FROM issues WHERE created >= ?1 AND deleted IS NULL",
            )?;
            let rows = stmt.query_map([since], |r| {
                Ok((
                    r.get::<_, String>(0).unwrap_or_default(),
                    r.get::<_, String>(1).unwrap_or_default(),
                ))
            })?;
            for row in rows.flatten() {
                let (sess, status) = row;
                let s = slot!(per, sess);
                let cc = s["cards_created"].as_i64().unwrap_or(0);
                s.insert("cards_created".into(), json!(cc + 1));
                match status.as_str() {
                    "done" => {
                        let cd = s["cards_done"].as_i64().unwrap_or(0);
                        s.insert("cards_done".into(), json!(cd + 1));
                    }
                    "verified" => {
                        let cv = s["cards_verified"].as_i64().unwrap_or(0);
                        s.insert("cards_verified".into(), json!(cv + 1));
                    }
                    _ => {}
                }
            }
        }

        // Tokens from token_ledger
        {
            let mut stmt = conn.prepare(
                "SELECT session, SUM(COALESCE(input,0)+COALESCE(output,0)),
                        SUM(COALESCE(cost_usd,0))
                 FROM token_ledger WHERE ts >= ?1 GROUP BY session",
            )?;
            let rows = stmt.query_map([since], |r| {
                Ok((
                    r.get::<_, String>(0).unwrap_or_default(),
                    r.get::<_, i64>(1).unwrap_or(0),
                    r.get::<_, f64>(2).unwrap_or(0.0),
                ))
            })?;
            for row in rows.flatten() {
                let (sess, tokens, cost) = row;
                let s = slot!(per, sess);
                s.insert("tokens".into(), json!(tokens));
                s.insert("cost_usd".into(), json!((cost * 100.0).round() / 100.0));
            }
        }

        let mut rows: Vec<serde_json::Value> = per
            .into_values()
            .map(serde_json::Value::Object)
            .collect();
        rows.sort_by(|a, b| {
            let score =
                |v: &serde_json::Value| v["messages"].as_i64().unwrap_or(0) + v["cards_created"].as_i64().unwrap_or(0);
            score(b).cmp(&score(a))
        });

        let totals = json!({
            "messages": rows.iter().map(|r| r["messages"].as_i64().unwrap_or(0)).sum::<i64>(),
            "cards_created": rows.iter().map(|r| r["cards_created"].as_i64().unwrap_or(0)).sum::<i64>(),
            "cards_done": rows.iter().map(|r| r["cards_done"].as_i64().unwrap_or(0)).sum::<i64>(),
            "cards_verified": rows.iter().map(|r| r["cards_verified"].as_i64().unwrap_or(0)).sum::<i64>(),
            "tokens": rows.iter().map(|r| r["tokens"].as_i64().unwrap_or(0)).sum::<i64>(),
            "cost_usd": (rows.iter().map(|r| r["cost_usd"].as_f64().unwrap_or(0.0)).sum::<f64>() * 100.0).round() / 100.0,
            "active_sessions": rows.iter().filter(|r| r["messages"].as_i64().unwrap_or(0) > 0 || r["cards_created"].as_i64().unwrap_or(0) > 0).count(),
        });

        Ok(json!({
            "days": days,
            "since": since,
            "totals": totals,
            "per_session": rows,
        }))
    })
    .await;

    match result {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct DigestQuery {
    #[serde(default)]
    file: String,
}

fn digest_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Dev/amux/docs/weekly-review")
}

async fn digest(Query(q): Query<DigestQuery>) -> Response {
    let dir = digest_dir();

    if !q.file.is_empty() {
        // Serve a specific digest file
        let safe_name = q.file.replace(['/', '\\', '\0'], "").trim().to_string();
        let path = dir.join(&safe_name);
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                return Json(json!({"file": safe_name, "markdown": content})).into_response()
            }
            Err(_) => {
                return (
                    axum::http::StatusCode::NOT_FOUND,
                    Json(json!({"error": "digest not found"})),
                )
                    .into_response()
            }
        }
    }

    // List available digests
    let mut weeks: Vec<serde_json::Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".md") {
                weeks.push(json!({"file": name}));
            }
        }
    }
    weeks.sort_by(|a, b| {
        let af = a["file"].as_str().unwrap_or("");
        let bf = b["file"].as_str().unwrap_or("");
        bf.cmp(af)
    });

    // Return the latest digest content too
    let latest = weeks.first().and_then(|w| {
        let f = w["file"].as_str()?;
        let content = std::fs::read_to_string(dir.join(f)).ok()?;
        Some(json!({"file": f, "markdown": content}))
    });

    Json(json!({
        "weeks": weeks,
        "file": latest.as_ref().and_then(|l| l["file"].as_str()),
        "markdown": latest.as_ref().and_then(|l| l["markdown"].as_str()),
    }))
    .into_response()
}
