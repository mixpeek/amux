//! What a model-prompt (tmux) schedule fire costs, and when it is skipped.
//!
//! # Why this exists
//!
//! SCHED-656, "mxp-gs12: keep working the GS-12 /goal", every 30 minutes from
//! 2026-10-09: 354 deliveries (about $2,200 at the measured $6.20 a fire), 26
//! of them after the goal was met. Most arrived "queued while generating", so
//! the worker was already busy and the prompt only extended its turn. In 84
//! cases it and another continuation (the goal keeper, Claude Code's own goal
//! check-in) woke the worker within ten minutes of each other. 144 fires led to
//! two or fewer tool calls. Each fire re-sent a 2,404-character brief of
//! standing rules, some of them contradicted by later rulings.
//!
//! # Two rules, both computed (ethos rule 2)
//!
//! 1. A GOAL-CONTINUATION schedule (its title or command names `/goal` and
//!    says keep going / keep working / continue) is one more goal keeper, so
//!    it obeys the keeper's rules: no fire while the goal is met (the checker
//!    decides, see `goal_check`), none while the worker is mid-turn, and none
//!    within `AMUX_GOAL_CONTINUE_INTERVAL_S` (600 s) of another continuation.
//!    A skipped fire is a refusal with [`GATE_PREFIX`], which the failure
//!    router treats as neutral. Verdict `schedule_goal_wake_skipped`. A
//!    manual Run now is never skipped: the human asked.
//! 2. A LONG BRIEF (`AMUX_SCHEDULE_BRIEF_MIN_CHARS`, 800) is a versioned file.
//!    The full text is saved at `~/.amux/schedules/briefs/<id>/<version>.md`
//!    and sent in full on the first fire and whenever it changes; while it is
//!    unchanged a fire sends its first sentence plus the path. Verdicts
//!    `schedule_brief_full` and `schedule_brief_pointer`.

use std::path::{Path, PathBuf};

use super::goal_keeper;
use super::scheduler::DurableSchedule;

pub const GATE_PREFIX: &str = "goal wake gate:";

/// Does this schedule exist to keep a worker going on its `/goal`?
pub fn is_goal_continuation(title: &str, command: &str) -> bool {
    let t = format!("{title}\n{command}").to_ascii_lowercase();
    t.contains("/goal") && ["keep going", "keep working", "continue", "keep pushing"].iter().any(|w| t.contains(w))
}

/// The decision, pure: given the worker's resolved goal, its boundary status
/// and the last continuation, should a goal-continuation fire be skipped?
pub fn gate_reason(
    goal_state: &str,
    goal_why: &str,
    boundary: Option<&str>,
    last_wake: Option<(f64, String)>,
    now: f64,
    interval_s: f64,
) -> Option<(&'static str, String)> {
    if goal_state == "met" {
        return Some(("goal_met", format!("{GATE_PREFIX} the goal is met ({goal_why}); a keep-going fire would only spend a turn")));
    }
    if goal_state == "cleared" {
        return Some(("goal_cleared", format!("{GATE_PREFIX} the /goal was cleared ({goal_why}); nothing to continue")));
    }
    if let Some(b) = boundary {
        if b != "idle" {
            return Some((
                "worker_busy",
                format!("{GATE_PREFIX} the worker is {b}, mid-turn; a continuation only helps an idle worker, so this fire is skipped rather than queued"),
            ));
        }
    }
    if let Some((at, by)) = last_wake {
        if now - at < interval_s {
            return Some((
                "recent_wake",
                format!("{GATE_PREFIX} {by} already continued this worker {:.0}s ago (AMUX_GOAL_CONTINUE_INTERVAL_S {interval_s:.0})", now - at),
            ));
        }
    }
    None
}

