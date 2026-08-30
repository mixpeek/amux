//! SessionBackend: process lifecycle ONLY (Invariant 33, RR-0031/0032).
//!
//! A backend spawns, kills, and inspects agent processes. It does NOT carry
//! prompts, messages, or agent semantics — those go through the OpenCode
//! `AgentProtocol` (crate::opencode). This split is what makes backends
//! interchangeable: switching herdr <-> tmux changes nothing about how a
//! prompt is delivered, and the Python system's 250-line send-keys fight is
//! structurally impossible here.
//!
//! Backend refs derive from WorkerId, never display_name (Invariant 43 —
//! renames must not orphan processes).

pub mod adapter;
pub mod bootstrap;
pub mod herdr;
pub mod tmux;

use amux_core::ids::WorkerId;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// What a backend needs to start a session process. Everything resolved
/// upstream (scope, provider CLI invocation) — the backend runs a command in
/// a cwd with an environment, nothing more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSpec {
    pub worker: WorkerId,
    /// e.g. `["claude", "--dangerously-skip-permissions"]` — the provider
    /// layer builds this; the backend never interprets it.
    pub command: Vec<String>,
    pub cwd: String,
    pub env: std::collections::BTreeMap<String, String>,
    /// The worker's display name, for backends that can show one (herdr
    /// `workspace report-metadata` tokens — AMUX-2613 gap 5). DISPLAY ONLY:
    /// never an identifier; the backend ref stays derived from `worker`
    /// (Invariant 43 — renames must not orphan processes).
    #[serde(default)]
    pub human_label: Option<String>,
}

/// Opaque handle to a running (or once-running) process in some backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRef {
    /// Backend-scoped identifier: herdr agent name or tmux session name.
    /// Always derived as `amux-{worker_id}`.
    pub backend_ref: String,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BackendStatus {
    Running,
    Completed { exit_code: i32 },
    Crashed { signal: Option<i32> },
    NotFound,
}

/// How a human attaches a real terminal to the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachInfo {
    /// e.g. `herdr attach amux-wrk_...` or `tmux attach -t =amux-wrk_...:`
    pub command: String,
}

