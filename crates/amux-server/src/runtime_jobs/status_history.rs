//! `session.status_decided` — a durable record of WHICH RULE decided a lane's
//! status, sampled on change (AMUX-3761).
//!
//! # The question this exists to answer
//!
//! Ethan screenshotted a lane reading WORKING + AGENTS with "Last interaction
//! 31m ago", over a pane whose visible text was the agent saying it had no task
//! queued and "Worked for 9s" — a finished turn — and asked whether that was
//! accurate. By the time the screenshot arrived the badge read `idle`,
//! `decided_by: report`, over a fresh stop-hook. Accurate NOW. Which rule fired
//! 31 minutes earlier was unrecoverable: `derive_status_explain` is computed
//! fresh per request and never persisted, and `session_events` held ZERO status
//! rows for that lane across the whole window.
//!
//! AMUX-3434 built `status-explain` precisely so a wrong badge would not cost a
//! screenshot investigation. It still did, one layer up: the instrument answers
//! "why is this WORKING right now" and every question anyone asks about a badge
//! is retrospective, because a screenshot always arrives minutes late.
//!
//! # Why a job and not the request path
//!
//! The fleet-wide derivation lives in the `/api/sessions` LIST handler, which
//! every open dashboard hits every few seconds. Recording from there is a write
//! amplified by client count, on a 2.2GB SQLite, from a GET. This job derives
//! the same verdict through the same function on its own cadence instead, so
//! the write rate is a property of the FLEET rather than of how many browser
//! tabs are open.
//!
//! # The row rate, measured before this shipped
//!
//! Sampling `/api/sessions` every 15s for 306s over 52 running lanes: **7
//! status changes, 82.4/hour, ~1,978 rows/day**, concentrated in four lanes
//! (nissan 3, radio-canada 2, tubescience 1, gtm-engine 1). That is a LOWER
//! BOUND on status alone — this job also records a change of `decided_by` at an
//! unchanged status, which is the more interesting case and adds rows — and an
//! UPPER bound in the other direction, because a 20s tick misses flips a 15s
//! probe caught. Call it single-digit thousands a day.
//!
//! Against a `session_events` table already holding 208k rows, 14-day retention
//! puts this at roughly a doubling in the worst case and tens of MB on a 2.2GB
//! database. Bounded, and the prune is wired on the same day as the writer
//! rather than after someone notices.
//!
//! # It is a SAMPLE, and the payload says so
//!
//! At a 20s tick, a lane that flips active -> idle -> active inside one tick
//! records one row or none. That gap must not read as "the status never
//! changed", so `status-explain` publishes the cadence beside the history and
//! `history_recorded_since` distinguishes "this lane has been stable" from
//! "this job has not been running long enough to know" (ethos rule 4).
//!
//! SAMPLING IS THE LOAD-BEARING CHOICE, not a convenience, and the reviewer's
//! argument for it is better than the one this module shipped with (random,
//! 2026-08-26): the badge in the original incident flipped because a report's
//! trust window AGED OUT, and nothing fires when a window expires. Status is a
//! pure function of (report age, pane, subagents, clocks) evaluated on read, so
//! an event-driven hook can catch a report landing or a pane repaint and is
//! structurally blind to a time-driven flip — which is precisely the class of
//! bug this feature chases. Only sampling observes the whole space.
//!
//! ONE EDGE WHERE "EMPTY IS HONEST" LEAKS, recorded rather than hidden: the
//! guarantee holds for RUNNING lanes, each of which gets a baseline row on its
//! first census. A lane that has NEVER run returns `history: []` with a
//! non-null `history_recorded_since`, which reads as "stable and sampled" when
//! the truth is "never in the census". Benign, because a lane that never ran has
//! no live badge to interrogate — but it is a leak, and making it airtight means
//! separating "seen in a census at least once" from "never seen" on the read
//! path.
//!
//! TICKS CANNOT OVERLAP, which is what makes it safe for these rows to carry no
//! idempotency key: `spawn_periodic_every` is a single
//! `loop { tick.tick().await; f().await; }` with `MissedTickBehavior::Delay`, so
//! a slow tick delays the next one rather than running beside it. Two concurrent
//! ticks would read the same `prev` and both insert the same change row.

