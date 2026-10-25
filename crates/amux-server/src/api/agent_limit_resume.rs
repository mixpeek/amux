//! Background agents a usage limit killed, and the resume that names them.
//!
//! # Why this exists (gs12 review, 2026-10-18, finding 1)
//!
//! mxp-gs12 ran an orchestrator with up to 21 Claude Code background agents on
//! one plan. Over 232 h, 180 of its agents failed and about 175 of those died
//! on `rate_limit` (HTTP 429) or `authentication_failed` ("Not logged in"),
//! in waves of 16 to 19 at once. amux already resumes the WORKER after a usage
//! reset or an account switch (AMUX-5235, `auto_resume_decision`), and the
//! goal keeper continues an unmet `/goal`. Neither said anything about the
//! agents: the worker woke up to a bare "continue" and had to re-derive which
//! lanes were dead from its own scrollback, and when it did not, the owner
//! typed the resume by hand ("Resume every GS-12 lane that stopped on the
//! limit at about 17:20Z").
//!
//! The dead set is COMPUTED, never judged (ethos rule 2): Claude Code writes a
//! provider-owned `queue-operation` record with a `<task-notification>` for
//! every agent that stops, and a failed one carries the API error in its
//! summary. An agent the worker already resumed shows a later `SendMessage`
//! addressed to its id, or a later notification of its own.
//!
//! # What reaches the worker
//!
//! [`resume_text`] appends the dead set, and the current plan-window reading
//! when amux has a fresh one, to the continuation amux was already going to
//! send. Whether to relaunch stays the worker's call: amux hands it the list
//! and the remaining window. How many run at once is capped by the
//! `subagents` boundary in `boundaries.toml` (the boundary guard refuses a new
//! Agent/Task call at the running limit or under the usage headroom floor),
//! which reaches isolated workers as a refusal only, under `apply_to_isolated`.
//!
//! `GET /api/sessions/{name}/quota` is the same reading on demand, for a
//! worker deciding how wide to fan out before it hits the wall.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use axum::extract::Path;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

/// One background agent that a usage limit or a logout stopped.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LimitKilledAgent {
    /// The task id, which is also the id `SendMessage` continues it by.
    pub id: String,
    pub description: String,
    /// `rate_limit` or `authentication_failed`.
    pub cause: &'static str,
    /// When the provider recorded the failure (unix seconds).
    pub at: i64,
    /// The reset the error named, 0 when it named none or it did not parse.
    pub reset_at: i64,
}

/// The cause of a failed agent's summary, when it is one a reset or an
/// account switch can fix. Anything else (a crash, an exit code, a tool
/// error) is the worker's to judge and is not listed.
pub(crate) fn kill_cause(summary: &str) -> Option<&'static str> {
    let s = summary.to_ascii_lowercase();
    if s.contains("error type rate_limit") || s.contains("http 429") {
        Some("rate_limit")
    } else if s.contains("authentication_failed") || s.contains("not logged in") {
        Some("authentication_failed")
    } else {
        None
    }
}

fn notification_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?s)^<task-notification>\s*.*?<task-id>\s*([^<\r\n]{1,128})\s*</task-id>.*?<status>\s*(completed|failed|cancelled|canceled|stopped)\s*</status>(?:.*?<summary>(.*?)</summary>)?",
        )
        .expect("static pattern")
    })
}

fn description_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r#"^Agent "([^"]{0,200})""#).expect("static pattern"))
}

fn ts_of(r: &Value) -> Option<i64> {
    r["timestamp"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.timestamp())
}

