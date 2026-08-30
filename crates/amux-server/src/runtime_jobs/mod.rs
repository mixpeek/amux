//! Runtime jobs (Phase 3, RR-0058..RR-0062): two DELIBERATELY separate kinds
//! of "run something later".
//!
//! # DurableSchedule vs PeriodicTask — why two types
//!
//! The plan is explicit ("these are different things with different
//! semantics") and the separation is type-level on purpose:
//!
//! - [`scheduler::DurableSchedule`] is USER data. It is DB-backed (the live
//!   `schedules` table the Python server also reads/writes), survives
//!   restarts, has run history (`schedule_runs` with a `source` field), an
//!   audit trail (`schedule_audit` with X-Amux-Session attribution),
//!   missed-run policy, and timezone semantics. Losing one is losing a
//!   user's intent; every mutation must be attributable.
//! - [`PeriodicTask`] is INTERNAL plumbing. In-memory only, no run history,
//!   no audit, gone on restart by design. It exists so maintenance loops
//!   (rot sweeps, cache refresh, heartbeats) never masquerade as user
//!   schedules — the 451-folds/journal-card lesson generalized: if internal
//!   ticks were rows in `schedules`, the user's schedule list would
//!   accumulate machinery it can neither own nor delete (ethos rule 8), and
//!   every internal tick would demand fake audit attribution (ethos rule 3).
//!
//! There is intentionally NO conversion between the two. A `PeriodicTask`
//! cannot be persisted; a `DurableSchedule` cannot be constructed without a
//! DB row. If you find yourself wanting a hybrid, one of the two lists above
//! tells you which properties your job actually needs.
//!
//! # DUAL-SCHEDULER SAFETY (critical while the strangler fig is live)
//!
//! The Python server on :8822 keeps running and OWNS schedule firing. Both
//! servers read/write the SAME `schedules` / `schedule_runs` rows, so two
//! live firing loops would double-deliver every schedule. Therefore:
//!
//! - The Rust firing loop ships DISABLED. `AMUX_RS_SCHEDULER=1` in the
//!   environment enables real firing; anything else means SHADOW MODE.
//! - In shadow mode [`scheduler::run_scheduler`] still computes next-run
//!   times and, for every schedule that comes due, records what it WOULD
//!   have fired as an `_amux_state_events` row with entity type
//!   `Other("schedule_shadow")` — one event per (schedule, occurrence),
//!   deduped in memory. It touches NEITHER `schedules` nor `schedule_runs`,
//!   so Python's loop is entirely undisturbed. Phase 11 validation compares
//!   these shadow events against Python's actual `schedule_runs` fires.
//! - The CRUD API (`api::schedules`) is always live — creates, edits,
//!   deletes and audit rows are shared state both servers agree on. The ONE
//!   exception is run-now: with firing disabled it answers 409
//!   `{"error":"rust scheduler is in shadow mode","enable":"AMUX_RS_SCHEDULER=1"}`
//!   instead of silently recording a run that delivered nothing (ethos rule
//!   3: the honest refusal beats the quiet lie).

pub mod autofix;
pub mod board_drive;
pub mod commit_mention_notes;
pub mod commit_nudge;
pub mod context_health;
pub mod status_history;
pub mod disk_watch;
pub mod ghost_rescue;
pub mod heartbeat;
pub mod pane_size;
/// The live registry of the jobs below — see [`registry`] for why it is
/// derived from the spawn sites rather than declared alongside them.
pub mod registry;
pub mod scheduler;
pub mod storage;
pub mod tailnet_watch;
pub mod token_ledger;

pub use scheduler::{
    firing_enabled, run_scheduler, scheduler_tick, DueRuns, DurableSchedule, ExprParseError,
    MissedRunPolicy, RetryPolicy, ScheduleExpr, TickReport, SCHEDULER_TICK_SECS,
};

use std::time::Duration;
use tokio::time::MissedTickBehavior;

/// An internal, ephemeral periodic job. See the module docs for why this is
/// a different type from [`scheduler::DurableSchedule`] and must stay one:
/// no persistence, no history, no audit — by design, not by omission.
///
/// Each task runs on its OWN spawned tokio task driven by
/// `tokio::time::interval`, so a slow or wedged task cannot delay any other
/// task (the plan's non-blocking requirement; contrast Python's shared
/// `_JOB_REGISTRY` thread). A run that overshoots its interval delays only
/// its own next tick (`MissedTickBehavior::Delay` — no catch-up burst),
/// mirroring the Python registry's `_running` skip guard.
pub struct PeriodicTask {
    name: String,
    interval: Duration,
    handle: tokio::task::JoinHandle<()>,
}

