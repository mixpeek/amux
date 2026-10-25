//! Goal keeper: keeps a worker going while its owner-set Claude Code `/goal`
//! is not met.
//!
//! # Why this exists
//!
//! Claude Code's `/goal` evaluates the goal each time the agent stops and, if
//! it is not met, sends the agent back to work. That loop does not survive a
//! resume. Measured 2026-09-26 on gs-10-zero-base-cicd: the goal was set at
//! 13:14, evaluated at 13:48, then a resume at 18:50 re-registered it (a
//! `goal_status` record with `sentinel: true`) and no evaluation ran again. The
//! footer still read "◎ /goal active" while the worker sat idle with work it
//! could do, and the owner had to notice ("this needs to have been continued
//! automatically on the harness level. especially since its a /goal").
//!
//! # What counts as an active, unmet goal
//!
//! Two signals, both required: the transcript's latest `goal_status`
//! attachment says `met: false`, AND the provider's footer shows
//! "/goal active" (a cleared goal leaves no footer). The worker must be idle
//! on an authoritative report for `AMUX_GOAL_CONTINUE_IDLE_S` (120s), which
//! also means Claude Code's own loop did not continue it.
//!
//! # Bounds
//!
//! At most one continue per `AMUX_GOAL_CONTINUE_INTERVAL_S` (600s). A continue
//! after which the agent made no tool call counts as no progress; after two in
//! a row the keeper stops for that goal (`goal_continue_exhausted`) until the
//! goal changes or someone else sends the worker a message. The text tells the
//! agent to stop and list what needs the owner when only that remains, so a
//! goal blocked on the owner costs two short turns, not a loop.
//!
//! # What "met" means (mxp-gs12, 2026-10-17)
//!
//! Claude Code writes `goal_status {met: true, sentinel: true}` when a goal is
//! CLEARED, by `/goal clear` or by an error: at 22:54:45Z an authentication
//! failure logged "Goal cleared after an unrecoverable error" in the same
//! millisecond as a sentinel `met: true`, amux read it as met, and the keeper
//! stopped while the worker's own checker said 44 of 56 rows. An evaluator's
//! real verdict carries `reason`, `iterations` and `durationMs` and no
//! `sentinel`. So a sentinel `met` is a clear, an error clear reads UNKNOWN and
//! keeps being continued (a failure to read the goal never clears it), and a
//! configured checker (`goal_check`, `AMUX_GOAL_CHECK`) outranks both: the
//! checker decides "met". [`resolve`] is the one predicate the payload, the
//! keeper and the schedule gate share.
//!
//! # Usage limits (gs12 review, 2026-10-18)
//!
//! A worker whose last turn ended on a usage limit is held, not continued:
//! a continue into an unexpired limit fails at once and used to spend the
//! two-continue budget, leaving the keeper exhausted through the reset
//! (mxp-gs12, 10-12 17:13Z). The hold ends at the named reset or at an account
//! switch after the limit, and that end clears any exhaustion the limit
//! caused. A continue answered by a 429 is not counted as no progress. The
//! continue also lists the worker's background agents a limit or logout
//! killed (`api::agent_limit_resume`). Log verdicts:
//! `goal_continue_held_for_limit`, `goal_continue_rearmed_after_limit`, and
//! `killed_agents_listed` on `goal_continue_sent`.
//!
//! # Isolated workers
//!
//! The owner set the goal in that CLI, so continuing toward it is the owner's
//! standing instruction, like a schedule: it is delivered with a `goal:` guard,
//! which the isolation gate treats as owner configuration. `AMUX_GOAL_CONTINUE=0`
//! (process env, or scoped per worker/group/global) opts out.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

use super::registry::ids;

const JOB: &str = ids::GOAL_KEEPER;
/// Prefix on every continue, so the keeper can tell its own messages from the
/// owner's when it resets.
pub const MARK: &str = "[amux goal keeper]";

fn env_f64(k: &str, d: f64) -> f64 {
    std::env::var(k).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(d)
}

fn enabled(name: &str) -> bool {
    let off = |v: &str| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no");
    if let Ok(v) = std::env::var("AMUX_GOAL_CONTINUE") {
        if !v.trim().is_empty() {
            return !off(&v);
        }
    }
    crate::api::session_verbs::scoped_setting_in(&crate::api::session_verbs::home(), name, "AMUX_GOAL_CONTINUE")
        .map(|v| !off(&v))
        .unwrap_or(true)
}

fn ts_of(r: &Value) -> f64 {
    r["timestamp"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.timestamp_millis() as f64 / 1000.0)
        .unwrap_or(0.0)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Goal {
    /// The EVALUATOR said met. A sentinel `met: true` is a clear, never this.
    pub met: bool,
    pub condition: String,
    pub ts: f64,
    pub cleared: Option<Cleared>,
}

/// A `/goal` Claude Code cleared, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Cleared {
    /// Cleared by an error (authentication, usage), not by anyone's decision.
    pub by_error: bool,
    pub why: String,
}

/// The informational Claude Code logs when an error clears a goal.
const ERROR_CLEAR: &str = "Goal cleared after an unrecoverable error";

fn goal_of(r: &Value) -> Option<Goal> {
    let a = &r["attachment"];
    (a["type"] == "goal_status").then(|| {
        let raw_met = a["met"].as_bool().unwrap_or(false);
        let sentinel = a["sentinel"].as_bool().unwrap_or(false);
        Goal {
            met: raw_met && !sentinel,
            condition: a["condition"].as_str().unwrap_or("").to_string(),
            ts: ts_of(r),
            cleared: (raw_met && sentinel).then(|| Cleared {
                by_error: false,
                why: "the /goal was cleared (by /goal clear or amux's goal-loop park)".into(),
            }),
        }
    })
}

/// `(ts, text)` of an error-clear informational record.
fn error_clear_of(r: &Value) -> Option<(f64, String)> {
    let t = r["content"].as_str()?;
    (r["type"] == "system" && t.starts_with(ERROR_CLEAR)).then(|| (ts_of(r), t.chars().take(240).collect()))
}

/// Folds transcript records, oldest first, into the newest goal.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scan {
    pub goal: Option<Goal>,
    err: Option<(f64, String)>,
}

impl Scan {
    /// Clear records and the error informational share a timestamp (to the
    /// millisecond in the specimen); 5 s allows for either order.
    fn attach(g: &mut Goal, err: &(f64, String)) {
        if g.cleared.is_some() && (err.0 - g.ts).abs() <= 5.0 {
            g.cleared = Some(Cleared { by_error: true, why: err.1.clone() });
        }
    }
    pub fn feed(&mut self, r: &Value) {
        if let Some(mut g) = goal_of(r) {
            if let Some(e) = &self.err {
                Self::attach(&mut g, e);
            }
            self.goal = Some(g);
        } else if let Some(e) = error_clear_of(r) {
            if let Some(g) = self.goal.as_mut() {
                Self::attach(g, &e);
            }
            self.err = Some(e);
        }
    }
}

fn relevant(line: &[u8]) -> bool {
    line.windows(11).any(|w| w == b"goal_status") || line.windows(ERROR_CLEAR.len()).any(|w| w == ERROR_CLEAR.as_bytes())
}

/// The newest goal in a transcript tail.
pub fn latest_goal(records: &[Value]) -> Option<Goal> {
    let mut s = Scan::default();
    for r in records {
        s.feed(r);
    }
    s.goal
}

