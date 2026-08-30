//! /api/gmail read+send — the Mail view's inbox browser (AMUX-2883).
//!
//! The auth half (accounts/auth/callback/account-delete) lives in
//! gmail_auth.rs; this is the MAILBOX half the SPA has been calling into
//! 404s since the python retirement: labels, inbox summaries, full threads,
//! and the legacy send. All logic is on `GmailClient`
//! (integrations/email.rs) — the same plumbing `/api/email/*` ships on, so
//! token refresh, the 401-retry, and the invalid_grant discriminator are
//! shared rather than re-implemented.
//!
//! Contract from 792ce1f^:amux-server.py:72165-72210 + the helpers at
//! :26762-27320, shapes matched not redesigned:
//!
//!   GET  /api/gmail/labels?account=          {"labels":[...]} (system first)
//!   GET  /api/gmail/inbox?account=&label=INBOX&page_token=&q=
//!                                            {"messages":[summary...],
//!                                             "next_page_token":...}
//!   GET  /api/gmail/thread/{id}?account=     full thread, bodies decoded,
//!                                            unread marked read on open
//!   POST /api/gmail/send                     {account,to,subject,body,
//!                                             reply_to_message_id?,thread_id?}
//!
//! Send goes through `compose_send` (multipart/alternative) with the
//! signature OFF — python's legacy path was bare text/plain MIMEText, and
//! auto-appending a signature here would change what this caller has always
//! sent. Successful sends land in the send-audit ledger like every other
//! API send (AMUX-1897: an unattributed send cost real forensics time).

use super::AppState;
use crate::integrations::email::{email_log, GmailClient};
use axum::extract::{Path as AxPath, Query};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

pub fn routes() -> Router<AppState> {
    routes_with(Arc::new(GmailClient::new_default()))
}

/// Absolute paths, merged (same reason as gmail_auth: a nest wildcard at
/// /api/gmail would shadow the public callback route).
pub fn routes_with(client: Arc<GmailClient>) -> Router<AppState> {
    Router::new()
        .route("/api/gmail/labels", get(labels))
        .route("/api/gmail/inbox", get(inbox))
        .route("/api/gmail/thread/{id}", get(thread))
        .route("/api/gmail/send", post(send))
        .layer(Extension(client))
}

fn qp<'a>(q: &'a HashMap<String, String>, k: &str) -> &'a str {
    q.get(k).map(String::as_str).unwrap_or("")
}

async fn labels(
    Extension(client): Extension<Arc<GmailClient>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let labels = client.list_labels(qp(&q, "account")).await;
    Json(json!({ "labels": labels })).into_response()
}

async fn inbox(
    Extension(client): Extension<Arc<GmailClient>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let label = {
        let l = qp(&q, "label");
        if l.is_empty() { "INBOX" } else { l }
    };
    let out = client
        .list_messages(qp(&q, "account"), label, qp(&q, "page_token"), qp(&q, "q"), 50)
        .await;
    Json(out).into_response()
}

async fn thread(
    Extension(client): Extension<Arc<GmailClient>>,
    AxPath(id): AxPath<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    Json(client.get_thread(qp(&q, "account"), &id).await).into_response()
}

async fn send(
    Extension(client): Extension<Arc<GmailClient>>,
    headers: axum::http::HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let s = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let (account, to) = (s("account"), s("to"));
    let reply_id = s("reply_to_message_id");
    let result = client
        .compose_send(
            &account,
            &to,
            &s("subject"),
            &s("body"),
            "",        // cc: not in this legacy contract
            &reply_id, // In-Reply-To
            &reply_id, // References defaults to In-Reply-To (python parity)
            &s("thread_id"),
            false, // signature OFF — see module docs
            &[],   // legacy contract carries no attachments
        )
        .await;
    match result {
        Ok(v) => {
            // Audit-log the send like python did — only on success, same
            // endpoint tag, with the session attribution header if present.
            let session = headers
                .get("x-amux-session")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            email_log(
                client.home(),
                json!({
                    "endpoint": "gmail_send_legacy",
                    "via": "gmail",
                    "from": account,
                    "to": to,
                    "subject": s("subject"),
                    "session": session,
                }),
            );
            Json(v).into_response()
        }
        Err(e) => Json(json!({ "error": e })).into_response(),
    }
}
