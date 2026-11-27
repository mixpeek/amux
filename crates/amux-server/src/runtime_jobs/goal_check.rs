//! Goal checker: the worker's own definition of "done", run by amux as shell.
//!
//! # Why this exists
//!
//! mxp-gs12, 2026-10-01 to 10-18. The owner asked "is gs12 progressing?" 84 to
//! 86 times; an hourly model-run check logged pace BEHIND for a week and told
//! only the orchestrator; three progress counts competed. Then, at 10-17
//! 22:54Z, an authentication failure made Claude Code clear the `/goal`, which
//! it records as `met: true`. amux read that as "goal met", the dashboard said
//! done and the goal keeper stopped, while the worker's own checker still said
//! 44 of 56 rows verified.
//!
//! The worker already had the right instrument: a script that prints the
//! finish-line count and knows whether the goal is met. This module runs it.
//!
//! # Contract
//!
//! `AMUX_GOAL_CHECK` (scoped worker > group > global, process env wins) is a
//! shell command. Exit 0 means met, exit 1 means not met, anything else (or a
//! timeout, or a spawn error) means UNKNOWN and says why. Progress comes from
//! the output: a JSON line `{"done":44,"total":56,"summary":"..."}`, or the
//! first `N/M` or `N of M`. No model call (ethos rule 2).
//!
//! # It is a schedule
//!
//! The key creates an ordinary `kind: shell` schedule owned by the worker,
//! `managed_by = goal-check` (migration 0181), run every
//! `AMUX_GOAL_CHECK_INTERVAL_S` (600 s, whole minutes) in the worker's
//! directory. So a check has run history, an audit trail, failure notices and
//! a row in the Schedules tab, where the owner can run it now or pause it.
//! [`reconcile`] keeps the row in step with the key on every goal-keeper tick:
//! it creates it once, rewrites a changed command or interval, pauses it while
//! the worker is stopped (`disabled_reason = goal_check_worker_stopped`) and
//! resumes it when the worker runs, and soft-deletes it when the key is
//! cleared. Re-reading the key never makes a second row (a unique index on
//! `(session, managed_by)` backs that). The scheduler's 600 s shell limit
//! bounds a run. Exit 1 maps to `log` in `exit_actions`, so an unmet goal is a
//! reading and never a failed run; an UNKNOWN exit is a failed run, and the
//! worker hears about it through the schedule-failure notice.
//!
//! [`sync`] then reads each schedule's newest finished run, folds it into the
//! per-worker state and the goal samples, and raises the stall alert.
//! Verdicts: `goal_check_schedule_created`, `_updated`, `_paused`, `_resumed`,
//! `_removed`, `goal_check_ran`, `goal_check_reconcile_failed`,
//! `goal_check_read_failed`, `goal_check_reconcile_skipped`.
//!
//! # What reads it
//!
//! - The goal payload on `GET /api/sessions` ([`super::goal_keeper::goal_payload`]):
//!   `checker` plus a `state` the checker decides. The checker outranks Claude
//!   Code's evaluator: "met" is the checker's word when one is configured.
//! - The goal keeper, which keeps continuing a worker whose checker says not
//!   met, even after an error cleared the `/goal`.
//! - The stall alert: when the progress has not moved for `AMUX_GOAL_STALL_H`
//!   (6 h), or the checker has been unreadable that long, the owner gets one
//!   push and one email per episode (verdict `goal_stall_alerted`).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const KEY: &str = "AMUX_GOAL_CHECK";

/// The last checker run, as stored and as served.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Last {
    pub checked_at: f64,
    /// Did the checker produce a verdict (exit 0 or 1)?
    pub measured: bool,
    pub met: Option<bool>,
    pub done: Option<u64>,
    pub total: Option<u64>,
    pub summary: String,
    pub exit: Option<i32>,
    pub why_unmeasured: Option<String>,
}

/// Per-worker checker state, persisted so a server restart (the builder
/// restarts it on every commit) does not reset the stall clock.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub last: Option<Last>,
    /// What counts as movement: `done/total` when the checker prints one,
    /// else its summary line.
    pub progress_key: String,
    pub progress_changed_at: f64,
    /// When the checker started failing to produce a verdict; 0 when it is
    /// producing one.
    pub unknown_since: f64,
    pub stall_alerted_at: f64,
    #[serde(default)]
    pub runs: u64,
    /// The worker's goal-check schedule (`managed_by = goal-check`).
    #[serde(default)]
    pub schedule_id: String,
    #[serde(default)]
    pub schedule_enabled: bool,
    #[serde(default)]
    pub schedule_disabled_reason: String,
    /// The newest `schedule_runs.id` already folded in, so a run counts once.
    #[serde(default)]
    pub last_run_id: i64,
}

fn setting(home: &Path, name: &str, key: &str) -> Option<String> {
    crate::environment::resolve_in(home, crate::environment::Scope::Lane(name), key, crate::environment::process(key).as_deref())
        .map(|r| r.value.trim().to_string())
}

fn setting_f64(home: &Path, name: &str, key: &str, d: f64) -> f64 {
    setting(home, name, key).and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// The configured checker command, cached 30 s per worker: the session list
/// asks for every worker on every poll.
pub fn command(home: &Path, name: &str) -> Option<String> {
    type C = Mutex<HashMap<(PathBuf, String), (std::time::Instant, Option<String>)>>;
    static CACHE: std::sync::OnceLock<C> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let k = (home.to_path_buf(), name.to_string());
    if let Ok(m) = cache.lock() {
        if let Some((at, v)) = m.get(&k) {
            if at.elapsed() < std::time::Duration::from_secs(30) {
                return v.clone();
            }
        }
    }
    let v = setting(home, name, KEY);
    if let Ok(mut m) = cache.lock() {
        m.insert(k, (std::time::Instant::now(), v.clone()));
    }
    v
}

fn state_path(home: &Path, name: &str) -> PathBuf {
    home.join("goal-check").join(format!("{name}.json"))
}

fn states() -> &'static Mutex<HashMap<PathBuf, State>> {
    static S: std::sync::OnceLock<Mutex<HashMap<PathBuf, State>>> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

/// The worker's checker state: memory first, then the persisted file.
pub fn load(home: &Path, name: &str) -> State {
    let p = state_path(home, name);
    if let Ok(m) = states().lock() {
        if let Some(s) = m.get(&p) {
            return s.clone();
        }
    }
    let s: State = std::fs::read(&p)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if let Ok(mut m) = states().lock() {
        m.insert(p, s.clone());
    }
    s
}

fn store(home: &Path, name: &str, s: &State) {
    let p = state_path(home, name);
    if let Ok(mut m) = states().lock() {
        m.insert(p.clone(), s.clone());
    }
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = p.with_extension(format!("json.tmp{}", std::process::id()));
    if let Ok(b) = serde_json::to_vec_pretty(s) {
        if std::fs::write(&tmp, b).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(n).collect::<String>())
    }
}

