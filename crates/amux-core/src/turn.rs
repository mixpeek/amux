//! Turn: one cycle of worker execution (RR-0013; Invariants 6, 16, 27).
//!
//! A turn starts when the worker begins processing and ends when it yields
//! (waiting for input, rate-limited, idle, done). Turn boundaries are where
//! everything lands: steering messages are delivered, board consequences
//! evaluated, memory refreshed, snapshots taken, orchestrator decisions
//! made. Without an explicit turn, "turn boundary" is a pile of heuristics —
//! which is exactly what the Python system's terminal-scraping was
//! (ethos D1). Making it a first-class entity is the exit.
//!
//! Two runtime primitives ride on the turn:
//! - [`TokenUsage`] (Invariant 16): budgets govern context assembly, turn
//!   execution, and task-level cost. Tokens-per-verified-task is a core
//!   dashboard metric, so usage must accumulate reliably (saturating — a
//!   counter that wraps is a wrong answer that looks plausible).
//! - [`ContextSnapshot`] (Invariant 27): every assignment records exactly
//!   what the worker received, content-addressed. "Worker X failed AR-123
//!   using snapshot C-991" becomes a reproducible statement, caching becomes
//!   a hash compare, and token optimization becomes measurable.

use crate::ids::{SessionId, TaskId, TurnId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How a turn ended. A running turn has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TurnOutcome {
    /// Yielded normally (finished its work or is waiting for input).
    Completed,
    /// Cut short from outside (steering, cancel, session replacement).
    Interrupted,
    /// The provider throttled mid-turn; the worker is `RateLimited`, the
    /// turn is over (a new one starts after reset — turns do not pause).
    RateLimited,
    /// The turn died: process crash, API error, context exhaustion.
    Failed { reason: String },
}

/// Token counts for one turn (Invariant 16: budgets are a runtime
/// primitive, not just a context-assembler concern).
///
/// All arithmetic is saturating: usage is a meter, and a meter that wraps
/// past `u64::MAX` back to small numbers would under-bill silently — the
/// most dangerous kind of wrong answer (ethos rule 7: a broken instrument
/// that hands you a usable number prompts no recheck).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl TokenUsage {
    /// Field-wise saturating accumulation.
    pub fn saturating_add(&self, other: &TokenUsage) -> TokenUsage {
        TokenUsage {
            input: self.input.saturating_add(other.input),
            output: self.output.saturating_add(other.output),
            cache_read: self.cache_read.saturating_add(other.cache_read),
            cache_write: self.cache_write.saturating_add(other.cache_write),
        }
    }

    /// Total tokens moved, for budget checks and the tokens-per-verified-task
    /// metric.
    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }
}

/// One cycle of a worker's execution (Invariant 6). Belongs to a session
/// (Invariant 1: the session is ephemeral, but turn HISTORY is durable
/// worker state and survives session replacement).
///
/// Running turn: `ended_at == None && outcome == None`.
/// Ended turn: both `Some` — set together, exactly once, by [`Turn::end`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub id: TurnId,
    pub session_id: SessionId,
    /// The task being worked, if any — a turn can be taskless (steering
    /// chatter, a compaction pass).
    pub task_id: Option<TaskId>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub outcome: Option<TurnOutcome>,
    pub tokens: TokenUsage,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TurnError {
    /// A turn ends exactly once. Ending twice would overwrite the original
    /// outcome/time and corrupt every metric derived from turn history
    /// (ethos rule 6: the trail must be real).
    #[error("turn {id} already ended at {ended_at}")]
    AlreadyEnded { id: TurnId, ended_at: DateTime<Utc> },
}

impl Turn {
    /// Begin a turn. Core is pure: the id is pre-minted by the caller and
    /// `started_at` is a parameter, never a clock read.
    pub fn start(
        id: TurnId,
        session_id: SessionId,
        task_id: Option<TaskId>,
        started_at: DateTime<Utc>,
    ) -> Turn {
        Turn {
            id,
            session_id,
            task_id,
            started_at,
            ended_at: None,
            outcome: None,
            tokens: TokenUsage::default(),
        }
    }

