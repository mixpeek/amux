//! Session: one execution instance of a worker inside a backend (RR-0004).
//!
//! A worker is durable; a session is disposable (Invariant 1). A worker may
//! burn through many sessions — crash, context exhaustion, explicit restart,
//! config-driven replacement (Invariant 43) — without its identity, tasks,
//! messages, or history changing. Everything in this module is therefore
//! deliberately ephemeral: nothing a session carries is worker identity.
//!
//! Backend independence (Invariant 33): switching a worker between herdr and
//! tmux must not alter worker identity, task lifecycle, turns, or API
//! semantics. The backend hosts a process; it does not observe or decide.

use crate::ids::{SessionId, WorkerId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Open backend identity (Invariant 8): `"herdr"`, `"tmux"`,
/// `"native-pty"`, etc. Registry-resolved by string, never matched as a
/// closed set — adding a backend must not require recompiling amux-core.
/// Built-in values: `"herdr"` (default), `"tmux"` (fallback).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BackendId(pub String);

impl BackendId {
    pub const HERDR: &'static str = "herdr";
    pub const TMUX: &'static str = "tmux";

    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn herdr() -> Self {
        Self(Self::HERDR.to_string())
    }

    pub fn tmux() -> Self {
        Self(Self::TMUX.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for BackendId {
    fn default() -> Self {
        Self::herdr()
    }
}

impl std::fmt::Display for BackendId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for BackendId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for BackendId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Handle to the OS process a backend spawned for this session.
///
/// `backend_ref` is the backend-side name (see [`backend_ref`]); `pid` is
/// best-effort — a reconnected tmux pane may not expose one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRef {
    pub backend_ref: String,
    pub pid: Option<u32>,
}

/// Why a session ended. A session that is still running has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ExitReason {
    /// The agent finished and exited cleanly.
    Completed,
    /// The process died (OOM, panic, kill -9 from outside, ...).
    Crashed { signal: Option<i32> },
    /// The model ran out of context; the worker survives, the session does
    /// not (Invariant 1 — this is the canonical worker-outlives-session case).
    ContextExhausted,
    /// Deliberately terminated by amux or the user.
    Killed,
    /// Superseded by a new session during a `SessionRestart` config change
    /// (Invariant 43): the worker never disappears, the session is swapped.
    Replaced,
}

/// The backend-side process name for a worker.
///
/// Derives from the immutable `WorkerId`, NEVER from `display_name`
/// (Invariant 43): renaming a worker is an `Immediate` config change and must
/// not orphan its running process. `amux-{display_name}` is exactly the bug
/// this signature makes unrepresentable — display names are not a parameter,
/// so no call site can reintroduce it.
pub fn backend_ref(worker_id: &WorkerId) -> String {
    format!("amux-{}", worker_id)
}

/// One execution instance of a worker (Invariant 1).
///
/// Live session: `ended_at == None && exit_reason == None`.
/// Ended session: both `Some` — set together, exactly once, by [`Session::end`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub worker_id: WorkerId,
    pub backend: BackendId,
    pub process: Option<ProcessRef>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub exit_reason: Option<ExitReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// A session ends exactly once. Ending twice would overwrite the original
    /// exit reason/time — destroying the audit trail of why it really died
    /// (ethos rule 6: the trail must be real).
    #[error("session {id} already ended at {ended_at}")]
    AlreadyEnded {
        id: SessionId,
        ended_at: DateTime<Utc>,
    },
}

impl Session {
    /// Begin a session. Core is pure: the id is pre-minted by the caller and
    /// `started_at` is a parameter, never a clock read.
    pub fn start(
        id: SessionId,
        worker_id: WorkerId,
        backend: BackendId,
        process: Option<ProcessRef>,
        started_at: DateTime<Utc>,
    ) -> Session {
        Session {
            id,
            worker_id,
            backend,
            process,
            started_at,
            ended_at: None,
            exit_reason: None,
        }
    }

    /// End the session: sets `ended_at` and `exit_reason` together, exactly
    /// once. Consuming/returning (rather than `&mut`) keeps the lifecycle a
    /// pure state transition the caller must persist deliberately.
    pub fn end(self, reason: ExitReason, at: DateTime<Utc>) -> Result<Session, SessionError> {
        if let Some(ended_at) = self.ended_at {
            return Err(SessionError::AlreadyEnded {
                id: self.id,
                ended_at,
            });
        }
        Ok(Session {
            ended_at: Some(at),
            exit_reason: Some(reason),
            ..self
        })
    }

    /// A session is live iff it has not ended.
    pub fn is_live(&self) -> bool {
        self.ended_at.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_ulid(suffix: &str) -> ulid::Ulid {
        // ULID alphabet excludes I, L, O, U; suffixes below stay within it.
        format!("01JGXV0000000000000000{suffix}").parse().unwrap()
    }

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn backend_ref_derives_from_worker_id_not_display_name() {
        let wid = WorkerId::from_ulid(fixed_ulid("TEST"));
        let r = backend_ref(&wid);
        assert_eq!(r, format!("amux-{}", wid.as_str()));
        // The ref embeds the immutable id...
        assert!(r.contains("wrk_"));
        // ...and the function signature takes no display name at all, so a
        // rename cannot change it. (worker.rs has the end-to-end rename test.)
    }

    #[test]
    fn start_is_live_with_no_exit_fields() {
        let s = Session::start(
            SessionId::from_ulid(fixed_ulid("AAAA")),
            WorkerId::from_ulid(fixed_ulid("TEST")),
            BackendId::herdr(),
            Some(ProcessRef {
                backend_ref: "amux-wrk_x".into(),
                pid: Some(4242),
            }),
            t("2026-08-09T12:00:00Z"),
        );
        assert!(s.is_live());
        assert_eq!(s.ended_at, None);
        assert_eq!(s.exit_reason, None);
    }

    #[test]
    fn end_sets_ended_at_and_reason_together() {
        let s = Session::start(
            SessionId::from_ulid(fixed_ulid("AAAA")),
            WorkerId::from_ulid(fixed_ulid("TEST")),
            BackendId::tmux(),
            None,
            t("2026-08-09T12:00:00Z"),
        );
        let ended = s
            .end(ExitReason::ContextExhausted, t("2026-08-09T13:00:00Z"))
            .unwrap();
        assert!(!ended.is_live());
        assert_eq!(ended.ended_at, Some(t("2026-08-09T13:00:00Z")));
        assert_eq!(ended.exit_reason, Some(ExitReason::ContextExhausted));
    }

    #[test]
    fn cannot_end_twice() {
        let s = Session::start(
            SessionId::from_ulid(fixed_ulid("AAAA")),
            WorkerId::from_ulid(fixed_ulid("TEST")),
            BackendId::herdr(),
            None,
            t("2026-08-09T12:00:00Z"),
        );
        let ended = s
            .end(ExitReason::Completed, t("2026-08-09T13:00:00Z"))
            .unwrap();
        let err = ended
            .clone()
            .end(ExitReason::Killed, t("2026-08-09T14:00:00Z"))
            .unwrap_err();
        assert_eq!(
            err,
            SessionError::AlreadyEnded {
                id: ended.id,
                ended_at: t("2026-08-09T13:00:00Z"),
            }
        );
    }

    #[test]
    fn backend_id_is_an_open_transparent_string() {
        // Invariant 8: BackendId is open — built-ins serialize as their bare
        // string values and an unknown backend must round-trip untouched
        // (registering a new backend never requires recompiling amux-core).
        assert_eq!(
            serde_json::to_string(&BackendId::herdr()).unwrap(),
            "\"herdr\""
        );
        assert_eq!(
            serde_json::to_string(&BackendId::tmux()).unwrap(),
            "\"tmux\""
        );
        assert_eq!(BackendId::default(), BackendId::herdr());

        let future = BackendId::new("native-pty");
        let json = serde_json::to_string(&future).unwrap();
        assert_eq!(json, "\"native-pty\"");
        let back: BackendId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, future);
    }

    #[test]
    fn exit_reason_serde_round_trips() {
        for r in [
            ExitReason::Completed,
            ExitReason::Crashed { signal: Some(9) },
            ExitReason::Crashed { signal: None },
            ExitReason::ContextExhausted,
            ExitReason::Killed,
            ExitReason::Replaced,
        ] {
            let json = serde_json::to_string(&r).unwrap();
            let back: ExitReason = serde_json::from_str(&json).unwrap();
            assert_eq!(r, back);
        }
    }
}