/// Turn one checker run into a verdict. Pure.
pub fn parse(exit: Option<i32>, stdout: &str, stderr: &str, now: f64) -> Last {
    let mut last = Last { checked_at: now, exit, ..Default::default() };
    match exit {
        Some(0) => {
            last.measured = true;
            last.met = Some(true);
        }
        Some(1) => {
            last.measured = true;
            last.met = Some(false);
        }
        Some(c) => {
            let tail = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
            last.why_unmeasured = Some(format!(
                "checker exited {c} (0 = met, 1 = not met, anything else = could not decide){}",
                if tail.is_empty() { String::new() } else { format!(": {}", clip(tail, 160)) }
            ));
        }
        None => last.why_unmeasured = Some("checker produced no exit status".into()),
    }
    // Progress: a JSON line wins, else the first N/M or N of M.
    let json_line = stdout
        .lines()
        .rev()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
        .find_map(|l| serde_json::from_str::<Value>(l).ok());
    if let Some(v) = json_line {
        last.done = v["done"].as_u64();
        last.total = v["total"].as_u64();
        if let Some(s) = v["summary"].as_str() {
            last.summary = clip(s, 200);
        }
    }
    if last.done.is_none() {
        static RX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let rx = RX.get_or_init(|| regex::Regex::new(r"(\d+)\s*(?:/|\bof\b)\s*(\d+)").expect("regex"));
        if let Some(c) = rx.captures(stdout) {
            last.done = c[1].parse().ok();
            last.total = c[2].parse().ok();
        }
    }
    if last.summary.is_empty() {
        last.summary = stdout
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('{'))
            .map(|l| clip(l, 200))
            .unwrap_or_default();
    }
    last
}

