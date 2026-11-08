//! Goal-loop guard (AMUX-5277): a lane whose Claude Code `/goal` keeps
//! re-prompting it while the only step left needs the owner.
//!
//! # The incident
//!
//! 2026-09-27, tubescience-parity, under `/goal keep going until you can switch
//! between mixpeek and their current apis and the results are the same`. The
//! last step needed the owner to sign in at a URL (card TP-37, needsyou).
//! Claude Code's goal check runs as a Stop hook: it evaluated "not met" after
//! every turn and sent the agent back, and every turn ended on the same text:
//! "The results still don't match production. The only thing left is TP-37:
//! sign in once at https://semantic-search-tawny.vercel.app in the
//! `ethan-tubescience` Chrome profile." The transcript holds 2,271 `goal_status`
//! records with `met: false`; one run lasted 27 minutes and 61.6k tokens, and
//! one turn (by its `turn_duration` record) lasted 83 minutes. amux showed the
//! lane `active` with no waiting reason, which is what Ethan asked about:
//! "amux should've understood this and intervened or indicated via status".
//! gs-4-gke-minimization showed the same shape the day before.
//!
//! # What this does
//!
//! 1. DETECT at every Claude turn end: the last three turn-ending texts within
//!    an hour are near-duplicates (token Jaccard >= 0.8 with URLs and numbers
//!    normalised, compared on the whole text and on the final paragraph) AND
//!    the text is an owner ask, or names a card that is waiting on the owner.
//!    Verdict `goal_loop_detected`.
//! 2. STATUS: lane meta `blocked_on_owner_since/_card/_ask`, which the session
//!    list projects as `status: waiting`, `waiting_reason: owner`.
//! 3. PARK: the goal condition goes into lane meta and onto the card with the
//!    exact restore command, then `/goal clear` is delivered (see [`park`] for
//!    why that needs an Escape first). Verdicts `goal_parked`,
//!    `goal_park_failed`, `goal_park_skipped_isolated`.
//! 4. RESTORE: when the card leaves needsyou/decision, `/goal <condition>` is
//!    delivered once per park. Verdict `goal_restored` (or
//!    `goal_restore_failed`).
//!
//! Kill switch `AMUX_GOAL_LOOP_GUARD`, default ON, scoped worker > group >
//! global with the process env winning (the same resolver as the owner-ask
//! steer). Every side effect is claimed once per occurrence in
//! `session_events`, so a restart or a duplicate Stop cannot act twice.

use super::session_verbs as sv;
use super::turn_end::{self, clip, rx, OwnerAsk, TurnTail};
use super::AppState;
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub(crate) const GUARD_KEY: &str = "AMUX_GOAL_LOOP_GUARD";
/// The repeats that make a loop, and the window they must fall in.
const REPEATS: usize = 3;
const WINDOW_S: f64 = 3600.0;
const SIMILARITY: f64 = 0.8;

// Lane meta. The STATUS stamps say "this lane is blocked on the owner" and are
// what the session list reads. The PARK stamps remember the goal that was
// cleared, so the restore survives the status clearing first (the owner can
// talk to the lane about something else while the card is still open).
pub(crate) const M_SINCE: &str = "blocked_on_owner_since";
pub(crate) const M_CARD: &str = "blocked_on_owner_card";
pub(crate) const M_ASK: &str = "blocked_on_owner_ask";
const M_TEXT: &str = "blocked_on_owner_text";
pub(crate) const M_GOAL: &str = "goal_parked_condition";
pub(crate) const M_PARK_CARD: &str = "goal_parked_card";
pub(crate) const M_PARK_ID: &str = "goal_park_id";
pub(crate) const M_PARKED: &str = "goal_parked";

// ---------------------------------------------------------------------------
// Pure: turn ends, similarity, card ids, detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnEnd {
    pub id: String,
    pub ts: f64,
    pub text: String,
}

