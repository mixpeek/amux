//! Unified capability-policy gate for protected API mutations.
//!
//! The middleware classifies every request into the core action vocabulary,
//! evaluates the configured policy, persists a receipt, and only then invokes
//! the handler. `ask` decisions require a single-use approval token bound to
//! actor + action + exact resource.

use super::AppState;
use amux_core::policy::{
    ActionClass, ActionContext, CapabilityDecision, CapabilityEffect, CapabilityPolicy, TrustLevel,
};
use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

const APPROVAL_BODY_LIMIT: usize = 64 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", axum::routing::get(get_policy))
        .route("/evaluate", axum::routing::post(evaluate))
        .route("/approvals", axum::routing::post(create_approval))
        .route("/receipts", axum::routing::get(list_receipts))
}

fn policy_path() -> PathBuf {
    std::env::var("AMUX_CAPABILITY_POLICY")
        .map(PathBuf::from)
        .unwrap_or_else(|_| super::settings::amux_home().join("capability-policy.toml"))
}

pub fn load_policy() -> Result<CapabilityPolicy, String> {
    let path = policy_path();
    if !path.exists() {
        return Ok(CapabilityPolicy::default());
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("read capability policy {}: {e}", path.display()))?;
    let policy: CapabilityPolicy = toml::from_str(&raw)
        .map_err(|e| format!("parse capability policy {}: {e}", path.display()))?;
    policy
        .validate()
        .map_err(|e| format!("validate capability policy {}: {e}", path.display()))?;
    Ok(policy)
}

/// Authorize the orchestrator's worker dispatch inside its write transaction.
/// This closes the provider path as well as the HTTP path: even a worker that
/// never calls the AMUX API cannot receive a task without a policy receipt.
pub(crate) fn authorize_dispatch(
    conn: &rusqlite::Connection,
    worker: &amux_core::ids::WorkerId,
    task: &amux_core::ids::TaskId,
) -> rusqlite::Result<CapabilityDecision> {
    let policy = load_policy()
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e))))?;
    let ctx = ActionContext {
        actor: worker.to_string(),
        role: crate::db::harness_store::planning_role_for_actor(conn, worker.as_str())?,
        action: ActionClass::ExecuteTask,
        resource: task.to_string(),
        trust: TrustLevel::Trusted,
        reversible: true,
        cost_microusd: 0,
        capability_expansion: false,
    };
    let mut decision = policy.decide(&ctx);
    if let Some(row) = crate::db::queries::get_worker(conn, worker.as_str())? {
        if row
            .permissions
            .iter()
            .any(|p| p == "deny:*" || p == "deny:execute_task")
        {
            decision.effect = CapabilityEffect::Deny;
            decision.rule_id = Some("worker-permissions".into());
            decision.rationale = "worker permissions deny task execution".into();
        }
    }
    if let Some(limit) = decision.rate_limit {
        if let Some(rule) = decision.rule_id.as_deref() {
            let cutoff = (Utc::now() - Duration::seconds(limit.window_secs as i64)).to_rfc3339();
            let count: u32 = conn.query_row(
                "SELECT COUNT(*) FROM _amux_policy_receipts
                 WHERE rule_id=?1 AND effect='allow' AND created_at>=?2",
                rusqlite::params![rule, cutoff],
                |r| r.get(0),
            )?;
            if count >= limit.max_per_window {
                decision.effect = CapabilityEffect::RateLimited;
                decision.rationale = "configured capability rate limit reached".into();
            }
        }
    }
    conn.execute(
        "INSERT INTO _amux_policy_receipts
         (id,actor,role,action,resource,trust,reversible,effect,rule_id,rationale,cost_microusd,created_at)
         VALUES(?1,?2,?3,?4,?5,'trusted',1,?6,?7,?8,0,?9)",
        rusqlite::params![
            format!("pol_{}", ulid::Ulid::new().to_string().to_lowercase()),
            ctx.actor,
            ctx.role.and_then(|role| serde_json::to_value(role).ok())
                .and_then(|value| value.as_str().map(str::to_owned)),
            ctx.action.as_str(),
            ctx.resource,
            effect_token(decision.effect),
            decision.rule_id,
            decision.rationale,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(decision)
}

fn actor(headers: &HeaderMap) -> String {
    headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("api-anonymous")
        .to_string()
}

fn classify(method: &Method, path: &str) -> (ActionClass, bool) {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return (ActionClass::Read, true);
    }
    if *method == Method::DELETE {
        return (ActionClass::Delete, false);
    }
    let lower = path.to_ascii_lowercase();
    if reconciliation_requires_approval(method, &lower) {
        (ActionClass::Deploy, false)
    } else if lower.starts_with("/api/policy")
        || lower.starts_with("/api/criteria")
        || lower.contains("/permissions")
        || lower.starts_with("/api/harness/guides")
        || lower.starts_with("/api/harness/sensors")
        || lower.starts_with("/api/harness/budgets")
        || lower.starts_with("/api/harness/ratchet")
        || lower.starts_with("/api/harness/goals")
        || lower.starts_with("/api/harness/planning-nodes")
        || lower.starts_with("/api/harness/handoffs")
    {
        (ActionClass::CapabilityChange, true)
    } else if lower.contains("deploy") {
        (ActionClass::Deploy, false)
    } else if lower.contains("git") && lower.contains("push") {
        (ActionClass::GitPush, false)
    } else if [
        "/api/gmail",
        "/api/email",
        "/api/calendar",
        "/api/telegram",
        "/api/push",
        "/api/alert",
        "/api/tunnel",
        "/api/proxies",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
    {
        (ActionClass::ExternalWrite, false)
    } else if lower.starts_with("/api/fs")
        || lower.starts_with("/api/file")
        || lower.starts_with("/api/upload")
    {
        (ActionClass::FileWrite, true)
    } else if lower.starts_with("/api/connectors") {
        (ActionClass::ConnectorWrite, false)
    } else {
        (ActionClass::ToolUse, true)
    }
}

fn effect_token(effect: CapabilityEffect) -> &'static str {
    match effect {
        CapabilityEffect::Allow => "allow",
        CapabilityEffect::Ask => "ask",
        CapabilityEffect::Deny => "deny",
        CapabilityEffect::RateLimited => "rate_limited",
    }
}

