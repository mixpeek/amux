//! Orchestration contract rule 7, phase 1 (AH-383).
//!
//! docs/orchestration-contract.md: a card boundary is a fresh session seeded
//! by a harness-built brief; mid-card compaction stays with the provider.
//!
//! When the server grants a contract card done (rule 2), the lane is marked
//! `contract_fresh_pending`. The next time board-drive delivers a card to that
//! lane, which it does only at an idle boundary, the conversation is recycled
//! through the same path as `amux fresh` (config `new_conversation` +
//! `restart`), and the dispatch text is prefixed with a brief built from the
//! board: the card, its frozen contract, its description tail and the lane's
//! last three outcomes. No model call.
//!
//! Never recycles a lane that is not running, or that holds a card in doing
//! other than the one being dispatched. Verdicts (rule 14): contract_fresh_session,
//! contract_fresh_skipped (with a reason).
use crate::api::session_verbs as sv;
use crate::api::AppState;
use crate::db::board_store::{self as bs, IssueRow};
use serde_json::{json, Value};

pub const PENDING: &str = "contract_fresh_pending";
/// A boundary older than this is no longer the one that was marked.
const PENDING_MAX_S: f64 = 24.0 * 3600.0;
const DESC_TAIL: usize = 1200;
const RECENT: usize = 3;

/// Mark the lane: its next dispatched card starts a fresh session.
pub fn mark_pending(lane: &str, card: &str) {
    sv::update_meta(lane, &[(PENDING, json!({"card": card, "at": crate::config::now_f64()}))]);
}

fn clear_pending(lane: &str) {
    let mut meta = sv::load_meta(lane);
    if meta.remove(PENDING).is_some() {
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

/// The brief, built only from board data.
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
            None => b.push_str("No frozen contract yet: entering doing freezes acceptance criteria and a verify command (rule 1).\n"),
        }
        let d = tail(&n.desc, DESC_TAIL);
        if !d.is_empty() {
            b.push_str(&format!("Description (tail):\n{d}\n"));
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

/// Called by board-drive just before it queues a dispatch to `lane`. Returns
/// the text to deliver: unchanged unless the lane is at a pending boundary, in
/// which case the conversation is recycled and the brief prefixes the text.
pub async fn at_dispatch(state: &AppState, lane: &str, text: &str) -> String {
    let home = crate::config::amux_home();
    if !crate::api::contract::enabled_for(&home, lane) {
        return text.to_string();
    }
    let meta = sv::load_meta(lane);
    let Some(pending) = meta.get(PENDING).cloned() else {
        return text.to_string();
    };
    let prev = pending.get("card").and_then(Value::as_str).unwrap_or("the last card").to_string();
    let age = crate::config::now_f64() - pending.get("at").and_then(Value::as_f64).unwrap_or(0.0);
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
    // The dispatched card is the doing card the dispatch text names.
    let next = rows.iter().find(|r| r.status == "doing" && text.contains(&r.id)).cloned();
    let other_doing = rows.iter().filter(|r| r.status == "doing" && Some(&r.id) != next.as_ref().map(|n| &n.id)).count();
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
            let mut recent: Vec<IssueRow> = rows.into_iter().filter(|r| r.status == "done" || r.status == "verified").collect();
            recent.sort_by_key(|r| std::cmp::Reverse(r.updated));
            recent.truncate(RECENT);
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
        std::mem::forget(dir);
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
        let next = IssueRow { id: "GE1-9".into(), title: "budget inherit".into(), desc: "x".repeat(2000) + "TAIL", ..Default::default() };
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
    }
}