/// Every turn that ENDED on assistant text, oldest first.
///
/// A turn end is the edge Claude Code marks with a `stop_hook_summary` (every
/// Stop, including the ones the goal hook blocks) or a `turn_duration` system
/// record. The nearest main-thread assistant record before it must carry text
/// and no tool_use; its text is joined across the records sharing its
/// `message.id`. Read from the transcript rather than kept in memory, so a
/// server restart mid-loop loses nothing.
pub(crate) fn turn_ends(records: &[Value]) -> Vec<TurnEnd> {
    let mut out: Vec<TurnEnd> = Vec::new();
    for (i, r) in records.iter().enumerate() {
        if r["type"] != "system"
            || !matches!(r["subtype"].as_str(), Some("stop_hook_summary" | "turn_duration"))
        {
            continue;
        }
        let Some(j) = records[..i]
            .iter()
            .rposition(|a| a["type"] == "assistant" && a["isSidechain"] != true)
        else {
            continue;
        };
        // A real prompt between the assistant text and this edge means the
        // edge belongs to a later, textless turn.
        if records[j + 1..i].iter().any(|u| {
            u["type"] == "user" && u["isMeta"] != true && {
                let c = &u["message"]["content"];
                !c.as_array().is_some_and(|a| a.iter().any(|b| b["type"] == "tool_result"))
            }
        }) {
            continue;
        }
        let last = &records[j];
        let blocks = last["message"]["content"].as_array();
        if blocks.is_none_or(|b| b.iter().any(|x| x["type"] == "tool_use")) {
            continue;
        }
        let mid = last["message"]["id"].as_str().unwrap_or("").to_string();
        let mut parts = Vec::new();
        for a in records[..=j].iter().rev() {
            if a["type"] != "assistant" {
                continue;
            }
            if mid.is_empty() || a["message"]["id"].as_str() != Some(mid.as_str()) {
                break;
            }
            let t = turn_end::text_blocks(&a["message"]["content"]).join("\n\n");
            if !t.trim().is_empty() {
                parts.push(t);
            }
        }
        if parts.is_empty() {
            continue;
        }
        parts.reverse();
        let id = if mid.is_empty() { last["uuid"].as_str().unwrap_or("").to_string() } else { mid };
        if out.last().is_some_and(|p| p.id == id) {
            continue;
        }
        out.push(TurnEnd { id, ts: turn_end::rec_ts(last).unwrap_or(0.0), text: parts.join("\n\n") });
    }
    out
}

/// Normalised token set: lowercase, URLs and numbers folded to placeholders,
/// card ids (`TP-37`) kept whole so two different cards never read as one ask.
pub(crate) fn tokens(text: &str) -> BTreeSet<String> {
    let t = text.to_lowercase();
    let t = rx!(r"https?://\S+").replace_all(&t, " zurl ");
    let mut out = BTreeSet::new();
    for tok in t.split(|c: char| !(c.is_alphanumeric() || c == '-')) {
        let tok = tok.trim_matches('-');
        if tok.is_empty() {
            continue;
        }
        if rx!(r"^[a-z]{2,6}-\d+$").is_match(tok) {
            out.insert(tok.to_string());
        } else if tok.chars().any(|c| c.is_ascii_digit()) {
            out.insert("znum".to_string());
        } else {
            out.insert(tok.to_string());
        }
    }
    out
}

fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    inter / union
}

/// How alike two turn-ending texts are: the best of the whole text, the final
/// paragraph and the final sentence, the last two counted only when they say
/// something (six tokens or more). The narrower arms exist because the loop
/// varies its preamble: tubescience-parity alternated "I checked again and the
/// semantic-search token still returns 401." with no preamble at all, while
/// the sentence it ENDED on never changed (whole-text Jaccard 0.7).
pub(crate) fn similarity(a: &str, b: &str) -> f64 {
    let substantive = |x: &BTreeSet<String>, y: &BTreeSet<String>| {
        if x.len() >= 6 && y.len() >= 6 { jaccard(x, y) } else { 0.0 }
    };
    let last_sentence = |t: &str| {
        turn_end::sentences(&turn_end::tail_paragraphs(t, 1)).pop().unwrap_or_default()
    };
    let whole = jaccard(&tokens(a), &tokens(b));
    let para = substantive(&tokens(&turn_end::tail_paragraphs(a, 1)), &tokens(&turn_end::tail_paragraphs(b, 1)));
    let sent = substantive(&tokens(&last_sentence(a)), &tokens(&last_sentence(b)));
    whole.max(para).max(sent)
}

