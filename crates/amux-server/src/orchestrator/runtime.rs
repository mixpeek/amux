//! Orchestrator runtime (RR-0041, Invariants 9, 10, 11) + FleetProgress
//! heartbeat (Lesson L4).
//!
//! Drives the pure planner (`amux_core::orchestrator::plan_tick`) on an
//! interval, executes its plan against the store, and reconciles DB state
//! against backend reality at startup. Task assembly arrives with the board
//! API (Phase 2, RR-0049) — until then the planner runs over an empty task
//! list, which still exercises lease reclaim and the heartbeat.

use crate::backend::{BackendStatus, SessionBackend};
use crate::db::{PendingEvent, SharedStore, WriteOutcome};
use amux_core::orchestrator::{plan_tick, Lease, TickInputs, TickPlan};
use amux_core::revision::{EntityType, MutationKind};
use amux_core::worker::Worker;
use chrono::{DateTime, Utc};
use rusqlite::params;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The L4 heartbeat: "is progress continuing?" answered as data, not
/// inferred from scrollback. Published as a StateEvent so SSE clients and
/// the dashboard status bar receive it like any other state change.
#[derive(Debug, Clone, Serialize)]
pub struct FleetProgress {
    pub at: DateTime<Utc>,
    pub workers_total: usize,
    pub workers_active: usize,
    pub live_leases: usize,
    pub reclaimed_last_tick: usize,
    pub stall_violations: usize,
    pub quarantined_total: u64,
}

/// Deterministic WorkerId for a Python-fleet owner name: matches no
/// registered worker by construction (epoch-0 timestamp + name hash), so
/// the planner can SEE the task without ever being able to assign it.
pub fn foreign_worker_id(name: &str) -> amux_core::ids::WorkerId {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.to_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    amux_core::ids::WorkerId::from_ulid(ulid::Ulid::from_parts(0, h as u128))
}

/// Hydrate every worker row into a core `Worker`. Shared by the tick loop
/// AND the /api/metrics/fleet provider view, so the dashboard's per-provider
/// picture is derived from the same rows by the same code as the mechanism
/// that parks workers — a view that re-derived its own filter would drift
/// (ethos rule 1: a view must share the predicate of the mechanism it
/// claims to describe).
pub fn hydrate_workers(conn: &rusqlite::Connection) -> anyhow::Result<Vec<Worker>> {
    // (offset, limit) — a swapped pair here silently loads ZERO workers,
    // which the recovery test caught: name the intent.
    let (offset, limit) = (0u64, 10_000u64);
    let (rows, _total) = crate::db::queries::list_workers(conn, offset, limit)?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let id = amux_core::ids::WorkerId::parse(&row.id).ok()?;
            let mut w = Worker::new(id, row.config(), Default::default());
            w.state = row.state.clone();
            w.version = row.version;
            Some(w)
        })
        .collect())
}

pub struct Runtime {
    pub store: SharedStore,
    pub backends: Vec<Arc<dyn SessionBackend>>,
    pub tick_secs: u64,
    /// Heartbeat cadence in ticks (heartbeat every Nth tick).
    pub heartbeat_every: u64,
    /// Fleet circuit breaker (RR-0048b, Invariant 48). The state lives here,
    /// in memory: a restart resets to Normal, and the first post-restart
    /// window re-trips if the condition persists — a breaker that survives
    /// its own process is a breaker nobody can reset.
    pub breaker: amux_core::circuit::FleetCircuitBreaker,
    pub fleet_state: std::sync::Mutex<amux_core::circuit::FleetState>,
    /// Agent protocol for command delivery (None until a transport is
    /// configured — the pump then leaves queues untouched rather than
    /// failing every command against a void).
    pub protocol: Option<Arc<dyn crate::opencode::AgentProtocol>>,
    /// STRANGLER-FIG SAFETY: may the Rust orchestrator pick up UNOWNED
    /// board tasks? Default false — while the Python fleet runs, an
    /// unowned card may be a Python session's next pickup, and two
    /// orchestrators claiming one queue is the dual-scheduler double-fire
    /// hazard on the board. Tasks owned by a name that resolves to a
    /// REGISTERED Rust worker are always eligible; tasks owned by
    /// unresolvable names (the Python fleet's) are NEVER touched.
    pub pickup_unowned: bool,
    /// RR-0044b thundering-herd prevention: seconds between successive
    /// un-parks of same-provider workers when a rate-limit reset passes
    /// (`AMUX_RS_RESUME_STAGGER_SECS`, default 5). Worker i in the
    /// deterministic (sorted-id) parked order becomes eligible at
    /// `reset_at + i * stagger`.
    pub resume_stagger_secs: u64,
}


/// True when a board status is outside the closed `TaskStatus` vocabulary.
///
/// Deliberately asks `parse_status` rather than keeping a second list — a
/// duplicated vocabulary is one that disagrees with itself the first time either
/// copy changes, which is the seam this repo keeps paying for.
fn amux_server_parse_status_is_unmodelled(raw: &str) -> bool {
    crate::db::board_store::parse_status(raw).is_none()
}


impl Runtime {
    /// Startup reconciliation (Invariant 9): the DB's picture of live
    /// sessions vs what each backend actually hosts. Every mismatch becomes
    /// a StateEvent — reported, never silently patched over.
    pub async fn reconcile_on_startup(&self) -> anyhow::Result<ReconcileReport> {
        let mut report = ReconcileReport::default();

        // What the backends actually host.
        let mut backend_refs: BTreeMap<String, BackendStatus> = BTreeMap::new();
        for b in &self.backends {
            match b.reconcile().await {
                Ok(sessions) => {
                    for s in sessions {
                        backend_refs.insert(s.backend_ref, s.status);
                    }
                }
                Err(e) => {
                    // A backend that cannot answer is reported, not skipped
                    // silently — its sessions would all read as "missing"
                    // and mass-ending them on a flaky probe would be the
                    // reaper incident all over again.
                    report.backend_probe_failures.push(format!("{}: {e}", b.name()));
                }
            }
        }
        let probe_ok = report.backend_probe_failures.is_empty();

        // DB live sessions vs backend truth.
        let db_live: Vec<(String, String, String)> = {
            let conn = self.store.read()?;
            let mut stmt = conn.prepare(
                "SELECT id, worker_id, backend_ref FROM _amux_sessions WHERE ended_at IS NULL",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })?;
            rows.collect::<Result<_, _>>()?
        };

        for (session_id, worker_id, backend_ref) in db_live {
            let live_in_backend = matches!(
                backend_refs.get(&backend_ref),
                Some(BackendStatus::Running)
            );
            if !live_in_backend && probe_ok {
                // DB says running, backend says gone -> mark interrupted.
                report.interrupted.push(worker_id.clone());
                let sid = session_id.clone();
                self.store
                    .write_async(move |conn| {
                        conn.execute(
                            "UPDATE _amux_sessions SET ended_at = ?1, exit_reason = ?2
                             WHERE id = ?3 AND ended_at IS NULL",
                            params![
                                Utc::now().to_rfc3339(),
                                serde_json::json!({"reason": "crashed", "signal": null})
                                    .to_string(),
                                sid
                            ],
                        )?;
                        Ok(WriteOutcome {
                            applied: true,
                            events: vec![PendingEvent {
                                entity_type: EntityType::Session,
                                entity_id: session_id.clone(),
                                mutation: MutationKind::StatusChanged {
                                    from: "running".into(),
                                    to: "interrupted".into(),
                                },
                                payload: None,
                            }],
                        })
                    })
                    .await?;
            }
        }

        // Backend hosts an amux ref the DB has no live row for -> stale
        // process, reported for a human/next phase to adopt or kill (ethos
        // rule 8: it may be someone's live work — never auto-kill on sight).
        let db_refs: std::collections::BTreeSet<String> = {
            let conn = self.store.read()?;
            let mut stmt = conn
                .prepare("SELECT backend_ref FROM _amux_sessions WHERE ended_at IS NULL")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for (bref, status) in &backend_refs {
            if matches!(status, BackendStatus::Running) && !db_refs.contains(bref) {
                report.stale_backend.push(bref.clone());
            }
        }

        Ok(report)
    }

