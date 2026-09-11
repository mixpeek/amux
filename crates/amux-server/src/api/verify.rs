//! Canonical task verification.
//!
//! Criteria are loaded by immutable task/version reference; the request may
//! not supply executable code or a working directory. Verification evidence
//! is produced by the harness, persisted in full, and advances through the
//! ordinary transition engine with a gate acknowledgement rather than force.

use super::AppState;
use crate::db::{board_store, verification_store};
use amux_core::board::GateCriterion;
use amux_core::verification::{VerificationResult, VerifierKind};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/{id}",
        axum::routing::get(list_verifications).post(verify_task),
    )
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyRequest {
    /// Optional compare-and-set guard. Omission means the current stored
    /// version; a stale explicit value is refused.
    #[serde(default)]
    pub criteria_version: Option<u32>,
    /// A custom workflow gate remains authoritative alongside executable criteria.
    #[serde(default)]
    pub gate_checked: Vec<String>,
}

fn actor(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("api-anonymous")
        .to_string()
}

fn kind_name(kind: &VerifierKind) -> &'static str {
    match kind {
        VerifierKind::Command { .. } => "command",
        VerifierKind::HttpCheck { .. } => "http_check",
        VerifierKind::FileExists { .. } => "file_exists",
        VerifierKind::Temporal { .. } => "temporal",
        VerifierKind::PlaywrightAssertion { .. } => "playwright_assertion",
        VerifierKind::ModelJudgment { .. } => "model_judgment",
    }
}

