//! Task attempts (RR-0052, Invariant 1: every worker execution belongs to a
//! task attempt).
//!
//! A lease is the CURRENT holding; an attempt is the durable record of each
//! one. A card on its fourth try reads as attempt #4 with three recorded
//! outcomes, not as a `lease_generation` of 8. The only writer is
//! [`record_lease_change`], called at the two lease choke points after the card
//! is saved, so a refused transition never leaves an attempt behind, and the
//! attempt log and the lease can only disagree when a path bypasses both.
//! [`reconcile_orphans`] closes those.
//!
//! WHY THE TABLE IS ENSURED HERE AND NOT IN A NUMBERED MIGRATION. On
//! 2026-09-14 the live database recorded migration 69 as
//! `0069_worker_lifecycle`, applied by a dirty build from the shared checkout
//! while origin/main registered only 68 (AMUX-4533). `migrate.rs` skips a
//! version it has already applied without reading the name, so a
//! `0069_task_attempts` landing on origin would boot green on that box and
//! never create this table, and every lease change would then fail its write
//! transaction. `CREATE TABLE IF NOT EXISTS` at the point of use cannot be
//! skipped that way. Move it into a migration once the version collision is
//! resolved on origin.
//!
//! Outcome vocabulary (NULL while the attempt runs):
//! done | review | blocked | parked | needs_input | failed | discarded |
//! released (put back in todo by a worker or human) | abandoned (the lease
//! reaper reclaimed it from a silent holder) | reassigned (another lane took
//! the lease) | orphaned (the card left its lease through a path that bypassed
//! both choke points).

use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::HashMap;

/// The actor the lease reaper advances as. An attempt it ends is `abandoned`.
pub const LEASE_REAPER_ACTOR: &str = "amux:lease-reaper";

/// Create the table and its indexes if absent. Cheap (a no-op parse once the
/// table exists) and only called on lease changes, which are rare next to reads.
pub fn ensure_table(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_attempts (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            card        TEXT    NOT NULL,
            attempt     INTEGER NOT NULL,
            worker      TEXT    NOT NULL,
            generation  INTEGER NOT NULL,
            started_at  INTEGER NOT NULL,
            ended_at    INTEGER,
            outcome     TEXT,
            to_status   TEXT,
            ended_by    TEXT,
            reason      TEXT
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_task_attempts_card_attempt ON task_attempts(card, attempt);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_task_attempts_running ON task_attempts(card) WHERE ended_at IS NULL;
        CREATE INDEX IF NOT EXISTS idx_task_attempts_worker ON task_attempts(worker, started_at);",
    )
}

/// A read against a database where no lease has changed since this shipped has
/// no table yet. That is "no attempts", not an error.
fn missing_table(e: &rusqlite::Error) -> bool {
    e.to_string().contains("no such table: task_attempts")
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Attempt {
    pub attempt: i64,
    pub worker: String,
    pub generation: i64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    /// None while running.
    pub outcome: Option<String>,
    pub to_status: Option<String>,
    pub ended_by: Option<String>,
    pub reason: Option<String>,
}

/// How an attempt ended, from where the card went and who moved it. The three
/// outcomes the redesign names map directly: `done`/`review` (DONE), `blocked`
/// and `parked` (BLOCKED), `failed` (FAILED). The rest say who ended it when the
/// worker did not.
pub fn outcome_for(to_status: &str, actor: &str, reassigned: bool) -> &'static str {
    if reassigned {
        return "reassigned";
    }
    match to_status {
        "done" | "verified" => "done",
        "review" => "review",
        "blocked" => "blocked",
        "backlog" => "parked",
        "needsyou" | "needs_you" => "needs_input",
        "quarantined" => "failed",
        "discarded" => "discarded",
        "todo" if actor == LEASE_REAPER_ACTOR => "abandoned",
        _ => "released",
    }
}

