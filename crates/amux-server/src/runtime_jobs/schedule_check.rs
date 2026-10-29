//! Pass/fail schedules: a schedule may carry a pass condition, and a sweep
//! re-runs a failing one until the condition holds (Ethan, 2026-10-26: "a sweep
//! that goes through all of the failed scheduled tasks and reconciles them until
//! they pass ... there should be a condition that's specified and easy to
//! measure").
//!
//! # The condition
//!
//! `schedules.pass_check` is a shell command. Its exit code is the verdict: 0
//! passes, anything else fails. That is the whole contract, so the condition
//! is always measurable: there is no output to parse and no model to ask. The
//! API refuses a check that bash cannot parse, and refuses `pass_fail` turned
//! on without a check, so a schedule cannot claim a condition it does not have.
//! A check that exits 126 or 127 (not executable, command not found) is
//! `unrunnable`: the condition itself is broken, and re-running the schedule
//! cannot fix it, so the sweep does not re-fire and tells the owner instead.
//!
//! # When the check runs
//!
//! Every recorded fire of the schedule (cron, run-now, a reconcile re-fire)
//! sets `pass_state=pending` and schedules the check: right away for a shell
//! schedule, after [`grace_s`] for a tmux schedule, whose worker needs time to
//! do the work. Saving a new check sets `probe`: the check runs once to show
//! the current state, and a failing probe does not fire the schedule.
//!
//! # The sweep
//!
//! [`sweep`] rides the scheduler loop (every 30s, firing mode only). For each
//! schedule whose check is due it runs the check and applies [`decide`]:
//!
//! - pass: `pass`, attempts reset.
//! - fail after a fire: re-fire the schedule (source `reconcile`), up to
//!   [`max_attempts`] in a row. A tmux re-fire carries the failing check and
//!   its output, so the worker knows what "done" means.
//! - fail with the attempts spent: wait [`cooldown_s`], tell the worker or the
//!   owner once through the schedule-failure notice, then start a new round.
//!   Bounded, so a schedule that cannot pass does not fire forever.
//!
//! A reconcile re-fire is background work, so the plan-window reserve gates it
//! like a cron fire. Every decision logs a stable verdict:
//! `schedule_check_passed`, `schedule_check_failed_refire`,
//! `schedule_check_failed_exhausted`, `schedule_check_unrunnable`,
//! `schedule_check_probe_failed`, `schedule_check_reconcile_lost`. Each change
//! of `pass_state` is written to the schedule's change history.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;

use super::scheduler::{get_schedule, insert_audit, Deliverer, DurableSchedule, RunOutcome};
use crate::db::{SharedStore, WriteOutcome};

/// The source stamped on a reconcile re-fire.
pub const SOURCE: &str = "reconcile";
/// The source of a check-driven escalation; never schedules another check.
pub const CHECK_SOURCE: &str = "check";
/// Who the change history names for the sweep's own writes.
const BY: &str = "harness:scheduler";
const TAIL_BYTES: usize = 1500;

fn env_i64(key: &str, default: i64) -> i64 {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).filter(|v: &i64| *v >= 0).unwrap_or(default)
}

/// Seconds after a tmux fire before its check runs.
pub fn grace_s() -> i64 {
    env_i64("AMUX_SCHEDULE_CHECK_GRACE_S", 900)
}
/// Reconcile re-fires per failing round.
pub fn max_attempts() -> i64 {
    env_i64("AMUX_SCHEDULE_RECONCILE_MAX", 3)
}
/// Seconds between rounds once a round's re-fires are spent.
pub fn cooldown_s() -> i64 {
    env_i64("AMUX_SCHEDULE_RECONCILE_COOLDOWN_S", 12 * 3600)
}
/// Seconds a check may run before it counts as failed.
pub fn timeout_s() -> u64 {
    env_i64("AMUX_SCHEDULE_CHECK_TIMEOUT_S", 120) as u64
}
/// A re-fire that never recorded its run is looked at again after this long.
const LOST_REFIRE_S: i64 = 3600;