/// Fold a run into the state: movement, the unknown clock. Pure.
pub fn fold(mut st: State, last: Last) -> State {
    let now = last.checked_at;
    st.runs += 1;
    if last.measured {
        st.unknown_since = 0.0;
        let key = match (last.done, last.total) {
            (Some(d), Some(t)) => format!("{d}/{t}"),
            (Some(d), None) => d.to_string(),
            _ => last.summary.clone(),
        };
        let key = format!("{}|{key}", if last.met == Some(true) { "met" } else { "unmet" });
        if key != st.progress_key || st.progress_changed_at <= 0.0 {
            st.progress_key = key;
            st.progress_changed_at = now;
        }
    } else if st.unknown_since <= 0.0 {
        st.unknown_since = now;
    }
    st.last = Some(last);
    st
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stall {
    /// Not met, and the progress has not moved for the stall window.
    NoMovement,
    /// The checker has not produced a verdict for the stall window.
    Unreadable,
}

/// Is an owner alert due? Once per episode: movement, or the checker coming
/// back, starts a new one. Pure.
pub fn stall_due(st: &State, now: f64, stall_s: f64) -> Option<Stall> {
    if stall_s <= 0.0 {
        return None;
    }
    let last = st.last.as_ref()?;
    if last.measured {
        (last.met == Some(false)
            && st.progress_changed_at > 0.0
            && now - st.progress_changed_at >= stall_s
            && st.stall_alerted_at < st.progress_changed_at)
            .then_some(Stall::NoMovement)
    } else {
        (st.unknown_since > 0.0 && now - st.unknown_since >= stall_s && st.stall_alerted_at < st.unknown_since)
            .then_some(Stall::Unreadable)
    }
}

pub fn stall_s(home: &Path, name: &str) -> f64 {
    setting_f64(home, name, "AMUX_GOAL_STALL_H", 6.0) * 3600.0
}

/// The `checker` field of the goal payload. `configured: false` when no
/// checker is set, so the dashboard can tell "none" from "not run yet".
pub fn view(home: &Path, name: &str, now: f64) -> Value {
    let Some(cmd) = command(home, name) else {
        return json!({"configured": false});
    };
    let st = load(home, name);
    let stall = stall_s(home, name);
    let Some(last) = st.last.as_ref() else {
        let why = if st.schedule_id.is_empty() {
            "the checker has not run yet: its schedule is created on the goal keeper's next tick".to_string()
        } else {
            format!("the checker has not run yet: schedule {} runs it every AMUX_GOAL_CHECK_INTERVAL_S while the worker is running", st.schedule_id)
        };
        return json!({
            "configured": true, "measured": false, "command": clip(&cmd, 120),
            "why_unmeasured": why, "schedule": schedule_view(&st),
        });
    };
    let stalled = stall_due(&State { stall_alerted_at: 0.0, ..st.clone() }, now, stall).is_some();
    json!({
        "configured": true,
        "command": clip(&cmd, 120),
        "measured": last.measured,
        "met": last.met,
        "done": last.done,
        "total": last.total,
        "summary": last.summary,
        "exit": last.exit,
        "why_unmeasured": last.why_unmeasured,
        "checked_at": last.checked_at,
        "progress_changed_at": st.progress_changed_at,
        "unknown_since": if st.unknown_since > 0.0 { json!(st.unknown_since) } else { Value::Null },
        "stalled": stalled,
        "stall_after_s": stall,
        "stall_alerted_at": if st.stall_alerted_at > 0.0 { json!(st.stall_alerted_at) } else { Value::Null },
        "runs": st.runs,
        "schedule": schedule_view(&st),
    })
}

/// Where the check runs, for the goal tile: the schedule and whether it fires.
fn schedule_view(st: &State) -> Value {
    if st.schedule_id.is_empty() {
        return Value::Null;
    }
    json!({
        "id": st.schedule_id,
        "enabled": st.schedule_enabled,
        "disabled_reason": if st.schedule_disabled_reason.is_empty() { Value::Null } else { json!(st.schedule_disabled_reason) },
    })
}

/// How long [`record_sample`] keeps a reading. Bounds the table and the
/// goal trend's "peak" (`api::velocity::goal_trend`), a peak over this window.
pub const SAMPLE_RETENTION_DAYS: f64 = 14.0;

/// Keep one run for the goal trend's history and trim what has aged out.
/// Returns false when the run carries no number to keep: a run with no verdict
/// or no `done` stores nothing, so the gap reads as unmeasured, never as zero.
pub fn record_sample(conn: &rusqlite::Connection, name: &str, last: &Last) -> rusqlite::Result<bool> {
    let Some(done) = last.done.filter(|_| last.measured) else {
        return Ok(false);
    };
    conn.execute(
        "INSERT OR REPLACE INTO goal_samples (session, at, done, total, met) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![name, last.checked_at, done as i64, last.total.map(|t| t as i64), last.met.map(i64::from)],
    )?;
    conn.execute(
        "DELETE FROM goal_samples WHERE at < ?1",
        [last.checked_at - SAMPLE_RETENTION_DAYS * 86_400.0],
    )?;
    Ok(true)
}

/// [`record_sample`] off the runtime, with a failure a log sweep can find.
async fn record_sample_logged(app: &crate::api::AppState, name: &str, last: &Last) {
    let (store, n, l) = (app.store.clone(), name.to_string(), last.clone());
    let wrote = tokio::task::spawn_blocking(move || {
        store.write(move |conn| {
            let applied = record_sample(conn, &n, &l)?;
            Ok(crate::db::WriteOutcome { applied, events: vec![] })
        })
    })
    .await;
    if let Err(why) = wrote.map_err(|e| e.to_string()).and_then(|r| r.map(|_| ()).map_err(|e| e.to_string())) {
        tracing::warn!(target: "amux::goal_check", session = %name, verdict = "goal_sample_write_failed",
            measured = false, n_considered = 1, why_unmeasured = %why,
            "goal checker: the reading was not retained, so the goal trend has a gap here");
    }
}

/// `schedules.managed_by` on a worker's goal-check schedule (migration 0181).
pub const MANAGED_BY: &str = "goal-check";
/// The proven creator stamped on it: an owner-set configuration key, which
/// `schedule_authority::decide` admits as `policy:`.
pub const CREATOR: &str = "policy:AMUX_GOAL_CHECK";
/// `disabled_reason` while the worker is not running. Only a row paused for
/// this reason is resumed by the reconcile; an owner's or worker's pause stays.
pub const PAUSED_REASON: &str = "goal_check_worker_stopped";
/// The audit trail's `source` for the reconcile's own writes.
const AUDIT_SOURCE: &str = "goal-check";
/// A not-met goal (exit 1) is a reading, never a failed run: without this the
/// schedule-failure notice would fire on every unmet check.
pub const EXIT_ACTIONS: &str = r#"{"1":"log"}"#;

/// What one worker's goal-check schedule should look like.
#[derive(Debug, Clone, PartialEq)]
pub struct Want {
    pub session: String,
    /// The schedule's command: [`compose`] of the check and the directory.
    pub command: String,
    pub expr: String,
    /// Fire only while the worker runs, as the checker always has.
    pub running: bool,
}

/// The shell a goal check runs as: in the worker's directory, with
/// `AMUX_GOAL_CHECK_RUN=1`, then the check exactly as configured. A directory
/// that no longer exists stops the run (exit 2 reads UNKNOWN with cd's reason)
/// instead of reading another checkout.
pub fn compose(check: &str, dir: &str) -> String {
    let mut s = String::new();
    if !dir.is_empty() && Path::new(dir).is_dir() {
        s.push_str(&format!("cd -- '{}' || exit 2\n", dir.replace('\'', "'\\''")));
    }
    s.push_str("export AMUX_GOAL_CHECK_RUN=1\n");
    s.push_str(check.trim());
    s
}

/// `AMUX_GOAL_CHECK_INTERVAL_S` as a schedule expression, whole minutes (the
/// scheduler's clock), at least one.
pub fn interval_expr(interval_s: f64) -> String {
    let m = if interval_s.is_finite() { (interval_s / 60.0).ceil().max(1.0) as i64 } else { 10 };
    format!("every {m}m")
}

/// The schedule a worker should have, or None when it has no check.
pub fn want(home: &Path, name: &str, dir: &str, running: bool) -> Option<Want> {
    let check = command(home, name)?;
    if check.trim().is_empty() {
        return None;
    }
    let interval = setting_f64(home, name, "AMUX_GOAL_CHECK_INTERVAL_S", 600.0);
    Some(Want { session: name.to_string(), command: compose(&check, dir), expr: interval_expr(interval), running })
}

/// One write the reconcile made, for its log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Created { id: String, session: String },
    Updated { id: String, session: String, fields: Vec<&'static str> },
    Paused { id: String, session: String },
    Resumed { id: String, session: String },
    Removed { id: String, session: String },
}

impl Change {
    pub fn verdict(&self) -> &'static str {
        match self {
            Change::Created { .. } => "goal_check_schedule_created",
            Change::Updated { .. } => "goal_check_schedule_updated",
            Change::Paused { .. } => "goal_check_schedule_paused",
            Change::Resumed { .. } => "goal_check_schedule_resumed",
            Change::Removed { .. } => "goal_check_schedule_removed",
        }
    }
    fn ids(&self) -> (&str, &str) {
        match self {
            Change::Created { id, session }
            | Change::Updated { id, session, .. }
            | Change::Paused { id, session }
            | Change::Resumed { id, session }
            | Change::Removed { id, session } => (id, session),
        }
    }
}

struct Row {
    id: String,
    session: String,
    command: String,
    expr: String,
    exit_actions: String,
    enabled: bool,
    disabled_reason: String,
    locked: bool,
}

