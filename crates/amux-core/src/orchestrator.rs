//! Orchestrator planning core (RR-0029, Invariants 9, 10, 22, 47, 48, 49).
//!
//! Pure: `plan_tick` maps a complete picture of the fleet to a plan —
//! assignments to make, leases to reclaim, stalls to report. The runtime
//! loop in amux-server feeds it real state and executes the plan; the
//! simulation feeds it fake state and asserts on the plan. Same function,
//! which is what makes the simulation evidence rather than theatre
//! (Invariant 22 / ethos rule 7).

use crate::board::{disposition, Task, TaskDisposition};
use crate::circuit::{stall_check_enabled, FleetState};
use crate::ids::{TaskId, WorkerId};
use crate::limits::AttemptRecord;
use crate::stall::{StallReason, StallViolation};
use crate::worker::{Worker, WorkerState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A worker's claim on a task, with an expiry so a dead worker's task
/// returns to the pool without human intervention (Invariant 9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub task: TaskId,
    pub worker: WorkerId,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// Bumped on every reclaim; a write carrying a stale generation is
    /// recognizably from a dead claimant.
    pub generation: u64,
}

impl Lease {
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at
    }
}

/// One unit of "worker, do this task" (Invariant 9: idempotent,
/// at-least-once). `prior_attempts` is the feed-forward channel
/// (Invariant 49): attempt N+1 sees why attempts 1..N failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkAssignment {
    pub task: TaskId,
    pub worker: WorkerId,
    pub attempt: u32,
    pub lease: Lease,
    pub idempotency_key: String,
    pub prior_attempts: Vec<AttemptRecord>,
}

/// Priority inputs the scorer weighs. All optional-with-defaults so callers
/// supply what they know (Invariant 25: hints, not hard scheduling).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PriorityHints {
    /// Explicit user priority (higher = sooner).
    pub explicit: i32,
    /// How many tasks transitively depend on this one (critical path).
    pub dependents: u32,
    /// Tasks this worker touched before score higher on the same worker.
    pub affinity_worker: Option<WorkerId>,
}

/// Everything `plan_tick` looks at. The runtime assembles it from the store;
/// the simulation constructs it directly.
#[derive(Debug, Clone)]
pub struct TickInputs<'a> {
    pub now: DateTime<Utc>,
    pub tasks: &'a [Task],
    pub workers: &'a [Worker],
    pub leases: &'a [Lease],
    pub fleet_state: &'a FleetState,
    pub hints: &'a BTreeMap<TaskId, PriorityHints>,
    /// Attempt history per task, newest last (feed-forward).
    pub attempts: &'a BTreeMap<TaskId, Vec<AttemptRecord>>,
    /// Effective gates for disposition (resolved upstream per scope).
    pub gates: &'a [crate::board::Gate],
    /// How long a fresh lease lives.
    pub lease_secs: i64,
    /// Max concurrent leases per worker (WIP limit).
    pub wip_limit: usize,
    /// Per-provider fleet state (RR-0044b). A worker on a `QuotaExhausted`
    /// provider receives NO assignments even when the worker itself looks
    /// idle — the provider knows first. An absent provider is not evidence
    /// of anything and gates nothing (Invariant 20).
    pub provider_states:
        &'a BTreeMap<crate::provider::ProviderId, crate::provider_fleet::ProviderFleetState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TickPlan {
    pub assignments: Vec<WorkAssignment>,
    /// Leases past expiry: runtime releases them and bumps generation.
    pub reclaim: Vec<Lease>,
    pub stalls: Vec<StallViolation>,
}

/// Measured inputs for the bounded WIP controller. The controller changes by
/// at most one slot per evaluation and is inert until it has enough evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdaptiveWipInputs {
    pub current: usize,
    pub min: usize,
    pub max: usize,
    pub sample_size: u64,
    pub queue_depth: u64,
    pub active_workers: u64,
    pub average_queue_wait_ms: Option<f64>,
    pub average_lock_wait_ms: Option<f64>,
    pub conflict_rate: Option<f64>,
    pub rework_rate: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdaptiveWipDecision {
    pub recommended: usize,
    pub reason: String,
}

pub fn adaptive_wip(inputs: &AdaptiveWipInputs) -> AdaptiveWipDecision {
    let current = inputs.current.clamp(inputs.min, inputs.max.max(inputs.min));
    if inputs.sample_size < 5 {
        return AdaptiveWipDecision {
            recommended: current,
            reason: format!("insufficient evidence: {} of 5 samples", inputs.sample_size),
        };
    }
    let contention = inputs.average_lock_wait_ms.is_some_and(|ms| ms > 1_000.0)
        || inputs.conflict_rate.is_some_and(|rate| rate > 0.15)
        || inputs.rework_rate.is_some_and(|rate| rate > 0.15);
    if contention && current > inputs.min {
        return AdaptiveWipDecision {
            recommended: current - 1,
            reason: "contention or rework exceeded the safe band".into(),
        };
    }
    let demand = inputs.queue_depth > inputs.active_workers.max(1).saturating_mul(current as u64)
        && inputs.average_queue_wait_ms.is_some_and(|ms| ms > 30_000.0);
    let safe_to_expand = inputs.average_lock_wait_ms.unwrap_or(0.0) < 1_000.0
        && inputs.conflict_rate.unwrap_or(0.0) < 0.05
        && inputs.rework_rate.unwrap_or(0.0) < 0.08;
    if demand && safe_to_expand && current < inputs.max {
        return AdaptiveWipDecision {
            recommended: current + 1,
            reason: "queued demand is high and contention remains inside the safe band".into(),
        };
    }
    AdaptiveWipDecision {
        recommended: current,
        reason: "measurements do not justify a WIP change".into(),
    }
}

