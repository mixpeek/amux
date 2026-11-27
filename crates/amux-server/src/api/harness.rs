//! Production-harness control and measurement plane.
//!
//! These endpoints expose the durable objects the runtime already enforces:
//! checkpoints/handoffs, sensor profiles, and versioned
//! guide rules.  Compilation and health responses always carry a measured
//! flag and population size so an empty database cannot masquerade as a pass.

use super::AppState;
use amux_core::harness::{
    EnforcementLayer, GuideRule, GuideRuleStatus, HandoffPacket, SensorProfile, TaskCheckpoint,
};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use chrono::{Duration, Utc};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/checkpoints/{id}",
            axum::routing::get(get_checkpoint).put(put_checkpoint),
        )
        .route(
            "/handoffs/{id}",
            axum::routing::get(get_handoff).post(post_handoff),
        )
        .route("/sensors", axum::routing::get(list_sensors))
        .route(
            "/sensors/{task_type}",
            axum::routing::get(get_sensor).put(put_sensor),
        )
        .route("/guides", axum::routing::get(list_guides).put(put_guide))
        .route("/compile", axum::routing::post(compile_harness))
        .route("/ratchet", axum::routing::post(ratchet))
        .route("/traces/{turn_id}", axum::routing::get(get_turn_traces))
        .route("/work-metrics", axum::routing::post(post_work_metric))
        .route(
            "/adaptive-wip",
            axum::routing::get(get_adaptive_wip).put(put_adaptive_wip),
        )
        .route("/health", axum::routing::get(health))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkMetricInput {
    kind: String,
    duration_ms: Option<u64>,
    task_id: Option<String>,
    detail: Option<String>,
}

async fn post_work_metric(
    State(state): State<AppState>,
    Json(body): Json<WorkMetricInput>,
) -> Response {
    let result = std::sync::Arc::new(std::sync::Mutex::new(None));
    let result_w = result.clone();
    match state
        .store
        .write_async(move |conn| {
            let id = crate::db::throughput_store::record_metric(
                conn,
                &body.kind,
                "api",
                body.duration_ms,
                body.task_id.as_deref(),
                body.detail.as_deref(),
                Utc::now(),
            )?;
            let wip = crate::db::throughput_store::evaluate_wip(conn, Utc::now())?;
            *result_w.lock().expect("metric result") = Some((id.clone(), wip));
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Other("work_metric".into()),
                    entity_id: id,
                    mutation: amux_core::revision::MutationKind::Created,
                    payload: None,
                }],
            })
        })
        .await
    {
        Ok(_) => match result.lock().expect("metric result").clone() {
            Some((id, adaptive_wip)) => (
                StatusCode::CREATED,
                Json(json!({"id": id, "adaptive_wip": adaptive_wip})),
            )
                .into_response(),
            None => internal("metric write produced no result"),
        },
        Err(error) => internal(error),
    }
}

async fn get_adaptive_wip(State(state): State<AppState>) -> Response {
    let conn = match state.store.read() {
        Ok(conn) => conn,
        Err(error) => return internal(error),
    };
    match crate::db::throughput_store::get_wip_state(&conn) {
        Ok(wip) => Json(json!({"measured": true, "n_considered": wip.sample_size, "state": wip}))
            .into_response(),
        Err(error) => internal(error),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WipInput {
    mode: String,
    current_limit: usize,
    min_limit: usize,
    max_limit: usize,
}

async fn put_adaptive_wip(State(state): State<AppState>, Json(body): Json<WipInput>) -> Response {
    let result = std::sync::Arc::new(std::sync::Mutex::new(None));
    let result_w = result.clone();
    match state
        .store
        .write_async(move |conn| {
            let wip = crate::db::throughput_store::configure_wip(
                conn,
                &body.mode,
                body.current_limit,
                body.min_limit,
                body.max_limit,
                Utc::now(),
            )?;
            *result_w.lock().expect("wip result") = Some(wip.clone());
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Other("adaptive_wip".into()),
                    entity_id: "singleton".into(),
                    mutation: amux_core::revision::MutationKind::Updated,
                    payload: serde_json::to_value(&wip).ok(),
                }],
            })
        })
        .await
    {
        Ok(_) => match result.lock().expect("wip result").clone() {
            Some(wip) => Json(wip).into_response(),
            None => internal("wip write produced no result"),
        },
        Err(error) => internal(error),
    }
}

