//! Fleet-wide provider rate-limit / subscription coordination (RR-0044b,
//! Invariants 20, 22).
//!
//! When 20 workers share one Claude subscription and the limit hits, all 20
//! must pause — not each independently discover the limit, thrash against
//! it, and wait for a human. This module owns the STRUCTURAL provider state
//! the orchestrator reasons about directly (replacing the Python server's
//! regex-based rate-limit watchdog + daily budget counter), plus the stagger
//! protocol that prevents a thundering herd on resume.
//!
//! Everything here is pure: [`derive`] maps worker states + a clock to
//! per-provider fleet state, [`resume_schedule`] maps parked workers to
//! staggered resume instants. The runtime feeds them real rows; the
//! simulation feeds them fixed timestamps and asserts on the outputs — same
//! functions, which is what makes the simulation evidence (Invariant 22).

use crate::ids::{CommandId, WorkerId};
use crate::protocol::{RateLimit, RateLimitKind};
use crate::provider::ProviderId;
use crate::worker::{Worker, WorkerState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Default seconds between successive un-parks of workers on a recovered
/// provider. Overridable via `AMUX_RS_RESUME_STAGGER_SECS` (the runtime
/// reads the env; core only carries the default so both the runtime and the
/// metrics view agree on one number).
pub const DEFAULT_RESUME_STAGGER_SECS: u64 = 5;

/// What the provider can currently do for the fleet.
///
/// `QuotaExhausted` covers BOTH a rate limit and subscription exhaustion —
/// the RR is explicit that a monthly plan running out is the same flow with
/// a longer `reset_at` (next billing cycle). `kind` says which bucket
/// tripped when the observation carried it; a `Credit` kind with
/// `reset_at: None` clears on payment, not a clock (AF-14), which is why
/// `reset_at` stays `Option` rather than inventing a retry time
/// (Invariant 20: never invent numbers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProviderState {
    Available,
    QuotaExhausted {
        reset_at: Option<DateTime<Utc>>,
        kind: RateLimitKind,
    },
    /// No data — a provider nobody has observed. Honest cell, never
    /// collapsed into Available or Exhausted (ethos rule 7: the answer
    /// space must fit the claim).
    Unknown,
}

impl ProviderState {
    pub fn is_exhausted(&self) -> bool {
        matches!(self, ProviderState::QuotaExhausted { .. })
    }

    /// The honest constructor from a structured rate-limit observation
    /// (`WorkerEvent::RateLimited`) — the event carries kind + reset, so an
    /// event-driven producer never has to degrade to `Unknown` kind the way
    /// a derivation from bare `WorkerState` must.
    pub fn from_rate_limit(rl: &RateLimit) -> ProviderState {
        ProviderState::QuotaExhausted {
            reset_at: rl.reset_at,
            kind: rl.kind,
        }
    }
}

/// How parked workers come back (RR-0044b). amux never APPLIES
/// `Redistribute` on its own: the configured provider is the user's
/// decision (routing.rs's rule — never silently swap it), so redistribution
/// ships as a recommendation event the human/policy layer acts on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum ResumeStrategy {
    /// Wait for the provider's reset instant, then resume. `auto_resume`
    /// defaults true — no human presses "continue".
    WaitForReset {
        reset_at: DateTime<Utc>,
        auto_resume: bool,
    },
    /// Move workers to a fallback provider. Advisory only (see above).
    Redistribute {
        fallback: ProviderId,
        workers_moved: Vec<WorkerId>,
    },
    /// Resume workers `interval_secs` apart in `order` — the thundering-herd
    /// preventer. `order` is deterministic (sorted worker ids).
    Stagger {
        interval_secs: u64,
        order: Vec<WorkerId>,
    },
}

/// One worker's scheduled resume instant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledResume {
    pub worker: WorkerId,
    pub resume_at: DateTime<Utc>,
}

/// Fleet-level view of one provider (RR-0044b).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderFleetState {
    pub provider: ProviderId,
    pub state: ProviderState,
    /// Every worker configured on this provider (sorted). The pause scope:
    /// an exhausted provider blocks assignment AND delivery to all of them,
    /// including workers that still LOOK idle — the provider knows first.
    pub workers: Vec<WorkerId>,
    /// The subset currently parked (`WorkerState::RateLimited`), sorted.
    /// This is the resume `order` and the dashboard's "N workers parked".
    pub affected_workers: Vec<WorkerId>,
    /// Commands parked for affected workers. [`derive`] leaves this empty —
    /// worker states carry no queue data; the durable truth is the command
    /// queue itself (`_amux_commands` stays Queued while the pump refuses
    /// delivery), and a caller holding queue rows may fill this for display.
    pub parked_commands: Vec<(WorkerId, CommandId)>,
    /// `None` = no clock-based resume exists (nothing parked, or no reset
    /// time is known — a Credit cap clears on payment, not a timer).
    pub resume_strategy: Option<ResumeStrategy>,
}