fn managed_rows(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<Row>> {
    let mut st = conn.prepare(
        "SELECT id, session, command, COALESCE(schedule_expr,''), COALESCE(exit_actions,''), enabled,
                COALESCE(disabled_reason,''), COALESCE(locked_by,'')
           FROM schedules WHERE managed_by=?1 AND deleted IS NULL ORDER BY id",
    )?;
    let rows = st.query_map([MANAGED_BY], |r| {
        Ok(Row {
            id: r.get(0)?,
            session: r.get(1)?,
            command: r.get(2)?,
            expr: r.get(3)?,
            exit_actions: r.get(4)?,
            enabled: r.get::<_, i64>(5)? != 0,
            disabled_reason: r.get(6)?,
            locked: !r.get::<_, String>(7)?.is_empty(),
        })
    })?;
    rows.collect()
}

/// One audited write by the key: `schedule_audit` keeps the values, and the
/// ledger names `policy:AMUX_GOAL_CHECK` as the proven actor.
fn audit(conn: &rusqlite::Connection, id: &str, field: &str, old: &str, new: &str) -> rusqlite::Result<()> {
    let principal = crate::api::ledger::Principal {
        actor: CREATOR.to_string(),
        claimed: None,
        on_behalf_of: None,
        authority: "owner_policy".to_string(),
    };
    super::scheduler::insert_audit_as(conn, id, field, old, new, AUDIT_SOURCE, CREATOR, Some(&principal))
}

fn next_run_now(now: chrono::DateTime<chrono::Local>) -> String {
    super::scheduler::fmt_minute(now)
}

/// Bring every goal-check schedule in step with `wants`, in one transaction:
/// create the missing, rewrite a changed command or interval, pause a stopped
/// worker's and resume it when it runs again, and soft-delete the schedule of
/// a worker whose check was cleared. Idempotent: a second call with the same
/// wants writes nothing. Every write leaves a `schedule_audit` row by
/// [`CREATOR`].
pub fn reconcile(
    conn: &rusqlite::Connection,
    wants: &[Want],
    now: chrono::DateTime<chrono::Local>,
) -> rusqlite::Result<Vec<Change>> {
    use super::scheduler::{insert_schedule, mint_schedule_id, soft_delete_schedule, DurableSchedule};
    let now_ts = now.timestamp();
    let rows = managed_rows(conn)?;
    let mut out = Vec::new();
    for w in wants {
        let Some(row) = rows.iter().find(|r| r.session == w.session) else {
            let id = mint_schedule_id(conn)?;
            let mut m = serde_json::Map::new();
            let nr = next_run_now(now);
            for (k, v) in [
                ("id", json!(id)),
                ("title", json!(format!("Goal check: {}", w.session))),
                ("session", json!(w.session)),
                ("command", json!(w.command)),
                ("kind", json!("shell")),
                ("sched_type", json!("recurring")),
                ("schedule_expr", json!(w.expr)),
                ("run_at", json!(nr)),
                ("next_run", json!(nr)),
                ("enabled", json!(i64::from(w.running))),
                ("disabled_reason", if w.running { Value::Null } else { json!(PAUSED_REASON) }),
                ("exit_actions", json!(EXIT_ACTIONS)),
                ("created", json!(now_ts)),
                ("updated", json!(now_ts)),
                ("created_by", json!(CREATOR)),
                ("created_by_verified", json!(1)),
                ("managed_by", json!(MANAGED_BY)),
            ] {
                m.insert(k.into(), v);
            }
            insert_schedule(conn, &DurableSchedule::from_map(m))?;
            let summary = json!({"title": format!("Goal check: {}", w.session), "session": w.session,
                "expr": w.expr, "enabled": i64::from(w.running), "kind": "shell", "managed_by": MANAGED_BY});
            audit(conn, &id, "created", "", &summary.to_string())?;
            out.push(Change::Created { id, session: w.session.clone() });
            continue;
        };
        let mut fields = Vec::new();
        for (col, old, new) in [
            ("command", &row.command, &w.command),
            ("schedule_expr", &row.expr, &w.expr),
            ("exit_actions", &row.exit_actions, &EXIT_ACTIONS.to_string()),
        ] {
            if old != new {
                conn.execute(&format!("UPDATE schedules SET {col}=?1, updated=?2 WHERE id=?3"), rusqlite::params![new, now_ts, row.id])?;
                audit(conn, &row.id, col, old, new)?;
                fields.push(col);
            }
        }
        if !fields.is_empty() {
            out.push(Change::Updated { id: row.id.clone(), session: w.session.clone(), fields });
        }
        if row.enabled && !w.running {
            conn.execute(
                "UPDATE schedules SET enabled=0, disabled_reason=?1, updated=?2 WHERE id=?3",
                rusqlite::params![PAUSED_REASON, now_ts, row.id],
            )?;
            audit(conn, &row.id, "enabled", "1", "0")?;
            out.push(Change::Paused { id: row.id.clone(), session: w.session.clone() });
        } else if !row.enabled && w.running && row.disabled_reason == PAUSED_REASON && !row.locked {
            // Resumed due now: the worker is back, so its goal reads current.
            conn.execute(
                "UPDATE schedules SET enabled=1, disabled_reason=NULL, next_run=?1, updated=?2 WHERE id=?3",
                rusqlite::params![next_run_now(now), now_ts, row.id],
            )?;
            audit(conn, &row.id, "enabled", "0", "1")?;
            out.push(Change::Resumed { id: row.id.clone(), session: w.session.clone() });
        }
    }
    for row in rows.iter().filter(|r| !wants.iter().any(|w| w.session == r.session)) {
        if soft_delete_schedule(conn, &row.id, now_ts)? > 0 {
            let old = json!({"title": format!("Goal check: {}", row.session), "session": row.session, "managed_by": MANAGED_BY});
            audit(conn, &row.id, "deleted", &old.to_string(), &now_ts.to_string())?;
            out.push(Change::Removed { id: row.id.clone(), session: row.session.clone() });
        }
    }
    Ok(out)
}

/// The newest finished run of each worker's goal-check schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub session: String,
    pub schedule_id: String,
    pub enabled: bool,
    pub disabled_reason: String,
    /// `(run id, ran_at, exit_code, output_tail, note)`; None before the first run.
    pub run: Option<(i64, i64, Option<i32>, String, String)>,
}