fn close_running(
    conn: &Connection,
    card: &str,
    outcome: &str,
    to_status: &str,
    ended_by: &str,
    reason: Option<&str>,
    now: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE task_attempts SET ended_at = ?2, outcome = ?3, to_status = ?4, ended_by = ?5, reason = ?6 \
         WHERE card = ?1 AND ended_at IS NULL",
        params![card, now, outcome, to_status, ended_by, reason.map(|r| truncate(r, 400))],
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

/// Record what a SAVED lease change means for the card's attempts. A no-op when
/// the holder did not change (a self-renewal, or a card that never had a lease).
#[allow(clippy::too_many_arguments)]
pub fn record_lease_change(
    conn: &Connection,
    card: &str,
    prev_holder: Option<&str>,
    new_holder: Option<&str>,
    generation: i64,
    to_status: &str,
    actor: &str,
    reason: Option<&str>,
    now: i64,
) -> rusqlite::Result<()> {
    if prev_holder == new_holder {
        return Ok(());
    }
    ensure_table(conn)?;
    if prev_holder.is_some() {
        let outcome = outcome_for(to_status, actor, new_holder.is_some());
        close_running(conn, card, outcome, to_status, actor, reason, now)?;
    }
    if let Some(worker) = new_holder {
        // A running attempt left by a bypass path would violate the one-running
        // index; close it honestly as orphaned rather than failing the claim.
        close_running(
            conn,
            card,
            "orphaned",
            to_status,
            actor,
            Some("superseded by a new lease"),
            now,
        )?;
        let next: i64 = conn.query_row(
            "SELECT COALESCE(MAX(attempt), 0) + 1 FROM task_attempts WHERE card = ?1",
            [card],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO task_attempts (card, attempt, worker, generation, started_at) VALUES (?1,?2,?3,?4,?5)",
            params![card, next, worker, generation, now],
        )?;
    }
    Ok(())
}

/// Every attempt on one card, oldest first.
pub fn list_for_card(conn: &Connection, card: &str) -> rusqlite::Result<Vec<Attempt>> {
    match list_for_card_inner(conn, card) {
        Err(e) if missing_table(&e) => Ok(Vec::new()),
        other => other,
    }
}

fn list_for_card_inner(conn: &Connection, card: &str) -> rusqlite::Result<Vec<Attempt>> {
    let mut st = conn.prepare(
        "SELECT attempt, worker, generation, started_at, ended_at, outcome, to_status, ended_by, reason \
         FROM task_attempts WHERE card = ?1 ORDER BY attempt ASC",
    )?;
    let rows = st.query_map([card], |r| {
        Ok(Attempt {
            attempt: r.get(0)?,
            worker: r.get(1)?,
            generation: r.get(2)?,
            started_at: r.get(3)?,
            ended_at: r.get(4)?,
            outcome: r.get(5)?,
            to_status: r.get(6)?,
            ended_by: r.get(7)?,
            reason: r.get(8)?,
        })
    })?;
    rows.collect()
}

/// Running attempt number per card, for list rows. One query over the running
/// set, which is bounded by the number of leased cards, not the board size.
pub fn running_attempt_numbers(conn: &Connection) -> rusqlite::Result<HashMap<String, i64>> {
    let inner = || -> rusqlite::Result<HashMap<String, i64>> {
        let mut st =
            conn.prepare("SELECT card, attempt FROM task_attempts WHERE ended_at IS NULL")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        rows.collect()
    };
    match inner() {
        Err(e) if missing_table(&e) => Ok(HashMap::new()),
        other => other,
    }
}

/// One card this worker is still holding with nothing recorded about how the
/// work stands.
pub struct OpenHold {
    pub card: String,
    pub attempt: i64,
    pub status: String,
    /// When the attempt began, in epoch SECONDS (the unit `task_attempts`
    /// stores). The caller subtracts it from its own `now` rather than reading
    /// a duration computed at a different moment than the decision.
    pub started_at: i64,
}

/// What this worker is still holding, for the turn boundary (RR-0052 Inv 4).
///
/// Matched on the LEASE rather than on `task_attempts.worker`: the lease is
/// what says the card is this lane's to move right now, and a rename moves
/// `lease_owner` while the attempt keeps the name it was claimed under. Asking
/// the attempt's worker instead would miss a renamed lane's own card.
///
/// Open means NOTHING has been recorded about where the work stands, so both
/// halves of a close are checked. [`close_running`] writes `ended_at` and
/// `outcome` together, and either one alone still answers the question this
/// asks: an `outcome` says where the work landed, and an `ended_at` says the
/// attempt is over and no longer this lane's to answer for.
pub fn open_holds_for_worker(conn: &Connection, worker: &str) -> rusqlite::Result<Vec<OpenHold>> {
    let inner = || -> rusqlite::Result<Vec<OpenHold>> {
        let mut st = conn.prepare(
            "SELECT a.card, a.attempt, i.status, a.started_at \
             FROM task_attempts a JOIN issues i ON i.id = a.card \
             WHERE i.lease_owner = ?1 AND i.deleted IS NULL \
               AND a.ended_at IS NULL AND a.outcome IS NULL \
             ORDER BY a.started_at",
        )?;
        let rows = st.query_map([worker], |r| {
            Ok(OpenHold {
                card: r.get(0)?,
                attempt: r.get(1)?,
                status: r.get(2)?,
                started_at: r.get(3)?,
            })
        })?;
        rows.collect()
    };
    match inner() {
        Err(e) if missing_table(&e) => Ok(Vec::new()),
        other => other,
    }
}

/// Close running attempts whose card no longer holds a lease. Matched on the
/// CARD, not the worker name: a lane rename moves `lease_owner` but keeps the
/// attempt's `worker` as the name it held the card under, and that attempt is
/// still running.
/// A card can leave `doing` through a raw UPDATE (board hygiene) or a create
/// straight into a terminal status, and neither passes a lease choke point.
/// Returns how many were closed, which the reaper publishes: a steady nonzero
/// count names a bypass path worth routing through `advance`.
pub fn reconcile_orphans(conn: &Connection, now: i64) -> rusqlite::Result<usize> {
    ensure_table(conn)?;
    conn.execute(
        "UPDATE task_attempts SET ended_at = ?1, outcome = 'orphaned', ended_by = 'amux:attempt-reconcile', \
             to_status = (SELECT status FROM issues i WHERE i.id = task_attempts.card), \
             reason = 'card left its lease without passing a lease choke point' \
         WHERE ended_at IS NULL AND NOT EXISTS ( \
             SELECT 1 FROM issues i WHERE i.id = task_attempts.card AND i.status = 'doing' \
               AND i.lease_owner IS NOT NULL AND i.deleted IS NULL)",
        [now],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(conn: &Connection, id: &str, status: &str, owner: Option<&str>) {
        conn.execute(
            "INSERT OR REPLACE INTO issues (id,title,desc,status,session,created,updated,owner_type,type,lease_owner) \
             VALUES (?1,?1,'',?2,'lane',1,1,'agent','code',?3)",
            params![id, status, owner],
        )
        .unwrap();
    }

    #[test]
    fn outcomes_name_the_three_endings_and_who_ended_it_otherwise() {
        assert_eq!(outcome_for("done", "lane", false), "done");
        assert_eq!(outcome_for("review", "lane", false), "review");
        assert_eq!(outcome_for("blocked", "lane", false), "blocked");
        assert_eq!(outcome_for("quarantined", "lane", false), "failed");
        assert_eq!(outcome_for("todo", LEASE_REAPER_ACTOR, false), "abandoned");
        assert_eq!(outcome_for("todo", "lane", false), "released");
        assert_eq!(outcome_for("doing", "other", true), "reassigned");
    }

    #[test]
    fn a_claim_opens_attempt_one_and_each_later_holding_counts_up() {
        let conn = crate::db::migrate::test_memdb();
        card(&conn, "C", "doing", Some("a"));
        record_lease_change(&conn, "C", None, Some("a"), 1, "doing", "a", None, 100).unwrap();
        assert_eq!(running_attempt_numbers(&conn).unwrap().get("C"), Some(&1));

        // Reaper reclaims: attempt 1 abandoned, nothing running.
        record_lease_change(
            &conn,
            "C",
            Some("a"),
            None,
            2,
            "todo",
            LEASE_REAPER_ACTOR,
            Some("silent"),
            200,
        )
        .unwrap();
        assert!(running_attempt_numbers(&conn).unwrap().is_empty());

        // Re-claimed by b, then b hands it to c mid-doing.
        record_lease_change(&conn, "C", None, Some("b"), 3, "doing", "b", None, 300).unwrap();
        record_lease_change(&conn, "C", Some("b"), Some("c"), 4, "doing", "c", None, 400).unwrap();
        // c finishes.
        record_lease_change(&conn, "C", Some("c"), None, 5, "review", "c", None, 500).unwrap();

        let all = list_for_card(&conn, "C").unwrap();
        let got: Vec<(i64, &str, Option<&str>)> = all
            .iter()
            .map(|a| (a.attempt, a.worker.as_str(), a.outcome.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                (1, "a", Some("abandoned")),
                (2, "b", Some("reassigned")),
                (3, "c", Some("review"))
            ]
        );
        assert_eq!(all[0].reason.as_deref(), Some("silent"));
    }

    #[test]
    fn a_self_renewal_or_a_leaseless_card_records_nothing() {
        let conn = crate::db::migrate::test_memdb();
        // Before any lease change the table does not exist; reads say "none".
        assert!(list_for_card(&conn, "C").unwrap().is_empty());
        assert!(running_attempt_numbers(&conn).unwrap().is_empty());
        record_lease_change(&conn, "C", Some("a"), Some("a"), 1, "doing", "a", None, 1).unwrap();
        record_lease_change(&conn, "C", None, None, 1, "done", "a", None, 1).unwrap();
        assert!(list_for_card(&conn, "C").unwrap().is_empty());
    }

    #[test]
    fn an_attempt_whose_card_left_doing_by_a_bypass_is_closed_as_orphaned() {
        let conn = crate::db::migrate::test_memdb();
        card(&conn, "KEEP", "doing", Some("a"));
        card(&conn, "GONE", "doing", Some("a"));
        record_lease_change(&conn, "KEEP", None, Some("a"), 1, "doing", "a", None, 1).unwrap();
        record_lease_change(&conn, "GONE", None, Some("a"), 1, "doing", "a", None, 1).unwrap();
        // A raw UPDATE, the way board hygiene discards: no choke point sees it.
        conn.execute(
            "UPDATE issues SET status='discarded', lease_owner=NULL WHERE id='GONE'",
            [],
        )
        .unwrap();
        assert_eq!(reconcile_orphans(&conn, 9).unwrap(), 1);
        let gone = list_for_card(&conn, "GONE").unwrap();
        assert_eq!(gone[0].outcome.as_deref(), Some("orphaned"));
        assert_eq!(gone[0].to_status.as_deref(), Some("discarded"));
        assert_eq!(
            running_attempt_numbers(&conn).unwrap().get("KEEP"),
            Some(&1)
        );
    }
}
