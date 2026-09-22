//! Append-only durable event history (RR-0009, Invariants 24 and 30).
//!
//! Every meaningful state mutation emits a [`DurableEvent`]. This is NOT
//! event sourcing — current state is still the DB row — it is the audit
//! trail: "why did AR-421 end up here?", replay for offline sync, metrics,
//! and the `why-blocked` query all read this stream. Invariant 30 splits the
//! world in two: structured events (this module) for machines, append-only
//! text logs for humans, correlated by the IDs in [`Correlation`].
//!
//! ## Immutability contract (Invariant 24)
//!
//! Events are append-only. The store enforces it physically (INSERT-only
//! table, no UPDATE/DELETE path); this module enforces it by SHAPE: there is
//! a constructor and there are no mutating methods — nothing in amux-core
//! edits an event after construction. Fields are `pub` for reading and for
//! serde, and Rust cannot forbid mutation of an owned value, so the contract
//! is documented here and enforced at the persistence boundary. An audit
//! trail that can be edited is worse than none, because it gets trusted
//! (ethos rule 6: the force-bypass that claimed "logged" and was not).

use crate::ids::{EventId, SessionId, TaskId, TurnId, WorkerId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Who caused an event. Provenance comes from the system stamping this at
/// the boundary, never from free text in a payload (AMUX-1768: a body-text
/// "from X" signature is not trustworthy; the stamp is).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Actor {
    /// A person. Named, because "human" without a name is an unattributed
    /// write — the shape AMUX-1812 exists to prevent.
    Human { name: String },
    /// A worker, by durable identity (survives renames — Invariant 43).
    Worker { id: WorkerId },
    /// The harness itself; `component` names which subsystem decided
    /// (scheduler, orchestrator, gate engine), so "the system did it" is
    /// still diagnosable to a specific actor (ethos rule 4).
    System { component: String },
}

/// The lifecycle transitions the audit trail records. `Custom(String)` keeps
/// the enum open for plugins and future kinds without recompiling amux-core
/// (same reasoning as open provider IDs, Invariant 8) — but a named variant
/// is always preferable where one exists, because `Custom` is invisible to
/// exhaustive matching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum EventKind {
    WorkerCreated,
    WorkerStarted,
    WorkerStopped,
    SessionStarted,
    SessionEnded,
    TurnStarted,
    TurnCompleted,
    TaskCreated,
    /// Board transition with both sides named: "transitioned" without
    /// from/to would force every consumer to re-fetch to learn what
    /// happened (ethos rule 4). Strings, not the board enum, because an
    /// audit row must stay readable even after the status vocabulary
    /// evolves — history outlives schemas.
    TaskTransitioned {
        from: String,
        to: String,
    },
    CommandQueued,
    CommandDelivered,
    /// A dead letter is a system failure (something the orchestrator wanted
    /// did not happen) — this event is what makes it non-silent
    /// (Invariant 34).
    CommandDeadLettered,
    MessageSent,
    MessageDelivered,
    GateBlocked,
    GatePassed,
    VerificationStarted,
    VerificationCompleted,
    /// An agent chose a pre-committed default (Invariant 45) — recorded so
    /// the choice is auditable as a decision, not buried in behavior.
    PolicyDecisionMade,
    /// Fleet halt (Invariant 48).
    CircuitOpened,
    /// Fleet resumed (Invariant 48).
    CircuitClosed,
    /// Open extension point for kinds that do not exist yet.
    Custom(String),
}

/// Correlation IDs tying a structured event to the task/worker/session/turn
/// it belongs to (Invariant 30). All optional — a fleet-level event (circuit
/// open) correlates with nothing — but everything that CAN be correlated
/// should be: these IDs are what let a task detail cross-link its gate
/// evaluations, tool calls, and terminal output into one timeline.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Correlation {
    pub task: Option<TaskId>,
    pub worker: Option<WorkerId>,
    pub session: Option<SessionId>,
    pub turn: Option<TurnId>,
}

impl Correlation {
    /// No correlation — for events about the system as a whole.
    pub fn none() -> Self {
        Self::default()
    }
}

/// One append-only audit event (Invariant 24). Constructed once, never
/// edited — see the module docs for the immutability contract. `at` is
/// supplied by the caller because core never reads a clock; the store stamps
/// insertion time at the boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableEvent {
    pub id: EventId,
    pub kind: EventKind,
    pub actor: Actor,
    pub at: DateTime<Utc>,
    pub correlation: Correlation,
}

