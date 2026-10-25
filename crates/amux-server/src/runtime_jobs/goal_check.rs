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
//! shell command run in the worker's directory every
//! `AMUX_GOAL_CHECK_INTERVAL_S` (600 s), with `AMUX_GOAL_CHECK_TIMEOUT_S`
//! (120 s) to finish. Exit 0 means met, exit 1 means not met, anything else
//! (or a timeout, or a spawn error) means UNKNOWN and says why. Progress comes
//! from stdout: a JSON line `{"done":44,"total":56,"summary":"..."}`, or the
//! first `N/M` or `N of M` in the output. No model call (ethos rule 2).
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
}

fn setting(home: &Path, name: &str, key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(key) {
        if !v.trim().is_empty() {
            return Some(v.trim().to_string());
        }
    }
    crate::api::session_verbs::scoped_setting_in(home, name, key)
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
        return json!({
            "configured": true, "measured": false, "command": clip(&cmd, 120),
            "why_unmeasured": "the checker has not run yet (it runs every AMUX_GOAL_CHECK_INTERVAL_S while the worker is running)",
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
    })
}

fn in_flight() -> &'static Mutex<std::collections::HashSet<String>> {
    static S: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

/// Run the worker's checker if one is configured and the interval has passed.
/// Spawned, so a slow checker never holds the keeper's tick. One run per
/// worker at a time.
pub fn maybe_run(app: &crate::api::AppState, name: &str, dir: &str, now: f64) {
    let home = crate::api::session_verbs::home();
    let Some(cmd) = command(&home, name) else { return };
    let interval = setting_f64(&home, name, "AMUX_GOAL_CHECK_INTERVAL_S", 600.0).max(30.0);
    let st = load(&home, name);
    if st.last.as_ref().is_some_and(|l| now - l.checked_at < interval) {
        return;
    }
    {
        let Ok(mut f) = in_flight().lock() else { return };
        if !f.insert(name.to_string()) {
            return;
        }
    }
    let timeout = setting_f64(&home, name, "AMUX_GOAL_CHECK_TIMEOUT_S", 120.0).max(1.0);
    let (app, name, dir) = (app.clone(), name.to_string(), dir.to_string());
    tokio::spawn(async move {
        let last = run(&cmd, &name, &dir, timeout).await;
        let st = fold(load(&home, &name), last.clone());
        tracing::info!(target: "amux::goal_check", session = %name, verdict = "goal_check_ran",
            measured = last.measured, n_considered = 1, met = ?last.met, done = ?last.done, total = ?last.total,
            why_unmeasured = last.why_unmeasured.as_deref().unwrap_or(""),
            "goal checker: {}", last.summary);
        let mut st = st;
        if let Some(kind) = stall_due(&st, crate::config::now_f64(), stall_s(&home, &name)) {
            alert_owner(&app, &home, &name, &st, kind).await;
            st.stall_alerted_at = crate::config::now_f64();
        }
        store(&home, &name, &st);
        if let Ok(mut f) = in_flight().lock() {
            f.remove(&name);
        }
    });
}

async fn run(cmd: &str, name: &str, dir: &str, timeout_s: f64) -> Last {
    let now = crate::config::now_f64();
    let mut c = tokio::process::Command::new("sh");
    c.arg("-c").arg(cmd).env("AMUX_SESSION", name).env("AMUX_GOAL_CHECK_RUN", "1");
    c.stdin(std::process::Stdio::null()).kill_on_drop(true);
    if !dir.is_empty() && Path::new(dir).is_dir() {
        c.current_dir(dir);
    }
    match tokio::time::timeout(std::time::Duration::from_secs_f64(timeout_s), c.output()).await {
        Err(_) => Last {
            checked_at: now,
            why_unmeasured: Some(format!("checker timed out after {timeout_s:.0}s (AMUX_GOAL_CHECK_TIMEOUT_S)")),
            ..Default::default()
        },
        Ok(Err(e)) => Last { checked_at: now, why_unmeasured: Some(format!("checker could not start: {e}")), ..Default::default() },
        Ok(Ok(out)) => {
            let cap = |b: &[u8]| String::from_utf8_lossy(&b[b.len().saturating_sub(64 << 10)..]).to_string();
            parse(out.status.code(), &cap(&out.stdout), &cap(&out.stderr), now)
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
    fn state_persists_across_a_restart() {
        let home = tempfile::tempdir().unwrap();
        let st = fold(State::default(), parse(Some(1), "3/9", "", 42.0));
        store(home.path(), "w", &st);
        states().lock().unwrap().remove(&state_path(home.path(), "w"));
        assert_eq!(load(home.path(), "w"), st);
    }
}