async fn get_turn_traces(State(state): State<AppState>, Path(turn_id): Path<String>) -> Response {
    let conn = match state.store.read() {
        Ok(conn) => conn,
        Err(error) => return internal(error),
    };
    match crate::db::trace_store::list_for_turn(&conn, &turn_id) {
        Ok(items) => Json(json!({
            "measured": true,
            "n_considered": items.len(),
            "turn_id": turn_id,
            "items": items
        }))
        .into_response(),
        Err(error) => internal(error),
    }
}

fn actor(headers: &HeaderMap) -> String {
    headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("api-anonymous")
        .to_string()
}

fn internal(error: impl std::fmt::Display) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": error.to_string()})),
    )
        .into_response()
}

async fn get_checkpoint(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let conn = match state.store.read() {
        Ok(conn) => conn,
        Err(error) => return internal(error),
    };
    match crate::db::harness_store::get_checkpoint(&conn, &id) {
        Ok(Some(checkpoint)) => Json(checkpoint).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no checkpoint", "task_id": id})),
        )
            .into_response(),
        Err(error) => internal(error),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointInput {
    #[serde(default)]
    completed_steps: Vec<String>,
    next_action: String,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    unresolved: Vec<String>,
    input_hash: String,
    context_hash: Option<String>,
    last_result: Option<String>,
}

async fn put_checkpoint(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CheckpointInput>,
) -> Response {
    let checkpoint = TaskCheckpoint {
        task_id: id.clone(),
        version: 0,
        completed_steps: body.completed_steps,
        next_action: body.next_action,
        artifacts: body.artifacts,
        unresolved: body.unresolved,
        input_hash: body.input_hash,
        context_hash: body.context_hash,
        last_result: body.last_result,
        updated_by: actor(&headers),
        updated_at: Utc::now(),
    };
    if let Err(error) = checkpoint.validate() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response();
    }
    let result = std::sync::Arc::new(std::sync::Mutex::new(None));
    let result_w = result.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let stored = crate::db::harness_store::put_checkpoint(conn, &checkpoint)?;
            *result_w.lock().expect("checkpoint result") = Some(stored.clone());
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Other("checkpoint".into()),
                    entity_id: id,
                    mutation: amux_core::revision::MutationKind::Updated,
                    payload: serde_json::to_value(stored).ok(),
                }],
            })
        })
        .await;
    match write {
        Ok(_) => match result.lock().expect("checkpoint result").clone() {
            Some(stored) => Json(stored).into_response(),
            None => internal("checkpoint write produced no result"),
        },
        Err(error) => internal(error),
    }
}