async fn list_verifications(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    match verification_store::list_for_task(&conn, &id) {
        Ok(items) => {
            Json(json!({"item": id, "total": items.len(), "items": items})).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn verify_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(req): Json<VerifyRequest>,
) -> Response {
    let verifying_actor = actor(&headers);
    if verifying_actor == "api-anonymous" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "independent verification requires a named verifier"})),
        )
            .into_response();
    }
    let (row, criteria, cwd, harness_version, gate_snapshot) = {
        let conn = match state.store.read() {
            Ok(c) => c,
            Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
        };
        let row = match board_store::get_issue(&conn, &id) {
            Ok(Some(r)) => r,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "no such task", "item": id})),
                )
                    .into_response()
            }
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
        if !matches!(board_store::parse_status(&row.status), Some(amux_core::board::TaskStatus::Done | amux_core::board::TaskStatus::Verified)) {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "verification runs against done or previously verified tasks", "item": id, "status": row.status})),
            )
                .into_response();
        }
        let owner_worker = match row.session.as_deref().filter(|owner| !owner.is_empty()) {
            Some(owner) => match crate::db::queries::get_worker(&conn, owner) {
                Ok(worker) => worker,
                Err(e) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
                }
            },
            None => None,
        };
        let verifier_worker = match crate::db::queries::get_worker(&conn, &verifying_actor) {
            Ok(worker) => worker,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
        let same_owner = row
            .session
            .as_deref()
            .is_some_and(|owner| owner == verifying_actor)
            || owner_worker
                .as_ref()
                .zip(verifier_worker.as_ref())
                .is_some_and(|(owner, verifier)| owner.id == verifier.id);
        if same_owner {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "task owner cannot independently verify its own completion",
                    "item": id,
                    "owner": verifying_actor,
                })),
            )
                .into_response();
        }
        let criteria = match super::criteria::load(&conn, &id) {
            Ok(Some(c)) if !c.criteria.is_empty() => c,
            Ok(_) => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"error": "no stored acceptance criteria", "item": id, "hint": format!("PUT /api/criteria/{id}")})),
                )
                    .into_response()
            }
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
        if req
            .criteria_version
            .is_some_and(|expected| expected != criteria.version)
        {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "criteria version changed", "expected": req.criteria_version, "actual": criteria.version})),
            )
                .into_response();
        }
        let groups = row.session.as_deref().map(super::session_verbs::lane_groups).unwrap_or_default();
        let gate = board_store::effective_gate_trail(&conn, &row, amux_core::board::TaskStatus::Verified, &groups);
        if gate.source != board_store::GateSource::TypeDefault && gate.criteria.iter().any(|c| !req.gate_checked.contains(c)) {
            return (StatusCode::CONFLICT, Json(json!({"error":"custom verified gate requires the current checklist", "gate":gate.criteria,
                "source":gate.source.token(), "how_to_ack":{"gate_checked":gate.criteria}}))).into_response();
        }
        let sensor_profile = match crate::db::harness_store::get_sensor_profile(
            &conn,
            &row.item_type,
        ) {
            Ok(profile) => profile,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
        if let Some(profile) = sensor_profile {
            let actual: std::collections::BTreeSet<_> = criteria
                .criteria
                .iter()
                .map(|c| kind_name(&c.verifier))
                .collect();
            let missing: Vec<_> = profile
                .criteria
                .iter()
                .map(|c| kind_name(&c.verifier))
                .filter(|kind| !actual.contains(kind))
                .collect();
            if !missing.is_empty() {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "task criteria do not cover the required sensor profile",
                        "task_type": row.item_type,
                        "profile_version": profile.version,
                        "missing_verifier_kinds": missing,
                    })),
                )
                    .into_response();
            }
        }
        let cwd = owner_worker
            .map(|worker| worker.cwd)
            .unwrap_or_else(|| ".".into());
        let cwd = match std::fs::canonicalize(&cwd) {
            Ok(path) if path.is_dir() => path.to_string_lossy().into_owned(),
            _ => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"error": "task workspace is not an existing directory", "workspace": cwd})),
                )
                    .into_response()
            }
        };
        let harness_version = match crate::db::harness_store::harness_version(&conn) {
            Ok(version) => version,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
        (row, criteria, cwd, harness_version, gate.criteria)
    };

    let executable: Vec<GateCriterion> = criteria
        .criteria
        .iter()
        .map(|criterion| GateCriterion {
            description: criterion.description.clone(),
            verifier: criterion.verifier.clone(),
            required: criterion.required,
        })
        .collect();
    let execution =
        match crate::orchestrator::verify::run_verification_async(executable, cwd).await {
            Ok(run) => run,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    let passed = matches!(execution.run.verdict, VerificationResult::Passed)
        && !execution.evidence.is_empty();
    let target = if passed { "verified" } else { "doing" };
    let verdict_str = if passed { "passed" } else { "failed" };
    let reason_str = match &execution.run.verdict {
        VerificationResult::Failed { reason } => Some(reason.clone()),
        VerificationResult::Passed if execution.evidence.is_empty() => {
            Some("verification produced no independent evidence".into())
        }
        VerificationResult::Passed => None,
    };
    let summary = if passed {
        format!(
            "verification PASSED ({} criteria, criteria v{}) by independent harness for {verifying_actor}",
            execution.run.ran.len(),
            criteria.version
        )
    } else {
        format!(
            "verification FAILED: {} (requested by {verifying_actor})",
            reason_str.clone().unwrap_or_else(|| "unknown".into())
        )
    };
    let ver_id = format!("VER-{}", ulid::Ulid::new().to_string().to_lowercase());
    let mut recorded_run = serde_json::to_value(&execution).unwrap_or_default();
    recorded_run["gate_snapshot"] = json!(gate_snapshot);
    let run_json = match serde_json::to_string(&recorded_run) {
        Ok(value) => Some(value),
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    };
    let evidence_json = match serde_json::to_string(&execution.evidence) {
        Ok(value) => value,
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    };
    let criteria_ids: Vec<_> = criteria.criteria.iter().map(|c| c.id.clone()).collect();
    let criteria_json = match serde_json::to_string(&criteria_ids) {
        Ok(value) => value,
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    };
    let actor2 = verifying_actor.clone();
    let id2 = id.clone();
    let ver_id_for_write = ver_id.clone();
    let harness_version_for_write = harness_version.clone();
    let duration_ms = execution.duration_ms;
    let criteria_version = criteria.version;
    let expected_status = row.status.clone();
    let write = state
        .store
        .write_async(move |conn| {
            // Verification executes outside the writer. A concurrent criteria
            // amendment must never certify a result against the new version.
            if super::criteria::load(conn, &id2)?.as_ref() != Some(&criteria) {
                tracing::warn!(target: "amux::verification", card = %id2, criteria_version,
                    "verification refused: criteria changed during execution");
                return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                    std::io::Error::other("criteria changed during verification; run the current version"))));
            }
            let current = board_store::get_issue(conn, &id2)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let groups = current.session.as_deref().map(super::session_verbs::lane_groups).unwrap_or_default();
            if board_store::effective_gate_trail(conn, &current, amux_core::board::TaskStatus::Verified, &groups).criteria != gate_snapshot {
                tracing::warn!(target:"amux::verification", card=%id2, "gate changed during verification; result not applied");
                return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other("gate changed during verification"))));
            }
            let opts = crate::db::advance::AdvanceOpts {
                force: false,
                gate_ack: true,
                expected_from: Some(expected_status.clone()),
                log_line: Some(summary.clone()),
                skip_continuation: true,
                ..Default::default()
            };
            let events = if passed && expected_status == "verified" {
                let mut current = board_store::get_issue(conn, &id2)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
                if current.status != expected_status || current.archived != 0 {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        std::io::Error::other("task changed during verification"))));
                }
                current.log = Some(board_store::append_log(current.log.as_deref(), &chrono::Local::now().format("%H:%M").to_string(), &summary));
                current.updated = chrono::Utc::now().timestamp();
                current.rev += 1;
                current.version += 1;
                board_store::save_patched(conn, &mut current)?;
                vec![crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Task,
                    entity_id: id2.clone(),
                    mutation: amux_core::revision::MutationKind::Updated,
                    payload: Some(current.snapshot()),
                }]
            } else {
            let result = crate::db::advance::advance(conn, &id2, target, &actor2, &opts)?;
            match result {
                Ok(outcome) => outcome.events,
                Err(refusal) => {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        std::io::Error::other(format!(
                            "verification transition refused: {refusal:?}"
                        )),
                    )))
                }
            }
            };
            if passed {
                conn.execute(
                    "UPDATE issues SET last_verified_at=?1 WHERE id=?2",
                    rusqlite::params![chrono::Utc::now().timestamp(), id2],
                )?;
            }
            verification_store::insert(
                conn,
                &verification_store::VerificationRow {
                    id: ver_id_for_write,
                    task_id: id2,
                    verifier: json!({"type": "system", "component": "verify_api"}).to_string(),
                    criteria: criteria_json,
                    evidence: evidence_json,
                    verdict: verdict_str.into(),
                    reason: reason_str,
                    run_detail: run_json,
                    actor: actor2,
                    created_at: chrono::Utc::now().timestamp(),
                    criteria_version,
                    harness_version: Some(harness_version_for_write),
                    duration_ms,
                },
            )?;
            Ok(crate::db::WriteOutcome {
                applied: true,
                events,
            })
        })
        .await;
    if let Err(e) = write {
        return (StatusCode::CONFLICT, e.to_string()).into_response();
    }

    (
        StatusCode::OK,
        Json(json!({
            "item": row.id,
            "verification_id": ver_id,
            "criteria_version": criteria_version,
            "harness_version": harness_version,
            "verdict": execution.run.verdict,
            "evidence": execution.evidence,
            "ran": execution.run.ran,
            "skipped": execution.run.skipped,
            "duration_ms": execution.duration_ms,
            "new_status": target,
        })),
    )
        .into_response()
}