    /// The tick loop. Runs forever; errors are logged and the loop
    /// continues — a failed tick must not kill the orchestrator.
    pub async fn run(self: Arc<Self>) {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(self.tick_secs.max(1)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut tick_n: u64 = 0;
        loop {
            interval.tick().await;
            crate::runtime_jobs::registry::tick(crate::runtime_jobs::registry::ids::ORCH_RUNTIME);
            tick_n += 1;
            let heartbeat = tick_n.is_multiple_of(self.heartbeat_every.max(1));
            if let Err(e) = self.tick_once(heartbeat).await {
                tracing::warn!(error = %e, "orchestrator tick failed");
            }
        }
    }

    /// One tick: load state, evaluate the circuit breaker, plan, execute.
    pub async fn tick_once(&self, heartbeat: bool) -> anyhow::Result<()> {
        let now = Utc::now();
        let (workers, leases, quarantined_total) = self.load_state()?;

        // Circuit breaker (RR-0048b): evaluate the rolling window BEFORE
        // planning. While open, reconciliation looks for runnable work and
        // auto-closes when it finds the fleet can actually move again.
        let window = self.window_stats(now)?;
        // Evaluate under the lock WITHOUT awaiting (the guard is not Send);
        // publish the change after the guard drops.
        let (fleet_state, changed) = {
            let mut fs = self.fleet_state.lock().unwrap();
            let mut changed = None;
            match &*fs {
                amux_core::circuit::FleetState::Normal => {
                    if let Some(tripped) = self.breaker.trip(&window, now) {
                        tracing::warn!(state = ?tripped, "fleet circuit OPENED");
                        *fs = tripped.clone();
                        changed = Some(tripped);
                    }
                }
                _ => {
                    // Open/reconciling: a healthy window is the auto-close
                    // signal — the fleet demonstrably moves again.
                    if self.breaker.evaluate(&window).is_none() {
                        if let Some(closed) = fs.close() {
                            tracing::info!("fleet circuit CLOSED (window healthy)");
                            *fs = closed.clone();
                            changed = Some(closed);
                        }
                    }
                }
            }
            (fs.clone(), changed)
        };
        if let Some(state) = &changed {
            self.publish_fleet_state(state).await.ok();
        }

        // Fleet-wide provider coordination (RR-0044b): ONE derivation per
        // tick from worker states — the same map feeds the pump gate, the
        // planner, and the redistribute recommendation, so no two consumers
        // can disagree about which provider is exhausted (ethos rule 4).
        let provider_states =
            amux_core::provider_fleet::derive(&workers, now, self.resume_stagger_secs);
        if let Err(e) = self.recommend_redistribute(&provider_states).await {
            tracing::warn!(error = %e, "redistribute recommendation failed this tick");
        }

        // Command delivery pump (Invariant 34): drain each worker's queue
        // head through the agent protocol, honoring DeliveryTiming.
        if let Err(e) = self.pump_commands(now, &provider_states).await {
            tracing::warn!(error = %e, "command pump failed this tick");
        }

        // RR-0072 + RR-0044b: STAGGERED rate-limit recovery. A worker whose
        // reset instant has passed returns to Idle automatically — but not
        // all at once: parked worker i (sorted-id order per provider)
        // becomes eligible at reset_at + i*stagger, so a 20-worker fleet
        // does not fire 20 simultaneous first requests at the provider that
        // just un-throttled it (thundering herd). Workers rate-limited with
        // NO reset time stay parked until an event or a human moves them:
        // inventing a retry time for an unknown window would be guessing
        // (Invariant 20), and a Credit cap clears on payment, not a clock.
        let stagger = chrono::Duration::seconds(self.resume_stagger_secs.min(i64::MAX as u64) as i64);
        let mut parked_by_provider: BTreeMap<
            amux_core::provider::ProviderId,
            Vec<(&Worker, DateTime<Utc>)>,
        > = BTreeMap::new();
        for w in &workers {
            if let amux_core::worker::WorkerState::RateLimited { reset_at: Some(reset) } = &w.state {
                parked_by_provider
                    .entry(w.config.provider.clone())
                    .or_default()
                    .push((w, *reset));
            }
        }
        for (_, parked) in parked_by_provider {
            let order: Vec<amux_core::ids::WorkerId> =
                parked.iter().map(|(w, _)| w.id().clone()).collect();
            for (w, reset) in parked {
                // Slot arithmetic through the ONE implementation (core's
                // resume_schedule, anchored at this worker's own reset so a
                // solo limit recovers exactly at its reset, index 0).
                let eligible_at = amux_core::provider_fleet::resume_schedule(&order, reset, stagger)
                    .into_iter()
                    .find(|s| &s.worker == w.id())
                    .map(|s| s.resume_at)
                    .unwrap_or(reset);
                if now < eligible_at {
                    continue;
                }
                let wid = w.id().to_string();
                self.store
                    .write_async(move |conn| {
                        let n = crate::db::queries::update_worker_state(
                            conn,
                            &wid,
                            &amux_core::worker::WorkerState::Idle { since: Utc::now() },
                            &Utc::now().to_rfc3339(),
                        )?;
                        // Post-mutation snapshot for the journal (RR-0111a):
                        // one indexed read inside the same transaction. A
                        // payload-less Worker event would push the replay
                        // horizon forward, leaving this worker's replayed
                        // state permanently unknown.
                        let payload = if n > 0 {
                            crate::db::queries::get_worker(conn, &wid)?.map(|r| r.snapshot())
                        } else {
                            None
                        };
                        Ok(WriteOutcome {
                            applied: n > 0,
                            events: if n > 0 {
                                vec![PendingEvent {
                                    entity_type: EntityType::Worker,
                                    entity_id: wid.clone(),
                                    mutation: MutationKind::StatusChanged {
                                        from: "rate_limited".into(),
                                        to: "idle".into(),
                                    },
                                    payload,
                                }]
                            } else {
                                vec![]
                            },
                        })
                    })
                    .await?;
                tracing::info!(worker = %w.id(), "rate limit reset passed — worker recovered (staggered)");
            }
        }

        let tasks = self.load_board_tasks(&workers)?;
        let hints = BTreeMap::new();
        // The attempt ledger (Invariant 49) feeds BOTH the planner (so
        // attempt N+1's prompt carries why 1..N failed) and enforce_limits
        // (so exhaustion can actually fire). This was an empty map until the
        // 2026-08-09 adherence audit — which made the anti-livelock check
        // below a check that could not fail (ethos rule 7): every assignment
        // arrived with zero prior attempts, so no task could ever exhaust
        // its budget and quarantine never triggered.
        let attempts = self.load_attempts()?;

        let plan = plan_tick(&TickInputs {
            now,
            tasks: &tasks,
            workers: &workers,
            leases: &leases,
            fleet_state: &fleet_state,
            hints: &hints,
            attempts: &attempts,
            gates: &[],
            lease_secs: 600,
            wip_limit: 1,
            provider_states: &provider_states,
        });

        // Anti-livelock (RR-0048a): limits filter what actually executes.
        let (proceed, exhaustion) = amux_core::orchestrator::enforce_limits(
            plan.assignments.clone(),
            &amux_core::limits::ExecutionLimits::default(),
            now,
            &BTreeMap::new(),
        );
        let plan = TickPlan { assignments: proceed, ..plan };
        self.execute(&plan).await?;
        for action in exhaustion {
            self.apply_exhaustion(action, now).await?;
        }

        if heartbeat {
            let progress = FleetProgress {
                at: now,
                workers_total: workers.len(),
                workers_active: workers
                    .iter()
                    .filter(|w| {
                        matches!(w.state, amux_core::worker::WorkerState::Active { .. })
                    })
                    .count(),
                live_leases: leases.iter().filter(|l| !l.is_expired(now)).count(),
                reclaimed_last_tick: plan.reclaim.len(),
                stall_violations: plan.stalls.len(),
                quarantined_total,
            };
            let payload = serde_json::to_string(&progress)?;
            self.store
                .write_async(move |_conn| {
                    Ok(WriteOutcome {
                        applied: true,
                        events: vec![PendingEvent {
                            entity_type: EntityType::Other("fleet_progress".into()),
                            entity_id: payload.clone(),
                            mutation: MutationKind::Updated,
                            payload: None,
                        }],
                    })
                })
                .await?;
        }
        Ok(())
    }

    /// Rolling-window stats from the revisioned event journal — the same
    /// events every other consumer sees, so the breaker and the dashboard
    /// can never disagree about what happened (ethos rule 4).
    fn window_stats(&self, now: DateTime<Utc>) -> anyhow::Result<amux_core::circuit::WindowStats> {
        let conn = self.store.read()?;
        let cutoff = (now - chrono::Duration::seconds(self.breaker.window_secs as i64)).to_rfc3339();
        let completed: u32 = conn.query_row(
            r#"SELECT COUNT(*) FROM _amux_state_events WHERE at > ?1
             AND entity_type = 'task' AND mutation LIKE '%"to":"done"%'"#,
            params![cutoff], |r| r.get(0)).unwrap_or(0);
        let failures: u32 = conn.query_row(
            "SELECT COUNT(*) FROM _amux_state_events WHERE at > ?1
             AND mutation LIKE '%interrupted%'",
            params![cutoff], |r| r.get(0)).unwrap_or(0);
        Ok(amux_core::circuit::WindowStats {
            // Token accounting joins with the turn ledger (Phase 4); 0 keeps
            // the spend trip disabled rather than fed invented numbers.
            tokens_spent: 0,
            tasks_completed: completed,
            failures,
            all_items_blocked: false,
        })
    }

    async fn publish_fleet_state(&self, state: &amux_core::circuit::FleetState) -> anyhow::Result<()> {
        let payload = serde_json::to_string(state)?;
        self.store
            .write_async(move |_conn| {
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![PendingEvent {
                        entity_type: EntityType::Other("fleet_state".into()),
                        entity_id: payload.clone(),
                        mutation: MutationKind::Updated,
                        payload: None,
                    }],
                })
            })
            .await?;
        Ok(())
    }

    /// Open board tasks for planning. The FULL board rides in the slice —
    /// disposition()'s dependency lookup treats an absent row as unmet
    /// (an absent row proves nothing), so a todo-only slice made every
    /// satisfied dependency read as unsatisfied forever: a dependent card
    /// could never become runnable through the runtime. Caught by the
    /// RR-0081 golden scenario's tripwire.
    ///
    /// Ownership: resolvable names map to their WorkerId; a name that is
    /// NOT a Rust worker (the Python fleet's) maps to a deterministic
    /// FOREIGN id that matches no registered worker — the task stays
    /// visible for dependency lookup but can never be assigned or counted
    /// as a stall (strangler-fig safety, see `pickup_unowned`). Unowned
    /// tasks assign only under pickup_unowned.
    fn load_board_tasks(&self, workers: &[Worker]) -> anyhow::Result<Vec<amux_core::board::Task>> {
        let conn = self.store.read()?;
        let rows = crate::db::board_store::list_issues(
            &conn,
            &[], // ALL statuses: dependencies live in done/verified
            &[],
            crate::db::board_store::ArchivedFilter::ActiveOnly,
        )?;
        let mut names: BTreeMap<String, amux_core::ids::WorkerId> = BTreeMap::new();
        for w in workers {
            names.insert(w.config.display_name.to_lowercase(), w.id().clone());
            for a in &w.config.name_aliases {
                names.entry(a.to_lowercase()).or_insert_with(|| w.id().clone());
            }
        }
        let mut out = Vec::new();
        for row in rows {
            // `else { continue }` USED TO BE HERE, and it was the silent drop
            // (AMUX-2632): a card in an operator-created column vanished from
            // the orchestrator with no log, no wait state, and nothing on the
            // card. `to_task` now maps an unmodelled column to Blocked, so the
            // card is visible — but "visible as Blocked" without the column
            // name is a card that reads as a dependency wait and will be
            // debugged as one, so the raw status is named here where the row
            // still has it.
            let Some(mut task) = row.to_task() else { continue };
            if amux_server_parse_status_is_unmodelled(&row.status) {
                tracing::warn!(
                    card = %row.id,
                    column = %row.status,
                    "card sits in an unmodelled column — visible to the orchestrator as BLOCKED \
                     on configuration, not actionable until the column is modelled or the card \
                     is moved"
                );
            }
            match row.session.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(owner_name) => match names.get(&owner_name.to_lowercase()) {
                    Some(wid) => task.worker = Some(wid.clone()),
                    None => task.worker = Some(foreign_worker_id(owner_name)),
                },
                None => {
                    if !self.pickup_unowned && !task.status.is_terminal() {
                        // Unowned + pickup disabled: keep terminal rows for
                        // dependency lookup, drop assignable ones.
                        continue;
                    }
                }
            }
            out.push(task);
        }
        Ok(out)
    }

    /// Attempt history per task, newest last (the TickInputs contract) —
    /// read from the `_amux_attempts` ledger the event processor writes on
    /// every WorkerEvent::Failed. Unparseable records are skipped with a
    /// warning, never silently: a record that cannot feed forward is itself
    /// a diagnosis (ethos rule 4).
    fn load_attempts(
        &self,
    ) -> anyhow::Result<BTreeMap<amux_core::ids::TaskId, Vec<amux_core::limits::AttemptRecord>>>
    {
        let conn = self.store.read()?;
        let mut stmt = conn.prepare(
            "SELECT task_id, record FROM _amux_attempts ORDER BY at ASC, attempt ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out: BTreeMap<amux_core::ids::TaskId, Vec<amux_core::limits::AttemptRecord>> =
            BTreeMap::new();
        for row in rows {
            let (task_s, record_s) = row?;
            let Ok(task) = amux_core::ids::TaskId::parse(&task_s) else {
                tracing::warn!(task = %task_s, "attempt row with unparseable task id skipped");
                continue;
            };
            match serde_json::from_str::<amux_core::limits::AttemptRecord>(&record_s) {
                Ok(rec) => out.entry(task).or_default().push(rec),
                Err(e) => {
                    tracing::warn!(task = %task_s, error = %e, "unparseable attempt record skipped")
                }
            }
        }
        Ok(out)
    }

    fn load_state(&self) -> anyhow::Result<(Vec<Worker>, Vec<Lease>, u64)> {
        let conn = self.store.read()?;
        let workers = hydrate_workers(&conn)?;
        let mut stmt = conn.prepare(
            "SELECT task_id, worker_id, acquired_at, expires_at, generation FROM _amux_leases",
        )?;
        let leases: Vec<Lease> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, u64>(4)?,
                ))
            })?
            .filter_map(|row| {
                let (task, worker, acq, exp, generation) = row.ok()?;
                Some(Lease {
                    task: amux_core::ids::TaskId::parse(&task).ok()?,
                    worker: amux_core::ids::WorkerId::parse(&worker).ok()?,
                    acquired_at: acq.parse().ok()?,
                    expires_at: exp.parse().ok()?,
                    generation,
                })
            })
            .collect();
        let quarantined: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM issues WHERE status = 'quarantined'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok((workers, leases, quarantined))
    }

    async fn execute(&self, plan: &TickPlan) -> anyhow::Result<()> {
        // Reclaim expired leases: delete the row, bump generation via the
        // task's next lease. Each reclaim is an event — a lease that
        // vanishes silently is a diagnosis that can't be made (ethos 4).
        for lease in &plan.reclaim {
            let task = lease.task.to_string();
            let worker = lease.worker.to_string();
            let generation = lease.generation;
            self.store
                .write_async(move |conn| {
                    let n = conn.execute(
                        "DELETE FROM _amux_leases WHERE task_id = ?1 AND generation = ?2",
                        params![task, generation],
                    )?;
                    Ok(WriteOutcome {
                        applied: n > 0,
                        events: if n > 0 {
                            vec![PendingEvent {
                                entity_type: EntityType::Other("lease".into()),
                                entity_id: task.clone(),
                                mutation: MutationKind::StatusChanged {
                                    from: format!("held:{worker}"),
                                    to: "reclaimed".into(),
                                },
                                payload: None,
                            }]
                        } else {
                            vec![]
                        },
                    })
                })
                .await?;
        }
        // Immutable context snapshots (RR-0070, Invariant 27): record exactly
        // what each assigned worker will receive, BEFORE the command that
        // delivers it is enqueued. INSERT OR IGNORE on the assignment's
        // idempotency key keeps this idempotent — a re-planned assignment
        // re-records nothing and bumps no revision. A task the board no
        // longer resolves is skipped as an honest no-op (the assignment
        // itself will fail downstream and say so there).
        for asg in &plan.assignments {
            let worker_id = asg.worker.clone();
            let task_id = asg.task.clone();
            let key = asg.idempotency_key.clone();
            self.store
                .write_async(move |conn| {
                    let Some(task) =
                        crate::orchestrator::context::task_by_internal_id(conn, &task_id)?
                    else {
                        return Ok(WriteOutcome { applied: false, events: vec![] });
                    };
                    let snap = crate::orchestrator::context::assemble_context(
                        conn, &worker_id, &task,
                    )?;
                    let recorded = crate::orchestrator::context::record_snapshot(
                        conn, &key, &task_id, &worker_id, &snap,
                    )?;
                    Ok(WriteOutcome {
                        applied: recorded,
                        events: if recorded {
                            vec![PendingEvent {
                                entity_type: EntityType::Other("context_snapshot".into()),
                                entity_id: snap.content_hash.clone(),
                                mutation: MutationKind::Created,
                                payload: None,
                            }]
                        } else {
                            vec![]
                        },
                    })
                })
                .await?;
        }
        // Assignments: lease + an ExecuteTask command the pump delivers
        // through the agent protocol — the full loop: board task -> lease
        // -> command -> headless CLI run.
        for asg in &plan.assignments {
            let cmd_id = amux_core::ids::CommandId::from_ulid(ulid::Ulid::new());
            let worker_id = asg.worker.clone();
            let task_id = asg.task.clone();
            let key = asg.idempotency_key.clone();
            self.store
                .write_async(move |conn| {
                    let (_, created) = crate::db::commands::enqueue(
                        conn,
                        cmd_id,
                        &worker_id,
                        &amux_core::protocol::WorkerCommand::ExecuteTask(task_id),
                        &key,
                        &amux_core::protocol::DeliveryTiming::WhenIdle,
                        None,
                        Utc::now(),
                    )?;
                    Ok(WriteOutcome { applied: created, events: vec![] })
                })
                .await?;
        }
        for asg in &plan.assignments {
            let task = asg.task.to_string();
            let worker = asg.worker.to_string();
            let acquired = asg.lease.acquired_at.to_rfc3339();
            let expires = asg.lease.expires_at.to_rfc3339();
            self.store
                .write_async(move |conn| {
                    let n = conn.execute(
                        // INSERT OR IGNORE: the primary key on task_id is the
                        // atomic claim — a concurrent claimant loses cleanly.
                        "INSERT OR IGNORE INTO _amux_leases
                         (task_id, worker_id, acquired_at, expires_at, generation)
                         VALUES (?1, ?2, ?3, ?4,
                                 COALESCE((SELECT generation + 1 FROM _amux_leases WHERE task_id = ?1), 0))",
                        params![task, worker, acquired, expires],
                    )?;
                    Ok(WriteOutcome {
                        applied: n > 0,
                        events: if n > 0 {
                            vec![PendingEvent {
                                entity_type: EntityType::Other("lease".into()),
                                entity_id: task.clone(),
                                mutation: MutationKind::Created,
                                payload: None,
                            }]
                        } else {
                            vec![]
                        },
                    })
                })
                .await?;
        }
        Ok(())
    }
}

