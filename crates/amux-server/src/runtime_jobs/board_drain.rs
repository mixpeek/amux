//! Drain state: what stands between a lane and an empty board (RR-0052,
//! Invariant 5: "a durable orchestrator, not an LLM conversation, owns board
//! drainage").
//!
//! The board driver already dispatches, reclaims and promotes. What it could not
//! do was ANSWER the drain question: is this lane done, and if not, what exactly
//! is in the way? `LaneTrace` records what the last tick did, which is a
//! different fact. This computes the answer from the board alone, with the same
//! dependency predicate dispatch uses (`deps_blocking` ->
//! `bs::dependency_resolved`), so the view cannot disagree with the mechanism it
//! describes.

use crate::db::board_store as bs;
use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Blocker {
    pub id: String,
    /// Status of the blocking card, or "missing" when it no longer exists.
    pub status: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BlockedCard {
    pub id: String,
    pub status: String,
    pub blocked_by: Vec<Blocker>,
    /// Free-text external watch (`blocked_on`), when the block is not a card.
    pub blocked_on: Option<String>,
    /// Some blocker is waiting on a human (a `needsyou` card). No worker can
    /// clear this one.
    pub needs_human: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RunningCard {
    pub id: String,
    pub holder: Option<String>,
    pub attempt: Option<i64>,
    pub heartbeat_age_s: Option<i64>,
    pub lease_expires_in_s: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DrainState {
    pub lane: String,
    /// False when the board could not be read. Every count below is then 0 and
    /// means nothing (ethos rule 4).
    pub measured: bool,
    /// Open agent-owned cards considered: todo, doing, backlog, blocked,
    /// review, needsyou. done/verified/discarded/quarantined are finished work
    /// for this question; verification has its own loop.
    pub n_considered: usize,
    pub ready: usize,
    pub running: Vec<RunningCard>,
    pub blocked: Vec<BlockedCard>,
    pub review: usize,
    pub needs_human: usize,
    /// Backlog with nothing blocking it. Not dispatched unless the lane opts in
    /// (AMUX_DISPATCH_BACKLOG_WHEN_IDLE), so it is reported apart from `ready`.
    pub parked: usize,
    /// `blocked` cards with no unresolved dependency and no external watch: the
    /// block is gone and nothing has moved them. The driver unblocks these.
    pub unblockable: Vec<String>,
    pub verdict: &'static str,
}

/// The verdict, from counts alone. Pure so every arm has a test.
///
/// - `draining`: something is running or ready; the driver has work to hand out.
/// - `waiting_on_dependency`: nothing runnable, but a blocker is another card a
///   worker can finish. The swarm clears this with no human.
/// - `unblockable`: a `blocked` card whose block is gone. A bug signal if it
///   persists past one driver tick.
/// - `waiting_on_human`: only human-owned asks stand in the way.
/// - `waiting_on_external`: blocked on a free-text watch, not a card.
/// - `waiting_on_review`: only review remains.
/// - `backlog_only`: only undispatched backlog remains.
/// - `drained`: nothing open.
pub fn verdict(
    ready: usize,
    running: usize,
    blocked: &[BlockedCard],
    unblockable: usize,
    review: usize,
    needs_human: usize,
    parked: usize,
) -> &'static str {
    if running > 0 || ready > 0 {
        return "draining";
    }
    if blocked
        .iter()
        .any(|b| !b.needs_human && !b.blocked_by.is_empty())
    {
        return "waiting_on_dependency";
    }
    if unblockable > 0 {
        return "unblockable";
    }
    if needs_human > 0 || blocked.iter().any(|b| b.needs_human) {
        return "waiting_on_human";
    }
    if !blocked.is_empty() {
        return "waiting_on_external";
    }
    if review > 0 {
        return "waiting_on_review";
    }
    if parked > 0 {
        return "backlog_only";
    }
    "drained"
}

const OPEN: [&str; 6] = ["todo", "doing", "backlog", "blocked", "review", "needsyou"];

/// The answer when the board could not be read at all.
pub fn drain_state_unmeasured(lane: &str) -> DrainState {
    DrainState {
        lane: lane.to_string(),
        measured: false,
        n_considered: 0,
        ready: 0,
        running: vec![],
        blocked: vec![],
        review: 0,
        needs_human: 0,
        parked: 0,
        unblockable: vec![],
        verdict: "unmeasured",
    }
}

pub fn drain_state(conn: &Connection, lane: &str, now: i64) -> DrainState {
    let mut st = drain_state_unmeasured(lane);
    let statuses: Vec<String> = OPEN.iter().map(|s| s.to_string()).collect();
    let rows = match bs::list_issues(
        conn,
        &statuses,
        &[lane.to_string()],
        bs::ArchivedFilter::ActiveOnly,
    ) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                target: "amux::board_drive", lane, %error, measured = false, n_considered = 0,
                verdict = "drain_state_unmeasured", "board drain: could not read the lane's open cards"
            );
            return st;
        }
    };
    let attempts = crate::db::attempts::running_attempt_numbers(conn).unwrap_or_default();
    let status_of = |id: &str| -> String {
        conn.query_row(
            "SELECT status FROM issues WHERE id=?1 AND deleted IS NULL",
            [id],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "missing".to_string())
    };
    for row in rows.iter().filter(|r| r.owner_type == "agent") {
        st.n_considered += 1;
        let unresolved = crate::runtime_jobs::board_drive::deps_blocking(conn, row);
        let blockers: Vec<Blocker> = unresolved
            .iter()
            .map(|id| Blocker {
                id: id.clone(),
                status: status_of(id),
            })
            .collect();
        let blocked_on = row.blocked_on.clone().filter(|b| !b.trim().is_empty());
        let as_blocked = |blockers: Vec<Blocker>| BlockedCard {
            id: row.id.clone(),
            status: row.status.clone(),
            needs_human: blockers.iter().any(|b| b.status == "needsyou"),
            blocked_by: blockers,
            blocked_on: blocked_on.clone(),
        };
        match row.status.as_str() {
            "doing" => st.running.push(RunningCard {
                id: row.id.clone(),
                holder: row.lease_owner.clone().or_else(|| row.session.clone()),
                attempt: attempts.get(&row.id).copied(),
                heartbeat_age_s: row.lease_heartbeat_at.map(|h| now - h),
                lease_expires_in_s: row.lease_expires_at.map(|e| e - now),
            }),
            "todo" if blockers.is_empty() && blocked_on.is_none() => st.ready += 1,
            "backlog" if blockers.is_empty() && blocked_on.is_none() => st.parked += 1,
            "todo" | "backlog" => st.blocked.push(as_blocked(blockers)),
            "blocked" => {
                if blockers.is_empty() && blocked_on.is_none() {
                    st.unblockable.push(row.id.clone());
                } else {
                    st.blocked.push(as_blocked(blockers));
                }
            }
            "review" => st.review += 1,
            "needsyou" => st.needs_human += 1,
            _ => {}
        }
    }
    st.measured = true;
    st.verdict = verdict(
        st.ready,
        st.running.len(),
        &st.blocked,
        st.unblockable.len(),
        st.review,
        st.needs_human,
        st.parked,
    );
    st
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(needs_human: bool, by_card: bool) -> BlockedCard {
        BlockedCard {
            id: "B".into(),
            status: "blocked".into(),
            blocked_by: if by_card {
                vec![Blocker {
                    id: "D".into(),
                    status: "doing".into(),
                }]
            } else {
                vec![]
            },
            blocked_on: (!by_card).then(|| "vendor ships the fix".to_string()),
            needs_human,
        }
    }

    #[test]
    fn every_verdict_arm_is_reachable_and_ordered() {
        assert_eq!(verdict(1, 0, &[], 0, 0, 0, 0), "draining");
        assert_eq!(
            verdict(0, 1, &[blocked(true, true)], 0, 3, 3, 3),
            "draining",
            "anything running outranks what is waiting"
        );
        assert_eq!(
            verdict(0, 0, &[blocked(false, true)], 1, 0, 1, 0),
            "waiting_on_dependency",
            "a card a worker can finish outranks a human ask"
        );
        assert_eq!(verdict(0, 0, &[], 1, 0, 0, 0), "unblockable");
        assert_eq!(
            verdict(0, 0, &[blocked(true, true)], 0, 0, 0, 0),
            "waiting_on_human"
        );
        assert_eq!(verdict(0, 0, &[], 0, 0, 2, 0), "waiting_on_human");
        assert_eq!(
            verdict(0, 0, &[blocked(false, false)], 0, 0, 0, 0),
            "waiting_on_external"
        );
        assert_eq!(verdict(0, 0, &[], 0, 2, 0, 0), "waiting_on_review");
        assert_eq!(verdict(0, 0, &[], 0, 0, 0, 4), "backlog_only");
        assert_eq!(verdict(0, 0, &[], 0, 0, 0, 0), "drained");
    }

    #[test]
    fn drain_state_reads_the_same_dependency_rule_dispatch_uses() {
        let conn = crate::db::migrate::test_memdb();
        let ins = |id: &str, status: &str, kind: &str, deps: &str| {
            conn.execute(
                "INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type,depends_on) \
                 VALUES (?1,?1,'',?2,'lane',1,1,'agent',?3,?4)",
                rusqlite::params![id, status, kind, deps],
            )
            .unwrap();
        };
        // A code dependency that is only `done` still blocks: code completes at verified.
        ins("DEP", "done", "code", "[]");
        ins("WAITS", "todo", "code", "[\"DEP\"]");
        ins("FREE", "todo", "chore", "[]");
        ins("STUCK", "blocked", "code", "[]");
        ins("ASK", "needsyou", "decision", "[]");
        let st = drain_state(&conn, "lane", 100);
        assert!(st.measured);
        assert_eq!(
            st.n_considered, 4,
            "done is not open work for this question"
        );
        assert_eq!(st.ready, 1);
        assert_eq!(st.blocked.len(), 1);
        assert_eq!(st.blocked[0].id, "WAITS");
        assert_eq!(
            st.blocked[0].blocked_by,
            vec![Blocker {
                id: "DEP".into(),
                status: "done".into()
            }]
        );
        assert_eq!(st.unblockable, vec!["STUCK".to_string()]);
        assert_eq!(st.needs_human, 1);
        assert_eq!(st.verdict, "draining");

        conn.execute("UPDATE issues SET status='discarded' WHERE id='FREE'", [])
            .unwrap();
        assert_eq!(
            drain_state(&conn, "lane", 100).verdict,
            "waiting_on_dependency"
        );
        conn.execute("UPDATE issues SET status='verified' WHERE id='DEP'", [])
            .unwrap();
        let st = drain_state(&conn, "lane", 100);
        assert_eq!(st.ready, 1, "a verified dependency frees its successor");
        assert_eq!(st.verdict, "draining");
    }
}