/// Refuse a check that is empty or that bash cannot parse. `Ok(None)` is "no
/// check" (the caller cleared it).
pub fn validate(check: Option<&str>) -> Result<Option<String>, String> {
    let Some(c) = check.map(str::trim).filter(|c| !c.is_empty()) else {
        return Ok(None);
    };
    let out = std::process::Command::new("/bin/bash")
        .args(["-n", "-c", c])
        .output()
        .map_err(|e| format!("could not run bash to parse the check: {e}"))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().replace("bash: -c: ", "");
        return Err(format!("the pass check is not valid bash: {why}"));
    }
    Ok(Some(c.to_string()))
}

/// What one run of the check said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    /// The check could not run at all (spawn error, exit 126 or 127).
    Unrunnable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub verdict: Verdict,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub tail: String,
}

impl CheckResult {
    pub fn detail(&self) -> String {
        let head = match (self.timed_out, self.exit_code) {
            (true, _) => format!("timed out after {}s", timeout_s()),
            (false, Some(c)) => format!("exit {c}"),
            (false, None) => "did not run".to_string(),
        };
        if self.tail.trim().is_empty() {
            head
        } else {
            format!("{head}\n{}", self.tail.trim_end())
        }
    }
}

pub fn verdict_for(exit_code: Option<i32>, timed_out: bool) -> Verdict {
    match (timed_out, exit_code) {
        (true, _) => Verdict::Fail,
        (false, Some(0)) => Verdict::Pass,
        (false, Some(126 | 127)) | (false, None) => Verdict::Unrunnable,
        (false, Some(_)) => Verdict::Fail,
    }
}

/// Run `check` as the schedule's owner, bounded by [`timeout_s`].
pub async fn run_check(check: &str, owner: &str) -> CheckResult {
    run_bounded(check, owner, timeout_s(), false).await
}

/// Run `check` as `owner` for at most `secs`. `with_token` also gives it the
/// owner's worker run token, the environment a shell schedule run gets.
pub async fn run_bounded(check: &str, owner: &str, secs: u64, with_token: bool) -> CheckResult {
    let mut cmd = super::scheduler::shell_schedule_command(check);
    if owner.is_empty() {
        cmd.env_remove("AMUX_SESSION");
    } else {
        cmd.env("AMUX_SESSION", owner);
    }
    // Held until the child exits; dropping it removes the token file.
    let _run_token = (with_token && !owner.is_empty())
        .then(|| crate::api::worker_identity::mint_run(&crate::config::amux_home(), owner))
        .flatten();
    if with_token {
        match &_run_token {
            Some(t) => cmd.env(crate::api::worker_identity::TOKEN_ENV, &t.value),
            None => cmd.env_remove(crate::api::worker_identity::TOKEN_ENV),
        };
    }
    cmd.kill_on_drop(true).stdin(std::process::Stdio::null());
    let ran = tokio::time::timeout(Duration::from_secs(secs), cmd.output()).await;
    match ran {
        Err(_) => CheckResult { verdict: Verdict::Fail, exit_code: None, timed_out: true, tail: String::new() },
        Ok(Err(e)) => CheckResult {
            verdict: Verdict::Unrunnable,
            exit_code: None,
            timed_out: false,
            tail: format!("could not spawn /bin/bash: {e}"),
        },
        Ok(Ok(out)) => {
            let mut all = out.stdout;
            all.extend_from_slice(&out.stderr);
            let code = out.status.code();
            CheckResult {
                verdict: verdict_for(code, false),
                exit_code: code,
                timed_out: false,
                tail: super::scheduler::shell_output_tail(&all, 0, TAIL_BYTES),
            }
        }
    }
}