/// Staggered resume schedule: worker `i` (in sorted, deduped order) resumes
/// at `reset_at + i * interval`. Deterministic: the same set of workers
/// always yields the same schedule regardless of input order, so the
/// runtime, a retry of the runtime, and the simulation can never disagree
/// about who resumes when (ethos rule 4).
pub fn resume_schedule(
    affected: &[WorkerId],
    reset_at: DateTime<Utc>,
    interval: chrono::Duration,
) -> Vec<ScheduledResume> {
    let mut order: Vec<WorkerId> = affected.to_vec();
    order.sort();
    order.dedup();
    order
        .into_iter()
        .enumerate()
        .map(|(i, worker)| ScheduledResume {
            worker,
            resume_at: reset_at + interval * (i as i32),
        })
        .collect()
}

/// Derive per-provider fleet state from worker states — the tick-time
/// mapping the runtime and the metrics endpoint both use (one derivation,
/// so the view can never disagree with the mechanism it describes).
///
/// Evidence rule: a worker `RateLimited { reset_at }` is evidence of
/// provider exhaustion only while the reset has NOT passed (`reset_at > now`
/// or unknown). A parked worker whose reset already passed is not evidence —
/// its limit lifted and it is merely awaiting its stagger slot — so the
/// provider reads Available and already-resumed siblings can receive work
/// mid-drain (the staggered-resume phase would otherwise deadlock: parked
/// stragglers would keep the provider "exhausted" against the workers that
/// already resumed).
///
/// The provider's `reset_at` is the LATEST reset among evidence workers:
/// resuming at the earliest observation would thrash workers whose window
/// is known to extend further. `kind` is `Unknown` here because
/// `WorkerState` does not carry the bucket; event-driven producers use
/// [`ProviderState::from_rate_limit`] and keep the real kind.
pub fn derive(
    workers: &[Worker],
    now: DateTime<Utc>,
    stagger_interval_secs: u64,
) -> BTreeMap<ProviderId, ProviderFleetState> {
    struct Acc {
        workers: Vec<WorkerId>,
        parked: Vec<(WorkerId, Option<DateTime<Utc>>)>,
    }
    let mut acc: BTreeMap<ProviderId, Acc> = BTreeMap::new();
    for w in workers {
        let a = acc.entry(w.config.provider.clone()).or_insert(Acc {
            workers: vec![],
            parked: vec![],
        });
        a.workers.push(w.id().clone());
        if let WorkerState::RateLimited { reset_at } = &w.state {
            a.parked.push((w.id().clone(), *reset_at));
        }
    }

    acc.into_iter()
        .map(|(provider, mut a)| {
            a.workers.sort();
            a.parked.sort_by(|x, y| x.0.cmp(&y.0));
            // Exhaustion evidence: unresolved parks only (see doc above).
            let evidence: Vec<Option<DateTime<Utc>>> = a
                .parked
                .iter()
                .filter(|(_, r)| r.is_none_or(|r| r > now))
                .map(|(_, r)| *r)
                .collect();
            let state = if evidence.is_empty() {
                ProviderState::Available
            } else {
                ProviderState::QuotaExhausted {
                    reset_at: evidence.iter().filter_map(|r| *r).max(),
                    kind: RateLimitKind::Unknown,
                }
            };
            let affected: Vec<WorkerId> = a.parked.iter().map(|(w, _)| w.clone()).collect();
            let latest_known_reset = a.parked.iter().filter_map(|(_, r)| *r).max();
            let resume_strategy = match (&affected[..], latest_known_reset) {
                ([], _) | (_, None) => None,
                ([_one], Some(reset_at)) => Some(ResumeStrategy::WaitForReset {
                    reset_at,
                    auto_resume: true,
                }),
                (_, Some(_)) => Some(ResumeStrategy::Stagger {
                    interval_secs: stagger_interval_secs,
                    order: affected.clone(),
                }),
            };
            (
                provider.clone(),
                ProviderFleetState {
                    provider,
                    state,
                    workers: a.workers,
                    affected_workers: affected,
                    parked_commands: vec![],
                    resume_strategy,
                },
            )
        })
        .collect()
}