/// Live gate for one fire. `None` = deliver.
pub async fn goal_gate(state: &crate::api::AppState, sched: &DurableSchedule, source: &str) -> Option<String> {
    if source.starts_with("manual:") || !is_goal_continuation(sched.str_field("title"), sched.str_field("command")) {
        return None;
    }
    let name = sched.str_field("session").to_string();
    if name.is_empty() {
        return None;
    }
    let n2 = name.clone();
    let resolved = tokio::task::spawn_blocking(move || goal_keeper::resolved_for(&n2)).await.ok()?;
    let boundary = crate::api::session_verbs::boundary_signals(state, Some(&name))
        .await
        .and_then(|s| s.turn_boundary_status(&name));
    let now = crate::config::now_f64();
    let interval = goal_keeper::interval_s();
    let (reason, text) = gate_reason(
        resolved.state,
        &resolved.why,
        boundary.as_deref(),
        goal_keeper::last_wake(&name),
        now,
        interval,
    )?;
    tracing::info!(target: "amux::schedule_wake", schedule = %sched.id(), session = %name,
        verdict = "schedule_goal_wake_skipped", reason, goal_state = resolved.state,
        measured = true, n_considered = 1, "{text}");
    Some(text)
}

/// A long brief, versioned on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Brief {
    pub text: String,
    /// Version to record as delivered once the fire lands; `None` when the
    /// brief rule does not apply.
    pub version: Option<String>,
    pub marker: Option<PathBuf>,
}

fn fnv(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")[..12].to_string()
}

fn first_sentence(s: &str) -> String {
    let s = s.trim();
    let end = s
        .char_indices()
        .find(|(i, c)| (*c == '.' || *c == '\n') && s[i + 1..].starts_with([' ', '\n']))
        .map(|(i, _)| i + 1)
        .unwrap_or(s.len());
    let head = &s[..end];
    if head.chars().count() > 300 {
        format!("{}...", head.chars().take(300).collect::<String>())
    } else {
        head.trim().to_string()
    }
}

