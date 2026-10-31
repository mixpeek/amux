//! Grooming a lane's stale board cards (docs/design/board-cards-stay-current.md, steps 2 and 5).
//!
//! Only the worker knows whether its card still matters: it knows what shipped and what replaced
//! what (VISION.md A2). So amux does not judge a card. It finds the lane's cards that have not
//! moved in `AMUX_BOARD_GROOM_DAYS` (7) days and hands the lane one turn with them as context,
//! asking it to keep, finish, fold or retire each with the board's existing verbs. A card with a
//! human owner, and a needs-you card holding a budget or customer-outbound decision, are never
//! offered: they are the owner's (A3).
//!
//! - `GET  /api/board-groom/staleness?days=7` per lane and in total: open cards, stale cards,
//!   the oldest, when the lane last groomed, and the fleet's stale ratio, with `measured` and
//!   `n_considered` (A5).
//! - `POST /api/board-groom/{lane}` delivers the turn now (`?dry=1` returns the text only;
//!   `?force=1` skips the once-a-day limit). The board-hygiene job calls the same function for
//!   every lane with stale cards (`AMUX_BOARD_GROOM=0` turns it off for a lane).
//!
//! Each delivery logs `verdict=board_groom_delivered` (or why not) and is recorded in
//! `~/.amux/board-groom.json`, so a lane is offered at most one turn a day.

use super::AppState;
use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

const MAX_CARDS: usize = 20;
const DAY: i64 = 86_400;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/staleness", get(staleness))
        .route("/{lane}", post(groom))
}

/// Days a card must sit untouched to count as stale.
pub(crate) fn stale_days() -> i64 {
    std::env::var("AMUX_BOARD_GROOM_DAYS").ok().and_then(|v| v.parse().ok()).filter(|d: &i64| *d > 0).unwrap_or(7)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StaleCard {
    pub id: String,
    pub title: String,
    pub status: String,
    pub age_days: i64,
    pub done_when: String,
    pub last_log: String,
}

fn open_filter() -> &'static str {
    "archived=0 AND status NOT IN ('done','verified','discarded')"
}

/// The lane's stale cards a worker may judge, oldest first, at most `MAX_CARDS`.
pub(crate) fn stale_cards(conn: &rusqlite::Connection, lane: &str, now: i64, days: i64) -> rusqlite::Result<Vec<StaleCard>> {
    let sql = format!(
        "SELECT id, title, status, updated, COALESCE(acceptance_criteria,''), COALESCE(log,'') FROM issues \
         WHERE session=?1 AND {} AND updated < ?2 \
         AND COALESCE(owner_type,'agent') <> 'human' \
         AND NOT (status='needsyou' AND COALESCE(ask_type,'') IN ('budget','customer_outbound')) \
         ORDER BY updated ASC LIMIT ?3",
        open_filter()
    );
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map(rusqlite::params![lane, now - days * DAY, MAX_CARDS as i64], |r| {
        let updated: i64 = r.get(3)?;
        let log: String = r.get(5)?;
        Ok(StaleCard {
            id: r.get(0)?,
            title: r.get(1)?,
            status: r.get(2)?,
            age_days: (now - updated) / DAY,
            done_when: r.get(4)?,
            last_log: log.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").to_string(),
        })
    })?;
    rows.collect()
}

fn clip(s: &str, n: usize) -> String {
    let t: String = s.trim().chars().take(n).collect();
    if s.trim().chars().count() > n { format!("{t}…") } else { t }
}

/// The turn's text: the cards as context and the verbs that already exist. No verdict on any card.
pub(crate) fn groom_text(lane: &str, cards: &[StaleCard], days: i64) -> String {
    let mut t = format!(
        "[amux board grooming] {} of your open cards have not moved in {days}+ days. You know what \
         shipped and what replaced what, so judge each one against what it was for. For each card do \
         exactly one, then go back to what you were doing:\n\
         - still needed: `amux board progress <ID> \"groom: keep: <why it still matters>\"`\n\
         - finished: `amux board done <ID>` with its evidence (the commit, the card or the output)\n\
         - replaced by another card: `amux board discard <ID> --folded-into <OTHER-ID>`\n\
         - obsolete: `curl -sk -X PATCH -H 'Content-Type: application/json' -H \"X-Amux-Session: {lane}\" \
         -d '{{\"archived\":true,\"archive_outcome\":\"obsolete: <why>\"}}' \"$(amux url)/api/board/<ID>\"` \
         (undo: `amux board unarchive <ID>`)\n\
         Cards (oldest first):\n",
        cards.len()
    );
    for c in cards {
        t.push_str(&format!("- {} ({}, {}d) {}", c.id, c.status, c.age_days, clip(&c.title, 120)));
        if !c.done_when.trim().is_empty() {
            t.push_str(&format!(" | done when: {}", clip(&c.done_when, 160)));
        }
        if !c.last_log.trim().is_empty() {
            t.push_str(&format!(" | last: {}", clip(&c.last_log, 140)));
        }
        t.push('\n');
    }
    t
}

