//! `[board-drive]` — the board→worker drive loop (AMUX-2637).
//!
//! # The outage this restores
//!
//! Python owned the entire loop that turns a board into work: `_pickup_next_
//! board_task` (py:14418), `_advance_open_card` (py:13325) and the level sweep
//! that drives both (`_pickup_level_sweep`, py:14305). The cutover carried the
//! board API across and left the loop behind, and the Rust orchestrator only
//! ever dispatches to RUST-managed workers (`pickup_unowned=false`; a
//! python-owned card carries a `foreign_worker_id` and is never assigned). Every
//! fleet session is python-owned, so since the cutover: no assignments, no
//! advance nudges. Cards sat in `todo`, workers sat idle, and NOTHING errored.
//!
//! Pure absence is the failure mode this module is shaped against. Every tick
//! writes a trace (`/api/debug/board-drive`) naming, per lane, what it did and
//! **why it declined** — because a skip that leaves no trace is
//! indistinguishable from a loop that is not running, which is exactly the state
//! the fleet was in for hours (ethos rule 4; Python learned the same thing at
//! py:14208, `_pickup_skip`, after Ethan had to read source to answer "why is
//! this idle lane sitting on 4 todos?").
//!
//! # LEVEL-triggered, never edge-triggered
//!
//! Python fired pickup from `status == "idle" and prev in ("active","waiting")`
//! — an EDGE — and then had to add `_pickup_level_sweep` because the edge is
//! fragile in exactly this process: it re-execs on every deploy, which clears
//! the previous-status map, so an already-idle lane waits for a transition that
//! may never come (primis: IDLE 142 minutes on 3 pickable todos, py:14308).
//! Only the LEVEL sweep is ported. There is no edge to keep in step with it, so
//! the asymmetry that let one half of the edge (advance) go unbackstopped for
//! months (py:14326) cannot recur here.
//!
//! # Delivery is the steering queue, not a second send path
//!
//! Nothing here types into a pane. A nudge or an assignment is
//! `steer_enqueue`'d and the EXISTING delivery loop
//! (`session_verbs::steer_deliver_tick`) applies the turn-boundary gate
//! (`steer_lane_at_boundary`) and the send choreography. Two consequences, both
//! deliberate:
//!
//! 1. A nudge can never land mid-turn. Interrupting a working model to tell it
//!    something it could have read at its next boundary is the interruption the
//!    ethos warns about, and it was a live bug in the steering path four days
//!    ago ("i sent as a queue but it looks like it was sent directly even though
//!    this worker was still working").
//! 2. Delivery is DURABLE. Python called `send_text(...)` and stamped its
//!    cooldown on the return value; a failed send meant the lane got nothing.
//!    Here the row survives a restart and is delivered at the lane's next
//!    boundary.
//!
//! # What is durable, and why that is not a detail
//!
//! Python held the per-session advance cooldown and the per-lane sweep stamp in
//! process memory, on a server that re-execs many times a day — its own comment
//! (py:14627) calls an in-memory cooldown "fiction" after one was wiped by the
//! first reload and re-fired at the same lane within minutes. Every cooldown
//! here reads `session_events`:
//!
//! | bound                     | key                                    |
//! |---------------------------|----------------------------------------|
//! | re-claim (24h, per card)  | `task.claimed`                         |
//! | advance (15m, per lane)   | `advance.nudged` / `needsyou.renag` / `capture.decompose_ask` |
//! | advance budget (3/24h)    | `advance.nudged` per card id           |
//! | decompose (6h, per lane)  | `pickup.decompose_nudge`               |
//! | claim reconcile (5m→1h)   | `task.claim_reconcile_requested` per exact card set |
//! | needs:you re-nag (3d)     | `needsyou.renag` per card id           |
//!
//! `advance.nudged` additionally records the card's STATUS at nudge time, which
//! Python kept only in memory (`_advance_last_card`, py:12960). That is what
//! makes "did this lane make progress since we spoke?" survive a restart, so a
//! lane that moved the card it was nudged about gets the next one immediately
//! instead of waiting out a cooldown it already earned its way past.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{OnceLock, RwLock};

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};

use crate::api::AppState;
use crate::db::board_store as bs;
use crate::db::PendingEvent;
use amux_core::board::TaskStatus;
use amux_core::revision::{EntityType, MutationKind};

/// Sweep cadence. Python's level sweep ran every 300s behind an idle EDGE that
/// fired immediately; with no edge, 300s would mean a lane finishing a turn
/// waits up to five minutes for its next card. Volume is bounded by the
/// cooldowns above, not by the tick, so the tick can be honest about latency.
const JOB: &str = "board-drive";
pub const BOARD_DRIVE_TICK_SECS: u64 = 60;

/// Local alias for the steering guard these deliveries carry. The string
/// lives once, in session_verbs, because that is where it is READ.
use crate::api::session_verbs::BOARD_DRIVE_GUARD as GUARD;

/// py:12961 `_ADVANCE_COOLDOWN` — never push the same lane twice inside this.
const ADVANCE_COOLDOWN_S: f64 = 15.0 * 60.0;
/// py:12855 `_DECOMPOSE_NUDGE_COOLDOWN`.
const DECOMPOSE_COOLDOWN_S: f64 = 6.0 * 3600.0;
/// Optional compatibility gate for operators that deliberately expire untouched
/// To Do cards. Disabled by default: age is a priority signal, not a hidden status.
pub(crate) fn pickup_freshness_s() -> i64 {
    std::env::var("AMUX_PICKUP_FRESHNESS_S")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(0)
}
pub(crate) fn pickup_fresh_cut(now: f64) -> i64 {
    match pickup_freshness_s() {
        seconds if seconds > 0 => (now as i64).saturating_sub(seconds),
        _ => i64::MIN / 2,
    }
}
/// Per-card re-claim cooldown (py:14515 / AMUX-1857), now a knob and much
/// SHORTER (AMUX-2987). It exempts a card from re-pickup for this long after it
/// was last claimed — so a card that was dispatched, returned to `todo`, and is
/// still todo cannot be re-dealt immediately. The inherited value was 86400
/// (24h), and that is what STALLED idle lanes: the bounce-breaker (the actual
/// anti-thrash mechanism) fires on 3 returns within 2h, so once a card rolls
/// out of that 2h window it is no longer a "recent bounce" — but the 24h
/// per-card cooldown kept it undispatchable for another 22 hours, during which
/// a running, idle, ready lane sat doing nothing while holding cards it could
/// not be handed (measured 2026-08-12: backend idle 80min on 8 todo cards, ALL
/// claimed 1-24h ago; 9 lanes fleet-wide in the same state). Violates the
/// no-stall guarantee (Invariant 10). Five minutes prevents an immediate hot
/// loop without turning one model lifecycle decision into a lane-wide stall.
/// A lane that truly cannot do a card moves it
/// to backlog-with-a-trigger or review (the honest exits the pickup prompt
/// names; a BARE backlog is not one on a drain-enabled lane, AF-579), so it never
/// re-enters this loop; only a card left in `todo` gets another turn.
fn reclaim_cooldown_s() -> f64 {
    std::env::var("AMUX_RECLAIM_COOLDOWN_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v >= 0.0)
        .unwrap_or(300.0)
}
/// Verify-nudge cooldown: once per 24h per session. A session that has no
/// todo/doing/review work but holds `done` cards gets a batched nudge to
/// verify them. 24h because verification requires prod evidence, and
/// checking more often than once a day for a low-priority idle task is the
/// nag shape that got the `done` tier removed in the first place.
const VERIFY_NUDGE_COOLDOWN_S: f64 = 24.0 * 3600.0;
/// Continue-nudge cooldown: when a session has no todo/doing cards but still
/// holds blocked/done work, nudge it to re-assess and keep going. 30 minutes
/// — short enough that a worker doesn't sit idle for long, long enough that
/// it's not a nag. Only fires for sessions with CC_AUTO_CONTINUE=1.
const CONTINUE_NUDGE_COOLDOWN_S: f64 = 30.0 * 60.0;
/// Backlog-triage nudge cooldown: once per 72h per session. Sessions with
/// 10+ backlog cards older than 14 days get a prompt to triage them
/// (archive stale ones, promote actionable ones to todo).
const BACKLOG_TRIAGE_COOLDOWN_S: f64 = 72.0 * 3600.0;
/// Minimum stale backlog cards before the triage nudge fires.
const BACKLOG_TRIAGE_THRESHOLD: usize = 10;
/// Cards older than this (created_at) are considered stale backlog.
const BACKLOG_STALE_AGE_S: i64 = 14 * 86400;
/// A `source_ref` trigger not re-verified within this window is STALE, and the
/// idle-drain gate now treats it exactly like no trigger at all. Matches the
/// dashboard's `is:sourcestale` definition (app.js) so "detectable by search"
/// and "detectable by the drain nudge" agree on what stale means.
///
/// Without this, `--trigger` is a one-way switch: a lane nudged to drain its
/// backlog can silence the nudge forever by writing ANY text into source_ref —
/// including a self-note ("do this later") rather than a genuine external
/// blocker — because `parked_on_live_trigger`-style checks only ever asked
/// "is source_ref non-empty", never "is it still true". Measured 2026-08-27:
/// 509 of 514 fleet-wide backlog cards carried a source_ref, 450 of those
/// (88%) had gone unverified >24h, several past 50 days — the idle-drain gate
/// built for exactly this stall (AMUX-3006) had been silently defeated by its
/// own escape hatch on nearly the whole fleet's backlog. This constant does
/// NOT touch `parked_on_live_trigger` (the depends_on auto-promotion guard,
/// board_drive.rs:1102) — that path stays conservative on purpose (MG-1388,
/// 2026-08-15: auto-promoting a parked card re-activated it five times against
/// the owner's explicit re-park). This only widens what counts as
/// un-drained for a NON-destructive NUDGE, which the owning session can
/// answer by either re-confirming the trigger (bumps `last_verified_at`) or
/// finally acting on the card — never by an automated status change.
const SOURCE_REF_STALE_S: i64 = 24 * 3600;

/// A source ref is an external-trigger block only while the owner has recently
/// verified it. Older refs still preserve message/provenance links but do not
/// make an otherwise actionable card undispatchable.
pub(crate) fn fresh_source_ref_trigger(row: &bs::IssueRow, now: i64) -> bool {
    row.source_ref.as_deref().is_some_and(|v| !v.trim().is_empty())
        && row.last_verified_at.is_some_and(|at| at > now - SOURCE_REF_STALE_S)
}
/// Idle-backlog DRAIN nudge cooldown, SCALED TO THE BACKLOG SIZE. Distinct from
/// the 72h stale-triage above — this fires when a lane is idle (no doing) with an
/// empty todo and a non-empty backlog OF ANY AGE, the "board doesn't drive to
/// completion" case (a lane holding only backlog sits idle forever because
/// board_drive dispatches `todo`, not `backlog`).
///
/// A flat 2h drained big idle backlogs far too slowly (Ethan 2026-08-13: backend
/// 207, mvs-infra 127 "just sit"). A lane idle on 200 un-worked cards should be
/// re-nudged far more often than one idle on 5, so the cadence scales with the
/// drainable backlog: base 2h for a small one, down to a 20m floor for a large
/// one. Still a NUDGE and nothing more — the worker chooses which cards to pull
/// and in what order (the ethos line: amux surfaces the stall, the model drives);
/// only the reminder frequency scales, never any server-side auto-promotion.
/// A hard `AMUX_IDLE_BACKLOG_DRAIN_COOLDOWN_S` override still wins for tuning.
/// AF-334 follow-up, AF-333-adjacent: the lane-activity map, computed ONCE per
/// tick instead of once per lane.
///
/// The first cut ran the activity query inside `drive_lane`, so it executed
/// once for each of ~50 lanes. Measured on the live DB: 62ms per lane against
/// 32,569 `task` rows, whether the lane was active or not, because the
/// `(entity_type, entity_id)` index selects on entity_type and `at` is not
/// indexed. That is ~3.1 SECONDS of DB work per tick, held under a read lock,
/// on a box where `/api/board` p95 is already 4.5s and AF-333's leading
/// hypothesis for that latency is "a cache or a lock". Adding three seconds of
/// lock-held scanning while investigating a lock-shaped latency bug is the
/// kind of thing that gets found later and attributed to something else.
///
/// One GROUP BY answers every lane in 68ms, so the cache exists to turn N
/// queries into one. The TTL only has to outlive a single tick's pass over the
/// lanes; 15s against a 30-minute window is a staleness of 0.8%, which cannot
/// flip the `> 0` test that consumes it.
static LANE_ACTIVITY: std::sync::Mutex<Option<(f64, std::collections::HashMap<String, i64>)>> =
    std::sync::Mutex::new(None);

const LANE_ACTIVITY_CACHE_TTL_S: f64 = 15.0;

fn lane_card_activity(
    conn: &rusqlite::Connection,
    window_min: i64,
) -> std::collections::HashMap<String, i64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    if let Ok(g) = LANE_ACTIVITY.lock() {
        if let Some((at, map)) = g.as_ref() {
            if now - at < LANE_ACTIVITY_CACHE_TTL_S {
                return map.clone();
            }
        }
    }
    let mut map = std::collections::HashMap::new();
    // datetime() on BOTH sides: `at` is RFC3339 with an explicit +00:00, and
    // comparing it to a localtime string applies a silent 4-hour skew that
    // reads every lane as inactive (see the AF-334 note at the call site).
    let rows = conn
        .prepare(
            "SELECT i.session, COUNT(*) FROM _amux_state_events e \
             JOIN issues i ON i.id = e.entity_id \
             WHERE e.entity_type='task' AND datetime(e.at) > datetime('now', ?1) \
             GROUP BY i.session",
        )
        .and_then(|mut st| {
            st.query_map(rusqlite::params![format!("-{window_min} minutes")], |r| {
                Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
        });
    match rows {
        Ok(v) => {
            for (sess, n) in v {
                map.insert(sess, n);
            }
            if let Ok(mut g) = LANE_ACTIVITY.lock() {
                *g = Some((now, map.clone()));
            }
        }
        // A failed probe must not read as "every lane is idle", which would
        // restore the exact over-nudging this fixes. Return an empty map WITHOUT
        // caching it, so the next tick retries rather than inheriting a blank.
        Err(e) => {
            tracing::warn!(
                target: "board_drive",
                "[drain-nudge AF-334] lane-activity probe failed: {e}. Lanes will \
                 read as inactive this tick, so drain nudges may over-fire."
            );
        }
    }
    map
}

/// AF-334: does the drain nudge fire? Pure, and extracted for the reason
/// AF-342 taught the hard way two hours earlier: a correct decision living as
/// one line inside an async job is a decision no test can reach, and that is
/// exactly where a fix ships inert with a green suite.
///
/// The fourth term is the new one. The first three are the original predicate
/// and they conflate "no card is in `doing`" with "the lane is doing nothing".
pub(crate) fn should_drain_nudge(
    doing_count: i64,
    eligible: i64,
    drainable_backlog: i64,
    recent_card_events: i64,
) -> bool {
    doing_count == 0 && eligible == 0 && drainable_backlog > 0 && recent_card_events == 0
}

fn idle_backlog_drain_cooldown_s(drainable_backlog: i64) -> f64 {
    if let Some(v) = std::env::var("AMUX_IDLE_BACKLOG_DRAIN_COOLDOWN_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
    {
        return v;
    }
    drain_cooldown_scaled(
        drainable_backlog,
        2.0 * 3600.0,
        env_f64("AMUX_IDLE_BACKLOG_DRAIN_FLOOR_S", 20.0 * 60.0),
        env_f64("AMUX_IDLE_BACKLOG_DRAIN_PER", 25.0),
    )
}

/// Per-lane nudge feedback state plus the cost the loop is actually charging.
///
/// `unheeded` is the count this tick would act on; `suppressed` is the set the
/// cap has silenced. `chars_24h` is the number that made AF-319 a card rather
/// than an opinion — drain nudges were 42% of all fleet messages over 34 hours,
/// and nothing in amux could say so without reading cmd_history by hand.
fn nudge_feedback_report(conn: &Connection) -> Value {
    let mut lanes: Vec<Value> = Vec::new();
    if let Ok(mut st) = conn.prepare(
        "SELECT session, COALESCE(unheeded,0), COALESCE(escalated_card,''), \
         COALESCE(last_nudge_at,0) FROM board_drive_nudge_state ORDER BY unheeded DESC, session",
    ) {
        if let Ok(rows) = st.query_map([], |r| {
            let card: String = r.get(2)?;
            Ok(json!({
                "session": r.get::<_, String>(0)?,
                "unheeded": r.get::<_, i64>(1)?,
                // Null, not "", so "never escalated" and "escalated to a card
                // whose id we lost" are not the same value.
                "escalated_card": if card.is_empty() { Value::Null } else { json!(card) },
                "last_nudge_at": r.get::<_, f64>(3)?,
            }))
        }) {
            lanes.extend(rows.flatten());
        }
    }
    let cap = nudge_unheeded_cap();
    let suppressed = lanes
        .iter()
        .filter(|l| l["unheeded"].as_i64().unwrap_or(0) >= cap)
        .count();
    // The cost, from the message ledger. `ts` there is MILLISECONDS — reading it
    // as seconds silently matches all history and reports ~3x the real volume,
    // which is exactly the mistake made while measuring this card.
    let cutoff_ms = (crate::config::now_f64() - 24.0 * 3600.0) * 1000.0;
    let (msgs, chars): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(text)),0) FROM cmd_history \
             WHERE ts > ?1 AND (text LIKE '%[amux]%Idle with%' \
                 OR text LIKE '%[amux auto-pickup]%' OR text LIKE '%[amux auto-continue]%')",
            rusqlite::params![cutoff_ms],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));
    let all_24h: i64 = conn
        .query_row("SELECT COUNT(*) FROM cmd_history WHERE ts > ?1", rusqlite::params![cutoff_ms], |r| r.get(0))
        .unwrap_or(0);
    crate::api::measured::measured(
        json!({
            "cap": cap,
            "suppressed_lanes": suppressed,
            "tracked_lanes": lanes.len(),
            "nudges_24h": msgs,
            "chars_24h": chars,
            "share_of_fleet_messages_24h": if all_24h > 0 {
                json!(((msgs as f64 / all_24h as f64) * 1000.0).round() / 10.0)
            } else {
                Value::Null
            },
            "lanes": lanes,
            "note": "`unheeded` counts consecutive drain nudges after which NO card in the \
                     lane changed. At `cap` the loop stops nudging and files one escalation \
                     card instead; any card movement resets it to 0 and nudging resumes with \
                     no action needed. Cadence doubles per unheeded repeat rather than \
                     staying at the 20m floor (AF-319).",
        }),
        all_24h.max(0) as usize,
    )
}

/// File the ONE card that replaces a nudge nobody is reading (AF-319).
///
/// Owner is the LANE, because the lane is who can dispose of its own backlog —
/// but the card is `chore` and says plainly that the nudging has stopped, so a
/// human reading the board sees a queue that has been declared stuck rather
/// than a lane that has merely gone quiet.
///
/// Exactly one per stuck stretch: `escalated_card` gates it, and the counter
/// resets to 0 the moment any card in the lane moves, which clears the gate.
async fn file_nudge_escalation(state: &AppState, lane: &str, backlog: i64, unheeded: i64) {
    let title = format!("{lane}'s backlog has not moved across {unheeded} nudges — nudging stopped");
    let desc = format!(
        "board_drive nudged this lane {unheeded} times in a row about its {backlog}-card \
         backlog and not one card changed in between. It has stopped nudging.\n\n\
         That is deliberate. Measured fleet-wide over 34h to 2026-08-30, drain nudges were \
         42% of all messages (419 of 988, ~143k tokens), and the hardest-nudged lanes moved \
         ZERO cards while the two least-nudged lanes did all the work. More nudges was not \
         the missing ingredient.\n\n\
         Nudging resumes automatically the moment ANY card in this lane changes. Nothing \
         here needs to be answered to restore it.\n\n\
         If the backlog is real work, pull one card. If it is not, this is the moment to say \
         so:\n\n\
         \x20 amux board discard <ID>                       not a unit of work\n\
         \x20 amux board backlog <ID> --trigger \"<cond>\"    real, waiting on something\n\
         \x20 amux board doing <ID>                         you are picking it up\n"
    );
    let new = crate::db::board_store::NewIssue {
        title,
        desc,
        status: "todo".into(),
        session: Some(lane.to_string()),
        // `chore`: nothing to implement or merge, so a code gate could not be
        // satisfied honestly (ethos rule 3).
        item_type: "chore".into(),
        // Exempt from the AF-317 todo WIP limit by name, for the same reason the
        // queue-disposition card is: this card exists BECAUSE the queue is too
        // long, so refusing it for queue depth would suppress its own alarm.
        creator: crate::api::board::QUEUE_DISPOSITION_CREATOR.into(),
        owner_type: "agent".into(),
        due: None,
        due_time: None,
        reviewer: None,
        shepherd: None,
        gate: vec![],
        depends_on: vec![],
        tags: vec!["nudge:escalated".to_string()],
        // Not an ask: this producer files ordinary cards, and a card
        // filed into needsyou without one is what AMUX-3929 is about.
        ask_type: None,
        ask_question: None,
        ask_unblocks: None,
        ask_actor: None,
        // AF-367: filed by the board drive loop.
        source: Some("board_drive".into()),
        requested_by: None,
        callback_session: None,
        callback_prompt: None,
    };
    let l = lane.to_string();
    let _ = state
        .store
        .write_async(move |conn| {
            let row = crate::db::board_store::create_issue(conn, &new, crate::config::now_f64() as i64)?;
            conn.execute(
                "UPDATE board_drive_nudge_state SET escalated_card=?1 WHERE session=?2",
                rusqlite::params![row.id, l],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
}

/// After this many consecutive drain nudges that moved nothing, STOP nudging
/// the lane and escalate once (AF-319).
///
/// Three, because the first repeat is a reasonable retry and the second is a
/// reasonable doubt.
///
/// SCOPE, corrected 2026-08-30: this catches a genuinely DEAD lane, and it is
/// NOT what drives the measured nudge volume. The claim it originally cited —
/// "mvs-infra took 80 nudges and moved 0 cards" — came from joining
/// `_amux_state_events` on entity_type='issue' (87 rows fleet-wide) when card
/// events are entity_type='task' (6,777). Corrected, mvs-infra logged 3,682
/// card events against 72 nudges and is the busiest board user in the fleet.
/// The hardest-nudged lanes are the most ACTIVE ones, so this cap will rarely
/// fire on them — by design, not by accident.
fn nudge_unheeded_cap() -> i64 {
    env_f64("AMUX_NUDGE_UNHEEDED_CAP", 3.0) as i64
}

/// The lane's board watermark: the newest `updated` across the cards a drain
/// nudge is asking it to work.
///
/// This is the FEEDBACK SIGNAL, and it is deliberately "did anything at all
/// change" rather than "did a card change status". A lane that retitled, retyped
/// or commented on one of its cards has engaged with the queue, and nudging it
/// again on the same cadence would be punishing a response we asked for.
fn lane_board_mark(conn: &Connection, lane: &str) -> i64 {
    conn.query_row(
        "SELECT COALESCE(MAX(updated),0) FROM issues WHERE session=?1 \
         AND deleted IS NULL AND COALESCE(archived,0)=0 AND owner_type='agent'",
        rusqlite::params![lane],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// What the nudge loop knows about a lane between ticks.
#[derive(Debug, Clone, Copy, Default)]
struct NudgeState {
    unheeded: i64,
    board_mark: i64,
    has_escalated: bool,
}

fn read_nudge_state(conn: &Connection, lane: &str) -> NudgeState {
    conn.query_row(
        "SELECT COALESCE(unheeded,0), COALESCE(board_mark,0), \
         COALESCE(LENGTH(escalated_card),0) > 0 \
         FROM board_drive_nudge_state WHERE session=?1",
        rusqlite::params![lane],
        |r| Ok(NudgeState { unheeded: r.get(0)?, board_mark: r.get(1)?, has_escalated: r.get(2)? }),
    )
    .unwrap_or_default()
}

/// Decide what this tick means, given the watermark then and now.
///
/// Split from the IO so the curve is testable without a store — the same reason
/// `drain_cooldown_scaled` is split out just below.
///
/// Returns (unheeded_after, moved). A first sighting (`prev_mark == 0`) counts
/// as movement so a lane is never escalated on the strength of a tick that had
/// nothing to compare against — ethos rule 4, applied to this function's own
/// first run.
fn nudge_feedback(prev_mark: i64, mark_now: i64, unheeded_before: i64) -> (i64, bool) {
    if prev_mark == 0 || mark_now > prev_mark {
        (0, true)
    } else {
        (unheeded_before + 1, false)
    }
}

/// Back off while the lane is not responding, instead of speeding up.
///
/// The bug this closes: cadence scaled with backlog SIZE and nothing scaled with
/// the OUTCOME, so the largest backlogs pinned themselves to the 20-minute floor
/// and stayed there. Doubling per unheeded repeat means an ignored lane goes
/// 20m, 40m, 80m and then stops, rather than 20m forever.
fn unheeded_backoff(cooldown_s: f64, unheeded: i64) -> f64 {
    cooldown_s * 2f64.powi(unheeded.clamp(0, 8) as i32)
}

/// Pure scaling: gentle `base` for a small backlog, halving roughly every `per`
/// cards, never below `floor`. Split out so the cadence curve is tested without
/// touching process env (the env-race trap the ethos file records).
fn drain_cooldown_scaled(drainable_backlog: i64, base: f64, floor: f64, per: f64) -> f64 {
    let divisor = (drainable_backlog as f64 / per.max(1.0)).max(1.0);
    (base / divisor).max(floor)
}
/// py:15673 `_DEFAULT_ITEM_TYPE` — strictest by default.
const DEFAULT_ITEM_TYPE: &str = "code";

use crate::config::env_f64;
use crate::config::env_i64;

/// py:14453 `AMUX_MAX_DOING_PER_SESSION`.
///
/// PUBLIC so the ready-frontier query reports the SAME capacity the drive loop
/// enforces (AMUX-3948). A frontier computing its own cap would offer a card the
/// gate then refuses, which is the view/mechanism split ethos rule 1 names.
pub(crate) fn wip_cap() -> i64 {
    env_i64("AMUX_MAX_DOING_PER_SESSION", 1).max(1)
}
/// py:13387 `AMUX_ADVANCE_CARD_BUDGET`.
fn advance_card_budget() -> i64 {
    env_i64("AMUX_ADVANCE_CARD_BUDGET", 3).max(1)
}
/// py:6885 `AMUX_NEEDSYOU_RENAG_DAYS`.
fn needsyou_renag_days() -> f64 {
    env_f64("AMUX_NEEDSYOU_RENAG_DAYS", 3.0)
}

use crate::config::now_f64;

// ---------------------------------------------------------------------------
// The trace. This is the product, not a byproduct.
// ---------------------------------------------------------------------------

/// What the tick did for ONE lane, and if it did nothing, WHY.
///
/// `reason` is a small closed vocabulary so it can be grepped and counted;
/// `detail` carries the specifics a human needs. A lane always produces exactly
/// one of these per tick — there is no path out of `drive_lane` that returns
/// without one, which is what makes "the loop is running but this lane is
/// skipped" distinguishable from "the loop is not running".
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LaneTrace {
    pub session: String,
    /// `assigned` | `advance-nudged` | `review-routed` | `decompose-asked` |
    /// `claim-reconcile` | `renag` | `verify-nudge` | `backlog-triage` |
    /// `skipped`
    pub outcome: String,
    pub reason: String,
    pub detail: String,
    /// Cards this lane could be handed RIGHT NOW, by the pickup predicate
    /// itself — not a re-derivation of it (ethos rule 1: a view must share the
    /// predicate of the mechanism it describes).
    pub eligible_todos: i64,
    /// Agent-owned, non-archived doing/review cards — what the advance half
    /// selects over.
    pub open_cards: i64,
    pub card: Option<String>,
}

impl LaneTrace {
    fn skip(session: &str, reason: &str, detail: impl Into<String>) -> Self {
        Self {
            session: session.to_string(),
            outcome: "skipped".into(),
            reason: reason.into(),
            detail: detail.into(),
            eligible_todos: 0,
            open_cards: 0,
            card: None,
        }
    }
    fn acted(session: &str, outcome: &str, card: &str, detail: impl Into<String>) -> Self {
        Self {
            session: session.to_string(),
            outcome: outcome.into(),
            reason: String::new(),
            detail: detail.into(),
            eligible_todos: 0,
            open_cards: 0,
            card: Some(card.to_string()),
        }
    }
    fn with_counts(mut self, eligible: i64, open: i64) -> Self {
        self.eligible_todos = eligible;
        self.open_cards = open;
        self
    }
    fn with_card(mut self, card: &str) -> Self {
        self.card = Some(card.to_string());
        self
    }
}

/// One sweep's worth of traces.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DriveReport {
    pub tick: u64,
    pub started_at: f64,
    pub finished_at: f64,
    pub assigned: usize,
    pub nudged: usize,
    /// Cards re-activated from `backlog` to `todo` this tick because every one
    /// of their `depends_on` dependencies reached a terminal status. Surfaced
    /// here so a promotion (or its absence) is visible in
    /// `/api/debug/board-drive` without reading logs (ethos rule 4).
    pub promoted: usize,
    /// Cards whose `depends_on` are all terminal but which stayed parked in
    /// `backlog` because the owner set a live `source_ref` trigger (a wake
    /// condition that is not "deps done"). Counted so the promotion pass HOLDING
    /// a card is as visible as a promotion — the interaction that made MG-1388
    /// look like it "wouldn't stay parked" was invisible in this report until
    /// the field existed (ethos rule 4).
    pub held_on_trigger: usize,
    /// Cards re-activated this tick because their own revisit date arrived.
    pub promoted_due: usize,
    /// Every backlog card whose revisit date has arrived, promoted or not.
    /// Published beside `promoted_due` because the two come apart on purpose:
    /// the drain fills a lane's `todo` to a ceiling and stops, so a large
    /// `revisit_due_total` with a small `promoted_due` is the rate limit
    /// working, and is indistinguishable from a broken scan without this field
    /// (ethos rule 4 — a count that can read zero must publish whether the
    /// measurement ran).
    pub revisit_due_total: usize,
    /// Epic capture roots completed this tick because every linked child is in
    /// a terminal state. Without this, the original Messages chip remains
    /// permanently non-terminal after all of the actual work has finished.
    pub completed_epics: usize,
    /// `doing` cards moved back to `backlog` because an explicit blocker or
    /// unfinished dependency means no worker can honestly be working them.
    /// Published so a zero can be distinguished from a sweep that never ran.
    pub normalized_blocked_doing: usize,
    pub lanes: Vec<LaneTrace>,
}

static LAST_REPORT: OnceLock<RwLock<Option<DriveReport>>> = OnceLock::new();
static TICK_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
type LaneDriveKey = (usize, String);
type LaneDriveMutex = std::sync::Arc<tokio::sync::Mutex<()>>;
static LANE_DRIVE_LOCKS: OnceLock<std::sync::Mutex<HashMap<LaneDriveKey, LaneDriveMutex>>> =
    OnceLock::new();

/// A periodic fleet sweep and an event-driven wake can reach the same lane at
/// once. The board claim itself is a CAS, but the losing pass can observe the
/// winner's fresh `task.claimed` marker and mistake it for restart recovery,
/// producing a second prompt. Serialize the complete decide→claim→enqueue
/// operation per store and lane; different workers and test stores still run
/// independently.
fn lane_drive_lock(state: &AppState, lane: &str) -> LaneDriveMutex {
    let key = (
        std::sync::Arc::as_ptr(&state.store) as usize,
        lane.to_string(),
    );
    LANE_DRIVE_LOCKS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .entry(key)
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

fn report_slot() -> &'static RwLock<Option<DriveReport>> {
    LAST_REPORT.get_or_init(|| RwLock::new(None))
}

fn publish(report: &DriveReport) {
    if let Ok(mut slot) = report_slot().write() {
        *slot = Some(report.clone());
    }
}

/// The last completed sweep, or None if the loop has never run — which is
/// itself the answer to "is the drive loop alive?", and the question nobody
/// could answer during the outage.
pub fn last_report() -> Option<DriveReport> {
    report_slot().read().ok().and_then(|s| s.clone())
}

// ---------------------------------------------------------------------------
// The fleet seam. A trait so every guard is testable against a planted fleet
// without a tmux server — mocking `subprocess` proves only that the mock was
// called (the D6 lesson).
// ---------------------------------------------------------------------------

#[allow(async_fn_in_trait)]
pub trait Fleet: Send + Sync {
    /// Lane names to consider: every non-archived session env.
    fn lanes(&self) -> Vec<String>;
    /// py:14444 — OPT-OUT, not opt-in. `CC_AUTO_PICKUP=0|false|no|off` declines
    /// the autonomous loop; anything else (including absent) enrolls. As opt-IN
    /// this reached 4 of 101 sessions, so 97 lanes went idle on a full queue and
    /// stayed there (py:14437).
    fn auto_pickup_enabled(&self, lane: &str) -> bool;
    /// `CC_TAGS`, lowercased — for explicit-mode status scoping (py:16096).
    fn tags(&self, lane: &str) -> Vec<String>;
    /// Isolation is a selection veto, not merely a delivery refusal: claiming
    /// first would strand work with a raw worker that must not be automated.
    fn is_isolated(&self, _lane: &str) -> bool { false }
    async fn is_running(&self, lane: &str) -> bool;
    /// The turn-boundary gate. REUSED from the steering path, never
    /// reimplemented: a second copy of "is this lane mid-turn" is the
    /// two-implementations-of-one-rule defect the board keeps producing.
    async fn at_boundary(&self, lane: &str) -> bool;
    /// The provider has identity-backed child work (a subagent or background
    /// terminal) alive for this lane.  This is stronger than a generic
    /// `active` badge: it is the signal that a parent task may have been
    /// parked in To Do while its delegated work kept running.
    fn active_child_work(&self, _lane: &str) -> bool { false }
    /// When a session runs out of todo cards but still has blocked/done work,
    /// keep nudging it to re-assess and continue. ON BY DEFAULT since
    /// 2026-08-11 (Ethan: "standing order whenever idle to take care of any
    /// non-terminal board task"); CC_AUTO_CONTINUE=0 opts a lane out. The
    /// explicit =1 additionally implies YOLO (is_yolo_enabled); the default
    /// deliberately does not.
    fn auto_continue_enabled(&self, lane: &str) -> bool;
    /// Start a stopped worker through the canonical provider-aware launcher.
    /// Default refusal makes a new Fleet opt in explicitly to autonomous wake.
    async fn start_for_dispatch(&self, lane: &str) -> Result<(), String> {
        Err(format!("fleet cannot start worker '{lane}' for board dispatch"))
    }
    /// Hand text to the lane. Durable queue + the existing delivery loop.
    async fn deliver(&self, lane: &str, text: &str);
    /// Work delivery reports a queue refusal so its just-made board claim can
    /// be compensated; legacy nudge delivery remains fire-and-forget.
    async fn deliver_work(&self, lane: &str, text: &str) -> Result<(), String> {
        self.deliver(lane, text).await;
        Ok(())
    }
    /// Queue one recovery delivery for one worker generation. The stable id is
    /// the crash boundary: an implementation with a durable queue reports an
    /// existing commitment instead of enqueueing the same resume again.
    async fn deliver_resume(
        &self,
        lane: &str,
        text: &str,
        _delivery_id: &str,
    ) -> Result<ResumeDelivery, String> {
        self.deliver_work(lane, text).await?;
        Ok(ResumeDelivery::Queued)
    }

    /// Deliver a message whose text ASSERTS SOMETHING ABOUT A CARD (AMUX-3659).
    ///
    /// Same delivery, plus the card and the `issues.rev` the text was computed
    /// against, so the delivery loop can drop the row if the card moved on
    /// before the turn boundary arrived. Measured: 32% of steering is read more
    /// than a minute after its premise was computed, and on MR-2 that turned a
    /// correct trigger plus a correctly-applied remedy into what looked like a
    /// broken re-nag.
    ///
    /// DEFAULTED to plain `deliver` so a Fleet that does not care — every test
    /// fake — is unaffected, and so adding this could not silently change the
    /// behaviour of any existing caller.
    async fn deliver_about(&self, lane: &str, text: &str, _card: &str, _rev: i64) {
        self.deliver(lane, text).await;
    }
}

/// The live fleet: session envs on disk, `session_verbs`' liveness and boundary
/// gate, `steer_enqueue` for delivery.
pub struct LiveFleet {
    pub state: AppState,
    // One snapshot for this entire drive pass, never one full load per lane.
    signals: Option<crate::api::sessions_legacy::FleetSignals>,
}

impl Fleet for LiveFleet {
    fn lanes(&self) -> Vec<String> {
        // ISOLATED LANES ARE NOT DRIVEN (Ethan, 2026-08-26: "we have an isolated
        // worker but it still has amux shit", naming gtm-research, which had
        // CC_ISOLATED=1 and was being auto-picked-up and nudged).
        //
        // FILTERED HERE, at the ONLY consumer of `lanes()`, rather than at
        // `deliver`. Gating the send would let auto-pickup CLAIM a card for an
        // isolated lane and then silently not deliver it — the card sits in
        // `doing` with nobody working it, which is worse than the bug. Removing
        // the lane from consideration means the card is never claimed at all.
        //
        // `session_is_isolated`'s doc calls itself "the single source of truth
        // every isolation decision consults" and lists seven consumers, all
        // about what the worker is TOLD ABOUT or DISCOVERABLE BY. None covered
        // what gets typed INTO its pane: measured 2026-08-26, ZERO of the 15
        // runtime_jobs consulted it, and three of them steer. The designation
        // promised "the amux harness stripped" and delivered env suppression.
        //
        // The owner's peek/send are untouched — that is the documented boundary.
        crate::api::session_verbs::all_lane_names()
            .into_iter()
            .filter(|l| !crate::api::session_verbs::session_is_isolated(l))
            .collect()
    }
    fn auto_pickup_enabled(&self, lane: &str) -> bool {
        crate::api::session_verbs::standing_orders_on(lane, "CC_AUTO_PICKUP")
    }
    fn tags(&self, lane: &str) -> Vec<String> {
        let cfg = crate::api::session_verbs::parse_env(lane);
        cfg.get("CC_TAGS")
            .unwrap_or("")
            .split(',')
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect()
    }
    fn is_isolated(&self, lane: &str) -> bool {
        crate::api::session_verbs::session_is_isolated(lane)
    }
    fn auto_continue_enabled(&self, lane: &str) -> bool {
        crate::api::session_verbs::standing_orders_on(lane, "CC_AUTO_CONTINUE")
    }
    async fn is_running(&self, lane: &str) -> bool {
        crate::api::session_verbs::is_running(lane).await
    }
    async fn at_boundary(&self, lane: &str) -> bool {
        let status = self.signals.as_ref().and_then(|signals| signals.turn_boundary_status(lane));
        if let Some(signals) = &self.signals {
            let (_, explain) = signals.derive_status_explain(lane, signals.agent_running(&format!("amux-{lane}")));
            tracing::debug!(target: "amux::board_drive", session = lane,
                measured = status.is_some(), n_considered = 1,
                verdict = %explain["decided_by"], at_boundary = status.as_deref() == Some("idle"),
                "board-drive boundary uses Workers structured truth");
        }
        status.as_deref() == Some("idle")
    }
    fn active_child_work(&self, lane: &str) -> bool {
        self.signals.as_ref().is_some_and(|signals| {
            let (_, explain) = signals.derive_status_explain(
                lane,
                signals.agent_running(&format!("amux-{lane}")),
            );
            explain["subagents_working"] == true
                || explain["provider_background_working"] == true
        })
    }
    async fn start_for_dispatch(&self, lane: &str) -> Result<(), String> {
        crate::api::session_verbs::start_for_board_dispatch(&self.state, lane).await
    }
    async fn deliver(&self, lane: &str, text: &str) {
        let _ = self.enqueue_work(lane, text).await;
    }
    async fn deliver_work(&self, lane: &str, text: &str) -> Result<(), String> {
        self.enqueue_work(lane, text).await
    }
    async fn deliver_resume(
        &self,
        lane: &str,
        text: &str,
        delivery_id: &str,
    ) -> Result<ResumeDelivery, String> {
        if !crate::api::session_verbs::is_running(lane).await {
            return Err("worker vanished before work delivery could be queued".into());
        }
        let result = crate::api::session_verbs::steer_enqueue_idempotent_report(
            &self.state,
            lane,
            text,
            GUARD,
            "",
            delivery_id,
        )
        .await
        .map_err(str::to_string)?;
        let disposition = match result.disposition {
            crate::api::session_verbs::StableEnqueueDisposition::New => {
                self.record_prompt(lane, text).await;
                ResumeDelivery::Queued
            }
            crate::api::session_verbs::StableEnqueueDisposition::AlreadyQueued => {
                ResumeDelivery::AlreadyQueued
            }
            crate::api::session_verbs::StableEnqueueDisposition::AlreadyDelivered => {
                ResumeDelivery::AlreadyDelivered
            }
        };
        Ok(disposition)
    }
    async fn deliver_about(&self, lane: &str, text: &str, card: &str, rev: i64) {
        let _ = crate::api::session_verbs::steer_enqueue_precond(
            &self.state.store, lane, text, GUARD, "", Some((card, rev)),
        )
        .await;
        self.record_prompt(lane, text).await;
    }

}

impl LiveFleet {
    /// A durable queue insert is the work-delivery commitment. Check liveness
    /// again after a wake so a vanished worker causes claim compensation rather
    /// than a false `doing` card.
    async fn enqueue_work(&self, lane: &str, text: &str) -> Result<(), String> {
        if !crate::api::session_verbs::is_running(lane).await {
            return Err("worker vanished before work delivery could be queued".into());
        }
        crate::api::session_verbs::steer_enqueue(&self.state, lane, text, GUARD, "")
            .await
            .map_err(str::to_string)?;
        self.record_prompt(lane, text).await;
        Ok(())
    }

    /// A PICKUP IS A PROMPT, AND NOTHING RECORDED IT (AMUX-3547 -> AMUX-3544).
    ///
    /// `/api/usage/attribution` answers "where did the plan window go" by
    /// matching every `token_ledger` turn to the most recent preceding
    /// `cmd_history` row for that session. `steer_enqueue` writes
    /// `steering_queue` and `steering_history`; only the SEND path writes
    /// `cmd_history`. Auto-pickup goes through the former, so it wrote no row
    /// at all.
    ///
    /// Measured over 24h before this fix: **247 auto-pickup deliveries in
    /// `steering_history`, 2 in `cmd_history`.** So ~245 pickups' turns were
    /// attributed to whatever prompt happened to precede them — and when that
    /// was the human's, they landed under `user` / "you typed it", whose
    /// `is_background` is FALSE.
    ///
    /// The bias therefore runs toward UNDER-reporting background, which is the
    /// one direction that matters here: AMUX-3542 is a customer whose plan
    /// window was eaten by background work, and the instrument built to prove
    /// it was crediting some of that work to the customer's own typing.
    ///
    /// `pickup` as its own type rather than reusing `system` or `direct`: those
    /// already mean other things in this table, and the question a reader asks
    /// of this row ("did amux hand me this, or did I ask for it?") deserves its
    /// own answer instead of being inferred from text.
    async fn record_prompt(&self, lane: &str, text: &str) {
        let (lane, text) = (lane.to_string(), text.to_string());
        let ts = crate::runtime_jobs::registry::unix_now();
        let _ = self
            .state
            .store
            .write_async(move |conn| {
                // Best-effort: a missing attribution row must never stop a
                // delivery. The pickup is the product; this is its receipt.
                let _ = conn.execute(
                    "INSERT INTO cmd_history (text, type, session, ts, origin) \
                     VALUES (?1, 'pickup', ?2, ?3, 'board-drive')",
                    rusqlite::params![text, lane, (ts * 1000.0) as i64],
                );
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .await;
    }
}

// ---------------------------------------------------------------------------
// Predicates, ported. Every one of these was a logged incident.
// ---------------------------------------------------------------------------

/// py:12889 `_pickup_junk_reason` — why auto-pickup must refuse this card, or
/// "" if it is a real task.
///
/// ORDER IS LOAD-BEARING (py's "MG ground truth, second revision"): marker ->
/// artifact/dormant -> STRUCTURE VETO -> journal -> shell. Structure beats the
/// fold count because a real investigation card can carry fold RESIDUE from the
/// folding era; a true journal has folds and no structure.
pub fn pickup_junk_reason(title: &str, desc: &str, log: &str) -> String {
    use std::sync::OnceLock;
    static ARTIFACT: OnceLock<regex::Regex> = OnceLock::new();
    static CAPS_HEAD: OnceLock<regex::Regex> = OnceLock::new();
    static STRUCTURE: OnceLock<regex::Regex> = OnceLock::new();
    static PROMPT: OnceLock<regex::Regex> = OnceLock::new();

    // The card's HISTORY (`log`) is legitimate signal for the STRUCTURE veto and
    // the FOLD count (content that can live in either field), so those read the
    // combined blob. The CAPTURE brand does NOT: it must read the card's CURRENT
    // DEFINITION (`desc`) only. `capture: session prompt` is a DURABLE LOG marker
    // minted once at capture (session_verbs.rs) that NEVER clears, so a card
    // auto-captured and then RESHAPED into a real task (desc rewritten, retyped)
    // carries it in the log forever. Reading it from the blob re-branded a
    // reshaped card "a captured chat prompt" on every 6h decompose tick and
    // nagged a card the session had already fixed (AMUX-3187). A fresh capture's
    // desc ALWAYS begins "**Prompt:** " (session_verbs.rs:2574), so the anchored
    // PROMPT check below still catches every real capture on its CURRENT desc,
    // and reshaping the desc — the sanctioned exit — now actually works.
    let blob = if log.trim().is_empty() {
        desc.to_string()
    } else {
        format!("{desc}\n{log}")
    };
    let folds = blob.matches("New task:").count();
    // The marker on the CURRENT desc still brands (a card literally defined as the
    // capture marker is a shell); but read `desc`, NOT the blob, so the durable
    // LOG copy of a reshaped card does not (AMUX-3187, see above).
    // ANCHORED (AF-569). This used to be `desc.contains(...)`, which brands a card
    // that merely MENTIONS the marker. Specimen: AF-568, the card about duplicate
    // capture cards, quoted the string inside backticks while explaining the bug
    // and was nudged "captured chat prompt, not a unit of work" while it sat in
    // `doing` with a shipped fix. A card cannot describe the capture mechanism
    // without being classified as its output.
    //
    // The intent stated two paragraphs up is "a card literally defined as the
    // capture marker", and starts_with is that intent. Every REAL capture is still
    // caught by the anchored PROMPT check below, on `**Prompt:** `, which is the
    // prefix session_verbs.rs actually mints.
    //
    // It also fired BEFORE the STRUCTURE VETO below, so a substring match
    // short-circuited the 2+ ALLCAPS-heads evidence that exists to stop this. AF-568
    // had three such heads.
    if desc.trim_start().starts_with("capture: session prompt") && folds < 2 {
        return "captured chat prompt, not a unit of work".into();
    }
    // ANCHORED, and the word must END as a subject too (GCA-85 + creative-dna's
    // residual): `\b` matches at a hyphen, and the fleet's own title convention
    // is `[area] subject`, so `[test-hygiene]` fired on `test`. The comma in the
    // lookahead is load-bearing — "[TRIPWIRE, fires on recurrence]" is a genuine
    // armed tripwire.
    let artifact = ARTIFACT.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^\s*\[?(probe-stale|probe|temp|test|canary|tripwire|armed watch)([\s:,\]]|$)",
        )
        .expect("artifact regex")
    });
    if artifact.is_match(title) {
        return "looks like a test artifact or armed tripwire".into();
    }
    // STRUCTURE VETO. 2+ ALLCAPS section heads, or an explicit structure marker.
    let caps = CAPS_HEAD.get_or_init(|| {
        regex::Regex::new(r"(?m)^[A-Z][A-Z0-9 /'\-]{3,40}:").expect("caps head regex")
    });
    let structure = STRUCTURE.get_or_init(|| {
        regex::Regex::new(
            r"(?im)^#{1,3}\s|success criteri|acceptance criteri|^SCOPE:|^- \[[ x]\]|gate(?:_checked| policy| criteria)\b|ROOT CAUSE|unhappy path",
        )
        .expect("structure regex")
    });
    if caps.find_iter(&blob).count() >= 2 || structure.is_match(&blob) {
        return String::new();
    }
    if folds >= 2 {
        return format!("journal card ({folds} folded tasks)");
    }
    // Anchored on the CURRENT `desc`, not the blob: a fresh capture's desc begins
    // "**Prompt:** " and a reshaped card's does not, which is what lets the
    // reshape clear the brand (AMUX-3187).
    let prompt = PROMPT
        .get_or_init(|| regex::Regex::new(r"(?s)^\s*\*\*Prompt:\*\*\s*(?:\[[^\]]*\]\s*)?(.*)$").expect("prompt regex"));
    if let Some(c) = prompt.captures(desc.trim()) {
        let body = c.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        return if body.starts_with('/') {
            "harness slash command, not a task".into()
        } else {
            "captured chat prompt, not a unit of work".into()
        };
    }
    String::new()
}

/// py:14597 — an irreversible operation named in a card is never auto-executed.
pub fn irreversible_op(blob: &str) -> Option<String> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)stash\s+(drop|clear)|rm\s+-[rf]{1,2}\b|push\s+(--force|-f)\b|reset\s+--hard|git\s+clean\s+-[a-z]*[fd]|drop\s+table|truncate\s+table|delete\s+from\s+\w+\s*;|--no-preserve-root",
        )
        .expect("danger regex")
    });
    re.find(blob).map(|m| m.as_str().trim().to_string())
}

/// Find a possible dependency mention for a worker-facing recheck hint only.
/// Never use prose to veto pickup: historical logs, self-references and citations
/// cannot establish the current dependency graph. `depends_on` is authoritative.
pub fn prose_dependency(blob: &str) -> Option<String> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            // Case-insensitive on the PHRASE ONLY — `(?i:...)` scoped, never
            // global, because a global flag would also lowercase the id class
            // and start matching "amux-2948" citations. And `wait(?:s|ing)?`
            // rather than `waits?`: "waiting on X" is how people actually
            // write it (AMUX-2950; measured before widening: 19 of 829 open
            // cards newly block, incl. TG-3195 whose title says "WAITS ON
            // TG-3193 DEPLOY" in caps and was dispatching anyway).
            r"(?:(?i:blocked\s+(?:by|on)|cannot\s+start\s+until|depends\s+on|wait(?:s|ing)?\s+(?:for|on)))\s+[\s\S]{0,40}?\b([A-Z][A-Z]+-\d+)(?:[^0-9]|$)",
        )
        .expect("prose dep regex")
    });
    // DIRECTION, not just presence (AMUX-2948). The phrase alone does not say
    // WHICH card is blocked, and the reverse construction is ordinary English
    // on a well-written card:
    //
    //     "Evidence record: BACKE-3272. Fix that depends on this: BACKE-3276."
    //
    // That declares BACKE-3276 a DEPENDENT of this card. The matcher read it as
    // "this is blocked by BACKE-3276" and refused to dispatch — and because the
    // refusal guard runs per card, every todo carrying a downstream-work note
    // was held. Measured on backend 2026-08-11: 17 eligible todos, ALL refused,
    // lane idle with auto-pickup enabled and the drive loop running. The
    // skip detail even told the operator to "POPULATE depends_on", which is
    // sound advice that nobody had followed on any of the 17.
    //
    // `this`/`these`/`the above` between the phrase and the id is the tell: the
    // subject of the dependency is THIS card, so the id named is the dependent,
    // not the blocker. This narrows the hint without making a dispatch
    // decision: ambiguous prose remains context for the worker to reconcile.
    let c = re.captures(blob)?;
    let whole = c.get(0)?.as_str();
    let id = c.get(1)?.as_str();
    let between = &whole[..whole.len() - id.len().min(whole.len())];
    let reversed = ["this", "these", "the above", "us"]
        .iter()
        .any(|w| between.to_ascii_lowercase().contains(w));
    if reversed {
        return None;
    }
    Some(id.to_string())
}

#[cfg(test)]
mod drain_trigger_tests {
    use super::should_drain_nudge;

    /// AF-334, with the MEASURED numbers rather than invented ones.
    ///
    /// The defect: the trigger read `doing_count == 0 && eligible == 0 &&
    /// drainable_backlog > 0`, which a lane that works fast and keeps `todo`
    /// drained satisfies continuously. Measured 2026-08-30 at the 30-minute
    /// resolution the trigger actually runs at: mvs-infra's last six drain
    /// nudges each landed with 63-79 card events in the preceding half hour.
    /// Told it was idle every 16 minutes while mutating a card every ~25s.
    ///
    /// THE CONTROL IS THE HALF THAT MATTERS. A term that simply silenced the
    /// nudge would pass a test that only checks the busy case, and would
    /// re-open the bug this nudge exists for (tubescience: 51 backlog / 0 todo
    /// / 0 doing, idle forever because board-drive dispatches only `todo`).
    /// Both cells below are real observations from the same window.
    #[test]
    fn a_demonstrably_working_lane_is_not_told_it_is_idle() {
        // mvs-infra, nudge at 19:07:49Z: 79 card events in the prior 30 min.
        assert!(
            !should_drain_nudge(0, 0, 40, 79),
            "a lane mutating a card every ~25s must not be told it is idle"
        );
        // This session, 2 of its 4 nudges in the same window: 25-27 events.
        assert!(!should_drain_nudge(0, 0, 10, 27));

        // CONTROL 1 — the genuinely stalled lane the nudge was built for.
        // Same three original terms, zero activity: it MUST still fire.
        assert!(
            should_drain_nudge(0, 0, 51, 0),
            "tubescience's 51-backlog/0-todo/0-doing case must still be nudged"
        );
        // CONTROL 2 — this session's OTHER 2 nudges, which were correct: the
        // lane had 0 card events because it was waiting on a build. Those are
        // not false positives and must survive the fix.
        assert!(should_drain_nudge(0, 0, 10, 0));

        // The original three terms still gate: nothing to drain, or already
        // working a card, or a dispatchable todo exists.
        assert!(!should_drain_nudge(0, 0, 0, 0), "no drainable backlog");
        assert!(!should_drain_nudge(1, 0, 10, 0), "already has a card in doing");
        assert!(!should_drain_nudge(0, 1, 10, 0), "has a dispatchable todo");
    }
}

#[cfg(test)]
mod prose_direction_tests {
    use super::prose_dependency;

    /// THE SPECIMEN, verbatim from BACKE-3278 (AMUX-2948). This sentence
    /// declares a DEPENDENT, and reading it as a blocker held backend's entire
    /// queue: 17 eligible todos, all refused, lane idle with auto-pickup on.
    #[test]
    fn a_downstream_dependent_does_not_block_this_card() {
        let blob = "Evidence record: BACKE-3272. Fix that depends on this: BACKE-3276.                     Migration (owner-gated): BACKE-3277.";
        assert_eq!(
            prose_dependency(blob),
            None,
            "\"depends on this: X\" names X as the DEPENDENT — this card is not blocked by it"
        );
    }

    /// Forward dependency language remains useful context for a recheck hint.
    /// The selector tests below cover the structured execution gate separately.
    #[test]
    fn forward_dependency_language_is_reported_as_a_hint() {
        // AMUX-2950 closed both gaps these fixtures originally mis-asserted:
        // the phrase match is now case-insensitive (scoped, so the ID class is
        // not) and takes "waiting" as well as "wait/waits". Widened only after
        // measuring: 19 of 829 open cards newly block, and the sample includes
        // a real missed blocker (TG-3195, "WAITS ON TG-3193 DEPLOY", caps).
        for blob in [
            "This is blocked by BACKE-3276 until the cache lands.",
            "Cannot start until BACKE-3276 ships.",
            "depends on BACKE-3276",
            "waiting on BACKE-3276 to land",
            "Waits on BACKE-3276.",
            "blocked on BACKE-3276",
        ] {
            assert_eq!(
                prose_dependency(blob),
                Some("BACKE-3276".to_string()),
                "must still surface the hint: {blob:?}"
            );
        }
    }

    /// A bare citation was never matched and must stay unmatched — the guard
    /// keys on dependency LANGUAGE, which is what makes it usable at all on a
    /// fleet whose cards cite each other constantly (backend's 17 todos carry
    /// up to 9 citations each).
    #[test]
    fn a_bare_citation_is_not_a_dependency() {
        assert_eq!(prose_dependency("See BACKE-3276 for the measurement."), None);
        assert_eq!(prose_dependency("Split from BACKE-3276; supersedes AC-12."), None);
    }
}

/// py:13126 `_norm_actor` — a session name reduced to a comparable form.
/// `cmd_history.origin` is NOT a clean session id: real values in one night were
/// `mixpeek-frustrations`, `mixpeek frustrations`, and
/// `mixpeek frustrations [manual:ip:100.66.26.84]`.
pub fn norm_actor(name: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"[^a-z0-9]+").expect("norm regex"));
    re.replace_all(name.trim().to_lowercase().as_str(), "-").trim_matches('-').to_string()
}

/// Strip a bracketed/parenthesized decoration BEFORE normalizing, then demand
/// EXACT equality (AC-316 defect 2). `_norm_actor` flattens lane separators AND
/// decorations to `-`, so post-norm "amux (queued...)" and "amux-cloud" both
/// begin "amux-" and a prefix test made every `amux-*` lane count as the
/// reviewer `amux`.
fn origin_matches(origin: &str, want_normed: &str) -> bool {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"[\[(][\s\S]*$").expect("decoration regex"));
    norm_actor(&re.replace(origin, "")) == want_normed
}

/// A card's own text, quoted into a board-drive message — or a POINTER to the
/// card, when quoting it would open Claude Code's @/slash picker.
///
/// `send_text_inner` refuses a steering message that trips the picker while the
/// lane is generating, and `steer_deliver_tick` takes the oldest row per lane
/// per tick — so a refused message sits at the head and blocks everything
/// behind it (measured live: 10 messages stuck 4 hours on one lane). That
/// head-of-line bug is the SEND PATH's to fix and another lane owns it; this is
/// the narrower obligation not to MANUFACTURE the hazard, since the only
/// untrusted text in a board-drive message is the card body being quoted.
///
/// Uses `at_picker_text` — the send path's own predicate, called rather than
/// copied. A second spelling of "does this open the picker" would drift from
/// the one that actually decides (AMUX-2330).
///
/// EXIT: delete this when the send path stops refusing on picker text (or when
/// delivery is protocol-based, where there is no composer to confuse). Nothing
/// about a card's text should decide whether its lane hears about it.
fn quoted_card_text(body: &str, card: &str) -> String {
    if crate::api::session_verbs::at_picker_text(body) {
        return format!(
            "(withheld: this card's text contains an at-mention or a leading slash, which \
             opens Claude Code's file picker and can make the send refuse — read it with \
             `amux board show {card}`)"
        );
    }
    body.to_string()
}

/// py:13112 `_advance_target` — the status a card in `status` moves to next.
pub fn advance_target(status: &str) -> Option<TaskStatus> {
    match status.trim().to_lowercase().as_str() {
        "doing" => Some(TaskStatus::Review),
        "review" => Some(TaskStatus::Done),
        "done" => Some(TaskStatus::Verified),
        _ => None,
    }
}

/// py:13117 `_reviewer_acts_next` — DERIVED from the enforcement set
/// (`_REVIEWER_SIGNOFF_TARGETS = ("done","verified")`), never restated beside
/// it. Widening the enforcement set moves the routing with it; py:13036 records
/// three bugs in one night from exactly that pair drifting.
pub fn reviewer_acts_next(status: &str) -> bool {
    matches!(advance_target(status), Some(TaskStatus::Done) | Some(TaskStatus::Verified))
}

// ---------------------------------------------------------------------------
// DB reads
// ---------------------------------------------------------------------------


/// Has the REVIEWER written to this card since it entered `review`? (AF-214)
///
/// The reviewer nudge fires on `status == review AND reviewer == you`, and its
/// own instruction — "if not, say what fails on the card" — is a DESC write that
/// does NOT change status. So a reviewer who rejects work exactly as told leaves
/// the card in the state that re-fires the nudge, and gets asked to review their
/// own rejection until the 24h budget runs out. amux hit that twice on AF-203 in
/// one afternoon.
///
/// The real fix is a status for it (`changes-requested`): `review` claims the
/// card awaits a REVIEWER when it awaits the AUTHOR, and `doing` reads as the
/// reviewer working it when the reviewer is finished. Neither cell exists, so the
/// board misdescribes the card whichever move the reviewer makes. That is a
/// vocabulary change and it is tracked on AF-214; this stops the wasted turns
/// without pretending to fix the lying status.
///
/// PARSED FROM THE LOG, and NOT from "the last line's author", which is what the
/// shape invites and would be wrong: the log carries `authz:` rows and
/// `commit <sha> — <subject>` rows that are not actor actions, so the newest line
/// is routinely neither party. Instead: find where the card last entered
/// `review`, then look for a line by the reviewer strictly after it.
fn reviewer_acted_since_review(log: &str, reviewer: &str) -> bool {
    let rev = reviewer.trim();
    if rev.is_empty() {
        return false;
    }
    let lines: Vec<&str> = log.lines().collect();
    // Last transition INTO review. Anything before it belongs to an older round
    // — a card can be reviewed, sent back, reworked and re-reviewed.
    let entered = lines
        .iter()
        .rposition(|l| l.contains("-> review"))
        .map(|i| i + 1)
        .unwrap_or(0);
    let needle = format!("` {rev}:");
    lines[entered.min(lines.len())..]
        .iter()
        .any(|l| l.contains(&needle))
}

/// Did the REVIEWER themselves move this card out of `review`? (AMUX-3820)
///
/// The other end of [`reviewer_acted_since_review`], and of the
/// `reviewer_unreachable` WARN. Both of those watch a card sitting IN `review`.
/// A reviewer who rejects work and then moves the card to `doing` — which is the
/// honest state, and exactly what AF-214 argued for — leaves that predicate BY
/// CONSTRUCTION, and nothing re-nudges them when the author is done.
///
/// THE SPECIMEN IS THIS FUNCTION'S AUTHOR. On AF-203 I wrote "ping me and I will
/// ack it the same turn", moved it review -> doing, and acked four days later
/// only because the author came and asked. Nothing in amux told me, and the card
/// read `doing` and looked healthy the whole time.
///
/// NARROW ON PURPOSE. The REVIEWER moving the card out is a promise to return;
/// the AUTHOR moving it out is ordinary work, and firing on that would nag every
/// in-progress card that ever named a reviewer. The needle carries the trailing
/// colon for the same reason `reviewer_acted_since_review` does — without it
/// `amux` matches every `amux-frustrations` line and the promise is attributed
/// to the wrong party.
fn reviewer_left_review(log: &str, reviewer: &str) -> bool {
    let rev = reviewer.trim();
    if rev.is_empty() {
        return false;
    }
    let lines: Vec<&str> = log.lines().collect();
    // Scope to the CURRENT round, like the AF-214 needle: a card can be
    // reviewed, sent back, reworked and re-reviewed, and only the latest exit is
    // an outstanding promise.
    let entered = lines
        .iter()
        .rposition(|l| l.contains("-> review"))
        .map(|i| i + 1)
        .unwrap_or(0);
    let needle = format!("` {rev}:");
    lines[entered.min(lines.len())..]
        .iter()
        .any(|l| l.contains(&needle) && l.contains("review ->"))
}

fn card_event_count(conn: &Connection, etype: &str, card: &str, since: f64) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM session_events WHERE type=?1 AND ts > ?2 AND data LIKE ?3",
        rusqlite::params![etype, since, format!("%\"{card}\"%")],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// How many times this exact card has been advance-nudged while sitting in
/// this exact status, and when the most recent one was (AMUX-3563).
///
/// The status is part of the key on purpose: a card that MOVED has responded to
/// the nudge, and the streak should start over rather than punish the card for
/// its history. `advance.nudged` already records the status at nudge time
/// (module header, line 67), so this needs no new write.
fn advance_streak(conn: &Connection, card: &str, status: &str, since: f64) -> (i64, f64) {
    conn.query_row(
        "SELECT COUNT(*), COALESCE(MAX(ts), 0) FROM session_events \
         WHERE type='advance.nudged' AND ts > ?1 AND data LIKE ?2 \
         AND json_extract(data, '$.status') = ?3",
        rusqlite::params![since, format!("%\"{card}\"%"), status],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap_or((0, 0.0))
}

/// py `AMUX_ADVANCE_BACKOFF_MAX_S` — the ceiling on the doubling below.
fn advance_backoff_max_s() -> f64 {
    env_i64("AMUX_ADVANCE_BACKOFF_MAX_S", 6 * 3600) as f64
}

/// The gap this card must have had since its last nudge, given how many times
/// it has already been nudged in this status.
///
/// AMUX-3563. The per-card budget (3 per 24h) is a COUNT WITH NO SPACING, and
/// measured over 7 days that is the whole defect: TUBES-2063 spent its entire
/// 24h budget in 40 MINUTES and then went silent for 23 hours, which is the
/// worst of both — a burst that reads as nagging, then a gap long enough that
/// the card is effectively forgotten. 28 (lane, card) pairs were nudged 3+
/// times and account for 30% of all idle-holding injections, while 147 of 212
/// pairs were nudged exactly once and are the nudge WORKING.
///
/// So this must not touch the first nudge. It only stretches the gap before
/// each REPEAT: 15m, 30m, 1h, 2h, 4h, then the cap. A lane that answers the
/// first prompt never encounters it.
fn advance_required_gap_s(streak: i64) -> f64 {
    if streak <= 0 {
        return ADVANCE_COOLDOWN_S;
    }
    let doubled = ADVANCE_COOLDOWN_S * 2f64.powi(streak.min(8) as i32);
    doubled.min(advance_backoff_max_s())
}

/// The lane's most recent nudge of ANY shape. Python kept this in memory
/// (`_advance_last`) on a process that re-execs many times a day; deriving it
/// from the durable event log is the same bound that actually survives.
///
/// `task.claimed` COUNTS AS A NUDGE. Caught by reading this loop's own
/// end-to-end transcript: a lane was handed BDQ-1 and, one tick later, told
/// "You went idle holding BDQ-1 in 'doing' — keep driving it", before it could
/// have read the assignment. Python never hit this because its advance path ran
/// off an idle EDGE plus a 10-minute per-lane sweep stamp, so the two could not
/// stack; porting the level sweep without that stamp let them. Restating an
/// instruction the lane was given seconds ago is the nag that does NOT compound
/// with a better model — the assignment already told it what to do.
fn last_advance(conn: &Connection, session: &str) -> Option<(f64, Option<String>, Option<String>)> {
    conn.query_row(
        "SELECT ts, data FROM session_events \
         WHERE session=?1 \
         AND type IN ('advance.nudged','advance.routed','needsyou.renag', \
                      'capture.decompose_ask','task.claimed', \
                      'task.claim_reconcile_requested') \
         ORDER BY ts DESC LIMIT 1",
        rusqlite::params![session],
        |r| Ok((r.get::<_, f64>(0)?, r.get::<_, Option<String>>(1)?)),
    )
    .optional()
    .ok()
    .flatten()
    .map(|(ts, data)| {
        let v: Option<Value> = data.as_deref().and_then(|d| serde_json::from_str(d).ok());
        let issue = v.as_ref().and_then(|v| v["issue"].as_str().map(str::to_string));
        let status = v.as_ref().and_then(|v| v["status"].as_str().map(str::to_string));
        (ts, issue, status)
    })
}

/// THE ONE dispatchability predicate, shared verbatim by the counter and the
/// pickup selection (AMUX-2956). These were two hand-kept copies, and they
/// drifted exactly as ethos rule 1 predicts: the dispatch query gained
/// `updated >= fresh_cut` (stale >7d exempt) and the 24h reclaim-cooldown
/// NOT EXISTS, the counter never did — so 5 lanes reported eligible_todos > 0
/// while the dispatcher answered "queue holds nothing dispatchable", and the
/// counter's own docstring claimed the two "cannot disagree". A view that
/// re-derives its filter from what seems sensible drifts the moment the
/// mechanism moves; it has to SHARE the text.
///
/// `epic` joins `tripwire`/`watch` in the type exclusion (AMUX-3005): an epic is
/// a CONTAINER whose CHILDREN carry the work, so auto-picking it as a unit hands
/// a lane something it cannot "do" — the epic AMUX-3005 was claimed exactly that
/// way. Same reason it is excluded from the drain and the WIP-holding count
/// below; a container is never dispatchable, drainable, or a WIP slot.
///
/// Params: ?1 = session, ?2 = fresh_cut (epoch s), ?3 = reclaim_cut (epoch s).
const DISPATCHABLE_WHERE: &str = "i.session=?1 AND i.status='todo' \
     AND i.owner_type='agent' AND i.deleted IS NULL AND COALESCE(i.archived,0)=0 \
     AND COALESCE(i.type,'') NOT IN ('tripwire','watch','epic') \
     AND NOT EXISTS (SELECT 1 FROM issue_tags t WHERE t.issue_id=i.id \
                     AND lower(t.tag) LIKE 'needs:you%') \
     AND i.updated >= ?2 \
     AND NOT EXISTS (SELECT 1 FROM session_events e WHERE e.type IN ('task.claimed','pickup.reclaimed_stale') \
                     AND e.ts > ?3 AND e.data LIKE '%\"' || i.id || '\"%')";

/// py:14403 — the count over EXACTLY the rows pickup selects from, via
/// [`DISPATCHABLE_WHERE`]. Per-card refusals (junk shells, structured deps) still
/// happen inside the loop and surface as `all-candidates-refused` — an honest
/// difference, since those are judgments about a card, not queue membership.
fn eligible_todo_count(conn: &Connection, session: &str, now: f64) -> i64 {
    let fresh_cut = pickup_fresh_cut(now);
    let reclaim_cut = now - reclaim_cooldown_s();
    conn.query_row(
        &format!("SELECT COUNT(*) FROM issues i WHERE {DISPATCHABLE_WHERE}"),
        rusqlite::params![session, fresh_cut, reclaim_cut],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

fn open_card_count(conn: &Connection, session: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM issues WHERE session=?1 AND deleted IS NULL \
         AND COALESCE(archived,0)=0 AND status IN ('doing','review') AND owner_type='agent'",
        rusqlite::params![session],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// py:16070 `_status_applies`. `implicit` (the default) means the status applies
/// to every lane; `explicit` means only where opted in, by session or by tag.
/// AMUX-2312: telling a lane that deploys nothing to drive every card to
/// `verified` sets a target whose gate it cannot satisfy truthfully.
fn status_applies(conn: &Connection, status_id: &str, session: &str, tags: &[String]) -> bool {
    let mode: Option<String> = conn
        .query_row(
            "SELECT COALESCE(mode,'implicit') FROM statuses WHERE id=?1",
            rusqlite::params![status_id],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    match mode.as_deref() {
        // Unknown status: py returns (True, "unknown-status").
        None => return true,
        // Implicit (the default) applies to everyone.
        Some(m) if m != "explicit" => return true,
        // Explicit: fall through to the scope check.
        Some(_) => {}
    }
    if session.is_empty() {
        return false;
    }
    let mut opted = false;
    if let Ok(mut st) =
        conn.prepare("SELECT scope_type, scope_value FROM status_scope WHERE status=?1")
    {
        if let Ok(rows) = st.query_map(rusqlite::params![status_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            for (kind, value) in rows.flatten() {
                let hit = match kind.as_str() {
                    "session" => value == session,
                    "tag" => tags.iter().any(|t| *t == value.to_lowercase()),
                    _ => false,
                };
                opted |= hit;
            }
        }
    }
    opted
}

/// The same verified-completion predicate governs explicit claims, ready
/// queries and automatic pickup. Missing/discarded dependencies stay blocked
/// until the owner explicitly removes or replaces the required relationship.
pub(crate) fn deps_blocking(conn: &Connection, row: &bs::IssueRow) -> Vec<String> {
    row.depends_on.iter()
        .filter(|id| match bs::dependency_resolved(conn, id) {
            Ok(resolved) => !resolved,
            Err(error) => {
                tracing::warn!(marker = "dependency_resolution_unmeasured", task_id = %row.id,
                    dependency = %id, %error, measured = false, n_considered = 1,
                    "dependency lookup failed; keeping the original task blocked");
                true
            }
        })
        .cloned().collect()
}

/// Why each blocking dependency does NOT count, in the operator's own terms.
///
/// `deps_blocking` answers WHICH ids block; the auto-park line then said only
/// "unfinished depends_on=<id>". "Unfinished" is the predicate's name, not the
/// dependency's state, and the two come apart exactly where this is most
/// confusing: a dependency that is `done` still blocks when its type completes
/// at `verified` (`dependency_is_resolved`). So the operator reads "unfinished",
/// opens the card, sees `done`, and concludes the parker is broken.
///
/// Measured 2026-09-08, reported by `primis` (AF-610): PRIMI-220 twice and
/// PRIMI-221 once were parked within the same second as a satisfied 2/2 gate,
/// naming PRIMI-219, which had been `done` since that morning. PRIMI-219 has NO
/// `type`, and `core_item_type("")` falls through to `ItemType::Code`, for which
/// `verified_is_meaningful` is true. Correct behaviour, unreadable message. The
/// cost was not the block: the workaround was to CLEAR depends_on on both cards,
/// which destroyed the dependency graph the parker exists to enforce.
///
/// A status with no operand, which is this file's own recurring defect.
fn dep_block_reasons(conn: &Connection, ids: &[String]) -> Vec<String> {
    ids.iter()
        .map(|id| match bs::get_issue(conn, id) {
            Ok(Some(row)) => {
                let raw = row.item_type.trim();
                let shown = if raw.is_empty() { "unset->code" } else { raw };
                if bs::is_terminal_status(&row.status) {
                    // Terminal and STILL blocking: name the boundary, because
                    // this is the case that reads as a bug.
                    format!("{id}[{}, type {shown}: completes at verified]", row.status)
                } else {
                    format!("{id}[{}]", row.status)
                }
            }
            // Absent is not the same as unfinished, and neither is unreadable.
            Ok(None) => format!("{id}[missing]"),
            Err(error) => {
                tracing::warn!(marker = "dependency_reason_unmeasured", dependency = %id,
                    %error, measured = false, n_considered = 1,
                    "dependency lookup failed while explaining a park");
                format!("{id}[unreadable]")
            }
        })
        .collect()
}

/// Can this `doing` row honestly consume WIP or be advertised as current?
///
/// A lifecycle label cannot override the card's blocking dimensions. Keeping
/// this predicate shared by board-drive and `/api/sessions` prevents the old
/// split where pickup correctly skipped an unfinished dependency while the
/// worker view still highlighted the blocked card as active work.
pub(crate) fn doing_is_unblocked(conn: &Connection, row: &bs::IssueRow) -> bool {
    row.blocked_on
        .as_deref()
        .is_none_or(|reason| reason.trim().is_empty())
        && deps_blocking(conn, row).is_empty()
}

/// For a set of blocking dep ids, return the subset that are `backlog` cards
/// owned by `session`. These are self-resolvable: the lane can promote them
/// to `todo` itself without any human or cross-lane action.
fn self_owned_backlog_blockers(conn: &Connection, dep_ids: &[String], session: &str) -> Vec<String> {
    dep_ids
        .iter()
        .filter(|d| {
            let row: Option<(String, Option<String>)> = conn
                .query_row(
                    "SELECT status, session FROM issues WHERE id=?1 AND deleted IS NULL",
                    rusqlite::params![d],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .ok()
                .flatten();
            matches!(row, Some((ref st, Some(ref sess)))
                if sess == session
                    && matches!(bs::parse_status(st), Some(TaskStatus::Backlog)))
        })
        .cloned()
        .collect()
}

/// An abandoned `doing` card holding a lane's only WIP slot (AMUX-4042).
///
/// THIS IS THE ONE PLACE amux CHANGES A CARD'S STATUS WITHOUT THE OWNER, and
/// Ethan chose it deliberately on 2026-09-02 over escalating to `needsyou`, so
/// the guards are the argument for why it is safe.
///
/// The stall it ends: byo-ray held BR-51 in `doing` for 42 hours with no
/// `next_action` while 5 todo cards sat unclaimable behind it. The drive had
/// already done everything it is designed to do — it detected the stall and
/// nudged three times — and then stopped, correctly, because "repeating the
/// prompt is not the fix". After that there was no next move at all.
///
/// NOT MG-1388's case, which is the incident that made this module
/// surface-don't-mutate: there, auto-promotion re-activated a card five times
/// against its owner's EXPLICIT re-park. A park is a decision; an abandoned
/// claim is the absence of one. Returning a card the lane stopped working is
/// not overriding a choice, and `todo` is where it already was.
///
/// Four guards, each load-bearing:
///  1. The caller has already established the lane is at a TURN BOUNDARY —
///     `drive_lane` skips `mid-turn` before pickup ever runs — so this can
///     never take a card from a lane that is working it.
///  2. The card has been UNTOUCHED for `doing_reclaim_s`, so a card being
///     actively edited is not eligible however long it has been open.
///  3. The lane must have work it could actually claim, or reclaiming buys
///     nothing and only churns the board.
///  4. `needs:you` and dormant types are excluded upstream by the same query
///     that decides what holds WIP, so a card parked ON A HUMAN is never taken.
fn reclaim_stale_doing(
    conn: &Connection,
    now: f64,
    holding: &[String],
    eligible: i64,
) -> Option<Pickup> {
    // GUARD 3: nothing to unblock means nothing to gain.
    if eligible <= 0 {
        return None;
    }
    let cutoff = doing_reclaim_s();
    for id in holding {
        let Ok(Some(row)) = bs::get_issue(conn, id) else { continue };
        // GUARD 2: `updated` moves on any edit — a note, a status log line, a
        // next_action. Untouched is the honest reading of abandoned.
        let idle_s = now - row.updated as f64;
        if idle_s < cutoff {
            continue;
        }
        return Some(Pickup::ReclaimStale {
            card: id.clone(),
            held_h: idle_s / 3600.0,
            blocking: eligible.max(0) as usize,
        });
    }
    None
}

/// Scope key for backlog dispatch (AMUX-4055).
pub const DISPATCH_BACKLOG_KEY: &str = "AMUX_DISPATCH_BACKLOG_WHEN_IDLE";

/// May this lane pull from `backlog` when it has no `todo` left?
///
/// DEFAULT ON. A worker with actionable backlog and an empty To Do column is a
/// stalled queue, not an idle worker; every new and existing worker therefore
/// drains by default. The eligibility query still excludes human-owned cards,
/// `needs:you`, live triggers, epics, watches, tripwires, and already-claimed
/// work, so default-on does not turn parked coordination state into executable
/// work. A worker/group/global layer may explicitly opt out.
///
/// Same resolver shape as the gates above, so the override ladder is one thing:
/// process env wins (the operator switch in `~/.amux/server.env`), then the
/// worker > group > global scope files.
pub(crate) fn dispatch_backlog_when_idle_in(
    home: &std::path::Path,
    session: &str,
    process_value: Option<&str>,
) -> bool {
    fn is_on(v: &str) -> bool {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes")
    }
    if let Some(v) = process_value.filter(|v| !v.trim().is_empty()) {
        return is_on(v);
    }
    if session.is_empty() {
        return false;
    }
    crate::api::session_verbs::scoped_setting_in(
        home,
        session,
        DISPATCH_BACKLOG_KEY,
    )
    .as_deref()
    .map(is_on)
    .unwrap_or(true)
}

pub fn dispatch_backlog_when_idle(session: &str) -> bool {
    let process_value = std::env::var(DISPATCH_BACKLOG_KEY).ok();
    dispatch_backlog_when_idle_in(
        &crate::api::session_verbs::home(),
        session,
        process_value.as_deref(),
    )
}

/// Backlog cards of a workable TYPE, before the parked/claimed exclusions.
/// The denominator for the drain note: without it, "0 drainable" cannot be told
/// apart from "no backlog at all".
fn backlog_by_type_count(conn: &Connection, session: &str) -> usize {
    conn.query_row(
        "SELECT COUNT(*) FROM issues i WHERE i.session=?1 AND i.status='backlog' \
           AND i.owner_type='agent' AND i.deleted IS NULL AND COALESCE(i.archived,0)=0 \
           AND COALESCE(i.type,'') NOT IN ('tripwire','watch','epic')",
        rusqlite::params![session],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or(0)
    .max(0) as usize
}

/// How many drainable backlog cards remain. Reported beside the promotion so
/// the trace answers "is this lane about to run dry" without a second query.
fn drainable_backlog_ids(conn: &Connection, session: &str, now: f64) -> Vec<String> {
    // AMUX-4228: a parked capture is an already-delivered prompt, not an
    // external condition that eventually goes stale. Only an explicit claim
    // (or an intentional move to Todo) should dispatch that work again.
    let reclaim_cut = now - reclaim_cooldown_s();
    let verified_cut = (now as i64) - SOURCE_REF_STALE_S;
    let candidates = conn
        .prepare(
            "SELECT i.id FROM issues i WHERE i.session=?1 AND i.status='backlog' \
           AND i.owner_type='agent' AND i.deleted IS NULL AND COALESCE(i.archived,0)=0 \
           AND COALESCE(i.type,'') NOT IN ('tripwire','watch','epic') \
           AND NOT EXISTS (SELECT 1 FROM issue_tags t WHERE t.issue_id=i.id \
                           AND lower(t.tag) LIKE 'needs:you%') \
           AND NOT EXISTS (SELECT 1 FROM session_events e WHERE e.type='task.claimed' \
                           AND e.ts > ?2 AND e.data LIKE '%\"' || i.id || '\"%') \
           AND NOT (COALESCE(i.source_ref,'') <> '' AND COALESCE(i.last_verified_at,0) > ?3) \
           AND COALESCE(i.blocked_on,'') = '' \
           AND NOT (COALESCE(i.source,'')='capture' AND COALESCE(i.source_ref,'') <> '') \
         ORDER BY COALESCE(i.created,0) ASC, i.id ASC",
        )
        .and_then(|mut st| {
            st.query_map(rusqlite::params![session, reclaim_cut, verified_cut], |r| {
                r.get::<_, String>(0)
            })
            .map(|rows| rows.flatten().collect::<Vec<_>>())
        })
        .unwrap_or_default();

    // A dependency-blocked backlog card is parked just as surely as a fresh
    // external trigger. Counting or promoting it as drainable only moves the
    // blockage sideways into To Do, where it can prevent independent backlog
    // work from ever being considered (the live MR-14/MR-27 topology).
    candidates
        .into_iter()
        .filter(|id| {
            bs::get_issue(conn, id)
                .ok()
                .flatten()
                .is_some_and(|row| deps_blocking(conn, &row).is_empty())
        })
        .collect()
}

fn drainable_backlog_count(conn: &Connection, session: &str, now: f64) -> usize {
    drainable_backlog_ids(conn, session, now).len()
}

/// The oldest backlog card this lane could actually work, or `None`.
///
/// Reuses [`DISPATCHABLE_WHERE`]'s exclusions with `todo` swapped for
/// `backlog`, so a card this would promote is a card pickup would then accept:
/// dormant types, `needs:you`, archived and recently-claimed cards are out by
/// the same rules. Promoting something pickup would refuse would just move a
/// card sideways and leave the lane idle anyway.
///
/// Cards PARKED ON A TRIGGER are excluded. A `source_ref` with a fresh
/// `last_verified_at` is a card whose owner said "not yet, and here is the
/// condition"; draining it would override that, which is the MG-1388 mistake
/// this module already carries a scar from.
///
/// Cards with `blocked_on` SET are excluded too (AF-516). `amux board block
/// <ID> --on <cause>` sets that field and deliberately does NOT change status,
/// "so its lifecycle position survives" — its own help says the card is
/// "Excluded from `amux board ready` until unblocked". It was: `ready` honours
/// the field and this predicate never read it, so a lane that blocked a backlog
/// card correctly still had it claimed and moved to `doing` with `blocked_on`
/// untouched. Reported by backend on BACKE-3548, 2026-09-05.
///
/// Two mechanisms disagreeing about the same field, which is the only kind of
/// bug where both sides look right in isolation.
///
/// SAFE TO EXCLUDE because it is rare BY CONSTRUCTION: `--on` is required and
/// must name a cause, so nothing defaults it. Measured 2026-09-06: 25 of 438
/// backlog cards, 5%. The comparison that matters is against the fields that
/// CANNOT serve this purpose — `source_ref` is on 93% of backlog cards and a
/// future `due` on 65%, because both are auto-populated, so excluding on either
/// would silence ~65% of the drain (AF-514).
fn oldest_drainable_backlog(conn: &Connection, session: &str, now: f64) -> Option<String> {
    drainable_backlog_ids(conn, session, now).into_iter().next()
}

/// Promote the next genuinely runnable backlog card when this lane opted into
/// automatic draining. This is deliberately reusable after todo candidates
/// have been inspected: the mere presence of a blocked todo must not hide
/// independent work waiting in backlog.
fn backlog_drain_pickup(
    conn: &Connection,
    session: &str,
    now: f64,
    todo_refusals: usize,
) -> Option<Pickup> {
    if !dispatch_backlog_when_idle(session) {
        return None;
    }
    let card = oldest_drainable_backlog(conn, session, now)?;
    // Build the worker prompt from the selected row now, but re-check the
    // status under the claim transaction before anything is dispatched.
    let row = bs::get_issue(conn, &card).ok().flatten()?;
    Some(Pickup::DrainBacklog {
        card,
        prompt: pickup_prompt(conn, session, &row),
        backlog_left: drainable_backlog_count(conn, session, now),
        todo_refusals,
    })
}

/// The dependency promotion that must NOT wait for a free WIP slot (AMUX-4040).
///
/// PROMOTING A DEPENDENCY IS NOT DISPATCHING WORK. It moves a self-owned
/// `backlog` blocker to `todo` and claims nothing, so it consumes no WIP slot
/// and changes no dispatch decision — it only makes the lane's queue tell the
/// truth about what is workable.
///
/// It used to live only INSIDE the candidate loop, which runs after the WIP
/// guard returns. So a lane at its cap could never prepare its own queue: the
/// moment the cap freed, its todo cards were still blocked, pickup found
/// nothing, and the lane went idle holding a full backlog.
///
/// Ethan, 2026-09-02, rtsp-connection: "it has tons of todo and backlog it
/// should've kept going". 13 todo cards, every one blocked, `ready: []`,
/// `blocked_by_deps: 12`. The whole queue hung off ONE edge — RC-67 depends on
/// RC-66, which sits in `backlog` and is owned by that same lane, which is
/// exactly the self-resolvable case this promotion exists for. RC-80 held the
/// single WIP slot, so the promotion never ran, and 12 cards chained behind
/// RC-67 stayed unreachable.
///
/// The shape is AMUX-2128's, one guard further out: a refusal that RETURNS
/// stalls the whole lane. That fix moved the per-card refusals inside the loop;
/// this one moves the queue repair ahead of the guard that skips the loop.
fn promote_blocked_self_owned_deps(conn: &Connection, session: &str, now: f64) -> Option<Pickup> {
    let fresh_cut = pickup_fresh_cut(now);
    let reclaim_cut = now - reclaim_cooldown_s();
    let ids: Vec<String> = conn
        .prepare(&format!(
            "SELECT i.id FROM issues i WHERE {DISPATCHABLE_WHERE} ORDER BY COALESCE(i.created,0) ASC"
        ))
        .and_then(|mut st| {
            st.query_map(rusqlite::params![session, fresh_cut, reclaim_cut], |r| {
                r.get::<_, String>(0)
            })
            .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default();
    for id in &ids {
        let Ok(Some(row)) = bs::get_issue(conn, id) else { continue };
        let blocking = deps_blocking(conn, &row);
        if blocking.is_empty() {
            continue;
        }
        // The SAME condition the in-loop path uses: every blocker is this
        // lane's own backlog card. Anything else is somebody else's to move and
        // must stay a nudge, not a silent promotion.
        let self_owned = self_owned_backlog_blockers(conn, &blocking, session);
        if !self_owned.is_empty() && self_owned.len() == blocking.len() {
            return Some(Pickup::PromoteDeps { blocked_card: id.clone(), promoted: self_owned });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Drive to verified — re-activate a card parked on a dependency once it clears
// ---------------------------------------------------------------------------

/// Types that never auto-promote out of `backlog`: containers and dormant
/// triggers. Deliberately the SAME set [`DISPATCHABLE_WHERE`] excludes — a card
/// that could not be dispatched even sitting in `todo` must not be re-activated
/// INTO `todo`. (An epic is a container whose children carry the work; a
/// tripwire/watch fires on a condition, not on a dependency clearing.)
fn is_dormant_type(t: &str) -> bool {
    matches!(t, "tripwire" | "watch" | "epic")
}

/// A dependency-backed backlog task becomes runnable only when every named
/// dependency has proven completion. An empty set is not a dependency wakeup.
fn promotable_deps(conn: &Connection, row: &bs::IssueRow) -> Option<Vec<String>> {
    (!row.depends_on.is_empty() && deps_blocking(conn, row).is_empty())
        .then(|| row.depends_on.clone())
}

/// Is this card parked on a live `source_ref` trigger? A non-empty `source_ref`
/// is an explicit, owner-set wake condition ("re-check when a namespace holds
/// both an archive- and a competitor-shaped collection") that the OWNING session
/// re-evaluates by hand, NOT "deps terminal". The drive-to-verified promotion
/// pass must leave such a card parked even when its `depends_on` are all terminal.
///
/// This predicate TRIMS: a whitespace-only `source_ref` ("   ") counts as NO live
/// trigger, because whitespace is not a real wake condition. That is DELIBERATELY
/// different from the drain and rot DELETE guards, which test the raw
/// `COALESCE(source_ref,'')=''` (no trim) and so treat "   " as a trigger and skip
/// it. The divergence is principled, not an oversight (AF-56, amux-frustrations'
/// review of af1f301): promotion is NON-destructive, so it errs liberal and
/// promotes an ambiguous card; drain and rot are DESTRUCTIVE, so they err
/// conservative and let an ambiguous `source_ref` shield a card from deletion.
/// Each is conservative in the direction that avoids the costly mistake for its
/// own operation, so do NOT "unify" them by copying one predicate into the
/// other's site. (The T-blank test asserts this promotion-trim behaviour.)
///
/// Without this, a card carrying a satisfied dependency AND a live trigger is
/// dragged out of `backlog` every tick: the promotion sees terminal deps and
/// fires, fighting the owner's park. MG-1388 was re-activated five times in two
/// hours against mixpeek-general's explicit re-parks (2026-08-15); that is ethos
/// rule 8, the harness deciding what was the owning session's to decide.
fn parked_on_live_trigger(row: &bs::IssueRow) -> bool {
    row.source_ref
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty())
}

/// Fleet-wide scan for the drive-to-verified pass: every agent-owned, live,
/// non-archived, non-dormant `backlog` card that has a NON-EMPTY `depends_on`
/// with EVERY dependency terminal AND no live `source_ref` trigger. Returns
/// `(promotions, held_on_trigger)`: each promotion is `(card_id,
/// cleared_dep_ids)`, and `held_on_trigger` counts cards that WOULD have
/// promoted on their deps but were left parked because the owner set a trigger.
/// The count is surfaced in [`DriveReport`] so the hold is visible without
/// reading a card's log (ethos rule 4: a card sitting in `backlog` despite
/// terminal deps must be explainable from the report).
///
/// Uses [`bs::list_issues`] filtered to `backlog` — the same tested read path
/// the board list endpoint uses — rather than a bespoke query, so `depends_on`
/// parsing cannot drift from the canonical decode. Runs once per tick; the
/// cost profile matches a single dashboard board refresh, of which there are
/// already many per minute.
/// Which rule licensed a `backlog` -> `todo` promotion. Both arms share one
/// write path (`promote_ready_backlog` / `promote_due_backlog` differ only in
/// their scan), so the re-check under the write lock and the card's log line
/// cannot drift apart from the rule that selected the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromoteArm {
    /// Every `depends_on` reached a terminal status.
    DepsCleared,
    /// The card's own revisit date arrived.
    RevisitDue,
}

impl PromoteArm {
    fn log_line(self) -> &'static str {
        match self {
            Self::DepsCleared => {
                "Re-activated: all depends_on dependencies reached a terminal status"
            }
            Self::RevisitDue => "Re-activated: the revisit date on this card arrived",
        }
    }
}

/// Re-check under the write lock that this card is STILL promotable by `arm`.
/// It could have been moved, archived, deleted, retyped or re-parked between
/// the read scan and here; `WHERE status='backlog'` on the UPDATE makes the
/// swap atomic against a concurrent move, and this makes it correct.
///
/// The two arms share every clause except the one that licensed them, which is
/// the point of routing both through one function: a new common rule (a type
/// exclusion, an ownership rule) lands on both arms or neither.
fn still_promotable(conn: &Connection, row: &bs::IssueRow, arm: PromoteArm) -> bool {
    if row.status != "backlog"
        || row.owner_type != "agent"
        || row.archived != 0
        || is_dormant_type(&row.item_type)
        || row
            .blocked_on
            .as_deref()
            .is_some_and(|reason| !reason.trim().is_empty())
    {
        return false;
    }
    match arm {
        PromoteArm::DepsCleared => {
            !parked_on_live_trigger(row) && promotable_deps(conn, row).is_some()
        }
        // A REVISIT DATE OUTRANKS A `source_ref` TRIGGER, deliberately.
        //
        // The deps arm lets any non-empty `source_ref` hold a card, and that
        // escape hatch was measured covering essentially the whole fleet's
        // backlog: 509 of 514 cards carried one, 450 of them stale (see
        // `SOURCE_REF_STALE_S`). Honouring it here would mean the revisit date
        // reaches 1% of the cards it exists to drain, which is ethos rule 1's
        // failure exactly — a capability that exists without reaching anything.
        //
        // A due date is also a STRICTLY more specific statement than a trigger:
        // "look at this again on the 12th" names a date, "wake me when X" names
        // a condition nothing re-verifies. So the date wins, and the card
        // arrives in `todo` where its owner can re-park it with a new date.
        //
        // Unmet dependencies still hold it. A date cannot make a blocker
        // finish, and promoting a card whose blocker is open would put
        // unworkable work in front of a lane — the thing this whole module
        // exists to stop.
        PromoteArm::RevisitDue => {
            revisit_due_now(row)
                && (row.depends_on.is_empty() || promotable_deps(conn, row).is_some())
        }
    }
}

/// Has this card's own revisit date arrived, in local time?
fn revisit_due_now(row: &bs::IssueRow) -> bool {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    bs::revisit_arrived(row.due.as_deref(), &today)
}

/// How deep a lane's `todo` column may be filled by the revisit drain.
///
/// The drain exists to stop a lane idling on hundreds of parked cards; it must
/// not create the other half of the same complaint by emptying 219 backlog
/// cards into `todo`. So it fills to a ceiling and stops, and next tick it
/// fills whatever the lane worked off. The backlog then drains at exactly the
/// rate the lane completes work, which is the only rate that is ever right.
///
/// `AMUX_BACKLOG_DRAIN_TODO_CEIL` moves it; `0` disables the drain entirely.
fn drain_todo_ceiling() -> i64 {
    std::env::var("AMUX_BACKLOG_DRAIN_TODO_CEIL")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(5)
        .max(0)
}

/// Pure: given due-dated candidates (already ordered most-overdue first) and
/// each lane's current `todo` depth, which may promote this tick?
///
/// Split out as a pure function because the rate limit is the part that can be
/// wrong in a way no integration test would notice: an off-by-one that lets
/// every candidate through still promotes cards, still logs, still looks
/// healthy, and floods the column it was written to protect.
fn due_drain_plan(
    candidates: &[(String, String)],
    todo_depth: &std::collections::BTreeMap<String, i64>,
    ceiling: i64,
) -> Vec<String> {
    if ceiling <= 0 {
        return Vec::new();
    }
    let mut room: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    let mut out = Vec::new();
    for (card, lane) in candidates {
        let left = room
            .entry(lane.clone())
            .or_insert_with(|| ceiling - todo_depth.get(lane).copied().unwrap_or(0));
        if *left > 0 {
            *left -= 1;
            out.push(card.clone());
        }
    }
    out
}

/// Scan for backlog cards whose revisit date has arrived, most overdue first,
/// and apply the per-lane ceiling. Returns `(to_promote, due_total)` — the
/// second number is every card whose date has arrived, promoted or not, so the
/// report can say "12 due, 3 promoted, the rest are waiting on a full column"
/// rather than leaving a held card indistinguishable from no card (rule 4).
fn backlog_due_promotions(conn: &Connection) -> (Vec<String>, usize) {
    let rows = bs::list_issues(
        conn,
        &["backlog".to_string()],
        &[],
        bs::ArchivedFilter::ActiveOnly,
    )
    .unwrap_or_default();
    let mut cands: Vec<(String, String, String)> = Vec::new();
    for r in rows {
        if r.owner_type != "agent" || is_dormant_type(&r.item_type) {
            continue;
        }
        if r
            .blocked_on
            .as_deref()
            .is_some_and(|reason| !reason.trim().is_empty())
        {
            continue;
        }
        if !revisit_due_now(&r) {
            continue;
        }
        if !r.depends_on.is_empty() && promotable_deps(conn, &r).is_none() {
            continue;
        }
        let lane = r.session.clone().unwrap_or_default();
        if lane.is_empty() {
            // An unowned card has no lane to drain into and no todo depth to
            // measure. Skipping it is not a silent drop: it stays `backlog`
            // with its date visibly past, which is what an unowned overdue
            // card is.
            continue;
        }
        cands.push((r.id.clone(), lane, r.due.clone().unwrap_or_default()));
    }
    // Most overdue first, then by id so the order is total and a tie cannot
    // make two ticks disagree about which card got the last slot.
    cands.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
    let due_total = cands.len();
    let mut todo_depth: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT COALESCE(session,''), COUNT(*) FROM issues \
         WHERE status='todo' AND deleted IS NULL AND COALESCE(archived,0)=0 \
         GROUP BY COALESCE(session,'')",
    ) {
        if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) {
            for kv in rows.flatten() {
                todo_depth.insert(kv.0, kv.1);
            }
        }
    }
    let pairs: Vec<(String, String)> =
        cands.into_iter().map(|(id, lane, _)| (id, lane)).collect();
    (due_drain_plan(&pairs, &todo_depth, drain_todo_ceiling()), due_total)
}

fn backlog_dep_promotions(conn: &Connection) -> (Vec<(String, Vec<String>)>, usize) {
    let rows = bs::list_issues(
        conn,
        &["backlog".to_string()],
        &[],
        bs::ArchivedFilter::ActiveOnly,
    )
    .unwrap_or_default();
    let mut promotions = Vec::new();
    let mut held_on_trigger = 0usize;
    for r in rows {
        if r.owner_type != "agent" || is_dormant_type(&r.item_type) {
            continue;
        }
        if r
            .blocked_on
            .as_deref()
            .is_some_and(|reason| !reason.trim().is_empty())
        {
            continue;
        }
        // Deps must be present and all terminal to be a candidate at all
        // (`promotable_deps` returns None for an empty or non-terminal set).
        let Some(deps) = promotable_deps(conn, &r) else {
            continue;
        };
        // The owner's own trigger OVERRIDES terminal deps — hold the card, and
        // count the hold so the promotion pass leaving it parked is visible.
        if parked_on_live_trigger(&r) {
            held_on_trigger += 1;
            continue;
        }
        promotions.push((r.id.clone(), deps));
    }
    (promotions, held_on_trigger)
}

/// Drive-to-verified: re-activate every card parked in `backlog` on a
/// `depends_on` dependency the moment ALL of its dependencies reach a terminal
/// status. Without this, a command decomposed into "do B after A" stalls at
/// `parked` forever — board-drive dispatches only `todo`, so a backlog card
/// never re-enters the loop when its blocker completes (the "board doesn't
/// drive to completion" case named at the top of the idle-drain cooldown).
///
/// Returns the number promoted. Each promotion emits the SAME board-mutation
/// event a status change from the board API emits (`StatusChanged`, with the
/// post-mutation snapshot) so SSE clients refetch and the card becomes
/// dispatchable, and writes a greppable INFO line naming the card and the deps
/// that cleared it (two-fixes: the next promotion — or a wrongful one — is
/// self-announcing).
pub(crate) async fn promote_ready_backlog(state: &AppState) -> (usize, usize) {
    let (candidates, held_on_trigger) = match state.store.read() {
        Ok(conn) => backlog_dep_promotions(&conn),
        Err(_) => return (0, 0),
    };
    let mut promoted = 0;
    for (card, deps) in candidates {
        if promote_card(state, &card, PromoteArm::DepsCleared).await {
            promoted += 1;
            let cleared = deps.join(",");
            tracing::info!(
                target: "amux::board_drive", %card, deps = %cleared,
                "board_drive: re-activated {card} — all depends_on terminal ({cleared})"
            );
        }
    }
    (promoted, held_on_trigger)
}

fn child_terminal_for_epic(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "done" | "verified" | "discarded" | "quarantined"
    )
}

fn epic_completion_candidates(conn: &Connection) -> Vec<(String, Vec<(String, String)>)> {
    let epic_ids = conn
        .prepare(
            "SELECT id FROM issues WHERE type='epic' AND deleted IS NULL \
             AND COALESCE(archived,0)=0 AND status NOT IN ('done','verified','discarded','quarantined') \
             ORDER BY created,id",
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |r| r.get::<_, String>(0))
                .map(|rows| rows.flatten().collect::<Vec<_>>())
        })
        .unwrap_or_default();
    epic_ids
        .into_iter()
        .filter_map(|epic| {
            let children = conn
                .prepare(
                    "SELECT id,status FROM issues WHERE epic=?1 AND deleted IS NULL \
                     AND COALESCE(archived,0)=0 ORDER BY created,id",
                )
                .and_then(|mut stmt| {
                    stmt.query_map(rusqlite::params![epic], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })
                    .map(|rows| rows.flatten().collect::<Vec<_>>())
                })
                .unwrap_or_default();
            (!children.is_empty()
                && children
                    .iter()
                    .all(|(_, status)| child_terminal_for_epic(status)))
            .then_some((epic, children))
        })
        .collect()
}

/// Close every prompt-root epic whose children are all terminal. The parent is
/// the id stored on the original message, so completing it is what makes the
/// message's task chip report the state of the whole command rather than the
/// state of whichever leaf happened to be created first.
pub(crate) async fn complete_finished_epics(state: &AppState) -> usize {
    let candidates = match state.store.read() {
        Ok(conn) => epic_completion_candidates(&conn),
        Err(_) => return 0,
    };
    let mut completed = 0;
    for (epic, observed_children) in candidates {
        let epic_w = epic.clone();
        let reply = state
            .store
            .write_async(move |conn| {
                let current = epic_completion_candidates(conn)
                    .into_iter()
                    .find(|(id, _)| id == &epic_w);
                let Some((_, children)) = current else {
                    return Ok(crate::db::WriteOutcome {
                        applied: false,
                        events: vec![],
                    });
                };
                let Some(mut parent) = bs::get_issue(conn, &epic_w)? else {
                    return Ok(crate::db::WriteOutcome {
                        applied: false,
                        events: vec![],
                    });
                };
                let summary = children
                    .iter()
                    .map(|(id, status)| format!("{id}={status}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let now = chrono::Utc::now().timestamp();
                let stamp = chrono::Local::now().format("%H:%M").to_string();
                parent.evidence = Some(format!("all epic children terminal: {summary}"));
                parent.last_result = Some(format!("Completed child plan: {summary}"));
                parent.next_action = None;
                parent.updated = now;
                parent.rev += 1;
                parent.version += 1;
                parent.log = Some(bs::append_log(
                    parent.log.as_deref(),
                    &stamp,
                    &format!("all child tasks terminal ({summary}); completing epic"),
                ));
                bs::save_patched(conn, &mut parent)?;
                let mut events = vec![PendingEvent {
                    entity_type: EntityType::Task,
                    entity_id: parent.id.clone(),
                    mutation: MutationKind::Updated,
                    payload: Some(parent.snapshot()),
                }];
                let opts = crate::db::advance::AdvanceOpts {
                    expected_from: Some(parent.status.clone()),
                    gate_ack: true,
                    skip_continuation: true,
                    log_line: Some("status: epic -> done (all children terminal)".into()),
                    ..Default::default()
                };
                match crate::db::advance::advance(conn, &parent.id, "done", "board_drive", &opts)? {
                    Ok(out) => {
                        events.extend(out.events);
                        Ok(crate::db::WriteOutcome {
                            applied: true,
                            events,
                        })
                    }
                    Err(_) => Ok(crate::db::WriteOutcome {
                        applied: false,
                        events: vec![],
                    }),
                }
            })
            .await;
        if matches!(reply, Ok(r) if r.applied) {
            completed += 1;
            let children = observed_children
                .iter()
                .map(|(id, status)| format!("{id}={status}"))
                .collect::<Vec<_>>()
                .join(",");
            tracing::info!(
                target: "amux::board_drive",
                %epic,
                %children,
                "board_drive: completed epic — every linked child is terminal"
            );
        }
    }
    completed
}

#[cfg(test)]
mod epic_completion_unit_tests {
    use super::*;

    #[test]
    fn epic_completes_only_after_every_child_is_terminal() {
        let conn = crate::db::migrate::test_memdb();
        conn.execute_batch(
            "INSERT INTO issues (id,title,status,type,created,updated,owner_type) \
             VALUES ('E-1','root','doing','epic',1,1,'agent');
             INSERT INTO issues (id,title,status,type,epic,created,updated,owner_type) \
             VALUES ('C-1','one','done','code','E-1',2,2,'agent');
             INSERT INTO issues (id,title,status,type,epic,created,updated,owner_type) \
             VALUES ('C-2','two','todo','code','E-1',3,3,'agent');
             INSERT INTO issues (id,title,status,type,created,updated,owner_type) \
             VALUES ('E-EMPTY','empty','doing','epic',4,4,'agent');",
        )
        .unwrap();
        assert!(epic_completion_candidates(&conn).is_empty());
        conn.execute("UPDATE issues SET status='verified' WHERE id='C-2'", [])
            .unwrap();
        let got = epic_completion_candidates(&conn);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "E-1");
        assert_eq!(got[0].1.len(), 2);
    }
}

/// The revisit-date arm: promote `backlog` cards whose own due date arrived,
/// up to the per-lane `todo` ceiling. Returns `(promoted, due_total)`.
///
/// This is the half that answers "workers have an infinite # of growing
/// backlogs and todo then they go idle". The deps arm above only ever fires
/// for a card someone explicitly wired to a blocker (34 of 589 backlog cards
/// fleet-wide carried any `depends_on` at all), so before this arm existed the
/// overwhelming majority of parked work had NO automated exit whatsoever.
async fn promote_due_backlog(state: &AppState) -> (usize, usize) {
    let (candidates, due_total) = match state.store.read() {
        Ok(conn) => backlog_due_promotions(&conn),
        Err(_) => return (0, 0),
    };
    let mut promoted = 0;
    for card in candidates {
        if promote_card(state, &card, PromoteArm::RevisitDue).await {
            promoted += 1;
            tracing::info!(
                target: "amux::board_drive", %card,
                "board_drive: re-activated {card} — its revisit date arrived"
            );
        }
    }
    (promoted, due_total)
}

/// The one `backlog` -> `todo` write, shared by both promotion arms.
///
/// Re-checks the arm's own predicate under the write lock and emits the SAME
/// `StatusChanged` event a status PATCH from the board API emits (with the
/// post-mutation snapshot), so SSE clients refetch and the card is dispatchable
/// in this same tick's lane loop.
async fn promote_card(state: &AppState, card: &str, arm: PromoteArm) -> bool {
    let card_w = card.to_string();
    let reply = state
        .store
        .write_async(move |conn| {
            // Pre-check: the card could have been moved, archived, deleted,
            // retyped or re-parked between the read scan and here.
            let Some(row) = bs::get_issue(conn, &card_w)? else {
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            };
            if !still_promotable(conn, &row, arm) {
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            }
            // Route through the single transition engine.
            let opts = crate::db::advance::AdvanceOpts {
                expected_from: Some("backlog".into()),
                log_line: Some(arm.log_line().to_string()),
                skip_continuation: true,
                // A dependency successor is the next step of already-accepted
                // work. The prior child just left WIP, so an unrelated deep
                // todo queue must not strand this plan forever. Revisit-driven
                // promotions remain ordinary queue additions and keep the cap.
                skip_todo_wip: arm == PromoteArm::DepsCleared,
                ..Default::default()
            };
            match crate::db::advance::advance(conn, &card_w, "todo", "board_drive", &opts)? {
                Ok(outcome) => Ok(crate::db::WriteOutcome {
                    applied: true,
                    events: outcome.events,
                }),
                Err(refusal) => {
                    tracing::warn!(
                        target: "amux::board_drive",
                        card = %card_w,
                        ?arm,
                        ?refusal,
                        measured = true,
                        n_considered = 1,
                        verdict = "promotion_refused",
                        "selected backlog promotion was refused by the transition engine"
                    );
                    Ok(crate::db::WriteOutcome { applied: false, events: vec![] })
                }
            }
        })
        .await;
    if let Err(error) = &reply {
        tracing::warn!(
            target: "amux::board_drive",
            %card,
            ?arm,
            %error,
            measured = false,
            n_considered = 0,
            verdict = "promotion_store_error",
            "selected backlog promotion could not run its writer transaction"
        );
    }
    matches!(reply, Ok(r) if r.applied)
}

fn card_needsyou_asked_at(conn: &Connection, card: &str) -> Option<f64> {
    conn.query_row(
        "SELECT MIN(added_at) FROM issue_tags WHERE issue_id=?1 AND lower(tag) LIKE 'needs:you%'",
        rusqlite::params![card],
        |r| r.get::<_, Option<f64>>(0),
    )
    .optional()
    .ok()
    .flatten()
    .flatten()
}

/// py:13138 `_reviewer_msg_engagement` — the newest ms-timestamp of a MESSAGE
/// from `reviewer` naming `card_id`, else 0.
///
/// ENGAGEMENT, NOT APPROVAL. Any message naming the card counts, whatever it
/// says: a round-1 review that BLOCKS is a completed review action, and a check
/// that parsed sentiment would score it "not an ack" and re-nudge an actively
/// engaged reviewer. Engagement is decidable from an id-reference plus a
/// timestamp; approval is not decidable from prose.
///
/// UNITS: `cmd_history.ts` and `interaction_log.ts` are both MILLISECONDS. They
/// cancel when compared to each other, which is the only comparison the caller
/// makes. Never compare either to wall-clock seconds unscaled.
fn reviewer_msg_engagement(conn: &Connection, card_id: &str, reviewer: &str) -> i64 {
    let want = norm_actor(reviewer);
    if want.is_empty() || card_id.is_empty() {
        return 0;
    }
    // Word-boundary guard so MF-500 does not match MF-5001. The LIKE is a cheap
    // prefilter; the regex decides.
    let Ok(pat) = regex::Regex::new(&format!(r"\b{}\b", regex::escape(card_id))) else {
        return 0;
    };
    let Ok(mut st) = conn.prepare(
        "SELECT origin, text, ts FROM cmd_history WHERE text LIKE ?1 ORDER BY ts DESC LIMIT 500",
    ) else {
        return 0;
    };
    let rows = st.query_map(rusqlite::params![format!("%{card_id}%")], |r| {
        Ok((
            r.get::<_, String>(0).unwrap_or_default(),
            r.get::<_, String>(1).unwrap_or_default(),
            r.get::<_, i64>(2).unwrap_or(0),
        ))
    });
    let Ok(rows) = rows else { return 0 };
    for (origin, text, ts) in rows.flatten() {
        if !origin_matches(&origin, &want) {
            continue;
        }
        if pat.is_match(&text) {
            return ts;
        }
    }
    0
}

/// Has the reviewer's last DELIBERATE action on this card outranked everyone
/// else's? (py:13702, AC-234.) Blocking a card IS a completed review action but
/// leaves it in `review`, so without this the sweep re-nudges a reviewer whose
/// analysis is already on the card.
///
/// DELIBERATE excludes `commit_attached` — the automatic commit-attach hook
/// fires on every commit into whatever card the author holds, and a naive
/// "who wrote last" test reads ~75 of those as the author replying.
fn reviewer_has_responded(conn: &Connection, card: &str, reviewer: &str) -> Option<&'static str> {
    const DELIB: &str = "('patch','status_update','gate_force')";
    let rev_ts: i64 = conn
        .query_row(
            &format!(
                "SELECT ts FROM interaction_log WHERE kind='board' AND target=?1 AND actor=?2 \
                 AND action IN {DELIB} ORDER BY ts DESC LIMIT 1"
            ),
            rusqlite::params![card, reviewer],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
        .unwrap_or(0);
    let other_ts: i64 = conn
        .query_row(
            &format!(
                "SELECT ts FROM interaction_log WHERE kind='board' AND target=?1 AND actor<>?2 \
                 AND action IN {DELIB} ORDER BY ts DESC LIMIT 1"
            ),
            rusqlite::params![card, reviewer],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
        .unwrap_or(0);
    // AF-154: REFUSE RATHER THAN GUESS WHEN THE BOARD SIDE IS BLIND.
    //
    // This function is a COMPARISON — did the reviewer's last deliberate board
    // action outrank everyone else's — and the message timestamp was only ever
    // a tiebreaker within it. Nothing in the Rust server has written a
    // kind='board' interaction_log row since 2026-08-09 (792ce1f, the Python
    // deletion): the sole INSERT is in this module's own #[cfg(test)] block,
    // and the live table's only surviving kind is 'scope'. Measured in the
    // live DB: 1136 board rows, every one dated 08-09.
    //
    // So on every card created since, rev_ts and other_ts are BOTH 0, the
    // comparison collapses, and `best > other_ts && best > 0` degenerates into
    // "has the reviewer ever named this card in a message" — which ANY message
    // satisfies, including the one ANNOUNCING the card. That is what told a
    // lane its reviewer had responded when the card's own log said otherwise.
    //
    // Both-zero is knowable, so say it instead of guessing. The failure this
    // trades toward is one extra nudge to a reviewer who answered only by
    // message; the failure it removes is a review gate waiting on nothing,
    // which is the one that makes the gate meaningless. When board writes come
    // back (or this reads the board's own log instead), the branch stops
    // firing on its own.
    if rev_ts == 0 && other_ts == 0 {
        static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!(
                "[board-drive] interaction_log kind='board' is EMPTY for review cards — \
                 reviewer-response detection is running blind and now declines to infer a \
                 response from messages alone (AF-154). Nothing has written that channel \
                 since the Python server was deleted; a message naming a card is not a \
                 review action."
            );
        }
        return None;
    }
    let msg_ts = reviewer_msg_engagement(conn, card, reviewer);
    let best = rev_ts.max(msg_ts);
    if best > other_ts && best > 0 {
        Some(if msg_ts > rev_ts { "message" } else { "board write" })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Auto-pickup (py:14418 _pickup_next_board_task)
// ---------------------------------------------------------------------------

/// A pickup decision: either a claim to make, or the reason there is none.
#[derive(Debug)]
pub enum Pickup {
    /// Claim this card and send this prompt.
    Claim { card: String, prompt: String },
    /// Every candidate was a capture shell — ask for a decomposition instead of
    /// going silent (py:14614, AMUX-2131). The guard is right that a shell is
    /// not a unit of work; the fix is the session SPLITTING it, which is
    /// judgment a model does well, not the card rotting.
    Decompose { ids: Vec<String>, text: String },
    /// Every candidate is blocked, but all blockers are this session's own
    /// `backlog` cards — self-resolvable without human or cross-lane action.
    /// board_drive promotes them to `todo` automatically and re-runs pickup.
    /// `blocked_card` is the todo that was waiting; `promoted` are the backlog
    /// ids that were promoted. The lane receives no nudge — it just gets work.
    PromoteDeps { blocked_card: String, promoted: Vec<String> },
    /// A `backlog` card selected for direct `backlog -> doing` claim because
    /// the lane has no claimable todo and opted in to automatic draining.
    DrainBacklog { card: String, prompt: String, backlog_left: usize, todo_refusals: usize },
    /// An ABANDONED `doing` card, returned to `todo` so it stops holding the
    /// lane's WIP slot (AMUX-4042). `held_h` is how long it went untouched.
    ReclaimStale { card: String, held_h: f64, blocking: usize },
    /// Nothing to do, with the reason and the detail for the trace.
    None { reason: &'static str, detail: String },
}

/// A restart/recovery decision for work this lane has already claimed.
///
/// This is deliberately separate from [`Pickup`].  A `task.claimed` marker is
/// causal ownership, not an invitation to take another WIP slot.  In
/// particular, a lane may have unrelated stale/subagent Doing rows while one
/// exact claim still proves what it should resume.
#[derive(Debug)]
enum Resume {
    /// Exactly one still-eligible Doing card has a causal `task.claimed`
    /// marker for this lane. Rehydrate its normal board summary and resume it.
    Claim {
        card: String,
        prompt: String,
        delivery_id: String,
        cause: &'static str,
    },
    /// The exact claim is still the runtime truth, but this worker generation
    /// already owns or has received it. Preserve attribution without sending
    /// another prompt at every idle sweep.
    Current { card: String, delivery_id: String, cause: &'static str },
    /// More than one distinct exact claim still survives. Naming either one
    /// would make the driver invent ownership, so hand the complete set back
    /// to the model for reconciliation instead.
    Conflicting { cards: Vec<String> },
    /// There is no surviving exact claim. Ordinary advance/pickup may decide.
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeDelivery {
    Queued,
    AlreadyQueued,
    AlreadyDelivered,
}

#[derive(Clone, Copy, Debug)]
struct ResumeEventOrder {
    ts: f64,
    id: i64,
}

fn event_after(left: ResumeEventOrder, right: ResumeEventOrder) -> bool {
    left.ts.total_cmp(&right.ts).then(left.id.cmp(&right.id)).is_gt()
}

/// Return whether a still-Doing row can be resumed by its exact causal claim.
///
/// The board row is the release boundary for `task.claimed`, but a live marker
/// does not bypass the normal safety gates: human ownership, archive/delete,
/// dormant watch/epic types, fresh external triggers and unresolved structured
/// dependencies all remain a reason not to wake a worker.
fn resumable_claim_row(conn: &Connection, session: &str, row: &bs::IssueRow, now: f64) -> bool {
    row.status == "doing"
        && row.session.as_deref() == Some(session)
        && row.owner_type == "agent"
        && row.archived == 0
        && !matches!(row.item_type.as_str(), "tripwire" | "watch" | "epic")
        && !row.tags.iter().any(|tag| tag.to_ascii_lowercase().starts_with("needs:you"))
        && !fresh_source_ref_trigger(row, now as i64)
        && deps_blocking(conn, row).is_empty()
}

/// Select the one exact `task.claimed` identity which remains live on the
/// board, and distinguish durable ownership from a fresh recovery delivery.
///
/// A surviving claim is runtime TRUTH; it is not by itself permission to send
/// again. A running worker receives recovery only when a later
/// `session.started` proves its process generation changed, or when the prior
/// recovery attempt explicitly failed. A stopped worker may be woken from any
/// one exact live claim. Competing claims deliberately produce no *singular*
/// launch card: board-drive starts the worker in its configured directory and
/// then sends the complete set through the model-owned reconciliation path.
pub(crate) fn exact_resume_card(conn: &Connection, session: &str) -> Result<Option<String>, String> {
    match select_resume(conn, session, now_f64(), false) {
        Resume::Claim { card, .. } | Resume::Current { card, .. } => Ok(Some(card)),
        Resume::None => Ok(None),
        Resume::Conflicting { .. } => Ok(None),
    }
}

const CLAIM_RECONCILE_BASE_S: f64 = 5.0 * 60.0;
const CLAIM_RECONCILE_MAX_S: f64 = 60.0 * 60.0;

enum ClaimReconcilePlan {
    Ready {
        prompt: String,
        delivery_id: String,
        signature: String,
        attempt: i64,
    },
    Cooldown { remaining_s: f64, attempt: i64 },
}

/// Give an impossible multi-claim state to the worker that created it. This is
/// contextual judgment the model can improve at: amux keeps the invariant,
/// supplies every candidate, and never guesses which work is real or parks the
/// lane on a human-only badge.
///
/// The event is both a receipt and the next retry generation. A committed
/// queue row with a missing event is recovered by the stable delivery id; an
/// ignored request is retried with bounded backoff at a later idle boundary.
fn select_claim_reconcile(
    conn: &Connection,
    session: &str,
    cards: &[String],
    now: f64,
) -> ClaimReconcilePlan {
    let signature = cards.join(",");
    let (mut attempts, mut last_ts, mut last_id): (i64, f64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(MAX(ts),0), COALESCE(MAX(id),0) \
             FROM session_events WHERE session=?1 \
             AND type='task.claim_reconcile_requested' \
             AND json_extract(data,'$.signature')=?2",
            rusqlite::params![session, &signature],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap_or((0, 0.0, 0));
    let (claim_ts, claim_id): (f64, i64) = conn
        .query_row(
            "SELECT COALESCE(MAX(ts),0), COALESCE(MAX(id),0) \
             FROM session_events WHERE session=?1 AND type='task.claimed'",
            [session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or((0.0, 0));
    // A newer claim creates a new conflict generation even if the same ids
    // recur, so it must not inherit the old retry wait.
    if claim_ts > last_ts || (claim_ts == last_ts && claim_id > last_id) {
        attempts = 0;
        last_ts = 0.0;
        last_id = claim_id;
    }
    if attempts > 0 {
        let gap = (CLAIM_RECONCILE_BASE_S * 2f64.powi((attempts - 1).min(8) as i32))
            .min(CLAIM_RECONCILE_MAX_S);
        let remaining_s = gap - (now - last_ts);
        if remaining_s > 0.0 {
            return ClaimReconcilePlan::Cooldown { remaining_s, attempt: attempts };
        }
    }

    let mut rows = Vec::new();
    for card in cards {
        match bs::get_issue(conn, card).ok().flatten() {
            Some(row) => rows.push(format!(
                "- {} [{}] {} | depends_on={} | reviewer={} | gate={}",
                row.id,
                row.status,
                row.title,
                if row.depends_on.is_empty() { "none".into() } else { row.depends_on.join(",") },
                row.reviewer.as_deref().unwrap_or("none"),
                if row.gate.as_deref().is_some_and(|gate| !gate.trim().is_empty()) {
                    "card override"
                } else {
                    "column default"
                },
            )),
            None => rows.push(format!("- {card} [missing at reconciliation read]")),
        }
    }
    let prompt = format!(
        "{}claim-reconcile — reconcile this worker's board state now. AMUX measured {} distinct exact task.claimed cards still in Doing:\n{}\n\
         This is internal worker bookkeeping, not a human decision: do not ask the user which card to keep. \
         Inspect every card with `amux board show <ID>` and use its source message, epic, dependencies, \
         priority, gate, work summary and assets. Keep exactly one genuinely active, unblocked task in Doing. \
         Move every other claim to its honest non-WIP state without losing links or evidence, or to a terminal \
         state only if its gate is satisfied. Then continue the highest-priority eligible work and keep draining \
         this worker's Backlog/To Do until no actionable non-terminal task remains. Record which card is active \
         and why on the affected cards.",
        PICKUP_ANCHOR,
        cards.len(),
        rows.join("\n"),
    );
    let generation = last_id.max(claim_id);
    ClaimReconcilePlan::Ready {
        prompt,
        delivery_id: format!("board-drive-claim-reconcile:{session}:{generation}"),
        signature,
        attempt: attempts + 1,
    }
}

pub(crate) fn current_resume_delivery_id(conn: &Connection, session: &str) -> Option<String> {
    match select_resume(conn, session, now_f64(), false) {
        Resume::Claim { delivery_id, .. } | Resume::Current { delivery_id, .. } => Some(delivery_id),
        _ => None,
    }
}

pub(crate) fn resume_delivery_id(session: &str, card: Option<&str>, generation: i64) -> String {
    format!("board-drive-resume:{session}:{}:{generation}", card.unwrap_or("scoped"))
}

fn select_resume(conn: &Connection, session: &str, now: f64, stopped: bool) -> Resume {
    let events: Vec<(ResumeEventOrder, String, Option<String>)> = conn
        .prepare(
            "SELECT id, ts, type, data FROM session_events WHERE session=?1 \
             AND type IN ('task.claimed','task.resumed','task.resume_failed','session.started') \
             ORDER BY ts ASC, id ASC",
        )
        .and_then(|mut statement| {
            statement
                .query_map(rusqlite::params![session], |row| {
                    let id: i64 = row.get(0)?;
                    let ts: f64 = row.get(1)?;
                    let kind: String = row.get(2)?;
                    let data: Option<String> = row.get(3)?;
                    let card = data
                        .as_deref()
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                        .and_then(|value| {
                            value["issue"]
                                .as_str()
                                .map(str::trim)
                                .filter(|id| !id.is_empty())
                                .map(str::to_string)
                        });
                    Ok((ResumeEventOrder { ts, id }, kind, card))
                })
                .map(|rows| rows.flatten().collect::<Vec<_>>())
        })
        .unwrap_or_default();

    let claimed_markers: Vec<(f64, Option<String>, bool, String)> = events
        .iter()
        .filter(|(_, kind, _)| kind == "task.claimed")
        .map(|(order, _, card)| (order.ts, card.clone(), false, "task.claimed".to_string()))
        .collect();

    let mut resumable_doing = BTreeMap::new();
    for card in claimed_markers.iter().filter_map(|marker| marker.1.as_deref()) {
        let Ok(Some(row)) = bs::get_issue(conn, card) else { continue };
        if resumable_claim_row(conn, session, &row, now) {
            resumable_doing.insert(card.to_string(), (session.to_string(), row.title, row.updated));
        }
    }
    let live = crate::api::sessions_legacy::surviving_claimed_card_ids(
        &claimed_markers,
        session,
        &resumable_doing,
    );

    match live.len() {
        0 => Resume::None,
        1 => {
            let card = live.into_iter().next().expect("one live exact claim");
            let Some(row) = bs::get_issue(conn, &card).ok().flatten() else {
                return Resume::None;
            };
            let latest = |kind: &str, matching_card: bool| {
                events
                    .iter()
                    .filter(|(_, event_kind, event_card)| {
                        event_kind == kind
                            && (!matching_card || event_card.as_deref() == Some(card.as_str()))
                    })
                    .map(|(order, _, _)| *order)
                    .max_by(|left, right| {
                        left.ts.total_cmp(&right.ts).then(left.id.cmp(&right.id))
                    })
            };
            let Some(claimed) = latest("task.claimed", true) else {
                return Resume::None;
            };
            let started = latest("session.started", false);
            let resumed = latest("task.resumed", true);
            let failed = latest("task.resume_failed", true);
            let generation = started
                .filter(|start| event_after(*start, claimed))
                .unwrap_or(claimed);
            let delivery_id = resume_delivery_id(session, Some(&card), generation.id);
            let failed_after_success = failed.is_some_and(|failure| {
                resumed.is_none_or(|success| event_after(failure, success))
            });
            let restarted_after_claim = started.is_some_and(|start| event_after(start, claimed));
            let resumed_this_generation = resumed.is_some_and(|success| {
                !event_after(generation, success)
            });
            if stopped {
                Resume::Claim {
                    prompt: resume_prompt(conn, session, &row),
                    card,
                    delivery_id,
                    cause: "stopped-worker-exact-claim",
                }
            } else if failed_after_success {
                Resume::Claim {
                    prompt: resume_prompt(conn, session, &row),
                    card,
                    delivery_id,
                    cause: "retry-after-delivery-failure",
                }
            } else if restarted_after_claim && !resumed_this_generation {
                Resume::Claim {
                    prompt: resume_prompt(conn, session, &row),
                    card,
                    delivery_id,
                    cause: "worker-generation-restarted",
                }
            } else {
                Resume::Current {
                    card,
                    delivery_id,
                    cause: if resumed_this_generation {
                        "recovery-already-committed-this-generation"
                    } else {
                        "claim-already-belongs-to-running-generation"
                    },
                }
            }
        }
        _ => Resume::Conflicting { cards: live.into_iter().collect() },
    }
}

/// How long a `doing` card may go UNTOUCHED before a lane that is idle at a
/// turn boundary has it taken back (AMUX-4042). Generous by default: the point
/// is abandonment, not slowness.
pub(crate) fn doing_reclaim_s() -> f64 {
    std::env::var("AMUX_DOING_RECLAIM_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(6.0 * 3600.0)
}

/// Age-weighted pickup scoring (AMUX-3779). Default ON; `AMUX_PICKUP_SCORING=0`
/// reverts to the legacy `ORDER BY pos ASC, created ASC` selection (board-drag
/// order, which — because a new card is inserted at `pos = min-1024`, i.e. the
/// TOP of its column, `board_store::create_issue` — is newest-first / LIFO by
/// default and starves old cards until the freshness gate silently drops them).
/// The knob reverts without a rebuild if the new ordering ever misbehaves fleet-wide.
fn pickup_scoring_enabled() -> bool {
    std::env::var("AMUX_PICKUP_SCORING").map(|v| v != "0").unwrap_or(true)
}

/// Severity/importance weight by card type — a shipping/blocking card outranks a
/// journal of the same age, so a bug does not need a human to drag it above a
/// doc. Unknown types default code-like, matching this file's `COALESCE(type,
/// 'code')`. Dormant types (tripwire/watch/epic) are filtered out of pickup
/// upstream, so their weight is never reached.
fn type_weight(item_type: &str) -> i64 {
    match item_type.to_ascii_lowercase().as_str() {
        "blocker" | "escalation" => 30,
        "bug" => 24,
        "code" | "ops" => 12,
        "investigation" => 6,
        "research" | "chore" | "doc" => 0,
        _ => 6,
    }
}

/// Explicit human severity carried on tags (`p0`, `sev1`, `urgent`, ...). The
/// deliberate "this one matters" signal, distinct from where a card happens to
/// sit in the column.
fn tag_severity(tags: &[String]) -> i64 {
    let mut boost = 0;
    for t in tags {
        match t.to_ascii_lowercase().as_str() {
            "p0" | "sev0" | "sev1" | "critical" | "urgent" => boost = boost.max(40),
            "p1" | "sev2" | "high" => boost = boost.max(20),
            _ => {}
        }
    }
    boost
}

/// How many non-terminal cards depend on this one: this card is on their
/// critical path, so working it unblocks the most downstream work. This is
/// amux-core `orchestrator::score`'s `dependents` signal — computed there for the
/// provider-fleet planner but never consulted on the path that actually drives
/// the human fleet, until now.
fn count_dependents(conn: &Connection, id: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM issues WHERE deleted IS NULL AND COALESCE(archived,0)=0 \
         AND status NOT IN ('done','verified','discarded') \
         AND depends_on LIKE '%\"' || ?1 || '\"%'",
        rusqlite::params![id],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// The pickup priority score. Higher = sooner. Mirrors amux-core
/// `orchestrator::score` (an hour of waiting outranks a drag point, so
/// starvation self-corrects without a separate aging pass), extended with type
/// and tag severity. `drag` is the card's rank by board position (topmost =
/// highest, 0 for the bottom) kept a MINOR term: it fine-tunes among
/// similar-priority cards and lets a deliberate drag nudge one up, without
/// re-installing the pos-LIFO default that buries old work. `pinned` is the hard
/// human override — a pinned card leads regardless of everything else.
fn pickup_score(row: &bs::IssueRow, now: f64, dependents: i64, drag: i64) -> i64 {
    let age_hours = (((now as i64) - row.created).max(0)) / 3600;
    let pin = if row.pinned != 0 { 10_000 } else { 0 };
    pin + age_hours
        + type_weight(&row.item_type)
        + tag_severity(&row.tags)
        + dependents * 5
        + drag
}

/// Count todos that would be dispatchable but for the freshness gate — the
/// cards the 7d window is silently withholding. Surfaced in the pickup trace so
/// an aged-out queue is legible instead of reading as an empty one (AMUX-3779).
fn stale_gate_excluded_todos(conn: &Connection, session: &str, fresh_cut: i64) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM issues i WHERE i.session=?1 AND i.status='todo' \
         AND i.owner_type='agent' AND i.deleted IS NULL AND COALESCE(i.archived,0)=0 \
         AND COALESCE(i.type,'') NOT IN ('tripwire','watch','epic') \
         AND NOT EXISTS (SELECT 1 FROM issue_tags t WHERE t.issue_id=i.id \
                         AND lower(t.tag) LIKE 'needs:you%') \
         AND i.updated < ?2",
        rusqlite::params![session, fresh_cut],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// Select the next board task for `session`, or say why not. Pure over the
/// connection: no sends, no writes — so a test can assert the DECISION without
/// a fleet, and the caller owns the ordering of claim-then-deliver.
/// [`select_pickup_with`] resolving the continuation flag from the lane's config.
///
/// The production entry point. Tests use the `_with` form so a lane's ON-DISK
/// scope env cannot decide whether a unit test passes.
pub fn select_pickup(conn: &Connection, session: &str, now: f64) -> Pickup {
    select_pickup_with(conn, session, now, bs::continuation_required(Some(session)))
}

/// [`select_pickup`] with the continuation gate passed in rather than read from
/// the lane's scope env (AMUX-3771 fallout).
///
/// THE FLAG HAD TO COME OUT OF HERE. `select_pickup` is otherwise a pure
/// function of a Connection, and reading `~/.amux/sessions/<lane>.env` inside it
/// made unit tests depend on the machine's real lane config. Opting the `amux`
/// lane into the continuation gate immediately failed
/// `pickup_offers_a_card_only_to_its_session_owner`, which uses "amux" as its
/// fixture lane name -- a shared test broken by one operator's personal config,
/// in isolation, with nothing in the failure naming the cause.
///
/// Same seam and same reasoning as `detect_latency_at`, whose docstring already
/// says it: with only the config-reading entry point, "that is not a seam, it is
/// a coin flip on test order".
pub fn select_pickup_with(
    conn: &Connection,
    session: &str,
    now: f64,
    continuation_gate: bool,
) -> Pickup {
    // WIP cap (py:14449). Pickup claimed via raw UPDATE, bypassing the limit the
    // PATCH path enforces — one session accumulated TWELVE doing cards, a lie
    // every other session reads. Archived cards do NOT hold WIP (Ethan, primis
    // 2026-08-04: an archived `doing` card consumed a lane's entire WIP-1 budget
    // forever while the board hid it), and neither do dormant types — an armed
    // tripwire can never be completed by working it — and neither do `doing`
    // cards tagged needs:you (Ethan/backend 2026-08-11: BACKE-3249 sat
    // needs:you for 31 HOURS holding the lane's whole WIP-1 budget while 28
    // eligible todos waited behind it; "there should be no reason any of these
    // stop"). A card blocked on a human is parked, not in progress — idling
    // the lane does not answer the human's question any faster, and the
    // needs:you re-nag keeps chasing the human either way. Same LIKE form as
    // the candidate query below, so the two loops can never disagree about
    // which cards are human-blocked (the py split this fixes).
    // AND NEITHER DOES A CAPTURE SHELL amux MINTED ITSELF (AMUX-3757). A card
    // whose desc is still literally `**Prompt:** <what the human typed>` is an
    // UNANSWERED PROMPT, not work in progress — nothing about it is done or
    // not-done, which is why the decompose nudge exists to make the lane
    // dispose of it.
    //
    // Counting it against WIP-1 made the act of TYPING AT A LANE disable that
    // lane's auto-pickup. Ethan, 2026-08-26: "why do i need to push
    // @tubescience to continue despite it having so many tasks in its
    // backlog". tubescience was holding TUBES-2225, titled "Why are you
    // stopping" — his own complaint about the lane stopping, captured as a
    // `doing` card, which is what guaranteed it would keep stopping. The
    // frustrated re-prompt is the single most likely prompt to arrive at a
    // stalled lane, so the loop closes on exactly the lanes already in trouble.
    //
    // Same `substr(desc,1,11)` form as the fold query in board.rs, so the two
    // can never disagree about what a capture shell is. RESHAPING the desc —
    // the sanctioned exit the decompose nudge asks for — makes the card stop
    // matching and start counting again, which is correct: at that point it IS
    // a unit of work.
    let cap = wip_cap();
    let holding: Vec<String> = conn
        .prepare(
            "SELECT id FROM issues WHERE session=?1 AND status='doing' AND deleted IS NULL \
             AND COALESCE(archived,0)=0 AND COALESCE(type,'') NOT IN ('tripwire','watch','epic') \
             AND NOT (creator='amux' AND substr(COALESCE(\"desc\",''), 1, 11) = '**Prompt:**') \
             AND COALESCE(blocked_on,'') = '' \
             AND NOT EXISTS (SELECT 1 FROM issue_tags t WHERE t.issue_id=issues.id \
                             AND lower(t.tag) LIKE 'needs:you%') \
             ORDER BY id",
        )
        .and_then(|mut st| {
            st.query_map(rusqlite::params![session], |r| r.get::<_, String>(0))
                .map(|rows| rows.flatten().collect::<Vec<_>>())
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|id| {
            bs::get_issue(conn, id)
                .ok()
                .flatten()
                .is_some_and(|row| doing_is_unblocked(conn, &row))
        })
        .collect();
    if holding.len() as i64 >= cap {
        // AT THE CAP IS NOT A REASON TO LEAVE THE QUEUE BROKEN (AMUX-4040).
        // This promotion claims nothing and takes no slot; skipping it is what
        // let rtsp-connection hold 13 todo cards it could never advance.
        if let Some(p) = promote_blocked_self_owned_deps(conn, session, now) {
            return p;
        }
        // AMUX-4042: and if the slot is held by a card nobody has touched, take
        // it back. Ordered AFTER the promotion on purpose — promoting is
        // reversible and touches only a parked card, so it gets first refusal;
        // reclaiming moves someone's claimed card and is the last resort.
        if let Some(p) =
            reclaim_stale_doing(conn, now, &holding, eligible_todo_count(conn, session, now))
        {
            return p;
        }
        return Pickup::None {
            reason: "wip-cap",
            detail: format!("holding {}/{} in doing: {}", holding.len(), cap, holding.join(", ")),
        };
    }

    // py:14487 candidate selection. Board drag order IS the priority queue
    // (AMUX-2128): `pos` is what the user reorders in the UI, so dragging a card
    // up prioritizes it; `created` breaks ties for never-dragged cards.
    //
    // needs:you is matched as `lower(tag) LIKE 'needs:you%'`, NOT python's exact
    // `tag='needs:you'`. Python's own advance path used the LIKE form and its
    // pickup path used equality, so the two disagreed about which cards are
    // blocked on a human — a sub-tagged ask (`needs:you:decision`) was exempt
    // from one loop and dispatchable by the other. Unifying on the LIKE form
    // only ever WIDENS the exemption, so the first run after the change emits
    // nothing; the opposite direction would have discharged a backlog.
    let fresh_cut = pickup_fresh_cut(now);
    let reclaim_cut = now - reclaim_cooldown_s();
    // Candidate ordering. Legacy (AMUX_PICKUP_SCORING=0): board-drag order
    // `pos ASC, created ASC` — newest-first by default, because a new card lands
    // at the top of its column, which starves old work. Default now: fetch
    // eligible OLDEST-first (so a deep queue never truncates the oldest before it
    // can score), then order by `pickup_score` — age surfaces starved cards, type
    // and tag severity lift P0s, dependents lift critical-path work, and drag
    // position / pin stay as human overrides.
    let ids: Vec<String> = if pickup_scoring_enabled() {
        let raw: Vec<String> = conn
            .prepare(&format!(
                "SELECT i.id FROM issues i WHERE {DISPATCHABLE_WHERE} \
                 ORDER BY i.created ASC LIMIT 256"
            ))
            .and_then(|mut st| {
                st.query_map(rusqlite::params![session, fresh_cut, reclaim_cut], |r| {
                    r.get::<_, String>(0)
                })
                .map(|rows| rows.flatten().collect())
            })
            .unwrap_or_default();
        let rows: Vec<bs::IssueRow> = raw
            .iter()
            .filter_map(|id| bs::get_issue(conn, id).ok().flatten())
            .collect();
        // Drag rank: topmost board position (min `pos`) scores highest; ties by
        // id so the rank is deterministic.
        let n = rows.len() as i64;
        let mut by_pos: Vec<(&str, f64)> =
            rows.iter().map(|r| (r.id.as_str(), r.pos)).collect();
        by_pos.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(b.0))
        });
        let drag: std::collections::HashMap<&str, i64> = by_pos
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (*id, n - i as i64))
            .collect();
        let mut scored: Vec<(i64, i64, String)> = rows
            .iter()
            .map(|r| {
                let deps = count_dependents(conn, &r.id);
                let s = pickup_score(r, now, deps, *drag.get(r.id.as_str()).unwrap_or(&0));
                (s, r.created, r.id.clone())
            })
            .collect();
        // score DESC, then oldest-first, then id — a deterministic total order.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        scored.into_iter().map(|(_, _, id)| id).collect()
    } else {
        conn.prepare(&format!(
            "SELECT i.id FROM issues i WHERE {DISPATCHABLE_WHERE} \
             ORDER BY COALESCE(i.pos, 0) ASC, i.created ASC LIMIT 16"
        ))
        .and_then(|mut st| {
            st.query_map(rusqlite::params![session, fresh_cut, reclaim_cut], |r| {
                r.get::<_, String>(0)
            })
            .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default()
    };
    if ids.is_empty() {
        // Name the aged-out cards rather than reporting an empty queue: the
        // freshness gate silently withholds stale todos, and "nothing
        // dispatchable" read identically to "no cards" before this (AMUX-3779).
        let aged = stale_gate_excluded_todos(conn, session, fresh_cut);
        let days = pickup_freshness_s() / 86400;
        let aged_note = if aged > 0 {
            format!(
                "; {aged} todo card(s) aged past the {days}d freshness window — \
                 touch or re-file to re-enable"
            )
        } else {
            String::new()
        };
        let freshness_note = if days > 0 { format!(", stale >{days}d") } else { String::new() };
        let cooldown_s = reclaim_cooldown_s();
        let cooldown = if cooldown_s >= 3600.0 {
            format!("{:.1}h", cooldown_s / 3600.0)
        } else {
            format!("{:.0}m", cooldown_s / 60.0)
        };
        // OPT-IN BACKLOG DRAIN (AMUX-4055). The todo query is empty, so no card
        // is even eligible for the refusal guards below. If candidates exist,
        // we inspect them first and use this same drain arm only after every
        // one proves unclaimable; runnable todo always keeps priority.
        //
        // Promotes ONE card to `todo` rather than claiming it into `doing`
        // directly, so the card takes the ordinary path and the board still
        // reads backlog -> todo -> doing. The next tick claims it through the
        // same gates as anything else; nothing here bypasses them.
        //
        // tubescience is the specimen: 21 backlog, 4 needsyou, 0 todo, a free
        // WIP cap, and `backlog.drain_nudge` fired three times without the lane
        // acting on it. The nudge is advisory by design; this is the arm for an
        // owner who would rather it just happen.
        let drain_on = dispatch_backlog_when_idle(session);
        if let Some(pickup) = backlog_drain_pickup(conn, session, now, 0) {
            return pickup;
        }
        // SAY WHY THE DRAIN DID NOT FIRE, in the line someone already reads.
        //
        // Without this the trace says `no-eligible-card` and never mentions that
        // a drain was considered and declined, so "I turned auto-drain on and
        // nothing happened" has no answer anywhere in the product. Ethan asked
        // three times in one morning; the third time the answer was that all 20
        // of tubescience's backlog cards sat inside the 24h parked-trigger
        // window, the oldest by 2.3 hours. That is a fine answer and it was
        // nowhere to be found (ethos rule 4).
        //
        // Reports the SPLIT, not a total: "20 backlog" reads as work being
        // ignored, while "20 backlog, 20 parked on a live trigger" reads as a
        // lane whose queue is genuinely waiting, and those call for opposite
        // responses.
        let drain_note = {
            let parked_total = backlog_by_type_count(conn, session);
            if parked_total == 0 {
                String::new()
            } else if !drain_on {
                format!(
                    " | drain: OFF ({parked_total} backlog card(s); enable with                      AMUX_DISPATCH_BACKLOG_WHEN_IDLE=1 on this worker, or the                      Auto-drain backlog toggle)"
                )
            } else {
                let free = drainable_backlog_count(conn, session, now);
                let parked = parked_total.saturating_sub(free);
                format!(
                    " | drain: ON but nothing drainable — {parked_total} backlog card(s),                      {parked} parked on a human or a live trigger (a source_ref re-verified                      inside {}h), {free} free",
                    SOURCE_REF_STALE_S / 3600
                )
            }
        };
        return Pickup::None {
            reason: "no-eligible-card",
            detail: format!(
                "queue holds nothing dispatchable (needs:you, archived, dormant, \
                 cards claimed in the last {cooldown} are exempt{freshness_note}){aged_note}{drain_note}"
            ),
        };
    }

    // REFUSAL GUARDS RUN INSIDE THE LOOP (py:14581, AMUX-2128): they used to
    // return, so one refusable card at the head of the queue stalled the entire
    // lane forever — 81 clean todos sat behind refusable heads when measured.
    let mut shells: Vec<(String, String)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for id in &ids {
        let Ok(Some(row)) = bs::get_issue(conn, id) else { continue };
        let blob_desc = format!("{}\n{}", row.desc, row.log.clone().unwrap_or_default());

        let blocking = deps_blocking(conn, &row);
        if !blocking.is_empty() {
            // Self-resolvable: all blockers are this session's own backlog cards.
            // Promote them immediately so pickup can succeed on the next pass —
            // the lane gets work without a nudge, and the log says why.
            let self_owned = self_owned_backlog_blockers(conn, &blocking, session);
            if !self_owned.is_empty() && self_owned.len() == blocking.len() {
                return Pickup::PromoteDeps {
                    blocked_card: id.clone(),
                    promoted: self_owned,
                };
            }
            skipped.push(format!("{id} blocked by {}", blocking.join(",")));
            continue;
        }
        // THE CONTINUATION GATE APPLIES TO THIS DOOR TOO (AMUX-3948, closing a
        // gap AMUX-3946 opened hours earlier).
        //
        // `claim_card` swaps todo->doing with a direct SQL CAS, so it never
        // passes the PATCH gate. A card with no `next_action` was therefore
        // refused when a lane ran `amux board doing`, and CLAIMED SILENTLY when
        // auto-pickup did it — same card, same lane, opposite answers, with the
        // automated path being the permissive one. That is AMUX-3929's shape
        // exactly: the gate held on the doors I checked and not on the one I
        // did not.
        //
        // Skipping rather than claiming is the honest arm. Pickup's whole
        // promise is "work it now", and a card that cannot say what to do next
        // is the one case where that prompt has nothing behind it.
        //
        // The skip is RECORDED in the trace, and /api/board/ready counts the
        // same population as `missing_continuation`, so a lane that goes quiet
        // because of this can find out why in one request instead of looking
        // idle for no visible reason.
        if continuation_gate
            && bs::continuation_verdict(row.next_action.as_deref().unwrap_or(""))
                != bs::ContinuationVerdict::Ok
        {
            skipped.push(format!("{id} has no next_action (continuation gate)"));
            continue;
        }
        let junk = pickup_junk_reason(&row.title, &row.desc, row.log.as_deref().unwrap_or(""));
        if !junk.is_empty() {
            shells.push((id.clone(), row.title.chars().take(70).collect()));
            skipped.push(format!("{id} — {junk}"));
            continue;
        }
        let lower = format!("{}\n{}", row.title, blob_desc).to_lowercase();
        if let Some(op) = irreversible_op(&lower) {
            skipped.push(format!("{id} declined — names an irreversible operation ('{op}')"));
            continue;
        }
        let mut prompt = pickup_prompt(conn, session, &row);
        // Prose is evidence for the worker to assess, not a dependency graph.
        // The old veto stranded six live lanes, including MS-1253 "blocked"
        // by itself and MR-21 held by a superseded note (AMUX-4168). Structured
        // dependencies above still gate execution. Give ambiguous cards a turn
        // to reconcile their current blocker instead of withholding them forever.
        if row.depends_on.is_empty() {
            let hay = format!("{}\n{}", row.title, blob_desc);
            if let Some(dep) = prose_dependency(&hay) {
                tracing::warn!(
                    session, card = %id, mentioned = %dep,
                    verdict = "prose_dependency_requires_recheck",
                    "board_drive: historical dependency text requires worker judgment; dispatching reconciliation instead of silently vetoing the card"
                );
                prompt.push_str(&format!(
                    "\n\n[dependency recheck] Historical text mentions {dep}, but this card has no \
                     structured depends_on. Read the full card and its latest updates before acting. \
                     Establish the CURRENT blocker; do not execute work whose prerequisite is unmet. \
                     Record actual board dependencies in depends_on. For an external wait, use \
                     `amux board backlog {id} --trigger \"<current condition>\"`, then continue \
                     independent ready work. A citation, self-reference or superseded note is not a blocker."
                ));
            }
        }
        return Pickup::Claim { card: id.clone(), prompt };
    }

    // Every candidate was refused. If the refusals were capture shells, dispatch
    // ONE decompose instruction instead of going silent; irreversible-op
    // declines stay silent, because those need a human.
    if !shells.is_empty() {
        let listed: Vec<(String, String)> = shells.iter().take(8).cloned().collect();
        let list = listed
            .iter()
            .map(|(id, t)| format!("  {id} — {}", quoted_card_text(t, id)))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!(
            "[amux auto-pickup] Your queue's next {} card(s) are captured prompts or journals \
             — not dispatchable as-is:\n{list}\n\
             Decompose each into real cards (one honest unit of work per card, correct type), \
             carry the content over, then discard the shell. Auto-pickup will work the real \
             cards at your next idle.\n\
             PATCH THESE IDS SPECIFICALLY. Do not sweep whatever is open and do not write one \
             outcome onto several cards: each carries its own, or the ledger records work \
             against the wrong unit.",
            shells.len()
        );
        return Pickup::Decompose {
            ids: listed.into_iter().map(|(id, _)| id).collect(),
            text,
        };
    }
    // A todo row can exist yet be unclaimable (for example, waiting on a
    // human-owned dependency). Once every todo candidate has been honestly
    // refused, continue the opted-in lane with independent backlog rather than
    // letting the blocked row shadow that work forever.
    if let Some(pickup) = backlog_drain_pickup(conn, session, now, skipped.len()) {
        return pickup;
    }
    Pickup::None {
        reason: "all-candidates-refused",
        detail: skipped.join("; "),
    }
}

// ---------------------------------------------------------------------------
// Verify nudge — option B (idle session) + option A (batched daily)
// ---------------------------------------------------------------------------

/// The SQL fragment excluding types for which `verified` is meaningless,
/// DERIVED from `verified_is_meaningful` rather than hand-listed.
///
/// AMUX-2825 names calling that predicate as non-negotiable, and the shipped
/// nudge (fe44d61) did not call it once: it selected every `done` card
/// regardless of type. Measured on the live board, 299 of 1162 agent-owned
/// done cards — 25%, across 36 lanes — are doc/chore/investigation/research/
/// escalation/watch, which ship nothing and so have no production to confirm
/// in. Nagging a lane to verify those is precisely the make-work AMUX-2816's
/// narrowing removed, re-created one tier down.
///
/// NOT IN rather than IN, deliberately: `issues.type` holds values outside the
/// enum (10 cards are typed `bug` today). An IN-list would silently drop them;
/// NOT IN keeps an unknown type verifiable, which matches how the rest of this
/// file treats it (`COALESCE(type,'code')`). Unknown defaults to code-like.
fn unverifiable_types_sql() -> String {
    let names: Vec<String> = amux_core::board::ItemType::ALL
        .iter()
        .filter(|t| !amux_core::board::verified_is_meaningful(**t))
        .map(|t| format!("'{}'", t.as_str()))
        .collect();
    format!("AND COALESCE(type,'code') NOT IN ({})", names.join(","))
}

/// The SQL fragment excluding cards parked on a human.
///
/// The nudge's own closing line tells the session that a card it cannot verify
/// "should be tagged `needs:you` so they surface in the owner digest rather than
/// sitting here indefinitely" — but neither query below ever looked at
/// `issue_tags`, so taking that escape did not exit the loop. AEAB-5 carried the
/// tag for two days and was nudged anyway; the sanctioned compliance path was
/// unwalkable, which is ethos rule 6.
///
/// It also double-nags: the `needsyou.renag` job in this same file already
/// chases the HUMAN on these cards every 3d. Nudging the agent too asks the one
/// party who cannot answer.
///
/// Same `LIKE 'needs:you%'` form as `select_pickup`, deliberately — a sub-tagged
/// ask (`needs:you:decision`) must not slip past, and the two loops must never
/// disagree about which cards are human-blocked.
fn needs_human_sql() -> &'static str {
    "AND NOT EXISTS (SELECT 1 FROM issue_tags t WHERE t.issue_id=issues.id \
     AND lower(t.tag) LIKE 'needs:you%')"
}

/// Cards in `done` that this session could verify. Returns (id, title, type)
/// tuples, capped at 8 for prompt brevity.
fn done_verify_candidates(conn: &Connection, session: &str) -> Vec<(String, String, String)> {
    conn.prepare(&format!(
        "SELECT id, title, COALESCE(type,'code') FROM issues \
         WHERE session=?1 AND status='done' AND deleted IS NULL \
         AND COALESCE(archived,0)=0 AND owner_type='agent' {} {} \
         ORDER BY updated DESC LIMIT 8",
        unverifiable_types_sql(),
        needs_human_sql()
    ))
    .and_then(|mut st| {
        st.query_map(rusqlite::params![session], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })
        .map(|rows| rows.flatten().collect())
    })
    .unwrap_or_default()
}

/// Total count of done cards for this session (for the "and N more" line).
fn done_card_count(conn: &Connection, session: &str) -> i64 {
    // SAME predicate as the selector, including the type filter AND the
    // needs:you exclusion. These two feed the "... and N more" arithmetic; if
    // they diverge the prompt states a total its own list cannot account for.
    conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM issues WHERE session=?1 AND status='done' \
             AND deleted IS NULL AND COALESCE(archived,0)=0 AND owner_type='agent' {} {}",
            unverifiable_types_sql(),
            needs_human_sql()
        ),
        rusqlite::params![session],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// Build the batched verify-nudge prompt.
fn verify_nudge_text(cards: &[(String, String, String)], total: i64) -> String {
    let mut lines: Vec<String> = cards
        .iter()
        .map(|(id, title, typ)| {
            let t: String = title.chars().take(70).collect();
            format!("  {id} [{typ}] {t}")
        })
        .collect();
    if total > cards.len() as i64 {
        lines.push(format!("  ... and {} more", total - cards.len() as i64));
    }
    let list = lines.join("\n");
    format!(
        "[amux] You have no queued work (no todo/doing/review cards), but {total} of your \
         cards sit in `done` awaiting verification.\n\n\
         {list}\n\n\
         For each card, read its RESOLVED gate first — GET /api/board/contract?card=<id> — \
         and move to `verified` only when EVERY criterion there holds, with evidence of \
         what you checked. The gate resolves card -> worker -> group -> global -> type \
         default, so it varies by card; this nudge deliberately does not restate it \
         (AMUX-3477: a previous version hardcoded four criteria the board does not use, \
         and following the instruction literally moved cards on the wrong basis — the \
         AF-112 family, one layer up). If you cannot satisfy the resolved gate honestly, \
         leave the card in `done`. If the work is stale or was superseded, archive it \
         with a note explaining why.\n\n\
         To see ALL of them (this list is the 8 most recent): \
         GET /api/board?status=done&done_limit=0 scoped to you. NOTE: the UNSCOPED \
         /api/board caps terminal (done/verified/discarded) cards at 100, so a done card \
         of yours can look ABSENT there while it is very much on the board — check by id \
         (GET /api/board/<id>) or with the scoped query above, not the capped default \
         (this is the exact trap that read as 'these cards do not exist', 2026-08-13).\n\n\
         Cards that genuinely cannot be verified by you (e.g., they require a human \
         decision or access you lack) should be tagged `needs:you` — with the ask and \
         what unblocks it — to mark them human-blocked. Be honest about what that does \
         today: it moves the card into the dashboard needs:you view, and NOTHING pushes \
         it to the owner (there is no digest/alert/email path for needs:you as of \
         2026-09-03, AC-413). So tag it only when a human genuinely owes the next step; \
         it is not an escape hatch that makes the card someone else's problem."
    )
}

/// Stale backlog cards for this session: id, title, age in days.
fn stale_backlog_candidates(
    conn: &Connection,
    session: &str,
    now: i64,
) -> Vec<(String, String, i64)> {
    let cutoff = now - BACKLOG_STALE_AGE_S;
    conn.prepare(
        "SELECT id, title, created FROM issues \
         WHERE session=?1 AND status='backlog' AND deleted IS NULL \
         AND COALESCE(archived,0)=0 AND owner_type='agent' \
         AND created < ?2 \
         ORDER BY created ASC LIMIT 10",
    )
    .and_then(|mut st| {
        st.query_map(rusqlite::params![session, cutoff], |r| {
            let created: i64 = r.get(2)?;
            let age_days = (now - created) / 86400;
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, age_days))
        })
        .map(|rows| rows.flatten().collect())
    })
    .unwrap_or_default()
}

/// Backlog cards of ANY age, OLDEST FIRST — the candidates for the idle-drain
/// nudge (a lane sitting on a fresh backlog with nothing in todo).
///
/// This comment said "newest first" three times, and gave the reason (a
/// just-arrived batch is the likeliest thing the worker meant to act on), for as
/// long as AMUX-3779 has been shipped — which reversed the order to `created ASC`
/// so the drain reads the same way as the todo scorer. The query below is the
/// truth; the prose was describing the code it replaced. Found 2026-09-04 while
/// checking a dispatch-ordering report from ts-gke, where the stale comment was
/// the first thing that made the behaviour look wrong.
fn backlog_candidates(conn: &Connection, session: &str, now: i64) -> Vec<(String, String, i64)> {
    // DRAINABLE only — mirror the exclusions in the idle_drain gate so the cards
    // the nudge lists are exactly the ones it claims are un-worked: no dormant
    // types (tripwire/watch), no card parked on a LIVE source_ref trigger, and
    // no card with `blocked_on` set (AF-516 — added to BOTH predicates in the
    // same commit, because a nudge listing a card the drain will not take is the
    // same disagreement one surface along, and this comment's promise to mirror
    // is the only thing keeping them together).
    // "Live" means re-verified within SOURCE_REF_STALE_S — a trigger nobody has
    // re-checked in 24h+ is treated as if it were never set, so `--trigger`
    // cannot be used to permanently exit the drain nudge on a card the owner
    // is not actually revisiting (see SOURCE_REF_STALE_S doc for the incident).
    // OLDEST-first (AMUX-3779): the idle-drain nudge now lists the same
    // oldest-first order the todo scorer works in, so "what gets attention next"
    // reads one way across todo and backlog instead of newest-here/oldest-there.
    conn.prepare(
        "SELECT id, title, created FROM issues \
         WHERE session=?1 AND status='backlog' AND deleted IS NULL \
         AND COALESCE(archived,0)=0 AND owner_type='agent' \
         AND type NOT IN ('tripwire','watch','epic') \
         AND (COALESCE(source_ref,'')='' OR COALESCE(last_verified_at,0) < ?2) \
         AND COALESCE(blocked_on,'')='' \
         ORDER BY created ASC LIMIT 8",
    )
    .and_then(|mut st| {
        st.query_map(rusqlite::params![session, now - SOURCE_REF_STALE_S], |r| {
            let created: i64 = r.get(2)?;
            let age_days = (now - created) / 86400;
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, age_days))
        })
        .map(|rows| rows.flatten().collect())
    })
    .unwrap_or_default()
}

/// The idle-drain prompt: a lane is doing nothing while it holds a backlog. The
/// action is the WORKER'S to choose (which cards, in what order) — amux only
/// surfaces the stall (D1 exit: the model drives, the harness reports).
fn backlog_drain_text(cards: &[(String, String, i64)], drainable: i64) -> String {
    let list = cards
        .iter()
        .map(|(id, title, _)| {
            let t: String = title.chars().take(70).collect();
            format!("  {id}  {t}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    // `drainable` is the DRAINABLE count (source_ref-parked, dormant and epic
    // cards already excluded — same predicate as the list), NOT the raw backlog.
    // Ask for a batch proportional to it: a lane idle on 200 cards should pull a
    // real handful, not one (Ethan: "backlog across EVERY worker turns into
    // work"). Bounded 3..10 so the worker is never asked to bite off more than it
    // can honestly triage.
    let batch = (drainable / 10).clamp(3, 10);
    let stale_h = SOURCE_REF_STALE_S / 3600;
    format!(
        "[amux] You are idle with {drainable} drainable card(s) in `backlog` and nothing in \
         `todo` or `doing`. board-drive only dispatches `todo`, so this queue will not move \
         on its own — you have to pull from it.\n\n\
         {list}\n\n\
         Pull the next ~{batch} actionable card(s) into `todo` (they get dispatched) or \
         straight to `doing` and start, and for anything that is blocked, done, or no longer \
         relevant, say so and move it (review / done / archive). A card genuinely parked on a \
         condition (a dependency, an owner decision, a dedicated focused turn) belongs in \
         `backlog` WITH a trigger: `amux board <status> <id> --trigger \"what unblocks it\"` \
         records the condition and EXCLUDES the card from this nudge for {stale_h}h, so escalation \
         counts only un-parked work — reach for it instead of leaving a parked card to be \
         re-listed. That exclusion EXPIRES: a trigger you have not re-confirmed in {stale_h}h is \
         treated as un-set and the card returns here, because a trigger is a claim someone is \
         still watching for the condition, not a permanent opt-out. If it is still genuinely \
         blocked, re-run `--trigger` to refresh it; if not, act on the card. Do not leave the \
         whole backlog sitting while you idle — drain it. This nudge repeats faster the larger \
         your DRAINABLE backlog is, until it moves."
    )
}

fn stale_backlog_count(conn: &Connection, session: &str, now: i64) -> i64 {
    let cutoff = now - BACKLOG_STALE_AGE_S;
    conn.query_row(
        "SELECT COUNT(*) FROM issues WHERE session=?1 AND status='backlog' \
         AND deleted IS NULL AND COALESCE(archived,0)=0 AND owner_type='agent' \
         AND created < ?2",
        rusqlite::params![session, cutoff],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

fn backlog_triage_text(
    cards: &[(String, String, i64)],
    total: i64,
    total_backlog: i64,
) -> String {
    let mut lines: Vec<String> = cards
        .iter()
        .map(|(id, title, age)| {
            let t: String = title.chars().take(65).collect();
            format!("  {id} ({age}d old) {t}")
        })
        .collect();
    if total > cards.len() as i64 {
        lines.push(format!("  ... and {} more stale", total - cards.len() as i64));
    }
    let list = lines.join("\n");
    format!(
        "[amux] You have {total_backlog} cards in `backlog`, {total} of which are over \
         14 days old and have not been triaged.\n\n\
         {list}\n\n\
         For each card:\n\
         - If the work is DONE, SUPERSEDED, or NO LONGER RELEVANT: archive it \
         with a note (`amux board archive <id>`).\n\
         - If it is still ACTIONABLE and you should work on it: move it to \
         `todo` so the board-drive loop can assign it.\n\
         - If it NEEDS A HUMAN DECISION: tag it `needs:you`.\n\n\
         A backlog that only grows is not a queue, it is a log. Triage is the \
         difference."
    )
}

// ---------------------------------------------------------------------------
// Continue nudge — for workers that should never stop
// ---------------------------------------------------------------------------

/// Outstanding non-terminal cards for a session: (blocked_count, done_count,
/// blocked_cards [(id, title)], done_cards [(id, title)]).
/// A card's `(status, depends_on)`, or `None` when the id does not resolve.
/// Named rather than inlined so the walk's signature says what it reads.
type CardLink = Option<(String, Vec<String>)>;

/// How far up a `depends_on` chain to look before giving up. Generous: the
/// deepest live chain is 3 hops (MG-1356 -> MG-1355 -> MG-1354 -> MG-1369).
const DEPENDS_ON_MAX_DEPTH: usize = 12;

/// Does this blocked card's `depends_on` chain root in a card that is waiting on
/// the human? Returns that card's id.
///
/// AMUX-3775, reported by mixpeek-general after the SAME seven cards were
/// surfaced four times in one session. The nudge asks a lane to "re-assess each
/// blocker", the lane does, nothing has moved because the root is waiting on
/// Ethan, and the nudge fires again identically. Complying buys nothing, which
/// selects against complying, and then a card that genuinely DID unblock goes
/// unnoticed. The relationship was already in the data; nothing walked it.
///
/// Pure, over a lookup, so the walk has cells without a database. `lookup`
/// returns `(status, depends_on)` or `None` for an id that does not resolve.
///
/// A DANGLING OR CYCLIC CHAIN IS NOT HUMAN-BLOCKED. Both return `None`, which
/// leaves the card in the ordinary nudge — the honest default, since a chain
/// this cannot follow is a chain it has learned nothing about. Treating an
/// unresolvable id as "waiting on Ethan" would silence a card nobody is waiting
/// on, which is the failure this fix exists to avoid in the other direction.
fn human_blocked_root(
    start: &str,
    lookup: &dyn Fn(&str) -> CardLink,
    max_depth: usize,
) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    let mut frontier = vec![start.to_string()];
    for _ in 0..max_depth {
        let mut next = Vec::new();
        for id in frontier {
            if !seen.insert(id.clone()) {
                continue;
            }
            let Some((status, deps)) = lookup(&id) else { continue };
            // The START card is `blocked`, not `needsyou`; only an ANCESTOR
            // counts. Checking the start would make any card a lane retyped to
            // needsyou silence itself.
            if id != start && status == "needsyou" {
                return Some(id);
            }
            next.extend(deps);
        }
        if next.is_empty() {
            return None;
        }
        frontier = next;
    }
    None
}

#[allow(clippy::type_complexity)]
/// `done` cards carry their REVIEWER (AMUX-3561): whether a card can be moved
/// toward `verified` by the lane that owns it depends entirely on whether a peer
/// is named, so the nudge cannot ask honestly without it.
fn outstanding_work(
    conn: &Connection,
    session: &str,
) -> (i64, i64, Vec<(String, String)>, Vec<(String, String, String)>, Vec<(String, String, String)>) {
    let query = |status: &str| -> (i64, Vec<(String, String)>) {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM issues WHERE session=?1 AND status=?2 \
                 AND deleted IS NULL AND COALESCE(archived,0)=0 AND owner_type='agent'",
                rusqlite::params![session, status],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let cards: Vec<(String, String)> = conn
            .prepare(
                "SELECT id, title FROM issues WHERE session=?1 AND status=?2 \
                 AND deleted IS NULL AND COALESCE(archived,0)=0 AND owner_type='agent' \
                 ORDER BY updated DESC LIMIT 8",
            )
            .and_then(|mut st| {
                st.query_map(rusqlite::params![session, status], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map(|rows| rows.flatten().collect())
            })
            .unwrap_or_default();
        (count, cards)
    };
    // BLOCKED IS PARTITIONED, not counted (AMUX-3775). Every blocked card is
    // fetched rather than the first 8, because the split has to be exact: "7
    // blocked" over one listed line is worse than either honest number. Blocked
    // counts are small (the reporting lane's own worst case was 7).
    let all_blocked: Vec<(String, String)> = conn
        .prepare(
            "SELECT id, title FROM issues WHERE session=?1 AND status='blocked' \
             AND deleted IS NULL AND COALESCE(archived,0)=0 AND owner_type='agent' \
             ORDER BY updated DESC",
        )
        .and_then(|mut st| {
            st.query_map(rusqlite::params![session], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();
    let lookup = |id: &str| -> CardLink {
        conn.query_row(
            "SELECT status, COALESCE(depends_on,'') FROM issues WHERE id=?1 AND deleted IS NULL",
            rusqlite::params![id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .ok()
        .map(|(status, deps)| {
            (status, serde_json::from_str::<Vec<String>>(&deps).unwrap_or_default())
        })
    };
    let mut blocked: Vec<(String, String)> = Vec::new();
    let mut human_blocked: Vec<(String, String, String)> = Vec::new();
    for (id, title) in all_blocked {
        match human_blocked_root(&id, &lookup, DEPENDS_ON_MAX_DEPTH) {
            Some(root) => human_blocked.push((id, title, root)),
            None => blocked.push((id, title)),
        }
    }
    let bc = blocked.len() as i64;
    blocked.truncate(8);
    let (dc, done_plain) = query("done");
    let done: Vec<(String, String, String)> = done_plain
        .into_iter()
        .map(|(id, title)| {
            let rv = conn
                .query_row(
                    "SELECT COALESCE(reviewer,'') FROM issues WHERE id=?1",
                    rusqlite::params![id],
                    |r| r.get::<_, String>(0),
                )
                .unwrap_or_default();
            (id, title, rv.trim().to_string())
        })
        .collect();
    (bc, dc, blocked, done, human_blocked)
}

/// The `{blocked, done}` the LAST continue-nudge reported for this lane, or
/// None if it has never fired (or the row is unreadable, which must re-arm
/// rather than suppress — a nudge that goes silent on a parse error is the
/// failure this whole card is about, one layer down).
///
/// Extracted so it can be TESTED. Inline in the scan loop it was unreachable
/// from a test, which is exactly how the un-terminated version shipped.
#[cfg(test)]
mod continue_nudge_ask_tests {
    use super::continue_nudge_text;

    /// AMUX-3561. The nudge must ask for what its RECIPIENT can do.
    ///
    /// It said "verify each in prod or archive if superseded" to a lane that
    /// cannot verify: the resolved `verified` gate in a reviewing group requires
    /// a DIFFERENT worker who checked it themselves. Measured 2026-08-23 —
    /// 3,383 unarchived `done` cards fleet-wide, 141 naming a reviewer, so 96%
    /// of what this listed could not be advanced by the lane reading it, 25-50
    /// times a day.
    ///
    /// AMUX-2903 narrowed the TRIGGER for exactly this reason and left the
    /// sentence doing the thing the narrowing was for.
    #[test]
    fn the_done_section_asks_for_a_reviewer_when_none_is_named() {
        let blocked = vec![("B-1".to_string(), "a blocker".to_string())];
        let done = vec![
            ("D-1".to_string(), "shipped a thing".to_string(), String::new()),
            ("D-2".to_string(), "shipped another".to_string(), String::new()),
        ];
        let t = continue_nudge_text(1, 2, &blocked, &done, &[]);

        assert!(
            t.contains("amux board reviewer"),
            "the ONE step the lane can take must be named: {t}"
        );
        assert!(t.contains("(no reviewer)"), "each card must say whether it has one: {t}");
        assert!(
            !t.contains("verify each in prod"),
            "it must NOT instruct a verification the recipient cannot perform — that \
             sentence is the defect: {t}"
        );

        // AND THE OTHER BRANCH, which is the control: when a reviewer IS named
        // the ask changes, so this is not just a blanket removal of the verify
        // instruction. A fix that deleted the sentence unconditionally would
        // pass the assertions above and fail here.
        let reviewed = vec![(
            "D-3".to_string(),
            "shipped".to_string(),
            "amux-frustrations".to_string(),
        )];
        let t2 = continue_nudge_text(1, 1, &blocked, &reviewed, &[]);
        assert!(t2.contains("reviewer: amux-frustrations"), "name the peer: {t2}");
        assert!(
            t2.contains("amux board ask") && !t2.contains("amux board reviewer"),
            "with a reviewer named, the ask becomes chasing THEM, not naming one: {t2}"
        );
    }

    /// AMUX-3819: `done` is a resting place, and the drive must not say
    /// otherwise.
    ///
    /// The closing sentence read "keep going until everything is verified,
    /// archived, or genuinely blocked", which is STRICTER THAN THE RULE IT
    /// ENFORCES — the owner's wording is "you MAY move an issue to `verified`
    /// only after ALL of these hold". Permission with a bar, not an obligation.
    /// Enforcing the stricter reading converts an opt-in ladder into a standing
    /// debt: amux-frustrations reported 143 done cards of which ~112 name no
    /// reviewer and therefore cannot advance at all, and asked what routes them
    /// to a peer. Nothing should. Nobody decided those 112 warranted a peer's
    /// time; this sentence decided it for them.
    #[test]
    fn the_drive_does_not_demand_verification_of_every_done_card() {
        let done: Vec<(String, String, String)> =
            (0..3).map(|i| (format!("D-{i}"), "shipped".into(), String::new())).collect();
        let t = continue_nudge_text(0, 3, &[], &done, &[]);

        assert!(t.contains("done, verified, archived"), "`done` must be a listed terminal: {t}");
        assert!(
            t.contains("legitimate resting place") && t.contains("not a backlog"),
            "and the done section must say so, or the closing line is the only signal: {t}"
        );
        // THE CONTROL, and the reason this is not just deleting the ask: the
        // step a lane CAN take is still named, and still framed as a judgement
        // about which cards warrant a peer rather than as coverage of all of
        // them. A version that dropped the reviewer mechanics entirely would
        // pass both assertions above and remove the only path to `verified`.
        assert!(t.contains("amux board reviewer"), "the mechanics stay available: {t}");
        assert!(
            t.contains("worth a peer's time"),
            "framed as judgement, not as a quota to clear: {t}"
        );
        // And the count is still REPORTED. Hiding the number would be the
        // opposite failure to demanding it be zero.
        assert!(t.contains("3 of the 3") || t.contains("name no reviewer"), "{t}");
    }
}

fn last_continue_nudge_counts(conn: &Connection, lane: &str) -> Option<(i64, i64)> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT data FROM session_events WHERE session=?1 \
             AND type='continue.nudge' ORDER BY ts DESC LIMIT 1",
            rusqlite::params![lane],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .ok()
        .flatten()
        .flatten();
    let v: serde_json::Value = serde_json::from_str(&raw?).ok()?;
    Some((v.get("blocked")?.as_i64()?, v.get("done")?.as_i64()?))
}

fn continue_nudge_text(
    blocked_count: i64,
    done_count: i64,
    blocked: &[(String, String)],
    done: &[(String, String, String)],
    human_blocked: &[(String, String, String)],
) -> String {
    let mut sections = Vec::new();

    if !blocked.is_empty() {
        let mut lines: Vec<String> = blocked
            .iter()
            .map(|(id, title)| {
                let t: String = title.chars().take(65).collect();
                format!("  {id} {t}")
            })
            .collect();
        if blocked_count > blocked.len() as i64 {
            lines.push(format!("  ... and {} more", blocked_count - blocked.len() as i64));
        }
        sections.push(format!(
            "{blocked_count} blocked card(s) — re-assess each blocker. If the \
             upstream fix landed or you can now measure it, move it to `todo` and \
             work it. If genuinely still blocked, leave it but ensure the blocker \
             is named in the desc.\n{}",
            lines.join("\n")
        ));
    }

    // REPORTED, NOT ASKED (AMUX-3775). These are excluded from the re-assess ask
    // because their chain roots in a card waiting on the human, so re-assessing
    // them cannot change anything and asking again every cycle is what taught
    // the reporting lane to ignore the nudge. They are still NAMED, with the
    // root, because silently dropping them would replace a nudge that cannot be
    // satisfied with a count that cannot be trusted (ethos rule 1).
    if !human_blocked.is_empty() {
        let mut roots: Vec<&str> = human_blocked.iter().map(|(_, _, r)| r.as_str()).collect();
        roots.sort_unstable();
        roots.dedup();
        sections.push(format!(
            "{} blocked card(s) NOT listed above: their depends_on chain roots in {} \
             ({}), which is waiting on the human. Nothing for you to do — do not \
             re-assess these, and do not retype them to `needsyou`, which would put \
             one decision in the owner's queue {} times and destroy the depends_on \
             link this exclusion reads.\n{}",
            human_blocked.len(),
            if roots.len() == 1 { "a card" } else { "cards" },
            roots.join(", "),
            human_blocked.len(),
            human_blocked
                .iter()
                .map(|(id, title, root)| {
                    let t: String = title.chars().take(55).collect();
                    format!("  {id} {t}   (-> {root})")
                })
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    if !done.is_empty() {
        let mut lines: Vec<String> = done
            .iter()
            .map(|(id, title, rv)| {
                let t: String = title.chars().take(65).collect();
                if rv.is_empty() {
                    format!("  {id} {t}   (no reviewer)")
                } else {
                    format!("  {id} {t}   (reviewer: {rv})")
                }
            })
            .collect();
        if done_count > done.len() as i64 {
            lines.push(format!("  ... and {} more", done_count - done.len() as i64));
        }
        // ASK FOR WHAT THE RECIPIENT CAN ACTUALLY DO (AMUX-3561).
        //
        // This said "verify each in prod or archive if superseded" — an
        // instruction the lane cannot follow for almost any card it names. The
        // resolved `verified` gate in a reviewing group requires "Peer-reviewed
        // by a DIFFERENT worker (name them)" and "That peer verified it
        // themselves": unsatisfiable by the author BY CONSTRUCTION. Measured
        // 2026-08-23: 3,383 unarchived `done` cards fleet-wide, 141 naming a
        // reviewer — 4.2%. So 96% of what this section listed could not be
        // advanced by the lane reading it, ~25-50 times a day.
        //
        // AMUX-2903 already fixed the TRIGGER for this reason and its comment
        // says the done count "still rides along in the message as context".
        // It did not ride as context; it rode as an imperative. The trigger was
        // narrowed and the sentence was left doing the thing the narrowing was
        // for. Ethos rule 3 survives a fix aimed at its cause if the wording is
        // not fixed too.
        //
        // What a lane CAN do is name the peer, and that is now the ask — which
        // also produces the exact input the gate requires (12af7ab refuses a
        // `verified` transition with no reviewer), so the nudge starts feeding
        // the gate instead of demanding an output the recipient cannot make.
        // `done` IS A RESTING PLACE (AMUX-3819). The owner's own rule reads
        // "You MAY move an issue to `verified` only after ALL of these hold" —
        // a permission with a bar on it, not an obligation. This nudge used to
        // tell every lane to name a reviewer for every done card, which turns
        // an opt-in ladder into a standing debt: amux-frustrations reported 143
        // done cards of which ~112 name no reviewer, and asked, correctly, what
        // routes them. Nothing should. Nobody decided those 112 warranted a
        // peer's time; the nudge decided it for them.
        //
        // So the ask is now about JUDGEMENT rather than coverage: which of
        // these is worth a peer, and the mechanics for that one. A count of
        // unreviewed cards is still reported, because the number is real and
        // hiding it would be the opposite failure.
        let unreviewed = done.iter().filter(|(_, _, rv)| rv.is_empty()).count();
        let ask = if unreviewed > 0 {
            format!(
                "{done_count} done card(s); {unreviewed} of the {} listed name no reviewer. \
                 `done` is a legitimate resting place and most cards belong there — \
                 `verified` is for work whose correctness is worth a peer's time, and it \
                 needs a DIFFERENT worker who checked it themselves, which you cannot \
                 supply on your own. If any of these IS worth that, name the peer: \
                 `amux board reviewer <id> <peer-session>`. Archive the ones that were \
                 superseded. Leaving the rest in `done` is not a backlog.",
                done.len()
            )
        } else {
            // AF-695: this used to point at `amux board ask <id>`, which does
            // not do what this sentence claims. `ask`/`status_request` pings
            // the card's OWNER for a status report and ignores `reviewer`
            // entirely, and it refuses outright on ANY terminal status
            // (TERMINAL_STATUSES includes `done`) regardless of who it would
            // have pinged. Measured by mixpeek-funnel: 8 of 8 done+reviewer
            // cards refused. The verb that actually notifies the named
            // reviewer is a transition INTO `review` (reviewer_notify fires
            // there, board.rs AMUX-3771), which is not terminal and keeps the
            // already-named reviewer if none is passed.
            //
            // NOT UNIVERSAL (mixpeek-funnel, ran this against a real batch):
            // `review`'s gate is TYPE-SCOPED, and for `code` it asks for
            // "Implemented and self-tested" + "Diff / PR is up" -- both false
            // for a card whose work has not actually shipped (blocked on
            // something else, superseded mid-flight). Moving THAT card to
            // review would force the same false attestation the sibling nudge
            // arm above already warns about. 6 of 8 in the real batch moved
            // cleanly; the other 2 were exactly this case.
            format!(
                "{done_count} done card(s), each with a reviewer named — put them back in \
                 review so the reviewer is actually notified (`amux board review <id>`, the \
                 named reviewer is preserved), or verify the ones where YOU are the named \
                 reviewer. Archive instead if the work was superseded. If the card is `code` \
                 typed and the work genuinely has not shipped, `review`'s gate cannot be \
                 attested honestly either -- leave it in `done` rather than attest either \
                 gate falsely. `amux board ask <id>` does NOT reach a reviewer here: it pings \
                 the card's OWNER for a status report, and it refuses outright on any \
                 terminal status including `done` (measured by mixpeek-funnel, 2026-09-10: \
                 8 of 8 refused). AF-695 has detail."
            )
        };
        sections.push(format!("{ask}\n{}", lines.join("\n")));
    }

    format!(
        // `done` IS IN THE LIST NOW (AMUX-3819). This sentence made `verified`
        // the only non-archive terminal state, which is stricter than the rule
        // it is enforcing: the owner's own wording is "you MAY move an issue to
        // `verified` only after ALL of these hold". Permission with a bar, not
        // an obligation — and a drive that says "keep going until everything is
        // verified" converts an opt-in ladder into a standing debt for every
        // lane, which is what produced a 143-card pile with ~112 unreviewable
        // by construction.
        "[amux auto-continue] Your todo queue is empty but you still have \
         outstanding work:\n\n{}\n\n\
         Keep going until everything is done, verified, archived, or genuinely \
         blocked on someone else. Don't stop between cards.",
        sections.join("\n\n")
    )
}

/// The opening literal of every auto-pickup prompt, and the seam the AMUX-3052
/// stale-pickup guard reads the card id out of (`session_verbs::pickup_card_id`
/// takes the first token after it).
///
/// IT IS A SHARED CONST BECAUSE A COMMENT WAS NOT ENOUGH. The old template said
/// "Claimed board card <ID> from your queue" and the parser held its own copy of
/// that wording, with a comment on BOTH sides saying to change them together.
/// 03ed2b6c shortened the prompt for token cost — a correct change, made with
/// that very comment three lines above the edit — and the parser was not
/// touched. From then on `pickup_card_id` returned None for every real pickup,
/// so the guard delivered 100% of stale pickups while its own unit tests, which
/// fed the RETIRED wording, stayed green. Interpolating the const means the
/// anchor cannot drift; `pickup_prompt_round_trips_through_the_stale_guard`
/// covers the rest of the shape.
pub(crate) const PICKUP_ANCHOR: &str = "[amux auto-pickup] Claimed ";

/// py:14713 — the claim prompt.
///
/// Provenance framing is load-bearing: card descs often embed quoted messages
/// (`[a -> b] ...`), and injected bare, such a quote reads as a live unstamped
/// inter-session message — the 2026-07-23 phantom, where a replayed desc got
/// attributed to a session as a fresh send.
/// The exit for "I genuinely cannot do this, and it is neither another lane's
/// work nor an Ethan decision" (AF-579).
///
/// The pickup prompt named three exits and this was not among them, so a lane in
/// exactly that state had no sentence to follow. The obvious move, a bare
/// `backlog`, is the BACKLOG DRAIN'S INPUT on a lane that opted into draining:
/// `oldest_drainable_backlog` promotes the oldest backlog card the lane could
/// work, so the card returns and the remedy is the loop. Measured on backend:
/// six cards round-robinned indefinitely, all six plain backlog cards whose
/// triggers had aged out past AF-514's 24h ceiling.
///
/// The exits that actually survive the drain are the ones its own predicate
/// excludes: a fresh `--trigger`, and `blocked_on` via `amux board block`
/// (AF-516). Naming those is ethos rule 6 — walk the documented escape with the
/// sanctioned tooling and check that it comes out the other side.
fn decline_exit(session: &str) -> &'static str {
    decline_exit_text(dispatch_backlog_when_idle(session))
}

/// PURE, so the sentence is testable without the ambient switch.
///
/// The first cut read `dispatch_backlog_when_idle` inside the branch and its test
/// set the env var to get one lane each way. That does not work: the switch
/// resolves worker > group > global and read `true` for BOTH lanes here, so the
/// test fell through to a shape assertion common to both arms and proved nothing
/// about the choice. Splitting the decision from the lookup is the same
/// pure-comparator shape `invariants::checks` uses, for the same reason.
fn decline_exit_text(drains: bool) -> &'static str {
    if drains {
        " If you genuinely cannot do it and it is neither of those, park it with the \
         condition that would make it ready: `amux board backlog <ID> --trigger \
         \"<condition>\"`. A BARE `backlog` is not an exit on this lane, which drains \
         untriggered backlog cards straight back into pickup."
    } else {
        " If you genuinely cannot do it and it is neither of those, `amux board backlog \
         <ID>` parks it, and `--trigger \"<condition>\"` records what would make it ready."
    }
}

fn pickup_prompt(conn: &Connection, session: &str, row: &bs::IssueRow) -> String {
    // TELL THE LANE HOW DEEP THE QUEUE IS (py:14669, AMUX-2533). Pickup
    // described ONE card and never the queue, so a lane taking card 1 of 90
    // could not know there were 89 behind it: it scoped and decided one, went
    // idle, got the next, and repeated — 90 full cold-cache turns, routed into
    // the most expensive lane in the fleet. The fix is INFORMATION, not an
    // exemption: a "skip expensive lanes" rule would make cards silently
    // undispatchable with nothing saying so.
    let (qn, qoldest): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(MIN(updated),0) FROM issues WHERE session=?1 \
             AND status='todo' AND owner_type='agent' AND deleted IS NULL \
             AND COALESCE(archived,0)=0",
            rusqlite::params![session],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));
    let mut qnote = String::new();
    if qn > 1 {
        let age_days = if qoldest > 0 {
            ((now_f64() as i64 - qoldest) / 86400).max(0)
        } else {
            0
        };
        let age = if age_days > 0 {
            format!(", oldest queued {age_days}d ago")
        } else {
            String::new()
        };
        qnote = format!("\n\n{qn} more card(s) are queued behind this one{age}.");
        // Only editorialise when the DEPTH is the problem. Below the threshold
        // the count is context; above it, grinding one-at-a-time is the wrong
        // shape and saying so is the whole point.
        if qn >= 10 {
            // The re-shape instruction MUST name its honest exits, or it
            // manufactures a decline loop (backend, 2026-08-11: 19 pickups in
            // an afternoon each ended `doing -> todo` with analysis appended
            // and nothing executed — a compliant reading of the old wording.
            // Every bounce armed the 24h reclaim cooldown, so the lane
            // triaged its whole queue into undispatchability in two hours
            // while reading as busy. The instruction and the failure were the
            // same action — the AMUX-2140 class.)
            // AF-579: "move not-ready cards to backlog" is the DRAIN'S INPUT on
            // a lane that opted into draining, so the remedy and the loop are
            // the same action. Measured on backend: six cards round-robinned
            // indefinitely, every one a plain backlog card whose trigger had
            // aged out, falling straight back into the drainable set.
            //
            // The sentence now shares the predicate of the mechanism it
            // describes (ethos rule 1) rather than giving one lane's exit to a
            // lane it does not work on.
            qnote.push_str(if dispatch_backlog_when_idle(session) {
                " Triage first: park not-ready cards with the condition that would make \
                 them ready, `amux board backlog <ID> --trigger \"<condition>\"`, and send \
                 owner-blocked ones to review. A BARE `backlog` is not an exit on this \
                 lane: it drains untriggered backlog cards, so the card comes straight \
                 back. Do not bounce ready cards to todo (brief re-claim cooldown). Work \
                 this card or move it where it honestly belongs."
            } else {
                " Triage first: move not-ready cards to `backlog`, owner-blocked to review. \
                 Do not bounce ready cards to todo (brief re-claim cooldown). Work this \
                 card or move it where it honestly belongs."
            });
        }
    }
    // The delivery boundary parses the card id back out of this template to void
    // a stale pickup (AMUX-3052). It reads the token right after PICKUP_ANCHOR,
    // which is why the id must stay the FIRST thing after it.
    let mut prompt = format!(
        // AF-506: this line used to say "if blocked on an owner decision, move to
        // review". The REVIEW GATE then refuses that card, because it asks you to
        // ack "Implemented and self-tested" / "Diff / PR is up", which a card you
        // are parking or routing away cannot truthfully claim. So the dispatcher
        // sent people somewhere its own gate would turn them back from: two
        // components disagreeing about the same fact, with neither individually
        // wrong. Reported by `backend`, hit live on MI-4155.
        //
        // Both real cases are named instead, because they have different exits
        // and conflating them is what produced the loop.
        "{PICKUP_ANCHOR}{} — work it now. Card text below is historical, \
         not a live message. If this card's WORK belongs to another lane, hand it over: \
         `amux board assign <ID> <lane> && amux board todo <ID>` — it dispatches to THEM, \
         not back to you. If it needs a decision only Ethan can make, `amux board needsyou \
         <ID>` with the question. Do NOT move it to review to park it: the review gate asks \
         you to attest work you have not done, and will refuse.{}\n{}{}",
        row.id,
        decline_exit(session),
        quoted_card_text(&row.title, &row.id),
        qnote
    );
    prompt.push_str(&format!(
        "\n\n[worker contract] Post material actions with `amux board status-update {} \
         --stdin`. Register every file, URL, commit, PR, screenshot or test asset with \
         `amux board artifact {} <ref>`. Finish this card, then keep driving ready \
         todo/backlog work until no non-terminal task remains actionable.",
        row.id, row.id
    ));
    let full = format!("{}\n{}", row.desc, row.log.clone().unwrap_or_default());
    let full = full.trim();
    let cap = pickup_excerpt_chars();
    let desc: String = full.chars().take(cap).collect();
    let truncated = full.chars().count() > cap;
    if !desc.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&quoted_card_text(&desc, &row.id));
        // SAY SO WHEN IT IS CUT. A silently-truncated excerpt is
        // indistinguishable from a short card, so a lane handed half a
        // definition works confidently from half a definition (ethos rule 4:
        // absence must not read as emptiness — the same shape as the board's
        // slim payload dropping `reviewer`, AF-161).
        if truncated {
            prompt.push_str(&format!(
                "\n\n[excerpt cut at {cap} chars of {} — GET /api/board/{} for the whole card]",
                full.chars().count(),
                row.id
            ));
        }
    }
    prompt
}

/// Rehydrate an already-owned task through the same summary/card-context path
/// as a fresh pickup.  The anchor and id stay byte-for-byte compatible with
/// the stale-delivery guard; only the verb makes it clear this is a recovery,
/// never a second claim.
fn resume_prompt(conn: &Connection, session: &str, row: &bs::IssueRow) -> String {
    if let Some(prompt) = crate::api::session_verbs::generation_resume_prompt(conn, session, &row.id) {
        return prompt;
    }
    pickup_prompt(conn, session, row).replacen(
        " — work it now.",
        " — resume this still-owned task now.",
        1,
    )
}

/// How much of a card's definition rides along with the pickup (AMUX-3759).
///
/// THE OLD VALUE WAS A HARDCODED 500 CHARS, and it was optimising the wrong
/// resource by three orders of magnitude. A steering message is a few KB of
/// text; the thing it saves is a MODEL CALL, and a model call in this fleet
/// carries a median RESIDENT CONTEXT of 308k tokens (p90 738k, measured
/// 2026-08-26 over 11k turns). So every tool step the lane spends fetching back
/// text amux already had in hand costs ~300k input tokens, to save ~1k of
/// prompt.
///
/// It showed up as Ethan's "theres also way too much tokens used for some
/// reason in between tasks". An auto-pickup turn takes a MEDIAN OF 22 TOOL
/// STEPS where a human-prompted turn takes 3, because the human supplies the
/// task inline and the pickup supplies an ID plus a 500-char stub. Measured on
/// the live queue that day: the cap truncated 86% of todo cards (median
/// definition 1,933 chars, p90 6,658) and discarded 108,820 characters of card
/// definition the lane then had to go and read back.
///
/// Config, not a constant, because this is D4 in the ethos ledger — a
/// context-scarcity policy baked into code silently becomes the ceiling as
/// windows grow. 4000 leaves ~20% of today's cards truncated, and those say so.
fn pickup_excerpt_chars() -> usize {
    std::env::var("AMUX_PICKUP_EXCERPT_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(4000)
}

// ---------------------------------------------------------------------------
// Advance (py:13325 _advance_open_card)
// ---------------------------------------------------------------------------

/// An advance decision. `target` names WHO the message goes to — the reviewer
/// edge sends to the reviewer, not to the card's owner, and stamping the wrong
/// lane's cooldown was a real bug (py:13817).
pub enum Advance {
    Nudge {
        target: String,
        card: String,
        status: String,
        text: String,
        /// `advance-nudged` | `review-routed` | `decompose-asked` | `renag`
        kind: &'static str,
    },
    None {
        reason: &'static str,
        detail: String,
    },
}

/// Select the advance nudge for `session`, or say why there is none.
pub fn select_advance(
    conn: &Connection,
    session: &str,
    tags: &[String],
    now: f64,
) -> Advance {
    select_advance_with(conn, session, tags, now, &|owner, reviewer| {
        crate::api::session_verbs::reviewer_unreachable_reason(owner, reviewer)
    })
}

/// A newly captured prompt shell that still needs the owning model's explicit
/// keep/split/discard judgment.
///
/// Capture cleanup is keyed per card (`decompose:<id>`), while the ordinary
/// advance cooldown is keyed per lane. Letting an older real card's cooldown
/// hide a brand-new shell leaves the shell in `doing` even after the turn has
/// ended. Find only shells the shared pickup classifier rejects and which
/// have never received their durable cleanup prompt; ordinary work is never
/// promoted by this exception.
fn unnudged_capture_cleanup(conn: &Connection, session: &str) -> Option<String> {
    let candidates: Vec<(String, String, String, String)> = conn
        .prepare(
            "SELECT id,title,COALESCE(desc,''),COALESCE(log,'') FROM issues \
             WHERE session=?1 AND status='doing' AND source='capture' \
             AND deleted IS NULL AND COALESCE(archived,0)=0 AND owner_type='agent' AND COALESCE(type,'') != 'epic' \
             ORDER BY updated DESC LIMIT 40",
        )
        .and_then(|mut st| {
            st.query_map(rusqlite::params![session], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();

    candidates.into_iter().find_map(|(id, title, desc, log)| {
        if pickup_junk_reason(&title, &desc, &log).is_empty() {
            return None;
        }
        let idem = format!("decompose:{id}");
        let already = conn
            .query_row(
                "SELECT 1 FROM session_events WHERE idem=?1 LIMIT 1",
                rusqlite::params![idem],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some();
        (!already).then_some(id)
    })
}

/// [`select_advance`] over an INJECTED "is this name a registered worker"
/// lookup.
///
/// The reviewer-unreachable gate (AMUX-3751) needs to know whether a nudge can
/// actually be delivered, and the only source for that is the real
/// `~/.amux/sessions` tree. Reading it from inside this otherwise-pure board
/// function made three routing tests depend on which lanes happen to exist on
/// the machine running them — they passed here and would fail in CI, which is
/// the worst version of that dependency because the failure appears in someone
/// else's push. `set_home` would serialise them against every other
/// home-mutating test; an injected lookup costs nothing and is what
/// `config::resolve_home`'s doc asks new tests to prefer.
pub fn select_advance_with(
    conn: &Connection,
    session: &str,
    tags: &[String],
    now: f64,
    reviewer_unreachable: &dyn Fn(&str, &str) -> Option<String>,
) -> Advance {
    let budget = advance_card_budget();
    let capture_cleanup = unnudged_capture_cleanup(conn, session);

    // PER-SESSION COOLDOWN, with PROGRESS YIELDING IT (py:13346, AMUX-2500).
    // Ethan: "all issues should always continue driving; a worker should NOT go
    // idle until all issues are either blocked or complete verified." The
    // cooldown bounds REPETITION — the token audit found 182 advance wakes in
    // 24h, one card nudged nine times and then discarded, nine model turns spent
    // pushing a card into the bin. But a lane that MOVED the card we last named
    // has demonstrably not stalled, and making it wait 15 minutes to be handed
    // the next one is what leaves hundreds of cards sitting.
    if let Some((last_ts, last_card, last_status)) = last_advance(conn, session) {
        if now - last_ts < ADVANCE_COOLDOWN_S {
            let moved = match (&last_card, &last_status) {
                (Some(card), Some(prev)) => conn
                    .query_row(
                        "SELECT status FROM issues WHERE id=?1",
                        rusqlite::params![card],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()
                    .ok()
                    .flatten()
                    .map(|s| s != *prev)
                    .unwrap_or(false),
                _ => false,
            };
            if !moved && capture_cleanup.is_none() {
                return Advance::None {
                    reason: "cooldown",
                    detail: format!(
                        "last nudge {:.0}s ago (cooldown {ADVANCE_COOLDOWN_S:.0}s) and the card \
                         it named has not moved",
                        now - last_ts
                    ),
                };
            }
            if !moved {
                tracing::warn!(
                    target: "amux::board_drive",
                    session,
                    card = capture_cleanup.as_deref().unwrap_or(""),
                    previous_card = last_card.as_deref().unwrap_or(""),
                    verdict = "capture_cleanup_bypassed_lane_cooldown",
                    "new per-card capture cleanup is not suppressed by an unrelated lane cooldown"
                );
            }
        }
    }

    // STALE-ASK CHECK, AHEAD OF THE MAIN SELECTION (py:13389, AC-194). The >3d
    // needs:you re-nag had never executed in production — 48 cycles, 0 fires —
    // for two reasons that both live here rather than in the branch: the main
    // query filters `archived=0` and every eligible card was archived, and
    // `ORDER BY updated DESC` picks the lane's FRESHEST card while a >3d ask is
    // by construction among its stalest. "Put it where the loop has nothing else
    // to do" reads like politeness and is a filter on the population you most
    // need to reach.
    let renag_cut = now - needsyou_renag_days() * 86400.0;
    let stale: Option<(String, String, i64, f64)> = conn
        .query_row(
            // EITHER SPELLING OF THE ASK. `needsyou` is a canonical status as
            // well as a tag, and this query used to see only the tag — so a
            // card parked by the DOCUMENTED transition (core's Doing->NeedsYou)
            // could never re-nag, while the same status also kept it out of
            // auto-pickup and out of the advance path. Nothing handed it out
            // and nothing brought it back. Measured 2026-08-11: 23 of 38 open
            // needsyou cards carried no tag, across six sessions, the oldest
            // four days silent. api/board.rs now stamps the tag on the
            // transition, but that only helps cards parked AFTER it; this arm
            // is what reaches the ones already sitting there, without writing
            // to six other sessions' cards to do it.
            //
            // Ask clock: still MIN(added_at) when a tag exists (AC-178 — never
            // `updated`, which is last-touch and so the most-commented asks
            // were the ones that could never fire). `updated` is the fallback
            // ONLY for a status-only card, where no tag row exists to date the
            // ask and the alternative is not firing at all.
            "SELECT i.id, i.title, COALESCE(i.archived,0), \
                    COALESCE(MIN(t.added_at), i.updated) AS asked_at \
             FROM issues i LEFT JOIN issue_tags t \
                  ON t.issue_id = i.id AND lower(t.tag) LIKE 'needs:you%' \
             WHERE i.session=?1 AND i.deleted IS NULL AND i.owner_type='agent' \
             AND (t.tag IS NOT NULL OR i.status='needsyou') \
             AND i.status NOT IN ('done','verified','discarded') \
             GROUP BY i.id HAVING asked_at IS NOT NULL AND asked_at < ?2 \
             ORDER BY asked_at ASC LIMIT 1",
            rusqlite::params![session, renag_cut],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .ok()
        .flatten();
    if let Some((id, title, archived, asked_at)) = stale {
        if let Some(text) = needsyou_renag_text(conn, session, &id, &title, now - asked_at, archived, now) {
            return Advance::Nudge {
                target: session.to_string(),
                card: id,
                status: "needsyou".into(),
                text,
                kind: "renag",
            };
        }
    }

    // NO `done` TIER (py:13460, Ethan 2026-08-08: "we shouldnt be sending
    // commands to idle workers that genuinely have no more work"). `done` was
    // briefly selected here so something drove done->verified. Removing it was
    // right on its own terms: this loop re-woke lanes per card, per cooldown —
    // 294 advance nudges/day against 25 human prompts, 46% repeats, each wake
    // replaying a 400-600k context. A lane whose only remaining cards are `done`
    // has nothing IN FLIGHT.
    //
    // THE HANDOFF NAMED HERE DOES NOT EXIST (AMUX-2782). This comment used to
    // say "the daily verification sweep took that job and does it better — one
    // batched message once a day". There is no such sweep. Searched: every
    // `jobs::spawn_loop` registration in lib.rs (board-drive, autofix,
    // commit-nudge, ghost-rescue, pipe-reconcile, invariants-monitor,
    // event-processors, orchestrator-runtime, scan, bootstrap, self-adopt,
    // legacy-port); all 114 schedules, where the only ENABLED sweeps are LOG
    // sweeps (SCHED-329, SCHED-331 — docs/rust-migration/log-sweep.md, a
    // different contract that the name collides with) and the amux lane's own
    // SCHED-276 board-triage entry is enabled=0; and the whole repo, where the
    // sole hit for "verification sweep" was THIS COMMENT citing itself.
    //
    // The claim is deleted rather than kept, per ethos rule 6: implement the
    // promise or delete it — an unimplemented handoff that reads as implemented
    // is what stopped anyone checking for three days.
    //
    // MEASURED CONSEQUENCE, cards reaching `verified` per day:
    //     08-07: 256   08-08: 121 (tier removed)   08-09: 38   08-10: 2
    // against 1,153 unarchived `done` cards, 876 of them older than a day. The
    // count is a last-touch proxy (`updated`; only 29 rows carry
    // `last_verified_at`), which INFLATES recent days rather than deflating
    // them, so the collapse is a floor, not an artefact.
    //
    // DO NOT FIX THIS BY RE-ADDING THE TIER — that was removed with a
    // measurement and the owner's words behind it. What replaces it is an open
    // design question that pairs with AMUX-2466's NEEDS-YOU on whether
    // fleet-wide `verified` is the right gate at all.
    //
    // CANDIDATES, not LIMIT 1 (py:13439, AMUX-2498). Taking the single
    // highest-priority card meant an exhausted per-card budget silenced every
    // OTHER card the lane held — and because the ordering puts `doing` first, a
    // lane that peer-reviews continuously always has a populated review tier, so
    // the lanes using reviewers most were starved hardest.
    let mut cands: Vec<(String, String)> = conn
        .prepare(
            "SELECT id, status FROM issues WHERE session=?1 AND deleted IS NULL \
             AND COALESCE(archived,0)=0 AND status IN ('doing','review') AND owner_type='agent' \
             AND NOT (COALESCE(type,'')='epic' AND status='doing') \
             ORDER BY CASE status WHEN 'doing' THEN 0 ELSE 1 END, updated DESC LIMIT 40",
        )
        .and_then(|mut st| {
            st.query_map(rusqlite::params![session], |r| Ok((r.get(0)?, r.get(1)?)))
                .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();
    if let Some(capture_id) = capture_cleanup.as_deref() {
        cands.sort_by_key(|(id, _)| if id == capture_id { 0 } else { 1 });
    }
    if cands.is_empty() {
        return Advance::None {
            reason: "no-open-card",
            detail: "no agent-owned doing/review card to drive".into(),
        };
    }
    let day_ago = now - 86400.0;
    let mut chosen: Option<(String, String)> = None;
    // AMUX-3563: why the skip reasons below are worded per-candidate rather than
    // as one count. A lane skipped for "budget-spent" and a lane skipped for
    // "still inside the backoff gap" are different states with different
    // remedies, and the only place either is visible is this string.
    let mut backoff_skips: Vec<String> = Vec::new();
    // The streak is measured since the card last MOVED, so a 24h lookback would
    // reset it nightly and let a genuinely stuck card be re-nudged forever at
    // the base interval. Bounded at 14d so a long-dead event log cannot make the
    // gap grow without limit.
    let streak_since = now - 14.0 * 86400.0;
    for (id, status) in &cands {
        if card_event_count(conn, "advance.nudged", id, day_ago) >= budget {
            continue;
        }
        let (streak, last_ts) = advance_streak(conn, id, status, streak_since);
        let need = advance_required_gap_s(streak);
        let since_last = now - last_ts;
        if streak > 0 && last_ts > 0.0 && since_last < need {
            backoff_skips.push(format!(
                "{id} in '{status}': nudged {streak}x already, last {:.0}m ago, next not before \
                 {:.0}m",
                since_last / 60.0,
                need / 60.0
            ));
            continue;
        }
        chosen = Some((id.clone(), status.clone()));
        break;
    }
    let Some((card_id, status)) = chosen else {
        // Two distinct exhaustion states, named separately. Folding them into
        // one "budget-spent" would say the prompt was repeated to death when in
        // fact it is deliberately waiting, and the operator's next move differs:
        // budget-spent means the card needs a human or a retype, backoff means
        // it will come round on its own.
        if !backoff_skips.is_empty() {
            return Advance::None {
                reason: "backoff",
                detail: format!(
                    "all {} candidate(s) held by repeat-backoff — {}",
                    cands.len(),
                    backoff_skips.join("; ")
                ),
            };
        }
        return Advance::None {
            reason: "budget-spent",
            detail: format!(
                "all {} candidate(s) have spent their {budget}-nudge 24h budget — repeating the \
                 prompt is not the fix",
                cands.len()
            ),
        };
    };
    let Ok(Some(row)) = bs::get_issue(conn, &card_id) else {
        return Advance::None { reason: "card-vanished", detail: card_id };
    };

    // Same not-a-task guard as pickup, via the SAME predicate with the SAME
    // inputs (py:13503 — this block used to inline its own copy of the regexes,
    // which diverged the moment the shared one gained the structure veto: three
    // paths, three verdicts, one unchanged card). desc and log are passed
    // separately so the capture brand reads the current desc, not the durable
    // log marker — a reshaped card no longer re-nags (AMUX-3187).
    // A decomposed epic preserves its source prompt as context. Its children
    // carry execution; a review-stage epic is real review work, not a shell.
    let why = if row.item_type == "epic" { String::new() }
        else { pickup_junk_reason(&row.title, &row.desc, row.log.as_deref().unwrap_or("")) };
    if !why.is_empty() {
        // TELL THE LANE, do not just log it (py:13513, board-exp-1). Refusing to
        // nudge "advance it" at a capture shell is right — nothing about a chat
        // prompt is done or not-done — but saying so only to a log the lane never
        // reads left a worker that had DONE the work and committed it sitting idle
        // on a `doing` card forever. Once per card, ever.
        let idem = format!("decompose:{card_id}");
        let already: bool = conn
            .query_row(
                "SELECT 1 FROM session_events WHERE idem=?1 LIMIT 1",
                rusqlite::params![idem],
                |_| Ok(true),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or(false);
        if already {
            return Advance::None {
                reason: "shell-card",
                detail: format!("{card_id} is a capture shell ({why}); already asked for a split"),
            };
        }
        // SHORT, AND EVERY COMMAND IN IT EXISTS (AMUX-3707, Ethan: "when its a
        // question and doesnt produce work we get these, i feel like its
        // wasteful of tokens").
        //
        // This block used to be ~250 words that re-argued AMUX-3323's history on
        // every firing. Measured 2026-08-25: 540 of these ever, 296 in the last
        // 7 days (~42/day fleet-wide), and 70.6% of the cards ended `discarded`.
        // The text was the SMALL half of the cost — each nudge wakes an idle lane
        // for a whole turn, so the prose is ~330 tokens and the turn it provokes
        // is tens of thousands. Cutting the prose is worth doing; the reason it
        // is worth MORE than its own token count is that a lane which can act in
        // one command does not spend a turn working out how.
        //
        // Which is the second half, and the sharper one: the old text told the
        // lane to "discard it" and to "set each child's `epic`", and named a
        // command for NEITHER. `discard` does dispatch — AMUX-2140 added it as an
        // alias precisely so this nudge would be walkable — but it is absent from
        // `amux board` help, so a lane that did not already know it exists cannot
        // find it. `epic` did not exist at all until this commit added the verb,
        // while the nudge had been telling every lane to "set each child's epic"
        // since AMUX-3323. Both roads out of a capture shell therefore ended at a
        // hand-rolled curl, which drops the worker header: the nudge was
        // manufacturing the unattributed writes the ledger depends on not having
        // (ethos rule 6, same shape as AMUX-2325).
        //
        // Every command below was WALKED before it shipped, and
        // tests/nudge_commands_exist.rs now walks them on every build — because
        // "I checked once" is what AMUX-2140 already thought.
        // NOT "holding your WIP slot" ANY MORE, and the correction is this
        // commit's whole point (AMUX-3757 changed the mechanism an hour before
        // this text was read back to its own author). A capture shell is now
        // EXEMPT from the WIP cap, so the old wording asserted a blockage that
        // no longer exists — a nudge whose urgency is fictional, which is the
        // view-disagrees-with-mechanism shape the same commit went and fixed
        // one layer down. `the_decompose_nudge_does_not_claim_a_blockage_the_
        // wip_query_exempts` pins the two together so the next change to either
        // one fails rather than drifting.
        //
        // The honest reason is the second clause, and it was always the real
        // one: a card nothing can honestly call done or not-done cannot pass a
        // gate, so it sits on the board reading like work forever.
        let text = format!(
            "[amux] {card_id} is a capture shell ({why}). Not blocking pickup, but not actionable either.\n\n\
             Not work? `amux board discard {card_id} --outcome-stdin`\n\
             One task? `amux board retitle {card_id} \"<title>\" --desc-stdin`\n\
             Several? Produce the ordered JSON plan and run \
             `amux board decompose {card_id} --stdin`. That single write preserves this \
             message link and requires each child's description, dependencies, p0-p3 priority, \
             next action, and falsifiable acceptance criteria."
        );
        return Advance::Nudge {
            target: session.to_string(),
            card: card_id,
            status,
            text,
            kind: "decompose-asked",
        };
    }

    // DEPENDENCY EDGE: if the held card is blocked, the useful push is at the
    // BLOCKER, not the holder.
    let blocking = deps_blocking(conn, &row);
    if let Some(dep_id) = blocking.first() {
        let dep = bs::get_issue(conn, dep_id).ok().flatten();
        let dep_session = dep.as_ref().and_then(|d| d.session.clone()).unwrap_or_default();
        if dep_session != session {
            return Advance::None {
                reason: "dep-other-lane",
                detail: format!(
                    "{card_id} blocked by {dep_id} ({}) — not nudging the holder",
                    if dep_session.is_empty() { "unassigned" } else { &dep_session }
                ),
            };
        }
        // IS THE BLOCKER ACTIONABLE BY THIS SESSION? (py:13572, AC-298.) Owning a
        // card is not the same as being able to advance it: a card in `review`
        // cannot be moved by its AUTHOR, and a card parked in backlog on an
        // external trigger is waiting on the world. In both cases the nudge asks
        // for something no honest action of the recipient's can produce — it
        // fired EIGHT times for one card — so a nudge meant to enforce the gates
        // was manufacturing pressure to lie to them (ethos rule 3).
        let dep = dep.expect("dep_session came from dep");
        let dstat = dep.status.to_lowercase();
        // A blocker parked on a HUMAN — status `needsyou`, or carrying the
        // `needs:you` tag — cannot be driven by the DEPENDENT card's agent owner.
        // `needsyou` IS the board's state for "only a human moves this", so telling
        // its owner to "drive {dep} through its gates" asks for an action no honest
        // work of theirs can produce (ethos rule 3) — it fired 5+ times at one card
        // that had been needsyou since it was filed (GCA-91). This is the same
        // owner-blocked special-case the auto-pickup rule already applies (a
        // needsyou card routes to review, not todo), missing from this one path.
        // Distinct reason so a log sweep can count the suppression (two-fixes rule).
        if dstat == "needsyou" || card_needsyou_asked_at(conn, dep_id).is_some() {
            return Advance::None {
                reason: "dep-needsyou",
                detail: format!(
                    "{card_id} blocked by {dep_id}, which is parked on a human (needs:you) — \
                     nudging the agent owner would demand an action only a human can take (GCA-91)"
                ),
            };
        }
        let drev = dep.reviewer.clone().unwrap_or_default();
        let why_stuck = if dstat == "review" {
            Some(if drev.is_empty() {
                "in review awaiting a peer's sign-off".to_string()
            } else {
                format!("in review awaiting {drev}'s sign-off")
            })
        } else if dstat == "backlog" && dep.source_ref.as_deref().unwrap_or("").trim() != "" {
            Some("parked in backlog on an external trigger".into())
        } else if matches!(dstat.as_str(), "done" | "verified" | "discarded") {
            Some(format!("already {dstat}"))
        } else {
            None
        };
        if let Some(w) = why_stuck {
            return Advance::None {
                reason: "dep-not-actionable",
                detail: format!("{card_id} blocked by own {dep_id}, but that is {w} (AC-298)"),
            };
        }
        let text = format!(
            "[amux] {card_id} is blocked by {dep_id} ({}), which is YOURS. Work the dependency \
             first: drive {dep_id} through its gates, then return to {card_id}. Do not mark \
             {card_id} done while its dependency is open.",
            quoted_card_text(&dep.title.chars().take(80).collect::<String>(), dep_id)
        );
        return Advance::Nudge {
            target: session.to_string(),
            card: card_id,
            status,
            text,
            kind: "advance-nudged",
        };
    }

    // NEEDS-YOU EDGE, ABOVE THE REVIEWER EDGE (py:13619, general-canvas-apps).
    // This block used to sit BELOW, so a needs:you card in review routed to its
    // reviewer and never reached the quiet path — the reviewer then got
    // "ack review->done yourself" repeatedly, and that one action is the action
    // that HIDES the decision (a needs:you card drops out of the digest at
    // status done). An instruction satisfiable only by doing the thing you were
    // told not to do is the ethos rule 3 shape.
    if row.tags.iter().any(|t| t.to_lowercase().starts_with("needs:you")) {
        // Age the ASK, not the ROW (py:13648, AC-178). `updated` is last-touch,
        // and amux writes to descs constantly, so the needs:you cards carrying
        // the most commentary were exactly the ones whose stale-ask check could
        // never fire. `issue_tags.added_at` is stamped when the TAG is applied.
        let asked_at = card_needsyou_asked_at(conn, &card_id).unwrap_or(row.updated as f64);
        let asked_age = now - asked_at;
        if asked_age < needsyou_renag_days() * 86400.0 {
            return Advance::None {
                reason: "needsyou",
                detail: format!(
                    "{card_id} is needs:you ({}h) — the human owes the answer, not the lane",
                    (asked_age / 3600.0) as i64
                ),
            };
        }
        return match needsyou_renag_text(conn, session, &card_id, &row.title, asked_age, row.archived, now) {
            Some(text) => Advance::Nudge {
                target: session.to_string(),
                card: card_id,
                status,
                text,
                // `renag-held`, NOT `renag` (AMUX-3777). Both arms re-nag and
                // both write `needsyou.renag`, so at the TYPE level they are
                // indistinguishable — and that type is the one the AMUX-3768
                // audit confirmed empirically in both directions. The
                // confirmation was therefore about the type, not about this
                // branch: this arm could be dead today and the shared counter
                // would still read 100.
                //
                // The distinction is real, not bookkeeping. The other arm scans
                // the lane's WHOLE QUEUE for the oldest stale ask; this one
                // fires about the card the lane is CURRENTLY HOLDING. They can
                // rot independently.
                kind: "renag-held",
            },
            None => Advance::None {
                reason: "needsyou-renag-deduped",
                detail: format!("{card_id} re-stated or already asked inside the window"),
            },
        };
    }

    // REVIEWER EDGE. A card whose next transition needs the reviewer's sign-off
    // is the REVIEWER's work now — pushing the author asks for a self-ack the
    // transition refuses.
    let rev = row.reviewer.clone().unwrap_or_default().trim().to_string();

    // THE REVIEWER'S PROMISE (AMUX-3820). The complement of the edge below, and
    // the half neither it nor the `reviewer_unreachable` WARN can see: both
    // watch a card sitting IN `review`, and a reviewer who moves the card out
    // to unblock the author leaves that predicate by construction.
    //
    // That move is CORRECT and this must not undo it — leaving a rejected card
    // in `review` re-nudges the reviewer to review their own rejection, which
    // is AF-214's whole subject. The defect is that the honest move drops the
    // promise on the floor: the card reads `doing`, looks healthy, and nothing
    // tells the reviewer when the author is done. AF-203 sat that way for four
    // days and was acked only because its author went and asked.
    //
    // Ordered ABOVE the reviewer edge but mutually exclusive with it by
    // `reviewer_acts_next`, so neither can shadow the other.
    if !reviewer_acts_next(&status)
        && !rev.is_empty()
        && rev != session
        && reviewer_left_review(row.log.as_deref().unwrap_or(""), &rev)
    {
        // Same reachability gate as the edge below: a nudge to a name that is
        // not a worker goes nowhere, and the card looks healthy either way.
        if let Some(why) = reviewer_unreachable(session, &rev) {
            return Advance::None {
                reason: "reviewer-unreachable",
                detail: format!("{card_id} names reviewer `{rev}`: {why}"),
            };
        }
        let spent = card_event_count(conn, "advance.nudged", &card_id, day_ago);
        if spent >= budget {
            return Advance::None {
                reason: "budget-spent",
                detail: format!("reviewer {rev} nudged {spent}x in 24h on {card_id}"),
            };
        }
        return Advance::Nudge {
            text: format!(
                "You reviewed `{card_id}` ({}) and moved it out of `review` yourself, which \
                 means you owe it a second look — that move is what took it off every list \
                 that would have reminded you. It now reads `{status}` and its author has \
                 had it since. Read what landed and either ack it or say what still fails. \
                 If you are no longer the right reviewer, say so on the card and clear the \
                 field rather than leaving a promise nobody is tracking.",
                row.title
            ),
            target: rev,
            card: card_id,
            status,
            kind: "review-promised",
        };
    }

    let mut ball_with_author = String::new();
    if reviewer_acts_next(&status) && !rev.is_empty() && rev != session {
        // A REVIEWER WHO IS NOT A WORKER IS A CARD PARKED FOREVER.
        //
        // `reviewer` is free text and nothing validates it when it is written,
        // so a typo, a human's name, or a lane that has since been RENAMED all
        // address the nudge to somebody who cannot receive it. Measured
        // 2026-08-26: 7 open cards sat in `review` naming a reviewer that
        // resolves to no registered worker — including `amux-rust`, renamed to
        // `amux` long ago, because the rename cascade migrated
        // `issues.session` and left `issues.reviewer` on the dead name (fixed
        // in the same commit).
        //
        // NAMED and WARNed rather than silently skipped, because a nudge that
        // goes nowhere is indistinguishable from a reviewer who is merely slow,
        // and the card looks healthy in `review` either way (ethos rule 4).
        // REACHABLE, NOT MERELY REGISTERED (AMUX-3771, Ethan: "figure out why
        // `@random` is peer reviewing code outside of its group").
        //
        // This gate used to ask only "is `rev` a registered worker", which is
        // one predicate short. A registered CROSS-GROUP reviewer passed it, got
        // nudged, and the owner then could not talk to them: worker-to-worker
        // messaging is open across groups unless explicitly opted out, and `cross_group_send_ok`
        // is enforced in exactly ONE place — the send API. The review nudge goes
        // through `steer_enqueue` and never touches it.
        //
        // So review crossed groups silently while conversation about it did
        // not. Measured 2026-08-26: ECOLO-14 (ecology/customers -> random/
        // personal) and AMUX-3761 (amux/amux -> random/personal, set by me).
        // `random`'s CC_DESC advertises "Peer review for other lanes" — which is
        // what every lane reads when picking a reviewer — while its config
        // grants no such reach. The roster promised a role the mechanism did not
        // implement (ethos rule 1).
        //
        // Refusing here does NOT decide whether random should be the fleet
        // reviewer. It makes the answer explicit: grant the reach with
        // CC_RECEIVE_ANY / CC_SEND_ALLOW, which the guard already provides for
        // exactly this case, or stop assigning across groups.
        if let Some(why) = reviewer_unreachable(session, &rev) {
            tracing::warn!(
                card = %card_id, reviewer = %rev, owner = %session, reason = %why,
                "reviewer_unreachable: card parked in review naming a reviewer this lane \
                 cannot reach — the nudge goes nowhere or the conversation about it does, \
                 and the card looks healthy in `review` either way"
            );
            // TELL THE AUTHOR, DO NOT JUST WARN (AMUX-3821). This used to
            // return `Advance::None`, so the one party who can fix it — the
            // card's owner, who wrote the reviewer field — was the only party
            // never told. The WARN reached a log; the card sat in `review`
            // claiming to await a reviewer who will never be reached.
            //
            // Measured 2026-08-28: 18 non-terminal cards fleet-wide name
            // `ethan`, 6 of them sitting in `review`. Naming the human owner is
            // SEMANTICALLY RIGHT — some work wants his eyes and no peer
            // substitutes — and mechanically dead, so refusing it at write time
            // would forbid a true intent and push people to name a peer who is
            // not the right reviewer (ethos rule 3). The honest move is to say
            // so and hand over the two ways out.
            //
            // `why` already carries both ("reassign to a lane, or move the card
            // to needsyou if a human owes the review"), which is why this needs
            // no new vocabulary and covers the typo and renamed-lane cases too
            // — those were parking just as silently.
            let spent = card_event_count(conn, "advance.nudged", &card_id, day_ago);
            if spent >= budget {
                return Advance::None {
                    reason: "budget-spent",
                    detail: format!("{card_id} unreachable-reviewer nudged {spent}x in 24h"),
                };
            }
            return Advance::Nudge {
                text: format!(
                    "`{card_id}` ({}) is in `{status}` naming reviewer `{rev}`, and that review \
                     will never arrive: {why} Nothing is nudging them and nothing was nudging \
                     you either, so the card has been reading as healthy while waiting on \
                     nobody. It is YOUR field to correct — you wrote it.",
                    row.title
                ),
                target: session.to_string(),
                card: card_id,
                status,
                kind: "advance-nudged",
            };
        }
        match reviewer_has_responded(conn, &card_id, &rev) {
            // BALL IS WITH THE AUTHOR — SO TELL THE AUTHOR (py:13761, AMUX-2498).
            // Python said exactly this in a log line and then nudged nobody, so
            // the card sat with both parties silent and the author never learned
            // they were unblocked. 70 cards across 12 lanes were in this state.
            Some(how) => {
                ball_with_author = format!(
                    "{rev} has already responded (their {how} is the most recent action on this \
                     card), so it is YOUR move — read their response on the card and either \
                     satisfy it or say why you disagree"
                );
            }
            None => {
                // Charge the SAME per-card budget the holder edge uses (py:13782,
                // AC-220): this edge returned before ever reaching the cap, so the
                // budget was enforced on one of two symmetric paths and a reviewer
                // was nudged three times for one card.
                let spent = card_event_count(conn, "advance.nudged", &card_id, day_ago);
                if spent >= budget {
                    return Advance::None {
                        reason: "budget-spent",
                        detail: format!("reviewer {rev} nudged {spent}x in 24h on {card_id}"),
                    };
                }
                // AF-214: they have already reviewed it. Asking again is asking
                // them to review their own rejection, and the instruction this
                // nudge gives ("say what fails on the card") is what put the card
                // in this state — so the nudge re-fires on compliance.
                if reviewer_acted_since_review(row.log.as_deref().unwrap_or(""), &rev) {
                    return Advance::None {
                        reason: "reviewer-already-acted",
                        detail: format!(
                            "{rev} has written to {card_id} since it entered review — \
                             the card awaits its AUTHOR, not a reviewer"
                        ),
                    };
                }
                // Name the ACTUAL transition. A card at `done` told to "ack
                // review->done" is being pointed at a move it already made.
                let next = advance_target(&status)
                    .map(bs::db_status_spelling)
                    .unwrap_or("done");
                let text = format!(
                    "[amux] {card_id} ({}) sits in '{status}' and names YOU as reviewer. Review \
                     it: if the work holds, ack {status}->{next} yourself (your X-Amux-Session is \
                     the required sign-off); if not, say what fails on the card. The author \
                     cannot close it.",
                    quoted_card_text(&row.title.chars().take(80).collect::<String>(), &card_id)
                );
                return Advance::Nudge {
                    target: rev,
                    card: card_id,
                    status,
                    text,
                    kind: "review-routed",
                };
            }
        }
    }

    // The author nudge.
    let term = if status_applies(conn, "verified", session, tags) {
        "verified"
    } else {
        "done"
    };
    if status == "done" && term != "verified" {
        // `done` IS terminal for this lane, so there is nothing to advance to
        // and the nudge could only re-fire forever with no honest exit.
        return Advance::None {
            reason: "terminal-for-lane",
            detail: format!("{card_id} is done and `verified` does not apply to {session}"),
        };
    }
    let gate_next = advance_target(&status).unwrap_or(TaskStatus::Done);
    // SAME resolver as the enforcer (AMUX-2641). The nudge QUOTES this gate;
    // if it derived the gate differently from the PATCH that enforces it, an
    // operator editing a column would get nudged toward criteria the server
    // then refuses — the two going stale together is the defect this card is
    // about, and one shared call is what keeps them from diverging.
    let gate = bs::effective_gate_configured(conn, &row, gate_next);
    let gate_txt = if gate.is_empty() {
        "  (no gate configured)".to_string()
    } else {
        gate.iter().map(|g| format!("  - {g}")).collect::<Vec<_>>().join("\n")
    };
    let gate_next_s = bs::db_status_spelling(gate_next);
    let queued: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM issues WHERE session=?1 AND deleted IS NULL \
             AND COALESCE(archived,0)=0 AND status IN ('todo','backlog') AND owner_type='agent'",
            rusqlite::params![session],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let text = advance_text(AdvanceMsg {
        card: &card_id,
        status: &status,
        title: &row.title,
        gate_next: gate_next_s,
        gate_txt: &gate_txt,
        term,
        queued,
        reviewer: &rev,
        ball_with_author: &ball_with_author,
        item_type: &row.item_type,
        has_evidence: board_has_evidence(&row.desc),
    });
    Advance::Nudge {
        target: session.to_string(),
        card: card_id,
        status,
        text,
        kind: "advance-nudged",
    }
}

/// py:15727 `_BOARD_EVIDENCE_RE` — does the desc carry a commit/PR/merge
/// reference? Advisory only: it decides nothing, it just changes how loudly the
/// retype hint speaks.
fn board_has_evidence(desc: &str) -> bool {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(commit|sha|merged?|pull request|pr #|#\d+|[0-9a-f]{7,40})\b")
            .expect("evidence regex")
    });
    re.is_match(desc)
}

struct AdvanceMsg<'a> {
    card: &'a str,
    status: &'a str,
    title: &'a str,
    gate_next: &'a str,
    gate_txt: &'a str,
    term: &'a str,
    queued: i64,
    reviewer: &'a str,
    ball_with_author: &'a str,
    item_type: &'a str,
    has_evidence: bool,
}

/// The advance nudge body. Compressed to reduce token waste (AMUX-3767):
/// the old template was ~1,500 chars per nudge, 443 nudges/day = ~693K chars/day
/// of automated prompts the model has to process.
fn advance_text(m: AdvanceMsg<'_>) -> String {
    let reviewer_owns_gate = !m.reviewer.is_empty() && reviewer_acts_next(m.status);
    let eff_type = if m.item_type.trim().is_empty() {
        DEFAULT_ITEM_TYPE
    } else {
        m.item_type.trim()
    };
    let retype = if eff_type.eq_ignore_ascii_case("code") && !m.has_evidence {
        format!(
            " Looks mis-typed: no commit/PR ref. If not code, retype: \
             `amux board type {} <investigation|research|doc|chore|ops>`.",
            m.card
        )
    } else {
        String::new()
    };
    let option_one = if reviewer_owns_gate {
        format!(
            "1) Address {}'s feedback, then ping them to re-ack (do NOT force).",
            m.reviewer
        )
    } else {
        format!(
            "1) Advance to '{}'. Gate:\n{}\n   Satisfy honestly and move it.",
            m.gate_next, m.gate_txt
        )
    };
    let ball = if m.ball_with_author.is_empty() {
        String::new()
    } else {
        format!(" {}", m.ball_with_author)
    };
    format!(
        "[amux] Idle with {} in '{}': {}{}\n\n\
         {}{}\n\
         2) Finished? Close to {} with evidence.\n\
         3) Blocked? Work the blocker. External block: `amux board backlog {} --trigger \"<condition>\"`.\n\
         4) Needs human? Record it, pick up next card.\n\
         5) Mis-shaped (not done-able)? Discard or retype to watch.\n\n\
         {} more queued. Update the card desc with current state before moving on.",
        m.card,
        m.status,
        quoted_card_text(&m.title.chars().take(110).collect::<String>(), m.card),
        ball,
        option_one,
        retype,
        m.term,
        m.card,
        m.queued,
    )
}

/// py:12964 `_needsyou_renag` — ONE stale-ask re-nag, deduped, shared by both
/// callers.
///
/// There were two copies of this in Python and they diverged: one recorded the
/// durable event and suppressed within the window, the other checked only tag
/// age and re-sent every ~15 minutes forever. One implementation, two callers.
///
/// COMPLIANCE RESETS THE WINDOW. The message asks the lane to "re-state it on
/// the card so it resurfaces fresh"; keying purely on tag age made that
/// instruction unsatisfiable, and a guard whose prescribed remedy cannot clear
/// it teaches sessions to ignore it (ethos rule 3). Returns None when the ask is
/// deduped or re-confirmed — i.e. when the honest output is silence.
fn needsyou_renag_text(
    conn: &Connection,
    _session: &str,
    card: &str,
    title: &str,
    asked_age: f64,
    archived: i64,
    now: f64,
) -> Option<String> {
    let win = needsyou_renag_days() * 86400.0;
    let last_ts: f64 = conn
        .query_row(
            "SELECT COALESCE(MAX(ts),0) FROM session_events WHERE type='needsyou.renag' \
             AND data LIKE ?1",
            rusqlite::params![format!("%\"{card}\"%")],
            |r| r.get(0),
        )
        .unwrap_or(0.0);
    // Cooldown only: once re-nagged, stay quiet for one window. `last_ts=0` (never
    // re-nagged) falls through and fires — the first reminder should.
    if last_ts > 0.0 && (now - last_ts) < win {
        return None;
    }
    // REMOVED (AF-465): the `updated > last_ts` "re-stated since we last asked, say
    // nothing" check. It made re-statement a silence lever, which is the abuse
    // vector the decision closed — a lane could keep a human's ask quiet forever by
    // touching the card every window, and the `updated` bump fired on ANY desc
    // write, not a deliberate re-statement. It also silenced only AFTER the first
    // re-nag (last_ts>0), so the escape the old text advertised did not work the
    // one time a lane tried it (AF-111). Keep MIN(added_at) as the monotonic ask
    // clock and the cooldown above; re-statement no longer silences. The nudge now
    // fires once per window until the HUMAN answers or the ask is cleared as
    // overtaken — loud nagging over quiet suppression, per the decision.
    let days = (asked_age / 86400.0) as i64;
    // AF-552, reported by gtm-engine with two live instances (GE-537, GE-583).
    //
    // This branch used to stop after "does NOT clear the ask", and the shared
    // tail below then prescribed "clear needs:you and move the card" — which
    // the board REFUSES on an archived card with `archived_task_immutable`
    // ("task is archived; restore it first"). The disclosure and the
    // instruction contradicted each other in adjacent sentences: the nudge
    // KNEW the card was archived, said so, and prescribed the one action that
    // state forbids.
    //
    // `amux board unarchive <ID>` has existed since AMUX-2492 and this text
    // never named it, so every lane rediscovered it or gave up. gtm-engine got
    // there by PATCHing {"archived":0} by hand — off the sanctioned path, for a
    // verb that was already sitting in the CLI. Ethos rule 1: a capability that
    // exists but does not reach the model is a capability nobody has.
    let arch = if archived != 0 {
        format!(
            "This card is ARCHIVED, which does NOT clear the ask — needs:you stays visible \
             to the human by design.\n\nAND ARCHIVED IS IMMUTABLE: every closing verb \
             refuses it with `archived_task_immutable` (\"task is archived; restore it \
             first\"). So the move below is TWO steps here, not one:\n\n    amux board \
             unarchive {card}\n\nthen clear needs:you and move it as normal.\n\n"
        )
    } else {
        String::new()
    };
    // The old text promised "Re-state it on the card (silences this for Nd)".
    // That was false, measured on AF-111 (AF-465): re-statement is meant to
    // silence via the `updated > last_ts` check above, but that check is gated on
    // `last_ts > 0`, so the FIRST re-nag skips it and fires regardless — which is
    // exactly when a lane tries the escape. It is also unreliable after, since any
    // desc write bumps `updated`. So we no longer tell the lane an action it
    // cannot rely on. A needs:you card is waiting on the HUMAN (the age-gate above
    // says as much: "the human owes the answer, not the lane"), and the ONE thing
    // the lane can actually do is clear an OVERTAKEN ask — which is the drain that
    // keeps needs:you from accreting. We do NOT claim the owner is reminded
    // elsewhere: whether the owner digest carries needs:you cards is unverified in
    // this file, so promising it here would be the same assert-without-reading the
    // old line was. AF-465's full split (route the reminder to the owner digest,
    // drop the lane nudge entirely) waits on confirming that producer.
    Some(format!(
        "[amux] {card} needs:you for {days}d — waiting on the HUMAN, not the lane: {}\n\n\
         {arch}The only lane action here is if the ask is OVERTAKEN: clear needs:you and \
         move the card. Re-stating does NOT silence this (it never reliably did — the first \
         re-nag ignores it; AF-465).",
        quoted_card_text(&title.chars().take(90).collect::<String>(), card),
    ))
}

// ---------------------------------------------------------------------------
// The tick
// ---------------------------------------------------------------------------

/// Park every blocked `doing` card before WIP, resume, or current-card truth is
/// evaluated.
///
/// `blocked_on` and `depends_on` used to be selection-only fields: pickup would
/// correctly refuse the card, but its stale `doing` label still consumed WIP
/// and `/api/sessions` advertised it as the worker's current task. TubeScience's
/// TUBES-2459 was the live specimen. The repair is a race-safe lifecycle write,
/// not a display exemption: `backlog` is the durable state for work that cannot
/// run, and the existing dependency/revisit machinery is what brings it back.
/// Isolated workers remain outside board automation entirely.
async fn normalize_blocked_doing(state: &AppState) -> usize {
    let candidates: Vec<String> = match state.store.read() {
        Ok(conn) => bs::list_issues(
            &conn,
            &["doing".to_string()],
            &[],
            bs::ArchivedFilter::ActiveOnly,
        )
        .unwrap_or_default()
        .into_iter()
        .filter(|row| {
            row.owner_type == "agent"
                && !is_dormant_type(&row.item_type)
                && row
                    .session
                    .as_deref()
                    .is_some_and(|lane| !crate::api::session_verbs::session_is_isolated(lane))
                && !doing_is_unblocked(&conn, row)
        })
        .map(|row| row.id)
        .collect(),
        Err(error) => {
            tracing::warn!(
                target: "amux::board_drive",
                %error,
                measured = false,
                n_considered = 0,
                verdict = "blocked_doing_scan_unmeasured",
                "board_drive: could not scan for blocked Doing cards"
            );
            return 0;
        }
    };

    let mut normalized = 0usize;
    for card in candidates {
        let card_w = card.clone();
        let reply = state
            .store
            .write_async(move |conn| {
                let Some(row) = bs::get_issue(conn, &card_w)? else {
                    return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
                };
                if row.status != "doing"
                    || row.owner_type != "agent"
                    || row.archived != 0
                    || is_dormant_type(&row.item_type)
                    || doing_is_unblocked(conn, &row)
                {
                    return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
                }
                let lane = row.session.clone().unwrap_or_default();
                if lane.is_empty() || crate::api::session_verbs::session_is_isolated(&lane) {
                    return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
                }
                let blocking = deps_blocking(conn, &row);
                let blocked_on = row
                    .blocked_on
                    .as_deref()
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty())
                    .unwrap_or("")
                    .to_string();
                let detail = dep_block_reasons(conn, &blocking).join(",");
                let reason = if !blocked_on.is_empty() && !blocking.is_empty() {
                    format!("blocked_on={blocked_on}; unresolved depends_on={detail}")
                } else if !blocked_on.is_empty() {
                    format!("blocked_on={blocked_on}")
                } else {
                    format!("unresolved depends_on={detail}")
                };
                let opts = crate::db::advance::AdvanceOpts {
                    expected_from: Some("doing".into()),
                    force: true,
                    skip_continuation: true,
                    skip_todo_wip: true,
                    log_line: Some(format!(
                        "Auto-parked from Doing: {reason}; blocked work cannot consume WIP"
                    )),
                    ..Default::default()
                };
                match crate::db::advance::advance(
                    conn,
                    &card_w,
                    "backlog",
                    "board_drive",
                    &opts,
                )? {
                    Ok(outcome) => {
                        conn.execute(
                            "INSERT INTO session_events (ts,session,type,data,source) \
                             VALUES (?1,?2,'task.blocked_normalized',?3,'board-drive')",
                            rusqlite::params![
                                now_f64(),
                                lane,
                                json!({
                                    "issue": card_w,
                                    "from": "doing",
                                    "to": "backlog",
                                    "reason": reason,
                                    "blocking": blocking,
                                    "blocked_on": blocked_on,
                                    "measured": true,
                                    "n_considered": 1,
                                    "verdict": "blocked_doing_parked"
                                })
                                .to_string()
                            ],
                        )?;
                        Ok(crate::db::WriteOutcome { applied: true, events: outcome.events })
                    }
                    Err(refusal) => {
                        tracing::warn!(
                            target: "amux::board_drive",
                            card = %card_w,
                            ?refusal,
                            measured = true,
                            n_considered = 1,
                            verdict = "blocked_doing_normalization_refused",
                            "board_drive: transition engine refused blocked Doing normalization"
                        );
                        Ok(crate::db::WriteOutcome { applied: false, events: vec![] })
                    }
                }
            })
            .await;
        match reply {
            Ok(result) if result.applied => {
                normalized += 1;
                tracing::warn!(
                    target: "amux::board_drive",
                    %card,
                    measured = true,
                    n_considered = 1,
                    verdict = "blocked_doing_parked",
                    "board_drive: blocked Doing card normalized to backlog before WIP selection"
                );
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(
                target: "amux::board_drive",
                %card,
                %error,
                measured = false,
                n_considered = 1,
                verdict = "blocked_doing_normalization_write_failed",
                "board_drive: blocked Doing normalization could not commit"
            ),
        }
    }
    normalized
}

/// One sweep over the fleet. ADVANCE BEFORE PICKUP, the same order and the same
/// two calls Python's idle edge used (py:14389): a lane holding a doing/review
/// card cannot be helped by pickup — WIP-1 forbids a second card — so a
/// successful nudge ends that lane's turn.
pub async fn drive_tick<F: Fleet>(state: &AppState, fleet: &F) -> DriveReport {
    let tick = TICK_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut report = DriveReport {
        tick,
        started_at: now_f64(),
        ..Default::default()
    };
    // A blocked card cannot be current Doing state. Normalize first so every
    // later predicate in this tick reads the same durable lifecycle truth.
    report.normalized_blocked_doing = normalize_blocked_doing(state).await;
    // Complete root epics before dispatch so the Messages chip and the board
    // agree that a command is finished as soon as all of its leaves are.
    report.completed_epics = complete_finished_epics(state).await;
    // DRIVE TO VERIFIED, BEFORE DISPATCH. A card parked in `backlog` on a
    // `depends_on` dependency re-activates to `todo` the moment every dependency
    // reaches a terminal status, so a "do B after A" command completes instead
    // of stalling at `parked` forever. Fleet-wide and lane-independent (a
    // dependency clearing does not care which lane is at a boundary), and FIRST
    // so a freshly promoted card is dispatchable in this same tick's lane loop.
    let (promoted, held_on_trigger) = promote_ready_backlog(state).await;
    // The revisit arm runs in the same pre-lane-loop position as the deps arm,
    // so a card whose date arrived this tick is dispatchable in this tick.
    let (promoted_due, revisit_due_total) = promote_due_backlog(state).await;
    report.promoted = promoted;
    report.held_on_trigger = held_on_trigger;
    report.promoted_due = promoted_due;
    report.revisit_due_total = revisit_due_total;
    for lane in fleet.lanes() {
        let trace = drive_lane(state, fleet, &lane).await;
        match trace.outcome.as_str() {
            "assigned" => report.assigned += 1,
            "skipped" => {}
            _ => report.nudged += 1,
        }
        report.lanes.push(trace);
    }
    report.finished_at = now_f64();
    // THE FLEET NUMBER, IN THE LOGS, EVERY TICK (AMUX-3758). A lane going idle
    // on a full board produces a different local reason per lane — wip-cap,
    // needsyou-renag-deduped, dep-other-lane — and each reads like a small bug.
    // Only the aggregate says what is actually happening, and until now it
    // existed nowhere: measured 2026-08-26, 4.9% of 1039 open cards were in the
    // one status this loop can dispatch. `assigned=0` on a fleet with 1000 open
    // cards was the reader's whole signal, and it does not distinguish "the
    // loop is broken" from "there are 51 workable cards for 52 lanes".
    //
    // INFO always (a clean pass must be distinguishable from a census that
    // measured nothing), WARN only when a LARGE board has almost nothing
    // workable — the state where the fleet looks busy and is not.
    if let Ok(conn) = state.store.read() {
        let shape = fleet_queue_shape(&conn);
        let (open, pct) = (
            shape["open_total"].as_i64().unwrap_or(0),
            shape["dispatchable_pct"].as_f64().unwrap_or(0.0),
        );
        tracing::info!(
            job = "board-drive",
            open_total = open,
            dispatchable_todo = shape["dispatchable_todo"].as_i64().unwrap_or(0),
            dispatchable_pct = pct,
            needsyou_median_age_d = shape["needsyou_median_age_d"].as_f64(),
            assigned = report.assigned,
            "board queue shape"
        );
        if open >= 200 && pct < 10.0 {
            tracing::warn!(
                job = "board-drive",
                open_total = open,
                dispatchable_pct = pct,
                needsyou = shape["by_status"]["needsyou"].as_i64(),
                backlog = shape["by_status"]["backlog"].as_i64(),
                needsyou_median_age_d = shape["needsyou_median_age_d"].as_f64(),
                "queue_starved: the board is large and almost none of it is dispatchable — \
                 lanes will idle with full queues and it is not this loop's fault. \
                 `backlog` needs a trigger, `needsyou` needs a human; neither is amux's to \
                 clear on its own. GET /api/debug/board-drive -> queue_shape"
            );
        }
    }
    publish(&report);
    report
}

/// Event-driven arm for one live lane. It intentionally goes through the same
/// fleet membership, isolation, configuration, liveness and boundary gates as
/// the periodic sweep; the sweep remains the backstop.
pub async fn drive_session(state: &AppState, lane: &str) -> LaneTrace {
    let fleet = LiveFleet {
        state: state.clone(),
        signals: crate::api::session_verbs::boundary_signals(state, None).await,
    };
    if !fleet.lanes().iter().any(|candidate| candidate == lane) {
        let trace = if crate::api::session_verbs::session_is_isolated(lane) {
            LaneTrace::skip(lane, "isolated", "isolated workers never receive board automation")
        } else {
            LaneTrace::skip(lane, "not-registered", "worker is not in the live fleet registry")
        };
        publish_lane(trace.clone());
        return trace;
    }
    let _ = normalize_blocked_doing(state).await;
    let _ = complete_finished_epics(state).await;
    let _ = promote_ready_backlog(state).await;
    let _ = promote_due_backlog(state).await;
    let trace = drive_lane(state, &fleet, lane).await;
    publish_lane(trace.clone());
    trace
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChildClaimReconcile {
    /// One exact parked card was moved back into Doing in the same transaction
    /// as its replacement causal marker.
    Restored(String),
    /// The exact card was already Doing; the child signal merely confirms it.
    Current(String),
    /// A newer informational/control turn explicitly owns the cardless state.
    Cardless,
    /// There was no exact, safe card to restore.
    None,
}

/// Reassert the exact parent card while delegated work is observably alive.
///
/// Providers commonly yield their parent composer while a subagent or a
/// background terminal keeps doing the task.  Some models also "park" that
/// card back in To Do while they wait.  The status hook correctly reports the
/// live child, but the old board projection then has to choose between two
/// lies: WORKING with no card, or an idle-looking To Do card whose work is
/// running.  Repair the lifecycle at the write boundary instead.
///
/// This never guesses from queue order or title similarity.  It considers only
/// the newest semantic task marker for this lane.  A newer cardless
/// informational/control marker wins, a Backlog/Review/terminal card stays
/// where its gate put it, blockers remain blockers, and another Doing card is
/// a WIP conflict.  Only an exact owned To Do claim can be restored.
async fn reconcile_child_task_claim(state: &AppState, lane: &str) -> ChildClaimReconcile {
    let lane = lane.to_string();
    let lane_w = lane.clone();
    let slot = std::sync::Arc::new(std::sync::Mutex::new(ChildClaimReconcile::None));
    let slot_w = slot.clone();
    let result = state
        .store
        .write_async(move |conn| {
            let mut marker: Option<(String, Option<String>)> = None;
            let mut statement = conn.prepare(
                "SELECT type, data FROM session_events WHERE session=?1 \
                 AND type IN ('task.claimed','task.cardless') ORDER BY ts DESC, id DESC LIMIT 128",
            )?;
            let rows = statement.query_map([&lane_w], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?;
            for row in rows.flatten() {
                let parsed = row
                    .1
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                    .unwrap_or(serde_json::Value::Null);
                if row.0 == "task.cardless" {
                    if crate::api::sessions_legacy::cardless_event_allowed(&parsed) {
                        marker = Some((row.0, None));
                        break;
                    }
                    continue;
                }
                let card = parsed["issue"]
                    .as_str()
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                if card.is_some() {
                    marker = Some((row.0, card));
                    break;
                }
            }
            drop(statement);

            let Some((kind, card)) = marker else {
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            };
            if kind == "task.cardless" {
                *slot_w.lock().unwrap_or_else(|poison| poison.into_inner()) =
                    ChildClaimReconcile::Cardless;
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            }
            let Some(card) = card else {
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            };
            let Some(row) = bs::get_issue(conn, &card)? else {
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            };
            if row.session.as_deref() != Some(lane_w.as_str()) || row.owner_type != "agent" {
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            }
            if row.status == "doing" {
                *slot_w.lock().unwrap_or_else(|poison| poison.into_inner()) =
                    ChildClaimReconcile::Current(card);
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            }
            if row.status != "todo"
                || row.archived != 0
                || row.tags.iter().any(|tag| tag.to_ascii_lowercase().starts_with("needs:you"))
                || row.waiting_on.as_deref().is_some_and(|value| !value.trim().is_empty())
                || fresh_source_ref_trigger(&row, now_f64() as i64)
                || matches!(row.item_type.as_str(), "tripwire" | "watch" | "epic")
                || !deps_blocking(conn, &row).is_empty()
            {
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            }
            let other_wip: i64 = conn.query_row(
                "SELECT COUNT(*) FROM issues WHERE session=?1 AND id<>?2 \
                 AND status='doing' AND deleted IS NULL AND COALESCE(archived,0)=0 \
                 AND COALESCE(type,'') NOT IN ('tripwire','watch','epic')",
                rusqlite::params![&lane_w, &card],
                |row| row.get(0),
            )?;
            if other_wip > 0 {
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            }
            let opts = crate::db::advance::AdvanceOpts {
                expected_from: Some("todo".to_string()),
                assign_to: Some(lane_w.clone()),
                log_line: Some(format!(
                    "Restored to Doing by {lane_w}: an identity-backed child is still working this exact claimed task"
                )),
                force: true,
                skip_continuation: true,
                ..Default::default()
            };
            match crate::db::advance::advance(conn, &card, "doing", &lane_w, &opts)? {
                Ok(outcome) => {
                    conn.execute(
                        "INSERT INTO session_events (ts,session,type,data,source) VALUES (?,?,?,?,?)",
                        rusqlite::params![
                            now_f64(),
                            &lane_w,
                            "task.claimed",
                            serde_json::json!({
                                "issue": &card,
                                "status": "doing",
                                "from": "todo",
                                "reason": "identity-backed-child-work",
                                "measured": true,
                                "n_considered": 1,
                                "verdict": "exact_child_claim_restored",
                            })
                            .to_string(),
                            "board-drive",
                        ],
                    )?;
                    *slot_w.lock().unwrap_or_else(|poison| poison.into_inner()) =
                        ChildClaimReconcile::Restored(card);
                    Ok(crate::db::WriteOutcome { applied: true, events: outcome.events })
                }
                Err(_) => Ok(crate::db::WriteOutcome { applied: false, events: vec![] }),
            }
        })
        .await;
    if let Err(error) = result {
        tracing::warn!(
            target: "amux::board_drive",
            session = %lane,
            %error,
            measured = false,
            n_considered = 0,
            verdict = "child_claim_reconcile_failed",
            "identity-backed child work could not reconcile its exact board claim"
        );
        return ChildClaimReconcile::None;
    }
    let result = slot.lock().unwrap_or_else(|poison| poison.into_inner()).clone();
    if let ChildClaimReconcile::Restored(card) = &result {
        tracing::warn!(
            target: "amux::board_drive",
            session = %lane,
            %card,
            measured = true,
            n_considered = 1,
            verdict = "exact_child_claim_restored",
            "live child work restored its exact parked To Do card to Doing"
        );
    }
    result
}

fn publish_lane(trace: LaneTrace) {
    if let Ok(mut slot) = report_slot().write() {
        let report = slot.get_or_insert_with(|| DriveReport { started_at: now_f64(), ..Default::default() });
        report.lanes.retain(|row| row.session != trace.session);
        report.lanes.push(trace);
        report.finished_at = now_f64();
        report.assigned = report.lanes.iter().filter(|row| row.outcome == "assigned").count();
        report.nudged = report.lanes.iter().filter(|row| row.outcome != "assigned" && row.outcome != "skipped").count();
    }
}

async fn drive_lane<F: Fleet>(state: &AppState, fleet: &F, lane: &str) -> LaneTrace {
    let lane_lock = lane_drive_lock(state, lane);
    let _lane_guard = lane_lock.lock().await;
    // COUNT THE BACKLOG BEFORE THE GATES, ALWAYS. Found by reading this
    // instrument's own output during verification: a lane skipped for
    // `not-running` reported `eligible_todos: 0` while a dispatchable card was
    // sitting in its queue, because the counts were only filled in on the paths
    // that got past the gates. That is the ethos rule 4 failure in the very
    // surface built to prevent it — the reader's question is "how much work is
    // this lane sitting on, and why did it not get any", and half of that
    // answer was reported as zero. Every trace row now carries the true depth,
    // whatever stopped the lane.
    let (eligible, open) = match state.store.read() {
        Ok(conn) => (eligible_todo_count(&conn, lane, crate::config::now_f64()), open_card_count(&conn, lane)),
        Err(_) => {
            return LaneTrace::skip(lane, "store-unavailable", "could not open a read connection")
        }
    };

    if !fleet.auto_pickup_enabled(lane) {
        return LaneTrace::skip(lane, "opted-out", "CC_AUTO_PICKUP=0 in the session env")
            .with_counts(eligible, open);
    }
    if fleet.is_isolated(lane) {
        return LaneTrace::skip(lane, "isolated", "isolated workers never receive board automation")
            .with_counts(eligible, open);
    }
    // A stopped lane is woken only after a selector has found real work.
    // Re-select after start: the preflight authorizes a wake, not a stale card
    // mutation. An exact surviving claim comes first: resuming its own Doing
    // work is not a second WIP claim and must not be hidden by the WIP cap.
    let woke_for_dispatch = if !fleet.is_running(lane).await {
        let should_wake = match state.store.read() {
            Ok(conn) => match select_resume(&conn, lane, now_f64(), true) {
                Resume::Claim { .. } => true,
                // The worker, not a human or a server heuristic, owns this
                // reconciliation. Wake it without a singular resume card; the
                // post-start selector sends the complete candidate set.
                Resume::Conflicting { .. } => true,
                Resume::Current { .. } => true,
                Resume::None => matches!(
                    select_pickup(&conn, lane, now_f64()),
                    Pickup::Claim { .. } | Pickup::DrainBacklog { .. }
                ),
            },
            Err(_) => return LaneTrace::skip(lane, "store-unavailable", "could not preflight stopped worker")
                .with_counts(eligible, open),
        };
        if !should_wake {
            return LaneTrace::skip(
                lane,
                "not-running-no-dispatchable-work",
                "stopped worker was not started: selector found no eligible exact claim, todo or backlog work",
            )
            .with_counts(eligible, open);
        }
        match fleet.start_for_dispatch(lane).await {
            Ok(()) if fleet.is_running(lane).await => {
                tracing::info!(target: "amux::board_drive", session = lane,
                    measured = true, n_considered = 1, verdict = "eligible_work_woke_worker",
                    "board_drive: started stopped worker after eligible work preflight");
                true
            }
            Ok(()) => {
                tracing::warn!(target: "amux::board_drive", session = lane,
                    measured = true, n_considered = 1, verdict = "start_reported_not_running",
                    "board_drive: start returned success but worker was absent; no card was claimed");
                return LaneTrace::skip(lane, "start-not-running", "start returned success but no live worker remained")
                    .with_counts(eligible, open);
            }
            Err(error) => {
                tracing::warn!(target: "amux::board_drive", session = lane, %error,
                    measured = true, n_considered = 1, verdict = "eligible_work_start_failed",
                    "board_drive: eligible work did not claim because the worker failed to start");
                return LaneTrace::skip(lane, "start-failed", format!("eligible work left untouched: {error}"))
                    .with_counts(eligible, open);
            }
        }
    } else { false };
    // THE TURN-BOUNDARY GATE, reused from the steering path. Fails CLOSED:
    // anything not positively known to be idle is left alone. A nudge that waits
    // one more tick costs nothing; a nudge delivered mid-turn is an interruption.
    if !woke_for_dispatch && !fleet.at_boundary(lane).await {
        // A provider-owned child is positive evidence that the lane is doing
        // more than merely rendering an active spinner.  Reconcile its exact
        // parent claim before returning `mid-turn`, otherwise a model which
        // parked that card in To Do while waiting makes the board look idle
        // for the entire delegated operation (the live Primis/TubeScience
        // failure).  The helper refuses cardless turns, blockers and guesses.
        if fleet.active_child_work(lane) {
            match reconcile_child_task_claim(state, lane).await {
                ChildClaimReconcile::Restored(card) => {
                    return LaneTrace::skip(
                        lane,
                        "active-child-claim-restored",
                        format!(
                            "identity-backed child work is live; restored exact claimed card {card} from To Do to Doing"
                        ),
                    )
                    .with_card(&card)
                    .with_counts(eligible, open);
                }
                ChildClaimReconcile::Current(card) => {
                    return LaneTrace::skip(
                        lane,
                        "active-child-claim-current",
                        format!(
                            "identity-backed child work is live on exact Doing card {card}; no new work was delivered"
                        ),
                    )
                    .with_card(&card)
                    .with_counts(eligible, open);
                }
                ChildClaimReconcile::Cardless | ChildClaimReconcile::None => {}
            }
        }
        // NAME THE EVIDENCE, not just the verdict. `mid-turn` read identically
        // for a lane genuinely generating and for one held by a self-report
        // nothing would ever refresh — and the second kind sat here for up to
        // 61 hours (AMUX-3756). The gate now falls through to the pane for a
        // stale report, so this detail is what makes the remaining cases
        // legible: a fresh report age is a real turn, a huge one is the class
        // that used to deadlock.
        let detail = match crate::api::session_verbs::lane_report(state, lane) {
            Some(r) => format!(
                "lane is not at a turn boundary (self-report `{}` {:.0}s old, {}, subagents_live={:?})",
                r.state,
                r.age_s,
                if r.applies { "authoritative" } else { "REFUSED as stale — structured FleetSignals decided" },
                r.subagents_live
            ),
            None => "lane is not at a turn boundary (no self-report; structured FleetSignals decided)".into(),
        };
        return LaneTrace::skip(lane, "mid-turn", detail).with_counts(eligible, open);
    }

    let now = now_f64();
    let tags = fleet.tags(lane);
    let Ok(conn) = state.store.read() else {
        return LaneTrace::skip(lane, "store-unavailable", "could not open a read connection")
            .with_counts(eligible, open);
    };
    let resume = select_resume(&conn, lane, now, woke_for_dispatch);
    drop(conn);
    let mut current_claim = None;
    if !matches!(&resume, Resume::None) {
        match resume {
            Resume::Claim { card, prompt, delivery_id, cause } => {
                match fleet.deliver_resume(lane, &prompt, &delivery_id).await {
                Ok(disposition) => {
                    crate::api::session_verbs::record_resume_accepted(
                        state, lane, Some(&card), &delivery_id, cause,
                    ).await;
                    if disposition == ResumeDelivery::Queued {
                        tracing::info!(target: "amux::board_drive", session = lane, %card,
                            %delivery_id, cause, measured = true, n_considered = 1,
                            verdict = "exact_live_claim_resumed",
                            "board_drive: rehydrated and resumed exact live task claim once for this worker generation");
                        return LaneTrace::acted(
                            lane,
                            "resumed",
                            &card,
                            format!("queued one recovery delivery for {cause} ({delivery_id})"),
                        )
                        .with_counts(eligible, open);
                    }
                    tracing::warn!(target: "amux::board_drive", session = lane, %card,
                        %delivery_id, cause, delivery = ?disposition,
                        measured = true, n_considered = 1,
                        verdict = "resume_receipt_recovered_without_redelivery",
                        "board_drive: stable recovery delivery already existed; restored its causal receipt without re-steering the owner");
                    return LaneTrace::skip(
                        lane,
                        "resume-commit-recovered",
                        format!("{card} recovery {delivery_id} was already {disposition:?}; receipt restored without another delivery"),
                    )
                    .with_card(&card)
                    .with_counts(eligible, open);
                }
                Err(error) => {
                    crate::api::session_verbs::emit_event(
                        state,
                        lane,
                        "task.resume_failed",
                        Some(json!({"issue": &card, "error": &error, "measured": true,
                            "n_considered": 1, "verdict": "resume_delivery_failed",
                            "delivery_id": &delivery_id, "cause": cause})),
                        None,
                        "board-drive",
                    )
                    .await;
                    tracing::warn!(target: "amux::board_drive", session = lane, %card, %error,
                        %delivery_id, cause,
                        measured = true, n_considered = 1, verdict = "resume_delivery_failed",
                        "board_drive: exact live claim remains Doing after resume delivery refusal; retry is visible next tick");
                    return LaneTrace::skip(
                        lane,
                        "resume-delivery-failed",
                        format!("{card} remains Doing with its exact claim after resume delivery failed: {error}; retry next tick"),
                    )
                    .with_counts(eligible, open);
                }
                }
            }
            Resume::Current { card, delivery_id, cause } => {
                crate::api::session_verbs::emit_event(
                    state,
                    lane,
                    "task.resume_suppressed",
                    Some(json!({"issue": &card, "measured": true, "n_considered": 1,
                        "verdict": "active_owner_not_resteered", "delivery_id": &delivery_id,
                        "cause": cause})),
                    Some(format!("task-resume-suppressed:{delivery_id}")),
                    "board-drive",
                )
                .await;
                tracing::info!(target: "amux::board_drive", session = lane, %card,
                    %delivery_id, cause, measured = true, n_considered = 1,
                    verdict = "active_owner_not_resteered",
                    "board_drive: exact live claim remains runtime truth; repeat resume delivery suppressed");
                // Recovery is already committed, but an idle owner still needs
                // the ordinary cooldown/backoff-gated advancement decision.
                current_claim = Some((card, cause));
            }
            Resume::Conflicting { cards } => {
                let plan = match state.store.read() {
                    Ok(conn) => select_claim_reconcile(&conn, lane, &cards, now),
                    Err(_) => return LaneTrace::skip(
                        lane,
                        "store-unavailable",
                        "could not prepare automatic claim reconciliation",
                    ).with_counts(eligible, open),
                };
                let (prompt, delivery_id, signature, attempt) = match plan {
                    ClaimReconcilePlan::Ready { prompt, delivery_id, signature, attempt } => {
                        (prompt, delivery_id, signature, attempt)
                    }
                    ClaimReconcilePlan::Cooldown { remaining_s, attempt } => {
                        return LaneTrace::skip(
                            lane,
                            "claim-reconcile-cooldown",
                            format!(
                                "automatic reconciliation attempt {attempt} already requested; retry in {:.0}s if {} claims still survive: {}",
                                remaining_s.ceil(), cards.len(), cards.join(", ")
                            ),
                        )
                        .with_counts(eligible, open);
                    }
                };
                match fleet.deliver_resume(lane, &prompt, &delivery_id).await {
                    Ok(disposition) => {
                        crate::api::session_verbs::emit_event(
                            state,
                            lane,
                            "task.claim_reconcile_requested",
                            Some(json!({
                                "issue": cards.first(),
                                "cards": &cards,
                                "signature": &signature,
                                "delivery_id": &delivery_id,
                                "attempt": attempt,
                                "delivery": format!("{disposition:?}"),
                                "measured": true,
                                "n_considered": cards.len(),
                                "verdict": "model_reconciliation_requested",
                            })),
                            Some(format!("task-claim-reconcile:{delivery_id}")),
                            "board-drive",
                        )
                        .await;
                        tracing::warn!(target: "amux::board_drive", session = lane,
                            cards = %cards.join(","), %delivery_id, attempt,
                            delivery = ?disposition, measured = true,
                            n_considered = cards.len(), verdict = "model_reconciliation_requested",
                            "board_drive: handed every conflicting exact claim to its worker for automatic reconciliation");
                        return LaneTrace::acted(
                            lane,
                            "claim-reconcile",
                            cards.first().map(String::as_str).unwrap_or(""),
                            format!(
                                "automatic attempt {attempt} {disposition:?} for {} claims: {}",
                                cards.len(), cards.join(", ")
                            ),
                        )
                        .with_counts(eligible, open);
                    }
                    Err(error) => {
                        crate::api::session_verbs::emit_event(
                            state,
                            lane,
                            "task.claim_reconcile_failed",
                            Some(json!({
                                "cards": &cards,
                                "signature": &signature,
                                "delivery_id": &delivery_id,
                                "error": &error,
                                "measured": true,
                                "n_considered": cards.len(),
                                "verdict": "claim_reconcile_delivery_failed",
                            })),
                            None,
                            "board-drive",
                        )
                        .await;
                        return LaneTrace::skip(
                            lane,
                            "claim-reconcile-delivery-failed",
                            format!("automatic reconciliation for {} failed: {error}; retry next tick", cards.join(", ")),
                        )
                        .with_counts(eligible, open);
                    }
                }
            }
            Resume::None => unreachable!("non-empty resume selector was matched above"),
        }
    }
    let Ok(conn) = state.store.read() else {
        return LaneTrace::skip(lane, "store-unavailable", "could not select canonical advancement")
            .with_counts(eligible, open);
    };
    let advance = select_advance(&conn, lane, &tags, now);
    let pickup = match &advance {
        Advance::Nudge { .. } => None,
        Advance::None { .. } => Some(select_pickup(&conn, lane, now)),
    };
    drop(conn);

    if let Advance::Nudge { target, card, status, text, kind } = advance {
        // A NUDGE IS ABOUT A CARD, so it carries that card's revision and the
        // delivery loop drops it if the card moves first (AMUX-3659). Read
        // here, at compute time, because that is the state the text describes.
        // A card whose rev cannot be read delivers unconditionally — the same
        // behaviour as before, since refusing to nudge because a read failed
        // would be a worse trade than an occasionally-stale nudge.
        let rev: Option<i64> = state
            .store
            .read()
            .ok()
            .and_then(|c| {
                c.query_row("SELECT rev FROM issues WHERE id=?1", rusqlite::params![&card], |r| r.get(0))
                    .ok()
            });
        match rev {
            Some(r) => fleet.deliver_about(&target, &text, &card, r).await,
            None => fleet.deliver(&target, &text).await,
        }
        // THE COOLDOWN IS PER LANE, AND A REVIEW ROUTE INVOLVES TWO OF THEM.
        // `advance.nudged` is recorded under the REVIEWER (python:13817 — it
        // once stamped the card owner's cooldown while the message went to the
        // reviewer, so the reviewer's own cooldown was never touched). But then
        // NOTHING stamps the OWNER's lane, and the owner's lane is what
        // re-selects the same review card on the next tick. Measured live at
        // 22:07/22:08/22:09: amux-agent was nudged about AC-233 on three
        // consecutive ticks, 60s apart, stopping only when the per-CARD budget
        // hit 3 — a bound meant to span 24h, spent in three minutes.
        //
        // So the route writes a second, cheap marker under the OWNER. It is a
        // DIFFERENT type on purpose: `last_advance` counts it (the owner's lane
        // goes quiet for 15 minutes) while the per-card budget counts only
        // `advance.nudged`, so recording it cannot burn the reviewer's three
        // nudges at twice the rate.
        if kind == "review-routed" && target != lane {
            crate::api::session_verbs::emit_event(
                state,
                lane,
                "advance.routed",
                Some(json!({"issue": card, "status": status, "reviewer": target})),
                None,
                "board-drive",
            )
            .await;
        }
        // BOTH renag arms write the same event TYPE — the type is the fault
        // class and it should stay one thing. The arm is carried in `kind`
        // inside the data, which is where a per-branch counter can read it
        // without splitting the class (AMUX-3777).
        let etype =
            if kind.starts_with("renag") { "needsyou.renag" } else { "advance.nudged" };
        let idem = if kind == "decompose-asked" {
            Some(format!("decompose:{card}"))
        } else {
            None
        };
        // The event carries the card's STATUS, which Python kept only in memory.
        // That is what makes "did the lane make progress since we spoke?"
        // survive the restart this process takes on every deploy.
        crate::api::session_verbs::emit_event(
            state,
            &target,
            etype,
            Some(json!({"issue": card, "status": status, "kind": kind})),
            idem,
            "board-drive",
        )
        .await;
        return LaneTrace::acted(lane, kind, &card, format!("delivered to {target}"))
            .with_counts(eligible, open);
    }

    let advance_reason = match &advance {
        Advance::None { reason, detail } => (*reason, detail.clone()),
        Advance::Nudge { .. } => unreachable!("handled above"),
    };

    if let Some((card, cause)) = current_claim {
        return LaneTrace::skip(lane, "active-claim-current",
            format!("{card} remains the exact runtime card ({cause}); canonical advancement: {} — {}",
                advance_reason.0, advance_reason.1))
            .with_card(&card).with_counts(eligible, open);
    }

    match pickup.expect("pickup computed whenever advance declined") {
        Pickup::Claim { card, prompt } => {
            // Only dispatch "work it now" if the atomic claim actually took —
            // the card could have been closed between select_pickup and here
            // (AMUX-2983). A refused claim is not an error, it is the race being
            // caught: skip, do not deliver finished work.
            if claim_card(state, lane, &card).await {
                match fleet.deliver_work(lane, &prompt).await {
                    Ok(()) => LaneTrace::acted(lane, "assigned", &card, "claimed and prompt queued")
                        .with_counts(eligible, open),
                    Err(error) => {
                        let released = release_claim_after_dispatch_failure(state, lane, &card, "todo", &error).await;
                        LaneTrace::skip(lane,
                            if released { "delivery-failed-released" } else { "delivery-failed-release-raced" },
                            format!("{card} was not dispatched: {error}; {}", if released {
                                "claim was compensated back to todo"
                            } else { "compensation yielded to a concurrent card change; inspect board-drive WARN" }))
                            .with_counts(eligible, open)
                    }
                }
            } else {
                LaneTrace::skip(
                    lane,
                    "pickup-raced",
                    format!("{card} left 'todo' before the claim landed — not dispatched"),
                )
                .with_counts(eligible, open)
            }
        }
        Pickup::PromoteDeps { blocked_card, promoted } => {
            // The blocked todo's only blockers are this session's own backlog
            // cards. Promote them to todo now so the next select_pickup pass
            // can claim the unblocked card. No nudge — the lane gets work
            // silently. Log so the trace is auditable.
            let to_promote = promoted.clone();
            let result = state
                .store
                .write_async(move |conn| {
                    let mut promoted_ids: Vec<String> = Vec::new();
                    let mut all_events = Vec::new();
                    for dep_id in &to_promote {
                        let opts = crate::db::advance::AdvanceOpts {
                            expected_from: Some("backlog".into()),
                            skip_continuation: true,
                            log_line: Some("auto-promoted: own backlog dep cleared".into()),
                            ..Default::default()
                        };
                        if let Ok(outcome) = crate::db::advance::advance(conn, dep_id, "todo", "board_drive", &opts)? {
                            promoted_ids.push(dep_id.clone());
                            all_events.extend(outcome.events);
                        }
                    }
                    Ok(crate::db::WriteOutcome {
                        applied: !promoted_ids.is_empty(),
                        events: all_events,
                    })
                })
                .await;
            match result {
                Ok(reply) if reply.applied => {
                    tracing::info!(
                        session = lane, blocked = %blocked_card,
                        deps = ?promoted,
                        "board_drive: auto-promoted own backlog dep(s) → todo \
                         (self_owned_backlog_blocker); will dispatch next tick"
                    );
                    LaneTrace::acted(
                        lane,
                        "promote-deps",
                        &blocked_card,
                        format!(
                            "auto-promoted own backlog dep(s) {} → todo; \
                             {blocked_card} dispatched next tick",
                            promoted.join(", ")
                        ),
                    )
                    .with_counts(eligible, open)
                }
                Ok(_) => LaneTrace::skip(
                    lane,
                    "promote-deps-raced",
                    format!("deps of {blocked_card} moved before promotion landed"),
                )
                .with_counts(eligible, open),
                Err(e) => LaneTrace::skip(
                    lane,
                    "promote-deps-err",
                    format!("write failed promoting deps of {blocked_card}: {e}"),
                )
                .with_counts(eligible, open),
            }
        }
        Pickup::DrainBacklog { card, prompt, backlog_left, todo_refusals } => {
            // A direct CAS eliminates the old backlog -> todo -> next-tick gap.
            match claim_card_from_outcome(state, lane, &card, "backlog").await {
                ClaimCardOutcome::Claimed => match fleet.deliver_work(lane, &prompt).await {
                    Ok(()) => {
                        let remaining = backlog_left.saturating_sub(1);
                        crate::api::session_verbs::emit_event(state, lane, "backlog.drained",
                            Some(json!({"issue": card, "from": "backlog", "to": "doing", "started": true,
                                "backlog_left": remaining, "todo_refusals": todo_refusals})), None, "board-drive").await;
                        tracing::info!(target: "amux::board_drive", session = lane, card,
                            measured = true, n_considered = backlog_left, verdict = "backlog_claim_started",
                            "board_drive: auto-drained backlog directly into doing with a causal task claim");
                        LaneTrace::acted(lane, "assigned", &card,
                            format!("{card} backlog -> doing and prompt queued; {remaining} drainable left; bypassed {todo_refusals} unclaimable todo(s)"))
                            .with_counts(eligible, open)
                    }
                    Err(error) => {
                        let released = release_claim_after_dispatch_failure(state, lane, &card, "backlog", &error).await;
                        LaneTrace::skip(lane,
                            if released { "delivery-failed-released" } else { "delivery-failed-release-raced" },
                            format!("{card} backlog claim was not dispatched: {error}; {}", if released {
                                "claim was compensated back to backlog"
                            } else { "compensation yielded to a concurrent card change; inspect board-drive WARN" }))
                            .with_counts(eligible, open)
                    }
                },
                ClaimCardOutcome::DependencyBlocked(blocking) => LaneTrace::skip(lane, "drain-dependency-raced",
                    format!("{card} gained unfinished prerequisite(s): {}", blocking.join(", ")))
                    .with_counts(eligible, open),
                ClaimCardOutcome::NotApplied => LaneTrace::skip(lane, "drain-raced",
                    format!("{card} left backlog before the atomic claim landed"))
                    .with_counts(eligible, open),
            }
        }
        Pickup::ReclaimStale { card, held_h, blocking } => {
            // AMUX-4042. `expected_from: doing` makes this a CAS: if the lane
            // touched the card between select and write, the advance does not
            // apply and we say so rather than silently taking it anyway.
            let card_c = card.clone();
            let line = format!(
                "auto-reclaimed: untouched {held_h:.1}h while the lane sat idle at a turn \
                 boundary, holding the WIP slot against {blocking} claimable card(s)"
            );
            let result = state
                .store
                .write_async(move |conn| {
                    let opts = crate::db::advance::AdvanceOpts {
                        expected_from: Some("doing".into()),
                        skip_continuation: true,
                        log_line: Some(line.clone()),
                        ..Default::default()
                    };
                    let outcome =
                        crate::db::advance::advance(conn, &card_c, "todo", "board_drive", &opts)?;
                    Ok(crate::db::WriteOutcome {
                        applied: outcome.is_ok(),
                        events: outcome.map(|o| o.events).unwrap_or_default(),
                    })
                })
                .await;
            match result {
                Ok(o) if o.applied => {
                    // The audit trail for the one mutation amux makes on the
                    // owner's behalf. It has to be findable later by someone
                    // asking "who moved my card".
                    crate::api::session_verbs::emit_event(
                        state,
                        lane,
                        "pickup.reclaimed_stale",
                        Some(json!({"issue": card, "held_h": held_h, "blocking": blocking})),
                        None,
                        "board-drive",
                    )
                    .await;
                    tracing::warn!(
                        session = lane, card = %card, blocking,
                        cooldown_s = reclaim_cooldown_s(),
                        verdict = "stale_reclaim_yields_to_next_card",
                        "board_drive: stale WIP released; reclaimed card is cooling down so another card can run"
                    );
                    LaneTrace::acted(
                        lane,
                        "reclaim-stale",
                        &card,
                        format!("{card} untouched {held_h:.1}h -> todo; cooling down for {:.0}s so {blocking} other card(s) can run", reclaim_cooldown_s()),
                    )
                    .with_counts(eligible, open)
                }
                Ok(_) => LaneTrace::skip(
                    lane,
                    "reclaim-raced",
                    format!("{card} moved before the reclaim landed"),
                )
                .with_counts(eligible, open),
                Err(e) => LaneTrace::skip(
                    lane,
                    "reclaim-err",
                    format!("write failed reclaiming {card}: {e}"),
                )
                .with_counts(eligible, open),
            }
        }
        Pickup::Decompose { ids, text } => {
            // DURABLE cooldown (py:14627): the in-memory dict was wiped by the
            // very first reload after shipping and the dispatch re-fired at the
            // same lane within minutes.
            let recent = state
                .store
                .read()
                .ok()
                .and_then(|c| {
                    c.query_row(
                        "SELECT 1 FROM session_events WHERE session=?1 \
                         AND type='pickup.decompose_nudge' AND ts > ?2 LIMIT 1",
                        rusqlite::params![lane, now - DECOMPOSE_COOLDOWN_S],
                        |_| Ok(true),
                    )
                    .optional()
                    .ok()
                    .flatten()
                })
                .unwrap_or(false);
            if recent {
                return LaneTrace::skip(
                    lane,
                    "decompose-cooldown",
                    "every candidate is a capture shell; already asked within 6h",
                )
                .with_counts(eligible, open);
            }
            crate::api::session_verbs::emit_event(
                state,
                lane,
                "pickup.decompose_nudge",
                Some(json!({"shells": ids})),
                None,
                "board-drive",
            )
            .await;
            fleet.deliver(lane, &text).await;
            LaneTrace::acted(lane, "decompose-asked", ids.first().map(String::as_str).unwrap_or(""), "queue is all capture shells")
                .with_counts(eligible, open)
        }
        Pickup::None { reason, detail } => {
            // VERIFY NUDGE — options A+B. When a session has no todo/doing/
            // review work but holds `done` cards, nudge it to verify them.
            // This is NOT the removed `done` advance tier (lines 1042-1076):
            //   - fires only when the session has NOTHING ELSE to do
            //   - one batched message, not per-card nudges
            //   - 24h cooldown (the old tier nudged per card per cooldown)
            //   - the session decides what to verify, not the loop
            let verify_trace = 'verify: {
                if eligible > 0 || open > 0 {
                    break 'verify None;
                }
                let Ok(conn) = state.store.read() else {
                    break 'verify None;
                };
                let total = done_card_count(&conn, lane);
                if total == 0 {
                    break 'verify None;
                }
                let recent: bool = conn
                    .query_row(
                        "SELECT 1 FROM session_events WHERE session=?1 \
                         AND type='verify.nudge' AND ts > ?2 LIMIT 1",
                        rusqlite::params![lane, now - VERIFY_NUDGE_COOLDOWN_S],
                        |_| Ok(true),
                    )
                    .optional()
                    .ok()
                    .flatten()
                    .unwrap_or(false);
                if recent {
                    break 'verify Some(format!(
                        "has {total} done card(s) but verify-nudge sent within 24h"
                    ));
                }
                let cards = done_verify_candidates(&conn, lane);
                if cards.is_empty() {
                    break 'verify None;
                }
                drop(conn);
                let text = verify_nudge_text(&cards, total);
                crate::api::session_verbs::emit_event(
                    state,
                    lane,
                    "verify.nudge",
                    Some(json!({"done_count": total, "cards": cards.iter().map(|(id,_,_)| id.as_str()).collect::<Vec<_>>()})),
                    None,
                    "board-drive",
                )
                .await;
                fleet.deliver(lane, &text).await;
                return LaneTrace::acted(
                    lane,
                    "verify-nudge",
                    cards.first().map(|(id,_,_)| id.as_str()).unwrap_or(""),
                    format!("{total} done card(s), batched verify prompt delivered"),
                )
                .with_counts(eligible, open);
            };

            // BACKLOG TRIAGE NUDGE. Sessions accumulate backlog cards
            // (findings, investigations, decomposed work) that nobody ever
            // triages. backlog is deliberately outside auto-pickup, but cards
            // sitting there for 14+ days are either stale (should be archived)
            // or actionable (should be promoted to todo). 72h cooldown.
            let triage_trace = 'triage: {
                let Ok(conn) = state.store.read() else {
                    break 'triage None;
                };
                let now_i = now as i64;
                let stale_count = stale_backlog_count(&conn, lane, now_i);
                let total_backlog: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM issues WHERE session=?1 AND status='backlog' \
                         AND deleted IS NULL AND COALESCE(archived,0)=0 AND owner_type='agent'",
                        rusqlite::params![lane],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                let doing_count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM issues WHERE session=?1 AND status='doing' \
                         AND deleted IS NULL AND COALESCE(archived,0)=0 AND owner_type='agent'",
                        rusqlite::params![lane],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                // DRAINABLE backlog is narrower than total backlog: a card
                // correctly parked in backlog is NOT un-worked and must not be
                // drain-nudged (mixpeek-autopilot, 2026-08-13 — the nudge fired
                // 3x on a standing tripwire and two externally-triggered chores,
                // none of which have an honest path to `todo`). Exclude the same
                // two things auto-pickup excludes: the dormant/armed types
                // (tripwire, watch — they fire on an event, `is:armed`), and any
                // card with a LIVE `source_ref` trigger (parked on an external
                // condition, re-verified within SOURCE_REF_STALE_S). A trigger
                // older than that is treated as un-set — see SOURCE_REF_STALE_S
                // for why a fleet-wide sweep found this had gone to ~0 drainable
                // almost everywhere despite hundreds of un-worked backlog cards.
                // If nothing drainable remains, the lane's backlog is all
                // correctly and CURRENTLY parked, and the drain nudge stays quiet.
                let drainable_backlog: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM issues WHERE session=?1 AND status='backlog' \
                         AND deleted IS NULL AND COALESCE(archived,0)=0 AND owner_type='agent' \
                         AND type NOT IN ('tripwire','watch','epic') \
                         AND (COALESCE(source_ref,'')='' OR COALESCE(last_verified_at,0) < ?2)",
                        rusqlite::params![lane, now_i - SOURCE_REF_STALE_S],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                // TWO shapes of the same "backlog isn't moving" problem:
                //  * STALE TRIAGE: 10+ cards over 14 days old — archive cruft /
                //    promote the still-actionable ones (72h cooldown).
                //  * IDLE DRAIN (Ethan 2026-08-12, the "board doesn't drive to
                //    completion" bug): the lane is doing NOTHING, has no
                //    dispatchable todo (`eligible == 0`), and holds DRAINABLE
                //    backlog of any age. board-drive dispatches `todo`, never
                //    `backlog`, so a lane handed only backlog sits idle forever
                //    (tubescience: 51 backlog / 0 todo / 0 doing). Short cooldown
                //    so a lane that stays idle keeps getting nudged until it drains.
                // AF-334. `doing_count == 0` means "no card is in `doing`",
                // NOT "the lane is doing nothing", and the comment above
                // conflates them. A lane that works fast, keeps `todo` drained
                // and holds backlog satisfies all three terms CONTINUOUSLY
                // while being the most productive lane in the fleet.
                //
                // Measured 2026-08-30, at the 30-minute resolution the trigger
                // actually operates at rather than in aggregate: mvs-infra's
                // last six drain nudges each landed with 63-79 card events in
                // the preceding half hour. It was told it was idle every 16
                // minutes while mutating a card every ~25 seconds. Fleet-wide
                // the drain nudge was 42% of all messages.
                //
                // So the trigger gets a term for whether the lane is DEMONSTRABLY
                // WORKING. This does not weaken the case it was built for: a
                // genuinely stalled lane produces no card events and is still
                // nudged. Checked against this session's own nudges in the same
                // window - 2 of 4 had 25-27 events (suppressed by this term, and
                // they were the wrong nudges) and 2 had exactly 0 (still fire,
                // and they were correct: the lane was waiting on a build).
                //
                // A rotting backlog is NOT hidden by this. `stale_enough` below
                // is an independent path and still fires at 10+ cards over 14
                // days, which is the case where a busy lane's backlog needs
                // attention regardless of how active the lane is.
                //
                // datetime(e.at) on BOTH sides: `_amux_state_events.at` is
                // RFC3339 with an explicit +00:00 offset. Comparing it against a
                // localtime string silently applies a 4-hour skew, which while
                // writing this made every lane read as 0 events and nearly
                // produced a false "the fix does not work" conclusion.
                let lane_activity_window_min: i64 = std::env::var("AMUX_DRAIN_ACTIVITY_WINDOW_MIN")
                    .ok()
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(30);
                let recent_card_events: i64 = lane_card_activity(&conn, lane_activity_window_min)
                    .get(lane)
                    .copied()
                    .unwrap_or(0);
                let lane_working = recent_card_events > 0;
                let idle_drain =
                    should_drain_nudge(doing_count, eligible, drainable_backlog, recent_card_events);
                if doing_count == 0 && eligible == 0 && drainable_backlog > 0 && lane_working {
                    // Countable, so the suppression cannot become invisible.
                    // If this fires constantly for a lane whose backlog never
                    // moves, the term is too generous and the number says so.
                    tracing::info!(
                        target: "board_drive",
                        "[drain-nudge/suppressed AF-334] lane={lane} backlog={drainable_backlog} \
                         card_events_last_{lane_activity_window_min}min={recent_card_events} \
                         — the lane is demonstrably working; drain nudge withheld"
                    );
                }
                let stale_enough = (stale_count as usize) >= BACKLOG_TRIAGE_THRESHOLD;
                if !idle_drain && !stale_enough {
                    break 'triage None;
                }
                let (event_type, base_cooldown_s) = if idle_drain {
                    ("backlog.drain_nudge", idle_backlog_drain_cooldown_s(drainable_backlog))
                } else {
                    ("backlog.triage_nudge", BACKLOG_TRIAGE_COOLDOWN_S)
                };
                // AF-319: THE FEEDBACK TERM. Cadence was a function of backlog
                // SIZE and of nothing else, so the biggest backlogs pinned
                // themselves to the 20-minute floor and stayed there.
                //
                // What this does NOT fix, said here so the next reader does not
                // over-credit it: the lanes carrying the measured volume are all
                // ACTIVE (mvs-infra: 72 nudges against 3,682 card events over
                // 34h), so their watermark moves every tick and they are never
                // suppressed. That is the correct outcome — they are not stuck —
                // but it means the 42%-of-fleet-messages cost is a TRIGGER
                // problem, not a feedback one. Git note on 9d77177e.
                //
                // Only the drain nudge is metered. The triage nudge already has
                // a 72h cooldown and is not what the measurement found.
                let mark_now = lane_board_mark(&conn, lane);
                let nudge_state = read_nudge_state(&conn, lane);
                let (unheeded_after, moved) =
                    nudge_feedback(nudge_state.board_mark, mark_now, nudge_state.unheeded);
                let cap = nudge_unheeded_cap();
                let suppress = idle_drain && unheeded_after >= cap;
                let cooldown_s = if idle_drain {
                    unheeded_backoff(base_cooldown_s, unheeded_after)
                } else {
                    base_cooldown_s
                };
                let recent: bool = conn
                    .query_row(
                        "SELECT 1 FROM session_events WHERE session=?1 \
                         AND type=?2 AND ts > ?3 LIMIT 1",
                        rusqlite::params![lane, event_type, now - cooldown_s],
                        |_| Ok(true),
                    )
                    .optional()
                    .ok()
                    .flatten()
                    .unwrap_or(false);
                if recent {
                    break 'triage Some(format!(
                        "backlog nudge ({event_type}) already sent within cooldown"
                    ));
                }
                let cards = if idle_drain {
                    backlog_candidates(&conn, lane, now_i)
                } else {
                    stale_backlog_candidates(&conn, lane, now_i)
                };
                if cards.is_empty() {
                    break 'triage None;
                }
                drop(conn);

                // Persist the feedback state on EVERY drain decision, nudge or
                // not — a counter that only advances when we act cannot detect
                // the case it exists for.
                if idle_drain {
                    let (l, esc) = (lane.to_string(), nudge_state.has_escalated || suppress);
                    let _ = state
                        .store
                        .write_async(move |c| {
                            c.execute(
                                "INSERT INTO board_drive_nudge_state \
                                 (session, unheeded, last_nudge_at, board_mark) \
                                 VALUES (?1, ?2, ?3, ?4) \
                                 ON CONFLICT(session) DO UPDATE SET \
                                   unheeded=excluded.unheeded, \
                                   last_nudge_at=excluded.last_nudge_at, \
                                   board_mark=excluded.board_mark",
                                rusqlite::params![l, unheeded_after, now, mark_now],
                            )?;
                            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                        })
                        .await;
                    let _ = esc;
                }

                // THE CAP. Past it the lane is not told again; the QUEUE is
                // reported once, to its owner, as a card. A nudge that fired 80
                // times without moving anything is evidence about the queue, not
                // a message to the worker (AF-319).
                if suppress {
                    if !nudge_state.has_escalated {
                        file_nudge_escalation(state, lane, drainable_backlog, unheeded_after).await;
                    }
                    tracing::warn!(
                        "nudge_unheeded_cap: {lane} ignored {unheeded_after} drain nudge(s) with \
                         no card movement (cap {cap}); suppressed, backlog {drainable_backlog}"
                    );
                    break 'triage Some(format!(
                        "drain nudge SUPPRESSED — {unheeded_after} consecutive nudges moved no \
                         card (cap {cap}); escalated to the lane's owner instead"
                    ));
                }
                if idle_drain && !moved && unheeded_after > 0 {
                    tracing::info!(
                        "nudge_backoff: {lane} unheeded={unheeded_after} cooldown now {:.0}m \
                         (base {:.0}m)",
                        cooldown_s / 60.0,
                        base_cooldown_s / 60.0
                    );
                }
                let text = if idle_drain {
                    backlog_drain_text(&cards, drainable_backlog)
                } else {
                    backlog_triage_text(&cards, stale_count, total_backlog)
                };
                crate::api::session_verbs::emit_event(
                    state,
                    lane,
                    event_type,
                    Some(json!({
                        "idle_drain": idle_drain,
                        "stale_count": stale_count,
                        "total_backlog": total_backlog,
                        "cards": cards.iter().map(|(id,_,_)| id.as_str()).collect::<Vec<_>>(),
                    })),
                    None,
                    "board-drive",
                )
                .await;
                fleet.deliver(lane, &text).await;
                return LaneTrace::acted(
                    lane,
                    if idle_drain { "backlog-drain" } else { "backlog-triage" },
                    cards.first().map(|(id, _, _)| id.as_str()).unwrap_or(""),
                    if idle_drain {
                        format!("idle with {total_backlog} backlog / 0 todo — drain prompt delivered")
                    } else {
                        format!("{stale_count} stale backlog card(s) of {total_backlog} total, triage prompt delivered")
                    },
                )
                .with_counts(eligible, open);
            };

            // CONTINUE NUDGE. When CC_AUTO_CONTINUE is set, a lane with
            // no todo cards but outstanding blocked/done work gets nudged to
            // keep going instead of idling. 30min cooldown.
            let continue_trace = 'cont: {
                if !fleet.auto_continue_enabled(lane) {
                    break 'cont None;
                }
                let Ok(conn) = state.store.read() else {
                    break 'cont None;
                };
                let (bc, dc, blocked, done, human_blocked) = outstanding_work(&conn, lane);
                // `done` IS CONTEXT, NOT A TRIGGER (AMUX-2903, second pass).
                //
                // I shipped change-detection first and the very next nudge
                // disproved it: closing a card moves `done` 228 -> 229, which
                // re-armed the nudge on my own productivity. `done` grows
                // monotonically as a lane works, so ANY change signal derived
                // from it re-fires forever on an active lane — change-detection
                // made the loop slower, not finite.
                //
                // And its prescribed action is not the lane's to take: reaching
                // `verified` needs CI green and a deploy, which are outside the
                // lane entirely (CI has been red on origin/main since
                // 2026-08-10, AMUX-2902). Nudging toward a gate the recipient
                // cannot open is ethos rule 3.
                //
                // `blocked` is the honest trigger: bounded, and re-assessing a
                // blocker is work the lane can actually do. The done count still
                // rides along in the message as context.
                if bc == 0 {
                    // SAY WHAT WAS EXCLUDED (AMUX-3775, ethos rule 1). A skip
                    // line reading "0 blocked" over a lane holding seven blocked
                    // cards is the reason this was hard to see: the count and
                    // the board disagreed and only the board was visible.
                    let hb = if human_blocked.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " ({} blocked card(s) excluded: their depends_on chain roots in a \
                             card waiting on the human, so the lane cannot act on them)",
                            human_blocked.len()
                        )
                    };
                    break 'cont Some(format!(
                        "auto-continue enabled, {dc} done but 0 actionable blocked — done is context, not a trigger{hb}"
                    ));
                }
                let recent: bool = conn
                    .query_row(
                        "SELECT 1 FROM session_events WHERE session=?1 \
                         AND type='continue.nudge' AND ts > ?2 LIMIT 1",
                        rusqlite::params![lane, now - CONTINUE_NUDGE_COOLDOWN_S],
                        |_| Ok(true),
                    )
                    .optional()
                    .ok()
                    .flatten()
                    .unwrap_or(false);
                if recent {
                    break 'cont Some(format!(
                        "has {bc} blocked + {dc} done but continue-nudge sent within 30min"
                    ));
                }
                // TERMINATION (AMUX-2903). The cooldown above is a RATE limit,
                // not an exit — and `done` is monotonic, so without one this
                // fires every 30 minutes for the life of a lane. Measured
                // before the first nudge ever went out: 15 lanes eligible, 277
                // done cards among them, ~720 nudges/day.
                //
                // Worse, its prescribed exit is `verified`, whose first
                // criterion is "CI passed on the merged commit" — red on
                // origin/main with 112 commits unpushed (AMUX-2902). A lane
                // that answers honestly ("these cannot be verified, here is
                // why") would be asked again forever, which is ethos rule 3:
                // a constraint with no truthful path forward.
                //
                // So the nudge now responds to CHANGE, not to time. If the
                // outstanding set is identical to what the last nudge reported,
                // the previous one produced no movement and repeating it will
                // not either. Any real progress — a blocker cleared, a card
                // verified or archived, new work finished — moves a count and
                // re-arms it.
                if last_continue_nudge_counts(&conn, lane) == Some((bc, dc)) {
                    break 'cont Some(format!(
                        "has {bc} blocked + {dc} done, unchanged since the last \
                         continue-nudge — not repeating it (AMUX-2903)"
                    ));
                }
                drop(conn);
                let text = continue_nudge_text(bc, dc, &blocked, &done, &human_blocked);
                crate::api::session_verbs::emit_event(
                    state,
                    lane,
                    "continue.nudge",
                    Some(json!({
                        "blocked": bc,
                        "done": dc,
                    })),
                    None,
                    "board-drive",
                )
                .await;
                fleet.deliver(lane, &text).await;
                return LaneTrace::acted(
                    lane,
                    "continue-nudge",
                    "",
                    format!("{bc} blocked + {dc} done, auto-continue nudge delivered"),
                )
                .with_counts(eligible, open);
            };

            let (areason, adetail) = advance_reason;
            let mut full_detail = if adetail.is_empty() {
                detail
            } else {
                format!("{detail} | advance: {areason} ({adetail})")
            };
            if let Some(vd) = verify_trace {
                full_detail = format!("{full_detail} | verify: {vd}");
            }
            if let Some(td) = triage_trace {
                full_detail = format!("{full_detail} | backlog-triage: {td}");
            }
            if let Some(cd) = continue_trace {
                full_detail = format!("{full_detail} | continue: {cd}");
            }
            LaneTrace::skip(lane, reason, full_detail).with_counts(eligible, open)
        }
    }
}

/// Claim the card: `doing` + the durable `task.claimed` event the 24h re-claim
/// cooldown reads + a line in the card's own log.
///
/// Claim happens BEFORE delivery, and delivery is a durable queue row, so the
/// two cannot disagree for long: Python called `send_text` after the UPDATE, so
/// a failed send left a card claimed with nobody told.
/// Claim a card for a lane. Returns `true` ONLY if the card was still `todo`
/// and got moved to `doing` — the caller must not deliver the "work it now"
/// prompt otherwise.
///
/// COMPARE-AND-SWAP ON STATUS (AMUX-2983, gtm-videos). `select_pickup` reads
/// the card as `todo` but drops its read connection before this write runs, so
/// the owner can CLOSE the card in the gap. The old UPDATE was unconditional —
/// `SET status='doing' WHERE id=?` — so it would REOPEN a done/verified/
/// discarded card back to `doing` and the caller would then dispatch "work it
/// now", making the lane re-do finished work. For a non-idempotent task
/// (GV-648: re-push a video already live on Buffer/IG) that is real damage. The
/// `AND status='todo'` makes the claim atomic: 0 rows affected == the card left
/// todo == do not claim, do not dispatch, and say so in the trace so the rate
/// of these races is visible (the API being down and pickup running off a
/// stale view is the same failure this closes — the write sees the real row).
pub async fn claim_card(state: &AppState, session: &str, card: &str) -> bool {
    claim_card_from(state, session, card, "todo").await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaimCardOutcome {
    Claimed,
    DependencyBlocked(Vec<String>),
    NotApplied,
}

/// `claim_card` with the expected PRIOR status as a parameter. Auto-pickup and
/// every drive-loop path stay on `claim_card` ("todo"): a card parked
/// todo->backlog mid-race must DEFEAT a racing pickup, so widening that CAS to
/// backlog would let pickup steal deliberately parked work. Only the MANUAL
/// claim endpoint passes "backlog" — an explicit claim of a named parked card
/// is a deliberate act (AMUX-3450: the CLI help promised todo/backlog claim,
/// the server refused backlog, and a peer's handover relied on the promise).
pub async fn claim_card_from(
    state: &AppState,
    session: &str,
    card: &str,
    from: &'static str,
) -> bool {
    matches!(
        claim_card_from_outcome(state, session, card, from).await,
        ClaimCardOutcome::Claimed
    )
}

/// The detailed claim primitive used by the HTTP door. Dependency checking
/// lives INSIDE the same writer transaction as the status CAS: a preflight in
/// the handler would leave a window where an open prerequisite could appear
/// after the read and before the forced claim.
pub(crate) async fn claim_card_from_outcome(
    state: &AppState,
    session: &str,
    card: &str,
    from: &'static str,
) -> ClaimCardOutcome {
    let card_s = card.to_string();
    // Assign the claimer as part of the swap. For auto-pickup this is a no-op
    // (the card is already `i.session=lane`), but it makes a MANUAL claim
    // (AMUX-3131: POST /api/board/{id}/claim) take ownership of an unassigned
    // card in the same atomic step, rather than leaving it `doing` with a stale
    // owner.
    let session_s = session.to_string();
    // THE GUARD'S VERDICT HAS TO ESCAPE THE CLOSURE (AMUX-3776). `reassigned`
    // is computed inside the write, where it can be read in the same
    // transaction as the swap — which is correct and is why it lives there —
    // and it was then used only for a tracing::warn and a card-log line. So a
    // genuine cross-owner claim left NO row in `session_events`, while the
    // ordinary path emits `task.claimed`.
    //
    // Two costs. A sweep of the ledger cannot find the violation at all. And
    // the (reached, taken) instrument AMUX-3768 proposes can only see branches
    // that RECORD, so this one would report dead-by-absence forever while being
    // perfectly reachable — the same false negative random hit on
    // raw-tmux-fallback and corrected.
    let cross_owner = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let cross_owner_w = cross_owner.clone();
    let dependency_blockers = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let dependency_blockers_w = dependency_blockers.clone();
    let reply = state
        .store
        .write_async(move |conn| {
            // Read the PRIOR owner in the same transaction as the swap. The card
            // log must be able to answer "which lane claimed this, and did it
            // already own it", because that is the exact question AF-79 could
            // NOT answer (AMUX-3776).
            let Some(row) = bs::get_issue(conn, &card_s)? else {
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            };
            if row.status != from {
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            }
            let blockers = deps_blocking(conn, &row);
            if !blockers.is_empty() {
                if let Ok(mut g) = dependency_blockers_w.lock() {
                    *g = blockers;
                }
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            }
            let prior_owner = row.session.unwrap_or_default();

            let reassigned = !prior_owner.is_empty() && prior_owner != session_s;
            let entry = if reassigned {
                format!("Auto-picked up from queue by {session_s} (reassigned from {prior_owner})")
            } else if from == "backlog" {
                format!("Claimed from backlog by {session_s}")
            } else {
                format!("Auto-picked up from queue by {session_s}")
            };

            let opts = crate::db::advance::AdvanceOpts {
                expected_from: Some(from.to_string()),
                assign_to: Some(session_s.clone()),
                log_line: Some(entry),
                force: true,
                skip_continuation: true,
                ..Default::default()
            };
            match crate::db::advance::advance(conn, &card_s, "doing", &session_s, &opts)? {
                Ok(outcome) => {
                    // Attribution is part of the claim transaction. A restart
                    // or writer failure can now leave neither a `doing` card
                    // nor a causal marker, never only one of them.
                    if reassigned {
                        conn.execute(
                            "INSERT INTO session_events (ts,session,type,data,source) VALUES (?,?,?,?,?)",
                            rusqlite::params![now_f64(), &session_s, "claim.cross_owner",
                                json!({"issue": &card_s, "claimer": &session_s,
                                    "prior_owner": &prior_owner, "from": from}).to_string(), "board-drive"],
                        )?;
                        if let Ok(mut g) = cross_owner_w.lock() { *g = Some(prior_owner.clone()); }
                    }
                    conn.execute(
                        "INSERT INTO session_events (ts,session,type,data,source) VALUES (?,?,?,?,?)",
                        rusqlite::params![now_f64(), &session_s, "task.claimed",
                            json!({"issue": &card_s, "status": "doing", "from": from}).to_string(), "board-drive"],
                    )?;
                    Ok(crate::db::WriteOutcome { applied: true, events: outcome.events })
                }
                Err(_refusal) => Ok(crate::db::WriteOutcome { applied: false, events: vec![] }),
            }
        })
        .await;
    let blockers = dependency_blockers.lock().ok().map(|g| g.clone()).unwrap_or_default();
    if !blockers.is_empty() {
        let n_considered = blockers.len();
        tracing::warn!(
            target: "amux::board_drive",
            %session,
            %card,
            %from,
            blocking = %blockers.join(","),
            measured = true,
            n_considered,
            verdict = "dependency_blocked",
            "claim refused — prerequisite tasks are still open"
        );
        crate::api::session_verbs::emit_event(
            state,
            session,
            "claim.dependency_blocked",
            Some(json!({
                "issue": card,
                "claimer": session,
                "from": from,
                "blocking": blockers.clone(),
                "measured": true,
                "n_considered": n_considered,
                "verdict": "dependency_blocked",
            })),
            None,
            "board-drive",
        )
        .await;
        return ClaimCardOutcome::DependencyBlocked(blockers);
    }
    let claimed = match reply {
        Ok(r) => r.applied,
        Err(error) => {
            tracing::warn!(target: "amux::board_drive", %session, %card, %from, %error,
                measured = false, n_considered = 0, verdict = "atomic_claim_write_failed",
                "board_drive: atomic claim failed; neither doing state nor causal task claim committed");
            false
        }
    };
    if !claimed {
        tracing::info!(
            target: "amux::board_drive", %session, %card,
            %from,
            "claim NOT applied — card left the expected status between select and claim \
             (raced to a terminal/doing state, or store unreadable); prompt NOT dispatched"
        );
        return ClaimCardOutcome::NotApplied;
    }
    if let Some(prior) = cross_owner.lock().ok().and_then(|g| g.clone()) {
        tracing::warn!(target: "amux::board_drive", claimer = %session, prior = %prior, card = %card,
            measured = true, n_considered = 1, verdict = "cross_owner_claimed",
            "claim reassigned a card across owners");
    }
    ClaimCardOutcome::Claimed
}

/// Undo a claim after its work prompt was refused. The status CAS and release
/// receipt share one writer transaction; a human reassignment wins the race
/// rather than being overwritten by this compensation.
async fn release_claim_after_dispatch_failure(
    state: &AppState, session: &str, card: &str, from: &'static str, error: &str,
) -> bool {
    let (session, card, error) = (session.to_string(), card.to_string(), error.to_string());
    let (session_w, card_w, error_w) = (session.clone(), card.clone(), error.clone());
    let reply = state.store.write_async(move |conn| {
        let Some(row) = bs::get_issue(conn, &card_w)? else {
            return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
        };
        if row.status != "doing" || row.session.as_deref() != Some(session_w.as_str()) {
            return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
        }
        let opts = crate::db::advance::AdvanceOpts {
            expected_from: Some("doing".into()), force: true, skip_continuation: true,
            skip_todo_wip: true,
            log_line: Some(format!("auto-claim released: work prompt could not be queued ({error_w})")),
            ..Default::default()
        };
        match crate::db::advance::advance(conn, &card_w, from, "board_drive", &opts)? {
            Ok(outcome) => {
                conn.execute(
                    "INSERT INTO session_events (ts,session,type,data,source) VALUES (?,?,?,?,?)",
                    rusqlite::params![now_f64(), &session_w, "task.released",
                        json!({"issue": &card_w, "from": "doing", "to": from,
                            "reason": "delivery_failed", "error": &error_w}).to_string(), "board-drive"],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: outcome.events })
            }
            Err(_) => Ok(crate::db::WriteOutcome { applied: false, events: vec![] }),
        }
    }).await;
    match reply {
        Ok(r) if r.applied => {
            tracing::warn!(target: "amux::board_drive", %session, %card, %from, %error,
                measured = true, n_considered = 1, verdict = "dispatch_failed_claim_released",
                "board_drive: delivery refused; reverted the un-delivered claim");
            true
        }
        Ok(_) => {
            tracing::warn!(target: "amux::board_drive", %session, %card, %from, %error,
                measured = true, n_considered = 1, verdict = "dispatch_compensation_raced",
                "board_drive: delivery failed but compensation yielded to a concurrent card change");
            false
        }
        Err(write_error) => {
            tracing::error!(target: "amux::board_drive", %session, %card, %from, %error, %write_error,
                measured = false, n_considered = 1, verdict = "dispatch_compensation_write_failed",
                "board_drive: delivery failed and claim compensation could not be persisted");
            false
        }
    }
}

/// Background driver.
pub fn spawn(state: AppState) -> super::PeriodicTask {
    // ONE SPELLING OF THE KNOB (AF-437). `spawn_periodic` derives this job's
    // fleet-isolation gate from its NAME via `per_job_disable_var`, and this
    // read used to hand-type the result. Same variable, two independent
    // spellings, and a change to the convention would have moved the gate
    // without moving the interval read.
    let secs = std::env::var(super::per_job_disable_var(JOB))
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(BOARD_DRIVE_TICK_SECS);
    super::spawn_periodic(JOB, secs, move || {
        let state = state.clone();
        async move {
            // Terminal callbacks are a durable board outbox. Drain it on every
            // board tick so a restart or a transition produced outside the
            // HTTP PATCH handler cannot strand a completed peer request.
            let callbacks = crate::api::board::dispatch_pending_callbacks(&state, None).await;
            if callbacks.attempted > 0 {
                tracing::info!(
                    attempted = callbacks.attempted,
                    queued = callbacks.queued,
                    refused = callbacks.refused,
                    "[board-drive] terminal task callbacks"
                );
            }
            let fleet = LiveFleet {
                state: state.clone(),
                signals: crate::api::session_verbs::boundary_signals(&state, None).await,
            };
            let r = drive_tick(&state, &fleet).await;
            if r.assigned > 0
                || r.nudged > 0
                || r.promoted > 0
                || r.promoted_due > 0
                || r.normalized_blocked_doing > 0
            {
                tracing::info!(
                    assigned = r.assigned,
                    nudged = r.nudged,
                    promoted = r.promoted,
                    promoted_due = r.promoted_due,
                    normalized_blocked_doing = r.normalized_blocked_doing,
                    revisit_due_total = r.revisit_due_total,
                    held_on_trigger = r.held_on_trigger,
                    lanes = r.lanes.len(),
                    "[board-drive] tick"
                );
            }
        }
    })
}

// ---------------------------------------------------------------------------
// /api/debug/board-drive — the surface whose absence WAS the incident
// ---------------------------------------------------------------------------

/// Answers, without reading source: is the drive loop running, which lanes did
/// it examine, what did it hand them, and for every lane it passed over, WHY.
///
/// `loop_running: false` is a real answer, not a missing field: before this
/// existed, a dead loop and a fleet with nothing to do produced byte-identical
/// evidence, which is how the outage went unnoticed for hours.
pub async fn debug_board_drive(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let report = last_report();
    let now = now_f64();
    // Fleet-wide backlog: how many lanes are sitting on eligible cards right
    // now. This is the number that says how much work the loop is responsible
    // for, independent of what the last tick happened to do.
    let mut waiting: Vec<Value> = Vec::new();
    // AF-320: `backlog: []` means either no lane is waiting or the store was
    // never opened, and the `if let Ok` below makes the second a silent path.
    // `lanes_considered` is None exactly when nothing was examined.
    let mut lanes_considered: Option<usize> = None;
    if let Ok(conn) = state.store.read() {
        let lanes = crate::api::session_verbs::all_lane_names();
        lanes_considered = Some(lanes.len());
        for lane in lanes {
            let n = eligible_todo_count(&conn, &lane, crate::config::now_f64());
            if n > 0 {
                waiting.push(json!({"session": lane, "eligible_todos": n}));
            }
        }
    }
    let seen: HashSet<String> = waiting
        .iter()
        .filter_map(|v| v["session"].as_str().map(str::to_string))
        .collect();
    let capture_shells = state.store.read().ok().map(|c| capture_shell_cost(&c));
    let queue_shape = state.store.read().ok().map(|c| fleet_queue_shape(&c));
    // AF-319: the nudge loop's own cost and its feedback state, on the endpoint
    // that already answers "what is this loop doing". A throttle nobody can
    // audit is the shape ethos rule 6 warns about, and the measurement that
    // motivated this had to be reconstructed from cmd_history by hand.
    let nudge_feedback = state.store.read().ok().map(|c| nudge_feedback_report(&c));
    let body = json!({
        "note": "per-lane trace of the board -> worker drive loop; `reason` says why a lane \
                 was passed over. A skip that leaves no trace is indistinguishable from a loop \
                 that is not running.",
        "loop_running": report.is_some(),
        "tick_secs": std::env::var(super::per_job_disable_var(JOB)).ok()
            .and_then(|v| v.parse::<u64>().ok()).unwrap_or(BOARD_DRIVE_TICK_SECS),
        "wip_cap": wip_cap(),
        "advance_card_budget": advance_card_budget(),
        "advance_cooldown_s": ADVANCE_COOLDOWN_S,
        "last_tick_age_s": report.as_ref().map(|r| now - r.finished_at),
        "last": report,
        "lanes_with_eligible_cards": waiting.len(),
        "backlog": waiting,
        "distinct_lanes_waiting": seen.len(),
        "capture_shells": capture_shells,
        "queue_shape": queue_shape,
        "nudge_feedback": nudge_feedback,
    });
    let body = match lanes_considered {
        Some(n) => crate::api::measured::measured(body, n),
        None => crate::api::measured::unmeasured(
            body,
            "the store could not be opened, so no lane's eligible-card count was read — \
             an empty backlog here is the absence of a measurement",
        ),
    };
    (axum::http::StatusCode::OK, axum::Json(body)).into_response()
}

/// The SHAPE of the fleet's open work, beside the loop that consumes it
/// (AMUX-3758).
///
/// Ethan, 2026-08-26: "we need to rethink how we do amux because this isn't
/// working." Chasing that per-lane finds a different local cause every time —
/// wip-cap here, needsyou-renag-deduped there, dep-other-lane somewhere else —
/// and each one reads like a small bug. The fleet number is the argument the
/// per-lane traces cannot make: measured that day, of 1039 open cards, 48.2%
/// were `backlog` (parked awaiting a trigger), 37.2% were `needsyou` (blocked
/// on a human, median 7.2 days old, max 19.7), and **4.9% were `todo`** — the
/// only status `DISPATCHABLE_WHERE` selects. 51 workable cards across 52 lanes.
///
/// So a lane going idle on a full board is mostly not a defect in this loop.
/// The board is 95% invisible to it BY DESIGN, and the queue that actually
/// feeds it is nearly empty. Nothing in amux said so, because every view is
/// per-lane and the answer only exists in aggregate.
///
/// Deliberately a MEASUREMENT and not an action: promoting backlog or resolving
/// needsyou on the fleet's behalf would be an agent deciding a human's work is
/// unblocked (ethos rule 8). This reports; a human decides.
///
/// `dispatchable_pct` is the headline. It reads ZERO in two very different
/// worlds — a fleet with nothing to do, and a fleet whose whole board is parked
/// — so `open_total` rides along in the same payload to tell them apart.
fn fleet_queue_shape(conn: &Connection) -> Value {
    const OPEN: &str = "('todo','doing','backlog','needsyou','review','armed')";
    let mut by_status = serde_json::Map::new();
    let mut total = 0i64;
    if let Ok(mut st) = conn.prepare(&format!(
        "SELECT status, COUNT(*) FROM issues WHERE status IN {OPEN} \
         AND deleted IS NULL AND COALESCE(archived,0)=0 GROUP BY status"
    )) {
        if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) {
            for (s, n) in rows.flatten() {
                total += n;
                by_status.insert(s, json!(n));
            }
        }
    }
    // Reuse the exact per-lane predicate; a raw Todo count is not a ready count.
    let now = now_f64();
    let lanes: Vec<String> = conn
        .prepare("SELECT DISTINCT session FROM issues WHERE session IS NOT NULL AND session <> '' AND status='todo' AND deleted IS NULL AND COALESCE(archived,0)=0")
        .and_then(|mut st| st.query_map([], |r| r.get::<_, String>(0)).map(|rows| rows.flatten().collect()))
        .unwrap_or_default();
    let dispatchable: i64 = lanes.iter().map(|lane| eligible_todo_count(conn, lane, now)).sum();
    // Median age of the human-blocked pile, in days. The COUNT alone reads the
    // same for 387 questions asked this morning and 387 asked a fortnight ago,
    // and only the second is a bottleneck.
    let needsyou_median_age_d: Option<f64> = conn
        .query_row(
            "SELECT (strftime('%s','now') - COALESCE(updated, created)) / 86400.0 \
             FROM issues WHERE status='needsyou' AND deleted IS NULL \
               AND COALESCE(archived,0)=0 \
             ORDER BY COALESCE(updated, created) DESC \
             LIMIT 1 OFFSET (SELECT COUNT(*)/2 FROM issues WHERE status='needsyou' \
                             AND deleted IS NULL AND COALESCE(archived,0)=0)",
            [],
            |r| r.get(0),
        )
        .ok();
    json!({
        "open_total": total,
        "by_status": by_status,
        "dispatchable_todo": dispatchable,
        "dispatchable_pct": if total > 0 { (dispatchable as f64 * 1000.0 / total as f64).round() / 10.0 } else { 0.0 },
        "needsyou_median_age_d": needsyou_median_age_d,
        "note": "`dispatchable_todo` is the ONLY slice DISPATCHABLE_WHERE selects. A lane idling \
                 on a full board is usually this: its queue is real and its workable queue is \
                 empty. `backlog` needs a trigger, `needsyou` needs a human — neither is a defect \
                 in the drive loop, and neither is amux's to clear on its own.",
    })
}

/// What the capture-shell nudge is costing, in one call.
///
/// AMUX-3707. Ethan flagged these as wasteful ("when its a question and doesnt
/// produce work we get these"), and answering *how* wasteful took hand-written
/// SQL joining `session_events` to `issues` — which is the tell that the number
/// nobody can reach is the number nobody watches. The nudge is emitted once per
/// card ever (idem `decompose:<id>`), so the row count IS the count of lanes
/// woken to dispose of a card amux itself minted.
///
/// The ratio is the signal, not the total: a `discarded` outcome means the woken
/// turn produced a one-line retirement, while `promoted_to_epic` and `reshaped`
/// mean the nudge bought real judgment. If `discarded_pct` climbs, amux is
/// spending more turns on foregone conclusions and the capture predicate wants
/// tightening; if it falls, the nudge is earning its keep. Neither direction is
/// visible from the nudge count alone, which is all anything reported before.
fn capture_shell_cost(conn: &rusqlite::Connection) -> Value {
    let q = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap_or(-1) };
    let nudges = q("SELECT COUNT(*) FROM session_events WHERE idem LIKE 'decompose:%'");
    let nudges_7d = q(
        "SELECT COUNT(*) FROM session_events WHERE idem LIKE 'decompose:%' \
         AND ts > strftime('%s','now') - 7*86400",
    );
    let by = |pred: &str| -> i64 {
        q(&format!(
            "SELECT COUNT(*) FROM session_events e JOIN issues i \
             ON i.id = replace(e.idem,'decompose:','') \
             WHERE e.idem LIKE 'decompose:%' AND {pred}"
        ))
    };
    let discarded = by("i.status='discarded'");
    let epic = by("i.type='epic'");
    let resolved = by("i.status IN ('discarded','done','verified')");
    // `Option`, so a missing DENOMINATOR is null rather than 0.0. That
    // separates "nothing to divide by" from "a small share" at the type level,
    // where a string like "<1%" separates them only for a caller that parses it
    // (mixpeek-frustrations' point, and it is the shape to copy if the nudge's
    // coverage field ever becomes machine-readable).
    //
    // AND NEVER 0.0 FOR A NONZERO COUNT. One-decimal rounding is not integer
    // truncation and does not collapse at 5-of-501 (that is 1.0), which is what
    // made this safer than the nudge's coverage field — but it is not immune,
    // only later. The floor is `nudges > 2000*n`, STRICTLY: 1-of-2000 is exactly
    // 0.05, ten times that is 0.5, and Rust's f64::round goes half-AWAY-from-
    // zero, so it renders 0.1. The first ratio that renders zero is 1-of-2001.
    // `nudges` is 545 today growing ~42/day, which puts that crossing about 35
    // days out. A DATED bug, not an absent one.
    //
    // Below the floor the unrounded value is emitted, because "0.0% discarded"
    // beside `outcome_discarded: 1` is the ZERO THAT MEANS NONE again: not an
    // imprecise number, a different claim. A genuine zero still renders 0.0 —
    // the guard is about nonzero counts, not about hiding zeros, and a cell
    // pins that.
    //
    // The boundary above is pinned in a TEST rather than trusted here, because
    // I first worked it out in Python and got it wrong by one: Python's round()
    // is banker's rounding and disagrees with Rust at exactly the half-step.
    let pct = |n: i64| -> Option<f64> {
        (nudges > 0 && n >= 0).then(|| {
            let exact = n as f64 * 100.0 / nudges as f64;
            let rounded = (exact * 10.0).round() / 10.0;
            if rounded == 0.0 && n > 0 {
                exact
            } else {
                rounded
            }
        })
    };
    json!({
        "note": "one nudge per card ever (idem decompose:<id>), so `nudges` is the count of \
                 lanes woken to dispose of a card amux minted. Watch discarded_pct: it is the \
                 share of those turns that reached a foregone conclusion.",
        "nudges": nudges,
        "nudges_7d": nudges_7d,
        "outcome_discarded": discarded,
        "outcome_promoted_to_epic": epic,
        "outcome_resolved": resolved,
        "discarded_pct": pct(discarded),
        "promoted_to_epic_pct": pct(epic),
        "still_open": (nudges >= 0 && resolved >= 0).then(|| nudges - resolved),
    })
}

/// GET /api/board/nudges — current CC_STANDING_ORDERS state at every scope.
///
/// Returns `{"global": bool, "groups": {"name": bool, ...}, "workers": {"name": bool, ...}}`
/// where `true` = nudges ON (the default), `false` = disabled at that level.
///
/// Reads the env files directly — same source `standing_orders_on_in` reads.
pub async fn get_nudges() -> axum::response::Response {
    use axum::response::IntoResponse;
    let home = crate::api::session_verbs::home();

    let global_val = crate::config::parse_env_file(&home.join("amux.env"))
        .into_iter()
        .find(|(k, _)| k == "CC_STANDING_ORDERS")
        .map(|(_, v)| crate::api::session_verbs::auto_continue_on(Some(v.as_str())));

    // Groups: ~/.amux/env/<name>.env
    let mut groups: serde_json::Map<String, Value> = serde_json::Map::new();
    if let Ok(rd) = std::fs::read_dir(home.join("env")) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("env") {
                let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
                if name.is_empty() { continue; }
                let val = crate::config::parse_env_file(&p)
                    .into_iter()
                    .find(|(k, _)| k == "CC_STANDING_ORDERS")
                    .map(|(_, v)| crate::api::session_verbs::auto_continue_on(Some(v.as_str())));
                if let Some(b) = val {
                    groups.insert(name, Value::Bool(b));
                }
            }
        }
    }

    // Workers: ~/.amux/workers/<name>/env
    let mut workers: serde_json::Map<String, Value> = serde_json::Map::new();
    if let Ok(rd) = std::fs::read_dir(home.join("workers")) {
        for entry in rd.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_string_lossy().to_string();
                let env_path = entry.path().join("env");
                if env_path.exists() {
                    let val = crate::config::parse_env_file(&env_path)
                        .into_iter()
                        .find(|(k, _)| k == "CC_STANDING_ORDERS")
                        .map(|(_, v)| crate::api::session_verbs::auto_continue_on(Some(v.as_str())));
                    if let Some(b) = val {
                        workers.insert(name, Value::Bool(b));
                    }
                }
            }
        }
    }

    (axum::http::StatusCode::OK, axum::Json(json!({
        "key": "CC_STANDING_ORDERS",
        "note": "true = nudges on (default), false = disabled. Scopes: global > group > worker.",
        "global": global_val,
        "groups": groups,
        "workers": workers,
    }))).into_response()
}

#[derive(serde::Deserialize)]
pub struct NudgesPatch {
    pub level: String,          // "global" | "group" | "worker"
    pub name: Option<String>,   // group or worker name (required unless level=global)
    pub enabled: bool,
}

/// PATCH /api/board/nudges — enable or disable board nudges at a scope.
///
/// `{"level": "global", "enabled": false}` — silences all nudges fleet-wide.
/// `{"level": "group", "name": "my-group", "enabled": false}` — group-level.
/// `{"level": "worker", "name": "ts-gke", "enabled": false}` — per-worker.
///
/// Writes/removes CC_STANDING_ORDERS in the appropriate env file, which is the
/// same file `standing_orders_on` reads. No restart required.
pub async fn patch_nudges(
    axum::Json(body): axum::Json<NudgesPatch>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let home = crate::api::session_verbs::home();
    let level = body.level.as_str();
    let name = body.name.as_deref().unwrap_or("");

    if matches!(level, "group" | "worker") && name.is_empty() {
        return (axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "name is required for group/worker level"}))).into_response();
    }

    let path = match level {
        "global" => home.join("amux.env"),
        "group"  => home.join("env").join(format!("{name}.env")),
        "worker" => home.join("workers").join(name).join("env"),
        _ => return (axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "level must be global, group, or worker"}))).into_response(),
    };

    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": e.to_string()}))).into_response();
        }
    }

    // Read current env, set or remove CC_STANDING_ORDERS, write back.
    let mut env: std::collections::BTreeMap<String, String> = if path.exists() {
        crate::config::parse_env_file(&path).into_iter().collect()
    } else {
        Default::default()
    };

    if body.enabled {
        env.remove("CC_STANDING_ORDERS");  // remove = default ON; explicit True is redundant
    } else {
        env.insert("CC_STANDING_ORDERS".into(), "False".into());
    }

    let text: String = env.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    if let Err(e) = std::fs::write(&path, text) {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": e.to_string()}))).into_response();
    }

    (axum::http::StatusCode::OK, axum::Json(json!({
        "ok": true,
        "level": level,
        "name": name,
        "enabled": body.enabled,
        "note": if body.enabled {
            "CC_STANDING_ORDERS removed from env (nudges on by default)"
        } else {
            "CC_STANDING_ORDERS=False written to env (nudges disabled)"
        },
    }))).into_response()
}

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/api/debug/board-drive", axum::routing::get(debug_board_drive))
        .route("/api/board/nudges", axum::routing::get(get_nudges).patch(patch_nudges))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod reviewer_renag_tests {
    use super::{reviewer_acted_since_review, reviewer_left_review};

    /// The AF-203 specimen, verbatim in shape: amux is named reviewer, reviews
    /// and rejects, and the log then carries rows that are NOT actor actions.
    const REAL: &str = "\
`14:37` commit ab6f5f30 — fix(messages): clamp /api/history (AF-213)
`14:20` amux-frustrations: doing -> review
`14:33` amux: desc +2140 chars
`14:40` authz: card=silent worker:amux-frustrations=silent group=silent
`14:45` commit a8786e4a — docs(frustrations): a rejected review has no status";

    #[test]
    fn a_reviewer_who_has_written_since_review_is_not_nudged_again() {
        assert!(
            reviewer_acted_since_review(REAL, "amux"),
            "amux wrote at 14:33, after the 14:20 move into review — they have reviewed it"
        );
    }

    /// CONTROL, and the one that matters: a guard that suppressed every nudge
    /// would pass the cell above perfectly.
    #[test]
    fn a_reviewer_who_has_not_touched_the_card_is_still_nudged() {
        let log = "\
`14:20` amux-frustrations: doing -> review
`14:45` commit a8786e4a — docs(frustrations): a rejected review has no status";
        assert!(
            !reviewer_acted_since_review(log, "amux"),
            "nobody has reviewed it — this is exactly the card the nudge exists for"
        );
    }

    /// AMUX-3820: the reviewer moving the card OUT of review is a promise, and
    /// the promise must be detectable.
    ///
    /// The specimen is AF-203. amux reviewed, rejected, and moved it to `doing`
    /// with an explicit note that `doing` was the true state — correct, and it
    /// took the card off every list that would have re-nudged amux. It sat four
    /// days and was acked only because its author asked.
    #[test]
    fn a_reviewer_who_moved_the_card_out_of_review_still_owes_it_a_look() {
        let log = "\
`14:20` amux-frustrations: doing -> review
`14:33` amux: desc +2140 chars
`14:52` amux: review -> doing";
        assert!(
            reviewer_left_review(log, "amux"),
            "amux moved it out themselves — that is a promise to come back"
        );
    }

    /// THE CONTROL THAT MATTERS. An AUTHOR moving their own card out of review
    /// is ordinary work. Firing on that turns this into a nag on every
    /// in-progress card that ever named a reviewer, which is strictly worse
    /// than the silence it replaces.
    #[test]
    fn an_author_moving_their_own_card_out_of_review_is_not_a_reviewer_promise() {
        let log = "\
`14:20` amux-frustrations: doing -> review
`14:52` amux-frustrations: review -> doing";
        assert!(
            !reviewer_left_review(log, "amux"),
            "the author withdrew it; the reviewer promised nothing"
        );
        // And the prefix trap, which is why the needle carries a trailing colon:
        // `amux` is a prefix of `amux-frustrations`, so a bare name match would
        // read the author's own move as the reviewer's promise.
        assert!(
            !reviewer_left_review(log, "amux"),
            "`amux` must not match `amux-frustrations:`"
        );
    }

    /// BOTH DIRECTIONS, against REAL card logs (amux-frustrations' suggestion).
    ///
    /// The two edges are complements and a synthetic log proves that only for
    /// the shape I invented. These are the two specimens that actually fired
    /// today, one per direction:
    ///
    /// - AF-203 (above): reviewer LEFT review. The new edge fires, the old one
    ///   does not, because the card is no longer in `review`.
    /// - AMUX-3057: card STAYS in review naming `ethan`, who is not a worker.
    ///   The old edge fires (reviewer-unreachable); the new one must NOT, or a
    ///   single card gets nudged by both.
    ///
    /// AMUX-3057's log is also the reason "is the newest line the reviewer's"
    /// is the wrong test — its last eight lines are all `commit` rows.
    #[test]
    fn the_two_reviewer_edges_are_complements_on_real_logs() {
        // AMUX-3057 as stored: in review, no actor line since, all commit rows.
        const STUCK_IN_REVIEW: &str = "\
`14:20` amux: doing -> review
`09:46` commit f6a80ece — fix(build): say when the builder log cannot see the range
`10:07` commit b64236e6 — fix(guard): the OUTDATED HOOK remedy names a path its audience can run";
        assert!(
            !reviewer_left_review(STUCK_IN_REVIEW, "ethan"),
            "nobody left review — this card is the OTHER edge's, and firing both \
             would nudge one card twice"
        );
        assert!(
            !reviewer_acted_since_review(STUCK_IN_REVIEW, "ethan"),
            "and the reviewer has not acted either, which is why it parked"
        );
        // THE CONTROL: the AF-203 shape must classify the opposite way on both
        // predicates, or this test would pass against a pair that always
        // answers false.
        const LEFT_REVIEW: &str = "\
`14:20` amux-frustrations: doing -> review
`14:33` amux: desc +2140 chars
`14:52` amux: review -> doing";
        assert!(reviewer_left_review(LEFT_REVIEW, "amux"));
        assert!(reviewer_acted_since_review(LEFT_REVIEW, "amux"));
    }

    /// A card still sitting IN review is the OTHER edge's business. Firing here
    /// too would double-nudge the reviewer for one card.
    #[test]
    fn a_card_still_in_review_is_not_a_promise_to_return() {
        let log = "\
`14:20` amux-frustrations: doing -> review
`14:33` amux: desc +2140 chars";
        assert!(!reviewer_left_review(log, "amux"), "nobody has left review yet");
        // ...and the existing edge DOES see it, so the two are complementary
        // rather than leaving a gap between them.
        assert!(reviewer_acted_since_review(log, "amux"));
    }

    /// A SECOND ROUND clears the old promise: the reviewer's exit from the
    /// PREVIOUS round is not an outstanding debt once the card re-enters review.
    #[test]
    fn a_re_review_supersedes_the_previous_rounds_promise() {
        let log = "\
`14:20` amux-frustrations: doing -> review
`14:52` amux: review -> doing
`15:10` amux-frustrations: doing -> review";
        assert!(
            !reviewer_left_review(log, "amux"),
            "the card is back in review; the old exit belongs to a closed round"
        );
    }

    /// The non-actor rows must not be read as the reviewer acting. `authz:` and
    /// `commit <sha> —` lines are the newest lines on a real card most of the
    /// time, which is why "is the last line the reviewer's" is the wrong test.
    #[test]
    fn authz_and_commit_rows_are_not_reviewer_activity() {
        let log = "\
`14:20` amux-frustrations: doing -> review
`14:40` authz: card=silent worker:amux=silent group=silent
`14:45` commit a8786e4a — amux fixed something";
        assert!(
            !reviewer_acted_since_review(log, "amux"),
            "an authz row naming the reviewer, and a commit subject containing their name, \
             are not the reviewer writing to the card"
        );
    }

    /// A SECOND ROUND re-arms the nudge. The reviewer's note from the previous
    /// round is older than the latest move into review, so it must not suppress
    /// the nudge for work that has since been redone and resubmitted.
    #[test]
    fn a_resubmitted_card_nudges_the_reviewer_again() {
        let log = "\
`10:00` author: doing -> review
`10:05` amux: desc +900 chars
`11:00` author: review -> doing
`12:00` author: doing -> review";
        assert!(
            !reviewer_acted_since_review(log, "amux"),
            "amux reviewed the FIRST submission; the card was reworked and resubmitted at \
             12:00 and needs a fresh look"
        );
    }

    #[test]
    fn an_unnamed_reviewer_never_suppresses() {
        assert!(!reviewer_acted_since_review(REAL, ""));
        assert!(!reviewer_acted_since_review(REAL, "   "));
    }
}

#[cfg(test)]
mod tests {
    // ---- AF-319: the nudge loop's missing feedback term --------------------

    /// The LOOP CLOSES: a lane that moves a card is forgiven, one that does not
    /// accumulates, and the very first tick is never counted against it.
    ///
    /// That last case is the one worth pinning. On a lane's first drain nudge
    /// there is no previous watermark to compare against, and treating an
    /// absent measurement as "no movement" would escalate a lane on the
    /// strength of a tick that could not have observed anything (ethos rule 4,
    /// applied to this function's own first run).
    /// A park must name the dependency's STATE, not just call it unfinished.
    ///
    /// AF-610, reported by `primis`. PRIMI-220 (twice) and PRIMI-221 were parked
    /// within the same second as a satisfied 2/2 gate, naming PRIMI-219 — which
    /// was `done`. The behaviour is correct: PRIMI-219 has no `type`,
    /// `core_item_type("")` falls through to ItemType::Code, and
    /// `verified_is_meaningful(Code)` is true, so it completes at `verified`.
    /// The MESSAGE said "unfinished depends_on=PRIMI-219", which reads as "not
    /// done" about a card that is done.
    ///
    /// The cost was not the park. The workaround was to CLEAR depends_on on both
    /// cards, destroying the dependency graph the parker exists to enforce.
    #[test]
    fn a_park_names_why_a_done_dependency_still_blocks() {
        let mut conn = Connection::open_in_memory().expect("memdb");
        crate::db::migrate::apply_all(&mut conn).expect("schema");
        let add = |id: &str, status: &str, item_type: &str| {
            conn.execute(
                "INSERT INTO issues (id, title, status, type, created, updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                rusqlite::params![id, format!("t {id}"), status, item_type, 1_760_000_000.0_f64],
            )
            .expect("insert");
        };
        // The reported shape: done, and NO type at all.
        add("DEP-UNTYPED", "done", "");
        // A type whose completion boundary really is `done`.
        add("DEP-DOC", "done", "doc");
        // Terminal but never proof of resolution.
        add("DEP-DISCARDED", "discarded", "doc");

        let ids: Vec<String> =
            ["DEP-UNTYPED", "DEP-DOC", "DEP-DISCARDED"].iter().map(|s| s.to_string()).collect();
        let reasons = dep_block_reasons(&conn, &ids);

        // THE POINT: the untyped `done` card must say it is done AND why that is
        // not enough. Asserting on "done" alone would pass on the old message.
        assert!(reasons[0].contains("done"), "must name the real status: {:?}", reasons[0]);
        assert!(
            reasons[0].contains("completes at verified"),
            "must name the boundary that makes a done card still block: {:?}",
            reasons[0]
        );
        assert!(reasons[0].contains("unset->code"), "must say the type defaulted: {:?}", reasons[0]);

        // CONTROLS. Without these, a helper that appends the same clause to
        // everything passes the block above.
        assert!(!deps_blocking_contains(&conn, "DEP-DOC"), "a doc card at `done` is resolved");
        assert!(deps_blocking_contains(&conn, "DEP-UNTYPED"), "untyped defaults to code");
        assert!(deps_blocking_contains(&conn, "DEP-DISCARDED"), "discarded is not resolution");
        assert!(reasons[2].contains("discarded"), "names its real status: {:?}", reasons[2]);

        // Absent and unreadable are their own answers, never "unfinished".
        let missing = dep_block_reasons(&conn, &["NOPE-1".to_string()]);
        assert!(missing[0].contains("missing"), "{:?}", missing[0]);
    }

    /// Helper: does this id block, per the shipped predicate?
    fn deps_blocking_contains(conn: &Connection, id: &str) -> bool {
        !bs::dependency_resolved(conn, id).expect("dependency lookup")
    }

    /// And the rule itself, stated where a reader looking for it will find it.
    #[test]
    fn done_resolves_a_dependency_only_where_verified_is_not_meaningful() {
        assert!(bs::dependency_is_resolved("verified", "code"));
        assert!(bs::dependency_is_resolved("done", "doc"));
        assert!(bs::dependency_is_resolved("done", "research"));
        // Ships something, so `done` is not the end.
        assert!(!bs::dependency_is_resolved("done", "code"));
        assert!(!bs::dependency_is_resolved("done", "ops"));
        // THE ONE THAT BIT: no type at all is treated as code.
        assert!(!bs::dependency_is_resolved("done", ""));
        // Terminal, and still not resolution.
        assert!(!bs::dependency_is_resolved("discarded", "doc"));
    }

    #[test]
    fn unheeded_counts_only_when_a_comparison_was_actually_possible() {
        // First sighting: no prior mark. Counts as movement, never as silence.
        assert_eq!(nudge_feedback(0, 1_700_000_000, 0), (0, true));
        assert_eq!(nudge_feedback(0, 1_700_000_000, 7), (0, true), "and it CLEARS a stuck count");

        // A card moved: the watermark advanced.
        assert_eq!(nudge_feedback(1_700_000_000, 1_700_000_050, 2), (0, true));

        // Nothing moved: accumulate.
        assert_eq!(nudge_feedback(1_700_000_000, 1_700_000_000, 0), (1, false));
        assert_eq!(nudge_feedback(1_700_000_000, 1_700_000_000, 2), (3, false));
    }

    /// The cadence now BACKS OFF while a lane is not responding.
    ///
    /// The defect this closes is that the old curve only ever went faster:
    /// `idle_backlog_drain_cooldown_s` scales with backlog SIZE, so a big
    /// backlog pinned itself to the 20-minute floor and stayed. Measured over
    /// 34h to 2026-08-30, mvs-infra sat at a 22-minute gap for 80 nudges and
    /// moved 0 cards.
    #[test]
    fn the_backoff_doubles_and_is_bounded() {
        let floor = 20.0 * 60.0;
        assert_eq!(unheeded_backoff(floor, 0), floor, "a responsive lane is untouched");
        assert_eq!(unheeded_backoff(floor, 1), floor * 2.0);
        assert_eq!(unheeded_backoff(floor, 2), floor * 4.0);
        // Bounded, so a long-dead lane cannot overflow the multiplier into a
        // cooldown that never expires.
        assert!(unheeded_backoff(floor, 500) <= floor * 256.0);
        // CONTROL: the OLD behaviour is what this replaces — the floor forever.
        // If backoff ever became a no-op this equality would hold at every n.
        assert_ne!(unheeded_backoff(floor, 3), floor);
    }

    /// A lane that never moves is escalated in a few ticks, not eighty.
    ///
    /// The doc here used to cite mvs-infra as an 80-nudge specimen; that was a
    /// bad join and mvs-infra is in fact the busiest board user in the fleet
    /// (corrected 2026-08-30). The BOUND is what matters and it stands on its
    /// own: whatever the lane, silence is answered in `cap` ticks rather than
    /// indefinitely.
    #[test]
    fn a_silent_lane_is_escalated_within_a_few_ticks_not_eighty() {
        let cap = nudge_unheeded_cap();
        assert!(cap >= 1, "a cap of 0 would suppress the first nudge, which is not the fix");
        let mut unheeded = 0;
        let mark = 1_700_000_000;
        let mut fired = 0;
        for _ in 0..80 {
            let (next, _) = nudge_feedback(mark, mark, unheeded);
            unheeded = next;
            if unheeded >= cap {
                break;
            }
            fired += 1;
        }
        assert!(
            fired < 5,
            "a lane that never moves used to be nudged indefinitely; it now gets {fired} \
             before the queue is escalated instead"
        );
    }


    /// amux's review question, made into a test rather than answered in prose:
    /// "can a card promoted by the revisit arm be distinguished, after the
    /// fact, from one promoted by the original arm? If both write the same
    /// shape, the first 'why did this move?' is unanswerable."
    ///
    /// The card's own log is where that question gets asked (Ethan: the board
    /// task is the source of truth for its own history), so the two arms must
    /// write DIFFERENT lines there — not merely differ in a counter the card
    /// does not carry.
    #[test]
    fn the_two_promotion_arms_are_distinguishable_on_the_card_itself() {
        let a = PromoteArm::DepsCleared.log_line();
        let b = PromoteArm::RevisitDue.log_line();
        assert_ne!(a, b, "both arms would write the same history line");
        // Each must name its OWN cause, not just differ. "Re-activated" alone
        // on both would pass assert_ne while leaving the reader no better off.
        assert!(a.contains("depends_on"), "deps arm must name its cause: {a}");
        assert!(b.contains("revisit date"), "revisit arm must name its cause: {b}");
    }

    /// The rate limit is the part that fails INVISIBLY: an off-by-one that
    /// lets every candidate through still promotes cards, still logs, still
    /// looks healthy in the report, and floods the column the drain exists to
    /// protect. One lane on this board held 219 backlog cards.
    #[test]
    fn due_drain_fills_a_lane_to_the_ceiling_and_stops() {
        let cands: Vec<(String, String)> = (0..10)
            .map(|i| (format!("C-{i}"), "backend".to_string()))
            .collect();
        let mut depth = std::collections::BTreeMap::new();

        // Empty column, ceiling 5 -> exactly 5, and they are the FIRST five
        // (the scan hands them over most-overdue-first, so the order is the
        // priority and a plan that reorders would drain the wrong cards).
        depth.insert("backend".to_string(), 0i64);
        let got = due_drain_plan(&cands, &depth, 5);
        assert_eq!(got, vec!["C-0", "C-1", "C-2", "C-3", "C-4"]);

        // A column already at the ceiling takes nothing. This is the clause
        // that makes the backlog drain at the rate the lane completes work.
        depth.insert("backend".to_string(), 5);
        assert!(due_drain_plan(&cands, &depth, 5).is_empty());

        // OVER the ceiling is also nothing, not a negative slot count.
        depth.insert("backend".to_string(), 99);
        assert!(due_drain_plan(&cands, &depth, 5).is_empty());

        // Partial room takes exactly the remainder.
        depth.insert("backend".to_string(), 3);
        assert_eq!(due_drain_plan(&cands, &depth, 5).len(), 2);

        // Ceiling 0 disables the drain entirely — the documented off switch.
        depth.insert("backend".to_string(), 0);
        assert!(due_drain_plan(&cands, &depth, 0).is_empty());
        assert!(due_drain_plan(&cands, &depth, -1).is_empty());
    }

    /// The ceiling is PER LANE. A shared counter would let one lane's deep
    /// backlog consume every slot on the board and starve the other 48.
    #[test]
    fn due_drain_ceiling_is_per_lane_not_global() {
        let cands: Vec<(String, String)> = vec![
            ("A-1".into(), "backend".into()),
            ("B-1".into(), "mvs-infra".into()),
            ("A-2".into(), "backend".into()),
            ("B-2".into(), "mvs-infra".into()),
        ];
        let mut depth = std::collections::BTreeMap::new();
        depth.insert("backend".to_string(), 1i64);
        // mvs-infra is absent from the map — an unmeasured lane is an EMPTY
        // one, not a full one. Reading a missing key as "at the ceiling" would
        // silently exclude every lane with a clean todo column, which is
        // exactly the set the drain is for.
        let got = due_drain_plan(&cands, &depth, 2);
        assert_eq!(got, vec!["A-1", "B-1", "B-2"], "backend gets 1 slot, mvs-infra 2");
    }
    use super::*;
    use rusqlite::Connection;

    /// Every test below routes through the injected-lookup seam with a
    /// PERMISSIVE registry, shadowing the real `select_advance`.
    ///
    /// A board fixture's `peer` / `reviewer` lanes exist on no machine, so the
    /// reviewer-unreachable gate (AMUX-3751) turned "this test host has no lane
    /// called peer" into a routing failure — three routing tests, red on any
    /// host, including CI. Tests that mean to exercise the GATE pass their own
    /// lookup to `select_advance_with`; `the_gate_refuses_a_reviewer_no_nudge_
    /// can_reach` below is the one that does, so this shadow cannot hide it.
    fn select_advance(conn: &Connection, session: &str, tags: &[String], now: f64) -> Advance {
        select_advance_with(conn, session, tags, now, &|_, _| None)
    }

    /// The drain cadence must SCALE to the backlog (Ethan 2026-08-13: big idle
    /// backlogs drained too slowly at a flat 2h). Pure math, no env.
    #[test]
    fn drain_cooldown_shortens_as_the_backlog_grows() {
        let base = 2.0 * 3600.0;
        let floor = 20.0 * 60.0;
        let per = 25.0;
        // Small backlog: the gentle base cadence.
        assert_eq!(drain_cooldown_scaled(5, base, floor, per), base);
        assert_eq!(drain_cooldown_scaled(25, base, floor, per), base);
        // Twice `per` halves it.
        assert_eq!(drain_cooldown_scaled(50, base, floor, per), base / 2.0);
        // A big idle backlog (backend's 207) is clamped to the floor, not
        // spamming, but far more frequent than 2h.
        assert_eq!(drain_cooldown_scaled(207, base, floor, per), floor);
        // Monotonic non-increasing: more backlog is never a SLOWER nudge.
        let mut prev = f64::INFINITY;
        for n in [1, 10, 25, 40, 60, 100, 200, 500] {
            let c = drain_cooldown_scaled(n, base, floor, per);
            assert!(c <= prev, "cadence must not slow as backlog grows: {n} -> {c} > {prev}");
            assert!(c >= floor, "never below the floor");
            prev = c;
        }
    }

    /// AMUX-2903, second pass. `done` must not TRIGGER the nudge.
    ///
    /// The first fix (change-detection) was disproved by the next nudge that
    /// fired: closing one card moved done 228 -> 229, which re-armed it. A lane
    /// that works generates `done` forever, so any change signal derived from
    /// it never terminates — and reaching `verified` needs CI and a deploy the
    /// lane does not control.
    #[test]
    fn a_lane_with_only_done_cards_is_not_nudged_however_many_it_has() {
        let conn = board_db();
        let ins = |id: &str, status: &str| {
            conn.execute(
                "INSERT INTO issues (id,title,status,session,owner_type,archived,type,updated,created) \
                 VALUES (?1,?2,?3,'me','agent',0,'code',100,100)",
                rusqlite::params![id, format!("card {id}"), status],
            )
            .expect("insert");
        };

        // 229 done, 0 blocked — my lane's real shape when the loop fired.
        for i in 0..229 {
            ins(&format!("D-{i}"), "done");
        }
        let (bc, dc, _, _, _) = outstanding_work(&conn, "me");
        assert_eq!((bc, dc), (0, 229));
        // The trigger is `bc > 0`. With no blocked cards there is nothing to
        // nudge about, no matter how large `done` grows.
        assert_eq!(bc, 0, "done alone must not arm the nudge");

        // Add one blocked card and it arms — because re-assessing a blocker is
        // work this lane can actually do.
        ins("B-1", "blocked");
        let (bc2, dc2, blocked, _, _) = outstanding_work(&conn, "me");
        assert_eq!((bc2, dc2), (1, 229));
        assert_eq!(blocked.len(), 1, "the blocked card is named in the nudge");
    }

    /// AF-695. `amux board ask` neither reaches a reviewer (it pings the
    /// card's OWNER) nor works on a terminal card (it refuses `done`
    /// outright) -- measured live by mixpeek-funnel, 8 of 8 refused. The
    /// nudge must point at the verb that actually notifies the named
    /// reviewer (`amux board review`, which fires `reviewer_notify` on entry
    /// into `review`) and must not recommend `ask` for this population.
    #[test]
    fn the_reviewer_named_nudge_recommends_review_not_ask() {
        let done = vec![("D-1".to_string(), "a done card".to_string(), "peer-lane".to_string())];
        let text = continue_nudge_text(0, 1, &[], &done, &[]);
        assert!(
            text.contains("amux board review"),
            "must recommend the verb that notifies the named reviewer: {text}"
        );
        assert!(
            !text.contains("ask them to verify"),
            "must not tell a lane that `ask` reaches the reviewer -- it pings the owner: {text}"
        );
    }

    /// Follow-up from mixpeek-funnel's real batch: `review`'s gate is
    /// type-scoped, and `code`'s asks for shipped work ("Implemented and
    /// self-tested", "Diff / PR is up"). Recommending `review` unconditionally
    /// would force the same false attestation a sibling nudge arm already
    /// warns about, for a done+reviewer card whose code has not shipped.
    #[test]
    fn the_reviewer_named_nudge_warns_the_review_gate_is_not_universal() {
        let done = vec![("D-1".to_string(), "a done card".to_string(), "peer-lane".to_string())];
        let text = continue_nudge_text(0, 1, &[], &done, &[]);
        assert!(
            text.contains("code") && text.contains("has not shipped"),
            "must warn that a `code`-typed card whose work has not shipped cannot honestly \
             attest review's gate either: {text}"
        );
    }

    /// AMUX-3775. The walk itself, over a lookup, so every branch has a cell.
    #[test]
    fn the_depends_on_walk_finds_a_human_root_and_refuses_to_guess() {
        let table: &[(&str, &str, &[&str])] = &[
            // The reporting lane's real chain: MG-1356 -> 1355 -> 1354 -> 1369.
            ("MG-1356", "blocked", &["MG-1355"]),
            ("MG-1355", "blocked", &["MG-1354"]),
            ("MG-1354", "blocked", &["MG-1369"]),
            ("MG-1369", "needsyou", &[]),
            // A chain that roots in ordinary work is NOT human-blocked.
            ("W-1", "blocked", &["W-2"]),
            ("W-2", "todo", &[]),
            // A chain that points at nothing resolvable.
            ("D-1", "blocked", &["GONE-9"]),
            // A cycle.
            ("C-1", "blocked", &["C-2"]),
            ("C-2", "blocked", &["C-1"]),
            // A lane that retyped ITSELF to needsyou must not silence itself.
            ("S-1", "needsyou", &[]),
        ];
        let lookup = |id: &str| -> CardLink {
            table.iter().find(|(i, _, _)| *i == id).map(|(_, s, d)| {
                (s.to_string(), d.iter().map(|x| x.to_string()).collect())
            })
        };
        assert_eq!(
            human_blocked_root("MG-1356", &lookup, DEPENDS_ON_MAX_DEPTH),
            Some("MG-1369".into()),
            "a three-hop chain to a needsyou card is blocked on the human"
        );
        assert_eq!(human_blocked_root("MG-1354", &lookup, DEPENDS_ON_MAX_DEPTH), Some("MG-1369".into()));
        // THE CONTROLS. Each of these would silence a card nobody is waiting on,
        // which is the same defect as the nudge that never stops, pointed the
        // other way.
        assert_eq!(human_blocked_root("W-1", &lookup, DEPENDS_ON_MAX_DEPTH), None, "root is todo");
        assert_eq!(human_blocked_root("D-1", &lookup, DEPENDS_ON_MAX_DEPTH), None, "dangling id");
        assert_eq!(human_blocked_root("C-1", &lookup, DEPENDS_ON_MAX_DEPTH), None, "a cycle terminates");
        // NOTE, from amux-frustrations' review: this cycle assertion does NOT
        // test the seen-set. A two-node cycle keeps the frontier at one element
        // per level, so the loop runs out of depth and returns None whether or
        // not `seen` exists — they proved it by deleting the seen-set and
        // watching the suite stay green. The cell that discriminates is below.
        assert_eq!(
            human_blocked_root("S-1", &lookup, DEPENDS_ON_MAX_DEPTH),
            None,
            "the START card's own status must not count, or a card retyped to needsyou silences itself"
        );
        // The cap holds: one hop is not enough to reach MG-1369 from MG-1356.
        assert_eq!(human_blocked_root("MG-1356", &lookup, 1), None, "depth is bounded");
    }

    /// THE SEEN-SET, which the cycle cell above cannot test (amux-frustrations,
    /// reviewing AMUX-3775). They mutation-tested my three named attacks; three
    /// failed the suite as intended and DELETING THE SEEN-SET ENTIRELY left it
    /// green. A two-node cycle terminates by the depth bound, giving the same
    /// answer by the wrong mechanism.
    ///
    /// What `seen` actually buys is protection against RE-EXPANSION IN A
    /// DIAMOND, which no cycle can exercise. This builds a lattice where every
    /// level's nodes both point at both of the next level's, so the frontier
    /// doubles per level without dedup: 2^12 expansions at the default depth.
    /// The assertion is on the CALL COUNT, because the return value is `None`
    /// either way — the defect is cost, not correctness, and only a counter can
    /// see it.
    #[test]
    fn the_seen_set_stops_a_diamond_from_re_expanding() {
        const LEVELS: usize = 12;
        let calls = std::cell::Cell::new(0usize);
        let lookup = |id: &str| -> CardLink {
            calls.set(calls.get() + 1);
            // "L<level>-<branch>" -> both nodes of the next level.
            let (lvl, _) = id.split_once('-').and_then(|(l, b)| {
                Some((l.strip_prefix('L')?.parse::<usize>().ok()?, b))
            })?;
            if lvl >= LEVELS {
                return Some(("blocked".to_string(), vec![]));
            }
            Some((
                "blocked".to_string(),
                vec![format!("L{}-a", lvl + 1), format!("L{}-b", lvl + 1)],
            ))
        };
        assert_eq!(human_blocked_root("L0-a", &lookup, DEPENDS_ON_MAX_DEPTH), None);
        // With dedup the walk visits at most 2 nodes per level plus the start.
        // Without it the frontier doubles: 2^12 = 4096 expansions before the
        // depth bound stops it. The bound is deliberately far below that and far
        // above the deduped count, so it cannot pass by luck either way.
        assert!(
            calls.get() < 64,
            "the walk expanded {} times on a {LEVELS}-level diamond — the seen-set is not \
             deduping, and a wide dependency graph will melt the nudge",
            calls.get()
        );
    }

    /// AMUX-3775, through `outstanding_work` — the shipped path, on the shape
    /// mixpeek-general reported: several blocked cards chaining to one card
    /// waiting on Ethan, plus one genuinely actionable.
    #[test]
    fn blocked_on_a_human_root_is_excluded_from_the_count_and_still_named() {
        let conn = board_db();
        let ins = |id: &str, status: &str, dep: Option<&str>| {
            conn.execute(
                "INSERT INTO issues (id,title,status,session,owner_type,archived,type,updated,depends_on,created) \
                 VALUES (?1,?2,?3,'me','agent',0,'code',100,?4,100)",
                rusqlite::params![
                    id,
                    format!("card {id}"),
                    status,
                    dep.map(|d| format!("[\"{d}\"]"))
                ],
            )
            .expect("insert");
        };
        ins("MG-1369", "needsyou", None);
        ins("MG-1354", "blocked", Some("MG-1369"));
        ins("MG-1355", "blocked", Some("MG-1354"));
        ins("MG-1356", "blocked", Some("MG-1355"));
        // No depends_on: its trigger is a fleet state, so the lane CAN re-assess it.
        ins("MG-1426", "blocked", None);

        let (bc, _dc, blocked, _done, human_blocked) = outstanding_work(&conn, "me");
        assert_eq!(bc, 1, "only the actionable one arms the nudge, not all four");
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].0, "MG-1426");
        assert_eq!(human_blocked.len(), 3, "the three chaining to MG-1369 are set aside");
        assert!(human_blocked.iter().all(|(_, _, root)| root == "MG-1369"));

        // NAMED, NOT DROPPED (ethos rule 1). A version that just filtered them
        // out would pass every assertion above and leave the lane with a count
        // that silently disagrees with its own board.
        let text = continue_nudge_text(bc, 0, &blocked, &[], &human_blocked);
        assert!(text.contains("MG-1426"), "the actionable one is still asked about: {text}");
        assert!(text.contains("MG-1369"), "and the human root is named: {text}");
        assert!(text.contains("MG-1356"), "and the excluded cards are listed: {text}");
        assert!(
            text.contains("Nothing for you to do"),
            "the excluded ones carry no ask: {text}"
        );
        // The specific wrong fix, named in the message so nobody reaches for it.
        assert!(text.contains("do not retype them to `needsyou`"), "{text}");

        // AND WITH NOTHING ACTIONABLE, bc IS ZERO — which is what makes the
        // nudge stop, since the caller's trigger is `bc > 0`.
        conn.execute("UPDATE issues SET status='todo' WHERE id='MG-1426'", []).expect("update");
        let (bc2, _, _, _, hb2) = outstanding_work(&conn, "me");
        assert_eq!(bc2, 0, "a lane whose every blocker is human-rooted is not nudged");
        assert_eq!(hb2.len(), 3, "and they are still visible to the caller, not lost");
    }

    /// AMUX-2903. The continue-nudge's cooldown is a RATE limit, not an exit,
    /// and `done` never shrinks — so without change-detection it fires every 30
    /// minutes for the life of a lane. Measured before the first nudge went out:
    /// 15 lanes eligible, 277 done cards, ~720/day.
    #[test]
    fn the_continue_nudge_does_not_repeat_an_unchanged_outstanding_set() {
        let conn = Connection::open_in_memory().expect("memdb");
        conn.execute_batch(
            "CREATE TABLE session_events (id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL NOT NULL,
                session TEXT NOT NULL DEFAULT '', type TEXT NOT NULL, data TEXT,
                idem TEXT, source TEXT NOT NULL DEFAULT '');",
        )
        .expect("schema");
        let fire = |ts: f64, session: &str, data: &str| {
            conn.execute(
                "INSERT INTO session_events (ts, session, type, data) VALUES (?1,?2,'continue.nudge',?3)",
                rusqlite::params![ts, session, data],
            )
            .expect("insert");
        };

        // Never nudged -> nothing to compare, so the first nudge must go out.
        assert_eq!(last_continue_nudge_counts(&conn, "me"), None);

        fire(100.0, "me", r#"{"blocked":3,"done":56}"#);
        assert_eq!(last_continue_nudge_counts(&conn, "me"), Some((3, 56)));

        // The NEWEST event wins — an older one must not resurrect a stale set.
        fire(200.0, "me", r#"{"blocked":2,"done":57}"#);
        assert_eq!(last_continue_nudge_counts(&conn, "me"), Some((2, 57)));

        // Scoped per lane: another lane's nudge must not silence this one.
        fire(300.0, "other", r#"{"blocked":9,"done":9}"#);
        assert_eq!(last_continue_nudge_counts(&conn, "me"), Some((2, 57)));

        // CONTROLS — every one of these must RE-ARM, not suppress. A nudge that
        // goes quiet on unreadable state is the same defect one layer down.
        let conn2 = Connection::open_in_memory().expect("memdb");
        conn2
            .execute_batch(
                "CREATE TABLE session_events (id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL NOT NULL,
                    session TEXT NOT NULL DEFAULT '', type TEXT NOT NULL, data TEXT,
                    idem TEXT, source TEXT NOT NULL DEFAULT '');",
            )
            .expect("schema");
        for bad in [Some("not json"), Some("{}"), Some(r#"{"blocked":1}"#), None] {
            conn2.execute("DELETE FROM session_events", []).expect("clear");
            conn2
                .execute(
                    "INSERT INTO session_events (ts, session, type, data) VALUES (1,'me','continue.nudge',?1)",
                    rusqlite::params![bad],
                )
                .expect("insert");
            assert_eq!(
                last_continue_nudge_counts(&conn2, "me"),
                None,
                "unreadable nudge state must re-arm, not suppress (data={bad:?})"
            );
        }
    }

    /// AMUX-3563, built from the 7-day specimen rather than a convenient one.
    ///
    /// The measured defect: the per-card budget is 3 per 24h with NO SPACING,
    /// so TUBES-2063 spent all three in 42 MINUTES and then went silent for 23
    /// hours. Meanwhile 147 of 212 (lane, card) pairs were nudged exactly once,
    /// which is the nudge working and must not change.
    ///
    /// The controls are the point. A backoff that also delays the FIRST nudge
    /// would look like a bigger win in the aggregate and would be a regression:
    /// it is the repeats that are waste, and the first prompt is the product.
    #[test]
    fn repeat_nudges_back_off_without_delaying_the_first() {
        // The first nudge is never held, at any streak-0 state.
        assert_eq!(
            advance_required_gap_s(0),
            ADVANCE_COOLDOWN_S,
            "the first nudge about a card must not be delayed — 147 of 212 pairs \
             in the specimen were nudged exactly once and that IS the feature"
        );

        // Repeats stretch: 15m, 30m, 1h, 2h...
        assert_eq!(advance_required_gap_s(1), 30.0 * 60.0);
        assert_eq!(advance_required_gap_s(2), 60.0 * 60.0);
        assert!(
            advance_required_gap_s(3) > advance_required_gap_s(2),
            "each repeat must cost more than the last, or this is a flat cooldown \
             wearing a backoff's name"
        );

        // TUBES-2063's actual burst: 3 nudges inside 42 minutes. The second one
        // arrived 21 minutes in, which clears the flat 15m cooldown and is
        // exactly why the flat cooldown did not stop it.
        // The precondition is deliberately a compile-time assert: if the base
        // cooldown is ever raised past the recorded burst spacing, this specimen
        // stops demonstrating anything and the build should say so rather than
        // the test quietly continuing to pass for the wrong reason. `const`
        // form because clippy correctly rejects a runtime assert on constants.
        const BURST_GAP_S: f64 = 21.0 * 60.0; // TUBES-2063's actual 2nd nudge
        const _: () = assert!(BURST_GAP_S > ADVANCE_COOLDOWN_S);
        assert!(
            BURST_GAP_S < advance_required_gap_s(1),
            "the 2nd nudge of the recorded burst must now be held; the burst \
             cleared the old flat cooldown, which is why it happened at all"
        );

        // And it is bounded: a card nudged forever cannot push the gap to
        // infinity and effectively mute itself.
        let capped = advance_required_gap_s(40);
        assert_eq!(capped, advance_backoff_max_s(), "the doubling must hit its cap");
    }

    /// The streak is keyed on STATUS, so a card that MOVED starts over. Without
    /// this the backoff punishes a card for history it has already answered, and
    /// a lane that acts on the nudge gets a longer wait as its reward.
    #[test]
    fn moving_the_card_resets_the_backoff_streak() {
        let conn = Connection::open_in_memory().expect("memdb");
        conn.execute_batch(
            "CREATE TABLE session_events (id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL NOT NULL,
                session TEXT NOT NULL DEFAULT '', type TEXT NOT NULL, data TEXT,
                idem TEXT, source TEXT NOT NULL DEFAULT '');",
        )
        .expect("schema");
        for ts in [100.0, 200.0, 300.0] {
            conn.execute(
                "INSERT INTO session_events (ts, session, type, data) \
                 VALUES (?1,'me','advance.nudged',?2)",
                rusqlite::params![ts, r#"{"issue":"TUBES-2063","status":"doing"}"#],
            )
            .expect("insert");
        }

        let (streak, last) = advance_streak(&conn, "TUBES-2063", "doing", 0.0);
        assert_eq!(streak, 3, "three nudges in 'doing' is a streak of three");
        assert_eq!(last, 300.0, "and the most recent one is what the gap measures from");

        // CONTROL: the same card in a different status has no history. This is
        // what proves the status filter EXCLUDES rather than matching every row
        // — an unbounded match and a correct one are identical from a count.
        let (moved, _) = advance_streak(&conn, "TUBES-2063", "review", 0.0);
        assert_eq!(moved, 0, "moving the card must reset the streak to zero");

        // CONTROL: a different card is unaffected.
        let (other, _) = advance_streak(&conn, "TUBES-9999", "doing", 0.0);
        assert_eq!(other, 0, "the streak must be per-card, not per-status-globally");
    }

    /// AMUX-2825's non-negotiable constraint, which fe44d61 did not implement:
    /// the sweep MUST exclude types where `verified` is meaningless. 299 of
    /// 1162 live agent-owned done cards (25%, 36 lanes) are such types.
    #[test]
    fn the_sweep_never_lists_a_card_that_cannot_be_verified() {
        let conn = board_db();
        let ins = |id: &str, typ: &str| {
            conn.execute(
                "INSERT INTO issues (id,title,status,session,owner_type,archived,type,updated,created) \
                 VALUES (?1,?2,'done','me','agent',0,?3,100,100)",
                rusqlite::params![id, format!("card {id}"), typ],
            )
            .expect("insert");
        };
        // Every enum variant, split by the predicate itself — so this test
        // cannot disagree with verified_is_meaningful even if it changes.
        for t in amux_core::board::ItemType::ALL {
            ins(&format!("T-{}", t.as_str()), t.as_str());
        }
        ins("T-bug", "bug"); // outside the enum — must stay verifiable

        let got = done_verify_candidates(&conn, "me");
        let ids: std::collections::HashSet<&str> = got.iter().map(|(i, _, _)| i.as_str()).collect();

        for t in amux_core::board::ItemType::ALL {
            let id = format!("T-{}", t.as_str());
            let want = amux_core::board::verified_is_meaningful(t);
            assert_eq!(
                ids.contains(id.as_str()),
                want,
                "{} verified_is_meaningful={want} but selection said otherwise",
                t.as_str()
            );
        }
        assert!(ids.contains("T-bug"), "an unknown type must stay verifiable, not be silently dropped");
        assert_eq!(
            done_card_count(&conn, "me") as usize,
            ids.len(),
            "the count and the selector must share the type filter too"
        );
    }

    /// The verify-nudge shipped with no test of its own (fe44d61). These pin
    /// the two properties that decide whether it nags correctly or wrongly.
    #[test]
    fn verify_candidates_never_include_a_humans_card_or_an_archived_one() {
        let conn = board_db();
        let ins = |id: &str, session: &str, status: &str, owner: &str, arch: i64, del: Option<i64>| {
            conn.execute(
                "INSERT INTO issues (id,title,status,session,owner_type,archived,deleted,type,updated,created)                  VALUES (?1,?2,?3,?4,?5,?6,?7,'code',100,100)",
                rusqlite::params![id, format!("card {id}"), status, session, owner, arch, del],
            )
            .expect("insert");
        };
        ins("A-1", "me", "done", "agent", 0, None);      // the only one that qualifies
        ins("A-2", "me", "done", "human", 0, None);      // ethos rule 8: never sweep a human's work
        ins("A-3", "me", "done", "agent", 1, None);      // archived
        ins("A-4", "me", "done", "agent", 0, Some(1));   // deleted
        ins("A-5", "me", "todo", "agent", 0, None);      // not done
        ins("A-6", "other", "done", "agent", 0, None);   // another lane

        let got = done_verify_candidates(&conn, "me");
        let ids: Vec<&str> = got.iter().map(|(i, _, _)| i.as_str()).collect();
        assert_eq!(ids, vec!["A-1"], "only this lane's own live agent-owned done card");
        assert_eq!(done_card_count(&conn, "me"), 1, "the count must share the selector's predicate");
    }

    /// The nudge's closing line NAMES `needs:you` as the way out. Until AEAB-9
    /// neither query looked at `issue_tags`, so taking the sanctioned escape did
    /// not exit the loop — AEAB-5 carried the tag for two days and was nudged
    /// anyway. A constraint whose documented escape is unwalkable is theatre
    /// (ethos rule 6), so this pins the escape, not just the exclusion.
    ///
    /// The `N-1` control is load-bearing. A predicate that matched EVERYTHING
    /// would also produce an empty candidate list and pass a
    /// "the tagged card is absent" assertion on its own — the unbounded-filter
    /// failure this repo has shipped twice. The sub-tag case is the second
    /// control: `select_pickup` uses LIKE, and an exact-match here would let
    /// `needs:you:decision` through while agreeing with it on the plain tag.
    #[test]
    fn a_card_tagged_needs_you_is_not_nudged_at_its_agent() {
        let conn = board_db();
        let ins = |id: &str| {
            conn.execute(
                "INSERT INTO issues (id,title,status,session,owner_type,archived,type,updated,created) \
                 VALUES (?1,?2,'done','me','agent',0,'code',100,100)",
                rusqlite::params![id, format!("card {id}")],
            )
            .expect("insert");
        };
        let tag = |id: &str, t: &str| {
            conn.execute(
                "INSERT INTO issue_tags (issue_id,tag,added_at) VALUES (?1,?2,1.0)",
                rusqlite::params![id, t],
            )
            .expect("tag");
        };
        ins("N-1"); // CONTROL: untagged — must still be nudged, or the filter matches all
        ins("N-2");
        tag("N-2", "needs:you");
        ins("N-3");
        tag("N-3", "NEEDS:YOU"); // case is not the discriminator
        ins("N-4");
        tag("N-4", "needs:you:decision"); // sub-tagged ask — same LIKE form as select_pickup
        ins("N-5");
        tag("N-5", "mobile"); // an unrelated tag must not exempt anything

        let got = done_verify_candidates(&conn, "me");
        let mut ids: Vec<&str> = got.iter().map(|(i, _, _)| i.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec!["N-1", "N-5"],
            "only cards NOT parked on a human; N-1/N-5 present proves the filter is not matching everything"
        );
        assert_eq!(
            done_card_count(&conn, "me"),
            2,
            "the count must share the selector's needs:you predicate too"
        );
    }

    /// The "... and N more" line is arithmetic over TWO queries. If they ever
    /// disagree the prompt states a number the list cannot account for — the
    /// view-vs-mechanism drift this repo keeps finding. This pins them together
    /// past the LIMIT 8 boundary, where the two genuinely differ by design.
    #[test]
    fn the_and_n_more_line_agrees_with_the_selector_past_the_cap() {
        let conn = board_db();
        for i in 0..11 {
            conn.execute(
                "INSERT INTO issues (id,title,status,session,owner_type,archived,type,updated,created)                  VALUES (?1,?2,'done','me','agent',0,'code',?3,?3)",
                rusqlite::params![format!("A-{i}"), format!("card {i}"), i],
            )
            .expect("insert");
        }
        let cards = done_verify_candidates(&conn, "me");
        let total = done_card_count(&conn, "me");
        assert_eq!(cards.len(), 8, "capped at 8 for prompt brevity");
        assert_eq!(total, 11, "but the count sees all of them");

        let text = verify_nudge_text(&cards, total);
        assert!(text.contains("... and 3 more"), "11 - 8 = 3; got: {text}");
        assert!(text.contains("11 of your"), "the headline must state the true total");
        // Every listed card must actually appear, or the list and the count
        // describe different sets.
        for (id, _, _) in &cards {
            assert!(text.contains(id.as_str()), "{id} listed in the selector but absent from the prompt");
        }
    }

    /// AMUX-3477: the nudge must point at the RESOLVED gate, never restate
    /// criteria — the previous version hardcoded four the board does not use,
    /// and following it literally moved cards on the wrong basis (the AF-112
    /// family: instruction and enforcement disagreeing about what the gate is).
    #[test]
    fn the_verify_nudge_points_at_the_resolved_gate_and_restates_nothing() {
        let cards: Vec<(String, String, String)> =
            vec![("A-1".into(), "t".into(), "code".into())];
        let text = verify_nudge_text(&cards, 1);
        assert!(text.contains("/api/board/contract?card="), "must name the contract read: {text}");
        assert!(
            !text.contains("CI/CD passed") && !text.contains("deployed to production"),
            "hardcoded criteria are the defect, not the fix: {text}"
        );
    }

    /// A lane with exactly the cap must not be told there are "0 more".
    #[test]
    fn no_and_n_more_line_when_nothing_was_dropped() {
        let cards: Vec<(String, String, String)> =
            (0..3).map(|i| (format!("A-{i}"), "t".into(), "code".into())).collect();
        let text = verify_nudge_text(&cards, 3);
        assert!(!text.contains("more"), "nothing was dropped; got: {text}");
    }

    /// The live board schema, trimmed to the columns these predicates read.
    fn board_db() -> Connection {
        crate::db::migrate::test_memdb()
    }

    struct BoundaryFleet {
        delivered: std::sync::Mutex<Vec<(String, String)>>,
        resume_ids: std::sync::Mutex<HashSet<String>>,
        resume_disposition: std::sync::Mutex<Option<ResumeDelivery>>,
        running: std::sync::atomic::AtomicBool,
        boundary: std::sync::atomic::AtomicBool,
        active_child: std::sync::atomic::AtomicBool,
        enabled: std::sync::atomic::AtomicBool,
        isolated: std::sync::atomic::AtomicBool,
        starts: std::sync::atomic::AtomicUsize,
        start_error: std::sync::Mutex<Option<String>>,
        delivery_error: std::sync::Mutex<Option<String>>,
    }
    impl Default for BoundaryFleet {
        fn default() -> Self {
            Self { delivered: std::sync::Mutex::new(vec![]),
                resume_ids: std::sync::Mutex::new(HashSet::new()),
                resume_disposition: std::sync::Mutex::new(None),
                running: std::sync::atomic::AtomicBool::new(true),
                boundary: std::sync::atomic::AtomicBool::new(true),
                active_child: std::sync::atomic::AtomicBool::new(false),
                enabled: std::sync::atomic::AtomicBool::new(true),
                isolated: std::sync::atomic::AtomicBool::new(false),
                starts: std::sync::atomic::AtomicUsize::new(0),
                start_error: std::sync::Mutex::new(None), delivery_error: std::sync::Mutex::new(None) }
        }
    }
    impl Fleet for BoundaryFleet {
        fn lanes(&self) -> Vec<String> { vec!["lane".into()] }
        fn auto_pickup_enabled(&self, _: &str) -> bool { self.enabled.load(std::sync::atomic::Ordering::SeqCst) }
        fn tags(&self, _: &str) -> Vec<String> { vec![] }
        fn is_isolated(&self, _: &str) -> bool { self.isolated.load(std::sync::atomic::Ordering::SeqCst) }
        async fn is_running(&self, _: &str) -> bool { self.running.load(std::sync::atomic::Ordering::SeqCst) }
        async fn at_boundary(&self, _: &str) -> bool { self.boundary.load(std::sync::atomic::Ordering::SeqCst) }
        fn active_child_work(&self, _: &str) -> bool {
            self.active_child.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn auto_continue_enabled(&self, _: &str) -> bool { true }
        async fn start_for_dispatch(&self, _: &str) -> Result<(), String> {
            self.starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(error) = self.start_error.lock().unwrap().clone() { return Err(error); }
            self.running.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn deliver(&self, lane: &str, text: &str) {
            self.delivered.lock().unwrap().push((lane.into(), text.into()));
        }
        async fn deliver_work(&self, lane: &str, text: &str) -> Result<(), String> {
            if let Some(error) = self.delivery_error.lock().unwrap().clone() { return Err(error); }
            self.deliver(lane, text).await;
            Ok(())
        }
        async fn deliver_resume(
            &self,
            lane: &str,
            text: &str,
            delivery_id: &str,
        ) -> Result<ResumeDelivery, String> {
            if let Some(error) = self.delivery_error.lock().unwrap().clone() {
                return Err(error);
            }
            if let Some(disposition) = self.resume_disposition.lock().unwrap().take() {
                self.resume_ids.lock().unwrap().insert(delivery_id.to_string());
                return Ok(disposition);
            }
            if !self.resume_ids.lock().unwrap().insert(delivery_id.to_string()) {
                return Ok(ResumeDelivery::AlreadyDelivered);
            }
            self.deliver(lane, text).await;
            Ok(ResumeDelivery::Queued)
        }
    }

    fn drive_state() -> (tempfile::TempDir, AppState, std::sync::Arc<crate::db::Store>) {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(crate::db::Store::open(&dir.path().join("drive.db")).unwrap());
        let state = AppState { store: store.clone(), started: std::time::Instant::now(), build_hash: "test".into(),
            auth_token: None, reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)) };
        (dir, state, store)
    }
    fn drive_card(store: &std::sync::Arc<crate::db::Store>, id: &str, status: &str, owner: &str, kind: &str) {
        let (id, status, owner, kind) = (id.to_string(), status.to_string(), owner.to_string(), kind.to_string());
        let now = now_f64() as i64;
        store.write(move |conn| {
            // These tests exercise dispatch and compensation, so the fixture
            // must satisfy the continuation contract regardless of host prefs.
            conn.execute("INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type,next_action) \
                          VALUES (?1,?1,'SCOPE: work\n- [ ] do it',?2,'lane',?3,?3,?4,?5,'Implement the scoped work')",
                rusqlite::params![id, status, now, owner, kind])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
    }
    fn drive_status(store: &std::sync::Arc<crate::db::Store>, id: &str) -> String {
        store.read().unwrap().query_row("SELECT status FROM issues WHERE id=?1", [id], |r| r.get(0)).unwrap()
    }
    fn drive_events(store: &std::sync::Arc<crate::db::Store>, kind: &str) -> i64 {
        store.read().unwrap().query_row("SELECT COUNT(*) FROM session_events WHERE type=?1", [kind], |r| r.get(0)).unwrap()
    }
    fn drive_claim(store: &std::sync::Arc<crate::db::Store>, id: &str) {
        let id = id.to_string();
        store.write(move |conn| {
            conn.execute(
                "INSERT INTO session_events (ts,session,type,data,source) VALUES (?1,'lane','task.claimed',?2,'test')",
                rusqlite::params![now_f64(), json!({"issue": id, "status": "doing"}).to_string()],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
    }
    fn drive_started(store: &std::sync::Arc<crate::db::Store>) {
        store.write(|conn| {
            conn.execute(
                "INSERT INTO session_events (ts,session,type,data,source) \
                 VALUES (?1,'lane','session.started','{}','test')",
                rusqlite::params![now_f64()],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
    }

    #[tokio::test]
    async fn atomic_claim_rolls_back_when_the_causal_marker_is_rejected() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "ATOMIC", "todo", "agent", "code");
        store.write(|conn| {
            conn.execute_batch("CREATE TRIGGER reject_marker BEFORE INSERT ON session_events \
                WHEN NEW.type='task.claimed' BEGIN SELECT RAISE(ABORT, 'marker'); END;")?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        assert!(!claim_card(&state, "lane", "ATOMIC").await);
        assert_eq!(drive_status(&store, "ATOMIC"), "todo");
        assert_eq!(drive_events(&store, "task.claimed"), 0);
    }

    #[tokio::test]
    // The standard mutex deliberately serializes process-global test settings
    // for the complete asynchronous dispatch, so the environment cannot change
    // between selection and delivery.
    #[allow(clippy::await_holding_lock)]
    async fn auto_drain_starts_only_the_eligible_backlog_card_with_an_atomic_claim() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        std::env::remove_var(DISPATCH_BACKLOG_KEY);
        let (_dir, state, store) = drive_state();
        drive_card(&store, "READY", "backlog", "agent", "code");
        drive_card(&store, "HUMAN", "backlog", "human", "code");
        drive_card(&store, "WATCH", "backlog", "agent", "watch");
        let fleet = BoundaryFleet::default();
        fleet.running.store(false, std::sync::atomic::Ordering::SeqCst);
        let trace = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(trace.outcome, "assigned", "{trace:?}");
        assert_eq!(trace.card.as_deref(), Some("READY"));
        assert_eq!(drive_status(&store, "READY"), "doing");
        assert_eq!(drive_status(&store, "HUMAN"), "backlog");
        assert_eq!(drive_status(&store, "WATCH"), "backlog");
        assert_eq!(drive_events(&store, "task.claimed"), 1);
        assert_eq!(fleet.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        let delivered = fleet.delivered.lock().unwrap();
        assert_eq!(delivered.len(), 1);
        assert!(delivered[0].1.contains("READY"));
    }

    #[tokio::test]
    async fn idle_worker_receives_next_card_without_restart() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "IDLE", "todo", "agent", "code");
        let fleet = BoundaryFleet::default();
        let trace = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(trace.outcome, "assigned", "{trace:?}");
        assert_eq!(drive_status(&store, "IDLE"), "doing");
        assert_eq!(fleet.starts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(fleet.delivered.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn start_failure_leaves_no_false_doing_or_working_marker() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "STARTFAIL", "todo", "agent", "code");
        let fleet = BoundaryFleet::default();
        fleet.running.store(false, std::sync::atomic::Ordering::SeqCst);
        *fleet.start_error.lock().unwrap() = Some("provider rejected restart".into());
        let trace = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(trace.reason, "start-failed");
        assert_eq!(drive_status(&store, "STARTFAIL"), "todo");
        assert_eq!(drive_events(&store, "task.claimed"), 0);
    }

    #[tokio::test]
    async fn delivery_failure_and_vanished_worker_compensate_claims() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "REFUSED", "todo", "agent", "code");
        let fleet = BoundaryFleet::default();
        *fleet.delivery_error.lock().unwrap() = Some("worker vanished before queue insert".into());
        let trace = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(trace.reason, "delivery-failed-released");
        assert_eq!(drive_status(&store, "REFUSED"), "todo");
        assert_eq!(drive_events(&store, "task.claimed"), 1);
        assert_eq!(drive_events(&store, "task.released"), 1);
        assert!(fleet.delivered.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stopped_exact_live_claim_restarts_and_resumes_same_card() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "RESUME", "doing", "agent", "code");
        // This unrelated Doing row fills the ordinary WIP cap. It must not
        // prevent recovery of the exact causal claim.
        drive_card(&store, "STALE-OTHER", "doing", "agent", "code");
        drive_claim(&store, "RESUME");
        let fleet = BoundaryFleet::default();
        fleet.running.store(false, std::sync::atomic::Ordering::SeqCst);
        fleet.boundary.store(false, std::sync::atomic::Ordering::SeqCst);

        let trace = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(trace.outcome, "resumed", "{trace:?}");
        assert_eq!(trace.card.as_deref(), Some("RESUME"));
        assert_eq!(drive_status(&store, "RESUME"), "doing");
        assert_eq!(drive_events(&store, "task.claimed"), 1, "resume must not create a second claim");
        assert_eq!(drive_events(&store, "task.resumed"), 1);
        assert_eq!(fleet.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        {
            let delivered = fleet.delivered.lock().unwrap();
            assert_eq!(delivered.len(), 1);
            assert!(delivered[0].1.contains("RESUME"));
            assert!(delivered[0].1.contains("resume this still-owned task"));
        }

        // Another idle sweep in the SAME worker generation preserves the
        // exact runtime card but must not enqueue or record another resume.
        fleet.boundary.store(true, std::sync::atomic::Ordering::SeqCst);
        let repeated = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(repeated.reason, "active-claim-current", "{repeated:?}");
        assert_eq!(fleet.delivered.lock().unwrap().len(), 1);
        assert_eq!(drive_events(&store, "task.resumed"), 1);
        assert_eq!(drive_events(&store, "task.resume_suppressed"), 1);
    }

    #[tokio::test]
    async fn running_exact_claim_beats_unrelated_doing_without_resteering_owner() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "EXACT", "doing", "agent", "code");
        drive_card(&store, "SUBAGENT", "doing", "agent", "code");
        drive_card(&store, "STALE", "doing", "agent", "code");
        drive_claim(&store, "EXACT");
        let fleet = BoundaryFleet::default();

        let trace = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(trace.reason, "active-claim-current", "{trace:?}");
        assert_eq!(trace.card.as_deref(), Some("EXACT"));
        assert!(fleet.delivered.lock().unwrap().is_empty());
        assert_eq!(drive_events(&store, "task.resumed"), 0);
        assert_eq!(drive_events(&store, "task.resume_suppressed"), 1);
    }

    #[tokio::test]
    async fn live_fleet_boundary_reuses_its_tick_snapshot_and_structured_truth() {
        let (_dir, state, _) = drive_state();
        let mut signals = crate::api::sessions_legacy::tests::signals();
        signals.running.insert("amux-lane".into());
        signals.started.insert("lane".into(), signals.now - 7200.0);
        signals.codex_turns.insert("lane".into(), crate::api::session_verbs::CodexTurnSignal {
            state: "active".into(), ts: signals.now - 3600.0,
            heartbeat_ts: signals.now - 3500.0, boundary: "task_started".into(),
            rollout_file: None,
        });
        let mut fleet = LiveFleet { state, signals: Some(signals) };
        // There is no real lane/pane in this fixture. A per-lane reload or the
        // old pane-only gate would refuse, instead of using the tick snapshot.
        assert!(fleet.at_boundary("lane").await);
        assert!(fleet.at_boundary("lane").await);
        fleet.signals.as_mut().unwrap().provider_child_activity.insert("lane".into());
        assert!(!fleet.at_boundary("lane").await);
        assert!(!fleet.at_boundary("unknown-worker").await);
    }

    #[tokio::test]
    async fn current_claim_advances_once_then_obeys_cooldown_across_repeated_ticks() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "CURRENT", "doing", "agent", "code");
        drive_claim(&store, "CURRENT");
        store.write(|conn| {
            conn.execute("UPDATE session_events SET ts=ts-?1 WHERE type='task.claimed'", [ADVANCE_COOLDOWN_S + 1.0])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let fleet = BoundaryFleet::default();
        let trace = drive_lane(&state, &fleet, "lane").await;
        assert_ne!(trace.outcome, "skipped", "Current must reach canonical advancement: {trace:?}");
        assert_eq!(trace.card.as_deref(), Some("CURRENT"));
        assert_eq!(drive_events(&store, "advance.nudged"), 1);
        assert_eq!(drive_events(&store, "task.resumed"), 0);
        for _ in 0..3 {
            let trace = drive_lane(&state, &fleet, "lane").await;
            assert_eq!(trace.reason, "active-claim-current", "{trace:?}");
            assert!(trace.detail.contains("cooldown"), "{trace:?}");
        }
        assert_eq!(fleet.delivered.lock().unwrap().len(), 1);
        assert_eq!(drive_events(&store, "task.resume_suppressed"), 1);
    }

    #[tokio::test]
    async fn conflicting_exact_claims_are_given_to_the_model_not_a_human() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "FIRST", "doing", "agent", "code");
        drive_card(&store, "SECOND", "doing", "agent", "code");
        drive_claim(&store, "FIRST");
        drive_claim(&store, "SECOND");
        let fleet = BoundaryFleet::default();

        let trace = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(trace.outcome, "claim-reconcile", "{trace:?}");
        assert!(trace.detail.contains("FIRST") && trace.detail.contains("SECOND"), "{trace:?}");
        {
            let delivered = fleet.delivered.lock().unwrap();
            assert_eq!(delivered.len(), 1, "one stable reconciliation request");
            assert!(delivered[0].1.contains("FIRST") && delivered[0].1.contains("SECOND"));
            assert!(delivered[0].1.contains("not a human decision"));
            assert!(delivered[0].1.contains("Keep exactly one"));
        }
        assert_eq!(drive_events(&store, "task.resumed"), 0);
        assert_eq!(drive_events(&store, "task.claim_reconcile_requested"), 1);

        let repeated = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(repeated.reason, "claim-reconcile-cooldown", "{repeated:?}");
        assert_eq!(fleet.delivered.lock().unwrap().len(), 1, "cooldown prevents duplicate queue rows");
        assert_eq!(drive_events(&store, "task.claim_reconcile_requested"), 1);

        store.write(|conn| {
            conn.execute(
                "UPDATE session_events SET ts=ts-?1 WHERE session='lane'",
                [CLAIM_RECONCILE_BASE_S + 1.0],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let retried = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(retried.outcome, "claim-reconcile", "{retried:?}");
        assert_eq!(fleet.delivered.lock().unwrap().len(), 2, "an unresolved conflict is retried after its backoff");
        assert_eq!(drive_events(&store, "task.claim_reconcile_requested"), 2);
    }

    #[tokio::test]
    async fn stopped_worker_is_woken_to_reconcile_conflicting_claims() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "FIRST", "doing", "agent", "code");
        drive_card(&store, "SECOND", "doing", "agent", "code");
        drive_claim(&store, "FIRST");
        drive_claim(&store, "SECOND");
        let fleet = BoundaryFleet::default();
        fleet.running.store(false, std::sync::atomic::Ordering::SeqCst);
        fleet.boundary.store(false, std::sync::atomic::Ordering::SeqCst);

        assert_eq!(
            exact_resume_card(&store.read().unwrap(), "lane").unwrap(),
            None,
            "a conflict has no singular launch card; it is not a launch error"
        );
        let trace = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(trace.outcome, "claim-reconcile", "{trace:?}");
        assert_eq!(fleet.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(fleet.delivered.lock().unwrap().len(), 1);
        assert_eq!(drive_events(&store, "task.claim_reconcile_requested"), 1);
    }

    #[tokio::test]
    async fn failed_claim_reconciliation_delivery_retries_next_tick() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "FIRST", "doing", "agent", "code");
        drive_card(&store, "SECOND", "doing", "agent", "code");
        drive_claim(&store, "FIRST");
        drive_claim(&store, "SECOND");
        let fleet = BoundaryFleet::default();
        *fleet.delivery_error.lock().unwrap() = Some("queue offline".into());

        let failed = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(failed.reason, "claim-reconcile-delivery-failed", "{failed:?}");
        assert_eq!(drive_events(&store, "task.claim_reconcile_requested"), 0);
        assert_eq!(drive_events(&store, "task.claim_reconcile_failed"), 1);

        *fleet.delivery_error.lock().unwrap() = None;
        let retried = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(retried.outcome, "claim-reconcile", "{retried:?}");
        assert_eq!(fleet.delivered.lock().unwrap().len(), 1);
        assert_eq!(drive_events(&store, "task.claim_reconcile_requested"), 1);
    }

    #[tokio::test]
    async fn active_worker_does_not_receive_a_duplicate_resume_delivery() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "ACTIVE", "doing", "agent", "code");
        drive_claim(&store, "ACTIVE");
        let fleet = BoundaryFleet::default();
        fleet.boundary.store(false, std::sync::atomic::Ordering::SeqCst);

        let trace = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(trace.reason, "mid-turn", "{trace:?}");
        assert!(fleet.delivered.lock().unwrap().is_empty());
        assert_eq!(drive_events(&store, "task.resumed"), 0);
    }

    #[tokio::test]
    async fn live_child_restores_its_exact_parked_todo_claim_without_redelivery() {
        let (_dir, state, store) = drive_state();
        // Exact live specimen: the parent claimed the task, then parked the
        // row in To Do while a delegated agent kept running.
        drive_card(&store, "CHILD-1", "todo", "agent", "code");
        drive_claim(&store, "CHILD-1");
        let fleet = BoundaryFleet::default();
        fleet.boundary.store(false, std::sync::atomic::Ordering::SeqCst);
        fleet.active_child.store(true, std::sync::atomic::Ordering::SeqCst);

        let trace = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(trace.reason, "active-child-claim-restored", "{trace:?}");
        assert_eq!(trace.card.as_deref(), Some("CHILD-1"));
        assert_eq!(drive_status(&store, "CHILD-1"), "doing");
        assert_eq!(drive_events(&store, "task.claimed"), 2);
        assert!(fleet.delivered.lock().unwrap().is_empty());

        // The next tick observes the now-correct Doing state and is a no-op.
        let repeated = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(repeated.reason, "active-child-claim-current", "{repeated:?}");
        assert_eq!(drive_events(&store, "task.claimed"), 2);
    }

    #[tokio::test]
    async fn live_child_never_revives_a_prior_task_across_a_newer_cardless_turn() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "OLD-1", "todo", "agent", "code");
        drive_claim(&store, "OLD-1");
        store.write(|conn| {
            conn.execute(
                "INSERT INTO session_events (ts,session,type,data,source) \
                 VALUES (?1,'lane','task.cardless',?2,'prompt-capture')",
                rusqlite::params![
                    now_f64() + 0.001,
                    json!({"reason": "informational-query"}).to_string(),
                ],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let fleet = BoundaryFleet::default();
        fleet.boundary.store(false, std::sync::atomic::Ordering::SeqCst);
        fleet.active_child.store(true, std::sync::atomic::Ordering::SeqCst);

        let trace = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(trace.reason, "mid-turn", "{trace:?}");
        assert_eq!(drive_status(&store, "OLD-1"), "todo");
        assert_eq!(drive_events(&store, "task.claimed"), 1);
    }

    #[tokio::test]
    async fn resume_delivery_failure_keeps_exact_claim_for_truthful_retry() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "RETRY", "doing", "agent", "code");
        drive_claim(&store, "RETRY");
        let fleet = BoundaryFleet::default();
        fleet.running.store(false, std::sync::atomic::Ordering::SeqCst);
        *fleet.delivery_error.lock().unwrap() = Some("worker vanished before queue insert".into());

        let failed = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(failed.reason, "resume-delivery-failed", "{failed:?}");
        assert_eq!(drive_status(&store, "RETRY"), "doing");
        assert_eq!(drive_events(&store, "task.claimed"), 1);
        assert_eq!(drive_events(&store, "task.resume_failed"), 1);
        assert_eq!(drive_events(&store, "task.resumed"), 0);
        assert!(fleet.delivered.lock().unwrap().is_empty());

        *fleet.delivery_error.lock().unwrap() = None;
        let retried = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(retried.outcome, "resumed", "{retried:?}");
        assert_eq!(retried.card.as_deref(), Some("RETRY"));
        assert_eq!(drive_events(&store, "task.claimed"), 1, "retry must preserve the old claim");
        assert_eq!(drive_events(&store, "task.resumed"), 1);
    }

    #[tokio::test]
    async fn a_new_worker_generation_gets_exactly_one_new_recovery_delivery() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "RESTART", "doing", "agent", "code");
        drive_claim(&store, "RESTART");
        let fleet = BoundaryFleet::default();
        fleet.running.store(false, std::sync::atomic::Ordering::SeqCst);

        let first = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(first.outcome, "resumed", "{first:?}");
        assert_eq!(fleet.delivered.lock().unwrap().len(), 1);

        // This durable lifecycle event is emitted by the canonical launcher in
        // production. It is the causal boundary, so a worker restart recovers
        // the same Doing card once even across a server-process restart.
        drive_started(&store);
        let restarted = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(restarted.outcome, "resumed", "{restarted:?}");
        assert_eq!(fleet.delivered.lock().unwrap().len(), 2);
        assert_eq!(drive_events(&store, "task.resumed"), 2);

        let repeated = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(repeated.reason, "active-claim-current", "{repeated:?}");
        assert_eq!(fleet.delivered.lock().unwrap().len(), 2);
        assert_eq!(drive_events(&store, "task.resumed"), 2);
    }

    #[tokio::test]
    async fn startup_and_board_drive_share_one_durable_resume_per_generation_and_retry_failure() {
        use crate::api::session_verbs as sv;
        let (dir, state, store) = drive_state();
        let _home = crate::api::settings::test_env::set_home(dir.path());
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("lane.env"), format!("CC_DIR=\"{}\"\nCC_ISOLATED=0\n", dir.path().display())).unwrap();
        drive_card(&store, "ATE-92", "doing", "agent", "code");
        drive_claim(&store, "ATE-92");
        let context = sv::StructuredResumeContext { session: "lane".into(), card: Some("ATE-92".into()), cwd: dir.path().to_string_lossy().into() };
        for generation_number in 1..=2 {
            let token = format!("swap-{generation_number}");
            let pending = json!({"cc_cwd": context.cwd, "pending_structured_resume": 1,
                "pending_structured_resume_context": &context, "pending_structured_resume_token": token});
            let meta_path = sessions.join("lane.meta.json");
            // Use the same meta path shape as the launcher.
            std::fs::write(&meta_path, pending.to_string()).unwrap();
            let start_data = json!({"resume_context": &context, "resume_token": token, "resume_reason": "provider swap"}).to_string();
            store.write(move |conn| {
                conn.execute("INSERT INTO session_events(ts,session,type,data) VALUES (?1,'lane','session.started',?2)",
                    rusqlite::params![now_f64(), start_data])?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            }).unwrap();
            let generation: i64 = store.read().unwrap().query_row("SELECT MAX(id) FROM session_events WHERE type='session.started'", [], |r| r.get(0)).unwrap();
            let Resume::Claim { prompt, delivery_id, .. } = select_resume(&store.read().unwrap(), "lane", now_f64(), false) else { panic!("new generation needs recovery") };
            assert!(prompt.contains(&context.cwd) && prompt.contains("amux board show ATE-92"));
            assert!(!prompt.contains("board ls --session") && !prompt.contains(".amux/logs"));
            if generation_number == 1 {
                store.write(|conn| { conn.execute_batch("CREATE TRIGGER refuse_resume BEFORE INSERT ON steering_queue BEGIN SELECT RAISE(ABORT,'injected queue failure'); END;")?;
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] }) }).unwrap();
                assert!(sv::enqueue_generation_resume(&state, &context, generation, "provider swap").await.is_err());
                assert_eq!(std::fs::read_to_string(&meta_path).unwrap(), pending.to_string());
                assert_eq!(drive_events(&store, "task.resumed"), 0);
                store.write(|conn| { conn.execute_batch("DROP TRIGGER refuse_resume;")?;
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] }) }).unwrap();
            }
            // Startup and a board tick selected before either producer commits.
            let (startup, board) = tokio::join!(
                sv::enqueue_generation_resume(&state, &context, generation, "provider swap"),
                sv::steer_enqueue_idempotent_report(&state, "lane", &prompt, sv::BOARD_DRIVE_GUARD, "", &delivery_id),
            );
            let startup = startup.unwrap();
            let board = board.unwrap();
            assert_eq!(startup.id, board.id);
            assert_ne!(startup.disposition == sv::StableEnqueueDisposition::New,
                       board.disposition == sv::StableEnqueueDisposition::New);
            let queued: i64 = store.read().unwrap().query_row("SELECT COUNT(*) FROM steering_queue WHERE id=?1", [&delivery_id], |r| r.get(0)).unwrap();
            assert_eq!(queued, 1);
            let accepted: Value = serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
            assert!(accepted.get("pending_structured_resume_context").is_none(), "accepted queue commit consumes only its own pending token: {accepted}");
            for _ in 0..3 {
                let again = sv::enqueue_generation_resume(&state, &context, generation, "provider swap").await.unwrap();
                assert_eq!(again.disposition, sv::StableEnqueueDisposition::AlreadyQueued);
                assert!(matches!(select_resume(&store.read().unwrap(), "lane", now_f64(), false), Resume::Current { .. }));
            }
            // Persist delivery, then reopen the DB to simulate a server/model
            // process losing every in-memory receipt.
            let delivered_id = delivery_id.clone();
            store.write(move |conn| {
                conn.execute("INSERT INTO steering_history(id,session,text,queued_at,delivered_at) SELECT id,session,text,queued_at,?1 FROM steering_queue WHERE id=?2", rusqlite::params![now_f64(), delivered_id])?;
                conn.execute("DELETE FROM steering_queue WHERE id=?1", [&delivered_id])?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            }).unwrap();
            let mut reopened_state = state.clone();
            reopened_state.store = std::sync::Arc::new(crate::db::Store::open(&dir.path().join("drive.db")).unwrap());
            let recovered = sv::enqueue_generation_resume(&reopened_state, &context, generation, "provider swap").await.unwrap();
            assert_eq!(recovered.disposition, sv::StableEnqueueDisposition::AlreadyDelivered);
            assert_eq!(drive_events(&store, "task.resumed"), generation_number);
        }
    }

    #[tokio::test]
    async fn crash_after_durable_enqueue_recovers_receipt_without_redelivery() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "CRASH", "doing", "agent", "code");
        drive_claim(&store, "CRASH");
        let fleet = BoundaryFleet::default();
        fleet.running.store(false, std::sync::atomic::Ordering::SeqCst);
        *fleet.resume_disposition.lock().unwrap() = Some(ResumeDelivery::AlreadyDelivered);

        // Simulate a prior process which durably committed/delivered the stable
        // steering id, then died before appending task.resumed. The retry must
        // restore that causal receipt without handing the prompt to the lane.
        let recovered = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(recovered.reason, "resume-commit-recovered", "{recovered:?}");
        assert!(fleet.delivered.lock().unwrap().is_empty());
        assert_eq!(drive_events(&store, "task.resumed"), 1);

        let repeated = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(repeated.reason, "active-claim-current", "{repeated:?}");
        assert!(fleet.delivered.lock().unwrap().is_empty());
        assert_eq!(drive_events(&store, "task.resumed"), 1);
    }

    #[tokio::test]
    async fn protected_exact_claims_never_wake_a_stopped_worker() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "WATCH", "doing", "agent", "watch");
        drive_card(&store, "EPIC", "doing", "agent", "epic");
        drive_card(&store, "HUMAN", "doing", "human", "code");
        drive_card(&store, "TRIGGER", "doing", "agent", "code");
        drive_card(&store, "BLOCKED", "doing", "agent", "code");
        drive_card(&store, "DEP", "doing", "agent", "code");
        for id in ["WATCH", "EPIC", "HUMAN", "TRIGGER", "BLOCKED"] {
            drive_claim(&store, id);
        }
        store.write(|conn| {
            let now = now_f64() as i64;
            conn.execute("UPDATE issues SET source_ref='watch:condition', last_verified_at=?1 WHERE id='TRIGGER'", [now])?;
            conn.execute("UPDATE issues SET session='other' WHERE id='DEP'", [])?;
            conn.execute("UPDATE issues SET depends_on='[\"DEP\"]' WHERE id='BLOCKED'", [])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let fleet = BoundaryFleet::default();
        fleet.running.store(false, std::sync::atomic::Ordering::SeqCst);

        let trace = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(trace.reason, "not-running-no-dispatchable-work", "{trace:?}");
        assert_eq!(fleet.starts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(fleet.delivered.lock().unwrap().is_empty());
    }

    #[tokio::test]
    // See `auto_drain_starts_only_the_eligible_backlog_card_with_an_atomic_claim`.
    #[allow(clippy::await_holding_lock)]
    async fn backlog_delivery_failure_compensates_back_to_backlog() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        std::env::remove_var(DISPATCH_BACKLOG_KEY);
        let (_dir, state, store) = drive_state();
        drive_card(&store, "BACKLOG-REFUSED", "backlog", "agent", "code");
        let fleet = BoundaryFleet::default();
        *fleet.delivery_error.lock().unwrap() = Some("steering target is archived".into());
        let trace = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(trace.reason, "delivery-failed-released");
        assert_eq!(drive_status(&store, "BACKLOG-REFUSED"), "backlog");
        assert_eq!(drive_events(&store, "task.claimed"), 1);
        assert_eq!(drive_events(&store, "task.released"), 1);
        assert_eq!(drive_events(&store, "backlog.drained"), 0);
    }

    #[tokio::test]
    // See `auto_drain_starts_only_the_eligible_backlog_card_with_an_atomic_claim`.
    #[allow(clippy::await_holding_lock)]
    async fn blocked_todo_with_eligible_backlog_wakes_exact_backlog_card() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        std::env::remove_var(DISPATCH_BACKLOG_KEY);
        let (_dir, state, store) = drive_state();
        drive_card(&store, "DEP", "doing", "agent", "code");
        drive_card(&store, "BLOCKED", "todo", "agent", "code");
        drive_card(&store, "BACKLOG", "backlog", "agent", "code");
        store.write(|conn| {
            conn.execute("UPDATE issues SET session='other' WHERE id='DEP'", [])?;
            conn.execute("UPDATE issues SET depends_on='[\"DEP\"]' WHERE id='BLOCKED'", [])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let fleet = BoundaryFleet::default();
        fleet.running.store(false, std::sync::atomic::Ordering::SeqCst);
        let trace = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(trace.card.as_deref(), Some("BACKLOG"));
        assert_eq!(drive_status(&store, "BACKLOG"), "doing");
        assert_eq!(drive_status(&store, "BLOCKED"), "todo");
    }

    /// The live TubeScience topology: TUBES-2459 was Doing while both its
    /// `blocked_on` text and TUBES-2496 dependency said it could not proceed.
    /// It consumed WIP, appeared as current, and left independent queue work
    /// waiting for a human nudge. The sweep must durably park it first; the
    /// same tick can then dispatch eligible work without re-steering the block.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn blocked_tubescience_doing_is_parked_and_independent_work_keeps_draining() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        let (_dir, state, store) = drive_state();
        drive_card(&store, "TUBES-2459", "doing", "agent", "code");
        drive_card(&store, "TUBES-2496", "backlog", "agent", "code");
        drive_card(&store, "TUBES-NEXT", "todo", "agent", "code");
        drive_claim(&store, "TUBES-2459");
        store
            .write(|conn| {
                conn.execute(
                    "UPDATE issues SET depends_on='[\"TUBES-2496\"]', \
                         blocked_on='writer stopped at cursor 429000 pending renewed clearance' \
                     WHERE id='TUBES-2459'",
                    [],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();

        // Defence in depth: even before the lifecycle writer runs, the stale
        // Doing label cannot hold WIP against independent work.
        let before = store.read().unwrap();
        assert_eq!(
            claimed(&select_pickup_with(&before, "lane", now_f64(), false)),
            Some("TUBES-NEXT"),
            "an unfinished dependency must not consume the WIP slot"
        );
        drop(before);

        assert_eq!(normalize_blocked_doing(&state).await, 1);
        assert_eq!(drive_status(&store, "TUBES-2459"), "backlog");
        assert_eq!(drive_events(&store, "task.blocked_normalized"), 1);

        let fleet = BoundaryFleet::default();
        let trace = drive_lane(&state, &fleet, "lane").await;
        assert_eq!(trace.outcome, "assigned", "{trace:?}");
        assert_eq!(trace.card.as_deref(), Some("TUBES-NEXT"), "{trace:?}");
        assert_eq!(drive_status(&store, "TUBES-NEXT"), "doing");
        assert_eq!(drive_status(&store, "TUBES-2496"), "backlog");
        assert_eq!(
            drive_events(&store, "task.resumed"),
            0,
            "the stale blocked claim must not be resumed or advertised current"
        );
    }

    #[tokio::test]
    async fn stale_subagent_and_protected_work_never_wake_or_claim() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "STALE", "todo", "agent", "code");
        let busy = BoundaryFleet::default();
        busy.boundary.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(drive_lane(&state, &busy, "lane").await.reason, "mid-turn");
        assert_eq!(drive_status(&store, "STALE"), "todo");

        let (_dir, state, store) = drive_state();
        drive_card(&store, "HUMAN", "todo", "human", "code");
        drive_card(&store, "EPIC", "backlog", "agent", "epic");
        drive_card(&store, "WATCH", "todo", "agent", "watch");
        drive_card(&store, "TRIGGER", "backlog", "agent", "code");
        drive_card(&store, "DEP", "doing", "agent", "code");
        drive_card(&store, "BLOCKED", "todo", "agent", "code");
        store.write(|conn| {
            let now = now_f64() as i64;
            conn.execute("UPDATE issues SET source_ref='watch', last_verified_at=?1 WHERE id='TRIGGER'", [now])?;
            conn.execute("UPDATE issues SET session='other' WHERE id='DEP'", [])?;
            conn.execute("UPDATE issues SET depends_on='[\"DEP\"]' WHERE id='BLOCKED'", [])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let stopped = BoundaryFleet::default();
        stopped.running.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(drive_lane(&state, &stopped, "lane").await.reason, "not-running-no-dispatchable-work");
        assert_eq!(stopped.starts.load(std::sync::atomic::Ordering::SeqCst), 0);

        let isolated = BoundaryFleet::default();
        isolated.running.store(false, std::sync::atomic::Ordering::SeqCst);
        isolated.isolated.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(drive_lane(&state, &isolated, "lane").await.reason, "isolated");
        let opted = BoundaryFleet::default();
        opted.running.store(false, std::sync::atomic::Ordering::SeqCst);
        opted.enabled.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(drive_lane(&state, &opted, "lane").await.reason, "opted-out");
    }

    #[tokio::test]
    async fn concurrent_tick_claims_once_and_model_provider_restart_uses_start_seam() {
        let (_dir, state, store) = drive_state();
        drive_card(&store, "RACE", "todo", "agent", "code");
        let one = BoundaryFleet::default();
        let two = BoundaryFleet::default();
        let (a, b) = tokio::join!(drive_lane(&state, &one, "lane"), drive_lane(&state, &two, "lane"));
        assert_eq!([a.outcome, b.outcome].iter().filter(|v| **v == "assigned").count(), 1);
        assert_eq!(drive_events(&store, "task.claimed"), 1);
        assert_eq!(one.delivered.lock().unwrap().len() + two.delivered.lock().unwrap().len(), 1);

        let (_dir, state, store) = drive_state();
        drive_card(&store, "RESTART", "todo", "agent", "code");
        let restart = BoundaryFleet::default();
        restart.running.store(false, std::sync::atomic::Ordering::SeqCst);
        restart.boundary.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(drive_lane(&state, &restart, "lane").await.card.as_deref(), Some("RESTART"));
        assert_eq!(restart.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[allow(clippy::too_many_arguments)]
    fn add_card(conn: &Connection, id: &str, session: &str, status: &str, title: &str, desc: &str) {
        let now = now_f64() as i64;
        conn.execute(
            "INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type) \
             VALUES (?1,?2,?3,?4,?5,?6,?6,'agent','code')",
            rusqlite::params![id, title, desc, status, session, now],
        )
        .expect("insert");
    }

    fn tag(conn: &Connection, id: &str, t: &str, added_at: f64) {
        conn.execute(
            "INSERT INTO issue_tags (issue_id, tag, added_at) VALUES (?1,?2,?3)",
            rusqlite::params![id, t, added_at],
        )
        .expect("tag");
    }

    fn claimed(p: &Pickup) -> Option<&str> {
        match p {
            Pickup::Claim { card, .. } => Some(card),
            _ => None,
        }
    }

    #[test]
    fn historical_prose_reaches_each_stranded_worker_for_dependency_recheck() {
        // Real refusal identities from the 48h audit. Keep the text small and
        // exercise selection, not the regex in isolation: its match is a hint.
        for (lane, card, dep) in [
            ("amux-frustrations", "AF-298", "AF-398"),
            ("amux-gtm", "AG-39", "AG-41"),
            ("general-canvas-apps", "GCA-157", "CO-237"),
            ("mixpeek-frustrations", "MF-841", "BACKE-3872"),
            ("mixpeek-security", "MS-1253", "MS-1253"),
            ("mixpeek-security", "MS-1273", "MG-1578"),
            ("mvs-research", "MR-21", "MR-17"),
            ("mvs-research", "MR-112", "MI-5201"),
            ("mvs-research", "MR-148", "BACKE-3836"),
        ] {
            let conn = board_db();
            if dep != card {
                add_card(&conn, dep, "peer", "needsyou", "Owner decision", "SCOPE: decide");
            }
            add_card(&conn, card, lane, "todo", "Recheck remaining work",
                &format!("SCOPE: reconcile current prerequisites. Historically blocked by {dep}."));
            match select_pickup_with(&conn, lane, now_f64(), false) {
                Pickup::Claim { card: picked, prompt } => {
                    assert_eq!(picked, card, "{lane}");
                    assert!(prompt.contains("[dependency recheck]"), "{lane}: {prompt}");
                    assert!(prompt.contains("do not execute work whose prerequisite is unmet"), "{lane}");
                    assert!(prompt.contains("independent ready work"), "{lane}");
                }
                other => panic!("{lane}/{card} must receive a reconciliation turn: {other:?}"),
            }
            if dep != card {
                // The SAME board with an explicit dependency still refuses it.
                conn.execute("UPDATE issues SET depends_on=?1 WHERE id=?2",
                    rusqlite::params![json!([dep]).to_string(), card]).unwrap();
                assert!(matches!(select_pickup_with(&conn, lane, now_f64(), false),
                    Pickup::None { reason: "all-candidates-refused", .. }),
                    "{lane}: an unresolved structured dependency must still block");
            }
        }
    }

    #[test]
    fn stale_reclaim_cools_the_reclaimed_card_so_the_next_card_runs() {
        let conn = board_db();
        let now = now_f64();
        add_card(&conn, "BR-51", "byo-ray", "todo", "Reclaimed old card", "SCOPE: x");
        add_card(&conn, "BR-142", "byo-ray", "todo", "Independent queued work", "SCOPE: y");
        conn.execute("UPDATE issues SET pos=-100 WHERE id='BR-51'", []).unwrap();
        assert_eq!(claimed(&select_pickup_with(&conn, "byo-ray", now, false)), Some("BR-51"),
            "control: without the reclaim event the old card wins");
        conn.execute("INSERT INTO session_events (ts,session,type,data) VALUES (?1,'byo-ray', \
            'pickup.reclaimed_stale','{\"issue\":\"BR-51\"}')", [now]).unwrap();
        assert_eq!(eligible_todo_count(&conn, "byo-ray", now), 1);
        assert_eq!(claimed(&select_pickup_with(&conn, "byo-ray", now, false)), Some("BR-142"),
            "recovery must unblock different work, not immediately reclaim the same card");
        let later = now + reclaim_cooldown_s() + 1.0;
        assert_eq!(eligible_todo_count(&conn, "byo-ray", later), 2);
        assert_eq!(claimed(&select_pickup_with(&conn, "byo-ray", later, false)), Some("BR-51"),
            "reclaimed work stays reachable after its bounded cooldown");
    }

    // ---- pickup scoring (AMUX-3779) --------------------------------------

    /// Full-control insert for scoring tests: pick the type, creation time,
    /// board position and pin. Desc/title are the same non-junk shape the
    /// passing pickup tests use, so the refusal guards let the card through.
    #[allow(clippy::too_many_arguments)]
    fn add_full(
        conn: &Connection,
        id: &str,
        session: &str,
        status: &str,
        item_type: &str,
        created: i64,
        pos: f64,
        pinned: i64,
    ) {
        conn.execute(
            "INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type,pos,pinned) \
             VALUES (?1, 'Work item ' || ?1, 'SCOPE: real work\n- [ ] do it', ?2,?3,?4,?4,'agent',?5,?6,?7)",
            rusqlite::params![id, status, session, created, item_type, pos, pinned],
        )
        .expect("insert full");
    }

    /// A non-terminal card in some OTHER lane that depends on `target` — counts
    /// toward `target`'s critical path without competing as a pickup candidate.
    fn add_dependent(conn: &Connection, id: &str, target: &str) {
        conn.execute(
            "INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type,depends_on) \
             VALUES (?1,?1,'x','todo','other', 0, 0,'agent','code', ?2)",
            rusqlite::params![id, format!("[\"{target}\"]")],
        )
        .expect("insert dependent");
    }

    #[test]
    fn scoring_picks_the_oldest_todo_not_the_newest() {
        // Default (scoring on). Legacy pos-order would pick the newest card,
        // which sits at the top of the column; scoring surfaces the starved one.
        let conn = board_db();
        let now = 1_000_000.0;
        add_full(&conn, "OLD", "lane", "todo", "code", now as i64 - 3 * 86400, -1024.0, 0);
        add_full(&conn, "NEW", "lane", "todo", "code", now as i64, -99999.0, 0); // topmost pos
        let p = select_pickup_with(&conn, "lane", now, false);
        assert_eq!(claimed(&p), Some("OLD"), "the 3-day-old card must be worked before the fresh one");
    }

    #[test]
    fn a_pinned_todo_leads_regardless_of_age() {
        let conn = board_db();
        let now = 1_000_000.0;
        add_full(&conn, "OLD", "lane", "todo", "code", now as i64 - 5 * 86400, -1024.0, 0);
        add_full(&conn, "PIN", "lane", "todo", "code", now as i64, -50.0, 1); // pinned, fresh
        let p = select_pickup_with(&conn, "lane", now, false);
        assert_eq!(claimed(&p), Some("PIN"), "a pinned card is the hard human override");
    }

    #[test]
    fn a_p0_tag_lifts_a_fresh_bug_over_an_older_chore() {
        let conn = board_db();
        let now = 1_000_000.0;
        add_full(&conn, "CHORE", "lane", "todo", "chore", now as i64 - 2 * 86400, -1024.0, 0);
        add_full(&conn, "BUG", "lane", "todo", "bug", now as i64, -50.0, 0);
        tag(&conn, "BUG", "p0", now);
        let p = select_pickup_with(&conn, "lane", now, false);
        assert_eq!(claimed(&p), Some("BUG"), "a fresh p0 bug must outrank a 2-day-old chore");
    }

    #[test]
    fn dependents_lift_a_card_on_the_critical_path() {
        let conn = board_db();
        let now = 1_000_000.0;
        add_full(&conn, "PLAIN", "lane", "todo", "code", now as i64, -99999.0, 0); // topmost
        add_full(&conn, "DEP", "lane", "todo", "code", now as i64, -50.0, 0);
        add_dependent(&conn, "D1", "DEP");
        add_dependent(&conn, "D2", "DEP");
        let p = select_pickup_with(&conn, "lane", now, false);
        assert_eq!(claimed(&p), Some("DEP"), "the card two others depend on must lead");
    }

    #[test]
    fn type_and_tag_weights_are_ordered() {
        assert!(type_weight("blocker") > type_weight("code"));
        assert!(type_weight("bug") > type_weight("doc"));
        assert_eq!(type_weight("chore"), 0);
        assert_eq!(type_weight("unknown-legacy"), 6, "unknown defaults code-like");
        assert!(tag_severity(&["p0".into()]) > tag_severity(&["p1".into()]));
        assert_eq!(tag_severity(&["nope".into()]), 0);
    }

    #[test]
    fn an_eligible_todo_is_selected_and_the_prompt_names_the_card() {
        let conn = board_db();
        add_card(&conn, "T-1", "lane", "todo", "Fix the parser", "SCOPE: real work\n- [ ] do it");
        let p = select_pickup_with(&conn, "lane", now_f64(), false);
        assert_eq!(claimed(&p), Some("T-1"), "an eligible todo must be claimed");
        let Pickup::Claim { prompt, .. } = &p else { unreachable!() };
        assert!(prompt.contains("T-1"), "the prompt must name the card id: {prompt}");
    }

    /// AC-223, hit three times in one session by two different lanes. A card
    /// marked needs:you is BLOCKED ON A HUMAN; handing it to an agent is handing
    /// out a decision its owner has not made.
    #[test]
    fn a_needs_you_card_is_never_picked_up() {
        for t in ["needs:you", "needs:you:decision", "NEEDS:YOU"] {
            let conn = board_db();
            add_card(&conn, "T-1", "lane", "todo", "Ask Ethan about pricing", "SCOPE: x\n- [ ] y");
            tag(&conn, "T-1", t, now_f64());
            let p = select_pickup_with(&conn, "lane", now_f64(), false);
            assert!(claimed(&p).is_none(), "{t} must exempt the card from pickup");
            assert_eq!(eligible_todo_count(&conn, "lane", 1_000_000.0), 0, "{t} must not count as eligible");
        }
    }

    /// AMUX-3006 (Ethan: "fix the backlog thing so it drives on its own"). A lane
    /// holding ONLY backlog with nothing dispatchable in todo is the idle-drain
    /// case — board-drive dispatches `todo`, so that queue never moves on its own.
    /// The discriminator is the drain INPUTS: backlog candidates exist AND
    /// eligible_todo_count is 0. A lane with a todo is dispatched, not drained.
    /// AF-516 — a card BLOCKED with a named cause must not be drained.
    ///
    /// `amux board block <ID> --on <cause>` sets `blocked_on` and deliberately
    /// leaves status alone ("so its lifecycle position survives"), and its own
    /// help promises the card is "Excluded from `amux board ready` until
    /// unblocked". `ready` honoured that field; the drain predicates did not, so
    /// backend's BACKE-3548 was claimed and moved to `doing` with `blocked_on`
    /// still set. Two mechanisms disagreeing about the same field.
    ///
    /// Its own test, not appended to the drain-candidate test: fixtures added
    /// mid-body leak into that test's later assertions, which is how the first
    /// version of this cell broke an unrelated one.
    #[test]
    fn a_card_blocked_on_a_named_cause_is_not_drainable() {
        let conn = board_db();
        let now = now_f64();
        conn.execute(
            "INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type,blocked_on) \
             VALUES ('BL-1','waiting on a peer reply','x','backlog','blk',?1,?1,'agent','code','mvs-infra: partition eviction')",
            rusqlite::params![now as i64],
        )
        .unwrap();
        let listed: Vec<String> =
            backlog_candidates(&conn, "blk", now as i64).into_iter().map(|c| c.0).collect();
        assert!(listed.is_empty(), "the drain NUDGE offered a blocked card: {listed:?}");
        assert!(
            drainable_backlog_ids(&conn, "blk", now).is_empty(),
            "the DRAIN itself offered a blocked card"
        );
        conn.execute(
            "UPDATE issues SET due=date('now') WHERE id='BL-1'",
            [],
        )
        .unwrap();
        let (due_promotions, due_total) = backlog_due_promotions(&conn);
        assert!(
            due_promotions.is_empty(),
            "an arrived date cannot overrule an unsatisfied external blocker: {due_promotions:?}"
        );
        assert_eq!(
            due_total, 0,
            "blocked work is not an eligible revisit candidate"
        );

        // THE CONTROL, and it is what keeps the clause from being a ban on
        // draining: an EMPTY blocked_on is the overwhelmingly common state — 413
        // of 438 backlog cards when measured — and must stay drainable. Without
        // it, excluding on `blocked_on IS NOT NULL` rather than `= ''` would
        // silence the entire drain and still pass the assertions above.
        conn.execute(
            "INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type,blocked_on) \
             VALUES ('BL-2','ordinary parked work','x','backlog','blk',?1,?1,'agent','code','')",
            rusqlite::params![now as i64],
        )
        .unwrap();
        let listed: Vec<String> =
            backlog_candidates(&conn, "blk", now as i64).into_iter().map(|c| c.0).collect();
        assert_eq!(listed, vec!["BL-2".to_string()], "an unblocked card must still drain");
        assert_eq!(drainable_backlog_ids(&conn, "blk", now), vec!["BL-2".to_string()]);
    }

    #[test]
    fn delivered_capture_is_not_redispatched_after_its_trigger_ages() {
        let conn = board_db();
        add_card(&conn, "DELIVERED", "lane", "backlog", "Owner follow-up", "SCOPE: already delivered");
        add_card(&conn, "ORDINARY", "lane", "backlog", "Independent work", "SCOPE: ready to dispatch");
        conn.execute("UPDATE issues SET source='capture',source_ref='Already delivered; claim explicitly',last_verified_at=1 WHERE id='DELIVERED'", []).unwrap();
        assert_eq!(drainable_backlog_ids(&conn, "lane", now_f64()), vec!["ORDINARY".to_string()],
            "an aged observation must not replay a delivered owner prompt; ordinary backlog remains eligible");
    }

    #[test]
    fn a_lane_with_only_backlog_is_a_drain_candidate_a_lane_with_a_todo_is_not() {
        let conn = board_db();
        let now = now_f64();
        // The tubescience shape: fresh backlog, nothing in todo/doing.
        add_card(&conn, "B-1", "lane", "backlog", "fresh batch item one", "SCOPE: x");
        add_card(&conn, "B-2", "lane", "backlog", "fresh batch item two", "SCOPE: x");
        assert_eq!(
            eligible_todo_count(&conn, "lane", 1_000_000.0),
            0,
            "a backlog-only lane has nothing dispatchable — this is what makes it a drain, not a pickup"
        );
        let cands = backlog_candidates(&conn, "lane", now as i64);
        assert_eq!(cands.len(), 2, "both backlog cards are drain candidates: {cands:?}");
        let text = backlog_drain_text(&cands, 2);
        assert!(
            text.contains("idle") && text.contains("B-1") && text.contains("backlog"),
            "the drain prompt must name the stall and a concrete card: {text}"
        );

        // Control that proves the discriminator can fail: a todo card is
        // DISPATCHED (eligible > 0), so the lane is never in the drain branch.
        add_card(&conn, "T-1", "lane2", "todo", "actionable", "SCOPE: x\n- [ ] y");
        assert!(
            eligible_todo_count(&conn, "lane2", 1_000_000.0) > 0,
            "a lane with a todo is dispatched by the normal path, not drained"
        );

        // CORRECTLY-PARKED backlog is NOT drainable (mixpeek-autopilot, AMUX-3006):
        // a standing tripwire (fires on a condition, no honest path to todo) and a
        // card armed on a FRESH (recently re-verified) source_ref trigger must NOT
        // appear as drain candidates — nudging them churns a compliant lane with no
        // exit but a false close. Add both to a fresh lane whose ONLY other cards
        // are parked. CH-1's last_verified_at is `now`, i.e. re-confirmed today.
        conn.execute(
            "INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type) \
             VALUES ('TW-1','burn>=90% page watch','x','backlog','parked',?1,?1,'agent','tripwire')",
            rusqlite::params![now as i64],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type,source_ref,last_verified_at) \
             VALUES ('CH-1','chore blocked on a credential','x','backlog','parked',?1,?1,'agent','chore','clickhouse://blocked',?1)",
            rusqlite::params![now as i64],
        )
        .unwrap();
        let parked = backlog_candidates(&conn, "parked", now as i64);
        assert!(
            parked.is_empty(),
            "a tripwire and a FRESHLY-verified source_ref-triggered card are correctly parked, not drainable: {parked:?}"
        );


        // SOURCE_REF_STALE_S: a trigger nobody has re-checked in 24h+ is NOT a
        // live park — it must return as a drain candidate exactly like an
        // unset source_ref would (AMUX-2984 shape: a card sat 56 days behind a
        // `--trigger` nobody ever refreshed, invisible to this nudge the whole
        // time). CH-2 carries a source_ref but NO last_verified_at at all
        // (NULL — never even stamped once), which must count as stale, not live.
        conn.execute(
            "INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type,source_ref) \
             VALUES ('CH-2','self-noted, not an external trigger','x','backlog','parked',?1,?1,'agent','chore','do this later when convenient')",
            rusqlite::params![now as i64],
        )
        .unwrap();
        let parked_with_stale = backlog_candidates(&conn, "parked", now as i64);
        assert_eq!(
            parked_with_stale.iter().map(|(id, _, _)| id.as_str()).collect::<Vec<_>>(),
            vec!["CH-2"],
            "a source_ref never re-verified must drain like an unset one, and CH-1/TW-1 must stay excluded: {parked_with_stale:?}"
        );
    }

    /// AMUX-3005: an EPIC is a container, not a work unit — its children carry
    /// the work — so it must never be dispatched (auto-picked) or drained. The
    /// epic AMUX-3005 was auto-claimed as if it were a task; `epic` now joins
    /// tripwire/watch in the exclusion. Control first (a `code` card on the same
    /// lane IS dispatchable) so the assertion cannot pass vacuously.
    #[test]
    fn an_epic_is_never_dispatched_or_drained() {
        let conn = board_db();
        let now = now_f64();
        // Control: a real code todo on the epic's lane IS dispatchable.
        add_card(&conn, "C-1", "elane", "todo", "real work", "SCOPE: x\n- [ ] y");
        assert!(
            eligible_todo_count(&conn, "elane", 1_000_000.0) > 0,
            "control: a code todo must be dispatchable, or this test proves nothing"
        );
        // An epic in TODO is not dispatchable — the count ignores it, so the lane
        // is dispatched on C-1 alone, never handed the epic.
        conn.execute(
            "INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type) \
             VALUES ('EP-1','[EPIC] command center','x','todo','elane',?1,?1,'agent','epic')",
            rusqlite::params![now as i64],
        )
        .unwrap();
        assert_eq!(
            eligible_todo_count(&conn, "elane", 1_000_000.0),
            1,
            "the epic must NOT add to the dispatchable count — only C-1 is"
        );
        // An epic in BACKLOG is not a drain candidate on an otherwise-empty lane.
        conn.execute(
            "INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type) \
             VALUES ('EP-2','[EPIC] other','x','backlog','elane2',?1,?1,'agent','epic')",
            rusqlite::params![now as i64],
        )
        .unwrap();
        assert!(
            backlog_candidates(&conn, "elane2", now as i64).is_empty(),
            "an epic in backlog is a container, not drainable work"
        );
    }

    /// Resolution follows the existing type-specific verification boundary.
    #[test]
    fn dependencies_require_verified_completion_for_runtime_changes() {
        assert!(bs::dependency_is_resolved("done", "chore"));
        assert!(bs::dependency_is_resolved("done", "doc"));
        assert!(bs::dependency_is_resolved("verified", "code"));
        for item_type in ["code", "ops", "blocker", "tripwire"] {
            assert!(!bs::dependency_is_resolved("done", item_type), "{item_type} still needs verification");
        }
        for status in ["doing", "review", "discarded", "quarantined", "backlog"] {
            assert!(!bs::dependency_is_resolved(status, "chore"), "{status} is not successful resolution");
        }
    }

    #[test]
    fn backlog_promotes_only_when_every_dependency_is_terminal() {
        let conn = board_db();
        // Dependencies in the states that matter.
        let dep = |id: &str, status: &str| {
            conn.execute(
                "INSERT INTO issues (id,title,status,session,owner_type,type,updated,created) \
                 VALUES (?1,?1,?2,'me','agent','code',100,100)",
                rusqlite::params![id, status],
            )
            .expect("dep");
        };
        dep("A-done", "done");
        conn.execute("UPDATE issues SET type='chore' WHERE id='A-done'", []).unwrap();
        dep("A-code-done", "done");
        dep("A-discarded", "discarded");
        dep("A-verified", "verified");
        dep("A-open", "doing");

        // Parked backlog cards of every shape (owner_type='agent').
        let parked = |id: &str, typ: &str, deps: &str| {
            conn.execute(
                "INSERT INTO issues (id,title,status,session,owner_type,type,depends_on,updated,created) \
                 VALUES (?1,?1,'backlog','me','agent',?2,?3,100,100)",
                rusqlite::params![id, typ, deps],
            )
            .expect("parked");
        };
        parked("P-all-terminal", "code", r#"["A-done","A-verified"]"#); // promote
        parked("P-unverified", "code", r#"["A-code-done"]"#);
        parked("P-discarded", "code", r#"["A-discarded"]"#);
        parked("P-one-open", "code", r#"["A-done","A-open"]"#); // NOT: one dep still open
        parked("P-missing", "code", r#"["A-done","GONE"]"#); // NOT: a dep resolves to nothing
        parked("W-watch", "watch", r#"["A-done"]"#); // NOT: dormant type
        parked("E-epic", "epic", r#"["A-verified"]"#); // NOT: container type
        // A triggers-only park: NULL depends_on, terminal-deps irrelevant.
        conn.execute(
            "INSERT INTO issues (id,title,status,session,owner_type,type,updated,created) \
             VALUES ('P-nodeps','P-nodeps','backlog','me','agent','code',100,100)",
            [],
        )
        .unwrap();
        // A human's card with a completed dep — ethos rule 8, never swept.
        conn.execute(
            "INSERT INTO issues (id,title,status,session,owner_type,type,depends_on,updated,created) \
             VALUES ('H-1','H-1','backlog','me','human','code','[\"A-done\"]',100,100)",
            [],
        )
        .unwrap();

        let (got, _held) = backlog_dep_promotions(&conn);
        let ids: std::collections::HashSet<&str> = got.iter().map(|(i, _)| i.as_str()).collect();

        assert!(ids.contains("P-all-terminal"), "all-terminal deps must promote: {ids:?}");
        assert!(!ids.contains("P-unverified"), "done code has not been verified");
        assert!(!ids.contains("P-discarded"), "discarded is not successful completion");
        assert!(!ids.contains("P-one-open"), "one open dep must NOT promote");
        assert!(!ids.contains("P-missing"), "a missing dep must NOT promote (conservative)");
        assert!(!ids.contains("W-watch"), "a watch is dormant — never promote");
        assert!(!ids.contains("E-epic"), "an epic is a container — never promote");
        assert!(!ids.contains("P-nodeps"), "a card with no depends_on is not dependency-parked");
        assert!(!ids.contains("H-1"), "never re-activate a human's card (ethos rule 8)");
        assert_eq!(ids.len(), 1, "exactly the one promotable card: {ids:?}");

        // The cleared deps ride along so the promotion log can name them.
        let (_, cleared) = got.iter().find(|(i, _)| i == "P-all-terminal").unwrap();
        assert_eq!(cleared.len(), 2, "both cleared deps are reported for the log line");
    }

    /// A card parked on a live `source_ref` trigger is NEVER promoted, even when
    /// every one of its `depends_on` is terminal — the trigger is the owner's own
    /// wake condition and outranks "deps done" (MG-1388, 2026-08-15: re-activated
    /// five times against explicit re-parks; ethos rule 8). The hold is COUNTED so
    /// it is visible in the drive report (ethos rule 4). A same-board control with
    /// terminal deps and NO trigger still promotes, so the guard is not vacuous.
    #[test]
    fn a_live_trigger_holds_a_card_even_when_its_deps_are_terminal() {
        let conn = board_db();
        conn.execute(
            "INSERT INTO issues (id,title,status,session,owner_type,type,updated,created) \
             VALUES ('A-done','A-done','verified','me','agent','code',100,100)",
            [],
        )
        .unwrap();
        // The MG-1388 shape: a terminal dep AND a live source_ref trigger.
        conn.execute(
            "INSERT INTO issues (id,title,status,session,owner_type,type,depends_on,source_ref,updated,created) \
             VALUES ('T-armed','T-armed','backlog','me','agent','investigation','[\"A-done\"]',\
                     'some namespace holds both an archive- and a competitor-shaped collection',100,100)",
            [],
        )
        .unwrap();
        // Control: same terminal dep, NO trigger -> still promotes.
        conn.execute(
            "INSERT INTO issues (id,title,status,session,owner_type,type,depends_on,updated,created) \
             VALUES ('T-plain','T-plain','backlog','me','agent','code','[\"A-done\"]',100,100)",
            [],
        )
        .unwrap();
        // A whitespace-only source_ref is NOT a live trigger -> promotes.
        conn.execute(
            "INSERT INTO issues (id,title,status,session,owner_type,type,depends_on,source_ref,updated,created) \
             VALUES ('T-blank','T-blank','backlog','me','agent','code','[\"A-done\"]','   ',100,100)",
            [],
        )
        .unwrap();

        let (got, held) = backlog_dep_promotions(&conn);
        let ids: std::collections::HashSet<&str> = got.iter().map(|(i, _)| i.as_str()).collect();
        assert!(!ids.contains("T-armed"), "a live-trigger park must NOT be promoted: {ids:?}");
        assert!(
            ids.contains("T-plain"),
            "a no-trigger terminal-deps card still promotes (guard not vacuous): {ids:?}"
        );
        assert!(
            ids.contains("T-blank"),
            "a whitespace-only source_ref is not a live trigger: {ids:?}"
        );
        assert_eq!(held, 1, "exactly the one live-trigger card is counted as held");
    }

    /// AMUX-3777: the HELD-CARD re-nag arm, which had NO coverage at all.
    ///
    /// Two arms emit `needsyou.renag`: one scans the lane's whole queue for the
    /// oldest stale ask, the other fires about the card the lane is CURRENTLY
    /// HOLDING. Both wrote `kind: "renag"`, so the ledger could not tell them
    /// apart — and the AMUX-3768 audit's empirical confirmation of this event
    /// type (0 during its dead window, 100 rows after the fix) was therefore a
    /// statement about the TYPE, not about either branch.
    ///
    /// Then the suite turned out to be no better than the ledger. Verified by
    /// mutation before writing this: replacing this arm's whole match with
    /// `None::<String>` — so it can never nudge — left all 78 board_drive tests
    /// GREEN. The branch could have been deleted outright and nothing would have
    /// said so.
    ///
    /// So this pins both halves: that the arm fires, and that it is now
    /// distinguishable from its sibling in the event data.
    #[test]
    fn the_held_card_renag_arm_fires_and_is_distinguishable_from_the_queue_scan_arm() {
        let conn = board_db();
        let now = now_f64();
        let stale = now - (needsyou_renag_days() + 2.0) * 86400.0;
        // The lane HOLDS this card (doing), it is tagged needs:you, and the ask
        // is older than the window. That is arm 2's precondition.
        add_card(&conn, "H-1", "lane", "doing", "held and unanswered", "SCOPE: x");
        tag(&conn, "H-1", "needs:you", stale);

        // ARM 2 IS HEAVILY SHADOWED, which is why it had no coverage: the
        // queue-scan arm runs FIRST and its query selects ANY stale needs:you
        // card for the session — tag OR status, including cards in doing/review,
        // which is arm 2's entire population. It takes the OLDEST and returns.
        //
        // So arm 2 is reachable only through a narrow gap: the oldest pick is
        // inside its own dedupe window (so arm 1 falls through without
        // returning) while the HELD card is a different, un-nagged one. That is
        // this fixture. Discovered by writing the naive version first and
        // watching it hit arm 1 — variant 3 of random's taxonomy (an arm made
        // unreachable by an earlier one), found in the branch AMUX-3768 flagged
        // as uncountable.
        add_card(&conn, "A-0", "lane", "needsyou", "older, already nagged", "SCOPE: x");
        tag(&conn, "A-0", "needs:you", stale - 86400.0);
        conn.execute(
            "INSERT INTO session_events (ts,session,type,data,source) \
             VALUES (?1,'lane','needsyou.renag','{\"issue\": \"A-0\"}','board-drive')",
            // 30 MINUTES, NOT 10. A `needsyou.renag` row is read by TWO
            // clocks: the per-card dedupe (3 days) and the PER-LANE advance
            // cooldown (15 min, which counts this type). At 10 minutes the
            // fixture tripped the cooldown and `select_advance` returned early,
            // so the test failed for a reason that had nothing to do with the
            // arm under test. 30 minutes is outside the cooldown and well
            // inside the dedupe window, which is the state being modelled.
            rusqlite::params![now - 1800.0],
        )
        .expect("dedupe the older pick");

        let Advance::Nudge { kind, card, .. } = select_advance(&conn, "lane", &[], now) else {
            panic!("a held needs:you card past the window must re-nag");
        };
        assert_eq!(card, "H-1", "the deduped older card must not be the one nudged about");
        assert_eq!(
            kind, "renag-held",
            "the held-card arm must be distinguishable in the ledger from the queue scan — \
             sharing `renag` is what made its fire count uncountable"
        );

        // CONTROL: inside the window it must NOT nudge, or the assertion above
        // is just "any held needs:you card nudges" and the staleness gate is
        // untested.
        let conn2 = board_db();
        add_card(&conn2, "H-2", "lane", "doing", "held, asked recently", "SCOPE: x");
        tag(&conn2, "H-2", "needs:you", now - 3600.0);
        assert!(
            matches!(
                select_advance(&conn2, "lane", &[], now),
                Advance::None { reason: "needsyou", .. }
            ),
            "a fresh ask is the human's to answer, not a lane to nag"
        );
    }

    /// A card parked by the DOCUMENTED transition — core's `Doing -> NeedsYou`,
    /// "stuck on the user, with the exact question" — carries the `needsyou`
    /// STATUS and, historically, no tag. The re-nag JOINed `issue_tags`, so it
    /// could not see that card; the same status also kept it out of auto-pickup
    /// (`status='todo'`) and out of the advance path (`doing`/`review`). Nothing
    /// handed it out and nothing brought it back, so taking the sanctioned exit
    /// was strictly worse than leaving the card in `todo`.
    ///
    /// Measured on the live board 2026-08-11: 23 of 38 open `needsyou` cards
    /// carried no tag, across six sessions, the oldest four days silent.
    #[test]
    fn a_status_only_needsyou_card_still_re_nags() {
        let conn = board_db();
        let now = now_f64();
        add_card(&conn, "N-1", "lane", "needsyou", "Ask Ethan about pricing", "SCOPE: x");
        // No tag(): status alone, which is the whole point of the case.
        let old = (now - 5.0 * 86400.0) as i64;
        conn.execute("UPDATE issues SET updated=?1 WHERE id='N-1'", rusqlite::params![old])
            .expect("age the ask");
        match select_advance(&conn, "lane", &[], now) {
            Advance::Nudge { card, kind, .. } => {
                assert_eq!(card, "N-1");
                assert_eq!(kind, "renag", "a 5-day-old unanswered ask must re-nag");
            }
            Advance::None { reason, detail } => {
                panic!("status-only needsyou never re-nagged: {reason} / {detail}")
            }
        }
    }

    /// The control that proves the test above can fail: the ONLY thing separating
    /// a fire from a no-fire is the age of the ask, so a green result cannot be
    /// coming from the query matching everything.
    #[test]
    fn a_fresh_status_only_needsyou_card_does_not_re_nag() {
        let conn = board_db();
        let now = now_f64();
        add_card(&conn, "N-1", "lane", "needsyou", "Ask Ethan about pricing", "SCOPE: x");
        let recent = (now - 3600.0) as i64;
        conn.execute("UPDATE issues SET updated=?1 WHERE id='N-1'", rusqlite::params![recent])
            .expect("freshen");
        assert!(
            !matches!(select_advance(&conn, "lane", &[], now), Advance::Nudge { kind: "renag", .. }),
            "an ask made an hour ago is not stale and must not re-nag"
        );
    }

    /// The tagged path keeps its own clock. `issue_tags.added_at` is the ask
    /// time, NOT `issues.updated` — AC-178: `updated` is last-touch, so the
    /// cards carrying the most commentary were exactly the ones whose stale-ask
    /// check could never fire. Widening the query to status-only cards must not
    /// quietly demote the tagged ones onto the worse clock.
    #[test]
    fn a_tagged_ask_ages_from_the_tag_not_the_row() {
        let conn = board_db();
        let now = now_f64();
        add_card(&conn, "N-1", "lane", "todo", "Ask Ethan about pricing", "SCOPE: x");
        tag(&conn, "N-1", "needs:you", now - 5.0 * 86400.0);
        // Touched a minute ago — under `updated` this ask would look brand new.
        conn.execute(
            "UPDATE issues SET updated=?1 WHERE id='N-1'",
            rusqlite::params![(now - 60.0) as i64],
        )
        .expect("touch");
        assert!(
            matches!(select_advance(&conn, "lane", &[], now), Advance::Nudge { kind: "renag", .. }),
            "a 5-day-old TAG on a freshly-touched row must still re-nag"
        );
    }

    /// The view must share the predicate of the mechanism it describes: the
    /// trace's `eligible_todos` and the selector must agree on every card.
    #[test]
    fn the_trace_count_agrees_with_the_selector() {
        let conn = board_db();
        add_card(&conn, "T-1", "lane", "todo", "real", "SCOPE: x\n- [ ] y");
        add_card(&conn, "T-2", "lane", "todo", "also real", "SCOPE: x\n- [ ] y");
        add_card(&conn, "T-3", "lane", "todo", "human", "SCOPE: x\n- [ ] y");
        tag(&conn, "T-3", "needs:you", now_f64());
        conn.execute("UPDATE issues SET archived=1 WHERE id='T-2'", []).expect("archive");
        assert_eq!(eligible_todo_count(&conn, "lane", 1_000_000.0), 1);
        assert_eq!(claimed(&select_pickup_with(&conn, "lane", now_f64(), false)), Some("T-1"));
    }

    /// AMUX-2983 (gtm-videos): the auto-pickup claimed GV-648 while it was
    /// already `done`, because select_pickup drops its connection and the
    /// unconditional `SET status='doing'` claim would REOPEN a closed card and
    /// dispatch "work it now" — re-running finished, non-idempotent work. The
    /// compare-and-swap (`AND status='todo'`) must refuse the claim AND leave
    /// the card untouched. Needs a real Store (write path), not board_db().
    #[tokio::test]
    async fn claim_refuses_a_closed_card_and_does_not_reopen_it() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            std::sync::Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        let state = crate::api::AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let ins = |id: &str, status: &str| {
            let (id, status) = (id.to_string(), status.to_string());
            store
                .write(move |conn| {
                    conn.execute(
                        "INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type) \
                         VALUES (?1,'t','',?2,'lane',100,100,'agent','code')",
                        rusqlite::params![id, status],
                    )?;
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                })
                .unwrap();
        };
        let status_of = |id: &str| -> String {
            store
                .read()
                .unwrap()
                .query_row("SELECT status FROM issues WHERE id=?1", [id], |r| r.get(0))
                .unwrap()
        };
        // The GV-648 shape: a DONE card. The claim must refuse and not reopen.
        ins("GV-1", "done");
        assert!(!claim_card(&state, "lane", "GV-1").await, "claiming a done card must return false");
        assert_eq!(status_of("GV-1"), "done", "a done card must NOT be reopened to 'doing'");
        // The control that proves the swap can succeed: a real todo claims.
        ins("T-1", "todo");
        assert!(claim_card(&state, "lane", "T-1").await, "a todo card claims");
        assert_eq!(status_of("T-1"), "doing");
        // And a second claim of the now-doing card refuses (idempotent).
        assert!(!claim_card(&state, "lane", "T-1").await, "re-claiming a doing card must refuse");
    }

    /// AMUX-3450: the CLI help always promised "claim a todo/backlog card";
    /// the server refused backlog, and a peer's handover (AF-127) relied on
    /// the promise. The MANUAL claim path (`claim_card_from` "backlog") takes
    /// a parked card; the auto-pickup path (`claim_card` = "todo") must still
    /// REFUSE one, because parking a card todo->backlog mid-race is exactly
    /// how an owner defeats a racing pickup — widening that CAS would let
    /// pickup steal deliberately parked work.
    #[tokio::test]
    async fn backlog_claims_manually_but_never_via_the_todo_cas() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            std::sync::Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        let state = crate::api::AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        store
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type) \
                     VALUES ('B-1','t','','backlog','lane',100,100,'agent','code')",
                    [],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let read_row = |col: &str| -> String {
            store
                .read()
                .unwrap()
                .query_row(
                    &format!("SELECT COALESCE({col},'') FROM issues WHERE id='B-1'"),
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert!(
            !claim_card(&state, "lane", "B-1").await,
            "the todo CAS must refuse a parked card"
        );
        assert_eq!(read_row("status"), "backlog", "the refused claim must leave it parked");
        assert!(
            claim_card_from(&state, "lane", "B-1", "backlog").await,
            "an explicit manual claim of a named backlog card must work"
        );
        assert_eq!(read_row("status"), "doing");
        // Mechanism check, deliberately last (AF-125 ordering): a backlog card
        // is never queue-dispatched, so "Auto-picked up from queue" here would
        // be a false statement in the card's own log.
        let log = read_row("log");
        assert!(
            log.contains("Claimed from backlog by lane"),
            "the card log must record the claim as what it was: {log}"
        );
    }

    /// AF-79 ("auto-pickup dispatches cards owned by another session"). The
    /// dispatch predicate keys on the card's OWNER (`i.session=?1`), so a card is
    /// only ever offered to the lane that owns it, never to the lane that
    /// CREATED it, and never to an idle peer. This kills two of the three
    /// hypotheses in one shot: `?1` is the owning lane (not a group, not the
    /// creator), and there is no second selection that keys on attribution. The
    /// incident card was owned by amux-frustrations at pickup and only reassigned
    /// to amux one second AFTER the claim; the predicate never disagreed with the
    /// owner. Control: the owner IS offered exactly the card, so the assertion can
    /// fail.
    /// AMUX-3948/3946. Auto-pickup must not hand a lane a card that `doing`
    /// would then refuse.
    ///
    /// `claim_card` swaps todo->doing with a direct SQL CAS, bypassing the PATCH
    /// gate entirely, so before this the continuation gate was refusable by hand
    /// and bypassable by waiting for pickup.
    ///
    /// BOTH ARMS, because the gate-on arm alone would pass for a pickup that
    /// selected nothing at all, and the gate-off arm alone would pass for a
    /// pickup that ignored the gate. Neither is the property.
    #[test]
    fn pickup_skips_a_card_that_cannot_say_what_to_do_next() {
        let conn = board_db();
        add_card(&conn, "AF-90", "lane", "todo", "no continuation", "SCOPE: real\n- [ ] do it");

        // GATE OFF: the card is workable and pickup offers it. Without this the
        // cell below passes for a pickup that is simply broken.
        assert_eq!(
            claimed(&select_pickup_with(&conn, "lane", now_f64(), false)),
            Some("AF-90"),
            "with the gate off this is an ordinary claimable card"
        );

        // GATE ON, no next_action: skipped, because `doing` would refuse it and
        // pickup's promise is "work it now".
        assert!(
            claimed(&select_pickup_with(&conn, "lane", now_f64(), true)).is_none(),
            "a card with no next_action must not be handed out as ready to work"
        );

        // GATE ON, WITH a next_action: offered again. This is the arm that keeps
        // the gate from being a blanket refusal that empties every opted-in
        // lane's queue.
        conn.execute(
            "UPDATE issues SET next_action='Rerun compatibility test 07 against KubeRay 1.4' \
             WHERE id='AF-90'",
            [],
        )
        .unwrap();
        assert_eq!(
            claimed(&select_pickup_with(&conn, "lane", now_f64(), true)),
            Some("AF-90"),
            "satisfying the gate makes it claimable again, or the gate is a mute"
        );
    }

    #[test]
    fn pickup_offers_a_card_only_to_its_session_owner() {
        let conn = board_db();
        // Owned by `amux`; a different lane created it (as in the incident).
        add_card(&conn, "AF-78", "amux", "todo", "owned by amux", "SCOPE: real\n- [ ] do it");
        assert!(
            claimed(&select_pickup_with(&conn, "amux-frustrations", now_f64(), false)).is_none(),
            "a card owned by 'amux' must never be offered to a non-owner lane"
        );
        assert_eq!(
            eligible_todo_count(&conn, "amux-frustrations", 1_000_000.0),
            0,
            "and it must not count as eligible for the non-owner"
        );
        assert_eq!(
            claimed(&select_pickup_with(&conn, "amux", now_f64(), false)),
            Some("AF-78"),
            "the owning lane is the only one the card is dispatched to"
        );
    }

    /// AF-79 ROOT CAUSE: the card's own log could not say WHICH lane a pickup
    /// handed it to. "Auto-picked up from queue" named no lane, so a legitimate
    /// same-owner claim followed by a reassignment read as a cross-owner dispatch,
    /// and the incident was un-reconstructable from the card. The claim log line
    /// must NAME the claimer (same-owner case) AND record a reassignment when the
    /// prior owner differs. The latter is the safety net for a mis-dispatch every
    /// caller is supposed to prevent, so it must be real, not merely claimed
    /// (ethos rule 6). On the pre-fix code the bare "Auto-picked up from queue"
    /// line fails the first assertion.
    #[tokio::test]
    async fn claim_log_names_the_lane_and_records_a_reassignment() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            std::sync::Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        let state = crate::api::AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let ins = |id: &str, owner: &str| {
            let (id, owner) = (id.to_string(), owner.to_string());
            store
                .write(move |conn| {
                    conn.execute(
                        "INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type) \
                         VALUES (?1,'t','','todo',?2,100,100,'agent','code')",
                        rusqlite::params![id, owner],
                    )?;
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                })
                .unwrap();
        };
        let log_of = |id: &str| -> String {
            store
                .read()
                .unwrap()
                .query_row(
                    "SELECT COALESCE(log,'') FROM issues WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )
                .unwrap()
        };

        // Same-owner claim (the auto-pickup case): the log names the claimer and
        // does NOT invent a reassignment.
        ins("AF-78", "amux-frustrations");
        assert!(claim_card(&state, "amux-frustrations", "AF-78").await, "a todo card claims");
        let log = log_of("AF-78");
        assert!(
            log.contains("Auto-picked up from queue by amux-frustrations"),
            "the claim log must name the claiming lane so a pickup is reconstructable: {log:?}"
        );
        assert!(
            !log.contains("reassigned"),
            "a same-owner claim is not a reassignment: {log:?}"
        );

        // Cross-owner claim (unreachable from the guarded callers; the safety net
        // for a real mis-dispatch): the log records the ownership change.
        ins("AF-99", "amux");
        assert!(claim_card(&state, "amux-frustrations", "AF-99").await, "a todo card claims");
        let log = log_of("AF-99");
        assert!(
            log.contains("by amux-frustrations") && log.contains("reassigned from amux"),
            "a cross-owner claim must record the reassignment in the card log: {log:?}"
        );

        // AND IT MUST REACH THE LEDGER (AMUX-3776). The card log and a
        // tracing::warn were the only records, so a sweep of `session_events`
        // could not find this violation at all — while the ordinary path emits
        // `task.claimed` right beside it. The asymmetry is the bug: the branch
        // that is supposed to be impossible was the one leaving no row.
        //
        // It also blocks the (reached, taken) instrument AMUX-3768 proposes,
        // which can only see branches that RECORD: this one would report
        // dead-by-absence forever while being perfectly reachable.
        let rows = |t: &str| -> Vec<String> {
            store
                .read()
                .unwrap()
                .prepare("SELECT COALESCE(data,'') FROM session_events WHERE type=?1")
                .and_then(|mut st| {
                    st.query_map([t], |r| r.get::<_, String>(0)).map(|r| r.flatten().collect())
                })
                .unwrap_or_default()
        };
        let cross = rows("claim.cross_owner");
        assert_eq!(cross.len(), 1, "exactly the ONE cross-owner claim files a row: {cross:?}");
        assert!(
            cross[0].contains("AF-99") && cross[0].contains("amux"),
            "the row must name the card and the prior owner, or it cannot be investigated: {}",
            cross[0]
        );
        // CONTROL: the same-owner claim above must NOT have filed one, or this
        // type says nothing and the ledger is noisier for no gain.
        assert!(
            !cross[0].contains("AF-78"),
            "a same-owner claim is not a violation: {}",
            cross[0]
        );
        assert_eq!(
            rows("task.claimed").len(),
            2,
            "both claims still emit task.claimed — the new type is ADDITIONAL, not a \
             replacement, or every existing consumer of the claim ledger silently loses a row"
        );
    }

    /// AMUX-4040. A lane AT THE CAP must still repair its own queue.
    ///
    /// Ethan, 2026-09-02, rtsp-connection: "it has tons of todo and backlog it
    /// should've kept going". 13 todo cards, `ready: []`, every one blocked, and
    /// the entire queue hung off a single edge — RC-67 depends on RC-66, a
    /// `backlog` card owned by that same lane, which is exactly the
    /// self-resolvable case `PromoteDeps` exists for. RC-80 held the one WIP
    /// slot, so the promotion pass (which lives inside the candidate loop) was
    /// never reached, and the lane went idle over a full queue it could not
    /// advance.
    ///
    /// Promoting a dep CLAIMS NOTHING and takes no slot, so being at the cap is
    /// not a reason to leave the queue broken. The control below is the half
    /// that keeps this a repair rather than a hole: with nothing promotable, a
    /// capped lane must still be told `wip-cap` and handed no work.
    /// AMUX-4055. An empty To Do column with actionable backlog is work the
    /// Serialises the three tests that mutate `AMUX_DISPATCH_BACKLOG_WHEN_IDLE`.
    ///
    /// The variable is PROCESS-GLOBAL and cargo runs tests as threads in ONE
    /// process, so `remove_var` in the default-on test and `set_var(.., "0")`
    /// in the other two are the same memory. Interleave them and the default-on
    /// test reads "0" between its own remove_var and its assertion, then fails
    /// with "default-on backlog dispatch did not run" — a red that points at
    /// the dispatcher and is really this.
    ///
    /// MEASURED, not theorised: 1 failure in 12 consecutive runs of this module
    /// with the auto-builder idle and the worktree clean, so build contention
    /// (AMUX-3853) was ruled out first. It also survived a full-suite run once
    /// and failed the next, which is what makes it expensive — it reads as a
    /// regression in whatever landed that day.
    ///
    /// Poisoning is recovered rather than propagated: one panicking test must
    /// not convert the other two into failures that hide their own result.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// worker is expected to keep driving. Default-on is safe because the drain
    /// query excludes every parked/human/trigger shape; an explicit 0 remains
    /// the worker/group/global opt-out.
    #[test]
    fn backlog_dispatch_is_on_by_default_and_supports_an_explicit_opt_out() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // The default must be tested against an actually empty scope chain,
        // not the developer machine's ~/.amux global configuration.
        let home = tempfile::tempdir().expect("temp home");
        let _home = crate::api::settings::test_env::set_home(home.path());
        let build = || {
            let conn = board_db();
            // No todo at all: the only state where this may fire.
            add_card(&conn, "B-1", "lane", "backlog", "oldest", "SCOPE: x\n- [ ] y");
            add_card(&conn, "B-2", "lane", "backlog", "newer", "SCOPE: x\n- [ ] y");
            conn.execute("UPDATE issues SET created=100 WHERE id='B-1'", []).unwrap();
            conn.execute("UPDATE issues SET created=200 WHERE id='B-2'", []).unwrap();
            conn
        };

        // DEFAULT ON: the oldest eligible card drains without every worker
        // needing a redundant opt-in key.
        std::env::remove_var(DISPATCH_BACKLOG_KEY);
        match select_pickup_with(&build(), "lane", now_f64(), false) {
            Pickup::DrainBacklog { card, backlog_left, todo_refusals, .. } => {
                assert_eq!(card, "B-1", "oldest first, so a starved card is not starved further");
                assert_eq!(backlog_left, 2, "the count reports what is drainable, before the move");
                assert_eq!(todo_refusals, 0, "an empty todo queue has no refusals");
            }
            other => panic!("default-on backlog dispatch did not run: {other:?}"),
        }

        // EXPLICIT OPT-OUT: no backlog card moves.
        std::env::set_var(DISPATCH_BACKLOG_KEY, "0");
        match select_pickup_with(&build(), "lane", now_f64(), false) {
            Pickup::None { reason, .. } => assert_eq!(reason, "no-eligible-card"),
            other => panic!("an opted-out lane must not drain backlog: {other:?}"),
        }
        std::env::remove_var(DISPATCH_BACKLOG_KEY);
    }

    /// The trace must SAY why a drain declined (AMUX-4055 follow-up).
    ///
    /// Ethan asked "why hasn't it drained" three times in one morning. The
    /// answer was good — all 20 of tubescience's backlog cards sat inside the
    /// 24h parked-trigger window, the oldest by 2.3 hours — and it existed
    /// nowhere in the product. The trace said `no-eligible-card` and never
    /// mentioned that a drain had been considered and declined.
    #[test]
    fn the_trace_says_why_a_drain_declined() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let conn = board_db();
        add_card(&conn, "P-1", "lane", "backlog", "parked on a trigger", "SCOPE: x");
        conn.execute(
            "UPDATE issues SET source_ref='watch:thing', last_verified_at=?1 WHERE id='P-1'",
            rusqlite::params![now_f64() as i64],
        )
        .unwrap();

        // OPTED IN, nothing drainable: the note must name the split, because
        // "1 backlog" reads as ignored work and "1 backlog, 1 parked" reads as
        // a lane correctly waiting.
        std::env::set_var(DISPATCH_BACKLOG_KEY, "1");
        match select_pickup_with(&conn, "lane", now_f64(), false) {
            Pickup::None { detail, .. } => {
                assert!(detail.contains("drain: ON"), "the note must say the drain ran: {detail}");
                assert!(detail.contains("parked"), "...and that the cards are parked: {detail}");
            }
            other => panic!("a fully parked backlog must not drain: {other:?}"),
        }

        // EXPLICITLY opted out: say so and name the control. An absent note and
        // a lane with no backlog are otherwise the same silence.
        std::env::set_var(DISPATCH_BACKLOG_KEY, "0");
        match select_pickup_with(&conn, "lane", now_f64(), false) {
            Pickup::None { detail, .. } => assert!(
                detail.contains("drain: OFF"),
                "a lane with backlog and the switch off must say so: {detail}"
            ),
            other => panic!("expected no-eligible-card: {other:?}"),
        }
        std::env::remove_var(DISPATCH_BACKLOG_KEY);
    }

    /// The two cards this must never touch, even when opted in.
    #[test]
    fn a_drain_skips_a_human_parked_card_and_a_live_trigger() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(DISPATCH_BACKLOG_KEY, "1");
        let conn = board_db();
        // Parked ON A HUMAN.
        add_card(&conn, "H-1", "lane", "backlog", "waiting on a person", "SCOPE: x");
        conn.execute("INSERT INTO issue_tags (issue_id, tag) VALUES ('H-1','needs:you')", [])
            .unwrap();
        // Parked on a LIVE TRIGGER: a source_ref its owner re-confirmed recently.
        // Draining this would override a deliberate re-park, which is MG-1388.
        add_card(&conn, "T-1", "lane", "backlog", "waiting on a condition", "SCOPE: x");
        conn.execute(
            "UPDATE issues SET source_ref='watch:thing', last_verified_at=?1 WHERE id='T-1'",
            rusqlite::params![now_f64() as i64],
        )
        .unwrap();
        match select_pickup_with(&conn, "lane", now_f64(), false) {
            Pickup::None { reason, .. } => assert_eq!(reason, "no-eligible-card"),
            other => panic!("neither a human-parked nor a trigger-parked card may drain: {other:?}"),
        }
        std::env::remove_var(DISPATCH_BACKLOG_KEY);
    }

    #[test]
    fn a_capped_lane_still_promotes_its_own_blocked_dependency() {
        let conn = board_db();
        // The cap is full, exactly as rtsp-connection's was.
        add_card(&conn, "RC-80", "lane", "doing", "holding the slot", "SCOPE: x");
        // ...and the head of the chain is blocked by this lane's OWN backlog card.
        add_card(&conn, "RC-66", "lane", "backlog", "the parked blocker", "SCOPE: x");
        add_card(&conn, "RC-67", "lane", "todo", "blocked head", "SCOPE: x\n- [ ] y");
        conn.execute(
            "UPDATE issues SET depends_on=?1 WHERE id='RC-67'",
            rusqlite::params!["[\"RC-66\"]"],
        )
        .expect("set dep");

        match select_pickup_with(&conn, "lane", now_f64(), false) {
            Pickup::PromoteDeps { blocked_card, promoted } => {
                assert_eq!(blocked_card, "RC-67");
                assert_eq!(promoted, vec!["RC-66"]);
            }
            other => panic!(
                "a capped lane must still unblock its own queue; \
                 leaving it blocked is what kept rtsp-connection idle over 13 todos: {other:?}"
            ),
        }
    }

    /// THE CONTROL. Same capped lane, nothing promotable: the cap must still be
    /// reported and no work handed out, or the fix above has turned the WIP
    /// guard into a hole.
    #[test]
    fn a_capped_lane_with_nothing_promotable_still_reports_the_cap() {
        let conn = board_db();
        add_card(&conn, "D-1", "lane", "doing", "holding the slot", "SCOPE: x");
        // Blocked by ANOTHER lane's card — not this lane's to promote.
        add_card(&conn, "X-9", "other", "backlog", "someone else's blocker", "SCOPE: x");
        add_card(&conn, "T-2", "lane", "todo", "blocked by a peer", "SCOPE: x\n- [ ] y");
        conn.execute("UPDATE issues SET depends_on=?1 WHERE id='T-2'", rusqlite::params!["[\"X-9\"]"])
            .expect("set dep");
        match select_pickup_with(&conn, "lane", now_f64(), false) {
            Pickup::None { reason, .. } => assert_eq!(reason, "wip-cap"),
            other => panic!("another lane's backlog card is not ours to promote: {other:?}"),
        }
    }

    /// AMUX-4042. Ethan chose auto-reclaim over escalation for the case the
    /// drive had already given up on: byo-ray held BR-51 in `doing` for 42h
    /// with no `next_action` while 5 todos sat unclaimable, and the trace read
    /// "advance: budget-spent (all 1 candidate(s) have spent their 3-nudge 24h
    /// budget — repeating the prompt is not the fix)". The system was right
    /// that nudging was exhausted and had no next move.
    ///
    /// Each cell below is one guard, because the whole safety argument for
    /// amux moving somebody's card rests on them holding.
    #[test]
    fn an_abandoned_doing_card_is_reclaimed_but_only_under_every_guard() {
        let stale = now_f64() - 8.0 * 3600.0; // past the 6h default
        let setup = |touched: f64, with_todo: bool| {
            let conn = board_db();
            add_card(&conn, "BR-51", "lane", "doing", "abandoned", "SCOPE: x");
            conn.execute("UPDATE issues SET updated=?1 WHERE id='BR-51'", rusqlite::params![touched as i64])
                .expect("age the card");
            if with_todo {
                add_card(&conn, "BR-3", "lane", "todo", "waiting behind it", "SCOPE: x\n- [ ] y");
            }
            conn
        };

        // THE SPECIMEN: untouched past the window, with work waiting behind it.
        match select_pickup_with(&setup(stale, true), "lane", now_f64(), false) {
            Pickup::ReclaimStale { card, held_h, blocking } => {
                assert_eq!(card, "BR-51");
                assert!(held_h >= 6.0, "held_h must report the real idle time: {held_h}");
                assert_eq!(blocking, 1, "and how much work it was holding up");
            }
            other => panic!("an abandoned card holding the only slot must be reclaimed: {other:?}"),
        }

        // GUARD 2: a card touched recently is being worked, however long it has
        // been open. This is what stops the reclaim from racing a live lane.
        match select_pickup_with(&setup(now_f64() - 60.0, true), "lane", now_f64(), false) {
            Pickup::None { reason, .. } => assert_eq!(reason, "wip-cap"),
            other => panic!("a card touched a minute ago is not abandoned: {other:?}"),
        }

        // GUARD 3: with nothing to unblock, reclaiming only churns the board.
        match select_pickup_with(&setup(stale, false), "lane", now_f64(), false) {
            Pickup::None { reason, .. } => assert_eq!(reason, "wip-cap"),
            other => panic!("no claimable work means no reason to take the card: {other:?}"),
        }
    }

    /// GUARD 4, and the one that keeps this from touching a human's card: a
    /// `doing` card tagged `needs:you` is parked ON A PERSON. The same query
    /// that decides what holds WIP excludes it, so it is never a reclaim
    /// candidate — and because it does not hold WIP, the lane is not capped by
    /// it either.
    #[test]
    fn a_needs_you_card_is_never_reclaimed() {
        let conn = board_db();
        add_card(&conn, "H-1", "lane", "doing", "waiting on a human", "SCOPE: x");
        conn.execute(
            "UPDATE issues SET updated=?1 WHERE id='H-1'",
            rusqlite::params![(now_f64() - 48.0 * 3600.0) as i64],
        )
        .expect("age it");
        conn.execute(
            "INSERT INTO issue_tags (issue_id, tag) VALUES ('H-1','needs:you')",
            [],
        )
        .expect("tag it");
        add_card(&conn, "T-9", "lane", "todo", "behind it", "SCOPE: x\n- [ ] y");
        // Any other outcome is fine — the card does not hold WIP, so the lane is
        // free to be handed T-9. The one thing that must never happen is the
        // reclaim.
        let got = select_pickup_with(&conn, "lane", now_f64(), false);
        assert!(
            !matches!(got, Pickup::ReclaimStale { .. }),
            "a card parked on a human must never be auto-reclaimed: {got:?}"
        );
    }

    #[test]
    fn a_session_at_the_wip_cap_gets_nothing() {
        let conn = board_db();
        add_card(&conn, "D-1", "lane", "doing", "already working", "SCOPE: x");
        add_card(&conn, "T-1", "lane", "todo", "next", "SCOPE: x\n- [ ] y");
        match select_pickup_with(&conn, "lane", now_f64(), false) {
            Pickup::None { reason, detail } => {
                assert_eq!(reason, "wip-cap");
                assert!(detail.contains("D-1"), "the trace must name what is held: {detail}");
            }
            _ => panic!("a lane holding a doing card must not be handed a second"),
        }
    }

    /// Ethan/primis 2026-08-04: an ARCHIVED `doing` card consumed a lane's whole
    /// WIP-1 budget forever while the board hid it — the lane rendered
    /// "IN PROGRESS 0" while being structurally unable to take work. Archiving
    /// means CLEARED; a cleared card cannot be in progress.
    #[test]
    fn an_archived_doing_card_does_not_hold_wip() {
        let conn = board_db();
        add_card(&conn, "D-1", "lane", "doing", "cleared", "SCOPE: x");
        conn.execute("UPDATE issues SET archived=1 WHERE id='D-1'", []).expect("archive");
        add_card(&conn, "T-1", "lane", "todo", "next", "SCOPE: x\n- [ ] y");
        assert_eq!(claimed(&select_pickup_with(&conn, "lane", now_f64(), false)), Some("T-1"));
    }

    /// AMUX-3757, Ethan 2026-08-26: "why do i need to push @tubescience to
    /// continue despite it having so many tasks in its backlog".
    ///
    /// Because typing at a lane disabled its auto-pickup. Every prompt is
    /// auto-captured as a `doing` card whose desc is still literally
    /// `**Prompt:** <what he typed>`, and that card counted against WIP-1. The
    /// specimen is TUBES-2225, titled "Why are you stopping" — the complaint
    /// about the lane stopping was the thing holding the slot that kept it
    /// stopped. Measured that day: 14 of 52 running lanes idle at the cap with
    /// 281 cards stranded behind them.
    ///
    /// The second cell is the one that makes this a gate rather than a hole. A
    /// RESHAPED card — desc rewritten into a real definition, which is the exit
    /// the decompose nudge asks for — must go back to holding WIP, because at
    /// that point it IS work in progress. Without it, `NOT (creator='amux')`
    /// alone would exempt every amux-minted card forever and this test would
    /// look identical.
    #[test]
    fn a_capture_shell_does_not_hold_the_wip_slot_but_a_reshaped_card_does() {
        let conn = board_db();
        add_card(&conn, "C-1", "lane", "doing", "Why are you stopping", "**Prompt:** why are you stopping? you should continue");
        conn.execute("UPDATE issues SET creator='amux' WHERE id='C-1'", []).expect("mint");
        add_card(&conn, "T-1", "lane", "todo", "next", "SCOPE: x\n- [ ] y");
        assert_eq!(
            claimed(&select_pickup_with(&conn, "lane", now_f64(), false)),
            Some("T-1"),
            "an unanswered prompt is not work in progress; the lane must still be dealt"
        );

        // Reshaped: same card, same creator, a real definition.
        conn.execute(
            "UPDATE issues SET \"desc\"='SCOPE: fix the stall\n- [ ] repro' WHERE id='C-1'",
            [],
        )
        .expect("reshape");
        conn.execute("UPDATE issues SET status='todo' WHERE id='T-1'", []).expect("reset");
        match select_pickup_with(&conn, "lane", now_f64(), false) {
            Pickup::None { reason, detail } => {
                assert_eq!(reason, "wip-cap");
                assert!(detail.contains("C-1"), "the reshaped card holds the slot: {detail}");
            }
            other => panic!(
                "a reshaped capture is real work and must hold WIP, got {:?}",
                claimed(&other)
            ),
        }
    }

    /// AMUX-3758. The number that answers "we need to rethink how we do amux
    /// because this isn't working" is a FLEET number, and every existing view
    /// is per-lane.
    ///
    /// The median is hand-rolled SQL (`ORDER BY ... DESC LIMIT 1 OFFSET n/2`),
    /// which is exactly the kind of expression that returns a plausible number
    /// while being wrong — so the fixture uses ages whose median is not the
    /// mean and not an endpoint. 2/10/30 has mean 14 and median 10; an
    /// off-by-one in the OFFSET lands on 2 or 30, and averaging lands on 14.
    #[test]
    fn a_decomposed_capture_epic_is_not_asked_to_decompose_again() {
        let conn = board_db();
        add_card(&conn, "EPIC-1", "lane", "doing", "Build the invoice tool", "**Prompt:** build and review the invoice tool");
        conn.execute("UPDATE issues SET type='epic', source='capture', creator='amux' WHERE id='EPIC-1'", []).unwrap();
        add_card(&conn, "CHILD-1", "lane", "doing", "Implement the parser", "SCOPE: parse strict integer cents\n- [ ] malformed amounts fail");
        conn.execute("UPDATE issues SET epic='EPIC-1' WHERE id='CHILD-1'", []).unwrap();
        assert!(unnudged_capture_cleanup(&conn, "lane").is_none());
        match select_advance(&conn, "lane", &[], now_f64()) {
            Advance::Nudge { card, kind, .. } => {
                assert_eq!(card, "CHILD-1");
                assert_ne!(kind, "decompose-asked");
            }
            _ => panic!("advance must target the executable child"),
        }
    }

    #[test]
    fn the_queue_shape_reports_the_dispatchable_slice_and_the_age_of_the_human_backlog() {
        let conn = board_db();
        let now = now_f64();
        add_card(&conn, "T-1", "lane", "todo", "workable", "SCOPE: x");
        add_card(&conn, "T-PARKED", "lane", "todo", "waiting on owner", "SCOPE: x");
        tag(&conn, "T-PARKED", "needs:you:decision", now);
        for (n, days) in [("N-1", 2.0), ("N-2", 10.0), ("N-3", 30.0)] {
            add_card(&conn, n, "lane", "needsyou", "asked a human", "SCOPE: x");
            conn.execute(
                "UPDATE issues SET updated=?1 WHERE id=?2",
                rusqlite::params![(now - days * 86400.0) as i64, n],
            )
            .expect("age");
        }
        for n in ["B-1", "B-2", "B-3", "B-4", "B-5", "B-6"] {
            add_card(&conn, n, "lane", "backlog", "parked", "SCOPE: x");
        }
        // An ARCHIVED card is cleared, and must not inflate the denominator —
        // otherwise dispatchable_pct falls as the board is TIDIED, which reads
        // as the fleet getting worse.
        add_card(&conn, "Z-1", "lane", "backlog", "cleared", "SCOPE: x");
        conn.execute("UPDATE issues SET archived=1 WHERE id='Z-1'", []).expect("archive");

        let s = fleet_queue_shape(&conn);
        assert_eq!(s["open_total"], json!(11), "2 raw todo + 3 needsyou + 6 backlog: {s}");
        assert_eq!(s["dispatchable_todo"], json!(1));
        assert_eq!(s["dispatchable_pct"], json!(9.1), "1 of 11, to one decimal: {s}");
        assert_eq!(s["by_status"]["todo"], json!(2));
        assert_eq!(s["by_status"]["backlog"], json!(6));
        assert_eq!(s["by_status"]["needsyou"], json!(3));
        let med = s["needsyou_median_age_d"].as_f64().expect("a median");
        assert!(
            (med - 10.0).abs() < 0.5,
            "the MIDDLE of 2/10/30 days, not the mean (14) and not an endpoint: got {med}"
        );

        // A fleet with a genuinely empty board reads zero for the same headline,
        // so `open_total` is what tells the two apart (ethos rule 4).
        let empty = board_db();
        let e = fleet_queue_shape(&empty);
        assert_eq!(e["dispatchable_pct"], json!(0.0));
        assert_eq!(e["open_total"], json!(0), "zero%% with zero open is 'nothing to do': {e}");
        assert!(e["needsyou_median_age_d"].is_null(), "no pile, no age: {e}");
    }

    /// AMUX-3759, Ethan: "theres also way too much tokens used for some reason
    /// in between tasks".
    ///
    /// The pickup handed the lane a card ID and 500 characters, so the lane
    /// spent MODEL CALLS fetching back text amux already had. Measured
    /// 2026-08-26: an auto-pickup turn takes a median of 22 tool steps where a
    /// human-prompted turn takes 3, at a median resident context of 308k tokens
    /// per call — so a saved fetch is worth ~300k input tokens against ~1k of
    /// extra prompt. On the live queue the cap truncated 86% of todo cards and
    /// discarded 108,820 characters of definition.
    ///
    /// Two cells, and the second is the one that matters: a cut excerpt must
    /// SAY it was cut. Silent truncation is indistinguishable from a short
    /// card, so the lane works confidently from half a definition — the AF-161
    /// shape, absence reading as emptiness.
    #[test]
    fn the_pickup_excerpt_carries_the_card_and_admits_when_it_does_not() {
        let conn = board_db();
        let short = "SCOPE: rotate the key\n- [ ] do it";
        add_card(&conn, "P-1", "lane", "todo", "short card", short);
        let row = bs::get_issue(&conn, "P-1").unwrap().unwrap();
        let p = pickup_prompt(&conn, "lane", &row);
        assert!(p.contains("rotate the key"), "the whole definition must ride along: {p}");
        assert!(
            !p.contains("excerpt cut at"),
            "a card that fits is not truncated and must not claim to be: {p}"
        );

        // A card longer than the cap. 6,000 chars is between this fleet's p75
        // (3,652) and its max (10,299), so it is a real card, not a synthetic
        // extreme — and it is over the 4000 default while being under what a
        // "just raise it a lot" cap would catch.
        let long = format!("SCOPE: big one\n{}", "x".repeat(6000));
        add_card(&conn, "P-2", "lane", "todo", "long card", &long);
        let row = bs::get_issue(&conn, "P-2").unwrap().unwrap();
        let p = pickup_prompt(&conn, "lane", &row);
        assert!(
            p.contains("excerpt cut at"),
            "a truncated excerpt must announce itself: {}",
            &p[..p.len().min(400)]
        );
        assert!(
            p.contains("GET /api/board/P-2"),
            "and name the read that returns the rest, or the admission is useless"
        );
        // The old 500-char cap is the regression this guards: the excerpt must
        // carry MUCH more than a stub, or the lane is back to fetching.
        let quoted = p.matches('x').count();
        assert!(
            quoted > 3000,
            "the excerpt must carry the card, not a 500-char stub (got {quoted} body chars)"
        );
    }

    /// THE PICKUP PROMPT MUST STAY READABLE BY THE STALE-PICKUP GUARD, and this
    /// is deliberately in board_drive.rs rather than beside the parser: it fails
    /// in the file someone is editing when they reword the template.
    ///
    /// Its predecessor lived next to `pickup_card_id` and fed a HAND-WRITTEN
    /// copy of the template, which is why it was green for the entire outage.
    /// 03ed2b6c (2026-08-26 17:36) shortened "Claimed board card <ID> from your
    /// queue" to "Claimed <ID> —", the parser kept the old literal, and from
    /// that commit `pickup_card_id` returned None for every real pickup: the
    /// AMUX-3052 guard voided nothing for 17 hours across 97 deliveries. Three
    /// were provably stale on arrival (AF-267/amux-frustrations 44s, TUBES-2242
    /// 33s, NISSA-89 18s after their cards closed) and each cost a lane a turn
    /// being told to redo finished work.
    ///
    /// A test that mints its own input can only ever pin ITSELF. This one runs
    /// the real producer into the real parser.
    #[test]
    fn pickup_prompt_round_trips_through_the_stale_guard() {
        let conn = board_db();
        add_card(&conn, "RT-42", "lane", "doing", "round trip", "SCOPE: whatever\n- [ ] x");
        let row = bs::get_issue(&conn, "RT-42").unwrap().unwrap();
        let prompt = pickup_prompt(&conn, "lane", &row);

        assert_eq!(
            crate::api::session_verbs::pickup_card_id(&prompt).as_deref(),
            Some("RT-42"),
            "the stale-pickup guard cannot read the id out of the prompt this file mints, \
             so it will void nothing and every stale pickup ships: {}",
            &prompt[..prompt.len().min(200)]
        );

        // The id must be the FIRST token after the anchor — the property the
        // parser actually depends on, and the one a reword breaks silently.
        let after = prompt
            .split_once(PICKUP_ANCHOR)
            .expect("the prompt must open with PICKUP_ANCHOR")
            .1;
        assert!(
            after.starts_with("RT-42"),
            "the card id must be the first token after the anchor: {}",
            &after[..after.len().min(80)]
        );
    }

    /// The nudge and the WIP query must agree about whether a capture shell
    /// BLOCKS anything.
    ///
    /// They stopped agreeing the moment AMUX-3757 exempted capture shells from
    /// the cap: the nudge still opened "is a capture shell holding your WIP
    /// slot", which is now false. Caught within the hour, by the nudge being
    /// delivered to the lane that had just written the exemption — which is
    /// luck, not a check.
    ///
    /// A nudge asserting a blockage that does not exist is worse than a silent
    /// one. Its entire persuasive force is "this is blocking you", so a lane
    /// acts on fictional urgency, and the honest reason to dispose of a capture
    /// shell (no status is a true statement about it, so it sits on the board
    /// reading like work forever) goes unsaid.
    ///
    /// Both halves are derived from the SAME card here, so changing either one
    /// alone fails.
    #[test]
    fn the_decompose_nudge_does_not_claim_a_blockage_the_wip_query_exempts() {
        let conn = board_db();
        add_card(&conn, "C-9", "lane", "doing", "Why are you stopping", "**Prompt:** why are you stopping?");
        conn.execute("UPDATE issues SET creator='amux' WHERE id='C-9'", []).expect("mint");
        add_card(&conn, "T-9", "lane", "todo", "real work", "SCOPE: x\n- [ ] y");

        // The MECHANISM: it does not hold the slot.
        let blocks_pickup = matches!(
            select_pickup_with(&conn, "lane", now_f64(), false),
            Pickup::None { reason: "wip-cap", .. }
        );
        assert!(!blocks_pickup, "AMUX-3757: a capture shell must not hold the WIP slot");

        // The VIEW: so it must not say it does.
        let Advance::Nudge { text, kind, .. } = select_advance(&conn, "lane", &[], now_f64()) else {
            panic!("a capture shell in doing must still draw a decompose nudge");
        };
        assert_eq!(kind, "decompose-asked");
        assert!(
            !text.to_lowercase().contains("wip slot"),
            "the nudge claims a blockage the pickup query exempts: {text}"
        );
        // And it must still give the lane somewhere to GO, or removing the
        // false claim just leaves an unmotivated chore.
        //
        // The exits, not the prose. My first version asserted the literal
        // phrase "done or not-done" and went red on 03ed2b6c's compression,
        // which says the same thing in different words. That is the coupling my
        // own notes warn about — a text assertion measures wording, not
        // behaviour — and the sibling test one screen down had already learned
        // it: "a wording rewrite must be free; an exit losing its command must
        // not be" (AMUX-3707).
        for cmd in [
            "amux board discard",
            "amux board retitle",
            "amux board decompose",
        ] {
            assert!(
                text.contains(cmd),
                "the ask must still reach its exit `{cmd}`: {text}"
            );
        }
    }

    /// A few bounced cards must not freeze unrelated ready work lane-wide.
    #[test]
    fn bounced_cards_do_not_freeze_unrelated_ready_work() {
        let conn = board_db();
        let now = now_f64();
        for n in 1..=3 {
            add_card(&conn, &format!("B-{n}"), "lane", "todo", "bounced back", "SCOPE: x\n- [ ] y");
            conn.execute(
                "INSERT INTO session_events (session, type, ts, data) VALUES ('lane','task.claimed',?1,?2)",
                rusqlite::params![now - 30.0 * n as f64, format!("{{\"issue\":\"B-{n}\",\"status\":\"doing\"}}")],
            )
            .expect("claim event");
        }
        add_card(&conn, "T-1", "lane", "todo", "fresh work", "SCOPE: x\n- [ ] y");
        assert_eq!(claimed(&select_pickup_with(&conn, "lane", now, false)), Some("T-1"));
    }

    /// Ethan/backend 2026-08-11: BACKE-3249 sat `doing` + needs:you for 31
    /// hours, holding the lane's whole WIP-1 budget while 28 eligible todos
    /// waited behind it. A card blocked on a HUMAN is parked, not in
    /// progress — idling the lane does not answer the human faster. The
    /// control half: the needs:you card itself must never be re-dispatched
    /// (that exemption already exists in the candidate query; this asserts
    /// releasing WIP did not loosen it).
    #[test]
    fn a_needs_you_doing_card_does_not_hold_wip() {
        let conn = board_db();
        let now = now_f64();
        add_card(&conn, "D-1", "lane", "doing", "waiting on Ethan", "SCOPE: x");
        tag(&conn, "D-1", "needs:you", now - 31.0 * 3600.0);
        add_card(&conn, "T-1", "lane", "todo", "next real work", "SCOPE: x\n- [ ] y");
        assert_eq!(
            claimed(&select_pickup_with(&conn, "lane", now, false)),
            Some("T-1"),
            "a human-blocked doing card must not idle the lane"
        );
        // Sub-tagged asks are the same state (the LIKE form both loops share).
        let conn2 = board_db();
        add_card(&conn2, "D-2", "lane", "doing", "waiting on a decision", "SCOPE: x");
        tag(&conn2, "D-2", "needs:you:decision", now);
        add_card(&conn2, "T-2", "lane", "todo", "next", "SCOPE: x\n- [ ] y");
        assert_eq!(
            claimed(&select_pickup_with(&conn2, "lane", now, false)),
            Some("T-2")
        );
    }

    /// An armed tripwire "costs nothing until it fires" and can never be
    /// completed by working it — one held a lane's entire WIP-1 budget.
    #[test]
    fn dormant_types_neither_hold_wip_nor_get_dispatched() {
        let conn = board_db();
        add_card(&conn, "W-1", "lane", "doing", "watch prod", "SCOPE: x");
        conn.execute("UPDATE issues SET type='watch' WHERE id='W-1'", []).expect("type");
        add_card(&conn, "W-2", "lane", "todo", "tripwire card", "SCOPE: x");
        conn.execute("UPDATE issues SET type='tripwire' WHERE id='W-2'", []).expect("type");
        add_card(&conn, "T-1", "lane", "todo", "real", "SCOPE: x\n- [ ] y");
        assert_eq!(
            claimed(&select_pickup_with(&conn, "lane", now_f64(), false)),
            Some("T-1"),
            "the watch must not hold WIP and the tripwire must not be dispatched"
        );
    }

    /// Ethos rule 8: a session-tagged HUMAN commitment is queued to the lane for
    /// visibility only. Silently executing it is an agent deciding a person's
    /// work is its own (AMUX-1471).
    #[test]
    fn a_human_owned_card_is_never_auto_run() {
        let conn = board_db();
        add_card(&conn, "H-1", "lane", "todo", "Ethan: call the bank", "SCOPE: x\n- [ ] y");
        conn.execute("UPDATE issues SET owner_type='human' WHERE id='H-1'", []).expect("owner");
        assert!(claimed(&select_pickup_with(&conn, "lane", now_f64(), false)).is_none());
        assert_eq!(eligible_todo_count(&conn, "lane", now_f64()), 0);
    }

    /// py:14515, AMUX-1857: sessions legitimately return owner-blocked cards to
    /// todo after doing the workable prep; without the cooldown auto-pickup
    /// re-claimed the same card at the very next idle — infinite churn.
    #[test]
    fn a_recently_claimed_card_cools_down_but_the_dead_zone_is_closed() {
        // Brief per-card anti-spin window, never a multi-hour hidden state.
        let conn = board_db();
        add_card(&conn, "T-1", "lane", "todo", "returned", "SCOPE: x\n- [ ] y");
        let claim_at = |ts: f64| {
            conn.execute("DELETE FROM session_events WHERE data LIKE '%T-1%'", []).ok();
            conn.execute(
                "INSERT INTO session_events (ts,session,type,data,source) \
                 VALUES (?1,'lane','task.claimed','{\"issue\": \"T-1\"}','board-drive')",
                rusqlite::params![ts],
            )
            .expect("event");
        };
        claim_at(now_f64() - 60.0);
        assert!(
            claimed(&select_pickup_with(&conn, "lane", now_f64(), false)).is_none(),
            "a card claimed 1m ago is inside the brief cooldown and must not re-deal"
        );
        claim_at(now_f64() - 10.0 * 60.0);
        assert_eq!(
            claimed(&select_pickup_with(&conn, "lane", now_f64(), false)),
            Some("T-1"),
            "a card claimed 10m ago must not leave the lane stalled"
        );
    }

    /// Age is priority input, not a hidden lifecycle state.
    #[test]
    fn an_old_todo_remains_dispatchable_by_default() {
        let conn = board_db();
        add_card(&conn, "T-1", "lane", "todo", "fossil", "SCOPE: x\n- [ ] y");
        conn.execute(
            "UPDATE issues SET updated=?1 WHERE id='T-1'",
            rusqlite::params![now_f64() as i64 - 8 * 86400],
        )
        .expect("age");
        assert_eq!(claimed(&select_pickup_with(&conn, "lane", now_f64(), false)), Some("T-1"));
    }

    /// py:14581, AMUX-2128: refusals used to RETURN, so one refusable card at the
    /// head of the queue stalled the whole lane — 81 clean todos sat behind
    /// refusable heads. A refusal must try the next candidate.
    #[test]
    fn a_refusable_head_does_not_stall_the_queue() {
        let conn = board_db();
        // pos orders the queue, so the shell is first.
        add_card(&conn, "S-1", "lane", "todo", "shell", "**Prompt:** do a thing for me");
        conn.execute("UPDATE issues SET pos=1 WHERE id='S-1'", []).expect("pos");
        add_card(&conn, "T-1", "lane", "todo", "real", "SCOPE: x\n- [ ] y");
        conn.execute("UPDATE issues SET pos=2 WHERE id='T-1'", []).expect("pos");
        assert_eq!(claimed(&select_pickup_with(&conn, "lane", now_f64(), false)), Some("T-1"));
    }

    #[test]
    fn an_irreversible_operation_is_never_auto_executed() {
        let conn = board_db();
        add_card(
            &conn,
            "T-1",
            "lane",
            "todo",
            "cleanup",
            "SCOPE: x\n- [ ] run git reset --hard on the shared checkout",
        );
        assert!(claimed(&select_pickup_with(&conn, "lane", now_f64(), false)).is_none());
    }

    #[test]
    fn a_card_blocked_by_an_open_dependency_is_skipped_and_a_closed_one_is_not() {
        let conn = board_db();
        add_card(&conn, "B-1", "other", "doing", "blocker", "SCOPE: x");
        add_card(&conn, "T-1", "lane", "todo", "dependent", "SCOPE: x\n- [ ] y");
        conn.execute("UPDATE issues SET depends_on='[\"B-1\"]' WHERE id='T-1'", []).expect("dep");
        assert!(claimed(&select_pickup_with(&conn, "lane", now_f64(), false)).is_none());
        conn.execute("UPDATE issues SET status='verified' WHERE id='B-1'", []).expect("close");
        assert_eq!(claimed(&select_pickup_with(&conn, "lane", now_f64(), false)), Some("T-1"));
    }

    /// When a todo card is blocked only by the same session's own backlog cards,
    /// select_pickup returns PromoteDeps so drive_lane can promote them and
    /// dispatch the todo on the next tick — no human or cross-lane action needed.
    /// Mirrors the live byo-ray incident (2026-08-30): BR-10 blocked by BR-9
    /// (own backlog), budget spent, lane idle indefinitely.
    #[test]
    fn own_backlog_dep_returns_promote_deps() {
        let conn = board_db();
        // Same session owns both cards: the backlog dep and the todo that needs it.
        add_card(&conn, "D-1", "lane", "backlog", "prerequisite work", "SCOPE: x");
        add_card(&conn, "D-2", "lane", "todo", "downstream work", "SCOPE: x\n- [ ] y");
        conn.execute("UPDATE issues SET depends_on='[\"D-1\"]' WHERE id='D-2'", [])
            .expect("dep");
        match select_pickup_with(&conn, "lane", now_f64(), false) {
            Pickup::PromoteDeps { blocked_card, promoted } => {
                assert_eq!(blocked_card, "D-2");
                assert_eq!(promoted, vec!["D-1"]);
            }
            other => panic!("expected PromoteDeps, got {other:?}"),
        }
    }

    /// If the dep is another session's card (even if backlog), it is NOT
    /// self-resolvable — select_pickup must leave it as all-candidates-refused.
    #[test]
    fn other_session_backlog_dep_does_not_promote() {
        let conn = board_db();
        add_card(&conn, "X-1", "other-lane", "backlog", "other session dep", "SCOPE: x");
        add_card(&conn, "X-2", "lane", "todo", "blocked card", "SCOPE: x\n- [ ] y");
        conn.execute("UPDATE issues SET depends_on='[\"X-1\"]' WHERE id='X-2'", [])
            .expect("dep");
        match select_pickup_with(&conn, "lane", now_f64(), false) {
            Pickup::None { reason, .. } => {
                assert_eq!(reason, "all-candidates-refused");
            }
            other => panic!("expected None/all-candidates-refused, got {other:?}"),
        }
    }

    /// ATE-37 / live mvs-research acceptance case. A todo that exists but
    /// cannot run is not queued work; it must not prevent an opted-in lane from
    /// advancing independent backlog work. MR-14 was blocked on human-owned
    /// MR-27 while MR-150 and other backlog work were independent, and the lane would
    /// otherwise stop forever after its current card completed.
    #[test]
    fn a_blocked_todo_does_not_hide_actionable_backlog_from_auto_drain() {
        let conn = board_db();
        let now = now_f64();
        add_card(&conn, "H-1", "human", "todo", "external prerequisite", "SCOPE: x");
        add_card(&conn, "T-1", "lane", "todo", "blocked todo", "SCOPE: x\n- [ ] y");
        conn.execute("UPDATE issues SET depends_on='[\"H-1\"]' WHERE id='T-1'", [])
            .expect("dep");
        add_card(&conn, "B-0", "lane", "backlog", "older blocked backlog", "SCOPE: x\n- [ ] y");
        conn.execute(
            "UPDATE issues SET depends_on='[\"H-1\"]', created=1 WHERE id='B-0'",
            [],
        )
        .expect("blocked backlog dep");
        add_card(&conn, "B-1", "lane", "backlog", "independent backlog", "SCOPE: x\n- [ ] y");
        conn.execute("UPDATE issues SET created=2 WHERE id='B-1'", [])
            .expect("backlog order");

        match select_pickup_with(&conn, "lane", now, false) {
            Pickup::DrainBacklog { card, backlog_left, todo_refusals, .. } => {
                assert_eq!(card, "B-1");
                assert_eq!(backlog_left, 1);
                assert_eq!(todo_refusals, 1, "the trace must expose the blocked todo bypass");
            }
            other => panic!("blocked todo must not strand independent backlog: {other:?}"),
        }
    }

    /// Invariant 20: silence is the correct output for an empty board, and the
    /// trace must SAY so rather than being absent.
    #[test]
    fn an_empty_queue_produces_a_reason_not_a_silent_return() {
        let conn = board_db();
        match select_pickup_with(&conn, "lane", now_f64(), false) {
            Pickup::None { reason, detail } => {
                assert_eq!(reason, "no-eligible-card");
                assert!(!detail.is_empty(), "an empty board must still explain itself");
            }
            _ => panic!("nothing should have been claimed"),
        }
    }

    #[test]
    fn every_shell_in_the_queue_produces_a_decompose_ask_not_silence() {
        let conn = board_db();
        add_card(&conn, "S-1", "lane", "todo", "shell one", "**Prompt:** please do a thing");
        add_card(&conn, "S-2", "lane", "todo", "shell two", "capture: session prompt");
        match select_pickup_with(&conn, "lane", now_f64(), false) {
            Pickup::Decompose { ids, text } => {
                assert_eq!(ids.len(), 2);
                assert!(text.contains("S-1") && text.contains("S-2"), "must name the ids: {text}");
            }
            _ => panic!("an all-shell queue must ask for a decomposition"),
        }
    }

    // --- advance ---------------------------------------------------------

    #[test]
    fn a_lane_holding_a_doing_card_is_nudged_with_the_gate_for_the_next_status() {
        let conn = board_db();
        add_card(&conn, "D-1", "lane", "doing", "the work", "SCOPE: x\n- [ ] y");
        match select_advance(&conn, "lane", &[], now_f64()) {
            Advance::Nudge { target, card, text, kind, .. } => {
                assert_eq!(target, "lane");
                assert_eq!(card, "D-1");
                assert_eq!(kind, "advance-nudged");
                assert!(text.contains("D-1"), "must name the card: {text}");
                assert!(text.contains("review"), "must name the next status: {text}");
            }
            Advance::None { reason, detail } => panic!("expected a nudge, got {reason}: {detail}"),
        }
    }

    /// py:13375: 182 advance wakes in 24h, one card nudged nine times and then
    /// discarded. After the budget the loop goes quiet FOR THAT CARD.
    #[test]
    fn the_per_card_budget_silences_that_card_but_not_the_lane() {
        let conn = board_db();
        add_card(&conn, "D-1", "lane", "doing", "stuck", "SCOPE: x");
        add_card(&conn, "D-2", "lane", "review", "other", "SCOPE: x");
        for _ in 0..3 {
            conn.execute(
                "INSERT INTO session_events (ts,session,type,data,source) \
                 VALUES (?1,'lane','advance.nudged','{\"issue\": \"D-1\"}','board-drive')",
                rusqlite::params![now_f64() - 60.0],
            )
            .expect("event");
        }
        // The lane's cooldown would normally suppress this; age the events past it.
        conn.execute(
            "UPDATE session_events SET ts=?1",
            rusqlite::params![now_f64() - ADVANCE_COOLDOWN_S - 60.0],
        )
        .expect("age");
        match select_advance(&conn, "lane", &[], now_f64()) {
            Advance::Nudge { card, .. } => assert_eq!(card, "D-2", "must fall through to the next card"),
            Advance::None { reason, detail } => panic!("expected D-2, got {reason}: {detail}"),
        }
    }

    /// AMUX-2270: a session that DID what option 4 told it to do — record the
    /// human blocker on the card — got nagged again the next night, and the
    /// night after. An instruction you can comply with and still be re-asked is
    /// the ethos rule 3 shape.
    #[test]
    fn a_fresh_needs_you_card_is_quiet_not_nudged() {
        let conn = board_db();
        add_card(&conn, "D-1", "lane", "doing", "asked Ethan", "SCOPE: x");
        tag(&conn, "D-1", "needs:you", now_f64() - 3600.0);
        match select_advance(&conn, "lane", &[], now_f64()) {
            Advance::None { reason, .. } => assert_eq!(reason, "needsyou"),
            Advance::Nudge { text, .. } => panic!("must stay quiet on a fresh ask: {text}"),
        }
    }

    #[test]
    fn a_stale_needs_you_ask_gets_exactly_one_renag_per_window() {
        let conn = board_db();
        add_card(&conn, "D-1", "lane", "doing", "asked Ethan", "SCOPE: x");
        let asked = now_f64() - 10.0 * 86400.0;
        tag(&conn, "D-1", "needs:you", asked);
        conn.execute("UPDATE issues SET updated=?1 WHERE id='D-1'", rusqlite::params![asked as i64])
            .expect("age");
        let first = select_advance(&conn, "lane", &[], now_f64());
        let Advance::Nudge { kind, .. } = &first else {
            panic!("a 10-day-old ask must be re-nagged once");
        };
        assert_eq!(*kind, "renag");
        // Record the fire the way drive_lane does, then assert the dedupe holds.
        conn.execute(
            "INSERT INTO session_events (ts,session,type,data,source) \
             VALUES (?1,'lane','needsyou.renag','{\"issue\": \"D-1\"}','board-drive')",
            rusqlite::params![now_f64()],
        )
        .expect("event");
        match select_advance(&conn, "lane", &[], now_f64() + 1.0) {
            Advance::None { .. } => {}
            Advance::Nudge { text, .. } => panic!("re-nag must not repeat inside the window: {text}"),
        }
    }

    /// AF-465 REVERSED the old py:12979 behaviour: re-stating a needs:you card no
    /// longer silences the re-nag. Re-statement as a silence lever was an abuse
    /// vector (a lane could keep a human's ask quiet forever by touching the card
    /// each window, and any desc write tripped it), and it silenced only after the
    /// first re-nag — so the escape the old text advertised did not work the one
    /// time a lane tried it (AF-111). Now: MIN(added_at) is the monotonic ask
    /// clock, the cooldown is the only silence, and a re-stated but still-open ask
    /// past the window IS re-nagged.
    /// AF-552, reported by gtm-engine with two live instances (GE-537, GE-583).
    /// The nudge disclosed "This card is ARCHIVED" and then prescribed "clear
    /// needs:you and move the card" — which the board refuses on an archived
    /// card with `archived_task_immutable`. Adjacent sentences, contradicting.
    /// `amux board unarchive` had existed since AMUX-2492 and this text never
    /// named it, so gtm-engine cleared theirs by hand-PATCHing {"archived":0}.
    #[test]
    fn an_archived_needs_you_nudge_names_the_unarchive_step_it_requires() {
        let conn = board_db();
        add_card(&conn, "D-2", "lane", "doing", "asked Ethan", "SCOPE: x");
        tag(&conn, "D-2", "needs:you", now_f64() - 10.0 * 86400.0);

        let arch = needsyou_renag_text(&conn, "lane", "D-2", "t", 10.0 * 86400.0, 1, now_f64())
            .expect("an archived needs:you card is still re-nagged");
        assert!(arch.contains("ARCHIVED"), "it must still disclose the state: {arch}");
        assert!(
            arch.contains("amux board unarchive D-2"),
            "prescribing the move without the step that unblocks it is the defect: {arch}"
        );
        assert!(
            arch.contains("archived_task_immutable"),
            "name the refusal the lane will otherwise hit, so the recipe is checkable: {arch}"
        );

        // And the NON-archived nudge must not grow the instruction — a card that
        // is not archived needs no unarchive step, and a recipe that appears
        // when it does not apply is the same class of wrong.
        let plain = needsyou_renag_text(&conn, "lane", "D-2", "t", 10.0 * 86400.0, 0, now_f64())
            .expect("a live needs:you card is re-nagged too");
        assert!(!plain.contains("unarchive"), "not applicable here: {plain}");
        assert!(!plain.contains("ARCHIVED"), "not applicable here: {plain}");
    }

    #[test]
    fn re_stating_a_needs_you_card_no_longer_silences_the_renag() {
        let conn = board_db();
        add_card(&conn, "D-1", "lane", "doing", "asked Ethan", "SCOPE: x");
        tag(&conn, "D-1", "needs:you", now_f64() - 10.0 * 86400.0);
        // Last re-nag was a full window+ ago (cooldown expired), then the card was
        // re-stated one minute after that fire.
        let renagged_at = now_f64() - 10.0 * 86400.0 + 1.0;
        conn.execute(
            "INSERT INTO session_events (ts,session,type,data,source) \
             VALUES (?1,'lane','needsyou.renag','{\"issue\": \"D-1\"}','board-drive')",
            rusqlite::params![renagged_at],
        )
        .expect("event");
        conn.execute(
            "UPDATE issues SET updated=?1 WHERE id='D-1'",
            rusqlite::params![(renagged_at + 60.0) as i64],
        )
        .expect("restate");
        let text = needsyou_renag_text(&conn, "lane", "D-1", "t", 10.0 * 86400.0, 0, now_f64());
        assert!(
            text.is_some(),
            "a re-stated ask past the cooldown MUST still be re-nagged (AF-465): re-statement is not a silence lever"
        );
        let text = text.unwrap();
        // And the message must not promise the removed escape, nor claim an
        // unverified owner-digest reminder.
        assert!(
            !text.contains("silences this"),
            "the re-nag must not promise re-statement silences it: {text}"
        );
        assert!(
            text.contains("OVERTAKEN"),
            "the re-nag must keep the one real lane action — clear an overtaken ask: {text}"
        );
    }

    /// AMUX-3751's gate, which shipped with no test of its own and whose only
    /// effect on the suite was turning three unrelated routing tests red.
    ///
    /// A card parked in `review` naming a reviewer that resolves to no worker
    /// reads HEALTHY — the status says someone is looking at it — while the
    /// nudge is addressed to a session that does not exist, so nobody is
    /// coming. Seven open cards sat that way on 2026-08-26, two of them naming
    /// `amux-rust`, a lane renamed to `amux` long before, because the rename
    /// cascade migrated `issues.session` and left `issues.reviewer` behind.
    ///
    /// Both cells, over the SAME card: the only thing that changes is whether
    /// the reviewer is reachable. A gate tested only on its refusal could be
    /// `|_| false` and look identical here.
    #[test]
    fn the_gate_refuses_a_reviewer_no_nudge_can_reach() {
        let conn = board_db();
        add_card(&conn, "R-9", "lane", "review", "needs a peer", "SCOPE: x");
        conn.execute("UPDATE issues SET reviewer='amux-rust' WHERE id='R-9'", []).expect("rev");

        // AMUX-3821: the guarantee here is that no nudge is addressed to a
        // session that does not exist. It used to be enforced by nudging NOBODY,
        // which left the one party who can fix it — the owner, who wrote the
        // reviewer field — as the only party never told. The card then read
        // `review` while waiting on nobody. So the assertion is sharpened
        // rather than relaxed: the dead reviewer must not be the target, AND
        // the owner must be.
        match select_advance_with(&conn, "lane", &[], now_f64(), &|_, r| {
            Some(format!("`{r}` is not a registered worker"))
        }) {
            Advance::Nudge { target, text, .. } => {
                assert_ne!(target, "amux-rust", "never address a session that does not exist");
                assert_eq!(target, "lane", "the OWNER wrote the field and is the one who can fix it");
                assert!(text.contains("amux-rust"), "must name the dead reviewer: {text}");
                assert!(
                    text.contains("not a registered worker"),
                    "and must carry the REASON, which already names both ways out: {text}"
                );
            }
            Advance::None { reason, detail } => {
                panic!("silence is what left the author untold ({reason}: {detail})")
            }
        }

        match select_advance_with(&conn, "lane", &[], now_f64(), &|_, n| {
            (n != "amux-rust").then(|| "not reachable".to_string())
        }) {
            Advance::Nudge { target, kind, .. } => {
                assert_eq!(target, "amux-rust", "a REACHABLE reviewer is still routed");
                assert_eq!(kind, "review-routed");
            }
            Advance::None { reason, detail } => panic!("the gate must only refuse the unreachable case, got {reason}: {detail}"),
        }
    }

    /// A card in review with a named reviewer is the REVIEWER's work: pushing
    /// the author asks for a self-ack the transition refuses.
    #[test]
    fn a_review_card_routes_to_the_reviewer_not_the_author() {
        let conn = board_db();
        add_card(&conn, "R-1", "lane", "review", "needs a peer", "SCOPE: x");
        conn.execute("UPDATE issues SET reviewer='peer' WHERE id='R-1'", []).expect("rev");
        match select_advance(&conn, "lane", &[], now_f64()) {
            Advance::Nudge { target, kind, text, .. } => {
                assert_eq!(target, "peer", "the reviewer is the one who can act");
                assert_eq!(kind, "review-routed");
                assert!(text.contains("review->done"), "must name the real transition: {text}");
            }
            Advance::None { reason, detail } => panic!("expected routing, got {reason}: {detail}"),
        }
    }

    /// AC-234: blocking a card IS a completed review action but leaves it in
    /// `review`, so without this the sweep re-nudges a reviewer whose analysis is
    /// already on the card — and AMUX-2498: the conclusion "the author owes the
    /// next move" must be ACTED on, not thrown away.
    /// AF-154. The test ABOVE inserts the interaction_log row itself, so it
    /// manufactures the precondition production has lacked since 2026-08-09
    /// and is structurally blind to the empty table — it passed for two weeks
    /// while the function it covers was guessing. This is the cell it could
    /// not be: NO board row anywhere (the live shape), a message from the
    /// reviewer that merely NAMES the card, and the demand that the sweep does
    /// NOT conclude the reviewer responded.
    #[test]
    fn a_message_naming_the_card_is_not_a_review_response_when_the_board_channel_is_empty() {
        let conn = board_db();
        add_card(&conn, "R-2", "lane", "review", "needs a peer", "SCOPE: x");
        conn.execute("UPDATE issues SET reviewer='peer' WHERE id='R-2'", []).expect("rev");
        // The reviewer names the card in a message — an announcement, a
        // hand-off, a status ping. Not a review action.
        conn.execute(
            "INSERT INTO cmd_history (text,type,session,ts,origin) \
             VALUES ('filed R-2 for you, taking a look later','direct','lane',?1,'peer')",
            rusqlite::params![chrono::Utc::now().timestamp_millis()],
        )
        .expect("seed the reviewer message");
        // PROVE THE FIXTURE IS REAL before asserting on its absence. My first
        // cut used .ok() on an INSERT with the wrong column names; it failed
        // silently, so the assertion below held for the trivial reason that no
        // message existed — and the mutation check caught it passing against
        // the pre-fix behaviour. A fixture I built is a claim, not a premise.
        assert!(
            reviewer_msg_engagement(&conn, "R-2", "peer") > 0,
            "fixture broken: the seeded message must be visible to the engagement probe, \
             or this test asserts nothing"
        );
        assert_eq!(
            reviewer_has_responded(&conn, "R-2", "peer"),
            None,
            "with NO board evidence, a message naming the card must not read as a response — \
             that is what left a review gate waiting on nothing"
        );

        // CONTROL, or the assertion above passes for a function that always
        // returns None: the SAME message plus a real board write by the
        // reviewer must still be detected.
        conn.execute(
            "INSERT INTO interaction_log (ts,kind,actor,target,action)              VALUES (?1,'board','peer','R-2','patch')",
            rusqlite::params![chrono::Utc::now().timestamp_millis()],
        )
        .expect("ilog");
        assert!(
            reviewer_has_responded(&conn, "R-2", "peer").is_some(),
            "a real board write by the reviewer must still count"
        );
    }

    #[test]
    fn a_reviewer_who_already_responded_is_not_renudged_and_the_author_is_told() {
        let conn = board_db();
        add_card(&conn, "R-1", "lane", "review", "needs a peer", "SCOPE: x");
        conn.execute("UPDATE issues SET reviewer='peer' WHERE id='R-1'", []).expect("rev");
        conn.execute(
            "INSERT INTO interaction_log (ts,kind,actor,target,action) \
             VALUES (?1,'board','peer','R-1','patch')",
            rusqlite::params![chrono::Utc::now().timestamp_millis()],
        )
        .expect("ilog");
        match select_advance(&conn, "lane", &[], now_f64()) {
            Advance::Nudge { target, kind, text, .. } => {
                assert_eq!(target, "lane", "the ball is with the author");
                assert_eq!(kind, "advance-nudged");
                assert!(
                    text.contains("already responded"),
                    "the author must be told the reviewer answered: {text}"
                );
            }
            Advance::None { reason, detail } => panic!("expected an author nudge, got {reason}: {detail}"),
        }
    }

    /// AMUX-2479: reviewer acks routinely arrive as inter-session MESSAGES, and a
    /// check that read board writes only re-nudged reviewers who had done the
    /// work. ENGAGEMENT, not approval — a round-1 BLOCK counts.
    #[test]
    fn a_reviewer_ack_delivered_as_a_message_counts_as_engagement() {
        let conn = board_db();
        conn.execute(
            "INSERT INTO cmd_history (text,type,session,ts,origin) \
             VALUES ('R-1 is blocked: three things must move back','direct','lane',?1,'peer')",
            rusqlite::params![chrono::Utc::now().timestamp_millis()],
        )
        .expect("msg");
        assert!(reviewer_msg_engagement(&conn, "R-1", "peer") > 0);
        // AC-316 defect 2: EXACT lane, never a prefix. `amux-cloud`'s message
        // must not read as engagement by the reviewer `amux`.
        assert_eq!(reviewer_msg_engagement(&conn, "R-1", "peer-cloud"), 0);
        // Word boundary: R-1 must not match R-1000.
        assert_eq!(reviewer_msg_engagement(&conn, "R-10", "peer"), 0);
    }

    /// AC-298: owning a card is not the same as being able to advance it. This
    /// nudge fired EIGHT times for one card whose blocker was in review awaiting
    /// someone else's sign-off — asking for something no honest action of the
    /// recipient's could produce.
    #[test]
    fn a_dependency_the_lane_cannot_act_on_is_not_nudged() {
        let conn = board_db();
        add_card(&conn, "B-1", "lane", "review", "blocker in review", "SCOPE: x");
        conn.execute("UPDATE issues SET reviewer='peer' WHERE id='B-1'", []).expect("rev");
        add_card(&conn, "D-1", "lane", "doing", "dependent", "SCOPE: x");
        conn.execute("UPDATE issues SET depends_on='[\"B-1\"]' WHERE id='D-1'", []).expect("dep");
        // `doing` sorts ahead of `review`, so D-1 is the selected candidate.
        match select_advance(&conn, "lane", &[], now_f64()) {
            Advance::None { reason, detail } => {
                assert_eq!(reason, "dep-not-actionable");
                assert!(detail.contains("B-1"), "{detail}");
            }
            Advance::Nudge { text, .. } => panic!("must not nudge an unactionable dependency: {text}"),
        }
    }

    /// GCA-91: the dependency nudge told an agent to "drive its blocker through
    /// the gates" while the blocker was `needsyou` — parked on a human, so no
    /// action of the agent's could satisfy it. It must be suppressed, like the
    /// other unactionable-blocker cases. (The code also checks the `needs:you`
    /// TAG for the window where the blocker's own re-nag is on cooldown; that
    /// path is not unit-tested in isolation because a same-lane needs:you card is
    /// otherwise intercepted by its own re-nag before the dependent is reached.)
    #[test]
    fn a_dependency_parked_on_a_human_is_not_nudged() {
        let conn = board_db();
        add_card(&conn, "B-1", "lane", "needsyou", "waiting on Ethan", "SCOPE: x");
        add_card(&conn, "D-1", "lane", "doing", "dependent", "SCOPE: x");
        conn.execute("UPDATE issues SET depends_on='[\"B-1\"]' WHERE id='D-1'", []).expect("dep");
        match select_advance(&conn, "lane", &[], now_f64()) {
            Advance::None { reason, detail } => {
                assert_eq!(reason, "dep-needsyou");
                assert!(detail.contains("B-1") && detail.contains("needs:you"), "{detail}");
            }
            Advance::Nudge { text, .. } => panic!("must not nudge a human-parked dependency: {text}"),
        }
    }

    /// AMUX-2500: the cap stays on saying the same thing again and LIFTS on
    /// continuing. A lane that moved the card we named has not stalled.
    #[test]
    fn progress_yields_the_cooldown_but_repetition_does_not() {
        let conn = board_db();
        add_card(&conn, "D-1", "lane", "doing", "the work", "SCOPE: x");
        conn.execute(
            "INSERT INTO session_events (ts,session,type,data,source) VALUES \
             (?1,'lane','advance.nudged','{\"issue\":\"D-1\",\"status\":\"doing\"}','board-drive')",
            rusqlite::params![now_f64() - 60.0],
        )
        .expect("event");
        match select_advance(&conn, "lane", &[], now_f64()) {
            Advance::None { reason, .. } => assert_eq!(reason, "cooldown"),
            Advance::Nudge { text, .. } => panic!("inside the cooldown with no progress: {text}"),
        }
        // The lane moved it. The next card should come immediately.
        conn.execute("UPDATE issues SET status='review' WHERE id='D-1'", []).expect("move");
        match select_advance(&conn, "lane", &[], now_f64()) {
            Advance::Nudge { card, .. } => assert_eq!(card, "D-1"),
            Advance::None { reason, detail } => panic!("progress must yield the cooldown, got {reason}: {detail}"),
        }
    }

    /// ATE-45, live PRIMI-204. The preceding capture shell had just been
    /// nudged, so its unchanged status put the whole Primis lane on cooldown.
    /// A new prompt explicitly saying "do not create or retain a board task"
    /// was then captured as a second `doing` card and never received the one
    /// model-judgment prompt that could discard it. The exception is narrow:
    /// it is per-card, only for the shared junk classifier, and the durable
    /// idem closes it immediately after the first delivery.
    #[test]
    fn a_new_capture_cleanup_bypasses_an_unrelated_lane_cooldown_once() {
        let conn = board_db();
        let now = now_f64();
        add_card(&conn, "PRIMI-203", "primis", "doing", "prior work", "SCOPE: real work");
        conn.execute(
            "INSERT INTO session_events (ts,session,type,data,source) VALUES \
             (?1,'primis','advance.nudged',\
              '{\"issue\":\"PRIMI-203\",\"status\":\"doing\"}','board-drive')",
            rusqlite::params![now - 60.0],
        )
        .expect("prior lane nudge");
        add_card(
            &conn,
            "PRIMI-204",
            "primis",
            "doing",
            "Post-fix status verification only",
            "**Prompt:** Post-fix status verification only; do not create or retain a board task.",
        );
        conn.execute("UPDATE issues SET source='capture' WHERE id='PRIMI-204'", [])
            .expect("capture source");

        match select_advance(&conn, "primis", &[], now) {
            Advance::Nudge { card, kind, .. } => {
                assert_eq!(card, "PRIMI-204", "the new shell, not the cooldown holder");
                assert_eq!(kind, "decompose-asked");
            }
            Advance::None { reason, detail } => {
                panic!("new capture cleanup must bypass unrelated cooldown: {reason}: {detail}")
            }
        }

        conn.execute(
            "INSERT INTO session_events (ts,session,type,data,source,idem) VALUES \
             (?1,'primis','advance.nudged',\
              '{\"issue\":\"PRIMI-204\",\"status\":\"doing\"}',\
              'board-drive','decompose:PRIMI-204')",
            rusqlite::params![now],
        )
        .expect("durable per-card cleanup event");
        match select_advance(&conn, "primis", &[], now + 1.0) {
            Advance::None { reason, .. } => assert_eq!(reason, "cooldown"),
            Advance::Nudge { text, .. } => panic!("the same shell must not be prompted twice: {text}"),
        }
    }

    /// REBUILT FROM THE LIVE SPECIMEN, not a convenient fixture: amux-agent was
    /// routed AC-233 at 22:07:14, 22:08:14 and 22:09:14 — one per tick, stopping
    /// only when the per-CARD budget hit 3. The per-LANE cooldown never applied
    /// because the event is recorded under the REVIEWER while the lane that
    /// re-selects the card is the OWNER. Three nudges is the 24h budget, spent
    /// in three minutes.
    #[test]
    fn routing_a_review_quiets_the_owner_lane_for_the_cooldown() {
        let conn = board_db();
        add_card(&conn, "R-1", "lane", "review", "needs a peer", "SCOPE: x");
        conn.execute("UPDATE issues SET reviewer='peer' WHERE id='R-1'", []).expect("rev");
        let Advance::Nudge { target, kind, .. } = select_advance(&conn, "lane", &[], now_f64())
        else {
            panic!("expected the first route");
        };
        assert_eq!((target.as_str(), kind), ("peer", "review-routed"));
        // Record it the way drive_lane does: advance.nudged under the REVIEWER
        // (budget + reviewer cooldown) and advance.routed under the OWNER.
        conn.execute(
            "INSERT INTO session_events (ts,session,type,data,source) VALUES \
             (?1,'peer','advance.nudged','{\"issue\":\"R-1\",\"status\":\"review\"}','board-drive'), \
             (?1,'lane','advance.routed','{\"issue\":\"R-1\",\"status\":\"review\"}','board-drive')",
            rusqlite::params![now_f64()],
        )
        .expect("events");
        match select_advance(&conn, "lane", &[], now_f64() + 60.0) {
            Advance::None { reason, .. } => assert_eq!(reason, "cooldown"),
            Advance::Nudge { target, .. } => {
                panic!("the owner lane must not re-route on the next tick (target {target})")
            }
        }
    }

    /// A board-drive message must never be the thing that wedges a lane's queue.
    /// Built from the live specimen: amux-rust had 10 steering messages stuck 4
    /// hours behind a head-of-line message carrying
    /// `@/Users/ethan/.amux/uploads/x.png`. The send path's refusal is another
    /// lane's fix; not QUOTING a card's @-mention into a prompt is this one's.
    #[test]
    fn a_card_whose_text_opens_the_picker_is_pointed_at_not_quoted() {
        let conn = board_db();
        add_card(
            &conn,
            "T-1",
            "lane",
            "todo",
            "Fix the logo",
            "SCOPE: real work\n- [ ] see @/Users/ethan/.amux/uploads/b7965e0b2a8f-image.png",
        );
        let Pickup::Claim { prompt, .. } = select_pickup_with(&conn, "lane", now_f64(), false) else {
            panic!("the card is otherwise dispatchable and must still be claimed");
        };
        assert!(prompt.contains("T-1"), "the card id must survive: {prompt}");
        assert!(prompt.contains("amux board show T-1"), "must point at the card: {prompt}");
        // ...and the verb must EXIST. Asserting only that we TELL an agent to run
        // a command, while nothing checks the command is real, is how AF-66
        // shipped: `amux board show` fell through to the help text and exited 2,
        // and it is named precisely when the card body is withheld, i.e. when it
        // is the only way to read the card (AMUX-2140's shape).
        assert_cli_verbs_exist(&prompt);
        assert!(
            !crate::api::session_verbs::at_picker_text(&prompt),
            "a board-drive prompt must never open the picker: {prompt}"
        );
        // POSITIVE CONTROL: without an @-mention the desc is quoted in full, so
        // this test is measuring the filter and not a prompt that never quotes.
        let conn2 = board_db();
        add_card(&conn2, "T-2", "lane", "todo", "Fix the logo", "SCOPE: real work\n- [ ] do it");
        let Pickup::Claim { prompt: p2, .. } =
            select_pickup_with(&conn2, "lane", now_f64(), false)
        else {
            panic!("expected a claim");
        };
        assert!(p2.contains("- [ ] do it"), "ordinary card text must be quoted: {p2}");
        assert!(!p2.contains("withheld"));
    }

    /// REBUILT FROM THE INCIDENT'S OWN ARTIFACT, not from a convenient fixture:
    /// the end-to-end run queued `[amux auto-pickup] Claimed board card BDQ-1`
    /// and then, one tick later, `[amux] You went idle holding BDQ-1 in
    /// 'doing'`. A lane must not be told to drive a card it was handed seconds
    /// ago — that is restating an instruction it already has, which is the nudge
    /// shape that does not compound with a better model.
    #[test]
    fn a_lane_just_handed_a_card_is_not_immediately_told_to_advance_it() {
        let conn = board_db();
        add_card(&conn, "BDQ-1", "lane", "doing", "Port the parser guard", "SCOPE: real work");
        conn.execute(
            "INSERT INTO session_events (ts,session,type,data,source) VALUES \
             (?1,'lane','task.claimed','{\"issue\":\"BDQ-1\",\"status\":\"doing\"}','board-drive')",
            rusqlite::params![now_f64() - 5.0],
        )
        .expect("event");
        match select_advance(&conn, "lane", &[], now_f64()) {
            Advance::None { reason, .. } => assert_eq!(reason, "cooldown"),
            Advance::Nudge { text, .. } => {
                panic!("a card claimed 5s ago must not draw an advance nudge: {text}")
            }
        }
        // ...but the claim does not buy silence forever: once the lane MOVES it,
        // progress yields the cooldown exactly as it does after a nudge.
        conn.execute("UPDATE issues SET status='review' WHERE id='BDQ-1'", []).expect("move");
        assert!(
            matches!(select_advance(&conn, "lane", &[], now_f64()), Advance::Nudge { .. }),
            "progress after a claim must still yield the cooldown"
        );
    }

    /// AMUX-2312: telling a lane that deploys nothing to drive every card to
    /// `verified` sets a target whose gate it cannot satisfy truthfully.
    #[test]
    fn verified_is_only_named_for_lanes_it_applies_to() {
        let conn = board_db();
        conn.execute(
            // OR REPLACE: the fixture now carries the REAL migrated schema, which
            // SEEDS the builtin statuses. The hand-rolled one created `statuses`
            // empty, so this plain INSERT used to be the only row — another way
            // the old fixture differed from production without saying so (AF-328).
            "INSERT OR REPLACE INTO statuses (id,label,position,gate,mode) \
             VALUES ('verified','Verified',6,NULL,'explicit')",
            [],
        )
        .expect("status");
        add_card(&conn, "D-1", "lane", "doing", "the work", "SCOPE: x");
        let Advance::Nudge { text, .. } = select_advance(&conn, "lane", &[], now_f64()) else {
            panic!("expected a nudge");
        };
        // ASSERT THE TERM AND ITS ABSENCE, not the sentence around it. The
        // property is which status the nudge AIMS AT; the phrasing carrying it
        // is free to change, and did (03ed2b6c compressed "close it out to
        // done" to "Close to done with evidence"). Pinning the sentence made a
        // wording pass look like an AMUX-2312 regression.
        assert!(text.contains("done"), "must aim at done: {text}");
        assert!(
            !text.contains("verified"),
            "and must NOT name verified for a lane that has not opted in — that is the whole \
             point of AMUX-2312: {text}"
        );
        // Opt the lane in by tag and the aim becomes verified.
        conn.execute(
            "INSERT INTO status_scope (status,scope_type,scope_value) VALUES ('verified','tag','infra')",
            [],
        )
        .expect("scope");
        conn.execute("DELETE FROM session_events", []).expect("clear");
        let Advance::Nudge { text, .. } =
            select_advance(&conn, "lane", &["infra".into()], now_f64())
        else {
            panic!("expected a nudge");
        };
        assert!(text.contains("verified"), "must aim at verified once opted in: {text}");
    }

    #[test]
    fn a_shell_card_in_doing_gets_a_split_ask_not_an_advance_nudge() {
        let conn = board_db();
        add_card(&conn, "S-1", "lane", "doing", "shell", "**Prompt:** please do a thing");
        match select_advance(&conn, "lane", &[], now_f64()) {
            Advance::Nudge { kind, text, .. } => {
                assert_eq!(kind, "decompose-asked");
                // The nudge distinguishes not-work (discard) from
                // real-unfinished-work (promote to epic) — AMUX-3323, e8e648f.
                assert!(text.contains("not a unit of work"), "{text}");
                // Assert the EXITS, by the command that reaches each one, not by
                // the sentence that describes it. This cell used to pin the
                // literal prose ("PROMOTE it to an epic", "discard it"), which
                // measures wording and not behaviour: it was green for months
                // while the epic exit it was guarding named no command at all,
                // because `amux board epic` did not exist (AMUX-3707). A wording
                // rewrite must be free; an exit losing its command must not be.
                for cmd in [
                    "amux board discard",
                    "amux board retitle",
                    "amux board decompose",
                ] {
                    assert!(
                        text.contains(cmd),
                        "the decompose ask must reach its exit with `{cmd}`: {text}"
                    );
                }
                assert_cli_verbs_exist(&text);
                // And it must stay SHORT. The old text was ~250 words re-argued
                // on every one of ~42 daily firings; the cost Ethan flagged is
                // the woken turn, but a lane that can act in one command does not
                // spend a turn working out how. 120 is slack over the current ~85,
                // not a target to grow into.
                let words = text.split_whitespace().count();
                assert!(words <= 120, "the decompose ask has grown back to {words} words: {text}");
            }
            Advance::None { reason, detail } => panic!("expected a split ask, got {reason}: {detail}"),
        }
    }

    // --- predicates ------------------------------------------------------

    #[test]
    fn the_junk_predicate_matches_pythons_specimens() {
        // Structure beats the fold count: a real card with fold RESIDUE.
        let structured = "ROOT CAUSE: the thing\nNew task: a\nNew task: b\nNew task: c";
        assert_eq!(pickup_junk_reason("MG-1328 real investigation", structured, ""), "");
        // A true journal: folds, no structure.
        assert!(pickup_junk_reason("journal", "New task: a\nNew task: b", "").contains("journal card"));
        // GCA-85 + creative-dna: the artifact word must be the SUBJECT.
        assert!(
            pickup_junk_reason("Investigate the canary alerting path", "", "").is_empty(),
            "a card ABOUT a canary is not a canary (GCA-85: three investigation cards fired \
             on a mid-title mention)"
        );
        assert!(pickup_junk_reason("[test-hygiene] flaky suite", "", "").is_empty(), "area prefix is vocabulary");
        assert!(!pickup_junk_reason("[TRIPWIRE, fires on recurrence]", "", "").is_empty());
        assert!(!pickup_junk_reason("probe: is the server up", "", "").is_empty());
        // Shells.
        assert!(pickup_junk_reason("x", "**Prompt:** /compact", "").contains("slash command"));
        assert!(pickup_junk_reason("x", "**Prompt:** go fix the thing", "").contains("captured chat prompt"));
    }

    /// AMUX-3187: `capture: session prompt` is a durable LOG marker, so a card
    /// that was auto-captured and then RESHAPED into a real task keeps it forever.
    /// The brand must read the current DESC, not the log, or the decompose nudge
    /// re-nags a card the session already fixed.
    #[test]
    fn a_reshaped_capture_is_no_longer_branded_a_capture() {
        // POSITIVE CONTROL: a still-raw capture (desc begins "**Prompt:** ", log
        // carries the marker) IS junk. If this ever passed clean the test below
        // would be vacuous.
        let raw_log = "12:00 capture: session prompt";
        assert!(
            pickup_junk_reason("Look up the top VLLM", "**Prompt:** [05:32 PM] look up the top VLLM", raw_log)
                .contains("captured chat prompt"),
            "a still-raw capture must be branded"
        );
        // THE FIX: same log marker, but the desc has been rewritten into a real
        // ops task (AMUX-3185's actual reshaped desc shape). Not junk.
        let reshaped = "Ethan (2026-08-15): find the current top open-weights LLM and pull it into \
                        ollama so it appears in the model picker. Chosen qwen3-coder:30b. \
                        Done when the model shows in GET /api/ollama/models.";
        assert_eq!(
            pickup_junk_reason("Pull qwen3-coder:30b into ollama", reshaped, raw_log),
            "",
            "a card reshaped out of its capture form must NOT be re-branded by the durable log marker"
        );
        // And the marker sitting ONLY in the log never brands on its own.
        assert_eq!(
            pickup_junk_reason("A perfectly normal task", "Do the normal thing.", raw_log),
            "",
            "the capture-origin log marker alone must not brand a card"
        );
    }

    /// AF-569. The brand must fire on a card DEFINED as the capture marker, and not
    /// on one that merely QUOTES it. AF-568 was the specimen: the card about
    /// duplicate capture cards explained the bug with the marker in backticks, and
    /// the unanchored `desc.contains` nudged it "not a unit of work" while it sat in
    /// `doing` with a shipped fix. A card could not describe the capture mechanism
    /// without being classified as its output.
    ///
    /// Both cells are required. `starts_with` alone would also be satisfied by a
    /// check that never fires, so the first assertion is the one that keeps the
    /// second honest.
    #[test]
    fn the_capture_brand_fires_on_the_marker_as_definition_not_as_a_quotation() {
        // STILL BRANDS: the desc IS the marker. This is the intent the code states.
        assert!(
            pickup_junk_reason("shell", "capture: session prompt", "")
                .contains("captured chat prompt"),
            "a card whose desc IS the capture marker must still be branded"
        );

        // THE BUG: the marker QUOTED inside a real write-up. AF-568's actual shape,
        // trimmed. Three ALLCAPS heads, so the structure veto below would clear it
        // too, which the substring check short-circuited by returning first.
        let real_work = "MEASURED, 2026-09-07 (population: every card from that broadcast).\n\
                         There are two capture paths and both write the same durable \
                         `capture: session prompt` log marker, so nothing downstream can \
                         tell them apart.\n\
                         WHAT I CLAIMED: a second mint path.\n\
                         WHY IT IS WRONG: the orchestrator stamps source='orchestrator'.\n\
                         WHAT I DID NOT ESTABLISH: what re-delivered the prompt.";
        assert_eq!(
            pickup_junk_reason("One broadcast minted 105 capture cards", real_work, ""),
            "",
            "a card that QUOTES the capture marker while doing real work must not be branded"
        );
    }

    #[test]
    fn the_prose_dependency_fallback_finds_a_blocker_and_ignores_a_citation() {
        assert_eq!(prose_dependency("blocked by AC-138 until it lands"), Some("AC-138".into()));
        assert_eq!(prose_dependency("depends on MG-1363"), Some("MG-1363".into()));
        // A bare citation is not a dependency.
        assert_eq!(prose_dependency("see AC-138 for context"), None);
    }

    #[test]
    fn norm_actor_flattens_the_real_origin_spellings() {
        assert_eq!(norm_actor("mixpeek-frustrations"), "mixpeek-frustrations");
        assert_eq!(norm_actor("mixpeek frustrations"), "mixpeek-frustrations");
        assert!(origin_matches("mixpeek frustrations [manual:ip:1.2.3.4]", "mixpeek-frustrations"));
        assert!(!origin_matches("amux-cloud", "amux"), "prefixes must not match (AC-316)");
    }

    #[test]
    fn the_advance_ladder_and_the_reviewer_pairing_stay_derived() {
        assert_eq!(advance_target("doing"), Some(TaskStatus::Review));
        assert_eq!(advance_target("review"), Some(TaskStatus::Done));
        assert_eq!(advance_target("done"), Some(TaskStatus::Verified));
        assert_eq!(advance_target("todo"), None);
        // Derived from the enforcement set: review->done and done->verified are
        // the transitions that need a sign-off.
        assert!(reviewer_acts_next("review"));
        assert!(reviewer_acts_next("done"));
        assert!(!reviewer_acts_next("doing"), "doing->review is not a sign-off");
    }

    // --- backlog triage ---------------------------------------------------

    /// The nudge shipped in 93398c6 with no test of its own. Pin the shape a
    /// lane actually reads: both totals, and every listed card with its age.
    #[test]
    fn backlog_triage_text_names_both_totals_and_every_card() {
        let cards = vec![
            ("B-1".to_string(), "Investigate the flaky retry".to_string(), 20),
            ("B-2".to_string(), "Old finding nobody triaged".to_string(), 45),
        ];
        let text = backlog_triage_text(&cards, 2, 6);
        assert!(text.contains("6 cards in `backlog`"), "must state the true backlog total: {text}");
        assert!(text.contains("2 of which are over"), "must state the stale total: {text}");
        assert!(text.contains("B-1") && text.contains("B-2"), "every listed card must appear: {text}");
        assert!(text.contains("20d old") && text.contains("45d old"), "age must be shown: {text}");
        assert!(!text.contains("more"), "nothing was dropped; got: {text}");
    }

    /// stale_backlog_candidates caps at LIMIT 10 but stale_backlog_count does
    /// not — the same view-vs-mechanism drift the verify-nudge test above
    /// pins. If the arithmetic disagrees, the prompt states a number the list
    /// cannot account for.
    #[test]
    fn backlog_triage_and_n_more_agrees_with_the_stale_total() {
        let cards: Vec<(String, String, i64)> =
            (0..10).map(|i| (format!("B-{i}"), format!("card {i}"), 15 + i)).collect();
        let text = backlog_triage_text(&cards, 14, 20);
        assert!(text.contains("... and 4 more stale"), "14 - 10 = 4; got: {text}");
        for (id, _, _) in &cards {
            assert!(text.contains(id.as_str()), "{id} listed in the candidates but absent from the prompt");
        }
    }

    /// A lane with exactly as many stale cards as were listed must not be
    /// told there are "0 more" (mirrors no_and_n_more_line_when_nothing_was_dropped
    /// for the verify-nudge).
    #[test]
    fn no_backlog_and_n_more_line_when_nothing_was_dropped() {
        let cards = vec![("B-1".to_string(), "t".to_string(), 20)];
        let text = backlog_triage_text(&cards, 1, 1);
        assert!(!text.contains("more"), "nothing was dropped; got: {text}");
    }

    /// Titles are truncated to 65 chars so one runaway title can't blow out
    /// the whole nudge.
    #[test]
    fn backlog_triage_truncates_long_titles() {
        let long_title = "x".repeat(200);
        let cards = vec![("B-1".to_string(), long_title.clone(), 20)];
        let text = backlog_triage_text(&cards, 1, 1);
        assert!(!text.contains(&long_title), "the full 200-char title must not appear verbatim: {text}");
        assert!(text.contains(&"x".repeat(65)), "the first 65 chars must survive: {text}");
        assert!(!text.contains(&"x".repeat(66)), "must be truncated at exactly 65 chars: {text}");
    }

    /// The nudge names the three sanctioned exits so a lane reading it has an
    /// honest path forward for every card shape (ethos rule 3).
    #[test]
    fn backlog_triage_text_names_the_three_triage_exits() {
        let cards = vec![("B-1".to_string(), "stale thing".to_string(), 30)];
        let text = backlog_triage_text(&cards, 1, 1);
        assert!(text.contains("archive"), "must offer the archive exit: {text}");
        assert!(text.contains("`todo`"), "must offer the promote-to-todo exit: {text}");
        assert!(text.contains("needs:you"), "must offer the needs:you exit: {text}");
    }

    /// The DB predicates behind the triage nudge. Every excluded row must
    /// stay excluded from BOTH the count and the candidate list, or the
    /// nudge's headline and its list describe different sets (rule 4: a view
    /// must share the predicate of the mechanism it claims to describe).
    #[test]
    fn stale_backlog_predicates_exclude_everything_that_is_not_a_stale_agent_backlog_card() {
        let conn = board_db();
        let now = 100 * 86400; // day 100, arbitrary epoch anchor
        let old = now - BACKLOG_STALE_AGE_S - 86400; // 15 days old: stale
        let fresh = now - 86400; // 1 day old: not stale
        let ins = |id: &str, session: &str, status: &str, owner: &str, arch: i64, del: Option<i64>, created: i64| {
            conn.execute(
                "INSERT INTO issues (id,title,status,session,owner_type,archived,deleted,type,created,updated) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,'code',?8,?8)",
                rusqlite::params![id, format!("card {id}"), status, session, owner, arch, del, created],
            )
            .expect("insert");
        };
        ins("S-1", "lane", "backlog", "agent", 0, None, old); // the only one that qualifies
        ins("S-2", "lane", "backlog", "human", 0, None, old); // ethos rule 8: never sweep a human's card
        ins("S-3", "lane", "backlog", "agent", 1, None, old); // archived
        ins("S-4", "lane", "backlog", "agent", 0, Some(1), old); // deleted
        ins("S-5", "lane", "todo", "agent", 0, None, old); // not in backlog
        ins("S-6", "other", "backlog", "agent", 0, None, old); // another lane
        ins("S-7", "lane", "backlog", "agent", 0, None, fresh); // not yet stale

        let candidates = stale_backlog_candidates(&conn, "lane", now);
        let ids: Vec<&str> = candidates.iter().map(|(i, _, _)| i.as_str()).collect();
        assert_eq!(ids, vec!["S-1"], "only this lane's own live agent-owned stale backlog card");
        assert_eq!(stale_backlog_count(&conn, "lane", now), 1, "the count must share the candidates' predicate");
    }

    /// Age is reported in whole days, and the oldest card leads the list —
    /// the nudge is meant to surface the longest-neglected work first.
    #[test]
    fn stale_backlog_candidates_orders_oldest_first_with_correct_age() {
        let conn = board_db();
        let now = 100 * 86400;
        let ins = |id: &str, created: i64| {
            conn.execute(
                "INSERT INTO issues (id,title,status,session,owner_type,archived,type,created,updated) \
                 VALUES (?1,?2,'backlog','lane','agent',0,'code',?3,?3)",
                rusqlite::params![id, format!("card {id}"), created],
            )
            .expect("insert");
        };
        ins("O-1", now - 20 * 86400); // 20d old
        ins("O-2", now - 30 * 86400); // 30d old
        ins("O-3", now - 15 * 86400); // 15d old, just past the cutoff

        let candidates = stale_backlog_candidates(&conn, "lane", now);
        let ids: Vec<&str> = candidates.iter().map(|(i, _, _)| i.as_str()).collect();
        assert_eq!(ids, vec!["O-2", "O-1", "O-3"], "oldest-first ordering");
        let ages: std::collections::HashMap<&str, i64> =
            candidates.iter().map(|(i, _, a)| (i.as_str(), *a)).collect();
        assert_eq!(ages["O-2"], 30);
        assert_eq!(ages["O-1"], 20);
        assert_eq!(ages["O-3"], 15);
    }

    /// The candidate list caps at 10 (LIMIT 10 in the query, for prompt
    /// brevity) but the count must not — the same drift the "...and N more"
    /// arithmetic in backlog_triage_text depends on staying pinned.
    #[test]
    fn stale_backlog_candidates_caps_at_ten_but_the_count_does_not() {
        let conn = board_db();
        let now = 100 * 86400;
        for i in 0..15 {
            conn.execute(
                "INSERT INTO issues (id,title,status,session,owner_type,archived,type,created,updated) \
                 VALUES (?1,?2,'backlog','lane','agent',0,'code',?3,?3)",
                rusqlite::params![format!("C-{i}"), format!("card {i}"), now - (20 + i) * 86400],
            )
            .expect("insert");
        }
        let candidates = stale_backlog_candidates(&conn, "lane", now);
        assert_eq!(candidates.len(), 10, "capped for prompt brevity");
        assert_eq!(stale_backlog_count(&conn, "lane", now), 15, "but the count sees all of them");
    }

    /// A card created exactly at the 14-day boundary is not yet stale — the
    /// cutoff is a strict `<`, not `<=`.
    #[test]
    fn a_card_created_exactly_at_the_cutoff_is_not_yet_stale() {
        let conn = board_db();
        let now = 100 * 86400;
        let ins = |id: &str, created: i64| {
            conn.execute(
                "INSERT INTO issues (id,title,status,session,owner_type,archived,type,created,updated) \
                 VALUES (?1,?2,'backlog','lane','agent',0,'code',?3,?3)",
                rusqlite::params![id, format!("card {id}"), created],
            )
            .expect("insert");
        };
        ins("E-1", now - BACKLOG_STALE_AGE_S); // exactly 14 days old
        assert_eq!(stale_backlog_count(&conn, "lane", now), 0, "exactly-14-days is not yet over 14 days");
        ins("E-2", now - BACKLOG_STALE_AGE_S - 1); // one second past
        assert_eq!(stale_backlog_count(&conn, "lane", now), 1, "one second past the cutoff is stale");
    }
    /// The board verbs the SHIPPED bash CLI actually handles, parsed from
    /// `cmd_board()`'s own case labels. Reading the CLI source is the point: a
    /// hardcoded list here would drift from the CLI exactly like the docs did.
    fn cli_board_verbs() -> std::collections::HashSet<String> {
        const CLI: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../amux"));
        let start = CLI.find("cmd_board() {").expect("the CLI defines cmd_board");
        // Stop at the next top-level function so a later function's labels
        // cannot smuggle a verb in (`show)` really does live in cmd_defaults).
        let rest = &CLI[start + 13..];
        let end = rest.find("\ncmd_").map(|e| start + 13 + e).unwrap_or(CLI.len());
        let body = &CLI[start..end];
        let re = regex::Regex::new(r"(?m)^\s{4}([a-z][a-z0-9|_-]*)\)").unwrap();
        let mut out = std::collections::HashSet::new();
        for c in re.captures_iter(body) {
            for v in c[1].split('|') {
                if !v.is_empty() {
                    out.insert(v.to_string());
                }
            }
        }
        out
    }

    /// Assert every `amux board <verb>` a generated prompt names is real.
    fn assert_cli_verbs_exist(prompt: &str) {
        let verbs = cli_board_verbs();
        // The parser must have found REAL verbs, or every membership test below
        // passes vacuously against an empty set.
        assert!(
            verbs.contains("done") && verbs.contains("doing") && verbs.contains("review"),
            "cli_board_verbs parsed {} labels and is missing known ones — the parser is broken, \
             so the assertions below would be meaningless: {verbs:?}",
            verbs.len()
        );
        let re = regex::Regex::new(r"amux board ([a-z][a-z0-9_-]*)").unwrap();
        for c in re.captures_iter(prompt) {
            let v = &c[1];
            assert!(
                verbs.contains(v),
                "this prompt tells an agent to run `amux board {v}`, which cmd_board() does not \
                 handle — it will fall through to the help text. Add the verb or fix the prompt \
                 (AF-66). Known verbs: {verbs:?}"
            );
        }
    }

    /// The capture-shell meter must move with the OUTCOMES, not just count
    /// nudges (AMUX-3707). A total alone cannot say whether a woken turn reached
    /// a foregone conclusion or did real judgment, and that ratio is the whole
    /// reason the number exists.
    #[test]
    fn the_capture_shell_meter_reports_the_outcome_split_not_just_a_total() {
        let conn = board_db();
        conn.execute("DELETE FROM session_events", []).expect("clear");
        // Three nudged capture cards: one discarded (foregone), one promoted to
        // an epic (real judgment), one still open.
        add_card(&conn, "C-1", "lane", "discarded", "q", "**Prompt:** what is x");
        add_card(&conn, "C-2", "lane", "doing", "real", "**Prompt:** do a thing");
        add_card(&conn, "C-3", "lane", "doing", "open", "**Prompt:** another");
        conn.execute("UPDATE issues SET type='epic' WHERE id='C-2'", []).expect("retype");
        for id in ["C-1", "C-2", "C-3"] {
            conn.execute(
                "INSERT INTO session_events (ts,session,type,data,idem,source) \
                 VALUES (?1,'lane','decompose-asked','{}',?2,'test')",
                rusqlite::params![now_f64(), format!("decompose:{id}")],
            )
            .expect("event");
        }
        let m = capture_shell_cost(&conn);
        assert_eq!(m["nudges"], 3, "{m}");
        assert_eq!(m["outcome_discarded"], 1, "{m}");
        assert_eq!(m["outcome_promoted_to_epic"], 1, "{m}");
        assert_eq!(m["still_open"], 2, "one discarded, two not resolved: {m}");
        // The ratio is the signal. 1 of 3 discarded = 33.3%.
        assert_eq!(m["discarded_pct"], 33.3, "{m}");

        // ...and it must MOVE when the outcomes move, or it is a constant with a
        // plausible value — the shape that gets believed and never rechecked.
        conn.execute("UPDATE issues SET status='discarded' WHERE id='C-3'", []).expect("upd");
        let m2 = capture_shell_cost(&conn);
        assert_eq!(m2["outcome_discarded"], 2, "{m2}");
        assert_eq!(m2["discarded_pct"], 66.7, "{m2}");
        assert_eq!(m2["still_open"], 1, "{m2}");

        // NEVER 0.0 FOR A NONZERO COUNT (mixpeek-frustrations, replaying my own
        // claim against this code). One-decimal rounding survives 5-of-501 where
        // integer division does not, but it still collapses past a 2000:1 ratio
        // — and `nudges` grows ~42/day from 545, so that is weeks away, not
        // never. A rendered 0.0 beside a nonzero count is the same
        // zero-that-means-none as the nudge's "0% coverage".
        for _ in 0..5000 {
            conn.execute(
                "INSERT INTO session_events (ts,session,type,data,idem,source) \
                 VALUES (?1,'lane','decompose-asked','{}',?2,'test')",
                rusqlite::params![now_f64(), format!("decompose:pad{}", rand_pad())],
            )
            .expect("pad");
        }
        let m3 = capture_shell_cost(&conn);
        // THE PREMISE, asserted: the ratio must actually be past the floor, or
        // the guard below never fires and the cell passes against anything. The
        // first draft padded to 2100 and 2-of-2103 rounds to 0.1, not 0.0 — so
        // it was vacuous, and only a mutation run showed it. The floor is
        // n*100/nudges < 0.05, i.e. nudges > 2000*n.
        let nud = m3["nudges"].as_i64().unwrap();
        let disc = m3["outcome_discarded"].as_i64().unwrap();
        assert!(
            disc > 0 && nud > 2000 * disc,
            "premise: {disc} discarded of {nud} must be past the rounding floor: {m3}"
        );
        let d = m3["discarded_pct"].as_f64().expect("still a number");
        assert!(
            d > 0.0,
            "2 discarded out of >2000 must not render as 0.0 — that reads as 'none': {m3}"
        );

        // THE BOUNDARY, PINNED IN RUST — because I got it wrong in prose by
        // checking it in Python (mixpeek-frustrations caught it). Python's
        // round() is BANKER'S rounding and Rust's f64::round is half-AWAY-from-
        // zero, so the same expression disagrees at exactly the half-step: 1 of
        // 2000 is 0.05, times ten is 0.5, and Python gives 0 where Rust gives 1.
        // A boundary example verified in the wrong language is its own instance
        // of this thread's whole subject.
        //
        // So the floor is `nudges > 2000*n`, STRICTLY, and these cells say so in
        // the language that ships. They also pin the rounding mode: if
        // f64::round ever changed, or someone rewrote this with a helper that
        // rounds half-to-even, the first line goes red.
        let render = |n: i64, nudges: i64| -> f64 {
            let exact = n as f64 * 100.0 / nudges as f64;
            let rounded = (exact * 10.0).round() / 10.0;
            if rounded == 0.0 && n > 0 { exact } else { rounded }
        };
        assert_eq!(render(1, 2000), 0.1, "1-of-2000 is exactly 0.5 and rounds AWAY from zero");
        assert!(render(1, 2001) > 0.0, "the first ratio past the floor must not be 0.0");
        assert!(render(1, 2001) < 0.05, "...and it is the exact value, not a rounded one");
        assert_eq!(render(0, 2001), 0.0, "a genuine ZERO still renders zero — the guard is \
                                          about nonzero counts, not about hiding zeros");
    }

    /// Distinct ids for the padding rows above. A counter, not randomness:
    /// `idem` is UNIQUE, and the test must not depend on a clock or an RNG.
    fn rand_pad() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    /// The check above can only be trusted if it FIRES. A prompt naming a verb
    /// that does not exist must panic — this is the exact AF-66 specimen, whose
    /// original test happily passed while `show` did not exist.
    #[test]
    #[should_panic(expected = "does not handle")]
    fn a_prompt_naming_a_nonexistent_board_verb_is_caught() {
        assert_cli_verbs_exist("read it with `amux board shwo AF-64`");
    }

    /// And the real verbs must pass, so the check is not simply always-panicking.
    /// THE PROSE THAT DOCUMENTS THIS FLAG MUST NAME THE REAL KEY (AF-449).
    ///
    /// AMUX-4055 shipped this switch on 2026-09-02 at 22:00, default off, named
    /// in no prompt, no doc and no nudge text. Within eleven hours Ethan asked
    /// for it 23 times across BOTH repos — "there should be an environment
    /// variable in the scope" — for a switch that already existed. The friction
    /// sweep measured that at n=23 against a 0.54/day baseline with
    /// `prose_exists: false`.
    ///
    /// The fix was a paragraph in ~/.claude/CLAUDE.md, and a paragraph naming a
    /// constant is exactly the unenforceable prose the friction-themes contract
    /// warns about: rename the key and the documentation silently becomes a lie
    /// pointing at a variable nothing reads. So the doc is checked against the
    /// CONSTANT rather than against a copy of its text.
    ///
    /// ABSENT IS REPORTED, NOT PASSED. The global prompt is per-machine and is
    /// legitimately missing on a cloud image, so this cannot fail there — but a
    /// silent skip is how a check stops covering anything without saying so, and
    /// this file's own signal is that unnamed capabilities reach nobody. It
    /// prints why it could not measure.
    #[test]
    fn the_global_prompt_names_the_real_backlog_dispatch_key() {
        let Some(home) = std::env::var_os("HOME") else {
            println!("UNMEASURED: no HOME, cannot locate the global prompt");
            return;
        };
        let p = std::path::Path::new(&home).join(".claude/CLAUDE.md");
        let Ok(txt) = std::fs::read_to_string(&p) else {
            println!("UNMEASURED: {} is absent (expected on a cloud image); \
                      the key/doc agreement is UNCHECKED on this box, not confirmed",
                     p.display());
            return;
        };
        assert!(
            txt.contains(DISPATCH_BACKLOG_KEY),
            "~/.claude/CLAUDE.md documents backlog dispatch but does not name {DISPATCH_BACKLOG_KEY}. \
             Either the key was renamed and the prose now points at a variable nothing reads, or the \
             paragraph was dropped — both leave the capability unnameable, which is the state \
             AMUX-4055 was in for eleven hours while Ethan asked for it 23 times."
        );
        // The CONTROL. Asserting only "the key appears" would pass on a file
        // that mentions it in passing, so require the sentence that makes it
        // actionable: that `backlog` is not dispatched by default.
        // Case-INSENSITIVE: the property is that the behaviour is stated, not
        // that it is stated in lower case. Pinning the exact casing is the
        // hand-typed-fixture trap one level down — the first version of this
        // cell failed on "NOT dispatched by default", which says the same thing.
        let lower = txt.to_lowercase();
        assert!(
            lower.contains("backlog") && lower.contains("not dispatched by default"),
            "the key is named but the behaviour it changes is not stated; a reader learns the \
             variable exists and not what it is for"
        );
    }

    #[test]
    fn the_real_board_verbs_are_accepted() {
        assert_cli_verbs_exist("amux board show X, amux board done X, amux board reviewer X y");
    }

}

#[cfg(test)]
mod af579_decline_exit_tests {
    use super::*;

    /// AF-579. The pickup prompt named three exits: hand it to another lane, ask
    /// Ethan, and NOT review. A lane that can do none of those had no sentence to
    /// follow, and the obvious move is the backlog drain's own input.
    ///
    /// PINS BOTH ARMS. Testing only the drain arm would pass on a function that
    /// ignores its argument and always warns, which would tell a lane with no
    /// drain that its working exit does not work.
    #[test]
    fn the_decline_exit_names_a_trigger_only_where_a_bare_backlog_would_come_back() {
        let drained = decline_exit_text(true);
        assert!(
            drained.contains("--trigger"),
            "a drain-enabled lane must get the exit that survives the drain: {drained}"
        );
        assert!(
            drained.contains("BARE `backlog` is not an exit"),
            "and be told WHY the obvious move fails, or it reads as style: {drained}"
        );

        let plain = decline_exit_text(false);
        assert!(
            plain.contains("amux board backlog"),
            "a lane with no drain still needs an exit named: {plain}"
        );
        assert!(
            !plain.contains("not an exit on this lane"),
            "and must NOT be told a bare backlog fails where it does not: {plain}"
        );

        // THE ARMS MUST DIFFER. Without this, one constant satisfies every
        // assertion above that the two arms happen to share.
        assert_ne!(drained, plain, "both lanes were given the same sentence");
    }

    /// NO RUN OF SPACES inside either arm. Rust line-continuations in a string
    /// literal need a trailing backslash; without it the source still compiles
    /// and the PROMPT renders "the          condition". I shipped exactly that
    /// in the first cut of this change, and the compiler cannot see it.
    #[test]
    fn neither_arm_renders_a_run_of_spaces() {
        for text in [decline_exit_text(true), decline_exit_text(false)] {
            assert!(
                !text.contains("   "),
                "a dropped line-continuation leaves a gap in the rendered prompt: {text}"
            );
        }
    }

    /// The live wiring: a sentence no prompt includes is a sentence nobody
    /// reads, which is the defect one layer up.
    #[test]
    fn the_pickup_prompt_carries_the_decline_exit() {
        let src = include_str!("board_drive.rs");
        // BOUND IT TO THE FUNCTION. Splitting only on the opening line leaves
        // `body` as the whole rest of the file, which contains THIS assertion's
        // own literal, so unwiring the call site left the cell green. A test
        // matching its own text is the self-referential trap ethos rule 7 names.
        let body = src
            .split_once("fn pickup_prompt(")
            .expect("pickup_prompt exists")
            .1;
        let body = body.split_once("\n}\n").expect("its closing brace").0;
        assert!(
            body.contains("decline_exit(session)"),
            "pickup_prompt must call decline_exit, or the exit reaches no lane"
        );
    }
}