async fn get_handoff(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let conn = match state.store.read() {
        Ok(conn) => conn,
        Err(error) => return internal(error),
    };
    match crate::db::harness_store::latest_handoff(&conn, &id) {
        Ok(Some(packet)) => Json(packet).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no handoff", "task_id": id})),
        )
            .into_response(),
        Err(error) => internal(error),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffInput {
    objective: String,
    #[serde(default)]
    criteria_version: u32,
    checkpoint_version: Option<u32>,
    checkpoint_hash: Option<String>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    evidence: Vec<String>,
    #[serde(default)]
    assumptions: Vec<String>,
    #[serde(default)]
    unresolved: Vec<String>,
    #[serde(default)]
    concerns: Vec<String>,
    #[serde(default)]
    deviations: Vec<String>,
    #[serde(default)]
    findings: Vec<String>,
    #[serde(default)]
    requires_replan: bool,
    planning_scope_id: Option<String>,
    next_action: String,
    deadline: Option<String>,
    receiver: String,
}

async fn post_handoff(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<HandoffInput>,
) -> Response {
    let sender = actor(&headers);
    // Planning nodes were retired with their table (audit step 2, migration
    // 0180), so a handoff can no longer be scoped to one.
    if body.planning_scope_id.is_some() {
        tracing::warn!(task = %id, measured = true, n_considered = 1, verdict = "handoff_planning_scope_retired",
            "a handoff named a planning node; planning nodes were retired");
        return (
            StatusCode::GONE,
            Json(json!({
                "error": "planning_scope_id names a planning node, and planning nodes were retired on 2026-11-26; send the handoff without it",
                "code": "planning_scope_retired"
            })),
        )
            .into_response();
    }
    let packet = HandoffPacket {
        id: format!("hof_{}", ulid::Ulid::new().to_string().to_lowercase()),
        task_id: id.clone(),
        assignment_key: None,
        objective: body.objective,
        criteria_version: body.criteria_version,
        checkpoint_version: body.checkpoint_version,
        checkpoint_hash: body.checkpoint_hash,
        artifacts: body.artifacts,
        evidence: body.evidence,
        assumptions: body.assumptions,
        unresolved: body.unresolved,
        concerns: body.concerns,
        deviations: body.deviations,
        findings: body.findings,
        requires_replan: body.requires_replan,
        planning_scope_id: body.planning_scope_id,
        next_action: body.next_action,
        deadline: body.deadline,
        sender,
        receiver: body.receiver,
        created_at: Utc::now(),
    };
    if let Err(error) = packet.validate() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response();
    }
    let packet_w = packet.clone();
    match state
        .store
        .write_async(move |conn| {
            let inserted = crate::db::harness_store::insert_handoff(conn, &packet_w)?;
            Ok(crate::db::WriteOutcome {
                applied: inserted,
                events: inserted
                    .then(|| crate::db::PendingEvent {
                        entity_type: amux_core::revision::EntityType::Other("handoff".into()),
                        entity_id: packet_w.id.clone(),
                        mutation: amux_core::revision::MutationKind::Created,
                        payload: serde_json::to_value(&packet_w).ok(),
                    })
                    .into_iter()
                    .collect(),
            })
        })
        .await
    {
        Ok(_) => (StatusCode::CREATED, Json(packet)).into_response(),
        Err(error) => internal(error),
    }
}

async fn list_sensors(State(state): State<AppState>) -> Response {
    let conn = match state.store.read() {
        Ok(conn) => conn,
        Err(error) => return internal(error),
    };
    match crate::db::harness_store::list_sensor_profiles(&conn) {
        Ok(items) => Json(json!({"total": items.len(), "items": items})).into_response(),
        Err(error) => internal(error),
    }
}

async fn get_sensor(State(state): State<AppState>, Path(task_type): Path<String>) -> Response {
    let conn = match state.store.read() {
        Ok(conn) => conn,
        Err(error) => return internal(error),
    };
    match crate::db::harness_store::get_sensor_profile(&conn, &task_type) {
        Ok(Some(profile)) => Json(profile).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no sensor profile", "task_type": task_type})),
        )
            .into_response(),
        Err(error) => internal(error),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SensorInput {
    criteria: Vec<amux_core::criteria::Criterion>,
}

async fn put_sensor(
    State(state): State<AppState>,
    Path(task_type): Path<String>,
    Json(body): Json<SensorInput>,
) -> Response {
    if task_type.trim().is_empty() || body.criteria.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "task_type and at least one criterion are required"})),
        )
            .into_response();
    }
    let profile = SensorProfile {
        task_type: task_type.clone(),
        criteria: body.criteria,
        version: 0,
        updated_at: Utc::now(),
    };
    let result = std::sync::Arc::new(std::sync::Mutex::new(0));
    let result_w = result.clone();
    let profile_w = profile.clone();
    match state
        .store
        .write_async(move |conn| {
            let version = crate::db::harness_store::put_sensor_profile(conn, &profile_w)?;
            *result_w.lock().expect("sensor version") = version;
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Other("sensor_profile".into()),
                    entity_id: task_type,
                    mutation: amux_core::revision::MutationKind::Updated,
                    payload: Some(json!({"version": version, "criteria": profile_w.criteria})),
                }],
            })
        })
        .await
    {
        Ok(_) => Json(json!({
            "profile": profile,
            "version": *result.lock().expect("sensor version")
        }))
        .into_response(),
        Err(error) => internal(error),
    }
}

#[derive(Debug, Default, Deserialize)]
struct GuideQuery {
    status: Option<String>,
}

