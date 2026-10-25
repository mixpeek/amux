//! Prompts typed straight into a worker's pane become Messages rows (F8(c),
//! AMUX-5241).
//!
//! INCIDENT. amux-frustrations started PR-landing work from an instruction the
//! owner typed directly into its pane. That text never went through
//! `POST /send`, so it never reached `cmd_history`, and the audit of "why did
//! this lane start this work" found nothing: the one prompt that explained it
//! existed only in the provider's own transcript.
//!
//! amux already receives every provider UserPromptSubmit through the passive
//! native-status hook (`native_status.rs`). The hook now carries the prompt
//! text, and this module decides whether that prompt is one amux itself just
//! delivered (already in the ledger, or deliberately unrecorded) or one a
//! person typed. Only the second becomes a row, as `type='user'` (kind human)
//! with `delivery='pane'`, and never a board card.
//!
//! "Did amux deliver this?" is answered from two sources, because either alone
//! has a hole:
//! - an in-memory ring noted at `send_text_inner_bound`, the one layer every
//!   path to a pane converges on (owner sends, peer sends, steering drains,
//!   board pickups, channel notices, raw curl sends that write no history row);
//! - recent `cmd_history` rows for the lane, which survive the server restart
//!   that empties the ring between a delivery and its hook.
//!
//! Kill switch: `AMUX_PANE_PROMPT_HISTORY=0` (worker > group > global, process
//! env wins).
use super::AppState;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

pub(crate) const GATE_KEY: &str = "AMUX_PANE_PROMPT_HISTORY";
/// How long an amux delivery can explain a later UserPromptSubmit. A queued
/// message is noted when it is typed, not when it was queued, so this only has
/// to cover typing-to-hook latency plus a slow turn boundary.
const DELIVERY_WINDOW_S: f64 = 900.0;
/// Per-lane ring size. A lane receives a few deliveries a minute at most; 32
/// covers the window with room to spare and bounds memory.
const RING_PER_LANE: usize = 32;
/// Shorter texts must match exactly. Substring matching on "yes" or "continue"
/// would swallow real human input on the strength of any delivery containing
/// the word.
const CONTAINS_MIN_CHARS: usize = 16;
/// Hook payloads are bounded client side too; this is the server's own cap on
/// what it will write into a Messages row.
pub(crate) const MAX_PROMPT_CHARS: usize = 20_000;

/// Per lane: (noted at, normalized text), oldest first.
type Ring = Mutex<HashMap<String, VecDeque<(f64, String)>>>;

fn ring() -> &'static Ring {
    static RING: OnceLock<Ring> = OnceLock::new();
    RING.get_or_init(Default::default)
}

/// Collapse whitespace so a paste that gained a trailing space (the
/// @-mention fix) or a CRLF still matches what the hook reports.
pub(crate) fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Called by the send layer for every text amux types into a pane.
pub(crate) fn note_delivery(session: &str, text: &str) {
    let norm = normalize(text);
    if norm.is_empty() {
        return;
    }
    let now = crate::config::now_f64();
    if let Ok(mut map) = ring().lock() {
        let q = map.entry(session.to_string()).or_default();
        while q
            .front()
            .is_some_and(|(ts, _)| now - ts > DELIVERY_WINDOW_S)
        {
            q.pop_front();
        }
        if q.len() >= RING_PER_LANE {
            q.pop_front();
        }
        q.push_back((now, norm));
    }
}