impl PeriodicTask {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// True once the driving task has exited (only via `abort`; the loop
    /// itself never returns).
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Stop the task. Fire-and-forget tasks are NOT aborted on drop — an
    /// internal maintenance loop should outlive the handle that spawned it
    /// unless someone explicitly says stop.
    pub fn abort(&self) {
        self.handle.abort();
    }
}

/// Spawn an internal periodic job: `f` is invoked every `secs` seconds on a
/// dedicated tokio task (first invocation immediately, matching
/// `tokio::time::interval` and Python's job registry, whose jobs are due on
/// the first tick). Used by later phases for internal maintenance loops.
///
/// This is the ONLY constructor for [`PeriodicTask`] — there is no DB row
/// behind it and none is created; a job that needs history or attribution
/// belongs in `schedules` via the API instead.
pub fn spawn_periodic<F, Fut>(name: impl Into<String>, secs: u64, f: F) -> PeriodicTask
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    spawn_periodic_every(name, Duration::from_secs(secs.max(1)), f)
}

/// The per-job fleet-isolation opt-out variable for `name`: `AMUX_<NAME>_SECS`,
/// name uppercased with `-` turned into `_`. So the `pane_size` job is switched
/// by `AMUX_PANE_SIZE_SECS` and `ghost-rescue` by `AMUX_GHOST_RESCUE_SECS`.
/// Setting it to `0` disables that one loop; the global switches below disable
/// every loop at once.
fn per_job_disable_var(name: &str) -> String {
    format!("AMUX_{}_SECS", name.to_uppercase().replace('-', "_"))
}

/// Why this periodic job must NOT run, if it must not - the fleet-isolation
/// decision (AF-69), split from the env read (`get`) so it has a negative
/// control without touching process-global env, which cargo's parallel tests
/// share.
///
/// The hazard: some periodic jobs (`pane_size`, `ghost_rescue`) enumerate the
/// tmux fleet directly and take no `AppState`, so `AMUX_HOME` does not scope
/// them. A SECOND or TEST amux-server pointed at the production tmux socket would
/// therefore press Enter and resize panes in the real lanes. Two switches turn a
/// job off, both returning the exact switch string for the log and the registry:
///   - a GLOBAL isolation switch (`AMUX_ISOLATED=1` / `AMUX_NO_FLEET=1`) - the
///     one knob a dev/test server sets ONCE to opt the whole process out;
///   - a PER-JOB `AMUX_<NAME>_SECS=0` opt-out, to silence one loop.
fn isolation_reason_with<F: Fn(&str) -> Option<String>>(name: &str, get: F) -> Option<String> {
    for var in ["AMUX_ISOLATED", "AMUX_NO_FLEET"] {
        if get(var).as_deref().map(str::trim) == Some("1") {
            return Some(format!("{var}=1"));
        }
    }
    let per_job = per_job_disable_var(name);
    if get(&per_job).as_deref().map(str::trim) == Some("0") {
        return Some(format!("{per_job}=0"));
    }
    None
}

/// [`isolation_reason_with`] against the live process environment.
pub(crate) fn fleet_isolation_reason(name: &str) -> Option<String> {
    isolation_reason_with(name, |k| std::env::var(k).ok())
}