    /// Accumulate usage reported mid-turn (providers stream usage in
    /// increments; Invariant 20 normalizes them, this just counts).
    pub fn record_tokens(&mut self, delta: &TokenUsage) {
        self.tokens = self.tokens.saturating_add(delta);
    }

    /// End the turn: sets `ended_at` and `outcome` together, exactly once.
    pub fn end(self, outcome: TurnOutcome, at: DateTime<Utc>) -> Result<Turn, TurnError> {
        if let Some(ended_at) = self.ended_at {
            return Err(TurnError::AlreadyEnded {
                id: self.id,
                ended_at,
            });
        }
        Ok(Turn {
            ended_at: Some(at),
            outcome: Some(outcome),
            ..self
        })
    }

    /// A turn is running iff it has not ended.
    pub fn is_running(&self) -> bool {
        self.ended_at.is_none()
    }
}

/// One piece of assembled context (Invariant 16's deterministic pipeline:
/// task + criteria > dependencies > memory > recent turns > broad history).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFragment {
    /// Lower = earlier in the assembled context. The number IS the pipeline
    /// position, so assembly order is data, not code.
    pub priority: u32,
    /// Where this came from ("task", "memory", "dependency", ...) — kept so
    /// a snapshot explains itself (ethos rule 4).
    pub source: String,
    pub content: String,
    /// Instruction/data trust is part of the snapshot identity. Imported or
    /// retrieved content defaults to untrusted when old rows are read.
    #[serde(default)]
    pub trust: crate::policy::TrustLevel,
    /// Human-readable origin (`human`, worker id, import URI, harness).
    #[serde(default)]
    pub provenance: String,
}

/// Exactly what a worker received on assignment (Invariant 27). Immutable
/// once built; the hash is the identity.
///
/// `content_hash` is sha256 (hex) over a canonical serialization of the
/// SORTED fragments, so identical content produces an identical hash no
/// matter what order the assembler emitted it in. That property is what
/// makes "if the hash matches, reuse" a safe cache rule and "same context,
/// different outcome" a meaningful statement across attempts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSnapshot {
    pub fragments: Vec<ContextFragment>,
    pub content_hash: String,
}

