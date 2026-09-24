//! Execution adapters: how a worker TYPE runs, behind one seam (ACW-1/3/6).
//!
//! Every worker is the same durable entity: one env file (or `_amux_workers`
//! row), one board identity, one message ledger, one event stream. What
//! differs by `worker_type` is only how a turn executes and how its output is
//! rendered. The lifecycle code (`start_session`, `stop_for_pause`,
//! `stop_session_process`, `send_text_inner_bound`, `is_running`, `peek_verb`
//! and the fleet list) asks the worker's adapter first and only runs the
//! terminal pipeline when the adapter answers [`Dispatch::Terminal`].
//!
//! The coding adapter answers `Terminal` everywhere: it IS the existing
//! tmux/herdr pipeline, unchanged. The chat adapter handles every operation
//! itself (headless provider turns). A new type implements
//! [`ExecutionAdapter`], adds itself to [`ADAPTERS`] and registers a
//! descriptor in `amux_core::worker_type::REGISTRY`; nothing in the lifecycle
//! code changes.
//!
//! Log signal (two-fix rule): every handled dispatch emits
//! `verdict="worker_exec_dispatch"` with the type and operation, so a sweep
//! can count which adapter served what and spot a chat worker that fell into
//! the terminal pipeline (it would show a tmux `not running` with no dispatch
//! line beside it).

use super::session_verbs::{parse_env, SendOrigin};
use super::AppState;
use amux_core::worker_type::{WorkerTypeId, REGISTRY};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

/// An adapter's answer: it handled the operation, or the worker runs on the
/// shared terminal pipeline and the caller should continue with it.
pub(crate) enum Dispatch<T> {
    Handled(T),
    Terminal,
}

#[async_trait::async_trait]
pub(crate) trait ExecutionAdapter: Send + Sync {
    /// The `amux_core::worker_type` id this adapter executes.
    fn worker_type(&self) -> &'static str;
    /// Bring the worker to a state where it accepts turns.
    async fn start(&self, state: &AppState, name: &str) -> Dispatch<(bool, String)>;
    /// Stop in-flight work. No state: delete/archive call this from paths
    /// that have none, and stopping must not depend on the DB being up.
    async fn stop(&self, name: &str) -> Dispatch<(bool, String)>;
    /// Deliver one message (owner send, peer message, steering, schedule,
    /// board dispatch: all of them converge here).
    async fn deliver(
        &self,
        state: &AppState,
        name: &str,
        text: &str,
        origin: SendOrigin,
    ) -> Dispatch<(bool, String)>;
    /// Liveness for the fleet list and every `is_running` gate.
    fn running(&self, name: &str) -> Dispatch<bool>;
    /// The `/peek` payload, same keys as the terminal shape.
    async fn peek(&self, state: &AppState, name: &str, lines: i64) -> Dispatch<Value>;
}

/// The coding worker: the terminal pipeline, untouched.
struct TerminalAdapter;

#[async_trait::async_trait]
impl ExecutionAdapter for TerminalAdapter {
    fn worker_type(&self) -> &'static str {
        WorkerTypeId::CODING
    }
    async fn start(&self, _: &AppState, _: &str) -> Dispatch<(bool, String)> {
        Dispatch::Terminal
    }
    async fn stop(&self, _: &str) -> Dispatch<(bool, String)> {
        Dispatch::Terminal
    }
    async fn deliver(&self, _: &AppState, _: &str, _: &str, _: SendOrigin) -> Dispatch<(bool, String)> {
        Dispatch::Terminal
    }
    fn running(&self, _: &str) -> Dispatch<bool> {
        Dispatch::Terminal
    }
    async fn peek(&self, _: &AppState, _: &str, _: i64) -> Dispatch<Value> {
        Dispatch::Terminal
    }
}

static TERMINAL: TerminalAdapter = TerminalAdapter;
static CHAT: super::chat_worker::ChatAdapter = super::chat_worker::ChatAdapter;

/// One adapter per registered worker type. `ADAPTERS[0]` serves any id
/// without an adapter, which `WorkerTypeId::descriptor` already maps to
/// coding behaviour.
static ADAPTERS: &[&dyn ExecutionAdapter] = &[&TERMINAL, &CHAT];

pub(crate) fn adapter_for(worker_type: &WorkerTypeId) -> &'static dyn ExecutionAdapter {
    let id = worker_type.descriptor().id;
    ADAPTERS
        .iter()
        .copied()
        .find(|a| a.worker_type() == id)
        .unwrap_or(ADAPTERS[0])
}

/// The type recorded in an env map (`CC_WORKER_TYPE`; absent = coding).
pub(crate) fn worker_type_of_env(value: Option<&str>) -> WorkerTypeId {
    WorkerTypeId::parse(value.unwrap_or("")).unwrap_or_default()
}

pub(crate) fn worker_type_of(name: &str) -> WorkerTypeId {
    worker_type_of_env(parse_env(name).get("CC_WORKER_TYPE"))
}

pub(crate) fn adapter_for_session(name: &str) -> &'static dyn ExecutionAdapter {
    adapter_for(&worker_type_of(name))
}

/// Count a handled dispatch (see module doc).
pub(crate) fn note_dispatch(name: &str, op: &'static str, worker_type: &str) {
    tracing::debug!(
        session = %name, op, worker_type, measured = true, n_considered = 1,
        verdict = "worker_exec_dispatch",
        "worker operation served by its type's execution adapter"
    );
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/worker-types", get(list_types))
        .route("/api/sessions/{name}/chat", get(super::chat_worker::history_route))
        .route(
            "/api/sessions/{name}/chat/stream",
            get(super::chat_worker::stream_route),
        )
}

/// The registry, so the create/edit UI renders options and requirements from
/// data rather than from a hardcoded list.
async fn list_types() -> Response {
    Json(json!({
        "default": WorkerTypeId::CODING,
        "types": REGISTRY,
    }))
    .into_response()
}
