//! Worker API: CRUD + start/stop/peek (RR-0034; parts of RR-0035 rename/alias
//! and RR-0040 group/env/permissions changes; Invariants 13, 17, 37, 43).
//!
//! Mounted at `/api/workers` inside the `protected` router (api/mod.rs), so
//! every handler sits behind bearer auth; the legacy `/api/sessions/*` paths
//! reach the same handlers through `aliases::alias_layer` (RR-0018a) and are
//! equally protected because the rewrite happens outside auth.
//!
//! Every mutation goes through `Store::write_async` and reports honestly:
//! `applied: false` with no rev/version bump on no-ops (Invariant 37), a
//! `ConfigChangeResult` naming the apply mode and any session swap on config
//! changes (Invariant 43), and `PendingEvent`s for every real change so
//! SSE/delta-sync consumers see it (Invariant 35).

use super::aliases::{alias_fields, FieldStyle};
use super::AppState;
use crate::db::queries::{self, SessionRow, WorkerRow};
use crate::db::{PendingEvent, WriteOutcome};
use amux_core::ids::{GroupId, SessionId, WorkerId};
use amux_core::provider::{ProviderCapabilities, ProviderId};
use amux_core::revision::{EntityType, MutationKind};
use amux_core::search::PagedResponse;
use amux_core::session::{backend_ref, BackendId, ExitReason};
use amux_core::worker::{
    apply_config, ConfigChangeResult, Worker, WorkerCapabilities, WorkerConfig, WorkerState,
};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_workers).post(create_worker))
        .route(
            "/{id}",
            get(get_worker).patch(patch_worker).delete(delete_worker),
        )
        .route("/{id}/start", post(start_worker))
        .route("/{id}/stop", post(stop_worker))
        .route("/{id}/peek", get(peek_worker))
        // THE CANONICAL SPELLING OF `send`, promoted out of the catch-all
        // (`/api/workers/{name}/{*verb}` in session_verbs.rs) that has been
        // answering it. Only `send` is promoted: naming the other 40-odd verbs
        // here would freeze the fleet substrate's spelling into this API before
        // anyone has decided which of them belong to it. The catch-all shrinks
        // as verbs earn a home, rather than being replaced wholesale.
        //
        // The body limit is disabled for the same reason the catch-all router
        // disables it: long prompts ride /send bodies. Scoped to this route, so
        // no other worker route's limit changes.
        .route(
            "/{id}/send",
            post(send_worker).layer(axum::extract::DefaultBodyLimit::disable()),
        )
}

/// `GET /api/ollama/models` — list locally installed Ollama models by running
/// `ollama list`. Returns `{"models": ["qwen3.8:27b", ...]}`. Empty array when
/// the Ollama daemon is not running or the binary is missing — never an error,
/// so the dashboard can use the result to populate a picker without catching.
pub async fn ollama_models() -> impl IntoResponse {
    use crate::provider::static_providers::OllamaAdapter;
    use crate::provider::ProviderAdapter;
    let models = OllamaAdapter::default().models().await;
    Json(json!({ "models": models }))
}

// ---- shared helpers -----------------------------------------------------

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

use super::internal;

fn not_found(key: &str) -> Response {
    err(
        StatusCode::NOT_FOUND,
        json!({ "error": "worker not found", "key": key }),
    )
}

/// Provider capabilities lookup, served from the REAL registry (AMUX-2613
/// gap 3 — this used to hardcode the all-false default, so a claude worker's
/// model change always classified as SessionRestart while the adapter's
/// measured matrix said `hot_model_switch: true`; the capability existed and
/// never reached the classifier, ethos rule 1). The registry is process-wide
/// and immutable, built once: `resolve` handles the worker-row legacy
/// spelling ("claude" -> "claude-code"). A provider the registry does not
/// know keeps the conservative all-false default — over-restarting is the
/// honest fallback for capabilities nobody measured.
/// `pub(crate)` because session_verbs' hot model switch (AMUX-2617) asks the
/// same question of the same registry: duplicating the accessor would give the
/// fleet path its own OnceLock and its own idea of the capability matrix, and
/// two components disagreeing about one fact is the shape of ethos rule 4.
pub(crate) fn provider_caps(provider: &str) -> ProviderCapabilities {
    static REGISTRY: std::sync::OnceLock<crate::provider::ProviderRegistry> =
        std::sync::OnceLock::new();
    REGISTRY
        .get_or_init(crate::provider::default_registry)
        .resolve(provider)
        .map(|a| a.capabilities())
        .unwrap_or_default()
}

/// The serde tag of a WorkerState ("stopped", "starting", ...) for
/// StatusChanged events and error bodies.
fn state_tag(state: &WorkerState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.get("state").and_then(|s| s.as_str()).map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

/// Map WorkerState to the dashboard status string the JS expects.
/// The dashboard reads `s.status` and renders status badges from it.
fn dashboard_status(state: &WorkerState) -> &'static str {
    match state {
        WorkerState::Stopped => "stopped",
        WorkerState::Starting => "starting",
        WorkerState::Active { .. } => "active",
        WorkerState::Idle { .. } => "idle",
        WorkerState::Waiting { .. } => "waiting",
        WorkerState::RateLimited { .. } => "rate_limited",
        WorkerState::Error { .. } => "error",
    }
}

/// The canonical API body for a worker. Field aliasing (RR-0018a) is applied
/// by callers via `alias_fields`.
///
/// Includes dashboard-facing fields (`status`, `running`, `rate_limited_until`,
/// `name`) so the JS `_renderSessionCard` renders status badges on every card
/// and shows terminal preview on expand. Fields that require backend adapters
/// (Phase 1) return null until then: `preview_lines`, `preview`, `tokens`,
/// `last_activity`, `task_name`.
fn worker_body(row: &WorkerRow) -> Value {
    let status = dashboard_status(&row.state);
    let running = !matches!(row.state, WorkerState::Stopped);
    let rate_limited_until = match &row.state {
        WorkerState::RateLimited { reset_at } => reset_at.map(|t| t.to_rfc3339()),
        _ => None,
    };
    json!({
        "id": row.id,
        "name": row.display_name,
        "display_name": row.display_name,
        "name_aliases": row.name_aliases,
        "cwd": row.cwd,
        "dir": row.cwd,
        "provider": row.provider,
        "model": row.model,
        "backend": row.backend,
        "environment": row.environment,
        "permissions": row.permissions,
        "group": row.group_id,
        "state": serde_json::to_value(&row.state).unwrap_or_else(|_| json!({"state": "stopped"})),
        "status": status,
        "running": running,
        "rate_limited_until": rate_limited_until,
        "version": row.version,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
        // Phase 1 (backend adapters): preview_lines, preview, tokens,
        // last_activity, task_name, credit_limited, session_created
        "preview_lines": Value::Null,
        "preview": Value::Null,
        "tokens": Value::Null,
        "last_activity": row.updated_at,
        "task_name": Value::Null,
    })
}

/// Event with no snapshot (session rows and other entities replay does not
/// yet reconstruct). Worker mutations must use [`ev_worker`] instead so the
/// journal stays replayable for them (RR-0111a).
fn ev(entity_type: EntityType, id: &str, mutation: MutationKind) -> PendingEvent {
    PendingEvent {
        entity_type,
        entity_id: id.to_string(),
        mutation,
        payload: None,
    }
}

/// Worker event carrying the post-mutation snapshot (RR-0111a). Every worker
/// event site has the row it just wrote in hand inside the write closure, so
/// the snapshot is one serialization — the journal can then replay worker
/// state without consulting the live table.
fn ev_worker(row: &WorkerRow, mutation: MutationKind) -> PendingEvent {
    PendingEvent {
        entity_type: EntityType::Worker,
        entity_id: row.id.clone(),
        mutation,
        payload: Some(row.snapshot()),
    }
}