pub fn readings(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<Reading>> {
    let mut st = conn.prepare(
        "SELECT s.session, s.id, s.enabled, COALESCE(s.disabled_reason,''),
                r.id, r.ran_at, r.exit_code, COALESCE(r.output_tail,''), COALESCE(r.note,'')
           FROM schedules s
           LEFT JOIN schedule_runs r ON r.id = (
                SELECT MAX(id) FROM schedule_runs WHERE schedule_id = s.id AND status <> 'running')
          WHERE s.managed_by = ?1 AND s.deleted IS NULL",
    )?;
    let rows = st.query_map([MANAGED_BY], |r| {
        let run = match r.get::<_, Option<i64>>(4)? {
            Some(id) => Some((id, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
            None => None,
        };
        Ok(Reading {
            session: r.get(0)?,
            schedule_id: r.get(1)?,
            enabled: r.get::<_, i64>(2)? != 0,
            disabled_reason: r.get(3)?,
            run,
        })
    })?;
    rows.collect()
}

/// A recorded run as a checker verdict. Pure. The run row keeps stdout and
/// stderr as one tail, so progress and the reason are both read from it. A
/// run with no exit code (a timeout, a spawn failure, a refusal) is UNKNOWN
/// with the run's own note as the reason.
pub fn from_run(ran_at: i64, exit: Option<i32>, tail: &str, note: &str) -> Last {
    match exit {
        Some(c) => parse(Some(c), tail, tail, ran_at as f64),
        None => Last {
            checked_at: ran_at as f64,
            why_unmeasured: Some(format!(
                "the check's run recorded no exit code: {}",
                if note.trim().is_empty() { "no note" } else { note.trim() }
            )),
            ..Default::default()
        },
    }
}

/// Fold one reading into the worker's state. Returns the new run's verdict
/// when there was one to fold. Pure.
pub fn absorb(mut st: State, r: &Reading) -> (State, Option<Last>) {
    st.schedule_id = r.schedule_id.clone();
    st.schedule_enabled = r.enabled;
    st.schedule_disabled_reason = r.disabled_reason.clone();
    let Some((run_id, ran_at, exit, tail, note)) = r.run.as_ref() else {
        return (st, None);
    };
    if *run_id <= st.last_run_id {
        return (st, None);
    }
    let last = from_run(*ran_at, *exit, tail, note);
    let mut st = fold(st, last.clone());
    st.last_run_id = *run_id;
    (st, Some(last))
}

/// The goal keeper's tick: reconcile every worker's goal-check schedule with
/// `AMUX_GOAL_CHECK`, then read each schedule's newest run into the goal
/// payload, the samples and the stall alert. The check itself runs in the
/// scheduler, with run history, audit and the Schedules tab.
pub async fn sync(app: &crate::api::AppState, sessions: &[Value], now: f64) {
    let home = crate::api::session_verbs::home();
    let mut wants = Vec::new();
    let mut running = std::collections::HashSet::new();
    for s in sessions {
        let name = s["name"].as_str().unwrap_or("");
        if name.is_empty() || s["archived"] == true || s["provider"] != "claude" {
            continue;
        }
        let is_running = s["running"] == true;
        if is_running {
            running.insert(name.to_string());
        }
        if let Some(w) = want(&home, name, super::goal_keeper::goal_check_dir(s), is_running) {
            wants.push(w);
        }
    }
    // An empty list is a failed read far more often than a fleet with no
    // workers; reconciling it would delete every goal-check schedule.
    if sessions.is_empty() {
        tracing::debug!(target: "amux::goal_check", measured = false, n_considered = 0,
            why_unmeasured = "the session list was empty", verdict = "goal_check_reconcile_skipped",
            "goal checker: no sessions listed; schedules left as they are");
        return;
    }
    // Most fleets set no check: then the writer is not asked for anything.
    if wants.is_empty() {
        let any = app
            .store
            .read_async(|c| {
                Ok(c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM schedules WHERE managed_by=?1 AND deleted IS NULL)",
                    [MANAGED_BY],
                    |r| r.get::<_, bool>(0),
                )?)
            })
            .await
            .unwrap_or(true);
        if !any {
            return;
        }
    }
    let n_wants = wants.len();
    let slot: std::sync::Arc<Mutex<Vec<Change>>> = Default::default();
    let slot_w = slot.clone();
    let wrote = app
        .store
        .write_async(move |conn| {
            let changes = reconcile(conn, &wants, chrono::Local::now())?;
            let events = changes
                .iter()
                .map(|c| crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Schedule,
                    entity_id: c.ids().0.to_string(),
                    mutation: match c {
                        Change::Created { .. } => amux_core::revision::MutationKind::Created,
                        Change::Removed { .. } => amux_core::revision::MutationKind::Deleted,
                        _ => amux_core::revision::MutationKind::Updated,
                    },
                    payload: None,
                })
                .collect();
            let applied = !changes.is_empty();
            if let Ok(mut g) = slot_w.lock() {
                *g = changes;
            }
            Ok(crate::db::WriteOutcome { applied, events })
        })
        .await;
    match wrote {
        Ok(_) => {
            let changes = slot.lock().map(|mut g| std::mem::take(&mut *g)).unwrap_or_default();
            for c in changes {
                let (id, session) = c.ids();
                tracing::info!(target: "amux::goal_check", schedule = %id, session = %session, verdict = c.verdict(),
                    change = ?c, measured = true, n_considered = n_wants, "goal checker: schedule kept in step with AMUX_GOAL_CHECK");
            }
        }
        Err(e) => {
            tracing::warn!(target: "amux::goal_check", error = %e, verdict = "goal_check_reconcile_failed",
                measured = false, n_considered = n_wants, why_unmeasured = %e,
                "goal checker: could not reconcile the goal-check schedules");
            return;
        }
    }
    let read = app.store.read_async(|c| Ok(readings(c)?)).await;
    let readings = match read {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "amux::goal_check", error = %e, verdict = "goal_check_read_failed",
                measured = false, n_considered = 0, why_unmeasured = %e,
                "goal checker: could not read the goal-check runs");
            return;
        }
    };
    for r in &readings {
        let before = load(&home, &r.session);
        let (mut st, last) = absorb(before.clone(), r);
        if let Some(last) = &last {
            tracing::info!(target: "amux::goal_check", session = %r.session, schedule = %r.schedule_id,
                verdict = "goal_check_ran", measured = last.measured, n_considered = 1, met = ?last.met,
                done = ?last.done, total = ?last.total, why_unmeasured = last.why_unmeasured.as_deref().unwrap_or(""),
                "goal checker: {}", last.summary);
            // A stopped worker's goal does not move by design; only a running
            // worker's stall is the owner's business.
            if running.contains(&r.session) {
                if let Some(kind) = stall_due(&st, now, stall_s(&home, &r.session)) {
                    alert_owner(app, &home, &r.session, &st, kind).await;
                    st.stall_alerted_at = crate::config::now_f64();
                }
            }
        }
        if st != before {
            store(&home, &r.session, &st);
        }
        if let Some(last) = &last {
            record_sample_logged(app, &r.session, last).await;
        }
    }
}