#[cfg(test)]
mod adaptive_wip_tests {
    use super::*;

    fn inputs() -> AdaptiveWipInputs {
        AdaptiveWipInputs {
            current: 2,
            min: 1,
            max: 4,
            sample_size: 5,
            queue_depth: 10,
            active_workers: 2,
            average_queue_wait_ms: Some(45_000.0),
            average_lock_wait_ms: Some(100.0),
            conflict_rate: Some(0.0),
            rework_rate: Some(0.0),
        }
    }

    #[test]
    fn waits_for_evidence_and_never_changes_by_more_than_one() {
        let mut measured = inputs();
        measured.sample_size = 4;
        assert_eq!(adaptive_wip(&measured).recommended, 2);

        measured.sample_size = 5;
        assert_eq!(adaptive_wip(&measured).recommended, 3);
        measured.average_lock_wait_ms = Some(8_000.0);
        assert_eq!(adaptive_wip(&measured).recommended, 1);
    }

    #[test]
    fn contention_wins_over_demand_and_limits_are_hard_bounds() {
        let mut measured = inputs();
        measured.current = 1;
        measured.conflict_rate = Some(0.5);
        assert_eq!(adaptive_wip(&measured).recommended, 1);

        measured.current = 4;
        measured.conflict_rate = Some(0.0);
        assert_eq!(adaptive_wip(&measured).recommended, 4);

        measured.current = 3;
        measured.rework_rate = Some(0.20);
        assert_eq!(adaptive_wip(&measured).recommended, 2);
    }
}

/// Age factor: an hour of waiting outranks one explicit priority point, so
/// starvation self-corrects without a separate aging pass.
fn score(task: &Task, hints: Option<&PriorityHints>, now: DateTime<Utc>) -> i64 {
    let h = hints.cloned().unwrap_or_default();
    let age_hours = (now - task.created_at).num_hours().max(0);
    (h.explicit as i64) * 10 + (h.dependents as i64) * 5 + age_hours
}

fn worker_available(w: &Worker, live_leases: usize, wip_limit: usize) -> bool {
    if !w.lifecycle.is_drivable() {
        return false;
    }
    if live_leases >= wip_limit {
        return false;
    }
    matches!(
        w.state,
        WorkerState::Idle { .. } | WorkerState::Stopped | WorkerState::Starting
    )
}