/// A session the backend knows about, for startup reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendSession {
    pub backend_ref: String,
    pub status: BackendStatus,
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("backend process spawn failed: {0}")]
    SpawnFailed(String),
    #[error("backend ref not found: {0}")]
    NotFound(String),
    #[error("backend command failed: {0}")]
    CommandFailed(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, BackendError>;

/// The canonical backend ref for a worker. ONE derivation, used by every
/// backend — the Python outage class where session-level and pane-level
/// target formats diverged (L2) is closed by owning the format here.
pub fn backend_ref(worker: &WorkerId) -> String {
    format!("amux-{worker}")
}

/// The backend set for this server, from configuration. tmux is always
/// present (it needs no server of its own); herdr joins when
/// `AMUX_HERDR_SESSION` names the herdr session hosting amux workspaces —
/// constructing a HerdrBackend against a missing herdr server would make
/// every probe a failure report, so absence of the variable means absence
/// of the backend. `BackendId::default()` is herdr, so a deployment that
/// wants the default worker backend to actually work sets this variable.
pub fn backends_from_env(
    env: &std::collections::BTreeMap<String, String>,
) -> Vec<Arc<dyn SessionBackend>> {
    let mut backends: Vec<Arc<dyn SessionBackend>> = vec![Arc::new(tmux::TmuxBackend::new())];
    if let Some(session) = env.get("AMUX_HERDR_SESSION").map(|s| s.trim()).filter(|s| !s.is_empty())
    {
        backends.push(Arc::new(herdr::HerdrBackend::new(session.to_string())));
    }
    backends
}

/// The process-wide backend set, published once at boot (lib.rs) for API
/// handlers that need a terminal read — worker peek (AMUX-2613 gap 4) was a
/// 501 precisely because the API layer had no backend handle. A slot rather
/// than an `AppState` field because AppState is constructed at 40+ sites
/// (every test rig included) and a required field there would be churn with
/// no information; `None` (never published — unit tests, migrate-only mode)
/// stays distinguishable from "published but this backend absent" so peek
/// can answer 503 vs "backend not configured" honestly.
static PROCESS_BACKENDS: std::sync::OnceLock<std::sync::RwLock<Vec<Arc<dyn SessionBackend>>>> =
    std::sync::OnceLock::new();

/// Publish (or, in tests, replace) the process backend set.
pub fn set_process_backends(backends: Vec<Arc<dyn SessionBackend>>) {
    let slot = PROCESS_BACKENDS.get_or_init(|| std::sync::RwLock::new(Vec::new()));
    *slot.write().unwrap() = backends;
}

/// Has `set_process_backends` ever run in this process?
pub fn process_backends_published() -> bool {
    PROCESS_BACKENDS.get().is_some()
}

/// The published backend with this name, if any.
pub fn process_backend(name: &str) -> Option<Arc<dyn SessionBackend>> {
    PROCESS_BACKENDS
        .get()?
        .read()
        .unwrap()
        .iter()
        .find(|b| b.name() == name)
        .cloned()
}

#[async_trait]
pub trait SessionBackend: Send + Sync {
    /// Human-readable backend name ("herdr", "tmux") — matches
    /// `amux_core::session::BackendId` wire values.
    fn name(&self) -> &'static str;

    async fn spawn(&self, spec: &SessionSpec) -> Result<ProcessRef>;
    async fn terminate(&self, proc: &ProcessRef) -> Result<()>;
    async fn status(&self, proc: &ProcessRef) -> Result<BackendStatus>;
    async fn attach_info(&self, proc: &ProcessRef) -> Result<AttachInfo>;
    /// Everything this backend currently hosts under the `amux-` prefix,
    /// for startup reconciliation (Invariant 9).
    async fn reconcile(&self) -> Result<Vec<BackendSession>>;
    /// Capture recent visible output. This is a LIVENESS/diagnostic view,
    /// not the control plane (D1): structured state comes from OpenCode.
    async fn capture(&self, proc: &ProcessRef, lines: u32) -> Result<String>;
    /// Native agent lifecycle states for everything this backend hosts,
    /// keyed by backend ref, in ONE batched call for the whole fleet
    /// (herdr: a single `workspace list`; #84 — the D1 exit applied to a
    /// backend). Empty = no native signal (the tmux default); `Err` =
    /// could not read. The two must stay distinguishable, or a transient
    /// backend outage masquerades as a stopped fleet.
    async fn agent_states(&self) -> Result<std::collections::BTreeMap<String, String>> {
        Ok(std::collections::BTreeMap::new())
    }
    /// Type literal text into the session's terminal and submit it.
    ///
    /// This is KEYSTROKE DELIVERY, not the control plane — the same category
    /// as `capture`, and D1 still stands: a provider that speaks OpenCode gets
    /// its prompts through `AgentProtocol`, and nothing here interprets the
    /// text. It exists because a backend ref is the only handle to the
    /// terminal that hosts a provider WITHOUT a structured protocol, and the
    /// caller (session_verbs) has no other way to reach the pane.
    ///
    /// Default = unsupported, so a backend with no delivery verb reports that
    /// instead of silently dropping a message; herdr overrides it. tmux keeps
    /// its own delivery path in session_verbs (the escape/C-u/paste-buffer
    /// discipline and its read-back verification), which this must not
    /// duplicate.
    async fn send_text(&self, _proc: &ProcessRef, _text: &str) -> Result<()> {
        Err(BackendError::CommandFailed(format!(
            "backend '{}' cannot deliver text",
            self.name()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A backend with no delivery verb must REFUSE and name itself. The one
    /// thing the default must never do is answer `Ok` — a caller reads that as
    /// "delivered" and the message is gone with nobody told.
    #[tokio::test]
    async fn default_send_text_refuses_and_names_the_backend() {
        let tmux = tmux::TmuxBackend::new();
        let proc = ProcessRef { backend_ref: "amux-wrk_x".into(), pid: None };
        let err = tmux
            .send_text(&proc, "hello")
            .await
            .expect_err("tmux keeps its own delivery path in session_verbs");
        assert!(err.to_string().contains("tmux"), "{err}");
    }

    #[test]
    fn herdr_backend_joins_only_when_a_session_is_named() {
        let none: BTreeMap<String, String> = BTreeMap::new();
        let names: Vec<&str> = backends_from_env(&none).iter().map(|b| b.name()).collect();
        assert_eq!(names, vec!["tmux"], "unconfigured herdr must stay absent");

        let mut env = BTreeMap::new();
        env.insert("AMUX_HERDR_SESSION".to_string(), "amux".to_string());
        let names: Vec<&str> = backends_from_env(&env).iter().map(|b| b.name()).collect();
        assert_eq!(names, vec!["tmux", "herdr"]);

        // Whitespace-only is absence, not a session named "  ".
        env.insert("AMUX_HERDR_SESSION".to_string(), "   ".to_string());
        let names: Vec<&str> = backends_from_env(&env).iter().map(|b| b.name()).collect();
        assert_eq!(names, vec!["tmux"]);
    }
}
