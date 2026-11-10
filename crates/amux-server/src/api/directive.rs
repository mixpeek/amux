//! A named sender's instruction to a worker outside its groups, approved by the
//! owner on 2026-11-03 (MSG-103901). It replaces a script typing into panes, which
//! went around the group boundary with no origin and no record.
//!
//! The target's scope has to name the sender in `AMUX_DIRECTIVE_SENDERS`, so the
//! owner opens this per worker or group and it is off by default. The opening covers
//! this route only: ordinary sends from that sender stay refused. A directive
//! interrupts the worker, is stamped with the sender, lands in Messages like any
//! send, and stays open until the worker answers it with what it stopped, what is
//! still running and its first action. `runtime_jobs::directive` retries a delivery
//! a restart or a refusal lost, repeats one unanswered directive once and then tells
//! the owner.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashMap;

use super::session_verbs as sv;
use super::AppState;
use crate::config::now_f64;

pub(crate) const SENDERS_KEY: &str = "AMUX_DIRECTIVE_SENDERS";
const MAX_TRIES: i64 = 10;

tokio::task_local! {
    /// The (sender, target) of the directive being delivered on this task.
    static DELIVERING: (String, String);
}

/// Whether this task is delivering a directive from `origin` to `target`. Only
/// [`deliver`] sets it, after the target's scope named the sender, so nothing a
/// caller sends can widen the group check.
pub(crate) fn delivering(origin: &str, target: &str) -> bool {
    DELIVERING.try_with(|(s, t)| s == origin && t == target).unwrap_or(false)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/sessions/{name}/directive", post(issue_route))
        .route("/api/directives", get(list_route))
        .route("/api/directives/{id}/ack", post(ack_route))
}

fn fail(code: StatusCode, error: &str, name: &str) -> Response {
    (code, Json(json!({"ok": false, "error": error, "code": name}))).into_response()
}

pub(crate) fn written() -> crate::db::WriteOutcome {
    crate::db::WriteOutcome { applied: true, events: vec![] }
}

fn listed(senders: Option<&str>, sender: &str) -> bool {
    !sender.is_empty()
        && senders.unwrap_or("").split(',').any(|s| s.trim().trim_matches(['\'', '"']) == sender)
}

/// Why this sender may not direct this worker, or None. Pure, so a refusal is
/// tested without touching a live pane.
fn refusal(sender: &str, allowed: Option<&str>, lifecycle: &str, isolated: bool) -> Option<(&'static str, String)> {
    if !listed(allowed, sender) {
        return Some(("directive_sender_not_allowed", format!(
            "'{}' is not in the target's {SENDERS_KEY}. The owner sets that on the worker or its group.",
            if sender.is_empty() { "(no sender)" } else { sender })));
    }
    if isolated {
        return Some(("directive_target_isolated",
            "an isolated worker takes input only from its owner".to_string()));
    }
    if lifecycle != "active" {
        return Some(("directive_target_not_active", format!("the target is {lifecycle}; resume it first")));
    }
    None
}

fn delivered_text(id: &str, text: &str) -> String {
    format!(
        "DIRECTIVE {id}. This interrupted your turn. Do this now, before anything else:\n\n{text}\n\n\
         Then answer it, naming what you stopped, what is still running and why, and your first action:\n\
         amux directive ack {id} \"<that>\""
    )
}

fn row_json(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, String>(0)?, "session": r.get::<_, String>(1)?, "sender": r.get::<_, String>(2)?,
        "text": r.get::<_, String>(3)?, "issued_at": r.get::<_, f64>(4)?,
        "delivered_at": r.get::<_, Option<f64>>(5)?, "tries": r.get::<_, i64>(6)?,
        "detail": r.get::<_, String>(7)?, "resent_at": r.get::<_, Option<f64>>(8)?,
        "told_at": r.get::<_, Option<f64>>(9)?, "acked_at": r.get::<_, Option<f64>>(10)?,
        "ack_by": r.get::<_, Option<String>>(11)?, "ack_note": r.get::<_, Option<String>>(12)?,
    }))
}