use rusqlite::Connection;
use serde_json::json;

use crate::api::AppState;

const JOB: &str = "status-history";
const TICK_SECS: u64 = 20;
pub const EVENT: &str = "session.status_decided";

/// How long a decision row is kept. This table is 208k rows on a 2.2GB
/// database and a new writer needs a retention story on the day it ships, not
/// after someone notices. Two weeks comfortably covers "I screenshotted this
/// yesterday, what happened", which is the whole use case.
fn retention_days() -> f64 {
    std::env::var("AMUX_STATUS_HISTORY_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|d: &f64| *d > 0.0)
        .unwrap_or(14.0)
}

pub fn tick_secs() -> u64 {
    std::env::var("AMUX_STATUS_HISTORY_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|s: &u64| *s >= 5)
        .unwrap_or(TICK_SECS)
}

/// The last decision recorded for each lane, read from the DB rather than held
/// in memory.
///
/// In-memory state is fiction in this process: it re-execs on every deploy, and
/// a builder cycle is minutes apart on a busy day. An in-memory "last seen" map
/// would be wiped by each adoption and re-record every lane's unchanged status
/// as a change — turning a deploy into a fleet-wide burst of false transitions,
/// which is the D1 lesson learned the expensive way on the report table.
fn last_recorded(conn: &Connection) -> std::collections::HashMap<String, (String, String)> {
    let mut out = std::collections::HashMap::new();
    let sql = format!(
        "SELECT e.session, e.data FROM session_events e \
         JOIN (SELECT session, MAX(id) AS mid FROM session_events \
               WHERE type='{EVENT}' GROUP BY session) m ON e.id = m.mid"
    );
    if let Ok(mut st) = conn.prepare(&sql) {
        if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        {
            for (s, d) in rows.flatten() {
                let v: serde_json::Value = serde_json::from_str(&d).unwrap_or(json!({}));
                out.insert(
                    s,
                    (
                        v["status"].as_str().unwrap_or("").to_string(),
                        v["decided_by"].as_str().unwrap_or("").to_string(),
                    ),
                );
            }
        }
    }
    out
}

/// Does this pair differ from what was last recorded? Split out so the change
/// rule is one testable thing rather than an `if` buried in the loop.
///
/// A FIRST SIGHTING IS A CHANGE. Otherwise a lane that has been stable since
/// before this job existed never appears at all, and its empty history reads as
/// "nothing to see" rather than "never sampled" — the failure this whole module
/// is about, reproduced inside it.
pub fn is_change(prev: Option<&(String, String)>, status: &str, decided_by: &str) -> bool {
    match prev {
        None => true,
        Some((s, d)) => s != status || d != decided_by,
    }
}

/// One pass. Returns (lanes seen, rows recorded, rows pruned).
pub fn tick(state: &AppState) -> (u32, u32, u32) {
    let Ok(conn) = state.store.read() else {
        return (0, 0, 0);
    };
    let mut signals = crate::api::sessions_legacy::FleetSignals::load(&conn);
    let prev = last_recorded(&conn);
    // DROP THE POOLED CONNECTION BEFORE THE TMUX FAN-OUT (reviewer note, random
    // 2026-08-26). `capture_panes` shells out across ~52 lanes and needs no DB;
    // holding an r2d2 handle across it kept a pooled connection checked out for
    // the whole fan-out. Writers take their own connection, so this was never a
    // stall, only waste.
    drop(conn);
    // The same evidence the badge weighs. Without this the job would derive a
    // status from signals the dashboard does not have, and record a history
    // that disagrees with the thing it exists to explain.
    signals.capture_panes();

    let mut rows: Vec<(String, String)> = Vec::new();
    let mut lanes = 0u32;
    for name in crate::api::session_verbs::all_lane_names() {
        let running = signals.agent_running(&format!("amux-{name}"));
        if !running {
            continue;
        }
        lanes += 1;
        let (status, explain) = signals.derive_status_explain(&name, running);
        let decided_by = explain["decided_by"].as_str().unwrap_or("").to_string();
        if !is_change(prev.get(&name), &status, &decided_by) {
            continue;
        }
        let (ps, pd) = prev
            .get(&name)
            .map(|(a, b)| (a.clone(), b.clone()))
            .unwrap_or_else(|| (String::new(), String::new()));
        // `last_recorded_*`, NOT `prev_*` (reviewer note, random 2026-08-26).
        // These are the last SAMPLED decision, which is not always the
        // immediately preceding real one: within a single tick, A -> B -> C
        // records only C carrying A, so the row reads as if A and C were
        // adjacent while B was never seen. `prev_` claims an adjacency the
        // sampler cannot promise, and this feature's whole spine is not
        // overstating what an instrument knows.
        //
        // Carry the EVIDENCE, not only the verdict. "decided_by: report" a day
        // later is half an answer; the report's state and age are what let
        // someone check whether the rule was applied to something sane.
        let data = json!({
            "status": status,
            "decided_by": decided_by,
            "last_recorded_status": ps,
            "last_recorded_decided_by": pd,
            "report": explain.get("report").cloned().unwrap_or(serde_json::Value::Null),
            // The WHOLE pane block, not just `says_working` — the same argument
            // the reviewer made for keeping the report object, applied
            // consistently. Without `churn_distinct_frames`, "was the pane
            // actually painting then" was the one retrospective question a row
            // could not answer, which is an odd gap in a feature built for
            // retrospective questions.
            "pane": explain.get("pane").cloned().unwrap_or(serde_json::Value::Null),
            "idle_report_age_s": explain.get("idle_report_age_s").cloned().unwrap_or(serde_json::Value::Null),
            "subagents_live": explain.get("subagents_live").cloned().unwrap_or(serde_json::Value::Null),
        });
        rows.push((name, data.to_string()));
    }

    let recorded = rows.len() as u32;
    // PRUNE ON A CLOCK, NOT EVERY TICK. The delete predicate is on `ts` and
    // `type`, and no index covers both, so running it every 20s would scan for
    // rows that are almost never there. Once an hour keeps the table bounded at
    // a cost nobody has to think about.
    let due = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        n.is_multiple_of((3600 / tick_secs()).max(1))
    };
    let cutoff = crate::config::now_f64() - retention_days() * 86400.0;
    let pruned_cell = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let cell = pruned_cell.clone();
    if recorded > 0 || due {
        let _ = state.store.write(move |c| {
            let now = crate::config::now_f64();
            for (session, data) in &rows {
                c.execute(
                    "INSERT INTO session_events (ts, session, type, data, source) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![now, session, EVENT, data, JOB],
                )?;
            }
            if due {
                let n = c.execute(
                    "DELETE FROM session_events WHERE type=?1 AND ts < ?2",
                    rusqlite::params![EVENT, cutoff],
                )?;
                cell.store(n as u32, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        });
    }
    let pruned = pruned_cell.load(std::sync::atomic::Ordering::Relaxed);

    // ALWAYS logged. A tick that only speaks when something changed makes "the
    // fleet was stable" and "the job is wedged" the same silence.
    tracing::info!(job = JOB, lanes, recorded, "status-history census");
    (lanes, recorded, pruned)
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, tick_secs(), move || {
        let state = state.clone();
        async move {
            let _ = tokio::task::spawn_blocking(move || {
                tick(&state);
            })
            .await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The change rule, including the cell that makes an empty history honest.
    #[test]
    fn a_first_sighting_counts_as_a_change_and_a_repeat_does_not() {
        assert!(is_change(None, "idle", "report"), "a lane never recorded must be recorded");
        let prev = ("idle".to_string(), "report".to_string());
        assert!(!is_change(Some(&prev), "idle", "report"), "an unchanged lane writes nothing");
        assert!(
            is_change(Some(&prev), "active", "report"),
            "the STATUS changing is a change"
        );
        assert!(
            is_change(Some(&prev), "idle", "activity_fallback"),
            "the same status reached by a DIFFERENT RULE is the interesting case: the badge \
             reads identically and the reason it reads that way has moved"
        );
    }

    /// Retention and cadence are config, and their floors hold. A tick under 5s
    /// would turn a sampler into a hot loop over 52 pane captures.
    #[test]
    fn the_cadence_and_retention_floors_hold() {
        assert!(tick_secs() >= 5, "a sub-5s tick captures panes faster than they paint");
        assert!(retention_days() > 0.0, "a zero retention would delete every row it just wrote");
    }
}
