//! Orchestration contract A2, rule 6 and A5, phase 1 (AH-382).
//!
//! docs/orchestration-contract.md. Every automated nudge into a lane already
//! passes one chokepoint, `session_verbs::steer_enqueue_precond_with_id`. This
//! module is the gate that chokepoint calls for contract lanes
//! (`AMUX_CONTRACT_DONE`), so budgets and capacity are applied once, for every
//! producer, instead of per producer:
//!
//! - A5: a lane on its provider's limit is not nudged (the text would queue
//!   behind the limit menu), and a lane whose pane is gone is not either.
//!   Both are held, never charged to the card. Host pressure already holds
//!   board dispatch (`host_guard::admit_automation("board-dispatch")`).
//! - Rule 6: each card in doing has a budget of nudges, wall time and spend,
//!   and a repeat breaker for identical nudges. When one runs out the card
//!   goes to needsyou for the lane's orchestrator, tagged `needs-split`, and
//!   nudges stop. The verdict names whether the lane was still moving the
//!   card (`active`) or not (`stall`).
//!
//! Verdicts: contract_dispatch_held, contract_budget_exhausted.
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;

pub const ACTOR: &str = "harness:runner";
pub const TAG: &str = "needs-split";

/// Per-card limits, from the lane's scoped settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    pub nudges: i64,
    pub hours: f64,
    /// 0 disables the spend limit.
    pub usd: f64,
    pub repeats: i64,
}

impl Default for Limits {
    /// Spend: about the 99th percentile of per-card cost over the 7 days to
    /// 2026-10-06 (571 cards, mean $155, p99 $1019, max $3588).
    fn default() -> Self {
        Limits { nudges: 20, hours: 24.0, usd: 1000.0, repeats: 3 }
    }
}

impl Limits {
    pub fn for_lane(home: &Path, lane: &str) -> Self {
        let d = Limits::default();
        let num = |k: &str| {
            crate::api::contract::lane_setting(home, lane, k).and_then(|v| v.trim().trim_matches('"').parse::<f64>().ok())
        };
        Limits {
            nudges: num("AMUX_CARD_MAX_NUDGES").map(|v| v as i64).unwrap_or(d.nudges),
            hours: num("AMUX_CARD_MAX_HOURS").unwrap_or(d.hours),
            usd: num("AMUX_CARD_MAX_USD").unwrap_or(d.usd),
            repeats: num("AMUX_CARD_MAX_REPEATS").map(|v| v as i64).unwrap_or(d.repeats),
        }
    }
}

/// A card's budget record.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Budget {
    pub nudges: i64,
    pub last_hash: Option<String>,
    pub repeats: i64,
}

/// What one nudge costs: the updated record, and the exhausted limit if any.
/// Pure, so every limit is testable. `entered` is when the card entered
/// doing; `spend` its lane's cost since then.
pub fn charge(prev: &Budget, hash: &str, now: f64, entered: f64, spend: f64, l: &Limits) -> (Budget, Option<&'static str>) {
    let same = prev.last_hash.as_deref() == Some(hash);
    let next = Budget {
        nudges: prev.nudges + 1,
        last_hash: Some(hash.to_string()),
        repeats: if same { prev.repeats + 1 } else { 1 },
    };
    let cause = if next.repeats > l.repeats {
        Some("repeat")
    } else if next.nudges > l.nudges {
        Some("turns")
    } else if entered > 0.0 && now - entered > l.hours * 3600.0 {
        Some("time")
    } else if l.usd > 0.0 && spend > l.usd {
        Some("spend")
    } else {
        None
    };
    (next, cause)
}

/// Text identity for the repeat breaker: digits removed, so a nudge that
/// differs only in a count or a timestamp is the same nudge.
pub fn text_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let norm: String = text.chars().filter(|c| !c.is_ascii_digit()).collect();
    format!("{:x}", Sha256::digest(norm.split_whitespace().collect::<Vec<_>>().join(" ").as_bytes()))[..16].to_string()
}

fn pane_alive(lane: &str) -> bool {
    let st = crate::backend::tmux::session_target(&format!("amux-{lane}"));
    std::process::Command::new("tmux").args(["has-session", "-t", &st]).status().is_ok_and(|s| s.success())
}

fn provider_limited(lane: &str) -> bool {
    let meta = crate::api::session_verbs::load_meta(lane);
    crate::api::session_verbs::meta_i64(&meta, "rate_limited_since") > 0
        && meta.get("rate_limited_by").and_then(|v| v.as_str()) != Some("auto-resume")
}