async fn list_guides(State(state): State<AppState>, Query(query): Query<GuideQuery>) -> Response {
    let conn = match state.store.read() {
        Ok(conn) => conn,
        Err(error) => return internal(error),
    };
    match crate::db::harness_store::list_guide_rules(&conn) {
        Ok(mut items) => {
            if let Some(status) = query.status {
                items.retain(|item| {
                    serde_json::to_value(item.status)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .as_deref()
                        == Some(status.as_str())
                });
            }
            Json(json!({"total": items.len(), "items": items})).into_response()
        }
        Err(error) => internal(error),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuideInput {
    id: Option<String>,
    scope: String,
    name: String,
    content: String,
    owner: Option<String>,
    source_failure: String,
    rationale: String,
    status: GuideRuleStatus,
    enforcement_layer: EnforcementLayer,
    sensor_ref: Option<String>,
    expires_at: Option<chrono::DateTime<Utc>>,
    superseded_by: Option<String>,
}

async fn put_guide(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<GuideInput>,
) -> Response {
    let now = Utc::now();
    let rule = GuideRule {
        id: body
            .id
            .unwrap_or_else(|| format!("rule_{}", ulid::Ulid::new().to_string().to_lowercase())),
        scope: body.scope,
        name: body.name,
        content: body.content,
        owner: body.owner.unwrap_or_else(|| actor(&headers)),
        source_failure: body.source_failure,
        rationale: body.rationale,
        status: body.status,
        enforcement_layer: body.enforcement_layer,
        sensor_ref: body.sensor_ref,
        version: 0,
        added_at: now,
        last_validated_at: matches!(body.status, GuideRuleStatus::Active).then_some(now),
        expires_at: body.expires_at,
        superseded_by: body.superseded_by,
    };
    let errors = rule.validate(now);
    if !errors.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"errors": errors}))).into_response();
    }
    if rule.content.chars().count() > 4_000 {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"error": "guide rule content exceeds 4000 characters"})),
        )
            .into_response();
    }
    let result = std::sync::Arc::new(std::sync::Mutex::new(None));
    let result_w = result.clone();
    let id = rule.id.clone();
    match state
        .store
        .write_async(move |conn| {
            if let Some(sensor) = rule.sensor_ref.as_deref() {
                if crate::db::harness_store::get_sensor_profile(conn, sensor)?.is_none() {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        std::io::Error::other(format!("unknown sensor profile: {sensor}")),
                    )));
                }
            }
            let stored = crate::db::harness_store::put_guide_rule(conn, &rule)?;
            *result_w.lock().expect("guide result") = Some(stored.clone());
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Other("guide_rule".into()),
                    entity_id: id,
                    mutation: amux_core::revision::MutationKind::Updated,
                    payload: serde_json::to_value(stored).ok(),
                }],
            })
        })
        .await
    {
        Ok(_) => match result.lock().expect("guide result").clone() {
            Some(stored) => Json(stored).into_response(),
            None => internal("guide write produced no result"),
        },
        Err(error) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": error.to_string()})),
        )
            .into_response(),
    }
}

fn compile_report(
    conn: &rusqlite::Connection,
    now: chrono::DateTime<Utc>,
) -> rusqlite::Result<Value> {
    let rules = crate::db::harness_store::list_guide_rules(conn)?;
    let sensors = crate::db::harness_store::list_sensor_profiles(conn)?;
    let sensor_names: std::collections::BTreeSet<_> = sensors
        .iter()
        .map(|profile| profile.task_type.as_str())
        .collect();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut live_names: BTreeMap<(&str, &str), &str> = BTreeMap::new();
    let mut active_chars = 0usize;
    for rule in &rules {
        for error in rule.validate(now) {
            errors.push(json!({"rule": rule.id, "kind": "invalid", "detail": error}));
        }
        if matches!(
            rule.status,
            GuideRuleStatus::Active | GuideRuleStatus::Candidate
        ) {
            if let Some(prior) = live_names.insert((&rule.scope, &rule.name), &rule.id) {
                errors.push(json!({
                    "rule": rule.id,
                    "kind": "contradiction",
                    "detail": format!("same live scope/name as {prior}")
                }));
            }
        }
        if matches!(rule.status, GuideRuleStatus::Active) {
            active_chars = active_chars.saturating_add(rule.content.chars().count());
        }
        if rule.content.chars().count() > 4_000 {
            errors.push(json!({"rule": rule.id, "kind": "oversized", "chars": rule.content.chars().count()}));
        }
        if rule
            .last_validated_at
            .is_none_or(|last| now.signed_duration_since(last) > Duration::days(90))
            && matches!(rule.status, GuideRuleStatus::Active)
        {
            warnings.push(
                json!({"rule": rule.id, "kind": "stale", "detail": "not validated in 90 days"}),
            );
        }
        if let Some(sensor) = rule.sensor_ref.as_deref() {
            if !sensor_names.contains(sensor) {
                errors.push(json!({"rule": rule.id, "kind": "missing_sensor", "sensor": sensor}));
            }
        }
    }
    if active_chars > 120_000 {
        errors.push(json!({
            "kind": "context_budget",
            "detail": "active guide content exceeds the default context budget",
            "chars": active_chars
        }));
    }
    let version = crate::db::harness_store::harness_version(conn)?;
    Ok(json!({
        "ok": errors.is_empty(),
        "measured": true,
        "n_considered": rules.len() + sensors.len(),
        "rules_considered": rules.len(),
        "sensors_considered": sensors.len(),
        "active_guide_chars": active_chars,
        "harness_version": version,
        "errors": errors,
        "warnings": warnings
    }))
}