pub fn brief_min_chars() -> usize {
    std::env::var("AMUX_SCHEDULE_BRIEF_MIN_CHARS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(800)
}

/// The text to send for `command`. `dir` is the briefs root
/// (`~/.amux/schedules/briefs`).
pub fn brief(dir: &Path, sched_id: &str, session: &str, command: &str, source: &str, min_chars: usize) -> Brief {
    let full = || Brief { text: command.to_string(), version: None, marker: None };
    if min_chars == 0 || command.chars().count() < min_chars || source.starts_with("manual:") || sched_id.is_empty() {
        return full();
    }
    let v = fnv(command);
    let d = dir.join(sched_id);
    let file = d.join(format!("{v}.md"));
    if std::fs::create_dir_all(&d).is_err() {
        return full();
    }
    if !file.exists() && std::fs::write(&file, command).is_err() {
        return full();
    }
    let _ = std::fs::write(d.join("current.md"), command);
    let marker = d.join(format!("delivered-{session}"));
    let prev = std::fs::read_to_string(&marker).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let text = if prev.as_deref() == Some(v.as_str()) {
        tracing::info!(target: "amux::schedule_wake", schedule = %sched_id, session = %session,
            verdict = "schedule_brief_pointer", version = %v, saved_chars = command.chars().count(),
            measured = true, n_considered = 1, "sent a pointer to an unchanged brief instead of the full text");
        format!(
            "{}\n\n[amux] The rest of this schedule's standing brief is unchanged since you last got it \
             (version {v}, {} chars). Full text: {}. Read it again only if you need it, for example after a compaction.",
            first_sentence(command),
            command.chars().count(),
            file.display()
        )
    } else {
        tracing::info!(target: "amux::schedule_wake", schedule = %sched_id, session = %session,
            verdict = "schedule_brief_full", version = %v, previous = prev.as_deref().unwrap_or(""),
            measured = true, n_considered = 1, "sent a new or changed brief in full");
        let what = match &prev {
            Some(p) => format!("changed from version {p} to {v}"),
            None => format!("version {v}"),
        };
        format!(
            "{command}\n\n[amux] This brief ({what}) is saved at {}. While it stays unchanged, later fires send only \
             its first sentence and this path.",
            file.display()
        )
    };
    Brief { text, version: Some(v), marker: Some(marker) }
}

/// Record that the brief reached the worker.
pub fn brief_delivered(b: &Brief) {
    if let (Some(v), Some(m)) = (&b.version, &b.marker) {
        let _ = std::fs::write(m, v);
    }
}

pub fn briefs_dir() -> PathBuf {
    crate::api::session_verbs::home().join("schedules").join("briefs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sched_656_shape_is_a_goal_continuation_and_a_sweep_is_not() {
        assert!(is_goal_continuation(
            "mxp-gs12: keep working the GS-12 /goal (Ethan 2026-10-09)",
            "[schedule] Keep going on your /goal: every section 6 proof row passing"
        ));
        assert!(!is_goal_continuation("GS-12 BOTTLENECK SWEEP", "find the slowest row and fix it"));
        assert!(!is_goal_continuation("daily digest", "summarise /goal progress for Ethan"));
    }

    #[test]
    fn a_continuation_is_skipped_when_met_busy_or_just_woken() {
        let g = |state, b: Option<&str>, w: Option<(f64, String)>| gate_reason(state, "why", b, w, 1000.0, 600.0).map(|r| r.0);
        assert_eq!(g("met", Some("idle"), None), Some("goal_met"));
        assert_eq!(g("cleared", Some("idle"), None), Some("goal_cleared"));
        assert_eq!(g("unmet", Some("active"), None), Some("worker_busy"));
        assert_eq!(g("unmet", Some("idle"), Some((700.0, "goal keeper".into()))), Some("recent_wake"));
        assert_eq!(g("unmet", Some("idle"), Some((300.0, "goal keeper".into()))), None);
        // UNKNOWN is never "met": an unreadable goal keeps the worker going.
        assert_eq!(g("unknown", Some("idle"), None), None);
        // Not running: the deliver path decides (it can resume the worker).
        assert_eq!(g("unmet", None, None), None);
        let r = gate_reason("met", "checker says met", Some("idle"), None, 0.0, 600.0).unwrap().1;
        assert!(r.starts_with(GATE_PREFIX), "{r}");
    }

    #[test]
    fn an_unchanged_long_brief_becomes_a_pointer_and_a_change_is_sent_in_full() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = format!("[schedule] Keep going on your /goal: every proof row passes. {}", "Rule. ".repeat(200));
        let b1 = brief(dir.path(), "SCHED-1", "w", &cmd, "cron-rs", 800);
        assert!(b1.text.starts_with(&cmd), "first fire is full");
        // Not delivered yet: the next fire is full again.
        let b2 = brief(dir.path(), "SCHED-1", "w", &cmd, "cron-rs", 800);
        assert!(b2.text.starts_with(&cmd));
        brief_delivered(&b2);
        let b3 = brief(dir.path(), "SCHED-1", "w", &cmd, "cron-rs", 800);
        assert!(b3.text.len() < 600, "{}", b3.text);
        assert!(b3.text.starts_with("[schedule] Keep going on your /goal: every proof row passes."), "{}", b3.text);
        let saved = b3.text.split("Full text: ").nth(1).unwrap().split(". Read").next().unwrap();
        assert_eq!(std::fs::read_to_string(saved).unwrap(), cmd, "the pointer names the full text");
        // Another session has not seen it.
        assert!(brief(dir.path(), "SCHED-1", "other", &cmd, "cron-rs", 800).text.starts_with(&cmd));
        // A changed brief goes in full and names the change.
        let cmd2 = format!("{cmd} New rule.");
        let b4 = brief(dir.path(), "SCHED-1", "w", &cmd2, "cron-rs", 800);
        assert!(b4.text.starts_with(&cmd2) && b4.text.contains("changed from version"), "{}", b4.text);
        // Run now and short commands are untouched.
        assert_eq!(brief(dir.path(), "SCHED-1", "w", &cmd, "manual:ethan", 800).text, cmd);
        assert_eq!(brief(dir.path(), "SCHED-1", "w", "short", "cron-rs", 800).text, "short");
    }
}
