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

pub mod events;
pub mod mock;
pub mod structured;

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
