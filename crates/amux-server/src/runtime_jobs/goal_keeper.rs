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
    pub met: bool,
    pub condition: String,
    pub ts: f64,
}

/// The newest `goal_status` attachment in a transcript tail.
pub fn latest_goal(records: &[Value]) -> Option<Goal> {
    records.iter().rev().find_map(|r| {
        let a = &r["attachment"];
        (a["type"] == "goal_status").then(|| Goal {
            met: a["met"].as_bool().unwrap_or(false),
            condition: a["condition"].as_str().unwrap_or("").to_string(),
            ts: ts_of(r),
        })
    })
}

/// A worker's Claude Code `/goal` for the session payload (amux-helper ask,
/// 2026-10-08: GET /api/sessions/mxp-gs12 carried no goal while the worker
/// showed "/goal active (10m)", and the absence was read as "no goal").
///
/// INCREMENTAL, because the session list is polled ~6000 times an hour: per
/// transcript it keeps the bytes already read and the last goal seen, and on
/// a later call scans only what was appended for `goal_status` lines. A new
/// or truncated file is read once from its tail. Always answers `measured`;
/// a transcript that cannot be read says why instead of leaving the field out.
pub fn goal_payload(name: &str) -> Value {
    use std::io::{Read, Seek, SeekFrom};
    type Cache = std::collections::HashMap<std::path::PathBuf, (u64, Option<Goal>)>;
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Cache>> = std::sync::OnceLock::new();
    let Some(path) = crate::api::session_verbs::session_jsonl_path(name) else {
        return serde_json::json!({"measured": false, "why_unmeasured": "no transcript for this worker", "active": false});
    };
    let len = match std::fs::metadata(&path) {
        Ok(m) => m.len(),
        Err(e) => return serde_json::json!({"measured": false, "why_unmeasured": format!("transcript unreadable: {e}"), "active": false}),
    };
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    let (mut offset, mut goal) = cache.get(&path).cloned().unwrap_or((u64::MAX, None));
    if offset == u64::MAX || len < offset {
        goal = latest_goal(&crate::api::session_verbs::iter_jsonl_tail(&path, 6_000_000));
        offset = len;
    } else if len > offset {
        let mut buf = Vec::new();
        if let Ok(mut f) = std::fs::File::open(&path) {
            if f.seek(SeekFrom::Start(offset)).is_ok() {
                let _ = f.take(len - offset).read_to_end(&mut buf);
            }
        }
        // Only whole lines; a half-written last record is read next time.
        let whole = buf.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
        for line in buf[..whole].split(|&b| b == b'\n') {
            if line.windows(11).any(|w| w == b"goal_status") {
                if let Ok(v) = serde_json::from_slice::<Value>(line) {
                    if let Some(g) = latest_goal(std::slice::from_ref(&v)) {
                        goal = Some(g);
                    }
                }
            }
        }
        offset += whole as u64;
    }
    cache.insert(path, (offset, goal.clone()));
    match goal {
        Some(g) => serde_json::json!({
            "measured": true,
            "active": !g.met && !g.condition.trim().is_empty(),
            "met": g.met,
            "condition": g.condition,
            "since": g.ts,
        }),
        None => serde_json::json!({"measured": true, "active": false}),
    }
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

fn decide(k: &mut Keep, goal: &Goal, records: &[Value], footer_active: bool, now: f64) -> Decision {
    if goal.met {
        return Decision::Skip("goal met");
    }
    if !footer_active {
        return Decision::Skip("footer shows no active goal (cleared)");
    }
    if k.condition != goal.condition {
        *k = Keep { condition: goal.condition.clone(), ..Default::default() };
    }
    // The interval ALWAYS holds, whatever else happened since the last send.
    if k.last_sent > 0.0 && now - k.last_sent < env_f64("AMUX_GOAL_CONTINUE_INTERVAL_S", 600.0) {
        return Decision::Skip("interval");
    }
    // Someone else writing to the worker resets the no-progress count only.
    if k.last_sent > 0.0 && foreign_input_since(records, k.last_sent) {
        k.no_progress = 0;
        k.exhausted_logged = false;
    } else if k.last_sent > 0.0 {
        if tool_uses_since(records, k.last_sent) == 0 {
            k.no_progress += 1;
        } else {
            k.no_progress = 0;
        }
    }
    if k.no_progress >= 2 {
        return Decision::Exhausted;
    }
    Decision::Send(format!(
        "{MARK} Your /goal is not met yet: \"{}\". Keep going on everything you can do without the owner. \
         If every remaining step needs the owner, list those in one line and stop.",
        goal.condition.chars().take(400).collect::<String>()
    ))
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
    // Owner policy: answer in-boundary asks already parked on idle isolated lanes.
    let idle_isolated: Vec<String> = sessions
        .iter()
        .filter(|s| s["running"] == true && s["provider"] == "claude" && s["status"] == "idle"
            || (s["running"] == true && s["provider"] == "claude" && s["waiting_reason"] == "owner"))
        .filter_map(|s| s["name"].as_str())
        .filter(|n| crate::api::session_verbs::session_is_isolated(n))
        .map(str::to_string)
        .collect();
    if !idle_isolated.is_empty() {
        crate::api::turn_end::auto_proceed_open_isolated_asks(&app, &idle_isolated).await;
    }
    for s in &sessions {
        let name = s["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() || s["running"] != true || s["archived"] == true || s["provider"] != "claude" {
            continue;
        }
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
        let (records, footer) = tokio::task::spawn_blocking(move || {
            let records = crate::api::session_verbs::iter_jsonl_tail(&path, 6_000_000);
            // Exact-match target: a bare `amux-x` resolves to a sibling
            // `amux-x-2` pane while `amux-x` is briefly absent.
            let pt = crate::backend::tmux::pane_target(&format!("amux-{n2}"));
            let footer = std::process::Command::new("tmux")
                .args(["capture-pane", "-p", "-t", &pt, "-S", "-12"])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains("/goal active"))
                .unwrap_or(false);
            (records, footer)
        })
        .await
        .unwrap_or_default();
        let Some(goal) = latest_goal(&records) else { continue };
        considered += 1;
        let decision = {
            let Ok(mut map) = state().lock() else { return };
            let k = map.entry(name.clone()).or_default();
            let d = decide(k, &goal, &records, footer, now);
            if d == Decision::Exhausted {
                if k.exhausted_logged {
                    continue;
                }
                k.exhausted_logged = true;
            }
            d
        };
        match decision {
            Decision::Skip(_) => {}
            Decision::Exhausted => {
                tracing::warn!(target: "amux::goal_keeper", session = %name, verdict = "goal_continue_exhausted",
                    measured = true, n_considered = 1,
                    "two continues produced no tool call; the keeper stops for this goal until it changes or the owner writes");
            }
            Decision::Send(text) => {
                let out = crate::api::session_verbs::deliver_automated(&app, &name, &text, &format!("goal:{name}")).await;
                if let Ok(mut map) = state().lock() {
                    if let Some(k) = map.get_mut(&name) {
                        k.last_sent = now;
                    }
                }
                tracing::info!(target: "amux::goal_keeper", session = %name, verdict = "goal_continue_sent",
                    submitted = ?out.submitted, refused = out.refused, measured = true, n_considered = 1,
                    "continued a worker whose /goal is not met: {}", out.message);
            }
        }
    }
    tracing::debug!(target: "amux::goal_keeper", measured = true, n_considered = considered, "goal keeper tick");
}