/// The planning function. Deterministic: same inputs, same plan — ties
/// break on ids, never on iteration order of a hash map.
pub fn plan_tick(inputs: &TickInputs) -> TickPlan {
    let mut plan = TickPlan {
        assignments: vec![],
        reclaim: vec![],
        stalls: vec![],
    };

    // 1. Lease release: expiry (a crashed worker must not cost an extra
    // tick) AND terminal completion — a lease on a verified/discarded/
    // quarantined task pins its worker's WIP slot for nothing (the RR-0084
    // golden had to time-warp expiries to converge before this existed).
    let task_by_id: BTreeMap<&TaskId, &Task> = inputs.tasks.iter().map(|t| (&t.id, t)).collect();
    let mut live_leases: Vec<&Lease> = vec![];
    for lease in inputs.leases {
        let task_terminal = task_by_id
            .get(&lease.task)
            .map(|t| {
                matches!(
                    disposition(t, inputs.tasks, inputs.gates),
                    TaskDisposition::Terminal
                )
            })
            .unwrap_or(false);
        if lease.is_expired(inputs.now) || task_terminal {
            plan.reclaim.push(lease.clone());
        } else {
            live_leases.push(lease);
        }
    }
    let leased_tasks: BTreeSet<&TaskId> = live_leases.iter().map(|l| &l.task).collect();
    let mut lease_count: BTreeMap<&WorkerId, usize> = BTreeMap::new();
    for l in &live_leases {
        *lease_count.entry(&l.worker).or_default() += 1;
    }

    // 2. When the circuit is open the fleet is halted: no assignments, no
    // stall reports (Invariant 10+48 — the stall-fixer must not fight the
    // breaker).
    if !matches!(inputs.fleet_state, FleetState::Normal) {
        return plan;
    }

    // Fleet-wide provider pause (RR-0044b): a worker whose provider is
    // exhausted is out of the game regardless of its own state — one
    // worker's rate limit parks every same-provider worker, instead of each
    // one independently discovering the limit and thrashing against it.
    let provider_paused = |w: &Worker| {
        inputs
            .provider_states
            .get(&w.config.provider)
            .map(|p| p.state.is_exhausted())
            .unwrap_or(false)
    };

    // 3. Runnable, unleased tasks, best score first (ties: older id first
    // for determinism).
    let mut candidates: Vec<&Task> = inputs
        .tasks
        .iter()
        .filter(|t| {
            matches!(
                disposition(t, inputs.tasks, inputs.gates),
                TaskDisposition::Runnable
            ) && !leased_tasks.contains(&t.id)
        })
        .collect();
    candidates.sort_by(|a, b| {
        let sa = score(a, inputs.hints.get(&a.id), inputs.now);
        let sb = score(b, inputs.hints.get(&b.id), inputs.now);
        sb.cmp(&sa).then_with(|| a.id.cmp(&b.id))
    });

    // 4. Match to available workers. Owned tasks go only to their owner;
    // unowned tasks go to the best available worker (affinity first).
    let mut assigned_this_tick: BTreeMap<&WorkerId, usize> = BTreeMap::new();
    for task in candidates {
        let capacity = |w: &Worker| {
            let held = lease_count.get(&w.id()).copied().unwrap_or(0)
                + assigned_this_tick.get(&w.id()).copied().unwrap_or(0);
            !provider_paused(w) && worker_available(w, held, inputs.wip_limit)
        };
        let chosen: Option<&Worker> = match &task.worker {
            Some(owner) => inputs
                .workers
                .iter()
                .find(|w| w.id() == owner)
                .filter(|w| capacity(w)),
            None => {
                let hint = inputs.hints.get(&task.id);
                let affinity = hint.and_then(|h| h.affinity_worker.as_ref());
                inputs
                    .workers
                    .iter()
                    .filter(|w| capacity(w))
                    .min_by_key(|w| {
                        // Affinity wins; then least-loaded; then id for
                        // determinism.
                        let aff = if Some(w.id()) == affinity { 0 } else { 1 };
                        let load = lease_count.get(&w.id()).copied().unwrap_or(0)
                            + assigned_this_tick.get(&w.id()).copied().unwrap_or(0);
                        (aff, load, w.id().clone())
                    })
            }
        };
        if let Some(worker) = chosen {
            let attempt_history = inputs.attempts.get(&task.id).cloned().unwrap_or_default();
            let attempt = attempt_history.len() as u32 + 1;
            let lease = Lease {
                task: task.id.clone(),
                worker: worker.id().clone(),
                acquired_at: inputs.now,
                expires_at: inputs.now + chrono::Duration::seconds(inputs.lease_secs),
                generation: 0,
            };
            *assigned_this_tick.entry(worker.id()).or_default() += 1;
            plan.assignments.push(WorkAssignment {
                idempotency_key: format!("{}:{}:{}", task.id, worker.id(), attempt),
                task: task.id.clone(),
                worker: worker.id().clone(),
                attempt,
                lease,
                prior_attempts: attempt_history,
            });
        }
    }

    // 5. Stall check (Invariant 10): an idle worker owning a non-terminal,
    // non-waiting task that this tick did NOT assign is a system failure.
    if stall_check_enabled(inputs.fleet_state) {
        let assigned: BTreeSet<&TaskId> = plan.assignments.iter().map(|a| &a.task).collect();
        for w in inputs.workers {
            let WorkerState::Idle { since } = &w.state else {
                continue;
            };
            // An exhausted provider parks its workers DELIBERATELY — that
            // silence is not a stall, and reporting it would send the
            // stall-fixer to fight the provider gate (the same livelock the
            // circuit breaker suppression above exists to prevent).
            if provider_paused(w) {
                continue;
            }
            for t in inputs.tasks {
                if t.worker.as_ref() != Some(w.id()) || assigned.contains(&t.id) {
                    continue;
                }
                if leased_tasks.contains(&t.id) {
                    continue;
                }
                match disposition(t, inputs.tasks, inputs.gates) {
                    TaskDisposition::Terminal | TaskDisposition::Waiting(_) => {}
                    TaskDisposition::Assigned { .. } => {
                        // The owner filter above means this card is assigned
                        // to THIS worker, and the leased_tasks check above
                        // means no live lease covers it: an in-flight
                        // (`doing`) card whose owner sits idle with no claim
                        // is the DRIFT case — the worker took the card and
                        // stopped without moving it. It must be a stall,
                        // because it can exit no other way: Assigned is not
                        // Runnable (never re-assigned), and the empty arm
                        // this replaces made it invisible to every tick
                        // forever, contradicting stall.rs's cardinal rule
                        // ("a worker idle while any of its tasks is
                        // non-terminal is a SYSTEM FAILURE"). Deliberate
                        // pauses stay excluded: a live lease is the grace
                        // window, provider exhaustion is skipped above, and
                        // needs_you/blocked/review/backlog/armed are Waiting
                        // dispositions, not Assigned.
                        plan.stalls.push(StallViolation {
                            worker: w.id().clone(),
                            task: t.id.clone(),
                            status: format!("{:?}", t.status),
                            idle_since: *since,
                            reason: StallReason::WorkerIdle,
                        });
                    }
                    TaskDisposition::Runnable => {
                        // Runnable + owner idle + not assigned this tick:
                        // only possible when the owner had no capacity —
                        // report it, because "quietly waiting forever" is
                        // the Python board's L3 failure.
                        plan.stalls.push(StallViolation {
                            worker: w.id().clone(),
                            task: t.id.clone(),
                            status: format!("{:?}", t.status),
                            idle_since: *since,
                            reason: StallReason::WorkerIdle,
                        });
                    }
                }
            }
        }
    }

    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{ItemType, Task, TaskStatus};
    use crate::worker::{Worker, WorkerCapabilities, WorkerConfig, WorkerState};
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap()
    }

    fn wid(n: u32) -> WorkerId {
        WorkerId::from_ulid(ulid_n(n))
    }

    fn tid(n: u32) -> TaskId {
        TaskId::from_ulid(ulid_n(n + 10_000))
    }

    fn ulid_n(n: u32) -> ulid::Ulid {
        ulid::Ulid::from_parts(1_700_000_000_000, n as u128)
    }

    fn worker(n: u32, state: WorkerState) -> Worker {
        let mut w = Worker::new(
            wid(n),
            WorkerConfig {
                display_name: format!("w{n}"),
                name_aliases: vec![],
                cwd: "/tmp".into(),
                provider: crate::provider::ProviderId("claude".into()),
                model: None,
                backend: crate::session::BackendId::herdr(),
                environment: Default::default(),
                permissions: vec![],
                group: None,
            },
            WorkerCapabilities::default(),
        );
        w.state = state;
        w
    }

    fn task(n: u32, status: TaskStatus, owner: Option<u32>) -> Task {
        let mut t = Task::create(
            tid(n),
            format!("task {n}"),
            ItemType::Code,
            crate::events::Actor::System {
                component: "test".into(),
            },
            now() - chrono::Duration::hours(1),
        );
        t.status = status;
        t.worker = owner.map(wid);
        t
    }

    /// Shared empty provider map ('static so the helper below stays
    /// signature-compatible with every existing call site).
    fn no_providers(
    ) -> &'static BTreeMap<crate::provider::ProviderId, crate::provider_fleet::ProviderFleetState>
    {
        static EMPTY: std::sync::OnceLock<
            BTreeMap<crate::provider::ProviderId, crate::provider_fleet::ProviderFleetState>,
        > = std::sync::OnceLock::new();
        EMPTY.get_or_init(BTreeMap::new)
    }

    fn inputs<'a>(
        tasks: &'a [Task],
        workers: &'a [Worker],
        leases: &'a [Lease],
        fleet: &'a FleetState,
        hints: &'a BTreeMap<TaskId, PriorityHints>,
        attempts: &'a BTreeMap<TaskId, Vec<AttemptRecord>>,
    ) -> TickInputs<'a> {
        TickInputs {
            now: now(),
            tasks,
            workers,
            leases,
            fleet_state: fleet,
            hints,
            attempts,
            gates: &[],
            lease_secs: 600,
            wip_limit: 1,
            provider_states: no_providers(),
        }
    }

    #[test]
    fn assigns_runnable_task_to_idle_owner() {
        let tasks = vec![task(1, TaskStatus::Todo, Some(1))];
        let workers = vec![worker(1, WorkerState::Idle { since: now() })];
        let (h, a) = (BTreeMap::new(), BTreeMap::new());
        let plan = plan_tick(&inputs(&tasks, &workers, &[], &FleetState::Normal, &h, &a));
        assert_eq!(plan.assignments.len(), 1);
        assert_eq!(plan.assignments[0].worker, wid(1));
        assert_eq!(plan.assignments[0].attempt, 1);
        assert!(plan.stalls.is_empty(), "assigned means not stalled");
    }

    #[test]
    fn wip_limit_prevents_double_assignment() {
        let tasks = vec![
            task(1, TaskStatus::Todo, Some(1)),
            task(2, TaskStatus::Todo, Some(1)),
        ];
        let workers = vec![worker(1, WorkerState::Idle { since: now() })];
        let (h, a) = (BTreeMap::new(), BTreeMap::new());
        let plan = plan_tick(&inputs(&tasks, &workers, &[], &FleetState::Normal, &h, &a));
        assert_eq!(plan.assignments.len(), 1, "wip_limit 1 caps at one");
        // The second runnable task correctly reports a stall (owner is
        // saturated, task sits) — visible, not silent (L3).
        assert_eq!(plan.stalls.len(), 1);
    }

    #[test]
    fn expired_lease_reclaimed_and_task_reassigned_same_tick() {
        let tasks = vec![task(1, TaskStatus::Todo, Some(1))];
        let workers = vec![worker(1, WorkerState::Idle { since: now() })];
        let leases = vec![Lease {
            task: tid(1),
            worker: wid(2),
            acquired_at: now() - chrono::Duration::hours(2),
            expires_at: now() - chrono::Duration::hours(1),
            generation: 3,
        }];
        let (h, a) = (BTreeMap::new(), BTreeMap::new());
        let plan = plan_tick(&inputs(
            &tasks,
            &workers,
            &leases,
            &FleetState::Normal,
            &h,
            &a,
        ));
        assert_eq!(plan.reclaim.len(), 1);
        assert_eq!(
            plan.assignments.len(),
            1,
            "reclaim frees the task this tick"
        );
    }

    #[test]
    fn live_lease_blocks_reassignment() {
        let tasks = vec![task(1, TaskStatus::Todo, Some(1))];
        let workers = vec![worker(1, WorkerState::Idle { since: now() })];
        let leases = vec![Lease {
            task: tid(1),
            worker: wid(1),
            acquired_at: now(),
            expires_at: now() + chrono::Duration::hours(1),
            generation: 0,
        }];
        let (h, a) = (BTreeMap::new(), BTreeMap::new());
        let plan = plan_tick(&inputs(
            &tasks,
            &workers,
            &leases,
            &FleetState::Normal,
            &h,
            &a,
        ));
        assert!(plan.assignments.is_empty());
        assert!(plan.reclaim.is_empty());
    }

    /// The drift case (2026-08-09 adherence audit): a worker takes a card
    /// into `doing`, its turn ends, the worker goes idle and never touches
    /// the board again. Pre-fix the stall arm skipped `Assigned`
    /// dispositions entirely, so this state was invisible to every tick
    /// forever — no stall, no reassignment (Assigned is not Runnable),
    /// nothing board-visible. That contradicts stall.rs's own cardinal
    /// rule: a worker idle while any of its tasks is non-terminal is a
    /// SYSTEM FAILURE.
    #[test]
    fn idle_owner_with_unleased_doing_card_is_a_stall() {
        let tasks = vec![task(1, TaskStatus::Doing, Some(1))];
        let workers = vec![worker(1, WorkerState::Idle { since: now() })];
        let (h, a) = (BTreeMap::new(), BTreeMap::new());
        let plan = plan_tick(&inputs(&tasks, &workers, &[], &FleetState::Normal, &h, &a));
        assert!(
            plan.assignments.is_empty(),
            "a doing card is never re-assigned"
        );
        assert_eq!(
            plan.stalls.len(),
            1,
            "idle owner + unleased doing card = stall"
        );
        assert_eq!(plan.stalls[0].reason, StallReason::WorkerIdle);
        assert_eq!(plan.stalls[0].task, tid(1));
        assert_eq!(plan.stalls[0].worker, wid(1));

        // A LIVE lease is the grace window (the assignment may still be
        // mid-flight through the command pump): leased means not stalled.
        let leases = vec![Lease {
            task: tid(1),
            worker: wid(1),
            acquired_at: now(),
            expires_at: now() + chrono::Duration::hours(1),
            generation: 0,
        }];
        let plan = plan_tick(&inputs(
            &tasks,
            &workers,
            &leases,
            &FleetState::Normal,
            &h,
            &a,
        ));
        assert!(
            plan.stalls.is_empty(),
            "leased doing card is not a stall: {:?}",
            plan.stalls
        );

        // A worker mid-turn (Active) is not idle: no stall — the pause is
        // the work happening.
        let workers = vec![worker(1, WorkerState::Active { turn: None })];
        let plan = plan_tick(&inputs(&tasks, &workers, &[], &FleetState::Normal, &h, &a));
        assert!(plan.stalls.is_empty(), "active owner is not a stall");
    }

    #[test]
    fn circuit_open_halts_assignment_and_stall_reporting() {
        let tasks = vec![task(1, TaskStatus::Todo, Some(1))];
        let workers = vec![worker(1, WorkerState::Idle { since: now() })];
        let open = FleetState::CircuitOpen {
            reason: crate::circuit::CircuitOpenReason::ManualStop,
            opened_at: now(),
        };
        let (h, a) = (BTreeMap::new(), BTreeMap::new());
        let plan = plan_tick(&inputs(&tasks, &workers, &[], &open, &h, &a));
        assert!(plan.assignments.is_empty());
        assert!(plan.stalls.is_empty());
    }

    #[test]
    fn priority_scoring_orders_assignments() {
        // One worker, wip 1: only the highest-scored task gets assigned.
        let tasks = vec![
            task(1, TaskStatus::Todo, None),
            task(2, TaskStatus::Todo, None),
        ];
        let workers = vec![worker(1, WorkerState::Idle { since: now() })];
        let mut hints = BTreeMap::new();
        hints.insert(
            tid(2),
            PriorityHints {
                explicit: 5,
                ..Default::default()
            },
        );
        let a = BTreeMap::new();
        let plan = plan_tick(&inputs(
            &tasks,
            &workers,
            &[],
            &FleetState::Normal,
            &hints,
            &a,
        ));
        assert_eq!(plan.assignments.len(), 1);
        assert_eq!(plan.assignments[0].task, tid(2), "explicit priority wins");
    }

    #[test]
    fn prior_attempts_feed_forward() {
        let tasks = vec![task(1, TaskStatus::Todo, Some(1))];
        let workers = vec![worker(1, WorkerState::Idle { since: now() })];
        let mut attempts = BTreeMap::new();
        attempts.insert(
            tid(1),
            vec![AttemptRecord {
                attempt: 1,
                failure_reason: "tests failed: assertion x".into(),
                rejected_evidence: vec![],
                tokens_spent: 1000,
                wall_clock_secs: 60,
                tool_calls: 1,
                cost_microusd: 0,
                decomposition_attempted: false,
                tree_status: None,
                at: now() - chrono::Duration::hours(1),
            }],
        );
        let h = BTreeMap::new();
        let plan = plan_tick(&inputs(
            &tasks,
            &workers,
            &[],
            &FleetState::Normal,
            &h,
            &attempts,
        ));
        assert_eq!(plan.assignments[0].attempt, 2);
        assert_eq!(plan.assignments[0].prior_attempts.len(), 1);
        assert!(plan.assignments[0].prior_attempts[0]
            .failure_reason
            .contains("assertion x"));
    }

    /// RR-0029's simulation requirement: 50 workers / 200 tasks, no task
    /// double-assigned, every assignment to an available worker, plan is
    /// deterministic across runs.
    #[test]
    fn simulation_50_workers_200_tasks() {
        let workers: Vec<Worker> = (0..50)
            .map(|n| worker(n, WorkerState::Idle { since: now() }))
            .collect();
        let tasks: Vec<Task> = (0..200)
            .map(|n| {
                task(
                    n,
                    TaskStatus::Todo,
                    if n % 3 == 0 { Some(n % 50) } else { None },
                )
            })
            .collect();
        let (h, a) = (BTreeMap::new(), BTreeMap::new());
        let i = inputs(&tasks, &workers, &[], &FleetState::Normal, &h, &a);
        let plan1 = plan_tick(&i);
        let plan2 = plan_tick(&i);
        assert_eq!(plan1, plan2, "planning must be deterministic");

        // No double-assignment of tasks or over-assignment of workers.
        let mut seen_tasks = BTreeSet::new();
        let mut per_worker: BTreeMap<&WorkerId, usize> = BTreeMap::new();
        for asg in &plan1.assignments {
            assert!(seen_tasks.insert(&asg.task), "task double-assigned");
            *per_worker.entry(&asg.worker).or_default() += 1;
        }
        for (_, count) in per_worker {
            assert!(count <= 1, "wip limit exceeded");
        }
        // 50 idle workers, wip 1 -> exactly 50 assignments.
        assert_eq!(plan1.assignments.len(), 50);
    }
}

