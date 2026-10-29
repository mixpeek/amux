//! Conversation recycling at card boundaries.
//!
//! Two independent triggers, same mechanism (`new_conversation` + `restart`):
//!
//! 1. **Contract rule 7** (AH-383). When the server grants a contract card
//!    done (rule 2), the lane is marked `contract_fresh_pending`. The next
//!    dispatch recycles the conversation and prefixes a brief with the card,
//!    its frozen contract, its description tail and the lane's last outcomes.
//!
//! 2. **Per-task fresh** (GH-271). When `AMUX_NEW_CONVERSATION_PER_TASK=1`
//!    is set for a lane (scoped: worker > group > global), every new card
//!    pickup starts a clean conversation. Board-drive marks the lane with
//!    `per_task_fresh_pending` after claiming a card; `at_dispatch` consumes
//!    it. Resumes and nudges do not mark, so mid-card work keeps its context.
//!
//! Never recycles a lane that is not running, or that holds a card in doing
//! other than the one being dispatched.
use crate::api::session_verbs as sv;
use crate::api::AppState;
use crate::db::board_store::{self as bs, IssueRow};
use serde_json::{json, Value};

pub const PENDING: &str = "contract_fresh_pending";
const PER_TASK_PENDING: &str = "per_task_fresh_pending";
pub const PER_TASK_KEY: &str = "AMUX_NEW_CONVERSATION_PER_TASK";
/// A boundary older than this is no longer the one that was marked.
const PENDING_MAX_S: f64 = 24.0 * 3600.0;
const DESC_TAIL: usize = 1200;
const RECENT: usize = 3;

/// Mark the lane: its next dispatched card starts a fresh session (contract rule 7).
pub fn mark_pending(lane: &str, card: &str) {
    sv::update_meta(lane, &[(PENDING, json!({"card": card, "at": crate::config::now_f64()}))]);
}

fn clear_pending(lane: &str) {
    let mut meta = sv::load_meta(lane);
    if meta.remove(PENDING).is_some() {
        sv::save_meta(lane, &meta);
    }
}

fn is_on(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes")
}

fn per_task_on(home: &std::path::Path, lane: &str) -> bool {
    sv::scoped_setting_in(home, lane, PER_TASK_KEY).as_deref().map(is_on).unwrap_or(false)
}

/// If per-task fresh is enabled for `lane`, mark it so the next dispatch
/// recycles the conversation. Called by board-drive after claiming a card.
pub fn maybe_mark_per_task(lane: &str, card: &str) {
    let home = crate::config::amux_home();
    if per_task_on(&home, lane) {
        sv::update_meta(lane, &[(PER_TASK_PENDING, json!({"card": card, "at": crate::config::now_f64()}))]);
    }
}

