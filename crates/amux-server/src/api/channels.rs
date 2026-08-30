//! GET /api/channels?session=X — list channels for a session.
//! GET /api/channels/:a/:b/messages — message history.
//! POST /api/channels/:a/:b/messages — send a message.
//! DELETE /api/channels/:a/:b/messages — end the channel.

use super::session_verbs;
use super::AppState;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

fn channels_dir() -> PathBuf {
    let home = std::env::var("AMUX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".amux")
        });
    home.join("channels")
}

fn channel_id(a: &str, b: &str) -> String {
    let mut pair = [a, b];
    pair.sort();
    format!("{}__{}", pair[0], pair[1])
}

fn channel_file(a: &str, b: &str) -> PathBuf {
    let dir = channels_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{}.jsonl", channel_id(a, b)))
}

fn channel_history(a: &str, b: &str) -> Vec<serde_json::Value> {
    let path = channel_file(a, b);
    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            serde_json::from_str(line).ok()
        })
        .collect()
}

fn channel_append(sender: &str, recipient: &str, text: &str) -> serde_json::Value {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let msg = json!({
        "ts": ts,
        "from": sender,
        "to": recipient,
        "text": text,
    });
    let path = channel_file(sender, recipient);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", serde_json::to_string(&msg).unwrap_or_default());
    }
    msg
}

fn channel_list_for(session: &str) -> Vec<serde_json::Value> {
    let dir = channels_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".jsonl") {
            continue;
        }
        let stem = &name[..name.len() - 6];
        let parts: Vec<&str> = stem.split("__").collect();
        if parts.len() != 2 || !parts.contains(&session) {
            continue;
        }
        let other = if parts[0] == session {
            parts[1]
        } else {
            parts[0]
        };
        let text = match std::fs::read_to_string(entry.path()) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut last: Option<serde_json::Value> = None;
        let mut count = 0u64;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                last = Some(v);
                count += 1;
            }
        }
        let last_ts = last
            .as_ref()
            .and_then(|l| l["ts"].as_i64())
            .unwrap_or(0);
        let last_from = last
            .as_ref()
            .and_then(|l| l["from"].as_str())
            .unwrap_or("")
            .to_string();
        let last_text: String = last
            .as_ref()
            .and_then(|l| l["text"].as_str())
            .unwrap_or("")
            .chars()
            .take(200)
            .collect();
        out.push(json!({
            "other": other,
            "last_ts": last_ts,
            "last_from": last_from,
            "last_text": last_text,
            "count": count,
        }));
    }
    out.sort_by(|a, b| {
        let at = a["last_ts"].as_i64().unwrap_or(0);
        let bt = b["last_ts"].as_i64().unwrap_or(0);
        bt.cmp(&at)
    });
    out
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", axum::routing::get(list_channels))
        .route(
            "/{a}/{b}/messages",
            axum::routing::get(get_messages)
                .post(post_message)
                .delete(end_channel),
        )
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    session: String,
}

async fn list_channels(Query(q): Query<ListQuery>) -> Response {
    let sess = q.session.trim().to_string();
    if sess.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing 'session'"})),
        )
            .into_response();
    }
    let result =
        tokio::task::spawn_blocking(move || channel_list_for(&sess)).await;
    match result {
        Ok(channels) => Json(json!({"channels": channels})).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn get_messages(Path((a, b)): Path<(String, String)>) -> Response {
    if !session_verbs::valid_session_name(&a) || !session_verbs::valid_session_name(&b) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid session name"})),
        )
            .into_response();
    }
    if a == b {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "cannot open channel with self"})),
        )
            .into_response();
    }
    let result = tokio::task::spawn_blocking(move || {
        let messages = channel_history(&a, &b);
        json!({
            "channel": channel_id(&a, &b),
            "a": a,
            "b": b,
            "messages": messages,
        })
    })
    .await;
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct SendBody {
    #[serde(default)]
    text: String,
}

async fn post_message(
    State(state): State<AppState>,
    Path((a, b)): Path<(String, String)>,
    Json(body): Json<SendBody>,
) -> Response {
    if !session_verbs::valid_session_name(&a) || !session_verbs::valid_session_name(&b) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid session name"})),
        )
            .into_response();
    }
    if a == b {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "cannot open channel with self"})),
        )
            .into_response();
    }
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing 'text'"})),
        )
            .into_response();
    }
    let sender = a.clone();
    let recipient = b.clone();
    let msg = tokio::task::spawn_blocking({
        let text = text.clone();
        let sender = sender.clone();
        let recipient = recipient.clone();
        move || channel_append(&sender, &recipient, &text)
    })
    .await
    .unwrap_or(json!({}));

    let safe_sender = if session_verbs::valid_session_name(&sender) {
        sender.clone()
    } else {
        "unknown".to_string()
    };
    let wrapped = format!(
        "[amux channel message from session '{safe_sender}']\n\
         {text}\n\
         ---\n\
         This is from another amux session, not from the user. \
         To reply, run this bash command (do not paraphrase to the user):\n  \
         curl -sk -X POST $AMUX_URL/api/channels/$AMUX_SESSION/{safe_sender}/messages \
         -H 'Content-Type: application/json' -d '{{\"text\":\"YOUR REPLY HERE\"}}'"
    );
    let (delivered, delivery_status) =
        session_verbs::send_text(
            &state,
            &recipient,
            &wrapped,
            true,
            session_verbs::SendOrigin::Automation,
        )
        .await;

    Json(json!({
        "ok": true,
        "message": msg,
        "delivered": delivered,
        "delivery_status": delivery_status,
    }))
    .into_response()
}

async fn end_channel(
    State(state): State<AppState>,
    Path((a, b)): Path<(String, String)>,
) -> Response {
    if !session_verbs::valid_session_name(&a) || !session_verbs::valid_session_name(&b) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid session name"})),
        )
            .into_response();
    }

    let closer = a;
    let other = b;
    let safe_closer = closer.clone();

    let notice = format!(
        "[channel ended by @{safe_closer}] no reply needed — the channel has been closed."
    );
    if session_verbs::is_running(&other).await {
        let _ = session_verbs::send_text(
            &state,
            &other,
            &notice,
            true,
            session_verbs::SendOrigin::Automation,
        )
        .await;
    }

    let other_for_file = other.clone();
    let closer_for_file = closer.clone();
    let file_result = tokio::task::spawn_blocking(move || {
        let path = channel_file(&closer_for_file, &other_for_file);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| e.to_string())
        } else {
            Ok(())
        }
    })
    .await;

    match file_result {
        Ok(Ok(())) => Json(json!({"ok": true, "message": "ended"})).into_response(),
        Ok(Err(e)) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "message": format!("could not delete channel file: {e}")})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "message": e.to_string()})),
        )
            .into_response(),
    }
}