// ---------------------------------------------------------------------------
// Anti-livelock enforcement (RR-0048a/e, Invariants 47, 51)
// ---------------------------------------------------------------------------

/// Decomposition caps (Invariant 51). Depth 4 is rejected, an 11th child is
/// rejected, and a run that discovers more than 50 items must report the
/// overflow rather than silently keep appending (ethos rule 5: at 100x the
/// volume, does this still discriminate?).
pub const MAX_DECOMPOSITION_DEPTH: u32 = 3;
pub const MAX_CHILDREN_PER_TASK: u32 = 10;
pub const MAX_DISCOVERED_ITEMS_PER_RUN: u32 = 50;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, serde::Serialize, serde::Deserialize)]
pub enum DecompositionError {
    #[error(
        "decomposition depth {attempted} exceeds cap {max} — quarantine instead of digging deeper"
    )]
    DepthExceeded { attempted: u32, max: u32 },
    #[error("child count {attempted} exceeds cap {max} for one task")]
    TooManyChildren { attempted: u32, max: u32 },
}

/// Gate a proposed decomposition against the Invariant 51 caps.
pub fn check_decomposition(depth: u32, children: u32) -> Result<(), DecompositionError> {
    if depth > MAX_DECOMPOSITION_DEPTH {
        return Err(DecompositionError::DepthExceeded {
            attempted: depth,
            max: MAX_DECOMPOSITION_DEPTH,
        });
    }
    if children > MAX_CHILDREN_PER_TASK {
        return Err(DecompositionError::TooManyChildren {
            attempted: children,
            max: MAX_CHILDREN_PER_TASK,
        });
    }
    Ok(())
}