impl Runtime {
    /// Execute an anti-livelock decision (RR-0048a). Quarantine writes the
    /// terminal status onto the ISSUES row via the same board-store path
    /// the API uses — the docstring claimed this since RR-0048a while the
    /// body only recorded an event (the ethos-rule-6 shape: an audit trail
    /// claimed but not implemented); the 2026-08-09 adherence audit made it
    /// true. Decompose is recorded as a StateEvent for the decomposition
    /// worker to consume (model-driven splitting is a later phase —
    /// recording without acting keeps the decision auditable now).
    ///
    /// Both variants dedupe DURABLY on the serialized action: enforce_limits
    /// re-derives the same verdict every tick for a still-exhausted task, and
    /// re-recording it each tick would be a journal of the loop, not a
    /// decision (ethos rule 5). Quarantine self-limits anyway (the row goes
    /// terminal), so the dedupe mostly guards Decompose.
    async fn apply_exhaustion(
        &self,
        action: amux_core::orchestrator::ExhaustionAction,
        now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let payload = serde_json::to_string(&action)?;
        let already = {
            let conn = self.store.read()?;
            conn.query_row(
                "SELECT COUNT(*) FROM _amux_state_events
                 WHERE entity_type = 'exhaustion' AND entity_id = ?1",
                params![payload],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
                > 0
        };
        if already {
            return Ok(());
        }
        let exhaustion_event = PendingEvent {
            entity_type: EntityType::Other("exhaustion".into()),
            entity_id: payload.clone(),
            mutation: MutationKind::Created,
            payload: None,
        };
        match action {
            amux_core::orchestrator::ExhaustionAction::Quarantine { task, reason } => {
                self.store
                    .write_async(move |conn| {
                        let Some(row) =
                            crate::orchestrator::context::issue_by_internal_id(conn, &task)?
                        else {
                            // Card gone: record the decision (auditable),
                            // there is no row to move.
                            tracing::warn!(task = %task,
                                "quarantine verdict for a task the board no longer resolves");
                            return Ok(WriteOutcome {
                                applied: true,
                                events: vec![exhaustion_event],
                            });
                        };
                        let Some(core_task) = row.to_task() else {
                            tracing::warn!(card = %row.id,
                                "quarantine verdict for a row outside the shared status vocabulary");
                            return Ok(WriteOutcome { applied: true, events: vec![exhaustion_event] });
                        };
                        // Through the ONE transition path (Invariant 3), so
                        // an already-terminal card is a refused no-op here,
                        // never a silent overwrite.
                        let actor = amux_core::events::Actor::System {
                            component: "orchestrator".into(),
                        };
                        match amux_core::board::apply_transition(
                            &core_task,
                            amux_core::board::BoardTransition::Quarantine {
                                reason: reason.clone(),
                            },
                            &actor,
                            &[],
                            now,
                        ) {
                            Ok(updated) => {
                                let mut next = row;
                                let from_raw = next.status.clone();
                                let target_raw = crate::db::board_store::status_to_db(
                                    amux_core::board::TaskStatus::Quarantined,
                                    &next.status,
                                );
                                let stamp =
                                    chrono::Local::now().format("%H:%M").to_string();
                                next.log = Some(crate::db::board_store::append_log(
                                    next.log.as_deref(),
                                    &stamp,
                                    &format!(
                                        "orchestrator: quarantined ({from_raw} -> {target_raw}) — {reason}"
                                    ),
                                ));
                                next.status = target_raw.clone();
                                next.rev += 1;
                                next.version =
                                    i64::try_from(updated.version).unwrap_or(next.version + 1);
                                next.updated = now.timestamp();
                                crate::db::board_store::save_patched(conn, &mut next)?;
                                Ok(WriteOutcome {
                                    applied: true,
                                    events: vec![
                                        exhaustion_event,
                                        PendingEvent {
                                            entity_type: EntityType::Task,
                                            entity_id: next.id.clone(),
                                            mutation: MutationKind::StatusChanged {
                                                from: from_raw,
                                                to: target_raw,
                                            },
                                            payload: Some(next.snapshot()),
                                        },
                                    ],
                                })
                            }
                            Err(e) => {
                                // Refused (already terminal, archived...):
                                // record the decision, leave the row alone.
                                tracing::warn!(card = %row.id, error = %e,
                                    "quarantine transition refused; recording the verdict only");
                                Ok(WriteOutcome { applied: true, events: vec![exhaustion_event] })
                            }
                        }
                    })
                    .await?;
            }
            amux_core::orchestrator::ExhaustionAction::Decompose { .. } => {
                self.store
                    .write_async(move |_conn| {
                        Ok(WriteOutcome {
                            applied: true,
                            events: vec![exhaustion_event],
                        })
                    })
                    .await?;
            }
        }
        Ok(())
    }

    /// RR-0044b step 3, as a RECOMMENDATION: when a provider is exhausted,
    /// publish `provider_redistribute_recommended` so a policy layer (or
    /// the human) can move workers to a fallback via routing.rs. amux never
    /// applies the redistribution itself — the configured provider is the
    /// user's decision, and routing.rs's own rule is "never silently swap
    /// the configured provider" (ethos rule 8).
    ///
    /// Deduped DURABLY on (provider, reset_at): the journal is the memory,
    /// so a restart re-emits nothing for an episode already announced
    /// (in-memory dedupe state would be fiction across the re-exec). A new
    /// episode carries a new reset_at and announces itself again.
    async fn recommend_redistribute(
        &self,
        provider_states: &BTreeMap<
            amux_core::provider::ProviderId,
            amux_core::provider_fleet::ProviderFleetState,
        >,
    ) -> anyhow::Result<()> {
        for (pid, pstate) in provider_states {
            let amux_core::provider_fleet::ProviderState::QuotaExhausted { reset_at, kind } =
                &pstate.state
            else {
                continue;
            };
            // Stable episode key — parked COUNT is deliberately excluded
            // (it grows as siblings park, and each growth would re-fire).
            let key = serde_json::json!({
                "provider": pid.as_str(),
                "reset_at": reset_at.map(|r| r.to_rfc3339()),
                "kind": kind,
            })
            .to_string();
            // STORAGE FORMAT, resolved 2026-08-09: the writer (db/mod.rs
            // apply_write) stores the BARE tag; the serde object form
            // (`{"kind":"other","data":...}`) was an accident that left
            // every `entity_type = '<tag>'` filter matching nothing (this
            // dedupe, /api/metrics/fleet's `last()`, window_stats). Rows
            // written before the fix may still carry the object form, so
            // the dedupe reads BOTH — an old-format episode row must still
            // suppress re-announcement.
            let etype_legacy = serde_json::to_string(&EntityType::Other(
                "provider_redistribute_recommended".into(),
            ))?;
            let already = {
                let conn = self.store.read()?;
                conn.query_row(
                    "SELECT COUNT(*) FROM _amux_state_events
                     WHERE entity_type IN ('provider_redistribute_recommended', ?1)
                       AND entity_id = ?2",
                    params![etype_legacy, key],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0)
                    > 0
            };
            if already {
                continue;
            }
            tracing::warn!(provider = %pid, workers_parked = pstate.affected_workers.len(),
                "provider exhausted — recommending redistribution to a fallback (never applied automatically)");
            self.store
                .write_async(move |_conn| {
                    Ok(WriteOutcome {
                        applied: true,
                        events: vec![PendingEvent {
                            entity_type: EntityType::Other(
                                "provider_redistribute_recommended".into(),
                            ),
                            entity_id: key.clone(),
                            mutation: MutationKind::Created,
                            payload: None,
                        }],
                    })
                })
                .await?;
        }
        Ok(())
    }

    /// Deliver due commands through the agent protocol. One in-flight
    /// command per worker (queue discipline lives in db::commands); timing:
    /// Immediate always goes; AtTurnBoundary/WhenIdle require the agent to
    /// be at a boundary (Idle or WaitingForInput). Failures transition
    /// through the core state machine — retry budget 3 (Invariant 34).
    ///
    /// `provider_states` (RR-0044b): NO delivery to a worker on an
    /// exhausted provider, even `Immediate` — the worker may look idle, but
    /// the provider knows first, and delivering would spend the command's
    /// retry budget thrashing against a limit the fleet already knows
    /// about. The command simply stays Queued: the queue IS the park, and
    /// nothing is lost (lifecycle step 2: "commands queue, not lost").
    pub(crate) async fn pump_commands(
        &self,
        now: DateTime<Utc>,
        provider_states: &BTreeMap<
            amux_core::provider::ProviderId,
            amux_core::provider_fleet::ProviderFleetState,
        >,
    ) -> anyhow::Result<()> {
        let Some(protocol) = &self.protocol else {
            return Ok(());
        };
        let worker_ids: Vec<String> = {
            let conn = self.store.read()?;
            let mut stmt = conn.prepare(
                "SELECT DISTINCT worker_id FROM _amux_commands
                 WHERE state LIKE '%queued%' OR state LIKE '%dispatched%' OR state LIKE '%delivered%'",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for wid_str in worker_ids {
            let Ok(worker) = amux_core::ids::WorkerId::parse(&wid_str) else {
                continue;
            };
            // Fleet pause gate (RR-0044b) — sits BEFORE the timing gate so
            // even Immediate commands park while the provider is exhausted.
            if amux_core::provider_fleet::worker_on_exhausted_provider(provider_states, &worker) {
                continue;
            }
            let head = {
                let conn = self.store.read()?;
                crate::db::commands::next_deliverable(&conn, &worker)?
            };
            let Some(cmd) = head else { continue };

            // Timing gate.
            let due = match cmd.timing {
                amux_core::protocol::DeliveryTiming::Immediate => true,
                amux_core::protocol::DeliveryTiming::AtTurnBoundary
                | amux_core::protocol::DeliveryTiming::WhenIdle => matches!(
                    protocol.state(&worker).await,
                    Ok(crate::opencode::AgentState::Idle)
                        | Ok(crate::opencode::AgentState::WaitingForInput)
                ),
            };
            if !due {
                continue;
            }

            // Precondition gate (freshness at delivery, Invariant 38): a
            // command whose precondition no longer holds FAILS visibly
            // instead of firing against stale state.
            if let Some(pre) = &cmd.precondition {
                let holds = {
                    let conn = self.store.read()?;
                    let lookup = |entity: &str| -> Option<(u64, String)> {
                        conn.query_row(
                            "SELECT version, json_extract(state,'$.state') FROM _amux_workers WHERE id = ?1",
                            params![entity],
                            |r| Ok((r.get::<_, u64>(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default())),
                        )
                        .ok()
                    };
                    pre.evaluate(&lookup)
                };
                if !holds {
                    let id = cmd.id.clone();
                    self.store
                        .write_async(move |conn| {
                            crate::db::commands::transition(
                                conn,
                                &id,
                                amux_core::protocol::CommandTransition::Fail {
                                    reason: "precondition no longer holds at delivery".into(),
                                },
                                3,
                            )?;
                            Ok(WriteOutcome { applied: true, events: vec![] })
                        })
                        .await?;
                    continue;
                }
            }

            // Dispatch.
            let id = cmd.id.clone();
            self.store
                .write_async({
                    let id = id.clone();
                    move |conn| {
                        crate::db::commands::transition(
                            conn,
                            &id,
                            amux_core::protocol::CommandTransition::Dispatch,
                            3,
                        )?;
                        Ok(WriteOutcome { applied: true, events: vec![] })
                    }
                })
                .await?;
            // Kept past the match for the no-silent-work ledger capture
            // below: the delivered PROMPT is what a card is minted from.
            let mut delivered_body: Option<String> = None;
            let delivery = match &cmd.command {
                amux_core::protocol::WorkerCommand::DeliverMessage(msg_id) => {
                    // RR-0066: the command carries only the REFERENCE
                    // (Invariant 29); the durable body lives in
                    // _amux_messages and is resolved here, at delivery. A
                    // missing row is a delivery FAILURE, never an empty
                    // message — silently delivering "" would be the Python
                    // lost-steering-text bug wearing a Rust type.
                    let body: rusqlite::Result<String> = {
                        let conn = self.store.read()?;
                        conn.query_row(
                            "SELECT body FROM _amux_messages WHERE id = ?1",
                            params![msg_id.as_str()],
                            |r| r.get(0),
                        )
                    };
                    match body {
                        Ok(body) => {
                            delivered_body = Some(body.clone());
                            protocol.deliver_message(&worker, msg_id.clone(), body).await
                        }
                        Err(e) => Err(crate::opencode::ProtocolError::Transport(format!(
                            "message body lookup failed for {}: {e}",
                            msg_id.as_str()
                        ))),
                    }
                }
                amux_core::protocol::WorkerCommand::ExecuteTask(task_id) => {
                    // The MODEL needs the WORK, not a task id (the live
                    // golden bridged this with a CLAUDE.md briefing — the
                    // real fix). Build the feed-forward prompt from the
                    // board row + attempt history (Invariant 49).
                    let prompt_text = {
                        let conn = self.store.read()?;
                        let row = {
                            // internal id -> semantic row: scan open issues
                            // re-minting ids (the interop shim is one-way).
                            let rows = crate::db::board_store::list_issues(
                                &conn,
                                &[],
                                &[],
                                crate::db::board_store::ArchivedFilter::ActiveOnly,
                            )
                            .unwrap_or_default();
                            rows.into_iter().find(|r| {
                                crate::db::board_store::internal_id(&r.id) == *task_id
                            })
                        };
                        match row {
                            Some(r) => {
                                let asg = amux_core::orchestrator::WorkAssignment {
                                    task: task_id.clone(),
                                    worker: worker.clone(),
                                    attempt: cmd.attempts + 1,
                                    lease: amux_core::orchestrator::Lease {
                                        task: task_id.clone(),
                                        worker: worker.clone(),
                                        acquired_at: now,
                                        expires_at: now,
                                        generation: 0,
                                    },
                                    idempotency_key: cmd.idempotency_key.clone(),
                                    prior_attempts: vec![],
                                };
                                format!(
                                    "{}
(board card {} — move it through the board as you work)",
                                    amux_core::orchestrator::assignment_prompt(
                                        &r.title, &r.desc, &asg
                                    ),
                                    r.id
                                )
                            }
                            None => serde_json::to_string(&cmd.command).unwrap_or_default(),
                        }
                    };
                    protocol
                        .send_prompt(
                            &worker,
                            crate::opencode::Prompt {
                                text: prompt_text,
                                idempotency_key: cmd.idempotency_key.clone(),
                            },
                        )
                        .await
                }
                other => {
                    protocol
                        .send_prompt(
                            &worker,
                            crate::opencode::Prompt {
                                text: serde_json::to_string(other).unwrap_or_default(),
                                idempotency_key: cmd.idempotency_key.clone(),
                            },
                        )
                        .await
                }
            };
            // On successful message delivery, advance the MESSAGE's own
            // delivery state too (Invariant 29: the message is the durable
            // entity — its state must reflect what actually happened, not
            // just the command queue's view). Flagged by the Phase 4 agent
            // as the one gap it could not close from outside runtime.rs.
            if delivery.is_ok() {
                if let amux_core::protocol::WorkerCommand::DeliverMessage(msg_id) = &cmd.command {
                    // NO SILENT WORK (2026-08-09 adherence audit; Python
                    // `_autotask_from_command` parity): a prompt delivered
                    // straight to a worker that holds no open card is work
                    // the board cannot see. Mint the ledger card at the
                    // moment the worker actually receives the prompt.
                    if let Some(body) = delivered_body.take() {
                        if let Err(e) = self.capture_prompt_card(&worker, &body, now).await {
                            tracing::warn!(worker = %worker, error = %e,
                                "ledger capture failed for a delivered prompt");
                        }
                    }
                    let mid = msg_id.to_string();
                    let delivered = serde_json::json!({"state": "delivered", "at": now.to_rfc3339()});
                    self.store
                        .write_async(move |conn| {
                            let n = conn.execute(
                                "UPDATE _amux_messages SET delivery = ?2
                                 WHERE id = ?1 AND delivery LIKE '%queued%'",
                                params![mid, delivered.to_string()],
                            )?;
                            Ok(WriteOutcome {
                                applied: n > 0,
                                events: if n > 0 {
                                    vec![PendingEvent {
                                        entity_type: EntityType::Message,
                                        entity_id: mid.clone(),
                                        mutation: MutationKind::StatusChanged {
                                            from: "queued".into(),
                                            to: "delivered".into(),
                                        },
                                        payload: None,
                                    }]
                                } else {
                                    vec![]
                                },
                            })
                        })
                        .await?;
                }
            }
            let t = match delivery {
                Ok(()) => amux_core::protocol::CommandTransition::Deliver,
                Err(e) => amux_core::protocol::CommandTransition::Fail {
                    reason: format!("delivery failed at {now}: {e}"),
                },
            };
            self.store
                .write_async(move |conn| {
                    let cmd = crate::db::commands::transition(conn, &id, t, 3)?;
                    // Dead-letter handling (RR-0068, Invariant 34): a Fail
                    // that spends the whole retry budget is retried into its
                    // terminal state HERE, by the caller that observed it —
                    // and the DurableEvent is emitted in the same
                    // transaction, because a dead letter without a trace is
                    // the Python system's silent-vanish mode with extra
                    // steps. transition() returns the post-Fail state, so
                    // this cannot fire on a command with budget left.
                    let mut events = vec![];
                    if matches!(&cmd.state, amux_core::protocol::CommandState::Failed { .. })
                        && cmd.attempts >= 3
                    {
                        let dead = crate::db::commands::transition(
                            conn,
                            &id,
                            amux_core::protocol::CommandTransition::Retry,
                            3,
                        )?;
                        if let amux_core::protocol::CommandState::DeadLettered { reason } =
                            &dead.state
                        {
                            tracing::warn!(command = id.as_str(), reason = %reason,
                                "command dead-lettered after exhausting retries");
                            events.push(PendingEvent {
                                entity_type: EntityType::Other("command_dead_letter".into()),
                                entity_id: id.as_str().to_string(),
                                mutation: MutationKind::StatusChanged {
                                    from: "failed".into(),
                                    to: format!("dead_lettered: {reason}"),
                                },
                                payload: None,
                            });
                        }
                    }
                    Ok(WriteOutcome { applied: true, events })
                })
                .await?;
        }
        Ok(())
    }
    /// The ledger duty for DIRECT prompt delivery (mechanism: no silent
    /// work). The Python board auto-captures every human command as a card;
    /// the Rust messages path delivered prompts with no board trace at all
    /// until the 2026-08-09 adherence audit. Minimal honest version:
    ///
    /// - title is COMPUTED from the prompt's own first clause
    ///   (`amux_core::board::title_from_prompt`) — never a model call
    ///   (ethos rule 2); control words / `[no-board]` / bare slash commands
    ///   return None and mint nothing;
    /// - a worker with an OPEN card (non-terminal, agent-owned) gets no new
    ///   card — the prompt is steering the work in flight (folding beyond
    ///   that is the Python system's job; a second heuristic here would
    ///   drift from it);
    /// - the card lands in `doing`, attributed to the worker's display name,
    ///   with the durable `capture: session prompt` log marker (the same
    ///   marker Python's dedupe and not-a-task guards key on), and
    ///   `notified=1` — the worker already RECEIVED this prompt as its live
    ///   command; the card is the ledger of it, not news (re-announcing a
    ///   stale capture was a real Python incident).
    ///
    ///   `doing`, NOT `todo` (AMUX-2613 E2E finding, 2026-08-09): an
    ///   owned `todo` card is Runnable to the planner (deliberately — the
    ///   L3 shape), so a `todo` ledger card was picked up on the next tick
    ///   and its prompt REDELIVERED as an ExecuteTask assignment — every
    ///   direct prompt ran twice (observed live: the claude transcript
    ///   carried the same pomegranate prompt twice, once raw, once wrapped
    ///   in "Task tsk_…"). `doing` + owner is `Assigned` — never
    ///   re-dispatched — and if the turn ends with the card unmoved, that
    ///   is the stall detector's designed drift cell, which is the honest
    ///   ledger state for "prompt handled, card never closed".
    async fn capture_prompt_card(
        &self,
        worker: &amux_core::ids::WorkerId,
        body: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        // Redact secret shapes before the prompt reaches the fleet-readable board
        // — title AND desc both derive from `body` (AMUX-3384). Same helper the
        // send-path capture uses, so the two sites cannot drift on what leaks.
        let redacted = crate::api::session_verbs::redact_prompt_secrets(body);
        let body = redacted.as_str();
        let Some(title) = amux_core::board::title_from_prompt(body) else {
            return Ok(()); // steering, not a task
        };
        // AMUX-2604: a prompt is spoken INTO a context capture cannot see, so
        // "This should be one row" mints a card no one can dispatch later. The
        // check is COMPUTED here (never a model call — ethos rule 2) and the
        // REWRITE is asked of the worker at its next turn boundary, because
        // the worker is the only party that ever held the missing referent.
        let needs_self_desc = amux_core::board::title_needs_self_description(&title);
        let wid = worker.to_string();
        let body = body.to_string();
        // Carries (card id, session name) out of the writer so the nudge can
        // be addressed AFTER the card exists — the consequence hangs off the
        // write that already happens, with a named consumer and a durable
        // dedupe key, rather than a new bus (CLAUDE.md's recorded decision).
        let minted: std::sync::Arc<std::sync::Mutex<Option<(String, String)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let minted_w = minted.clone();
        self.store
            .write_async(move |conn| {
                let Some(wrow) = crate::db::queries::get_worker(conn, &wid)? else {
                    // A card must be attributable; a worker the store cannot
                    // name gets no invented attribution (Invariant 20).
                    tracing::warn!(worker = %wid,
                        "prompt delivered to a worker with no store row; no ledger card minted");
                    return Ok(WriteOutcome { applied: false, events: vec![] });
                };
                let name = wrow.display_name;
                if name.trim().is_empty() {
                    tracing::warn!(worker = %wid,
                        "worker has no display name; no ledger card minted");
                    return Ok(WriteOutcome { applied: false, events: vec![] });
                }
                let open: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM issues WHERE session = ?1 AND deleted IS NULL
                     AND owner_type = 'agent'
                     AND status NOT IN ('done','verified','discarded')",
                    params![name],
                    |r| r.get(0),
                )?;
                if open > 0 {
                    return Ok(WriteOutcome { applied: false, events: vec![] });
                }
                let desc_body: String = body.chars().take(300).collect();
                let mut row = crate::db::board_store::create_issue(
                    conn,
                    &crate::db::board_store::NewIssue {
                        title,
                        desc: format!("**Prompt:** {desc_body}"),
                        // In flight, not queued: see the doc comment — a
                        // `todo` mint was re-dispatched by the planner,
                        // double-running every direct prompt.
                        status: "doing".into(),
                        session: Some(name.clone()),
                        item_type: "code".into(),
                        creator: "amux".into(),
                        owner_type: "agent".into(),
                        due: None,
                        due_time: None,
                        reviewer: None,
                        shepherd: None,
                        gate: vec![],
                        depends_on: vec![],
                        // The tag is the durable half of the flag: the steer
                        // below is delivered once and consumed, but a card
                        // whose title was never repaired stays findable by
                        // anyone querying the board (`needs-self-description`)
                        // — a nudge with no residue is a nudge that silently
                        // did not happen (ethos rule 4).
                        tags: if needs_self_desc.is_some() {
                            vec!["needs-self-description".to_string()]
                        } else {
                            vec![]
                        },
                    },
                    now.timestamp(),
                )?;
                let stamp = chrono::Local::now().format("%H:%M").to_string();
                row.log = Some(crate::db::board_store::append_log(
                    row.log.as_deref(),
                    &stamp,
                    "capture: session prompt",
                ));
                if let Some(reason) = needs_self_desc {
                    row.log = Some(crate::db::board_store::append_log(
                        row.log.as_deref(),
                        &stamp,
                        &format!("capture: title needs self-description — {reason}"),
                    ));
                    *minted_w.lock().unwrap() = Some((row.id.clone(), name));
                }
                crate::db::board_store::save_patched(conn, &mut row)?;
                // notified is deliberately outside save_patched's SET list
                // (a Python-owned column); set it here so the assignment
                // notifier never re-announces a prompt the worker already
                // has in hand.
                conn.execute(
                    "UPDATE issues SET notified = 1 WHERE id = ?1",
                    params![row.id],
                )?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![PendingEvent {
                        entity_type: EntityType::Task,
                        entity_id: row.id.clone(),
                        mutation: MutationKind::Created,
                        payload: Some(row.snapshot()),
                    }],
                })
            })
            .await?;

        // The consequence, hung off the write that already happened and
        // addressed to a NAMED consumer: the worker that received the prompt.
        //
        // Delivery is `steer_enqueue`, the existing path, never a direct send
        // — the steering loop applies the turn-boundary gate, so this cannot
        // land mid-turn, and it arrives exactly when the worker has finished
        // the prompt and therefore HOLDS the context the capture lacked.
        //
        // Asked once, not every turn: the enqueue is fired from the MINT, which
        // happens once per card, and the guard key is the card id — so even a
        // duplicate mint replaces the queued row instead of stacking a second
        // copy (steer_enqueue dedupes on `guard`).
        let minted = minted.lock().unwrap().take();
        if let Some((card_id, session)) = minted {
            let reason = needs_self_desc.unwrap_or("it has no referent outside this conversation");
            let msg = format!(
                "Board card {card_id} was captured from your last prompt, and its title \
                 cannot be dispatched by anyone who was not in this conversation: {reason}.\n\n\
                 You have the context the capture never had. Rewrite the title (and the body \
                 if it needs it) so the card stands alone — name the thing, where it lives, \
                 and what \"done\" means:\n\n  \
                 amux board retitle {card_id} --stdin <<'EOF'\n  \
                 <a title that names its own subject>\n  EOF\n\n\
                 If the prompt was steering rather than a task, discard the card instead \
                 (`amux board status {card_id} discarded`) — a card that should not exist is \
                 the honest answer too."
            );
            let _ = crate::api::session_verbs::steer_enqueue_store(
                &self.store,
                &session,
                &msg,
                &format!("self-describe:{card_id}"),
                "",
            )
            .await;
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ReconcileReport {
    /// Workers whose DB session was live but whose backend process is gone.
    pub interrupted: Vec<String>,
    /// Backend refs running with no live DB session row.
    pub stale_backend: Vec<String>,
    /// Backends that could not be probed (their sessions were NOT judged).
    pub backend_probe_failures: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        AttachInfo, BackendError, BackendSession, ProcessRef, SessionSpec,
    };
    use async_trait::async_trait;

    /// Scripted fake backend for reconciliation tests (Invariant 22).
    struct FakeBackend {
        hosted: Vec<BackendSession>,
        fail_probe: bool,
    }

    #[async_trait]
    impl SessionBackend for FakeBackend {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn spawn(&self, _s: &SessionSpec) -> crate::backend::Result<ProcessRef> {
            Err(BackendError::SpawnFailed("fake".into()))
        }
        async fn terminate(&self, _p: &ProcessRef) -> crate::backend::Result<()> {
            Ok(())
        }
        async fn status(&self, _p: &ProcessRef) -> crate::backend::Result<BackendStatus> {
            Ok(BackendStatus::NotFound)
        }
        async fn attach_info(&self, _p: &ProcessRef) -> crate::backend::Result<AttachInfo> {
            Ok(AttachInfo {
                command: "true".into(),
            })
        }
        async fn reconcile(&self) -> crate::backend::Result<Vec<BackendSession>> {
            if self.fail_probe {
                Err(BackendError::CommandFailed("probe down".into()))
            } else {
                Ok(self.hosted.clone())
            }
        }
        async fn capture(&self, _p: &ProcessRef, _l: u32) -> crate::backend::Result<String> {
            Ok(String::new())
        }
    }

    fn test_runtime(store: SharedStore, backends: Vec<Arc<dyn SessionBackend>>) -> Runtime {
        Runtime {
            store,
            backends,
            tick_secs: 3,
            heartbeat_every: 1,
            breaker: amux_core::circuit::FleetCircuitBreaker {
                window_budget_tokens: 0, // 0 disables the spend trip in tests
                window_secs: 3600,
                min_progress_per_window: 0,
                max_failures_per_window: 1000,
            },
            fleet_state: std::sync::Mutex::new(amux_core::circuit::FleetState::Normal),
            protocol: None,
            pickup_unowned: false,
            resume_stagger_secs: 5,
        }
    }

    fn store() -> SharedStore {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let s = Arc::new(crate::db::Store::open(&path).unwrap());
        // Leak tempdir so the DB survives the test body.
        std::mem::forget(dir);
        s
    }

    fn seed_live_session(store: &SharedStore, sid: &str, wid: &str, bref: &str) {
        let (sid, wid, bref) = (sid.to_string(), wid.to_string(), bref.to_string());
        store
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO _amux_workers (id, display_name, created_at, updated_at)
                     VALUES (?1, 'w', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                    params![wid],
                )?;
                conn.execute(
                    "INSERT INTO _amux_sessions (id, worker_id, backend, backend_ref, started_at)
                     VALUES (?1, ?2, 'fake', ?3, '2026-01-01T00:00:00Z')",
                    params![sid, wid, bref],
                )?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .unwrap();
    }

    #[tokio::test]
    async fn reconcile_marks_vanished_sessions_interrupted() {
        let store = store();
        seed_live_session(&store, "ses_a", "wrk_a", "amux-wrk_a");
        let rt = test_runtime(store.clone(), vec![Arc::new(FakeBackend {
            hosted: vec![], // backend hosts nothing
            fail_probe: false,
        })]);
        let report = rt.reconcile_on_startup().await.unwrap();
        assert_eq!(report.interrupted, vec!["wrk_a".to_string()]);
        // The session row is ended.
        let conn = store.read().unwrap();
        let ended: Option<String> = conn
            .query_row("SELECT ended_at FROM _amux_sessions WHERE id='ses_a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(ended.is_some());
    }

    #[tokio::test]
    async fn reconcile_reports_stale_backend_refs_without_killing() {
        let store = store();
        let rt = test_runtime(store, vec![Arc::new(FakeBackend {
            hosted: vec![BackendSession {
                backend_ref: "amux-wrk_ghost".into(),
                status: BackendStatus::Running,
            }],
            fail_probe: false,
        })]);
        let report = rt.reconcile_on_startup().await.unwrap();
        assert_eq!(report.stale_backend, vec!["amux-wrk_ghost".to_string()]);
    }

    #[tokio::test]
    async fn failed_probe_never_mass_ends_sessions() {
        let store = store();
        seed_live_session(&store, "ses_b", "wrk_b", "amux-wrk_b");
        let rt = test_runtime(store.clone(), vec![Arc::new(FakeBackend {
            hosted: vec![],
            fail_probe: true, // probe down != sessions gone
        })]);
        let report = rt.reconcile_on_startup().await.unwrap();
        assert!(report.interrupted.is_empty(), "flaky probe must not reap");
        assert_eq!(report.backend_probe_failures.len(), 1);
        let conn = store.read().unwrap();
        let ended: Option<String> = conn
            .query_row("SELECT ended_at FROM _amux_sessions WHERE id='ses_b'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(ended.is_none(), "session must survive a failed probe");
    }

    #[tokio::test]
    async fn tick_reclaims_expired_lease_and_heartbeats() {
        let store = store();
        store
            .write(|conn| {
                conn.execute(
                    "INSERT INTO _amux_leases (task_id, worker_id, acquired_at, expires_at, generation)
                     VALUES ('tsk_01JGXV0000000000000000TEST', 'wrk_01JGXV0000000000000000TEST', '2026-01-01T00:00:00Z', '2026-01-01T00:10:00Z', 2)",
                    [],
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let rt = test_runtime(store.clone(), vec![]);
        let mut rx = store.subscribe();
        rt.tick_once(true).await.unwrap();
        // Lease is gone (expired long ago).
        let conn = store.read().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM _amux_leases", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
        drop(conn);
        // Both the reclaim event and the heartbeat were published.
        let mut kinds = vec![];
        while let Ok(ev) = rx.try_recv() {
            kinds.push(format!("{:?}", ev.entity_type));
        }
        assert!(kinds.iter().any(|k| k.contains("lease")), "{kinds:?}");
        assert!(kinds.iter().any(|k| k.contains("fleet_progress")), "{kinds:?}");
    }
}

#[cfg(test)]
mod pump_tests {
    use super::*;
    use crate::opencode::mock::{MockProtocol, RecordedCall};
    use crate::opencode::AgentState;
    use amux_core::ids::{CommandId, WorkerId};
    use amux_core::protocol::{CommandState, DeliveryTiming, WorkerCommand};

    fn store() -> SharedStore {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let s = Arc::new(crate::db::Store::open(&path).unwrap());
        std::mem::forget(dir);
        s
    }

    fn wid() -> WorkerId {
        WorkerId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 77))
    }

    fn runtime_with(store: SharedStore, protocol: Arc<MockProtocol>) -> Runtime {
        Runtime {
            store,
            backends: vec![],
            tick_secs: 3,
            heartbeat_every: 1000,
            breaker: amux_core::circuit::FleetCircuitBreaker {
                window_budget_tokens: u64::MAX,
                window_secs: 3600,
                min_progress_per_window: 0,
                max_failures_per_window: 1000,
            },
            fleet_state: std::sync::Mutex::new(amux_core::circuit::FleetState::Normal),
            protocol: Some(protocol),
            pickup_unowned: false,
            resume_stagger_secs: 5,
        }
    }

    #[tokio::test]
    async fn pump_delivers_when_idle_and_holds_mid_turn() {
        let store = store();
        let protocol = Arc::new(MockProtocol::new());
        // Worker mid-turn: an AtTurnBoundary command must WAIT.
        protocol.register(wid(), AgentState::Working { turn: None, progress: None });
        let cmd_id = CommandId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 501));
        {
            let id = cmd_id.clone();
            store
                .write(move |conn| {
                    crate::db::commands::enqueue(
                        conn, id, &wid(), &WorkerCommand::Continue, "pump-k1",
                        &DeliveryTiming::AtTurnBoundary, None, Utc::now(),
                    )?;
                    Ok(WriteOutcome { applied: true, events: vec![] })
                })
                .unwrap();
        }
        let rt = runtime_with(store.clone(), protocol.clone());
        rt.pump_commands(Utc::now(), &std::collections::BTreeMap::new()).await.unwrap();
        assert!(protocol.calls().is_empty(), "mid-turn: nothing delivered");
        {
            let conn = store.read().unwrap();
            let cmd = crate::db::commands::by_id(&conn, &cmd_id).unwrap().unwrap();
            assert_eq!(cmd.state, CommandState::Queued, "still queued");
        }

        // Turn ends -> delivery goes through and the state advances.
        protocol.set_state(&wid(), AgentState::Idle, None);
        rt.pump_commands(Utc::now(), &std::collections::BTreeMap::new()).await.unwrap();
        let calls = protocol.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(matches!(&calls[0], RecordedCall::SendPrompt { worker, .. } if worker == &wid()));
        let conn = store.read().unwrap();
        let cmd = crate::db::commands::by_id(&conn, &cmd_id).unwrap().unwrap();
        assert_eq!(cmd.state, CommandState::Delivered);
    }

    /// RR-0044b: the fleet gate outranks the timing gate. The worker's own
    /// agent is Idle and the command is Immediate — every pre-existing gate
    /// says GO — but a sibling on the same provider is rate-limited, so the
    /// provider is exhausted and the pump must refuse. The command stays
    /// Queued (parked, not failed): delivery burns retry budget against a
    /// limit the fleet already knows about.
    #[tokio::test]
    async fn pump_refuses_delivery_on_exhausted_provider_even_immediate() {
        let store = store();
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(wid(), AgentState::Idle);
        let cmd_id = CommandId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 503));
        {
            let id = cmd_id.clone();
            store
                .write(move |conn| {
                    crate::db::commands::enqueue(
                        conn, id, &wid(), &WorkerCommand::Continue, "pump-k3",
                        &DeliveryTiming::Immediate, None, Utc::now(),
                    )?;
                    Ok(WriteOutcome { applied: true, events: vec![] })
                })
                .unwrap();
        }
        // Fleet picture: target worker Idle, a same-provider sibling parked
        // on an unexpired limit -> provider exhausted. Built through the
        // same core derivation the tick uses.
        let mk = |id: amux_core::ids::WorkerId, state: amux_core::worker::WorkerState| {
            let mut w = amux_core::worker::Worker::new(
                id,
                amux_core::worker::WorkerConfig {
                    display_name: "w".into(),
                    name_aliases: vec![],
                    cwd: "/tmp".into(),
                    provider: amux_core::provider::ProviderId::new("claude"),
                    model: None,
                    backend: amux_core::session::BackendId::herdr(),
                    environment: Default::default(),
                    permissions: vec![],
                    group: None,
                },
                Default::default(),
            );
            w.state = state;
            w
        };
        let now = Utc::now();
        let sibling = WorkerId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 78));
        let workers = vec![
            mk(wid(), amux_core::worker::WorkerState::Idle { since: now }),
            mk(sibling, amux_core::worker::WorkerState::RateLimited {
                reset_at: Some(now + chrono::Duration::hours(1)),
            }),
        ];
        let exhausted = amux_core::provider_fleet::derive(&workers, now, 5);