const COLS: &str = "id, session, sender, text, issued_at, delivered_at, tries, detail, resent_at, told_at, \
                    acked_at, ack_by, ack_note";

pub(crate) fn open_rows(c: &Connection) -> rusqlite::Result<Vec<Value>> {
    let mut s = c.prepare(&format!("SELECT {COLS} FROM directives WHERE acked_at IS NULL ORDER BY issued_at"))?;
    let rows = s.query_map([], row_json)?.collect();
    rows
}

/// What an open directive needs now.
#[derive(Debug, PartialEq)]
pub(crate) enum Due {
    Nothing,
    Deliver,
    Resend,
    Tell,
    GiveUp,
}

pub(crate) fn due(row: &Value, now: f64, ack_s: f64) -> Due {
    let num = |k: &str| row[k].as_f64();
    if num("told_at").is_some() {
        return Due::Nothing;
    }
    let Some(delivered) = num("delivered_at") else {
        return if row["tries"].as_i64().unwrap_or(0) >= MAX_TRIES { Due::GiveUp } else { Due::Deliver };
    };
    match num("resent_at") {
        None if now - delivered >= ack_s => Due::Resend,
        Some(resent) if now - resent >= ack_s => Due::Tell,
        _ => Due::Nothing,
    }
}

/// Interrupts the worker and sends the directive under the sender's name. The
/// message id is the directive's, so a retry after a crash mid-send is settled by
/// the send path's own dedupe instead of arriving twice.
pub(crate) async fn deliver(state: &AppState, id: &str, session: &str, sender: &str, text: &str, attempt: i64) -> Result<(), String> {
    if sv::is_running(session).await {
        let keys = sv::keys_verb(session, &json!({"keys": "Escape"})).await;
        if !keys.status().is_success() {
            tracing::warn!(session, directive = id, status = %keys.status(), verdict = "directive_interrupt_failed",
                measured = true, n_considered = 1, "the Escape before a directive was not accepted");
        }
        tokio::time::sleep(std::time::Duration::from_millis(
            crate::config::env_i64("AMUX_DIRECTIVE_SETTLE_MS", 3000).max(0) as u64)).await;
    }
    let mut headers = HeaderMap::new();
    headers.insert("x-amux-session", sender.parse().map_err(|_| "the sender is not a valid header value".to_string())?);
    let body = json!({"text": delivered_text(id, text), "deliver_now": true,
                      "msg_id": format!("{id}-{attempt}")});
    let resp = DELIVERING
        .scope((sender.to_string(), session.to_string()), sv::send_post(state, session, &headers, &body))
        .await;
    let code = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap_or_default();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if code.is_success() && v["submitted"].as_bool() != Some(false) {
        return Ok(());
    }
    Err(v["error"].as_str().or(v["message"].as_str()).unwrap_or("the send did not submit").to_string())
}

/// Stamps a delivery outcome on the row, logs it, and returns whether it landed.
pub(crate) async fn deliver_and_record(state: &AppState, id: &str, session: &str, sender: &str, text: &str, tries: i64) -> Result<(), String> {
    let out = deliver(state, id, session, sender, text, tries + 1).await;
    let (i, detail, ok) = (id.to_string(), out.as_ref().err().cloned().unwrap_or_default(), out.is_ok());
    let stored = state.store.write_async(move |c| {
        c.execute(
            "UPDATE directives SET tries = tries + 1, detail = ?2, \
             delivered_at = CASE WHEN ?3 THEN COALESCE(delivered_at, ?4) ELSE delivered_at END WHERE id = ?1",
            params![i, detail, ok, now_f64()],
        )?;
        Ok(written())
    }).await;
    if let Err(error) = stored {
        tracing::warn!(directive = id, %error, verdict = "directive_write_failed", measured = true, n_considered = 1,
            "a directive's delivery could not be recorded");
    }
    match &out {
        Ok(()) => tracing::info!(session, sender, directive = id, verdict = "directive_delivered", measured = true,
            n_considered = 1, "directive delivered"),
        Err(why) => tracing::warn!(session, sender, directive = id, why = %why, verdict = "directive_not_delivered",
            measured = true, n_considered = 1, "directive not delivered; the watch retries it"),
    }
    out
}

