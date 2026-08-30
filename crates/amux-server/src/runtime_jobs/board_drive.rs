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
//! | needs:you re-nag (3d)     | `needsyou.renag` per card id           |
//!
//! `advance.nudged` additionally records the card's STATUS at nudge time, which
//! Python kept only in memory (`_advance_last_card`, py:12960). That is what
//! makes "did this lane make progress since we spoke?" survive a restart, so a
//! lane that moved the card it was nudged about gets the next one immediately
//! instead of waiting out a cooldown it already earned its way past.

use std::collections::HashSet;
use std::sync::{OnceLock, RwLock};

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};

use crate::api::AppState;
use crate::db::board_store as bs;
use amux_core::board::TaskStatus;

/// Sweep cadence. Python's level sweep ran every 300s behind an idle EDGE that
/// fired immediately; with no edge, 300s would mean a lane finishing a turn
/// waits up to five minutes for its next card. Volume is bounded by the
/// cooldowns above, not by the tick, so the tick can be honest about latency.
pub const BOARD_DRIVE_TICK_SECS: u64 = 60;

/// Local alias for the steering guard these deliveries carry. The string
/// lives once, in session_verbs, because that is where it is READ.
use crate::api::session_verbs::BOARD_DRIVE_GUARD as GUARD;

/// py:12961 `_ADVANCE_COOLDOWN` — never push the same lane twice inside this.
const ADVANCE_COOLDOWN_S: f64 = 15.0 * 60.0;
/// py:12855 `_DECOMPOSE_NUDGE_COOLDOWN`.
const DECOMPOSE_COOLDOWN_S: f64 = 6.0 * 3600.0;
/// py:14514 freshness gate — never auto-run a card nobody has touched in 7 days.
/// Default; overridable via `AMUX_PICKUP_FRESHNESS_S` (AMUX-3779). With pickup
/// scoring on, age SURFACES old cards before they reach this edge, so the gate
/// is a backstop for a genuinely-overwhelmed lane, not the thing that hides a
/// starved card — and when it does exclude a todo, `select_pickup` now says so
/// in the trace instead of the card silently vanishing from auto-pickup.
const PICKUP_FRESHNESS_S_DEFAULT: i64 = 7 * 86400;
fn pickup_freshness_s() -> i64 {
    std::env::var("AMUX_PICKUP_FRESHNESS_S")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(PICKUP_FRESHNESS_S_DEFAULT)
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
/// no-stall guarantee (Invariant 10). Default is now aligned WITH the breaker
/// window (2h): a card is exempt exactly as long as it still counts as a recent
/// bounce, and re-dispatchable the moment it stops — the two mechanisms share
/// one window instead of fighting. A lane that truly cannot do a card moves it
/// to backlog/review (the honest exits the pickup prompt names), so it never
/// re-enters this loop; only a card left in `todo` gets another turn.
fn reclaim_cooldown_s() -> f64 {
    std::env::var("AMUX_RECLAIM_COOLDOWN_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v >= 0.0)
        .unwrap_or(7200.0)
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
fn wip_cap() -> i64 {
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
    /// `renag` | `verify-nudge` | `backlog-triage` | `skipped`
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
    pub lanes: Vec<LaneTrace>,
}

static LAST_REPORT: OnceLock<RwLock<Option<DriveReport>>> = OnceLock::new();
static TICK_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    async fn is_running(&self, lane: &str) -> bool;
    /// The turn-boundary gate. REUSED from the steering path, never
    /// reimplemented: a second copy of "is this lane mid-turn" is the
    /// two-implementations-of-one-rule defect the board keeps producing.
    async fn at_boundary(&self, lane: &str) -> bool;
    /// When a session runs out of todo cards but still has blocked/done work,
    /// keep nudging it to re-assess and continue. ON BY DEFAULT since
    /// 2026-08-11 (Ethan: "standing order whenever idle to take care of any
    /// non-terminal board task"); CC_AUTO_CONTINUE=0 opts a lane out. The
    /// explicit =1 additionally implies YOLO (is_yolo_enabled); the default
    /// deliberately does not.
    fn auto_continue_enabled(&self, lane: &str) -> bool;
    /// Hand text to the lane. Durable queue + the existing delivery loop.
    async fn deliver(&self, lane: &str, text: &str);

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
    fn auto_continue_enabled(&self, lane: &str) -> bool {
        crate::api::session_verbs::standing_orders_on(lane, "CC_AUTO_CONTINUE")
    }
    async fn is_running(&self, lane: &str) -> bool {
        crate::api::session_verbs::is_running(lane).await
    }
    async fn at_boundary(&self, lane: &str) -> bool {
        crate::api::session_verbs::steer_lane_at_boundary(&self.state, lane).await
    }
    async fn deliver(&self, lane: &str, text: &str) {
        let _ = crate::api::session_verbs::steer_enqueue(&self.state, lane, text, GUARD, "").await;
        self.record_prompt(lane, text).await;
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
    if desc.contains("capture: session prompt") && folds < 2 {
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

/// py:14566 — the PROSE dependency fallback. Fires ONLY when `depends_on` is
/// empty: a card id in prose is ambiguous by nature (MG-1363's blocker names a
/// card in words while the only ID in it is the EPIC it cites for authority), so
/// a prose match can be the right answer via the wrong mechanism.
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
    // not the blocker. Deliberately narrow — it rejects a reversed reading
    // rather than trying to parse the sentence, and when it is unsure it still
    // blocks, which is the safe direction for a dispatch gate.
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

    /// The forward direction must still block, or the fix trades a stalled lane
    /// for a dispatch that ignores real dependencies — strictly worse, and
    /// invisible until something ships out of order.
    #[test]
    fn a_real_blocker_still_blocks() {
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
                "must still block: {blob:?}"
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
                      'capture.decompose_ask','task.claimed') \
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
     AND NOT EXISTS (SELECT 1 FROM session_events e WHERE e.type='task.claimed' \
                     AND e.ts > ?3 AND e.data LIKE '%\"' || i.id || '\"%')";

/// py:14403 — the count over EXACTLY the rows pickup selects from, via
/// [`DISPATCHABLE_WHERE`]. Per-card refusals (junk shells, prose deps) still
/// happen inside the loop and surface as `all-candidates-refused` — an honest
/// difference, since those are judgments about a card, not queue membership.
fn eligible_todo_count(conn: &Connection, session: &str, now: f64) -> i64 {
    let fresh_cut = (now as i64) - pickup_freshness_s();
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

/// py:14189 `_deps_blocking` — card ids in `depends_on` that are still OPEN.
/// Deleted or absent ids do NOT block: an id that resolves to nothing cannot be
/// worked, and treating it as a blocker parks the holder forever.
fn deps_blocking(conn: &Connection, row: &bs::IssueRow) -> Vec<String> {
    row.depends_on
        .iter()
        .filter(|d| {
            let st: Option<String> = conn
                .query_row(
                    "SELECT status FROM issues WHERE id=?1 AND deleted IS NULL",
                    rusqlite::params![d],
                    |r| r.get(0),
                )
                .optional()
                .ok()
                .flatten();
            match st.as_deref().map(bs::parse_status) {
                Some(Some(TaskStatus::Done))
                | Some(Some(TaskStatus::Verified))
                | Some(Some(TaskStatus::Discarded)) => false,
                Some(_) => true,
                None => false,
            }
        })
        .cloned()
        .collect()
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

/// Is a single dependency's stored status TERMINAL for the promotion pass?
/// Terminal here means COMPLETED: `done` or `verified` only. `discarded` is
/// deliberately excluded — a discarded dependency is an abandonment a human
/// should notice, not an all-clear that should silently re-activate the work
/// that depended on it. This is narrower than [`deps_blocking`] (which lets a
/// `discarded` dep un-block, because you cannot work an id that resolves to
/// nothing) on purpose: not-blocking is not the same as cleared.
fn dep_status_terminal(status: &str) -> bool {
    matches!(
        bs::parse_status(status),
        Some(TaskStatus::Done) | Some(TaskStatus::Verified)
    )
}

/// Pure: does this dependency set license a promotion? True iff there is at
/// least one dependency AND every one is terminal. An EMPTY slice returns
/// `false` — a card with no `depends_on` is not dependency-parked and this pass
/// must never touch it (a triggers-only park stays parked). Split out as a pure
/// function so the promotion rule is tested without a live DB.
fn deps_all_terminal(dep_statuses: &[&str]) -> bool {
    !dep_statuses.is_empty() && dep_statuses.iter().all(|s| dep_status_terminal(s))
}

/// If `row` is a dependency-parked card whose EVERY dependency resolves to a
/// live card in a terminal status, return the dependency ids (so the promotion
/// log can name what cleared); otherwise `None`. Conservative by construction:
/// a `depends_on` id that resolves to no live row (missing/deleted) is treated
/// as non-terminal, so a card is promoted ONLY when all its deps are provably
/// terminal. Mirrors [`deps_blocking`]'s per-id status lookup.
fn promotable_deps(conn: &Connection, row: &bs::IssueRow) -> Option<Vec<String>> {
    if row.depends_on.is_empty() {
        return None;
    }
    let statuses: Vec<Option<String>> = row
        .depends_on
        .iter()
        .map(|d| {
            conn.query_row(
                "SELECT status FROM issues WHERE id=?1 AND deleted IS NULL",
                rusqlite::params![d],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
        })
        .collect();
    // A dependency that did not resolve to a live row is not terminal.
    if statuses.iter().any(Option::is_none) {
        return None;
    }
    let refs: Vec<&str> = statuses.iter().map(|s| s.as_deref().unwrap_or("")).collect();
    if deps_all_terminal(&refs) {
        Some(row.depends_on.clone())
    } else {
        None
    }
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
async fn promote_ready_backlog(state: &AppState) -> (usize, usize) {
    let (candidates, held_on_trigger) = match state.store.read() {
        Ok(conn) => backlog_dep_promotions(&conn),
        Err(_) => return (0, 0),
    };
    let mut promoted = 0;
    for (card, deps) in candidates {
        let card_w = card.clone();
        let reply = state
            .store
            .write_async(move |conn| {
                // Re-check under the write lock. The card must STILL be a
                // dependency-parked agent backlog card whose deps are all
                // terminal — it could have been moved, archived, deleted, or
                // its deps changed between the read scan and here. `WHERE
                // status='backlog'` makes the swap atomic against a concurrent
                // move (mirrors claim_card's `WHERE status='todo'`).
                let Some(row) = bs::get_issue(conn, &card_w)? else {
                    return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
                };
                if row.status != "backlog"
                    || row.owner_type != "agent"
                    || row.archived != 0
                    || is_dormant_type(&row.item_type)
                    || parked_on_live_trigger(&row)
                    || promotable_deps(conn, &row).is_none()
                {
                    return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
                }
                let now = now_f64() as i64;
                let existing: Option<String> = conn
                    .query_row(
                        "SELECT log FROM issues WHERE id=?1",
                        rusqlite::params![card_w],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                let hhmm = chrono::Local::now().format("%H:%M").to_string();
                let log = bs::append_log(
                    existing.as_deref(),
                    &hhmm,
                    "Re-activated: all depends_on dependencies reached a terminal status",
                );
                let n = conn.execute(
                    "UPDATE issues SET status='todo', updated=?1, log=?2 \
                     WHERE id=?3 AND status='backlog'",
                    rusqlite::params![now, log, card_w],
                )?;
                if n == 0 {
                    return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
                }
                // The post-mutation snapshot the SSE/replay journal carries —
                // the identical shape board.rs emits on a status PATCH, so the
                // fan-out is a real StatusChanged, not a bare revision bump.
                let next = bs::get_issue(conn, &card_w)?
                    .expect("row present immediately after its own promote");
                let event = crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Task,
                    entity_id: next.id.clone(),
                    mutation: amux_core::revision::MutationKind::StatusChanged {
                        from: "backlog".into(),
                        to: "todo".into(),
                    },
                    payload: Some(next.snapshot()),
                };
                Ok(crate::db::WriteOutcome { applied: true, events: vec![event] })
            })
            .await;
        if matches!(reply, Ok(r) if r.applied) {
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
pub enum Pickup {
    /// Claim this card and send this prompt.
    Claim { card: String, prompt: String },
    /// Every candidate was a capture shell — ask for a decomposition instead of
    /// going silent (py:14614, AMUX-2131). The guard is right that a shell is
    /// not a unit of work; the fix is the session SPLITTING it, which is
    /// judgment a model does well, not the card rotting.
    Decompose { ids: Vec<String>, text: String },
    /// Nothing to do, with the reason and the detail for the trace.
    None { reason: &'static str, detail: String },
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
pub fn select_pickup(conn: &Connection, session: &str, now: f64) -> Pickup {
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
             AND NOT EXISTS (SELECT 1 FROM issue_tags t WHERE t.issue_id=issues.id \
                             AND lower(t.tag) LIKE 'needs:you%') \
             ORDER BY id",
        )
        .and_then(|mut st| {
            st.query_map(rusqlite::params![session], |r| r.get::<_, String>(0))
                .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();
    if holding.len() as i64 >= cap {
        return Pickup::None {
            reason: "wip-cap",
            detail: format!("holding {}/{} in doing: {}", holding.len(), cap, holding.join(", ")),
        };
    }

    // BOUNCE-LOOP BREAKER (backend, 2026-08-11). A lane that keeps returning
    // its pickups to todo converts its queue into 24h reclaim-cooldowns at
    // one card per tick — measured 16 claims in one hour, 19 cards enriched
    // with notes and nothing executed, and the drive kept feeding it. Three
    // bounced claims inside two hours means the NEXT card will not fare
    // better: stop dealing, say so in the trace, and let the advance/nudge
    // paths (and the fixed pickup prompt's honest exits) resolve the state.
    // The breaker clears itself: it counts only claims whose card is BACK in
    // todo, so moving any of them forward (or to backlog/review) releases it.
    let bounced: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_events e WHERE e.type='task.claimed' \
             AND e.session=?1 AND e.ts > ?2 \
             AND EXISTS (SELECT 1 FROM issues i WHERE i.status='todo' AND i.deleted IS NULL \
                         AND e.data LIKE '%\"' || i.id || '\"%')",
            rusqlite::params![session, now - 7200.0],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if bounced >= 3 {
        tracing::warn!(session = %session, bounced,
            "pickup bounce-loop: this lane returned its recent pickups to todo — dealing it more cards would only burn cooldowns");
        return Pickup::None {
            reason: "bounce-loop",
            detail: format!(
                "{bounced} claims in the last 2h are back in todo — the lane is declining \
                 pickups, not working them; withholding further cards until one moves"
            ),
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
    let fresh_cut = (now as i64) - pickup_freshness_s();
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
        return Pickup::None {
            reason: "no-eligible-card",
            detail: format!(
                "queue holds nothing dispatchable (needs:you, archived, dormant, \
                 stale >{days}d and cards claimed in the last {}h are all exempt){aged_note}",
                (reclaim_cooldown_s() / 3600.0).round() as i64
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
            skipped.push(format!("{id} blocked by {}", blocking.join(",")));
            continue;
        }
        // Prose fallback fires ONLY when the structured field is empty.
        if row.depends_on.is_empty() {
            let hay = format!("{}\n{}", row.title, blob_desc);
            if let Some(dep) = prose_dependency(&hay) {
                let dep_status: Option<String> = conn
                    .query_row(
                        "SELECT status FROM issues WHERE id=?1 AND deleted IS NULL",
                        rusqlite::params![dep],
                        |r| r.get(0),
                    )
                    .optional()
                    .ok()
                    .flatten();
                let open = matches!(
                    dep_status.as_deref().map(bs::parse_status),
                    Some(Some(s)) if !matches!(s, TaskStatus::Done | TaskStatus::Verified | TaskStatus::Discarded)
                );
                if open {
                    skipped.push(format!(
                        "{id} prose-blocked by {dep} — POPULATE depends_on (prose cannot \
                         distinguish a dependency from a citation)"
                    ));
                    continue;
                }
            }
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
        return Pickup::Claim {
            card: id.clone(),
            prompt: pickup_prompt(conn, session, &row),
        };
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
         decision or access you lack) should be tagged `needs:you` so they surface \
         in the owner digest rather than sitting here indefinitely."
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

/// Backlog cards of ANY age, newest first — the candidates for the idle-drain
/// nudge (a lane sitting on a fresh backlog with nothing in todo). Newest first
/// because a just-arrived batch is the likeliest thing the worker meant to act
/// on; the worker's own model picks which to promote.
fn backlog_candidates(conn: &Connection, session: &str, now: i64) -> Vec<(String, String, i64)> {
    // DRAINABLE only — mirror the exclusions in the idle_drain gate so the cards
    // the nudge lists are exactly the ones it claims are un-worked: no dormant
    // types (tripwire/watch) and no card parked on a live source_ref trigger.
    // OLDEST-first (AMUX-3779): the idle-drain nudge now lists the same
    // oldest-first order the todo scorer works in, so "what gets attention next"
    // reads one way across todo and backlog instead of newest-here/oldest-there.
    conn.prepare(
        "SELECT id, title, created FROM issues \
         WHERE session=?1 AND status='backlog' AND deleted IS NULL \
         AND COALESCE(archived,0)=0 AND owner_type='agent' \
         AND type NOT IN ('tripwire','watch','epic') AND COALESCE(source_ref,'')='' \
         ORDER BY created ASC LIMIT 8",
    )
    .and_then(|mut st| {
        st.query_map(rusqlite::params![session], |r| {
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
         records the condition and EXCLUDES the card from this nudge, so escalation counts only \
         un-parked work — reach for it instead of leaving a parked card to be re-listed. Do not \
         leave the whole backlog sitting while you idle — drain it. This nudge repeats faster \
         the larger your DRAINABLE backlog is, until it moves."
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
#[allow(clippy::type_complexity)]
/// `done` cards carry their REVIEWER (AMUX-3561): whether a card can be moved
/// toward `verified` by the lane that owns it depends entirely on whether a peer
/// is named, so the nudge cannot ask honestly without it.
fn outstanding_work(
    conn: &Connection,
    session: &str,
) -> (i64, i64, Vec<(String, String)>, Vec<(String, String, String)>) {
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
    let (bc, blocked) = query("blocked");
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
    (bc, dc, blocked, done)
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
        let t = continue_nudge_text(1, 2, &blocked, &done);

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
        let t2 = continue_nudge_text(1, 1, &blocked, &reviewed);
        assert!(t2.contains("reviewer: amux-frustrations"), "name the peer: {t2}");
        assert!(
            t2.contains("amux board ask") && !t2.contains("amux board reviewer"),
            "with a reviewer named, the ask becomes chasing THEM, not naming one: {t2}"
        );
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
        let unreviewed = done.iter().filter(|(_, _, rv)| rv.is_empty()).count();
        let ask = if unreviewed > 0 {
            format!(
                "{done_count} done card(s). {unreviewed} of the {} listed name NO reviewer,                  and a card cannot reach `verified` without one — the gate asks for a                  DIFFERENT worker who checked it themselves, which you cannot supply on                  your own. The step that IS yours: name the peer,                  `amux board reviewer <id> <peer-session>`. Archive instead if the work was                  superseded.",
                done.len()
            )
        } else {
            format!(
                "{done_count} done card(s), each with a reviewer named — ask them to verify                  (`amux board ask <id>`), or verify the ones where YOU are the named                  reviewer. Archive instead if the work was superseded."
            )
        };
        sections.push(format!("{ask}\n{}", lines.join("\n")));
    }

    format!(
        "[amux auto-continue] Your todo queue is empty but you still have \
         outstanding work:\n\n{}\n\n\
         Keep going until everything is verified, archived, or genuinely blocked \
         on someone else. Don't stop between cards.",
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
            qnote.push_str(
                " Triage first: move not-ready cards to `backlog`, owner-blocked to review. \
                 Do not bounce ready cards to todo (24h cooldown). Work this card or move it \
                 where it honestly belongs.",
            );
        }
    }
    // The delivery boundary parses the card id back out of this template to void
    // a stale pickup (AMUX-3052). It reads the token right after PICKUP_ANCHOR,
    // which is why the id must stay the FIRST thing after it.
    let mut prompt = format!(
        "{PICKUP_ANCHOR}{} — work it now. Card text below is historical, \
         not a live message. If blocked on an owner decision, move to review (not todo, \
         which re-queues after 24h cooldown):\n{}{}",
        row.id,
        quoted_card_text(&row.title, &row.id),
        qnote
    );
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
            if !moved {
                return Advance::None {
                    reason: "cooldown",
                    detail: format!(
                        "last nudge {:.0}s ago (cooldown {ADVANCE_COOLDOWN_S:.0}s) and the card \
                         it named has not moved",
                        now - last_ts
                    ),
                };
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
    let cands: Vec<(String, String)> = conn
        .prepare(
            "SELECT id, status FROM issues WHERE session=?1 AND deleted IS NULL \
             AND COALESCE(archived,0)=0 AND status IN ('doing','review') AND owner_type='agent' \
             ORDER BY CASE status WHEN 'doing' THEN 0 ELSE 1 END, updated DESC LIMIT 40",
        )
        .and_then(|mut st| {
            st.query_map(rusqlite::params![session], |r| Ok((r.get(0)?, r.get(1)?)))
                .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();
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
    let why = pickup_junk_reason(&row.title, &row.desc, row.log.as_deref().unwrap_or(""));
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
             Several? `amux board type {card_id} epic`, `amux board add \"<unit>\"`, \
             `amux board epic <NEW-ID> {card_id}`"
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
        // messaging is intra-group unless configured, and `cross_group_send_ok`
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
            return Advance::None {
                reason: "reviewer-unreachable",
                detail: format!("{card_id} names reviewer `{rev}`: {why}"),
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
    if last_ts > 0.0 && (now - last_ts) < win {
        return None;
    }
    if last_ts > 0.0 {
        let updated: f64 = conn
            .query_row(
                "SELECT COALESCE(updated,0) FROM issues WHERE id=?1",
                rusqlite::params![card],
                |r| r.get::<_, f64>(0),
            )
            .unwrap_or(0.0);
        if updated > last_ts {
            // Re-stated since we last asked: that IS the remedy the message
            // prescribes. Say nothing this round.
            return None;
        }
    }
    let days = (asked_age / 86400.0) as i64;
    let arch = if archived != 0 {
        "This card is ARCHIVED, which does NOT clear the ask — needs:you stays visible to the \
         human by design.\n\n"
    } else {
        ""
    };
    Some(format!(
        "[amux] {card} needs:you for {days}d: {}\n\n\
         {arch}Still the right question? Re-state it on the card (silences this for {}d). \
         Overtaken? Clear needs:you and move the card.",
        quoted_card_text(&title.chars().take(90).collect::<String>(), card),
        needsyou_renag_days() as i64
    ))
}

// ---------------------------------------------------------------------------
// The tick
// ---------------------------------------------------------------------------

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
    // DRIVE TO VERIFIED, BEFORE DISPATCH. A card parked in `backlog` on a
    // `depends_on` dependency re-activates to `todo` the moment every dependency
    // reaches a terminal status, so a "do B after A" command completes instead
    // of stalling at `parked` forever. Fleet-wide and lane-independent (a
    // dependency clearing does not care which lane is at a boundary), and FIRST
    // so a freshly promoted card is dispatchable in this same tick's lane loop.
    let (promoted, held_on_trigger) = promote_ready_backlog(state).await;
    report.promoted = promoted;
    report.held_on_trigger = held_on_trigger;
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

async fn drive_lane<F: Fleet>(state: &AppState, fleet: &F, lane: &str) -> LaneTrace {
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
    if !fleet.is_running(lane).await {
        return LaneTrace::skip(lane, "not-running", "no live session").with_counts(eligible, open);
    }
    // THE TURN-BOUNDARY GATE, reused from the steering path. Fails CLOSED:
    // anything not positively known to be idle is left alone. A nudge that waits
    // one more tick costs nothing; a nudge delivered mid-turn is an interruption.
    if !fleet.at_boundary(lane).await {
        // NAME THE EVIDENCE, not just the verdict. `mid-turn` read identically
        // for a lane genuinely generating and for one held by a self-report
        // nothing would ever refresh — and the second kind sat here for up to
        // 61 hours (AMUX-3756). The gate now falls through to the pane for a
        // stale report, so this detail is what makes the remaining cases
        // legible: a fresh report age is a real turn, a huge one is the class
        // that used to deadlock.
        let detail = match crate::api::session_verbs::lane_report(state, lane) {
            Some(r) => format!(
                "lane is not at a turn boundary (self-report `{}` {:.0}s old, {})",
                r.state,
                r.age_s,
                if r.applies { "authoritative" } else { "REFUSED as stale — pane decided" }
            ),
            None => "lane is not at a turn boundary (no self-report; the pane decided)".into(),
        };
        return LaneTrace::skip(lane, "mid-turn", detail).with_counts(eligible, open);
    }

    let now = now_f64();
    let tags = fleet.tags(lane);
    let Ok(conn) = state.store.read() else {
        return LaneTrace::skip(lane, "store-unavailable", "could not open a read connection")
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

    match pickup.expect("pickup computed whenever advance declined") {
        Pickup::Claim { card, prompt } => {
            // Only dispatch "work it now" if the atomic claim actually took —
            // the card could have been closed between select_pickup and here
            // (AMUX-2983). A refused claim is not an error, it is the race being
            // caught: skip, do not deliver finished work.
            if claim_card(state, lane, &card).await {
                fleet.deliver(lane, &prompt).await;
                LaneTrace::acted(lane, "assigned", &card, "claimed and prompt queued")
                    .with_counts(eligible, open)
            } else {
                LaneTrace::skip(
                    lane,
                    "pickup-raced",
                    format!("{card} left 'todo' before the claim landed — not dispatched"),
                )
                .with_counts(eligible, open)
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
                // card with a live `source_ref` trigger (parked on an external
                // condition). If nothing drainable remains, the lane's backlog is
                // all correctly parked and the drain nudge must stay quiet.
                let drainable_backlog: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM issues WHERE session=?1 AND status='backlog' \
                         AND deleted IS NULL AND COALESCE(archived,0)=0 AND owner_type='agent' \
                         AND type NOT IN ('tripwire','watch','epic') AND COALESCE(source_ref,'')=''",
                        rusqlite::params![lane],
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
                let idle_drain = doing_count == 0 && eligible == 0 && drainable_backlog > 0;
                let stale_enough = (stale_count as usize) >= BACKLOG_TRIAGE_THRESHOLD;
                if !idle_drain && !stale_enough {
                    break 'triage None;
                }
                let (event_type, cooldown_s) = if idle_drain {
                    ("backlog.drain_nudge", idle_backlog_drain_cooldown_s(drainable_backlog))
                } else {
                    ("backlog.triage_nudge", BACKLOG_TRIAGE_COOLDOWN_S)
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
                let (bc, dc, blocked, done) = outstanding_work(&conn, lane);
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
                    break 'cont Some(format!(
                        "auto-continue enabled, {dc} done but 0 blocked — done is context, not a trigger"
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
                let text = continue_nudge_text(bc, dc, &blocked, &done);
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
    let reply = state
        .store
        .write_async(move |conn| {
            let now = now_f64() as i64;
            // Read the PRIOR owner in the same transaction as the swap. The card
            // log must be able to answer "which lane claimed this, and did it
            // already own it", because that is the exact question AF-79 could
            // NOT answer. A lane created a card, it was auto-picked-up, and the
            // log said only "Auto-picked up from queue" with no lane; the owner
            // was then reassigned one second later, so a reader inspecting the
            // card afterward could not tell the pickup HAD honored ownership and
            // read it as a cross-owner dispatch. Recording the claimer (and the
            // prior owner when it differs) makes the card self-explaining and
            // makes a genuine mis-dispatch (prior != claimer, which every
            // caller is supposed to prevent) visible in the card's own log.
            let prior_owner: String = conn
                .query_row(
                    "SELECT COALESCE(session,'') FROM issues WHERE id=?1 AND status=?2",
                    rusqlite::params![card_s, from],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or_default();
            let n = conn.execute(
                "UPDATE issues SET status='doing', session=?1, updated=?2 WHERE id=?3 AND status=?4",
                rusqlite::params![session_s, now, card_s, from],
            )?;
            if n == 0 {
                // Not todo any more (closed, discarded, already doing, or gone):
                // do NOT touch the log or claim — applied=false tells the caller.
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            }
            let existing: Option<String> = conn
                .query_row(
                    "SELECT log FROM issues WHERE id=?1",
                    rusqlite::params![card_s],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            let reassigned = !prior_owner.is_empty() && prior_owner != session_s;
            let entry = if reassigned {
                // Should be unreachable from every caller (auto-pickup selects on
                // i.session=lane; manual claim 409s when owner != claimer), so if
                // it fires it IS the cross-owner claim to investigate, so say so in
                // the card log AND the server log so a sweep finds it.
                tracing::warn!(
                    target: "amux::board_drive", claimer = %session_s, prior = %prior_owner, card = %card_s,
                    "claim REASSIGNED a card across owners; every caller is supposed to prevent this; \
                     if you see this, a dispatch path let a lane claim another lane's card"
                );
                if let Ok(mut g) = cross_owner_w.lock() {
                    *g = Some(prior_owner.clone());
                }
                format!("Auto-picked up from queue by {session_s} (reassigned from {prior_owner})")
            } else if from == "backlog" {
                // A backlog card is never queue-dispatched; this line only ever
                // means a deliberate manual claim, so say that.
                format!("Claimed from backlog by {session_s}")
            } else {
                format!("Auto-picked up from queue by {session_s}")
            };
            let hhmm = chrono::Local::now().format("%H:%M").to_string();
            let log = bs::append_log(existing.as_deref(), &hhmm, &entry);
            conn.execute(
                "UPDATE issues SET log=?1 WHERE id=?2",
                rusqlite::params![log, card_s],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    let claimed = matches!(reply, Ok(r) if r.applied);
    if !claimed {
        tracing::info!(
            target: "amux::board_drive", %session, %card,
            %from,
            "claim NOT applied — card left the expected status between select and claim \
             (raced to a terminal/doing state, or store unreadable); prompt NOT dispatched"
        );
        return false;
    }
    // THE VIOLATION GETS ITS OWN TYPE, and it is emitted BEFORE `task.claimed`
    // so the ledger reads in causal order: the guard fired, then the claim
    // landed. A distinct type rather than a field on `task.claimed`, because
    // this is a different FACT — every caller is supposed to make it
    // impossible, so one row is a bug report, not a variation on a claim
    // (AMUX-3776).
    if let Some(prior) = cross_owner.lock().ok().and_then(|g| g.clone()) {
        crate::api::session_verbs::emit_event(
            state,
            session,
            "claim.cross_owner",
            Some(json!({"issue": card, "claimer": session, "prior_owner": prior, "from": from})),
            None,
            "board-drive",
        )
        .await;
    }
    crate::api::session_verbs::emit_event(
        state,
        session,
        "task.claimed",
        // `status` rides along so the progress-yields-cooldown check reads a
        // claim the same way it reads a nudge: if the lane moves this card
        // before the 15 minutes are up, that IS progress and it gets the next
        // one immediately.
        Some(json!({"issue": card, "status": "doing"})),
        None,
        "board-drive",
    )
    .await;
    true
}

/// Background driver.
pub fn spawn(state: AppState) -> super::PeriodicTask {
    let secs = std::env::var("AMUX_BOARD_DRIVE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(BOARD_DRIVE_TICK_SECS);
    super::spawn_periodic("board-drive", secs, move || {
        let state = state.clone();
        async move {
            let fleet = LiveFleet { state: state.clone() };
            let r = drive_tick(&state, &fleet).await;
            if r.assigned > 0 || r.nudged > 0 || r.promoted > 0 {
                tracing::info!(
                    assigned = r.assigned,
                    nudged = r.nudged,
                    promoted = r.promoted,
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
    if let Ok(conn) = state.store.read() {
        let lanes = crate::api::session_verbs::all_lane_names();
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
    let body = json!({
        "note": "per-lane trace of the board -> worker drive loop; `reason` says why a lane \
                 was passed over. A skip that leaves no trace is indistinguishable from a loop \
                 that is not running.",
        "loop_running": report.is_some(),
        "tick_secs": std::env::var("AMUX_BOARD_DRIVE_SECS").ok()
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
    });
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
    let todo = by_status.get("todo").and_then(Value::as_i64).unwrap_or(0);
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
        "dispatchable_todo": todo,
        "dispatchable_pct": if total > 0 { (todo as f64 * 1000.0 / total as f64).round() / 10.0 } else { 0.0 },
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

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new().route("/api/debug/board-drive", axum::routing::get(debug_board_drive))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod reviewer_renag_tests {
    use super::reviewer_acted_since_review;

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
                "INSERT INTO issues (id,title,status,session,owner_type,archived,type,updated) \
                 VALUES (?1,?2,?3,'me','agent',0,'code',100)",
                rusqlite::params![id, format!("card {id}"), status],
            )
            .expect("insert");
        };

        // 229 done, 0 blocked — my lane's real shape when the loop fired.
        for i in 0..229 {
            ins(&format!("D-{i}"), "done");
        }
        let (bc, dc, _, _) = outstanding_work(&conn, "me");
        assert_eq!((bc, dc), (0, 229));
        // The trigger is `bc > 0`. With no blocked cards there is nothing to
        // nudge about, no matter how large `done` grows.
        assert_eq!(bc, 0, "done alone must not arm the nudge");

        // Add one blocked card and it arms — because re-assessing a blocker is
        // work this lane can actually do.
        ins("B-1", "blocked");
        let (bc2, dc2, blocked, _) = outstanding_work(&conn, "me");
        assert_eq!((bc2, dc2), (1, 229));
        assert_eq!(blocked.len(), 1, "the blocked card is named in the nudge");
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
                "INSERT INTO issues (id,title,status,session,owner_type,archived,type,updated) \
                 VALUES (?1,?2,'done','me','agent',0,?3,100)",
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
                "INSERT INTO issues (id,title,status,session,owner_type,archived,deleted,type,updated)                  VALUES (?1,?2,?3,?4,?5,?6,?7,'code',100)",
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
                "INSERT INTO issues (id,title,status,session,owner_type,archived,type,updated) \
                 VALUES (?1,?2,'done','me','agent',0,'code',100)",
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
                "INSERT INTO issues (id,title,status,session,owner_type,archived,type,updated)                  VALUES (?1,?2,'done','me','agent',0,'code',?3)",
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
        let conn = Connection::open_in_memory().expect("memdb");
        conn.execute_batch(
            "CREATE TABLE issues (
                id TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '', desc TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'todo', session TEXT, creator TEXT NOT NULL DEFAULT '',
                due TEXT, created INTEGER NOT NULL DEFAULT 0, updated INTEGER NOT NULL DEFAULT 0,
                owner_type TEXT NOT NULL DEFAULT 'agent', due_time TEXT, pinned INTEGER DEFAULT 0,
                gcal_event_id TEXT, pos REAL DEFAULT 0, notified INTEGER DEFAULT 0, gate TEXT,
                shepherd TEXT, type TEXT NOT NULL DEFAULT 'code', archived INTEGER DEFAULT 0,
                depends_on TEXT, reviewer TEXT, log TEXT, rev INTEGER DEFAULT 0,
                source_ref TEXT, last_verified_at INTEGER, version INTEGER DEFAULT 0,
                epic TEXT, closed_at INTEGER, deleted INTEGER);
             CREATE TABLE issue_tags (issue_id TEXT, tag TEXT, added_at REAL,
                PRIMARY KEY (issue_id, tag));
             CREATE TABLE session_events (id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL NOT NULL,
                session TEXT NOT NULL DEFAULT '', type TEXT NOT NULL, data TEXT, idem TEXT,
                source TEXT NOT NULL DEFAULT '');
             CREATE TABLE cmd_history (id INTEGER PRIMARY KEY AUTOINCREMENT, text TEXT NOT NULL,
                type TEXT NOT NULL DEFAULT 'direct', session TEXT NOT NULL DEFAULT '',
                ts INTEGER NOT NULL, origin TEXT NOT NULL DEFAULT '');
             CREATE TABLE interaction_log (id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER,
                kind TEXT, actor TEXT, target TEXT, action TEXT, url TEXT, detail TEXT,
                before TEXT, result TEXT, ok INTEGER, ms INTEGER, seq INTEGER);
             CREATE TABLE statuses (id TEXT PRIMARY KEY, label TEXT, position INTEGER,
                is_builtin INTEGER DEFAULT 1, gate TEXT, mode TEXT DEFAULT 'implicit');
             CREATE TABLE status_scope (status TEXT, scope_type TEXT, scope_value TEXT);",
        )
        .expect("schema");
        conn
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
        let p = select_pickup(&conn, "lane", now);
        assert_eq!(claimed(&p), Some("OLD"), "the 3-day-old card must be worked before the fresh one");
    }

    #[test]
    fn a_pinned_todo_leads_regardless_of_age() {
        let conn = board_db();
        let now = 1_000_000.0;
        add_full(&conn, "OLD", "lane", "todo", "code", now as i64 - 5 * 86400, -1024.0, 0);
        add_full(&conn, "PIN", "lane", "todo", "code", now as i64, -50.0, 1); // pinned, fresh
        let p = select_pickup(&conn, "lane", now);
        assert_eq!(claimed(&p), Some("PIN"), "a pinned card is the hard human override");
    }

    #[test]
    fn a_p0_tag_lifts_a_fresh_bug_over_an_older_chore() {
        let conn = board_db();
        let now = 1_000_000.0;
        add_full(&conn, "CHORE", "lane", "todo", "chore", now as i64 - 2 * 86400, -1024.0, 0);
        add_full(&conn, "BUG", "lane", "todo", "bug", now as i64, -50.0, 0);
        tag(&conn, "BUG", "p0", now);
        let p = select_pickup(&conn, "lane", now);
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
        let p = select_pickup(&conn, "lane", now);
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
        let p = select_pickup(&conn, "lane", now_f64());
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
            let p = select_pickup(&conn, "lane", now_f64());
            assert!(claimed(&p).is_none(), "{t} must exempt the card from pickup");
            assert_eq!(eligible_todo_count(&conn, "lane", 1_000_000.0), 0, "{t} must not count as eligible");
        }
    }

    /// AMUX-3006 (Ethan: "fix the backlog thing so it drives on its own"). A lane
    /// holding ONLY backlog with nothing dispatchable in todo is the idle-drain
    /// case — board-drive dispatches `todo`, so that queue never moves on its own.
    /// The discriminator is the drain INPUTS: backlog candidates exist AND
    /// eligible_todo_count is 0. A lane with a todo is dispatched, not drained.
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
        // card armed on a live source_ref trigger must NOT appear as drain
        // candidates — nudging them churns a compliant lane with no exit but a
        // false close. Add both to a fresh lane whose ONLY other cards are parked.
        conn.execute(
            "INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type) \
             VALUES ('TW-1','burn>=90% page watch','x','backlog','parked',?1,?1,'agent','tripwire')",
            rusqlite::params![now as i64],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type,source_ref) \
             VALUES ('CH-1','chore blocked on a credential','x','backlog','parked',?1,?1,'agent','chore','clickhouse://blocked')",
            rusqlite::params![now as i64],
        )
        .unwrap();
        let parked = backlog_candidates(&conn, "parked", now as i64);
        assert!(
            parked.is_empty(),
            "a tripwire and a source_ref-triggered card are correctly parked, not drainable: {parked:?}"
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

    /// The promotion RULE, pure. Terminal for drive-to-verified is COMPLETION —
    /// `done`/`verified` only — and there must be at least one dependency, so a
    /// card with no `depends_on` (empty slice) is never "all terminal".
    #[test]
    fn deps_all_terminal_requires_at_least_one_and_all_completed() {
        assert!(deps_all_terminal(&["done", "verified"]), "all completed → promote");
        assert!(deps_all_terminal(&["verified"]), "a single verified dep is terminal");
        assert!(
            !deps_all_terminal(&[]),
            "no deps is not 'all terminal' — an unparked card must never be touched"
        );
        assert!(!deps_all_terminal(&["done", "doing"]), "one open dep blocks promotion");
        assert!(!deps_all_terminal(&["backlog"]), "backlog is not completion");
        assert!(
            !deps_all_terminal(&["discarded"]),
            "discarded is an abandonment, not a completion — must NOT re-activate"
        );
    }

    /// Drive-to-verified (the "board doesn't drive to completion" case). A card
    /// parked in `backlog` on a `depends_on` re-activates to `todo` ONLY when
    /// every dependency has completed, is NOT promoted while any dep is still
    /// open or missing, is NOT promoted when it has no deps (a triggers-only
    /// park), and is NEVER promoted for a container/dormant type or a human's
    /// card. Each `assert!` has a control on the same board so none passes
    /// vacuously (the one card that DOES promote proves the selector fires).
    #[test]
    fn backlog_promotes_only_when_every_dependency_is_terminal() {
        let conn = board_db();
        // Dependencies in the states that matter.
        let dep = |id: &str, status: &str| {
            conn.execute(
                "INSERT INTO issues (id,title,status,session,owner_type,type,updated) \
                 VALUES (?1,?1,?2,'me','agent','code',100)",
                rusqlite::params![id, status],
            )
            .expect("dep");
        };
        dep("A-done", "done");
        dep("A-verified", "verified");
        dep("A-open", "doing");

        // Parked backlog cards of every shape (owner_type='agent').
        let parked = |id: &str, typ: &str, deps: &str| {
            conn.execute(
                "INSERT INTO issues (id,title,status,session,owner_type,type,depends_on,updated) \
                 VALUES (?1,?1,'backlog','me','agent',?2,?3,100)",
                rusqlite::params![id, typ, deps],
            )
            .expect("parked");
        };
        parked("P-all-terminal", "code", r#"["A-done","A-verified"]"#); // promote
        parked("P-one-open", "code", r#"["A-done","A-open"]"#); // NOT: one dep still open
        parked("P-missing", "code", r#"["A-done","GONE"]"#); // NOT: a dep resolves to nothing
        parked("W-watch", "watch", r#"["A-done"]"#); // NOT: dormant type
        parked("E-epic", "epic", r#"["A-verified"]"#); // NOT: container type
        // A triggers-only park: NULL depends_on, terminal-deps irrelevant.
        conn.execute(
            "INSERT INTO issues (id,title,status,session,owner_type,type,updated) \
             VALUES ('P-nodeps','P-nodeps','backlog','me','agent','code',100)",
            [],
        )
        .unwrap();
        // A human's card with a completed dep — ethos rule 8, never swept.
        conn.execute(
            "INSERT INTO issues (id,title,status,session,owner_type,type,depends_on,updated) \
             VALUES ('H-1','H-1','backlog','me','human','code','[\"A-done\"]',100)",
            [],
        )
        .unwrap();

        let (got, _held) = backlog_dep_promotions(&conn);
        let ids: std::collections::HashSet<&str> = got.iter().map(|(i, _)| i.as_str()).collect();

        assert!(ids.contains("P-all-terminal"), "all-terminal deps must promote: {ids:?}");
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
            "INSERT INTO issues (id,title,status,session,owner_type,type,updated) \
             VALUES ('A-done','A-done','done','me','agent','code',100)",
            [],
        )
        .unwrap();
        // The MG-1388 shape: a terminal dep AND a live source_ref trigger.
        conn.execute(
            "INSERT INTO issues (id,title,status,session,owner_type,type,depends_on,source_ref,updated) \
             VALUES ('T-armed','T-armed','backlog','me','agent','investigation','[\"A-done\"]',\
                     'some namespace holds both an archive- and a competitor-shaped collection',100)",
            [],
        )
        .unwrap();
        // Control: same terminal dep, NO trigger -> still promotes.
        conn.execute(
            "INSERT INTO issues (id,title,status,session,owner_type,type,depends_on,updated) \
             VALUES ('T-plain','T-plain','backlog','me','agent','code','[\"A-done\"]',100)",
            [],
        )
        .unwrap();
        // A whitespace-only source_ref is NOT a live trigger -> promotes.
        conn.execute(
            "INSERT INTO issues (id,title,status,session,owner_type,type,depends_on,source_ref,updated) \
             VALUES ('T-blank','T-blank','backlog','me','agent','code','[\"A-done\"]','   ',100)",
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
        assert_eq!(claimed(&select_pickup(&conn, "lane", now_f64())), Some("T-1"));
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
    #[test]
    fn pickup_offers_a_card_only_to_its_session_owner() {
        let conn = board_db();
        // Owned by `amux`; a different lane created it (as in the incident).
        add_card(&conn, "AF-78", "amux", "todo", "owned by amux", "SCOPE: real\n- [ ] do it");
        assert!(
            claimed(&select_pickup(&conn, "amux-frustrations", now_f64())).is_none(),
            "a card owned by 'amux' must never be offered to a non-owner lane"
        );
        assert_eq!(
            eligible_todo_count(&conn, "amux-frustrations", 1_000_000.0),
            0,
            "and it must not count as eligible for the non-owner"
        );
        assert_eq!(
            claimed(&select_pickup(&conn, "amux", now_f64())),
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

    #[test]
    fn a_session_at_the_wip_cap_gets_nothing() {
        let conn = board_db();
        add_card(&conn, "D-1", "lane", "doing", "already working", "SCOPE: x");
        add_card(&conn, "T-1", "lane", "todo", "next", "SCOPE: x\n- [ ] y");
        match select_pickup(&conn, "lane", now_f64()) {
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
        assert_eq!(claimed(&select_pickup(&conn, "lane", now_f64())), Some("T-1"));
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
            claimed(&select_pickup(&conn, "lane", now_f64())),
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
        match select_pickup(&conn, "lane", now_f64()) {
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
    fn the_queue_shape_reports_the_dispatchable_slice_and_the_age_of_the_human_backlog() {
        let conn = board_db();
        let now = now_f64();
        add_card(&conn, "T-1", "lane", "todo", "workable", "SCOPE: x");
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
        assert_eq!(s["open_total"], json!(10), "1 todo + 3 needsyou + 6 backlog: {s}");
        assert_eq!(s["dispatchable_todo"], json!(1));
        assert_eq!(s["dispatchable_pct"], json!(10.0), "1 of 10, to one decimal: {s}");
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
            select_pickup(&conn, "lane", now_f64()),
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
        for cmd in ["amux board discard", "amux board retitle", "amux board type"] {
            assert!(text.contains(cmd), "the ask must still reach its exit `{cmd}`: {text}");
        }
    }

    /// Backend, 2026-08-11 afternoon: 16 claims in one hour, every card
    /// bounced `doing -> todo` with notes, nothing executed — and the drive
    /// kept dealing. Three bounced claims in 2h now stop the deal. The
    /// controls: two bounces do NOT trip it, and a bounced card that MOVED
    /// (worked, or re-shaped to backlog) no longer counts toward the trip.
    #[test]
    fn a_lane_bouncing_its_pickups_stops_being_dealt_cards() {
        let conn = board_db();
        let now = now_f64();
        for n in 1..=3 {
            add_card(&conn, &format!("B-{n}"), "lane", "todo", "bounced back", "SCOPE: x\n- [ ] y");
            conn.execute(
                "INSERT INTO session_events (session, type, ts, data) VALUES ('lane','task.claimed',?1,?2)",
                rusqlite::params![now - 600.0 * n as f64, format!("{{\"issue\":\"B-{n}\",\"status\":\"doing\"}}")],
            )
            .expect("claim event");
        }
        add_card(&conn, "T-1", "lane", "todo", "fresh work", "SCOPE: x\n- [ ] y");
        match select_pickup(&conn, "lane", now) {
            Pickup::None { reason, .. } => assert_eq!(reason, "bounce-loop"),
            _ => panic!("three bounced claims in 2h must stop the deal"),
        }
        // Working one of them (todo -> done) releases the breaker: only
        // claims whose card is still parked in todo count.
        conn.execute("UPDATE issues SET status='done' WHERE id='B-1'", []).expect("advance");
        assert!(
            !matches!(select_pickup(&conn, "lane", now), Pickup::None { reason: "bounce-loop", .. }),
            "moving a bounced card must release the breaker"
        );
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
            claimed(&select_pickup(&conn, "lane", now)),
            Some("T-1"),
            "a human-blocked doing card must not idle the lane"
        );
        // Sub-tagged asks are the same state (the LIKE form both loops share).
        let conn2 = board_db();
        add_card(&conn2, "D-2", "lane", "doing", "waiting on a decision", "SCOPE: x");
        tag(&conn2, "D-2", "needs:you:decision", now);
        add_card(&conn2, "T-2", "lane", "todo", "next", "SCOPE: x\n- [ ] y");
        assert_eq!(claimed(&select_pickup(&conn2, "lane", now)), Some("T-2"));
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
            claimed(&select_pickup(&conn, "lane", now_f64())),
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
        assert!(claimed(&select_pickup(&conn, "lane", now_f64())).is_none());
        assert_eq!(eligible_todo_count(&conn, "lane", now_f64()), 0);
    }

    /// py:14515, AMUX-1857: sessions legitimately return owner-blocked cards to
    /// todo after doing the workable prep; without the cooldown auto-pickup
    /// re-claimed the same card at the very next idle — infinite churn.
    #[test]
    fn a_recently_claimed_card_cools_down_but_the_dead_zone_is_closed() {
        // The re-claim cooldown is now 2h (AMUX-2987), aligned with the
        // bounce-breaker window, NOT 24h. Pins both ends: still exempt inside
        // the window, dispatchable again the moment it passes — which is the
        // fix for the idle-lane stall (a card claimed 3h ago used to be dead
        // for another 21 hours while its lane sat idle).
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
        // 1h ago: inside the 2h window -> still exempt.
        claim_at(now_f64() - 3600.0);
        assert!(
            claimed(&select_pickup(&conn, "lane", now_f64())).is_none(),
            "a card claimed 1h ago is inside the 2h cooldown and must not re-deal"
        );
        // 3h ago: THE DEAD ZONE. Under the old 24h cooldown this was exempt for
        // 21 more hours and the lane starved; now it is dispatchable.
        claim_at(now_f64() - 3.0 * 3600.0);
        assert_eq!(
            claimed(&select_pickup(&conn, "lane", now_f64())),
            Some("T-1"),
            "a card claimed 3h ago (past the 2h window) must be re-dealt — this is the AMUX-2987 fix"
        );
    }

    /// py:14510: fossils get triaged by a human, not silently executed at idle.
    #[test]
    fn a_card_nobody_has_touched_in_seven_days_is_not_auto_run() {
        let conn = board_db();
        add_card(&conn, "T-1", "lane", "todo", "fossil", "SCOPE: x\n- [ ] y");
        conn.execute(
            "UPDATE issues SET updated=?1 WHERE id='T-1'",
            rusqlite::params![now_f64() as i64 - 8 * 86400],
        )
        .expect("age");
        assert!(claimed(&select_pickup(&conn, "lane", now_f64())).is_none());
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
        assert_eq!(claimed(&select_pickup(&conn, "lane", now_f64())), Some("T-1"));
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
        assert!(claimed(&select_pickup(&conn, "lane", now_f64())).is_none());
    }

    #[test]
    fn a_card_blocked_by_an_open_dependency_is_skipped_and_a_closed_one_is_not() {
        let conn = board_db();
        add_card(&conn, "B-1", "other", "doing", "blocker", "SCOPE: x");
        add_card(&conn, "T-1", "lane", "todo", "dependent", "SCOPE: x\n- [ ] y");
        conn.execute("UPDATE issues SET depends_on='[\"B-1\"]' WHERE id='T-1'", []).expect("dep");
        assert!(claimed(&select_pickup(&conn, "lane", now_f64())).is_none());
        conn.execute("UPDATE issues SET status='verified' WHERE id='B-1'", []).expect("close");
        assert_eq!(claimed(&select_pickup(&conn, "lane", now_f64())), Some("T-1"));
    }

    /// Invariant 20: silence is the correct output for an empty board, and the
    /// trace must SAY so rather than being absent.
    #[test]
    fn an_empty_queue_produces_a_reason_not_a_silent_return() {
        let conn = board_db();
        match select_pickup(&conn, "lane", now_f64()) {
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
        match select_pickup(&conn, "lane", now_f64()) {
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

    /// py:12979: the message asks the lane to "re-state it on the card so it
    /// resurfaces fresh". A card edited SINCE the last re-nag counts as
    /// re-confirmed — a guard whose prescribed remedy cannot clear it teaches
    /// sessions to ignore it.
    #[test]
    fn re_stating_a_needs_you_card_resets_the_window() {
        let conn = board_db();
        add_card(&conn, "D-1", "lane", "doing", "asked Ethan", "SCOPE: x");
        tag(&conn, "D-1", "needs:you", now_f64() - 10.0 * 86400.0);
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
        assert!(
            needsyou_renag_text(&conn, "lane", "D-1", "t", 10.0 * 86400.0, 0, now_f64()).is_none(),
            "a re-stated ask must not be re-nagged"
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

        match select_advance_with(&conn, "lane", &[], now_f64(), &|_, r| {
            Some(format!("`{r}` is not a registered worker"))
        }) {
            Advance::None { reason, detail } => {
                assert_eq!(reason, "reviewer-unreachable");
                assert!(detail.contains("amux-rust"), "must name the dead reviewer: {detail}");
            }
            Advance::Nudge { kind, target, .. } => panic!("an unreachable reviewer must not be nudged (kind={kind}, target={target})"),
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
        let Pickup::Claim { prompt, .. } = select_pickup(&conn, "lane", now_f64()) else {
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
        let Pickup::Claim { prompt: p2, .. } = select_pickup(&conn2, "lane", now_f64()) else {
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
            "INSERT INTO statuses (id,label,position,gate,mode) VALUES ('verified','Verified',6,NULL,'explicit')",
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
                for cmd in ["amux board discard", "amux board type", "amux board epic"] {
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
    #[test]
    fn the_real_board_verbs_are_accepted() {
        assert_cli_verbs_exist("amux board show X, amux board done X, amux board reviewer X y");
    }

}