fn clear_per_task(lane: &str) {
    let mut meta = sv::load_meta(lane);
    if meta.remove(PER_TASK_PENDING).is_some() {
        sv::save_meta(lane, &meta);
    }
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    Fresh,
    Skip(&'static str),
}

/// Whether to recycle at this dispatch. `other_doing` counts the lane's doing
/// cards other than the one being dispatched.
pub fn decide(pending_age_s: Option<f64>, running: bool, other_doing: usize) -> Decision {
    match pending_age_s {
        None => Decision::Skip("not_pending"),
        Some(a) if a > PENDING_MAX_S => Decision::Skip("expired"),
        _ if !running => Decision::Skip("not_running"),
        _ if other_doing > 0 => Decision::Skip("card_in_doing"),
        _ => Decision::Fresh,
    }
}

fn first_line(s: &str, n: usize) -> String {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(n).collect()
}

fn tail(s: &str, n: usize) -> String {
    let t: Vec<char> = s.trim().chars().collect();
    t[t.len().saturating_sub(n)..].iter().collect()
}

/// The brief, built only from board data (contract rule 7).
pub fn brief(prev: &str, next: Option<&IssueRow>, contract: Option<&crate::api::contract::Contract>, recent: &[IssueRow]) -> String {
    let mut b = format!(
        "[amux contract rule 7] Fresh session at a card boundary: this conversation was recycled after {prev} was granted done. \
Your env, board cards and memory are unchanged.\n"
    );
    if let Some(n) = next {
        b.push_str(&format!("\nThis card: {}: {}\n", n.id, n.title));
        match contract {
            Some(c) => b.push_str(&format!(
                "Frozen contract ({} kind): acceptance: {}; verify: `{}`{}\n",
                c.kind,
                c.acceptance,
                c.command,
                c.deploy_check.as_deref().map(|d| format!("; post-deploy check: `{d}`")).unwrap_or_default()
            )),
            None => {
                b.push_str("No frozen contract yet: entering doing freezes acceptance criteria and a verify command (rule 1).\n");
                if let Some(criteria) = n.acceptance_criteria.as_deref().filter(|s| !s.trim().is_empty()) {
                    b.push_str(&format!("Acceptance criteria: {criteria}\n"));
                }
                if let Some(action) = n.next_action.as_deref().filter(|s| !s.trim().is_empty()) {
                    b.push_str(&format!("Next action: {action}\n"));
                }
            },
        }
        let d = tail(&n.desc, DESC_TAIL);
        if !d.is_empty() {
            b.push_str(&format!("Description excerpt (tail; full card: GET /api/board/{}):\n{d}\n", n.id));
        }
    }
    if !recent.is_empty() {
        b.push_str("\nYour last outcomes:\n");
        for r in recent.iter().take(RECENT) {
            b.push_str(&format!(
                "- {} ({}): {} | {}\n",
                r.id,
                r.status,
                first_line(&r.title, 100),
                first_line(r.evidence.as_deref().unwrap_or("no evidence"), 160)
            ));
        }
    }
    b.push_str("\n---\n");
    b
}

/// Per-task brief: no contract info, just the card and recent outcomes.
pub fn per_task_brief(next: Option<&IssueRow>, recent: &[IssueRow]) -> String {
    let mut b = String::from(
        "[amux] Fresh session: AMUX_NEW_CONVERSATION_PER_TASK is on for this worker, \
         so this new card starts a clean conversation. \
         Your env, board cards and memory are unchanged.\n",
    );
    if let Some(n) = next {
        b.push_str(&format!("\nThis card: {}: {}\n", n.id, n.title));
        let d = tail(&n.desc, DESC_TAIL);
        if !d.is_empty() {
            b.push_str(&format!("Description excerpt (tail; full card: GET /api/board/{}):\n{d}\n", n.id));
        }
    }
    if !recent.is_empty() {
        b.push_str("\nYour last outcomes:\n");
        for r in recent.iter().take(RECENT) {
            b.push_str(&format!(
                "- {} ({}): {} | {}\n",
                r.id,
                r.status,
                first_line(&r.title, 100),
                first_line(r.evidence.as_deref().unwrap_or("no evidence"), 160),
            ));
        }
    }
    b.push_str("\n---\n");
    b
}

/// Cards in doing on `lane` other than the one being dispatched.
async fn other_doing_count(state: &AppState, lane: &str, next: Option<&IssueRow>) -> usize {
    let lane_s = lane.to_string();
    let rows: Vec<IssueRow> = state
        .store
        .read_async(move |c| Ok(bs::list_issues(c, &["doing".into()], &[lane_s], bs::ArchivedFilter::ActiveOnly)?))
        .await
        .unwrap_or_default();
    rows.iter().filter(|r| Some(&r.id) != next.map(|n| &n.id)).count()
}

/// Per-task fresh decision: recycle only for the claim that was marked (the
/// dispatch names that card), with the same guards as rule 7. A mark left by
/// an earlier claim that never dispatched must not recycle a later card.
pub fn decide_per_task(marked_card: &str, age_s: Option<f64>, dispatch_text: &str, running: bool, other_doing: usize) -> Decision {
    if marked_card.is_empty() || !dispatch_text.contains(marked_card) {
        return Decision::Skip("mark_for_another_card");
    }
    decide(age_s, running, other_doing)
}

/// Fetch the lane's doing/done/verified rows and split into (next_card, recent_outcomes).
async fn lane_context(state: &AppState, lane: &str, text: &str) -> (Option<IssueRow>, Vec<IssueRow>) {
    let lane_s = lane.to_string();
    let rows: Vec<IssueRow> = state
        .store
        .read_async(move |c| {
            Ok(bs::list_issues(
                c,
                &["doing".into(), "done".into(), "verified".into()],
                &[lane_s],
                bs::ArchivedFilter::ActiveOnly,
            )?)
        })
        .await
        .unwrap_or_default();
    let next = rows.iter().find(|r| r.status == "doing" && text.contains(&r.id)).cloned();
    let mut recent: Vec<IssueRow> = rows.into_iter().filter(|r| r.status == "done" || r.status == "verified").collect();
    recent.sort_by_key(|r| std::cmp::Reverse(r.updated));
    recent.truncate(RECENT);
    (next, recent)
}

/// Called by board-drive just before it queues a dispatch to `lane`. Returns
/// the text to deliver: unchanged unless the lane is at a pending boundary, in
/// which case the conversation is recycled and the brief prefixes the text.
///
/// Checks per-task fresh first (fires on every new card pickup), then contract
/// rule 7 (fires only when a contract card was granted done).
pub async fn at_dispatch(state: &AppState, lane: &str, text: &str) -> String {
    let home = crate::config::amux_home();
    let meta = sv::load_meta(lane);

    // --- per-task fresh (GH-271) ---
    if let Some(pt) = meta.get(PER_TASK_PENDING).cloned() {
        let card = pt.get("card").and_then(Value::as_str).unwrap_or("-").to_string();
        let age = crate::config::now_f64() - pt.get("at").and_then(Value::as_f64).unwrap_or(0.0);
        let running = sv::is_running(lane).await;
        let (next, recent) = lane_context(state, lane, text).await;
        let other_doing = other_doing_count(state, lane, next.as_ref()).await;
        // The mark is consumed either way: it names one claim, never a later one.
        clear_per_task(lane);
        if let Decision::Skip(reason) = decide_per_task(&card, Some(age), text, running, other_doing) {
            tracing::info!(lane, card = %card, reason, measured = true, n_considered = 1,
                verdict = "per_task_fresh_skipped", "per-task fresh: skipped recycle");
        } else {
            let resp = sv::config_patch(state, lane, &json!({"new_conversation": true, "restart": true})).await;
            if resp.status().is_success() {
                // This recycle is also the card boundary rule 7 waits for;
                // without clearing it the next dispatch recycles again.
                clear_pending(lane);
                tracing::info!(lane, card = %card,
                    next = next.as_ref().map(|n| n.id.as_str()).unwrap_or("-"),
                    measured = true, n_considered = 1, verdict = "per_task_fresh_session",
                    "per-task fresh: conversation recycled for new card");
                return format!("{}{text}", per_task_brief(next.as_ref(), &recent));
            }
            tracing::warn!(lane, card = %card, reason = "recycle_refused",
                status = resp.status().as_u16(), measured = true, n_considered = 1,
                verdict = "per_task_fresh_skipped",
                "per-task fresh: recycle refused; dispatching on old conversation");
        }
    }

    // --- contract rule 7 ---
    if !crate::api::contract::rule_on(&home, lane, "7") {
        return text.to_string();
    }
    let Some(pending) = meta.get(PENDING).cloned() else {
        return text.to_string();
    };
    let prev = pending.get("card").and_then(Value::as_str).unwrap_or("the last card").to_string();
    let age = crate::config::now_f64() - pending.get("at").and_then(Value::as_f64).unwrap_or(0.0);
    let (next, recent) = lane_context(state, lane, text).await;
    let other_doing = other_doing_count(state, lane, next.as_ref()).await;
    let running = sv::is_running(lane).await;
    match decide(Some(age), running, other_doing) {
        Decision::Skip(reason) => {
            clear_pending(lane);
            tracing::info!(lane, prev = %prev, reason, measured = true, n_considered = 1,
                verdict = "contract_fresh_skipped", "card boundary kept the conversation");
            text.to_string()
        }
        Decision::Fresh => {
            let resp = sv::config_patch(state, lane, &json!({"new_conversation": true, "restart": true})).await;
            clear_pending(lane);
            if !resp.status().is_success() {
                tracing::warn!(lane, prev = %prev, reason = "recycle_refused", status = resp.status().as_u16(), measured = true,
                    n_considered = 1, verdict = "contract_fresh_skipped", "the fresh-session recycle was refused; dispatching on the old conversation");
                return text.to_string();
            }
            let contract = match &next {
                Some(n) => {
                    let id = n.id.clone();
                    state.store.read_async(move |c| Ok(crate::api::contract::load(c, &id)?)).await.ok().flatten()
                }
                None => None,
            };
            tracing::info!(lane, prev = %prev, next = next.as_ref().map(|n| n.id.as_str()).unwrap_or("-"), measured = true,
                n_considered = 1, verdict = "contract_fresh_session", "card boundary: fresh session with a harness-built brief");
            format!("{}{text}", brief(&prev, next.as_ref(), contract.as_ref(), &recent))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boundary_recycles_only_a_running_lane_with_no_other_card_in_doing() {
        assert_eq!(decide(Some(60.0), true, 0), Decision::Fresh);
        assert_eq!(decide(None, true, 0), Decision::Skip("not_pending"));
        assert_eq!(decide(Some(60.0), false, 0), Decision::Skip("not_running"));
        assert_eq!(decide(Some(60.0), true, 1), Decision::Skip("card_in_doing"), "never mid-card");
        assert_eq!(decide(Some(PENDING_MAX_S + 1.0), true, 0), Decision::Skip("expired"));
    }

    fn state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("fresh-test.db")).unwrap();
        crate::test_tempdirs::keep(dir);
        AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    /// The dispatch hook through its real entry: a lane outside the contract
    /// is untouched and keeps its mark; a contract lane that cannot be
    /// recycled (not running) gets the dispatch unchanged, and the mark is
    /// spent so the next boundary is judged afresh.
    #[tokio::test]
    async fn the_dispatch_hook_never_recycles_a_lane_it_cannot_and_spends_the_mark() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::create_dir_all(h.join("sessions")).unwrap();
        let _g = crate::api::settings::test_env::set_home(h);
        let st = state();
        std::fs::write(h.join("sessions/plain.env"), "CC_DIR=\"/tmp\"\n").unwrap();
        std::fs::write(h.join("sessions/pilot.env"), "CC_DIR=\"/tmp\"\nAMUX_CONTRACT_DONE=1\n").unwrap();
        for lane in ["plain", "pilot"] {
            mark_pending(lane, "GE1-8");
        }
        assert_eq!(at_dispatch(&st, "plain", "work GE1-9").await, "work GE1-9");
        assert!(sv::load_meta("plain").contains_key(PENDING), "outside the contract nothing is spent");
        assert_eq!(at_dispatch(&st, "pilot", "work GE1-9").await, "work GE1-9", "a stopped lane is never recycled");
        assert!(!sv::load_meta("pilot").contains_key(PENDING), "the mark is spent at the boundary");
    }

    #[test]
    fn the_brief_carries_the_card_its_contract_and_the_last_outcomes() {
        let next = IssueRow { id: "GE1-9".into(), title: "budget inherit".into(), desc: "x".repeat(2000) + "TAIL", acceptance_criteria: Some("measure original requirements".into()), next_action: Some("run all surfaces".into()), ..Default::default() };
        let c = crate::api::contract::Contract {
            card: "GE1-9".into(), acceptance: "ns inherits org budget".into(), command: "pytest server/tests/unit/test_b.py".into(),
            hash: "h".into(), state: "frozen".into(), sha: None, amended: false, kind: "code".into(), deploy_check: None,
        };
        let recent: Vec<IssueRow> = (1..=4)
            .map(|i| IssueRow { id: format!("GE1-{i}"), status: "done".into(), title: format!("t{i}"), evidence: Some(format!("ev{i}\nmore")), ..Default::default() })
            .collect();
        let b = brief("GE1-8", Some(&next), Some(&c), &recent);
        assert!(b.contains("GE1-8") && b.contains("GE1-9: budget inherit"));
        assert!(b.contains("ns inherits org budget") && b.contains("`pytest server/tests/unit/test_b.py`"));
        assert!(b.contains("TAIL") && b.len() < 2000, "desc is a tail, not the whole thing");
        assert!(b.contains("GE1-3 (done): t3 | ev3") && !b.contains("GE1-4"), "three outcomes only");
        let none = brief("GE1-8", Some(&next), None, &[]);
        assert!(none.contains("No frozen contract yet"));
        assert!(none.contains("measure original requirements") && none.contains("run all surfaces"));
        assert!(none.contains("GET /api/board/GE1-9"));
    }

    #[test]
    fn per_task_brief_includes_card_and_outcomes_without_contract() {
        let next = IssueRow {
            id: "PT-1".into(), title: "add caching".into(),
            desc: "x".repeat(2000) + "TAIL", ..Default::default()
        };
        let recent: Vec<IssueRow> = (1..=4)
            .map(|i| IssueRow {
                id: format!("PT-{i}"), status: "done".into(),
                title: format!("t{i}"), evidence: Some(format!("ev{i}")),
                ..Default::default()
            })
            .collect();
        let b = per_task_brief(Some(&next), &recent);
        assert!(b.contains("AMUX_NEW_CONVERSATION_PER_TASK"));
        assert!(b.contains("PT-1: add caching"));
        assert!(b.contains("TAIL"));
        assert!(b.contains("PT-3 (done): t3 | ev3"), "three outcomes");
        assert!(!b.contains("PT-4"), "capped at three");
        assert!(!b.contains("contract"), "no contract mention");
    }

    #[test]
    fn per_task_recycles_only_the_marked_claim_and_never_mid_card() {
        let txt = "Work card PT-7: add caching";
        assert_eq!(decide_per_task("PT-7", Some(5.0), txt, true, 0), Decision::Fresh);
        assert_eq!(decide_per_task("PT-6", Some(5.0), txt, true, 0), Decision::Skip("mark_for_another_card"));
        assert_eq!(decide_per_task("PT-7", Some(5.0), txt, true, 1), Decision::Skip("card_in_doing"));
        assert_eq!(decide_per_task("PT-7", Some(5.0), txt, false, 0), Decision::Skip("not_running"));
        assert_eq!(decide_per_task("PT-7", Some(PENDING_MAX_S + 1.0), txt, true, 0), Decision::Skip("expired"));
    }

    #[test]
    fn maybe_mark_per_task_respects_scoped_setting() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::create_dir_all(h.join("sessions")).unwrap();
        let _g = crate::api::settings::test_env::set_home(h);

        std::fs::write(h.join("sessions/off-lane.env"), "CC_DIR=\"/tmp\"\n").unwrap();
        std::fs::write(
            h.join("sessions/on-lane.env"),
            "CC_DIR=\"/tmp\"\nAMUX_NEW_CONVERSATION_PER_TASK=1\n",
        )
        .unwrap();

        maybe_mark_per_task("off-lane", "C-1");
        assert!(!sv::load_meta("off-lane").contains_key(PER_TASK_PENDING),
            "setting off: no mark");

        maybe_mark_per_task("on-lane", "C-2");
        assert!(sv::load_meta("on-lane").contains_key(PER_TASK_PENDING),
            "setting on: mark written");
    }
}