/// [`spawn_periodic`] with a raw `Duration`, for sub-second internal ticks
/// (and for tests, which drive real ~tens-of-ms intervals — the workspace
/// tokio has no `test-util`, so there is no paused clock to lean on).
pub fn spawn_periodic_every<F, Fut>(name: impl Into<String>, interval: Duration, mut f: F) -> PeriodicTask
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let name = name.into();

    // FLEET ISOLATION (AF-69). REGISTER FIRST, THEN decide whether to spawn the
    // tick loop - the ordering is load-bearing. A job that returned before
    // registering would vanish from /api/system-jobs, turning a live
    // fleet-driving hazard into a silent skip (ethos rule 4; this is the exact
    // failure the registry's own docs cite). So a suppressed job is registered
    // inert-but-visible, marked `disabled` with the switch that stopped it, and
    // its body NEVER runs - which is the whole point: a second/test amux-server
    // must not press Enter or resize panes in the production tmux lanes.
    if let Some(reason) = fleet_isolation_reason(&name) {
        registry::register_disabled(&name, "periodic", Some(interval), reason.clone());
        // Two-fixes rule: the NEXT time a stray/test server is pointed at the
        // prod socket, its log names every fleet-driving loop it refused to run
        // and why - so a log sweep catches "a second server was about to drive
        // prod" without a human noticing first.
        tracing::info!(
            job = %name,
            switch = %reason,
            "periodic job suppressed: fleet isolation is on, this loop will NOT tick \
             (prevents a second/test amux-server driving the production tmux fleet, AF-69)"
        );
        // No loop to drive; the registry row above, not this handle, is what
        // makes the job visible. Kept as a completed handle so PeriodicTask's
        // shape is unchanged for callers.
        let handle = tokio::spawn(async {});
        return PeriodicTask { name, interval, handle };
    }

    // VISIBILITY IS NOT OPTIONAL, and it is not the job's responsibility.
    // Because this is the ONLY constructor of a PeriodicTask, registering here
    // means every internal job — including ones written years from now by
    // someone who has never read `registry` — appears on
    // `GET /api/system-jobs` with a real interval and real tick timing. The
    // wrapper below brackets the job's own closure, so the tick timestamps are
    // recorded by the SPAWNER: a job cannot forget to report, and cannot
    // report a tick it did not run. See registry's docs for the three loops
    // that were dead for hours with nothing visible anywhere.
    let job_id = name.clone();
    let handle = tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        // Delay, not Burst: a run that overshoots must not be "paid back"
        // with a rapid-fire catch-up volley (the spin-catcher lesson — a
        // detector/maintainer must not amplify the load it exists to manage).
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            registry::tick_start(&job_id);
            f().await;
            registry::tick_end(&job_id);
        }
    });
    registry::register(&name, "periodic", Some(interval), Some(handle.abort_handle()));
    PeriodicTask {
        name,
        interval,
        handle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // Type separation is a compile-time property; this test documents it as
    // an executable statement rather than prose: PeriodicTask's public
    // surface has NO persistence/history/audit affordances to call, and
    // DurableSchedule (scheduler.rs) has no spawn affordance. If someone
    // adds a `PeriodicTask::persist` or `DurableSchedule::spawn`, this
    // comment is the tripwire explaining why review should bounce it.
    #[tokio::test]
    async fn periodic_task_exposes_only_ephemeral_surface() {
        let t = spawn_periodic("noop", 60, || async {});
        assert_eq!(t.name(), "noop");
        assert_eq!(t.interval(), Duration::from_secs(60));
        assert!(!t.is_finished());
        t.abort();
    }

    #[tokio::test]
    async fn ticks_at_interval() {
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let t = spawn_periodic_every("fast", Duration::from_millis(25), move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let n = count.load(Ordering::SeqCst);
        // Immediate first tick + ~7 more; generous bounds for CI jitter.
        assert!((4..=12).contains(&n), "expected ~8 ticks in 200ms, got {n}");
        t.abort();
    }

    #[tokio::test]
    async fn slow_task_does_not_block_others() {
        let fast = Arc::new(AtomicUsize::new(0));
        let slow = Arc::new(AtomicUsize::new(0));
        let f = fast.clone();
        let tf = spawn_periodic_every("fast", Duration::from_millis(20), move || {
            let f = f.clone();
            async move {
                f.fetch_add(1, Ordering::SeqCst);
            }
        });
        let s = slow.clone();
        let ts = spawn_periodic_every("slow", Duration::from_millis(20), move || {
            let s = s.clone();
            async move {
                s.fetch_add(1, Ordering::SeqCst);
                // Far longer than everyone's interval: if tasks shared a
                // driver, this would starve "fast".
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(400)).await;
        let nf = fast.load(Ordering::SeqCst);
        let ns = slow.load(Ordering::SeqCst);
        assert!(nf >= 8, "fast task starved by slow one: {nf} ticks in 400ms");
        assert!((1..=2).contains(&ns), "slow task should be mid-first-run: {ns}");
        tf.abort();
        ts.abort();
    }

    // ---- Fleet isolation (AF-69) ------------------------------------------

    #[test]
    fn per_job_disable_var_matches_the_documented_convention() {
        // The card's convention, verbatim: AMUX_<NAME>_SECS, name uppercased
        // with '-' turned into '_'. These two are the fleet-driving jobs AF-69
        // exists to keep a test server from running.
        assert_eq!(per_job_disable_var("pane_size"), "AMUX_PANE_SIZE_SECS");
        assert_eq!(per_job_disable_var("ghost-rescue"), "AMUX_GHOST_RESCUE_SECS");
    }

    /// The isolation predicate WITH its negative controls, exercised without
    /// touching process-global env (which cargo's parallel tests share). A rule
    /// that returned Some for everything would silently disable the whole fleet;
    /// one that returned None for everything would be the AF-69 hazard shipped
    /// unguarded. So both directions are asserted for every switch.
    #[test]
    fn isolation_reason_reads_both_switches_with_negative_controls() {
        // Prod default: no switch set, so no job is suppressed.
        assert_eq!(isolation_reason_with("pane_size", |_| None), None);

        // Global switches disable ANY job, and name themselves.
        let iso = |k: &str| (k == "AMUX_ISOLATED").then(|| "1".to_string());
        assert_eq!(
            isolation_reason_with("pane_size", iso).as_deref(),
            Some("AMUX_ISOLATED=1")
        );
        let nof = |k: &str| (k == "AMUX_NO_FLEET").then(|| "1".to_string());
        assert_eq!(
            isolation_reason_with("ghost-rescue", nof).as_deref(),
            Some("AMUX_NO_FLEET=1")
        );

        // Per-job opt-out: AMUX_<NAME>_SECS=0 disables that one loop.
        let per = |k: &str| (k == "AMUX_GHOST_RESCUE_SECS").then(|| "0".to_string());
        assert_eq!(
            isolation_reason_with("ghost-rescue", per).as_deref(),
            Some("AMUX_GHOST_RESCUE_SECS=0")
        );

        // NEGATIVE CONTROLS. A non-1 global value must NOT disable (AMUX_ISOLATED=0
        // is "off", not "on"), and a non-0 interval must NOT disable - otherwise
        // AMUX_BOARD_DRIVE_SECS=20 would silently kill a healthy loop.
        assert_eq!(
            isolation_reason_with("pane_size", |k| (k == "AMUX_ISOLATED").then(|| "0".to_string())),
            None
        );
        assert_eq!(
            isolation_reason_with("board-drive", |k| {
                (k == "AMUX_BOARD_DRIVE_SECS").then(|| "20".to_string())
            }),
            None
        );
    }

    /// The end-to-end guarantee, with its negative control in the same test:
    /// a job whose isolation switch is set REGISTERS but its body NEVER ticks,
    /// while an un-switched job spawned the same way DOES tick (prod default
    /// unchanged). The counter is observed directly, so this asserts the loop's
    /// EFFECT, not merely that a flag was read. It also asserts the suppressed
    /// job stays visible in the registry, marked disabled with its reason - the
    /// card's sharpened requirement (inert, not invisible; ethos rule 4).
    #[tokio::test]
    async fn an_isolated_periodic_job_registers_but_never_ticks() {
        // NEGATIVE CONTROL - no switch set: the body must run.
        let live_ct = Arc::new(AtomicUsize::new(0));
        let c = live_ct.clone();
        let live = spawn_periodic_every("af69-live-probe", Duration::from_millis(20), move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            live_ct.load(Ordering::SeqCst) >= 2,
            "with no isolation switch the body must run (prod default unchanged)"
        );
        live.abort();

        // Now suppress via the PER-JOB opt-out. A UNIQUE job name is used on
        // purpose: setting its AMUX_<NAME>_SECS var affects nothing else, whereas
        // the process-global AMUX_ISOLATED would disable every other spawn test
        // racing this one. The isolated code path is identical for both switches
        // (isolation_reason_with covers the global case above).
        let name = "af69-isolated-probe";
        let var = per_job_disable_var(name); // AMUX_AF69_ISOLATED_PROBE_SECS
        std::env::set_var(&var, "0");

        let iso_ct = Arc::new(AtomicUsize::new(0));
        let c = iso_ct.clone();
        let iso = spawn_periodic_every(name, Duration::from_millis(20), move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });
        tokio::time::sleep(Duration::from_millis(120)).await;

        // (1) The body did NOT run - the whole safety guarantee.
        assert_eq!(
            iso_ct.load(Ordering::SeqCst),
            0,
            "an isolated job's loop must never tick"
        );

        // (2) It is STILL registered and visible, marked disabled with its
        // reason - a suppressed fleet hazard must be inert, not invisible.
        let row = registry::snapshot()
            .into_iter()
            .find(|s| s.id == name)
            .expect("an isolated job must still appear in /api/system-jobs");
        assert_eq!(row.ticks, 0);
        assert_eq!(row.disabled_reason.as_deref(), Some("AMUX_AF69_ISOLATED_PROBE_SECS=0"));

        iso.abort();
        std::env::remove_var(&var);
    }
}