/// The newest `goal_status` in a whole transcript, scanning backward from
/// `len` in chunks until one is found or the file start is reached.
///
/// A fixed tail was not enough: measured 2026-10-09 on mxp-gs12, the goal was
/// set at 01:27Z and by 12:04Z the transcript had grown 7.1 MB past it. The
/// keeper read the last 6 MB, found no goal, and skipped the worker silently
/// while its footer read "/goal active" and it sat idle for hours.
fn last_goal_before(f: &mut std::fs::File, len: u64, chunk: u64) -> Scan {
    use std::io::{Read, Seek, SeekFrom};
    let mut end = len;
    let mut chunk = chunk.max(1);
    // Relevant records AFTER the goal (seen first, going backward).
    let mut later: Vec<Value> = Vec::new();
    while end > 0 {
        let start = end.saturating_sub(chunk);
        let mut buf = Vec::with_capacity((end - start) as usize);
        if f.seek(SeekFrom::Start(start)).is_err() || f.by_ref().take(end - start).read_to_end(&mut buf).is_err() {
            return Scan::default();
        }
        // Unless at the file start, the first line may be cut: leave it whole
        // for the next chunk. A chunk with no newline is inside one long line,
        // so widen it rather than skip what might be the record.
        let first = if start == 0 { 0 } else {
            match buf.iter().position(|&b| b == b'\n') {
                // A newline as the last byte leaves no whole line either.
                Some(i) if i + 1 < buf.len() => i + 1,
                _ => {
                    chunk = chunk.saturating_mul(2);
                    continue;
                }
            }
        };
        for line in buf[first..].split(|&b| b == b'\n').rev() {
            if relevant(line) {
                if let Ok(v) = serde_json::from_slice::<Value>(line) {
                    if goal_of(&v).is_some() {
                        let mut s = Scan::default();
                        s.feed(&v);
                        for r in later.iter().rev() {
                            s.feed(r);
                        }
                        return s;
                    }
                    later.push(v);
                }
            }
        }
        end = start + first as u64;
    }
    let mut s = Scan::default();
    for r in later.iter().rev() {
        s.feed(r);
    }
    s
}

/// A transcript's newest goal, INCREMENTAL: per path it keeps the bytes
/// already read and the last goal seen, and on a later call scans only what
/// was appended. A new or truncated file is scanned backward from its end
/// once ([`last_goal_before`]), so a goal set long ago is still found.
pub fn cached_goal(path: &std::path::Path) -> Result<Option<Goal>, String> {
    use std::io::{Read, Seek, SeekFrom};
    type Cache = std::collections::HashMap<std::path::PathBuf, (u64, Scan)>;
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Cache>> = std::sync::OnceLock::new();
    let len = std::fs::metadata(path).map_err(|e| format!("transcript unreadable: {e}"))?.len();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    let (mut offset, mut scan) = cache.get(path).cloned().unwrap_or((u64::MAX, Scan::default()));
    if offset == u64::MAX || len < offset {
        let mut f = std::fs::File::open(path).map_err(|e| format!("transcript unreadable: {e}"))?;
        scan = last_goal_before(&mut f, len, 4 << 20);
        offset = len;
    } else if len > offset {
        let mut buf = Vec::new();
        if let Ok(mut f) = std::fs::File::open(path) {
            if f.seek(SeekFrom::Start(offset)).is_ok() {
                let _ = f.take(len - offset).read_to_end(&mut buf);
            }
        }
        // Only whole lines; a half-written last record is read next time.
        let whole = buf.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
        for line in buf[..whole].split(|&b| b == b'\n') {
            if relevant(line) {
                if let Ok(v) = serde_json::from_slice::<Value>(line) {
                    scan.feed(&v);
                }
            }
        }
        offset += whole as u64;
    }
    let goal = scan.goal.clone();
    cache.insert(path.to_path_buf(), (offset, scan));
    Ok(goal)
}

/// A worker's Claude Code `/goal` for the session payload (amux-helper ask,
/// 2026-10-08: GET /api/sessions/mxp-gs12 carried no goal while the worker
/// showed "/goal active (10m)", and the absence was read as "no goal").
///
/// Polled ~6000 times an hour, so it reads through [`cached_goal`]. Always
/// answers `measured`; a transcript that cannot be read says why instead of
/// leaving the field out.
pub fn goal_payload(name: &str) -> Value {
    let home = crate::api::session_verbs::home();
    let checker = super::goal_check::view(&home, name, crate::config::now_f64());
    let mut v = match crate::api::session_verbs::session_jsonl_path(name) {
        None => goal_view_unread("no transcript for this worker", &checker),
        Some(path) => match cached_goal(&path) {
            // A failure to read the goal never clears it: it reads unknown.
            Err(why) => goal_view_unread(&why, &checker),
            Ok(g) => goal_view(g, || footer_goal_active(name), &checker),
        },
    };
    v["checker"] = checker;
    v
}

/// The goal could not be read. Without a checker that is UNKNOWN with why;
/// with one, the checker still decides.
fn goal_view_unread(why: &str, checker: &Value) -> Value {
    let r = resolve(None, || None, checker);
    let (state, state_why) = if r.state == "none" {
        ("unknown", why.to_string())
    } else {
        (r.state, format!("{}; transcript: {why}", r.why))
    };
    serde_json::json!({"measured": false, "why_unmeasured": why, "active": false, "met": state == "met",
        "state": state, "state_why": state_why})
}

/// The goal as the payload, the keeper and the schedule gate all see it.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// `none`, `unmet`, `met`, `cleared` or `unknown`.
    pub state: &'static str,
    pub why: String,
    /// The keeper continues this worker.
    pub active: bool,
    /// What the pane footer said, when it was read.
    pub footer: Option<bool>,
}

