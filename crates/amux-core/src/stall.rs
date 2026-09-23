//! No-stall vocabulary (RR-0012; Invariant 10 — the cardinal acceptance
//! criterion).
//!
//! THE CARDINAL RULE: **a worker idle while any of its tasks is non-terminal
//! is a SYSTEM FAILURE.** Terminal states are `verified`, `archived`,
//! `discarded`, and `quarantined` — nothing else. (The plan explicitly
//! rejects a `blocked_by_user` terminal state: a terminal state with no
//! observer is where autonomous work goes to die. An external block is a
//! structured WAIT with a re-entry path, never an end state.)
//!
//! Every non-terminal task must have exactly one of: a runnable next action,
//! a named actor responsible for the next action, or a structured wait
//! reason. "Nothing is driving this" is an impossible state to REPRESENT,
//! not a condition the stall detector discovers afterward. That is why this
//! module is two disjoint enums rather than one status string:
//!
//! - [`WaitingFor`] — legitimate, structured waits. A task in one of these is
//!   NOT stalled; the dashboard shows the reason inline and the orchestrator
//!   knows what unblocks it.
//! - [`StallReason`] — system failures. Each variant is a bug in the harness,
//!   not a state of the work.
//!
//! The type-level separation is the point: `TreeConflict` living in
//! `WaitingFor` and not in `StallReason` is what makes "a dirty shared tree
//! got reported as a stall" unrepresentable (Invariant 33 / RR-0028k), and a
//! wrong classification is detectable from the serialized data itself
//! (ethos rule 4 — the tag names which enum the state came from).

use crate::ids::{GateId, TaskId, WorkerId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Why a non-terminal, non-runnable task is legitimately waiting.
///
/// Covers every such state (Invariant 10): if a task is not terminal, not
/// runnable, and not assigned, it must carry exactly one of these. A wait
/// that cannot be expressed here is a missing variant — add the cell rather
/// than forcing the least-wrong existing one (ethos rule 7: an N-cell
/// question whose honest answer is "none" is missing a cell).
///
/// Some variants are the lean Phase-0 shape of richer forms in the plan
/// (e.g. `Gate` will grow the missing criteria with RR-0011's gate
/// entities; `Capability` will grow candidate workers with RR-0021's
/// matching). Lean is fine; absent is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitingFor {
    /// Blocked on another task (Invariant 4: the board is a dependency
    /// graph). Unblocks when `on` reaches a terminal state.
    Dependency { on: TaskId },
    /// Blocked on a gate (Invariant 18). Unblocks when the gate passes.
    Gate { gate: GateId },
    /// A named human owes the next action. NOT terminal — the task stays
    /// live and visible, and it re-enters the runnable set when the user
    /// acts (Invariant 10's rejection of `blocked_by_user`).
    User,
    /// The provider is the actor: rate limit, capacity, outage. A
    /// rate-limited worker is waiting, not stalled (Invariant 10,
    /// resolution rule 1).
    Provider,
    /// Waiting on something outside amux (a deploy window, an external
    /// review, DNS propagation...).
    ExternalCondition { desc: String },
    /// No worker currently has what this task needs. Waiting — but with no
    /// resolution path it escalates to a human rather than sitting silent
    /// (Invariant 10: unresolvable waits trigger an alert, they do not rot).
    Capability { missing: String },
    /// Dirty tree or merge conflict under `IsolationPolicy::Shared`
    /// (Invariant 33, RR-0028k): `holder` currently owns the tree state at
    /// `path`. A structured WAIT, deliberately NOT a `StallReason` — the
    /// shared checkout being contended is expected coordination, not a
    /// process failure, and it names the actor whose progress unblocks it.
    TreeConflict { holder: WorkerId, path: String },
}

/// Why the system failed a task — every variant here is a harness bug to
/// fix, not a work state to display. Disjoint from [`WaitingFor`] by
/// construction; see the module docs for why the two never share an enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum StallReason {
    /// The cardinal violation: a worker sat idle while one of its tasks was
    /// runnable. Rule 5 of Invariant 10's resolution: the worker MUST be
    /// given the task and told to continue.
    WorkerIdle,
    /// A runnable task exists and no worker can take it — and nothing
    /// escalated. (An expressed `WaitingFor::Capability` is a wait; reaching
    /// HERE means the wait was never recorded or never alerted.)
    NoCapableWorker,
    /// The backend lost the process and nobody restarted it (Invariant 33:
    /// the worker should have been respawned, on any backend).
    BackendFailure { error: String },
    /// A gate is blocking with no satisfiable path — nothing is evaluating
    /// it, or it cannot be satisfied honestly (ethos rule 3).
    GateBlocked { gate: GateId },
    /// The task references a worker/session that no longer exists. Orphaned
    /// work is invisible work; it must be re-dispatched, not discovered by
    /// scrolling (ethos rule 1's armed-watch incident).
    Orphaned,
}