async fn issue_route(State(state): State<AppState>, Path(name): Path<String>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let sender: String = super::alerts::hdr_worker(&headers).chars().take(64).collect();
    let text = body["text"].as_str().unwrap_or("").trim().to_string();
    if text.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "text is required", "directive_text_required");
    }
    let allowed = sv::scoped_setting_in(&crate::config::amux_home(), &name, SENDERS_KEY);
    if let Some((code, why)) = refusal(&sender, allowed.as_deref(), sv::lane_lifecycle(&name), sv::session_is_isolated(&name)) {
        tracing::warn!(session = %name, sender, verdict = code, measured = true, n_considered = 1, "directive refused");
        return fail(StatusCode::FORBIDDEN, &why, code);
    }
    let id = format!("DIR-{}", ulid::Ulid::new());
    let (i, n, s, t) = (id.clone(), name.clone(), sender.clone(), text.clone());
    let stored = state.store.write_async(move |c| {
        c.execute("INSERT INTO directives (id, session, sender, text, issued_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![i, n, s, t, now_f64()])?;
        Ok(written())
    }).await;
    if let Err(error) = stored {
        tracing::warn!(session = %name, %error, verdict = "directive_write_failed", measured = true, n_considered = 1,
            "a directive could not be recorded, so it was not sent");
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "the directive could not be recorded; nothing was sent", "directive_write_failed");
    }
    match deliver_and_record(&state, &id, &name, &sender, &text, 0).await {
        Ok(()) => Json(json!({"ok": true, "id": id, "delivered": true})).into_response(),
        Err(why) => (StatusCode::ACCEPTED, Json(json!({"ok": true, "id": id, "delivered": false, "why": why,
            "note": "recorded; it is retried every minute and the owner is told if it never lands"}))).into_response(),
    }
}

async fn list_route(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let session = q.get("session").cloned().unwrap_or_default();
    let open = q.get("open").is_some_and(|v| v == "1");
    let rows = state.store.read_async(move |c| {
        let mut s = c.prepare(&format!(
            "SELECT {COLS} FROM directives WHERE (?1 = '' OR session = ?1) AND (?2 = 0 OR acked_at IS NULL) \
             ORDER BY issued_at DESC LIMIT 200"))?;
        let v: Vec<Value> = s.query_map(params![session, open as i64], row_json)?.collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }).await;
    match rows {
        Ok(v) => Json(json!({"measured": true, "n_considered": v.len(), "directives": v})).into_response(),
        Err(e) => Json(json!({"measured": false, "n_considered": 0, "why_unmeasured": e.to_string(), "directives": []})).into_response(),
    }
}

/// The worker it went to answers it, or the owner does: a request with no worker
/// origin. The sender cannot answer its own directive.
fn may_ack(caller: &str, session: &str) -> bool {
    caller.is_empty() || caller == session
}