/// THE ONE PREDICATE. The checker (`goal_check`), when configured and
/// measured, decides met; a checker that could not run makes the answer
/// unknown, never met. Without a checker, Claude Code's evaluator decides,
/// a clear is a clear, and an ERROR clear is unknown and still continued.
///
/// The footer is read only when the transcript says an uncleared goal is
/// unmet (Ethan, 2026-10-10: a cleared `/goal` only removes the footer).
pub fn resolve(goal: Option<&Goal>, footer: impl FnOnce() -> Option<bool>, checker: &Value) -> Resolved {
    #[derive(PartialEq)]
    enum Ck {
        None,
        Met,
        Unmet,
        Unknown(String),
    }
    let ck = if checker["configured"] != true {
        Ck::None
    } else if checker["measured"] == true {
        if checker["met"] == true { Ck::Met } else { Ck::Unmet }
    } else {
        Ck::Unknown(checker["why_unmeasured"].as_str().unwrap_or("the checker has no verdict").to_string())
    };
    let progress = match (checker["done"].as_u64(), checker["total"].as_u64()) {
        (Some(d), Some(t)) => format!(" ({d} of {t})"),
        _ => String::new(),
    };
    let r = |state, why: String, active, footer| Resolved { state, why, active, footer };
    let Some(g) = goal else {
        return match ck {
            Ck::None => r("none", "no /goal and no checker".into(), false, None),
            Ck::Met => r("met", format!("the checker says met{progress}"), false, None),
            Ck::Unmet => r("unmet", format!("the checker says not met{progress}; no /goal is set"), false, None),
            Ck::Unknown(w) => r("unknown", format!("the checker could not decide: {w}"), false, None),
        };
    };
    if let Some(c) = &g.cleared {
        if c.by_error {
            return match ck {
                Ck::Met => r("met", format!("the checker says met{progress}"), false, None),
                Ck::Unmet => r("unmet", format!("Claude Code cleared the goal after an error, and the checker says not met{progress}: {}", c.why), true, None),
                Ck::None => r("unknown", format!("{}; nothing decided it met, so the keeper keeps going", c.why), true, None),
                Ck::Unknown(w) => r("unknown", format!("{}; the checker could not decide either: {w}", c.why), true, None),
            };
        }
        return match ck {
            Ck::Met => r("met", format!("the checker says met{progress}"), false, None),
            _ => r("cleared", c.why.clone(), false, None),
        };
    }
    if g.met {
        return match ck {
            Ck::Unmet => r("unmet", format!("Claude Code's evaluator said met, but the checker says not met{progress}, and the checker decides"), true, None),
            Ck::Unknown(w) => r("unknown", format!("Claude Code's evaluator said met, but the checker could not run: {w}"), false, None),
            _ => r("met", if ck == Ck::Met { format!("the checker says met{progress}") } else { "Claude Code's goal evaluator said met".into() }, false, None),
        };
    }
    if g.condition.trim().is_empty() {
        return r("none", "the goal record has no condition".into(), false, None);
    }
    if ck == Ck::Met {
        return r("met", format!("the checker says met{progress}"), false, None);
    }
    let note = match &ck {
        Ck::Unknown(w) => format!("; the checker could not decide: {w}"),
        Ck::Unmet => format!("; the checker says not met{progress}"),
        _ => String::new(),
    };
    match footer() {
        Some(true) => r("unmet", format!("the /goal is active and not met{note}"), true, Some(true)),
        Some(false) => r("cleared", format!("the footer no longer shows /goal active, so it was cleared{note}"), false, Some(false)),
        // No pane to read (worker stopped): nothing is pursuing the goal.
        None => r("unmet", format!("not met; the worker is not running{note}"), false, None),
    }
}

/// The goal for a worker, resolved, for callers outside the payload (the
/// schedule gate). Blocking: reads the transcript cache and the footer.
pub fn resolved_for(name: &str) -> Resolved {
    let home = crate::api::session_verbs::home();
    let checker = super::goal_check::view(&home, name, crate::config::now_f64());
    match crate::api::session_verbs::session_jsonl_path(name).map(|p| cached_goal(&p)) {
        Some(Ok(g)) => resolve(g.as_ref(), || footer_goal_active(name), &checker),
        Some(Err(why)) => Resolved { state: "unknown", why, active: false, footer: None },
        None => resolve(None, || None, &checker),
    }
}

/// THE BADGE AND THE KEEPER SHARE ONE PREDICATE (Ethan, 2026-10-10: "goal
/// indicator seems wrong"): both go through [`resolve`].
pub(crate) fn goal_view(goal: Option<Goal>, footer: impl FnOnce() -> Option<bool>, checker: &Value) -> Value {
    let res = resolve(goal.as_ref(), footer, checker);
    let Some(g) = goal else {
        return serde_json::json!({"measured": true, "active": false, "met": res.state == "met",
            "state": res.state, "state_why": res.why});
    };
    let by_error = g.cleared.as_ref().is_some_and(|c| c.by_error);
    serde_json::json!({
        "measured": true,
        "active": res.active,
        "met": res.state == "met",
        "evaluator_met": g.met,
        "cleared": res.state == "cleared" || by_error,
        "cleared_by_error": by_error,
        "state": res.state,
        "state_why": res.why,
        "footer_active": res.footer,
        "condition": g.condition,
        "since": g.ts,
    })
}

/// Whether the worker's pane footer shows "/goal active", cached 30s per
/// worker so the session list (polled often) costs at most one capture per
/// worker per 30s, and only for workers whose transcript holds an unmet goal.
/// None when the pane cannot be read.
type FooterCache = Mutex<HashMap<String, (std::time::Instant, Option<bool>)>>;

pub(crate) fn footer_goal_active(name: &str) -> Option<bool> {
    static CACHE: std::sync::OnceLock<FooterCache> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock() {
        if let Some((at, v)) = map.get(name) {
            if at.elapsed() < std::time::Duration::from_secs(30) {
                return *v;
            }
        }
    }
    // Exact-match target: a bare `amux-x` resolves to a sibling `amux-x-2`
    // pane while `amux-x` is briefly absent.
    let pt = crate::backend::tmux::pane_target(&format!("amux-{name}"));
    let v = std::process::Command::new("tmux")
        .args(["capture-pane", "-p", "-t", &pt, "-S", "-12"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("/goal active"));
    if let Ok(mut map) = cache.lock() {
        map.insert(name.to_string(), (std::time::Instant::now(), v));
    }
    v
}

/// Tool calls the agent made after `since` (progress is an event, not text).
pub fn tool_uses_since(records: &[Value], since: f64) -> usize {
    records
        .iter()
        .filter(|r| r["type"] == "assistant" && ts_of(r) > since)
        .map(|r| {
            r["message"]["content"]
                .as_array()
                .map(|c| c.iter().filter(|p| p["type"] == "tool_use").count())
                .unwrap_or(0)
        })
        .sum()
}

/// A typed user message after `since` that is not the keeper's own.
///
/// Claude Code also writes MACHINE records as `type: user`: background task
/// completions (`<task-notification>`), system reminders, slash-command
/// echoes, and `isMeta` records. Counting those as someone writing to the
/// worker reset the keeper on every finished background task, and the reset
/// also cleared the interval: tubescience-parity was continued at 04:39,
/// 04:44 and 04:54 on 2026-09-27 against a 10-minute minimum.
pub fn foreign_input_since(records: &[Value], since: f64) -> bool {
    records.iter().any(|r| {
        if r["type"] != "user" || ts_of(r) <= since || r["isMeta"] == true {
            return false;
        }
        let text = match &r["message"]["content"] {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter(|p| p["type"] == "text")
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        };
        let t = text.trim();
        !t.is_empty() && !t.contains(MARK) && !t.starts_with('<')
    })
}

#[derive(Default, Debug)]
struct Keep {
    condition: String,
    last_sent: f64,
    no_progress: u32,
    exhausted_logged: bool,
    no_record_logged: bool,
    override_logged: bool,
    /// The reset a limit hold was last logged for, so it is logged once.
    held_for_reset: i64,
    /// Set when a passed limit voided the no-progress count; logged once.
    rearmed_after_limit: bool,
    /// Agent deaths at or before this were already listed in a continue.
    listed_until: i64,
}

fn state() -> &'static Mutex<HashMap<String, Keep>> {
    static S: std::sync::OnceLock<Mutex<HashMap<String, Keep>>> = std::sync::OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// What to do for one worker, given its transcript, footer and keeper state.
/// Pure so the bounds are testable.
#[derive(Debug, PartialEq)]
pub enum Decision {
    Skip(&'static str),
    Send(String),
    Exhausted,
}

/// The minimum gap between two continuations of one worker, by anyone.
pub fn interval_s() -> f64 {
    env_f64("AMUX_GOAL_CONTINUE_INTERVAL_S", 600.0)
}

/// The last continuation of each worker by ANY mechanism (the keeper, or a
/// goal-continuation schedule), so two of them never wake one worker within
/// [`interval_s`] of each other. mxp-gs12 measured 84 such double wakes
/// inside ten minutes.
fn wakes() -> &'static Mutex<HashMap<String, (f64, String)>> {
    static W: std::sync::OnceLock<Mutex<HashMap<String, (f64, String)>>> = std::sync::OnceLock::new();
    W.get_or_init(Default::default)
}

pub fn note_wake(name: &str, now: f64, by: &str) {
    if let Ok(mut m) = wakes().lock() {
        m.insert(name.to_string(), (now, by.to_string()));
    }
}

pub fn last_wake(name: &str) -> Option<(f64, String)> {
    wakes().lock().ok()?.get(name).cloned()
}

/// `continue: X` is how a condition that was re-set by a continue looks
/// (mxp-gs12, 10-09); the keeper quotes the goal itself.
fn bare_condition(c: &str) -> &str {
    let t = c.trim();
    t.strip_prefix("continue:").map(str::trim).unwrap_or(t)
}

/// Where the worker's own last turn stands against a usage limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LimitGate {
    /// The last turn did not end on a usage limit.
    Clear,
    /// It did, and neither the named reset nor an account switch has come.
    Holding { reset_at: i64 },
    /// It did, and the reset passed or the account changed after it.
    Over,
}