        let rt = runtime_with(store.clone(), protocol.clone());
        rt.pump_commands(now, &exhausted).await.unwrap();
        assert!(protocol.calls().is_empty(), "exhausted provider: nothing delivered");
        {
            let conn = store.read().unwrap();
            let cmd = crate::db::commands::by_id(&conn, &cmd_id).unwrap().unwrap();
            assert_eq!(cmd.state, CommandState::Queued, "parked, not failed — nothing lost");
            assert_eq!(cmd.attempts, 0, "no retry budget spent while parked");
        }

        // Provider recovers (sibling's limit lifts) -> same command drains.
        let workers = vec![
            mk(wid(), amux_core::worker::WorkerState::Idle { since: now }),
            mk(
                WorkerId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 78)),
                amux_core::worker::WorkerState::Idle { since: now },
            ),
        ];
        let recovered = amux_core::provider_fleet::derive(&workers, now, 5);
        rt.pump_commands(now, &recovered).await.unwrap();
        assert_eq!(protocol.calls().len(), 1, "recovered provider: delivery drains");
        let conn = store.read().unwrap();
        let cmd = crate::db::commands::by_id(&conn, &cmd_id).unwrap().unwrap();
        assert_eq!(cmd.state, CommandState::Delivered);
    }

    #[tokio::test]
    async fn pump_fails_command_against_dead_worker() {
        let store = store();
        // Protocol knows NOTHING about this worker (no session).
        let protocol = Arc::new(MockProtocol::new());
        let cmd_id = CommandId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 502));
        {
            let id = cmd_id.clone();
            store
                .write(move |conn| {
                    crate::db::commands::enqueue(
                        conn, id, &wid(), &WorkerCommand::Continue, "pump-k2",
                        &DeliveryTiming::Immediate, None, Utc::now(),
                    )?;
                    Ok(WriteOutcome { applied: true, events: vec![] })
                })
                .unwrap();
        }
        let rt = runtime_with(store.clone(), protocol);
        rt.pump_commands(Utc::now(), &std::collections::BTreeMap::new()).await.unwrap();
        let conn = store.read().unwrap();
        let cmd = crate::db::commands::by_id(&conn, &cmd_id).unwrap().unwrap();
        assert!(
            matches!(&cmd.state, CommandState::Failed { reason } if reason.contains("delivery failed")),
            "{:?}", cmd.state
        );
        assert_eq!(cmd.attempts, 1, "failure recorded for the retry budget");
    }
}

