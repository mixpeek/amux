//! Prefixed ULID identifiers for every durable entity.
//!
//! IDs are `<prefix>_<ULID>` (RR-0003: `wrk_01J...`). The prefix makes an ID
//! self-describing in logs and API payloads; the ULID makes it sortable by
//! creation time. Identity is immutable — display names change, IDs never do
//! (Invariant 43).

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! entity_id {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Mint a fresh ID from a caller-supplied ULID. Core is pure —
            /// it never reads a clock — so randomness comes from the caller
            /// (the server mints; tests pass fixed ULIDs for determinism).
            pub fn from_ulid(ulid: ulid::Ulid) -> Self {
                Self(format!(concat!($prefix, "_{}"), ulid))
            }

            /// Parse an existing ID, validating prefix and ULID shape.
            pub fn parse(s: &str) -> Result<Self, IdError> {
                let rest = s
                    .strip_prefix(concat!($prefix, "_"))
                    .ok_or_else(|| IdError::WrongPrefix {
                        expected: $prefix,
                        got: s.to_string(),
                    })?;
                rest.parse::<ulid::Ulid>()
                    .map_err(|_| IdError::BadUlid(s.to_string()))?;
                Ok(Self(s.to_string()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub const PREFIX: &'static str = $prefix;
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

entity_id!(
    /// Durable worker identity (Invariant 1). Survives sessions, renames, crashes.
    WorkerId, "wrk"
);
entity_id!(
    /// One execution instance of a worker inside a backend (Invariant 1).
    SessionId, "ses"
);
entity_id!(
    /// A board task (unit of work). Semantic board IDs like `AMUX-123` are a
    /// display alias; this is the stable identity.
    TaskId, "tsk"
);
entity_id!(
    /// One cycle of worker execution (Invariant 6).
    TurnId, "trn"
);
entity_id!(
    /// Durable message entity (Invariant 29).
    MessageId, "msg"
);
entity_id!(
    /// First-class group (Invariant 12).
    GroupId, "grp"
);
entity_id!(
    /// First-class gate entity (Invariant 18).
    GateId, "gat"
);
entity_id!(
    /// Append-only durable event (Invariant 24).
    EventId, "evt"
);
entity_id!(
    /// Acceptance criterion (Invariant 50).
    CriterionId, "cri"
);
entity_id!(
    /// A queued command in the delivery pipeline (Invariant 34).
    CommandId, "cmd"
);
entity_id!(
    /// A scoped memory entry (Invariant 42).
    MemoryId, "mem"
);
entity_id!(
    /// A verification record (Invariant 7).
    VerificationId, "ver"
);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("expected prefix {expected}_ in id {got:?}")]
    WrongPrefix { expected: &'static str, got: String },
    #[error("invalid ULID in id {0:?}")]
    BadUlid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_ulid() -> ulid::Ulid {
        "01JGXV0000000000000000TEST".parse().unwrap()
    }

    #[test]
    fn id_round_trips_through_parse() {
        let id = WorkerId::from_ulid(fixed_ulid());
        assert!(id.as_str().starts_with("wrk_"));
        let parsed = WorkerId::parse(id.as_str()).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn wrong_prefix_rejected() {
        let id = WorkerId::from_ulid(fixed_ulid());
        let err = SessionId::parse(id.as_str()).unwrap_err();
        assert!(matches!(
            err,
            IdError::WrongPrefix {
                expected: "ses",
                ..
            }
        ));
    }

    #[test]
    fn garbage_ulid_rejected() {
        let err = WorkerId::parse("wrk_notaulid").unwrap_err();
        assert!(matches!(err, IdError::BadUlid(_)));
    }

    #[test]
    fn serde_is_transparent_string() {
        let id = TaskId::from_ulid(fixed_ulid());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{}\"", id.as_str()));
        let back: TaskId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }
}