/// [`LimitGate`] for a transcript tail, from the provider's structured 429
/// record (`transcript_rate_limit`) and the machine's last account switch.
pub fn limit_gate(records: &[Value], now: i64, account_changed_at: i64) -> LimitGate {
    let Some(reset_at) = crate::api::session_verbs::transcript_rate_limit(records) else {
        return LimitGate::Clear;
    };
    let limited_at = records
        .iter()
        .rev()
        .find(|r| r["type"] == "assistant" && r["error"] == "rate_limit" && r["isSidechain"] != true)
        .map(|r| ts_of(r) as i64)
        .unwrap_or(0);
    let reset_passed = reset_at > 0 && now >= reset_at + crate::api::agent_limit_resume::RESET_GRACE_S;
    let switched = account_changed_at > 0 && limited_at > 0 && account_changed_at >= limited_at;
    if reset_passed || switched {
        LimitGate::Over
    } else {
        LimitGate::Holding { reset_at }
    }
}

/// Did a turn after `since` end on a usage limit? A continue answered by a
/// 429 made no progress because it could not, so it must not count toward
/// exhaustion (gs12 review finding 1: the keeper spent its two continues into
/// a limit, then stayed exhausted through the reset).
pub fn limit_hit_since(records: &[Value], since: f64) -> bool {
    records
        .iter()
        .any(|r| r["type"] == "assistant" && r["error"] == "rate_limit" && r["isSidechain"] != true && ts_of(r) > since)
}

#[cfg(test)]
fn decide(k: &mut Keep, goal: &Goal, res: &Resolved, records: &[Value], now: f64) -> Decision {
    decide_with(k, goal, res, records, now, LimitGate::Clear)
}

fn decide_with(k: &mut Keep, goal: &Goal, res: &Resolved, records: &[Value], now: f64, limit: LimitGate) -> Decision {
    if res.state == "met" {
        return Decision::Skip("goal met");
    }
    if !res.active {
        return Decision::Skip("goal not active (cleared, or the worker is not pursuing it)");
    }
    if k.condition != goal.condition {
        *k = Keep { condition: goal.condition.clone(), ..Default::default() };
    }
    match limit {
        // A continue into an unexpired limit fails at once and only spends
        // the no-progress budget the reset will need.
        LimitGate::Holding { .. } => return Decision::Skip("usage limit not reset"),
        // The stop is over: whatever exhaustion the limit caused is void.
        LimitGate::Over if k.no_progress > 0 || k.exhausted_logged => {
            k.no_progress = 0;
            k.exhausted_logged = false;
            k.rearmed_after_limit = true;
        }
        _ => {}
    }
    // The interval ALWAYS holds, whatever else happened since the last send.
    if k.last_sent > 0.0 && now - k.last_sent < interval_s() {
        return Decision::Skip("interval");
    }
    // Someone else writing to the worker resets the no-progress count only.
    if k.last_sent > 0.0 && foreign_input_since(records, k.last_sent) {
        k.no_progress = 0;
        k.exhausted_logged = false;
    } else if k.last_sent > 0.0 && limit != LimitGate::Over && !limit_hit_since(records, k.last_sent) {
        if tool_uses_since(records, k.last_sent) == 0 {
            k.no_progress += 1;
        } else {
            k.no_progress = 0;
        }
    }
    if k.no_progress >= 2 {
        return Decision::Exhausted;
    }
    let cond = bare_condition(&goal.condition).chars().take(400).collect::<String>();
    let lead = if goal.cleared.as_ref().is_some_and(|c| c.by_error) {
        format!("Your /goal was cleared by an error, not met (amux keeps it): \"{cond}\". Re-arm it with /goal if you want Claude Code's own check back.")
    } else if goal.met {
        format!("Claude Code's evaluator said your /goal is met, but your goal checker says not: \"{cond}\". The checker decides.")
    } else {
        format!("Your /goal is not met yet: \"{cond}\".")
    };
    Decision::Send(format!(
        "{MARK} {lead} Keep going on everything you can do without the owner. \
         If every remaining step needs the owner, list those in one line and stop."
    ))
}

/// The dedupe key for "this goal was reported met": one notice per goal
/// condition, so a worker re-evaluated as met every tick is told once, and a
/// new /goal on the same worker is told again.
pub fn met_notice_key(condition: &str) -> String {
    use sha2::Digest;
    hex::encode(&sha2::Sha256::digest(condition.trim().as_bytes())[..8])
}

/// Tell the owner a worker's goal is met, once per goal. "Describe what you
/// want" starts a worker on a /goal and promises to say when it finishes
/// (Ethan, 2026-10-18); the dashboard says so live, this reaches a closed one
/// by web push. Recorded in lane meta (`goal_met_notified`) so a restart does
/// not repeat it. Log: verdict=goal_met_notified with the delivery count, so
/// "no subscriptions" reads as 0 delivered, not as silence.
async fn notify_met_once(app: &crate::api::AppState, name: &str, condition: &str) {
    let key = met_notice_key(condition);
    let meta = crate::api::session_verbs::load_meta(name);
    if crate::api::session_verbs::meta_str(&meta, "goal_met_notified") == key {
        return;
    }
    crate::api::session_verbs::update_meta(name, &[("goal_met_notified", Value::String(key))]);
    let body: String = condition.chars().take(180).collect();
    let results = crate::push::send_all(app, &format!("{name} finished its goal"), &body, name,
        &format!("goal-met:{name}"), &format!("/#peek={name}")).await;
    let delivered = results.iter().filter(|r| r["status"].as_u64().is_some_and(|c| (200..300).contains(&c))).count();
    tracing::info!(target: "amux::goal_keeper", session = %name, verdict = "goal_met_notified",
        delivered, measured = true, n_considered = results.len(),
        "the /goal is met; told the owner by web push ({delivered} of {} subscription(s) delivered)", results.len());
}

