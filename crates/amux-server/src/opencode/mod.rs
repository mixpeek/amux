//! AgentProtocol: direct agent communication (Invariant 5, RR-0030).
//!
//! ALL prompts, messages, cancellation, and state queries flow through this
//! trait — never through the terminal backend. The terminal is an adapter
//! at the boundary (D1 exit); as structured protocol coverage grows, the
//! terminal scraper shrinks to a liveness check.
//!
//! `MockProtocol` is the simulation seam (Invariant 22): orchestrator tests
//! drive it deterministically. The OpenCode transport implementation lands
//! against the RR-0028e spike's coverage matrix.

pub mod acp;
pub mod events;
pub mod mock;
pub mod structured;

/// The model an OpenCode worker runs when neither the worker nor the fleet
/// named one.
///
/// OpenCode's own default is not a fact amux can rely on. Measured 2026-10-25 on
/// this fleet: an ACP session left on the agent's default answered every prompt
/// with `503 Upstream request failed: Endpoint is unavailable` (reproduced with
/// bare `opencode acp`, no amux in the loop), while
/// `opencode/deepseek-v4.1-flash-free` finished a tool-calling turn in 12s.
/// Pinning also makes two workers of the same provider comparable in cost and
/// behaviour instead of silently running whatever the CLI chose today.
///
/// Two things override it, and both are meant to: the worker's own model
/// (`amux register --model …`, `_amux_workers.model`) and the fleet-wide
/// `AMUX_OPENCODE_MODEL`.
pub const DEFAULT_MODEL: &str = "opencode/deepseek-v4.1-flash-free";

/// The fleet default for OpenCode workers: `AMUX_OPENCODE_MODEL` when it is set
/// and non-empty, else [`DEFAULT_MODEL`].
///
/// One function, because the structured transport, the interactive launch and
/// the worker row must not be able to disagree about which model a worker runs
/// (ethos rule 1: a view shares the predicate of the mechanism it describes).
pub fn default_model() -> String {
    std::env::var("AMUX_OPENCODE_MODEL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

use amux_core::ids::{MessageId, TurnId, WorkerId};
use amux_core::protocol::{ProgressReport, RateLimit, WorkerEvent};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prompt {
    pub text: String,
    /// Idempotency key: redelivering the same prompt must not double-run it
    /// (Invariant 9).
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgentState {
    Idle,
    Working {
        turn: Option<TurnId>,
        progress: Option<ProgressReport>,
    },
    WaitingForInput,
    RateLimited(RateLimit),
    Paused,
    Exited {
        code: Option<i32>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("worker has no live agent session: {0}")]
    NoSession(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("agent rejected the request: {0}")]
    Rejected(String),
}

pub type Result<T> = std::result::Result<T, ProtocolError>;

#[async_trait]
pub trait AgentProtocol: Send + Sync {
    async fn send_prompt(&self, worker: &WorkerId, prompt: Prompt) -> Result<()>;
    async fn deliver_message(&self, worker: &WorkerId, msg: MessageId, body: String) -> Result<()>;
    async fn cancel(&self, worker: &WorkerId) -> Result<()>;
    async fn pause(&self, worker: &WorkerId) -> Result<()>;
    async fn resume(&self, worker: &WorkerId) -> Result<()>;
    async fn state(&self, worker: &WorkerId) -> Result<AgentState>;
    /// Subscribe to this worker's event stream. Events arrive in order;
    /// a lagged subscriber is told so (broadcast semantics match the SSE
    /// contract — Invariant 26).
    fn events(&self, worker: &WorkerId) -> tokio::sync::broadcast::Receiver<WorkerEvent>;
}

// The same protocol instance drives the command pump and lifecycle API.
static PROCESS_PROTOCOL: std::sync::RwLock<Option<std::sync::Arc<dyn AgentProtocol>>> =
    std::sync::RwLock::new(None);
pub fn set_process_protocol(protocol: std::sync::Arc<dyn AgentProtocol>) {
    *PROCESS_PROTOCOL.write().unwrap() = Some(protocol);
}
pub fn process_protocol() -> Option<std::sync::Arc<dyn AgentProtocol>> {
    PROCESS_PROTOCOL.read().unwrap().clone()
}