fn no_write() -> WriteOutcome {
    WriteOutcome {
        applied: false,
        events: Vec::new(),
    }
}

/// Park a handler-level outcome in the slot and return the write outcome —
/// the only way data leaves a `Store::write` closure, since the closure's
/// return type is fixed by the writer loop.
fn finish<T>(
    slot: &Mutex<Option<T>>,
    outcome: T,
    write: WriteOutcome,
) -> rusqlite::Result<WriteOutcome> {
    *slot.lock().expect("outcome slot poisoned") = Some(outcome);
    Ok(write)
}

/// Map a non-SQL corruption (e.g. an unparseable id in a row we minted) into
/// the closure's error type so it surfaces as a 500 instead of a panic.
fn corrupt(e: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
}

/// Resolve `display_name` vs the legacy `name` spelling (RR-0018a request
/// rule): either is accepted; both-present-and-different is a 400 naming
/// both values — never a silently picked winner (Invariant 37).
fn resolve_name_fields(
    display_name: Option<String>,
    name: Option<String>,
) -> Result<Option<String>, Box<Response>> {
    match (display_name, name) {
        (Some(a), Some(b)) if a != b => Err(Box::new(err(
            StatusCode::BAD_REQUEST,
            json!({
                "error": "display_name and name (legacy alias) carry different values",
                "display_name": a,
                "name": b,
            }),
        ))),
        (a, b) => Ok(a.or(b)),
    }
}

// ---- GET /api/workers ---------------------------------------------------

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default)]
    pub offset: u64,
    #[serde(default = "default_limit")]
    pub limit: u64,
}

fn default_limit() -> u64 {
    200
}

/// List workers, PagedResponse-shaped (Invariant 40: `total`/`truncated`
/// announce what a page omits instead of silently capping).
pub async fn list_workers(
    State(state): State<AppState>,
    Query(p): Query<ListParams>,
) -> Response {
    let offset = p.offset;
    let limit = p.limit.clamp(1, 1000);
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = store.read()?;
        Ok(queries::list_workers(&conn, offset, limit)?)
    })
    .await;
    let (rows, total) = match joined {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };
    let items: Vec<Value> = rows.iter().map(worker_body).collect();
    let page = match PagedResponse::new(items, total, offset, limit) {
        Ok(p) => p,
        Err(e) => return internal(e),
    };
    match serde_json::to_value(&page) {
        Ok(v) => Json(alias_fields(v, FieldStyle::default())).into_response(),
        Err(e) => internal(e),
    }
}

// ---- POST /api/workers --------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)] // Invariant 37: unknown fields are rejected, not dropped
pub struct CreateWorkerBody {
    #[serde(default)]
    pub display_name: Option<String>,
    /// Legacy spelling (the Python dashboard says "name"); RR-0018a request
    /// aliasing rules apply.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub environment: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub permissions: Option<Vec<String>>,
    #[serde(default)]
    pub group: Option<String>,
}

/// Fleet-membership writes drop the legacy session-list cache (AMUX-2957).
///
/// The 2s cache on GET /api/sessions (7ca14b5) is invalidated on CONFIG writes
/// (AMUX-2926) — but a worker CREATE is also a list-shape change, and it was
/// not covered. Unobservable until tonight: the new worker-card-counts e2e
/// polls the dashboard in parallel, keeping the cache perpetually warm, so
/// control-plane's create-then-read (which had always passed) started landing
/// inside a hot window and reading a list from before its own create —
/// "legacy array carries the worker: Received: undefined". A cache that is
/// only cold when nobody is looking is how a passing test and a broken
/// product trade places. Wrapper, not per-return calls, for the same reason
/// as config_patch: a dozen exits, and the next one added would miss it.
pub async fn create_worker(
    state: State<AppState>,
    body: Json<CreateWorkerBody>,
) -> Response {
    let out = create_worker_inner(state, body).await;
    crate::api::sessions_legacy::invalidate_sessions_cache();
    out
}

async fn create_worker_inner(
    State(state): State<AppState>,
    Json(body): Json<CreateWorkerBody>,
) -> Response {
    let display_name = match resolve_name_fields(body.display_name, body.name) {
        Ok(Some(n)) if !n.trim().is_empty() => n,
        Ok(_) => {
            return err(
                StatusCode::BAD_REQUEST,
                json!({ "error": "display_name is required" }),
            )
        }
        Err(resp) => return *resp,
    };
    let group = match &body.group {
        Some(g) => match GroupId::parse(g) {
            Ok(id) => Some(id),
            Err(e) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": e.to_string(), "group": g }),
                )
            }
        },
        None => None,
    };
    let config = WorkerConfig {
        display_name,
        name_aliases: Vec::new(),
        cwd: body.cwd.unwrap_or_default(),
        provider: ProviderId::new(body.provider.unwrap_or_else(|| "claude".into())),
        model: body.model,
        backend: body.backend.map(BackendId::from).unwrap_or_default(),
        environment: body.environment.unwrap_or_default(),
        permissions: body.permissions.unwrap_or_default(),
        group,
    };

    // RR-0034 create contract: server-minted ULID id, version 0, Stopped.
    let id = WorkerId::from_ulid(ulid::Ulid::new());
    let now = chrono::Utc::now().to_rfc3339();
    let row = WorkerRow::new(&id, &config, &now);

    let row_for_write = row.clone();
    let write = state
        .store
        .write_async(move |conn| {
            queries::insert_worker(conn, &row_for_write)?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![ev_worker(&row_for_write, MutationKind::Created)],
            })
        })
        .await;
    match write {
        Ok(reply) => {
            let mut v = alias_fields(worker_body(&row), FieldStyle::default());
            v["rev"] = json!(reply.rev.0);
            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(e) => internal(e),
    }
}

// ---- GET /api/workers/{id} ----------------------------------------------

/// Detail by id, display_name, or alias (Invariant 17 — see
/// `queries::get_worker` for the resolution order).
pub async fn get_worker(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    let store = state.store.clone();
    let k = key.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = store.read()?;
        Ok(queries::get_worker(&conn, &k)?)
    })
    .await;
    match joined {
        Ok(Ok(Some(row))) => {
            Json(alias_fields(worker_body(&row), FieldStyle::default())).into_response()
        }
        Ok(Ok(None)) => not_found(&key),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- PATCH /api/workers/{id} --------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)] // Invariant 37
pub struct PatchWorkerBody {
    #[serde(default)]
    pub display_name: Option<String>,
    /// Legacy spelling of display_name (RR-0018a).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    /// NOTE: absent = unchanged. Clearing a model back to provider-default
    /// (`"model": null`) is indistinguishable from absent in this shape and
    /// therefore not supported yet — a double-Option lands with the full
    /// RR-0037 model-change work.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub environment: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub permissions: Option<Vec<String>>,
    #[serde(default)]
    pub group: Option<String>,
    /// Optimistic concurrency (Invariant 35): when present, the write only
    /// applies if the entity is still at this version; otherwise 409.
    #[serde(default)]
    pub expect_version: Option<u64>,
}

enum PatchOutcome {
    NotFound,
    Conflict { current_version: u64 },
    Noop { body: Value },
    Applied { body: Value, change: ConfigChangeResult },
}