/// Board-adherence tests (2026-08-09 audit): the board is the mechanism that
/// keeps workers on board work. Each test here pins one adherence property:
/// no work without a card, no card mutation across the Python fence, no
/// infinite retry without the board hearing about it, and no silent work.
#[cfg(test)]
mod adherence_tests {
    use super::*;
    use crate::db::board_store as bs;
    use crate::opencode::mock::{MockProtocol, RecordedCall};
    use crate::opencode::AgentState;
    use amux_core::ids::{CommandId, MessageId, WorkerId};
    use amux_core::limits::AttemptRecord;
    use amux_core::protocol::{DeliveryTiming, WorkerCommand};
    use amux_core::worker::{WorkerConfig, WorkerState};

    fn store() -> SharedStore {
        let dir = tempfile::tempdir().unwrap();
        let s = Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        std::mem::forget(dir);
        s
    }

    fn wid(n: u128) -> WorkerId {
        WorkerId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 9_000 + n))
    }

    fn seed_worker(store: &SharedStore, n: u128, name: &str) -> WorkerId {
        let id = wid(n);
        let (idc, name) = (id.clone(), name.to_string());
        store
            .write(move |conn| {
                let row = crate::db::queries::WorkerRow::new(
                    &idc,
                    &WorkerConfig {
                        display_name: name.clone(),
                        name_aliases: vec![],
                        cwd: "/tmp".into(),
                        provider: amux_core::provider::ProviderId("claude".into()),
                        model: None,
                        backend: amux_core::session::BackendId::herdr(),
                        environment: Default::default(),
                        permissions: vec![],
                        group: None,
                    },
                    "2026-01-01T00:00:00Z",
                );
                crate::db::queries::insert_worker(conn, &row)?;
                crate::db::queries::update_worker_state(
                    conn,
                    idc.as_str(),
                    &WorkerState::Idle { since: Utc::now() },
                    "2026-01-01T00:00:00Z",
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        id
    }

    fn seed_issue(store: &SharedStore, title: &str, session: &str, status: &str) -> String {
        let (title, session, status) =
            (title.to_string(), session.to_string(), status.to_string());
        let out: Arc<std::sync::Mutex<String>> = Arc::default();
        let out_w = out.clone();
        store
            .write(move |conn| {
                let row = bs::create_issue(
                    conn,
                    &bs::NewIssue {
                        title: title.clone(),
                        desc: String::new(),
                        status: status.clone(),
                        session: Some(session.clone()).filter(|s| !s.is_empty()),
                        item_type: "code".into(),
                        creator: "test".into(),
                        owner_type: "agent".into(),
                        due: None,
                        due_time: None,
                        reviewer: None,
                        shepherd: None,
                        gate: vec![],
                        depends_on: vec![],
                        tags: vec![],
                    },
                    1_700_000_000,
                )?;
                *out_w.lock().unwrap() = row.id;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let sem = out.lock().unwrap().clone();
        sem
    }

    fn runtime(store: SharedStore, protocol: Option<Arc<MockProtocol>>, pickup: bool) -> Runtime {
        Runtime {
            store,
            backends: vec![],
            tick_secs: 3,
            heartbeat_every: 1,
            breaker: amux_core::circuit::FleetCircuitBreaker {
                window_budget_tokens: u64::MAX,
                window_secs: 3600,
                min_progress_per_window: 0,
                max_failures_per_window: 1000,
            },
            fleet_state: std::sync::Mutex::new(amux_core::circuit::FleetState::Normal),
            protocol: protocol.map(|p| p as Arc<dyn crate::opencode::AgentProtocol>),
            pickup_unowned: pickup,
            resume_stagger_secs: 5,
        }
    }

    fn count(store: &SharedStore, sql: &str) -> i64 {
        let conn = store.read().unwrap();
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn issue_field(store: &SharedStore, sem: &str, col: &str) -> String {
        let conn = store.read().unwrap();
        conn.query_row(
            &format!("SELECT COALESCE(CAST({col} AS TEXT), '') FROM issues WHERE id = ?1"),
            params![sem],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// M1 + M6: an idle Rust worker with NO eligible board card receives
    /// NOTHING — no invented work (Invariant 20) — and cards owned by the
    /// Python fleet are never assigned, leased, or mutated
    /// (strangler-fig isolation). The pickup_unowned=true leg is the
    /// POSITIVE CONTROL: the same probe detects assignment when it is
    /// legitimate, so the empty result on the isolation leg is a finding,
    /// not a broken instrument (ethos rule 7: confirm the probe could have
    /// produced a positive).
    #[tokio::test]
    async fn idle_worker_receives_nothing_without_an_eligible_card() {
        let store = store();
        let w = seed_worker(&store, 1, "alpha");
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle);
        let sem_py = seed_issue(&store, "python lane work", "py-lane", "todo");
        let sem_un = seed_issue(&store, "unowned pool work", "", "todo");
        let before_py: (String, String) = (
            issue_field(&store, &sem_py, "status"),
            issue_field(&store, &sem_py, "rev"),
        );

        let rt = runtime(store.clone(), Some(protocol.clone()), false);
        let mut rx = store.subscribe();
        rt.tick_once(true).await.unwrap();

        assert!(protocol.calls().is_empty(), "no eligible card -> no prompt: {:?}", protocol.calls());
        assert_eq!(count(&store, "SELECT COUNT(*) FROM _amux_commands"), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM _amux_leases"), 0);
        assert_eq!(issue_field(&store, &sem_py, "status"), before_py.0, "python card untouched");
        assert_eq!(issue_field(&store, &sem_py, "rev"), before_py.1, "python card rev unmoved");
        // Foreign/unowned cards are not this worker's stalls either.
        let mut stall_total = 0u64;
        while let Ok(ev) = rx.try_recv() {
            if format!("{:?}", ev.entity_type).contains("fleet_progress") {
                let hb: serde_json::Value = serde_json::from_str(&ev.entity_id).unwrap();
                stall_total += hb["stall_violations"].as_u64().unwrap_or(0);
            }
        }
        assert_eq!(stall_total, 0, "foreign work must not stall-report");

        // POSITIVE CONTROL: pickup_unowned=true assigns the unowned card —
        // and STILL never touches the Python-owned one.
        let rt2 = runtime(store.clone(), Some(protocol.clone()), true);
        rt2.tick_once(false).await.unwrap();
        let leased: Vec<String> = {
            let conn = store.read().unwrap();
            let mut stmt = conn.prepare("SELECT task_id FROM _amux_leases").unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.collect::<Result<_, _>>().unwrap()
        };
        assert_eq!(
            leased,
            vec![bs::internal_id(&sem_un).to_string()],
            "positive control: unowned card assigned, python card still never"
        );
        assert_eq!(issue_field(&store, &sem_py, "rev"), before_py.1);
    }

    /// M2: with the attempt ledger wired into the tick, a task that has
    /// exhausted its ExecutionLimits (and failed decomposition twice) is
    /// QUARANTINED ON THE BOARD — terminal, visible, never silently
    /// re-assigned. Pre-fix the tick fed `enforce_limits` an EMPTY attempts
    /// map, so the anti-livelock check could never fire (a check that cannot
    /// fail, ethos rule 7): this test then finds a fresh command + lease and
    /// a still-`todo` card, and fails.
    #[tokio::test]
    async fn exhausted_attempts_quarantine_the_card_on_the_board() {
        let store = store();
        seed_worker(&store, 2, "alpha");
        let sem = seed_issue(&store, "stuck task", "alpha", "todo");
        let task = bs::internal_id(&sem);
        // 5 failed attempts (= default max_attempts), two of which tried
        // decomposition -> the enforce_limits verdict is Quarantine.
        let task_s = task.to_string();
        store
            .write(move |conn| {
                for n in 1..=5u32 {
                    let rec = AttemptRecord {
                        attempt: n,
                        failure_reason: format!("attempt {n} failed: tests red"),
                        rejected_evidence: vec![],
                        tokens_spent: 100,
                        wall_clock_secs: 60,
                        decomposition_attempted: n >= 4,
                        tree_status: None,
                        at: Utc::now() - chrono::Duration::minutes(60 - n as i64),
                    };
                    conn.execute(
                        "INSERT INTO _amux_attempts (task_id, worker_id, attempt, record, at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            task_s,
                            "wrk_x",
                            n,
                            serde_json::to_string(&rec).unwrap(),
                            rec.at.to_rfc3339()
                        ],
                    )?;
                }
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();

        let rt = runtime(store.clone(), None, false);
        rt.tick_once(false).await.unwrap();

        assert_eq!(
            issue_field(&store, &sem, "status"),
            "quarantined",
            "exhausted limits must land ON THE BOARD as the terminal status"
        );
        let log = issue_field(&store, &sem, "log");
        assert!(log.contains("quarantined"), "audit line missing: {log}");
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM _amux_commands"),
            0,
            "an exhausted task must not be handed out again"
        );
        assert_eq!(count(&store, "SELECT COUNT(*) FROM _amux_leases"), 0);

        // Next tick: terminal disposition — no resurrection, no second write.
        let rev = issue_field(&store, &sem, "rev");
        rt.tick_once(false).await.unwrap();
        assert_eq!(issue_field(&store, &sem, "status"), "quarantined");
        assert_eq!(issue_field(&store, &sem, "rev"), rev, "quarantine writes once");
    }

    /// M5: NO SILENT WORK. A prompt delivered directly to a Rust worker
    /// (messages path) with no open card mints a ledger card attributed to
    /// that worker — title computed from the prompt's own first clause,
    /// never a model call (ethos rule 2). Pre-fix, delivery left the board
    /// blank: work a reviewer could not see anywhere.
    #[tokio::test]
    async fn delivered_prompt_with_no_open_card_mints_a_ledger_card() {
        let store = store();
        let w = seed_worker(&store, 3, "alpha");
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle);

        let msg = MessageId::from_ulid(ulid::Ulid::new());
        let body = "please fix the flaky auth test. It only fails on CI.";
        seed_message(&store, &msg, body);
        enqueue_deliver(&store, &w, &msg, "cap-k1");

        let rt = runtime(store.clone(), Some(protocol.clone()), false);
        rt.pump_commands(Utc::now(), &BTreeMap::new()).await.unwrap();
        assert!(
            matches!(&protocol.calls()[..], [RecordedCall::DeliverMessage { .. }]),
            "{:?}",
            protocol.calls()
        );

        let conn = store.read().unwrap();
        let (sem, title, status, session, owner_type, log, notified): (
            String, String, String, String, String, String, i64,
        ) = conn
            .query_row(
                "SELECT id, title, status, session, owner_type, COALESCE(log,''), notified
                 FROM issues",
                [],
                |r| {
                    Ok((
                        r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .expect("delivery must have minted exactly one ledger card");
        assert_eq!(title, "Fix the flaky auth test");
        assert_eq!(
            status, "doing",
            "in flight, not queued — a todo mint gets re-dispatched by the planner"
        );
        assert_eq!(session, "alpha", "attributed to the receiving worker");
        assert_eq!(owner_type, "agent");
        assert!(log.contains("capture: session prompt"), "durable marker: {log}");
        assert_eq!(notified, 1, "the worker already received this prompt; not news");
        assert!(!sem.is_empty());
    }

    /// AMUX-2613 E2E regression: the ledger card minted from a delivered
    /// prompt must NOT be picked up by the planner and redelivered as an
    /// ExecuteTask — pre-fix (status "todo") the next tick assigned it and
    /// the same prompt ran TWICE (observed in a live claude transcript:
    /// once raw, once wrapped in "Task tsk_…").
    #[tokio::test]
    async fn captured_ledger_card_is_not_redispatched_by_the_planner() {
        let store = store();
        let w = seed_worker(&store, 5, "alpha");
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle);
        let rt = runtime(store.clone(), Some(protocol.clone()), false);

        let msg = MessageId::from_ulid(ulid::Ulid::new());
        seed_message(&store, &msg, "please fix the flaky auth test. It only fails on CI.");
        enqueue_deliver(&store, &w, &msg, "cap-k9");
        rt.pump_commands(Utc::now(), &BTreeMap::new()).await.unwrap();
        assert_eq!(count(&store, "SELECT COUNT(*) FROM issues"), 1, "ledger card minted");

        // The tick that pre-fix re-dispatched the card as an ExecuteTask.
        rt.tick_once(false).await.unwrap();
        rt.pump_commands(Utc::now(), &BTreeMap::new()).await.unwrap();

        let execute_cmds = count(
            &store,
            "SELECT COUNT(*) FROM _amux_commands WHERE command LIKE '%execute_task%'",
        );
        assert_eq!(execute_cmds, 0, "the ledger of a delivered prompt is not new work");
        assert!(
            matches!(&protocol.calls()[..], [RecordedCall::DeliverMessage { .. }]),
            "exactly ONE delivery ever reaches the worker: {:?}",
            protocol.calls()
        );
    }

    /// AMUX-2604: a deictic prompt still mints its ledger card — but the card
    /// is TAGGED, its log says why, and the worker that received the prompt is
    /// asked, at its next turn boundary, to rewrite the title self-contained.
    ///
    /// The nudge goes through the existing steering queue rather than a second
    /// delivery path, and its guard is the card id so it is asked ONCE.
    #[tokio::test]
    async fn a_deictic_prompt_flags_its_card_and_asks_the_worker_to_rewrite_it() {
        let store = store();
        // AF-188 made the shared enqueue REFUSE a target with no session env
        // file, so a worker that exists only as a store row is now undeliverable
        // and the ask below silently never queues. `seed_worker` writes the row;
        // it has never written the file, which did not matter until the enqueue
        // started asking. Register it.
        //
        // This is the fourth fixture the widening caught and the first that was
        // NOT in session_verbs — which is why it survived my post-change suite
        // run and went red in CI instead. A change to a chokepoint reaches every
        // caller's tests, including the ones in modules you did not open.
        let _home = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(_home.path());
        std::fs::create_dir_all(_home.path().join("sessions")).unwrap();
        std::fs::write(_home.path().join("sessions/alpha.env"), "CC_DIR=/tmp\n").unwrap();
        let w = seed_worker(&store, 7, "alpha");
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle);
        let rt = runtime(store.clone(), Some(protocol.clone()), false);

        let msg = MessageId::from_ulid(ulid::Ulid::new());
        seed_message(&store, &msg, "this should be one row");
        enqueue_deliver(&store, &w, &msg, "cap-2604");
        rt.pump_commands(Utc::now(), &BTreeMap::new()).await.unwrap();

        let (id, title, tags, log): (String, String, String, String) = store
            .read()
            .unwrap()
            .query_row(
                "SELECT i.id, i.title, COALESCE(GROUP_CONCAT(t.tag),''), COALESCE(i.log,'') \
                 FROM issues i LEFT JOIN issue_tags t ON t.issue_id = i.id GROUP BY i.id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("the card is still minted — flagged, not suppressed");
        assert_eq!(title, "This should be one row");
        assert!(tags.contains("needs-self-description"), "durable flag missing: {tags}");
        assert!(log.contains("needs self-description"), "the log must say WHY: {log}");

        // The ask is queued for the worker, keyed to the card, once.
        let (n, session, guard, text): (i64, String, String, String) = store
            .read()
            .unwrap()
            .query_row(
                "SELECT COUNT(*), COALESCE(MAX(session),''), COALESCE(MAX(guard),''), \
                 COALESCE(MAX(text),'') FROM steering_queue",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(n, 1, "exactly one ask");
        assert_eq!(session, "alpha", "addressed to the worker that got the prompt");
        assert_eq!(guard, format!("self-describe:{id}"), "dedupe key is the card");
        assert!(text.contains(&id), "the ask must name the card: {text}");
        // It must name a SANCTIONED next step, not leave the worker to
        // hand-roll a PATCH (which is how attribution gets lost).
        assert!(text.contains("amux board retitle"), "no walkable next step: {text}");

        // A SECOND capture for the same card cannot stack a second ask — the
        // guard replaces. (A worker is asked once, not every turn.)
        let m2 = MessageId::from_ulid(ulid::Ulid::new());
        seed_message(&store, &m2, "this should be one row");
        enqueue_deliver(&store, &w, &m2, "cap-2604b");
        rt.pump_commands(Utc::now(), &BTreeMap::new()).await.unwrap();
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM steering_queue WHERE guard LIKE 'self-describe:%'"),
            1,
            "the ask stacked — a worker must be asked once, not every turn"
        );
    }

    /// The other direction, which is the one that decides whether this is a
    /// feature or a nag: a self-contained prompt is flagged with NOTHING and
    /// the worker is not interrupted at all.
    #[tokio::test]
    async fn a_self_contained_prompt_is_not_flagged_and_sends_no_nudge() {
        let store = store();
        let w = seed_worker(&store, 8, "alpha");
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle);
        let rt = runtime(store.clone(), Some(protocol.clone()), false);

        let msg = MessageId::from_ulid(ulid::Ulid::new());
        seed_message(&store, &msg, "please fix the flaky auth test. It only fails on CI.");
        enqueue_deliver(&store, &w, &msg, "cap-2604c");
        rt.pump_commands(Utc::now(), &BTreeMap::new()).await.unwrap();

        let tags: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT COALESCE(GROUP_CONCAT(t.tag),'') FROM issues i \
                 LEFT JOIN issue_tags t ON t.issue_id = i.id GROUP BY i.id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!tags.contains("needs-self-description"), "false positive: {tags}");
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM steering_queue"),
            0,
            "a dispatchable title must not interrupt the worker"
        );
    }

    /// M5 guards: steering words mint nothing, and an OPEN card means the
    /// prompt is steering the work already in flight — no duplicate card.
    #[tokio::test]
    async fn steering_and_open_card_prompts_mint_no_ledger_card() {
        let store = store();
        let w = seed_worker(&store, 4, "alpha");
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle);
        let rt = runtime(store.clone(), Some(protocol.clone()), false);

        // Control word: delivered, but not a task.
        let m1 = MessageId::from_ulid(ulid::Ulid::new());
        seed_message(&store, &m1, "continue");
        enqueue_deliver(&store, &w, &m1, "cap-k2");
        rt.pump_commands(Utc::now(), &BTreeMap::new()).await.unwrap();
        assert_eq!(count(&store, "SELECT COUNT(*) FROM issues"), 0, "steering mints nothing");

        // Open card: the prompt lands on the in-flight work, not a new card.
        seed_issue(&store, "already in flight", "alpha", "doing");
        let m2 = MessageId::from_ulid(ulid::Ulid::new());
        seed_message(&store, &m2, "also handle the retry path in the same module please");
        enqueue_deliver(&store, &w, &m2, "cap-k3");
        rt.pump_commands(Utc::now(), &BTreeMap::new()).await.unwrap();
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM issues"),
            1,
            "an open card absorbs steering; no duplicate mint"
        );
    }

    fn seed_message(store: &SharedStore, id: &MessageId, body: &str) {
        let (id, body) = (id.to_string(), body.to_string());
        store
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO _amux_messages (id, from_actor, target, body, thread, created_at, delivery)
                     VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
                    params![
                        id,
                        r#"{"kind":"human","name":"ethan"}"#,
                        r#"{"kind":"worker","id":"wrk_x"}"#,
                        body,
                        Utc::now().to_rfc3339(),
                        r#"{"state":"queued"}"#
                    ],
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
    }

    fn enqueue_deliver(store: &SharedStore, w: &WorkerId, msg: &MessageId, key: &str) {
        let (w, msg, key) = (w.clone(), msg.clone(), key.to_string());
        store
            .write(move |conn| {
                crate::db::commands::enqueue(
                    conn,
                    CommandId::from_ulid(ulid::Ulid::new()),
                    &w,
                    &WorkerCommand::DeliverMessage(msg.clone()),
                    &key,
                    &DeliveryTiming::Immediate,
                    None,
                    Utc::now(),
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
    }
}

#[cfg(test)]
mod rate_limit_recovery_tests {
    use super::*;
    use amux_core::worker::{WorkerConfig, WorkerState};

    #[tokio::test]
    async fn expired_rate_limit_recovers_to_idle_and_unexpired_stays() {
        let dir = tempfile::tempdir().unwrap();
        let store: SharedStore = Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        std::mem::forget(dir);
        let now = Utc::now();
        let seed = |n: u128, reset: Option<DateTime<Utc>>| {
            let id = amux_core::ids::WorkerId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, n));
            let idc = id.clone();
            store
                .write(move |conn| {
                    let row = crate::db::queries::WorkerRow::new(
                        &idc,
                        &WorkerConfig {
                            display_name: format!("w{n}"),
                            name_aliases: vec![],
                            cwd: "/tmp".into(),
                            provider: amux_core::provider::ProviderId("claude".into()),
                            model: None,
                            backend: amux_core::session::BackendId::herdr(),
                            environment: Default::default(),
                            permissions: vec![],
                            group: None,
                        },
                        "2026-01-01T00:00:00Z",
                    );
                    crate::db::queries::insert_worker(conn, &row)?;
                    crate::db::queries::update_worker_state(
                        conn,
                        idc.as_str(),
                        &WorkerState::RateLimited { reset_at: reset },
                        "2026-01-01T00:00:00Z",
                    )?;
                    Ok(WriteOutcome { applied: true, events: vec![] })
                })
                .unwrap();
            id
        };
        let expired = seed(1, Some(now - chrono::Duration::minutes(5)));
        let future = seed(2, Some(now + chrono::Duration::hours(1)));
        let unknown = seed(3, None);

        let rt = Runtime {
            store: store.clone(),
            backends: vec![],
            tick_secs: 3,
            heartbeat_every: 1000,
            breaker: amux_core::circuit::FleetCircuitBreaker {
                window_budget_tokens: u64::MAX,
                window_secs: 3600,
                min_progress_per_window: 0,
                max_failures_per_window: 1000,
            },
            fleet_state: std::sync::Mutex::new(amux_core::circuit::FleetState::Normal),
            protocol: None,
            pickup_unowned: false,
            resume_stagger_secs: 5,
        };
        rt.tick_once(false).await.unwrap();

        let conn = store.read().unwrap();
        let state_of = |id: &amux_core::ids::WorkerId| -> String {
            conn.query_row(
                "SELECT json_extract(state,'$.state') FROM _amux_workers WHERE id = ?1",
                rusqlite::params![id.as_str()],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(state_of(&expired), "idle", "past reset -> recovered");
        assert_eq!(state_of(&future), "rate_limited", "future reset stays parked");
        // No reset time: stays parked — inventing a retry would be guessing
        // (Inv 20), and Credit caps clear on payment, not clocks (AF-14).
        assert_eq!(state_of(&unknown), "rate_limited");
    }

    fn seed_rate_limited(
        store: &SharedStore,
        n: u128,
        reset: Option<DateTime<Utc>>,
    ) -> amux_core::ids::WorkerId {
        let id = amux_core::ids::WorkerId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, n));
        let idc = id.clone();
        store
            .write(move |conn| {
                let row = crate::db::queries::WorkerRow::new(
                    &idc,
                    &WorkerConfig {
                        display_name: format!("w{n}"),
                        name_aliases: vec![],
                        cwd: "/tmp".into(),
                        provider: amux_core::provider::ProviderId("claude".into()),
                        model: None,
                        backend: amux_core::session::BackendId::herdr(),
                        environment: Default::default(),
                        permissions: vec![],
                        group: None,
                    },
                    "2026-01-01T00:00:00Z",
                );
                crate::db::queries::insert_worker(conn, &row)?;
                crate::db::queries::update_worker_state(
                    conn,
                    idc.as_str(),
                    &WorkerState::RateLimited { reset_at: reset },
                    "2026-01-01T00:00:00Z",
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        id
    }

    fn recovery_runtime(store: SharedStore) -> Runtime {
        Runtime {
            store,
            backends: vec![],
            tick_secs: 3,
            heartbeat_every: 1000,
            breaker: amux_core::circuit::FleetCircuitBreaker {
                window_budget_tokens: u64::MAX,
                window_secs: 3600,
                min_progress_per_window: 0,
                max_failures_per_window: 1000,
            },
            fleet_state: std::sync::Mutex::new(amux_core::circuit::FleetState::Normal),
            protocol: None,
            pickup_unowned: false,
            resume_stagger_secs: 5,
        }
    }

    fn db_state_of(store: &SharedStore, id: &amux_core::ids::WorkerId) -> String {
        let conn = store.read().unwrap();
        conn.query_row(
            "SELECT json_extract(state,'$.state') FROM _amux_workers WHERE id = ?1",
            rusqlite::params![id.as_str()],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// RR-0044b staggered recovery (time-warp idiom): three same-provider
    /// workers share a reset 7s in the past. Slot i = reset + i*5s, so at
    /// "now" slots 0 (-7s) and 1 (-2s) have passed and slot 2 (+3s) has
    /// not: two workers recover, the third stays parked for its turn — the
    /// herd never fires at once.
    #[tokio::test]
    async fn recovery_is_staggered_across_same_provider_workers() {
        let dir = tempfile::tempdir().unwrap();
        let store: SharedStore =
            Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        std::mem::forget(dir);
        let now = Utc::now();
        let shared_reset = Some(now - chrono::Duration::seconds(7));
        let w1 = seed_rate_limited(&store, 11, shared_reset);
        let w2 = seed_rate_limited(&store, 12, shared_reset);
        let w3 = seed_rate_limited(&store, 13, shared_reset);

        let rt = recovery_runtime(store.clone());
        rt.tick_once(false).await.unwrap();

        assert_eq!(db_state_of(&store, &w1), "idle", "slot 0: reset+0s passed");
        assert_eq!(db_state_of(&store, &w2), "idle", "slot 1: reset+5s passed");
        assert_eq!(
            db_state_of(&store, &w3),
            "rate_limited",
            "slot 2: reset+10s is still 3s out — parked for its stagger turn"
        );

        // A tick after the last slot passes drains the straggler too: the
        // stagger delays, it never strands (zero user interaction).
        let w3s = w3.clone();
        store
            .write(move |conn| {
                crate::db::queries::update_worker_state(
                    conn,
                    w3s.as_str(),
                    &WorkerState::RateLimited {
                        reset_at: Some(Utc::now() - chrono::Duration::seconds(11)),
                    },
                    "2026-01-01T00:00:00Z",
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        rt.tick_once(false).await.unwrap();
        assert_eq!(db_state_of(&store, &w3), "idle", "own slot passed -> recovered");
    }

    /// RR-0044b: redistribution is a RECOMMENDATION event, deduped per
    /// exhaustion episode — amux never swaps a configured provider itself
    /// (routing.rs's rule; the provider choice is the user's).
    #[tokio::test]
    async fn exhausted_provider_emits_redistribute_recommendation_once() {
        let dir = tempfile::tempdir().unwrap();
        let store: SharedStore =
            Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        std::mem::forget(dir);
        let now = Utc::now();
        seed_rate_limited(&store, 21, Some(now + chrono::Duration::hours(2)));

        let rt = recovery_runtime(store.clone());
        let mut rx = store.subscribe();
        rt.tick_once(false).await.unwrap();
        let mut recommendations = vec![];
        while let Ok(ev) = rx.try_recv() {
            if format!("{:?}", ev.entity_type).contains("provider_redistribute_recommended") {
                recommendations.push(ev.entity_id.clone());
            }
        }
        assert_eq!(recommendations.len(), 1, "{recommendations:?}");
        assert!(
            recommendations[0].contains("claude"),
            "the recommendation names the provider: {}",
            recommendations[0]
        );

        // Same episode, next tick: silence (durable dedupe via the journal,
        // so even a restart would not re-announce this episode).
        rt.tick_once(false).await.unwrap();
        let mut second = 0;
        while let Ok(ev) = rx.try_recv() {
            if format!("{:?}", ev.entity_type).contains("provider_redistribute_recommended") {
                second += 1;
            }
        }
        assert_eq!(second, 0, "one announcement per exhaustion episode");
    }
}