impl DurableEvent {
    /// The one and only way to make an event. There are deliberately no
    /// `set_*`/`update` methods on this type and there must never be:
    /// append-only history is what makes the audit trail worth trusting.
    pub fn new(
        id: EventId,
        kind: EventKind,
        actor: Actor,
        at: DateTime<Utc>,
        correlation: Correlation,
    ) -> Self {
        Self {
            id,
            kind,
            actor,
            at,
            correlation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ulid(s: &str) -> ulid::Ulid {
        s.parse().unwrap()
    }

    fn t0() -> DateTime<Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    /// Every EventKind variant, once. Kept in one place so the exhaustive
    /// guard below can police it.
    fn all_kinds() -> Vec<EventKind> {
        vec![
            EventKind::WorkerCreated,
            EventKind::WorkerStarted,
            EventKind::WorkerStopped,
            EventKind::SessionStarted,
            EventKind::SessionEnded,
            EventKind::TurnStarted,
            EventKind::TurnCompleted,
            EventKind::TaskCreated,
            EventKind::TaskTransitioned {
                from: "todo".into(),
                to: "doing".into(),
            },
            EventKind::CommandQueued,
            EventKind::CommandDelivered,
            EventKind::CommandDeadLettered,
            EventKind::MessageSent,
            EventKind::MessageDelivered,
            EventKind::GateBlocked,
            EventKind::GatePassed,
            EventKind::VerificationStarted,
            EventKind::VerificationCompleted,
            EventKind::PolicyDecisionMade,
            EventKind::CircuitOpened,
            EventKind::CircuitClosed,
            EventKind::Custom("plugin.deploy_finished".into()),
        ]
    }

    /// Compile-time exhaustiveness guard: adding a variant to EventKind
    /// breaks this match, which forces `all_kinds()` above to be updated —
    /// so the construction test below can never silently skip a variant
    /// (ethos rule 7: a check that cannot fail is theatre).
    fn exhaustiveness_guard(kind: &EventKind) {
        match kind {
            EventKind::WorkerCreated
            | EventKind::WorkerStarted
            | EventKind::WorkerStopped
            | EventKind::SessionStarted
            | EventKind::SessionEnded
            | EventKind::TurnStarted
            | EventKind::TurnCompleted
            | EventKind::TaskCreated
            | EventKind::TaskTransitioned { .. }
            | EventKind::CommandQueued
            | EventKind::CommandDelivered
            | EventKind::CommandDeadLettered
            | EventKind::MessageSent
            | EventKind::MessageDelivered
            | EventKind::GateBlocked
            | EventKind::GatePassed
            | EventKind::VerificationStarted
            | EventKind::VerificationCompleted
            | EventKind::PolicyDecisionMade
            | EventKind::CircuitOpened
            | EventKind::CircuitClosed
            | EventKind::Custom(_) => {}
        }
    }

    #[test]
    fn event_constructs_for_every_kind() {
        let kinds = all_kinds();
        assert_eq!(kinds.len(), 22, "all_kinds() must list every variant once");
        for kind in kinds {
            exhaustiveness_guard(&kind);
            let ev = DurableEvent::new(
                EventId::from_ulid(ulid("01JGXV0000000000000000TEST")),
                kind.clone(),
                Actor::System {
                    component: "orchestrator".into(),
                },
                t0(),
                Correlation {
                    task: Some(TaskId::from_ulid(ulid("01JGXV0000000000000000AAAA"))),
                    worker: Some(WorkerId::from_ulid(ulid("01JGXV0000000000000000BBBB"))),
                    session: None,
                    turn: None,
                },
            );
            assert_eq!(ev.kind, kind);
            // Every kind must survive the wire: an event that cannot round-
            // trip is an audit row that cannot be read back.
            let json = serde_json::to_string(&ev).unwrap();
            let back: DurableEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(ev, back);
        }
    }

    #[test]
    fn actors_are_attributed() {
        let human = Actor::Human {
            name: "ethan".into(),
        };
        let json = serde_json::to_string(&human).unwrap();
        assert!(json.contains("\"type\":\"human\""), "{json}");
        assert!(json.contains("\"name\":\"ethan\""), "{json}");

        let system = Actor::System {
            component: "scheduler".into(),
        };
        let json = serde_json::to_string(&system).unwrap();
        // "the system did it" still names WHICH subsystem (ethos rule 4).
        assert!(json.contains("\"component\":\"scheduler\""), "{json}");

        let worker = Actor::Worker {
            id: WorkerId::from_ulid(ulid("01JGXV0000000000000000TEST")),
        };
        let back: Actor = serde_json::from_str(&serde_json::to_string(&worker).unwrap()).unwrap();
        assert_eq!(worker, back);
    }

    #[test]
    fn correlation_defaults_to_none() {
        let c = Correlation::none();
        assert!(c.task.is_none() && c.worker.is_none() && c.session.is_none() && c.turn.is_none());
    }

    #[test]
    fn task_transition_carries_both_sides() {
        let kind = EventKind::TaskTransitioned {
            from: "review".into(),
            to: "verified".into(),
        };
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains("\"from\":\"review\""), "{json}");
        assert!(json.contains("\"to\":\"verified\""), "{json}");
    }
}