pub async fn patch_worker(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(body): Json<PatchWorkerBody>,
) -> Response {
    let display_name = match resolve_name_fields(body.display_name.clone(), body.name.clone()) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    // Parse the group id OUTSIDE the write closure so a bad request is a
    // clean 400 before anything touches the writer thread.
    let group: Option<GroupId> = match &body.group {
        Some(g) => match GroupId::parse(g) {
            Ok(id) => Some(id),
            Err(e) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": e.to_string(), "group": g }),
                )
            }
        },
        None => None,
    };

    let slot: Arc<Mutex<Option<PatchOutcome>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let key_w = key.clone();

    let write = state
        .store
        .write_async(move |conn| {
            let Some(row) = queries::get_worker(conn, &key_w)? else {
                return finish(&slot_w, PatchOutcome::NotFound, no_write());
            };
            // Conflict outranks no-op: a stale caller should learn their view
            // of the world is old, even if the change they wanted is moot.
            if let Some(expect) = body.expect_version {
                if expect != row.version {
                    return finish(
                        &slot_w,
                        PatchOutcome::Conflict { current_version: row.version },
                        no_write(),
                    );
                }
            }

            let old_cfg = row.config();
            let mut new_cfg = old_cfg.clone();
            if let Some(v) = display_name {
                new_cfg.display_name = v;
            }
            if let Some(v) = body.cwd {
                new_cfg.cwd = v;
            }
            if let Some(v) = body.provider {
                new_cfg.provider = ProviderId::new(v);
            }
            if let Some(v) = body.model {
                new_cfg.model = Some(v);
            }
            if let Some(v) = body.backend {
                new_cfg.backend = BackendId::from(v);
            }
            if let Some(v) = body.environment {
                new_cfg.environment = v;
            }
            if let Some(v) = body.permissions {
                new_cfg.permissions = v;
            }
            if let Some(g) = group {
                new_cfg.group = Some(g);
            }
            // Invariant 17: a rename leaves the old name behind as an alias,
            // so `@old-name` written yesterday still resolves tomorrow.
            if new_cfg.display_name != old_cfg.display_name {
                new_cfg.name_aliases.retain(|a| a != &new_cfg.display_name);
                if !new_cfg.name_aliases.contains(&old_cfg.display_name) {
                    new_cfg.name_aliases.push(old_cfg.display_name.clone());
                }
            }

            if new_cfg == old_cfg {
                // Invariant 37: a no-op says so — no version bump, no rev
                // bump, no events.
                return finish(
                    &slot_w,
                    PatchOutcome::Noop { body: worker_body(&row) },
                    no_write(),
                );
            }

            let worker_id = WorkerId::parse(&row.id).map_err(corrupt)?;
            let live = queries::live_session_for(conn, &row.id)?;
            let current_session = live.as_ref().and_then(|s| SessionId::parse(&s.id).ok());

            // Classification + application via core (`apply_config` calls
            // `classify_config_change` and escalates to the strongest mode).
            let mut core_worker =
                Worker::new(worker_id.clone(), old_cfg, WorkerCapabilities::default());
            core_worker.state = row.state.clone();
            core_worker.version = row.version;
            let now = chrono::Utc::now();
            let (updated, change) = apply_config(
                core_worker,
                new_cfg.clone(),
                &provider_caps(new_cfg.provider.as_str()),
                now,
                current_session,
                || SessionId::from_ulid(ulid::Ulid::new()),
            );
            let now_s = now.to_rfc3339();

            let n = queries::update_worker_config(conn, &row.id, &new_cfg, row.version, &now_s)?;
            if n == 0 {
                // Unreachable under the single-writer (we read the version in
                // this same transaction), kept because the guarded UPDATE is
                // the real optimistic check when these queries compose
                // elsewhere.
                return finish(
                    &slot_w,
                    PatchOutcome::Conflict { current_version: row.version },
                    no_write(),
                );
            }

            // The post-mutation row, built BEFORE the events so each worker
            // event can journal it as its payload (RR-0111a): config from
            // new_cfg, version bumped exactly as update_worker_config wrote
            // it, state as apply_config decided.
            let mut new_row = row.clone();
            new_row.set_config(&new_cfg);
            new_row.version = row.version + 1;
            new_row.state = updated.state.clone();
            new_row.updated_at = now_s.clone();

            let mut events = Vec::new();
            if updated.state != row.state {
                queries::update_worker_state(conn, &row.id, &updated.state, &now_s)?;
                events.push(ev_worker(
                    &new_row,
                    MutationKind::StatusChanged {
                        from: state_tag(&row.state),
                        to: state_tag(&updated.state),
                    },
                ));
            } else {
                events.push(ev_worker(&new_row, MutationKind::Updated));
            }

            // Session replacement bookkeeping (Invariant 43). The actual
            // process swap arrives with the orchestrator (RR-0041); the
            // durable record — old session ended as Replaced, new session
            // row Starting — is written here so the swap is one auditable
            // event, never an unpaired death and birth.
            if change.session_replaced {
                if let (Some(old_ses), Some(new_ses)) = (&change.old_session, &change.new_session)
                {
                    queries::end_session(conn, old_ses.as_str(), &ExitReason::Replaced, &now_s)?;
                    events.push(ev(EntityType::Session, old_ses.as_str(), MutationKind::Updated));
                    queries::insert_session(
                        conn,
                        &SessionRow {
                            id: new_ses.as_str().to_string(),
                            worker_id: row.id.clone(),
                            backend: new_cfg.backend.as_str().to_string(),
                            backend_ref: backend_ref(&worker_id),
                            pid: None,
                            started_at: now_s.clone(),
                            ended_at: None,
                            exit_reason: None,
                        },
                    )?;
                    events.push(ev(EntityType::Session, new_ses.as_str(), MutationKind::Created));
                }
            }

            finish(
                &slot_w,
                PatchOutcome::Applied { body: worker_body(&new_row), change },
                WriteOutcome { applied: true, events },
            )
        })
        .await;

    let reply = match write {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let outcome = slot.lock().expect("outcome slot poisoned").take();
    match outcome {
        None => internal("patch produced no outcome"),
        Some(PatchOutcome::NotFound) => not_found(&key),
        Some(PatchOutcome::Conflict { current_version }) => err(
            StatusCode::CONFLICT,
            json!({ "error": "version conflict", "current_version": current_version }),
        ),
        Some(PatchOutcome::Noop { body }) => {
            let mut v = alias_fields(body, FieldStyle::default());
            v["applied"] = json!(false);
            v["change"] = Value::Null;
            Json(v).into_response()
        }
        Some(PatchOutcome::Applied { body, change }) => {
            let mut v = alias_fields(body, FieldStyle::default());
            v["applied"] = json!(true);
            v["change"] = serde_json::to_value(&change).unwrap_or(Value::Null);
            v["rev"] = json!(reply.rev.0);
            Json(v).into_response()
        }
    }
}

// ---- start / stop / delete ----------------------------------------------

