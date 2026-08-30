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

    // Persist the outcome: status change + log line through the board store.
    let id2 = id.clone();
    let target = if passed { "verified" } else { "doing" };
    let write = state
        .store
        .write_async(move |conn| {
            let mut row = board_store::get_issue(conn, &id2)?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let now = chrono::Local::now();
            row.status = board_store::status_to_db(
                if passed {
                    amux_core::board::TaskStatus::Verified
                } else {
                    amux_core::board::TaskStatus::Doing
                },
                &row.status,
            );
            row.log = Some(board_store::append_log(
                row.log.as_deref(),
                &now.format("%H:%M").to_string(),
                &summary,
            ));
            if passed {
                row.last_verified_at = Some(chrono::Utc::now().timestamp());
            }
            board_store::save_patched(conn, &mut row)?;
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Task,
                    entity_id: id2.clone(),
                    mutation: amux_core::revision::MutationKind::StatusChanged {
                        from: "done".into(),
                        to: target.into(),
                    },
                    // RR-0111a: the row just saved is in hand — journal its
                    // post-mutation snapshot so replay covers verification
                    // outcomes too.
                    payload: Some(row.snapshot()),
                }],
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