/// What the runtime must do about an assignment whose task has exhausted its
/// execution limits (Invariant 47).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ExhaustionAction {
    /// Limits spent, decomposition not yet tried twice: split the task.
    Decompose { task: TaskId, depth: u32 },
    /// Limits spent AND decomposition already failed twice: terminal
    /// quarantine — retrying identically forever is the livelock this
    /// invariant exists to kill.
    Quarantine { task: TaskId, reason: String },
}

/// Filter planned assignments through ExecutionLimits. Returns the
/// assignments that may proceed and the actions for those that may not.
/// Wall-clock elapsed per task is derived from its first attempt timestamp.
pub fn enforce_limits(
    assignments: Vec<WorkAssignment>,
    limits: &crate::limits::ExecutionLimits,
    now: DateTime<Utc>,
    decomposition_depth: &BTreeMap<TaskId, u32>,
) -> (Vec<WorkAssignment>, Vec<ExhaustionAction>) {
    let mut proceed = Vec::new();
    let mut actions = Vec::new();
    for asg in assignments {
        let elapsed = asg
            .prior_attempts
            .first()
            .map(|a| (now - a.at).num_seconds().max(0) as u64)
            .unwrap_or(0);
        match crate::limits::check(limits, &asg.prior_attempts, elapsed) {
            crate::limits::LimitCheck::WithinLimits => proceed.push(asg),
            crate::limits::LimitCheck::Exhausted { which } => {
                let decompositions_tried = asg
                    .prior_attempts
                    .iter()
                    .filter(|a| a.decomposition_attempted)
                    .count();
                if decompositions_tried >= 2 {
                    actions.push(ExhaustionAction::Quarantine {
                        task: asg.task.clone(),
                        reason: format!(
                            "limits exhausted ({which:?}) after {} attempts and {decompositions_tried} failed decompositions",
                            asg.prior_attempts.len()
                        ),
                    });
                } else {
                    let depth = decomposition_depth.get(&asg.task).copied().unwrap_or(0);
                    if depth >= MAX_DECOMPOSITION_DEPTH {
                        // At max depth, exhaustion goes straight to
                        // quarantine (RR-0048e) — there is no deeper to dig.
                        actions.push(ExhaustionAction::Quarantine {
                            task: asg.task.clone(),
                            reason: format!(
                                "limits exhausted ({which:?}) at max decomposition depth {depth}"
                            ),
                        });
                    } else {
                        actions.push(ExhaustionAction::Decompose {
                            task: asg.task.clone(),
                            depth: depth + 1,
                        });
                    }
                }
            }
        }
    }
    (proceed, actions)
}

