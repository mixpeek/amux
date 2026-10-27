//! `/api/bridge`: the chat bridge's surface (crate::bridge has the design).
//!
//! - `GET /api/bridge`: each platform's credential status, the links, and
//!   recent replies with their state (sent, held, failed).
//! - `POST /api/bridge/links`, `DELETE /api/bridge/links/{platform}/{conversation}`
//!   (owner only): link a conversation to a worker or group, or unlink it.
//!   The owner can also do this from the chat with `/link <target>`.
//! - `GET /api/bridge/outbound/{id}`, `POST /api/bridge/outbound/{id}/approve`
//!   (owner only): a reply held because the conversation includes someone
//!   other than the owner.
//! - `GET|POST /api/bridge/hook/whatsapp`, `POST /api/bridge/hook/slack`:
//!   platform webhooks, public, authenticated by the platform's signature.

use super::AppState;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use axum::Router;
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::HashMap;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/bridge", axum::routing::get(status_route))
        .route("/api/bridge/links", axum::routing::post(link_route))
        .route("/api/bridge/links/{platform}/{conversation}", axum::routing::delete(unlink_route))
        .route("/api/bridge/outbound/{id}", axum::routing::get(outbound_route))
        .route("/api/bridge/outbound/{id}/approve", axum::routing::post(approve_route))
        .route("/api/bridge/hook/whatsapp", axum::routing::get(whatsapp_verify_route).post(whatsapp_hook))
        .route("/api/bridge/hook/slack", axum::routing::post(slack_hook))
}

fn bad(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({"ok": false, "error": msg}))).into_response()
}

async fn status_route(State(state): State<AppState>) -> Response {
    let platforms: Vec<Value> = crate::bridge::adapters::configured().into_iter()
        .map(|(p, ok, missing)| json!({"platform": p, "configured": ok, "missing": missing})).collect();
    let (links, outbound) = match state.store.read() {
        Ok(c) => {
            let links: Vec<Value> = c.prepare("SELECT platform, conversation, kind, conv, linked_by, linked_at FROM bridge_links ORDER BY linked_at DESC")
                .and_then(|mut s| s.query_map([], |r| Ok(json!({"platform": r.get::<_, String>(0)?, "conversation": r.get::<_, String>(1)?,
                    "kind": r.get::<_, String>(2)?, "conv": r.get::<_, String>(3)?, "linked_by": r.get::<_, String>(4)?,
                    "linked_at": r.get::<_, i64>(5)?})))?.collect()).unwrap_or_default();
            let outbound: Vec<Value> = c.prepare("SELECT id, platform, conversation, state, attempts, error, created_at FROM bridge_outbound ORDER BY created_at DESC LIMIT 50")
                .and_then(|mut s| s.query_map([], |r| Ok(json!({"id": r.get::<_, String>(0)?, "platform": r.get::<_, String>(1)?,
                    "conversation": r.get::<_, String>(2)?, "state": r.get::<_, String>(3)?, "attempts": r.get::<_, i64>(4)?,
                    "error": r.get::<_, Option<String>>(5)?, "at": r.get::<_, i64>(6)?})))?.collect()).unwrap_or_default();
            (links, outbound)
        }
        Err(_) => (vec![], vec![]),
    };
    Json(json!({"platforms": platforms, "links": links, "outbound": outbound, "measured": true,
                "n_considered": links.len()})).into_response()
}

