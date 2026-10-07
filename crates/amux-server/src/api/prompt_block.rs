//! A lane blocked on a Claude Code permission prompt.
//!
//! gs12-mvs, 2026-10-07: a subagent's Bash call raised "This shell -c script
//! runs rm and could not be checked / Do you want to proceed?" under bypass
//! permissions. Claude Code fired no Notification or PermissionRequest hook
//! for it, so the lane's last hook edge said `active` and outranked the pane.
//! It read active for about an hour while three steering messages queued
//! behind it, and the only log was "steering queue STALLED
//! blocked_now=busy-past-deadline".
//!
//! amux does not answer the prompt (ethos D2: only the rate-limit and resume
//! prompts are answered for the model). It makes the block visible: the lane
//! reads `waiting` with `waiting_since`, the steering stall line names it, a
//! WARN fires once per lane per 10 minutes, and after
//! AMUX_PROMPT_BLOCK_ALERT_S (default 600) the lane's hub gets one message per
//! prompt.
use std::collections::HashMap;
use std::sync::Mutex;

/// The prompt's own reason line (the line above "Do you want to proceed?"),
/// when a Claude Code permission prompt is drawn at the bottom of the pane:
/// the question, then its "❯ 1." option, and no status bar after it (the
/// prompt replaces the bar, so a bar below means the question is scrollback).
pub fn prompt_line(raw: &str) -> Option<String> {
    let clean = crate::backend::adapter::strip_ansi(raw);
    let lines: Vec<&str> = clean.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let tail = &lines[lines.len().saturating_sub(10)..];
    let i = tail.iter().rposition(|l| l.to_lowercase().starts_with("do you want to proceed"))?;
    let after = &tail[i + 1..];
    if !after.iter().any(|l| l.starts_with('\u{276f}') && l.contains("1.")) {
        return None;
    }
    if after.iter().any(|l| l.contains("\u{23f5}\u{23f5}") || l.to_lowercase().contains("bypass permissions on")) {
        return None;
    }
    let reason = if i > 0 && !tail[i - 1].starts_with('\u{2502}') { tail[i - 1] } else { tail[i] };
    Some(reason.chars().take(160).collect())
}

#[derive(Clone, Debug)]
pub struct Seen {
    pub line: String,
    pub since: f64,
    pub last_seen: f64,
    warned_at: f64,
    alerted: bool,
}

fn seen() -> &'static Mutex<HashMap<String, Seen>> {
    static S: std::sync::OnceLock<Mutex<HashMap<String, Seen>>> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

/// Record what the pane shows now. Returns (prompt line, first seen) while the
/// lane is blocked, and clears the record when the prompt is gone.
pub fn observe(lane: &str, raw: Option<&str>, now: f64) -> Option<(String, f64)> {
    let mut map = seen().lock().unwrap_or_else(|e| e.into_inner());
    let Some(line) = raw.and_then(prompt_line) else {
        map.remove(lane);
        return None;
    };
    let entry = map.entry(lane.to_string()).or_insert_with(|| Seen {
        line: line.clone(),
        since: now,
        last_seen: now,
        warned_at: 0.0,
        alerted: false,
    });
    if entry.line != line {
        *entry = Seen { line: line.clone(), since: now, last_seen: now, warned_at: 0.0, alerted: false };
    }
    entry.last_seen = now;
    if now - entry.warned_at >= 600.0 {
        entry.warned_at = now;
        tracing::warn!(session = lane, age_s = (now - entry.since) as i64, prompt = %line, measured = true,
            n_considered = 1, verdict = "lane_blocked_on_permission_prompt",
            "lane is blocked on a Claude Code permission prompt; amux does not answer it");
    }
    Some((line, entry.since))
}

/// Whether the lane was seen blocked within the last two minutes, for the
/// steering stall line's `blocked_now`.
pub fn blocked_now(lane: &str, now: f64) -> bool {
    let map = seen().lock().unwrap_or_else(|e| e.into_inner());
    map.get(lane).is_some_and(|s| now - s.last_seen <= 120.0)
}

fn alert_after_s() -> f64 {
    std::env::var("AMUX_PROMPT_BLOCK_ALERT_S").ok().and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0).unwrap_or(600.0)
}

/// Blocks old enough to report and not yet reported, marked reported.
pub fn take_due(now: f64) -> Vec<(String, Seen)> {
    let threshold = alert_after_s();
    let mut map = seen().lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    for (lane, s) in map.iter_mut() {
        if !s.alerted && now - s.last_seen <= 120.0 && now - s.since >= threshold {
            s.alerted = true;
            out.push((lane.clone(), s.clone()));
        }
    }
    out
}

