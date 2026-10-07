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
//! - A2 phase 2: a lane with nothing todo or doing pulls the next ready card
//!   its hub (AMUX_CONTRACT_HUB) tagged `pool`, so work the orchestrator has
//!   released does not wait for it to hand each card out.
//!
//! Verdicts: contract_dispatch_held, contract_budget_exhausted, a2_pool_assigned,
//! a2_pool_empty, a2_pool_held, a2_pool_skipped_parked.
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
    if !crate::api::contract::rule_on(&home, lane, "6") {
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

// ---------------------------------------------------------------------------
// A2 phase 2: idle lanes pull ready work from the hub's pool
// ---------------------------------------------------------------------------

/// The tag a hub puts on a card its lanes may take without being assigned.
pub const POOL_TAG: &str = "pool";
pub const POOL_ACTOR: &str = "harness:a2";

/// What one pool pull did for an idle lane.
#[derive(Debug, PartialEq)]
pub enum PoolPull {
    /// The rule is off for the lane, it has no hub, or it already has work.
    NotApplicable,
    /// Capacity held it (contract A5).
    Held(&'static str),
    /// Nothing ready in the hub's pool.
    Empty { pool: usize },
    /// This card moved to the lane's todo: from the hub's board, or from the
    /// lane's own backlog.
    Assigned { card: String, hub: String },
}

/// A lane's pool title rule (AMUX_A2_POOL_TITLE_REGEX): a card whose title
/// matches is pool work without a tag. AH-395, 2026-10-07: eight gs12 lanes
/// idle while ~66 proof cards waited untagged in the orchestrator's backlog.
pub fn pool_title_re(home: &std::path::Path, lane: &str) -> Option<regex::Regex> {
    crate::api::contract::lane_setting(home, lane, "AMUX_A2_POOL_TITLE_REGEX")
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
        .and_then(|v| regex::Regex::new(&v).ok())
}

/// A card the harness reviewer sent back: a failed or escalated review on
/// record, or the reviewer's note in its description.
fn reopened_by_review(conn: &Connection, id: &str, desc: &str) -> rusqlite::Result<bool> {
    let state: Option<String> = conn
        .query_row("SELECT review_state FROM card_contracts WHERE card = ?1", [id], |r| r.get::<_, Option<String>>(0))
        .optional()?
        .flatten();
    Ok(matches!(state.as_deref(), Some("failed") | Some("escalated"))
        || (desc.contains("Review round") && desc.contains("failed (contract rule 3")))
}

/// How long a card its lane parked back to backlog stays out of the pool
/// (AMUX_A2_REPARK_COOLDOWN_S, default 4 h).
fn repark_cooldown_s() -> i64 {
    std::env::var("AMUX_A2_REPARK_COOLDOWN_S").ok().and_then(|v| v.trim().parse::<i64>().ok())
        .or_else(|| crate::config::parse_env_file(&crate::config::amux_home().join("server.env"))
            .get("AMUX_A2_REPARK_COOLDOWN_S").and_then(|v| v.trim().trim_matches('"').parse::<i64>().ok()))
        .unwrap_or(4 * 3600)
        .max(0)
}

/// The lane itself moved this card to backlog within the cooldown, and none
/// of its dependencies changed status since (2026-10-07: gs12-tiering parked
/// MO-4071 waiting on another lane's run and A2 pulled it back three times in
/// three minutes). The lane's own move is read from the card's log line
/// "`HH:MM` <lane>: <from> -> backlog", the board's transition record.
fn reparked_by_lane(conn: &Connection, owner: &str, entered: i64, log: &str, deps: &str, now: i64) -> rusqlite::Result<bool> {
    if entered <= 0 || now - entered >= repark_cooldown_s() {
        return Ok(false);
    }
    let last_park = log.lines().rev().find(|l| l.contains("-> backlog"));
    let by_lane = last_park.is_some_and(|l| {
        let body = l.split_once("` ").map(|(_, b)| b).unwrap_or(l);
        body.starts_with(&format!("{owner}:"))
    });
    if !by_lane {
        return Ok(false);
    }
    let ids: Vec<String> = serde_json::from_str(deps).unwrap_or_default();
    for id in ids {
        let changed: Option<i64> = conn
            .query_row("SELECT entered_state_at FROM issues WHERE id = ?1", [&id], |r| r.get::<_, Option<i64>>(0))
            .optional()?
            .flatten();
        if changed.is_some_and(|t| t > entered) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Readiness for the pool: exactly board-drive's auto-park predicate
/// (`board_store::dependency_resolved`, which `deps_blocking` uses), so a card
/// the pool hands out is never one auto-park would park again.
fn deps_resolved(conn: &Connection, depends_on: &str) -> rusqlite::Result<bool> {
    let ids: Vec<String> = serde_json::from_str(depends_on).unwrap_or_default();
    for id in ids {
        if !crate::db::board_store::dependency_resolved(conn, &id)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// One pool candidate: id, status, ready (every dependency resolved, by the
/// same predicate readiness and promotion use), reopened by review.
pub type PoolCard = (String, String, bool, bool);

/// Pool cards on `owner`'s board in todo or backlog: tagged `pool` (when
/// `tag_ok`), or titled to match `title_re`. Reopened-by-review cards first,
/// then board order.
fn candidates(conn: &Connection, owner: &str, title_re: Option<&regex::Regex>, tag_ok: bool) -> rusqlite::Result<Vec<PoolCard>> {
    type Row = (String, String, String, String, String, bool, bool, i64, String);
    // A PARKED backlog card is not ready (gs12-tiering MO-4071, gs12-obs
    // MO-4086, gs12-cicd MO-4438, 2026-10-07): `amux board backlog --trigger`
    // stores the wait in source_ref and stamps last_verified_at, and A2 pulled
    // the card straight back into pickup a minute later, looping between
    // pickup and backlog while the other lane's run was still queued. Same
    // predicate as the backlog drain (board_drive::drain_backlog): a trigger
    // confirmed within SOURCE_REF_STALE_S, or a blocked_on, holds the card.
    let verified_cut = crate::config::now_f64() as i64 - crate::runtime_jobs::board_drive::SOURCE_REF_STALE_S;
    let rows: Vec<Row> = conn
        .prepare(
            "SELECT i.id, i.status, COALESCE(i.depends_on, '[]'), COALESCE(i.title, ''), COALESCE(i.desc, ''), \
                    EXISTS(SELECT 1 FROM issue_tags t WHERE t.issue_id = i.id AND t.tag = ?2), \
                    (i.status = 'backlog' AND ((COALESCE(i.source_ref, '') <> '' AND COALESCE(i.last_verified_at, 0) > ?3) \
                                               OR COALESCE(i.blocked_on, '') <> '')), \
                    COALESCE(i.entered_state_at, 0), COALESCE(i.log, '') \
             FROM issues i \
             WHERE i.session = ?1 AND i.status IN ('todo', 'backlog') \
               AND i.deleted IS NULL AND COALESCE(i.archived, 0) = 0 \
             ORDER BY i.pos, i.created, i.id",
        )?
        .query_map(rusqlite::params![owner, POOL_TAG, verified_cut], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    let now = crate::config::now_f64() as i64;
    for (id, status, deps, title, desc, tagged, parked, entered, log) in rows {
        let by_title = title_re.is_some_and(|re| re.is_match(&title));
        if !((tag_ok && tagged) || by_title) {
            continue;
        }
        let parked = parked || (status == "backlog" && reparked_by_lane(conn, owner, entered, &log, &deps, now)?);
        // Once per card per 10 minutes, so the counter reads cards, not passes.
        if parked && first_this_window(&format!("parked:{id}")) {
            tracing::info!(card = %id, owner, measured = true, n_considered = 1, verdict = "a2_pool_skipped_parked",
                "A2 leaves a backlog card its lane parked (fresh trigger, blocker, or its own move within the cooldown) where it is");
        }
        let ready = !parked && deps_resolved(conn, &deps)?;
        let reopened = reopened_by_review(conn, &id, &desc)?;
        out.push((id, status, ready, reopened));
    }
    // Stable sort: reopened first, board order within each group.
    out.sort_by_key(|c| !c.3);
    Ok(out)
}

/// The hub's pool cards (tagged, or titled by the rule), reopened first.
pub fn pool_cards(conn: &Connection, hub: &str, title_re: Option<&regex::Regex>) -> rusqlite::Result<Vec<PoolCard>> {
    candidates(conn, hub, title_re, true)
}

fn lane_has_work(conn: &Connection, lane: &str) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM issues WHERE session = ?1 AND status IN ('todo', 'doing') \
         AND deleted IS NULL AND COALESCE(archived, 0) = 0",
        [lane],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// What a claim found, inside one write transaction.
#[derive(Debug, PartialEq)]
pub enum Claim {
    Busy,
    Empty(usize),
    /// card, source ("own" or "hub"), reopened by review
    Got(String, &'static str, bool),
    /// No proof card was ready; this card unblocks `count` open proof cards.
    Unblocks(String, &'static str, usize),
    /// Like Unblocks, but the card moved from the hub together with its
    /// dependency group: (card, count, group size).
    UnblocksGroup(String, usize, usize),
}

/// Largest dependency group A2 moves as one unit (AMUX_A2_GROUP_MAX).
const GROUP_MAX_DEFAULT: usize = 8;

fn group_max() -> usize {
    crate::api::contract::lane_setting(&crate::config::amux_home(), "", "AMUX_A2_GROUP_MAX")
        .or_else(|| std::env::var("AMUX_A2_GROUP_MAX").ok())
        .and_then(|v| v.trim().trim_matches('"').parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(GROUP_MAX_DEFAULT)
}

/// What a group move did.
#[derive(Debug, PartialEq)]
pub enum GroupTake {
    Moved(usize),
    TooLarge(usize),
    /// A member is in doing, archived, or tied to another board; or the
    /// board refused the batch.
    Refused(String),
}

/// The card's dependency component on the hub's board: every card linked to
/// it by depends_on in either direction, transitively. The board requires
/// every edge to stay on one board, done and verified targets included, so
/// this whole component is the smallest set that may move. Stops once it
/// exceeds `cap` (returns cap+1 members) and reports a member on another
/// board as Err(that card).
fn dependency_component(conn: &Connection, hub: &str, root: &str, cap: usize) -> rusqlite::Result<Result<Vec<String>, String>> {
    let mut seen: Vec<String> = vec![root.to_string()];
    let mut queue = vec![root.to_string()];
    while let Some(id) = queue.pop() {
        let deps: String = conn
            .query_row("SELECT COALESCE(depends_on, '[]') FROM issues WHERE id = ?1 AND deleted IS NULL", [&id], |r| r.get(0))
            .optional()?
            .unwrap_or_else(|| "[]".into());
        let mut next: Vec<String> = serde_json::from_str::<Vec<String>>(&deps).unwrap_or_default();
        let dependents: Vec<String> = conn
            .prepare("SELECT i.id FROM issues i, json_each(CASE WHEN json_valid(i.depends_on) THEN i.depends_on ELSE '[]' END) d \
                      WHERE i.deleted IS NULL AND d.value = ?1")?
            .query_map([&id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        next.extend(dependents);
        for n in next {
            if seen.contains(&n) {
                continue;
            }
            let owner: Option<(Option<String>, Option<String>)> = conn
                .query_row("SELECT project_group, session FROM issues WHERE id = ?1 AND deleted IS NULL", [&n],
                    |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            match owner {
                Some((None, Some(s))) if s == hub => {}
                _ => return Ok(Err(n)),
            }
            seen.push(n.clone());
            if seen.len() > cap {
                return Ok(Ok(seen));
            }
            queue.push(n);
        }
    }
    Ok(Ok(seen))
}

/// Move a blocking hub card and its dependency component to `lane` as one
/// unit (Ethan, 2026-10-07: the blocking plan items and the proof cards that
/// depend on them all sat on the orchestrator's board, and the same-board
/// rule refused moving any one of them alone). All or nothing in one
/// savepoint: ownership of every member changes together (validated as a
/// batch), the blocking card goes to todo, the rest keep their status, and
/// each gets a log line naming the group.
pub fn take_group(conn: &Connection, hub: &str, root: &str, root_status: String, lane: &str, line: &str, cap: usize) -> rusqlite::Result<GroupTake> {
    let members = match dependency_component(conn, hub, root, cap)? {
        Err(other) => return Ok(GroupTake::Refused(format!("{other} is on another board"))),
        Ok(m) if m.len() > cap => return Ok(GroupTake::TooLarge(m.len())),
        Ok(m) => m,
    };
    for m in &members {
        let (status, archived): (String, i64) = conn.query_row(
            "SELECT status, COALESCE(archived, 0) FROM issues WHERE id = ?1", [m], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if archived != 0 || (status == "doing" && m != root) {
            return Ok(GroupTake::Refused(format!("{m} is {}", if archived != 0 { "archived".to_string() } else { status })));
        }
    }
    conn.execute_batch("SAVEPOINT a2_group")?;
    let res = (|| -> rusqlite::Result<bool> {
        let owner = crate::db::board_store::BoardOwner::new(None, Some(lane));
        let changes: Vec<(String, crate::db::board_store::BoardOwner)> = members.iter().map(|m| (m.clone(), owner.clone())).collect();
        crate::db::board_store::validate_owner_changes(conn, &changes)?;
        let stamp = chrono::Local::now().format("%H:%M");
        let names = members.join(", ");
        for m in &members {
            let note = format!("\n`{stamp}` {POOL_ACTOR}: moved to {lane} with its dependency group [{names}] ({line})");
            conn.execute(
                "UPDATE issues SET session = ?2, log = COALESCE(log, '') || ?3, rev = rev + 1, version = version + 1 WHERE id = ?1",
                rusqlite::params![m, lane, note])?;
            conn.execute("DELETE FROM issue_tags WHERE issue_id = ?1 AND tag = ?2", rusqlite::params![m, POOL_TAG])?;
        }
        if root_status == "todo" {
            return Ok(true);
        }
        take_inner(conn, root, root_status, lane, line.to_string())
    })();
    match res {
        Ok(true) => {
            conn.execute_batch("RELEASE a2_group")?;
            Ok(GroupTake::Moved(members.len()))
        }
        Ok(false) => {
            conn.execute_batch("ROLLBACK TO a2_group; RELEASE a2_group")?;
            Ok(GroupTake::Refused(format!("{root} did not move to todo")))
        }
        Err(e) => {
            conn.execute_batch("ROLLBACK TO a2_group; RELEASE a2_group")?;
            Ok(GroupTake::Refused(e.to_string()))
        }
    }
}

/// For each card, how many open proof cards (title matches `title_re`, not
/// terminal) it blocks directly or through one more dependency hop.
fn proof_unblock_counts(conn: &Connection, title_re: &regex::Regex) -> rusqlite::Result<std::collections::HashMap<String, usize>> {
    let rows: Vec<(String, String)> = conn
        .prepare("SELECT COALESCE(title, ''), COALESCE(depends_on, '[]') FROM issues \
                  WHERE status NOT IN ('done', 'verified', 'discarded') AND deleted IS NULL AND COALESCE(archived, 0) = 0 \
                  AND COALESCE(depends_on, '') NOT IN ('', '[]')")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut counts: std::collections::HashMap<String, usize> = Default::default();
    for (title, deps) in rows {
        if !title_re.is_match(&title) {
            continue;
        }
        let mut reach: std::collections::HashSet<String> = Default::default();
        for d in serde_json::from_str::<Vec<String>>(&deps).unwrap_or_default() {
            let second: String = conn
                .query_row("SELECT COALESCE(depends_on, '[]') FROM issues WHERE id = ?1", [&d], |r| r.get(0))
                .optional()?
                .unwrap_or_default();
            for d2 in serde_json::from_str::<Vec<String>>(&second).unwrap_or_default() {
                reach.insert(d2);
            }
            reach.insert(d);
        }
        for d in reach {
            *counts.entry(d).or_insert(0) += 1;
        }
    }
    Ok(counts)
}

/// When no proof card is ready: the ready non-proof card, on the lane's own
/// board first and then the hub's, that unblocks the most open proof cards
/// (directly or one hop down), oldest first among equals. Returns
/// (card, status, source, count).
fn unblockers(conn: &Connection, hub: &str, lane: &str, title_re: &regex::Regex) -> rusqlite::Result<Vec<(String, String, &'static str, usize)>> {
    let counts = proof_unblock_counts(conn, title_re)?;
    let mut out = Vec::new();
    if counts.is_empty() {
        return Ok(out);
    }
    let any = regex::Regex::new(".?").expect("static regex");
    for (owner, source) in [(lane, "own"), (hub, "hub")] {
        let mut group: Vec<(String, String, &'static str, usize)> = Vec::new();
        for (id, status, ready, _) in candidates(conn, owner, Some(&any), false)? {
            let title: String = conn.query_row("SELECT COALESCE(title, '') FROM issues WHERE id = ?1", [&id], |r| r.get(0))?;
            if !ready || title_re.is_match(&title) {
                continue;
            }
            let n = counts.get(&id).copied().unwrap_or(0);
            if n > 0 {
                group.push((id, status, source, n));
            }
        }
        // Most proof cards unblocked first; board order (oldest) among equals.
        group.sort_by_key(|c| std::cmp::Reverse(c.3));
        out.extend(group);
    }
    Ok(out)
}

/// Move `card` to `lane`'s todo from `status`, logged as harness:a2.
///
/// A board refusal (e.g. cross_board_dependency_forbidden: other cards on the
/// hub's board depend on this one) skips the card instead of failing the whole
/// claim: the move runs inside a savepoint that is rolled back on error, and
/// the refusal is logged. Before this a refused move aborted the write and the
/// pull read as an empty pool (2026-10-07).
fn take(conn: &Connection, card: &str, status: String, lane: &str, line: String) -> rusqlite::Result<bool> {
    conn.execute_batch("SAVEPOINT a2_take")?;
    match take_inner(conn, card, status, lane, line) {
        Ok(moved) => {
            conn.execute_batch("RELEASE a2_take")?;
            Ok(moved)
        }
        Err(e) => {
            conn.execute_batch("ROLLBACK TO a2_take; RELEASE a2_take")?;
            if first_this_window(&format!("refused:{card}")) {
                tracing::info!(card, lane, error = %e, measured = true, n_considered = 1, verdict = "a2_pool_move_refused",
                    "the board refused an A2 move; the card stays where it is and the pull tries the next one");
            }
            Ok(false)
        }
    }
}

fn take_inner(conn: &Connection, card: &str, status: String, lane: &str, line: String) -> rusqlite::Result<bool> {
    if status == "todo" {
        let Some(mut row) = crate::db::board_store::get_issue(conn, card)? else {
            return Ok(false);
        };
        row.session = Some(lane.to_string());
        let stamp = chrono::Local::now().format("%H:%M");
        let log = row.log.clone().unwrap_or_default();
        row.log = Some(format!("{log}\n`{stamp}` {POOL_ACTOR}: {line}"));
        crate::db::board_store::save_patched(conn, &mut row)?;
        return Ok(true);
    }
    let opts = crate::db::advance::AdvanceOpts {
        expected_from: Some(status),
        assign_to: Some(lane.to_string()),
        gate_ack: true,
        skip_continuation: true,
        log_line: Some(line.clone()),
        reason: Some(line),
        ..Default::default()
    };
    Ok(crate::db::advance::advance(conn, card, "todo", POOL_ACTOR, &opts)?.is_ok())
}

/// One atomic claim inside the caller's write transaction: if `lane` holds no
/// todo or doing card, move the first ready pool card from `hub` to it, in
/// todo, logged as harness:a2. Two lanes never get the same card: the store
/// has one writer, and the card leaves the hub's board in the same write.
pub fn claim_pool_card(conn: &Connection, hub: &str, lane: &str, title_re: Option<&regex::Regex>) -> rusqlite::Result<Claim> {
    if lane_has_work(conn, lane)? {
        return Ok(Claim::Busy);
    }
    // The lane's own backlog first: a proof card the reviewer sent back to it
    // is its to fix before anything new (AH-395).
    if title_re.is_some() {
        for (card, status, ready, reopened) in candidates(conn, lane, title_re, false)? {
            if ready
                && status == "backlog"
                && take(conn, &card, status, lane, format!("moved from {lane}'s own backlog by the idle-lane pool (contract A2)"))?
            {
                return Ok(Claim::Got(card, "own", reopened));
            }
        }
    }
    let cards = pool_cards(conn, hub, title_re)?;
    let pool = cards.len();
    let mut got = None;
    for (card, status, ready, reopened) in cards {
        if ready && take(conn, &card, status, lane, format!("pulled from {hub}'s pool by idle lane {lane} (contract A2)"))? {
            got = Some((card, reopened));
            break;
        }
    }
    let Some((card, reopened)) = got else {
        if let Some(re) = title_re {
            for (card, status, source, n) in unblockers(conn, hub, lane, re)? {
                let line = format!("taken by idle lane {lane}: no proof card is ready and this unblocks {n} open proof card(s) (contract A2)");
                if source == "hub" {
                    let cap = group_max();
                    match take_group(conn, hub, &card, status, lane, &line, cap)? {
                        GroupTake::Moved(size) => return Ok(Claim::UnblocksGroup(card, n, size)),
                        // Once per card per 10 minutes: every idle lane's
                        // pass hits the same group, which logged 2058 lines
                        // an hour on GS-12 (2026-10-07).
                        GroupTake::TooLarge(size) if first_this_window(&format!("toolarge:{card}")) => {
                            tracing::info!(card, lane, hub, size, cap, measured = true, n_considered = size, verdict = "a2_pool_group_too_large",
                                "a blocking card's dependency group is larger than AMUX_A2_GROUP_MAX; it stays on the hub");
                        }
                        GroupTake::Refused(why) if first_this_window(&format!("refused:{card}")) => {
                            tracing::info!(card, lane, hub, error = %why, measured = true, n_considered = 1, verdict = "a2_pool_move_refused",
                                "a dependency group could not move as one unit; it stays on the hub and the pull tries the next card");
                        }
                        GroupTake::TooLarge(_) | GroupTake::Refused(_) => {}
                    }
                    continue;
                }
                if take(conn, &card, status, lane, line)? {
                    conn.execute("DELETE FROM issue_tags WHERE issue_id = ?1 AND tag = ?2", rusqlite::params![card, POOL_TAG])?;
                    return Ok(Claim::Unblocks(card, source, n));
                }
            }
        }
        return Ok(Claim::Empty(pool));
    };
    // A pulled card has left the pool: it is the lane's now.
    conn.execute("DELETE FROM issue_tags WHERE issue_id = ?1 AND tag = ?2", rusqlite::params![card, POOL_TAG])?;
    Ok(Claim::Got(card, "hub", reopened))
}

/// AH-395: a proof card for a lane its done-WIP cap would otherwise park,
/// inside the caller's write. Its own ready proof card in todo first (no
/// move), then its own backlog, then the hub's pool (both moved to the
/// lane's todo, logged as harness:a2). Returns (card, source, reopened).
pub fn claim_proof_for_capped(conn: &Connection, hub: Option<&str>, lane: &str, title_re: &regex::Regex) -> rusqlite::Result<Option<(String, &'static str, bool)>> {
    for (card, status, ready, reopened) in candidates(conn, lane, Some(title_re), false)? {
        if !ready {
            continue;
        }
        if status == "todo" {
            return Ok(Some((card, "own-todo", reopened)));
        }
        if take(conn, &card, status, lane, format!("moved from {lane}'s own backlog for proof work past the done-WIP cap (contract A2)"))? {
            return Ok(Some((card, "own", reopened)));
        }
    }
    let Some(hub) = hub else { return Ok(None) };
    for (card, status, ready, reopened) in pool_cards(conn, hub, Some(title_re))? {
        if ready && take(conn, &card, status, lane, format!("pulled from {hub}'s pool for proof work past the done-WIP cap (contract A2)"))? {
            conn.execute("DELETE FROM issue_tags WHERE issue_id = ?1 AND tag = ?2", rusqlite::params![card, POOL_TAG])?;
            return Ok(Some((card, "hub", reopened)));
        }
    }
    Ok(None)
}

/// A2 for one idle lane: when the rule is on and capacity allows, pull the
/// next ready card from the lane's hub (AMUX_CONTRACT_HUB). board-drive's
/// ordinary pickup dispatches it on the next pass.
pub async fn pull_from_pool(store: &crate::db::SharedStore, lane: &str) -> PoolPull {
    let home = crate::config::amux_home();
    if !crate::api::contract::rule_on(&home, lane, "A2") {
        return PoolPull::NotApplicable;
    }
    let Some(hub) = crate::api::contract::lane_setting(&home, lane, "AMUX_CONTRACT_HUB")
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|h| !h.is_empty() && h != lane)
    else {
        return PoolPull::NotApplicable;
    };
    if provider_limited(lane) {
        tracing::info!(session = lane, hub, measured = true, n_considered = 1, verdict = "a2_pool_held",
            cause = "provider_limit", "an idle lane did not pull pool work: it is on its provider's usage limit (contract A5)");
        return PoolPull::Held("provider_limit");
    }
    let (h, l) = (hub.clone(), lane.to_string());
    let title_re = pool_title_re(&home, lane);
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let slot_w = slot.clone();
    let _ = store.write_async(move |conn| {
        let r = claim_pool_card(conn, &h, &l, title_re.as_ref())?;
        let applied = matches!(r, Claim::Got(..) | Claim::Unblocks(..) | Claim::UnblocksGroup(..));
        *slot_w.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
        Ok(crate::db::WriteOutcome { applied, events: vec![] })
    }).await;
    let got = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    match got {
        Some(Claim::Got(card, source, reopened)) => {
            tracing::info!(session = lane, hub, card, source, reopened, reason = "proof", measured = true, n_considered = 1, verdict = "a2_pool_assigned",
                "an idle lane took a ready pool card (contract A2)");
            PoolPull::Assigned { card, hub }
        }
        Some(Claim::Unblocks(card, source, count)) => {
            tracing::info!(session = lane, hub, card, source, reason = "unblocks_proof", count, measured = true, n_considered = count,
                verdict = "a2_pool_assigned", "an idle lane took the ready card that unblocks the most open proof cards (contract A2)");
            PoolPull::Assigned { card, hub }
        }
        Some(Claim::UnblocksGroup(card, count, size)) => {
            tracing::info!(session = lane, hub, card, source = "hub", reason = "unblocks_proof_group", count, group_size = size,
                measured = true, n_considered = count, verdict = "a2_pool_assigned",
                "an idle lane took a blocking card with its dependency group from the hub (contract A2)");
            PoolPull::Assigned { card, hub }
        }
        Some(Claim::Busy) => PoolPull::NotApplicable,
        Some(Claim::Empty(pool)) => {
            if first_this_window(lane) {
                tracing::info!(session = lane, hub, pool, measured = true, n_considered = pool, verdict = "a2_pool_empty",
                    "an idle lane found nothing ready in its hub's pool (contract A2)");
            }
            PoolPull::Empty { pool }
        }
        None => PoolPull::Empty { pool: 0 },
    }
}

/// a2_pool_empty once per lane per 10 minutes, not every board-drive pass.
fn first_this_window(lane: &str) -> bool {
    static LAST: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, f64>>> =
        std::sync::LazyLock::new(Default::default);
    let now = crate::config::now_f64();
    let mut m = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if m.get(lane).is_some_and(|t| now - t < 600.0) {
        return false;
    }
    m.insert(lane.to_string(), now);
    true
}

/// Pool size and ready count per hub, for /api/contract/counters.
pub fn pool_census(conn: &Connection) -> rusqlite::Result<Vec<(String, usize, usize)>> {
    let hubs: Vec<String> = conn
        .prepare("SELECT DISTINCT i.session FROM issues i JOIN issue_tags t ON t.issue_id = i.id AND t.tag = ?1 \
                  WHERE i.status IN ('todo', 'backlog') AND i.deleted IS NULL AND i.session IS NOT NULL")?
        .query_map([POOL_TAG], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for hub in hubs {
        let cards = pool_cards(conn, &hub, None)?;
        let ready = cards.iter().filter(|c| c.2).count();
        out.push((hub, cards.len(), ready));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A2 phase 2 through the shipped pull: an idle pilot lane takes one ready
    /// pool card from its hub; a card with an open dependency is skipped; two
    /// idle lanes get different cards; a lane with its own todo gets none; a
    /// lane with the rule held back gets none.
    #[tokio::test]
    async fn an_idle_lane_pulls_one_ready_card_from_its_hubs_pool() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::create_dir_all(h.join("sessions")).unwrap();
        let _g = crate::api::settings::test_env::set_home(h);
        let store: crate::db::SharedStore = std::sync::Arc::new(crate::db::Store::open(&h.join("q.db")).unwrap());
        for lane in ["pl-a", "pl-b", "pl-busy"] {
            std::fs::write(h.join(format!("sessions/{lane}.env")), "AMUX_CONTRACT_DONE=1\nAMUX_CONTRACT_HUB=\"hub\"\n").unwrap();
        }
        std::fs::write(h.join("sessions/pl-off.env"), "AMUX_CONTRACT_DONE=1\nAMUX_CONTRACT_HUB=\"hub\"\nAMUX_CONTRACT_RULES_OFF=\"A2\"\n").unwrap();
        let card = |id: &'static str, session: &'static str, status: &'static str, deps: &'static str, pos: f64, pool: bool| {
            store.write(move |c| {
                c.execute(
                    "INSERT INTO issues (id, title, status, session, type, depends_on, pos, created, updated, archived) \
                     VALUES (?1, ?1, ?2, ?3, 'chore', ?4, ?5, 0, 0, 0)",
                    rusqlite::params![id, status, session, deps, pos],
                )?;
                if pool {
                    c.execute("INSERT INTO issue_tags (issue_id, tag, added_at) VALUES (?1, 'pool', 0)", [id])?;
                }
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            }).unwrap();
        };
        card("DEP-1", "hub", "doing", "[]", 0.0, false);
        card("P-BLOCKED", "hub", "backlog", "[\"DEP-1\"]", 1.0, true);
        card("P-1", "hub", "backlog", "[]", 2.0, true);
        card("P-2", "hub", "todo", "[]", 3.0, true);
        card("P-3", "hub", "backlog", "[]", 4.0, true);
        card("NOT-POOL", "hub", "backlog", "[]", 0.5, false);
        card("OWN", "pl-busy", "todo", "[]", 0.0, false);
        let session = |id: &str| crate::db::board_store::get_issue(&store.read().unwrap(), id).unwrap().unwrap();

        assert_eq!(pull_from_pool(&store, "pl-a").await, PoolPull::Assigned { card: "P-1".into(), hub: "hub".into() },
            "the first ready pool card, skipping the one with an open dependency");
        assert_eq!((session("P-1").session.as_deref(), session("P-1").status.as_str()), (Some("pl-a"), "todo"));
        assert_eq!(pull_from_pool(&store, "pl-a").await, PoolPull::NotApplicable, "a lane with work takes no more");
        assert_eq!(pull_from_pool(&store, "pl-b").await, PoolPull::Assigned { card: "P-2".into(), hub: "hub".into() },
            "a second idle lane gets a different card");
        assert_eq!(session("P-2").session.as_deref(), Some("pl-b"));
        assert_eq!(pull_from_pool(&store, "pl-busy").await, PoolPull::NotApplicable, "own todo first");
        assert_eq!(pull_from_pool(&store, "pl-off").await, PoolPull::NotApplicable, "A2 held back");
        assert_eq!(session("P-3").session.as_deref(), Some("hub"));
        assert_eq!(session("P-BLOCKED").session.as_deref(), Some("hub"), "never a card with an open dependency");
        assert_eq!(session("NOT-POOL").session.as_deref(), Some("hub"), "only cards the hub released");
        let census = pool_census(&store.read().unwrap()).unwrap();
        assert_eq!(census, vec![("hub".to_string(), 2, 1)], "P-BLOCKED and P-3 remain; one is ready");
    }

    /// AH-395: with AMUX_A2_POOL_TITLE_REGEX an idle lane takes proof cards
    /// from the hub without any tag, reopened-by-review first; its own
    /// backlog proof card comes before the hub's; a non-proof hub card is
    /// never taken; a card with an open dependency is skipped.
    #[tokio::test]
    async fn an_idle_lane_takes_the_next_reopened_proof_card_by_title() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::create_dir_all(h.join("sessions")).unwrap();
        let _g = crate::api::settings::test_env::set_home(h);
        let store: crate::db::SharedStore = std::sync::Arc::new(crate::db::Store::open(&h.join("q.db")).unwrap());
        for lane in ["pr-a", "pr-b", "pr-c", "pr-d"] {
            std::fs::write(
                h.join(format!("sessions/{lane}.env")),
                "AMUX_CONTRACT_DONE=1\nAMUX_CONTRACT_HUB=\"hub\"\nAMUX_A2_POOL_TITLE_REGEX=\"^GS12 (proof|requirement)\"\n",
            )
            .unwrap();
        }
        let card = |id: &'static str, title: &'static str, session: &'static str, status: &'static str, deps: &'static str, pos: f64, reopened: bool| {
            store.write(move |c| {
                c.execute(
                    "INSERT INTO issues (id, title, status, session, type, depends_on, pos, created, updated, archived) \
                     VALUES (?1, ?2, ?3, ?4, 'ops', ?5, ?6, 0, 0, 0)",
                    rusqlite::params![id, title, status, session, deps, pos],
                )?;
                if reopened {
                    c.execute(
                        "INSERT INTO card_contracts (card, acceptance, command, hash, frozen_at, state, review_state) \
                         VALUES (?1, 'a', '(none)', 'h', 0, 'frozen', 'failed')",
                        [id],
                    )?;
                }
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        };
        card("DEP-1", "GS12 plan item 1.1 open", "hub", "doing", "[]", 0.0, false);
        card("PLAN", "GS12 plan item 2.3 not proof", "hub", "backlog", "[]", 0.1, false);
        card("BLOCKED", "GS12 proof 9 blocked", "hub", "backlog", "[\"DEP-1\"]", 0.2, true);
        card("OLD", "GS12 proof 1 never reviewed", "hub", "backlog", "[]", 1.0, false);
        card("REOPENED", "GS12 requirement 4 sent back", "hub", "backlog", "[]", 2.0, true);
        card("OWN", "GS12 proof 7 reopened here", "pr-b", "backlog", "[]", 0.0, true);
        let who = |id: &str| crate::db::board_store::get_issue(&store.read().unwrap(), id).unwrap().unwrap();

        assert_eq!(pull_from_pool(&store, "pr-a").await, PoolPull::Assigned { card: "REOPENED".into(), hub: "hub".into() },
            "a reopened proof card comes before an older one, with no pool tag");
        assert_eq!((who("REOPENED").session.as_deref(), who("REOPENED").status.as_str()), (Some("pr-a"), "todo"));
        assert_eq!(pull_from_pool(&store, "pr-b").await, PoolPull::Assigned { card: "OWN".into(), hub: "hub".into() },
            "the lane's own backlog proof card first");
        assert_eq!((who("OWN").session.as_deref(), who("OWN").status.as_str()), (Some("pr-b"), "todo"));
        assert_eq!(pull_from_pool(&store, "pr-c").await, PoolPull::Assigned { card: "OLD".into(), hub: "hub".into() });
        assert!(matches!(pull_from_pool(&store, "pr-d").await, PoolPull::Empty { .. }),
            "nothing left: the plan card is not proof and the blocked one waits");
        assert_eq!(who("PLAN").session.as_deref(), Some("hub"), "a non-proof hub card is never taken");
        assert_eq!(who("BLOCKED").session.as_deref(), Some("hub"), "a card with an open dependency is skipped");
    }

    /// AH-395: a lane parked by its done-WIP cap still gets proof work: its
    /// own ready proof card in todo, else its own backlog, else the hub's,
    /// never a non-proof card.
    #[test]
    fn a_capped_lane_gets_proof_work_own_todo_then_own_backlog_then_hub() {
        let home = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&home.path().join("q.db")).unwrap();
        let re = regex::Regex::new("^GS12 (proof|requirement)").unwrap();
        store.write(|c| {
            for (id, title, session, status) in [
                ("NP", "GS12 plan item 3.1", "cap", "todo"),
                ("OB", "GS12 proof 5 reopened in its backlog", "cap", "backlog"),
                ("HB", "GS12 proof 6 on the hub", "hub", "backlog"),
                ("OT", "GS12 proof 7 already in todo", "cap2", "todo"),
            ] {
                c.execute(
                    "INSERT INTO issues (id, title, status, session, type, depends_on, pos, created, updated, archived) \
                     VALUES (?1, ?2, ?3, ?4, 'ops', '[]', 0, 0, 0, 0)",
                    rusqlite::params![id, title, status, session],
                )?;
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let got = |hub: Option<&'static str>, lane: &'static str| {
            let re = re.clone();
            let out = std::sync::Arc::new(std::sync::Mutex::new(None));
            let o = out.clone();
            store.write(move |c| {
                *o.lock().unwrap() = claim_proof_for_capped(c, hub, lane, &re)?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            }).unwrap();
            let v = out.lock().unwrap().take();
            v.map(|(card, source, _)| (card, source))
        };
        assert_eq!(got(Some("hub"), "cap2"), Some(("OT".into(), "own-todo")), "its own proof card in todo, unmoved");
        assert_eq!(got(Some("hub"), "cap"), Some(("OB".into(), "own")), "its own backlog before the hub, never the non-proof todo");
        assert_eq!(got(Some("hub"), "cap3"), Some(("HB".into(), "hub")), "then the hub's pool");
        assert_eq!(got(Some("hub"), "cap4"), None, "nothing proof left");
    }

    /// (1) The pool's readiness is board-drive's auto-park predicate: for
    /// every dependency status and type, a card the pool calls ready is one
    /// `deps_blocking` would not park.
    #[test]
    fn pool_readiness_is_the_auto_park_predicate() {
        let home = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&home.path().join("q.db")).unwrap();
        let cases = [("todo", "code"), ("doing", "code"), ("done", "code"), ("verified", "code"),
                     ("done", "chore"), ("done", "ops"), ("discarded", "code"), ("done", "")];
        store.write(move |c| {
            for (i, (st, ty)) in cases.iter().enumerate() {
                c.execute("INSERT INTO issues (id, title, status, type, created, updated) VALUES (?1, ?1, ?2, ?3, 0, 0)",
                    rusqlite::params![format!("D{i}"), st, ty])?;
                c.execute("INSERT INTO issues (id, title, status, session, depends_on, created, updated) VALUES (?1, ?1, 'backlog', 'lane', ?2, 0, 0)",
                    rusqlite::params![format!("C{i}"), format!("[\"D{i}\"]")])?;
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let conn = store.read().unwrap();
        for (i, case) in cases.iter().enumerate() {
            let row = crate::db::board_store::get_issue(&conn, &format!("C{i}")).unwrap().unwrap();
            let parks = !crate::runtime_jobs::board_drive::deps_blocking(&conn, &row).is_empty();
            assert_eq!(deps_resolved(&conn, &format!("[\"D{i}\"]")).unwrap(), !parks, "case {case:?}");
        }
    }

    /// (2) A card its lane parked to backlog stays out of the pool for the
    /// cooldown, unless a dependency changed status since; a card parked by
    /// someone else, or long ago, is pullable.
    #[test]
    fn a_card_its_lane_reparked_is_not_pulled_back_within_the_cooldown() {
        let home = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&home.path().join("q.db")).unwrap();
        let re = regex::Regex::new("^GS12 proof").unwrap();
        let now = crate::config::now_f64() as i64;
        let old = now - 5 * 3600;
        store.write(move |c| {
            c.execute("INSERT INTO issues (id, title, status, type, created, updated, entered_state_at) VALUES ('MOVED', 'MOVED', 'verified', 'code', 0, 0, ?1)", [now - 30])?;
            c.execute("INSERT INTO issues (id, title, status, type, created, updated, entered_state_at) VALUES ('STILL', 'STILL', 'verified', 'code', 0, 0, ?1)", [now - 900])?;
            for (id, actor, at, deps) in [("LANE", "lane", now - 60, "[\"STILL\"]"), ("PEER", "mixpeek-override", now - 60, "[]"),
                                          ("OLD", "lane", old, "[]"), ("DEPMOVED", "lane", now - 600, "[\"MOVED\"]")] {
                c.execute(
                    "INSERT INTO issues (id, title, status, session, type, depends_on, pos, created, updated, archived, entered_state_at, log) \
                     VALUES (?1, 'GS12 proof ' || ?1, 'backlog', 'lane', 'ops', ?2, 0, 0, 0, 0, ?3, ?4)",
                    rusqlite::params![id, deps, at, format!("`09:10` Auto-picked up from queue by lane\n`09:11` {actor}: doing -> backlog")],
                )?;
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let conn = store.read().unwrap();
        let ready: Vec<(String, bool)> = candidates(&conn, "lane", Some(&re), false).unwrap()
            .into_iter().map(|(id, _, ready, _)| (id, ready)).collect();
        let r = |id: &str| ready.iter().find(|(i, _)| i == id).map(|(_, r)| *r);
        assert_eq!(r("LANE"), Some(false), "the lane parked it a minute ago");
        assert_eq!(r("PEER"), Some(true), "parked by another actor");
        assert_eq!(r("OLD"), Some(true), "past the cooldown");
        assert_eq!(r("DEPMOVED"), Some(true), "a dependency moved since the park");
    }

    /// (3) With no ready proof card, an idle lane takes the ready non-proof
    /// card that unblocks the most open proof cards (directly or one hop),
    /// its own board first. A hub card the board refuses to move (proof cards
    /// on the hub depend on it: cross_board_dependency_forbidden) stays put
    /// and the pull reports the real pool size instead of failing.
    #[tokio::test]
    async fn an_idle_lane_with_no_ready_proof_takes_the_card_that_unblocks_the_most() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::create_dir_all(h.join("sessions")).unwrap();
        let _g = crate::api::settings::test_env::set_home(h);
        let store: crate::db::SharedStore = std::sync::Arc::new(crate::db::Store::open(&h.join("q.db")).unwrap());
        for lane in ["ub-a", "ub-c"] {
            std::fs::write(h.join(format!("sessions/{lane}.env")),
                "AMUX_CONTRACT_DONE=1\nAMUX_CONTRACT_HUB=\"hub\"\nAMUX_A2_POOL_TITLE_REGEX=\"^GS12 proof\"\n").unwrap();
        }
        store.write(|c| {
            for (id, title, session, deps, pos) in [
                ("P1", "GS12 proof 1", "hub", "[\"A-HIGH\"]", 0.0),
                ("P2", "GS12 proof 2", "hub", "[\"A-MID\"]", 0.1),
                ("P3", "GS12 proof 3", "hub", "[\"A-MID\"]", 0.2),
                ("A-MID", "GS12 plan item 5.5 mid", "ub-a", "[\"A-HIGH\"]", 0.3),
                ("A-LOW", "GS12 plan item 1.1", "ub-a", "[]", 0.0),
                ("A-HIGH", "GS12 plan item 2.2", "ub-a", "[]", 1.0),
                ("P4", "GS12 proof 4", "hub", "[\"A-LOW\"]", 0.4),
                ("HUB-DEP", "GS12 plan item 3.3", "hub", "[]", 0.0),
                ("P5", "GS12 proof 5", "hub", "[\"HUB-DEP\"]", 0.5),
            ] {
                c.execute(
                    "INSERT INTO issues (id, title, status, session, type, depends_on, pos, created, updated, archived) \
                     VALUES (?1, ?2, 'backlog', ?3, 'code', ?4, ?5, 0, 0, 0)",
                    rusqlite::params![id, title, session, deps, pos],
                )?;
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let card = |id: &str| crate::db::board_store::get_issue(&store.read().unwrap(), id).unwrap().unwrap();
        assert_eq!(pull_from_pool(&store, "ub-a").await, PoolPull::Assigned { card: "A-HIGH".into(), hub: "hub".into() },
            "A-HIGH unblocks P1 directly and P2, P3 through A-MID: 3, against A-LOW's 1");
        assert_eq!((card("A-HIGH").session.as_deref(), card("A-HIGH").status.as_str()), (Some("ub-a"), "todo"));
        // HUB-DEP cannot leave the hub alone while P5 depends on it there; it
        // moves with its dependency group (Ethan, 2026-10-07).
        assert_eq!(pull_from_pool(&store, "ub-c").await, PoolPull::Assigned { card: "HUB-DEP".into(), hub: "hub".into() });
        assert_eq!((card("HUB-DEP").session.as_deref(), card("HUB-DEP").status.as_str()), (Some("ub-c"), "todo"));
        assert_eq!((card("P5").session.as_deref(), card("P5").status.as_str()), (Some("ub-c"), "backlog"),
            "the dependent proof card moves with it and keeps its status");
        assert!(card("P5").log.unwrap_or_default().contains("dependency group"));
    }

    /// The group is all or nothing: over the cap it stays, and a member that
    /// cannot move (in doing) keeps every member on the hub.
    #[test]
    fn a_dependency_group_moves_whole_or_not_at_all() {
        let home = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&home.path().join("q.db")).unwrap();
        store.write(|c| {
            for (id, status, deps) in [
                ("B", "backlog", "[]"), ("D1", "backlog", "[\"B\"]"), ("D2", "backlog", "[\"B\"]"), ("D3", "backlog", "[\"D2\"]"),
                ("X", "backlog", "[]"), ("XD", "doing", "[\"X\"]"),
            ] {
                c.execute(
                    "INSERT INTO issues (id, title, status, session, type, depends_on, pos, created, updated, archived) \
                     VALUES (?1, ?1, ?2, 'hub', 'code', ?3, 0, 0, 0, 0)",
                    rusqlite::params![id, status, deps],
                )?;
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let owner = |id: &str| crate::db::board_store::get_issue(&store.read().unwrap(), id).unwrap().unwrap().session;
        let go = |root: &str, cap: usize| {
            let root = root.to_string();
            let out = std::sync::Arc::new(std::sync::Mutex::new(None));
            let o2 = out.clone();
            store.write(move |c| {
                let r = take_group(c, "hub", &root, "backlog".into(), "lane", "test", cap)?;
                *o2.lock().unwrap() = Some(r);
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            }).unwrap();
            let r = out.lock().unwrap().take().unwrap();
            r
        };
        assert_eq!(go("B", 3), GroupTake::TooLarge(4), "B, D1, D2 and D3 exceed a cap of 3");
        assert_eq!(owner("D3").as_deref(), Some("hub"));
        assert!(matches!(go("X", 8), GroupTake::Refused(_)), "XD is in doing");
        assert_eq!((owner("X").as_deref(), owner("XD").as_deref()), (Some("hub"), Some("hub")), "nothing moved");
        assert_eq!(go("B", 8), GroupTake::Moved(4));
        for id in ["B", "D1", "D2", "D3"] {
            assert_eq!(owner(id).as_deref(), Some("lane"), "{id} moved with the group");
        }
    }

    /// MO-4071/MO-4086/MO-4438: a proof card parked with a fresh trigger, or a
    /// blocker, stays in backlog; a stale trigger (past SOURCE_REF_STALE_S)
    /// releases it, as the backlog drain does.
    #[test]
    fn a2_logs_a_repeat_once_per_card_per_window() {
        // Keys are per card: a second pass over the same too-large group
        // stays quiet, a different card still logs.
        let k = format!("toolarge:T-{}", std::process::id());
        assert!(first_this_window(&k));
        assert!(!first_this_window(&k));
        assert!(first_this_window(&format!("{k}-other")));
    }

    #[test]
    fn a2_leaves_a_card_parked_on_a_fresh_trigger_in_backlog() {
        let home = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&home.path().join("q.db")).unwrap();
        let re = regex::Regex::new("^GS12 proof").unwrap();
        let now = crate::config::now_f64() as i64;
        let stale = now - crate::runtime_jobs::board_drive::SOURCE_REF_STALE_S - 60;
        store.write(move |c| {
            for (id, src, at, blocked) in [("TRIG", "GR-88 done", now - 60, ""), ("BLOCK", "", 0, "GT-316 run"),
                                           ("STALE", "GR-88 done", stale, ""), ("FREE", "", 0, "")] {
                c.execute(
                    "INSERT INTO issues (id, title, status, session, type, depends_on, pos, created, updated, archived, source_ref, last_verified_at, blocked_on) \
                     VALUES (?1, 'GS12 proof ' || ?1, 'backlog', 'lane', 'ops', '[]', 0, 0, 0, 0, ?2, ?3, ?4)",
                    rusqlite::params![id, src, at, blocked],
                )?;
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let conn = store.read().unwrap();
        let ready: Vec<(String, bool)> = candidates(&conn, "lane", Some(&re), false).unwrap()
            .into_iter().map(|(id, _, ready, _)| (id, ready)).collect();
        let r = |id: &str| ready.iter().find(|(i, _)| i == id).map(|(_, r)| *r);
        assert_eq!(r("TRIG"), Some(false), "a fresh trigger holds the card");
        assert_eq!(r("BLOCK"), Some(false), "a blocker holds the card");
        assert_eq!(r("STALE"), Some(true), "a stale trigger releases it");
        assert_eq!(r("FREE"), Some(true));
    }

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