/// What the sweep does after a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Pass,
    /// Fire the schedule again; `attempt` is this round's count after the fire.
    Refire { attempt: i64 },
    /// Spent this round: look again at `at`, and tell someone.
    Exhausted { at: i64 },
    /// A probe failed: record it, fire nothing.
    ProbeFail,
    Unrunnable,
}

/// The decision, pure. `state` is the `pass_state` the check ran from.
pub fn decide(state: &str, verdict: &Verdict, attempts: i64, max: i64, now: i64, cooldown: i64) -> Decision {
    match verdict {
        Verdict::Pass => Decision::Pass,
        Verdict::Unrunnable => Decision::Unrunnable,
        Verdict::Fail if state == "probe" => Decision::ProbeFail,
        // A cooldown ran out: a new round.
        Verdict::Fail if state == "fail" => {
            if max > 0 {
                Decision::Refire { attempt: 1 }
            } else {
                Decision::Exhausted { at: now + cooldown }
            }
        }
        Verdict::Fail if attempts < max => Decision::Refire { attempt: attempts + 1 },
        Verdict::Fail => Decision::Exhausted { at: now + cooldown },
    }
}

/// A fire of `sched` was recorded: its check is now due. Called for every
/// outcome `schedule_failure::on_run_outcome` sees.
pub async fn on_fired(store: &SharedStore, sched: &DurableSchedule, source: &str) {
    if source == CHECK_SOURCE {
        return;
    }
    let id = sched.id().to_string();
    let wait = if sched.str_field("kind") == "shell" { 0 } else { grace_s() };
    let r = store
        .write_async(move |conn| {
            let now = chrono::Utc::now().timestamp();
            let old: Option<String> = conn
                .query_row(
                    "SELECT COALESCE(pass_state,'') FROM schedules WHERE id=?1 AND COALESCE(TRIM(pass_check),'')<>''",
                    [&id],
                    |r| r.get(0),
                )
                .ok();
            let Some(old) = old else {
                return Ok(WriteOutcome { applied: false, events: vec![] });
            };
            // A check already pending keeps its earlier time: a schedule that
            // fires more often than the grace would otherwise push its check
            // out on every fire and never be checked.
            conn.execute(
                "UPDATE schedules SET pass_state='pending',
                        pass_next_at=CASE WHEN pass_state='pending' AND pass_next_at IS NOT NULL
                                          THEN MIN(pass_next_at, ?1) ELSE ?1 END
                 WHERE id=?2",
                rusqlite::params![now + wait, id],
            )?;
            if old != "pending" {
                insert_audit(conn, &id, "pass_state", &old, "pending", CHECK_SOURCE, BY)?;
            }
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    if let Err(e) = r {
        tracing::warn!(schedule = %sched.id(), error = %e, verdict = "schedule_check_arm_error",
            measured = true, n_considered = 1, "could not schedule a pass check after a fire");
    }
}

/// Schedules whose check is due now.
pub fn due(conn: &Connection, now: i64) -> rusqlite::Result<Vec<DurableSchedule>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM schedules
         WHERE COALESCE(TRIM(pass_check),'')<>'' AND deleted IS NULL AND enabled=1
           AND pass_state IN ('probe','pending','fail','reconciling')
           AND pass_next_at IS NOT NULL AND pass_next_at<=?1
         ORDER BY pass_next_at LIMIT 20",
    )?;
    let ids: Vec<String> = stmt.query_map([now], |r| r.get(0))?.collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for id in ids {
        if let Some(s) = get_schedule(conn, &id)? {
            out.push(s);
        }
    }
    Ok(out)
}