async fn ack_route(State(state): State<AppState>, Path(id): Path<String>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let caller = super::alerts::hdr_worker(&headers);
    let note = body["note"].as_str().unwrap_or("").trim().to_string();
    if note.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "say what you stopped, what is still running and your first action", "directive_ack_note_required");
    }
    let i = id.clone();
    let row = state.store.read_async(move |c| {
        Ok(c.query_row(&format!("SELECT {COLS} FROM directives WHERE id = ?1"), [i], row_json).optional()?)
    }).await;
    let row = match row {
        Ok(Some(r)) => r,
        Ok(None) => return fail(StatusCode::NOT_FOUND, "no such directive", "directive_not_found"),
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), "directive_read_failed"),
    };
    let session = row["session"].as_str().unwrap_or("").to_string();
    if !may_ack(&caller, &session) {
        tracing::warn!(directive = %id, caller, session, verdict = "directive_ack_refused", measured = true, n_considered = 1,
            "only the worker a directive went to, or the owner, can answer it");
        return fail(StatusCode::FORBIDDEN, "only the worker it went to, or the owner, can answer it", "directive_ack_refused");
    }
    if row["acked_at"].is_number() {
        return Json(json!({"ok": true, "id": id, "already": true})).into_response();
    }
    let by = if caller.is_empty() { "owner".to_string() } else { caller };
    let (i, b, n) = (id.clone(), by.clone(), note);
    let stored = state.store.write_async(move |c| {
        c.execute("UPDATE directives SET acked_at = ?2, ack_by = ?3, ack_note = ?4 WHERE id = ?1 AND acked_at IS NULL",
            params![i, now_f64(), b, n])?;
        Ok(written())
    }).await;
    if let Err(e) = stored {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), "directive_write_failed");
    }
    tracing::info!(directive = %id, session, by, verdict = "directive_acked", measured = true, n_considered = 1, "directive answered");
    Json(json!({"ok": true, "id": id})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_sender_the_target_names_gets_through() {
        assert_eq!(refusal("meta", None, "active", false).unwrap().0, "directive_sender_not_allowed");
        assert_eq!(refusal("meta", Some("other"), "active", false).unwrap().0, "directive_sender_not_allowed");
        assert_eq!(refusal("", Some("meta"), "active", false).unwrap().0, "directive_sender_not_allowed");
        assert_eq!(refusal("met", Some("meta"), "active", false).unwrap().0, "directive_sender_not_allowed");
        assert!(refusal("meta", Some("other, 'meta'"), "active", false).is_none());
    }

    #[test]
    fn an_isolated_or_paused_target_is_refused_even_for_a_named_sender() {
        assert_eq!(refusal("meta", Some("meta"), "active", true).unwrap().0, "directive_target_isolated");
        assert_eq!(refusal("meta", Some("meta"), "paused", false).unwrap().0, "directive_target_not_active");
    }

    #[test]
    fn the_group_opening_exists_only_inside_a_delivery() {
        assert!(!delivering("meta", "w"));
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(DELIVERING.scope(
            ("meta".to_string(), "w".to_string()),
            async {
                assert!(delivering("meta", "w"));
                assert!(!delivering("meta", "other"));
                assert!(!delivering("other", "w"));
            },
        ));
    }

    #[test]
    fn the_sender_cannot_answer_its_own_directive() {
        assert!(may_ack("w", "w"));
        assert!(may_ack("", "w"));
        assert!(!may_ack("meta", "w"));
    }

    #[test]
    fn an_open_directive_is_retried_then_repeated_then_told() {
        assert_eq!(due(&json!({"tries": 1}), 100.0, 600.0), Due::Deliver);
        assert_eq!(due(&json!({"tries": MAX_TRIES}), 100.0, 600.0), Due::GiveUp);
        assert_eq!(due(&json!({"delivered_at": 100.0}), 200.0, 600.0), Due::Nothing);
        assert_eq!(due(&json!({"delivered_at": 100.0}), 700.0, 600.0), Due::Resend);
        assert_eq!(due(&json!({"delivered_at": 100.0, "resent_at": 700.0}), 800.0, 600.0), Due::Nothing);
        assert_eq!(due(&json!({"delivered_at": 100.0, "resent_at": 700.0}), 1300.0, 600.0), Due::Tell);
        assert_eq!(due(&json!({"delivered_at": 100.0, "resent_at": 700.0, "told_at": 1300.0}), 9000.0, 600.0), Due::Nothing);
    }

    #[test]
    fn a_restart_finds_an_undelivered_directive_and_not_an_answered_one() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(include_str!("../../migrations/0156_directives.sql")).unwrap();
        c.execute("INSERT INTO directives (id, session, sender, text, issued_at) VALUES ('a','w','m','t',1)", []).unwrap();
        c.execute("INSERT INTO directives (id, session, sender, text, issued_at, acked_at) VALUES ('b','w','m','t',1,2)", []).unwrap();
        let open = open_rows(&c).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0]["id"], "a");
        assert_eq!(due(&open[0], 2.0, 600.0), Due::Deliver);
    }

    #[test]
    fn the_message_tells_the_worker_how_to_answer() {
        let t = delivered_text("DIR-1", "stop X");
        assert!(t.contains("stop X") && t.contains("amux directive ack DIR-1"));
    }
}