fn reconciliation_requires_approval(method: &Method, path: &str) -> bool {
    if *method != Method::POST {
        return false;
    }
    if path == "/api/harness/reconciliations" {
        return true;
    }
    let Some(suffix) = path.strip_prefix("/api/harness/reconciliations/") else {
        return false;
    };
    let mut segments = suffix.split('/');
    matches!(
        (segments.next(), segments.next(), segments.next()),
        (Some(id), Some("promote"), None) if !id.is_empty()
    )
}

fn require_reconciliation_approval(method: &Method, path: &str, decision: &mut CapabilityDecision) {
    if reconciliation_requires_approval(method, path) && decision.effect == CapabilityEffect::Allow
    {
        decision.effect = CapabilityEffect::Ask;
        decision.rule_id = Some("builtin-reconciliation-exact-approval".into());
        decision.rationale =
            "running repository code or promoting a green ref requires single-use human approval bound to the exact request"
                .into();
    }
}

async fn record_receipt(
    state: &AppState,
    ctx: &ActionContext,
    decision: &CapabilityDecision,
) -> Result<String, String> {
    let id = format!("pol_{}", ulid::Ulid::new().to_string().to_lowercase());
    let row_id = id.clone();
    let actor = ctx.actor.clone();
    let role = ctx
        .role
        .and_then(|role| serde_json::to_value(role).ok())
        .and_then(|value| value.as_str().map(str::to_owned));
    let action = ctx.action.as_str().to_string();
    let resource = ctx.resource.clone();
    let trust = match ctx.trust {
        TrustLevel::Trusted => "trusted",
        TrustLevel::Untrusted => "untrusted",
    }
    .to_string();
    let reversible = ctx.reversible;
    let cost = ctx.cost_microusd;
    let effect = effect_token(decision.effect).to_string();
    let rule = decision.rule_id.clone();
    let rationale = decision.rationale.clone();
    state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO _amux_policy_receipts
                 (id,actor,role,action,resource,trust,reversible,effect,rule_id,rationale,cost_microusd,created_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                rusqlite::params![
                    row_id,
                    actor,
                    role,
                    action,
                    resource,
                    trust,
                    reversible,
                    effect,
                    rule,
                    rationale,
                    cost,
                    Utc::now().to_rfc3339(),
                ],
            )?;
            Ok(crate::db::WriteOutcome {
                applied: false,
                events: vec![],
            })
        })
        .await
        .map_err(|e| e.to_string())?;
    Ok(id)
}

