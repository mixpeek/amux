//! A card that fell out of the dispatch queue has to say so (AF-317).
//!
//! Ethan, 2026-08-29: "some workers have an infinite # of growing backlogs and
//! todo then they go idle."
//!
//! THE FILING CARD'S HEADLINE NUMBER DID NOT SURVIVE VERIFICATION, and this
//! note is here because the wrong version shipped first (dfa7187c). AF-317
//! measured "todo: 321 cards, median age 28.8 days" from `/api/board`, and that
//! query counted ARCHIVED cards. Re-measured against the DB on 2026-08-30 with
//! `COALESCE(archived,0)=0`: 88 live todo cards, median age 0.8 DAYS. The
//! month-old population is real and it is entirely archived, which every
//! dispatch mechanism already excludes. So `todo` is not a graveyard.
//!
//! THE OBVIOUS READING OF THAT IS WRONG, and getting it wrong would have
//! produced a weaker fix than the one already in the tree. The card that filed
//! this asked for "21 days in todo/review, then the card stops being
//! dispatchable". `board_drive` has stopped dealing any todo nobody has touched
//! in SEVEN days since AMUX-3779 (`PICKUP_FRESHNESS_S_DEFAULT`), so a 21-day cut
//! would have loosened an existing rule while reading like a new constraint.
//!
//! The real gap is what happens at that edge, which is nothing. Measured
//! 2026-08-30 over LIVE cards: 4 of the 72 agent-owned todo cards in the
//! dispatch pool are past the freshness edge, median 9.9 days untouched. (The
//! first version of this comment said 55 of 125 at a median of 27.4 days, from
//! the same archived-inclusive query as above; 67 of those 55 were archived.
//! Correcting it rather than deleting it, because the mechanism below is
//! justified at 4 cards and a reader deserves the real order of magnitude.)
//! Those 4 still read `todo` on the board and in every list, so their status
//! says dispatchable while the mechanism has excluded them — ethos rule 4, in
//! the board's own state field. The exclusion
//! IS counted, by `stale_gate_excluded_todos`, and surfaced in the pickup trace;
//! but only when the queue is otherwise EMPTY, so a lane holding one live card
//! and eight dead ones never sees it, and the trace reaches no owner anyway.
//!
//! So this job does the one thing missing: it tells the owner, once, in the
//! surface the owner already reads. It does NOT retire, retype or archive
//! anything. Which of those a stale card deserves is a judgement about work this
//! job cannot see, and making it in bulk would be ethos rule 8 with a sweep
//! attached (the 445-card `needsyou` pile is what that looks like from the other
//! direction).

use crate::api::AppState;
use crate::db::board_store as bs;

/// Registry id. `spawn_periodic` derives this job's kill switch from the name.
const JOB: &str = "queue-disposition";
/// Six hours. The population moves on the scale of days, and a card that
/// re-nags faster than a person can act on it is the notice-fatigue shape this
/// whole card is about.
const TICK_SECS: u64 = 6 * 3600;
/// The tag that makes a disposition card findable, and makes "one per lane"
/// enforceable without a second table.
pub const DISPOSITION_TAG: &str = "queue:disposition";

/// Cards whose owner should be asked to dispose of them: `todo`, agent-owned,
/// past the dispatcher's own freshness edge.
///
/// The predicate is `board_drive`'s, minus the per-card claim cooldown: this
/// reports on QUEUE MEMBERSHIP, not on what is dispatchable this second. Using
/// the dispatcher's own freshness value rather than a number of our own is the
/// point — two thresholds that disagree about the same fact is how a card ends
/// up excluded by one rule and reported live by another.
fn stalled_todos(conn: &rusqlite::Connection, lane: &str) -> Vec<(String, String, i64)> {
    let now = crate::config::now_f64();
    if crate::runtime_jobs::board_drive::pickup_freshness_s() <= 0 { return Vec::new(); }
    let cut = crate::runtime_jobs::board_drive::pickup_fresh_cut(now) as f64;
    let mut out = Vec::new();
    if let Ok(mut st) = conn.prepare(
        "SELECT id, title, updated FROM issues i WHERE i.session=?1 AND i.status='todo' \
         AND i.owner_type='agent' AND i.deleted IS NULL AND COALESCE(i.archived,0)=0 \
         AND COALESCE(i.type,'') NOT IN ('tripwire','watch','epic') \
         AND NOT EXISTS (SELECT 1 FROM issue_tags t WHERE t.issue_id=i.id \
                         AND lower(t.tag) LIKE 'needs:you%') \
         AND i.updated < ?2 ORDER BY i.updated ASC",
    ) {
        if let Ok(rows) = st.query_map(rusqlite::params![lane, cut], |r| {
            let updated: f64 = r.get(2)?;
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, ((now - updated) / 86_400.0) as i64))
        }) {
            out.extend(rows.flatten());
        }
    }
    out
}