async fn compile_harness(State(state): State<AppState>) -> Response {
    let conn = match state.store.read() {
        Ok(conn) => conn,
        Err(error) => return internal(error),
    };
    match compile_report(&conn, Utc::now()) {
        Ok(report) if report["ok"] == true => Json(report).into_response(),
        Ok(report) => (StatusCode::UNPROCESSABLE_ENTITY, Json(report)).into_response(),
        Err(error) => internal(error),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RatchetInput {
    #[serde(default = "default_ratchet_threshold")]
    threshold: u32,
    #[serde(default = "default_ratchet_window_days")]
    window_days: u32,
}

const fn default_ratchet_threshold() -> u32 {
    3
}

const fn default_ratchet_window_days() -> u32 {
    30
}

fn clean_failure(reason: &str) -> String {
    reason
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\n')
        .take(500)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

async fn ratchet(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RatchetInput>,
) -> Response {
    if body.threshold < 2 || body.window_days == 0 || body.window_days > 3650 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "threshold must be >=2 and window_days must be 1..=3650"})),
        )
            .into_response();
    }
    let cutoff = (Utc::now() - Duration::days(body.window_days as i64)).timestamp();
    let owner = actor(&headers);
    let created = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let created_w = created.clone();
    let considered = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    let considered_w = considered.clone();
    let result = state
        .store
        .write_async(move |conn| {
            let failures: Vec<(String, u32)> = {
                let mut stmt = conn.prepare(
                    "SELECT reason, COUNT(*) FROM _amux_verifications
                     WHERE verdict='failed' AND reason IS NOT NULL AND created_at>=?1
                     GROUP BY reason HAVING COUNT(*)>=?2 ORDER BY COUNT(*) DESC",
                )?;
                let rows = stmt
                    .query_map(params![cutoff, body.threshold], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?
                    .collect::<Result<_, _>>()?;
                rows
            };
            *considered_w.lock().expect("ratchet population") = failures.len();
            let mut events = Vec::new();
            for (reason, count) in failures {
                let cleaned = clean_failure(&reason);
                let mut digest = Sha256::new();
                digest.update(reason.as_bytes());
                let suffix = hex::encode(&digest.finalize()[..6]);
                let id = format!("ratchet_{suffix}");
                let exists: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM _amux_harness_rules WHERE id=?1)",
                    [&id],
                    |row| row.get(0),
                )?;
                if exists {
                    continue;
                }
                let rule = GuideRule {
                    id: id.clone(),
                    scope: "global".into(),
                    name: format!("repeated-verification-failure-{suffix}"),
                    content: format!(
                        "Before claiming completion, explicitly prevent this repeated verification failure: {cleaned}"
                    ),
                    owner: owner.clone(),
                    source_failure: format!("verification:{suffix}"),
                    rationale: format!("observed {count} matching failures in {} days", body.window_days),
                    status: GuideRuleStatus::Candidate,
                    enforcement_layer: EnforcementLayer::Guide,
                    sensor_ref: None,
                    version: 0,
                    added_at: Utc::now(),
                    last_validated_at: None,
                    expires_at: Some(Utc::now() + Duration::days(90)),
                    superseded_by: None,
                };
                let stored = crate::db::harness_store::put_guide_rule(conn, &rule)?;
                created_w.lock().expect("ratchet result").push(stored.clone());
                events.push(crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Other("guide_candidate".into()),
                    entity_id: id,
                    mutation: amux_core::revision::MutationKind::Created,
                    payload: serde_json::to_value(stored).ok(),
                });
            }
            Ok(crate::db::WriteOutcome {
                applied: !events.is_empty(),
                events,
            })
        })
        .await;
    match result {
        Ok(_) => {
            let items = created.lock().expect("ratchet result").clone();
            Json(json!({
                "measured": true,
                "n_considered": *considered.lock().expect("ratchet population"),
                "created": items.len(),
                "items": items,
                "activation_required": true
            }))
            .into_response()
        }
        Err(error) => internal(error),
    }
}