fn hours(s: f64) -> String {
    format!("{:.1} h", s / 3600.0)
}

/// The owner's stall notice: what the checker says, since when, and how to
/// change the window. Pure so its text is testable.
pub fn stall_text(name: &str, st: &State, kind: Stall, now: f64) -> (String, String) {
    let last = st.last.clone().unwrap_or_default();
    let progress = match (last.done, last.total) {
        (Some(d), Some(t)) => format!("{d} of {t}"),
        _ => last.summary.clone(),
    };
    match kind {
        Stall::NoMovement => (
            format!("amux: {name}'s goal has not moved for {}", hours(now - st.progress_changed_at)),
            format!(
                "{name}'s goal checker still says NOT MET, at {progress}, unchanged since {}.\n\n\
                 Last check: {}\n\n\
                 You get this once per stall; the next one comes only after the progress moves and stalls again. \
                 Change the window with AMUX_GOAL_STALL_H (0 turns it off) in the worker's Scope tab.",
                chrono::DateTime::from_timestamp(st.progress_changed_at as i64, 0).map(|t| t.to_rfc3339()).unwrap_or_default(),
                if last.summary.is_empty() { "(no output)".into() } else { last.summary.clone() },
            ),
        ),
        Stall::Unreadable => (
            format!("amux: {name}'s goal checker has failed for {}", hours(now - st.unknown_since)),
            format!(
                "amux could not read {name}'s goal for {}: {}\n\n\
                 The goal reads UNKNOWN, never met, while this lasts, and the goal keeper keeps the worker going.",
                hours(now - st.unknown_since),
                last.why_unmeasured.as_deref().unwrap_or("no reason recorded"),
            ),
        ),
    }
}

