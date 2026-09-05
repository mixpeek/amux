//! POST /api/verify/{board_id} — execute typed verification criteria for a
//! Done task (RR-0076, Invariant 7).
//!
//! The caller supplies TYPED criteria (VerifierKind) because the live
//! board's string gate criteria ("Deployed to prod") are ack-based, not
//! executable; typed criteria stored on tasks arrive with RR-0048d. On
//! Passed the task moves done -> verified with the run recorded in its log;
//! on Failed it moves done -> doing carrying the rejection reason — the
//! verification loop from Invariant 7, executable today.

use super::AppState;
use crate::db::board_store;
use amux_core::board::GateCriterion;
use amux_core::verification::VerificationResult;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

pub fn routes() -> Router<AppState> {
    Router::new().route("/{id}", axum::routing::post(verify_task))
}

#[derive(Deserialize)]
pub struct VerifyRequest {
    pub criteria: Vec<GateCriterion>,
    #[serde(default)]
    pub cwd: String,
}

async fn verify_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(req): Json<VerifyRequest>,
) -> Response {
    if req.criteria.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no criteria supplied — a verification with nothing to check cannot fail, which makes it theatre"})),
        )
            .into_response();
    }
    // Load and gate on status = done.
    let row = {
        let conn = match state.store.read() {
            Ok(c) => c,
            Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
        };
        match board_store::get_issue(&conn, &id) {
            Ok(Some(r)) => r,
            Ok(None) => {
                return (StatusCode::NOT_FOUND, Json(json!({"error": "no such task", "item": id})))
                    .into_response()
            }
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    };
    if board_store::parse_status(&row.status) != Some(amux_core::board::TaskStatus::Done) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "verification runs against done tasks",
                "item": id,
                "status": row.status,
            })),
        )
            .into_response();
    }

    let run = match crate::orchestrator::verify::run_verification_async(req.criteria, req.cwd).await
    {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let actor = headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("api-anonymous")
        .to_string();
    let passed = matches!(run.verdict, VerificationResult::Passed);
    let summary = match &run.verdict {
        VerificationResult::Passed => format!(
            "verification PASSED ({} criteria) by {actor}",
            run.ran.len()
        ),
        VerificationResult::Failed { reason } => {
            format!("verification FAILED: {reason} (by {actor})")
        }
    };

    // Persist the outcome: status change + log line + structured verification
    // record through the board store.
    let id2 = id.clone();
    let actor2 = actor.clone();
    let run_json = serde_json::to_string(&run).ok();
    let target = if passed { "verified" } else { "doing" };
    let verdict_str = if passed { "passed" } else { "failed" };
    let reason_str = match &run.verdict {
        VerificationResult::Failed { reason } => Some(reason.clone()),
        _ => None,
    };
    let write = state
        .store
        .write_async(move |conn| {
            let opts = crate::db::advance::AdvanceOpts {
                force: true,
                expected_from: Some("done".into()),
                log_line: Some(summary),
                skip_continuation: true,
                ..Default::default()
            };
            let result = crate::db::advance::advance(conn, &id2, target, &actor2, &opts)?;
            let events = match result {
                Ok(outcome) => {
                    if passed {
                        conn.execute(
                            "UPDATE issues SET last_verified_at = ?1 WHERE id = ?2",
                            rusqlite::params![chrono::Utc::now().timestamp(), id2],
                        )?;
                    }
                    outcome.events
                }
                Err(_) => {
                    return Ok(crate::db::WriteOutcome {
                        applied: false,
                        events: vec![],
                    });
                }
            };
            let ver_id = format!("VER-{}", ulid::Ulid::new().to_string().to_lowercase());
            let ver_row = crate::db::verification_store::VerificationRow {
                id: ver_id,
                task_id: id2.clone(),
                verifier: json!({"type": "system", "component": "verify_api"}).to_string(),
                criteria: "[]".into(),
                evidence: "[]".into(),
                verdict: verdict_str.into(),
                reason: reason_str,
                run_detail: run_json,
                actor: actor2,
                created_at: chrono::Utc::now().timestamp(),
            };
            crate::db::verification_store::insert(conn, &ver_row)?;
            Ok(crate::db::WriteOutcome {
                applied: true,
                events,
            })
        })
        .await;
    if let Err(e) = write {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }

    (
        StatusCode::OK,
        Json(json!({
            "item": id,
            "verdict": run.verdict,
            "ran": run.ran,
            "skipped": run.skipped,
            "new_status": target,
        })),
    )
        .into_response()
}