async fn link_route(State(state): State<AppState>, headers: HeaderMap, uri: Uri, Json(b): Json<Value>) -> Response {
    if let Err(r) = super::land_approvals::owner_only(&state, &headers, &uri, "bridge_link_refused", "link a chat to amux") {
        return *r;
    }
    let s = |k: &str| b[k].as_str().unwrap_or("").trim().to_string();
    let (platform, conversation, target) = (s("platform"), s("conversation"), s("target"));
    if !crate::bridge::PLATFORMS.contains(&platform.as_str()) || conversation.is_empty() || target.is_empty() {
        return bad(StatusCode::BAD_REQUEST, "platform (whatsapp, slack, telegram, discord), conversation and target are required");
    }
    let Some((stored, _)) = super::conversations::resolve_link_target(&target) else {
        return bad(StatusCode::NOT_FOUND, &format!("no worker or group named '{target}'"));
    };
    match crate::bridge::save_link(&state, &platform, &conversation, b["is_group"].as_bool().unwrap_or(false), &stored, "owner").await {
        Ok(()) => Json(json!({"ok": true, "platform": platform, "conversation": conversation, "target": stored})).into_response(),
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

async fn unlink_route(State(state): State<AppState>, headers: HeaderMap, uri: Uri, Path((platform, conversation)): Path<(String, String)>) -> Response {
    if let Err(r) = super::land_approvals::owner_only(&state, &headers, &uri, "bridge_link_refused", "unlink a chat") {
        return *r;
    }
    let n = state.store.write_async(move |conn| {
        let n = conn.execute("DELETE FROM bridge_links WHERE platform=?1 AND conversation=?2", params![platform, conversation])?;
        Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
    }).await;
    match n {
        Ok(w) if w.applied => Json(json!({"ok": true})).into_response(),
        Ok(_) => bad(StatusCode::NOT_FOUND, "no such link"),
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn outbound_route(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let row = state.store.read().ok().and_then(|c| c.query_row(
        "SELECT o.platform, o.conversation, o.state, p.body, p.author_name FROM bridge_outbound o JOIN conversation_posts p ON p.id=o.post_id WHERE o.id=?1",
        params![id], |r| Ok(json!({"id": id, "platform": r.get::<_, String>(0)?, "conversation": r.get::<_, String>(1)?,
            "state": r.get::<_, String>(2)?, "body": r.get::<_, String>(3)?, "author": r.get::<_, String>(4)?}))).ok());
    match row {
        Some(v) => Json(v).into_response(),
        None => bad(StatusCode::NOT_FOUND, "no reply with that id"),
    }
}

async fn approve_route(State(state): State<AppState>, headers: HeaderMap, uri: Uri, Path(id): Path<String>) -> Response {
    if let Err(r) = super::land_approvals::owner_only(&state, &headers, &uri, "bridge_approve_refused", "approve a reply to someone outside") {
        return *r;
    }
    match crate::bridge::approve(&state, &id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => bad(StatusCode::NOT_FOUND, &e),
    }
}

// ---- webhooks: answered at once, processed after ----------------------------

async fn whatsapp_verify_route(Query(q): Query<HashMap<String, String>>) -> Response {
    match crate::bridge::adapters::whatsapp_verify(q.get("hub.mode").map(String::as_str), q.get("hub.verify_token").map(String::as_str),
        q.get("hub.challenge").map(String::as_str)) {
        Some(c) => (StatusCode::OK, c).into_response(),
        None => {
            tracing::warn!(verdict = "bridge_whatsapp_verify_refused", "bridge: a WhatsApp webhook subscription did not carry our verify token");
            (StatusCode::FORBIDDEN, "verify token mismatch").into_response()
        }
    }
}

async fn whatsapp_hook(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let sig = headers.get("x-hub-signature-256").and_then(|v| v.to_str().ok());
    if !crate::bridge::adapters::whatsapp_signature_ok(sig, &body) {
        tracing::warn!(verdict = "bridge_whatsapp_signature_refused", measured = true, n_considered = 1,
            "bridge: a WhatsApp webhook failed its signature check and was dropped");
        return (StatusCode::UNAUTHORIZED, "bad signature").into_response();
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else { return (StatusCode::BAD_REQUEST, "not JSON").into_response() };
    let msgs = crate::bridge::adapters::whatsapp_parse(&v);
    tokio::spawn(async move {
        for m in msgs {
            crate::bridge::inbound(&state, m).await;
        }
    });
    StatusCode::OK.into_response()
}

async fn slack_hook(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok());
    if !crate::bridge::adapters::slack_signature_ok(h("x-slack-request-timestamp"), h("x-slack-signature"), &body,
        crate::config::now_f64() as i64) {
        tracing::warn!(verdict = "bridge_slack_signature_refused", measured = true, n_considered = 1,
            "bridge: a Slack event failed its signature check and was dropped");
        return (StatusCode::UNAUTHORIZED, "bad signature").into_response();
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else { return (StatusCode::BAD_REQUEST, "not JSON").into_response() };
    if v["type"] == "url_verification" {
        return Json(json!({"challenge": v["challenge"]})).into_response();
    }
    if let Some(m) = crate::bridge::adapters::slack_parse(&v) {
        tokio::spawn(async move { crate::bridge::inbound(&state, m).await; });
    }
    StatusCode::OK.into_response()
}