/// Is this provider exhausted, per the derived map? Absent = not exhausted
/// (absence of data is absence of evidence, Invariant 20).
pub fn provider_exhausted(
    states: &BTreeMap<ProviderId, ProviderFleetState>,
    provider: &ProviderId,
) -> bool {
    states
        .get(provider)
        .map(|p| p.state.is_exhausted())
        .unwrap_or(false)
}

/// Does this worker sit on an exhausted provider? Used by the command pump:
/// delivery to such a worker is refused EVEN for `Immediate` timing — the
/// worker may look idle, but the provider knows first (RR-0044b).
pub fn worker_on_exhausted_provider(
    states: &BTreeMap<ProviderId, ProviderFleetState>,
    worker: &WorkerId,
) -> bool {
    states
        .values()
        .any(|p| p.state.is_exhausted() && p.workers.binary_search(worker).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{ItemType, Task, TaskStatus};
    use crate::circuit::FleetState;
    use crate::ids::TaskId;
    use crate::orchestrator::{plan_tick, PriorityHints, TickInputs, TickPlan};
    use crate::worker::{WorkerCapabilities, WorkerConfig};
    use chrono::TimeZone;
    use std::collections::BTreeMap;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap()
    }

    fn at(secs: i64) -> DateTime<Utc> {
        t0() + chrono::Duration::seconds(secs)
    }

    fn wid(n: u32) -> WorkerId {
        WorkerId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, n as u128))
    }

    fn tid(n: u32) -> TaskId {
        TaskId::from_ulid(ulid::Ulid::from_parts(
            1_700_000_000_000,
            40_000 + n as u128,
        ))
    }

    fn worker(n: u32, provider: &str, state: WorkerState) -> Worker {
        let mut w = Worker::new(
            wid(n),
            WorkerConfig {
                display_name: format!("w{n}"),
                name_aliases: vec![],
                cwd: "/tmp".into(),
                provider: ProviderId::new(provider),
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

    fn task(n: u32, owner: u32) -> Task {
        let mut t = Task::create(
            tid(n),
            format!("task {n}"),
            ItemType::Code,
            crate::events::Actor::System {
                component: "test".into(),
            },
            t0() - chrono::Duration::hours(1),
        );
        t.status = TaskStatus::Todo;
        t.worker = Some(wid(owner));
        t
    }

    /// plan_tick at a fixed instant with the derived provider map — the
    /// exact pipeline the runtime runs, on fixed timestamps.
    fn plan_at(now: DateTime<Utc>, tasks: &[Task], workers: &[Worker]) -> TickPlan {
        let provider_states = derive(workers, now, DEFAULT_RESUME_STAGGER_SECS);
        let hints: BTreeMap<TaskId, PriorityHints> = BTreeMap::new();
        let attempts = BTreeMap::new();
        plan_tick(&TickInputs {
            now,
            tasks,
            workers,
            leases: &[],
            fleet_state: &FleetState::Normal,
            hints: &hints,
            attempts: &attempts,
            gates: &[],
            lease_secs: 600,
            wip_limit: 1,
            provider_states: &provider_states,
        })
    }

    // ---- derive ---------------------------------------------------------

    #[test]
    fn derive_available_when_no_worker_is_limited() {
        let workers = vec![
            worker(1, "claude", WorkerState::Idle { since: t0() }),
            worker(2, "claude", WorkerState::Active { turn: None }),
        ];
        let m = derive(&workers, t0(), 5);
        let p = &m[&ProviderId::new("claude")];
        assert_eq!(p.state, ProviderState::Available);
        assert_eq!(p.workers, vec![wid(1), wid(2)]);
        assert!(p.affected_workers.is_empty());
        assert_eq!(p.resume_strategy, None);
    }

    #[test]
    fn derive_marks_provider_exhausted_from_one_workers_limit() {
        // One limited worker exhausts the provider for ALL its workers; the
        // provider reset is the LATEST evidence (early resume would thrash).
        let workers = vec![
            worker(
                1,
                "claude",
                WorkerState::RateLimited {
                    reset_at: Some(at(60)),
                },
            ),
            worker(2, "claude", WorkerState::Idle { since: t0() }),
            worker(
                3,
                "claude",
                WorkerState::RateLimited {
                    reset_at: Some(at(120)),
                },
            ),
            worker(4, "codex", WorkerState::Idle { since: t0() }),
        ];
        let m = derive(&workers, t0(), 5);
        let claude = &m[&ProviderId::new("claude")];
        assert_eq!(
            claude.state,
            ProviderState::QuotaExhausted {
                reset_at: Some(at(120)),
                kind: RateLimitKind::Unknown
            }
        );
        assert_eq!(claude.affected_workers, vec![wid(1), wid(3)]);
        assert_eq!(claude.workers, vec![wid(1), wid(2), wid(3)]);
        assert_eq!(
            claude.resume_strategy,
            Some(ResumeStrategy::Stagger {
                interval_secs: 5,
                order: vec![wid(1), wid(3)]
            })
        );
        // The sibling provider is untouched.
        assert_eq!(m[&ProviderId::new("codex")].state, ProviderState::Available);
        // Membership helpers: idle worker 2 IS on the exhausted provider.
        assert!(worker_on_exhausted_provider(&m, &wid(2)));
        assert!(!worker_on_exhausted_provider(&m, &wid(4)));
        assert!(provider_exhausted(&m, &ProviderId::new("claude")));
        assert!(!provider_exhausted(&m, &ProviderId::new("codex")));
    }

    #[test]
    fn derive_past_reset_is_not_exhaustion_evidence() {
        // A parked worker whose reset already passed: the LIMIT lifted; the
        // worker is only awaiting its stagger slot. Provider reads Available
        // so already-resumed siblings can take work mid-drain.
        let workers = vec![
            worker(
                1,
                "claude",
                WorkerState::RateLimited {
                    reset_at: Some(at(-10)),
                },
            ),
            worker(2, "claude", WorkerState::Idle { since: t0() }),
        ];
        let m = derive(&workers, t0(), 5);
        let p = &m[&ProviderId::new("claude")];
        assert_eq!(p.state, ProviderState::Available);
        // ...but the worker is still visibly parked (dashboard truth).
        assert_eq!(p.affected_workers, vec![wid(1)]);
        assert_eq!(
            p.resume_strategy,
            Some(ResumeStrategy::WaitForReset {
                reset_at: at(-10),
                auto_resume: true
            })
        );
    }

    #[test]
    fn derive_unknown_reset_exhausts_with_no_clock_resume() {
        // No reset time (Credit cap: clears on payment, not a clock) — the
        // provider is exhausted indefinitely and there is honestly no
        // schedule to offer (Invariant 20: never invent a retry time).
        let workers = vec![worker(
            1,
            "claude",
            WorkerState::RateLimited { reset_at: None },
        )];
        let m = derive(&workers, t0(), 5);
        let p = &m[&ProviderId::new("claude")];
        assert_eq!(
            p.state,
            ProviderState::QuotaExhausted {
                reset_at: None,
                kind: RateLimitKind::Unknown
            }
        );
        assert_eq!(p.resume_strategy, None);
    }

    // ---- stagger protocol ----------------------------------------------

    #[test]
    fn resume_schedule_is_staggered_deterministic_and_spaced() {
        let interval = chrono::Duration::seconds(5);
        // Shuffled input with a duplicate: order must come out sorted and
        // deduped — same schedule for the same set, always.
        let shuffled = vec![wid(3), wid(1), wid(2), wid(1)];
        let sched = resume_schedule(&shuffled, at(20), interval);
        assert_eq!(
            sched.iter().map(|s| s.worker.clone()).collect::<Vec<_>>(),
            vec![wid(1), wid(2), wid(3)]
        );
        assert_eq!(sched[0].resume_at, at(20));
        assert_eq!(sched[1].resume_at, at(25));
        assert_eq!(sched[2].resume_at, at(30));
        // No two resumes inside the interval.
        for pair in sched.windows(2) {
            assert!(pair[1].resume_at - pair[0].resume_at >= interval);
        }
        // Permutation invariance — determinism regardless of input order.
        let other_order = vec![wid(2), wid(3), wid(1)];
        assert_eq!(resume_schedule(&other_order, at(20), interval), sched);
    }

    // ---- fleet pause through plan_tick ---------------------------------

    #[test]
    fn fleet_pause_idle_same_provider_workers_get_nothing() {
        // Worker 1 hit the limit. Workers 2 and 3 LOOK idle and own
        // runnable tasks — the provider knows first: they get NOTHING, and
        // their silence is not a stall (the pause is deliberate; a stall
        // report here would send the stall-fixer to fight the provider
        // gate, the Invariant 10+48 livelock in provider clothes).
        let workers = vec![
            worker(
                1,
                "claude",
                WorkerState::RateLimited {
                    reset_at: Some(at(3600)),
                },
            ),
            worker(2, "claude", WorkerState::Idle { since: t0() }),
            worker(3, "claude", WorkerState::Idle { since: t0() }),
            worker(4, "codex", WorkerState::Idle { since: t0() }),
        ];
        let tasks = vec![task(1, 1), task(2, 2), task(3, 3), task(4, 4)];
        let plan = plan_at(t0(), &tasks, &workers);
        assert_eq!(plan.assignments.len(), 1, "{:?}", plan.assignments);
        assert_eq!(
            plan.assignments[0].worker,
            wid(4),
            "only the codex worker runs"
        );
        assert!(
            plan.stalls.is_empty(),
            "paused fleet is not stalled: {:?}",
            plan.stalls
        );
    }

    #[test]
    fn subscription_exhaustion_long_reset_is_the_same_flow() {
        // Monthly plan out: same flow, longer reset_at (next billing
        // cycle). Built with the real kind an event-driven producer keeps.
        let rl = RateLimit {
            kind: RateLimitKind::SubscriptionCap,
            reset_at: Some(t0() + chrono::Duration::days(23)),
            provider: ProviderId::new("claude"),
            raw: Some("subscription limit reached".into()),
        };
        let state = ProviderState::from_rate_limit(&rl);
        assert!(state.is_exhausted());

        let mut provider_states = BTreeMap::new();
        provider_states.insert(
            ProviderId::new("claude"),
            ProviderFleetState {
                provider: ProviderId::new("claude"),
                state,
                workers: vec![wid(1), wid(2)],
                affected_workers: vec![wid(1), wid(2)],
                parked_commands: vec![],
                resume_strategy: Some(ResumeStrategy::Stagger {
                    interval_secs: 5,
                    order: vec![wid(1), wid(2)],
                }),
            },
        );
        let workers = vec![
            worker(1, "claude", WorkerState::Idle { since: t0() }),
            worker(2, "claude", WorkerState::Idle { since: t0() }),
        ];
        let tasks = vec![task(1, 1), task(2, 2)];
        let hints: BTreeMap<TaskId, PriorityHints> = BTreeMap::new();
        let attempts = BTreeMap::new();
        let plan = plan_tick(&TickInputs {
            now: t0(),
            tasks: &tasks,
            workers: &workers,
            leases: &[],
            fleet_state: &FleetState::Normal,
            hints: &hints,
            attempts: &attempts,
            gates: &[],
            lease_secs: 600,
            wip_limit: 1,
            provider_states: &provider_states,
        });
        assert!(
            plan.assignments.is_empty(),
            "weeks-long exhaustion parks the fleet"
        );
        assert!(plan.stalls.is_empty());

        // The stagger arithmetic is horizon-agnostic: resume 23 days out is
        // the same protocol as resume 17 seconds out.
        let sched = resume_schedule(
            &[wid(1), wid(2)],
            t0() + chrono::Duration::days(23),
            chrono::Duration::seconds(5),
        );
        assert_eq!(
            sched[1].resume_at - sched[0].resume_at,
            chrono::Duration::seconds(5)
        );
    }

    // ---- the RR-0044b simulation (Invariant 22) ------------------------
    //
    // t=3 rate-limit, t=20 reset, staggered resume, all workers productive,
    // zero user interaction. Fixed timestamps; worker-state transitions are
    // exactly the ones the runtime performs from provider data + the clock
    // (park on observed limit, un-park at the resume_schedule slot) — no
    // step models a human.

    #[test]
    fn simulation_rate_limit_park_staggered_resume_all_productive() {
        let interval = chrono::Duration::seconds(DEFAULT_RESUME_STAGGER_SECS as i64);
        let reset = at(20);
        let mut workers = vec![
            worker(1, "claude", WorkerState::Idle { since: t0() }),
            worker(2, "claude", WorkerState::Idle { since: t0() }),
            worker(3, "claude", WorkerState::Idle { since: t0() }),
        ];
        // Phase A work exists from the start; phase B work arrives at t=3.
        let mut tasks = vec![task(1, 1), task(2, 2), task(3, 3)];
        let mut assigned_at: BTreeMap<TaskId, i64> = BTreeMap::new();
        let mut stalls_seen = 0usize;

        // The parked set and its staggered schedule, fixed at park time.
        let parked = [wid(1), wid(2), wid(3)];
        let schedule = resume_schedule(&parked, reset, interval);

        for t in 0..=30i64 {
            let now = at(t);
            match t {
                // t=3: worker 1's request hits the provider limit.
                3 => {
                    workers[0].state = WorkerState::RateLimited {
                        reset_at: Some(reset),
                    };
                    tasks.extend([task(4, 1), task(5, 2), task(6, 3)]);
                }
                // Workers 2 and 3 complete their current turns, then park
                // against the same window (shared subscription).
                4 => {
                    workers[1].state = WorkerState::RateLimited {
                        reset_at: Some(reset),
                    }
                }
                5 => {
                    workers[2].state = WorkerState::RateLimited {
                        reset_at: Some(reset),
                    }
                }
                _ => {}
            }
            // Staggered auto-resume: the RR-0072 loop un-parks a worker
            // when its schedule slot passes. No human in this transition.
            for s in &schedule {
                if now >= s.resume_at {
                    if let Some(w) = workers.iter_mut().find(|w| w.id() == &s.worker) {
                        if matches!(w.state, WorkerState::RateLimited { .. }) {
                            w.state = WorkerState::Idle { since: now };
                        }
                    }
                }
            }

            let plan = plan_at(now, &tasks, &workers);
            stalls_seen += plan.stalls.len();
            for asg in &plan.assignments {
                assigned_at.insert(asg.task.clone(), t);
                // "Execute": the task completes (terminal — a bare Done
                // would be Runnable again, since verification of an ungated
                // Done card is itself runnable work) and its worker returns
                // to idle before the next second's tick.
                let tk = tasks.iter_mut().find(|x| x.id == asg.task).unwrap();
                tk.status = TaskStatus::Verified;
            }

            // The fleet-pause window: from the first observed limit until
            // reset, NOTHING is assigned — including to workers still Idle.
            if (3..20).contains(&t) {
                assert!(
                    plan.assignments.is_empty(),
                    "t={t}: assignment during provider exhaustion: {:?}",
                    plan.assignments
                );
            }
        }

        // Phase A ran before the limit.
        assert_eq!(assigned_at[&tid(1)], 0);
        assert_eq!(assigned_at[&tid(2)], 0);
        assert_eq!(assigned_at[&tid(3)], 0);
        // Phase B drained on the staggered schedule: t=20, 25, 30 — no two
        // resumes inside the interval, no thundering herd at t=20.
        assert_eq!(assigned_at[&tid(4)], 20, "worker 1 resumes at reset");
        assert_eq!(
            assigned_at[&tid(5)],
            25,
            "worker 2 resumes one interval later"
        );
        assert_eq!(
            assigned_at[&tid(6)],
            30,
            "worker 3 resumes two intervals later"
        );
        // All workers productive again; zero interaction: no transition
        // above involved a human, and nothing ever surfaced as a stall
        // (a stall is the "needs attention" signal).
        assert_eq!(stalls_seen, 0, "zero stalls across the whole timeline");
    }

    // ---- serde ----------------------------------------------------------

    #[test]
    fn provider_state_and_strategy_serde_round_trip() {
        for s in [
            ProviderState::Available,
            ProviderState::QuotaExhausted {
                reset_at: Some(at(120)),
                kind: RateLimitKind::SubscriptionCap,
            },
            ProviderState::QuotaExhausted {
                reset_at: None,
                kind: RateLimitKind::Credit,
            },
            ProviderState::Unknown,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: ProviderState = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back);
        }
        for r in [
            ResumeStrategy::WaitForReset {
                reset_at: at(20),
                auto_resume: true,
            },
            ResumeStrategy::Redistribute {
                fallback: ProviderId::new("ollama"),
                workers_moved: vec![wid(1)],
            },
            ResumeStrategy::Stagger {
                interval_secs: 5,
                order: vec![wid(1), wid(2)],
            },
        ] {
            let json = serde_json::to_string(&r).unwrap();
            let back: ResumeStrategy = serde_json::from_str(&json).unwrap();
            assert_eq!(r, back);
        }
    }
}
