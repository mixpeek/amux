//! Telegram connector: poll status and outbound send.
//!
//! Mounted at `/api/telegram`. The bot TOKEN itself is not managed here: it
//! rides the connector credential path (`api/connectors.rs`'s `telegram`
//! registry row, `POST /api/connectors/telegram/credentials`), same as every
//! other API-key connector. Chat links are the chat bridge's: `/link` from
//! the chat, or `POST /api/bridge/links` and `DELETE
//! /api/bridge/links/telegram/<chat_id>`, listed by `GET /api/bridge`.
//!
//! Inbound routing (Telegram -> session) lives in
//! `runtime_jobs::telegram_poll`.

use super::AppState;
use crate::db::telegram as tg_db;
use crate::runtime_jobs::telegram_poll;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use std::sync::Arc;
use crate::integrations::email::{HttpTransport, ReqwestTransport};
use axum::http::HeaderMap;
use serde::Deserialize;
use serde_json::json;

pub fn routes() -> Router<AppState> {
    routes_with(Arc::new(ReqwestTransport::new()))
}
pub fn routes_with(http: Arc<dyn HttpTransport>) -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/send", axum::routing::post(send))
        .layer(Extension(http))
}

fn err(status: StatusCode, body: serde_json::Value) -> Response {
    (status, Json(body)).into_response()
}

async fn status(State(state): State<AppState>) -> Response {
    let mapping_count = state
        .store
        .read()
        .ok()
        .and_then(|conn| tg_db::count(&conn).ok())
        .unwrap_or(0);
    let report = telegram_poll::last_report();
    (
        StatusCode::OK,
        Json(json!({
            "bot_token_set": telegram_poll::bot_token().is_some(),
            "mapping_count": mapping_count,
            "last_poll_at": report.last_poll_at,
            "last_error": report.last_error,
            "messages_routed": report.messages_routed,
            "messages_unlinked": report.messages_unlinked,
            "messages_group_ignored": report.messages_group_ignored,
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct SendReq {
    /// Either `session` (resolved to its most-recently-linked chat) or a raw
    /// `chat_id` — exactly one must be present. `session` is the common case
    /// (a hook knows the session it's running as, not the chat_id);
    /// `chat_id` exists for callers that already resolved it (or want to
    /// message a chat with no session mapping at all).
    session: Option<String>,
    chat_id: Option<i64>,
    text: String,
    /// Telegram's `parse_mode` — "HTML" is the only mode this codebase's
    /// senders (the amux-relay Stop hook) produce; MarkdownV2 is valid
    /// Telegram-side too but nothing here generates it. `None` = Telegram's
    /// default plain-text interpretation, unchanged from before this field
    /// existed.
    parse_mode: Option<String>,
}

async fn send(
    State(state): State<AppState>,
    Extension(http): Extension<Arc<dyn HttpTransport>>,
    headers: HeaderMap,
    Json(body): Json<SendReq>,
) -> Response {
    if let Some(worker) = headers.get("X-Amux-Session").and_then(|v| v.to_str().ok()) {
        let conn = state.store.read().ok();
        if let Err(denial) = super::connectors::connector_entitlement_check(
            conn.as_deref(),
            &crate::config::amux_home(),
            worker,
            "telegram",
            None,
        ) {
            tracing::warn!(
                worker,
                verdict = "telegram_send_not_entitled",
                "worker connector scope denied Telegram send"
            );
            return err(StatusCode::FORBIDDEN, denial);
        }
    }
    if body.text.trim().is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": "text is required" }),
        );
    }
    let chat_id = match (body.chat_id, body.session.as_deref()) {
        (Some(id), _) => Some(id),
        (None, Some(session)) => {
            let looked_up = state
                .store
                .read()
                .ok()
                .and_then(|conn| tg_db::by_session(&conn, session).ok().flatten());
            match looked_up {
                Some(m) => Some(m.chat_id),
                None => {
                    return err(
                        StatusCode::NOT_FOUND,
                        json!({ "error": "no Telegram chat linked to that session", "session": session }),
                    );
                }
            }
        }
        (None, None) => {
            return err(
                StatusCode::BAD_REQUEST,
                json!({ "error": "one of chat_id or session is required" }),
            );
        }
    };
    let Some(chat_id) = chat_id else {
        unreachable!()
    };
    // A worker's message to a chat it names by id may reach someone outside,
    // so it needs an approved outbound request for that chat
    // (api/outbound.rs, channel telegram, to <chat_id>). A message to the
    // chat linked to its own session (the owner linked it, private only) is
    // its relay to the owner and goes as before.
    let mut approval: Option<String> = None;
    let own_chat = |worker: &str| {
        state.store.read().ok().and_then(|c| tg_db::by_chat(&c, chat_id).ok().flatten()).is_some_and(|m| m.session == worker)
    };
    if let (Some(worker), Some(_)) = (headers.get("X-Amux-Session").and_then(|v| v.to_str().ok()).filter(|w| !own_chat(w)), body.chat_id) {
        let host = super::outbound::recipient_key("telegram", &chat_id.to_string());
        approval = state.store.read().ok().and_then(|c| super::outbound::window_in(&c, worker, &host, crate::config::now_f64() as i64));
        if approval.is_none() {
            tracing::warn!(worker, chat_id, measured = true, n_considered = 1, verdict = "telegram_send_needs_approval",
                "telegram: a worker's message to a chat by id has no approved outbound request; refused");
            return err(StatusCode::FORBIDDEN, json!({
                "code": "outbound_needs_approval",
                "error": "a message to a Telegram chat by id needs the owner's approval of that exact message first",
                "next": format!("POST $AMUX_URL/api/outbound {{\"channel\":\"telegram\",\"action\":\"message\",\"to\":\"{chat_id}\",\"text\":...,\"reason\":...}}; send it here once approved"),
            }));
        }
    }
    let Some(token) = telegram_poll::bot_token() else {
        return err(
            StatusCode::PRECONDITION_FAILED,
            json!({ "error": "TELEGRAM_BOT_TOKEN not set" }),
        );
    };
    match telegram_poll::send_message_with(
        http.as_ref(),
        &token,
        chat_id,
        &body.text,
        body.parse_mode.as_deref(),
    )
    .await
    {
        Ok(()) => {
            if let (Some(id), Some(worker)) = (&approval, headers.get("X-Amux-Session").and_then(|v| v.to_str().ok())) {
                super::outbound::mark_sent(&state, id, worker, &format!("sent to Telegram chat {chat_id}")).await;
            }
            (StatusCode::OK, Json(json!({ "sent": true, "chat_id": chat_id, "approval": approval }))).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, json!({ "error": e })),
    }
}