pub fn spawn(app: crate::api::AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, 60, move || {
        let app = app.clone();
        async move { tick(app).await }
    })
}

#[cfg(test)]
mod tests {
    /// A worker whose transcript cannot be found says so: an absent field was
    /// read as "no goal" (amux-helper, mxp-gs12, 2026-10-08).
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
        let goal = Goal { met: false, condition: "finish cicd".into(), ts: 0.0 };
        let mut k = Keep::default();
        let base = t("2026-09-26T20:00:00Z");
        // First continue.
        assert!(matches!(decide(&mut k, &goal, &[], true, base), Decision::Send(_)));
        k.last_sent = base;
        // Too soon.
        assert_eq!(decide(&mut k, &goal, &[], true, base + 60.0), Decision::Skip("interval"));
        // Two intervals with no tool call: second send, then exhausted.
        assert!(matches!(decide(&mut k, &goal, &[], true, base + 700.0), Decision::Send(_)));
        k.last_sent = base + 700.0;
        assert_eq!(decide(&mut k, &goal, &[], true, base + 1400.0), Decision::Exhausted);
        // Progress would have kept it going.
        let mut k2 = Keep { condition: "finish cicd".into(), last_sent: base, ..Default::default() };
        let recs = vec![tool("2026-09-26T20:05:00Z")];
        assert!(matches!(decide(&mut k2, &goal, &recs, true, base + 700.0), Decision::Send(_)));
        assert_eq!(k2.no_progress, 0);
        // The owner writing resets the count...
        let owner = vec![user("2026-09-26T20:30:00Z", "keep going")];
        assert!(matches!(decide(&mut k, &goal, &owner, true, base + 2000.0), Decision::Send(_)));
        // ...but never the interval: a task notification or an owner message
        // one minute after a send does not allow another send.
        let mut k4 = Keep { condition: "finish cicd".into(), last_sent: base, ..Default::default() };
        let noise = vec![user("2026-09-26T20:01:00Z", "<task-notification>done"), user("2026-09-26T20:02:00Z", "hi")];
        assert_eq!(decide(&mut k4, &goal, &noise, true, base + 300.0), Decision::Skip("interval"));
        // A new goal resets too.
        let mut k3 = Keep { condition: "old".into(), last_sent: base, no_progress: 5, ..Default::default() };
        assert!(matches!(decide(&mut k3, &goal, &[], true, base + 10.0), Decision::Send(_)));
        // Met or cleared: never.
        assert_eq!(decide(&mut Keep::default(), &Goal { met: true, ..goal.clone() }, &[], true, base), Decision::Skip("goal met"));
        assert!(matches!(decide(&mut Keep::default(), &goal, &[], false, base), Decision::Skip(_)));
    }
}