#[derive(Debug, Deserialize)]
struct HealthQuery {
    #[serde(default = "default_health_days")]
    days: u32,
}

const fn default_health_days() -> u32 {
    30
}

fn ratio(numerator: u64, denominator: u64) -> Value {
    json!({
        "numerator": numerator,
        "denominator": denominator,
        "value": (denominator > 0).then(|| numerator as f64 / denominator as f64)
    })
}

async fn health(State(state): State<AppState>, Query(query): Query<HealthQuery>) -> Response {
    let days = query.days.clamp(1, 3650);
    let since = (Utc::now() - Duration::days(days as i64)).timestamp();
    let conn = match state.store.read() {
        Ok(conn) => conn,
        Err(error) => return internal(error),
    };
    let counts = (|| -> rusqlite::Result<Value> {
        let total_verifications: u64 = conn.query_row(
            "SELECT COUNT(*) FROM _amux_verifications WHERE created_at>=?1",
            [since],
            |row| row.get(0),
        )?;
        let passed: u64 = conn.query_row(
            "SELECT COUNT(*) FROM _amux_verifications WHERE created_at>=?1 AND verdict='passed'",
            [since],
            |row| row.get(0),
        )?;
        let verified_tasks: u64 = conn.query_row(
            "SELECT COUNT(DISTINCT task_id) FROM _amux_verifications
             WHERE created_at>=?1 AND verdict='passed'",
            [since],
            |row| row.get(0),
        )?;
        let reworked: u64 = conn.query_row(
            "SELECT COUNT(DISTINCT passed.task_id) FROM _amux_verifications passed
             WHERE passed.created_at>=?1 AND passed.verdict='passed'
               AND EXISTS(SELECT 1 FROM _amux_verifications failed
                          WHERE failed.task_id=passed.task_id AND failed.verdict='failed'
                            AND failed.created_at<=passed.created_at)",
            [since],
            |row| row.get(0),
        )?;
        let avg_verification_ms: Option<f64> = conn.query_row(
            "SELECT AVG(duration_ms) FROM _amux_verifications WHERE created_at>=?1",
            [since],
            |row| row.get(0),
        )?;
        let escalations: u64 = conn.query_row(
            "SELECT COUNT(*) FROM _amux_state_events
             WHERE at>=?1 AND entity_type='task'
               AND (mutation LIKE '%\"to\":\"needsyou\"%'
                    OR mutation LIKE '%\"to\":\"quarantined\"%')",
            [(Utc::now() - Duration::days(days as i64)).to_rfc3339()],
            |row| row.get(0),
        )?;
        let handoffs: u64 = conn.query_row(
            "SELECT COUNT(*) FROM _amux_handoffs WHERE created_at>=?1",
            [(Utc::now() - Duration::days(days as i64)).to_rfc3339()],
            |row| row.get(0),
        )?;
        let resumed: u64 = conn.query_row(
            "SELECT COUNT(*) FROM _amux_handoffs
             WHERE created_at>=?1 AND checkpoint_version IS NOT NULL",
            [(Utc::now() - Duration::days(days as i64)).to_rfc3339()],
            |row| row.get(0),
        )?;
        let (tokens, cost_usd): (u64, f64) = conn.query_row(
            "SELECT COALESCE(SUM(input+cache_read+cache_write+output),0),
                    COALESCE(SUM(cost_usd),0) FROM token_ledger WHERE ts>=?1",
            [since],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let tool_calls: u64 = conn.query_row(
            "SELECT COUNT(*) FROM _amux_tool_events WHERE created_at>=?1",
            [(Utc::now() - Duration::days(days as i64)).to_rfc3339()],
            |row| row.get(0),
        )?;
        let timeouts: u64 = conn.query_row(
            "SELECT COUNT(*) FROM _amux_verifications
             WHERE created_at>=?1 AND verdict='failed' AND LOWER(COALESCE(reason,'')) LIKE '%timeout%'",
            [since],
            |row| row.get(0),
        )?;
        let policy: Vec<Value> = {
            let mut stmt = conn.prepare(
                "SELECT effect,COUNT(*) FROM _amux_policy_receipts
                 WHERE created_at>=?1 GROUP BY effect ORDER BY effect",
            )?;
            let values = stmt
                .query_map(
                    [(Utc::now() - Duration::days(days as i64)).to_rfc3339()],
                    |row| Ok(json!({"effect": row.get::<_, String>(0)?, "count": row.get::<_, u64>(1)?})),
                )?
                .collect::<Result<_, _>>()?;
            values
        };
        let policy_total = policy
            .iter()
            .filter_map(|v| v["count"].as_u64())
            .sum::<u64>();
        let harness_version = crate::db::harness_store::harness_version(&conn)?;
        let active_rules: u64 = conn.query_row(
            "SELECT COUNT(*) FROM _amux_harness_rules WHERE status='active'",
            [],
            |row| row.get(0),
        )?;
        let sensor_profiles: u64 =
            conn.query_row("SELECT COUNT(*) FROM _amux_sensor_profiles", [], |row| {
                row.get(0)
            })?;
        let throughput =
            crate::db::throughput_store::health_snapshot(&conn, since, f64::from(days) * 24.0)?;
        Ok(json!({
            "measured": true,
            "n_considered": total_verifications + policy_total + handoffs + tool_calls,
            "window_days": days,
            "verified_completion_rate": ratio(passed, total_verifications),
            "rework_rate": ratio(reworked, verified_tasks),
            "checkpoint_resume_rate": ratio(resumed, handoffs),
            "escalations": {"count": escalations, "sample_size": total_verifications},
            "verification": {
                "passed": passed,
                "failed": total_verifications.saturating_sub(passed),
                "timeouts": timeouts,
                "sample_size": total_verifications,
                "average_duration_ms": avg_verification_ms
            },
            "efficiency_per_verified_task": {
                "sample_size": verified_tasks,
                "tokens": (verified_tasks > 0).then(|| tokens as f64 / verified_tasks as f64),
                "cost_usd": (verified_tasks > 0).then(|| cost_usd / verified_tasks as f64),
                "tool_calls": (verified_tasks > 0).then(|| tool_calls as f64 / verified_tasks as f64)
            },
            "throughput": throughput,
            "policy_outcomes": {"sample_size": policy_total, "items": policy},
            "versions": {
                "harness": harness_version,
                "active_guide_rules": active_rules,
                "sensor_profiles": sensor_profiles
            }
        }))
    })();
    match counts {
        Ok(value) => Json(value).into_response(),
        Err(error) => internal(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_names_missing_sensor_and_stale_rule() {
        let conn = crate::db::migrate::test_memdb();
        let now = Utc::now();
        crate::db::harness_store::put_guide_rule(
            &conn,
            &GuideRule {
                id: "rule-1".into(),
                scope: "global".into(),
                name: "browser-proof".into(),
                content: "Run the browser proof".into(),
                owner: "platform".into(),
                source_failure: "INC-1".into(),
                rationale: "UI shipped unverified".into(),
                status: GuideRuleStatus::Active,
                enforcement_layer: EnforcementLayer::Sensor,
                sensor_ref: Some("browser".into()),
                version: 0,
                added_at: now - Duration::days(100),
                last_validated_at: Some(now - Duration::days(100)),
                expires_at: None,
                superseded_by: None,
            },
        )
        .unwrap();
        let report = compile_report(&conn, now).unwrap();
        assert_eq!(report["ok"], false);
        assert!(report["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "missing_sensor"));
        assert!(report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "stale"));
    }
}