fn ledger_path() -> std::path::PathBuf {
    super::session_verbs::home().join("board-groom.json")
}

fn last_groomed() -> BTreeMap<String, i64> {
    std::fs::read_to_string(ledger_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn record_groomed(lane: &str, at: i64) {
    let mut m = last_groomed();
    m.insert(lane.to_string(), at);
    let path = ledger_path();
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec(&m).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Whether amux may offer this lane a grooming turn: not isolated, board on, not switched off.
pub(crate) fn groomable(lane: &str) -> Result<(), &'static str> {
    if super::session_verbs::session_is_isolated(lane) {
        return Err("isolated");
    }
    if super::board_lifecycle::no_board(lane) {
        return Err("board_off");
    }
    let off = super::session_verbs::scoped_setting_in(&super::session_verbs::home(), lane, "AMUX_BOARD_GROOM")
        .or_else(|| std::env::var("AMUX_BOARD_GROOM").ok())
        .is_some_and(|v| matches!(v.trim().trim_matches('"'), "0" | "false" | "off" | "no"));
    if off {
        return Err("switched_off");
    }
    Ok(())
}

/// Offer `lane` one grooming turn. Returns the outcome as JSON (also logged).
pub(crate) async fn groom_lane(state: &AppState, lane: &str, force: bool, dry: bool) -> Value {
    let now = chrono::Utc::now().timestamp();
    let days = stale_days();
    let refuse = |why: &str, n: usize| {
        tracing::info!(session = lane, reason = why, n_considered = n, measured = true,
            verdict = "board_groom_skipped", "board grooming: not offered");
        json!({"ok": false, "lane": lane, "skipped": why, "stale": n})
    };
    if let Err(why) = groomable(lane) {
        return refuse(why, 0);
    }
    let lane_s = lane.to_string();
    let cards = match state.store.read_async(move |conn| Ok(stale_cards(conn, &lane_s, now, days)?)).await {
        Ok(c) => c,
        Err(e) => return json!({"ok": false, "lane": lane, "error": e.to_string(), "measured": false}),
    };
    if cards.is_empty() {
        return refuse("nothing_stale", 0);
    }
    if !force && !dry && last_groomed().get(lane).is_some_and(|at| now - at < DAY) {
        return refuse("groomed_in_last_day", cards.len());
    }
    let text = groom_text(lane, &cards, days);
    if dry {
        return json!({"ok": true, "lane": lane, "dry": true, "stale": cards.len(), "text": text});
    }
    let d = super::session_verbs::deliver_automated(state, lane, &text, "board-groom").await;
    if d.refused {
        tracing::warn!(session = lane, message = %d.message, n_considered = cards.len(), measured = true,
            verdict = "board_groom_refused", "board grooming: the delivery was refused");
        return json!({"ok": false, "lane": lane, "stale": cards.len(), "refused": d.message});
    }
    record_groomed(lane, now);
    let ids: Vec<&str> = cards.iter().map(|c| c.id.as_str()).collect();
    tracing::info!(session = lane, cards = ?ids, n_considered = cards.len(), measured = true,
        submission = d.submission, verdict = "board_groom_delivered", "board grooming: turn offered");
    json!({"ok": true, "lane": lane, "stale": cards.len(), "cards": ids, "submission": d.submission, "message": d.message})
}

async fn groom(State(state): State<AppState>, Path(lane): Path<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    let on = |k: &str| q.get(k).is_some_and(|v| v == "1" || v == "true");
    let v = groom_lane(&state, &lane, on("force"), on("dry")).await;
    let code = if v["ok"].as_bool() == Some(true) || v.get("skipped").is_some() { 200 } else { 409 };
    (axum::http::StatusCode::from_u16(code).unwrap_or(axum::http::StatusCode::OK), Json(v)).into_response()
}

/// Per lane: open and stale counts, the oldest stale card's age, and when it last groomed.
pub(crate) fn staleness_report(conn: &rusqlite::Connection, now: i64, days: i64) -> rusqlite::Result<Value> {
    let sql = format!(
        "SELECT COALESCE(session,''), COUNT(*), SUM(CASE WHEN updated < ?1 THEN 1 ELSE 0 END), MIN(updated) \
         FROM issues WHERE {} GROUP BY COALESCE(session,'')",
        open_filter()
    );
    let mut st = conn.prepare(&sql)?;
    let groomed = last_groomed();
    let mut lanes = Vec::new();
    let (mut open_total, mut stale_total) = (0i64, 0i64);
    for row in st.query_map([now - days * DAY], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)))? {
        let (lane, open, stale, oldest) = row?;
        open_total += open;
        stale_total += stale;
        lanes.push(json!({
            "lane": if lane.is_empty() { Value::Null } else { json!(lane) },
            "open": open, "stale": stale, "oldest_days": (now - oldest) / DAY,
            "groomed_at": groomed.get(&lane),
        }));
    }
    lanes.sort_by(|a, b| b["stale"].as_i64().cmp(&a["stale"].as_i64())
        .then_with(|| a["lane"].as_str().unwrap_or("").cmp(b["lane"].as_str().unwrap_or(""))));
    Ok(json!({
        "measured": true,
        "n_considered": open_total,
        "days": days,
        "open": open_total,
        "stale": stale_total,
        "stale_ratio": if open_total > 0 { json!((stale_total as f64 / open_total as f64 * 1000.0).round() / 1000.0) } else { Value::Null },
        "lanes": lanes,
    }))
}