/// Every lane holding at least one stalled todo.
fn lanes_with_stalled(conn: &rusqlite::Connection) -> Vec<String> {
    if crate::runtime_jobs::board_drive::pickup_freshness_s() <= 0 { return Vec::new(); }
    let cut = crate::runtime_jobs::board_drive::pickup_fresh_cut(crate::config::now_f64());
    let mut out = Vec::new();
    if let Ok(mut st) = conn.prepare(
        "SELECT DISTINCT session FROM issues WHERE session IS NOT NULL AND session <> '' \
         AND status='todo' AND owner_type='agent' AND deleted IS NULL \
         AND COALESCE(archived,0)=0 \
         AND COALESCE(type,'') NOT IN ('tripwire','watch','epic') AND updated < ?1",
    ) {
        if let Ok(rows) = st.query_map(rusqlite::params![cut], |r| r.get::<_, String>(0)) {
            out.extend(rows.flatten());
        }
    }
    out
}

/// The open disposition card for `lane`, if there is one.
///
/// ONE PER LANE is the requirement, and it is the whole difference between this
/// and the pile it reports on. A job that files a card per stale card, or a new
/// card per tick, would be adding to the queue it exists to drain.
fn open_disposition_card(conn: &rusqlite::Connection, lane: &str) -> Option<String> {
    conn.query_row(
        "SELECT i.id FROM issues i JOIN issue_tags t ON t.issue_id = i.id \
         WHERE t.tag = ?1 AND i.session = ?2 AND i.deleted IS NULL \
         AND COALESCE(i.archived,0) = 0 \
         AND i.status NOT IN ('done','verified','discarded') LIMIT 1",
        rusqlite::params![DISPOSITION_TAG, lane],
        |r| r.get(0),
    )
    .ok()
}

/// The card body. Separate from the filing so it can be tested without a store.
///
/// Three dispositions and no fourth, because the fourth is what already
/// happened: leave it, and it stays in a queue nobody is dealing from.
pub fn body(lane: &str, stalled: &[(String, String, i64)], freshness_days: i64) -> (String, String) {
    let n = stalled.len();
    let title = format!(
        "{n} card(s) in {lane}'s todo are no longer being dispatched — re-commit, re-scope, or archive"
    );
    let mut desc = String::new();
    desc.push_str(&format!(
        "These {n} card(s) still read `todo`, and auto-pickup has already stopped offering them: \
         board_drive does not deal a todo nobody has touched in {freshness_days} days. Nothing \
         announced that, which is why they are here.\n\n\
         This card does not change any of them. Which disposition each deserves is a judgement \
         about the work, and it is yours.\n\n"
    ));
    for (id, t, days) in stalled {
        let short: String = t.chars().take(72).collect();
        desc.push_str(&format!("  {id}  {days}d untouched  {short}\n"));
    }
    desc.push_str(
        "\nPick one per card:\n\n\
         RE-COMMIT   you are going to work it. Touch it and it re-enters the queue:\n\
                     amux board doing <ID>\n\n\
         RE-SCOPE    real, but not next. `backlog` is unbounded on purpose:\n\
                     amux board backlog <ID> --trigger \"<what should bring it back>\"\n\n\
         ARCHIVE     not a unit of work, or overtaken:\n\
                     amux board discard <ID>\n\n\
         When the lane has no stalled todos left, this card closes with\n\
         `amux board done <ID> --evidence-stdin`.\n",
    );
    (title, desc)
}