async fn tick(app: crate::api::AppState) {
    let Ok(sessions) = crate::api::sessions_legacy::legacy_sessions_values(app.store.clone()).await else {
        return;
    };
    let now = crate::config::now_f64();
    let idle_s = env_f64("AMUX_GOAL_CONTINUE_IDLE_S", 120.0);
    let mut considered = 0usize;
    // AMUX-5277: release answered owner blocks and restore parked goals first.
    let stamped: Vec<String> = sessions
        .iter()
        .filter_map(|s| s["name"].as_str())
        .filter(|n| crate::api::goal_loop::stamped(&crate::api::session_verbs::load_meta(n)))
        .map(str::to_string)
        .collect();
    if !stamped.is_empty() {
        crate::api::goal_loop::restore_tick(&app, &stamped).await;
    }
    for s in &sessions {
        let name = s["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() || s["running"] != true || s["archived"] == true || s["provider"] != "claude" {
            continue;
        }
        // The worker's own checker runs whatever the worker is doing: it is
        // shell, costs no turn, and drives the goal tile and the stall alert.
        super::goal_check::maybe_run(&app, &name, s["dir"].as_str().unwrap_or(""), now);
        // A lane the goal-loop guard parked on the owner is not continued:
        // continuing it is the loop (AMUX-5277).
        if stamped.contains(&name) {
            continue;
        }
        // Board-drive's standing orders already continue these.
        if s["auto_continue"] == true {
            continue;
        }
        if s["agent_state"] != "idle" || s["turn_state"] != "completed" {
            continue;
        }
        let Some(progress_at) = s["last_progress_at"].as_f64() else { continue };
        if now - progress_at < idle_s || !enabled(&name) {
            continue;
        }
        let Some(path) = crate::api::session_verbs::session_jsonl_path(&name) else { continue };
        let n2 = name.clone();
        let (records, footer, goal) = tokio::task::spawn_blocking(move || {
            let records = crate::api::session_verbs::iter_jsonl_tail(&path, 6_000_000);
            let goal = cached_goal(&path).ok().flatten();
            let footer = footer_goal_active(&n2).unwrap_or(false);
            (records, footer, goal)
        })
        .await
        .unwrap_or_default();
        let Some(goal) = goal else {
            // The footer says a goal is active but no record of one was
            // found: the keeper cannot continue it, so say so once.
            if footer {
                let Ok(mut map) = state().lock() else { return };
                let k = map.entry(name.clone()).or_default();
                if !k.no_record_logged {
                    k.no_record_logged = true;
                    tracing::warn!(target: "amux::goal_keeper", session = %name, verdict = "goal_footer_without_record",
                        measured = true, n_considered = 1,
                        "footer shows /goal active but the transcript has no goal_status record; the keeper cannot continue this worker");
                }
            }
            continue;
        };
        considered += 1;
        let checker = super::goal_check::view(&crate::api::session_verbs::home(), &name, now);
        let res = resolve(Some(&goal), || Some(footer), &checker);
        if res.state == "met" {
            notify_met_once(&app, &name, &goal.condition).await;
        }
        // A goal-continuation schedule already woke this worker recently.
        if let Some((at, by)) = last_wake(&name) {
            if by != "goal keeper" && now - at < interval_s() {
                tracing::debug!(target: "amux::goal_keeper", session = %name, verdict = "goal_continue_skipped_recent_wake",
                    by = %by, measured = true, n_considered = 1, "another continuation woke this worker {:.0}s ago", now - at);
                continue;
            }
        }
        let now_s = now as i64;
        let account_changed_at = crate::api::session_verbs::observe_claude_account_change(now_s);
        let gate = limit_gate(&records, now_s, account_changed_at);
        let (decision, listed_until, rearmed) = {
            let Ok(mut map) = state().lock() else { return };
            let k = map.entry(name.clone()).or_default();
            let d = decide_with(k, &goal, &res, &records, now, gate);
            if let LimitGate::Holding { reset_at } = gate {
                if k.held_for_reset != reset_at {
                    k.held_for_reset = reset_at;
                    tracing::info!(target: "amux::goal_keeper", session = %name, verdict = "goal_continue_held_for_limit",
                        reset_at, account_changed_at, measured = true, n_considered = 1,
                        "the worker's last turn ended on a usage limit; the keeper waits for its reset or an account switch");
                }
            }
            if matches!(d, Decision::Send(_)) && !k.override_logged && (goal.met || goal.cleared.is_some()) {
                k.override_logged = true;
                let verdict = if goal.met { "goal_met_overruled_by_checker" } else { "goal_continue_after_error_clear" };
                tracing::warn!(target: "amux::goal_keeper", session = %name, verdict, state = res.state,
                    measured = true, n_considered = 1, "continuing a goal Claude Code no longer pursues: {}", res.why);
            }
            if d == Decision::Exhausted {
                if k.exhausted_logged {
                    continue;
                }
                k.exhausted_logged = true;
            }
            let rearmed = std::mem::take(&mut k.rearmed_after_limit);
            (d, k.listed_until, rearmed)
        };
        if rearmed {
            tracing::warn!(target: "amux::goal_keeper", session = %name, verdict = "goal_continue_rearmed_after_limit",
                measured = true, n_considered = 1,
                "a usage limit is over; the no-progress count it caused is cleared and the keeper continues the goal");
        }
        match decision {
            Decision::Skip(_) => {}
            Decision::Exhausted => {
                tracing::warn!(target: "amux::goal_keeper", session = %name, verdict = "goal_continue_exhausted",
                    measured = true, n_considered = 1,
                    "two continues produced no tool call; the keeper stops for this goal until it changes or the owner writes");
            }
            Decision::Send(text) => {
                // Background agents a limit or logout killed, whose stop is
                // over, ride along (gs12 review finding 1): the owner typed
                // these resumes by hand.
                use crate::api::agent_limit_resume as alr;
                let (killed, notes_considered) = alr::limit_killed_agents(&records, listed_until);
                let over: Vec<_> =
                    killed.into_iter().filter(|k| alr::kill_is_over(k, now_s, account_changed_at)).collect();
                let window_pct = alr::fresh_window_pct();
                let text = alr::resume_text(&text, &over, window_pct, &name);
                let home = crate::api::session_verbs::home();
                let (text, carried) = super::receipts::attach(&home, &name, &text);
                let out = crate::api::session_verbs::deliver_automated(&app, &name, &text, &format!("goal:{name}")).await;
                if !out.refused {
                    super::receipts::consume(&home, &name, carried);
                    note_wake(&name, now, "goal keeper");
                }
                // IN MESSAGES, LIKE A SCHEDULE FIRE (gs12 review, 2026-10-18):
                // 357 delivered continues into mxp-gs12 were in no history the
                // owner reads. Recorded only once delivery is confirmed, the
                // scheduler's rule, so the tab never shows a send that did not land.
                let recorded = goal_continue_recordable(&out);
                if recorded {
                    crate::api::session_verbs::cmd_hist_record_full(
                        &app,
                        &name,
                        &text,
                        "system",
                        GOAL_KEEPER_ORIGIN,
                        true,
                        crate::api::session_verbs::DeliveryMeta::direct(),
                    )
                    .await;
                }
                tracing::info!(target: "amux::goal_keeper", session = %name, recorded, measured = true, n_considered = 1,
                    verdict = if recorded { "goal_continue_recorded" } else { "goal_continue_not_recorded" },
                    "goal keeper continue and its Messages record");
                if let Ok(mut map) = state().lock() {
                    if let Some(k) = map.get_mut(&name) {
                        k.last_sent = now;
                        if !out.refused {
                            if let Some(newest) = over.iter().map(|a| a.at).max() {
                                k.listed_until = newest;
                            }
                        }
                    }
                }
                tracing::info!(target: "amux::goal_keeper", session = %name, verdict = "goal_continue_sent",
                    submitted = ?out.submitted, refused = out.refused, measured = true, n_considered = 1,
                    killed_agents_listed = over.len(), agent_notifications_considered = notes_considered,
                    window_pct = ?window_pct, limit_gate = ?gate,
                    "continued a worker whose /goal is not met: {}", out.message);
            }
        }
    }
    tracing::debug!(target: "amux::goal_keeper", measured = true, n_considered = considered, "goal keeper tick");
}

