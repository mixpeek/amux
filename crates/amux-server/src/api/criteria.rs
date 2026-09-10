//! /api/criteria/{board_id} — stored acceptance criteria (RR-0048d,
//! Invariant 50).
//!
//! Criteria are authored BEFORE execution by someone other than the
//! executor: `validate_authorship` rejects a worker writing criteria for a
//! task it owns. Enforcement of "cannot leave todo without criteria" is
//! opt-in via AMUX_RS_REQUIRE_CRITERIA=1 during coexistence — the Python
//! fleet does not author criteria yet, and a gate nobody can satisfy
//! honestly teaches lying (ethos rule 3).

use super::AppState;
use crate::db::board_store;
use amux_core::criteria::{validate_authorship, AcceptanceCriteria};
use amux_core::ids::WorkerId;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::json;

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/{id}",
        axum::routing::get(get_criteria).put(put_criteria),
    )
}

pub fn load(
    conn: &rusqlite::Connection,
    task_id: &str,
) -> rusqlite::Result<Option<AcceptanceCriteria>> {
    conn.query_row(
        "SELECT criteria, version FROM _amux_criteria WHERE task_id = ?1",
        [task_id],
        |r| {
            let raw: String = r.get(0)?;
            let mut criteria: AcceptanceCriteria = serde_json::from_str(&raw)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            // The writer owns this version; older JSON may retain the caller's
            // value even though the database revision has advanced.
            criteria.version = r.get(1)?;
            Ok(criteria)
        },
    )
    .optional()
}

/// The RR-0048d gate: does this task have criteria, when enforcement is on?
/// Called by the board PATCH path before a todo -> anything-forward move.
pub fn todo_exit_permitted(
    conn: &rusqlite::Connection,
    task_id: &str,
) -> rusqlite::Result<std::result::Result<(), String>> {
    let enforced = std::env::var("AMUX_RS_REQUIRE_CRITERIA").map(|v| v == "1").unwrap_or(false);
    if !enforced {
        return Ok(Ok(()));
    }
    match load(conn, task_id)? {
        Some(c) if !c.criteria.is_empty() => Ok(Ok(())),
        _ => Ok(Err(format!(
            "task {task_id} has no acceptance criteria — author them (PUT /api/criteria/{task_id}) \
             before execution starts (Invariant 50); a task nobody defined success for \
             cannot honestly finish"
        ))),
    }
}

async fn get_criteria(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    match load(&conn, &id) {
        Ok(Some(c)) => Json(serde_json::to_value(c).unwrap_or_default()).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no criteria authored", "item": id})),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct PutCriteria {
    #[serde(flatten)]
    criteria: AcceptanceCriteria,
}

async fn put_criteria(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PutCriteria>,
) -> Response {
    if body.criteria.criteria.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "empty criteria list — authoring nothing defines nothing"})),
        )
            .into_response();
    }
    // Authorship separation (Invariant 50): the task's EXECUTOR (its owning
    // worker) may not author its own acceptance. Owner resolution: the
    // issues row's session name -> registered worker id.
    {
        let conn = match state.store.read() {
            Ok(c) => c,
            Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
        };
        let row = match board_store::get_issue(&conn, &id) {
            Ok(Some(r)) => r,
            Ok(None) => {
                return (StatusCode::NOT_FOUND, Json(json!({"error": "no such task", "item": id})))
                    .into_response()
            }
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
        if let Some(owner_name) = row.session.as_deref().filter(|s| !s.is_empty()) {
            if let Ok(Some(owner)) = crate::db::queries::get_worker(&conn, owner_name) {
                if let Ok(executor) = WorkerId::parse(&owner.id) {
                    if let Err(e) = validate_authorship(&body.criteria, &executor) {
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({
                                "error": e.to_string(),
                                "item": id,
                                "executor": owner.id,
                                "hint": "criteria must be authored by a different worker or by Document (Invariant 50)",
                            })),
                        )
                            .into_response();
                    }
                }
            }
        }
        // A human author (CriteriaAuthor::Document) always satisfies
        // separation; unresolvable owners (Python fleet names) cannot be
        // structurally compared — allowed, recorded as-is.
    }

    let id2 = id.clone();
    let mut criteria = body.criteria;
    let author_json = serde_json::to_string(&criteria.authored_by).unwrap_or_default();
    let result = state
        .store
        .write_async(move |conn| {
            criteria.version = load(conn, &id2)?.map(|c| c.version + 1).unwrap_or(1);
            let criteria_json = serde_json::to_string(&criteria)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            tracing::info!(target: "amux::verification", card = %id2, criteria_version = criteria.version,
                "acceptance criteria amended; earlier verification does not cover this version");
            conn.execute(
                "INSERT INTO _amux_criteria (task_id, criteria, authored_by, version, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(task_id) DO UPDATE SET
                   criteria = ?2, authored_by = ?3,
                   version = _amux_criteria.version + 1, updated_at = ?5",
                rusqlite::params![
                    id2,
                    criteria_json,
                    author_json,
                    criteria.version,
                    chrono::Utc::now().to_rfc3339()
                ],
            )?;
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Other("criteria".into()),
                    entity_id: id2.clone(),
                    mutation: amux_core::revision::MutationKind::Updated,
                    payload: None,
                }],
            })
        })
        .await;
    match result {
        Ok(_) => Json(json!({"ok": true, "item": id})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amux_core::criteria::{Criterion, CriteriaAuthor};
    use amux_core::ids::CriterionId;

    #[test]
    fn todo_exit_gate_defaults_open_and_enforces_when_set() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate::apply_all(&mut conn).unwrap();
        // Default: enforcement off, always permitted.
        std::env::remove_var("AMUX_RS_REQUIRE_CRITERIA");
        assert!(todo_exit_permitted(&conn, "AMUX-1").unwrap().is_ok());
        // Enforced + no criteria: refused with the authoring pointer.
        std::env::set_var("AMUX_RS_REQUIRE_CRITERIA", "1");
        let refused = todo_exit_permitted(&conn, "AMUX-1").unwrap();
        assert!(refused.is_err());
        assert!(refused.unwrap_err().contains("PUT /api/criteria/AMUX-1"));
        // With criteria stored: permitted.
        let c = AcceptanceCriteria {
            criteria: vec![Criterion {
                id: CriterionId::from_ulid("01JGXV0000000000000000TEST".parse().unwrap()),
                description: "tests pass".into(),
                verifier: amux_core::verification::VerifierKind::Command {
                    cmd: "true".into(),
                    expected_exit: 0,
                },
                required: true,
            }],
            authored_by: CriteriaAuthor::Document,
            version: 1,
        };
        conn.execute(
            "INSERT INTO _amux_criteria (task_id, criteria, authored_by, version, updated_at)
             VALUES ('AMUX-1', ?1, ?2, 1, 'now')",
            rusqlite::params![
                serde_json::to_string(&c).unwrap(),
                serde_json::to_string(&c.authored_by).unwrap()
            ],
        )
        .unwrap();
        assert!(todo_exit_permitted(&conn, "AMUX-1").unwrap().is_ok());
        std::env::remove_var("AMUX_RS_REQUIRE_CRITERIA");
    }
}
