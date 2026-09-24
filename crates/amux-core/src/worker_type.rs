//! Worker type: which execution experience a worker has (ACW-1).
//!
//! A worker type selects an EXECUTION ADAPTER and a primary OUTPUT RENDERER.
//! It selects nothing else. Identity, lifecycle, boards, messages, groups,
//! memory, schedules, gates, artifacts and orchestration are the same code for
//! every type:
//!
//! ```text
//! Worker -> WorkerType -> Execution Adapter -> Output Renderer
//! coding -> terminal session (tmux/herdr) -> terminal / peek
//! chat   -> headless provider turns        -> chat transcript
//! ```
//!
//! The id is an OPEN string (Invariant 8, same shape as `BackendId`): a new
//! type is a new [`WorkerTypeDescriptor`] in [`REGISTRY`] plus its adapter and
//! renderer. Nothing that consumes a worker matches on the type name; it asks
//! the descriptor what the type requires.

use serde::{Deserialize, Serialize};

/// Open worker-type identity. Absent everywhere it is persisted means
/// `coding`, which is how every worker that predates the field migrates.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkerTypeId(pub String);

impl WorkerTypeId {
    pub const CODING: &'static str = "coding";
    pub const CHAT: &'static str = "chat";

    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn coding() -> Self {
        Self(Self::CODING.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse a persisted or requested value. Blank means the default
    /// (`coding`); anything else must name a registered type.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let id = raw.trim().to_ascii_lowercase();
        if id.is_empty() {
            return Ok(Self::coding());
        }
        if descriptor(&id).is_some() {
            Ok(Self(id))
        } else {
            Err(format!(
                "unknown worker_type {raw:?}; expected one of: {}",
                REGISTRY.iter().map(|d| d.id).collect::<Vec<_>>().join(", ")
            ))
        }
    }

    /// The descriptor for this id. An unregistered id (a row written by a
    /// newer server, say) reads as `coding`, the behaviour it had before the
    /// field existed, rather than as an error on every list.
    pub fn descriptor(&self) -> &'static WorkerTypeDescriptor {
        descriptor(&self.0).unwrap_or(&REGISTRY[0])
    }
}

impl Default for WorkerTypeId {
    fn default() -> Self {
        Self::coding()
    }
}

impl std::fmt::Display for WorkerTypeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a requirement applies to a type. Explicit per type, so a create call
/// can be refused with the reason instead of failing later at spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    Required,
    Optional,
    Unsupported,
}

/// Everything the rest of amux may ask about a worker type.
#[derive(Debug, Clone, Serialize)]
pub struct WorkerTypeDescriptor {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// Does the type run inside a terminal backend (tmux/herdr)?
    pub terminal: Requirement,
    /// Can the worker run in a git worktree?
    pub worktree: Requirement,
    /// Does the worker need a project directory? A type that does not gets
    /// a private scratch directory so every cwd-reading path still works.
    pub project_dir: Requirement,
    /// The primary output renderer the dashboard opens (`terminal`, `chat`).
    pub renderer: &'static str,
    /// Providers the adapter can drive. Empty = every provider.
    pub providers: &'static [&'static str],
}

impl WorkerTypeDescriptor {
    pub fn supports_provider(&self, provider: &str) -> bool {
        self.providers.is_empty() || self.providers.contains(&provider)
    }

    /// Refuse a configuration this type cannot run, naming why.
    pub fn validate(&self, provider: &str, worktree: bool) -> Result<(), String> {
        if !self.supports_provider(provider) {
            return Err(format!(
                "{} workers support providers: {} (got {provider:?})",
                self.id,
                self.providers.join(", ")
            ));
        }
        if worktree && self.worktree == Requirement::Unsupported {
            return Err(format!("{} workers do not use a git worktree", self.id));
        }
        Ok(())
    }
}

/// Registered types. `REGISTRY[0]` is the default.
pub static REGISTRY: &[WorkerTypeDescriptor] = &[
    WorkerTypeDescriptor {
        id: WorkerTypeId::CODING,
        label: "Coding",
        description: "Terminal coding agent in a repo or worktree. Opens to the terminal and peek.",
        terminal: Requirement::Required,
        worktree: Requirement::Optional,
        project_dir: Requirement::Required,
        renderer: "terminal",
        providers: &[],
    },
    WorkerTypeDescriptor {
        id: WorkerTypeId::CHAT,
        label: "Chat",
        description: "Conversational agent. Opens to a persistent chat; no terminal or worktree needed.",
        terminal: Requirement::Unsupported,
        worktree: Requirement::Unsupported,
        project_dir: Requirement::Optional,
        renderer: "chat",
        providers: &["claude", "codex"],
    },
];

pub fn descriptor(id: &str) -> Option<&'static WorkerTypeDescriptor> {
    REGISTRY.iter().find(|d| d.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_parses_to_coding_so_existing_workers_migrate() {
        assert_eq!(WorkerTypeId::parse("").unwrap(), WorkerTypeId::coding());
        assert_eq!(WorkerTypeId::parse("  ").unwrap().as_str(), "coding");
        assert_eq!(WorkerTypeId::default().as_str(), "coding");
    }

    #[test]
    fn registered_types_parse_and_unknown_is_refused_by_name() {
        assert_eq!(WorkerTypeId::parse("Chat").unwrap().as_str(), "chat");
        let err = WorkerTypeId::parse("robot").unwrap_err();
        assert!(err.contains("coding") && err.contains("chat"), "{err}");
    }

    #[test]
    fn unregistered_persisted_id_reads_as_coding_behaviour() {
        assert_eq!(WorkerTypeId::new("future").descriptor().id, "coding");
    }

    #[test]
    fn requirements_are_explicit_per_type() {
        let coding = descriptor("coding").unwrap();
        let chat = descriptor("chat").unwrap();
        assert_eq!(coding.terminal, Requirement::Required);
        assert_eq!(chat.terminal, Requirement::Unsupported);
        assert!(coding.validate("gemini", true).is_ok());
        assert!(chat.validate("claude", true).unwrap_err().contains("worktree"));
        assert!(chat.validate("ollama", false).unwrap_err().contains("providers"));
        assert!(chat.validate("codex", false).is_ok());
    }

    #[test]
    fn serde_is_a_bare_string() {
        let v = serde_json::to_value(WorkerTypeId::new("chat")).unwrap();
        assert_eq!(v, serde_json::json!("chat"));
    }
}
