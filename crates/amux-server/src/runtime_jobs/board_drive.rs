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

/// py:12961 `_ADVANCE_COOLDOWN` — never push the same lane twice inside this.
const ADVANCE_COOLDOWN_S: f64 = 15.0 * 60.0;
/// py:12855 `_DECOMPOSE_NUDGE_COOLDOWN`.
const DECOMPOSE_COOLDOWN_S: f64 = 6.0 * 3600.0;
/// py:14514 freshness gate — never auto-run a card nobody has touched in 7 days.
const PICKUP_FRESHNESS_S: i64 = 7 * 86400;
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
}

/// The live fleet: session envs on disk, `session_verbs`' liveness and boundary
/// gate, `steer_enqueue` for delivery.
pub struct LiveFleet {
    pub state: AppState,
}

impl Fleet for LiveFleet {
    fn lanes(&self) -> Vec<String> {
        crate::api::session_verbs::all_lane_names()
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
        crate::api::session_verbs::steer_enqueue(&self.state, lane, text, "board-drive", "").await;
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
pub fn pickup_junk_reason(title: &str, desc: &str) -> String {
    use std::sync::OnceLock;
    static ARTIFACT: OnceLock<regex::Regex> = OnceLock::new();
    static CAPS_HEAD: OnceLock<regex::Regex> = OnceLock::new();
    static STRUCTURE: OnceLock<regex::Regex> = OnceLock::new();
    static PROMPT: OnceLock<regex::Regex> = OnceLock::new();

    let folds = desc.matches("New task:").count();
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
    if caps.find_iter(desc).count() >= 2 || structure.is_match(desc) {
        return String::new();
    }
    if folds >= 2 {
        return format!("journal card ({folds} folded tasks)");
    }
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

fn card_event_count(conn: &Connection, etype: &str, card: &str, since: f64) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM session_events WHERE type=?1 AND ts > ?2 AND data LIKE ?3",
        rusqlite::params![etype, since, format!("%\"{card}\"%")],
        |r| r.get(0),
    )
    .unwrap_or(0)
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
    let fresh_cut = (now as i64) - PICKUP_FRESHNESS_S;
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
/// re-evaluates by hand — it is NOT "deps terminal". The drive-to-verified
/// promotion pass must leave such a card parked even when its `depends_on` are
/// all terminal, exactly as the drain and rot sweeps already do (their DELETE
/// guards carry `COALESCE(source_ref,'')=''`).
///
/// Without this, a card carrying a satisfied dependency AND a live trigger is
/// dragged out of `backlog` every tick — the promotion sees terminal deps and
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
    let cap = wip_cap();
    let holding: Vec<String> = conn
        .prepare(
            "SELECT id FROM issues WHERE session=?1 AND status='doing' AND deleted IS NULL \
             AND COALESCE(archived,0)=0 AND COALESCE(type,'') NOT IN ('tripwire','watch','epic') \
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
    let fresh_cut = (now as i64) - PICKUP_FRESHNESS_S;
    let reclaim_cut = now - reclaim_cooldown_s();
    let ids: Vec<String> = conn
        .prepare(
            &format!(
                "SELECT i.id FROM issues i WHERE {DISPATCHABLE_WHERE} \
                 ORDER BY COALESCE(i.pos, 0) ASC, i.created ASC LIMIT 16"
            ),
        )
        .and_then(|mut st| {
            st.query_map(rusqlite::params![session, fresh_cut, reclaim_cut], |r| {
                r.get::<_, String>(0)
            })
            .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return Pickup::None {
            reason: "no-eligible-card",
            detail: format!(
                "queue holds nothing dispatchable (needs:you, archived, dormant, \
                 stale >7d and cards claimed in the last {}h are all exempt)",
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
        let junk = pickup_junk_reason(&row.title, &blob_desc);
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
         For each card where ALL of these hold:\n\
         1. CI/CD passed on the merged commit\n\
         2. The change is deployed to production\n\
         3. Confirmed working end-to-end in prod\n\
         4. No regressions in existing behavior\n\n\
         Move it to `verified` with evidence of what you checked. If you cannot confirm \
         all four, leave it in `done`. If the work is stale or was superseded, archive \
         it with a note explaining why.\n\n\
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
    conn.prepare(
        "SELECT id, title, created FROM issues \
         WHERE session=?1 AND status='backlog' AND deleted IS NULL \
         AND COALESCE(archived,0)=0 AND owner_type='agent' \
         AND type NOT IN ('tripwire','watch','epic') AND COALESCE(source_ref,'')='' \
         ORDER BY created DESC LIMIT 8",
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
fn outstanding_work(
    conn: &Connection,
    session: &str,
) -> (i64, i64, Vec<(String, String)>, Vec<(String, String)>) {
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
    let (dc, done) = query("done");
    (bc, dc, blocked, done)
}

/// The `{blocked, done}` the LAST continue-nudge reported for this lane, or
/// None if it has never fired (or the row is unreadable, which must re-arm
/// rather than suppress — a nudge that goes silent on a parse error is the
/// failure this whole card is about, one layer down).
///
/// Extracted so it can be TESTED. Inline in the scan loop it was unreachable
/// from a test, which is exactly how the un-terminated version shipped.
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
    done: &[(String, String)],
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
            .map(|(id, title)| {
                let t: String = title.chars().take(65).collect();
                format!("  {id} {t}")
            })
            .collect();
        if done_count > done.len() as i64 {
            lines.push(format!("  ... and {} more", done_count - done.len() as i64));
        }
        sections.push(format!(
            "{done_count} done card(s) — verify each in prod or archive if \
             superseded.\n{}",
            lines.join("\n")
        ));
    }

    format!(
        "[amux auto-continue] Your todo queue is empty but you still have \
         outstanding work:\n\n{}\n\n\
         Keep going until everything is verified, archived, or genuinely blocked \
         on someone else. Don't stop between cards.",
        sections.join("\n\n")
    )
}

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
                " That is a BACKLOG, not a work queue, and picking it up one card per turn costs \
                 a full scope-and-decide cycle each time. Before working through it: check \
                 whether these are actually READY (a card that is real work but not yet ready is \
                 `backlog`, not `todo` — backlog is never auto-picked), and whether several \
                 should be handled together or triaged in one pass. Re-shaping means MOVING \
                 cards: not-ready ones to `backlog` (with what would make them ready), \
                 owner-blocked ones to review/reassigned. It never means returning a READY card \
                 to todo with notes — a todo-bounce re-queues it behind a 24h cooldown, and a \
                 lane that bounces every pickup converts its whole queue into cooldown while \
                 doing no work. THIS card you claimed: either advance it one real step now, or \
                 move it where it honestly belongs.",
            );
        }
    }
    // The delivery boundary parses the card id back out of this exact template
    // to void a stale pickup (AMUX-3052, session_verbs::pickup_card_id keyed on
    // "Claimed board card <ID> from your queue"). If you reword this line, update
    // that parser or the stale-pickup guard silently stops firing.
    let mut prompt = format!(
        "[amux auto-pickup] Claimed board card {} from your queue — work it now. Anything quoted \
         below is the CARD's stored text (historical log), not a live message. If the card turns \
         out to be blocked on an OWNER decision, do NOT return it to todo (it would re-queue for \
         pickup after a 24h cooldown) — move it to review or reassign it to the owner \
         instead:\n{}{}",
        row.id,
        quoted_card_text(&row.title, &row.id),
        qnote
    );
    let desc = format!("{}\n{}", row.desc, row.log.clone().unwrap_or_default());
    let desc: String = desc.trim().chars().take(500).collect();
    if !desc.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&quoted_card_text(&desc, &row.id));
    }
    prompt
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
    for (id, status) in &cands {
        if card_event_count(conn, "advance.nudged", id, day_ago) < budget {
            chosen = Some((id.clone(), status.clone()));
            break;
        }
    }
    let Some((card_id, status)) = chosen else {
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
    // paths, three verdicts, one unchanged card).
    let blob_desc = format!("{}\n{}", row.desc, row.log.clone().unwrap_or_default());
    let why = pickup_junk_reason(&row.title, &blob_desc);
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
        let text = format!(
            "[amux] {card_id} is a captured prompt, not a unit of work — {why}. It cannot move \
             through the gates as it stands, so it is holding your WIP slot and nothing is \
             driving it.\n\n\
             Split it: create one card per unit of work that can honestly be finished \
             (`amux board add \"...\"` for each), then discard {card_id} with a pointer to them, \
             or retype it if it is really one unit. Then drive each new card through its gates \
             to done.\n\n\
             If the work is ALREADY finished, that is exactly the case to split and close \
             honestly rather than leave open — the board is the record that it happened."
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
                kind: "renag",
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

/// py:13878 — the nudge body.
///
/// Option 5 exists because a prompt offering exactly `done` or `todo` about a
/// standing-role or mis-shaped card forces a false statement either way, and the
/// less-wrong pick recycles the card into the rot queue forever. Option 3b names
/// the command for parking on an external condition, because an exit the reader
/// has to go and discover is one they may reasonably conclude does not exist.
fn advance_text(m: AdvanceMsg<'_>) -> String {
    let reviewer_owns_gate = !m.reviewer.is_empty() && reviewer_acts_next(m.status);
    // py:13845 RE-TYPE, the honest exit the menu never offered (AMUX-2478). Four
    // finished cards sat terminal-at-done re-firing this nudge because, typed
    // `code`, they faced gates (CI green / deployed / confirmed-in-prod) with
    // NOTHING TO BIND TO, and correctly refused all three exits offered: false
    // verified, fabricated trigger, false discard. When a gate does not fit, the
    // fix is the TYPE, not the truth.
    let eff_type = if m.item_type.trim().is_empty() {
        DEFAULT_ITEM_TYPE
    } else {
        m.item_type.trim()
    };
    let retype = if eff_type.eq_ignore_ascii_case("code") {
        let lead = if m.has_evidence { "" } else { "THIS CARD LOOKS MIS-TYPED. " };
        let no_ev = if m.has_evidence {
            ""
        } else {
            " — and its description carries no commit, PR or merge reference, which is what a \
             code card would have by now"
        };
        format!(
            "  1b. {lead}If the work on this card is NOT CODE — a doc or file move, an \
             investigation whose result was negative, a research finding, a chore — then the \
             gate above does not fit it, and the reason is the card's TYPE, not the work. It is \
             typed `code`, so it inherits code's gates{no_ev}. Retyping is the HONEST exit and \
             it already exists:\n       amux board type {} <investigation|research|doc|chore|ops>\n\
             \x20    Those types gate on 'Outcome recorded in the item' for done and 'Outcome \
             confirmed to still hold' for verified — satisfiable truthfully for work that ships \
             no code. Fix the type, not the truth; never ack a merge or a deploy that did not \
             happen.\n",
            m.card
        )
    } else {
        String::new()
    };
    // py:13884, AC-316 defect 1: telling the HOLDER of a review card to "satisfy
    // the done gate and move it" offers an exit only the named reviewer can
    // take, while the closing line forbids the force that is the sole way to
    // obey. Same predicate as the reviewer edge, not re-derived.
    let option_one = if reviewer_owns_gate {
        format!(
            "  1. Address the reviewer's feedback on the card, then ask {} to re-ack — \
             '{}'->'{}' is {}'s sign-off, not yours; do NOT force it. If their feedback is \
             already addressed, say so on the card and ping them.\n",
            m.reviewer, m.status, m.gate_next, m.reviewer
        )
    } else {
        format!(
            "  1. Advance it. The gate for '{}' is:\n{}\n     Satisfy those honestly and move \
             it, then continue to the next card.\n",
            m.gate_next, m.gate_txt
        )
    };
    let ball = if m.ball_with_author.is_empty() {
        String::new()
    } else {
        format!("\n{}.\n", m.ball_with_author)
    };
    format!(
        "[amux] You went idle holding {} in '{}': {}\n\n{}Keep driving it. Do exactly one of:\n\
         {}{}\
         \x20 2. If it is genuinely finished, close it out to {} with the evidence.\n\
         \x20 3. If it is BLOCKED, say what on — and if the blocker is another card, go work \
         that dependency instead of waiting.\n\
         \x20 3b. If it is blocked on something EXTERNAL that no one here controls (a provider \
         outage, a deploy that cannot run, a third-party queue): record the condition and the \
         resume trigger on the card, then `amux board backlog {} --trigger \"<the external \
         condition>\"`. The --trigger records it as the card's source_ref and stamps \
         last_verified_at, so a trigger nobody re-checks becomes detectable instead of the card \
         sleeping forever — parking without it buys silence with no expiry. Do NOT leave it in \
         'doing' (this nudge re-fires) or move it to 'todo' (the untracked-work guard fires \
         instead). If it is a standing watcher rather than a one-off wait, retype it `watch` so \
         it also stays out of auto-pickup and shows under is:armed.\n\
         \x20 4. If it is blocked on a HUMAN decision, record that on the card and pick up the \
         next unblocked one.\n\
         \x20 5. If NEITHER done nor todo would be a TRUE statement about this card — a standing \
         role, a journal, a mis-shape — it cannot rot because it cannot finish: DISCARD it with \
         a note pointing at the closable units (or retype it tripwire/watch if it is a real \
         dormant watch).\n\n\
         You have {} more card(s) queued. Do not stall on a full queue: the aim is every card \
         driven to {}, working dependencies first. Never --force a gate you cannot satisfy — an \
         honest blocker beats a false 'done'.",
        m.card,
        m.status,
        quoted_card_text(&m.title.chars().take(110).collect::<String>(), m.card),
        ball,
        option_one,
        retype,
        m.term,
        m.card,
        m.queued,
        m.term,
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
        "[amux] {card} has been waiting on a human answer for at least {days} days: {}\n\n\
         {arch}Not asking you to advance it — you cannot. Asking whether the ASK is still right: \
         is the question you recorded still the question? If it is, re-state it on the card and \
         that counts as re-confirming it — you will not be asked again for {} days. If it has \
         been overtaken by events, clear the needs:you tag and move the card to whatever is now \
         true.",
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
        return LaneTrace::skip(lane, "mid-turn", "lane is not at a turn boundary")
            .with_counts(eligible, open);
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
        fleet.deliver(&target, &text).await;
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
        let etype = if kind == "renag" { "needsyou.renag" } else { "advance.nudged" };
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
    let card_s = card.to_string();
    // Assign the claimer as part of the swap. For auto-pickup this is a no-op
    // (the card is already `i.session=lane`), but it makes a MANUAL claim
    // (AMUX-3131: POST /api/board/{id}/claim) take ownership of an unassigned
    // card in the same atomic step, rather than leaving it `doing` with a stale
    // owner.
    let session_s = session.to_string();
    let reply = state
        .store
        .write_async(move |conn| {
            let now = now_f64() as i64;
            let n = conn.execute(
                "UPDATE issues SET status='doing', session=?1, updated=?2 WHERE id=?3 AND status='todo'",
                rusqlite::params![session_s, now, card_s],
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
            let hhmm = chrono::Local::now().format("%H:%M").to_string();
            let log = bs::append_log(existing.as_deref(), &hhmm, "Auto-picked up from queue");
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
            "auto-pickup NOT claimed — card left 'todo' between select and claim \
             (raced to a terminal/doing state, or store unreadable); prompt NOT dispatched"
        );
        return false;
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
    });
    (axum::http::StatusCode::OK, axum::Json(body)).into_response()
}

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new().route("/api/debug/board-drive", axum::routing::get(debug_board_drive))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

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
                epic TEXT, deleted INTEGER);
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
        assert!(text.contains("close it out to done"), "must aim at done: {text}");
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
        assert!(text.contains("close it out to verified"), "must aim at verified: {text}");
    }

    #[test]
    fn a_shell_card_in_doing_gets_a_split_ask_not_an_advance_nudge() {
        let conn = board_db();
        add_card(&conn, "S-1", "lane", "doing", "shell", "**Prompt:** please do a thing");
        match select_advance(&conn, "lane", &[], now_f64()) {
            Advance::Nudge { kind, text, .. } => {
                assert_eq!(kind, "decompose-asked");
                assert!(text.contains("Split it"), "{text}");
            }
            Advance::None { reason, detail } => panic!("expected a split ask, got {reason}: {detail}"),
        }
    }

    // --- predicates ------------------------------------------------------

    #[test]
    fn the_junk_predicate_matches_pythons_specimens() {
        // Structure beats the fold count: a real card with fold RESIDUE.
        let structured = "ROOT CAUSE: the thing\nNew task: a\nNew task: b\nNew task: c";
        assert_eq!(pickup_junk_reason("MG-1328 real investigation", structured), "");
        // A true journal: folds, no structure.
        assert!(pickup_junk_reason("journal", "New task: a\nNew task: b").contains("journal card"));
        // GCA-85 + creative-dna: the artifact word must be the SUBJECT.
        assert!(
            pickup_junk_reason("Investigate the canary alerting path", "").is_empty(),
            "a card ABOUT a canary is not a canary (GCA-85: three investigation cards fired \
             on a mid-title mention)"
        );
        assert!(pickup_junk_reason("[test-hygiene] flaky suite", "").is_empty(), "area prefix is vocabulary");
        assert!(!pickup_junk_reason("[TRIPWIRE, fires on recurrence]", "").is_empty());
        assert!(!pickup_junk_reason("probe: is the server up", "").is_empty());
        // Shells.
        assert!(pickup_junk_reason("x", "**Prompt:** /compact").contains("slash command"));
        assert!(pickup_junk_reason("x", "**Prompt:** go fix the thing").contains("captured chat prompt"));
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
}