/// One pass. Returns (lanes examined, lanes with stalled cards, cards named).
///
/// THREE numbers and not one, for the AF-320 reason this whole card keeps
/// running into: "0 lanes with stalled cards" is good news only if the first
/// number is non-zero. A pass that examined nothing produces the same second
/// number as a healthy fleet.
pub async fn tick(state: AppState) -> (usize, usize, usize) {
    let Ok(conn) = state.store.read() else {
        tracing::warn!("queue_disposition: store unavailable, no lane examined");
        return (0, 0, 0);
    };
    // Counted separately from the stalled set: `lanes_with_stalled` returns only
    // the lanes that HAVE one, so on its own it cannot say whether the sweep
    // looked at anything.
    let examined: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT session) FROM issues WHERE session IS NOT NULL \
             AND session <> '' AND status='todo' AND owner_type='agent' \
             AND deleted IS NULL AND COALESCE(archived,0)=0",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let lanes = lanes_with_stalled(&conn);
    let freshness_days = crate::runtime_jobs::board_drive::pickup_freshness_s() / 86_400;
    let mut named = 0usize;
    let mut work: Vec<(String, Option<String>, String, String)> = Vec::new();
    for lane in &lanes {
        let stalled = stalled_todos(&conn, lane);
        if stalled.is_empty() {
            continue;
        }
        named += stalled.len();
        let (title, desc) = body(lane, &stalled, freshness_days);
        work.push((lane.clone(), open_disposition_card(&conn, lane), title, desc));
    }
    drop(conn);

    for (lane, existing, title, desc) in work {
        match existing {
            // UPDATE, never a second card. The list changes as the lane works,
            // and a stale list is worse than none — it sends someone at a card
            // they already dealt with.
            Some(id) => {
                let (t, d) = (title.clone(), desc.clone());
                let _ = state
                    .store
                    .write_async(move |conn| {
                        // INTEGER, not f64. `updated` is an INTEGER column and
                        // every other writer stores it as one; SQLite is
                        // dynamically typed, so a single f64 here silently
                        // stores REAL in that row and `issue_from_row` then
                        // fails the WHOLE list read with "Invalid column type
                        // Real at index: 6". Three rows written by this job
                        // took `GET /api/board` down fleet-wide for ~20 minutes
                        // on 2026-08-30. The reader is hardened too (see
                        // `issue_from_row`); this is the half that stops
                        // producing the bad value in the first place.
                        conn.execute(
                            "UPDATE issues SET title=?1, \"desc\"=?2, updated=?3 WHERE id=?4",
                            rusqlite::params![t, d, crate::config::now_f64() as i64, id],
                        )?;
                        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                    })
                    .await;
            }
            None => {
                let new = bs::NewIssue {
                    title,
                    desc,
                    status: "todo".into(),
                    session: Some(lane.clone()),
                    // `chore`: there is nothing to implement or merge, so a
                    // `code` gate could not be satisfied honestly and the card
                    // would rot or be force-closed (ethos rule 3).
                    item_type: "chore".into(),
                    // Exempt from the todo WIP limit BY NAME (see
                    // QUEUE_DISPOSITION_CREATOR): this is the one card that has
                    // to arrive precisely when the queue is too long.
                    creator: crate::api::board::QUEUE_DISPOSITION_CREATOR.into(),
                    owner_type: "agent".into(),
                    due: None,
                    due_time: None,
                    reviewer: None,
                    shepherd: None,
                    gate: vec![],
                    depends_on: vec![],
                    tags: vec![DISPOSITION_TAG.to_string()],
                    // Not an ask: this producer files ordinary cards, and a card
                    // filed into needsyou without one is what AMUX-3929 is about.
                    ask_type: None,
                    ask_question: None,
                    ask_unblocks: None,
                    ask_actor: None,
                    // AF-367: filed by the queue-disposition job.
                    source: Some("queue_disposition".into()),
                    requested_by: None,
                    callback_session: None,
                    callback_prompt: None,
                };
                let _ = state
                    .store
                    .write_async(move |conn| {
                        let _ = bs::create_issue(conn, &new, crate::config::now_f64() as i64)?;
                        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                    })
                    .await;
            }
        }
    }
    // Both numbers, always: "0 lanes" and "0 lanes examined" are different
    // facts and the second one means this job is not working (AF-320).
    tracing::info!(
        job = JOB,
        lanes_examined = examined,
        lanes_with_stalled = lanes.len(),
        cards_named = named,
        freshness_days,
        "queue disposition pass"
    );
    (examined as usize, lanes.len(), named)
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, TICK_SECS, move || {
        let st = state.clone();
        async move {
            tick(st).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body names every stalled card AND all three dispositions.
    ///
    /// The list is the actionable half — a card that says "you have 8 stale
    /// cards" and does not name them makes the reader run the query this job
    /// just ran. The three verbs are the other half: the fourth option is what
    /// already happened, so it must not be offered.
    #[test]
    fn the_body_names_the_cards_and_all_three_dispositions() {
        let stalled = vec![
            ("AF-1".to_string(), "Compute Utilization Audit".to_string(), 27),
            ("AF-2".to_string(), "Fix Namespace Pollution".to_string(), 55),
        ];
        let (title, desc) = body("mvs-infra", &stalled, 7);
        assert!(title.starts_with("2 card(s) in mvs-infra's todo"), "{title}");
        for id in ["AF-1", "AF-2"] {
            assert!(desc.contains(id), "every stalled card must be named: {desc}");
        }
        assert!(desc.contains("27d untouched"), "and how long it has sat: {desc}");
        for verb in ["RE-COMMIT", "RE-SCOPE", "ARCHIVE"] {
            assert!(desc.contains(verb), "missing disposition {verb}: {desc}");
        }
        // It must say the cards are ALREADY excluded, not that they will be.
        assert!(
            desc.contains("already stopped offering them"),
            "the whole finding is that this has already happened: {desc}"
        );
        // And it must not claim to have changed anything (ethos rule 8).
        assert!(desc.contains("does not change any of them"), "{desc}");
    }

    /// The freshness number in the card is the DISPATCHER's, not one of ours.
    ///
    /// Two thresholds that disagree about the same fact is how a card ends up
    /// excluded by one rule and reported live by another, so this pins that the
    /// body quotes whatever board_drive is actually using.
    #[test]
    fn the_card_quotes_the_dispatchers_own_freshness_window() {
        let days = crate::runtime_jobs::board_drive::pickup_freshness_s() / 86_400;
        let (_, desc) = body("x", &[("A-1".into(), "t".into(), 9)], days);
        assert!(
            desc.contains(&format!("touched in {days} days")),
            "the card must quote the dispatcher's window ({days}d), not a second one: {desc}"
        );
    }
}