async fn staleness(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let days = q.get("days").and_then(|v| v.parse().ok()).filter(|d: &i64| *d > 0).unwrap_or_else(stale_days);
    let now = chrono::Utc::now().timestamp();
    match state.store.read_async(move |conn| Ok(staleness_report(conn, now, days)?)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => Json(json!({"measured": false, "n_considered": 0, "why_unmeasured": e.to_string()})).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, crate::db::Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("t.db")).unwrap();
        (dir, store)
    }

    fn card(store: &crate::db::Store, id: &str, lane: &str, status: &str, updated: i64, owner: &str, ask: &str) {
        let (id, lane, status, owner, ask) = (id.to_string(), lane.to_string(), status.to_string(), owner.to_string(), ask.to_string());
        store
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO issues (id,title,\"desc\",status,session,created,updated,archived,source,creator,type,owner_type,ask_type,acceptance_criteria,log) \
                     VALUES (?1,?1,'',?2,?3,?4,?4,0,'agent','w','task',?5,?6,'the report is sent','`10:00` started')",
                    rusqlite::params![id, status, lane, updated, owner, ask],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
    }

    #[test]
    fn a_lane_is_offered_its_own_stale_cards_and_never_the_owners() {
        let (_d, store) = db();
        let now = 1_800_000_000;
        card(&store, "OLD", "w", "todo", now - 10 * DAY, "agent", "");
        card(&store, "OLDER", "w", "backlog", now - 40 * DAY, "agent", "");
        card(&store, "FRESH", "w", "todo", now - DAY, "agent", "");
        card(&store, "OWNERS", "w", "todo", now - 30 * DAY, "human", "");
        card(&store, "BUDGET", "w", "needsyou", now - 30 * DAY, "agent", "budget");
        card(&store, "ASK", "w", "needsyou", now - 30 * DAY, "agent", "decision");
        card(&store, "OTHER", "x", "todo", now - 30 * DAY, "agent", "");
        let conn = store.read().unwrap();
        let got: Vec<String> = stale_cards(&conn, "w", now, 7).unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(got, vec!["OLDER", "ASK", "OLD"], "oldest first; not fresh, not the owner's, not a budget hold, not another lane's");
        let t = groom_text("w", &stale_cards(&conn, "w", now, 7).unwrap(), 7);
        assert!(t.contains("OLDER (backlog, 40d)") && t.contains("done when: the report is sent"));
        for verb in ["amux board progress", "amux board done", "--folded-into", "archive_outcome", "amux board unarchive"] {
            assert!(t.contains(verb), "the turn names the existing verb {verb}");
        }
    }

    #[test]
    fn staleness_counts_every_lane_and_says_it_measured() {
        let (_d, store) = db();
        let now = 1_800_000_000;
        card(&store, "A1", "a", "todo", now - 10 * DAY, "agent", "");
        card(&store, "A2", "a", "todo", now - DAY, "agent", "");
        card(&store, "B1", "b", "backlog", now - 20 * DAY, "agent", "");
        card(&store, "DONE", "b", "done", now - 20 * DAY, "agent", "");
        let conn = store.read().unwrap();
        let v = staleness_report(&conn, now, 7).unwrap();
        assert_eq!(v["measured"], true);
        assert_eq!((v["open"].as_i64(), v["stale"].as_i64()), (Some(3), Some(2)));
        assert_eq!(v["n_considered"], 3);
        assert_eq!(v["stale_ratio"], json!(0.667));
        assert_eq!(v["lanes"][0]["lane"], "a");
    }
}