/// Record a check's verdict and the decision on the row.
fn record(
    conn: &Connection,
    id: &str,
    old_state: &str,
    decision: &Decision,
    result: &CheckResult,
    now: i64,
) -> rusqlite::Result<()> {
    let (state, next_at, attempts): (&str, Option<i64>, Option<i64>) = match decision {
        Decision::Pass => ("pass", None, Some(0)),
        // The re-fire's own run moves it to `pending`; until then the sweep
        // leaves it alone, and looks again if that never happens.
        Decision::Refire { attempt } => ("reconciling", Some(now + LOST_REFIRE_S), Some(*attempt)),
        Decision::Exhausted { at } => ("fail", Some(*at), None),
        Decision::ProbeFail => ("fail", None, None),
        Decision::Unrunnable => ("unrunnable", None, None),
    };
    let detail: String = result.detail().chars().take(2000).collect();
    conn.execute(
        "UPDATE schedules SET pass_state=?1, pass_next_at=?2, pass_attempts=COALESCE(?3, pass_attempts),
                pass_checked_at=?4, pass_detail=?5
         WHERE id=?6",
        rusqlite::params![state, next_at, attempts, now, detail, id],
    )?;
    if old_state != state {
        insert_audit(conn, id, "pass_state", old_state, state, CHECK_SOURCE, BY)?;
    }
    Ok(())
}

/// The text a tmux worker receives on a reconcile re-fire.
pub fn reconcile_command(sched: &DurableSchedule, attempt: i64, max: i64, result: &CheckResult) -> String {
    let check = sched.str_field("pass_check").trim();
    format!(
        "[amux] Reconcile {attempt}/{max}: this schedule's pass check failed after the last run. \
         It is done when this command exits 0:\n\n    {check}\n\nLast check: {}\n\n\
         Do the work below until the check passes.\n\n{}",
        result.detail(),
        sched.str_field("command")
    )
}

static SWEEPING: AtomicBool = AtomicBool::new(false);

/// One sweep, in the background so a slow check does not hold the scheduler
/// tick. A sweep still running when the next tick comes is not doubled.
pub fn spawn_sweep(store: SharedStore, deliverer: Arc<dyn Deliverer>) {
    if SWEEPING.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                SWEEPING.store(false, Ordering::SeqCst);
            }
        }
        let _reset = Reset;
        if let Err(e) = sweep(&store, deliverer).await {
            tracing::warn!(error = %e, verdict = "schedule_check_sweep_error", measured = false,
                n_considered = 0, "pass-check sweep failed");
        }
    });
}

pub async fn sweep(store: &SharedStore, deliverer: Arc<dyn Deliverer>) -> anyhow::Result<usize> {
    let now = chrono::Utc::now().timestamp();
    let st = store.clone();
    let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = st.read()?;
        Ok(due(&conn, now)?)
    })
    .await??;
    let n = rows.len();
    for sched in rows {
        check_one(store, &deliverer, sched).await;
    }
    Ok(n)
}

