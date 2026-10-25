//! Promised next step, never taken (AMUX-5236).
//!
//! mvs-research ended a turn with "Next I'll build the streaming loader" and
//! never did; MR-288 sat in `doing`. mac-ops said a Monitor would notify it,
//! none was armed, and its next turn died on an API error; it sat idle until
//! the owner typed "continue" (docs/fleet-stall-review-2026-09-26.md, fix 3).
//! Nothing in amux read what a lane PROMISED, so a lane that meant to keep
//! going and a lane that had finished looked identical once idle.
//!
//! A turn that ends on a forward promise is recorded at the turn-end edge
//! (`turn_end::on_turn_end`, which also owns the owner-ask classifier; an ask
//! wins over a promise). A level-triggered sweep then re-prompts the lane ONCE
//! with its card id and its own sentence, only when all of these hold:
//! the lane is idle, the promise is still its final message (nothing newer in
//! the transcript), two minutes have passed, no background task (Bash
//! run_in_background, Monitor) or subagent is live, and it holds a `doing`
//! card. Dedupe is the transcript message uuid, stored as
//! `promise_nudged_uuid` and as the steering row's stable id.
//!
//! Kill switch `AMUX_PROMISE_NUDGE`, default ON, scoped worker > group >
//! global with the process env winning. `AMUX_PROMISE_IDLE_S` overrides the
//! two-minute threshold. Verdicts: `promise_recorded`, `promise_nudged`,
//! `promise_dropped`, `promise_waiting`, `promise_nudge_refused`,
//! `promise_nudge_disabled`.

use super::session_verbs as sv;
use super::turn_end::{
    clip, enabled, final_turn, original_sentence, rec_ts, rx, said_text, sentences,
    tail_paragraphs, text_blocks, transcript_for, TurnTail,
};
use super::AppState;
use serde_json::{json, Value};

pub(crate) const PROMISE_KEY: &str = "AMUX_PROMISE_NUDGE";
/// Steering guard label. Non-empty so `steer_enqueue` treats this as
/// automation (isolation and pause refusals apply), never as the owner's send.
pub(crate) const PROMISE_GUARD: &str = "promise-nudge";

/// How long a lane must sit idle after a promise before it is re-prompted.
/// Two minutes is the review's own threshold: long enough that a lane which is
/// about to pick itself back up (a queued message, a notification) has done
/// so, short enough that the stall is not hours.
fn promise_idle_s() -> f64 {
    std::env::var("AMUX_PROMISE_IDLE_S")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(120.0)
}
/// A promise older than this is dropped rather than nudged: the lane has
/// either moved on in a way the transcript check cannot see, or the owner has.
const PROMISE_EXPIRE_S: f64 = 6.0 * 3600.0;
/// Sweep cadence for promise nudges (piggybacks on the steering loop).
pub(crate) const PROMISE_SWEEP_SECS: f64 = 30.0;

// ---------------------------------------------------------------------------
// Forward-promise detector
// ---------------------------------------------------------------------------

/// A sentence at the end of the turn promising a next step the worker will
/// take by itself. Returns the sentence in the worker's own words.
pub(crate) fn detect_promise(text: &str) -> Option<String> {
    let said = said_text(text);
    let tail = tail_paragraphs(&said, 2);
    let hit = sentences(&tail).into_iter().rev().find(|s| {
        [
            rx!(r"\bnext,? i('ll| will)\b"),
            rx!(r"\bi('ll| will) (now )?(start on|start|begin|build|pick (it |this |that )?up|pick back up|check back|circle back|come back|follow up|report back|resume|continue)\b"),
            rx!(r"\bi('ll| will) let the (monitor|watcher|watch|background (task|job|agent)|agent)s? (notify|tell|wake|ping|alert) me\b"),
            rx!(r"\b(the )?(monitor|watcher|background (task|job))s? will (notify|tell|wake|ping|alert) me\b"),
            rx!(r"\bwill (report|check) back\b"),
            rx!(r"\b(when|once|after) (it|that|this|the [\w-]+( [\w-]+)?) (lands|finishes|completes|is done|is green|passes|returns)\b[^.]*\bi('ll| will)\b"),
        ]
        .iter()
        .any(|p| p.is_match(s))
    })?;
    Some(original_sentence(text, &hit))
}


