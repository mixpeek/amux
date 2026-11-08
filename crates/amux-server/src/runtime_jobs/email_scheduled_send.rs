//! Sends held outbound email at the time its approver scheduled (Ethan,
//! 2026-11-08: "when i approve outbound email there should be another button
//! to schedule for a date/time").
//!
//! Scheduling records the human's decision on the pending approval
//! (`email_approval::schedule`). Every tick this job releases each one whose
//! time has come through `api::email::approve`, the endpoint the Approve
//! button calls, with the approver and edited body the human scheduled it
//! with. That keeps one release path: the same creator check, the same
//! one-shot `consume` (so a scheduled email cannot go out twice, even across
//! a restart mid-send), and the same send-audit row, which also carries
//! `scheduled_for`. A release that fails goes back to plain pending with
//! `schedule_failed` on it, for a human to look at again.
//!
//! Switch: `AMUX_EMAIL_SCHEDULED_SEND_SECS=0` disables the job.

use crate::api::AppState;
use axum::http::{HeaderMap, HeaderValue};
use serde_json::{json, Value};
use std::path::Path;

const JOB: &str = super::registry::ids::EMAIL_SCHEDULED_SEND;

fn tick_secs() -> u64 {
    crate::config::env_i64("AMUX_EMAIL_SCHEDULED_SEND_TICK_S", 30).max(5) as u64
}

/// The release for one due approval. Separate from the tick so a test can
/// drive it with a mock transport.
pub(crate) async fn release(
    ctx: std::sync::Arc<crate::api::email::EmailCtx>,
    id: &str,
    approver: &str,
    body: Option<String>,
) -> (u16, Value) {
    let mut h = HeaderMap::new();
    let who = if approver.trim().is_empty() { "scheduled send" } else { approver };
    h.insert(
        "x-amux-approver",
        HeaderValue::from_str(who).unwrap_or(HeaderValue::from_static("scheduled send")),
    );
    let req = body.map(|b| axum::Json(json!({ "body": b })));
    let resp = crate::api::email::approve(axum::Extension(ctx), h, axum::extract::Path(id.to_string()), req).await;
    let code = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024).await.unwrap_or_default();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Release every due approval; returns how many were sent (each outcome is
/// also logged, so the spawned tick has nothing left to report).
pub(crate) async fn tick_with(ctx: std::sync::Arc<crate::api::email::EmailCtx>, home: &Path, now: f64) -> usize {
    let due = crate::api::email_approval::due_scheduled(home, now);
    let mut sent = 0;
    for (id, approver, body) in &due {
        let (code, v) = release(ctx.clone(), id, approver, body.clone()).await;
        if (200..300).contains(&code) {
            sent += 1;
            tracing::info!(approval = %id, measured = true, n_considered = 1,
                verdict = "email_scheduled_sent", "[email] a scheduled send went out");
        } else {
            tracing::warn!(approval = %id, status = code, error = %v["error"].as_str().unwrap_or(""),
                measured = true, n_considered = 1, verdict = "email_scheduled_send_failed",
                "[email] a scheduled send did not go out");
        }
    }
    if !due.is_empty() {
        tracing::info!(measured = true, n_considered = due.len(), sent,
            verdict = "email_scheduled_tick", "[email] scheduled sends released");
    }
    sent
}

pub fn spawn(_state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, tick_secs(), move || async move {
        let ctx = std::sync::Arc::new(crate::api::email::EmailCtx {
            client: std::sync::Arc::new(crate::integrations::email::GmailClient::new_default()),
            registry: crate::integrations::global_registry().clone(),
        });
        let home = crate::config::amux_home();
        tick_with(ctx, &home, crate::config::now_f64()).await;
    })
}