/// The Messages `origin` of a goal-keeper continue.
pub const GOAL_KEEPER_ORIGIN: &str = "goal-keeper";

/// A continue goes into Messages only when it landed now: not refused, not
/// parked on the steering queue, and not read back as unsubmitted.
pub(crate) fn goal_continue_recordable(out: &crate::api::session_verbs::AutoDelivery) -> bool {
    !out.refused && out.queue_id.is_none() && out.submitted != Some(false)
}

pub fn spawn(app: crate::api::AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, 60, move || {
        let app = app.clone();
        async move { tick(app).await }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_met_goal_is_noticed_once_per_condition() {
        let a = super::met_notice_key("ship dark mode");
        assert_eq!(a, super::met_notice_key("  ship dark mode \n"), "whitespace is not a new goal");
        assert_ne!(a, super::met_notice_key("ship light mode"), "a new goal is noticed again");
        assert_eq!(a.len(), 16);
    }

    /// Only a continue that landed now is written to Messages.
    #[test]
    fn only_a_landed_continue_is_recorded_in_messages() {
        use crate::api::session_verbs::AutoDelivery;
        let d = |submitted: Option<bool>, queue: Option<&str>, refused: bool| AutoDelivery {
            message: String::new(),
            submitted,
            submission: "",
            queue_id: queue.map(str::to_string),
            refused,
        };
        assert!(super::goal_continue_recordable(&d(Some(true), None, false)));
        assert!(super::goal_continue_recordable(&d(None, None, false)), "unread composer: delivered, as the scheduler counts it");
        assert!(!super::goal_continue_recordable(&d(Some(false), None, false)), "not submitted");
        assert!(!super::goal_continue_recordable(&d(None, Some("q1"), false)), "queued, not landed");
        assert!(!super::goal_continue_recordable(&d(Some(false), None, true)), "refused");
    }

    /// A worker whose transcript cannot be found says so: an absent field was
    /// read as "no goal" (amux-helper, mxp-gs12, 2026-10-08).
    /// The `random` case: the transcript's last goal_status is unmet, but
    /// the goal was cleared, so the footer no longer shows it. The badge must
    /// read inactive and say why; an unmet goal WITH the footer stays active;
    /// a met goal never consults the footer.
    #[test]
    fn a_cleared_goal_reads_inactive_and_the_footer_is_only_read_for_an_unmet_one() {
        let unmet = || Some(super::Goal { met: false, condition: "use the bumble app".into(), ts: 1.0, cleared: None });
        let v = super::goal_view(unmet(), || Some(false), &serde_json::json!({"configured": false}));
        assert_eq!(v["active"], false, "{v}");
        assert_eq!(v["cleared"], true, "{v}");
        let v = super::goal_view(unmet(), || Some(true), &serde_json::json!({"configured": false}));
        assert_eq!(v["active"], true, "{v}");
        assert_eq!(v["cleared"], false, "{v}");
        let v = super::goal_view(unmet(), || None, &serde_json::json!({"configured": false}));
        assert_eq!(v["active"], false, "no pane to read: nothing is pursuing it: {v}");
        let met = Some(super::Goal { met: true, condition: "x".into(), ts: 1.0, cleared: None });
        let v = super::goal_view(met, || panic!("a met goal must not read the footer"), &serde_json::json!({"configured": false}));
        assert_eq!(v["active"], false, "{v}");
        let v = super::goal_view(None, || panic!("no goal must not read the footer"), &serde_json::json!({"configured": false}));
        assert_eq!(v["active"], false, "{v}");
    }
    #[test]
    fn goal_payload_without_a_transcript_is_unmeasured_not_absent() {
        let v = super::goal_payload("no-such-worker-for-goal-test");
        assert_eq!(v["measured"], false, "{v}");
        assert!(v["why_unmeasured"].as_str().unwrap_or("").len() > 3, "{v}");
        assert_eq!(v["active"], false);
    }

    use super::*;
    use serde_json::json;

    fn goal_rec(ts: &str, met: bool, cond: &str) -> Value {
        json!({"type":"attachment","timestamp":ts,"attachment":{"type":"goal_status","met":met,"condition":cond}})
    }
    fn tool(ts: &str) -> Value {
        json!({"type":"assistant","timestamp":ts,"message":{"content":[{"type":"tool_use","name":"Bash"}]}})
    }
    fn user(ts: &str, text: &str) -> Value {
        json!({"type":"user","timestamp":ts,"message":{"content":text}})
    }
    fn t(s: &str) -> f64 { chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp() as f64 }

    /// mxp-gs12, 2026-10-09: the goal record sat 7.1 MB before the end and a
    /// 6 MB tail never saw it. The backward scan finds it at any distance,
    /// including when a chunk boundary cuts the record in half.
    #[test]
    fn a_goal_far_before_the_tail_is_still_found() {
        let dir = std::env::temp_dir().join(format!("goal-far-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let mut body = String::new();
        body.push_str(&format!("{}\n", goal_rec("2026-10-08T21:00:00Z", false, "older goal")));
        body.push_str(&format!("{}\n", goal_rec("2026-10-09T01:27:40Z", false, "every proof row passes")));
        for i in 0..2000 {
            body.push_str(&format!("{}\n", json!({"type":"assistant","timestamp":"2026-10-09T02:00:00Z","n":i,"pad":"x".repeat(200)})));
        }
        std::fs::write(&p, &body).unwrap();
        let len = body.len() as u64;
        // Tail reading misses it, as it did live.
        assert!(latest_goal(&crate::api::session_verbs::iter_jsonl_tail(&p, 100_000)).is_none());
        // Every chunk size finds the NEWER goal, whether or not a boundary cuts it.
        for chunk in [97u64, 1000, 4096, 100_000, len + 1] {
            let mut f = std::fs::File::open(&p).unwrap();
            let g = last_goal_before(&mut f, len, chunk).goal.unwrap_or_else(|| panic!("chunk {chunk}: not found"));
            assert_eq!(g.condition, "every proof row passes", "chunk {chunk}");
        }
        let g = cached_goal(&p).unwrap().unwrap();
        assert_eq!(g.condition, "every proof row passes");
        // An appended goal is picked up incrementally.
        let mut more = body.clone();
        more.push_str(&format!("{}\n", goal_rec("2026-10-09T03:00:00Z", true, "every proof row passes")));
        std::fs::write(&p, &more).unwrap();
        assert!(cached_goal(&p).unwrap().unwrap().met);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_the_newest_goal_and_counts_progress_as_tool_calls() {
        let recs = vec![goal_rec("2026-09-26T13:14:37Z", true, "old"), goal_rec("2026-09-26T18:50:42Z", false, "finish cicd"),
                        tool("2026-09-26T19:00:00Z"), user("2026-09-26T19:05:00Z", &format!("{MARK} keep going"))];
        let g = latest_goal(&recs).unwrap();
        assert_eq!((g.met, g.condition.as_str()), (false, "finish cicd"));
        assert_eq!(tool_uses_since(&recs, t("2026-09-26T18:55:00Z")), 1);
        assert_eq!(tool_uses_since(&recs, t("2026-09-26T19:01:00Z")), 0);
        // The keeper's own message is not foreign input; the owner's is.
        assert!(!foreign_input_since(&recs, t("2026-09-26T19:00:00Z")));
        let mut with_owner = recs.clone();
        with_owner.push(user("2026-09-26T19:10:00Z", "whats the status?"));
        assert!(foreign_input_since(&with_owner, t("2026-09-26T19:06:00Z")));
        // Machine records written as `user` are not someone writing.
        let machine = vec![
            user("2026-09-26T19:10:00Z", "<task-notification>\n<task-id>b1</task-id>"),
            user("2026-09-26T19:11:00Z", "<system-reminder>x</system-reminder>"),
            json!({"type":"user","isMeta":true,"timestamp":"2026-09-26T19:12:00Z","message":{"content":"Caveat"}}),
        ];
        assert!(!foreign_input_since(&machine, t("2026-09-26T19:06:00Z")));
    }

    #[test]
    fn two_idle_continues_exhaust_and_owner_input_or_a_new_goal_resets() {
        let goal = Goal { met: false, condition: "finish cicd".into(), ts: 0.0, cleared: None };
        let mut k = Keep::default();
        let base = t("2026-09-26T20:00:00Z");
        // First continue.
        assert!(matches!(dec(&mut k, &goal, &[], true, base), Decision::Send(_)));
        k.last_sent = base;
        // Too soon.
        assert_eq!(dec(&mut k, &goal, &[], true, base + 60.0), Decision::Skip("interval"));
        // Two intervals with no tool call: second send, then exhausted.
        assert!(matches!(dec(&mut k, &goal, &[], true, base + 700.0), Decision::Send(_)));
        k.last_sent = base + 700.0;
        assert_eq!(dec(&mut k, &goal, &[], true, base + 1400.0), Decision::Exhausted);
        // Progress would have kept it going.
        let mut k2 = Keep { condition: "finish cicd".into(), last_sent: base, ..Default::default() };
        let recs = vec![tool("2026-09-26T20:05:00Z")];
        assert!(matches!(dec(&mut k2, &goal, &recs, true, base + 700.0), Decision::Send(_)));
        assert_eq!(k2.no_progress, 0);
        // The owner writing resets the count...
        let owner = vec![user("2026-09-26T20:30:00Z", "keep going")];
        assert!(matches!(dec(&mut k, &goal, &owner, true, base + 2000.0), Decision::Send(_)));
        // ...but never the interval: a task notification or an owner message
        // one minute after a send does not allow another send.
        let mut k4 = Keep { condition: "finish cicd".into(), last_sent: base, ..Default::default() };
        let noise = vec![user("2026-09-26T20:01:00Z", "<task-notification>done"), user("2026-09-26T20:02:00Z", "hi")];
        assert_eq!(dec(&mut k4, &goal, &noise, true, base + 300.0), Decision::Skip("interval"));
        // A new goal resets too.
        let mut k3 = Keep { condition: "old".into(), last_sent: base, no_progress: 5, ..Default::default() };
        assert!(matches!(dec(&mut k3, &goal, &[], true, base + 10.0), Decision::Send(_)));
        // Met or cleared: never.
        assert_eq!(dec(&mut Keep::default(), &Goal { met: true, ..goal.clone() }, &[], true, base), Decision::Skip("goal met"));
        assert!(matches!(dec(&mut Keep::default(), &goal, &[], false, base), Decision::Skip(_)));
    }

    fn dec(k: &mut Keep, g: &Goal, recs: &[Value], footer: bool, now: f64) -> Decision {
        let res = resolve(Some(g), || Some(footer), &json!({"configured": false}));
        decide(k, g, &res, recs, now)
    }

    fn dec_gate(k: &mut Keep, g: &Goal, recs: &[Value], now: f64, gate: LimitGate) -> Decision {
        let res = resolve(Some(g), || Some(true), &json!({"configured": false}));
        decide_with(k, g, &res, recs, now, gate)
    }

    fn limit_rec(ts: &str, reset: i64) -> Value {
        json!({"type":"assistant","timestamp":ts,"error":"rate_limit","isApiErrorMessage":true,
            "quotaLimits":{"status":"rejected","resetsAt":reset,"rateLimitType":"five_hour"},
            "message":{"role":"assistant","content":[{"type":"text","text":"You've hit your session limit"}]}})
    }

    /// gs12 review finding 1, the live sequence of 2026-10-12: a limit at
    /// 17:13Z resetting 18:50Z; the keeper continued at 17:23 (answered by a
    /// 429 two seconds later) and again at 18:20, which spent its whole
    /// no-progress budget inside the limit.
    #[test]
    fn a_usage_limit_holds_the_keeper_and_its_end_rearms_it() {
        let goal = Goal { met: false, condition: "finish gs12".into(), ts: 0.0, cleared: None };
        let hit = t("2026-10-12T17:13:14Z");
        let reset = t("2026-10-12T18:50:00Z") as i64;
        let recs = vec![limit_rec("2026-10-12T17:13:14Z", reset)];
        assert_eq!(limit_gate(&recs, hit as i64 + 600, 0), LimitGate::Holding { reset_at: reset });
        let mut k = Keep::default();
        let held = dec_gate(&mut k, &goal, &recs, hit + 600.0, LimitGate::Holding { reset_at: reset });
        assert_eq!(held, Decision::Skip("usage limit not reset"));
        assert_eq!((k.no_progress, k.last_sent), (0, 0.0), "a hold spends nothing");
        // Over at the reset, or at an account switch after the limit; a
        // switch before it is the account that hit it.
        assert_eq!(limit_gate(&recs, reset + 61, 0), LimitGate::Over);
        assert_eq!(limit_gate(&recs, hit as i64 + 900, hit as i64 + 300), LimitGate::Over);
        assert_eq!(limit_gate(&recs, hit as i64 + 900, hit as i64 - 300), LimitGate::Holding { reset_at: reset });
        // A later turn ends the limit entirely.
        let mut after = recs.clone();
        after.push(user("2026-10-12T19:00:00Z", "continue"));
        assert_eq!(limit_gate(&after, reset + 600, 0), LimitGate::Clear);
        // An exhausted keeper (the pre-fix state) is re-armed by the reset.
        let mut spent = Keep { condition: "finish gs12".into(), last_sent: hit, no_progress: 2, exhausted_logged: true, ..Default::default() };
        let d = dec_gate(&mut spent, &goal, &recs, reset as f64 + 700.0, LimitGate::Over);
        assert!(matches!(d, Decision::Send(_)), "{d:?}");
        assert!(spent.rearmed_after_limit && !spent.exhausted_logged);
        // CONTROL: the same keeper with no limit in play stays exhausted.
        let mut spent2 = Keep { condition: "finish gs12".into(), last_sent: hit, no_progress: 2, exhausted_logged: true, ..Default::default() };
        assert_eq!(dec_gate(&mut spent2, &goal, &[], reset as f64 + 700.0, LimitGate::Clear), Decision::Exhausted);
    }

    #[test]
    fn a_continue_answered_by_a_429_is_not_counted_as_no_progress() {
        let goal = Goal { met: false, condition: "finish gs12".into(), ts: 0.0, cleared: None };
        let sent = t("2026-10-12T17:23:34Z");
        let answered = vec![user("2026-10-12T17:23:34Z", &format!("{MARK} keep going")), limit_rec("2026-10-12T17:23:36Z", 0)];
        assert!(limit_hit_since(&answered, sent - 1.0));
        let mut k = Keep { condition: "finish gs12".into(), last_sent: sent, no_progress: 1, ..Default::default() };
        let d = dec_gate(&mut k, &goal, &answered, sent + 700.0, LimitGate::Clear);
        assert!(matches!(d, Decision::Send(_)), "{d:?}");
        assert_eq!(k.no_progress, 1, "the 429 neither counts nor clears");
        // CONTROL: the same silence without a 429 does count.
        let silent = vec![user("2026-10-12T17:23:34Z", &format!("{MARK} keep going"))];
        let mut k2 = Keep { condition: "finish gs12".into(), last_sent: sent, no_progress: 1, ..Default::default() };
        assert_eq!(dec_gate(&mut k2, &goal, &silent, sent + 700.0, LimitGate::Clear), Decision::Exhausted);
    }

    fn sentinel(ts: &str, met: bool, cond: &str) -> Value {
        json!({"type":"attachment","timestamp":ts,"attachment":{"type":"goal_status","met":met,"sentinel":true,"condition":cond}})
    }

    /// The mxp-gs12 specimen, 2026-10-17 22:54:45.929Z: a sentinel `met: true`
    /// and the error informational in the same millisecond. That is a goal an
    /// error cleared, so it reads UNKNOWN (never met) and the keeper keeps
    /// going without the footer; a `/goal clear` reads cleared and is left
    /// alone; an evaluator's verdict (no sentinel) is met.
    #[test]
    fn an_error_clear_is_unknown_and_continued_and_a_command_clear_is_left_alone() {
        let cond = "continue: Every section 6 completion-proof row passes";
        let err = json!({"type":"system","subtype":"informational","timestamp":"2026-10-17T22:54:45.929Z",
            "content":"Goal cleared after an unrecoverable error (authentication failed): \"continue: Every\u{2026}\". Run /goal"});
        let no = json!({"configured": false});
        for recs in [
            vec![goal_rec("2026-10-17T21:43:04Z", false, cond), sentinel("2026-10-17T22:54:45.929Z", true, cond), err.clone()],
            vec![err.clone(), sentinel("2026-10-17T22:54:45.929Z", true, cond)],
        ] {
            let g = latest_goal(&recs).unwrap();
            assert!(!g.met, "a sentinel met is not the evaluator's verdict: {g:?}");
            assert!(g.cleared.as_ref().unwrap().by_error, "{g:?}");
            let r = resolve(Some(&g), || panic!("an error clear has no footer to read"), &no);
            assert_eq!((r.state, r.active), ("unknown", true), "{r:?}");
            assert!(r.why.contains("authentication failed"), "{r:?}");
            let v = goal_view(Some(g.clone()), || None, &no);
            assert_eq!((v["met"].clone(), v["state"].clone(), v["active"].clone()), (json!(false), json!("unknown"), json!(true)), "{v}");
            // The keeper continues it, quoting the goal without the `continue:` prefix.
            match decide(&mut Keep::default(), &g, &r, &[], 1e9) {
                Decision::Send(t) => {
                    assert!(t.contains("cleared by an error") && t.contains("\"Every section 6"), "{t}");
                }
                d => panic!("{d:?}"),
            }
        }
        // `/goal clear`: a sentinel met with no error nearby.
        let g = latest_goal(&[sentinel("2026-10-15T17:06:53Z", true, "execute the plan")]).unwrap();
        let r = resolve(Some(&g), || panic!("no footer"), &no);
        assert_eq!((r.state, r.active), ("cleared", false), "{r:?}");
        // An error informational for a different, older clear does not attach.
        let recs = vec![err.clone(), sentinel("2026-10-18T09:00:00Z", true, cond)];
        assert!(!latest_goal(&recs).unwrap().cleared.unwrap().by_error);
        // The evaluator's verdict.
        let real = json!({"type":"attachment","timestamp":"2026-10-15T17:00:00Z","attachment":{"type":"goal_status","met":true,
            "condition":"x","reason":"done","iterations":3,"durationMs":10}});
        let g = latest_goal(&[real]).unwrap();
        assert!(g.met && g.cleared.is_none());
        assert_eq!(resolve(Some(&g), || None, &no).state, "met");
    }

    /// The checker decides "met", and a checker that cannot run makes it
    /// unknown, never met.
    #[test]
    fn the_checker_outranks_the_evaluator_and_an_unreadable_checker_is_unknown() {
        let evaluator_met = Goal { met: true, condition: "rows pass".into(), ts: 1.0, cleared: None };
        let unmet_ck = json!({"configured": true, "measured": true, "met": false, "done": 44, "total": 56});
        let r = resolve(Some(&evaluator_met), || None, &unmet_ck);
        assert_eq!((r.state, r.active), ("unmet", true), "{r:?}");
        assert!(r.why.contains("44 of 56"), "{r:?}");
        let broken = json!({"configured": true, "measured": false, "why_unmeasured": "checker exited 2"});
        let r = resolve(Some(&evaluator_met), || None, &broken);
        assert_eq!(r.state, "unknown", "{r:?}");
        // An unmet goal whose checker says met: met, and the keeper stops.
        let unmet = Goal { met: false, ..evaluator_met.clone() };
        let met_ck = json!({"configured": true, "measured": true, "met": true, "done": 56, "total": 56});
        let r = resolve(Some(&unmet), || panic!("met by the checker needs no footer"), &met_ck);
        assert_eq!((r.state, r.active), ("met", false));
        assert_eq!(decide(&mut Keep::default(), &unmet, &r, &[], 1e9), Decision::Skip("goal met"));
        // No /goal at all: the checker alone drives the tile.
        assert_eq!(resolve(None, || None, &unmet_ck).state, "unmet");
        assert_eq!(resolve(None, || None, &json!({"configured": false})).state, "none");
        // An unreadable transcript is unknown, with why.
        let v = goal_view_unread("transcript unreadable: permission denied", &json!({"configured": false}));
        assert_eq!((v["state"].clone(), v["met"].clone()), (json!("unknown"), json!(false)), "{v}");
        assert!(v["state_why"].as_str().unwrap().contains("permission denied"));
    }

    #[test]
    fn the_cached_reader_sees_an_error_clear_appended_after_the_goal() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        let cond = "rows pass";
        std::fs::write(&p, format!("{}\n", goal_rec("2026-10-17T21:43:04Z", false, cond))).unwrap();
        assert!(cached_goal(&p).unwrap().unwrap().cleared.is_none());
        let mut body = std::fs::read_to_string(&p).unwrap();
        body.push_str(&format!("{}\n", sentinel("2026-10-17T22:54:45.929Z", true, cond)));
        body.push_str(&format!("{}\n", json!({"type":"system","subtype":"informational","timestamp":"2026-10-17T22:54:45.929Z",
            "content":"Goal cleared after an unrecoverable error (authentication failed)"})));
        std::fs::write(&p, &body).unwrap();
        let g = cached_goal(&p).unwrap().unwrap();
        assert!(g.cleared.unwrap().by_error && !g.met);
        // A fresh backward scan reaches the same answer.
        let mut f = std::fs::File::open(&p).unwrap();
        let s = last_goal_before(&mut f, body.len() as u64, 64);
        assert!(s.goal.unwrap().cleared.unwrap().by_error);
    }

    #[test]
    fn one_continuation_mechanism_suppresses_the_other() {
        note_wake("wake-test-w", 1000.0, "sched:SCHED-656");
        assert_eq!(last_wake("wake-test-w"), Some((1000.0, "sched:SCHED-656".to_string())));
    }
}