#[cfg(test)]
mod livelock_tests {
    use super::*;
    use crate::limits::{AttemptRecord, ExecutionLimits};
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap()
    }

    fn tid(n: u32) -> TaskId {
        TaskId::from_ulid(ulid::Ulid::from_parts(
            1_700_000_000_000,
            20_000 + n as u128,
        ))
    }

    fn wid() -> WorkerId {
        WorkerId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 1))
    }

    fn attempt(n: u32, decomposed: bool, tokens: u64) -> AttemptRecord {
        AttemptRecord {
            attempt: n,
            failure_reason: format!("attempt {n} failed"),
            rejected_evidence: vec![],
            tokens_spent: tokens,
            wall_clock_secs: 60,
            tool_calls: 1,
            cost_microusd: 0,
            decomposition_attempted: decomposed,
            tree_status: None,
            at: now() - chrono::Duration::hours(1),
        }
    }

    fn asg(task: TaskId, attempts: Vec<AttemptRecord>) -> WorkAssignment {
        WorkAssignment {
            idempotency_key: "k".into(),
            task: task.clone(),
            worker: wid(),
            attempt: attempts.len() as u32 + 1,
            lease: Lease {
                task,
                worker: wid(),
                acquired_at: now(),
                expires_at: now() + chrono::Duration::minutes(10),
                generation: 0,
            },
            prior_attempts: attempts,
        }
    }

    #[test]
    fn within_limits_proceeds() {
        let limits = ExecutionLimits::default();
        let (proceed, actions) = enforce_limits(
            vec![asg(tid(1), vec![attempt(1, false, 1000)])],
            &limits,
            now(),
            &BTreeMap::new(),
        );
        assert_eq!(proceed.len(), 1);
        assert!(actions.is_empty());
    }

    #[test]
    fn exhaustion_triggers_decomposition_first() {
        let limits = ExecutionLimits {
            max_attempts: 2,
            ..Default::default()
        };
        let attempts = vec![attempt(1, false, 1000), attempt(2, false, 1000)];
        let (proceed, actions) = enforce_limits(
            vec![asg(tid(1), attempts)],
            &limits,
            now(),
            &BTreeMap::new(),
        );
        assert!(proceed.is_empty());
        assert_eq!(
            actions,
            vec![ExhaustionAction::Decompose {
                task: tid(1),
                depth: 1
            }]
        );
    }

    #[test]
    fn double_decomposition_failure_quarantines() {
        let limits = ExecutionLimits {
            max_attempts: 2,
            ..Default::default()
        };
        let attempts = vec![attempt(1, true, 1000), attempt(2, true, 1000)];
        let (_, actions) = enforce_limits(
            vec![asg(tid(1), attempts)],
            &limits,
            now(),
            &BTreeMap::new(),
        );
        assert!(
            matches!(&actions[0], ExhaustionAction::Quarantine { task, reason }
                if task == &tid(1) && reason.contains("2 failed decompositions")),
            "{actions:?}"
        );
    }

    #[test]
    fn max_depth_exhaustion_quarantines_without_decomposing() {
        let limits = ExecutionLimits {
            max_attempts: 1,
            ..Default::default()
        };
        let mut depths = BTreeMap::new();
        depths.insert(tid(1), MAX_DECOMPOSITION_DEPTH);
        let (_, actions) = enforce_limits(
            vec![asg(tid(1), vec![attempt(1, false, 100)])],
            &limits,
            now(),
            &depths,
        );
        assert!(
            matches!(&actions[0], ExhaustionAction::Quarantine { reason, .. }
                if reason.contains("max decomposition depth")),
            "{actions:?}"
        );
    }

    #[test]
    fn decomposition_caps_enforced() {
        assert!(check_decomposition(3, 10).is_ok());
        assert!(matches!(
            check_decomposition(4, 1),
            Err(DecompositionError::DepthExceeded {
                attempted: 4,
                max: 3
            })
        ));
        assert!(matches!(
            check_decomposition(1, 11),
            Err(DecompositionError::TooManyChildren {
                attempted: 11,
                max: 10
            })
        ));
    }

    #[test]
    fn token_exhaustion_also_routes_through_actions() {
        let limits = ExecutionLimits {
            max_tokens: 1500,
            ..Default::default()
        };
        let attempts = vec![attempt(1, false, 1000), attempt(2, false, 1000)];
        let (proceed, actions) = enforce_limits(
            vec![asg(tid(1), attempts)],
            &limits,
            now(),
            &BTreeMap::new(),
        );
        assert!(proceed.is_empty());
        assert_eq!(actions.len(), 1);
    }
}