/// One message to the lane's hub per prompt that has blocked the lane past
/// the threshold. A lane with no hub is logged, not routed.
pub async fn alert_due(state: &crate::api::AppState) -> usize {
    let now = crate::config::now_f64();
    let home = crate::config::amux_home();
    let mut sent = 0;
    for (lane, s) in take_due(now) {
        let mins = ((now - s.since) / 60.0).round() as i64;
        let hub = crate::api::contract::lane_setting(&home, &lane, "AMUX_CONTRACT_HUB")
            .map(|v| v.trim().trim_matches('"').to_string())
            .filter(|v| !v.is_empty() && *v != lane);
        let Some(hub) = hub else {
            tracing::warn!(session = %lane, age_min = mins, prompt = %s.line, measured = true, n_considered = 1,
                verdict = "prompt_block_alert_unrouted", "a lane is blocked on a permission prompt and has no hub to tell");
            continue;
        };
        let text = format!(
            "[amux] {lane} has been blocked on a Claude Code permission prompt for {mins} min: \"{}\". \
             amux does not answer permission prompts; someone with access to the lane must.",
            s.line
        );
        match crate::api::session_verbs::steer_enqueue(state, &hub, &text, "prompt-block", "harness:prompt-block").await {
            Ok(_) => {
                sent += 1;
                tracing::warn!(session = %lane, hub = %hub, age_min = mins, measured = true, n_considered = 1,
                    verdict = "prompt_block_alert_sent", "told the hub a lane is blocked on a permission prompt");
            }
            Err(e) => tracing::warn!(session = %lane, hub = %hub, error = ?e, measured = true, n_considered = 1,
                verdict = "prompt_block_alert_failed", "could not tell the hub a lane is blocked on a permission prompt"),
        }
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUBAGENT_PROMPT: &str = "\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}
 Bash command \u{b7} from the general-purpose agent
 Run shell command
\u{254c}\u{254c}\u{254c}\u{254c}\u{254c}\u{254c}
 \u{2502} ZB_SHA=$(git rev-parse HEAD) bash -c 'rm -f \"${ZS:?}\"'
\u{254c}\u{254c}\u{254c}\u{254c}\u{254c}\u{254c}
 This shell -c script runs rm and could not be checked

 Do you want to proceed?
 \u{276f} 1. Yes
   2. No

 Esc to cancel \u{b7} Tab to amend \u{b7} ctrl+x ctrl+k twice to stop background agents";

    const PLAIN_PROMPT: &str = "\
\u{23fa} Bash(rm -rf build)

Do you want to proceed?
\u{276f} 1. Yes
  2. No, and tell Claude what to do differently";

    const SCROLLBACK_ONLY: &str = "\
Do you want to proceed?
\u{276f} 1. Yes
  2. No
\u{23fa} Done, removed the build dir.
\u{2733} Baked for 12s

\u{276f}
\u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle)";

    #[test]
    fn a_drawn_permission_prompt_names_its_reason_line() {
        assert_eq!(prompt_line(SUBAGENT_PROMPT).as_deref(), Some("This shell -c script runs rm and could not be checked"));
        assert_eq!(prompt_line(PLAIN_PROMPT).as_deref(), Some("\u{23fa} Bash(rm -rf build)"));
        assert_eq!(prompt_line(SCROLLBACK_ONLY), None, "a prompt in scrollback above the status bar is answered history");
        assert_eq!(prompt_line("\u{276f}\n? for shortcuts"), None);
    }

    #[test]
    fn a_block_is_dated_from_first_sight_warned_once_and_alerted_once() {
        let lane = "pb-test-lane";
        let (_, since) = observe(lane, Some(SUBAGENT_PROMPT), 1000.0).unwrap();
        assert_eq!(since, 1000.0);
        assert_eq!(observe(lane, Some(SUBAGENT_PROMPT), 1300.0).unwrap().1, 1000.0, "the first sighting dates the block");
        assert!(blocked_now(lane, 1350.0));
        assert!(take_due(1500.0).iter().all(|(l, _)| l != lane), "not due before the threshold");
        observe(lane, Some(SUBAGENT_PROMPT), 1700.0);
        assert!(take_due(1700.0).iter().any(|(l, _)| l == lane), "due past the threshold");
        observe(lane, Some(SUBAGENT_PROMPT), 1800.0);
        assert!(take_due(1800.0).iter().all(|(l, _)| l != lane), "one alert per prompt");
        assert!(observe(lane, Some("\u{276f}\n? for shortcuts"), 1900.0).is_none());
        assert!(!blocked_now(lane, 1900.0), "an answered prompt clears the block");
    }
}