/// The THIRD blocking acquire `enforce` used to make per write, and the only
/// conditional one: it is skipped unless the matched rule carries a rate limit.
/// That conditionality is why it is the easiest of the three to miss when
/// reading, and why it still belongs on the blocking pool rather than the HTTP
/// worker (AF-640 / AMUX-4744).
async fn rate_limited(state: &AppState, decision: &CapabilityDecision) -> Result<bool, String> {
    let Some(limit) = decision.rate_limit else {
        return Ok(false);
    };
    let Some(rule) = decision.rule_id.as_deref() else {
        return Ok(false);
    };
    let cutoff = (Utc::now() - Duration::seconds(limit.window_secs as i64)).to_rfc3339();
    let rule = rule.to_string();
    let count: u32 = state
        .store
        .read_async(move |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM _amux_policy_receipts
             WHERE rule_id=?1 AND effect='allow' AND created_at>=?2",
                rusqlite::params![rule, cutoff],
                |r| r.get(0),
            )?)
        })
        .await
        .map_err(|e| e.to_string())?;
    Ok(count >= limit.max_per_window)
}

fn approval_hash(raw: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(raw.as_bytes());
    hex::encode(hash.finalize())
}

fn exact_resource(uri: &axum::http::Uri, body: &[u8]) -> String {
    let route = uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or_else(|| uri.path());
    let mut hash = Sha256::new();
    hash.update(body);
    format!("{route}#sha256={}", hex::encode(hash.finalize()))
}

/// AF-640 / AMUX-4744: this runs on the HTTP runtime for EVERY write.
///
/// `enforce` short-circuits reads before it (`ActionClass::Read` returns
/// early), so this synchronous `store.read()` was a per-WRITE blocking acquire
/// on an HTTP worker while GETs never reached it. That is the exact asymmetry
/// the request log shows: reads 0.013% over 10s, writes 5.6%, on the same
/// server in the same window. A saturated read pool then pins HTTP workers, and
/// the pool's max_size is `available_parallelism`, which is also tokio's
/// default worker count, so it can pin every one of them.
///
/// Now awaits on the blocking pool instead. Same queries, same order, same
/// verdicts; only the thread it waits on changes.
async fn request_trust(state: &AppState, headers: &HeaderMap) -> Result<TrustLevel, String> {
    if headers
        .get("x-amux-input-trust")
        .and_then(|v| v.to_str().ok())
        == Some("untrusted")
    {
        return Ok(TrustLevel::Untrusted);
    }
    let actor_name = actor(headers);
    if actor_name == "api-anonymous" {
        return Ok(TrustLevel::Trusted);
    }
    state
        .store
        .read_async(move |conn| trust_from_conn(conn, &actor_name))
        .await
        .map_err(|e| e.to_string())
}

/// The verdict itself, unchanged, split out so it runs on the blocking pool.
fn trust_from_conn(conn: &rusqlite::Connection, actor_name: &str) -> anyhow::Result<TrustLevel> {
    let Some(worker) = crate::db::queries::get_worker(conn, actor_name)? else {
        return Ok(TrustLevel::Trusted);
    };
    let worker_id =
        amux_core::ids::WorkerId::parse(&worker.id).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let Some(command) = crate::db::commands::in_flight(conn, &worker_id)? else {
        return Ok(TrustLevel::Trusted);
    };
    if !matches!(
        command.command,
        amux_core::protocol::WorkerCommand::ExecuteTask(_)
    ) {
        return Ok(TrustLevel::Trusted);
    }
    let snapshot = crate::orchestrator::context::load_snapshot(conn, &command.idempotency_key)?;
    Ok(match snapshot {
        Some(snapshot)
            if snapshot
                .fragments
                .iter()
                .all(|fragment| fragment.trust == TrustLevel::Trusted) =>
        {
            TrustLevel::Trusted
        }
        // Missing provenance for an executing task is UNKNOWN, never trusted.
        Some(_) | None => TrustLevel::Untrusted,
    })
}