// ---------------------------------------------------------------------------
// Feed-forward prompt construction (RR-0048c, Invariant 49)
// ---------------------------------------------------------------------------

/// Build the execution prompt for an assignment. Attempt 1 is just the task;
/// attempt N+1 carries every prior failure VERBATIM plus an explicit
/// do-not-repeat instruction — a retry that cannot see why the last attempt
/// failed is a re-roll, and re-rolls converge on the same failure at full
/// token price (Invariant 49).
pub fn assignment_prompt(title: &str, desc: &str, asg: &WorkAssignment) -> String {
    let mut p = String::new();
    p.push_str(&format!("Task {}: {title}\n", asg.task));
    if !desc.is_empty() {
        p.push_str(&format!("\n{desc}\n"));
    }
    if !asg.prior_attempts.is_empty() {
        p.push_str(&format!(
            "\n--- PRIOR ATTEMPTS ({} failed) ---\n",
            asg.prior_attempts.len()
        ));
        for a in &asg.prior_attempts {
            p.push_str(&format!(
                "Attempt {}: FAILED — {}\n",
                a.attempt, a.failure_reason
            ));
            for ev in &a.rejected_evidence {
                p.push_str(&format!("  rejected evidence: {ev}\n"));
            }
            if a.decomposition_attempted {
                p.push_str("  (a decomposition was attempted this round)\n");
            }
            if let Some(tree) = &a.tree_status {
                p.push_str(&format!("  tree at failure: {tree}\n"));
            }
        }
        p.push_str(
            "\nDo NOT repeat the failed approaches above. Address the stated \
             failure reasons directly; if an approach was rejected, choose a \
             different one rather than resubmitting it.\n",
        );
    }
    p.push_str(&format!("\n(attempt {} of this task)\n", asg.attempt));
    p
}