/// Who decides a split: the lane's `AMUX_ORCHESTRATOR`, else the owner.
fn orchestrator(home: &Path, lane: &str) -> String {
    crate::api::contract::lane_setting(home, lane, "AMUX_ORCHESTRATOR")
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(crate::api::turn_end::owner_name)
}

/// Whether the gate applies to this enqueue at all.
pub fn applies(guard: &str, stable_id: Option<&str>) -> bool {
    // Schedules are the owner's standing configuration; contract results and
    // stable-id callbacks carry outcomes the lane must receive.
    !guard.is_empty()
        && stable_id.is_none()
        && !guard.starts_with("contract-")
        && !crate::api::session_verbs::is_owner_configured_guard(guard)
}

/// The gate. `Ok` lets the nudge through; `Err` says why it was not queued.
pub async fn gate(store: &crate::db::SharedStore, lane: &str, guard: &str, text: &str) -> Result<(), &'static str> {
    let home = crate::config::amux_home();
    if !crate::api::contract::enabled_for(&home, lane) {
        return Ok(());
    }
    for (held, why, reason) in [
        (provider_limited(lane), "provider_limit", "held: the lane is on its provider's usage limit (contract A5)"),
        (!pane_alive(lane), "crash", "held: the lane's pane is gone (contract A5)"),
    ] {
        if held {
            tracing::info!(session = lane, guard, cause = why, measured = true, n_considered = 1,
                verdict = "contract_dispatch_held", "a nudge was held for capacity, not charged to the card");
            return Err(reason);
        }
    }
    let limits = Limits::for_lane(&home, lane);
    let (lane_s, hash, owner_lane) = (lane.to_string(), text_hash(text), orchestrator(&home, lane));
    let now = crate::config::now_f64();
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let slot_w = slot.clone();
    let _ = store.write_async(move |conn| {
        let r = charge_card(conn, &lane_s, &hash, now, &limits, &owner_lane)?;
        let applied = r.is_some();
        *slot_w.lock().unwrap_or_else(|e| e.into_inner()) = r;
        Ok(crate::db::WriteOutcome { applied, events: vec![] })
    }).await;
    let charged = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    match charged {
        Some(Charged::Exhausted { card, cause, activity }) => {
            tracing::warn!(session = lane, card, cause, activity, guard, measured = true, n_considered = 1,
                verdict = "contract_budget_exhausted", "a card's budget ran out; it went to the orchestrator as needs-split");
            Err("the lane's card ran out of budget and went to its orchestrator to split (contract rule 6)")
        }
        Some(Charged::Spent) => Err("the lane's card is over budget, waiting on its orchestrator (contract rule 6)"),
        None => Ok(()),
    }
}

#[derive(Debug, PartialEq)]
pub enum Charged {
    Exhausted { card: String, cause: &'static str, activity: &'static str },
    /// Already exhausted earlier: nothing more is queued.
    Spent,
}