/// Background tasks (run_in_background Bash, Monitor) started in these records
/// with no terminal notification yet.
///
/// Starts come from the provider's structured `toolUseResult`
/// (`backgroundTaskId` for Bash, `taskId` for Monitor); ends from the
/// `<task-notification>` envelope with a terminal status, or a Monitor's own
/// expiry notice. A Monitor also ends when its `timeoutMs` has elapsed. An
/// unrecognised shape stays LIVE: absence of a terminal edge is not evidence
/// the task finished, and the cost of that error is a missed nudge.
pub(crate) fn live_background_tasks(records: &[Value], now: f64) -> Vec<String> {
    use std::collections::BTreeMap;
    let mut live: BTreeMap<String, Option<f64>> = BTreeMap::new();
    let block = rx!(r"(?s)<task-notification>.*?</task-notification>");
    let id_of = rx!(r"<task-id>\s*([^<\s]{1,128})\s*</task-id>");
    let terminal = rx!(r"<status>\s*(completed|failed|cancelled|canceled|stopped|killed|timed_out)\s*</status>|\[Monitor (expired|stopped|ended)");
    for r in records {
        let tur = &r["toolUseResult"];
        if let Some(id) = tur["backgroundTaskId"].as_str() {
            live.insert(id.to_string(), None);
        } else if let Some(id) = tur["taskId"].as_str() {
            let deadline = match (tur["persistent"].as_bool(), tur["timeoutMs"].as_f64(), rec_ts(r)) {
                (Some(false), Some(ms), Some(start)) => Some(start + ms / 1000.0),
                _ => None,
            };
            live.insert(id.to_string(), deadline);
        }
        // Provider framing only (the reconcile_terminal_subagents rule): a
        // queue record, or a user record that IS a notification. A prompt
        // that merely quotes one while debugging must not end a live task.
        let body = if r["type"] == "queue-operation" && r["operation"] == "enqueue" {
            r["content"].as_str().map(str::to_string)
        } else if r["type"] == "user" {
            Some(text_blocks(&r["message"]["content"]).join("\n"))
                .filter(|t| t.trim_start().starts_with("<task-notification>"))
        } else {
            None
        };
        for b in body.iter().flat_map(|b| block.find_iter(b)) {
            let b = b.as_str();
            if let (Some(id), true) = (id_of.captures(b).and_then(|c| c.get(1)), terminal.is_match(b)) {
                live.remove(id.as_str());
            }
        }
    }
    live.into_iter()
        .filter(|(_, deadline)| deadline.is_none_or(|d| d > now))
        .map(|(id, _)| id)
        .collect()
}

pub(crate) fn record_promise(name: &str, turn: &TurnTail) {
    match detect_promise(&turn.text) {
        Some(p) if !turn.uuid.is_empty() => {
            tracing::info!(session = %name, verdict = "promise_recorded", promise = %clip(&p, 160),
                "turn-end: turn ended on a promised next step; watching for an idle stall (AMUX-5236)");
            sv::update_meta(
                name,
                &[
                    ("promise_uuid", json!(turn.uuid)),
                    ("promise_text", json!(clip(&p, 300))),
                    ("promise_at", json!(turn.ts.max(1.0) as i64)),
                ],
            );
        }
        _ => forget_promise(name),
    }
}

/// A newer turn that ended on something else supersedes any earlier promise.
pub(crate) fn forget_promise(name: &str) {
    if !sv::meta_str(&sv::load_meta(name), "promise_uuid").is_empty() {
        sv::update_meta(name, &[("promise_uuid", json!(""))]);
    }
}