enum StepOutcome {
    NotFound,
    /// The requested transition is illegal from the current state -> 409.
    Refused { error: &'static str, state: String },
    Applied { body: Value },
    /// Already in the requested state -> honest no-op (Invariant 37).
    Noop { body: Value },
}

/// POST /api/workers/{id}/start — 202 Accepted. Writes the durable record of
/// the start (worker -> Starting, a live session row, events); the actual
/// process spawn is the orchestrator's job (RR-0041) and lands there — this
/// endpoint accepts the request, it does not pretend the process exists.
pub async fn start_worker(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    let slot: Arc<Mutex<Option<StepOutcome>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let key_w = key.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let Some(row) = queries::get_worker(conn, &key_w)? else {
                return finish(&slot_w, StepOutcome::NotFound, no_write());
            };
            if !matches!(row.state, WorkerState::Stopped) {
                return finish(
                    &slot_w,
                    StepOutcome::Refused {
                        error: "worker is not stopped",
                        state: state_tag(&row.state),
                    },
                    no_write(),
                );
            }
            let worker_id = WorkerId::parse(&row.id).map_err(corrupt)?;
            let ses_id = SessionId::from_ulid(ulid::Ulid::new());
            let now_s = chrono::Utc::now().to_rfc3339();
            queries::insert_session(
                conn,
                &SessionRow {
                    id: ses_id.as_str().to_string(),
                    worker_id: row.id.clone(),
                    backend: row.backend.clone(),
                    backend_ref: backend_ref(&worker_id),
                    pid: None,
                    started_at: now_s.clone(),
                    ended_at: None,
                    exit_reason: None,
                },
            )?;
            queries::update_worker_state(conn, &row.id, &WorkerState::Starting, &now_s)?;
            // Post-mutation snapshot for the journal (RR-0111a): the row as
            // update_worker_state just left it.
            let mut after = row.clone();
            after.state = WorkerState::Starting;
            after.updated_at = now_s.clone();
            let events = vec![
                ev_worker(
                    &after,
                    MutationKind::StatusChanged { from: "stopped".into(), to: "starting".into() },
                ),
                ev(EntityType::Session, ses_id.as_str(), MutationKind::Created),
            ];
            finish(
                &slot_w,
                StepOutcome::Applied {
                    body: json!({
                        "session": "starting",
                        "session_id": ses_id.as_str(),
                        "worker_id": row.id,
                        "state": "starting",
                    }),
                },
                WriteOutcome { applied: true, events },
            )
        })
        .await;
    step_response(write, slot, &key, StatusCode::ACCEPTED)
}

/// POST /api/workers/{id}/stop — worker -> Stopped; the live session row (if
/// any) is ended as Killed. Stopping a stopped worker is an honest no-op.
pub async fn stop_worker(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    let slot: Arc<Mutex<Option<StepOutcome>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let key_w = key.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let Some(row) = queries::get_worker(conn, &key_w)? else {
                return finish(&slot_w, StepOutcome::NotFound, no_write());
            };
            let live = queries::live_session_for(conn, &row.id)?;
            if matches!(row.state, WorkerState::Stopped) && live.is_none() {
                return finish(
                    &slot_w,
                    StepOutcome::Noop {
                        body: json!({ "applied": false, "state": "stopped", "worker_id": row.id }),
                    },
                    no_write(),
                );
            }
            let now_s = chrono::Utc::now().to_rfc3339();
            let mut events = Vec::new();
            if let Some(ses) = live {
                queries::end_session(conn, &ses.id, &ExitReason::Killed, &now_s)?;
                events.push(ev(EntityType::Session, &ses.id, MutationKind::Updated));
            }
            if !matches!(row.state, WorkerState::Stopped) {
                queries::update_worker_state(conn, &row.id, &WorkerState::Stopped, &now_s)?;
                let mut after = row.clone();
                after.state = WorkerState::Stopped;
                after.updated_at = now_s.clone();
                events.push(ev_worker(
                    &after,
                    MutationKind::StatusChanged {
                        from: state_tag(&row.state),
                        to: "stopped".into(),
                    },
                ));
            }
            finish(
                &slot_w,
                StepOutcome::Applied {
                    body: json!({ "applied": true, "state": "stopped", "worker_id": row.id }),
                },
                WriteOutcome { applied: true, events },
            )
        })
        .await;
    step_response(write, slot, &key, StatusCode::OK)
}

/// DELETE /api/workers/{id} — soft delete, only from Stopped (a running
/// worker must be stopped first; 409 otherwise). The row survives for the
/// audit/session history hanging off it; it just stops resolving.
pub async fn delete_worker(state: State<AppState>, key: Path<String>) -> Response {
    let out = delete_worker_inner(state, key).await;
    crate::api::sessions_legacy::invalidate_sessions_cache();
    out
}

async fn delete_worker_inner(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    let slot: Arc<Mutex<Option<StepOutcome>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let key_w = key.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let Some(row) = queries::get_worker(conn, &key_w)? else {
                return finish(&slot_w, StepOutcome::NotFound, no_write());
            };
            if !matches!(row.state, WorkerState::Stopped) {
                return finish(
                    &slot_w,
                    StepOutcome::Refused {
                        error: "worker is not stopped; stop it before deleting",
                        state: state_tag(&row.state),
                    },
                    no_write(),
                );
            }
            let now_s = chrono::Utc::now().to_rfc3339();
            let n = queries::soft_delete_worker(conn, &row.id, &now_s)?;
            if n == 0 {
                // Raced with another delete inside the same writer queue:
                // already gone, report absence rather than a fresh change.
                return finish(&slot_w, StepOutcome::NotFound, no_write());
            }
            // Deletion is SOFT: the row survives with `deleted_at` set, and
            // the Deleted event journals a snapshot of that surviving row —
            // replay then knows both that it was deleted and what it was
            // (RR-0111a).
            let mut after = row.clone();
            after.deleted_at = Some(now_s.clone());
            after.updated_at = now_s;
            finish(
                &slot_w,
                StepOutcome::Applied { body: json!({ "deleted": true, "id": row.id }) },
                WriteOutcome {
                    applied: true,
                    events: vec![ev_worker(&after, MutationKind::Deleted)],
                },
            )
        })
        .await;
    step_response(write, slot, &key, StatusCode::OK)
}

fn step_response(
    write: anyhow::Result<crate::db::WriteReply>,
    slot: Arc<Mutex<Option<StepOutcome>>>,
    key: &str,
    applied_status: StatusCode,
) -> Response {
    let reply = match write {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let outcome = slot.lock().expect("outcome slot poisoned").take();
    match outcome {
        None => internal("mutation produced no outcome"),
        Some(StepOutcome::NotFound) => not_found(key),
        Some(StepOutcome::Refused { error, state }) => err(
            StatusCode::CONFLICT,
            json!({ "error": error, "state": state }),
        ),
        Some(StepOutcome::Noop { body }) => (StatusCode::OK, Json(body)).into_response(),
        Some(StepOutcome::Applied { mut body }) => {
            body["rev"] = json!(reply.rev.0);
            (applied_status, Json(body)).into_response()
        }
    }
}

// ---- GET /api/workers/{id}/peek -----------------------------------------

#[derive(Deserialize)]
pub struct PeekParams {
    /// Terminal lines to capture (herdr `pane read --lines`, tmux
    /// `capture-pane` history depth). Clamped to 1..=2000.
    #[serde(default)]
    pub lines: Option<u32>,
}

/// GET /api/workers/{id}/peek?lines=N — recent terminal output of the
/// worker's live session, read through the session's own backend (herdr
/// pane read / tmux capture-pane). Was a 501 (AMUX-2613 gap 4: the API
/// layer had no backend handle); now answers from
/// `backend::process_backend`, with every non-answer NAMED rather than
/// shaped like empty output — the Python peek's viewport bug taught us
/// that "no output" and "could not look" must be distinguishable.
///
/// This is a DIAGNOSTIC view, not the control plane (D1): worker state
/// comes from the structured protocol; peek is for the human who wants to
/// see the terminal.
pub async fn peek_worker(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(p): Query<PeekParams>,
) -> Response {
    let store = state.store.clone();
    let k = key.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = store.read()?;
        let Some(row) = queries::get_worker(&conn, &k)? else {
            return Ok(None);
        };
        let live = queries::live_session_for(&conn, &row.id)?;
        Ok(Some((row, live)))
    })
    .await;
    let (row, live) = match joined {
        Ok(Ok(Some(v))) => v,
        Ok(Ok(None)) => return not_found(&key),
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };
    let Some(ses) = live else {
        // No live session: there is no terminal to read. 409, not an empty
        // 200 — absence of a session and absence of output are different
        // facts.
        return err(
            StatusCode::CONFLICT,
            json!({
                "error": "worker has no live session to peek",
                "worker_id": row.id,
                "state": state_tag(&row.state),
            }),
        );
    };
    let Some(backend) = crate::backend::process_backend(&ses.backend) else {
        // Published-but-absent vs never-published both mean "this process
        // cannot look", but the remedy differs — name which one it is.
        let detail = if crate::backend::process_backends_published() {
            format!(
                "backend '{}' is not available on this server \
                 (herdr requires AMUX_HERDR_SESSION)",
                ses.backend
            )
        } else {
            "terminal backends are not initialized in this process".to_string()
        };
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": detail, "worker_id": row.id, "backend": ses.backend }),
        );
    };
    let lines = p.lines.unwrap_or(200).clamp(1, 2000);
    let proc = crate::backend::ProcessRef {
        backend_ref: ses.backend_ref.clone(),
        pid: ses.pid.map(|p| p as u32),
    };
    match backend.capture(&proc, lines).await {
        Ok(output) => Json(json!({
            "worker_id": row.id,
            "session_id": ses.id,
            "backend": ses.backend,
            "backend_ref": ses.backend_ref,
            "lines_requested": lines,
            "output": output,
            "captured_at": chrono::Utc::now().to_rfc3339(),
        }))
        .into_response(),
        Err(crate::backend::BackendError::NotFound(what)) => err(
            // The store says live, the backend says gone — surface the
            // DISAGREEMENT (ethos rule 4), never an empty capture. With
            // herdr this is also the shape of a finished process (its
            // GAP-EXIT-CODE: exited panes are reaped, unobservably).
            StatusCode::CONFLICT,
            json!({
                "error": "session is recorded live but its backend process was not found",
                "worker_id": row.id,
                "session_id": ses.id,
                "backend": ses.backend,
                "backend_ref": ses.backend_ref,
                "backend_says": format!("not found: {what}"),
            }),
        ),
        Err(e) => err(
            StatusCode::BAD_GATEWAY,
            json!({
                "error": format!("terminal capture failed: {e}"),
                "worker_id": row.id,
                "backend": ses.backend,
                "backend_ref": ses.backend_ref,
            }),
        ),
    }
}