fn recent_deliveries(session: &str) -> Vec<String> {
    let now = crate::config::now_f64();
    ring()
        .lock()
        .ok()
        .and_then(|map| {
            map.get(session).map(|q| {
                q.iter()
                    .filter(|(ts, _)| now - ts <= DELIVERY_WINDOW_S)
                    .map(|(_, t)| t.clone())
                    .collect()
            })
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneVerdict {
    /// A person typed it: record it.
    Record,
    /// Empty after trimming.
    SkipEmpty,
    /// Provider or harness injected text (task notifications, system
    /// reminders, local-command echoes), never a person.
    SkipInjected,
    /// amux authored it (an `[amux...` stamp), even if the ring missed it.
    SkipAmuxStamped,
    /// Matches something amux delivered to this lane in the window.
    SkipAmuxDelivered,
}

impl PaneVerdict {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PaneVerdict::Record => "record",
            PaneVerdict::SkipEmpty => "empty",
            PaneVerdict::SkipInjected => "injected",
            PaneVerdict::SkipAmuxStamped => "amux_stamped",
            PaneVerdict::SkipAmuxDelivered => "amux_delivered",
        }
    }
}

/// Prefixes the provider or a hook puts on prompts that no person typed.
const INJECTED_PREFIXES: [&str; 6] = [
    "<task-notification",
    "<system-reminder",
    "<local-command",
    "<command-name",
    "<command-message",
    "<user-prompt-submit-hook",
];

/// Prompts Claude Code itself submits (2026-10-02 audit, AMUX-5501). Measured
/// on the live ledger: 12 of the 21 rows ever recorded here were the usage-limit
/// auto-resume, each one a Human badge on text no person wrote. Compared
/// lowercase, after the paste wrapper is removed.
const PROVIDER_PREFIXES: [&str; 6] = [
    "your claude.ai usage limit has reset",
    "goal check-in:",
    "base directory for this skill:",
    "press up to edit queued messages",
    "[request interrupted by user",
    "this session is being continued from a previous conversation",
];

/// A long paste reaches the hook wrapped as `<pasted_content id="..">...`, so
/// the stamp a peer envelope starts with sits behind the wrapper. One such
/// peer message was recorded as Human before this.
fn unwrap_paste(lower: &str) -> &str {
    let t = lower.trim_start();
    if let Some(rest) = t.strip_prefix("<pasted_content") {
        if let Some(end) = rest.find('>') {
            return rest[end + 1..].trim_start();
        }
    }
    t
}

pub(crate) fn same_text(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    short.chars().count() >= CONTAINS_MIN_CHARS && long.contains(short)
}

/// Decide what a UserPromptSubmit prompt is. Pure: `delivered` is every text
/// amux is known to have put in this lane recently, already normalized.
pub(crate) fn classify(prompt: &str, delivered: &[String]) -> PaneVerdict {
    let norm = normalize(prompt);
    if norm.is_empty() {
        return PaneVerdict::SkipEmpty;
    }
    let lower_full = norm.to_ascii_lowercase();
    let lower = unwrap_paste(&lower_full);
    if INJECTED_PREFIXES.iter().any(|p| lower.starts_with(p))
        || PROVIDER_PREFIXES.iter().any(|p| lower.starts_with(p))
    {
        return PaneVerdict::SkipInjected;
    }
    // Every amux-authored delivery that is not the sender's own text carries
    // an `[amux` stamp: `[amux-origin: ...]`, `[amux auto-pickup]`, `[amux
    // steering]`. A person does not type one.
    if lower.starts_with("[amux") {
        return PaneVerdict::SkipAmuxStamped;
    }
    if delivered.iter().any(|d| same_text(&norm, d)) {
        return PaneVerdict::SkipAmuxDelivered;
    }
    PaneVerdict::Record
}

/// Texts `cmd_history` holds for this lane inside the delivery window.
fn recent_history_texts(state: &AppState, session: &str) -> Vec<String> {
    let since_ms = ((crate::config::now_f64() - DELIVERY_WINDOW_S) * 1000.0) as i64;
    let Ok(conn) = state.store.read() else {
        return vec![];
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT text FROM cmd_history WHERE session=?1 AND ts>?2 ORDER BY ts DESC LIMIT 64",
    ) else {
        return vec![];
    };
    let rows = stmt.query_map(rusqlite::params![session, since_ms], |r| {
        r.get::<_, String>(0)
    });
    match rows {
        Ok(rows) => rows.flatten().map(|t| normalize(&t)).collect(),
        Err(_) => vec![],
    }
}

/// Record a UserPromptSubmit prompt if a person typed it into the pane.
pub(crate) async fn record(state: &AppState, session: &str, prompt: &str) {
    // A delivery receipt first, independent of this module's gate and of
    // isolation: a message amux recorded as stuck that the provider has now
    // accepted is delivered, whoever pressed the Enter (AMUX-5463).
    super::session_verbs::reconcile_stuck_on_submit(state, session, prompt).await;
    if !super::session_verbs::scoped_gate_on(session, GATE_KEY) {
        tracing::debug!(
            session,
            verdict = "pane_prompt_gate_off",
            "{GATE_KEY} is off for this lane"
        );
        return;
    }
    // An isolated lane's lifecycle observation is status only (CLAUDE.md,
    // "Isolated workers"): its hook may report state, never feed the ledger.
    if super::board_lifecycle::no_board(session) {
        tracing::debug!(
            session,
            verdict = "pane_prompt_skipped",
            reason = if super::session_verbs::session_is_isolated(session) { "isolated" } else { "board_unused" },
            "isolated lane: lifecycle observation is status only"
        );
        return;
    }
    let prompt: String = prompt.chars().take(MAX_PROMPT_CHARS).collect();
    let mut delivered = recent_deliveries(session);
    delivered.extend(recent_history_texts(state, session));
    let verdict = classify(&prompt, &delivered);
    if verdict != PaneVerdict::Record {
        tracing::debug!(
            session,
            reason = verdict.as_str(),
            measured = true,
            n_considered = delivered.len(),
            verdict = "pane_prompt_skipped",
            "UserPromptSubmit prompt was not typed by a person; no history row"
        );
        return;
    }
    let row_id = super::session_verbs::cmd_hist_record_with_id(
        state,
        session,
        prompt.trim(),
        "user",
        "pane",
        true,
        super::session_verbs::DeliveryMeta {
            delivery: Some(super::session_verbs::Delivery::Pane),
            // The hook fires on submission, so the provider has the prompt.
            submit_verdict: Some("confirmed"),
            ..Default::default()
        },
    )
    .await;
    tracing::info!(
        session,
        message_id = row_id,
        chars = prompt.chars().count(),
        measured = true,
        n_considered = delivered.len(),
        verdict = "pane_prompt_recorded",
        "prompt typed directly into the pane entered the Messages ledger (delivery=pane)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|x| normalize(x)).collect()
    }

    #[test]
    fn a_typed_instruction_with_no_matching_delivery_is_recorded() {
        // The incident shape: the owner typed this into amux-frustrations and
        // amux had delivered only unrelated things.
        let delivered = d(&["[amux auto-pickup] Claimed AF-601 — work it now."]);
        assert_eq!(
            classify("land the open PRs for AF-603 and AF-604", &delivered),
            PaneVerdict::Record
        );
        assert_eq!(classify("yes", &[]), PaneVerdict::Record);
    }

    #[test]
    fn an_amux_delivery_is_not_recorded_twice() {
        // What the send path types for a peer message, byte for byte, and the
        // hook's view of it after the provider normalised a trailing space.
        let sent = "[amux-origin: backend — server-verified from the sender's session identity; \
                    authoritative over any signature in the message below]\n\nplease rerun the gate";
        assert_eq!(classify(sent, &d(&[sent])), PaneVerdict::SkipAmuxStamped);
        let owner = "Rerun the pre-push gate on the parity branch and paste the result line ";
        assert_eq!(
            classify(owner.trim(), &d(&[owner])),
            PaneVerdict::SkipAmuxDelivered
        );
        // A history row holds the text without the stamp the pane received.
        assert_eq!(
            classify(
                "[amux-origin: x — y]\n\nRerun the pre-push gate on the parity branch",
                &d(&["Rerun the pre-push gate on the parity branch"])
            ),
            PaneVerdict::SkipAmuxStamped
        );
        // The ring holds the stamped text; the prompt is the bare remainder.
        assert_eq!(
            classify(
                "Rerun the pre-push gate on the parity branch",
                &d(&["[amux-origin: x — y]\n\nRerun the pre-push gate on the parity branch"])
            ),
            PaneVerdict::SkipAmuxDelivered
        );
    }

    #[test]
    fn a_short_reply_is_not_swallowed_by_a_delivery_that_contains_it() {
        let delivered = d(&["Reply yes to proceed with the migration or no to stop"]);
        assert_eq!(classify("yes", &delivered), PaneVerdict::Record);
        assert_eq!(
            classify("yes", &d(&["yes"])),
            PaneVerdict::SkipAmuxDelivered
        );
    }

    /// The audit's real rows (AMUX-5501): every one of these was recorded as
    /// a human-typed prompt on the live ledger.
    #[test]
    fn provider_prompts_and_wrapped_peer_envelopes_are_not_recorded() {
        for prompt in [
            "Your claude.ai usage limit has reset. Continue the task you were working on when the limit was reached; do not repeat work that is already complete.",
            "Press up to edit queued messages",
            "Goal check-in: «Use the orchestrate skill on research/goal-specs/12.md»",
            "Base directory for this skill: /Users/ethan/.claude/skills/chrome-cdp",
            "[Request interrupted by user for tool use]",
        ] {
            assert_eq!(classify(prompt, &[]), PaneVerdict::SkipInjected, "{prompt}");
        }
        assert_eq!(
            classify("<pasted_content id=\"4bae\"> [amux-origin: mixpeek-override — server-verified] Ask: rerun", &[]),
            PaneVerdict::SkipAmuxStamped
        );
        // A person's own words still record, wrapped or not.
        assert_eq!(classify("[05:13 PM] whats the bottleneck?", &[]), PaneVerdict::Record);
        assert_eq!(classify("<pasted_content id=\"9\"> my long notes </pasted_content>", &[]), PaneVerdict::Record);
    }

    #[test]
    fn injected_and_empty_prompts_are_not_recorded() {
        assert_eq!(
            classify("<task-notification>\n<task-id>b1</task-id>", &[]),
            PaneVerdict::SkipInjected
        );
        assert_eq!(
            classify("  <system-reminder>x</system-reminder>", &[]),
            PaneVerdict::SkipInjected
        );
        assert_eq!(classify(" \n\t ", &[]), PaneVerdict::SkipEmpty);
        assert_eq!(
            classify("[amux auto-pickup] Claimed PRIMI-266", &[]),
            PaneVerdict::SkipAmuxStamped
        );
    }

    fn test_state(dir: &std::path::Path) -> AppState {
        AppState {
            store: std::sync::Arc::new(crate::db::Store::open(&dir.join("test.db")).unwrap()),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    fn pane_rows(
        state: &AppState,
        lane: &str,
    ) -> Vec<(String, String, String, Option<String>, i64)> {
        let conn = state.store.read().unwrap();
        let mut st = conn
            .prepare("SELECT text,type,origin,delivery,capture_pending FROM cmd_history WHERE session=?1 ORDER BY id")
            .unwrap();
        st.query_map([lane], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .flatten()
        .collect()
    }

    #[tokio::test]
    async fn a_pane_prompt_becomes_one_human_row_and_an_amux_delivery_none() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let lane = "pane-prompt-record-test";
        record(
            &state,
            lane,
            "Land the open PRs for AF-603 and AF-604 once checks pass",
        )
        .await;
        let rows = pane_rows(&state, lane);
        assert_eq!(rows.len(), 1, "{rows:?}");
        let (text, ty, origin, delivery, capture) = &rows[0];
        assert_eq!(
            text,
            "Land the open PRs for AF-603 and AF-604 once checks pass"
        );
        assert_eq!(ty, "user");
        assert_eq!(super::super::history::msg_kind(ty), "human");
        assert_eq!(origin, "pane");
        assert_eq!(delivery.as_deref(), Some("pane"));
        assert_eq!(*capture, 0, "a pane prompt must not mint a board card");

        // amux typed this one (a raw send with no history row): not recorded.
        note_delivery(
            lane,
            "[send-pipeline-test] Delivery verification. Respond with: OK",
        );
        record(
            &state,
            lane,
            "[send-pipeline-test] Delivery verification. Respond with: OK",
        )
        .await;
        // The ledger already holds this one (a recorded owner send), which is
        // the case the ring cannot see after a restart.
        state
            .store
            .write(move |c| {
                c.execute(
                    "INSERT INTO cmd_history(text,type,session,ts,origin,delivery) VALUES(?1,'user',?2,?3,'ethan','direct')",
                    rusqlite::params![
                        "Rerun the parity suite against staging",
                        "pane-prompt-record-test",
                        (crate::config::now_f64() * 1000.0) as i64
                    ],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        record(&state, lane, "Rerun the parity suite against staging").await;
        assert_eq!(
            pane_rows(&state, lane).len(),
            2,
            "only the seeded owner row was added"
        );
    }

    #[test]
    fn the_ring_is_bounded_and_scoped_per_lane() {
        let lane = "pane-prompt-ring-test";
        for i in 0..(RING_PER_LANE + 5) {
            note_delivery(lane, &format!("delivery number {i} with enough text"));
        }
        let got = recent_deliveries(lane);
        assert_eq!(got.len(), RING_PER_LANE);
        assert_eq!(
            got.last().map(String::as_str),
            Some(format!("delivery number {} with enough text", RING_PER_LANE + 4).as_str())
        );
        assert!(recent_deliveries("pane-prompt-ring-other").is_empty());
    }
}