/// Charge one nudge to the lane's card in doing. `None` when the lane has no
/// card in doing (dispatch nudges are not charged) or the budget held.
pub fn charge_card(conn: &Connection, lane: &str, hash: &str, now: f64, l: &Limits, owner_lane: &str) -> rusqlite::Result<Option<Charged>> {
    // Created here, not by a numbered migration: the table is this module's
    // own working state, and dense migration versions are contended by
    // parallel contract work.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS card_budgets (card TEXT PRIMARY KEY, lane TEXT NOT NULL, started_at REAL NOT NULL,
         nudges INTEGER NOT NULL DEFAULT 0, last_hash TEXT, repeats INTEGER NOT NULL DEFAULT 0, exhausted_at REAL)",
    )?;
    let card: Option<(String, f64)> = conn.query_row(
        "SELECT id, COALESCE(entered_state_at, updated, 0) FROM issues WHERE session = ?1 AND status = 'doing'
         AND deleted IS NULL AND COALESCE(archived, 0) = 0 ORDER BY updated DESC LIMIT 1",
        [lane], |r| Ok((r.get(0)?, r.get::<_, Option<f64>>(1)?.unwrap_or(0.0)))).optional()?;
    let Some((card, entered)) = card else { return Ok(None) };
    // A card that re-entered doing (after a split or re-scope) starts a fresh budget.
    let prev: Option<(Budget, Option<f64>)> = conn.query_row(
        "SELECT nudges, last_hash, repeats, exhausted_at FROM card_budgets WHERE card = ?1 AND started_at = ?2",
        rusqlite::params![card, entered], |r| Ok((Budget { nudges: r.get(0)?, last_hash: r.get(1)?, repeats: r.get(2)? }, r.get(3)?))).optional()?;
    if prev.as_ref().is_some_and(|(_, ex)| ex.is_some()) {
        return Ok(Some(Charged::Spent));
    }
    let spend: f64 = conn.query_row(
        "SELECT COALESCE(SUM(cost_usd), 0) FROM token_ledger WHERE session = ?1 AND ts >= ?2",
        rusqlite::params![lane, entered as i64], |r| r.get(0)).unwrap_or(0.0);
    let (next, cause) = charge(&prev.map(|p| p.0).unwrap_or_default(), hash, now, entered, spend, l);
    conn.execute(
        "INSERT INTO card_budgets (card, lane, started_at, nudges, last_hash, repeats, exhausted_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(card) DO UPDATE SET started_at = ?3, nudges = ?4, last_hash = ?5, repeats = ?6, exhausted_at = ?7",
        rusqlite::params![card, lane, entered, next.nudges, next.last_hash, next.repeats, cause.map(|_| now)],
    )?;
    let Some(cause) = cause else { return Ok(None) };
    // Was the lane still moving the card? Any board write on it in the last hour.
    let recent: i64 = conn.query_row(
        "SELECT COUNT(*) FROM board_change_log WHERE row_id = ?1 AND changed_at >= ?2",
        rusqlite::params![card, now - 3600.0], |r| r.get(0)).unwrap_or(0);
    let activity = if recent > 0 { "active" } else { "stall" };
    if let Some(mut row) = crate::db::board_store::get_issue(conn, &card)? {
        let from = row.status.clone();
        row.ask_actor = Some(owner_lane.to_string());
        row.ask_type = Some("decision".into());
        row.ask_question = Some(format!(
            "{card} ran out of its {cause} budget ({}, lane {activity}): split it, re-scope it, or raise its budget?",
            match cause {
                "repeat" => format!("{} identical nudges in a row", next.repeats),
                "turns" => format!("{} nudges, limit {}", next.nudges, l.nudges),
                "time" => format!("{:.1} h in doing, limit {} h", (now - entered) / 3600.0, l.hours),
                _ => format!("${spend:.0} spent, limit ${}", l.usd),
            }
        ));
        row.ask_unblocks = Some("The orchestrator's split or re-scope; the lane resumes on the new cards.".into());
        row.desc.push_str(&format!("\nContract rule 6: {cause} budget exhausted (lane {activity}); sent to {owner_lane} as needs-split."));
        crate::db::board_store::save_patched(conn, &mut row)?;
        conn.execute("INSERT OR IGNORE INTO issue_tags (issue_id, tag, added_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![card, TAG, now as i64])?;
        let opts = crate::db::advance::AdvanceOpts {
            expected_from: Some(from),
            gate_ack: true,
            skip_continuation: true,
            reason: Some(format!("contract rule 6: {cause} budget exhausted")),
            ..Default::default()
        };
        if let Ok(Err(why)) = crate::db::advance::advance(conn, &card, "needsyou", ACTOR, &opts) {
            tracing::warn!(card, ?why, measured = true, n_considered = 1, verdict = "contract_budget_exhausted",
                "the exhausted card could not be moved to needsyou; it stays tagged needs-split");
        }
    }
    Ok(Some(Charged::Exhausted { card, cause, activity }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_limit_exhausts_on_its_own() {
        let l = Limits { nudges: 3, hours: 1.0, usd: 10.0, repeats: 2 };
        let b = |n, h: &str, r| Budget { nudges: n, last_hash: Some(h.into()), repeats: r };
        assert_eq!(charge(&Budget::default(), "a", 100.0, 50.0, 0.0, &l).1, None, "a fresh card passes");
        assert_eq!(charge(&b(1, "a", 2), "a", 100.0, 50.0, 0.0, &l).1, Some("repeat"), "a third identical nudge breaks");
        assert_eq!(charge(&b(1, "a", 2), "b", 100.0, 50.0, 0.0, &l).1, None, "a different nudge resets the streak");
        assert_eq!(charge(&b(3, "a", 1), "b", 100.0, 50.0, 0.0, &l).1, Some("turns"));
        assert_eq!(charge(&b(0, "a", 0), "b", 4000.0, 100.0, 0.0, &l).1, Some("time"));
        assert_eq!(charge(&b(0, "a", 0), "b", 100.0, 50.0, 11.0, &l).1, Some("spend"));
        let off = Limits { usd: 0.0, ..l };
        assert_eq!(charge(&b(0, "a", 0), "b", 100.0, 50.0, 1e9, &off).1, None, "usd 0 disables spend");
    }

    #[test]
    fn a_nudge_that_differs_only_in_numbers_is_the_same_nudge() {
        assert_eq!(text_hash("3 cards waiting, 12:01"), text_hash("4 cards  waiting, 12:07"));
        assert_ne!(text_hash("pick up GE2-5"), text_hash("verify GE2-5"));
    }

    #[test]
    fn schedules_callbacks_and_contract_results_are_never_gated() {
        assert!(applies("board-drive", None));
        assert!(!applies("", None), "the owner's own send");
        assert!(!applies("board-drive", Some("board-blocker:X")), "stable-id callbacks");
        assert!(!applies("contract-verify", None));
    }

    /// Through the shipped chokepoint: a contract lane with no live pane is
    /// held (A5), the same lane without the switch is queued as before, and
    /// the owner's own send is never gated.
    #[tokio::test]
    async fn the_one_enqueue_path_applies_the_gate_to_contract_lanes_only() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        let _g = crate::api::settings::test_env::set_home(home.path());
        let store: crate::db::SharedStore = std::sync::Arc::new(crate::db::Store::open(&home.path().join("q.db")).unwrap());
        let lane = "runner-gate-test-lane-no-pane";
        std::fs::write(home.path().join(format!("sessions/{lane}.env")), "AMUX_CONTRACT_DONE=1\n").unwrap();
        let held = crate::api::session_verbs::steer_enqueue_store(&store, lane, "keep going", "board-drive", "amux").await;
        assert!(held.as_ref().is_err_and(|e| e.contains("pane is gone")), "{held:?}");
        let owner = crate::api::session_verbs::steer_enqueue_store(&store, lane, "hi", "", "").await;
        assert!(!owner.as_ref().is_err_and(|e| e.contains("contract")), "the owner's send is not gated: {owner:?}");
        std::fs::write(home.path().join(format!("sessions/{lane}.env")), "AMUX_CONTRACT_DONE=0\n").unwrap();
        let plain = crate::api::session_verbs::steer_enqueue_store(&store, lane, "keep going", "board-drive", "amux").await;
        assert!(!plain.as_ref().is_err_and(|e| e.contains("contract")), "switch off: today's behavior: {plain:?}");
    }

    #[test]
    fn an_exhausted_card_goes_to_the_orchestrator_tagged_needs_split() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runner.db");
        let _store = crate::db::Store::open(&path).unwrap(); // runs the shipped migrations
        let conn = Connection::open(&path).unwrap();
        let mk = |status: &str| {
            crate::db::board_store::create_issue(&conn, &crate::db::board_store::NewIssue {
                acceptance_criteria: None, next_action: None, title: "card".into(), desc: "d".into(),
                status: status.into(), session: Some("lane-r".into()), item_type: "chore".into(),
                creator: "test".into(), owner_type: "agent".into(), due: None, due_time: None, reviewer: None,
                shepherd: None, gate: vec![], depends_on: vec![], tags: vec![], ask_type: None, ask_question: None,
                ask_unblocks: None, ask_actor: None, source: Some("test".into()), requested_by: None,
                callback_session: None, callback_prompt: None,
            }, 1_700_000_000).unwrap().id
        };
        let l = Limits { nudges: 2, hours: 1e6, usd: 0.0, repeats: 9 };
        assert_eq!(charge_card(&conn, "lane-r", "h1", 10.0, &l, "orch").unwrap(), None, "no card in doing: dispatch is free");
        let id = mk("doing");
        assert_eq!(charge_card(&conn, "lane-r", "h1", 10.0, &l, "orch").unwrap(), None);
        assert_eq!(charge_card(&conn, "lane-r", "h2", 11.0, &l, "orch").unwrap(), None);
        match charge_card(&conn, "lane-r", "h3", 12.0, &l, "orch").unwrap() {
            Some(Charged::Exhausted { card, cause, .. }) => assert_eq!((card.as_str(), cause), (id.as_str(), "turns")),
            other => panic!("the third nudge exhausts a 2-nudge budget: {other:?}"),
        }
        let row = crate::db::board_store::get_issue(&conn, &id).unwrap().unwrap();
        assert_eq!(row.status, "needsyou");
        assert_eq!(row.ask_actor.as_deref(), Some("orch"));
        let tagged: i64 = conn.query_row("SELECT COUNT(*) FROM issue_tags WHERE issue_id = ?1 AND tag = ?2",
            rusqlite::params![id, TAG], |r| r.get(0)).unwrap();
        assert_eq!(tagged, 1);
        assert_eq!(charge_card(&conn, "lane-r", "h4", 13.0, &l, "orch").unwrap(), None, "needsyou is not doing: nothing charged");
        conn.execute("UPDATE issues SET status = 'doing', entered_state_at = 99 WHERE id = ?1", [&id]).unwrap();
        assert_eq!(charge_card(&conn, "lane-r", "h5", 100.0, &l, "orch").unwrap(), None, "back in doing: a fresh budget");
    }
}