async fn consume_approval(
    state: &AppState,
    raw: &str,
    ctx: &ActionContext,
) -> Result<bool, String> {
    let hash = approval_hash(raw);
    let actor = ctx.actor.clone();
    let action = ctx.action.as_str().to_string();
    let resource = ctx.resource.clone();
    let now = Utc::now().to_rfc3339();
    let applied = std::sync::Arc::new(std::sync::Mutex::new(false));
    let applied_w = applied.clone();
    state
        .store
        .write_async(move |conn| {
            let changed = conn.execute(
                "UPDATE _amux_policy_approvals SET used_at=?1
                 WHERE token_hash=?2 AND actor=?3 AND action=?4 AND resource=?5
                   AND used_at IS NULL AND expires_at>?1",
                rusqlite::params![now, hash, actor, action, resource],
            )?;
            *applied_w.lock().expect("approval result") = changed == 1;
            Ok(crate::db::WriteOutcome {
                applied: false,
                events: vec![],
            })
        })
        .await
        .map_err(|e| e.to_string())?;
    let consumed = *applied.lock().expect("approval result");
    Ok(consumed)
}

pub async fn enforce(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    // Approval issuance is itself guarded by explicit human identity in the
    // handler; routing it through a policy that needs approval could make the
    // recovery path recursively unavailable.
    if req.uri().path() == "/api/policy/approvals" {
        return next.run(req).await;
    }
    if let Some(response) =
        super::projects::executor_mutation_guard(req.method(), req.uri().path(), req.headers())
    {
        return response;
    }
    let (action, reversible) = classify(req.method(), req.uri().path());
    if action == ActionClass::Read {
        return next.run(req).await;
    }
    let trust = match request_trust(&state, req.headers()).await {
        Ok(trust) => trust,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": error, "policy": "trust_provenance_required"})),
            )
                .into_response()
        }
    };
    let actor_name = actor(req.headers());
    // THE SECOND BLOCKING ACQUIRE ON THIS PATH, and the one that survives every
    // short-circuit above it. 150acf5d converted `request_trust` and left this
    // four lines below it, so writes still took a blocking read acquire on an
    // HTTP worker and the asymmetry that commit set out to remove was only half
    // removed.
    //
    // It is also why "anonymous callers short-circuit, so policy is not the
    // stall" was a wrong elimination. That short-circuit is inside
    // `request_trust` (actor == "api-anonymous" returns Trusted without a
    // read). This lookup is unconditional: it runs for api-anonymous too, with
    // that literal as the actor. An unauthenticated curl therefore skips the
    // read I had checked and still pays for this one.
    let role = {
        let actor_name = actor_name.clone();
        state
            .store
            .read_async(move |conn| {
                Ok(crate::db::harness_store::planning_role_for_actor(conn, &actor_name).ok())
            })
            .await
            .ok()
            .flatten()
            .flatten()
    };
    let mut ctx = ActionContext {
        actor: actor_name,
        role,
        action,
        resource: req
            .uri()
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or_else(|| req.uri().path())
            .to_string(),
        trust,
        reversible,
        cost_microusd: 0,
        capability_expansion: action == ActionClass::CapabilityChange,
    };
    let policy = match load_policy() {
        Ok(policy) => policy,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": error, "policy": "fail_closed"})),
            )
                .into_response()
        }
    };
    let request_path = req.uri().path().to_string();
    let mut decision = policy.decide(&ctx);
    require_reconciliation_approval(req.method(), &request_path, &mut decision);
    if decision.effect == CapabilityEffect::Ask {
        let body = std::mem::replace(req.body_mut(), Body::empty());
        let bytes = match axum::body::to_bytes(body, APPROVAL_BODY_LIMIT).await {
            Ok(bytes) => bytes,
            Err(error) => {
                return (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(json!({
                        "error": format!("approval-bound request body exceeds {APPROVAL_BODY_LIMIT} bytes or could not be read: {error}")
                    })),
                )
                    .into_response()
            }
        };
        ctx.resource = exact_resource(req.uri(), &bytes);
        *req.body_mut() = Body::from(bytes);
        decision = policy.decide(&ctx);
        require_reconciliation_approval(req.method(), &request_path, &mut decision);
    }
    match rate_limited(&state, &decision).await {
        Ok(true) => {
            decision.effect = CapabilityEffect::RateLimited;
            decision.rationale = "configured capability rate limit reached".into();
        }
        Ok(false) => {}
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    }

    if decision.effect == CapabilityEffect::Ask {
        let approved = match req
            .headers()
            .get("x-amux-approval")
            .and_then(|v| v.to_str().ok())
        {
            Some(token) => match consume_approval(&state, token, &ctx).await {
                Ok(approved) => approved,
                Err(error) => {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({"error": error, "policy": "approval_store_required"})),
                    )
                        .into_response()
                }
            },
            None => false,
        };
        if approved {
            decision.effect = CapabilityEffect::Allow;
            decision.rationale = format!("approved exact action: {}", decision.rationale);
        }
    }

    let receipt = match record_receipt(&state, &ctx, &decision).await {
        Ok(receipt) => receipt,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": error, "policy": "receipt_required"})),
            )
                .into_response()
        }
    };
    match decision.effect {
        CapabilityEffect::Allow => {
            let mut response = next.run(req).await;
            if let Ok(value) = receipt.parse() {
                response.headers_mut().insert("x-amux-policy-receipt", value);
            }
            response
        }
        CapabilityEffect::Ask => (
            StatusCode::PRECONDITION_REQUIRED,
            Json(json!({
                "error": "approval required",
                "receipt": receipt,
                "approval_resource": ctx.resource,
                "decision": decision
            })),
        )
            .into_response(),
        CapabilityEffect::Deny => (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "capability denied", "receipt": receipt, "decision": decision})),
        )
            .into_response(),
        CapabilityEffect::RateLimited => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": "capability rate limited", "receipt": receipt, "decision": decision})),
        )
            .into_response(),
    }
}