/// Check one due schedule and act on the verdict.
pub async fn check_one(store: &SharedStore, deliverer: &Arc<dyn Deliverer>, sched: DurableSchedule) {
    let id = sched.id().to_string();
    let old_state = sched.str_field("pass_state").to_string();
    if old_state == "reconciling" {
        tracing::warn!(schedule = %id, verdict = "schedule_check_reconcile_lost", measured = true, n_considered = 1,
            "a reconcile re-fire never recorded its run; checking again");
    }
    let result = run_check(sched.str_field("pass_check"), sched.str_field("session").trim()).await;
    let now = chrono::Utc::now().timestamp();
    let max = max_attempts();
    let decision = decide(&old_state, &result.verdict, sched.i64_field("pass_attempts", 0), max, now, cooldown_s());
    let (id_w, old_w, dec_w, res_w) = (id.clone(), old_state.clone(), decision.clone(), result.clone());
    let wrote = store
        .write_async(move |conn| {
            record(conn, &id_w, &old_w, &dec_w, &res_w, now)?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    if let Err(e) = wrote {
        tracing::warn!(schedule = %id, error = %e, verdict = "schedule_check_record_error", measured = true,
            n_considered = 1, "could not record a pass check; the next sweep retries it");
        return;
    }
    let detail = result.detail();
    match decision {
        Decision::Pass => tracing::info!(schedule = %id, from = %old_state, verdict = "schedule_check_passed",
            measured = true, n_considered = 1, "pass check passed"),
        Decision::ProbeFail => tracing::info!(schedule = %id, verdict = "schedule_check_probe_failed",
            measured = true, n_considered = 1, detail = %detail, "pass check fails now; the next fire is reconciled"),
        Decision::Refire { attempt } => {
            tracing::warn!(schedule = %id, attempt, max, verdict = "schedule_check_failed_refire", measured = true,
                n_considered = 1, detail = %detail, "pass check failed; firing the schedule again");
            let mut fire = sched.clone();
            if fire.str_field("kind") != "shell" {
                fire.set("command", serde_json::json!(reconcile_command(&sched, attempt, max, &result)));
            }
            let outcome = deliverer.deliver(&fire, SOURCE).await;
            let (sid, o) = (id.clone(), outcome.clone());
            let rec = store
                .write_async(move |conn| {
                    super::scheduler::record_run(conn, &sid, &o, SOURCE)?;
                    Ok(WriteOutcome { applied: true, events: vec![] })
                })
                .await;
            if let Err(e) = rec {
                tracing::warn!(schedule = %id, error = %e, verdict = "schedule_check_record_error", measured = true,
                    n_considered = 1, "could not record a reconcile re-fire");
            }
            super::schedule_failure::on_run_outcome(store, &sched, &outcome, SOURCE).await;
        }
        Decision::Exhausted { at } => {
            tracing::warn!(schedule = %id, attempts = max, next_round_at = at, verdict = "schedule_check_failed_exhausted",
                measured = true, n_considered = 1, detail = %detail, "pass check still fails after every re-fire this round");
            let reason = format!(
                "pass check still fails after {max} reconcile re-fire(s); next round in {}h. Check: {}\n{detail}",
                cooldown_s() / 3600,
                sched.str_field("pass_check").trim()
            );
            super::schedule_failure::on_run_outcome(store, &sched, &RunOutcome::Failed { reason }, CHECK_SOURCE).await;
        }
        Decision::Unrunnable => {
            tracing::warn!(schedule = %id, verdict = "schedule_check_unrunnable", measured = true, n_considered = 1,
                detail = %detail, "the pass check itself cannot run; not re-firing");
            if old_state != "unrunnable" {
                let reason = format!(
                    "the pass check cannot run ({detail}); fix the check. Check: {}",
                    sched.str_field("pass_check").trim()
                );
                super::schedule_failure::on_run_outcome(store, &sched, &RunOutcome::Failed { reason }, CHECK_SOURCE).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_is_the_verdict() {
        assert_eq!(verdict_for(Some(0), false), Verdict::Pass);
        assert_eq!(verdict_for(Some(1), false), Verdict::Fail);
        assert_eq!(verdict_for(Some(0), true), Verdict::Fail, "a timeout fails even if it would have passed");
        assert_eq!(verdict_for(Some(127), false), Verdict::Unrunnable);
        assert_eq!(verdict_for(Some(126), false), Verdict::Unrunnable);
        assert_eq!(verdict_for(None, false), Verdict::Unrunnable);
    }

    #[test]
    fn a_failing_check_refires_until_the_round_is_spent_then_waits() {
        let (now, cd) = (1000, 500);
        assert_eq!(decide("pending", &Verdict::Fail, 0, 3, now, cd), Decision::Refire { attempt: 1 });
        assert_eq!(decide("pending", &Verdict::Fail, 2, 3, now, cd), Decision::Refire { attempt: 3 });
        assert_eq!(decide("pending", &Verdict::Fail, 3, 3, now, cd), Decision::Exhausted { at: 1500 });
        // A lost re-fire is judged like a fire.
        assert_eq!(decide("reconciling", &Verdict::Fail, 1, 3, now, cd), Decision::Refire { attempt: 2 });
        // The cooldown ran out: a new round starts at 1.
        assert_eq!(decide("fail", &Verdict::Fail, 3, 3, now, cd), Decision::Refire { attempt: 1 });
        assert_eq!(decide("fail", &Verdict::Fail, 0, 0, now, cd), Decision::Exhausted { at: 1500 });
    }

    #[test]
    fn a_probe_never_fires_and_a_pass_or_broken_check_ends_the_round() {
        assert_eq!(decide("probe", &Verdict::Fail, 0, 3, 0, 1), Decision::ProbeFail);
        assert_eq!(decide("pending", &Verdict::Pass, 3, 3, 0, 1), Decision::Pass);
        assert_eq!(decide("pending", &Verdict::Unrunnable, 0, 3, 0, 1), Decision::Unrunnable);
    }

    #[test]
    fn validate_refuses_what_bash_cannot_parse() {
        assert_eq!(validate(None), Ok(None));
        assert_eq!(validate(Some("   ")), Ok(None));
        assert_eq!(validate(Some(" test -f /tmp/x ")), Ok(Some("test -f /tmp/x".into())));
        let e = validate(Some("if then fi (")).unwrap_err();
        assert!(e.contains("not valid bash"), "{e}");
    }

    /// A deliverer standing in for the worker: it does the work (touches the
    /// marker) only on the `fix_on`-th fire, and counts every fire.
    struct Worker {
        fires: std::sync::atomic::AtomicUsize,
        fix_on: usize,
        marker: std::path::PathBuf,
    }

    #[async_trait::async_trait]
    impl Deliverer for Worker {
        async fn deliver(&self, sched: &DurableSchedule, source: &str) -> RunOutcome {
            assert_eq!(source, SOURCE);
            assert_eq!(sched.str_field("command"), "do the work", "a shell re-fire runs its own command");
            let n = self.fires.fetch_add(1, Ordering::SeqCst) + 1;
            if n == self.fix_on {
                std::fs::write(&self.marker, "done").unwrap();
            }
            RunOutcome::ShellOk { note: None, output: None }
        }
    }

    async fn store_with_schedule(dir: &std::path::Path, marker: &std::path::Path) -> SharedStore {
        let store: SharedStore = Arc::new(crate::db::Store::open(&dir.join("check.db")).unwrap());
        let mut m = serde_json::Map::new();
        for (k, v) in [
            ("id", serde_json::json!("SCHED-1")),
            ("title", serde_json::json!("make the report")),
            ("session", serde_json::json!("")),
            ("command", serde_json::json!("do the work")),
            ("kind", serde_json::json!("shell")),
            ("enabled", serde_json::json!(1)),
            ("created", serde_json::json!(1)),
            ("updated", serde_json::json!(1)),
            ("pass_check", serde_json::json!(format!("test -f '{}'", marker.display()))),
            // A fire was just recorded: the check is due.
            ("pass_state", serde_json::json!("pending")),
            ("pass_next_at", serde_json::json!(0)),
        ] {
            m.insert(k.into(), v);
        }
        let row = DurableSchedule::from_map(m);
        store
            .write_async(move |conn| {
                super::super::scheduler::insert_schedule(conn, &row)?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
        store
    }

    fn row(store: &SharedStore) -> (String, i64, Option<i64>) {
        store
            .read()
            .unwrap()
            .query_row("SELECT pass_state, pass_attempts, pass_next_at FROM schedules WHERE id='SCHED-1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap()
    }

    #[tokio::test]
    async fn the_sweep_refires_a_failing_schedule_until_its_check_passes() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("report.md");
        let store = store_with_schedule(dir.path(), &marker).await;
        let worker = Arc::new(Worker { fires: 0.into(), fix_on: 2, marker });
        let d: Arc<dyn Deliverer> = worker.clone();
        // fail -> re-fire 1; fail -> re-fire 2 (does the work); pass.
        for _ in 0..3 {
            assert_eq!(sweep(&store, d.clone()).await.unwrap(), 1);
        }
        assert_eq!(worker.fires.load(Ordering::SeqCst), 2);
        assert_eq!(row(&store), ("pass".into(), 0, None));
        // Nothing is due once it passes.
        assert_eq!(sweep(&store, d.clone()).await.unwrap(), 0);
        let conn = store.read().unwrap();
        let trail: Vec<String> = conn
            .prepare("SELECT new_value FROM schedule_audit WHERE schedule_id='SCHED-1' AND field='pass_state' ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(trail, ["reconciling", "pending", "reconciling", "pending", "pass"]);
        let reconcile_runs: i64 = conn
            .query_row("SELECT COUNT(*) FROM schedule_runs WHERE source='reconcile'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(reconcile_runs, 2, "every re-fire is a recorded run");
    }

    #[tokio::test]
    async fn a_schedule_that_cannot_pass_stops_refiring_after_the_round() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("never.md");
        let store = store_with_schedule(dir.path(), &marker).await;
        let worker = Arc::new(Worker { fires: 0.into(), fix_on: usize::MAX, marker });
        let d: Arc<dyn Deliverer> = worker.clone();
        for _ in 0..6 {
            sweep(&store, d.clone()).await.unwrap();
        }
        assert_eq!(worker.fires.load(Ordering::SeqCst) as i64, max_attempts());
        let (state, attempts, next_at) = row(&store);
        assert_eq!((state.as_str(), attempts), ("fail", max_attempts()));
        let wait = next_at.unwrap() - chrono::Utc::now().timestamp();
        assert!((cooldown_s() - 60..=cooldown_s()).contains(&wait), "next round after the cooldown, got {wait}s");
    }

    /// A tmux schedule firing every 5 minutes against a 15-minute grace: the
    /// second fire must not push the pending check out again.
    #[tokio::test]
    async fn a_frequent_fire_does_not_postpone_a_pending_check() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_schedule(dir.path(), &dir.path().join("x")).await;
        store
            .write_async(|conn| {
                conn.execute("UPDATE schedules SET kind='tmux', pass_state='pass', pass_next_at=NULL", [])?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
        let sched = store.read().map(|c| get_schedule(&c, "SCHED-1").unwrap().unwrap()).unwrap();
        on_fired(&store, &sched, "cron-rs").await;
        let first = row(&store).2.unwrap();
        assert!(first > chrono::Utc::now().timestamp() + 60, "a tmux check waits out the grace");
        // The first fire was earlier: its check comes due sooner than a new
        // fire's would.
        let earlier = first - 600;
        store
            .write_async(move |conn| {
                conn.execute("UPDATE schedules SET pass_next_at=?1", [earlier])?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
        on_fired(&store, &sched, "cron-rs").await;
        assert_eq!(row(&store).2.unwrap(), earlier, "the check stays due at the first fire's time");
        assert_eq!(row(&store).0, "pending");
        // A check-driven escalation is not a fire.
        store
            .write_async(|conn| {
                conn.execute("UPDATE schedules SET pass_state='fail', pass_next_at=NULL", [])?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
        on_fired(&store, &sched, CHECK_SOURCE).await;
        assert_eq!(row(&store).0, "fail");
    }

    #[tokio::test]
    async fn run_check_reports_exit_code_and_output() {
        let r = run_check("echo nope; exit 3", "").await;
        assert_eq!(r.verdict, Verdict::Fail);
        assert_eq!(r.exit_code, Some(3));
        assert!(r.detail().starts_with("exit 3\nnope"), "{}", r.detail());
        assert_eq!(run_check("true", "").await.verdict, Verdict::Pass);
        assert_eq!(run_check("definitely-not-a-command-xyz", "").await.verdict, Verdict::Unrunnable);
    }
}