#[cfg(test)]
mod prompt_tests {
    use super::*;
    use crate::limits::AttemptRecord;
    use chrono::TimeZone;

    #[test]
    fn first_attempt_is_clean_and_retry_carries_failures() {
        let t = TaskId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 900));
        let w = WorkerId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 901));
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let lease = Lease {
            task: t.clone(),
            worker: w.clone(),
            acquired_at: now,
            expires_at: now,
            generation: 0,
        };
        let fresh = WorkAssignment {
            idempotency_key: "k".into(),
            task: t.clone(),
            worker: w.clone(),
            attempt: 1,
            lease: lease.clone(),
            prior_attempts: vec![],
        };
        let p1 = assignment_prompt("fix build", "the CI is red", &fresh);
        assert!(p1.contains("fix build"));
        assert!(!p1.contains("PRIOR ATTEMPTS"));

        let retry = WorkAssignment {
            attempt: 2,
            prior_attempts: vec![AttemptRecord {
                attempt: 1,
                failure_reason: "tests failed: assertion timeout_x".into(),
                rejected_evidence: vec!["claimed green without running e2e".into()],
                tokens_spent: 5,
                wall_clock_secs: 5,
                tool_calls: 1,
                cost_microusd: 0,
                decomposition_attempted: false,
                tree_status: Some("2 files dirty".into()),
                at: now,
            }],
            ..fresh
        };
        let p2 = assignment_prompt("fix build", "", &retry);
        assert!(p2.contains("PRIOR ATTEMPTS (1 failed)"));
        assert!(p2.contains("assertion timeout_x"));
        assert!(p2.contains("claimed green without running e2e"));
        assert!(p2.contains("2 files dirty"));
        assert!(p2.contains("Do NOT repeat"));
        assert!(p2.contains("attempt 2"));
    }
}

#[cfg(test)]
mod lease_release_tests {
    use super::*;
    use crate::board::{ItemType, Task, TaskStatus};
    use chrono::TimeZone;

    #[test]
    fn terminal_task_lease_reclaimed_before_expiry() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let tid = TaskId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 30_001));
        let wid = WorkerId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 30_002));
        let mut task = Task::create(
            tid.clone(),
            "done deal",
            ItemType::Code,
            crate::events::Actor::System {
                component: "t".into(),
            },
            now,
        );
        task.status = TaskStatus::Verified; // terminal
        let leases = vec![Lease {
            task: tid.clone(),
            worker: wid,
            acquired_at: now,
            expires_at: now + chrono::Duration::hours(1), // NOT expired
            generation: 0,
        }];
        let tasks = vec![task];
        let (h, a) = (BTreeMap::new(), BTreeMap::new());
        let providers = BTreeMap::new();
        let plan = plan_tick(&TickInputs {
            now,
            tasks: &tasks,
            workers: &[],
            leases: &leases,
            fleet_state: &crate::circuit::FleetState::Normal,
            hints: &h,
            attempts: &a,
            gates: &[],
            lease_secs: 600,
            wip_limit: 1,
            provider_states: &providers,
        });
        assert_eq!(plan.reclaim.len(), 1, "terminal task frees its lease now");
    }
}