async fn get_policy() -> Response {
    match load_policy() {
        Ok(policy) => Json(json!({"policy": policy, "path": policy_path()})).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": error})),
        )
            .into_response(),
    }
}

async fn evaluate(Json(ctx): Json<ActionContext>) -> Response {
    match load_policy() {
        Ok(policy) => Json(json!({"decision": policy.decide(&ctx)})).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": error})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalRequest {
    actor: String,
    action: ActionClass,
    resource: String,
    #[serde(default = "approval_ttl")]
    ttl_secs: u64,
}

const fn approval_ttl() -> u64 {
    900
}

async fn create_approval(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ApprovalRequest>,
) -> Response {
    let approver = actor(&headers);
    let owner_token = super::auth::has_owner_token(
        &state,
        &headers,
        &axum::http::Uri::from_static("/api/policy/approvals"),
    );
    let org_member = super::org::is_verified_local_member(&headers);
    let local_identified_human = if state.auth_token.is_none() && approver != "api-anonymous" {
        let conn = match state.store.read() {
            Ok(conn) => conn,
            Err(error) => {
                return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()
            }
        };
        match crate::db::queries::get_worker(&conn, &approver) {
            Ok(worker) => worker.is_none(),
            Err(error) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
            }
        }
    } else {
        false
    };
    let human = owner_token || org_member || local_identified_human;
    if !human {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "a verified human member or owner token must issue approvals"})),
        )
            .into_response();
    }
    if body.actor.trim().is_empty() || body.resource.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "actor and exact resource are required"})),
        )
            .into_response();
    }
    if body.ttl_secs == 0 || body.ttl_secs > 3600 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "ttl_secs must be between 1 and 3600"})),
        )
            .into_response();
    }
    let raw = format!(
        "{}{}",
        ulid::Ulid::new().to_string().to_lowercase(),
        ulid::Ulid::new().to_string().to_lowercase()
    );
    let hash = approval_hash(&raw);
    let approved_by = if approver == "api-anonymous" && owner_token {
        super::auth::OWNER_TOKEN_ACTOR.to_string()
    } else {
        approver
    };
    let actor_name = body.actor;
    let action = body.action.as_str().to_string();
    let resource = body.resource;
    let now = Utc::now();
    let expires = now + Duration::seconds(body.ttl_secs as i64);
    let write = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO _amux_policy_approvals
                 (token_hash,actor,action,resource,approved_by,expires_at,created_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![
                    hash,
                    actor_name,
                    action,
                    resource,
                    approved_by,
                    expires.to_rfc3339(),
                    now.to_rfc3339(),
                ],
            )?;
            Ok(crate::db::WriteOutcome {
                applied: false,
                events: vec![],
            })
        })
        .await;
    match write {
        Ok(_) => Json(json!({"approval": raw, "expires_at": expires, "single_use": true}))
            .into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

#[derive(Default, Deserialize)]
struct ReceiptQuery {
    #[serde(default = "receipt_limit")]
    limit: u32,
}

const fn receipt_limit() -> u32 {
    100
}

async fn list_receipts(
    State(state): State<AppState>,
    Query(query): Query<ReceiptQuery>,
) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    let mut stmt = match conn.prepare(
        "SELECT id,actor,action,resource,trust,reversible,effect,rule_id,rationale,cost_microusd,created_at
         FROM _amux_policy_receipts ORDER BY created_at DESC LIMIT ?1",
    ) {
        Ok(stmt) => stmt,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let mapped = match stmt.query_map([query.limit.clamp(1, 1000)], |r| {
        Ok(json!({
            "id": r.get::<_, String>(0)?, "actor": r.get::<_, String>(1)?,
            "action": r.get::<_, String>(2)?, "resource": r.get::<_, String>(3)?,
            "trust": r.get::<_, String>(4)?, "reversible": r.get::<_, bool>(5)?,
            "effect": r.get::<_, String>(6)?, "rule_id": r.get::<_, Option<String>>(7)?,
            "rationale": r.get::<_, String>(8)?, "cost_microusd": r.get::<_, u64>(9)?,
            "created_at": r.get::<_, String>(10)?,
        }))
    }) {
        Ok(rows) => rows,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let rows = match mapped.collect::<Result<Vec<_>, _>>() {
        Ok(rows) => rows,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    Json(json!({"items": rows, "total": rows.len()})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mutating_route_has_an_action_class() {
        for path in [
            "/api/board/X",
            "/api/fs/write",
            "/api/gmail/send",
            "/api/policy/evaluate",
            "/api/criteria/AMUX-1",
            "/api/harness/budgets/AMUX-1",
            "/api/deploy/start",
        ] {
            assert_ne!(classify(&Method::POST, path).0, ActionClass::Read);
        }
        assert_eq!(classify(&Method::GET, "/api/board").0, ActionClass::Read);
    }

    #[test]
    fn policy_parse_failure_is_not_a_permissive_fallback() {
        let bad: Result<CapabilityPolicy, _> = toml::from_str("version = [");
        assert!(bad.is_err());
    }

    #[test]
    fn exact_approval_resource_binds_query_and_body() {
        let a = exact_resource(
            &"/api/email/send?account=a".parse().unwrap(),
            br#"{"to":"a"}"#,
        );
        let b = exact_resource(
            &"/api/email/send?account=a".parse().unwrap(),
            br#"{"to":"b"}"#,
        );
        let c = exact_resource(
            &"/api/email/send?account=b".parse().unwrap(),
            br#"{"to":"a"}"#,
        );
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn reconciliation_approval_applies_only_to_real_mutating_routes() {
        assert!(reconciliation_requires_approval(
            &Method::POST,
            "/api/harness/reconciliations"
        ));
        assert!(reconciliation_requires_approval(
            &Method::POST,
            "/api/harness/reconciliations/recon_1/promote"
        ));
        for (method, path) in [
            (Method::DELETE, "/api/harness/reconciliations"),
            (Method::POST, "/api/harness/reconciliations/recon_1"),
            (Method::PUT, "/api/harness/reconciliations/recon_1/promote"),
            (
                Method::POST,
                "/api/harness/reconciliations/recon_1/extra/promote",
            ),
        ] {
            assert!(
                !reconciliation_requires_approval(&method, path),
                "{method} {path}"
            );
        }
    }

    /// AMUX-4744: a WRITE must not pin an HTTP runtime worker inside the policy
    /// gate, the way a GET on the same route never could.
    ///
    /// `enforce` returns early for `ActionClass::Read`, so every blocking
    /// acquire it makes is paid by non-GET methods ONLY. That is the measured
    /// asymmetry on the card: 194,046 reads at 0.013% slow against 9,732 writes
    /// at 5.641%, same server, same 10s window.
    ///
    /// THE ROUTE IS CHOSEN, NOT INCIDENTAL. `/api/client-debug` is the card's
    /// own reproducer and its handler touches no database at all: a
    /// `tracing::info!` and a push onto an in-memory ring. So a stall measured
    /// here cannot be the handler, which is exactly why reading the handler
    /// found nothing and the middleware was never suspected.
    ///
    /// SINGLE-WORKER RUNTIME ON PURPOSE, so the cell cannot pass by finding a
    /// spare worker, and every pooled connection is held first so an acquire
    /// really must wait.
    #[test]
    fn a_write_through_the_policy_gate_does_not_pin_the_runtime_worker() {
        use tower::ServiceExt;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store =
            std::sync::Arc::new(crate::db::Store::open(&dir.path().join("pol.db")).unwrap());
        let app = crate::api::router(AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        });

        rt.block_on(async move {
            // Hold EVERY read connection, so any blocking acquire must wait.
            let held: Vec<_> = (0..store.read_pool.max_size())
                .map(|_| store.read_pool.get().expect("prefill"))
                .collect();

            let call = tokio::spawn(async move {
                app.oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/api/client-debug")
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"kind":"amux-4744-probe"}"#))
                        .unwrap(),
                )
                .await
            });

            // THE POINT. While that write sits in the policy gate, the single
            // runtime worker must still be free to poll something else. With a
            // synchronous `store.read()` in `enforce` this task cannot be
            // polled at all, because the blocked acquire owns the worker.
            //
            // THE BUDGET MUST SIT BELOW THE POOL'S OWN connection_timeout,
            // which AF-640 set to 5s. A blocking acquire against a saturated
            // pool releases the worker when that timeout expires, so a 5s
            // budget here passed under mutation: the pin was real and ended
            // one instant before the cell gave up. Measured, this run: 0.40s
            // with `read_async`, 5.46s with the blocking read restored. 1s
            // separates them by a factor of five in both directions.
            let started = std::time::Instant::now();
            let ticked = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                tokio::spawn(async { 42u8 }),
            )
            .await
            .expect("the runtime worker was pinned by a blocking read inside the policy gate")
            .expect("join");
            assert_eq!(ticked, 42);
            // Say the number, so a future timeout change cannot quietly turn
            // this back into a test of the pool's timeout rather than of the
            // gate. A pinned worker shows up here as ~5s, not as ~0s.
            let waited = started.elapsed();
            assert!(
                waited < std::time::Duration::from_secs(1),
                "an unrelated task waited {waited:?} to be polled while a write sat in the \
                 policy gate; that is a blocking acquire pinning the HTTP worker"
            );

            drop(held);
            let resp = call.await.expect("join").expect("response");
            // A write that is merely REFUSED fast would also leave the worker
            // free, so the cell would pass while proving nothing. Pin that the
            // request actually reached the handler.
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "the beacon must still be served once connections free"
            );
        });
    }
}