async fn alert_owner(app: &crate::api::AppState, home: &Path, name: &str, st: &State, kind: Stall) {
    use crate::api::alerts::AlertChannels;
    let (subject, body) = stall_text(name, st, kind, crate::config::now_f64());
    let results = crate::push::send_all(app, &subject, &body, name, "goal-stall", "/").await;
    let push = crate::api::alerts::push_delivery_verdict(&results);
    let email = match crate::api::alerts::owner_email_destination(home) {
        Some((to, _)) => crate::api::alerts::RealChannels.email(&to, &subject, &body).await,
        None => (false, "no AMUX_OWNER_EMAIL and no connected Gmail account".into()),
    };
    let delivered = push.is_ok() || email.0;
    let level_warn = !delivered;
    if level_warn {
        tracing::warn!(target: "amux::goal_check", session = %name, verdict = "goal_stall_alert_undelivered",
            kind = ?kind, measured = true, n_considered = 1, push = ?push, email = %email.1,
            "goal stall: no channel reached the owner");
    } else {
        tracing::warn!(target: "amux::goal_check", session = %name, verdict = "goal_stall_alerted",
            kind = ?kind, measured = true, n_considered = 1, push = ?push, email = %email.1,
            "goal stall: told the owner: {subject}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_decides_met_and_stdout_carries_progress() {
        let l = parse(Some(1), "row 22 pending\n44 of 56 finish-line rows verified\n", "", 10.0);
        assert_eq!((l.measured, l.met, l.done, l.total), (true, Some(false), Some(44), Some(56)), "{l:?}");
        assert_eq!(l.summary, "44 of 56 finish-line rows verified");
        let l = parse(Some(0), "{\"done\":56,\"total\":56,\"summary\":\"all rows pass\"}\n", "", 10.0);
        assert_eq!((l.met, l.done, l.total, l.summary.as_str()), (Some(true), Some(56), Some(56), "all rows pass"));
        // Anything but 0/1 is UNKNOWN, with the reason, never "not met" and never "met".
        let l = parse(Some(2), "", "Reauthentication failed\n", 10.0);
        assert!(!l.measured && l.met.is_none(), "{l:?}");
        assert!(l.why_unmeasured.as_deref().unwrap().contains("Reauthentication failed"), "{l:?}");
    }

    #[test]
    fn movement_resets_the_stall_and_an_alert_fires_once_per_episode() {
        let h = 3600.0;
        let mut st = fold(State::default(), parse(Some(1), "44/56", "", 1.0));
        assert_eq!(st.progress_changed_at, 1.0);
        st = fold(st, parse(Some(1), "44/56", "", 5.0 * h));
        assert_eq!(stall_due(&st, 5.0 * h, 6.0 * h), None, "under the window");
        st = fold(st, parse(Some(1), "44/56", "", 6.5 * h));
        assert_eq!(stall_due(&st, 6.5 * h, 6.0 * h), Some(Stall::NoMovement));
        st.stall_alerted_at = 6.5 * h;
        st = fold(st, parse(Some(1), "44/56", "", 20.0 * h));
        assert_eq!(stall_due(&st, 20.0 * h, 6.0 * h), None, "one alert per stall");
        // Movement starts a new episode.
        st = fold(st, parse(Some(1), "45/56", "", 21.0 * h));
        assert_eq!(st.progress_changed_at, 21.0 * h);
        assert_eq!(stall_due(&st, 26.0 * h, 6.0 * h), None);
        assert_eq!(stall_due(&st, 27.5 * h, 6.0 * h), Some(Stall::NoMovement));
        // A met goal never stalls; 0 disables.
        let met = fold(State::default(), parse(Some(0), "56/56", "", 1.0));
        assert_eq!(stall_due(&met, 100.0 * h, 6.0 * h), None);
        assert_eq!(stall_due(&st, 100.0 * h, 0.0), None);
    }

    #[test]
    fn an_unreadable_checker_alerts_after_the_window_and_recovers() {
        let h = 3600.0;
        let mut st = fold(State::default(), parse(Some(1), "44/56", "", 1.0));
        st = fold(st, parse(Some(127), "", "sh: checker.sh: not found", 1.0 * h));
        assert_eq!(st.unknown_since, 1.0 * h);
        st = fold(st, parse(Some(127), "", "sh: checker.sh: not found", 7.5 * h));
        assert_eq!(st.unknown_since, 1.0 * h, "the clock starts at the first failure");
        assert_eq!(stall_due(&st, 7.5 * h, 6.0 * h), Some(Stall::Unreadable));
        let (subject, body) = stall_text("w", &st, Stall::Unreadable, 7.5 * h);
        assert!(subject.contains("failed for 6.5 h"), "{subject}");
        assert!(body.contains("UNKNOWN") && body.contains("not found"), "{body}");
        st = fold(st, parse(Some(1), "44/56", "", 8.0 * h));
        assert_eq!(st.unknown_since, 0.0);
    }

    #[test]
    fn only_a_measured_number_is_retained_and_old_rows_age_out() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../../migrations/0155_goal_samples.sql")).unwrap();
        let count = |c: &rusqlite::Connection| -> i64 {
            c.query_row("SELECT COUNT(*) FROM goal_samples", [], |r| r.get(0)).unwrap()
        };
        assert!(record_sample(&conn, "w", &parse(Some(1), "3/9", "", 100.0)).unwrap());
        assert_eq!(count(&conn), 1);
        // No verdict, even with a number on stdout: nothing stored.
        assert!(!record_sample(&conn, "w", &parse(Some(2), "4/9", "boom", 200.0)).unwrap());
        // A verdict with no number: nothing stored.
        assert!(!record_sample(&conn, "w", &parse(Some(1), "still going", "", 300.0)).unwrap());
        assert_eq!(count(&conn), 1);
        let later = 100.0 + SAMPLE_RETENTION_DAYS * 86_400.0 + 1.0;
        assert!(record_sample(&conn, "w", &parse(Some(1), "5/9", "", later)).unwrap());
        let (at, done): (f64, i64) = conn
            .query_row("SELECT at, done FROM goal_samples", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((count(&conn), at, done), (1, later, 5), "the old row aged out, the new one stayed");
    }

    fn want_for(session: &str, cmd: &str, running: bool) -> Want {
        Want { session: session.into(), command: compose(cmd, ""), expr: "every 10m".into(), running }
    }

    fn live_rows(conn: &rusqlite::Connection) -> Vec<(String, String, i64, String)> {
        let mut st = conn
            .prepare("SELECT id, session, enabled, COALESCE(disabled_reason,'') FROM schedules WHERE managed_by='goal-check' AND deleted IS NULL ORDER BY id")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap().map(Result::unwrap).collect()
    }

    fn verdicts(c: &[Change]) -> Vec<&'static str> {
        c.iter().map(Change::verdict).collect()
    }

    /// AMUX_GOAL_CHECK is the way to create the schedule; re-reading it never
    /// makes a second row, a changed key rewrites the one row, a stopped
    /// worker's row pauses and comes back, and a cleared key removes it.
    #[test]
    fn the_key_keeps_one_schedule_per_worker_in_step() {
        let conn = crate::db::migrate::test_memdb_pub();
        let now = chrono::Local::now();
        let c = reconcile(&conn, &[want_for("w", "./check.sh", true)], now).unwrap();
        assert_eq!(verdicts(&c), ["goal_check_schedule_created"]);
        let rows = live_rows(&conn);
        assert_eq!(rows.len(), 1);
        let id = rows[0].0.clone();
        let (kind, expr, actions, by, verified, cmd): (String, String, String, String, i64, String) = conn
            .query_row(
                "SELECT kind, schedule_expr, exit_actions, created_by, created_by_verified, command FROM schedules WHERE id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!((kind.as_str(), expr.as_str(), actions.as_str(), by.as_str(), verified), ("shell", "every 10m", EXIT_ACTIONS, CREATOR, 1));
        assert!(cmd.ends_with("./check.sh") && cmd.contains("AMUX_GOAL_CHECK_RUN=1"), "{cmd}");
        // Re-reading the same key writes nothing.
        for _ in 0..3 {
            assert!(reconcile(&conn, &[want_for("w", "./check.sh", true)], now).unwrap().is_empty());
        }
        assert_eq!(live_rows(&conn).len(), 1, "never a duplicate");
        // A changed key rewrites the same row.
        let c = reconcile(&conn, &[want_for("w", "./other.sh", true)], now).unwrap();
        assert_eq!(c, vec![Change::Updated { id: id.clone(), session: "w".into(), fields: vec!["command"] }]);
        // Stopped, then running again.
        let c = reconcile(&conn, &[want_for("w", "./other.sh", false)], now).unwrap();
        assert_eq!(verdicts(&c), ["goal_check_schedule_paused"]);
        assert_eq!(live_rows(&conn)[0].2, 0);
        assert_eq!(live_rows(&conn)[0].3, PAUSED_REASON);
        let c = reconcile(&conn, &[want_for("w", "./other.sh", true)], now).unwrap();
        assert_eq!(verdicts(&c), ["goal_check_schedule_resumed"]);
        // An owner's pause is the owner's: the reconcile never undoes it.
        conn.execute("UPDATE schedules SET enabled=0, disabled_reason='owner', locked_by='owner' WHERE id=?1", [&id]).unwrap();
        assert!(reconcile(&conn, &[want_for("w", "./other.sh", true)], now).unwrap().is_empty());
        assert_eq!(live_rows(&conn)[0].2, 0, "still paused by the owner");
        // Clearing the key removes the schedule, once.
        let c = reconcile(&conn, &[], now).unwrap();
        assert_eq!(c, vec![Change::Removed { id: id.clone(), session: "w".into() }]);
        assert!(live_rows(&conn).is_empty());
        assert!(reconcile(&conn, &[], now).unwrap().is_empty());
        // Every write left an audit row by the key, and the no-op passes none.
        let audits: i64 = conn
            .query_row("SELECT COUNT(*) FROM schedule_audit WHERE schedule_id=?1 AND by_who=?2", rusqlite::params![id, CREATOR], |r| r.get(0))
            .unwrap();
        assert_eq!(audits, 5, "created, command, paused, resumed, deleted");
        let ledger: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _amux_ledger WHERE kind='schedule_audit' AND resource=?1 AND actor=?2 AND authority='owner_policy'",
                rusqlite::params![format!("schedule:{id}"), CREATOR],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ledger, 5, "the ledger names the key as the proven actor");
    }

    #[test]
    fn the_database_refuses_a_second_live_goal_check_row() {
        let conn = crate::db::migrate::test_memdb_pub();
        reconcile(&conn, &[want_for("w", "x", true)], chrono::Local::now()).unwrap();
        let dup = conn.execute(
            "INSERT INTO schedules (id,title,session,command,created,updated,managed_by) VALUES ('SCHED-X','t','w','x',0,0,'goal-check')",
            [],
        );
        assert!(dup.is_err(), "the unique index backs the reconcile");
    }

    #[test]
    fn a_run_is_read_once_and_a_run_without_an_exit_code_reads_unknown() {
        let r = |id: i64, exit: Option<i32>, tail: &str, note: &str| Reading {
            session: "w".into(),
            schedule_id: "SCHED-1".into(),
            enabled: true,
            disabled_reason: String::new(),
            run: Some((id, 100 + id, exit, tail.into(), note.into())),
        };
        let (st, last) = absorb(State::default(), &r(7, Some(1), "44 of 56 rows\n", "exit 1 [log]"));
        let last = last.unwrap();
        assert_eq!((last.met, last.done, last.total, st.last_run_id, st.runs), (Some(false), Some(44), Some(56), 7, 1));
        assert_eq!(st.schedule_id, "SCHED-1");
        let (st2, again) = absorb(st.clone(), &r(7, Some(0), "", ""));
        assert!(again.is_none() && st2.runs == 1, "the same run is never folded twice");
        let (_, unknown) = absorb(st, &r(8, None, "", "timed out after 600s"));
        let unknown = unknown.unwrap();
        assert!(!unknown.measured && unknown.met.is_none(), "{unknown:?}");
        assert!(unknown.why_unmeasured.unwrap_or_default().contains("timed out after 600s"));
        assert_eq!(interval_expr(600.0), "every 10m");
        assert_eq!(interval_expr(90.0), "every 2m");
        assert_eq!(interval_expr(5.0), "every 1m");
    }

    fn test_state(store: &crate::db::SharedStore) -> crate::api::AppState {
        crate::api::AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    /// The whole path through the real scheduler, then a restart: the
    /// reconcile finds its row in the database (no second schedule) and the
    /// persisted state means the run already read is not counted again.
    #[tokio::test]
    async fn a_check_runs_as_a_shell_schedule_and_survives_a_restart() {
        let home = tempfile::tempdir().unwrap();
        let _guard = crate::api::settings::test_env::set_home(home.path());
        let db = home.path().join("t.db");
        let work = tempfile::tempdir().unwrap();
        let dir = work.path().to_string_lossy().to_string();
        let check = r#"test "$AMUX_GOAL_CHECK_RUN" = 1 || exit 2; test -f marker || exit 3; echo '{"done":3,"total":9,"summary":"3 rows left"}'; exit 1"#;
        std::fs::write(work.path().join("marker"), "").unwrap();
        let want = Want { session: "w".into(), command: compose(check, &dir), expr: "every 10m".into(), running: true };
        let id = {
            let db_store: crate::db::SharedStore = std::sync::Arc::new(crate::db::Store::open(&db).unwrap());
            let w = want.clone();
            db_store
                .write(move |c| Ok(crate::db::WriteOutcome { applied: !reconcile(c, &[w], chrono::Local::now())?.is_empty(), events: vec![] }))
                .unwrap();
            let id: String = db_store.read().unwrap().query_row("SELECT id FROM schedules WHERE managed_by='goal-check'", [], |r| r.get(0)).unwrap();
            let live = super::super::scheduler::LiveDeliverer::new(test_state(&db_store));
            let fired = super::super::scheduler::fire_one(&db_store, &live, &id, chrono::Local::now(), super::super::scheduler::MissedRunPolicy::Skip)
                .await
                .unwrap();
            assert!(fired, "the due goal check fired");
            let rs = readings(&db_store.read().unwrap()).unwrap();
            assert_eq!(rs.len(), 1);
            let (st, last) = absorb(load(home.path(), "w"), &rs[0]);
            let last = last.unwrap_or_default();
            assert_eq!((last.measured, last.met, last.done, last.total), (true, Some(false), Some(3), Some(9)), "{last:?} {rs:?}");
            assert_eq!(last.summary, "3 rows left");
            store(home.path(), "w", &st);
            id
        };
        // Restart: a new store over the same file, and the memory cache gone.
        states().lock().unwrap().remove(&state_path(home.path(), "w"));
        let db_store: crate::db::SharedStore = std::sync::Arc::new(crate::db::Store::open(&db).unwrap());
        let w = want.clone();
        let reply = db_store
            .write(move |c| {
                let changes = reconcile(c, &[w], chrono::Local::now())?;
                assert!(changes.is_empty(), "the restarted reconcile adopts its row: {changes:?}");
                Ok(crate::db::WriteOutcome { applied: false, events: vec![] })
            });
        assert!(reply.is_ok());
        let conn = db_store.read().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM schedules WHERE managed_by='goal-check' AND deleted IS NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        let rs = readings(&conn).unwrap();
        assert_eq!(rs[0].schedule_id, id);
        let (st, again) = absorb(load(home.path(), "w"), &rs[0]);
        assert!(again.is_none(), "the run read before the restart is not counted twice");
        assert_eq!(st.runs, 1);
        // The run's history is the scheduler's own: exit code and output on the row.
        let (exit, status): (Option<i64>, String) = conn
            .query_row("SELECT exit_code, status FROM schedule_runs WHERE schedule_id=?1", [&id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(exit, Some(1));
        assert_ne!(status, "error", "an unmet goal is a reading, not a failed run");
    }

    #[test]
    fn state_persists_across_a_restart() {
        let home = tempfile::tempdir().unwrap();
        let st = fold(State::default(), parse(Some(1), "3/9", "", 42.0));
        store(home.path(), "w", &st);
        states().lock().unwrap().remove(&state_path(home.path(), "w"));
        assert_eq!(load(home.path(), "w"), st);
    }
}