/// One detected violation of the no-stall guarantee. Produced by the
/// orchestrator's per-tick `stall_check`; a stall is a CI failure in every
/// golden scenario, so this struct is the evidence a failing run keeps
/// (ethos rule 4: a wrong answer must be detectable from the data).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StallViolation {
    pub worker: WorkerId,
    pub task: TaskId,
    /// The task's board status at detection time — kept as evidence, since
    /// the board may have moved by the time a human reads the violation.
    pub status: String,
    /// How long the worker has been sitting. `WorkerState::Idle` carries
    /// this timestamp precisely so this report can exist.
    pub idle_since: DateTime<Utc>,
    pub reason: StallReason,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_ulid(suffix: &str) -> ulid::Ulid {
        // ULID alphabet excludes I, L, O, U; suffixes below stay within it.
        format!("01JGXV0000000000000000{suffix}").parse().unwrap()
    }

    fn all_waits() -> Vec<WaitingFor> {
        vec![
            WaitingFor::Dependency {
                on: TaskId::from_ulid(fixed_ulid("TSKA")),
            },
            WaitingFor::Gate {
                gate: GateId::from_ulid(fixed_ulid("GATE")),
            },
            WaitingFor::User,
            WaitingFor::Provider,
            WaitingFor::ExternalCondition {
                desc: "waiting for the deploy window".into(),
            },
            WaitingFor::Capability {
                missing: "browser".into(),
            },
            WaitingFor::TreeConflict {
                holder: WorkerId::from_ulid(fixed_ulid("TEST")),
                path: "/Users/ethan/Dev/amux".into(),
            },
        ]
    }

    fn all_stalls() -> Vec<StallReason> {
        vec![
            StallReason::WorkerIdle,
            StallReason::NoCapableWorker,
            StallReason::BackendFailure {
                error: "tmux server exited".into(),
            },
            StallReason::GateBlocked {
                gate: GateId::from_ulid(fixed_ulid("GATE")),
            },
            StallReason::Orphaned,
        ]
    }

    /// Exhaustiveness documentation (Invariant 10): every legitimate
    /// non-terminal, non-runnable state maps to exactly one variant. The
    /// match has NO wildcard on purpose — adding a variant breaks this test
    /// at compile time, forcing whoever adds it to name the wait it covers.
    #[test]
    fn waiting_for_covers_all_non_terminal_non_runnable_states() {
        for w in all_waits() {
            let covered_state = match &w {
                WaitingFor::Dependency { .. } => "blocked on another task",
                WaitingFor::Gate { .. } => "blocked on a gate",
                WaitingFor::User => "a human owes the next action (not terminal)",
                WaitingFor::Provider => "provider throttled/down",
                WaitingFor::ExternalCondition { .. } => "waiting on the world outside amux",
                WaitingFor::Capability { .. } => "no worker has what this needs",
                WaitingFor::TreeConflict { .. } => "shared tree contended (Invariant 33)",
            };
            assert!(!covered_state.is_empty());
        }
        assert_eq!(
            all_waits().len(),
            7,
            "new variant? add it to all_waits() too"
        );
    }

    /// Mirror doc for the failure side: every stall is a harness bug with a
    /// named fix direction. No wildcard, same compile-time forcing.
    #[test]
    fn every_stall_reason_names_a_system_failure() {
        for s in all_stalls() {
            let fix_direction = match &s {
                StallReason::WorkerIdle => "give the worker the task, tell it to continue",
                StallReason::NoCapableWorker => "escalate — the Capability wait never alerted",
                StallReason::BackendFailure { .. } => "respawn the session (any backend)",
                StallReason::GateBlocked { .. } => "fix the gate or the type, not the truth",
                StallReason::Orphaned => "re-dispatch to a live worker",
            };
            assert!(!fix_direction.is_empty());
        }
        assert_eq!(
            all_stalls().len(),
            5,
            "new variant? add it to all_stalls() too"
        );
    }

    #[test]
    fn tree_conflict_serializes_as_a_wait_not_a_stall() {
        let wait = WaitingFor::TreeConflict {
            holder: WorkerId::from_ulid(fixed_ulid("TEST")),
            path: "crates/amux-core/src/lib.rs".into(),
        };
        let json = serde_json::to_value(&wait).unwrap();
        // Tagged as a WaitingFor kind, carrying the actor whose progress
        // unblocks it...
        assert_eq!(json["kind"], "tree_conflict");
        assert!(json["holder"].as_str().unwrap().starts_with("wrk_"));
        // ...and the same payload is NOT a representable StallReason: the
        // wait/stall separation survives serialization (ethos rule 4 — the
        // stored bytes discriminate).
        assert!(serde_json::from_value::<StallReason>(json).is_err());
    }

    #[test]
    fn waiting_for_serde_round_trips() {
        for w in all_waits() {
            let json = serde_json::to_string(&w).unwrap();
            let back: WaitingFor = serde_json::from_str(&json).unwrap();
            assert_eq!(w, back);
        }
    }

    #[test]
    fn stall_violation_serde_round_trips_with_every_reason() {
        for reason in all_stalls() {
            let v = StallViolation {
                worker: WorkerId::from_ulid(fixed_ulid("TEST")),
                task: TaskId::from_ulid(fixed_ulid("TSKA")),
                status: "doing".into(),
                idle_since: "2026-08-09T12:00:00Z".parse().unwrap(),
                reason,
            };
            let json = serde_json::to_string(&v).unwrap();
            let back: StallViolation = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }
}