impl ContextSnapshot {
    /// Build a snapshot: sort fragments deterministically, then hash.
    ///
    /// Sort key is `(priority, source, content)`. The spec's key is
    /// `(priority, source)`; `content` is appended as a tie-break because
    /// two fragments MAY share priority and source (two memory entries at
    /// the same pipeline stage), and a stable sort would then leak INPUT
    /// order into fragment order — same content, order-dependent hash,
    /// which is exactly what Invariant 27 forbids.
    ///
    /// The canonical encoding length-prefixes every string (u64 BE) and
    /// fixes `priority` at u32 BE. Length prefixes matter: naive
    /// concatenation cannot tell ("ab","c") from ("a","bc"), so two
    /// different snapshots could collide on one hash — a cache poisoning
    /// bug waiting to be "impossible to diagnose" (ethos rule 4).
    pub fn build(mut fragments: Vec<ContextFragment>) -> ContextSnapshot {
        fragments.sort_by(|a, b| {
            (a.priority, &a.source, &a.content).cmp(&(b.priority, &b.source, &b.content))
        });

        let mut hasher = Sha256::new();
        for f in &fragments {
            hasher.update(f.priority.to_be_bytes());
            hasher.update((f.source.len() as u64).to_be_bytes());
            hasher.update(f.source.as_bytes());
            hasher.update((f.content.len() as u64).to_be_bytes());
            hasher.update(f.content.as_bytes());
            let trust = match f.trust {
                crate::policy::TrustLevel::Trusted => 1u8,
                crate::policy::TrustLevel::Untrusted => 0u8,
            };
            hasher.update([trust]);
            hasher.update((f.provenance.len() as u64).to_be_bytes());
            hasher.update(f.provenance.as_bytes());
        }
        let content_hash = hex::encode(hasher.finalize());

        ContextSnapshot {
            fragments,
            content_hash,
        }
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

    fn frag(priority: u32, source: &str, content: &str) -> ContextFragment {
        ContextFragment {
            priority,
            source: source.into(),
            content: content.into(),
            trust: crate::policy::TrustLevel::Trusted,
            provenance: "test".into(),
        }
    }

    fn fixture() -> Vec<ContextFragment> {
        vec![
            frag(1, "task", "Fix the login redirect loop"),
            frag(2, "dependency", "AMUX-9 landed the session model"),
            frag(5, "memory", "auth lives in src/auth.rs"),
        ]
    }

    // ---- Turn lifecycle -------------------------------------------------

    #[test]
    fn turn_starts_running_with_zero_tokens() {
        let turn = Turn::start(
            TurnId::from_ulid(fixed_ulid("TRNA")),
            SessionId::from_ulid(fixed_ulid("AAAA")),
            Some(TaskId::from_ulid(fixed_ulid("TSKA"))),
            t("2026-08-09T12:00:00Z"),
        );
        assert!(turn.is_running());
        assert_eq!(turn.outcome, None);
        assert_eq!(turn.tokens, TokenUsage::default());
    }

    #[test]
    fn end_sets_ended_at_and_outcome_together_and_cannot_end_twice() {
        let turn = Turn::start(
            TurnId::from_ulid(fixed_ulid("TRNA")),
            SessionId::from_ulid(fixed_ulid("AAAA")),
            None,
            t("2026-08-09T12:00:00Z"),
        );
        let ended = turn
            .end(TurnOutcome::Completed, t("2026-08-09T12:05:00Z"))
            .unwrap();
        assert!(!ended.is_running());
        assert_eq!(ended.ended_at, Some(t("2026-08-09T12:05:00Z")));
        assert_eq!(ended.outcome, Some(TurnOutcome::Completed));

        let err = ended
            .clone()
            .end(
                TurnOutcome::Failed {
                    reason: "late".into(),
                },
                t("2026-08-09T12:06:00Z"),
            )
            .unwrap_err();
        assert_eq!(
            err,
            TurnError::AlreadyEnded {
                id: ended.id,
                ended_at: t("2026-08-09T12:05:00Z"),
            }
        );
    }

    // ---- TokenUsage (Invariant 16) --------------------------------------

    #[test]
    fn token_usage_accumulates_field_wise() {
        let mut turn = Turn::start(
            TurnId::from_ulid(fixed_ulid("TRNA")),
            SessionId::from_ulid(fixed_ulid("AAAA")),
            None,
            t("2026-08-09T12:00:00Z"),
        );
        turn.record_tokens(&TokenUsage {
            input: 1000,
            output: 200,
            cache_read: 5000,
            cache_write: 0,
        });
        turn.record_tokens(&TokenUsage {
            input: 10,
            output: 30,
            cache_read: 0,
            cache_write: 700,
        });
        assert_eq!(
            turn.tokens,
            TokenUsage {
                input: 1010,
                output: 230,
                cache_read: 5000,
                cache_write: 700,
            }
        );
        assert_eq!(turn.tokens.total(), 6940);
    }

    #[test]
    fn token_usage_saturates_instead_of_wrapping() {
        let near_max = TokenUsage {
            input: u64::MAX - 1,
            output: u64::MAX,
            cache_read: 0,
            cache_write: 3,
        };
        let more = TokenUsage {
            input: 100,
            output: 1,
            cache_read: 2,
            cache_write: u64::MAX,
        };
        let sum = near_max.saturating_add(&more);
        assert_eq!(sum.input, u64::MAX);
        assert_eq!(sum.output, u64::MAX);
        assert_eq!(sum.cache_read, 2);
        assert_eq!(sum.cache_write, u64::MAX);
        // total() saturates too — a meter never wraps back to small numbers.
        assert_eq!(sum.total(), u64::MAX);
    }

    // ---- ContextSnapshot (Invariant 27) ---------------------------------

    #[test]
    fn shuffled_input_produces_identical_snapshot_and_hash() {
        let in_order = ContextSnapshot::build(fixture());

        let mut reversed = fixture();
        reversed.reverse();
        let from_reversed = ContextSnapshot::build(reversed);

        assert_eq!(in_order, from_reversed);
        assert_eq!(in_order.content_hash, from_reversed.content_hash);
        // And the stored fragment order is the canonical (priority, ...) one.
        let priorities: Vec<u32> = in_order.fragments.iter().map(|f| f.priority).collect();
        assert_eq!(priorities, vec![1, 2, 5]);
    }

    #[test]
    fn equal_priority_and_source_still_hash_order_independently() {
        // The (priority, source) collision case that motivates the content
        // tie-break: without it, a stable sort leaks input order into the
        // hash and identical content stops meaning identical hash.
        let a = frag(3, "memory", "first note");
        let b = frag(3, "memory", "second note");
        let one = ContextSnapshot::build(vec![a.clone(), b.clone()]);
        let two = ContextSnapshot::build(vec![b, a]);
        assert_eq!(one.content_hash, two.content_hash);
        assert_eq!(one.fragments, two.fragments);
    }

    #[test]
    fn hash_is_stable_against_known_fixture() {
        // Pinned hex: sha256 over the canonical encoding (u32-BE priority,
        // u64-BE length-prefixed source/content/provenance plus trust byte,
        // fragments in sorted order) of `fixture()`. If this test breaks, the ENCODING changed —
        // which silently invalidates every stored snapshot hash and every
        // cache built on them. That must be a deliberate, migrated change,
        // never a drive-by.
        let snap = ContextSnapshot::build(fixture());
        assert_eq!(
            snap.content_hash,
            "3f4c742528261cedfd91539e54d9361d1768a85d85bd61a3db9c0cd6c9230a2d"
        );
    }

    #[test]
    fn hash_changes_when_content_changes() {
        let base = ContextSnapshot::build(fixture());

        let mut edited = fixture();
        edited[2].content = "auth moved to src/authn.rs".into();
        assert_ne!(
            ContextSnapshot::build(edited).content_hash,
            base.content_hash
        );

        // Priority is part of the identity too: same text at a different
        // pipeline position is a DIFFERENT context (the model sees it in a
        // different place).
        let mut reprioritized = fixture();
        reprioritized[2].priority = 4;
        assert_ne!(
            ContextSnapshot::build(reprioritized).content_hash,
            base.content_hash
        );

        // Field-boundary ambiguity is hashed away by length prefixes:
        // ("ab","c") vs ("a","bc") must not collide.
        let x = ContextSnapshot::build(vec![frag(1, "ab", "c")]);
        let y = ContextSnapshot::build(vec![frag(1, "a", "bc")]);
        assert_ne!(x.content_hash, y.content_hash);
    }

    // ---- serde ----------------------------------------------------------

    #[test]
    fn turn_and_outcome_serde_round_trip() {
        let turn = Turn::start(
            TurnId::from_ulid(fixed_ulid("TRNA")),
            SessionId::from_ulid(fixed_ulid("AAAA")),
            Some(TaskId::from_ulid(fixed_ulid("TSKA"))),
            t("2026-08-09T12:00:00Z"),
        );
        let ended = turn
            .end(
                TurnOutcome::Failed {
                    reason: "api error".into(),
                },
                t("2026-08-09T12:05:00Z"),
            )
            .unwrap();
        let json = serde_json::to_string(&ended).unwrap();
        let back: Turn = serde_json::from_str(&json).unwrap();
        assert_eq!(ended, back);

        for o in [
            TurnOutcome::Completed,
            TurnOutcome::Interrupted,
            TurnOutcome::RateLimited,
            TurnOutcome::Failed { reason: "x".into() },
        ] {
            let json = serde_json::to_string(&o).unwrap();
            let back: TurnOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(o, back);
        }
    }

    #[test]
    fn snapshot_serde_round_trips() {
        let snap = ContextSnapshot::build(fixture());
        let json = serde_json::to_string(&snap).unwrap();
        let back: ContextSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }
}