/// `POST /api/workers/{id}/send` — deliver a prompt to a worker.
///
/// WHAT THIS ADDS over the catch-all it takes precedence over: the target is
/// resolved through the store ONCE, so an id or a name alias reaches the same
/// worker a display name does, and a rename between resolution and delivery
/// cannot land the text in a different lane than the one that was addressed.
///
/// WHAT THIS DELIBERATELY DOES NOT DO: implement `send`. It hands off to
/// `session_verbs::send_verb`, which is the one implementation — origin
/// stamping (AMUX-1768), cross-group scoping, msg_id idempotency, board
/// capture and the submitted/queued verdict all stay there. A second
/// implementation of send is how two callers end up disagreeing about whether
/// a message was delivered.
///
/// A key the store does not know is NOT a 404 here. The catch-all this route
/// displaces serves every session name, store-backed or not, and `amux send`
/// posts to this path for all of them; 404-ing the ones without a worker row
/// would take delivery away from sessions that have it today. Unknown keys are
/// passed through as session names, and `send_verb`'s existence gate answers
/// for them exactly as the dispatcher would have.
pub async fn send_worker(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: axum::http::HeaderMap,
    body_bytes: axum::body::Bytes,
) -> Response {
    let store = state.store.clone();
    let k = key.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = store.read()?;
        Ok(queries::get_worker(&conn, &k)?)
    })
    .await;
    let resolved = match joined {
        Ok(Ok(Some(row))) if !row.display_name.is_empty() => row.display_name,
        // A row with no display name has no env file to address either, so
        // there is nothing better to try than the key the caller used; the
        // existence gate then reports the miss under the name they asked for.
        Ok(Ok(_)) => key,
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };
    let body = match crate::api::fs::parse_body(&body_bytes) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e })),
    };
    crate::api::session_verbs::send_verb(&state, &resolved, &headers, &body).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{router, AppState};
    use crate::db::Store;
    use axum::body::Body;
    use axum::http::{header, HeaderMap, Request, StatusCode};
    use tower::ServiceExt;

    fn app_with_token(token: Option<String>) -> (axum::Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("amux-test.db")).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: token,
        };
        (router(state), dir)
    }

    fn app() -> (axum::Router, tempfile::TempDir) {
        app_with_token(None)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
        send_with(app, method, path, body, &[]).await
    }

    async fn send_with(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, Value) {
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let req = match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, headers, v)
    }

    async fn create(app: &axum::Router, name: &str) -> String {
        let (st, _, body) = send(
            app,
            "POST",
            "/api/workers",
            Some(json!({ "display_name": name, "cwd": "/tmp/w" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "create failed: {body}");
        body["id"].as_str().unwrap().to_string()
    }

    async fn health_rev(app: &axum::Router) -> u64 {
        let (st, _, body) = send(app, "GET", "/health", None).await;
        assert_eq!(st, StatusCode::OK);
        body["rev"].as_u64().unwrap()
    }

    // ---- RR-0034 test list ----------------------------------------------

    /// A worker ID addressed at the modern send route reaches that worker.
    ///
    /// THE DEFECT THIS FAILS ON: before `/{id}/send` was a route, this path
    /// fell to the catch-all `/api/workers/{name}/{*verb}`, which addresses the
    /// fleet substrate BY NAME. The ulid was handed to `env_path(<id>)`
    /// verbatim, matched nothing, and answered `session '<id>' not found` — so
    /// the assertion below reads back the id instead of `hw` without the fix.
    /// Every other worker route accepts an id; send was the one that did not,
    /// which leaves a caller holding the only handle a rename does not move
    /// unable to deliver with it.
    ///
    /// Asserted at the existence gate, not on a 200: a 200 needs a live
    /// terminal, would launch one on the machine running the suite, and — the
    /// reason that matters — would still pass against a route that resolved
    /// nothing, because the name only becomes observable in the answer.
    #[tokio::test]
    async fn send_route_resolves_a_worker_id_to_its_session_name() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        let (app, _dir) = app();
        let id = create(&app, "hw").await;

        let (st, _, v) = send(
            &app,
            "POST",
            &format!("/api/workers/{id}/send"),
            Some(json!({ "text": "hi" })),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
        assert_eq!(
            v["error"],
            json!("session 'hw' not found"),
            "the id must resolve to the worker's name, not be used as one: {v}"
        );
    }

    /// A promoted route that `/api/debug/routes` does not report is a route
    /// nobody can find.
    ///
    /// The drift guard in request_log.rs (`every_absolute_route_literal_is_in_
    /// route_table`) cannot see this one: it scans for `.route("/api/…")`
    /// literals, and everything this module mounts is written relative to the
    /// `/api/workers` nest — peek, start and stop are unguarded by it for the
    /// same reason. So the promoted route is pinned against the served table
    /// directly. Adjacent and pre-existing; not this change's to close.
    #[tokio::test]
    async fn the_promoted_send_route_is_reported_by_debug_routes() {
        let (app, _dir) = app();
        let (st, _, v) = send(&app, "GET", "/api/debug/routes", None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let row = v["routes"]
            .as_array()
            .expect("routes array")
            .iter()
            .find(|r| r["path"] == "/api/workers/{id}/send")
            .unwrap_or_else(|| panic!("promoted route absent from the served table: {v}"));
        assert_eq!(row["methods"], json!(["POST"]));
    }

    /// Promoting the route did not put a ceiling on prompts that had none.
    ///
    /// The catch-all's router disables the body limit outright (session_verbs.
    /// rs — "long prompts ride /send bodies"), so `POST /api/workers/<n>/send`
    /// has been uncapped. Nested under `/api/workers` it would instead inherit
    /// the protected router's 16MB cap (mod.rs), which is a narrowing, not a
    /// default — hence the disable on the route. Sized ABOVE 16MB deliberately:
    /// at 3MB this passes either way and pins nothing.
    #[tokio::test]
    async fn a_prompt_larger_than_the_default_body_limit_still_reaches_the_gate() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        let (app, _dir) = app();

        let big = "x".repeat(17 * 1024 * 1024);
        let (st, _, v) = send(
            &app,
            "POST",
            "/api/workers/ghost/send",
            Some(json!({ "text": big })),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::NOT_FOUND,
            "a 17MB prompt must reach the existence gate, not be refused by size: {v}"
        );
    }

    /// The promoted route does not narrow what the catch-all it displaces
    /// served.
    ///
    /// A real route takes precedence over the wildcard, so every `amux send`
    /// now lands here — including sends to plain env-file sessions that have no
    /// worker row at all. Answering `worker not found` for those would take
    /// delivery away from sessions that have it today, and it would be the
    /// wrong fact besides: the store's silence says nothing about whether the
    /// session exists.
    #[tokio::test]
    async fn send_route_passes_a_key_with_no_worker_row_through_as_a_session() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        let (app, _dir) = app();

        let (st, _, v) = send(
            &app,
            "POST",
            "/api/workers/ghost/send",
            Some(json!({ "text": "hi" })),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
        assert_eq!(
            v["error"],
            json!("session 'ghost' not found"),
            "an unknown key is a session miss, not a worker miss: {v}"
        );
    }

    #[tokio::test]
    async fn create_get_rename_then_alias_resolves() {
        let (app, _dir) = app();
        let (st, _, created) = send(
            &app,
            "POST",
            "/api/workers",
            Some(json!({ "display_name": "backend", "cwd": "/tmp/x" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let id = created["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("wrk_"));
        assert_eq!(created["version"], json!(0));
        assert_eq!(created["state"]["state"], json!("stopped"));

        let (st, _, by_id) = send(&app, "GET", &format!("/api/workers/{id}"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(by_id["id"].as_str().unwrap(), id);
        let (st, _, by_name) = send(&app, "GET", "/api/workers/backend", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(by_name["id"].as_str().unwrap(), id);

        // Rename (RR-0035): Immediate, version bumps, old name becomes alias.
        let (st, _, patched) = send(
            &app,
            "PATCH",
            &format!("/api/workers/{id}"),
            Some(json!({ "display_name": "rust-backend", "expect_version": 0 })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{patched}");
        assert_eq!(patched["applied"], json!(true));
        assert_eq!(patched["version"], json!(1));
        assert_eq!(patched["change"]["mode"], json!("immediate"));
        assert_eq!(patched["change"]["session_replaced"], json!(false));
        assert!(patched["name_aliases"]
            .as_array()
            .unwrap()
            .contains(&json!("backend")));

        // Invariant 17: the OLD name still resolves, to the SAME id.
        let (st, _, by_alias) = send(&app, "GET", "/api/workers/backend", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(by_alias["id"].as_str().unwrap(), id);
        assert_eq!(by_alias["display_name"], json!("rust-backend"));
    }

    #[tokio::test]
    async fn stale_expect_version_is_409_and_applies_nothing() {
        let (app, _dir) = app();
        let id = create(&app, "w").await;
        // Move to version 1.
        let (st, _, _) = send(
            &app,
            "PATCH",
            &format!("/api/workers/{id}"),
            Some(json!({ "display_name": "w2", "expect_version": 0 })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        // Stale write against version 0.
        let (st, _, body) = send(
            &app,
            "PATCH",
            &format!("/api/workers/{id}"),
            Some(json!({ "cwd": "/stale/view", "expect_version": 0 })),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(body["error"], json!("version conflict"));
        assert_eq!(body["current_version"], json!(1));

        // Nothing changed.
        let (_, _, back) = send(&app, "GET", &format!("/api/workers/{id}"), None).await;
        assert_eq!(back["cwd"], json!("/tmp/w"));
        assert_eq!(back["version"], json!(1));
    }

    #[tokio::test]
    async fn noop_patch_reports_unapplied_and_bumps_nothing() {
        let (app, _dir) = app();
        let id = create(&app, "w").await;
        let rev_before = health_rev(&app).await;

        // Same values -> no-op (Invariant 37).
        let (st, _, body) = send(
            &app,
            "PATCH",
            &format!("/api/workers/{id}"),
            Some(json!({ "display_name": "w", "cwd": "/tmp/w" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["applied"], json!(false));
        assert_eq!(body["change"], Value::Null);
        assert_eq!(body["version"], json!(0)); // NOT bumped
        assert!(body.get("rev").is_none()); // a no-op carries no new rev

        // Read back: entity version AND global rev both unmoved.
        let (_, _, back) = send(&app, "GET", &format!("/api/workers/{id}"), None).await;
        assert_eq!(back["version"], json!(0));
        assert_eq!(health_rev(&app).await, rev_before);
    }

    #[tokio::test]
    async fn start_stop_delete_lifecycle() {
        let (app, _dir) = app();
        let id = create(&app, "w").await;

        // Start: 202, durable Starting record.
        let (st, _, body) = send(&app, "POST", &format!("/api/workers/{id}/start"), None).await;
        assert_eq!(st, StatusCode::ACCEPTED);
        assert_eq!(body["session"], json!("starting"));
        assert!(body["session_id"].as_str().unwrap().starts_with("ses_"));
        let (_, _, back) = send(&app, "GET", &format!("/api/workers/{id}"), None).await;
        assert_eq!(back["state"]["state"], json!("starting"));

        // Delete while running: 409.
        let (st, _, body) = send(&app, "DELETE", &format!("/api/workers/{id}"), None).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(body["state"], json!("starting"));
        // Double-start: 409 too.
        let (st, _, _) = send(&app, "POST", &format!("/api/workers/{id}/start"), None).await;
        assert_eq!(st, StatusCode::CONFLICT);

        // Stop: applied, back to stopped, live session ended.
        let (st, _, body) = send(&app, "POST", &format!("/api/workers/{id}/stop"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["applied"], json!(true));
        let (_, _, back) = send(&app, "GET", &format!("/api/workers/{id}"), None).await;
        assert_eq!(back["state"]["state"], json!("stopped"));

        // Stopping again: honest no-op (Invariant 37).
        let (st, _, body) = send(&app, "POST", &format!("/api/workers/{id}/stop"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["applied"], json!(false));

        // Delete now succeeds (soft), and the worker stops resolving.
        let (st, _, body) = send(&app, "DELETE", &format!("/api/workers/{id}"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["deleted"], json!(true));
        let (st, _, _) = send(&app, "GET", &format!("/api/workers/{id}"), None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (_, _, list) = send(&app, "GET", "/api/workers", None).await;
        assert_eq!(list["total"], json!(0));
    }

    #[tokio::test]
    async fn cwd_change_with_live_session_replaces_it_atomically() {
        // Invariant 43: cwd is process-level -> SessionRestart; with a live
        // session the ONE result carries both ids and the DB shows the old
        // session ended as Replaced and the new one live.
        let (app, dir) = app();
        let id = create(&app, "w").await;
        let (st, _, started) = send(&app, "POST", &format!("/api/workers/{id}/start"), None).await;
        assert_eq!(st, StatusCode::ACCEPTED);
        let first_ses = started["session_id"].as_str().unwrap().to_string();

        let (st, _, body) = send(
            &app,
            "PATCH",
            &format!("/api/workers/{id}"),
            Some(json!({ "cwd": "/somewhere/else", "expect_version": 0 })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["applied"], json!(true));
        assert_eq!(body["change"]["mode"], json!("session_restart"));
        assert_eq!(body["change"]["session_replaced"], json!(true));
        let old_ses = body["change"]["old_session"].as_str().unwrap().to_string();
        let new_ses = body["change"]["new_session"].as_str().unwrap().to_string();
        assert_eq!(old_ses, first_ses);
        assert_ne!(old_ses, new_ses);

        // Verify the durable record directly against the store's DB file.
        let conn = rusqlite::Connection::open(dir.path().join("amux-test.db")).unwrap();
        let live = queries::live_session_for(&conn, &id).unwrap().unwrap();
        assert_eq!(live.id, new_ses);
        assert_eq!(
            queries::live_session_for(&conn, &id).unwrap().unwrap().ended_at,
            None
        );
        let reason: String = conn
            .query_row(
                "SELECT exit_reason FROM _amux_sessions WHERE id = ?1",
                rusqlite::params![old_ses],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<ExitReason>(&reason).unwrap(),
            ExitReason::Replaced
        );
    }

    // ---- RR-0018a wiring -------------------------------------------------

    #[tokio::test]
    async fn legacy_sessions_route_serves_workers_with_deprecated_header() {
        // Keep this test's verdict machine-independent: without suppression
        // the legacy route merges the REAL fleet (env + tmux read at call
        // time) and the assertion below depends on how many live sessions
        // this box runs. See SUPPRESS_FLEET_FOR_TEST for the named deviation.
        crate::api::sessions_legacy::SUPPRESS_FLEET_FOR_TEST
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let (app, _dir) = app();
        let id = create(&app, "w").await;

        let (st, headers, canonical) = send(&app, "GET", "/api/workers", None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(headers.get("deprecated").is_none());
        assert_eq!(canonical["total"], json!(1));
        assert_eq!(canonical["truncated"], json!(false)); // PagedResponse shape

        // Bare /api/sessions now serves the PYTHON SHAPE (bare array from
        // the dedicated handler, no Deprecated header) — the SPA's
        // fetchSessions throws on anything else (browser-golden finding #3).
        let (st, headers, legacy) = send(&app, "GET", "/api/sessions", None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(headers.get("deprecated").is_none());
        let arr = legacy.as_array().expect("bare array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], json!("w"));
        assert!(arr[0]["status"].is_string());

        // Per-session verbs now PROXY to the Python fleet owner; a
        // rust-managed worker on the legacy path gets the modern pointer,
        // never a silent Python 404.
        let (st, _headers, detail) = send(&app, "GET", "/api/sessions/w", None).await;
        assert_eq!(st, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(detail["hint"], json!("/api/workers/w"));
        // The modern path serves the detail.
        let (st, _h, detail) = send(&app, "GET", "/api/workers/w", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(detail["id"].as_str().unwrap(), id);
    }

    #[tokio::test]
    async fn conflicting_name_fields_are_400_and_legacy_name_is_accepted() {
        let (app, _dir) = app();
        // Both spellings, different values: 400 naming both (never a silent
        // winner — Invariant 37 via RR-0018a).
        let (st, _, body) = send(
            &app,
            "POST",
            "/api/workers",
            Some(json!({ "display_name": "a", "name": "b" })),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(body["display_name"], json!("a"));
        assert_eq!(body["name"], json!("b"));

        // The legacy spelling alone works.
        let (st, _, body) = send(
            &app,
            "POST",
            "/api/workers",
            Some(json!({ "name": "legacy-created" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        assert_eq!(body["display_name"], json!("legacy-created"));
    }

    // ---- auth + peek ------------------------------------------------------

    #[tokio::test]
    async fn worker_routes_sit_behind_auth_including_legacy_paths() {
        let (app, _dir) = app_with_token(Some("sekrit".into()));
        let (st, _, _) = send(&app, "GET", "/api/workers", None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        // The legacy alias must not be an auth bypass.
        let (st, _, _) = send(&app, "GET", "/api/sessions", None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        // Nor is the promoted send route, which is the one route here that
        // puts text into somebody else's terminal. It sits in the same
        // protected router as the rest, and this says so out loud.
        let (st, _, _) = send(
            &app,
            "POST",
            "/api/workers/hw/send",
            Some(json!({ "text": "hi" })),
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let (st, _, _) = send_with(
            &app,
            "GET",
            "/api/workers",
            None,
            &[("authorization", "Bearer sekrit")],
        )
        .await;
        assert_eq!(st, StatusCode::OK);
    }

    // ---- provider capabilities (AMUX-2613 gap 3) --------------------------

    #[test]
    fn provider_caps_serves_the_measured_matrix_not_all_false() {
        // Pre-fix, provider_caps() returned ProviderCapabilities::default()
        // (all false) for EVERY provider; this test then fails on the first
        // assert. claude-code's measured caps (provider/claude.rs, cited to
        // the RR-0028e spike) must reach the config classifier — and both
        // worker-row spellings must land on them.
        for spelling in ["claude", "claude-code"] {
            let caps = provider_caps(spelling);
            assert!(caps.hot_model_switch, "{spelling}: /model is a hot switch");
            assert!(caps.structured_events, "{spelling}: stream-json exists");
            assert!(caps.hooks, "{spelling}: lifecycle hooks exist");
            assert!(caps.reports_usage, "{spelling}: OAuth usage endpoint");
        }
        // gemini/codex: structured events + hooks, no usage surface.
        for spelling in ["gemini", "codex"] {
            let caps = provider_caps(spelling);
            assert!(caps.structured_events, "{spelling}");
            assert!(!caps.reports_usage, "{spelling}");
        }
        // Unknown providers keep the conservative default (over-restart,
        // never a promised capability nobody measured).
        let unknown = provider_caps("some-future-provider");
        assert!(!unknown.hot_model_switch && !unknown.structured_events);
    }

    #[tokio::test]
    async fn model_change_on_claude_is_next_turn_not_session_restart() {
        // The observable consequence of gap 3: a claude worker's model
        // change rides the hot-switch path — no session replacement.
        let (app, _dir) = app();
        let id = create(&app, "w").await;
        let (st, _, _) = send(&app, "POST", &format!("/api/workers/{id}/start"), None).await;
        assert_eq!(st, StatusCode::ACCEPTED);
        let (st, _, body) = send(
            &app,
            "PATCH",
            &format!("/api/workers/{id}"),
            Some(json!({ "model": "haiku" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["change"]["mode"], json!("next_turn"), "{body}");
        assert_eq!(body["change"]["session_replaced"], json!(false));
    }

    // ---- durable turn events reach the API (AMUX-2613 gap 1) --------------

    /// During a (mock-protocol) turn, GET /api/workers/{id} shows the worker
    /// ACTIVE with the turn id; after completion, idle — and the journal
    /// carries both transitions with payloads (RR-0111a). Pre-fix, nothing
    /// subscribed to protocol.events() in the server process, so a worker's
    /// DB state sat at whatever the last poll wrote for the whole turn.
    #[tokio::test]
    async fn turn_events_flow_to_worker_state_and_journal() {
        use crate::opencode::mock::MockProtocol;
        use amux_core::ids::TurnId;
        use amux_core::protocol::{TurnResult, WorkerEvent};

        let (app, dir) = app();
        let id = create(&app, "w").await;
        let (st, _, started) = send(&app, "POST", &format!("/api/workers/{id}/start"), None).await;
        assert_eq!(st, StatusCode::ACCEPTED, "{started}");

        // The same store the router serves, via the DB file.
        let store = std::sync::Arc::new(
            crate::db::Store::open(&dir.path().join("amux-test.db")).unwrap(),
        );
        let wid = WorkerId::parse(&id).unwrap();
        let protocol = std::sync::Arc::new(MockProtocol::new());
        protocol.register(wid.clone(), crate::opencode::AgentState::Idle);
        let proc = crate::orchestrator::events::spawn_event_processor(
            store.clone(),
            protocol.clone(),
            wid.clone(),
        );

        let turn = TurnId::from_ulid(ulid::Ulid::new());
        protocol.emit(&wid, WorkerEvent::TurnStarted { turn_id: turn.clone() });
        let mut active = Value::Null;
        for _ in 0..200 {
            let (_, _, body) = send(&app, "GET", &format!("/api/workers/{id}"), None).await;
            if body["status"] == json!("active") {
                active = body;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(active["status"], json!("active"), "worker never went active: {active}");
        assert_eq!(
            active["state"]["turn"],
            json!(turn.as_str()),
            "the API must name WHICH turn: {active}"
        );

        protocol.emit(
            &wid,
            WorkerEvent::TurnCompleted(TurnResult { turn_id: turn.clone(), outcome: "done".into() }),
        );
        let mut settled = Value::Null;
        for _ in 0..200 {
            let (_, _, body) = send(&app, "GET", &format!("/api/workers/{id}"), None).await;
            if body["status"] == json!("idle") {
                settled = body;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(settled["status"], json!("idle"), "worker never settled idle: {settled}");
        proc.abort();

        // Journal proof (RR-0111a): both transitions landed as worker
        // StatusChanged events WITH payload snapshots.
        let conn = rusqlite::Connection::open(dir.path().join("amux-test.db")).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT mutation, payload IS NOT NULL FROM _amux_state_events
                 WHERE entity_type = 'worker' AND entity_id = ?1 ORDER BY rev",
            )
            .unwrap();
        let rows: Vec<(String, bool)> = stmt
            .query_map(rusqlite::params![id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let to_active = rows
            .iter()
            .find(|(m, _)| m.contains("\"to\":\"active\""))
            .unwrap_or_else(|| panic!("no ->active journal row: {rows:?}"));
        assert!(to_active.1, "->active journal row must carry a payload snapshot");
        let to_idle = rows
            .iter()
            .find(|(m, _)| m.contains("\"to\":\"idle\""))
            .unwrap_or_else(|| panic!("no ->idle journal row: {rows:?}"));
        assert!(to_idle.1, "->idle journal row must carry a payload snapshot");
    }

    // ---- peek (AMUX-2613 gap 4) -------------------------------------------

    /// One test fn on purpose: the scenarios share the process-wide backend
    /// slot, and parallel tests mutating it would race each other.
    #[tokio::test]
    async fn peek_reads_the_live_terminal_and_names_every_non_answer() {
        use crate::backend::{
            AttachInfo, BackendError, BackendSession, BackendStatus, ProcessRef, SessionBackend,
            SessionSpec,
        };
        use async_trait::async_trait;

        struct ScriptedBackend {
            name: &'static str,
            frame: Result<String, fn() -> BackendError>,
        }
        #[async_trait]
        impl SessionBackend for ScriptedBackend {
            fn name(&self) -> &'static str {
                self.name
            }
            async fn spawn(&self, _s: &SessionSpec) -> crate::backend::Result<ProcessRef> {
                Err(BackendError::SpawnFailed("scripted".into()))
            }
            async fn terminate(&self, _p: &ProcessRef) -> crate::backend::Result<()> {
                Ok(())
            }
            async fn status(&self, _p: &ProcessRef) -> crate::backend::Result<BackendStatus> {
                Ok(BackendStatus::Running)
            }
            async fn attach_info(&self, _p: &ProcessRef) -> crate::backend::Result<AttachInfo> {
                Ok(AttachInfo { command: "true".into() })
            }
            async fn reconcile(&self) -> crate::backend::Result<Vec<BackendSession>> {
                Ok(vec![])
            }
            async fn capture(&self, _p: &ProcessRef, _l: u32) -> crate::backend::Result<String> {
                match &self.frame {
                    Ok(s) => Ok(s.clone()),
                    Err(mk) => Err(mk()),
                }
            }
        }

        let (app, _dir) = app();

        // Unknown worker: 404, before anything else.
        let (st, _, _) = send(&app, "GET", "/api/workers/ghost/peek", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        // No live session: 409 naming the fact — never an empty 200.
        let id = create(&app, "w").await;
        let (st, _, body) = send(&app, "GET", &format!("/api/workers/{id}/peek"), None).await;
        assert_eq!(st, StatusCode::CONFLICT, "{body}");
        assert!(body["error"].as_str().unwrap().contains("no live session"));

        // Live tmux-backed session + a scripted backend: 200 with the frame
        // and the full provenance shape.
        let (st, _, created) = send(
            &app,
            "POST",
            "/api/workers",
            Some(json!({ "display_name": "t", "cwd": "/tmp/w", "backend": "tmux" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let tid = created["id"].as_str().unwrap().to_string();
        let (st, _, started) = send(&app, "POST", &format!("/api/workers/{tid}/start"), None).await;
        assert_eq!(st, StatusCode::ACCEPTED);
        crate::backend::set_process_backends(vec![std::sync::Arc::new(ScriptedBackend {
            name: "tmux",
            frame: Ok("❯ cargo test\nok. 42 passed".into()),
        })]);
        let (st, _, body) =
            send(&app, "GET", &format!("/api/workers/{tid}/peek?lines=50"), None).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["output"], json!("❯ cargo test\nok. 42 passed"));
        assert_eq!(body["backend"], json!("tmux"));
        assert_eq!(body["lines_requested"], json!(50));
        assert_eq!(body["session_id"], started["session_id"]);
        assert_eq!(
            body["backend_ref"],
            json!(format!("amux-{tid}")),
            "ref derives from worker id (Invariant 43)"
        );

        // Backend configured out of this process (worker says herdr, only
        // tmux published): 503 naming the missing backend.
        let (st, _, body) = send(&app, "GET", &format!("/api/workers/{id}/peek"), None).await;
        // (id has no live session — start it on the default herdr backend.)
        let _ = (st, body);
        let (st, _, _) = send(&app, "POST", &format!("/api/workers/{id}/start"), None).await;
        assert_eq!(st, StatusCode::ACCEPTED);
        let (st, _, body) = send(&app, "GET", &format!("/api/workers/{id}/peek"), None).await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(body["error"].as_str().unwrap().contains("herdr"), "{body}");

        // Store says live, backend says gone: 409 surfacing the
        // disagreement (herdr's reaped-pane shape), not an empty capture.
        crate::backend::set_process_backends(vec![std::sync::Arc::new(ScriptedBackend {
            name: "tmux",
            frame: Err(|| BackendError::NotFound("pane gone".into())),
        })]);
        let (st, _, body) = send(&app, "GET", &format!("/api/workers/{tid}/peek"), None).await;
        assert_eq!(st, StatusCode::CONFLICT, "{body}");
        assert!(
            body["error"].as_str().unwrap().contains("recorded live"),
            "{body}"
        );
        assert!(body["backend_says"].as_str().unwrap().contains("pane gone"));

        // Other capture failures: 502 carrying the reason.
        crate::backend::set_process_backends(vec![std::sync::Arc::new(ScriptedBackend {
            name: "tmux",
            frame: Err(|| BackendError::CommandFailed("socket timeout".into())),
        })]);
        let (st, _, body) = send(&app, "GET", &format!("/api/workers/{tid}/peek"), None).await;
        assert_eq!(st, StatusCode::BAD_GATEWAY, "{body}");
        assert!(body["error"].as_str().unwrap().contains("socket timeout"));
    }
}