/// Board card ids named in a text, in order of first appearance.
pub(crate) fn card_ids(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in rx!(r"\b[A-Z]{2,6}-\d{1,6}\b").find_iter(text) {
        let id = m.as_str().to_string();
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Is a card waiting on the owner? `needsyou`, or a decision card that still
/// declares who it waits on. Terminal, archived, or back in the work queue
/// (todo/doing) means the owner has answered.
pub(crate) fn card_blocks_on_owner(status: &str, item_type: &str, waiting_on: bool, archived: bool) -> bool {
    if archived {
        return false;
    }
    match status {
        "needsyou" => true,
        "done" | "verified" | "discarded" | "todo" | "doing" => false,
        _ => item_type == "decision" && waiting_on,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LoopHit {
    pub text: String,
    pub ask: String,
    pub card: Option<String>,
    pub similarity: f64,
    pub span_s: f64,
    pub boundary: Option<turn_end::Boundary>,
}

/// The detector. `blocks(id)` answers whether a named card is waiting on the
/// owner (a board read in production, a table in tests).
pub(crate) fn detect(ends: &[TurnEnd], blocks: impl Fn(&str) -> bool) -> Option<LoopHit> {
    if ends.len() < REPEATS {
        return None;
    }
    let last = &ends[ends.len() - REPEATS..];
    let span_s = last[REPEATS - 1].ts - last[0].ts;
    if !(0.0..=WINDOW_S).contains(&span_s) {
        return None;
    }
    let newest = &last[REPEATS - 1].text;
    let mut sim = f64::MAX;
    for (i, a) in last.iter().enumerate() {
        for b in &last[i + 1..] {
            sim = sim.min(similarity(&a.text, &b.text));
        }
    }
    if sim < SIMILARITY {
        return None;
    }
    let (ask, boundary) = match turn_end::classify_owner_ask(newest) {
        OwnerAsk::None => (None, None),
        OwnerAsk::InBoundary { sentence } => (Some(sentence), None),
        OwnerAsk::Boundary { sentence, kind } => (Some(sentence), Some(kind)),
    };
    let card = card_ids(newest).into_iter().find(|c| blocks(c));
    if ask.is_none() && card.is_none() {
        return None;
    }
    let ask = ask.unwrap_or_else(|| clip(&turn_end::tail_paragraphs(newest, 1), 200));
    Some(LoopHit { text: newest.clone(), ask, card, similarity: sim, span_s, boundary })
}

/// A `/goal` command record (`<command-name>/goal</command-name>`) after
/// `since` whose arguments satisfy `want`. This is how a park or restore is
/// VERIFIED: Claude Code writes the record only when it ran the command, so a
/// `/goal clear` that arrived as prose (the origin-stamp bug, 14:42:07Z
/// 2026-09-27) or sat in the queue does not count.
pub(crate) fn goal_command_since(records: &[Value], since: f64, want: impl Fn(&str) -> bool) -> bool {
    records.iter().any(|r| {
        if r["type"] != "user" || turn_end::rec_ts(r).unwrap_or(0.0) < since {
            return false;
        }
        let t = turn_end::text_blocks(&r["message"]["content"]).join("");
        if !t.contains("<command-name>/goal</command-name>") {
            return false;
        }
        let args = rx!(r"(?s)<command-args>(.*?)</command-args>")
            .captures(&t)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();
        want(&args)
    })
}

/// The note appended to the card when a goal is parked. Carries the exact
/// command, so the owner can restore by hand if amux cannot.
pub(crate) fn park_note(lane: &str, goal: &str, ask: &str, when: &str) -> String {
    format!(
        "\n\n--- Goal parked {when} by the amux goal-loop guard (AMUX-5277) ---\n\n\
         {lane} ended three turns in a row on the same ask while its /goal kept re-prompting it: \
         \"{}\". amux cleared the goal to stop the loop. When this card leaves needsyou, amux \
         restores the goal automatically. To restore it by hand, send this to {lane}:\n\n/goal {goal}",
        clip(ask, 240)
    )
}

// ---------------------------------------------------------------------------
// Side effects
// ---------------------------------------------------------------------------

fn card_blocks_now(state: &AppState, id: &str) -> Option<bool> {
    let conn = state.store.read().ok()?;
    let row = crate::db::board_store::get_issue(&conn, id).ok()??;
    Some(card_blocks_on_owner(
        &row.status,
        &row.item_type,
        row.waiting_on.as_deref().is_some_and(|w| !w.trim().is_empty()),
        row.archived != 0,
    ))
}

async fn append_card_note(state: &AppState, id: &str, note: String) -> Result<(), String> {
    let id = id.to_string();
    state
        .store
        .write_async(move |conn| {
            use crate::db::board_store as bs;
            let mut applied = false;
            if let Some(mut row) = bs::get_issue(conn, &id)? {
                row.desc.push_str(&note);
                bs::save_patched(conn, &mut row)?;
                applied = true;
            }
            Ok(crate::db::WriteOutcome { applied, events: vec![] })
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|o| if o.applied { Ok(()) } else { Err("card not found".into()) })
}

fn clear_status(name: &str) {
    sv::update_meta(name, &[(M_SINCE, json!(0)), (M_CARD, json!("")), (M_ASK, json!("")), (M_TEXT, json!(""))]);
    crate::api::sessions_legacy::invalidate_sessions_cache();
}

fn clear_park(name: &str) {
    sv::update_meta(name, &[(M_GOAL, json!("")), (M_PARK_CARD, json!("")), (M_PARK_ID, json!("")), (M_PARKED, json!(false))]);
}

/// Run at every Claude turn end, isolated or not. Returns true while the lane
/// is goal-looping on the owner, so the caller does not steer it: an
/// owner-ask steer into this loop only buys it another turn (it steered
/// tubescience-parity "proceed" twice on a sign-in only the owner could do).
pub(crate) async fn on_turn_end(state: &AppState, name: &str, isolated: bool, records: &[Value], turn: &TurnTail) -> bool {
    let meta = sv::load_meta(name);
    let stamped = sv::meta_i64(&meta, M_SINCE) > 0;
    let mut ends = turn_ends(records);
    if ends.last().is_none_or(|e| e.ts + 0.5 < turn.ts) {
        ends.push(TurnEnd { id: turn.uuid.clone(), ts: turn.ts, text: turn.text.clone() });
    }
    let hit = detect(&ends, |id| card_blocks_now(state, id).unwrap_or(false));
    if !turn_end::enabled(name, GUARD_KEY) {
        if hit.is_some() {
            tracing::info!(session = %name, verdict = "goal_loop_guard_disabled",
                "goal-loop guard: loop shape seen but {GUARD_KEY} is off for this lane");
        }
        if stamped {
            clear_status(name);
        }
        return false;
    }
    let Some(hit) = hit else {
        // The status clears when the lane's turn ends on something else.
        if stamped && similarity(&turn.text, &sv::meta_str(&meta, M_TEXT)) < SIMILARITY {
            clear_status(name);
            tracing::info!(session = %name, verdict = "goal_loop_cleared", reason = "turn_moved_on",
                card = %sv::meta_str(&meta, M_CARD),
                "goal-loop guard: the lane's turn ended on something other than the owner ask; status cleared");
        }
        return false;
    };
    if stamped {
        tracing::debug!(session = %name, verdict = "goal_loop_already_stamped", "goal-loop guard: loop already handled");
        return true;
    }
    let Some(goal) = turn_end::goal_condition(records) else {
        tracing::debug!(session = %name, verdict = "goal_loop_no_goal",
            "goal-loop guard: repeated owner ask but no active /goal; left to the owner-ask classifier");
        return false;
    };
    // Name the card, or file one: the restore is keyed on a card leaving
    // needsyou, so a loop with no card cannot be parked safely.
    let card = match hit.card.clone() {
        Some(c) => Some(c),
        None => {
            let q = turn_end::as_question(&hit.ask, name);
            match turn_end::file_goal_loop_card(state, name, hit.boundary, &q, &turn_end::tail_paragraphs(&hit.text, 3), isolated).await {
                Ok((id, _, _)) => Some(id),
                Err(e) => {
                    tracing::warn!(session = %name, verdict = "goal_loop_card_failed", error = %e,
                        "goal-loop guard: could not file a card for the loop");
                    None
                }
            }
        }
    };
    let since = chrono::Utc::now().timestamp();
    sv::update_meta(name, &[
        (M_SINCE, json!(since)),
        (M_CARD, json!(card.clone().unwrap_or_default())),
        (M_ASK, json!(clip(&hit.ask, 200))),
        (M_TEXT, json!(clip(&hit.text, 2000))),
    ]);
    crate::api::sessions_legacy::invalidate_sessions_cache();
    tracing::warn!(session = %name, verdict = "goal_loop_detected", card = card.as_deref().unwrap_or(""),
        similarity = hit.similarity, span_s = hit.span_s, isolated, measured = true, n_considered = ends.len(),
        ask = %clip(&hit.ask, 160),
        "goal-loop guard: the last three turn ends under a /goal are the same owner ask; lane reported waiting on the owner (AMUX-5277)");
    let Some(card) = card else {
        tracing::warn!(session = %name, verdict = "goal_park_failed", reason = "no_card",
            "goal-loop guard: goal left active because there is no card to restore it from");
        return true;
    };
    let park_id = format!("{name}:{since}");
    if !turn_end::claim_once_pub(state, name, "goal_loop.park", format!("goal-park:{park_id}"),
        json!({"card": card, "goal": goal})).await
    {
        return true;
    }
    sv::update_meta(name, &[
        (M_GOAL, json!(goal)),
        (M_PARK_CARD, json!(card)),
        (M_PARK_ID, json!(park_id)),
        (M_PARKED, json!(false)),
    ]);
    let when = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
    if let Err(e) = append_card_note(state, &card, park_note(name, &goal, &hit.ask, &when)).await {
        tracing::warn!(session = %name, verdict = "goal_park_note_failed", card = %card, error = %e,
            "goal-loop guard: could not append the goal to the card; the condition is still in lane meta");
    }
    if isolated {
        // CLAUDE.md "Isolated workers": the only amux input an isolated lane
        // takes is owner configuration (schedules, the goal keeper CONTINUING
        // an owner-set goal). Clearing that goal is amux deciding, so it is not
        // delivered. The status and the card are the owner-only path: the card
        // says to press Escape in the lane and type `/goal clear`.
        tracing::warn!(session = %name, verdict = "goal_park_skipped_isolated", card = %card,
            "goal-loop guard: isolated lane; status set and card updated, /goal clear left to the owner");
        return true;
    }
    match park(state, name).await {
        Ok(how) => {
            sv::update_meta(name, &[(M_PARKED, json!(true))]);
            tracing::warn!(session = %name, verdict = "goal_parked", card = %card, how = %how, measured = true,
                "goal-loop guard: /goal clear landed (command record in the transcript); goal stored on the card");
        }
        Err(e) => tracing::warn!(session = %name, verdict = "goal_park_failed", card = %card, error = %e,
            "goal-loop guard: /goal clear did not land; the loop continues, the status and card still show it"),
    }
    true
}

async fn generating(name: &str) -> bool {
    sv::pane_bar_says_generating(&sv::tmux_capture(name, 12).await)
}

/// Deliver `/goal clear` to a lane whose goal hook keeps it busy.
///
/// WHY AN ESCAPE FIRST. Text pasted into a busy Claude Code composer is queued
/// (`queue-operation: enqueue`). Prose in the queue is folded into the running
/// turn (`remove` plus a `queued_command` attachment: the owner-ask steer
/// reached tubescience-parity 15 seconds after it was queued, mid-loop). A
/// SLASH COMMAND is not: it waits for the turn to end (`dequeue` beside a
/// `turn_duration` record). Measured on mixpeek-studio 2026-07-05: `/compact`
/// queued at 03:58:38 ran at 04:09:51, the turn's end, while three prose
/// messages queued after it were folded in at 04:04:47. A Stop hook that
/// blocks is not a turn end, so under a looping goal a queued `/goal clear`
/// waits for as long as the loop runs: tubescience-parity's turns lasted 83 and
/// 27 minutes. Delivering at the Stop edge does not help either, because the
/// goal check is itself a Stop hook and the lane is busy while it runs.
///
/// Escape interrupts the turn, which ends it without running Stop hooks, so
/// the composer is idle and the command runs at once. The price is the
/// in-flight turn, which in a loop is another copy of the same ask. Verified by
/// the transcript's own command record, never by the keystrokes landing.
async fn park(state: &AppState, name: &str) -> Result<String, String> {
    // Never interrupt over the owner's own typing. Escape can put text that is
    // sitting in the composer back into play or clear it, and a draft the owner
    // is writing is worth more than one more copy of the looping ask. The raw
    // frame keeps SGR attributes, so a dim suggestion is not mistaken for input.
    let raw = sv::tmux_capture(name, 25).await;
    if let Some(draft) = sv::composer_state(&raw).typed() {
        return Err(format!(
            "composer holds typed input ({} chars); not interrupting over it",
            draft.chars().count()
        ));
    }
    let interrupted = if generating(name).await {
        sv::send_key(name, "Escape").await;
        let mut idle = false;
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if !generating(name).await {
                idle = true;
                break;
            }
        }
        if !idle {
            return Err("lane still generating 10s after Escape".into());
        }
        true
    } else {
        false
    };
    deliver_goal_command(state, name, "/goal clear", |a| a.eq_ignore_ascii_case("clear"))
        .await
        .map(|msg| format!("interrupted={interrupted}; {msg}"))
}

async fn deliver_goal_command(state: &AppState, name: &str, cmd: &str, want: impl Fn(&str) -> bool) -> Result<String, String> {
    let sent_at = crate::config::now_f64() - 1.0;
    let (ok, msg) = sv::send_text(state, name, cmd, false, sv::SendOrigin::Automation).await;
    if !ok {
        return Err(msg);
    }
    let path = sv::session_jsonl_path(name).ok_or("no transcript to verify the command")?;
    for _ in 0..15 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let p = path.clone();
        let records = tokio::task::spawn_blocking(move || sv::iter_jsonl_tail(&p, 400_000)).await.unwrap_or_default();
        if goal_command_since(&records, sent_at, &want) {
            return Ok(msg);
        }
    }
    Err(format!("sent ({msg}) but no {cmd} command record in the transcript within 15s"))
}

/// `/goal <condition>`: a send that sets a goal, as opposed to `/goal clear`.
pub(crate) fn sets_goal(text: &str) -> bool {
    let mut parts = text.trim_start().splitn(2, char::is_whitespace);
    parts.next() == Some("/goal") && parts.next().is_some_and(|a| !a.trim().is_empty() && !a.trim().eq_ignore_ascii_case("clear"))
}

/// A `/goal` that went in as plain text is delivered, confirmed and inert, so
/// the send cannot prove it. Watch the transcript for the command record and
/// say so loudly if it never comes. A lane mid-turn or holding a queued send
/// registers it at its next boundary, hence the long wait.
pub(crate) fn watch_goal_registration(state: AppState, name: String, sent_at: f64) {
    let wait_s = std::env::var("AMUX_GOAL_REGISTER_WAIT_S").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(1800.0);
    tokio::spawn(async move {
        while crate::config::now_f64() < sent_at + wait_s {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let Some(path) = sv::session_jsonl_path(&name) else { continue };
            let records = tokio::task::spawn_blocking(move || sv::iter_jsonl_tail(&path, 2_000_000)).await.unwrap_or_default();
            if goal_command_since(&records, sent_at, |a| !a.is_empty()) {
                tracing::info!(session = %name, verdict = "goal_command_registered", measured = true, n_considered = 1,
                    "the /goal sent to this worker registered as a command");
                return;
            }
        }
        tracing::warn!(session = %name, verdict = "goal_command_not_registered", measured = true, n_considered = 1,
            "a /goal was delivered but no command record reached the transcript; the worker has no goal");
        crate::push::send_all(&state, &format!("{name}: /goal did not register"),
            "It arrived as plain text, so nothing keeps this worker on it. Send it again.",
            &name, &format!("goal-unregistered:{name}"), &format!("/#peek={name}")).await;
    });
}

/// Periodic (goal keeper tick): release status stamps whose card was answered,
/// and restore a parked goal once its card leaves needsyou/decision.
pub(crate) async fn restore_tick(state: &AppState, names: &[String]) {
    for name in names {
        let meta = sv::load_meta(name);
        let status_card = sv::meta_str(&meta, M_CARD);
        if sv::meta_i64(&meta, M_SINCE) > 0 && !status_card.is_empty()
            && card_blocks_now(state, &status_card) == Some(false)
        {
            clear_status(name);
            tracing::info!(session = %name, verdict = "goal_loop_cleared", reason = "card_resolved", card = %status_card,
                "goal-loop guard: the card the lane was blocked on left needsyou; status cleared");
        }
        let park_card = sv::meta_str(&meta, M_PARK_CARD);
        let goal = sv::meta_str(&meta, M_GOAL);
        if park_card.is_empty() || goal.is_empty() {
            continue;
        }
        if card_blocks_now(state, &park_card) != Some(false) {
            continue;
        }
        if meta.get(M_PARKED).and_then(Value::as_bool) != Some(true) {
            // The park never landed, so the goal is still active: nothing to put back.
            clear_park(name);
            tracing::info!(session = %name, verdict = "goal_restore_not_needed", card = %park_card,
                "goal-loop guard: card resolved but the goal was never cleared; nothing to restore");
            continue;
        }
        let park_id = sv::meta_str(&meta, M_PARK_ID);
        if !turn_end::claim_once_pub(state, name, "goal_loop.restore", format!("goal-restore:{park_id}"),
            json!({"card": park_card, "goal": goal})).await
        {
            clear_park(name);
            continue;
        }
        if !turn_end::enabled(name, GUARD_KEY) {
            clear_park(name);
            tracing::info!(session = %name, verdict = "goal_restore_disabled", card = %park_card,
                "goal-loop guard: {GUARD_KEY} is off; the restore command is on the card");
            continue;
        }
        let cmd = format!("/goal {goal}");
        let want_goal = goal.trim().to_lowercase();
        let outcome = deliver_goal_command(state, name, &cmd, |a| a.trim().to_lowercase() == want_goal).await;
        clear_park(name);
        clear_status(name);
        match outcome {
            Ok(msg) => tracing::warn!(session = %name, verdict = "goal_restored", card = %park_card, msg = %msg,
                "goal-loop guard: card left needsyou; /goal restored on the lane"),
            Err(e) => tracing::warn!(session = %name, verdict = "goal_restore_failed", card = %park_card, error = %e,
                "goal-loop guard: could not restore the goal; the exact command is on the card"),
        }
    }
}

/// Lanes carrying a status or park stamp. Reads meta only.
pub(crate) fn stamped(meta: &serde_json::Map<String, Value>) -> bool {
    sv::meta_i64(meta, M_SINCE) > 0 || !sv::meta_str(meta, M_PARK_CARD).is_empty()
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_a_goal_with_a_condition_is_watched() {
        assert!(super::sets_goal("/goal ship it"));
        assert!(super::sets_goal("  /goal ship\nit"));
        assert!(!super::sets_goal("/goal clear"));
        assert!(!super::sets_goal("/goal"));
        assert!(!super::sets_goal("/goals ship it"));
    }

    use super::*;

    const TP37: &str = "The results still don't match production. The only thing left is TP-37: sign in once at https://semantic-search-tawny.vercel.app in the `ethan-tubescience` Chrome profile.";
    const TP37_LONG: &str = "The results still don't match production. I checked again and the semantic-search token still returns 401.\n\nThe only thing left is TP-37: sign in once at https://semantic-search-tawny.vercel.app in the `ethan-tubescience` Chrome profile.";

    fn asst(ts: &str, mid: &str, text: &str) -> Value {
        json!({"type":"assistant","timestamp":ts,"uuid":format!("u-{mid}"),"message":{"id":mid,"content":[{"type":"text","text":text}]}})
    }
    fn thinking(ts: &str, mid: &str) -> Value {
        json!({"type":"assistant","timestamp":ts,"message":{"id":mid,"content":[{"type":"thinking","thinking":""}]}})
    }
    fn tool(ts: &str, mid: &str) -> Value {
        json!({"type":"assistant","timestamp":ts,"message":{"id":mid,"content":[{"type":"tool_use","name":"Bash"}]}})
    }
    fn tool_result(ts: &str) -> Value {
        json!({"type":"user","timestamp":ts,"message":{"content":[{"type":"tool_result","content":"401"}]}})
    }
    // The live shape, from 834292ca-...jsonl at 14:40:13Z: the goal hook's
    // feedback as an isMeta user record, the goal_status attachment, then the
    // stop_hook_summary.
    fn goal_stop(ts: &str) -> Vec<Value> {
        vec![
            json!({"type":"user","isMeta":true,"timestamp":ts,"message":{"content":"Stop hook feedback:\n[keep going until you can switch between mixpeek and their current apis and the results are the same]: The transcript explicitly and repeatedly states 'The results still don't match production'"}}),
            json!({"type":"attachment","timestamp":ts,"attachment":{"type":"goal_status","met":false,"condition":"keep going until you can switch between mixpeek and their current apis and the results are the same"}}),
            json!({"type":"system","subtype":"stop_hook_summary","timestamp":ts,"preventedContinuation":false}),
        ]
    }
    fn loop_records() -> Vec<Value> {
        let mut r = vec![tool("2026-09-27T14:40:05Z", "m0"), tool_result("2026-09-27T14:40:09Z")];
        r.push(asst("2026-09-27T14:40:11Z", "m1", TP37_LONG));
        r.extend(goal_stop("2026-09-27T14:40:13Z"));
        r.push(thinking("2026-09-27T14:40:16Z", "m2"));
        r.push(asst("2026-09-27T14:40:17Z", "m2", TP37));
        r.extend(goal_stop("2026-09-27T14:40:18Z"));
        r.push(tool("2026-09-27T14:40:27Z", "m3"));
        r.push(tool_result("2026-09-27T14:40:30Z"));
        r.push(asst("2026-09-27T14:40:33Z", "m4", TP37_LONG));
        r.extend(goal_stop("2026-09-27T14:40:36Z"));
        r.push(asst("2026-09-27T14:40:39Z", "m5", TP37));
        r.extend(goal_stop("2026-09-27T14:40:41Z"));
        r
    }

    #[test]
    fn turn_ends_reads_the_live_goal_loop_shape() {
        let ends = turn_ends(&loop_records());
        assert_eq!(ends.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["m1", "m2", "m4", "m5"]);
        assert_eq!(ends[1].text, TP37);
        // A turn that ended on a tool call is not a turn end.
        let r = vec![tool("2026-09-27T14:40:27Z", "m3"), json!({"type":"system","subtype":"stop_hook_summary"})];
        assert!(turn_ends(&r).is_empty());
    }

    #[test]
    fn the_real_tp37_texts_are_a_loop_on_the_owner() {
        let ends = turn_ends(&loop_records());
        // With no board at all, the sign-in sentence is itself an owner ask.
        let hit = detect(&ends, |_| false).expect("TP-37 loop detected");
        assert!(hit.ask.contains("sign in once"), "{}", hit.ask);
        assert_eq!(hit.boundary, Some(turn_end::Boundary::OwnerOnly));
        assert_eq!(hit.card, None);
        // With TP-37 in needsyou the card is named.
        let hit = detect(&ends, |id| id == "TP-37").unwrap();
        assert_eq!(hit.card.as_deref(), Some("TP-37"));
        assert!(hit.similarity >= 0.8 && hit.span_s < 60.0);
        // gs-4's shape: a pointer ask repeated, no card.
        let gs4 = "Still waiting for your answers: the six permission lines, the re-login, and the ten decisions in my earlier message. `/goal clear` stops these repeats.";
        let ends: Vec<TurnEnd> = (0..3).map(|i| TurnEnd { id: format!("g{i}"), ts: 100.0 + i as f64 * 30.0, text: gs4.into() }).collect();
        assert!(detect(&ends, |_| false).is_some());
    }

    #[test]
    fn varied_progress_reports_and_single_asks_are_not_a_loop() {
        let progress = [
            "Rebuilt the index for tenant A; 128/231 top-k now match. Next I will re-run the pair-order check.",
            "Pair order is 7551/7862. I found the tokenizer mismatch in the query path and am patching it now.",
            "Patched the tokenizer. Identical queries went from 2/14 to 9/14. Moving on to the talent field.",
        ];
        let ends: Vec<TurnEnd> = progress.iter().enumerate()
            .map(|(i, t)| TurnEnd { id: format!("p{i}"), ts: 100.0 + i as f64 * 60.0, text: (*t).into() }).collect();
        assert_eq!(detect(&ends, |_| true), None);
        // One ask, or two, is not a loop.
        let one = vec![TurnEnd { id: "a".into(), ts: 1.0, text: TP37.into() }];
        assert_eq!(detect(&one, |_| true), None);
        let two: Vec<TurnEnd> = (0..2).map(|i| TurnEnd { id: format!("a{i}"), ts: i as f64, text: TP37.into() }).collect();
        assert_eq!(detect(&two, |_| true), None);
        // Three identical status lines that ask nothing and name no waiting card.
        let status = "Tests are green and the branch is pushed. Continuing with the next card on the board now.";
        let three: Vec<TurnEnd> = (0..3).map(|i| TurnEnd { id: format!("s{i}"), ts: i as f64, text: status.into() }).collect();
        assert_eq!(detect(&three, |_| false), None);
        // The same ask spread over more than an hour is not a loop.
        let slow: Vec<TurnEnd> = (0..3).map(|i| TurnEnd { id: format!("x{i}"), ts: i as f64 * 2000.0, text: TP37.into() }).collect();
        assert_eq!(detect(&slow, |_| true), None);
    }

    #[test]
    fn card_ids_and_similarity_normalisation() {
        assert_eq!(card_ids("The only thing left is TP-37: see also AMUX-5277 and TP-37 again"), vec!["TP-37", "AMUX-5277"]);
        assert!(card_ids("utf8 and gs-3 in lowercase").is_empty());
        // URLs and numbers normalise; card ids do not.
        assert!(similarity("token returns 401 at https://a.example/x", "token returns 403 at https://b.example/y") > 0.99);
        assert!(similarity("waiting on TP-37 only", "waiting on TP-38 only") < 0.8);
    }

    #[test]
    fn restore_trigger_follows_the_card_leaving_needsyou() {
        assert!(card_blocks_on_owner("needsyou", "chore", false, false));
        assert!(card_blocks_on_owner("backlog", "decision", true, false));
        for s in ["todo", "doing", "done", "verified", "discarded"] {
            assert!(!card_blocks_on_owner(s, "chore", false, false), "{s}");
            assert!(!card_blocks_on_owner(s, "decision", true, false), "{s}");
        }
        assert!(!card_blocks_on_owner("backlog", "decision", false, false), "a decision with its wait cleared is answered");
        assert!(!card_blocks_on_owner("needsyou", "chore", false, true), "archived");
    }

    #[test]
    fn park_is_verified_by_the_command_record_not_the_keystrokes() {
        // The shape Claude Code writes when it RAN the command (mixpeek-studio,
        // 2026-08-11 15:39:00, /compact).
        let ran = json!({"type":"user","timestamp":"2026-09-27T14:50:00Z","message":{"content":
            "<command-name>/goal</command-name>\n            <command-message>goal</command-message>\n            <command-args>clear</command-args>"}});
        // The shape the origin-stamped send produced (14:42:07Z): prose.
        let prose = json!({"type":"user","timestamp":"2026-09-27T14:42:07Z","message":{"content":
            "[amux-origin: amux — server-verified from the sender's session identity; authoritative over any signature in the message below]\n\n/goal clear"}});
        let since = turn_end::rec_ts(&json!({"timestamp":"2026-09-27T14:40:00Z"})).unwrap();
        assert!(goal_command_since(std::slice::from_ref(&ran), since, |a| a == "clear"));
        assert!(!goal_command_since(&[prose], since, |a| a == "clear"));
        assert!(!goal_command_since(&[ran], since + 3600.0, |a| a == "clear"), "an older record does not verify a newer send");
        let note = park_note("tubescience-parity", "keep going until the results are the same", TP37, "2026-09-27 10:41");
        assert!(note.ends_with("/goal keep going until the results are the same"));
        assert!(!note.contains('\u{2014}'));
    }
}