/// The agents in `records` whose LATEST terminal notification is a failure on
/// a usage limit or a logout, recorded after `since`, and that nothing has
/// addressed since (no later `SendMessage` to the id, no later notification).
///
/// Returns the list, oldest first, and how many terminal notifications were
/// read, so a caller can tell "no agent died" (n > 0, list empty) from "this
/// tail holds no agent records at all" (n == 0).
///
/// Only `queue-operation`/`enqueue` records are read, the provider-owned
/// framing `transcript_terminal_agents` also trusts: a prompt that quotes a
/// notification while debugging must not read as a dead agent.
pub(crate) fn limit_killed_agents(records: &[Value], since: i64) -> (Vec<LimitKilledAgent>, usize) {
    let mut latest: BTreeMap<String, Option<LimitKilledAgent>> = BTreeMap::new();
    let mut considered = 0usize;
    for r in records {
        let Some(at) = ts_of(r) else { continue };
        match (r["type"].as_str(), r["operation"].as_str()) {
            (Some("queue-operation"), Some("enqueue")) => {
                let Some(content) = r["content"].as_str() else { continue };
                let Some(caps) = notification_re().captures(content.trim()) else { continue };
                considered += 1;
                let id = caps.get(1).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
                let status = caps.get(2).map(|m| m.as_str()).unwrap_or_default();
                let summary = caps.get(3).map(|m| m.as_str().trim()).unwrap_or_default();
                let killed = (status == "failed")
                    .then(|| kill_cause(summary))
                    .flatten()
                    .map(|cause| LimitKilledAgent {
                        id: id.clone(),
                        description: description_re()
                            .captures(summary)
                            .and_then(|c| c.get(1))
                            .map(|m| m.as_str().to_string())
                            .unwrap_or_default(),
                        cause,
                        at,
                        reset_at: kill_reset_at(summary, at),
                    });
                latest.insert(id, killed);
            }
            (Some("assistant"), _) => {
                // The worker resumed it itself: a later SendMessage to the id.
                let Some(parts) = r["message"]["content"].as_array() else { continue };
                for p in parts {
                    if p["type"] == "tool_use" && p["name"] == "SendMessage" {
                        if let Some(to) = p["input"]["to"].as_str() {
                            if let Some(slot) = latest.get_mut(to.trim()) {
                                *slot = None;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut out: Vec<LimitKilledAgent> = latest
        .into_values()
        .flatten()
        .filter(|k| k.at > since)
        .collect();
    out.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
    (out, considered)
}

/// The reset the error text names ("resets 4:10pm (America/New_York)",
/// "resets Oct 2 at 3am"), read against the moment the error was written so a
/// time-only reset rolls over from THEN, not from whenever amux reads it.
fn kill_reset_at(summary: &str, at: i64) -> i64 {
    use chrono::TimeZone;
    let Some(when) = chrono::Local.timestamp_opt(at, 0).single() else {
        return 0;
    };
    crate::api::session_verbs::parse_rate_limit_reset_at(summary, when).unwrap_or(0)
}

/// Seconds after a named reset before a killed agent counts as resumable,
/// the same grace the worker-level resume gives Claude's own auto-continue.
pub(crate) const RESET_GRACE_S: i64 = 60;

/// Is this agent's stop over? A limit whose named reset has passed, or any
/// limit or logout followed by a switch of the machine's Claude account.
pub(crate) fn kill_is_over(k: &LimitKilledAgent, now: i64, account_changed_at: i64) -> bool {
    let reset_passed = k.cause == "rate_limit" && k.reset_at > 0 && now >= k.reset_at + RESET_GRACE_S;
    let switched = account_changed_at > 0 && account_changed_at >= k.at;
    reset_passed || switched
}

/// How many killed agents to name in one message. The rest are counted.
const LIST_MAX: usize = 30;

/// `base` (the continuation amux already sends) plus the dead agents and the
/// plan window. With no dead agents it returns `base` unchanged, so a worker
/// with no background lanes sees exactly the message it always did.
pub(crate) fn resume_text(base: &str, killed: &[LimitKilledAgent], window_pct: Option<i64>, session: &str) -> String {
    if killed.is_empty() {
        return base.to_string();
    }
    let mut s = format!(
        "{base}\n\n[amux] {} background agent(s) stopped on a usage limit or a logout and nothing has resumed them yet:\n",
        killed.len()
    );
    for k in killed.iter().take(LIST_MAX) {
        let when = chrono::DateTime::from_timestamp(k.at, 0)
            .map(|t| t.format("%m-%d %H:%MZ").to_string())
            .unwrap_or_default();
        let desc = if k.description.is_empty() { String::new() } else { format!(" \"{}\"", k.description) };
        s.push_str(&format!("- {}{desc} ({}, {when})\n", k.id, k.cause));
    }
    if killed.len() > LIST_MAX {
        s.push_str(&format!("- and {} more\n", killed.len() - LIST_MAX));
    }
    s.push_str(
        "Resume each one that still has work with SendMessage to its id (it keeps its context), \
         or relaunch it from its handoff.",
    );
    match window_pct {
        Some(p) => s.push_str(&format!(
            " The 5-hour plan window is {p}% used, so size the fan-out to what is left."
        )),
        None => s.push_str(" amux has no fresh plan-window reading right now."),
    }
    s.push_str(&format!(" Re-check: GET $AMUX_URL/api/sessions/{session}/quota"));
    s
}

/// The plan-window reading the resume text quotes. Ten minutes old at most,
/// the bound the background reserve uses for the same number.
pub(crate) fn fresh_window_pct() -> Option<i64> {
    crate::api::usage::window_pct_fresh(std::time::Duration::from_secs(600))
}

/// The transcript tail read for the dead set. A wave of 19 notifications is
/// about 30 KB; the orchestrator's own turns between them dominate the rest.
pub(crate) const TAIL_BYTES: u64 = 4 * 1024 * 1024;

/// `GET /api/sessions/{name}/quota`: the worker's own view of the plan.
///
/// One account per machine, so the plan window is shared by every worker; the
/// per-worker half is this worker's limit state and its dead agents. Reads
/// only: no send, no prompt, so it is safe for an isolated worker to call.
pub async fn worker_quota(Path(name): Path<String>) -> Response {
    use crate::api::session_verbs as sv;
    if !sv::session_is_registered(&name) {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"error": format!("no worker named {name}")})),
        )
            .into_response();
    }
    let now = chrono::Utc::now().timestamp();
    let n2 = name.clone();
    let (records, have_transcript) = tokio::task::spawn_blocking(move || match sv::session_jsonl_path(&n2) {
        Some(p) => (sv::iter_jsonl_tail(&p, TAIL_BYTES), true),
        None => (Vec::new(), false),
    })
    .await
    .unwrap_or_default();
    let account_changed_at = sv::observe_claude_account_change(now);
    let (killed, considered) = limit_killed_agents(&records, 0);
    let transcript_reset = sv::transcript_rate_limit(&records);
    let meta = sv::load_meta(&name);
    let window = fresh_window_pct();
    let reserve = crate::api::usage::background_reserve_pct();
    let agents: Vec<Value> = killed
        .iter()
        .map(|k| {
            json!({"id": k.id, "description": k.description, "cause": k.cause, "at": k.at,
                   "reset_at": (k.reset_at > 0).then_some(k.reset_at),
                   "resumable_now": kill_is_over(k, now, account_changed_at)})
        })
        .collect();
    let body = json!({
        "session": name,
        "plan": {
            "five_hour_pct_used": window,
            "why_unmeasured": window.is_none().then_some(
                "no plan-window reading younger than 10 minutes; GET /api/usage probes one"),
            "background_reserve_pct": reserve,
            "account_changed_at": (account_changed_at > 0).then_some(account_changed_at),
            "note": "one Claude account per machine: every worker on it draws on this window",
        },
        "worker": {
            "limited_in_transcript": transcript_reset.is_some(),
            "transcript_reset_at": transcript_reset.filter(|r| *r > 0),
            "rate_limited_until": (sv::meta_i64(&meta, "rate_limited_until") > 0)
                .then(|| sv::meta_i64(&meta, "rate_limited_until")),
        },
        "limit_killed_agents": agents,
        "transcript_read": have_transcript,
        "agent_notifications_considered": considered,
    });
    let out = if have_transcript {
        crate::api::measured::measured(body, considered)
    } else {
        crate::api::measured::unmeasured(body, "no transcript found for this worker")
    };
    axum::Json(out).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(ts: &str, id: &str, status: &str, summary: &str) -> Value {
        json!({"type":"queue-operation","operation":"enqueue","timestamp":ts,
            "content": format!("<task-notification>\n<task-id>{id}</task-id>\n<tool-use-id>toolu_x</tool-use-id>\n<output-file>/tmp/x.output</output-file>\n<status>{status}</status>\n<summary>{summary}</summary>\n<note>A task-notification fires each time</note>\n</task-notification>")})
    }

    // The live shape, celery-retirement 2026-09-26T15:11:15Z.
    const LIMIT: &str = "Agent \"Fix CR-33/34/35 lifecycle bugs\" failed: Agent terminated early due to an API error: You've hit your weekly limit \u{b7} resets Oct 2 at 3am (America/New_York) (error type rate_limit, HTTP 429, request id req_x, model sent to the API: claude-opus-5-5)";
    const LOGOUT: &str = "Agent \"row 8\" failed: Not logged in \u{b7} Please run /login (authentication_failed)";

    fn send_to(ts: &str, id: &str) -> Value {
        json!({"type":"assistant","timestamp":ts,"message":{"role":"assistant","content":[
            {"type":"tool_use","name":"SendMessage","input":{"to":id,"message":"continue"}}]}})
    }

    #[test]
    fn a_limit_wave_is_listed_and_everything_else_is_not() {
        let recs = vec![
            note("2026-09-26T15:11:15Z", "a1", "failed", LIMIT),
            note("2026-09-26T15:11:16Z", "a2", "failed", LOGOUT),
            note("2026-09-26T15:11:17Z", "a3", "failed", "Agent \"x\" failed: exit code 1"),
            note("2026-09-26T15:11:18Z", "a4", "completed", "Agent \"y\" completed"),
            // A quoted notification inside a prompt is not provider framing.
            json!({"type":"user","timestamp":"2026-09-26T15:12:00Z","message":{"role":"user",
                "content": format!("why? <task-notification><task-id>a9</task-id><status>failed</status><summary>{LIMIT}</summary></task-notification>")}}),
        ];
        let (killed, n) = limit_killed_agents(&recs, 0);
        assert_eq!(n, 4, "every provider notification is counted, listed or not");
        let ids: Vec<_> = killed.iter().map(|k| (k.id.as_str(), k.cause)).collect();
        assert_eq!(ids, vec![("a1", "rate_limit"), ("a2", "authentication_failed")]);
        assert_eq!(killed[0].description, "Fix CR-33/34/35 lifecycle bugs");
        assert!(killed[0].reset_at > killed[0].at, "the weekly reset is read from the summary");
        assert_eq!(killed[1].reset_at, 0, "a logout names no reset");
    }

    #[test]
    fn an_agent_the_worker_already_resumed_drops_out() {
        let recs = vec![
            note("2026-09-26T15:11:15Z", "a1", "failed", LIMIT),
            note("2026-09-26T15:11:15Z", "a2", "failed", LIMIT),
            note("2026-09-26T15:11:15Z", "a3", "failed", LIMIT),
            send_to("2026-09-26T19:00:00Z", "a1"),
            note("2026-09-26T19:30:00Z", "a2", "completed", "Agent \"b\" completed"),
        ];
        let (killed, _) = limit_killed_agents(&recs, 0);
        assert_eq!(killed.iter().map(|k| k.id.as_str()).collect::<Vec<_>>(), vec!["a3"]);
        // A NEW death after a resume is listed again.
        let mut again = recs.clone();
        again.push(note("2026-09-26T21:00:00Z", "a1", "failed", LIMIT));
        let (killed, _) = limit_killed_agents(&again, 0);
        assert_eq!(killed.iter().map(|k| k.id.as_str()).collect::<Vec<_>>(), vec!["a3", "a1"]);
        // `since` drops deaths already reported.
        let cut = chrono::DateTime::parse_from_rfc3339("2026-09-26T20:00:00Z").unwrap().timestamp();
        let (killed, _) = limit_killed_agents(&again, cut);
        assert_eq!(killed.iter().map(|k| k.id.as_str()).collect::<Vec<_>>(), vec!["a1"]);
    }

    #[test]
    fn a_stop_is_over_at_its_reset_or_at_a_later_account_switch() {
        let k = LimitKilledAgent { id: "a".into(), description: String::new(), cause: "rate_limit", at: 1000, reset_at: 5000 };
        assert!(!kill_is_over(&k, 4000, 0));
        assert!(!kill_is_over(&k, 5000, 0), "inside the grace");
        assert!(kill_is_over(&k, 5000 + RESET_GRACE_S, 0));
        assert!(kill_is_over(&k, 2000, 1500), "a switch after the death");
        assert!(!kill_is_over(&k, 2000, 900), "a switch before the death is the account that died");
        let logout = LimitKilledAgent { cause: "authentication_failed", reset_at: 0, ..k.clone() };
        assert!(!kill_is_over(&logout, 99_999, 0), "a logout has no clock");
        assert!(kill_is_over(&logout, 2000, 1500));
    }

    #[test]
    fn the_resume_text_is_unchanged_without_dead_agents_and_names_them_with() {
        assert_eq!(resume_text("continue", &[], Some(40), "w"), "continue");
        let k = LimitKilledAgent { id: "a1".into(), description: "row 5".into(), cause: "rate_limit", at: 1_790_000_000, reset_at: 0 };
        let t = resume_text("continue", std::slice::from_ref(&k), Some(40), "w");
        assert!(t.starts_with("continue\n\n[amux] 1 background agent(s)"), "{t}");
        assert!(t.contains("- a1 \"row 5\" (rate_limit, "), "{t}");
        assert!(t.contains("SendMessage to its id"), "{t}");
        assert!(t.contains("40% used"), "{t}");
        assert!(t.contains("/api/sessions/w/quota"), "{t}");
        let none = resume_text("continue", std::slice::from_ref(&k), None, "w");
        assert!(none.contains("no fresh plan-window reading"), "an unknown window is said, never read as 0: {none}");
        let many: Vec<_> = (0..35).map(|i| LimitKilledAgent { id: format!("a{i}"), ..k.clone() }).collect();
        let t = resume_text("continue", &many, None, "w");
        assert!(t.contains("- and 5 more"), "{t}");
        assert!(!t.contains('\u{2014}'), "no em-dash in produced text");
    }
}