// ---------------------------------------------------------------------------
// Sweep
// ---------------------------------------------------------------------------

/// Pure gate for a promise nudge, so every refusal has a name and a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromiseGate {
    Nudge,
    Skip(&'static str),
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn promise_gate(
    promise_uuid: &str,
    nudged_uuid: &str,
    turn: Option<&TurnTail>,
    last_record_ts: f64,
    report_state: &str,
    report_ts: f64,
    live_bg: usize,
    subagents_live: bool,
    doing_card: Option<&str>,
    now: f64,
    idle_s: f64,
) -> PromiseGate {
    if promise_uuid.is_empty() {
        return PromiseGate::Skip("no_promise");
    }
    if nudged_uuid == promise_uuid {
        return PromiseGate::Skip("already_nudged");
    }
    let Some(turn) = turn else {
        return PromiseGate::Skip("superseded");
    };
    if turn.uuid != promise_uuid {
        return PromiseGate::Skip("superseded");
    }
    if now - turn.ts > PROMISE_EXPIRE_S {
        return PromiseGate::Skip("expired");
    }
    if report_state != "idle" {
        return PromiseGate::Skip("not_idle");
    }
    if now - report_ts < idle_s || now - last_record_ts < idle_s || now - turn.ts < idle_s {
        return PromiseGate::Skip("idle_too_short");
    }
    if live_bg > 0 {
        return PromiseGate::Skip("background_task_live");
    }
    if subagents_live {
        return PromiseGate::Skip("subagent_live");
    }
    if doing_card.is_none() {
        return PromiseGate::Skip("no_doing_card");
    }
    PromiseGate::Nudge
}

fn promise_text(card: &str, promise: &str, idle_min: i64) -> String {
    format!(
        "[amux promise] {card}: your last turn ended with \"{}\" and the lane has been idle \
         {idle_min} min with no background task or agent running, so nothing will wake you \
         for it. Take that step now. If it is waiting on something, arm a real watch \
         (Monitor or run_in_background) or note the blocker on {card}.",
        clip(promise, 240)
    )
}

fn doing_card(state: &AppState, lane: &str) -> Option<String> {
    use rusqlite::OptionalExtension;
    let conn = state.store.read().ok()?;
    conn.query_row(
        "SELECT id FROM legacy_execution_issues WHERE session = ?1 AND deleted IS NULL \
         AND COALESCE(archived,0) = 0 AND status = 'doing' ORDER BY updated DESC LIMIT 1",
        [lane],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// Level-triggered: every idle lane with a recorded, un-nudged promise.
pub(crate) async fn promise_sweep(state: &AppState) -> usize {
    let reports: Value = state
        .store
        .read()
        .ok()
        .and_then(|c| {
            c.query_row("SELECT value FROM prefs WHERE key='session_reports'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
        })
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let Some(map) = reports.as_object() else { return 0 };
    let now = crate::config::now_f64();
    let idle_s = promise_idle_s();
    let mut sub_activity = None;
    let mut nudged = 0;
    for (name, report) in map {
        if report["state"].as_str() != Some("idle") {
            continue;
        }
        let meta = sv::load_meta(name);
        let promise_uuid = sv::meta_str(&meta, "promise_uuid");
        if promise_uuid.is_empty() || sv::meta_str(&meta, "promise_nudged_uuid") == promise_uuid {
            continue;
        }
        if sv::session_is_isolated(name) || crate::api::board_lifecycle::board_unused(name) {
            continue;
        }
        if !enabled(name, PROMISE_KEY) {
            if crate::log_dedupe::first_this_bucket(&format!("promise-off:{name}:{promise_uuid}"), 0) {
                tracing::info!(session = %name, verdict = "promise_nudge_disabled",
                    "promise sweep: {PROMISE_KEY} is off for this lane");
            }
            continue;
        }
        let Some(path) = transcript_for(name, report["session_id"].as_str().unwrap_or("")) else {
            continue;
        };
        let records = sv::iter_jsonl_tail(&path, 4_000_000);
        let turn = final_turn(&records);
        let last_ts = records.iter().rev().find_map(rec_ts).unwrap_or(0.0);
        let bg = live_background_tasks(&records, now);
        let subs = sub_activity.get_or_insert_with(crate::api::sessions_legacy::scan_subagent_activity);
        let subagents_live = report["subagents"]["count"].as_i64().unwrap_or(0) > 0
            || subs.get(name).is_some_and(|m| now - m < 240.0);
        let card = doing_card(state, name);
        let gate = promise_gate(
            &promise_uuid,
            &sv::meta_str(&meta, "promise_nudged_uuid"),
            turn.as_ref(),
            last_ts,
            "idle",
            report["ts"].as_f64().unwrap_or(now),
            bg.len(),
            subagents_live,
            card.as_deref(),
            now,
            idle_s,
        );
        match gate {
            PromiseGate::Skip(reason @ ("superseded" | "expired" | "no_doing_card")) => {
                // Terminal for THIS promise: stop re-reading the transcript.
                sv::update_meta(name, &[("promise_uuid", json!(""))]);
                tracing::info!(session = %name, verdict = "promise_dropped", reason,
                    "promise sweep: promise will not be nudged");
            }
            PromiseGate::Skip(reason) => {
                if crate::log_dedupe::first_this_bucket(&format!("promise-skip:{name}:{promise_uuid}:{reason}"), 0) {
                    tracing::debug!(session = %name, verdict = "promise_waiting", reason, live_bg = bg.len(),
                        "promise sweep: not nudging yet");
                }
            }
            PromiseGate::Nudge if !super::turn_end::nudges_wanted(name, "promise") => {
                sv::update_meta(name, &[("promise_uuid", json!(""))]);
            }
            PromiseGate::Nudge => {
                let card = card.unwrap_or_default();
                let text = promise_text(&card, &sv::meta_str(&meta, "promise_text"),
                    ((now - turn.as_ref().map(|t| t.ts).unwrap_or(now)) / 60.0) as i64);
                sv::update_meta(name, &[("promise_nudged_uuid", json!(promise_uuid))]);
                let id = format!("promise-nudge-{promise_uuid}");
                match sv::steer_enqueue_idempotent_report(state, name, &text, PROMISE_GUARD, "", &id).await {
                    Ok(r) => {
                        nudged += 1;
                        tracing::warn!(session = %name, verdict = "promise_nudged", card = %card, steer_id = %r.id,
                            "promise sweep: lane went idle after promising a next step; re-prompted once (AMUX-5236)");
                        sv::steer_deliver_at_boundary(state, name, "promise-nudge").await;
                    }
                    Err(e) => tracing::warn!(session = %name, verdict = "promise_nudge_refused", error = e,
                        "promise sweep: nudge could not be queued; not retried for this promise"),
                }
            }
        }
    }
    nudged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mvs_research_next_ill_is_a_promise() {
        let t = "MR-288: schema for the loader is settled.\n\nNext I'll build the streaming loader.";
        assert_eq!(detect_promise(t).as_deref(), Some("Next I'll build the streaming loader."));
    }

    #[test]
    fn mac_ops_monitor_promise_is_a_promise() {
        let t = "Cleanup is running in the background.\n\nI'll let the Monitor notify me when it finishes.";
        assert!(detect_promise(t).is_some());
        assert!(detect_promise("I'll pick up when the build lands.").is_some());
        assert!(detect_promise("Kicked off the run. I'll check back once it's green.").is_some());
    }

    #[test]
    fn a_summary_is_not_a_promise() {
        assert_eq!(detect_promise("Done. All three cards are closed with evidence."), None);
        assert_eq!(detect_promise("The reviewer said \"next I'll build it\" last week."), None);
    }

    #[test]
    fn background_tasks_are_live_until_a_terminal_notification() {
        let bash = json!({"type":"user","timestamp":"2026-09-26T10:00:00Z","toolUseResult":{"backgroundTaskId":"bny98r9qg"},
            "message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"moved to the background"}]}});
        let now = 1_790_500_000.0;
        assert_eq!(live_background_tasks(std::slice::from_ref(&bash), now), vec!["bny98r9qg".to_string()]);
        let done = json!({"type":"queue-operation","operation":"enqueue","content":
            "<task-notification>\n<task-id>bny98r9qg</task-id>\n<status>completed</status>\n</task-notification>"});
        assert!(live_background_tasks(&[bash, done], now).is_empty());
    }

    #[test]
    fn a_prompt_quoting_a_notification_does_not_end_a_live_task() {
        let bash = json!({"type":"user","timestamp":"2026-09-26T10:00:00Z","toolUseResult":{"backgroundTaskId":"b1"}});
        let quoted = json!({"type":"user","timestamp":"2026-09-26T10:01:00Z","message":{"content":
            "why did this fire? <task-notification><task-id>b1</task-id><status>completed</status></task-notification>"}});
        assert_eq!(live_background_tasks(&[bash, quoted], 1_790_500_000.0), vec!["b1".to_string()]);
    }

    #[test]
    fn a_monitor_ends_on_expiry_notice_or_timeout() {
        let start = json!({"type":"user","timestamp":"2026-09-26T10:00:00Z",
            "toolUseResult":{"taskId":"b0aoo183z","timeoutMs":900000,"persistent":false}});
        let t0 = rec_ts(&start).unwrap();
        assert_eq!(live_background_tasks(std::slice::from_ref(&start), t0 + 60.0).len(), 1);
        assert!(live_background_tasks(std::slice::from_ref(&start), t0 + 901.0).is_empty(), "timeoutMs elapsed");
        let expired = json!({"type":"queue-operation","operation":"enqueue","content":
            "<task-notification>\n<task-id>b0aoo183z</task-id>\n<summary>Monitor event</summary>\n<event>[Monitor expired after 15m with no events delivered.]</event>\n</task-notification>"});
        assert!(live_background_tasks(&[start, expired], t0 + 60.0).is_empty());
    }

    fn turn(uuid: &str, ts: f64) -> TurnTail {
        TurnTail { uuid: uuid.into(), text: "Next I'll build it.".into(), ts, prompt: String::new() }
    }

    #[test]
    fn promise_gate_nudges_only_an_idle_unwatched_lane_with_a_doing_card() {
        let now = 10_000.0;
        let t = turn("u1", now - 300.0);
        let g = |nudged: &str, st: &str, bg: usize, subs: bool, card: Option<&str>, last: f64| {
            promise_gate("u1", nudged, Some(&t), last, st, now - 300.0, bg, subs, card, now, 120.0)
        };
        assert_eq!(g("", "idle", 0, false, Some("MR-288"), now - 300.0), PromiseGate::Nudge);
        assert_eq!(g("u1", "idle", 0, false, Some("MR-288"), now - 300.0), PromiseGate::Skip("already_nudged"));
        assert_eq!(g("", "active", 0, false, Some("MR-288"), now - 300.0), PromiseGate::Skip("not_idle"));
        assert_eq!(g("", "idle", 1, false, Some("MR-288"), now - 300.0), PromiseGate::Skip("background_task_live"));
        assert_eq!(g("", "idle", 0, true, Some("MR-288"), now - 300.0), PromiseGate::Skip("subagent_live"));
        assert_eq!(g("", "idle", 0, false, None, now - 300.0), PromiseGate::Skip("no_doing_card"));
        assert_eq!(g("", "idle", 0, false, Some("MR-288"), now - 30.0), PromiseGate::Skip("idle_too_short"));
        let newer = turn("u2", now - 200.0);
        assert_eq!(
            promise_gate("u1", "", Some(&newer), now - 200.0, "idle", now - 200.0, 0, false, Some("X"), now, 120.0),
            PromiseGate::Skip("superseded")
        );
    }
}
