//! Autofix — notice, file, hand off. **Not a repair engine.**
//!
//! The name is the owner's, the shape is deliberately smaller than the name:
//! amux does not fix anything here. It NOTICES that something broke, files one
//! board card per distinct fault with the evidence already computed, and lets
//! the existing board drive hand that card to a lane at its next turn
//! boundary. Every step is an existing primitive:
//!
//! | step        | primitive        | who does it            |
//! |-------------|------------------|------------------------|
//! | notice      | this runtime job | the server             |
//! | record      | board card       | the server             |
//! | route       | `board_drive`    | the server             |
//! | **fix**     | the worker       | a model, with a turn   |
//!
//! There is no ninth thing. No queue of its own (the board is the queue), no
//! scheduler of its own (`PeriodicTask`), no delivery path of its own
//! (`board_drive` already claims a `todo` card into `doing` under WIP-1 and
//! steers the lane). The single new artifact is a *detector*, and a detector
//! is not a primitive — it is a read over instruments amux already keeps.
//!
//! # It runs OUTSIDE every worker, on purpose
//!
//! Detection and filing happen in the SERVER process. Nothing in this file
//! touches a pane, a send, a tmux capture or a turn boundary; it reads SQLite
//! and the filesystem and it writes a row. That is the entire point: **the
//! thing that watches for breakage must not share fate with the thing that
//! breaks.** If every lane in the fleet is wedged, rate-limited, crashed or
//! simply idle, the cards still get filed and queue up for whenever a worker
//! comes back. Noticing is infrastructure; fixing is work.
//!
//! The corollary is that this job must never be "smart". It has no model call
//! anywhere — titles are COMPUTED from the signature (ethos rule 2: the
//! 12-15k-token label call is the most wasteful touchpoint this repo has
//! measured, and the throttle it needed is why most commands never reached the
//! board). Everything a model should decide — is this real, is it worth
//! fixing, what is the fix — is left for the lane that picks the card up, with
//! the evidence in hand. That is what compounds when the model gets better.
//!
//! # One filing path, many detectors
//!
//! [`DetectorKind`] enumerates what we watch. Each detector is a pure function
//! over a `&Connection` (plus, for the two that need it, the filesystem)
//! returning `Vec<Finding>` and `Vec<Suppressed>` — so dedupe, card shape,
//! ownership, quiet-detection and the debug surface are written ONCE and
//! cannot drift between detectors. Adding a seventh detector is a new match
//! arm, never a new job.
//!
//! # Dedupe is durable, because a restart must not refile
//!
//! The key is `session_events.idem = "autofix:<signature>"`, protected by the
//! existing `idx_sev_idem` unique partial index — the same mechanism
//! `board_drive` uses for its decompose ask. A signature filed before the
//! process died is still filed after it comes back. The card also carries
//! `issues.source_ref = "autofix:<signature>"`, which is how the quiet sweep
//! finds it again later without a second bookkeeping table.
//!
//! # "Stopped happening" is not "fixed"
//!
//! When a filed signature goes quiet for `AMUX_AUTOFIX_QUIET_H`, the card gets
//! a log line saying so — and nothing else. It is not closed, not archived,
//! not moved. Whether silence means fixed, or means the caller gave up, or
//! means the feature is now unreachable, is a judgment with evidence on both
//! sides, and it belongs to the worker holding the card (ethos rule 8).
//!
//! # Suppression is visible or it did not happen
//!
//! Every decision NOT to file is recorded with its reason and surfaced on
//! `GET /api/debug/autofix`. A detector that silently declines is exactly the
//! failure this whole subsystem exists to end: an absence nobody can see.

use crate::api::request_log as rl;
use crate::api::AppState;
use crate::db::board_store as bs;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Knobs. Every threshold in this file is named, defaulted and printed on the
// debug surface — a threshold you cannot read back is a threshold nobody can
// argue with (and the spin-catcher's `cpu >= 70`, which sat below the idle
// baseline, is what that costs).
// ---------------------------------------------------------------------------

/// Default tick. Slow on purpose: nothing here is time-critical, and a filer
/// that runs hot competes with the traffic it is measuring.
pub const AUTOFIX_TICK_SECS: u64 = 120;

use crate::config::env_f64;
use crate::config::env_i64;
fn env_str(key: &str) -> String {
    std::env::var(key).unwrap_or_default().trim().to_string()
}

/// May autofix FILE its findings as board cards on this instance?
///
/// Autofix findings — invariant failures, the disk floor, dead routes, latency
/// outliers, stale steering — are the SERVER's OWN health/ops concerns. On the
/// local single-instance box the board IS the ops board, so this is on by
/// default. But cloud is PER-TENANT: each customer org runs its own server, so
/// filing them drops the operator's health cards onto the CUSTOMER's board
/// (AMUX-3014: a customer org carried "disk … below floor" and "invariant …
/// failing" it never created). The cloud image sets `AMUX_OPS_HEALTH_CARDS=0`;
/// the operator reads cloud health from `/health` + `/api/health/invariants`
/// instead of from cards inside a tenant. Single codebase, env-driven, no build
/// flag — unset means the local default (on).
fn ops_health_cards_enabled() -> bool {
    // The explicit knob wins in either direction — an operator can force-on
    // inside a container, or force-off on a shared box.
    if let Ok(v) = std::env::var("AMUX_OPS_HEALTH_CARDS") {
        return parse_ops_health_cards(Some(&v));
    }
    // No knob set: ON by default, but OFF in an isolated SINGLE-TENANT container.
    // Such a container's board IS the customer's, so a cloud image that never
    // sets the knob still must not leak the operator's health cards onto it.
    // `IS_SANDBOX=1` is already set by the cloud image (for the root-refusal), so
    // this defends the fix against a future deployment forgetting the explicit
    // env — the leak cannot silently recur.
    std::env::var("IS_SANDBOX").ok().as_deref() != Some("1")
}

/// Pure parse, split out so the on/off semantics are tested without env races.
/// Unset (`None`) or any non-falsey value is ON (the local default); only an
/// explicit falsey value turns it off.
fn parse_ops_health_cards(v: Option<&str>) -> bool {
    match v {
        Some(s) => !matches!(s.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"),
        None => true,
    }
}

/// Window each detector looks back over.
fn window_h() -> f64 {
    env_f64("AMUX_AUTOFIX_WINDOW_H", 6.0).clamp(0.1, 24.0 * 30.0)
}
/// Trailing baseline for the latency comparison (excludes the window).
fn baseline_h() -> f64 {
    env_f64("AMUX_AUTOFIX_BASELINE_H", 72.0).clamp(1.0, 24.0 * 90.0)
}
/// p95 must exceed the trailing p95 by this multiple to be a regression.
fn latency_mult() -> f64 {
    env_f64("AMUX_AUTOFIX_LATENCY_MULT", 3.0).max(1.1)
}
/// Absolute floor on the WINDOW p95 before a ratio can file (AMUX-3471). A
/// pure ratio with no floor filed "/api/prefs p95 2ms — 3.7x its trailing
/// norm": a 2-millisecond p95 flagged because the baseline was
/// sub-millisecond. Nobody's capability is degraded by 2ms, and a report a
/// reader laughs at teaches them to skim the ones that matter — the
/// threshold-below-baseline instrument defect, ratio edition. The ratio
/// still discriminates ABOVE the floor; below it, fast-vs-faster is noise.
fn latency_floor_ms() -> f64 {
    env_f64("AMUX_AUTOFIX_LATENCY_FLOOR_MS", 250.0).max(0.0)
}
/// Sample floor, both sides. Below this a p95 is one request wearing a
/// percentile — the "filter that matched everything" failure in slow motion.
fn latency_min_samples() -> i64 {
    env_i64("AMUX_AUTOFIX_LATENCY_MIN_SAMPLES", 30).max(5)
}
/// Endpoints whose DESIGN includes a long blocking call, with their design
/// budget (AMUX-3485). POST /api/files/mdai/run is a model synthesis whose
/// accepted duration is ~100s for a large node (AF-142 — measured, judged,
/// and recommended for acceptance); its per-node budget is MODEL_TIMEOUT_S
/// (150s). A 10s outlier threshold under a 150s design budget files a card
/// per legitimate run — the threshold-below-baseline defect, per-endpoint,
/// and with occurrence-identity signatures that is a NEW card every time.
/// The outlier still fires ABOVE the design budget, where it means the
/// endpoint's own timeout failed to bound the call — the one latency story
/// on these routes that IS wrong.
const LONG_BY_DESIGN: &[(&str, f64)] = &[
    ("/api/files/mdai/run", 150_000.0),
    // GET /api/email/inbox (AMUX-3519, third filing in one day on the same
    // residual): a count=480 metadata fetch is quota-bound at ~12-15s — the
    // 8-wide concurrency sits deliberately under Gmail's 250 units/s budget
    // (AMUX-3495), so the duration IS the design. 30s = 2x the worst
    // measured legitimate run; a single wedged transport call alone blows it
    // (the per-call reqwest timeout is 30s), so an outlier past the budget
    // still means what an outlier should: something hung, not something big.
    ("/api/email/inbox", 30_000.0),
    // GET /api/email/search (AMUX-3690). THE ARGUMENT IS NOT "it is also slow",
    // it is that this route and the one above are the SAME CODE: both call
    // `email::inbox_messages`, so both inherit its 8-wide concurrency, its
    // batch-above-20 path, and its Gmail quota bound. `search` additionally fans
    // out across every connected account concurrently when no `account=` is
    // given, and `join_all` waits for the slowest, so a wide query pays the
    // worst account's latency. The filed row was `q=primis&days=120` with no
    // account: three mailboxes, 120 days, 10.076s.
    //
    // Measured over 7 days: 3226 requests, min 131ms, mean 657ms, worst 11.35s,
    // and exactly TWO past the 10s floor. 30s is ~2.7x the measured worst, the
    // same ratio /api/board/commit-mentions uses below for the same reason, and
    // it keeps the property that makes these budgets mean something: the
    // per-call reqwest timeout is 30s, so a single wedged transport call alone
    // blows the budget. Past it still means something HUNG, not something big.
    //
    // WHAT THIS TABLE CANNOT EXPRESS, said out loud rather than worked around:
    // the budget is keyed on the PATH and the cost lives in a shared function.
    // `inbox` got a row after filing three times in one day and `search` got one
    // after filing once, and there is no mechanism that would have given either
    // a budget in advance. The next route to call `inbox_messages` will file too,
    // for a duration that was designed in before it existed.
    ("/api/email/search", 30_000.0),
    // POST /api/alert/owner (AMUX-3522, third filing on one mechanism): while
    // the Messages Automation permission stays missing (MI-4933), the
    // iMessage breaker re-probes once per 15-min cooldown and that probe IS
    // a designed 12s TCC-wall wait (AMUX-3492/3500 — stamp verified working;
    // the filed rows were legitimate probes 61 min apart). 20s = probe plus
    // a normal email send; past it means a channel actually wedged, which is
    // the story an outlier should tell about a fire alarm. Delete this row
    // when the permission is granted and the wall class dies.
    ("/api/alert/owner", 20_000.0),
    // GET /api/board/commit-mentions (AMUX-3580). The endpoint shells out to
    // `git log --grep` across every repo behind an open card — 3 repos against
    // 65 cards when measured — and that scan IS the work. Timed deliberately
    // before it was put on a job: 10.6s, 11.1s, 10.7s across three consecutive
    // calls, worst filed row 11.037s. A 10s outlier floor under an ~11s design
    // duration files a card per legitimate call, which is the
    // threshold-below-baseline defect this table exists for, and it filed one
    // within hours of the endpoint gaining its first caller.
    //
    // 30s is ~2.7x the measured worst, matching the ratio the rows above use.
    // The headroom is deliberate rather than generous: the cost scales with
    // repos x open cards, so a bigger fleet legitimately pushes this up, and a
    // future reader seeing 20s should widen the budget rather than hunt a bug.
    //
    // Past 30s the outlier still means what an outlier should. The scan bounds
    // its OUTPUT (MAX_COMMITS, reported as `truncated`) but puts no wall clock
    // on the git subprocess, so a hung git — a locked index, a stalled network
    // checkout — has nothing else to stop it. That is the one latency story on
    // this route that is genuinely wrong, and it stays reportable.
    ("/api/board/commit-mentions", 30_000.0),
    // POST /api/schedules/{id}/run (AMUX-3704). Run-now executes the schedule
    // SYNCHRONOUSLY and returns its result, so the request's duration IS the
    // scheduled work's duration — for an arbitrary user-defined command. The
    // filed row was SCHED-173 at 54.9s, a `shell` schedule whose script queries
    // signups, sends a founder-welcome email and posts to Slack. That is the
    // endpoint doing exactly what it was asked to do.
    //
    // 600s BECAUSE THE ENDPOINT ENFORCES IT, not because 600 felt right:
    // `SHELL_TIMEOUT_S` bounds the shell path at 600s, and the tmux path's
    // boundary wait is bounded by `steer_max_age_s` at the same default. So a
    // run past 600s means one of those bounds FAILED, which is the one latency
    // story on this route that is genuinely wrong and stays reportable.
    //
    // This is the first entry in this table whose duration is not amux's to
    // predict — the command belongs to whoever wrote the schedule. That is an
    // argument for the budget tracking the TIMEOUT rather than any measured
    // worst case: measure a script today and the number is wrong the next time
    // someone edits it.
    ("/api/schedules/{id}/run", 600_000.0),
];

fn design_budget_ms(target: &str) -> f64 {
    LONG_BY_DESIGN
        .iter()
        .find(|(p, _)| target == *p)
        .map(|(_, b)| *b)
        .unwrap_or(0.0)
}

/// A single request slower than this is absurd on its face, whatever the
/// family's norm is.
fn outlier_ms() -> f64 {
    env_f64("AMUX_AUTOFIX_OUTLIER_MS", 10_000.0).max(250.0)
}
/// How many times a dead route must be hit by a browser before it is a card.
fn dead_route_min_hits() -> i64 {
    env_i64("AMUX_AUTOFIX_DEAD_ROUTE_MIN_HITS", 3).max(1)
}
/// A schedule this far past `next_run` with no run recorded is not late, it is
/// not firing.
fn schedule_overdue_min() -> f64 {
    env_f64("AMUX_AUTOFIX_SCHEDULE_OVERDUE_MIN", 45.0).max(5.0)
}
/// Oldest steering row older than this means the queue is not draining.
fn steering_stale_min() -> f64 {
    env_f64("AMUX_AUTOFIX_STEERING_STALE_MIN", 90.0).max(5.0)
}

/// The same deadline for a lane that CAN receive (AMUX-3696).
///
/// A queue that will never drain and a queue that drains at the next turn
/// boundary are different facts, and 90 minutes is the right deadline for only
/// one of them. A lane mid-turn is not stuck: an agent working a single long
/// task legitimately runs past 90 minutes without reaching a boundary, so the
/// old single threshold sat BELOW the natural turn length of a busy lane and
/// filed cards against healthy ones. That is the "threshold below its own
/// baseline" failure the card template itself warns about.
///
/// 6h, because a DELIVERABLE lane that has not reached a turn boundary in six
/// hours is a real report — just a different one, and the verdict says which.
fn steering_stale_deliverable_min() -> f64 {
    env_f64("AMUX_AUTOFIX_STEERING_STALE_DELIVERABLE_MIN", 360.0).max(steering_stale_min())
}
/// An invariant must stay breached across at least this many evaluations. A
/// flapping invariant is noise, and noise is how a board stops being read.
/// How recent an incident's last failing evaluation must be to mint a card
/// (AMUX-3486). Default: the detector's own 6h window.
fn invariant_incident_fresh_s() -> f64 {
    env_f64("AMUX_AUTOFIX_INCIDENT_FRESH_S", 21_600.0).max(600.0)
}
fn invariant_min_occurrences() -> i64 {
    env_i64("AMUX_AUTOFIX_INVARIANT_MIN_OCCURRENCES", 3).max(2)
}
/// HEAD moved this long ago and the running build still has not — the deploy
/// path is broken, and every session believes its work shipped.
fn build_stale_h() -> f64 {
    env_f64("AMUX_AUTOFIX_BUILD_STALE_H", 1.0).max(0.1)
}
/// Silence for this long marks a filed signature quiet (noted, never closed).
fn quiet_h() -> f64 {
    env_f64("AMUX_AUTOFIX_QUIET_H", 24.0).max(1.0)
}

// --- CI detector knobs (AMUX-2780) ------------------------------------------

/// Repos to watch, `owner/name`, comma-separated. Defaults to amux's own repo:
/// the detector that exists because THIS repo's deploy was red for three days
/// must watch this repo without anyone opting in (ethos rule 1 — an extension
/// point nobody is enrolled in is decoration).
fn ci_repos() -> Vec<String> {
    let raw = env_str("AMUX_CI_REPOS");
    let raw = if raw.is_empty() { "mixpeek/amux".to_string() } else { raw };
    raw.split(',').map(|s| s.trim().to_string()).filter(|s| s.contains('/')).collect()
}
/// How often to actually ask GitHub. The tick is 120s; asking every tick would
/// be 720 API calls/day per repo for a signal that changes on push.
fn ci_poll_min() -> f64 {
    env_f64("AMUX_CI_POLL_MIN", 15.0).clamp(1.0, 24.0 * 60.0)
}
/// How many consecutive failures before filing.
///
/// The honest note: BOTH 1 and 2 are guesses, and the ethos is explicit that
/// picking a threshold at all is the tell that you are guessing. There is no
/// structurally-absent signal available here — "red on main" is a level, not an
/// absence. 2 is chosen because it is the smallest number that distinguishes a
/// fault from a single flaky run, and the cost of the extra wait is one push;
/// the incident this detector exists for ran to 31. Set to 1 to file on the
/// first red run.
fn ci_min_failures() -> i64 {
    env_i64("AMUX_CI_MIN_FAILURES", 2).max(1)
}
/// How many recent runs to fetch per repo.
///
/// NOT merely a page size. The window is shared across ALL workflows, and every
/// push to this repo starts three of them — so 100 runs is only ~33 per
/// workflow, and the streak anchor is stable only while the last SUCCESS is
/// still inside it. Measured on the real 2026-08-10 outage: at 100 the deploy
/// streak reported "AT LEAST 21, last success unknown" when the truth was 31
/// with a known green run. 200 recovers both. When the success still falls out,
/// the finding says so rather than reporting a floor as a count.
fn ci_run_limit() -> i64 {
    env_i64("AMUX_CI_RUN_LIMIT", 200).clamp(10, 600)
}
/// Hard timeout on each `gh` invocation. A hung network call must not stall the
/// tick that every other detector shares.
fn ci_cmd_timeout_s() -> u64 {
    env_i64("AMUX_CI_TIMEOUT_S", 20).clamp(2, 120) as u64
}

/// Signature substrings that never file. Environmental causes with no code
/// fix: put them here rather than teaching a detector to lie about them.
fn ignore_list() -> Vec<String> {
    env_str("AMUX_AUTOFIX_IGNORE")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The lane that owns a card when the fault names no worker of its own.
///
/// Empty (the default) means UNOWNED — and UNOWNED means UNREACHABLE by
/// auto-pickup, whose predicate is `i.session=?1`; only a manual claim or a
/// human scroll ever finds such a card. The previous comment here said "the
/// board's own pickup decides", which described a mechanism that does not
/// exist, and under that claim 215 auto-filed reports accumulated invisible
/// to every lane while the nightly failed 13 of 13 runs (AF-137). Set
/// AMUX_AUTOFIX_SESSION in server.env to route filings to a lane;
/// board.autofix_cards_are_dispatchable goes red when unowned reports
/// accumulate, so this gap can never again be silent.
fn fixer_session() -> String {
    env_str("AMUX_AUTOFIX_SESSION")
}

/// The dashboard toggle. Persisted in `prefs` like every other setting, read
/// on EVERY tick so the switch takes effect live — a pref that needs a restart
/// is a pref that silently disagrees with the UI showing it.
///
/// Default ON. When OFF the detectors still run and their findings are still
/// published on the debug surface as suppressed; only the WRITE is skipped. A
/// disabled watcher that also goes blind loses the evidence for exactly the
/// window somebody turned it off to look at.
pub fn enabled(conn: &Connection) -> bool {
    let v: Option<String> = conn
        .query_row("SELECT value FROM prefs WHERE key='autofix_enabled'", [], |r| r.get(0))
        .ok();
    !matches!(v.as_deref().map(str::trim), Some("0") | Some("false") | Some("off"))
}

// ---------------------------------------------------------------------------
// The shared vocabulary: every detector speaks in these.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DetectorKind {
    /// An unhandled server failure. 501/503 are excluded by construction —
    /// they are honest degradations that NAME what is missing.
    Http5xx,
    /// A family's p95 far above its own trailing norm, or a single request
    /// that is absurd on its face.
    Latency,
    /// A 404/405 on a path a BROWSER asked for — i.e. the SPA calls it and it
    /// is not there. `/api/logs/analyze` already computes the verdict.
    DeadRoute,
    /// An instrument that should be moving and is not. These cost the most,
    /// precisely because absence is not an event.
    SilentSubsystem,
    /// An invariant that stays breached across evaluations.
    InvariantBreach,
    /// The build/deploy path itself is stuck.
    BuildDeploy,
    /// Free disk is below the floor, or falling fast enough to reach it soon.
    DiskPressure,
    /// A GitHub Actions workflow is failing on the default branch and has
    /// stayed failing. The one detector whose instrument lives OUTSIDE this
    /// machine — see [`detect_ci`] for why that changes the failure modes.
    CiFailure,
    /// Open file descriptors are close to the process limit, or climbing toward
    /// it fast enough to get there.
    FdPressure,
    /// A connector/Gmail account's stored grant is revoked or expired
    /// (`needs_reauth`). The fix is one human re-approval, and until it
    /// happens every worker touching that account fails — silently, in lanes
    /// nobody watches, which is why token rot gets a card instead of waiting
    /// for the Nth lane to rediscover it (AMUX-3353).
    ConnectorAuth,
}

impl DetectorKind {
    pub fn slug(self) -> &'static str {
        match self {
            DetectorKind::Http5xx => "5xx",
            DetectorKind::Latency => "latency",
            DetectorKind::DeadRoute => "dead-route",
            DetectorKind::SilentSubsystem => "silent",
            DetectorKind::InvariantBreach => "invariant",
            DetectorKind::BuildDeploy => "build",
            DetectorKind::DiskPressure => "disk",
            DetectorKind::CiFailure => "ci",
            DetectorKind::FdPressure => "fd",
            DetectorKind::ConnectorAuth => "connector-auth",
        }
    }

    /// The board type. Never `code`: an auto-filed fault has no merged commit
    /// to claim, so a `code` card could only ever be closed by acknowledging a
    /// gate that is not true (ethos rule 3 — 1,143 of 1,215 cards were `code`
    /// and most of them could not exit honestly). `investigation` and
    /// `blocker` both close on "outcome recorded on the item", which is a
    /// sentence the fixing lane can write truthfully either way.
    pub fn item_type(self) -> &'static str {
        match self {
            DetectorKind::BuildDeploy
            | DetectorKind::SilentSubsystem
            | DetectorKind::DiskPressure
            | DetectorKind::CiFailure
            | DetectorKind::FdPressure
            | DetectorKind::ConnectorAuth => "blocker",
            _ => "investigation",
        }
    }

    pub fn all() -> [DetectorKind; 10] {
        [
            DetectorKind::Http5xx,
            DetectorKind::Latency,
            DetectorKind::DeadRoute,
            DetectorKind::SilentSubsystem,
            DetectorKind::InvariantBreach,
            DetectorKind::BuildDeploy,
            DetectorKind::DiskPressure,
            DetectorKind::CiFailure,
            DetectorKind::FdPressure,
            DetectorKind::ConnectorAuth,
        ]
    }
}

/// One fault worth a card.
#[derive(Debug, Clone)]
pub struct Finding {
    pub kind: DetectorKind,
    /// Stable across restarts and across reworded messages. This is the dedupe
    /// key and the `source_ref`; if it moves, the same fault files twice.
    pub signature: String,
    /// COMPUTED from the signature. Never a model call.
    pub title: String,
    /// Ordered evidence, rendered into the card body as `key: value` lines.
    pub evidence: Vec<(String, String)>,
    /// The exact query/command that re-checks this. The card must let a lane
    /// start from evidence rather than from a description of evidence.
    pub recheck: String,
    /// The lane best placed to fix it, or None for unowned.
    pub owner: Option<String>,
    pub count: u64,
    pub last_ts: f64,
    /// Set when the fault's only exit is a CLOCK, to the epoch it heals at
    /// (AMUX-3645). A dwell-window check holds itself red on purpose — see
    /// [`crate::invariants::InvariantResult::heals_at`] — and a card for one
    /// has no action that closes it. Filed as `backlog` carrying this as its
    /// resume trigger rather than as `todo`, so it is parked with an expiry
    /// instead of queued as work a lane cannot do.
    ///
    /// `None` on every ordinary finding, which is the answer that means "an
    /// action is what closes this".
    pub parked_until: Option<f64>,
}

/// A fault we deliberately did not file, and why. Published, always.
#[derive(Debug, Clone)]
pub struct Suppressed {
    pub kind: DetectorKind,
    pub signature: String,
    pub reason: String,
}

/// What one tick did. Held in memory for `/api/debug/autofix`; the durable
/// record is the `session_events` rows and the cards themselves.
#[derive(Debug, Clone, Default)]
pub struct AutofixReport {
    pub at: f64,
    pub enabled: bool,
    pub signatures_seen: Vec<(String, String)>,
    pub filed: Vec<(String, String)>,
    pub already_filed: Vec<String>,
    pub suppressed: Vec<(String, String, String)>,
    pub quiet_noted: Vec<(String, String)>,
    /// (card, "resolved"|"stopped being checkable") — cards told that the
    /// incident behind them is gone (AMUX-3574).
    pub resolved_noted: Vec<(String, String)>,
    /// (card, signature, local heal time) — cards filed PARKED because the
    /// fault's only exit is a clock (AMUX-3645).
    ///
    /// Here so the next one is self-announcing. A parked card is a deliberate
    /// non-event: nothing is wrong, nobody is nudged, and it therefore leaves
    /// no trace anywhere a sweep would look. That is exactly how the ORIGINAL
    /// defect stayed invisible — AMUX-3640 was filed, picked up, investigated,
    /// and discarded, and the only record that a lane had burned a pickup on a
    /// check working as designed was the prose a human wrote onto the card
    /// afterwards. If parking ever fires on something it should not, or fires
    /// on nothing at all because a declaration stopped being written, this row
    /// is what says so without anyone going to look.
    pub parked: Vec<(String, String, String)>,
    pub errors: Vec<String>,
    pub took_ms: f64,
}

static LAST_REPORT: std::sync::OnceLock<std::sync::RwLock<Option<AutofixReport>>> =
    std::sync::OnceLock::new();

fn last_report_cell() -> &'static std::sync::RwLock<Option<AutofixReport>> {
    LAST_REPORT.get_or_init(|| std::sync::RwLock::new(None))
}

pub fn last_report() -> Option<AutofixReport> {
    last_report_cell().read().ok().and_then(|g| g.clone())
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Detector 1 — 5xx.
// ---------------------------------------------------------------------------

/// Group every 5xx in the window by the SAME key `/api/logs/analyze` groups
/// by: `(status, method, family, normalize_target(path))`. Reused rather than
/// re-derived — `normalize_target` is the route-table-aware collapse, and a
/// second copy of it would drift the first time somebody adds a route.
///
/// Excluded by construction:
/// - **501 and 503.** These are the honest-degradation codes: they name the
///   absent thing and how to supply it (`/api/email/search` on an unconnected
///   account, `/api/torrents` with aria2c down, and now the iTerm2 send path).
///   Filing them would put a card on the board that no lane can act on.
/// - anything matching `AMUX_AUTOFIX_IGNORE`.
pub fn detect_5xx(conn: &Connection, now: f64) -> (Vec<Finding>, Vec<Suppressed>) {
    let cutoff = now - window_h() * 3600.0;
    let mut groups: BTreeMap<(i64, String, String, String), Group> = BTreeMap::new();
    let mut suppressed = Vec::new();
    let q = conn.prepare(
        "SELECT ts, method, path, family, status, latency_ms, client_ip, amux_session, \
                worker, error_body \
         FROM _amux_request_log WHERE status >= 500 AND ts >= ?1 ORDER BY ts ASC LIMIT 200000",
    );
    let mut stmt = match q {
        Ok(s) => s,
        Err(e) => return (vec![], vec![sup(DetectorKind::Http5xx, "query", &e.to_string())]),
    };
    let rows = stmt.query_map(rusqlite::params![cutoff], |r| {
        Ok((
            r.get::<_, f64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, f64>(5)?,
            r.get::<_, Option<String>>(6)?.unwrap_or_default(),
            r.get::<_, Option<String>>(7)?.unwrap_or_default(),
            r.get::<_, Option<String>>(8)?.unwrap_or_default(),
            r.get::<_, Option<String>>(9)?.unwrap_or_default(),
        ))
    });
    let rows = match rows {
        Ok(r) => r,
        Err(e) => return (vec![], vec![sup(DetectorKind::Http5xx, "query", &e.to_string())]),
    };
    for row in rows.flatten() {
        let (ts, method, path, family, status, latency, ip, session, worker, body) = row;
        // The honest-degradation gate, applied BEFORE grouping so a suppressed
        // code cannot dilute a real group's sample.
        if status == 501 || status == 503 {
            suppressed.push(sup(
                DetectorKind::Http5xx,
                &format!("5xx|{status}|{method}|{}", rl::normalize_target(&path)),
                &format!(
                    "{status} is an honest degradation — the response names what is missing; \
                     not a defect to file"
                ),
            ));
            continue;
        }
        let target = rl::normalize_target(&path);
        let key = (status, method.clone(), family.clone(), target.clone());
        let g = groups.entry(key).or_insert_with(|| Group {
            count: 0,
            first_ts: ts,
            last_ts: ts,
            clients: Default::default(),
            workers: Default::default(),
            sample_path: path.clone(),
            sample_body: String::new(),
            max_latency: 0.0,
        });
        g.count += 1;
        g.last_ts = ts;
        g.max_latency = g.max_latency.max(latency);
        g.clients.insert(rl::client_identity(&session, &ip));
        if !worker.is_empty() {
            g.workers.insert(worker);
        }
        // Prefer the newest row that actually carries a body — same rule
        // /analyze uses to pick its sample.
        if !body.is_empty() {
            g.sample_body = body;
            g.sample_path = path;
        }
    }

    let mut out = Vec::new();
    for ((status, method, family, target), g) in groups {
        // OCCURRENCE IDENTITY (AMUX-3591), the same fix AMUX-3472 made for
        // latency outliers, arriving here because the 5xx signature never got
        // it and the loop it causes is now measured.
        //
        // Discarding an auto-filed report DELETES its idem to re-arm the
        // detector (board.rs, AF-137/AMUX-3464) — right for a CONDITION, whose
        // refile requires the condition to be live again. It is wrong for a
        // batch of specific requests. This signature named only
        // status+method+family+target, so "recurrence" meant "any 5xx on that
        // path still inside the window", and the SAME historical rows kept
        // qualifying.
        //
        // Measured, three filings of one incident: the hang at 00:49-00:51 on
        // 2026-08-24 filed AMUX-3581; discarding it re-armed and refiled
        // AMUX-3589; discarding THAT refiled AMUX-3591. Byte-identical
        // signature each time, zero new information, and every round cost a
        // lane a full scope-and-decide turn. A worker doing exactly the right
        // thing — judging a spurious report and discarding it — was the thing
        // driving the loop.
        //
        // With the newest offending row's second-resolution timestamp, the same
        // rows re-scanned mint the SAME signature (so a discard stays judged)
        // while a genuinely new 5xx mints a new one and files regardless. The
        // re-arm skip below then keeps `5xx|` for the same reason it keeps
        // `latency|outlier|`: an occurrence, once judged, is judged forever.
        let signature = format!(
            "5xx|{status}|{method}|{family}|{target}|{}",
            g.last_ts as i64
        );
        // COMPUTED title: the signature in English, plus the count. No model.
        let title = format!("{method} {target} → HTTP {status} ({}x)", g.count);
        let recheck = format!(
            "curl -sk \"$AMUX_URL/api/logs/analyze?since_h={}\" | python3 -c \"import json,sys; \
             [print(json.dumps(x,indent=2)) for x in json.load(sys.stdin)['groups'] \
             if x['status']=={status} and x['method']=='{method}' and x['target']=='{target}']\"",
            window_h() as i64
        );
        let evidence = vec![
            ("verdict".into(), format!(
                "{method} {target} answered HTTP {status} {} time(s) from {} distinct client(s). \
                 A 5xx is amux failing, not amux declining — if this is really a refusal, the fix \
                 is the STATUS CODE, not the caller.",
                g.count,
                g.clients.len()
            )),
            ("sample_request".into(), format!("{method} {}", g.sample_path)),
            ("error_body".into(), truncate(&g.sample_body, 800)),
            ("first_seen".into(), rl::local_when(g.first_ts)),
            ("last_seen".into(), rl::local_when(g.last_ts)),
            ("count".into(), g.count.to_string()),
            ("distinct_clients".into(), format!(
                "{} ({})",
                g.clients.len(),
                g.clients.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
            )),
            ("slowest_ms".into(), format!("{:.1}", g.max_latency)),
        ];
        out.push(Finding {
            kind: DetectorKind::Http5xx,
            signature,
            title,
            evidence,
            recheck,
            owner: g.workers.iter().next().cloned(),
            count: g.count,
            last_ts: g.last_ts,
            parked_until: None,
        });
    }
    (out, suppressed)
}

/// Per-family latency samples WITH the counts that produced them (AF-178).
///
/// `win`/`base` are what survived filtering; `win_raw`/`base_raw` are how many
/// rows the detector actually looked at. Keeping both is the whole point: a
/// family with 46,825 baseline rows and zero survivors and a family with no
/// traffic at all used to render as the identical sentence, "baseline has 0
/// samples (<30)", so a dead detector wore a quiet endpoint's clothes and its
/// own debug surface certified it as normal operation.
#[derive(Default)]
struct FamilySamples {
    win: Vec<f64>,
    base: Vec<f64>,
    win_raw: usize,
    base_raw: usize,
}

#[derive(Default)]
struct Group {
    count: u64,
    first_ts: f64,
    last_ts: f64,
    clients: std::collections::BTreeSet<String>,
    workers: std::collections::BTreeSet<String>,
    sample_path: String,
    sample_body: String,
    max_latency: f64,
}

// ---------------------------------------------------------------------------
// Detector 2 — latency.
// ---------------------------------------------------------------------------

/// Two shapes, because they fail differently.
///
/// **Regression**: a family's window p95 above `mult ×` its own trailing p95.
/// Compared against the family's OWN history, never a fleet-wide number — an
/// upload family is legitimately slow and a `/health` family is not, and one
/// absolute threshold would either scream at the first or never fire for the
/// second. Both sides need `min_samples`, because a p95 over four requests is
/// one request wearing a percentile.
///
/// **Outlier**: a single request past `outlier_ms`, which is absurd whatever
/// the norm is. Filed separately because the mechanism is different — a 27s
/// upload chunk is not "the upload family got slower", it is one request that
/// went wrong, and folding it into a percentile hides it.
///
/// The percentile is `rl::percentile_sorted` — the same nearest-rank function
/// `/api/logs/stats` reports, so a card and the endpoint can never quote
/// different p95s for the same window.
/// Did this request's clock span the start of this process? (AF-175)
///
/// `latency_ms` is wall time from arrival to completion. A request that ARRIVED
/// before the current process started and completed after it came up measured
/// the outage, not the endpoint — which is how a 107-second restart was filed
/// as "11 requests exceeded 10s, worst 62.0s" on 2026-08-23.
///
/// A pure function rather than an inline comparison so the decision is testable
/// without a process-global boot time, and so a control can mutate the LOGIC
/// (flip the comparison) rather than a string. A string-matching control cannot
/// tell "the assertion works" from "the assertion is mis-specified and fails on
/// everything" — amux-frustrations' point, from a cell of theirs whose first
/// draft failed on a correct implementation.
///
/// `boot == None` keeps every row: excluding on an unknown boundary would drop
/// everything and look identical to a filter that worked.
fn spans_restart(ts: f64, latency_ms: f64, boot: Option<f64>) -> bool {
    match boot {
        // BOTH conditions. "Started before boot" alone is not "spanned the boot":
        // a request that started AND finished before this process started is
        // simply OLD, and it is legitimate baseline data. I shipped the
        // one-sided version in d0c034ad and it excluded 213,420 rows within the
        // hour — a filter matching over half the table, which is the exact
        // "silently matches everything" trap this file's rule 7 records, in the
        // code written to fix a different instance of it.
        //
        // It was caught by the INFO line I added to make the exclusion
        // auditable: `excluded=213420` on a defect whose real population was
        // eleven. Without the count it would have looked like a working filter
        // and quietly hollowed out the detector's baseline, since this server
        // restarts ~27 times a day and most of a multi-hour baseline therefore
        // predates the current boot.
        Some(b) => ts - latency_ms / 1000.0 < b && ts >= b,
        None => false,
    }
}

/// The AF-175 form: decide from the row's OWN process boot when it has one.
///
/// `_amux_request_log.boot_at` (migration 0030) records which process wrote the
/// row, so this asks a question about the row rather than about whichever boot
/// happens to be current. That matters because the process-boot form can only
/// catch rows spanning the MOST RECENT restart, and this server restarts ~27
/// times a day against detector windows measured in hours — every earlier
/// restart inside the window stayed invisible.
///
/// With the row's own boot the test is ONE-SIDED and cannot regress the way the
/// process-boot form did. A row's `ts` is by construction at or after the boot
/// of the process that wrote it. The `ts >= b` conjunct existed only to stop
/// "before the current boot" matching every legitimately old row — the trap
/// that excluded 213,420 of 213,935 rows when it was omitted.
///
/// THE LATENCY ARITHMETIC IS GONE (AMUX-3647, fixed 2026-08-24).
///
/// `ts` is the request START. `request_log.rs` stamps it before
/// `next.run(req).await` and migration 0010 says so in the column comment, so
/// `ts - latency` names a moment BEFORE the request existed. The old expression
/// `ts - latency/1000 < b` therefore reduced to `latency > ts - b`: the request
/// was in flight longer than its own process had been up when it arrived. That
/// is not "spanned a restart" and it never was.
///
/// MEASURED against the live table before changing it, because the card filing
/// this credited the wrong expression with being accidentally right, and it is
/// not:
///
///   - 0 of 97,019 rows carrying a `boot_at` have `ts < boot_at`. A request
///     cannot arrive before its own process bound the listener, and the startup
///     order makes that structural: migrations run inside `Store::open`,
///     `record_boot` stamps the boot immediately after, and the bind is four
///     hundred lines later. `checks.rs` pins the same relation from the units
///     side.
///   - ALL 4 rows the old expression excluded in 24h were ordinary slow
///     requests that ARRIVED 2.1s, 2.9s, 14.6s and 15.2s after a boot. None of
///     them spanned anything. Two were `GET /api/sessions-git` at ~15s, which
///     is the cache stampede fixed under AMUX-3684. This filter was deleting
///     that defect's own evidence from `slow_outliers` while it was live.
///
/// In a full day the predicate produced false exclusions and nothing else, in
/// the under-reporting direction nobody notices.
///
/// WHY NOT THE `ready_at` COLUMN THE CARD PRESCRIBES: it would be a filter that
/// matches nothing. `ready_at` can only mean the instant the listener binds, and
/// no request can be stamped before that, so `ts < ready_at` is false for the
/// same structural reason `ts < boot_at` is. It would buy a schema change and
/// zero rows. The gap it was meant to measure is logged at startup instead
/// (`ready_after_boot_ms`, lib.rs), so if the assumption ever stops holding the
/// number is already on the record rather than being argued about.
///
/// WHAT REPLACED THE EXCLUSION. Nothing hides these rows now; they are
/// ANNOTATED. `/api/logs/stats` carries `since_boot_s` on every outlier and the
/// outlier finding says how far into its process's life the worst request
/// arrived. A reader sees "2.9s after a restart" instead of being silently
/// protected from the row, which answers AF-186's real concern (a lane
/// investigating an endpoint that was never broken) without discarding evidence.
///
/// A NULL `boot_at` is a legacy row from before the column existed (the newest
/// is 2026-08-23 23:12) and falls back to the process-boot comparison rather
/// than being treated as "did not span". Absence is not evidence, and this table
/// retains a week.
pub(crate) fn spans_own_restart(
    ts: f64,
    latency_ms: f64,
    row_boot: Option<f64>,
    proc_boot: Option<f64>,
) -> bool {
    match row_boot {
        // Says exactly what it means, and is structurally false today. Written
        // as the comparison rather than a bare `false` so the semantics stay
        // readable at the call sites, and so it starts working on its own if
        // the architecture ever lets a request predate its process's boot (an
        // inherited listener, socket activation). The `excluded=` counters at
        // every call site publish the zero, so this cannot become a silent
        // filter that matches nothing.
        Some(b) => ts < b,
        None => spans_restart(ts, latency_ms, proc_boot),
    }
}

pub fn detect_latency(conn: &Connection, now: f64) -> (Vec<Finding>, Vec<Suppressed>) {
    detect_latency_at(conn, now, crate::runtime_jobs::heartbeat::boot_at())
}

/// [`detect_latency`] with the boot boundary passed in rather than read from the
/// process (AF-178).
///
/// The boundary lives in a `OnceLock` that is set once per process and cannot be
/// set twice, so with only the public entry point a test can exercise the
/// filter's TRUE branch exactly once per test binary, and doing so leaks into
/// every other test in the file. That is not a seam, it is a coin flip on test
/// order — and the filter's true branch is precisely the code path that
/// deleted 213,397 rows on 2026-08-23.
///
/// The wrapper above is the only production caller and adds nothing, so a test
/// through this function is a test of the shipped path, not of a paraphrase of
/// it (rule 7: the fixture has to flow through the code where the defect would
/// be introduced).
/// How many CPUs this host has, for reading a load average against.
///
/// `None` rather than a guess: "load 37" means nothing without the denominator,
/// and a wrong denominator is worse than an absent one because it invites a
/// confident conclusion. `_SC_NPROCESSORS_ONLN` is the online count, which is the
/// right one for oversubscription (a core the OS has parked cannot run a thread).
fn host_ncpu() -> Option<i64> {
    // SAFETY: sysconf takes an int name and returns a long; no pointers.
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    (n > 0).then_some(n as i64)
}

/// The host-load sentence for a correlated-slowdown card (AMUX-3646).
///
/// STATES THE NUMBER, DOES NOT ASSERT THE CAUSE. The correlation between high
/// load and these excursions is plausible and, as of the day this shipped,
/// UNPROVEN: the card's own design section says to add the column first, let it
/// fill, and verify that slow rows carry high load1 while fast ones do not.
/// Writing "the host was oversubscribed, that is why" before that check would be
/// the loud-wrong instrument this file's rule 7 is mostly about, and it would be
/// believed because it reads like a diagnosis.
///
/// So the card gets the fact and the reader gets the judgment. That is also the
/// honest answer to the two-cell problem this card was filed about: the detector
/// had "one endpoint is slow" and "N endpoints are slow" and the truth needed a
/// third cell it could not express.
///
/// Sorts in place; the caller owns the vector and does not need the order.
fn describe_window_load(loads: &mut [f64]) -> String {
    if loads.is_empty() {
        return "not recorded — every row in this window predates migration 0034 \
                (load1), so the host's state during the excursion is unknown"
            .into();
    }
    loads.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50 = rl::percentile_sorted(loads, 0.5);
    let max = loads.last().copied().unwrap_or(0.0);
    match host_ncpu() {
        Some(n) => format!(
            "1-minute load across this window: median {p50:.1}, peak {max:.1}, on {n} cores \
             ({:.2}x at the median). This box compiles and tests amux continuously, so \
             oversubscription is routine rather than exceptional. It is stated here as a \
             FACT, not a verdict: check whether the slow rows carry the high load before \
             concluding it is the cause.",
            p50 / n as f64
        ),
        None => format!(
            "1-minute load across this window: median {p50:.1}, peak {max:.1}. Core count \
             unavailable, so there is no denominator to read it against."
        ),
    }
}

/// One family's p95 excursion, held until the fan-in can count them (AMUX-3646).
struct P95Hit {
    fam: String,
    p95_w: f64,
    p95_b: f64,
    win_p50: f64,
    win_max: f64,
    base_p50: f64,
    win_n: usize,
    base_n: usize,
}

/// The per-family p95 card, unchanged in content from when it was built inline.
///
/// Extracted only so the fan-in above can choose between one rollup and N of
/// these without the payload existing in two spellings. Two spellings of one
/// rule is how the outlier path kept the wrong restart predicate for a day after
/// the p95 path was fixed, and that lesson is already written into
/// `request_log.rs` a few hundred lines away.
fn p95_finding(h: &P95Hit, mult: f64, min_n: i64, now: f64) -> Finding {
    Finding {
        kind: DetectorKind::Latency,
        signature: format!("latency|p95|{}", h.fam),
        title: format!(
            "{} p95 {:.0}ms — {:.1}x its trailing norm",
            h.fam,
            h.p95_w,
            h.p95_w / h.p95_b
        ),
        evidence: vec![
            ("verdict".into(), format!(
                "{} p95 over the last {:.1}h is {:.0}ms against a {:.0}h trailing p95 of \
                 {:.0}ms ({:.1}x). Threshold: {mult}x with at least {min_n} samples on both \
                 sides.",
                h.fam, window_h(), h.p95_w, baseline_h(), h.p95_b, h.p95_w / h.p95_b
            )),
            ("window_samples".into(), h.win_n.to_string()),
            ("baseline_samples".into(), h.base_n.to_string()),
            ("window_p50_p95_max".into(), format!(
                "{:.0} / {:.0} / {:.0} ms", h.win_p50, h.p95_w, h.win_max
            )),
            ("baseline_p50_p95".into(), format!("{:.0} / {:.0} ms", h.base_p50, h.p95_b)),
            ("percentile_method".into(),
             "nearest-rank over the sorted window (same function /api/logs/stats reports)".into()),
        ],
        recheck: format!(
            "curl -sk \"$AMUX_URL/api/logs/stats?since_h={}\" | python3 -c \"import json,sys; \
             d=json.load(sys.stdin); print([f for f in d['families'] if f.get('family')=='{}'])\"",
            window_h() as i64,
            h.fam
        ),
        owner: None,
        count: h.win_n as u64,
        last_ts: now,
        parked_until: None,
    }
}

pub(crate) fn detect_latency_at(
    conn: &Connection,
    now: f64,
    boot: Option<f64>,
) -> (Vec<Finding>, Vec<Suppressed>) {
    let w_start = now - window_h() * 3600.0;
    let b_start = now - baseline_h() * 3600.0;
    let mut out = Vec::new();
    let mut suppressed = Vec::new();
    let min_n = latency_min_samples();

    let mut per_family: BTreeMap<String, FamilySamples> = BTreeMap::new();
    // The host's load across the WINDOW's rows (AMUX-3646). Filled by the scan
    // below and read by the p95 fan-in, which is the report that most needs it:
    // "N families regressed at once" and "the box was at 1.3x oversubscription"
    // are the same sentence, and until this column existed only the first half
    // could be said.
    let mut window_load: Vec<f64> = Vec::new();
    // Hoisted so the blindness check below can quote them (AF-180): the zero
    // answer is only useful next to how much was looked at.
    let mut considered_rows = 0usize;
    let mut excluded_rows = 0usize;
    // Rows stamped slow_ok are REQUESTED latency, not service latency
    // (AMUX-3513): a browser `wait` action polls for up to its caller-chosen
    // budget and a timed-out wait is a 200 at exactly that budget — twelve of
    // those raised /api/browser's p95 7.1x and filed a phantom regression.
    // The endpoint declares the semantics (x-amux-slow-ok response header ->
    // req_meta.slow_ok); both latency shapes skip what the caller asked for.
    if let Ok(mut stmt) = conn.prepare(
        "SELECT family, latency_ms, ts, boot_at, load1 FROM _amux_request_log WHERE ts >= ?1 \
           AND (req_meta IS NULL OR req_meta NOT LIKE '%\"slow_ok\"%') LIMIT 400000",
    ) {
        if let Ok(rows) = stmt.query_map(rusqlite::params![b_start], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, Option<f64>>(3)?,
                r.get::<_, Option<f64>>(4)?,
            ))
        }) {
            // A REQUEST WHOSE CLOCK SPANS A RESTART IS NOT A SLOW REQUEST
            // (AF-175). `latency_ms` is wall time from arrival to completion,
            // so a request that arrived before this process started and
            // completed after it came up records the OUTAGE, not the endpoint.
            //
            // 2026-08-23 filed "11 requests to GET /api/sessions exceeded 10s,
            // worst 62.0s" for a window in which the server was not running.
            // The dashboard polled every 5s across a 107-second outage; nine
            // requests completed 5s apart with latencies falling by exactly 5s,
            // which is a queue drain, not eleven independent slow requests.
            // /api/board's real p50 is 0.3ms.
            //
            // Same class as AMUX-3513's `slow_ok` exclusion directly above: the
            // number is real and it is not measuring the endpoint.
            //
            // `None` means the process never recorded a boot — every row is
            // kept, because excluding on an unknown boundary would silently
            // drop everything and look exactly like a filter that worked.
            let mut spanned_restart = 0usize;
            let mut considered = 0usize;
            for (fam, ms, ts, row_boot, row_load) in rows.flatten() {
                considered += 1;
                // AMUX-3646: the host's load during the WINDOW, so the rollup can
                // say "28 cores were carrying 37" instead of naming five innocent
                // endpoints. Window rows only; the baseline spans days and its
                // load distribution answers a different question.
                if ts >= w_start {
                    if let Some(l) = row_load {
                        window_load.push(l);
                    }
                }
                // COUNT THE ROW BEFORE DECIDING ITS FATE (AF-178). The entry is
                // taken unconditionally so a family whose rows are ALL filtered
                // still exists in the map with its pre-filter counts. Under the
                // old shape it vanished from `per_family` entirely and produced
                // no suppression at all — the quietest way for a detector to
                // stop working.
                let spanned = spans_own_restart(ts, ms, row_boot, boot);
                if spanned {
                    spanned_restart += 1;
                }
                let e = per_family.entry(fam).or_default();
                if ts >= w_start {
                    e.win_raw += 1;
                    if !spanned {
                        e.win.push(ms);
                    }
                } else {
                    e.base_raw += 1;
                    if !spanned {
                        e.base.push(ms);
                    }
                }
            }
            // UNCONDITIONAL, INCLUDING ZERO (AF-178). This line used to print
            // only when something was excluded, which made "the filter ran and
            // dropped nothing" and "no tick happened" byte-identical silences.
            // That cost a real measurement the same night it shipped: the
            // corrected two-sided predicate could not be confirmed, three
            // attempts running, because its success state emitted nothing and
            // this server is replaced faster than the ~2-minute tick. An
            // instrument that only speaks when the news is bad cannot tell you
            // it is working.
            tracing::info!(
                considered,
                excluded = spanned_restart,
                boot_at = boot.unwrap_or(0.0),
                "latency: scanned request rows; excluded those whose clock spans this \
                 process's start — wall time across a restart is not service time (AF-175)"
            );
            considered_rows = considered;
            excluded_rows = spanned_restart;
            if considered >= 400_000 {
                // The query's LIMIT is binding, so the sample is a silent,
                // unordered truncation of the period rather than the period.
                // A cap's direction is part of its correctness (AF-131) and
                // this one has no ORDER BY, so which rows survive is arbitrary.
                tracing::warn!(
                    considered,
                    "latency: row cap reached — the p95 window is a TRUNCATED, unordered \
                     sample of the period and the percentiles below are not the period's"
                );
            }
        }
    }
    // AF-178: families whose rows were ALL filtered away. Collected here rather
    // than judged inline because one card naming every affected family is a
    // report, and one card per family is a backlog (rule 5).
    let mut collapsed: Vec<(String, &'static str, usize)> = Vec::new();
    // Every family that regressed this window, collected so the fan-in after the
    // loop can see HOW MANY there were before deciding whether this is one event
    // or N tasks (AMUX-3646). Filing inside the loop is precisely what made this
    // detector unable to ask that question.
    let mut p95_hits: Vec<P95Hit> = Vec::new();
    let mult = latency_mult();
    let mut families_seen = 0usize;
    for (fam, fs) in per_family {
        families_seen += 1;
        // A SIDE WITH PLENTY OF ROWS AND NO SURVIVORS IS A DETECTOR OUTAGE, not
        // a quiet endpoint. Nothing legitimately discards 100% of a busy
        // family's rows: `slow_ok` is excluded in SQL and never reaches this
        // count, so everything measured here was dropped by a predicate in the
        // loop above. Named honestly: this catches in-loop filters, and would
        // NOT catch an over-broad clause added to the query itself, because
        // that lowers `*_raw` in step. If you add a WHERE clause, add its
        // counterpart here.
        if fs.win_raw >= min_n as usize && fs.win.is_empty() {
            collapsed.push((fam.clone(), "window", fs.win_raw));
        }
        if fs.base_raw >= min_n as usize && fs.base.is_empty() {
            collapsed.push((fam.clone(), "baseline", fs.base_raw));
        }
        let (mut win, mut base) = (fs.win, fs.base);
        if (win.len() as i64) < min_n {
            continue; // Not enough traffic to say anything. Not a suppression:
                      // there is no candidate here, only silence.
        }
        if (base.len() as i64) < min_n {
            // SAY WHAT WAS THERE BEFORE THE FILTER (AF-178). "baseline has 0
            // samples" is true of a brand-new install and true of a detector
            // whose input was just deleted by a bad predicate, and only the
            // second is a bug report. The pre-filter count is the whole
            // difference and it is already in hand.
            let reason = if fs.base_raw == 0 {
                format!(
                    "no rows at all in the {:.0}h baseline period — no trailing norm to \
                     compare against yet",
                    baseline_h()
                )
            } else {
                format!(
                    "baseline has {} of {} rows in the period after filtering (<{min_n}) — \
                     {} were excluded before the percentile",
                    base.len(),
                    fs.base_raw,
                    fs.base_raw - base.len()
                )
            };
            suppressed.push(sup(
                DetectorKind::Latency,
                &format!("latency|p95|{fam}"),
                &reason,
            ));
            continue;
        }
        win.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        base.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p95_w = rl::percentile_sorted(&win, 0.95);
        let p95_b = rl::percentile_sorted(&base, 0.95);
        if p95_b <= 0.0 || p95_w < p95_b * mult {
            continue;
        }
        // AMUX-3471: a ratio alone files fast-vs-faster noise (2ms at 3.7x).
        if p95_w < latency_floor_ms() {
            continue;
        }
        // ACCUMULATE, DO NOT FILE YET (AMUX-3646). See the fan-in below.
        p95_hits.push(P95Hit {
            fam: fam.clone(),
            p95_w,
            p95_b,
            win_p50: rl::percentile_sorted(&win, 0.5),
            win_max: win.last().copied().unwrap_or(0.0),
            base_p50: rl::percentile_sorted(&base, 0.5),
            win_n: win.len(),
            base_n: base.len(),
        });
    }

    // FAN-IN BEFORE FAN-OUT, for p95 too (AMUX-3646).
    //
    // The OUTLIER detector has rolled up simultaneous offenders since AMUX-3588,
    // and its verdict text argues the case: per-endpoint cards "each name an
    // endpoint that is probably not the fault, and would bury the board". This
    // detector had no such rollup, so it emitted exactly the cards the other
    // detector's own text argues against. Two detectors on one subsystem
    // disagreeing about whether a correlated slowdown is one fault or many.
    //
    // The specimen is a pair from 2026-08-24: AMUX-3657 (`/api/sessions-git` p95
    // 4624ms, 3.2x) and AMUX-3658 (`/api/board` p95 994ms, 3.1x), both fired in
    // the same window, both true, both unactionable, both discarded. This fan-in
    // would have collapsed them into one card, and that is worth doing whatever
    // the cause turns out to be: a p95 excursion on N families at once is one
    // event, not N tasks.
    //
    // SAME KNOB as the outlier rollup on purpose. `AMUX_OUTLIER_ROLLUP_AT` reads
    // as outlier-specific and is really "how many simultaneous slow endpoints
    // before this is one fault", which is one question. A second knob would be a
    // second thing to keep in step, and the two detectors disagreeing is the
    // defect being fixed here.
    if p95_hits.len() >= outlier_rollup_at() {
        let n_f = p95_hits.len();
        let fams: Vec<String> = p95_hits.iter().map(|h| h.fam.clone()).collect();
        let worst = p95_hits.iter().map(|h| h.p95_w / h.p95_b).fold(0.0f64, f64::max);
        out.push(Finding {
            kind: DetectorKind::Latency,
            // Keyed on the FAMILY SET, matching the outlier rollup: a different
            // collapse is different news, the same one stays idempotent, and no
            // occurrence timestamp, because a correlated slowdown is a standing
            // condition rather than one request.
            signature: format!("latency|p95|ROLLUP|{}", fams.join(",")),
            title: format!(
                "{n_f} families regressed at once — one event, not {n_f} tasks (worst {worst:.1}x)"
            ),
            evidence: vec![
                ("verdict".into(), format!(
                    "{n_f} DIFFERENT families exceeded {mult}x their own trailing p95 in the SAME \
                     window. Each statement is true and each is unactionable alone: nothing in a \
                     per-family payload separates \"this endpoint got slower\" from \"everything \
                     got slower\". Look for something host-wide or server-wide first."
                )),
                ("families".into(), p95_hits.iter()
                    .map(|h| format!("\n  {} {:.0}ms vs {:.0}ms ({:.1}x)",
                                     h.fam, h.p95_w, h.p95_b, h.p95_w / h.p95_b))
                    .collect::<String>()),
                ("percentile_method".into(),
                 "nearest-rank over the sorted window (same function /api/logs/stats reports)".into()),
                ("host_load".into(), describe_window_load(&mut window_load)),
                ("rollup_threshold".into(), format!(
                    "{} families (AMUX_OUTLIER_ROLLUP_AT, shared with the outlier rollup)",
                    outlier_rollup_at()
                )),
                ("if_this_is_wrong".into(),
                 "If these really are independent regressions, raise AMUX_OUTLIER_ROLLUP_AT \
                  rather than splitting the card by hand. But check the host first: on this \
                  box the fleet compiles and tests this server continuously, and a 28-core \
                  machine carrying a load average of 37 makes every endpoint slow at once \
                  (AMUX-3646).".into()),
            ],
            recheck: format!(
                "curl -sk \"$AMUX_URL/api/logs/stats?since_h={}\" | python3 -c \"import json,sys; \
                 d=json.load(sys.stdin); print([(f['family'], f.get('p95_ms')) for f in \
                 d['families']])\"  # look for ONE window, not one family",
                window_h() as i64
            ),
            owner: None,
            count: n_f as u64,
            last_ts: now,
            parked_until: None,
        });
    } else {
        for h in &p95_hits {
            out.push(p95_finding(h, mult, min_n, now));
        }
    }

    // AF-178: THE DETECTOR REPORTS ITS OWN BLINDNESS.
    //
    // Filed as an ordinary Finding so it rides the pipeline that already turns
    // a detector's output into a board card — no new mechanism, and the
    // signature carries the affected families so a DIFFERENT collapse is
    // different news while the same one stays idempotent.
    //
    // The incident this exists for: d0c034ad shipped a one-sided restart filter
    // that excluded 213,397 of 213,935 rows. Every family's baseline went to
    // zero, the latency regression shape could not fire at all, and the only
    // trace was two suppressions reading "baseline has 0 samples (<30)" for the
    // two busiest families in the system. It was found by a human reading that
    // specific commit. Nothing in amux could have surfaced it, which is the
    // half that makes a fix a class kill rather than one bug.
    if collapsed.is_empty() {
        // AF-180 (amux, reviewing AF-178): A HEALTHY ALARM AND A BROKEN ONE ARE
        // BYTE-IDENTICAL SILENCES. The collapse check above is an ordinary
        // Finding, which is right for reaching a human but means it speaks only
        // when the fault occurs — so "the blindness alarm ran and found nothing"
        // and "the blindness alarm was never wired in" look the same from
        // outside. That is this detector's own thesis one level up, and AF-178
        // called this half the part that makes it a class kill.
        //
        // The zero goes through `suppressed`, which already means "a candidate
        // the detector considered and declined" and is already rendered by
        // GET /api/debug/autofix. No new mechanism, no new process state, and
        // the healthy answer appears exactly where the unhealthy one would.
        suppressed.push(sup(
            DetectorKind::Latency,
            "latency|input-collapsed",
            &format!(
                "blindness check ran: 0 of {families_seen} families lost every row to \
                 filtering ({considered_rows} rows considered, {excluded_rows} excluded). \
                 A zero here is a measurement; silence would not be."
            ),
        ));
    }
    if !collapsed.is_empty() {
        let mut fams: Vec<&str> = collapsed.iter().map(|(f, _, _)| f.as_str()).collect();
        fams.sort_unstable();
        fams.dedup();
        let discarded: usize = collapsed.iter().map(|(_, _, n)| *n).sum();
        let detail = collapsed
            .iter()
            .map(|(f, side, n)| format!("{f} {side}: 0 of {n} rows survived"))
            .collect::<Vec<_>>()
            .join("; ");
        out.push(Finding {
            kind: DetectorKind::Latency,
            signature: format!("latency|input-collapsed|{}", fams.join(",")),
            title: format!(
                "latency detector is blind on {} — every row filtered out ({discarded} discarded)",
                fams.join(", ")
            ),
            evidence: vec![
                ("verdict".into(), format!(
                    "{} famil{} had at least {min_n} request rows in the period and ZERO of them \
                     survived filtering, so no percentile can be computed and no regression can \
                     be filed for them. This is a detector outage, not a quiet endpoint: the \
                     rows exist and something in detect_latency discarded all of them. Look at \
                     the most recently changed predicate in that function.",
                    fams.len(),
                    if fams.len() == 1 { "y" } else { "ies" },
                )),
                ("families".into(), fams.join(", ")),
                ("rows_discarded".into(), discarded.to_string()),
                ("detail".into(), detail),
                ("min_samples".into(), min_n.to_string()),
            ],
            recheck: "curl -sk \"$AMUX_URL/api/debug/autofix\" | python3 -c \"import json,sys; \
                      d=json.load(sys.stdin)['last']; print([s for s in d['suppressed'] \
                      if s['detector']=='latency'])\"".into(),
            owner: None,
            count: collapsed.len() as u64,
            last_ts: now,
            parked_until: None,
        });
    }

    // Single-request outliers: one card per (method, target), not per request.
    //
    // A NAMED STRUCT rather than the 5-tuple this started as. Clippy asked for
    // a type alias and was complaining about the right thing for the right
    // reason: the fifth field is `worst_since_boot`, and `e.4` is not something
    // a reader can check against the evidence line it feeds.
    struct OutlierGroup {
        n: u64,
        worst_ms: f64,
        last_ts: f64,
        sample: String,
        /// AMUX-3647: seconds between the WORST row's arrival and the boot of
        /// the process that served it. Rows landing near a boot used to be
        /// excluded outright, so a lane never saw them at all; they are filed
        /// now WITH the context that separates startup contention from an
        /// endpoint defect, which is what AF-186 wanted and the exclusion only
        /// approximated. `None` for a row predating migration 0030.
        worst_since_boot: Option<f64>,
        /// AMUX-3646: the host's 1-minute load when the WORST row was logged.
        /// A single-endpoint outlier card is unweighable without it: 30.3s on a
        /// quiet box is an endpoint defect and 30.3s on 28 cores carrying 37 is
        /// a queue. `None` for a row predating migration 0034.
        worst_load1: Option<f64>,
    }
    let mut seen: BTreeMap<(String, String), OutlierGroup> = BTreeMap::new();
    // AF-175: THIS shape is where the reported incident came from, and it had no
    // restart exclusion at all. The first fix put `spans_restart` in the p95 loop
    // above — a real improvement to a different question. The card's verdict
    // ("11 request(s) to GET /api/sessions exceeded 10s; worst 62.0s") is an
    // OUTLIER card, produced here, and it would have filed again unchanged.
    //
    // Fixing the wrong path is easy to do and hard to notice, because the p95
    // change was genuinely correct and its tests genuinely passed. Both shapes
    // now go through the SAME helper, so they cannot answer "is this number
    // service time" differently.
    let mut outliers_spanned = 0usize;
    let mut outliers_considered = 0usize;
    let mut outliers_failed = 0usize;
    if let Ok(mut stmt) = conn.prepare(
        "SELECT method, path, latency_ms, ts, status, boot_at, load1 FROM _amux_request_log \
         WHERE ts >= ?1 AND latency_ms >= ?2 \
           AND (req_meta IS NULL OR req_meta NOT LIKE '%\"slow_ok\"%') \
         ORDER BY latency_ms DESC LIMIT 2000",
    ) {
        if let Ok(rows) = stmt.query_map(rusqlite::params![w_start, outlier_ms()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<f64>>(5)?,
                r.get::<_, Option<f64>>(6)?,
            ))
        }) {
            for (method, path, ms, ts, status, row_boot, row_load) in rows.flatten() {
                outliers_considered += 1;
                if spans_own_restart(ts, ms, row_boot, boot) {
                    outliers_spanned += 1;
                    continue;
                }
                // A REQUEST THAT FAILED IS NOT A LATENCY MEASUREMENT (AMUX-3709).
                //
                // Its duration is whatever timeout terminated it, so filing it as
                // "took 30.0s" sends the reader hunting for slowness in an
                // endpoint that is fast. The specimen: GET /api/browser/screenshot
                // 502 at 30,006ms, which is the CDP `Page.captureScreenshot timed
                // out after 30s` path in api/browser.rs — the tab wedged. Every
                // other row on that endpoint in the same 24h was 200 at 197-3242ms.
                // It filed AMUX-3709 ("took 30.0s") while the SAME failure was
                // already AMUX-3708 ("→ HTTP 502 (2x)"): two cards, two queues,
                // one fault, and the accurate one is not the one a lane picked up.
                //
                // Scoped to >= 500 ON PURPOSE, and that scope is what makes the
                // exclusion safe rather than a blind spot: it is exactly the set
                // the 5xx filer above already selects (`WHERE status >= 500`), so
                // an excluded row still produces a card, just the one that names
                // the fault. 4xx rows stay in, because nothing else would report a
                // slow one.
                //
                // AMUX-3588 saw this double-filing first and fixed the fan-OUT
                // half (4+ endpoints failing together roll into one card). It
                // explicitly rejected "durations cluster at the timeout value" as
                // the discriminator, and rightly — that guesses at one failure
                // mode from a number. This is not that heuristic: it reads the
                // row's own status, and it closes the single-endpoint case the
                // breadth rule cannot see.
                if status >= 500 {
                    outliers_failed += 1;
                    continue;
                }
                let target = rl::normalize_target(&path);
                let e = seen.entry((method.clone(), target)).or_insert_with(|| OutlierGroup {
                    n: 0,
                    worst_ms: 0.0,
                    last_ts: ts,
                    sample: format!("{method} {path} → {status}"),
                    worst_since_boot: None,
                    worst_load1: None,
                });
                e.n += 1;
                // The age travels WITH the worst latency, so the evidence line
                // describes the request the title quotes rather than whichever
                // row happened to be last.
                if ms > e.worst_ms {
                    e.worst_ms = ms;
                    e.worst_since_boot = row_boot.map(|b| ts - b);
                    e.worst_load1 = row_load;
                }
                e.last_ts = e.last_ts.max(ts);
            }
        }
    }
    // UNCONDITIONAL, INCLUDING ZERO — same rule as the p95 line above (AF-178).
    // A filter that only speaks when it drops something cannot tell you it ran,
    // and this exclusion's whole history is of being hard to confirm.
    tracing::info!(
        target: "autofix",
        considered = outliers_considered,
        excluded = outliers_spanned,
        "latency outliers: restart-spanning rows excluded (AF-175)"
    );
    // Same rule, same reason (AF-178): unconditional, including zero. A silent
    // exclusion and an exclusion that never ran read identically, and this one
    // is the difference between a lane investigating a fast endpoint and a lane
    // reading the 5xx card that names the actual fault.
    tracing::info!(
        target: "autofix",
        considered = outliers_considered,
        excluded = outliers_failed,
        "latency outliers: FAILED (5xx) rows excluded — already filed by the 5xx path (AMUX-3709)"
    );

    // FAN-IN BEFORE FAN-OUT, for outliers (AMUX-3588). The same rule the
    // invariant detector already follows, arriving here because a server-wide
    // fault is not N endpoint faults.
    //
    // Measured: the hang at 00:49-00:51 on 2026-08-24 filed FOUR cards —
    // /api/sessions as a 5xx and again as a latency outlier, plus
    // /api/board/statuses and /api/board/session-gates. Every offending row was
    // stamped 00:49:41 or 00:50:29 at exactly 30.1s, which is a TIMEOUT, not an
    // endpoint doing work. Each card was individually well-formed and
    // individually useless: none named the fault, and a lane picking one up
    // starts by investigating an endpoint that was never broken. One of them
    // then re-filed and cost a second lane-turn.
    //
    // BREADTH IS THE DISCRIMINATOR, and deliberately not "durations cluster at
    // the timeout value", which is what this card was filed proposing. A
    // duration heuristic guesses at one failure mode; breadth is the same
    // predicate the invariant rollup already uses and it is right for the
    // general case too — if a deploy genuinely made every endpoint slow, that
    // is also ONE fault and also belongs in one card.
    let candidates: Vec<_> = seen
        .iter()
        .filter(|((_, target), g)| g.worst_ms > design_budget_ms(target))
        .collect();
    let distinct_targets: std::collections::BTreeSet<&String> =
        candidates.iter().map(|((_, t), _)| t).collect();
    if distinct_targets.len() >= outlier_rollup_at() {
        let n_t = distinct_targets.len();
        let worst_ms = candidates.iter().map(|(_, g)| g.worst_ms).fold(0.0f64, f64::max);
        let last = candidates.iter().map(|(_, g)| g.last_ts).fold(0.0f64, f64::max);
        let targets: Vec<String> = distinct_targets.iter().map(|t| (*t).clone()).collect();
        out.push(Finding {
            kind: DetectorKind::Latency,
            // Keyed on the TARGET SET, so a different collapse is different
            // news while the same one stays idempotent — the rule the invariant
            // rollup uses. Not keyed on an occurrence timestamp: a systemic
            // fault is a standing condition, unlike a single slow request.
            signature: format!("latency|outlier|ROLLUP|{}", targets.join(",")),
            title: format!(
                "{n_t} endpoints slow at once — one fault, not {n_t} tasks (worst {:.1}s)",
                worst_ms / 1000.0
            ),
            evidence: vec![
                ("verdict".into(), format!(
                    "{n_t} DIFFERENT endpoints exceeded the outlier threshold in the same                      window. Filed as ONE card on purpose: per-endpoint cards would each name                      an endpoint that is probably not the fault, and would bury the board.                      Look for something SERVER-WIDE first — a hang, a saturated DB pool, a                      stalled dependency. Check ~/.amux/logs/watchdog.log and                      GET /api/debug/invariants before investigating any single endpoint."
                )),
                ("endpoints".into(), format!("
  {}", targets.join("
  "))),
                ("worst_ms".into(), format!("{worst_ms:.0}")),
                ("last_seen".into(), rl::local_when(last)),
                // AMUX-3647: the cheapest way to name a SERVER-WIDE fault the
                // verdict above tells the reader to go looking for. N endpoints
                // slow at once, all of them a few seconds into the same
                // process's life, is a restart, and saying so here saves the
                // lane the pivot to sqlite. These rows were previously excluded
                // wholesale, which "named" it by deleting it.
                ("arrived_into_process_life".into(), {
                    let ages: Vec<f64> =
                        candidates.iter().filter_map(|(_, g)| g.worst_since_boot).collect();
                    match (ages.iter().cloned().fold(f64::INFINITY, f64::min),
                           ages.iter().cloned().fold(f64::NEG_INFINITY, f64::max)) {
                        _ if ages.len() < candidates.len() => format!(
                            "{} of {} worst-rows carry a boot_at; the rest predate the column",
                            ages.len(), candidates.len()
                        ),
                        (lo, hi) => format!(
                            "worst request per endpoint arrived {lo:.1}s to {hi:.1}s after its \
                             server process booted. A tight range in the first seconds means a \
                             RESTART, not {n_t} slow endpoints."
                        ),
                    }
                }),
                // AMUX-3646. THE SPECIMEN IS THIS EXACT CARD SHAPE: AMUX-3644
                // named /api/board, /api/board/{id}, /api/browser/start,
                // /api/sessions and /api/sessions/{name}/{*verb}, and not one of
                // them was the fault. The host was a 28-core box carrying a load
                // average of 36.86, and nothing in the payload could say so, so
                // the reader got five innocent endpoint names and no trace of
                // the one fact explaining all five.
                ("host_load".into(), describe_window_load(&mut window_load)),
                ("rollup_threshold".into(), format!(
                    "{} distinct targets (AMUX_OUTLIER_ROLLUP_AT)", outlier_rollup_at()
                )),
                ("if_this_is_wrong".into(),
                 "If these really are independent slow endpoints, the rollup threshold is too \
                  low for this fleet — raise AMUX_OUTLIER_ROLLUP_AT rather than splitting the \
                  card by hand.".into()),
            ],
            recheck: format!(
                "sqlite3 ~/.amux/amux.db \"SELECT datetime(ts,'unixepoch','localtime'), \
                 latency_ms, status, path FROM _amux_request_log WHERE latency_ms >= {:.0} \
                 ORDER BY ts DESC LIMIT 30;\"  # look for ONE window, not one path",
                outlier_ms()
            ),
            owner: None,
            count: candidates.len() as u64,
            last_ts: last,
            parked_until: None,
        });
        return (out, suppressed);
    }

    for (
        (method, target),
        OutlierGroup { n, worst_ms: worst, last_ts, sample, worst_since_boot, worst_load1 },
    ) in seen
    {
        // AMUX-3485: for long-by-design endpoints the effective threshold is
        // their design budget, not the global outlier floor.
        if worst <= design_budget_ms(&target) {
            continue;
        }
        // OCCURRENCE IDENTITY in the signature (AMUX-3472). An outlier report
        // is about SPECIFIC requests, not a standing condition — but the
        // signature carried only method+target, so discarding the card
        // (which re-arms the idem, AF-137) refiled the SAME rows on the next
        // scan as long as they sat inside the 6h window: the identical
        // specimen came back with zero new information, twice in one day.
        // The newest offending row's second-resolution timestamp makes each
        // occurrence batch its own signature: a NEW slow request files a NEW
        // card whatever happened to the old one, and the same rows re-scanned
        // mint the same signature. The board's re-arm hook deliberately skips
        // `latency|outlier|` refs for the same reason — an occurrence, once
        // judged, is judged forever.
        out.push(Finding {
            kind: DetectorKind::Latency,
            signature: format!("latency|outlier|{method}|{target}|{}", last_ts as i64),
            title: format!("{method} {target} took {:.1}s ({n}x over {:.0}s)", worst / 1000.0, outlier_ms() / 1000.0),
            evidence: vec![
                ("verdict".into(), format!(
                    "{n} request(s) to {method} {target} exceeded {:.0}s in the last {:.1}h; \
                     worst {:.1}s. This is not a percentile shift — it is individual requests \
                     going wrong, so look at the request, not the family.",
                    outlier_ms() / 1000.0, window_h(), worst / 1000.0
                )),
                ("worst_ms".into(), format!("{worst:.0}")),
                ("sample_request".into(), sample),
                ("threshold_ms".into(), format!("{:.0}", outlier_ms())),
                ("last_seen".into(), rl::local_when(last_ts)),
                // AMUX-3647. Until 2026-08-24 a row that arrived within
                // `latency` seconds of a boot was DROPPED here, so this card
                // did not exist for that request and nobody could weigh it.
                // Two of the four rows excluded in the preceding 24h were the
                // AMUX-3684 sessions-git stampede, whose whole signature is
                // four clients missing a cold cache right after a restart.
                // Reported with its age instead of hidden: a reader can rule a
                // restart in or out, which is the judgment the filter was
                // making silently and getting wrong.
                ("arrived_into_process_life".into(), match worst_since_boot {
                    Some(s) if s < 60.0 => format!(
                        "{s:.1}s after its server process booted. Check a RESTART before this \
                         endpoint: a cold cache, migrations, and ~15 background loops all start \
                         competing at t=0 (see /health boot_gap and 0032's header)."
                    ),
                    Some(s) if s < 3600.0 => format!(
                        "{:.1} minutes into its server process's life, so a restart is not the story",
                        s / 60.0
                    ),
                    Some(s) => format!(
                        "{:.1}h into its server process's life, so a restart is not the story",
                        s / 3600.0
                    ),
                    None => "unknown: the row predates the boot_at column (migration 0030)".into(),
                }),
                // AMUX-3646: 30.3s on a quiet box is an endpoint defect; 30.3s
                // on 28 cores carrying 37 is a queue. This card could not tell
                // the reader which, and the verdict above sends them to "look at
                // the request" either way.
                ("host_load_at_worst".into(), match (worst_load1, host_ncpu()) {
                    (Some(l), Some(cores)) => format!(
                        "1-minute load {l:.1} on {cores} cores ({:.2}x). Stated as a fact, not \
                         a cause: this box compiles and tests amux continuously, so check \
                         whether the endpoint is slow when the host is NOT loaded before \
                         treating the endpoint as the fault.",
                        l / cores as f64
                    ),
                    (Some(l), None) => format!("1-minute load {l:.1}; core count unavailable"),
                    (None, _) => "not recorded — this row predates migration 0034 (load1)".into(),
                }),
            ],
            recheck: format!(
                "sqlite3 ~/.amux/amux.db \"SELECT datetime(ts,'unixepoch','localtime'), \
                 latency_ms, status, path FROM _amux_request_log WHERE method='{method}' \
                 AND latency_ms >= {:.0} ORDER BY ts DESC LIMIT 20;\"",
                outlier_ms()
            ),
            owner: None,
            count: n,
            last_ts,
            parked_until: None,
        });
    }
    (out, suppressed)
}

// ---------------------------------------------------------------------------
// Detector 3 — dead routes.
// ---------------------------------------------------------------------------

/// A 404/405 that a BROWSER asked for. The user-agent is the discriminator and
/// it is the right one: a curl 404 is usually a human or an agent probing, and
/// filing those would bury the board under every mistyped path anyone ever
/// tried (`/api/credits`, `/api/burn`, `/api/canary/vendor-credit` were all in
/// tonight's window, all from curl, none of them defects). A path the SPA
/// FETCHED is a feature that is broken for every user of the dashboard, and
/// every one of tonight's was invisible until a person happened to notice.
///
/// The verdict is `/api/logs/analyze`'s own annotation, verbatim: which methods
/// ARE mounted (`routed_methods_at`), the nearest siblings (`nearest_routes`),
/// and for a 405 the computed sentence (`verdict_405`) that distinguishes
/// "wrong method on a real path" from "no such path wearing the GET-only SPA
/// catch-all's 405". That last cell is the whole reason the annotation exists,
/// and re-deriving it here would be the second grouping this file refuses to
/// write.
pub fn detect_dead_routes(conn: &Connection, now: f64) -> (Vec<Finding>, Vec<Suppressed>) {
    let cutoff = now - window_h() * 3600.0;
    let mut out = Vec::new();
    let mut suppressed = Vec::new();
    /// (count, first_ts, last_ts, sample_path, ever_called_by_a_browser)
    type DeadHits = (u64, f64, f64, String, bool);
    let mut groups: BTreeMap<(i64, String, String), DeadHits> = BTreeMap::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT ts, method, path, status, user_agent FROM _amux_request_log \
         WHERE status IN (404, 405) AND ts >= ?1 ORDER BY ts ASC LIMIT 200000",
    ) else {
        return (out, suppressed);
    };
    let Ok(rows) = stmt.query_map(rusqlite::params![cutoff], |r| {
        Ok((
            r.get::<_, f64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, Option<String>>(4)?.unwrap_or_default(),
        ))
    }) else {
        return (out, suppressed);
    };
    for (ts, method, path, status, ua) in rows.flatten() {
        let target = rl::normalize_target(&path);
        let browser = ua.contains("Mozilla/");
        let e = groups
            .entry((status, method.clone(), target))
            .or_insert((0, ts, ts, path.clone(), false));
        e.0 += 1;
        e.2 = ts;
        e.4 |= browser;
        e.3 = path;
    }
    for ((status, method, target), (count, first, last, sample_path, browser)) in groups {
        let signature = format!("dead-route|{status}|{method}|{target}");
        // GATEWAY-OWNED PATHS 404 HERE BY DESIGN (AMUX-2686).
        //
        // `/api/stripe/*`, `/api/gateway/*` and `/api/cloud-logout` are served
        // by cloud/gateway/gateway.py, never by amux-server — in cloud either,
        // so "this server does not own them" is true everywhere and needs no
        // `if IS_CLOUD` (the single-codebase rule). The SPA knows: its own
        // comment says "/api/stripe/status only exists on the cloud gateway, so
        // a failed fetch (self-hosted) simply leaves the card hidden", and it
        // returns early on !r.ok.
        //
        // So both sides were already correct and only this detector disagreed,
        // filing a card nobody could ever close. invariants/checks.rs has
        // excluded exactly these paths since 2026-08-11, for a reason its own
        // comment states and which applies verbatim here: "a failure list that
        // can never reach zero stops being read... permanent rows would have
        // trained everyone to skim past the real ones."
        //
        // CALLING THE SAME FUNCTION, not a second copy of the list. Two
        // spellings of one exclusion drift the moment either changes, and then
        // the invariant census and the autofix board disagree about which paths
        // are somebody else's (ethos rule 1).
        if crate::invariants::checks::gateway_owned(&sample_path) {
            suppressed.push(sup(
                DetectorKind::DeadRoute,
                &signature,
                "gateway-owned path: served by cloud/gateway/gateway.py and never by \
                 amux-server, so a 404 here is correct on every deployment. The SPA handles \
                 it (hides the card). Excluded by the same predicate invariants/checks.rs \
                 uses — a card that can never be closed trains readers to skim past the \
                 ones that can",
            ));
            continue;
        }
        if !browser {
            suppressed.push(sup(
                DetectorKind::DeadRoute,
                &signature,
                "no browser ever requested this path — a curl/agent 404 is a probe, not a \
                 broken feature",
            ));
            continue;
        }
        if (count as i64) < dead_route_min_hits() {
            suppressed.push(sup(
                DetectorKind::DeadRoute,
                &signature,
                &format!("{count} hit(s) < {} — one fetch is not yet a pattern", dead_route_min_hits()),
            ));
            continue;
        }
        let routed = rl::routed_methods_at(&sample_path);
        let near = rl::nearest_routes(&sample_path, 3);
        let verdict = if status == 405 {
            rl::verdict_405(&method, &target, &routed, &sample_path)
        } else if routed.is_empty() {
            format!(
                "{method} {target}: no route is mounted at this path in the current build, and \
                 the SPA fetches it. Nearest routes: {}",
                if near.is_empty() { "none".into() } else { near.join(", ") }
            )
        } else {
            format!(
                "{method} {target}: the path IS routed (methods: {}) but answered 404 — the \
                 handler ran and found nothing, so this is a data/id problem, not a mount \
                 problem",
                routed.join(", ")
            )
        };
        out.push(Finding {
            kind: DetectorKind::DeadRoute,
            signature,
            title: format!("SPA calls {method} {target} → {status} ({count}x)"),
            evidence: vec![
                ("verdict".into(), verdict),
                ("routed_methods".into(), if routed.is_empty() { "none".into() } else { routed.join(", ") }),
                ("nearest_routes".into(), if near.is_empty() { "none".into() } else { near.join(", ") }),
                ("sample_request".into(), format!("{method} {sample_path}")),
                ("count".into(), count.to_string()),
                ("first_seen".into(), rl::local_when(first)),
                ("last_seen".into(), rl::local_when(last)),
                ("called_by".into(), "a browser (Mozilla/* user-agent) — i.e. the dashboard".into()),
            ],
            recheck: format!(
                "curl -sk \"$AMUX_URL/api/debug/routes\" | grep -F '{target}' ; \
                 curl -sk \"$AMUX_URL/api/logs/analyze?since_h={}\"",
                window_h() as i64
            ),
            owner: None,
            count,
            last_ts: last,
            parked_until: None,
        });
    }
    (out, suppressed)
}

// ---------------------------------------------------------------------------
// Detector 4 — silent subsystems.
// ---------------------------------------------------------------------------

/// The expensive class: an instrument that should be moving and is not.
/// Nothing here fires on an event, because there is no event — that is the
/// point. Each check states the deadline it compared against, so the card can
/// be argued with instead of believed.
///
/// Both checks read tables the SERVER owns, deliberately: a silent-subsystem
/// detector that needed a worker to answer would go silent exactly when the
/// fleet did.
pub fn detect_silent(
    conn: &Connection,
    now: f64,
    blocked: &BTreeMap<String, Option<String>>,
) -> (Vec<Finding>, Vec<Suppressed>) {
    let mut out = Vec::new();
    let mut suppressed = Vec::new();

    // (a) Schedules whose next_run has passed with no run recorded since.
    let overdue_s = schedule_overdue_min() * 60.0;
    if let Ok(mut stmt) = conn.prepare(
        "SELECT id, title, session, next_run, \
                (SELECT MAX(ran_at) FROM schedule_runs r WHERE r.schedule_id = s.id) \
         FROM schedules s \
         WHERE s.deleted IS NULL AND s.enabled = 1 AND s.next_run IS NOT NULL AND s.next_run != ''",
    ) {
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ))
        });
        let mut late: Vec<(String, String, String, f64)> = Vec::new();
        if let Ok(rows) = rows {
            for (id, title, session, next_run, last_ran) in rows.flatten() {
                let Some(due) = parse_ts(&next_run) else { continue };
                let overdue_by = now - due;
                if overdue_by < overdue_s {
                    continue;
                }
                // A run recorded AFTER the due time means it fired and
                // next_run simply has not been advanced yet — not a stall.
                if last_ran.is_some_and(|t| t as f64 >= due) {
                    continue;
                }
                late.push((id, title, session, overdue_by));
            }
        }
        if !late.is_empty() {
            late.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
            let worst = late[0].3 / 60.0;
            let listing = late
                .iter()
                .take(10)
                .map(|(id, t, s, o)| format!("  {id} [{s}] {t} — {:.0} min late", o / 60.0))
                .collect::<Vec<_>>()
                .join("\n");
            out.push(Finding {
                kind: DetectorKind::SilentSubsystem,
                // Signature does NOT include the count: the subsystem is the
                // fault, not each schedule. One card, not N.
                signature: "silent|schedules|overdue".into(),
                title: format!("{} schedule(s) overdue, worst by {:.0} min", late.len(), worst),
                evidence: vec![
                    ("verdict".into(), format!(
                        "{} enabled schedule(s) are past next_run by more than {:.0} min with no \
                         schedule_runs row at or after their due time. The firing loop is not \
                         firing them — this is absence, so nothing else will report it.",
                        late.len(), schedule_overdue_min()
                    )),
                    ("overdue".into(), listing),
                    ("threshold_min".into(), format!("{:.0}", schedule_overdue_min())),
                ],
                recheck: "sqlite3 ~/.amux/amux.db \"SELECT id, next_run, (SELECT MAX(ran_at) FROM \
                          schedule_runs r WHERE r.schedule_id=s.id) FROM schedules s WHERE \
                          deleted IS NULL AND enabled=1 ORDER BY next_run;\"".into(),
                owner: None,
                count: late.len() as u64,
                last_ts: now,
                parked_until: None,
            });
        }
    }

    // (b) A steering queue that is not draining.
    let stale_s = steering_stale_min() * 60.0;
    // A DETECTOR THAT CANNOT RUN MUST SAY SO (AMUX-3696).
    //
    // This was a bare `if let Ok(..)`, so a failed prepare skipped the entire
    // steering block and left NOTHING behind — indistinguishable from "no lane
    // has a stalled queue", which is the shape ethos rule 4 is about.
    //
    // It is not hypothetical. `steering_queue.sender` is added by
    // `ensure_fleet_tables`'s runtime ALTER and by NO migration, so a database
    // built from `migrations/` alone does not have it and this query does not
    // prepare. That is exactly the state every test fixture is in, which is how
    // this block reached today with no coverage: every test that "exercised" it
    // was silently exercising nothing.
    //
    // Recorded as a Suppressed rather than logged, because the autofix report
    // already surfaces suppressions and that is where somebody reading "why did
    // the detector find nothing" will actually look.
    let steer_stmt = conn.prepare(
        "SELECT session, COUNT(*), MIN(queued_at), COALESCE(GROUP_CONCAT(DISTINCT sender),'') \
         FROM steering_queue GROUP BY session",
    );
    if let Err(e) = &steer_stmt {
        suppressed.push(sup(
            DetectorKind::SilentSubsystem,
            "silent|steering|<query failed>",
            &format!(
                "the steering-queue query did not prepare ({e}) — this detector reported NOTHING \
                 about stalled queues on this tick, which is not the same as finding nothing. \
                 Usually a missing column: `sender` comes from ensure_fleet_tables' runtime \
                 ALTER, not from any migration."
            ),
        ));
    }
    if let Ok(mut stmt) = steer_stmt {
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, String>(3)?,
            ))
        });
        if let Ok(rows) = rows {
            for (session, n, oldest, senders) in rows.flatten() {
                let age = now - oldest;
                // WHICH DEADLINE APPLIES DEPENDS ON WHETHER THE LANE CAN RECEIVE
                // (AMUX-3696). This used to be one threshold for both states,
                // and the card it filed against THIS lane is the specimen: 92
                // minutes queued, while `/api/debug/steering` said
                // `blocked_reason: null, deliverable: true, stalled: false` and
                // the drain loop's own last skip read "not-at-turn-boundary
                // (within max age)". The lane was working, not stuck.
                //
                // The detector had the distinction in front of it the whole
                // time: its own evidence block tells the reader to go and check
                // `blocked_reason` before assuming the lane is hung. It named
                // the discriminator and did not use it, so the verdict "stuck"
                // was a claim about a condition never tested — a view
                // disagreeing with the mechanism it describes (ethos rule 1).
                //
                // `blocked` comes from `lane_block_reason`, the SAME predicate
                // the drain loop and /api/debug/steering use, computed off the
                // runtime before this sync pass (the AF-97 pattern the disk
                // detector already follows). Not a second reading of it.
                let block = blocked.get(&session).and_then(|b| b.clone());
                let deadline_min = match &block {
                    Some(_) => steering_stale_min(),
                    None => steering_stale_deliverable_min(),
                };
                if age < deadline_min * 60.0 {
                    continue;
                }
                let _ = stale_s;
                // ROUTE TO THE SENDER, NEVER THE STALLED LANE (AMUX-2796).
                //
                // This used to set `owner: Some(session)` — the lane whose queue
                // is stuck. Live proof of why that is wrong: AA-1 was assigned to
                // `amux-agent`, a lane with NO ENV FILE. It cannot receive a
                // message, cannot be woken, and cannot work a board card. The
                // notice landed in exactly the silence it was reporting.
                //
                // The card's own verdict text names the injured party in the
                // same breath — "every one of these was reported to its SENDER
                // as accepted" — and then addressed the card away from them.
                //
                // `steering_queue.sender` (AMUX-2785) carries the
                // server-verified X-Amux-Session stamp, so the right owner is
                // now knowable. Empty sender means an automated producer
                // (commit-nudge / board-drive / sched:<id>), whose owner is
                // whoever owns that producer — not the dead lane. Several
                // distinct senders means no single owner, so it stays
                // unassigned with all of them named: UNASSIGNED BEATS
                // MISASSIGNED, because an unassigned card gets triaged while a
                // misassigned one is somebody else's problem forever.
                let senders: Vec<&str> = senders
                    .split(',')
                    .map(str::trim)
                    .filter(|x| !x.is_empty() && *x != session)
                    .collect();
                let owner = match senders.as_slice() {
                    [one] => Some((*one).to_string()),
                    _ => None,
                };
                out.push(Finding {
                    kind: DetectorKind::SilentSubsystem,
                    signature: format!("silent|steering|{session}"),
                    title: match &block {
                        Some(r) => format!(
                            "{session}: steering queue cannot drain ({r}), oldest {:.0} min",
                            age / 60.0
                        ),
                        // NOT "stuck". A deliverable lane's queue is waiting for
                        // a turn boundary, and calling that stuck sends the
                        // reader to look for a hang that is not there.
                        None => format!(
                            "{session}: reachable but has not taken a turn boundary in {:.1}h",
                            age / 3600.0
                        ),
                    },
                    evidence: vec![
                        ("verdict".into(), match &block {
                            Some(r) => format!(
                                "{n} message(s) queued for {session}; the oldest has waited \
                                 {:.0} min (deadline {deadline_min:.0} min). The lane is NOT \
                                 reachable: {r}. This queue will not drain on its own — queued \
                                 is not delivered, and every one of these was reported to its \
                                 sender as accepted.",
                                age / 60.0
                            ),
                            None => format!(
                                "{n} message(s) queued for {session}; the oldest has waited \
                                 {:.0} min (deadline {deadline_min:.0} min). The lane IS \
                                 reachable — no block reason, same predicate the drain loop \
                                 uses — so this is not a stall: delivery happens at the next \
                                 TURN BOUNDARY and the lane has not reached one. Look for a \
                                 lane stuck in one very long turn, not for a broken queue. \
                                 Senders were told 'accepted', which remains true and remains \
                                 not-yet-delivered.",
                                age / 60.0
                            ),
                        }),
                        ("lane_reachable".into(), match &block {
                            Some(r) => format!("NO — {r}"),
                            None => "yes — no block reason from lane_block_reason".into(),
                        }),
                        ("queued".into(), n.to_string()),
                        ("oldest_queued_at".into(), rl::local_when(oldest)),
                        // The deadline that ACTUALLY applied, not always the
                        // blocked one. Reporting 90 next to a card that fired at
                        // 360 is a field disagreeing with the mechanism beside it
                        // (ethos rule 1) — caught by reading this payload during
                        // the mutation run.
                        ("threshold_min".into(), format!("{deadline_min:.0}")),
                        // Say WHY this card is not addressed to the stalled lane,
                        // or the next reader "corrects" it straight back — the
                        // lane's name is in the title and assigning it there
                        // looks obviously right until you notice the lane cannot
                        // receive anything, which is the whole defect.
                        ("senders".into(), if senders.is_empty() {
                            format!(
                                "none recorded — these were queued by an automated producer \
                                 (commit-nudge / board-drive / sched:<id>), or before senders were \
                                 tracked. Deliberately UNASSIGNED rather than assigned to \
                                 '{session}': {}",
                                match &block {
                                    Some(r) => format!(
                                        "that lane cannot receive anything right now ({r}), which \
                                         is what this card reports"
                                    ),
                                    None => "the lane is reachable, so there is nobody here \
                                             holding a false belief to notify"
                                        .to_string(),
                                }
                            )
                        } else {
                            format!(
                                "{} — this card is addressed to the SENDER, never to '{session}'. \
                                 The sender holds the false belief (\"I sent it\") and is the only \
                                 party who can act; the stalled lane may be unable to receive a \
                                 message at all, in which case a card assigned to it lands in the \
                                 same silence it reports.",
                                senders.join(", ")
                            )
                        }),
                        ("is_the_lane_reachable".into(),
                         "GET /api/debug/steering reports `blocked_reason` per lane live — \
                          no-env-file / not-running / archived are each actionable and completely \
                          different from merely busy. Read it before assuming the lane is hung."
                            .to_string()),
                    ],
                    recheck: format!(
                        "curl -sk \"$AMUX_URL/api/debug/steering\" | python3 -c \"import json,sys; \
                         print([l for l in json.load(sys.stdin)['lanes'] if l['session']=='{session}'])\""
                    ),
                    owner,
                    count: n as u64,
                    last_ts: now,
                    parked_until: None,
                });
            }
        }
    }
    (out, suppressed)
}

// ---------------------------------------------------------------------------
// Detector 5 — invariant breaches.
// ---------------------------------------------------------------------------

/// The invariants monitor already writes typed incidents with `occurrences`
/// and `resolved_at`. We file only the ones still open AND seen at least
/// `min_occurrences` times: a flapping invariant is noise, and one card per
/// flap is how a board stops being read.
/// One row of `_amux_invariant_incident`:
/// (invariant_id, entity_key, status, first_seen, last_seen, occurrences, expected, observed).
type InvRow = (String, String, String, f64, f64, i64, String, String, String);

/// How many distinct entities one invariant must be failing for before it is
/// filed as a SINGLE rollup card instead of one card per entity.
///
/// 4 is deliberately low. The failure this guards against is not "slightly too
/// many cards", it is a board that stops being readable: one invariant produced
/// 64 cards, 36% of every open item, each quoting the same occurrence count.
/// Three related entities is a coincidence worth three cards; four is a
/// pattern, and a pattern is one piece of work.
/// Distinct slow TARGETS in one tick before the outlier detector rolls up
/// (AMUX-3588). Mirrors `AMUX_INVARIANT_ROLLUP_AT`; floor of 2 for the same
/// reason — a "rollup" of one is just a card with worse wording.
/// How stale a suppressing card may get before the suppression is called out
/// as a MUTE rather than a dedupe (AMUX-3774).
///
/// Two days, because that is what the live specimen cost: AMUX-3651 sat in
/// `backlog` from 08-24 while every server-wide stall since was correctly
/// detected, correctly deduped, and filed nowhere. Config rather than a
/// constant for the D4 reason — a policy baked into code becomes the ceiling.
fn mute_warn_days() -> f64 {
    env_f64("AMUX_AUTOFIX_MUTE_WARN_DAYS", 2.0).max(0.25)
}

/// When a suppressing card stops suppressing (AMUX-3774).
///
/// The mute has to END, or "one card per fault" quietly becomes "one card per
/// fault, forever, whoever parks it first". Only OPEN cards suppress — that is
/// deliberate, so a judged-and-discarded card lets the next occurrence through
/// — but `backlog` is open AND parked, which is neither judged nor worked.
///
/// SEVEN DAYS, and the number is a judgement rather than a measurement, so it
/// is stated as one: it is long enough that deliberately parking a card still
/// buys real quiet (the whole point of parking), and short enough that a fault
/// class cannot go dark for a fortnight without anyone choosing that. The live
/// specimen reached 2 days before it was found by accident.
///
/// This does NOT bump the card's `updated`. That was the tempting version and it
/// is worse: it would make a parked card look actively worked, defeat every
/// staleness sweep that reads `updated`, and trade one dishonest signal for
/// another. Expiring the suppression says the true thing instead — nobody has
/// touched this, so the fault is loud again.
fn mute_expire_days() -> f64 {
    env_f64("AMUX_AUTOFIX_MUTE_EXPIRE_DAYS", 7.0).max(mute_warn_days())
}

fn outlier_rollup_at() -> usize {
    std::env::var("AMUX_OUTLIER_ROLLUP_AT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n >= 2)
        .unwrap_or(3)
}

fn invariant_rollup_at() -> usize {
    std::env::var("AMUX_INVARIANT_ROLLUP_AT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n >= 2)
        .unwrap_or(4)
}

pub fn detect_invariants(conn: &Connection, now: f64) -> (Vec<Finding>, Vec<Suppressed>) {
    let mut out = Vec::new();
    let mut suppressed = Vec::new();
    let min_occ = invariant_min_occurrences();
    // FRESHNESS (AMUX-3486): "has not self-healed" is only a true claim while
    // the failure is still being OBSERVED. The pane-agreement probe only
    // evaluates lanes that painted recently, so a lane that goes quiet leaves
    // its incident open-stale forever — no pass can ever close it — and the
    // filer then refiled the same 08-21 specimen eternally: discard re-armed
    // the idem, the open row refiled it, three cards for one incident in one
    // day. An incident whose last failing evaluation is older than the window
    // is "cannot be re-evaluated", a different truth from "still failing";
    // it stays visible on the debug surface but does not mint cards. A lane
    // that wakes and fails again refreshes last_seen and files normally.
    let fresh_cutoff = now - invariant_incident_fresh_s();
    let Ok(mut stmt) = conn.prepare(
        // `evidence` rides along for AMUX-3645: a dwell-window check declares
        // its self-heal epoch in there (InvariantResult::heals_at), and that is
        // what separates "held red on purpose until Tuesday" from "a fault that
        // is getting worse". Without it the two file identical cards.
        "SELECT invariant_id, entity_key, status, first_seen, last_seen, occurrences, \
                expected, observed, COALESCE(evidence, '') \
         FROM _amux_invariant_incident WHERE resolved_at IS NULL AND status = 'fail' \
         AND last_seen >= ?1 \
         ORDER BY occurrences DESC LIMIT 200",
    ) else {
        return (out, suppressed);
    };
    let Ok(rows) = stmt.query_map([fresh_cutoff], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, f64>(3)?,
            r.get::<_, f64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, String>(8)?,
        ))
    }) else {
        return (out, suppressed);
    };
    // FAN-IN BEFORE FAN-OUT (AMUX-2814). One invariant breaching across many
    // entities is ONE fact, and filing it per-entity turned a single breach
    // into 64 cards — 36% of every open card on the board, all quoting the same
    // 1857 occurrences, differing only in which route they named. That is ethos
    // rule 5: at volume it stops being a set of tasks and becomes a log, and a
    // board that cannot be read is a board nobody reads, including for the
    // cards that DO discriminate.
    //
    // So: group the rows by invariant first. Below `INVARIANT_ROLLUP_AT`
    // distinct entities, per-entity cards are right — they are individually
    // actionable and the entity is the useful title. At or above it, the entity
    // list is a SYMPTOM of one systemic fault and belongs in one card's
    // evidence, where it can be read in a single pass.
    let mut by_invariant: BTreeMap<String, Vec<InvRow>> = BTreeMap::new();
    for r in rows.flatten() {
        by_invariant.entry(r.0.clone()).or_default().push(r);
    }
    let rollup_at = invariant_rollup_at();
    let mut flat: Vec<InvRow> = Vec::new();
    for (id, mut group) in by_invariant {
        // Only entities that clear the flap threshold count toward the rollup —
        // otherwise a noisy invariant could roll itself up on rows that would
        // each have been suppressed anyway.
        let filing: Vec<&InvRow> = group.iter().filter(|r| r.5 >= min_occ).collect();
        if filing.len() < rollup_at {
            flat.append(&mut group);
            continue;
        }
        let entities: Vec<String> = filing
            .iter()
            .map(|r| if r.1.is_empty() { "fleet".to_string() } else { r.1.clone() })
            .collect();
        let worst = filing.iter().max_by_key(|r| r.5).unwrap();
        let first_seen = filing.iter().map(|r| r.3).fold(f64::MAX, f64::min);
        let last_seen = filing.iter().map(|r| r.4).fold(0.0f64, f64::max);
        let n = entities.len();
        out.push(Finding {
            kind: DetectorKind::InvariantBreach,
            // Keyed on the INVARIANT, not an entity — so the rollup dedupes
            // against itself as entities come and go, rather than re-filing
            // every time the set changes by one.
            //
            // EPISODE IDENTITY (AMUX-3633), the same shape AMUX-3472 gave
            // outliers and AMUX-3591 gave 5xx. `first_seen` is the start of the
            // current UNBROKEN failing run: `_amux_invariant_incident` rows are
            // per-episode and get a `resolved_at` when the invariant recovers,
            // so a recovery-then-refail opens a NEW row with a NEW first_seen.
            signature: format!("invariant|{id}|ROLLUP|{}", first_seen as i64),
            title: format!("invariant {id} failing across {n} entities — one fault, not {n} tasks"),
            evidence: vec![
                ("verdict".into(), format!(
                    "`{id}` is failing for {n} different entities and has not self-healed. Filed                      as ONE card on purpose: {n} per-entity cards would all carry the same cause                      and the same fix, and would bury every other card on the board. Fix the                      invariant, not the entities — if some entities are legitimately exempt, the                      invariant's scope is what is wrong."
                )),
                ("entities".into(), format!("\n  {}", entities.join("\n  "))),
                ("worst_entity".into(), format!(
                    "{} ({}x)",
                    if worst.1.is_empty() { "fleet" } else { &worst.1 },
                    worst.5
                )),
                ("expected".into(), truncate(&worst.6, 500)),
                ("observed".into(), truncate(&worst.7, 500)),
                ("first_seen".into(), rl::local_when(first_seen)),
                ("last_seen".into(), rl::local_when(last_seen)),
                ("rollup_threshold".into(), format!(
                    "{rollup_at} distinct entities (AMUX_INVARIANT_ROLLUP_AT)"
                )),
            ],
            // READS `failures` AND `invariant_id` — the shape the endpoint
            // actually returns. This said `.get('results',[])` and `r.get('id')`
            // and the endpoint has NEITHER key, so it printed `[]` no matter
            // what was failing. Every auto-filed invariant card shipped a
            // re-check that reported CLEAN unconditionally, as its own first
            // instruction — the AMUX-2140 shape, where following the sanctioned
            // step exactly is what produces the wrong answer.
            //
            // AMUX-3574: this used to query /api/health/invariants and print
            // "clean now" on an empty result, which is the one claim it CANNOT
            // make. That endpoint reports FAILURES ONLY, so a check that healed
            // and a check that was never evaluated in this build return the
            // identical empty list. Running this recipe on AMUX-3572 is exactly
            // what cost a session an investigation: it said clean, and
            // establishing whether that meant healed or never-ran took a second
            // endpoint and a direct read of the incident table.
            //
            // /api/debug/invariants is the only surface where a PASS is
            // visible, so it can separate all four states a reader needs:
            // pass, fail, unknown (stopped being checkable), and ABSENT (never
            // evaluated — which reads as health and is the dangerous one).
            recheck: format!(
                "curl -sk \"$AMUX_URL/api/debug/invariants\" | python3 -c \"import json,sys; \
                 d=json.load(sys.stdin); \
                 r=[x for x in (d.get('latest_per_invariant') or []) \
                 if x.get('invariant_id')=='{id}']; \
                 print('NOT EVALUATED in this build — absence is not health') if not r \
                 else [print(x['status'].upper(), x.get('entity',''), x.get('observed','')) \
                 for x in r]\""
            ),
            owner: None,
            count: filing.len() as u64,
            last_ts: last_seen.max(now - 1.0),
            parked_until: None,
        });
    }

    for (id, entity, _status, first, last, occ, expected, observed, evidence) in flat {
        let sig_entity = if entity.is_empty() { "fleet".to_string() } else { entity.clone() };
        // Episode identity, same reason as the ROLLUP arm above (AMUX-3633).
        let signature = format!("invariant|{id}|{sig_entity}|{}", first as i64);
        if occ < min_occ {
            suppressed.push(sup(
                DetectorKind::InvariantBreach,
                &signature,
                &format!("{occ} occurrence(s) < {min_occ} — a flapping invariant is noise, not a card"),
            ));
            continue;
        }
        // DWELL WINDOW (AMUX-3645): the check declared when it heals on its own.
        // Only a heal epoch still in the FUTURE parks the card — a declaration
        // that has already lapsed means the invariant should have passed by now
        // and has not, which is a real fault and the loudest kind, so it must
        // not be filed as "parked until a date that is behind us".
        let parked_until = serde_json::from_str::<serde_json::Value>(&evidence)
            .ok()
            .as_ref()
            .and_then(crate::invariants::heals_at_of)
            .filter(|t| *t > now);
        let verdict = match parked_until {
            // "has not self-healed" is TRUE here and reads as an escalating
            // fault, and `{occ}` reads as severity when it is the evaluation
            // rate times the dwell. AMUX-3640 shipped both about a check that
            // was working exactly as designed, and establishing that took
            // reading the check's source. State the disposition instead.
            Some(t) => format!(
                "Invariant `{id}` is a DWELL WINDOW held red on purpose for {sig_entity}, and it \
                 heals on its own at {} with no action from anyone. The {occ} occurrences are the \
                 evaluation rate times the dwell, not a severity. Do not work this as a fault: \
                 either the thing it names is already handled elsewhere (say where, on this card) \
                 or it still needs acknowledging before the window closes.",
                rl::local_when(t),
            ),
            None => format!(
                "Invariant `{id}` has been failing for {sig_entity} across {occ} evaluations \
                 and has not self-healed. Threshold to file: {min_occ}.",
            ),
        };
        out.push(Finding {
            kind: DetectorKind::InvariantBreach,
            signature,
            title: if parked_until.is_some() {
                // The count is in the ordinary title because it IS the severity
                // signal there. On a dwell card it is noise dressed as alarm.
                format!("invariant {id} in its dwell window — {sig_entity}")
            } else {
                format!("invariant {id} failing ({occ}x) — {sig_entity}")
            },
            evidence: vec![
                ("verdict".into(), verdict),
                ("expected".into(), truncate(&expected, 500)),
                ("observed".into(), truncate(&observed, 500)),
                ("first_seen".into(), rl::local_when(first)),
                ("last_seen".into(), rl::local_when(last)),
                ("occurrences".into(), occ.to_string()),
            ],
            // READS `failures` AND `invariant_id` — the shape the endpoint
            // actually returns. This said `.get('results',[])` and `r.get('id')`
            // and the endpoint has NEITHER key, so it printed `[]` no matter
            // what was failing. Every auto-filed invariant card shipped a
            // re-check that reported CLEAN unconditionally, as its own first
            // instruction — the AMUX-2140 shape, where following the sanctioned
            // step exactly is what produces the wrong answer.
            //
            // AMUX-3574: this used to query /api/health/invariants and print
            // "clean now" on an empty result, which is the one claim it CANNOT
            // make. That endpoint reports FAILURES ONLY, so a check that healed
            // and a check that was never evaluated in this build return the
            // identical empty list. Running this recipe on AMUX-3572 is exactly
            // what cost a session an investigation: it said clean, and
            // establishing whether that meant healed or never-ran took a second
            // endpoint and a direct read of the incident table.
            //
            // /api/debug/invariants is the only surface where a PASS is
            // visible, so it can separate all four states a reader needs:
            // pass, fail, unknown (stopped being checkable), and ABSENT (never
            // evaluated — which reads as health and is the dangerous one).
            recheck: format!(
                "curl -sk \"$AMUX_URL/api/debug/invariants\" | python3 -c \"import json,sys; \
                 d=json.load(sys.stdin); \
                 r=[x for x in (d.get('latest_per_invariant') or []) \
                 if x.get('invariant_id')=='{id}']; \
                 print('NOT EVALUATED in this build — absence is not health') if not r \
                 else [print(x['status'].upper(), x.get('entity',''), x.get('observed','')) \
                 for x in r]\""
            ),
            owner: None,
            count: occ as u64,
            last_ts: last.max(now - 1.0),
            parked_until,
        });
    }
    (out, suppressed)
}

// ---------------------------------------------------------------------------
// Detector 6 — build / deploy.
// ---------------------------------------------------------------------------

/// The deploy path itself. Two failure shapes, both of which let a whole fleet
/// believe its work shipped when it did not:
///
/// - the auto-builder is failing (its log records `BUILD FAILED` and does not
///   advance the stamp, by design — so a broken main is invisible unless
///   somebody reads that file);
/// - the stamp has not moved for hours while committed Rust source HAS.
///
/// Reads the builder's own log and stamp — filesystem, not a worker. If the
/// builder is dead the log simply stops, which the stamp-age check catches.
pub fn detect_build(_conn: &Connection, now: f64, home: &std::path::Path) -> (Vec<Finding>, Vec<Suppressed>) {
    let mut out = Vec::new();
    let mut suppressed = Vec::new();
    let stamp = home.join("rust-build-stamp");
    let log = home.join("logs").join("rust-auto-build.log");

    let tail = std::fs::read_to_string(&log).unwrap_or_default();
    let tail: String = {
        let lines: Vec<&str> = tail.lines().collect();
        lines[lines.len().saturating_sub(80)..].join("\n")
    };
    let failures = tail.matches("BUILD FAILED").count();
    // Only the CURRENT streak counts: a failure followed by an install is a
    // build that was fixed, and filing it would be filing history.
    let fixed_since = tail
        .rfind("== installed")
        .zip(tail.rfind("BUILD FAILED"))
        .map(|(i, f)| i > f)
        .unwrap_or(false);
    if failures > 0 && !fixed_since {
        let last = tail
            .lines()
            .rev()
            .find(|l| l.contains("BUILD FAILED"))
            .unwrap_or_default()
            .to_string();
        out.push(Finding {
            kind: DetectorKind::BuildDeploy,
            signature: "build|failing".into(),
            title: format!("auto-build failing — {failures} failure(s), none fixed since"),
            evidence: vec![
                ("verdict".into(),
                 "The Rust auto-builder is failing and has not installed a build since. The \
                  running server keeps the last good binary BY DESIGN, so nothing looks broken: \
                  every session's committed work is simply not deploying, silently.".into()),
                ("last_failure_line".into(), last),
                ("failures_in_tail".into(), failures.to_string()),
                ("log".into(), log.display().to_string()),
            ],
            recheck: "tail -40 ~/.amux/logs/rust-auto-build.log; \
                      CARGO_TARGET_DIR=/tmp/amux-check cargo build --release -p amux-server".into(),
            owner: None,
            count: failures as u64,
            last_ts: now,
            parked_until: None,
        });
    }

    if let Ok(meta) = std::fs::metadata(&stamp) {
        if let Ok(modified) = meta.modified() {
            let age_h = modified
                .elapsed()
                .map(|d| d.as_secs_f64() / 3600.0)
                .unwrap_or(0.0);
            if age_h > build_stale_h() && out.is_empty() {
                // Only meaningful when source has actually moved; otherwise a
                // quiet night looks like a broken deploy. Absent a repo to
                // ask, we suppress with the reason rather than guessing.
                suppressed.push(sup(
                    DetectorKind::BuildDeploy,
                    "build|stamp-stale",
                    &format!(
                        "stamp is {age_h:.1}h old (>{:.1}h) but the builder reports no failure — \
                         cannot distinguish a quiet night from a stalled builder without asking \
                         the repo; not filing on a guess",
                        build_stale_h()
                    ),
                ));
            }
        }
    }
    (out, suppressed)
}

// ---------------------------------------------------------------------------
// Disk pressure (AMUX-2700, after the 2026-08-10 ENOSPC incident).
// ---------------------------------------------------------------------------

/// Free space below this files a card. Default 50GB: high enough that a fleet
/// of 50 sessions plus a Rust build (a debug target tree is 10-15GB on its own)
/// has room to finish what it started, rather than a floor that is only crossed
/// once writes are already failing.
fn disk_min_free_gb() -> f64 {
    env_f64("AMUX_DISK_MIN_FREE_GB", 50.0).max(1.0)
}
/// The RATE trigger. An absolute floor is a lagging indicator: on 2026-08-10
/// the volume went from comfortable to 741MB with nothing reported, because
/// every sample before the last one was fine. Falling faster than this — over a
/// window long enough to not be a single build — is the signal that would have
/// fired hours earlier.
fn disk_fall_gb_per_h() -> f64 {
    env_f64("AMUX_DISK_FALL_GB_PER_H", 20.0).max(1.0)
}
/// Minimum span before a rate is a rate and not two adjacent samples.
fn disk_rate_window_min() -> f64 {
    env_f64("AMUX_DISK_RATE_WINDOW_MIN", 20.0).clamp(2.0, 24.0 * 60.0)
}

/// Free-space samples, newest last. **In memory, and therefore fiction across a
/// restart** — this process re-execs whenever anyone saves the binary, and the
/// ethos file is explicit that in-memory state which survives nothing is the
/// same as no state. That is tolerable HERE and only here: the absolute floor
/// is a pure function of one sample and keeps working through a restart, so a
/// restart costs the rate trigger its window (~20 min) and costs the floor
/// nothing. Persisting a sample every tick would itself be an append-only table
/// with no retention, which is the bug this detector exists to report.
fn disk_samples() -> &'static std::sync::RwLock<Vec<(f64, u64)>> {
    static CELL: std::sync::OnceLock<std::sync::RwLock<Vec<(f64, u64)>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::RwLock::new(Vec::new()))
}

/// Ceiling for ONE `du` walk, and for the whole set. Both are env-tunable
/// because the right number is a property of the machine's disk, not of amux.
fn du_path_timeout_s() -> f64 {
    std::env::var("AMUX_DISK_DU_PATH_TIMEOUT_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(5.0)
}

fn du_total_timeout_s() -> f64 {
    std::env::var("AMUX_DISK_DU_TOTAL_TIMEOUT_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(15.0)
}

/// Why a path has no size. A skip and a zero are already distinguished (a silent
/// 0 would rank the biggest consumer last and point the report at an innocent
/// directory); this splits the SKIP, because the two kinds need opposite
/// responses and were reported identically.
///
/// `~/.Trash` is the specimen. `du` on it returns "Operation not permitted"
/// (macOS TCC) — it does not run slowly, it cannot run at all. It landed in the
/// same bucket as a timeout, under a message reading "these paths exceeded the
/// du budget", and was named there 914 times in 24h. That message is false for
/// this path, will be false forever, and points whoever reads it at a budget
/// knob that cannot help (ethos rule 4: the instrument must express the
/// discriminator).
#[derive(Debug, PartialEq)]
enum DuOutcome {
    Sized(u64),
    /// Ran, hit the wall clock, was killed. Raising the budget would help.
    TimedOut,
    /// Exited without a parseable size. Raising the budget would NOT help.
    /// Carries the exit code rather than stderr text on purpose: reading stderr
    /// means piping it, and `du` over a tree with many unreadable subdirectories
    /// emits enough to fill the pipe buffer and BLOCK the child while we poll
    /// `try_wait` — which would convert a path that would have finished into a
    /// timeout. The detector must not manufacture the fault it reports.
    Unreadable(Option<i32>),
}

/// One bounded `du -skx`.
///
/// A non-zero exit with USABLE stdout is still a size: `du` exits 1 when it
/// could not descend into some subdirectory but it still prints the total for
/// everything it did read. Treating that as unreadable would discard a
/// mostly-correct figure for the common case of a few protected subdirs.
fn du_one(p: &std::path::Path, deadline: std::time::Instant) -> DuOutcome {
    let mut child = match std::process::Command::new("/usr/bin/du")
        .args(["-skx"])
        .arg(p)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return DuOutcome::Unreadable(None),
    };
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    // Kill AND reap: an unreaped `du` is a zombie holding the
                    // very FDs the neighbouring FdPressure detector counts.
                    let _ = child.kill();
                    let _ = child.wait();
                    // debug, not warn: `du_top` reports every skip ONCE per run and
                    // names the paths. This line fired per attempt on the same
                    // unchanging path (~/Library/Caches), which made it 78% of the
                    // server log in a 30-minute window and buried a per-minute panic.
                    tracing::debug!(path = %p.display(), "disk: du timed out — path skipped in the size ranking");
                    return DuOutcome::TimedOut;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return DuOutcome::Unreadable(None),
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(_) => return DuOutcome::Unreadable(None),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    match text.split_whitespace().next().and_then(|t| t.parse::<u64>().ok()) {
        Some(kb) => DuOutcome::Sized(kb * 1024),
        None => DuOutcome::Unreadable(out.status.code()),
    }
}

/// One path in the size ranking. `stale_age_s` is `Some` when the figure is a
/// CARRIED-FORWARD reading rather than one taken on this run.
#[derive(Debug, Clone, PartialEq)]
struct Ranked {
    path: String,
    bytes: u64,
    stale_age_s: Option<f64>,
}

/// Last successful size per path, persisted so a skipped path still has a
/// figure (AEAB-33).
///
/// # Why carry a stale number forward at all
///
/// `du` time scales with directory size, so the paths that blow the budget are
/// systematically the LARGEST — the ranker was biased against exactly the answer
/// it exists to produce. Measured 2026-08-19: `~/Library/Caches` (9.9G) was
/// skipped on 914 of 914 attempts in 24h while free space fell to 3.7G, and the
/// ranking that survived listed only the paths small enough to finish, i.e. the
/// also-rans, presented as the top consumers.
///
/// A day-old 9.9G answers "what is eating my disk". Absence does not, and
/// absence is what it reported. Cache directories do not shrink fast enough for
/// staleness to mislead at this granularity, and the age is carried alongside so
/// the reader can judge rather than being asked to trust.
///
/// # Why a file and not memory
///
/// This process re-execs to adopt a new build. In-memory state would be empty
/// on exactly the first run after every restart — and the ethos file records
/// that same mistake costing a whole optimisation invisibly. A ~200-byte JSON
/// file survives; it is written only on the ticks that already paid for a `du`,
/// which is only when free space is under the floor.
fn du_cache_load(path: Option<&std::path::Path>) -> std::collections::HashMap<String, (u64, f64)> {
    let Some(p) = path else { return Default::default() };
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str::<std::collections::HashMap<String, (u64, f64)>>(&t).ok())
        .unwrap_or_default()
}

fn du_cache_save(path: Option<&std::path::Path>, m: &std::collections::HashMap<String, (u64, f64)>) {
    let Some(p) = path else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(t) = serde_json::to_string(m) {
        let _ = std::fs::write(p, t);
    }
}

/// Rank the biggest consumers, under a hard wall-clock budget.
///
/// EVERY `du` HERE IS BOUNDED, and that is the whole point. This runs only
/// when free space is already under the floor (see `detect_disk`), i.e. only
/// on the machine where a `du` of `~/Library/Caches` is slowest — so an
/// unbounded walk is not a slow path, it is the failure mode. Unbounded, it
/// took the SERVER DOWN: `std::process::Command::output()` blocks the calling
/// thread, this runs on a tokio worker, and the tick repeats — so each tick
/// parked another worker on a multi-minute directory walk until none were left
/// to poll the accept loop. The server then held its listening socket with
/// every thread parked and 0% CPU: no panic, no log line, connections accepted
/// by the kernel and answered by nobody. It reads as a hang with no cause,
/// which is the most expensive shape a fault can have.
///
/// This is ethos rule 7's "what does the DETECTOR cost, and is the cost paid in
/// the same resource as the fault?" — here it was worse than the spin-catcher
/// it echoes, because the cost landed on the runtime rather than on the disk.
fn du_top(
    paths: &[std::path::PathBuf],
    limit: usize,
    cache_path: Option<&std::path::Path>,
) -> Vec<Ranked> {
    let started = std::time::Instant::now();
    let now = crate::runtime_jobs::registry::unix_now();
    let mut cache = du_cache_load(cache_path);
    let total_budget = std::time::Duration::from_secs_f64(du_total_timeout_s());
    let per_path = std::time::Duration::from_secs_f64(du_path_timeout_s());
    let mut sized: Vec<Ranked> = Vec::new();
    // Two skip reasons, kept apart, because they need OPPOSITE responses: a
    // timeout is a budget story and an unreadable path is a permissions story,
    // and one message covering both sent readers at a knob that cannot help.
    let mut timed_out: Vec<String> = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();
    // A path we could not size THIS run still gets its last known figure, if we
    // have one. NEVER a fabricated 0: `du_one`'s own doc records why (a silent
    // zero sorts the biggest consumer last and aims the report at an innocent
    // directory), and a path never successfully sized simply does not appear.
    // A free fn rather than a closure: a closure capturing `cache` would hold an
    // immutable borrow across the `cache.insert` below.
    fn carry(
        cache: &std::collections::HashMap<String, (u64, f64)>,
        now: f64,
        key: &str,
        sized: &mut Vec<Ranked>,
    ) {
        if let Some((bytes, at)) = cache.get(key).copied() {
            sized.push(Ranked { path: key.to_string(), bytes, stale_age_s: Some(now - at) });
        }
    }
    for p in paths.iter().filter(|p| p.exists()) {
        let key = p.display().to_string();
        let remaining = total_budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            timed_out.push(key.clone());
            carry(&cache, now, &key, &mut sized);
            continue;
        }
        let deadline = std::time::Instant::now() + per_path.min(remaining);
        match du_one(p, deadline) {
            DuOutcome::Sized(bytes) => {
                cache.insert(key.clone(), (bytes, now));
                sized.push(Ranked { path: key, bytes, stale_age_s: None });
            }
            DuOutcome::TimedOut => {
                timed_out.push(key.clone());
                carry(&cache, now, &key, &mut sized);
            }
            DuOutcome::Unreadable(code) => {
                unreadable.push(match code {
                    Some(c) => format!("{key} (du exit {c})"),
                    None => format!("{key} (du could not run)"),
                });
                carry(&cache, now, &key, &mut sized);
            }
        }
    }
    du_cache_save(cache_path, &cache);
    // Rule 4: a truncated ranking that does not say it was truncated reads as a
    // complete one. Whoever acts on this report must see that paths are missing
    // — AND why, since the two reasons have different fixes.
    //
    // It says it once per SKIP SET per hour, naming the paths. It was first
    // written to say it once per RUN rather than once per attempt, because the
    // per-attempt spelling reported the same unchanging path thousands of times
    // and drowned the log it shares with real faults — see the AEAB-45 note
    // below for why that was only half the fix.
    //
    // The known bias is stated in the message itself rather than left for the
    // reader to infer: `du` time scales with directory size, so the paths that
    // blow the budget are disproportionately the LARGEST ones. Measured
    // 2026-08-19 on this machine, the three skipped paths held ~18.5G against
    // 3.7G free while the ranking that DID emerge listed only the also-rans. A
    // report saying merely "incomplete" reads as "here are the big ones plus a
    // few we missed", which is closer to the inverse of the truth.
    // AEAB-45: "once per run" is not a dedupe when the run is on a two-minute
    // timer. The paragraph above was written when this warning moved from
    // once-per-ATTEMPT to once-per-RUN, and the inner loop was the only half
    // that got fixed: the outer one emitted 1,336 identical lines in 24h naming
    // `~/.Trash (du exit 1)`, a condition that CANNOT change — 77% of the whole
    // log, competing with three real findings in the same window. AEAB-13
    // recorded the identical shape at the identical ratio one subsystem over,
    // which is why the mechanism is shared rather than re-spelled here.
    //
    // Keyed on the PATH LIST, not on a bare "disk warned" flag, so a skip set
    // that GROWS is new information and reports at once inside the same hour.
    // Keyed separately per REASON, because a budget timeout and a permission
    // refusal have different fixes and the message says so.
    let bucket = crate::log_dedupe::hour_bucket(now);
    if !timed_out.is_empty() {
        let paths = timed_out.join(", ");
        if crate::log_dedupe::first_this_bucket(&format!("disk-du-budget:{paths}"), bucket) {
            tracing::warn!(
                skipped = timed_out.len(),
                paths = %paths,
                "disk: size ranking is INCOMPLETE — these paths exceeded the du budget. \
                 du time scales with size, so the paths missing here are LIKELIER TO BE \
                 THE LARGEST than the ones ranked below"
            );
        }
    }
    if !unreadable.is_empty() {
        let paths = unreadable.join(", ");
        if crate::log_dedupe::first_this_bucket(&format!("disk-unreadable:{paths}"), bucket) {
            tracing::warn!(
                skipped = unreadable.len(),
                paths = %paths,
                "disk: size ranking is INCOMPLETE — these paths could not be READ at all \
                 (permissions, e.g. macOS TCC on ~/.Trash). This is NOT a budget problem \
                 and no timeout increase will fix it"
            );
        }
    }
    sized.sort_by_key(|r| std::cmp::Reverse(r.bytes));
    sized.truncate(limit);
    sized
}

/// Candidate consumers. Deliberately NOT just `~/.amux`: on the night this was
/// written the top three consumers were a Trash full of screen recordings, a
/// pile of per-agent cargo target dirs under /private/tmp, and 24 hourly Time
/// Machine snapshots — none of which live under amux's home. A detector that
/// could only see its own directory would have named the innocent party.
/// A plain FILE big enough to matter to the ranking (AEAB-42).
///
/// Default 256 MB. Env-overridable rather than a constant because it is a
/// policy about what is worth a human's attention, and deviation D4 is the
/// record of what happens when that kind of number is compiled in.
fn disk_file_floor_bytes() -> u64 {
    std::env::var("AMUX_DISK_FILE_MIN_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(256)
        * 1024
        * 1024
}

/// True for a regular file at or above the floor. Directories are handled by
/// the caller; symlinks are skipped, since `read_dir` metadata follows them and
/// a link into a scanned directory would double-count.
fn is_big_file(e: &std::fs::DirEntry, floor: u64) -> bool {
    match e.file_type() {
        Ok(ft) if ft.is_file() => e.metadata().map(|m| m.len() >= floor).unwrap_or(false),
        _ => false,
    }
}

/// Which top-level `/private/tmp` entries are worth ranking.
///
/// THE FOURTH INSTANCE OF THE SHAPE THIS FUNCTION ALREADY NAMES TWICE, and the
/// comment in `disk_candidates` predicted it: "after adding a surfacing
/// mechanism, ask what the mechanism itself cannot express — and then ask it
/// again about the answer you just gave." The answer given by AEAB-42 (files)
/// and AMUX-3665 (checkout build trees) still could not express the AGENT
/// SCRATCH ROOT, which is the one directory every session is *told* to write to.
///
/// The filter was `n.contains("target")`. Agent scratch lives at
/// `/private/tmp/claude-<uid>`, and "claude-501" contains no "target", so the
/// whole tree was unrankable BY CONSTRUCTION — not skipped for budget, not for
/// permissions, never a candidate. Measured on this machine the day this was
/// written: 18 GB total, 12 GB of it in a single lane. On another machine the
/// same tree took the root volume to 0 bytes free and the lane sitting on it
/// could not open a file to find out why — it had to ask a human, because
/// amux's own disk report structurally could not name it.
///
/// Matching the `claude-` PREFIX rather than a literal uid keeps it correct on
/// any machine; the uid differs per host and hardcoding this one is the
/// "fact about one machine compiled into a public server" mistake the codebase
/// has a name for.
///
/// Nesting is worth stating: this scan reads only the TOP level of
/// `/private/tmp`, so a build tree INSIDE the scratch root was invisible twice
/// over — its parent did not match and it was never itself a top-level entry.
/// Adding the root fixes both, because the root is where `du` then descends.
fn tmp_candidate_name(n: &str) -> bool {
    n.contains("target") || n.starts_with("claude-")
}

fn disk_candidates(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<std::path::PathBuf> = Vec::new();
    // AEAB-42: FILES count, not only directories. Every branch of this function
    // used to push only `is_dir()` entries, which made a large file unrankable
    // BY CONSTRUCTION — not skipped for budget, not skipped for permissions,
    // simply never a candidate, so no timeout or budget change could ever
    // surface one. On 2026-08-22 `~/.amux/amux.db` was 1.8 GB on a volume with
    // 1.8 GB free, would have ranked FOURTH (above ~/.claude), and was absent
    // from all 26 entries of the ranker's own cache. amux's disk report pointed
    // the owner at caches and node_modules and structurally could not mention
    // the largest thing amux itself writes.
    //
    // This sits one layer under AEAB-33. That card taught the ranking to declare
    // the candidates it FAILED on — and that warning can only ever report
    // candidates that were GENERATED, so it was blind to this by construction
    // too. After adding a surfacing mechanism, ask what the mechanism itself
    // cannot express.
    //
    // Sizing stays uniform: a file candidate goes through `du_one` like any
    // other, rather than being short-circuited from the metadata we already
    // hold. `du` on a plain file is instant, and two sizing paths is the second
    // spelling that drifts.
    let floor = disk_file_floor_bytes();
    if let Ok(rd) = std::fs::read_dir(home) {
        for e in rd.flatten() {
            if e.metadata().map(|m| m.is_dir()).unwrap_or(false) || is_big_file(&e, floor) {
                v.push(e.path());
            }
        }
    }
    // Build trees AND agent scratch, wherever sessions put them.
    if let Ok(rd) = std::fs::read_dir("/private/tmp") {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if tmp_candidate_name(&n) && e.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                v.push(e.path());
            }
        }
    }
    if let Ok(h) = std::env::var("HOME") {
        for p in ["Library/Caches", ".cache", ".claude", ".Trash", ".npm"] {
            v.push(std::path::Path::new(&h).join(p));
        }
    }
    // BUILD TREES IN THE SESSIONS' OWN CHECKOUTS (AMUX-3665).
    //
    // The `/private/tmp` branch above says it catches "build trees, wherever
    // sessions put them", and it did when per-session scratch target dirs were
    // the convention. CLAUDE.md retired those (they filled this volume once
    // already: ~37 trees at 10-15GB), so the strays that remain are in the
    // CHECKOUT — and a checkout is in none of the roots above.
    //
    // Measured 2026-08-24, on the report that motivated this: 23 GB sat in
    // /Users/ethan/Dev/amux/target while the ranking listed 47 GB of ~/.amux
    // and named nothing bigger. It is gitignored, so `git status` cannot show
    // it either. The largest single reclaimable thing on the volume was
    // unrankable BY CONSTRUCTION — not skipped for budget, not skipped for
    // permissions, simply never a candidate.
    //
    // This is the THIRD instance of the pattern the two comments above already
    // name (AEAB-42: files were not candidates; AEAB-33: the failure warning
    // can only report candidates that were GENERATED). Both fixes were one
    // layer inside the generator. This one is the generator's ROOTS, which is
    // the layer neither could see. After adding a surfacing mechanism, ask what
    // the mechanism itself cannot express — and then ask it again about the
    // answer you just gave.
    //
    // The sessions' `CC_DIR` is the right source rather than a hardcoded
    // `~/Dev/*`: it is where the fleet ACTUALLY works, it is already a
    // primitive amux owns, and it needs no new configuration to be correct on
    // a machine laid out differently. Deduped, because most of the fleet shares
    // one checkout.
    let mut seen: std::collections::BTreeSet<std::path::PathBuf> = Default::default();
    if let Ok(rd) = std::fs::read_dir(home.join("sessions")) {
        for e in rd.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) != Some("env") {
                continue;
            }
            let cfg = crate::config::parse_env_file(&e.path());
            let Some(dir) = cfg.get("CC_DIR").map(|s| s.trim()).filter(|s| !s.is_empty()) else {
                continue;
            };
            for sub in ["target", "node_modules"] {
                let p = std::path::Path::new(dir).join(sub);
                // `is_dir` and not merely `exists`: a checkout without a build
                // tree must not become a candidate that `du` then fails on,
                // which would spend the ranking's budget reporting nothing.
                if p.is_dir() && seen.insert(p.clone()) {
                    v.push(p);
                }
            }
        }
    }
    v
}

/// Count APFS local snapshots. **This is the instrument that was missing.**
/// On 2026-08-10 roughly 450GB was deleted and free space moved by 8GB, because
/// 24 hourly Time Machine snapshots were pinning every deleted block. Without
/// this number in the evidence, the only available conclusion is "we deleted
/// the wrong things" and the next action is deleting more of the right ones,
/// which also does nothing. Snapshots are purgeable by macOS under pressure and
/// thinnable with `tmutil`, so this line turns an unexplainable non-recovery
/// into a one-command fix.
/// Run a short subprocess with a wall-clock ceiling, killing AND reaping it on
/// overrun.
///
/// Exists because `du_one` was the only bounded subprocess in this file and the
/// bound was written into its body, so the next subprocess added here inherited
/// nothing (AF-97). `tmutil` was that next one. Reaping matters for the same
/// reason it does in `du_one`: an unreaped child is a zombie holding the very
/// FDs the neighbouring `detect_fd` detector counts, so an unbounded probe here
/// makes the detector beside it report pressure that the probe itself caused.
fn bounded_output(program: &str, args: &[&str], budget: std::time::Duration) -> Option<Vec<u8>> {
    let deadline = std::time::Instant::now() + budget;
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                let out = child.wait_with_output().ok()?;
                return st.success().then_some(out.stdout);
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // WARN, not debug: unlike du's per-path skips this is one
                    // line per tick at most, and a `tmutil` that stops answering
                    // is a real machine fault worth seeing in a log sweep.
                    tracing::warn!(
                        program,
                        budget_s = budget.as_secs_f64(),
                        "autofix: subprocess exceeded its budget and was killed — \
                         the value it would have produced is reported as absent, not as zero"
                    );
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

fn local_snapshot_count() -> Option<usize> {
    // 5s: `tmutil listlocalsnapshots /` answers in well under a second on a
    // healthy machine. It talks to backupd, so a wedged Time Machine can hang it
    // indefinitely — and before AF-97 that hang had no ceiling at all, on a
    // thread that was also holding the SQLite store lock.
    let stdout = bounded_output(
        "/usr/bin/tmutil",
        &["listlocalsnapshots", "/"],
        std::time::Duration::from_secs(5),
    )?;
    Some(String::from_utf8_lossy(&stdout).matches("com.apple.TimeMachine").count())
}

pub fn detect_disk(now: f64, home: &std::path::Path) -> (Vec<Finding>, Vec<Suppressed>) {
    let mut out = Vec::new();
    let mut suppressed = Vec::new();
    const GB: f64 = 1_073_741_824.0;

    let Some(free) = crate::runtime_jobs::storage::disk_free_bytes(home) else {
        suppressed.push(sup(
            DetectorKind::DiskPressure,
            "disk|unreadable",
            "df returned nothing — cannot measure free space, so not filing either way",
        ));
        return (out, suppressed);
    };
    let free_gb = free as f64 / GB;

    // Record the sample, keep a bounded window.
    let (mut rate_gb_h, mut span_min) = (0.0f64, 0.0f64);
    if let Ok(mut s) = disk_samples().write() {
        s.push((now, free));
        let keep_from = now - 6.0 * 3600.0;
        s.retain(|(t, _)| *t >= keep_from);
        if let (Some(first), Some(last)) = (s.first().copied(), s.last().copied()) {
            span_min = (last.0 - first.0) / 60.0;
            if span_min >= disk_rate_window_min() {
                let dropped_gb = (first.1 as f64 - last.1 as f64) / GB;
                rate_gb_h = dropped_gb / (span_min / 60.0);
            }
        }
    }

    let floor = disk_min_free_gb();
    let falling = rate_gb_h >= disk_fall_gb_per_h();
    if free_gb >= floor && !falling {
        return (out, suppressed);
    }

    // Only now pay for `du` — a directory walk on every tick would be a
    // detector whose cost lands on the resource it is watching.
    // `home` here is the AMUX home (`~/.amux`), NOT `$HOME` — see the call site,
    // which passes `autofix::amux_home()`. Joining ".amux" again wrote the cache
    // to `~/.amux/.amux/du-sizes.json`, which worked but left a stray nested
    // directory and would confuse anyone looking for it. Caught only by checking
    // the shipped path in prod rather than assuming the deploy matched intent.
    let du_cache = home.join("du-sizes.json");
    let top = du_top(&disk_candidates(home), 8, Some(&du_cache));
    // A carried-forward figure is LABELLED, never presented as a fresh
    // measurement. The whole value of keeping it is that the reader can weigh
    // it; passing it off as current would trade one wrong answer for another.
    let listing = top
        .iter()
        .map(|r| match r.stale_age_s {
            None => format!("  {:>8.1} GB  {}", r.bytes as f64 / GB, r.path),
            Some(age) => format!(
                "  {:>8.1} GB  {}   [stale — last measured {:.1}h ago; du could not \
                 finish it this run]",
                r.bytes as f64 / GB,
                r.path,
                age / 3600.0
            ),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let snaps = local_snapshot_count();

    let (signature, title, verdict) = if free_gb < floor {
        (
            "disk|below-floor".to_string(),
            format!("disk: {free_gb:.1} GB free, below the {floor:.0} GB floor"),
            "Free space is under the floor with a fleet running. Writes fail with ENOSPC \
             before anything else reports a fault: on 2026-08-10 this surfaced as transcript \
             writes failing, not as a disk alert."
                .to_string(),
        )
    } else {
        (
            "disk|falling".to_string(),
            format!("disk: falling {rate_gb_h:.0} GB/h — {free_gb:.1} GB left"),
            format!(
                "Free space is dropping at {rate_gb_h:.1} GB/h over the last {span_min:.0} min. \
                 At this rate the {floor:.0} GB floor is {:.1} h away. Nothing is broken yet — \
                 this is the lead time an absolute floor cannot give you.",
                ((free_gb - floor) / rate_gb_h).max(0.0)
            ),
        )
    };

    let mut evidence = vec![
        ("verdict".into(), verdict),
        ("free_gb".into(), format!("{free_gb:.1}")),
        ("floor_gb".into(), format!("{floor:.0} (AMUX_DISK_MIN_FREE_GB)")),
        (
            "fall_gb_per_h".into(),
            format!("{rate_gb_h:.1} over {span_min:.0} min (trigger: {:.0}, AMUX_DISK_FALL_GB_PER_H)", disk_fall_gb_per_h()),
        ),
        ("top_consumers".into(), format!("\n{listing}")),
    ];
    if let Some(n) = snaps {
        evidence.push((
            "apfs_local_snapshots".into(),
            format!(
                "{n} — READ THIS BEFORE DELETING ANYTHING. Each hourly Time Machine snapshot \
                 pins the blocks of every file deleted since it was taken, so with snapshots \
                 present, deleting files frees NOTHING until they age out (24h) or are thinned. \
                 On 2026-08-10, 450GB was deleted and free space moved 8GB for exactly this \
                 reason. Thin them first: sudo tmutil thinlocalsnapshots / 500000000000 4"
            ),
        ));
    }

    out.push(Finding {
        kind: DetectorKind::DiskPressure,
        signature,
        title,
        evidence,
        recheck: "df -h /System/Volumes/Data; tmutil listlocalsnapshots / | wc -l; \
                  curl -sk $AMUX_URL/api/debug/storage"
            .into(),
        owner: None,
        count: 1,
        last_ts: now,
        parked_until: None,
    });
    (out, suppressed)
}

// ---------------------------------------------------------------------------
// File-descriptor pressure (AMUX-2812, after the 2026-08-10 EMFILE incident).
//
// At ~18:26 every operation started failing with "Too many open files", the
// process spun at 115% CPU retrying spawns, `/health` stopped answering, and
// the dashboard read as OFFLINE — which is indistinguishable from a dead
// machine, so the first hour was spent looking in the wrong place. The launchd
// plist never set `NumberOfFiles`, so the service ran at macOS's default 256
// with ~60 tmux sessions, SSE clients and subprocess spawns against it. The
// limit is now 65536 soft+hard, which removes THIS occurrence and removes
// nothing about the next one: a fleet that grows into any limit reports the
// same silence.
// ---------------------------------------------------------------------------

/// Open descriptors as a FRACTION of the process limit.
///
/// A ratio, not a count, because the limit is precisely what changed: the same
/// 220 open descriptors were fatal at 256 and are unremarkable at 65536. A
/// count-based floor would have to be re-chosen every time the plist changes,
/// and a floor nobody re-chooses is a floor that silently stops meaning
/// anything.
fn fd_max_ratio() -> f64 {
    env_f64("AMUX_FD_MAX_RATIO", 0.7).clamp(0.1, 0.99)
}

/// The RATE trigger, in ratio-points per hour. Same reasoning as
/// `AMUX_DISK_FALL_GB_PER_H`: an absolute ceiling is a lagging indicator that
/// fires once things are already failing, and descriptors are LEAKED far more
/// often than they are legitimately consumed — a steady climb with no plateau
/// is the shape worth catching, hours before the ceiling.
fn fd_rise_per_h() -> f64 {
    env_f64("AMUX_FD_RISE_PER_H", 0.2).max(0.01)
}

/// Minimum span before a rate is a rate rather than two adjacent samples.
fn fd_rate_window_min() -> f64 {
    env_f64("AMUX_FD_RATE_WINDOW_MIN", 20.0).clamp(2.0, 24.0 * 60.0)
}

/// In-memory, and therefore fiction across a restart — the same trade
/// [`disk_samples`] documents, and tolerable for the same reason: the ceiling
/// check is a pure function of ONE sample and survives a restart intact, so a
/// restart costs the rate trigger its window and costs the ceiling nothing.
fn fd_samples() -> &'static std::sync::RwLock<Vec<(f64, usize)>> {
    static CELL: std::sync::OnceLock<std::sync::RwLock<Vec<(f64, usize)>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::RwLock::new(Vec::new()))
}

/// Open descriptors for THIS process, counted WITHOUT spawning anything.
///
/// `lsof -p $$ | wc -l` is the obvious instrument and it is the wrong one here,
/// for the reason ethos rule 7 records about the spin-catcher: **what does the
/// detector cost, and is the cost paid in the same resource as the fault?** The
/// fault is "this process cannot open files or spawn subprocesses" — during the
/// incident it was burning 115% CPU failing to spawn — so a detector that
/// shells out would fail exactly when it is needed, and each attempt would
/// consume the descriptors it is trying to report on. Reading `/dev/fd` costs
/// one descriptor, no fork, and no PATH lookup.
///
/// The returned count includes the directory handle this read itself holds, so
/// it is high by one. Left uncorrected on purpose: an off-by-one against a
/// 70%-of-limit trigger is noise, and a fudge factor in the measurement is
/// something the next reader has to reverse-engineer.
fn open_fd_count() -> Option<usize> {
    // /dev/fd on macOS and Linux both enumerate the CALLING process's
    // descriptors (on Linux via the /proc/self/fd symlink).
    std::fs::read_dir("/dev/fd").ok().map(|it| it.count())
}

/// The process's own soft limit — NOT `launchctl limit maxfiles`, which reports
/// the system-wide default and would have said 256 while the service ran at
/// 65536, or vice versa. The number that matters is the one this process is
/// actually subject to.
fn fd_limit() -> Option<u64> {
    let mut rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit writes into a fully-initialised local; no aliasing.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } == 0 {
        Some(rl.rlim_cur)
    } else {
        None
    }
}

/// THE DECISION, pure — so the regression corpus is the incident's own numbers
/// (220 open against a 256 limit) rather than whatever this machine happens to
/// be doing while the test runs. A detector whose trigger can only be exercised
/// by reproducing the outage is a detector nobody checks.
///
/// Returns the signature suffix, or `None` for "nothing to report".
fn fd_trigger(
    ratio: f64,
    rise_per_h: f64,
    ceiling: f64,
    rise_trigger: f64,
) -> Option<&'static str> {
    if ratio >= ceiling {
        // The ceiling wins when both fire: "you are nearly out" is a more
        // actionable sentence than "you are heading for nearly out".
        return Some("near-limit");
    }
    if rise_per_h >= rise_trigger {
        return Some("climbing");
    }
    None
}

pub fn detect_fd(now: f64) -> (Vec<Finding>, Vec<Suppressed>) {
    let mut out = Vec::new();
    let mut suppressed = Vec::new();

    let (Some(open), Some(limit)) = (open_fd_count(), fd_limit()) else {
        suppressed.push(sup(
            DetectorKind::FdPressure,
            "fd|unreadable",
            "could not read /dev/fd or getrlimit — cannot measure descriptor use, so not \
             filing either way",
        ));
        return (out, suppressed);
    };
    if limit == 0 {
        suppressed.push(sup(DetectorKind::FdPressure, "fd|no-limit", "RLIMIT_NOFILE is 0"));
        return (out, suppressed);
    }
    let ratio = open as f64 / limit as f64;

    let (mut rise_per_h, mut span_min) = (0.0f64, 0.0f64);
    if let Ok(mut s) = fd_samples().write() {
        s.push((now, open));
        let keep_from = now - 6.0 * 3600.0;
        s.retain(|(t, _)| *t >= keep_from);
        if let (Some(first), Some(last)) = (s.first().copied(), s.last().copied()) {
            span_min = (last.0 - first.0) / 60.0;
            if span_min >= fd_rate_window_min() {
                let gained = (last.1 as f64 - first.1 as f64) / limit as f64;
                rise_per_h = gained / (span_min / 60.0);
            }
        }
    }

    let ceiling = fd_max_ratio();
    let Some(trigger) = fd_trigger(ratio, rise_per_h, ceiling, fd_rise_per_h()) else {
        return (out, suppressed);
    };

    let (signature, title, verdict) = if trigger == "near-limit" {
        (
            "fd|near-limit".to_string(),
            format!("fds: {open}/{limit} open ({:.0}% of the limit)", ratio * 100.0),
            format!(
                "This process holds {open} of its {limit} allowed descriptors. Past the limit, \
                 EVERY open and EVERY spawn fails with EMFILE at once — on 2026-08-10 that \
                 surfaced as /health going SILENT and the dashboard reading offline, which is \
                 indistinguishable from the machine being down. Nothing reports 'out of \
                 descriptors' because reporting it also needs one."
            ),
        )
    } else {
        (
            "fd|climbing".to_string(),
            format!(
                "fds: climbing {:.0} pts/h — {open}/{limit} ({:.0}%)",
                rise_per_h * 100.0,
                ratio * 100.0
            ),
            format!(
                "Descriptor use is rising at {:.1} percentage points per hour over the last \
                 {span_min:.0} min, now at {:.0}% of {limit}. At this rate the {:.0}% ceiling is \
                 {:.1} h away. Nothing is broken yet — a steady climb with no plateau is a LEAK, \
                 and it is the shape a ceiling check cannot report until it is too late.",
                rise_per_h * 100.0,
                ratio * 100.0,
                ceiling * 100.0,
                ((ceiling - ratio) / rise_per_h).max(0.0)
            ),
        )
    };

    out.push(Finding {
        kind: DetectorKind::FdPressure,
        signature,
        title,
        evidence: vec![
            ("verdict".into(), verdict),
            ("open_fds".into(), open.to_string()),
            ("limit".into(), format!("{limit} (RLIMIT_NOFILE soft, this process)")),
            ("ratio".into(), format!("{:.2}", ratio)),
            (
                "ceiling".into(),
                format!("{:.2} (AMUX_FD_MAX_RATIO)", ceiling),
            ),
            (
                "rise_per_h".into(),
                format!(
                    "{:.3} over {span_min:.0} min (trigger: {:.3}, AMUX_FD_RISE_PER_H)",
                    rise_per_h,
                    fd_rise_per_h()
                ),
            ),
            (
                "if_the_limit_is_low".into(),
                "macOS defaults a launchd service to 256 descriptors and the amux plist did \
                 not set NumberOfFiles until 2026-08-10. If `limit` above reads 256, the plist \
                 is stale — check SoftResourceLimits/HardResourceLimits/NumberOfFiles in \
                 ~/Library/LaunchAgents/com.amux.server-rs.plist (should be 65536), then \
                 `launchctl kickstart -k gui/$(id -u)/com.amux.server-rs`."
                    .to_string(),
            ),
        ],
        recheck: "launchctl print gui/$(id -u)/com.amux.server-rs | grep -A3 'resource limits'; \
                  lsof -p $(curl -sk $AMUX_URL/health | python3 -c 'import json,sys;print(json.load(sys.stdin)[\"pid\"])') | wc -l"
            .into(),
        owner: None,
        count: 1,
        last_ts: now,
        parked_until: None,
    });
    (out, suppressed)
}

// ---------------------------------------------------------------------------
// The one filing path.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Detector 8 — CI / deploy (AMUX-2780).
//
// The motivating incident: `deploy-cloud.yml` failed 31 consecutive times over
// three days and NOBODY SAW IT. cloud.amux.io received no deploy at all in that
// window. Every push printed a red X that no instrument in amux read, because
// amux had no instrument that read GitHub at all. The defect is not the SSH key
// that broke — that is one incident. The defect is that CI can fail every night
// and nothing tells anyone.
//
// Two things make this detector different from the other seven, and both are
// worth stating because they change what can go wrong:
//
//  1. **Its instrument is off-machine.** Every other detector reads SQLite or
//     the local filesystem, so "no data" means "nothing happened". Here "no
//     data" can also mean gh is missing, unauthenticated, rate-limited, or the
//     network is down — i.e. the detector is blind, which looks EXACTLY like
//     the all-clear. So every one of those cases emits a `Suppressed` naming
//     the reason. A blind CI detector that reports silence is the precise
//     failure this file exists to end.
//  2. **Failure here is a LEVEL, not an absence.** The ethos warns that
//     picking a threshold is the tell that you are guessing, and it is right:
//     `ci_min_failures()` is a guess. What is NOT a guess is the dedupe key —
//     see the streak anchor below, which is what turns "red every night" into
//     one card instead of thirty.
// ---------------------------------------------------------------------------

/// One completed workflow run, as GitHub reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct CiRun {
    pub repo: String,
    /// `workflowName` — the human name, which is what a card should say.
    pub workflow: String,
    pub run_id: i64,
    /// `completed` / `in_progress` / `queued`.
    pub status: String,
    /// `success` / `failure` / `cancelled` / `timed_out` / `startup_failure` / …
    pub conclusion: String,
    /// `push` / `pull_request` / `schedule` / `workflow_dispatch` / …
    pub event: String,
    pub branch: String,
    /// Computed at fetch time against the repo's real default branch, so the
    /// decision function stays pure and testable without a network call.
    pub on_default_branch: bool,
    pub created_at: f64,
    pub url: String,
    /// Populated only for the newest failing run of a failing workflow — it
    /// costs one extra API call each, so it is not fetched for history.
    pub failing_step: Option<String>,
}

/// A workflow whose redness means MERGED AND DEPLOYED HAVE DIVERGED.
///
/// Matched on the name because that is the only signal `gh run list` gives, and
/// a repo-specific allowlist would be a second thing to keep in step with the
/// workflows themselves. Deliberately narrow: a red test suite is bad and gets
/// a card, but it does not mean shipped work is silently not reaching users.
fn ci_is_deploy_workflow(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("deploy") || n.contains("release") || n.contains("publish")
}

/// Consecutive failures before a DEPLOY workflow escalates past a card.
fn ci_deploy_alert_streak() -> i64 {
    env_i64("AMUX_CI_DEPLOY_ALERT_STREAK", 10).max(2)
}

/// ...AND it must have been red this long. Both conditions, because they catch
/// different mistakes: ten pushes in one busy hour is not three days of
/// undeployed work, and one push a day for a week is.
fn ci_deploy_alert_hours() -> f64 {
    env_f64("AMUX_CI_DEPLOY_ALERT_H", 24.0).max(1.0)
}

/// Deploy workflows that have been red long enough and often enough that a
/// board card has demonstrably not been enough.
///
/// Returns (repo, workflow, streak, hours_red, newest_url). Pure, so the bar can
/// be tested against the real 2026-08-10 outage rather than a fixture.
pub fn ci_deploy_escalations(runs: &[CiRun], now: f64) -> Vec<(String, String, i64, f64, String)> {
    let mut groups: BTreeMap<(String, String), Vec<&CiRun>> = BTreeMap::new();
    for r in runs {
        if !ci_is_deploy_workflow(&r.workflow) || !r.on_default_branch {
            continue;
        }
        if r.event == "pull_request" || r.event == "pull_request_target" || r.status != "completed" {
            continue;
        }
        groups.entry((r.repo.clone(), r.workflow.clone())).or_default().push(r);
    }
    let mut out = Vec::new();
    for ((repo, workflow), mut all) in groups {
        all.sort_by(|a, b| b.created_at.total_cmp(&a.created_at));
        let mut streak = 0i64;
        let mut oldest_failure = now;
        let mut newest_url = String::new();
        for r in &all {
            if ci_is_inconclusive(&r.conclusion) {
                continue; // carries no evidence either way
            }
            if !ci_is_failure(&r.conclusion) {
                break; // a green run ends the streak
            }
            if streak == 0 {
                newest_url = r.url.clone();
            }
            streak += 1;
            oldest_failure = oldest_failure.min(r.created_at);
        }
        let hours = ((now - oldest_failure) / 3600.0).max(0.0);
        if streak >= ci_deploy_alert_streak() && hours >= ci_deploy_alert_hours() {
            out.push((repo, workflow, streak, hours, newest_url));
        }
    }
    out
}

/// Conclusions that mean "this run failed".
fn ci_is_failure(c: &str) -> bool {
    matches!(c, "failure" | "timed_out" | "startup_failure")
}

/// Conclusions that carry NO evidence either way and must not break a streak.
///
/// `cancelled` is here because the owner cancelling a run is not a fault (the
/// task's own suppression requirement), and because letting it break the streak
/// would reset the anchor and refile a card mid-outage — the 08-10 16:42 run in
/// the motivating incident was exactly this.
fn ci_is_inconclusive(c: &str) -> bool {
    matches!(c, "cancelled" | "skipped" | "neutral" | "stale" | "action_required" | "")
}

fn ci_sanitize(s: &str) -> String {
    s.replace('|', "/").trim().to_string()
}

fn ci_when(ts: f64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M %Z").to_string())
        .unwrap_or_else(|| format!("{ts:.0}"))
}

/// Probe connector/Gmail account health (via the connectors rollup, which
/// caches its provider round trips 300s) and map every `needs_reauth` account
/// to a finding. A failed probe is a SUPPRESSION, never silence — absence of
/// data is not evidence the grants are healthy (the disk-detector rule).
async fn connector_auth_probe(now: f64, home: &std::path::Path) -> (Vec<Finding>, Vec<Suppressed>) {
    let http: std::sync::Arc<dyn crate::integrations::email::HttpTransport> =
        std::sync::Arc::new(crate::integrations::email::ReqwestTransport::new());
    let rollup = crate::api::connectors::accounts_rollup(&http, home, false).await;
    let findings = connector_findings_from_rollup(&rollup, now);
    for f in &findings {
        // The WARN is the log-surfacing half of the two-fixes rule: a sweep of
        // the server log sees token rot even if the card is never read.
        tracing::warn!(
            "connector_auth: {} — one re-approval repairs every worker (card follows via autofix)",
            f.title
        );
    }
    (findings, Vec::new())
}

/// Pure mapper: the rollup's `needs_reauth` entries → findings. Signature is
/// `connector-reauth|<account>` — stable per ACCOUNT, so a card files once per
/// rotted account however many families or ticks report it.
pub fn connector_findings_from_rollup(rollup: &serde_json::Value, now: f64) -> Vec<Finding> {
    let mut out = Vec::new();
    for e in rollup.get("needs_reauth").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        let account = e.get("account").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if account.is_empty() {
            continue;
        }
        let broken: Vec<String> = e
            .get("broken")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let reconnect = e.get("reconnect").and_then(|v| v.as_str()).unwrap_or("").to_string();
        out.push(Finding {
            kind: DetectorKind::ConnectorAuth,
            signature: format!("connector-reauth|{account}"),
            title: format!(
                "Connector account {account} needs re-authorization ({})",
                broken.join(", ")
            ),
            evidence: vec![
                ("account".into(), account.clone()),
                ("broken".into(), broken.join(", ")),
                (
                    "effect".into(),
                    "every worker using this account fails (email reads/sends, token mints) until ONE re-approval".into(),
                ),
                ("reconnect".into(), reconnect),
            ],
            recheck: "curl -sk $(amux url)/api/connectors/accounts".into(),
            owner: None,
            count: 1,
            last_ts: now,
            parked_until: None,
        });
    }
    // Canary legs (AMUX-3430): the grant refreshes but a granted API answers
    // failure — a rot the reauth finding cannot see. Only `api_error` files:
    // `unreachable` is network trouble (a dead network is not connector rot
    // and must never fan out one card per account), `not_granted` is neutral,
    // and needs_reauth legs are already the reauth finding above.
    for a in rollup.get("accounts").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        let account = a.get("account").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let Some(canary) = a.get("canary").and_then(|v| v.as_object()) else { continue };
        for (svc, leg) in canary {
            if leg.get("status").and_then(|v| v.as_str()) != Some("api_error") {
                continue;
            }
            let http_st = leg.get("http").and_then(|v| v.as_u64()).unwrap_or(0);
            out.push(Finding {
                kind: DetectorKind::ConnectorAuth,
                signature: format!("connector-canary|{account}|{svc}"),
                title: format!(
                    "Connector canary failing: {svc} API for {account} (HTTP {http_st})"
                ),
                evidence: vec![
                    ("account".into(), account.clone()),
                    ("service".into(), svc.clone()),
                    ("http".into(), http_st.to_string()),
                    (
                        "detail".into(),
                        leg.get("detail").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    ),
                    (
                        "effect".into(),
                        "the token mints but this API rejects it — workers using this service fail while the account still reads connected".into(),
                    ),
                ],
                recheck: "curl -sk $(amux url)/api/connectors/accounts".into(),
                owner: None,
                count: 1,
                last_ts: now,
                parked_until: None,
            });
        }
    }
    out
}

/// The decision. Pure: same runs in, same findings out, no clock beyond `now`
/// and no network — which is what lets the tests build a 31-failure streak and
/// a recovery without touching GitHub.
///
/// # The dedupe key is a STREAK ANCHOR, not the workflow
///
/// `ci|<repo>|<workflow>|<run id of the OLDEST failure in the current streak>`.
///
/// Keying on the workflow alone files once and then never again, so a workflow
/// that is fixed and breaks again months later stays silent forever. Keying on
/// the latest run files nightly, which is the thing the card asks us not to do.
/// The anchor is stable for as long as the outage lasts and changes exactly
/// when a green run splits one outage from the next — so a three-day outage is
/// one card, and a genuinely new outage after a fix is a genuinely new card.
///
/// The anchor's one dependency is that the last SUCCESS is still inside the
/// fetched window. When it is not, the anchor would silently drift older every
/// time a run ages out and refile on every drift; so that case gets the fixed
/// anchor `since-beyond-window` and says so in the evidence, rather than
/// pretending to a precision it does not have.
pub fn ci_findings(runs: &[CiRun], now: f64) -> (Vec<Finding>, Vec<Suppressed>) {
    let mut out = Vec::new();
    let mut suppressed = Vec::new();
    let min_failures = ci_min_failures();

    // Group by (repo, workflow). BTreeMap so the output order is deterministic
    // — a detector whose findings reorder between ticks is one nobody can diff.
    let mut groups: BTreeMap<(String, String), Vec<&CiRun>> = BTreeMap::new();
    for r in runs {
        groups.entry((r.repo.clone(), ci_sanitize(&r.workflow))).or_default().push(r);
    }

    for ((repo, workflow), all) in groups {
        // --- eligibility, with the exclusions PUBLISHED -----------------
        let mut n_pr = 0usize;
        let mut n_branch = 0usize;
        let mut n_running = 0usize;
        let mut eligible: Vec<&CiRun> = Vec::new();
        for r in &all {
            if r.event == "pull_request" || r.event == "pull_request_target" {
                n_pr += 1;
                continue;
            }
            if !r.on_default_branch {
                n_branch += 1;
                continue;
            }
            if r.status != "completed" {
                n_running += 1;
                continue;
            }
            eligible.push(r);
        }
        if n_pr + n_branch + n_running > 0 {
            suppressed.push(sup(
                DetectorKind::CiFailure,
                &format!("ci|{repo}|{workflow}"),
                &format!(
                    "excluded {n_pr} pull-request/fork run(s), {n_branch} non-default-branch \
                     run(s), {n_running} still-running run(s) — only completed default-branch \
                     runs are evidence about main"
                ),
            ));
        }

        // Newest first. Sorted here rather than trusting the caller: `gh`
        // happens to return newest-first today and a pure function that
        // silently depends on its input order is a bug waiting for a caller.
        eligible.sort_by(|a, b| {
            b.created_at
                .partial_cmp(&a.created_at)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.run_id.cmp(&a.run_id))
        });

        // Inconclusive runs are dropped ENTIRELY (not counted, not
        // streak-breaking) — see `ci_is_inconclusive`.
        let decisive: Vec<&&CiRun> = eligible.iter().filter(|r| !ci_is_inconclusive(&r.conclusion)).collect();
        let n_cancelled = eligible.len() - decisive.len();
        if n_cancelled > 0 {
            suppressed.push(sup(
                DetectorKind::CiFailure,
                &format!("ci|{repo}|{workflow}|cancelled"),
                &format!(
                    "{n_cancelled} cancelled/skipped run(s) ignored — a cancelled run is not a \
                     fault and must not break or reset the failure streak"
                ),
            ));
        }

        let Some(newest) = decisive.first() else { continue };
        if !ci_is_failure(&newest.conclusion) {
            // Green right now. Nothing to file. Whether an EXISTING card should
            // hear about it is `ci_recovered`'s job, and the answer is a note,
            // never a close.
            continue;
        }

        // --- the streak -------------------------------------------------
        let mut streak: Vec<&&CiRun> = Vec::new();
        let mut last_success: Option<&&CiRun> = None;
        for r in &decisive {
            if ci_is_failure(&r.conclusion) {
                streak.push(r);
            } else {
                last_success = Some(r);
                break;
            }
        }
        let count = streak.len() as i64;
        if count < min_failures {
            suppressed.push(sup(
                DetectorKind::CiFailure,
                &format!("ci|{repo}|{workflow}"),
                &format!(
                    "{count} consecutive failure(s), below AMUX_CI_MIN_FAILURES={min_failures} — \
                     not filed yet; a single red run is not distinguishable from a flake"
                ),
            ));
            continue;
        }

        let oldest = streak.last().copied().expect("streak is non-empty: count >= min_failures >= 1");
        let anchor = match last_success {
            Some(_) => oldest.run_id.to_string(),
            // The window ran out before a green run did. Fixed anchor so the
            // key cannot drift as runs age out; the evidence says so out loud.
            None => "since-beyond-window".to_string(),
        };
        let signature = format!("ci|{repo}|{workflow}|{anchor}");
        let first_seen = oldest.created_at;
        let step = newest.failing_step.clone().unwrap_or_else(|| "<step not resolved>".into());

        let mut evidence: Vec<(String, String)> = vec![
            (
                "verdict".into(),
                format!(
                    "`{workflow}` has failed {count} consecutive time(s) on {repo}'s default \
                     branch and is failing NOW. Nothing else in amux reads GitHub, so a red CI \
                     is invisible unless a human happens to look — this card is the only place \
                     it shows up."
                ),
            ),
            ("repo".into(), repo.clone()),
            ("workflow".into(), workflow.clone()),
            ("consecutive_failures".into(), count.to_string()),
            ("first_failure_at".into(), format!("{} (run {})", ci_when(first_seen), oldest.run_id)),
            ("first_failure_url".into(), oldest.url.clone()),
            ("failing_for".into(), format!("{:.1}h", ((now - first_seen) / 3600.0).max(0.0))),
            ("latest_run".into(), format!("{} (run {})", ci_when(newest.created_at), newest.run_id)),
            ("latest_run_url".into(), newest.url.clone()),
            ("failing_step".into(), step),
            ("trigger".into(), format!("{} on {}", newest.event, newest.branch)),
        ];
        match last_success {
            Some(s) => evidence.push((
                "last_success".into(),
                format!("{} (run {}) — {}", ci_when(s.created_at), s.run_id, s.url),
            )),
            None => evidence.push((
                "last_success".into(),
                format!(
                    "NONE in the last {} runs fetched — the streak is AT LEAST {count} and \
                     started before the window, so `consecutive_failures` is a floor, not a \
                     count. Raise AMUX_CI_RUN_LIMIT to see further back.",
                    ci_run_limit()
                ),
            )),
        }

        out.push(Finding {
            kind: DetectorKind::CiFailure,
            signature,
            title: format!(
                "CI red: {workflow} — {count}x consecutive since {}",
                ci_when(first_seen)
            ),
            evidence,
            recheck: format!(
                "gh run list --repo {repo} --workflow '{workflow}' --limit 10; \
                 gh run view {} --repo {repo} --log-failed",
                newest.run_id
            ),
            owner: None,
            count: count as u64,
            last_ts: newest.created_at,
            parked_until: None,
        });
    }

    (out, suppressed)
}

/// Has the workflow behind an already-filed `ci|…` signature gone green?
///
/// Returns the sentence to LOG on the card. It never returns a status, because
/// there is no status this may set: "stopped failing is not fixed" applies with
/// full force here — a green run can mean the bug is fixed, or that someone
/// disabled the failing step, or that the workflow no longer runs the assertion
/// at all. The lane holding the card decides (ethos rule 8).
pub fn ci_recovered(runs: &[CiRun], signature: &str) -> Option<String> {
    let rest = signature.strip_prefix("ci|")?;
    let parts: Vec<&str> = rest.split('|').collect();
    if parts.len() < 3 {
        return None;
    }
    let (repo, workflow) = (parts[0], parts[1]);
    let mut mine: Vec<&CiRun> = runs
        .iter()
        .filter(|r| {
            r.repo == repo
                && ci_sanitize(&r.workflow) == workflow
                && r.on_default_branch
                && r.status == "completed"
                && r.event != "pull_request"
                && r.event != "pull_request_target"
                && !ci_is_inconclusive(&r.conclusion)
        })
        .collect();
    mine.sort_by(|a, b| b.created_at.partial_cmp(&a.created_at).unwrap_or(std::cmp::Ordering::Equal));
    let newest = mine.first()?;
    if ci_is_failure(&newest.conclusion) {
        return None;
    }
    Some(format!(
        "autofix: `{workflow}` is GREEN again as of {} (run {}, {}). STOPPED FAILING IS NOT \
         FIXED — this may be the fix, or the failing step may have been removed, disabled or \
         skipped. This card stays open and stays yours; say which it was and close it yourself.",
        ci_when(newest.created_at),
        newest.run_id,
        newest.url
    ))
}

/// Cached run list, so the 120s tick does not become 720 GitHub calls a day.
#[allow(clippy::type_complexity)]
fn ci_cache() -> &'static std::sync::RwLock<Option<(f64, Vec<CiRun>, Vec<Suppressed>)>> {
    static CELL: std::sync::OnceLock<std::sync::RwLock<Option<(f64, Vec<CiRun>, Vec<Suppressed>)>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::RwLock::new(None))
}

async fn gh(args: &[String], timeout_s: u64) -> Result<String, String> {
    let fut = tokio::process::Command::new("gh").args(args).output();
    let out = match tokio::time::timeout(std::time::Duration::from_secs(timeout_s), fut).await {
        Err(_) => return Err(format!("`gh {}` timed out after {timeout_s}s", args.join(" "))),
        Ok(Err(e)) => return Err(format!("cannot run `gh`: {e}")),
        Ok(Ok(o)) => o,
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("`gh {}` failed: {}", args.join(" "), truncate(err.trim(), 300)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn ci_parse_ts(s: &str) -> f64 {
    chrono::DateTime::parse_from_rfc3339(s).map(|d| d.timestamp() as f64).unwrap_or(0.0)
}

/// Ask GitHub. Impure by definition — kept in its own function so
/// [`ci_findings`] can stay a pure decision over a `Vec<CiRun>`.
///
/// Runs OUTSIDE the store lock (see [`autofix_tick`]): a network call while
/// holding the SQLite read lock would make every other request on the server
/// wait on GitHub.
async fn fetch_ci_runs(now: f64) -> (Vec<CiRun>, Vec<Suppressed>) {
    if let Ok(g) = ci_cache().read() {
        if let Some((at, runs, sup)) = g.as_ref() {
            if now - at < ci_poll_min() * 60.0 {
                return (runs.clone(), sup.clone());
            }
        }
    }

    let mut runs: Vec<CiRun> = Vec::new();
    let mut suppressed: Vec<Suppressed> = Vec::new();
    let timeout_s = ci_cmd_timeout_s();

    for repo in ci_repos() {
        let default_branch = match gh(
            &["api".into(), format!("repos/{repo}"), "--jq".into(), ".default_branch".into()],
            timeout_s,
        )
        .await
        {
            Ok(s) => s.trim().to_string(),
            Err(e) => {
                // BLINDNESS IS PUBLISHED. gh missing, unauthenticated, rate
                // limited or offline all land here, and all of them look
                // identical to "CI is fine" if we return quietly.
                suppressed.push(sup(
                    DetectorKind::CiFailure,
                    &format!("ci|{repo}"),
                    &format!("cannot read {repo} — detector is BLIND, not clear: {e}"),
                ));
                continue;
            }
        };

        let list = match gh(
            &[
                "run".into(), "list".into(),
                "--repo".into(), repo.clone(),
                "--limit".into(), ci_run_limit().to_string(),
                "--json".into(),
                "databaseId,conclusion,createdAt,event,headBranch,url,workflowName,status".into(),
            ],
            timeout_s,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                suppressed.push(sup(
                    DetectorKind::CiFailure,
                    &format!("ci|{repo}"),
                    &format!("cannot list runs for {repo} — detector is BLIND, not clear: {e}"),
                ));
                continue;
            }
        };
        let parsed: Vec<Value> = serde_json::from_str(&list).unwrap_or_default();
        if parsed.is_empty() {
            suppressed.push(sup(
                DetectorKind::CiFailure,
                &format!("ci|{repo}"),
                "gh returned no runs — either the repo has none or the response did not parse",
            ));
        }
        for v in parsed {
            let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
            let branch = s("headBranch");
            runs.push(CiRun {
                repo: repo.clone(),
                workflow: s("workflowName"),
                run_id: v.get("databaseId").and_then(Value::as_i64).unwrap_or(0),
                status: s("status"),
                conclusion: s("conclusion"),
                event: s("event"),
                on_default_branch: branch == default_branch,
                branch,
                created_at: ci_parse_ts(&s("createdAt")),
                url: s("url"),
                failing_step: None,
            });
        }
    }

    // Resolve the failing STEP, but only for the newest failing run of each
    // workflow — one extra API call each, versus one per run in the window.
    let mut want: BTreeMap<(String, String), (i64, f64)> = BTreeMap::new();
    for r in &runs {
        if !ci_is_failure(&r.conclusion) || !r.on_default_branch || r.status != "completed" {
            continue;
        }
        let key = (r.repo.clone(), ci_sanitize(&r.workflow));
        let e = want.entry(key).or_insert((r.run_id, r.created_at));
        if r.created_at > e.1 {
            *e = (r.run_id, r.created_at);
        }
    }
    for ((repo, _wf), (run_id, _)) in want {
        let jq = r#".jobs[] | select(.conclusion=="failure") | .name as $j | .steps[] | select(.conclusion=="failure") | "\($j) / \(.name)""#;
        let step = gh(
            &[
                "api".into(),
                format!("repos/{repo}/actions/runs/{run_id}/jobs"),
                "--jq".into(),
                jq.into(),
            ],
            timeout_s,
        )
        .await
        .ok()
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
        .filter(|s| !s.is_empty());
        if let Some(step) = step {
            for r in runs.iter_mut().filter(|r| r.run_id == run_id) {
                r.failing_step = Some(step.clone());
            }
        }
    }

    if let Ok(mut g) = ci_cache().write() {
        *g = Some((now, runs.clone(), suppressed.clone()));
    }
    (runs, suppressed)
}

fn sup(kind: DetectorKind, signature: &str, reason: &str) -> Suppressed {
    Suppressed { kind, signature: signature.to_string(), reason: reason.to_string() }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head}… [{} chars truncated]", s.chars().count() - n)
}

/// `autofix:<signature>` — the durable dedupe key AND the card's `source_ref`.
/// One string, two jobs, so "have we filed this?" and "which card is it?"
/// cannot answer differently.
pub fn idem_of(signature: &str) -> String {
    format!("autofix:{signature}")
}

/// The card body. Evidence first, in a fixed order, with the recheck command
/// last — a lane picking this up should be able to reproduce the finding
/// before reading a word of prose.
pub fn render_desc(f: &Finding) -> String {
    let mut s = String::new();
    s.push_str("Filed automatically by amux (runtime_jobs/autofix) — nobody has looked at this yet.\n");
    s.push_str("It is a REPORT, not a diagnosis: the evidence below is computed, the cause is not.\n\n");
    for (k, v) in &f.evidence {
        s.push_str(&format!("{k}: {v}\n"));
    }
    s.push_str(&format!("\ndetector: {}\nsignature: {}\n", f.kind.slug(), f.signature));
    s.push_str(&format!("\nre-check (run this first — if it is now clean, say so on the card):\n  {}\n", f.recheck));
    s.push_str(
        "\nIf this turns out not to be a defect, the fix is usually the INSTRUMENT: a refusal \
         wearing a 5xx, a threshold below its own baseline, or a probe that cannot express the \
         answer. Say which, and the detector stops filing it.\n",
    );
    s
}

/// True when this signature has already been filed — durably, across restarts.
fn already_filed(conn: &Connection, signature: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM session_events WHERE idem = ?1 LIMIT 1",
        rusqlite::params![idem_of(signature)],
        |_| Ok(()),
    )
    .is_ok()
}

/// The fault identity, with the OCCURRENCE-BATCH timestamp stripped.
///
/// `5xx|` and `latency|outlier|` signatures end in the newest offending row's
/// epoch, deliberately: it is what makes a discard STICK, because re-scanning
/// the same rows mints the same signature and `already_filed` blocks the refile
/// (AMUX-3581 -> 3589 -> 3591, one incident, three cards, each discard re-arming
/// the next). That fix is correct and stays.
///
/// It has an inverse failure nobody measured, and it is the one filling the
/// board. For a CONTINUOUSLY failing endpoint every scan sees a newer row, so
/// "a genuinely new 5xx" is every tick, forever: `POST /api/browser/start`
/// filed EIGHT cards in 50 minutes on 2026-08-24 — AMUX-3650, 3652, 3653, 3654,
/// 3655, 3656, 3660, 3661 — identical but for a bumped count in the title, 8 of
/// the 23 cards in one lane's queue. That is ethos rule 5 exactly: at volume it
/// stops being a set of tasks and becomes a log, and a queue that cannot be read
/// is one nobody reads, including for the cards that DO discriminate.
fn fault_identity(signature: &str) -> Option<&str> {
    if !(signature.starts_with("5xx|") || signature.starts_with("latency|outlier|")) {
        return None;
    }
    // A ROLLUP'S IDENTITY IS "THE SERVER WAS SLOW", NOT WHICH ENDPOINTS IT
    // CAUGHT (AMUX-3673).
    //
    // The rollup signature is `latency|outlier|ROLLUP|<comma-joined targets>`,
    // and that target list varies with whichever endpoints happened to be over
    // threshold in that window. Measured 2026-08-24: SIX rollup cards, all
    // saying the server was slow, and AMUX-3651 vs AMUX-3673 differ by ONE
    // element — `/api/board/{id}` against `/api/browser/action`.
    //
    // The rollup exists because per-endpoint cards "each name an endpoint that
    // is probably not the fault" (AMUX-2814). It fixed that fan-out WITHIN a
    // window and reproduced it ACROSS windows, by putting the varying set into
    // the identity. The endpoint list is evidence; it belongs in the card body,
    // which is where the rollup already puts it.
    //
    // Two genuinely separate slowdowns still get two cards: only an OPEN card
    // suppresses, so once the first is judged the next occurrence files.
    if let Some(i) = signature.find("|ROLLUP|") {
        return Some(&signature[..i + "|ROLLUP".len()]);
    }
    // Only a trailing field that is a BARE EPOCH is stripped, same rule as
    // `invariant_signature_parts`: a target whose last `|`-part is numeric is
    // not a thing today, and if it becomes one this merges two faults rather
    // than splitting every one.
    match signature.rsplit_once('|') {
        Some((head, tail)) if !head.is_empty() && tail.parse::<i64>().is_ok() => Some(head),
        _ => None,
    }
}

/// Is there already an OPEN card for this exact fault? (AMUX-3667 follow-up)
///
/// Deliberately scoped to cards a lane can still act on. A `done`, `verified`
/// or `discarded` card does NOT suppress, which is what preserves the property
/// the timestamp was added for: judging a report and discarding it leaves the
/// signature judged via `already_filed`, and a genuinely new occurrence after
/// that still files.
///
/// So the two rules divide cleanly: `already_filed` answers "have I filed THIS
/// batch", and this answers "is the same fault already sitting in someone's
/// queue". Only the second can see that eight cards are one fault.
fn open_card_for_fault(conn: &Connection, signature: &str) -> Option<String> {
    let ident = fault_identity(signature)?;
    conn.query_row(
        "SELECT id FROM issues \
          WHERE source_ref LIKE ?1 \
            AND status NOT IN ('done','verified','discarded') \
            AND archived = 0 AND deleted IS NULL \
          ORDER BY id DESC LIMIT 1",
        rusqlite::params![format!("autofix:{ident}|%")],
        |r| r.get::<_, String>(0),
    )
    .ok()
}

/// One tick: run every detector, file what is new, note what went quiet.
///
/// Returns the report rather than logging it, so the debug surface shows
/// exactly what the tick decided — including the suppressions, which are the
/// half that is normally invisible.
/// The tick WITHOUT any CI input. **Reaches no network.**
///
/// This is the entry point for anything that is not the periodic job — tests,
/// one-shot invocations, anything holding a synthetic database. The CI detector
/// reads GitHub, and a function that quietly makes an HTTP request because you
/// asked it to look at a temp-dir SQLite file is a trap: it made a sibling
/// integration test file four cards about the REAL repo's CI into its fixture
/// database and fail on a count of 5-vs-1. The fetch belongs to the loop that
/// polls, not to the function everyone calls.
pub async fn autofix_tick(state: &AppState, home: &std::path::Path) -> AutofixReport {
    autofix_tick_with_ci(state, home, &[], Vec::new()).await
}

/// The tick, given whatever CI runs the caller fetched.
///
/// `ci_runs` empty with `ci_sup` empty means "nobody looked", and that is
/// recorded as a suppression rather than passing for "CI is fine" — the same
/// rule the fetch itself follows when `gh` is missing.
pub async fn autofix_tick_with_ci(
    state: &AppState,
    home: &std::path::Path,
    ci_runs: &[CiRun],
    ci_sup: Vec<Suppressed>,
) -> AutofixReport {
    let t0 = std::time::Instant::now();
    let now = unix_now();
    let mut rep = AutofixReport { at: now, ..Default::default() };

    let mut ci_sup = ci_sup;
    if ci_runs.is_empty() && ci_sup.is_empty() {
        ci_sup.push(sup(
            DetectorKind::CiFailure,
            "ci|<no input>",
            "this tick was invoked without a CI fetch, so the CI detector read nothing — \
             absence of findings here is absence of DATA, not evidence that CI is green",
        ));
    }

    // LANE REACHABILITY, RESOLVED BEFORE THE SYNC PASS (AMUX-3696).
    //
    // `detect_silent` needs to know whether a lane with a queued message CAN
    // receive one, because "will never drain" and "drains at the next turn
    // boundary" are different facts and want different deadlines. The predicate
    // is `lane_block_reason`, which is async (it asks tmux whether the lane is
    // running), and the detector pass is sync — so it is resolved here, the
    // same shape the disk detector below already uses.
    //
    // Deliberately the SHARED predicate rather than a re-derivation. The drain
    // loop and /api/debug/steering both call it, and a detector that computed
    // reachability its own way could report a lane unreachable that the loop
    // will happily deliver to (ethos rule 1: a view must share the predicate of
    // the mechanism it claims to describe).
    let steer_blocked: BTreeMap<String, Option<String>> = {
        let sessions: Vec<String> = state
            .store
            .read()
            .ok()
            .and_then(|c| {
                c.prepare("SELECT DISTINCT session FROM steering_queue")
                    .ok()
                    .and_then(|mut st| {
                        st.query_map([], |r| r.get::<_, String>(0))
                            .ok()
                            .map(|rows| rows.flatten().collect())
                    })
            })
            .unwrap_or_default();
        let mut m = BTreeMap::new();
        for s in sessions {
            let b = crate::api::session_verbs::lane_block_reason(&s).await;
            m.insert(s, b.map(str::to_string));
        }
        m
    };

    // DISK DETECTION RUNS OFF THE RUNTIME, AND OUTSIDE THE STORE LOCK (AF-97 —
    // the deeper exit AMUX-35 named and did not take).
    //
    // `detect_disk` is the only detector that shells out: a bounded `du -skx`
    // per candidate path, and `tmutil listlocalsnapshots /`. Every other
    // detector reads SQLite. Run inline it did two harmful things at once, and
    // the second reaches the whole fleet:
    //
    //   - it parked a tokio worker for the duration. `du_one` polls with
    //     `std::thread::sleep(50ms)` up to `AMUX_DISK_DU_TOTAL_TIMEOUT_S`
    //     (default 15s), which blocks the THREAD, not the task. Unbounded, this
    //     is precisely what took the server silent on AMUX-35: "each tick parked
    //     another worker until none were left to poll the accept loop". The
    //     budgets bounded the damage; they did not move it off the runtime.
    //
    //   - it held `state.store.read()` for that whole time, because the detector
    //     loop runs inside the lock scope — and `detect_disk` is one of the three
    //     detectors that never touches `conn`. Writers take BEGIN IMMEDIATE, so a
    //     15s disk walk was 15s of stalled writes for every lane, every 120s.
    //
    // `fetch_ci_runs` was hoisted out of this scope for exactly this reason
    // ("so the tick never touches the store lock and GitHub in the same breath").
    // The subprocess detector is the same hazard and was left behind.
    let (mut disk_f, mut disk_s) = {
        let home_owned = home.to_path_buf();
        match tokio::task::spawn_blocking(move || detect_disk(now, &home_owned)).await {
            Ok(v) => v,
            // A panicked or cancelled blocking task must not read as "disk is
            // fine". A detector that silently reports nothing is the exact shape
            // this file exists to catch, so the failure is filed as a suppression
            // and shows up in the report's `suppressed` list.
            Err(e) => (
                Vec::new(),
                vec![sup(
                    DetectorKind::DiskPressure,
                    "disk|detector-did-not-run",
                    &format!(
                        "the disk detector did not run ({e}) — this is absence of DATA, \
                         not evidence that the disk is healthy"
                    ),
                )],
            ),
        }
    };

    // Connector token rot: async provider probes, so computed OFF the store
    // lock like disk/CI (the rollup carries its own 300s cache, so a 120s tick
    // costs one probe per account per 300s, not per tick).
    let (mut connector_f, mut connector_s) = connector_auth_probe(now, home).await;

    let (findings, suppressed, on) = {
        let conn = match state.store.read() {
            Ok(c) => c,
            Err(e) => {
                rep.errors.push(format!("store read: {e}"));
                rep.took_ms = t0.elapsed().as_secs_f64() * 1000.0;
                *last_report_cell().write().unwrap() = Some(rep.clone());
                return rep;
            }
        };
        let on = enabled(&conn);
        let mut findings = Vec::new();
        let mut suppressed = ci_sup;
        for kind in DetectorKind::all() {
            let (f, s) = match kind {
                DetectorKind::Http5xx => detect_5xx(&conn, now),
                DetectorKind::Latency => detect_latency(&conn, now),
                DetectorKind::DeadRoute => detect_dead_routes(&conn, now),
                DetectorKind::SilentSubsystem => detect_silent(&conn, now, &steer_blocked),
                DetectorKind::InvariantBreach => detect_invariants(&conn, now),
                DetectorKind::BuildDeploy => detect_build(&conn, now, home),
                // Computed above, off-runtime and outside this lock (AF-97).
                DetectorKind::DiskPressure => {
                    (std::mem::take(&mut disk_f), std::mem::take(&mut disk_s))
                }
                DetectorKind::CiFailure => ci_findings(ci_runs, now),
                DetectorKind::FdPressure => detect_fd(now),
                // Computed above, off-runtime and outside this lock, like disk.
                DetectorKind::ConnectorAuth => {
                    (std::mem::take(&mut connector_f), std::mem::take(&mut connector_s))
                }
            };
            findings.extend(f);
            suppressed.extend(s);
        }
        (findings, suppressed, on)
    };
    rep.enabled = on;

    let ignore = ignore_list();
    let mut to_file: Vec<Finding> = Vec::new();
    for f in findings {
        rep.signatures_seen.push((f.kind.slug().to_string(), f.signature.clone()));
        if let Some(pat) = ignore.iter().find(|p| f.signature.contains(p.as_str())) {
            rep.suppressed.push((
                f.kind.slug().into(),
                f.signature.clone(),
                format!("matches AMUX_AUTOFIX_IGNORE entry {pat:?}"),
            ));
            continue;
        }
        if !on {
            // THE TOGGLE DOES NOT BLIND THE WATCHER. Detection ran, the
            // finding is published; only the board write is skipped. Otherwise
            // the window somebody switched it off is the one window with no
            // evidence in it.
            rep.suppressed.push((
                f.kind.slug().into(),
                f.signature.clone(),
                "autofix_enabled=0 (Settings toggle) — detected, not filed".into(),
            ));
            continue;
        }
        // ONE OPEN CARD PER FAULT (AMUX-3667 follow-up).
        //
        // `5xx|`/`latency|outlier|` signatures carry the occurrence batch's
        // timestamp so a DISCARD sticks (AMUX-3581 -> 3589 -> 3591). The
        // inverse, unmeasured until now: a continuously-failing endpoint has a
        // newer row every scan, so every tick is "a genuinely new 5xx" and
        // files. `POST /api/browser/start` minted EIGHT cards in 50 minutes on
        // 2026-08-24 (AMUX-3650, 3652-3656, 3660, 3661) — identical but for the
        // count in the title, and 8 of the 23 cards in one lane's queue.
        //
        // Suppressed, never silent: the reason NAMES the card that already
        // holds the fault, so a reader lands there instead of wondering where
        // the report went. Only open cards suppress, which is what keeps the
        // discard property — a judged-and-discarded card lets the next genuine
        // occurrence through.
        {
            // ORDER MATTERS, and getting it wrong shadowed a real signal:
            // running this ahead of `already_filed` made every repeat of an
            // open fault report as "suppressed" instead of "already_filed", and
            // `a_restart_does_not_refile` caught it. The two answer different
            // questions and both are worth keeping — "I already filed THIS
            // batch" is precise and cheap, "the fault is already in someone's
            // queue" is the broad one. Ask the precise one first, and let
            // file_finding own it.
            let existing = state.store.read().ok().and_then(|c| {
                if already_filed(&c, &f.signature) {
                    return None;
                }
                open_card_for_fault(&c, &f.signature)
            });
            if let Some(card) = existing {
                // NAME HOW STALE THE SUPPRESSING CARD IS (AMUX-3774).
                //
                // This used to assert "Its count is what moves; a second card
                // would carry no new information." Nothing implemented that.
                // This block pushes a report row and `continue`s; it never
                // touches the card. Measured 2026-08-26: AMUX-3651 last updated
                // 08-24 17:35 while the rollup suppressed against it every ~2
                // minutes for two days, including through a live six-family
                // stall. The count was not moving, so a second card WOULD have
                // carried new information — that the fault recurred today.
                //
                // ethos rule 6: grep for what the docstring promises, and either
                // implement it or delete the claim. Deleted, and replaced with a
                // fact this code can actually establish.
                //
                // THE DEEPER HAZARD the staleness exposes: only OPEN cards
                // suppress, and `backlog` is open. A fault card parked in
                // backlog is a permanent mute on its whole class, with no
                // expiry. `filed: []` then reads identically for "nothing is
                // wrong" and "everything is muted" — the idiom ethos.md names.
                let stale_d = state
                    .store
                    .read()
                    .ok()
                    .and_then(|c| {
                        c.query_row(
                            "SELECT COALESCE(updated, created) FROM issues WHERE id=?1",
                            rusqlite::params![&card],
                            |r| r.get::<_, f64>(0),
                        )
                        .ok()
                    })
                    .map(|t| (crate::config::now_f64() - t) / 86400.0);
                let age = match stale_d {
                    Some(d) => format!(" It has not been touched in {d:.1} day(s)."),
                    None => String::new(),
                };
                // SELF-ANNOUNCING once the mute is old enough to be a mute
                // rather than a dedupe. A suppression is normal; suppressing
                // against a card nobody has touched for days is the whole class
                // going dark, and it should not need someone to open the debug
                // surface to find out.
                // THE MUTE EXPIRES (AMUX-3774, criterion 2: "a deliberately
                // parked card still suppresses for a bounded, stated period").
                // Past the bound this stops suppressing and the fault files
                // again, because the alternative is that whoever parks a card
                // first owns silence on its whole class indefinitely.
                //
                // Deliberately NOT a bump of the card's `updated`, which was the
                // tempting fix: it would make a parked card look actively worked
                // and defeat every staleness sweep reading that field. Letting
                // the fault through says the true thing — nobody has touched
                // this, so it is loud again.
                if stale_d.unwrap_or(0.0) >= mute_expire_days() {
                    tracing::warn!(
                        card = %card,
                        detector = f.kind.slug(),
                        signature = %f.signature,
                        stale_days = stale_d.unwrap_or(0.0),
                        expire_days = mute_expire_days(),
                        "autofix_mute_expired: this fault has been suppressed against an \
                         untouched card past the bound, so it is being FILED again. The new \
                         card is not a duplicate — it is evidence the fault recurred while \
                         the old one sat parked (AMUX-3774)."
                    );
                    rep.suppressed.push((
                        f.kind.slug().into(),
                        format!("{}|mute-expired", f.signature),
                        format!(
                            "{card} held this fault for {:.1} day(s) without being touched, \
                             past AMUX_AUTOFIX_MUTE_EXPIRE_DAYS ({:.1}). NOT suppressed: \
                             re-filing, because a parked card must not mute its class forever.",
                            stale_d.unwrap_or(0.0),
                            mute_expire_days()
                        ),
                    ));
                    // Fall through to file. The decision is recorded above so
                    // "why did this re-file" is answerable without a bisect.
                } else {
                if stale_d.unwrap_or(0.0) >= mute_warn_days() {
                    tracing::warn!(
                        card = %card,
                        detector = f.kind.slug(),
                        signature = %f.signature,
                        stale_days = stale_d.unwrap_or(0.0),
                        "autofix_mute: this fault keeps recurring and is suppressed against a \
                         card nobody has touched. Only OPEN cards suppress and `backlog` is \
                         open, so a parked card mutes its entire class with no expiry \
                         (AMUX-3774). Judge or discard the card to let occurrences through."
                    );
                }
                rep.suppressed.push((
                    f.kind.slug().into(),
                    f.signature.clone(),
                    format!(
                        "{card} is open for this same fault — one card per fault, not one \
                         per scan (AMUX-3667).{age} NOTE: suppressing does NOT bump that \
                         card, so its age is the only signal that this recurred. It stops \
                         suppressing at {:.1} day(s).",
                        mute_expire_days()
                    ),
                ));
                continue;
                }
            }
        }
        to_file.push(f);
    }
    for s in suppressed {
        rep.suppressed.push((s.kind.slug().into(), s.signature, s.reason));
    }

    for f in to_file {
        match file_finding(state, &f).await {
            Ok(Some(card)) => {
                // LINK THE INCIDENT TO THE CARD IT MINTED (AMUX-3574).
                //
                // `_amux_invariant_incident.board_issue` has existed all along,
                // is SELECTed by `live_incidents`, and is rendered on
                // /api/debug/invariants — and nothing ever wrote it. Measured:
                // 0 of 224 incidents carry a link. A column the schema
                // promises, the debug surface displays, and no code populates
                // is ethos rule 6: grep for what the docstring promises.
                //
                // The cost is concrete and repeated. Three cards were
                // auto-filed to this lane in one evening (AMUX-3572, 3574,
                // 3575) describing conditions that had ALREADY self-healed,
                // each costing an investigation to establish that nothing was
                // wrong. The incident knew it had resolved; it just had no way
                // to say which card to tell.
                //
                // Written here rather than inside file_finding because this is
                // where the card id and the signature are both in hand, and
                // the signature is the only thing that maps back to
                // (invariant_id, entity_key).
                if let Some((inv, entity)) = invariant_signature_parts(&f.signature) {
                    let (c, i, e) = (card.clone(), inv, entity);
                    let _ = state
                        .store
                        .write_async(move |conn| {
                            link_incident_to_card(conn, &c, &i, &e)?;
                            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                        })
                        .await;
                }
                if let Some(t) = f.parked_until {
                    // WARN, not info: this is rare by construction, and the
                    // failure it guards against is the counter going quiet.
                    tracing::warn!(
                        card = %card, signature = %f.signature, heals_at = t,
                        "autofix filed a PARKED card — dwell window, no action closes it \
                         before {} (AMUX-3645)",
                        rl::local_when(t)
                    );
                    rep.parked.push((card.clone(), f.signature.clone(), rl::local_when(t)));
                }
                rep.filed.push((card, f.signature.clone()));
            }
            Ok(None) => rep.already_filed.push(f.signature.clone()),
            Err(e) => rep.errors.push(format!("{}: {e}", f.signature)),
        }
    }

    // ESCALATE A DEPLOY WORKFLOW PAST THE CARD (AMUX-2780).
    //
    // A card was not enough and we know it empirically: the cloud deploy has a
    // card (32x consecutive) sitting unread in `todo` while the outage runs into
    // its fourth day. "Merged" and "deployed" diverging is the one CI failure
    // whose cost compounds silently — every commit after the first looks shipped
    // and is not.
    //
    // Routed through the OWNER ALERT, which is Ethan's own configured channel
    // set, so this does not decide how to reach him — he already did, and
    // `urgent_alert_decision` supplies the 60s dedupe and the storm mute.
    //
    // TWO GUARDS AGAINST BECOMING THE NAG THIS REPO KEEPS FILING CARDS ABOUT:
    //   - a HIGH bar, both conditions: >= AMUX_CI_DEPLOY_ALERT_STREAK failures
    //     (10) AND >= AMUX_CI_DEPLOY_ALERT_H hours red (24). Ten pushes in one
    //     busy hour is not three days of undeployed work; one push a day for a
    //     week is, and only the pair catches both.
    //   - ONE PER TICK, and a durable per-day idem. Turning this on is a
    //     migration event, not a steady state: at the moment it ships, several
    //     workflows are already past the bar, and "alert on everything newly
    //     visible" is how the owner digest emitted 92 cards in one SMS. The cap
    //     means the backlog discharges over hours, in priority order, instead of
    //     as a burst.
    for (repo, workflow, streak, hours, url) in ci_deploy_escalations(ci_runs, now) {
        let day = (now / 86_400.0) as i64;
        let idem = format!("ci-deploy-escalation:{repo}:{workflow}:{day}");
        let already = {
            match state.store.read() {
                Ok(conn) => conn
                    .query_row(
                        "SELECT 1 FROM session_events WHERE idem=?1 LIMIT 1",
                        rusqlite::params![idem],
                        |_| Ok(()),
                    )
                    .is_ok(),
                Err(_) => true, // cannot check means do not send
            }
        };
        if already {
            continue;
        }
        let msg = format!(
            "DEPLOY RED {streak}x for {hours:.0}h — '{workflow}' ({repo}). Merged work is NOT \
             reaching production; every commit since the first failure looks shipped and is not. \
             Newest run: {url}"
        );
        crate::api::session_verbs::emit_event(
            state,
            "",
            "ci.deploy_escalation",
            Some(serde_json::json!({
                "repo": repo, "workflow": workflow, "streak": streak,
                "hours_red": hours as i64, "url": url, "message": msg,
            })),
            Some(idem),
            "autofix",
        )
        .await;
        rep.errors.push(format!("ESCALATED: {msg}"));
        tracing::error!(repo = %repo, workflow = %workflow, streak, hours, "deploy red — escalated past the card");
        break; // ONE per tick. See the cap note above.
    }

    match note_resolved_incidents(state).await {
        Ok(v) => rep.resolved_noted = v,
        Err(e) => rep.errors.push(format!("note_resolved_incidents: {e}")),
    }
    match note_quiet_signatures(state, now, ci_runs).await {
        Ok(v) => rep.quiet_noted = v,
        Err(e) => rep.errors.push(format!("quiet sweep: {e}")),
    }

    rep.took_ms = t0.elapsed().as_secs_f64() * 1000.0;
    if let Ok(mut g) = last_report_cell().write() {
        *g = Some(rep.clone());
    }
    rep
}

/// File one finding. Returns the new card id, or None when the signature was
/// already filed (the durable-dedupe path, which is the common case).
///
/// The dedupe row and the card are written in the SAME transaction as the
/// card: if the insert fails, no idem row is left claiming a card that does
/// not exist, and if the idem row exists the card does too.
/// Split an invariant finding's signature into `(invariant_id, entity_key)`.
///
/// The shape is `invariant|<invariant_id>|<entity_key>`, and the entity may
/// itself contain `|`, so the split is bounded to 3 parts rather than greedy.
/// Returns None for every other detector's signature, which is what keeps the
/// incident link scoped to findings that actually have an incident.
///
/// A rollup finding names several entities and its signature is not this shape,
/// so it links nothing — correctly: there is no single incident to point at,
/// and guessing one would attach the card to an arbitrary member.
/// Attach a freshly-filed card to the incident row it came from, and say so
/// when it cannot. Returns the number of rows linked (0 or 1).
///
/// This is what lets `note_resolved_incidents` tell a card that the condition
/// behind it is gone (AMUX-3572). The link is written from the SIGNATURE,
/// because that is the only thing in hand at the filing site that maps back to
/// `(invariant_id, entity_key)`.
///
/// A FLEET-WIDE INVARIANT STORES `entity_key=''` AND SIGNS ITSELF `fleet`
/// (AMUX-3664). `detect_invariants` computes `sig_entity = if entity.is_empty()
/// { "fleet" }` for the title and the signature, so for every fleet-scoped
/// invariant the exact-match UPDATE looked for `entity_key='fleet'`, matched
/// ZERO rows, and left `board_issue` empty — silently, because a 0-row UPDATE
/// is not an error.
///
/// A display substitution leaked into a storage key, and the cost is that the
/// resolved-incident notice was INERT for the entire fleet-wide half: it joins
/// on `board_issue != ''`. Measured 2026-08-24: **10 fleet-wide incidents, 0
/// with a card link**, including `hooks.shared_guard_matches_committed` — 359
/// occurrences, resolved at 12:36, its card never told. The notice was
/// validated the same morning against two entity-keyed specimens, which is
/// exactly the sample that cannot see this.
///
/// The fallback is a RETRY rather than a widened `WHERE`, so an exact match
/// always wins: an invariant whose real entity is literally named "fleet" keeps
/// its own row, and only a genuine miss falls through. The signature spelling
/// is deliberately NOT changed — it is a live dedupe key, and moving it re-files
/// every open invariant card (measured the hard way, AMUX-3633).
fn link_incident_to_card(
    conn: &Connection,
    card: &str,
    invariant: &str,
    entity: &str,
) -> rusqlite::Result<usize> {
    let n = conn.execute(
        "UPDATE _amux_invariant_incident SET board_issue=?1 \
         WHERE invariant_id=?2 AND entity_key=?3",
        rusqlite::params![card, invariant, entity],
    )?;
    let n = if n == 0 && entity == "fleet" {
        conn.execute(
            "UPDATE _amux_invariant_incident SET board_issue=?1 \
             WHERE invariant_id=?2 AND entity_key=''",
            rusqlite::params![card, invariant],
        )?
    } else {
        n
    };
    // A 0-ROW UPDATE IS NOT AN ERROR, WHICH IS HOW THIS LASTED FOUR DAYS.
    // Nothing recorded that a card had been minted with no incident to attach
    // it to, so the only way to find it was to notice a resolved incident that
    // never told its card — to look for the absence of a message. Say it out
    // loud instead, or the next spelling drift is exactly as quiet as this one.
    if n == 0 {
        tracing::warn!(
            card = %card, invariant = %invariant, entity = %entity,
            "autofix filed a card but could not link it to any incident row — a \
             resolved incident will never be able to tell this card (AMUX-3664)"
        );
    }
    Ok(n)
}

fn invariant_signature_parts(sig: &str) -> Option<(String, String)> {
    let mut it = sig.splitn(3, '|');
    match (it.next(), it.next(), it.next()) {
        (Some("invariant"), Some(inv), Some(rest)) if !inv.is_empty() && !rest.is_empty() => {
            // STRIP THE EPISODE SUFFIX (AMUX-3633). The signature gained a
            // trailing `|<first_seen epoch>` so a judged breach stays judged
            // for as long as it is the SAME unbroken failure. The entity may
            // itself contain `|` (route invariants name `GET /api/x|y`), so the
            // split above is bounded at 3 and the timestamp would otherwise be
            // absorbed into the entity — silently breaking the incident link
            // this function exists to make, for every invariant card.
            //
            // Only a trailing field that is a BARE EPOCH is stripped. An entity
            // whose last `|`-part happens to be numeric is not a thing today,
            // and if it ever is, this mis-links one card rather than dropping
            // the timestamp into every one.
            let ent = match rest.rsplit_once('|') {
                Some((head, tail))
                    if !head.is_empty() && tail.parse::<i64>().is_ok() => head,
                _ => rest,
            };
            (!ent.is_empty()).then(|| (inv.to_string(), ent.to_string()))
        }
        _ => None,
    }
}

/// AEAB-59. A suppressed re-finding whose TITLE has changed is NOT a duplicate —
/// it is news about the same condition, and dropping it is how an alarm becomes
/// a stopped clock.
///
/// Measured 2026-08-25: AMUX-30 still read "disk: 4.2 GB free, below the 50 GB
/// floor" while the volume was at 0.94 GB. The detector had found the condition
/// every two minutes for five days and every one of those findings was
/// discarded, because the idem index says one card per condition forever. That
/// is right for avoiding duplicate cards and wrong for a card whose CONTENT is a
/// measurement: the condition had not changed, the number had, by 4.5x, and the
/// number is the whole point. A stopped clock reading "fine" is worse than no
/// clock, because the board is read first and trusted.
///
/// Two gates, and they are what keep this from becoming the OTHER failure —
/// rule 5, a card that accumulates until nothing about it is done or not-done:
///   * the TITLE must have changed. These detectors put the measurement in the
///     title, so an unchanged title means the number has not moved enough to
///     render differently. A static condition writes nothing, forever.
///   * and not more often than `AMUX_AUTOFIX_REFRESH_MIN_SECS` (default 6h),
///     so a value oscillating around a rounding boundary cannot turn the card
///     into a journal.
///
/// The desc block is REPLACED, never appended, for the same reason: the card
/// stays one page however long the condition lasts.
fn refresh_min_secs() -> i64 {
    std::env::var("AMUX_AUTOFIX_REFRESH_MIN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6 * 3600)
}

const REFRESH_MARK: &str = "\n\n<!-- autofix:current -->\n";

/// Strip any previous refresh block, so the replacement cannot stack.
pub(crate) fn desc_without_refresh(desc: &str) -> &str {
    match desc.find(REFRESH_MARK) {
        Some(i) => &desc[..i],
        None => desc,
    }
}

/// Should the existing card be refreshed? Split out so both gates are testable
/// without a store or a clock — the whole defect was a decision nobody could see.
pub(crate) fn should_refresh(
    old_title: &str,
    new_title: &str,
    updated_at: i64,
    now: i64,
    min_secs: i64,
) -> bool {
    old_title != new_title && now - updated_at >= min_secs
}

async fn file_finding(state: &AppState, f: &Finding) -> anyhow::Result<Option<String>> {
    // PER-TENANT CLOUD: do not drop the server's own health/ops findings onto a
    // customer's board (AMUX-3014). Off by env in the cloud image; on locally
    // where the board is the ops board. Detection still runs — only the card
    // FILING is suppressed — and the operator reads /health + /api/health/
    // invariants for cloud health.
    if !ops_health_cards_enabled() {
        return Ok(None);
    }
    {
        let conn = state.store.read()?;
        if already_filed(&conn, &f.signature) {
            // AEAB-59: do not just drop it — if the MEASUREMENT moved, say so on
            // the card that already exists. Read-only here; the write is below.
            let fresh_title = truncate(&f.title, 160);
            let existing: Option<(String, String, String, i64)> = conn
                .query_row(
                    "SELECT id, title, COALESCE(desc,''), COALESCE(updated_at, created_at) \
                       FROM issues WHERE source_ref = ?1 AND archived = 0 LIMIT 1",
                    rusqlite::params![format!("autofix:{}", f.signature)],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .ok();
            let now_s = crate::api::reclaim::now_secs();
            let Some((card_id, old_title, old_desc, updated_at)) = existing else {
                return Ok(None);
            };
            if !should_refresh(&old_title, &fresh_title, updated_at, now_s, refresh_min_secs()) {
                return Ok(None);
            }
            drop(conn);
            let body = format!(
                "{}{}The measurement moved. Re-read {} — previously titled:\n  {}\n\n{}",
                desc_without_refresh(&old_desc),
                REFRESH_MARK,
                chrono::Utc::now().format("%Y-%m-%d %H:%M UTC"),
                old_title,
                render_desc(f)
            );
            let (cid, ttl) = (card_id.clone(), fresh_title.clone());
            let _ = state.store.write(move |c| {
                c.execute(
                    "UPDATE issues SET title = ?2, desc = ?3, updated_at = ?4 WHERE id = ?1",
                    rusqlite::params![cid, ttl, body, now_s],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            });
            tracing::info!(
                card = %card_id, detector = %f.signature,
                "autofix refreshed a standing card — the measurement moved (AEAB-59)"
            );
            return Ok(None);
        }
    }
    let owner = f.owner.clone().unwrap_or_else(fixer_session);
    let owner = if owner.trim().is_empty() { None } else { Some(owner) };
    let title = truncate(&f.title, 160);
    let desc = render_desc(f);
    let item_type = f.kind.item_type();
    let signature = f.signature.clone();
    let idem = idem_of(&signature);
    let kind_slug = f.kind.slug().to_string();
    let now_s = unix_now() as i64;
    let parked_until = f.parked_until;

    // The new id has to come back OUT of the writer closure, which runs on the
    // single writer thread — so it cannot ride a thread_local, and widening
    // `write_async`'s return type would touch every writer in the crate for
    // one string.
    let created_id: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
    let sink = created_id.clone();
    state
        .store
        .write_async(move |conn| {
            // Re-check inside the writer: two ticks cannot race a double file.
            if conn
                .query_row("SELECT 1 FROM session_events WHERE idem=?1 LIMIT 1", rusqlite::params![idem], |_| Ok(()))
                .is_ok()
            {
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            }
            let new = bs::NewIssue {
                title: title.clone(),
                desc: desc.clone(),
                // PARKED, NOT QUEUED (AMUX-3645). A dwell-window fault has no
                // action that closes it before its clock runs out, so filing it
                // `todo` hands a lane work it cannot do — and board_drive picks
                // from `todo`, so the cost is a real pickup and a real
                // investigation every time. `backlog` plus the non-empty
                // `source_ref` set below is the shape the drive already reads as
                // "parked on a live trigger" (board_drive.rs:1614): out of the
                // untracked-work nag, out of rot, still on the board.
                //
                // Deliberately NOT `tripwire`. That type is for a card ARMED
                // ahead of an event; this one is filed after the event, and its
                // exit is a date rather than a firing. Retyping it would trade
                // one gate that does not fit for another.
                status: if parked_until.is_some() { "backlog".into() } else { "todo".into() },
                session: owner.clone(),
                item_type: item_type.into(),
                creator: "autofix".into(),
                // owner_type=agent is what makes board_drive eligible to pick
                // it up. A human-owned card would sit forever: the drive
                // deliberately never touches a person's in-flight work.
                owner_type: "agent".into(),
                due: None,
                due_time: None,
                reviewer: None,
                shepherd: None,
                gate: vec![],
                depends_on: vec![],
                tags: vec!["autofix".into(), format!("detector:{kind_slug}")],
            };
            let row = bs::create_issue(conn, &new, now_s)?;
            conn.execute(
                "UPDATE issues SET source_ref = ?1 WHERE id = ?2",
                rusqlite::params![format!("autofix:{signature}"), row.id],
            )?;
            // Stamp the park (AMUX-3645). `last_verified_at` is what makes a
            // trigger nobody re-checks DETECTABLE rather than a card sleeping
            // forever — parking without it buys silence with no expiry, which
            // is the failure the backlog convention exists to prevent.
            if parked_until.is_some() {
                conn.execute(
                    "UPDATE issues SET last_verified_at = ?1 WHERE id = ?2",
                    rusqlite::params![now_s, row.id],
                )?;
            }
            conn.execute(
                "INSERT OR IGNORE INTO session_events (ts, session, type, data, idem, source) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    now_s as f64,
                    owner.clone().unwrap_or_default(),
                    "autofix.filed",
                    json!({"card": row.id, "signature": signature, "detector": kind_slug}).to_string(),
                    idem,
                    "autofix"
                ],
            )?;
            let event = crate::db::PendingEvent {
                entity_type: amux_core::revision::EntityType::Task,
                entity_id: row.id.clone(),
                mutation: amux_core::revision::MutationKind::Created,
                payload: Some(row.snapshot()),
            };
            if let Ok(mut g) = sink.lock() {
                *g = Some(row.id.clone());
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![event] })
        })
        .await?;
    let created = created_id.lock().ok().and_then(|g| g.clone());
    Ok(created)
}

///
/// Noted, never acted on — same rule as `note_quiet_signatures` above, and for
/// the same reason: the card belongs to whoever holds it, and closing it is
/// their judgment (ethos rule 8). A resolved incident is much stronger evidence
/// than silence, but "the condition is gone" and "the work on this card is
/// done" are different claims, and only the holder can make the second.
///
/// The cost this pays for is measured. Three cards were auto-filed to one lane
/// in a single evening (AMUX-3572, AMUX-3574, AMUX-3575) describing conditions
/// that had already healed, and each cost a full investigation to establish
/// that nothing was wrong. In every case the incident row KNEW — `resolved_at`
/// was set, sometimes a minute before the pickup notice went out — and had no
/// way to say which card to tell, because `board_issue` was never written.
///
/// Exactly-once via the same `session_events.idem` mechanism, so a resolved
/// incident cannot re-nag on every tick.
async fn note_resolved_incidents(state: &AppState) -> anyhow::Result<Vec<(String, String)>> {
    struct Note {
        card: String,
        what: String,
        line: String,
        idem: String,
    }
    let candidates: Vec<Note> = {
        let conn = state.store.read()?;
        let mut stmt = conn.prepare(
            "SELECT i.board_issue, i.invariant_id, i.entity_key, i.status, i.resolved_at \
               FROM _amux_invariant_incident i \
               JOIN issues c ON c.id = i.board_issue \
              WHERE i.resolved_at IS NOT NULL \
                AND i.board_issue != '' \
                AND c.status NOT IN ('done','verified','discarded') \
                AND c.archived = 0 \
              LIMIT 50",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, f64>(4)?,
                ))
            })?
            .flatten()
            .collect::<Vec<_>>();
        rows.into_iter()
            .map(|(card, inv, entity, status, at)| {
                // The terminal status matters and is carried verbatim. `pass`
                // means it healed; `unknown` means the check STOPPED BEING ABLE
                // TO RUN, which is not the same claim and must not read as one
                // (AMUX-3575).
                let what = if status == "unknown" {
                    "stopped being checkable"
                } else {
                    "resolved"
                };
                let line = format!(
                    "[amux autofix] the incident behind this card has {what}: `{inv}` on \
                     `{entity}` last evaluated {status} at {}. This card was filed \
                     automatically from that incident and nothing has re-checked whether the \
                     WORK here is still needed — that call is yours, not mine. Re-check with \
                     GET /api/debug/invariants (latest_per_invariant), where a PASS is visible; \
                     /api/health/invariants reports failures only, so it cannot tell a healed \
                     check from one that never ran.",
                    chrono::DateTime::from_timestamp(at as i64, 0)
                        .map(|t| {
                            t.with_timezone(&chrono::Local)
                                .format("%Y-%m-%d %H:%M")
                                .to_string()
                        })
                        .unwrap_or_else(|| "an unknown time".into()),
                );
                let idem = format!("autofix-inv-resolved:{card}:{inv}:{entity}");
                Note { card, what: what.to_string(), line, idem }
            })
            .collect()
    };

    let mut noted: Vec<(String, String)> = Vec::new();
    for Note { card, what, line, idem } in candidates {
        let already = {
            let conn = state.store.read()?;
            conn.query_row(
                "SELECT 1 FROM session_events WHERE idem=?1 LIMIT 1",
                rusqlite::params![idem],
                |_| Ok(()),
            )
            .is_ok()
        };
        if already {
            continue;
        }
        let c2 = card.clone();
        let i2 = idem.clone();
        state
            .store
            .write_async(move |conn| {
                use rusqlite::OptionalExtension;
                let existing: Option<String> = conn
                    .query_row("SELECT log FROM issues WHERE id=?1", rusqlite::params![c2], |r| {
                        r.get(0)
                    })
                    .optional()?
                    .flatten();
                let hhmm = chrono::Local::now().format("%H:%M").to_string();
                let log = bs::append_log(existing.as_deref(), &hhmm, &line);
                conn.execute(
                    "UPDATE issues SET log=?1 WHERE id=?2",
                    rusqlite::params![log, c2],
                )?;
                conn.execute(
                    "INSERT OR IGNORE INTO session_events (ts, session, type, data, idem, source) \
                     VALUES (?1, '', 'autofix.inv_resolved', ?2, ?3, 'autofix')",
                    rusqlite::params![unix_now(), json!({"card": c2}).to_string(), i2],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .await?;
        noted.push((card, what));
    }
    Ok(noted)
}


/// "Stopped happening" is NOTED, never acted on.
///
/// For every open autofix card whose signature has produced nothing for
/// `quiet_h`, append one log line saying so. The card is not closed, not
/// archived, not moved: silence is evidence, not a verdict, and deciding what
/// it means is the job of the lane holding the card. One line per card, ever —
/// the idem key makes the note exactly-once so a quiet signature cannot
/// re-nag forever (which is its own well-documented failure here).
async fn note_quiet_signatures(
    state: &AppState,
    now: f64,
    ci: &[CiRun],
) -> anyhow::Result<Vec<(String, String)>> {
    let quiet_cut = now - quiet_h() * 3600.0;
    /// One pending note: the card, the short "what" for the report, the line to
    /// log, and the idem that makes it exactly-once.
    struct Note {
        card: String,
        what: String,
        line: String,
        idem: String,
    }
    let mut candidates: Vec<Note> = Vec::new();
    {
        let conn = state.store.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, source_ref FROM issues \
             WHERE source_ref LIKE 'autofix:%' AND deleted IS NULL \
               AND status NOT IN ('done','verified','discarded') \
               AND COALESCE(archived,0)=0 LIMIT 500",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for (id, sref) in rows.flatten() {
            let sig = sref.trim_start_matches("autofix:").to_string();

            // CI is the second detector with a per-occurrence record to ask —
            // GitHub's own run list. Same rule as the 5xx sweep and stated in
            // `ci_recovered`: the card gets a LINE, never a status change.
            if sig.starts_with("ci|") {
                if let Some(line) = ci_recovered(ci, &sig) {
                    candidates.push(Note {
                        card: id.clone(),
                        what: "workflow is green again".into(),
                        line,
                        idem: format!("autofix-ci-green:{id}"),
                    });
                }
                continue;
            }

            // Only the request-log-backed detectors can be asked "has this
            // recurred?"; the others have no per-occurrence row to count, so
            // they are left alone rather than guessed at.
            let Some(rest) = sig.strip_prefix("5xx|") else { continue };
            let parts: Vec<&str> = rest.split('|').collect();
            if parts.len() != 4 {
                continue;
            }
            let (status, method, target) = (parts[0], parts[1], parts[3]);
            let Ok(status) = status.parse::<i64>() else { continue };
            let recent: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM _amux_request_log WHERE status=?1 AND method=?2 AND ts >= ?3",
                    rusqlite::params![status, method, quiet_cut],
                    |r| r.get(0),
                )
                .unwrap_or(1);
            if recent == 0 {
                candidates.push(Note {
                    card: id.clone(),
                    what: format!("{method} {target} → {status}"),
                    line: format!(
                        "autofix: no occurrence of this signature for {:.0}h ({method} {target} → \
                         {status}). STOPPED HAPPENING IS NOT FIXED — it may be fixed, or the \
                         caller may have given up, or the path may now be unreachable. This card \
                         stays open; whoever holds it decides which.",
                        quiet_h()
                    ),
                    idem: format!("autofix-quiet:{id}"),
                });
            }
        }
    }
    let mut noted = Vec::new();
    for Note { card, what, line, idem } in candidates {
        let already = {
            let conn = state.store.read()?;
            conn.query_row("SELECT 1 FROM session_events WHERE idem=?1 LIMIT 1", rusqlite::params![idem], |_| Ok(()))
                .is_ok()
        };
        if already {
            continue;
        }
        let c2 = card.clone();
        let i2 = idem.clone();
        state
            .store
            .write_async(move |conn| {
                use rusqlite::OptionalExtension;
                let existing: Option<String> = conn
                    .query_row("SELECT log FROM issues WHERE id=?1", rusqlite::params![c2], |r| r.get(0))
                    .optional()?
                    .flatten();
                let hhmm = chrono::Local::now().format("%H:%M").to_string();
                let log = bs::append_log(existing.as_deref(), &hhmm, &line);
                conn.execute("UPDATE issues SET log=?1 WHERE id=?2", rusqlite::params![log, c2])?;
                conn.execute(
                    "INSERT OR IGNORE INTO session_events (ts, session, type, data, idem, source) \
                     VALUES (?1, '', 'autofix.quiet', ?2, ?3, 'autofix')",
                    rusqlite::params![unix_now(), json!({"card": c2}).to_string(), i2],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .await?;
        noted.push((card, what));
    }
    Ok(noted)
}

// ---------------------------------------------------------------------------
// Spawn + debug surface.
// ---------------------------------------------------------------------------

/// `AMUX_AUTOFIX_SECS=0` disables the loop entirely (the repo's tick idiom).
/// Note the difference from the Settings toggle: 0 stops the JOB, the toggle
/// stops the WRITE and keeps the evidence.
pub fn spawn(state: AppState) -> Option<super::PeriodicTask> {
    let secs = std::env::var("AMUX_AUTOFIX_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(AUTOFIX_TICK_SECS);
    if secs == 0 {
        tracing::info!("autofix: disabled (AMUX_AUTOFIX_SECS=0)");
        return None;
    }
    let home = crate::runtime_jobs::autofix::amux_home();
    Some(super::spawn_periodic("autofix", secs, move || {
        let state = state.clone();
        let home = home.clone();
        async move {
            // The ONE place that reaches the network. Fetched here, before the
            // tick, so the tick never touches the store lock and GitHub in the
            // same breath — and so no other caller of `autofix_tick` inherits
            // an HTTP request it did not ask for. Cached behind
            // `AMUX_CI_POLL_MIN`, so a 120s tick is not 720 API calls a day.
            let (ci_runs, ci_sup) = fetch_ci_runs(unix_now()).await;
            let r = autofix_tick_with_ci(&state, &home, &ci_runs, ci_sup).await;
            if !r.filed.is_empty() || !r.errors.is_empty() {
                tracing::info!(
                    filed = r.filed.len(),
                    suppressed = r.suppressed.len(),
                    errors = ?r.errors,
                    "autofix tick"
                );
            }
        }
    }))
}

pub use crate::config::amux_home;

/// `GET /api/debug/autofix` — the answer to "why didn't it file?" in one
/// request. Carries the toggle state, every threshold, every signature seen,
/// every card filed, and every suppression WITH its reason.
async fn debug_autofix(axum::extract::State(state): axum::extract::State<AppState>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let r = last_report();
    let on = state.store.read().map(|c| enabled(&c)).unwrap_or(true);
    let secs = std::env::var("AMUX_AUTOFIX_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(AUTOFIX_TICK_SECS);
    let body = json!({
        "enabled": on,
        "loop_running": secs != 0,
        "tick_secs": secs,
        "detectors": DetectorKind::all().iter().map(|k| k.slug()).collect::<Vec<_>>(),
        "thresholds": {
            "window_h": window_h(),
            "baseline_h": baseline_h(),
            "latency_mult": latency_mult(),
            "latency_min_samples": latency_min_samples(),
            "outlier_ms": outlier_ms(),
            "dead_route_min_hits": dead_route_min_hits(),
            "schedule_overdue_min": schedule_overdue_min(),
            "steering_stale_min": steering_stale_min(),
            "invariant_min_occurrences": invariant_min_occurrences(),
            "build_stale_h": build_stale_h(),
            "quiet_h": quiet_h(),
            "ci_repos": ci_repos(),
            "ci_poll_min": ci_poll_min(),
            "ci_min_failures": ci_min_failures(),
            "ci_run_limit": ci_run_limit(),
            "ci_timeout_s": ci_cmd_timeout_s(),
        },
        "fixer_session": fixer_session(),
        "ignore": ignore_list(),
        "last": r.as_ref().map(|r| json!({
            "at": r.at,
            "at_local": rl::local_when(r.at),
            "age_s": (unix_now() - r.at).max(0.0),
            "enabled": r.enabled,
            "took_ms": r.took_ms,
            "signatures_seen": r.signatures_seen.iter()
                .map(|(k, s)| json!({"detector": k, "signature": s})).collect::<Vec<_>>(),
            "filed": r.filed.iter()
                .map(|(c, s)| json!({"card": c, "signature": s})).collect::<Vec<_>>(),
            "already_filed": r.already_filed,
            "suppressed": r.suppressed.iter()
                .map(|(k, s, why)| json!({"detector": k, "signature": s, "reason": why}))
                .collect::<Vec<_>>(),
            // Visible on /api/debug/autofix, or the notes go out and nothing
            // records that they did — the exact shape AF-180 closed one
            // detector over (ethos rule 4: will they see it where they look).
            "resolved_noted": r.resolved_noted.iter()
                .map(|(c, w)| json!({"card": c, "what": w}))
                .collect::<Vec<_>>(),
            "quiet_noted": r.quiet_noted.iter()
                .map(|(c, w)| json!({"card": c, "what": w})).collect::<Vec<_>>(),
            // AMUX-3645. A parked card is filed and then deliberately does
            // nothing, so it is invisible to every other row here: it is not
            // suppressed (it WAS filed), it is not quiet (its signature is
            // live), and it raises no error. Named separately or the whole
            // mechanism runs unobserved.
            "parked": r.parked.iter()
                .map(|(c, s, when)| json!({"card": c, "signature": s, "heals_at": when}))
                .collect::<Vec<_>>(),
            "errors": r.errors,
        })),
        "note": "suppressed lists EVERY decision not to file, with its reason — a detector that \
                 silently declines is the failure this job exists to end",
    });
    axum::Json::<Value>(body).into_response()
}

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new().route("/api/debug/autofix", axum::routing::get(debug_autofix))
}

/// Permissive timestamp parse for `schedules.next_run`, which is stored as
/// text in whatever the writer used. Returns None rather than guessing.
fn parse_ts(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<f64>() {
        // Epoch seconds, sanity-bounded so a bare year ("2026") cannot pass.
        if n > 1_000_000_000.0 {
            return Some(n);
        }
        return None;
    }
    use chrono::TimeZone;
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp() as f64);
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            if let Some(dt) = chrono::Local.from_local_datetime(&ndt).single() {
                return Some(dt.timestamp() as f64);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests. Each one is a property the design would be worthless without, and
// each is written so a plausible wrong implementation FAILS it — an N-cards
// filer, a refiling filer, a filer that buries the board in 501s, a card with
// no evidence on it.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    // ── AEAB-59: a standing card must re-measure, and must not become a journal ──
    //
    // Every "refreshes" below has a neighbour that must NOT, because the two
    // failures are opposite and both real: a card frozen at a five-day-old
    // number, and a card rewritten every tick until nothing on it is
    // done-or-not-done (ethos rule 5).

    /// THE BUG. AMUX-30 read "4.2 GB free" for five days while the volume fell
    /// to 0.94 GB. Same condition, same signature, different number — and the
    /// number is the whole point of the card.
    #[test]
    fn a_moved_measurement_refreshes_the_standing_card() {
        let day = 86_400;
        assert!(super::should_refresh(
            "disk: 4.2 GB free, below the 50 GB floor",
            "disk: 0.9 GB free, below the 50 GB floor",
            0, 5 * day, 6 * 3600,
        ));
    }

    /// THE CONTROL, and the one that stops this becoming rule 5's journal: an
    /// unchanged title writes nothing, however long the condition stands. A
    /// refresh that fired on a static condition would rewrite the card every
    /// tick forever.
    #[test]
    fn an_unchanged_measurement_writes_nothing_however_old() {
        let t = "disk: 4.2 GB free, below the 50 GB floor";
        assert!(!super::should_refresh(t, t, 0, 365 * 86_400, 6 * 3600));
    }

    /// The rate gate. A value oscillating across a rounding boundary would
    /// otherwise rewrite the card on every tick — the same journal, arrived at
    /// from the other direction.
    #[test]
    fn a_moved_measurement_still_waits_out_the_rate_gate() {
        let now = 1_000_000;
        assert!(!super::should_refresh("a", "b", now - 60, now, 6 * 3600));
        assert!(super::should_refresh("a", "b", now - 6 * 3600, now, 6 * 3600));
    }

    /// The refresh block is REPLACED, never appended, or the card grows without
    /// bound for as long as the condition lasts — which for a disk alarm is
    /// exactly the case where somebody eventually has to read it.
    #[test]
    fn the_refresh_block_replaces_and_never_stacks() {
        let base = "original body";
        let once = format!("{base}{}first", super::REFRESH_MARK);
        assert_eq!(super::desc_without_refresh(&once), base);
        let twice = format!("{once}{}second", super::REFRESH_MARK);
        assert_eq!(
            super::desc_without_refresh(&twice), base,
            "a second refresh must still strip back to the original body"
        );
        assert_eq!(super::desc_without_refresh(base), base, "no block is a no-op");
    }

    // ── connector-auth: token rot files one card per ACCOUNT ────────────────

    #[test]
    fn connector_findings_key_on_the_account_and_carry_the_reconnect() {
        use super::{connector_findings_from_rollup, DetectorKind};
        let rollup = serde_json::json!({
            "accounts": [],
            "needs_reauth": [
                {"account": "broken@x.io", "broken": ["gmail"],
                 "reconnect": "POST /api/connectors/google/auth?account=broken@x.io → open authorize_url, approve once"},
                {"account": "two@x.io", "broken": ["gmail", "google"],
                 "reconnect": "POST /api/connectors/google/auth?account=two@x.io → open authorize_url, approve once"},
            ],
        });
        let f = connector_findings_from_rollup(&rollup, 1000.0);
        assert_eq!(f.len(), 2);
        // Signature is per-ACCOUNT — two broken families on one account are
        // still ONE approval, so they must be one card.
        assert_eq!(f[0].signature, "connector-reauth|broken@x.io");
        assert_eq!(f[1].signature, "connector-reauth|two@x.io");
        assert!(matches!(f[0].kind, DetectorKind::ConnectorAuth));
        assert!(f[1].title.contains("two@x.io"), "{}", f[1].title);
        assert!(f[1].title.contains("gmail, google"), "{}", f[1].title);
        let reconnect = f[0].evidence.iter().find(|(k, _)| k == "reconnect").unwrap();
        assert!(reconnect.1.contains("/api/connectors/google/auth?account=broken@x.io"));

        // Healthy rollup → nothing to file (the check CAN pass).
        let quiet = connector_findings_from_rollup(&serde_json::json!({"needs_reauth": []}), 0.0);
        assert!(quiet.is_empty());
    }

    #[test]
    fn canary_api_errors_file_per_service_but_unreachable_and_not_granted_never_do() {
        use super::connector_findings_from_rollup;
        // One account, four canary legs: only the leg where the provider
        // ANSWERED with a failure becomes a card. `unreachable` is network
        // trouble (a dead network must never fan out one card per account),
        // `not_granted` is neutral, `ok` is ok.
        let rollup = serde_json::json!({
            "accounts": [
                {"account": "acct@x.io", "canary": {
                    "gmail": {"status": "ok", "http": 200},
                    "calendar": {"status": "api_error", "http": 403, "detail": "insufficientPermissions"},
                    "drive": {"status": "not_granted"},
                    "slack": {"status": "unreachable", "detail": "dns"},
                }},
            ],
            "needs_reauth": [],
        });
        let f = connector_findings_from_rollup(&rollup, 1000.0);
        assert_eq!(f.len(), 1, "{:?}", f.iter().map(|x| &x.signature).collect::<Vec<_>>());
        assert_eq!(f[0].signature, "connector-canary|acct@x.io|calendar");
        assert!(f[0].title.contains("HTTP 403"), "{}", f[0].title);
        let detail = f[0].evidence.iter().find(|(k, _)| k == "detail").unwrap();
        assert!(detail.1.contains("insufficientPermissions"));
    }

    // ── du_one: a timeout and an unreadable path are DIFFERENT (AEAB-33) ────
    //
    // These ran identically before: both returned `None` and both were reported
    // under "these paths exceeded the du budget", which is false for a path that
    // cannot be read at all and sends the reader at a knob that cannot help.

    /// The POSITIVE control. Without it every assertion below is satisfied by a
    /// function that returns Unreadable for everything.
    #[test]
    fn a_readable_directory_is_sized() {
        let d = std::env::temp_dir().join(format!("amux-du-ok-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let _ = std::fs::write(d.join("f"), vec![0u8; 4096]);
        let out = du_one(&d, std::time::Instant::now() + std::time::Duration::from_secs(10));
        let _ = std::fs::remove_dir_all(&d);
        assert!(matches!(out, DuOutcome::Sized(_)), "got {out:?}");
    }

    /// Rebuilt from the INCIDENT'S OWN SPECIMEN rather than a convenient one:
    /// `~/.Trash` is the path that was named 914 times in 24h as a budget
    /// overrun while `du` on it actually returns "Operation not permitted".
    ///
    /// Skipped rather than failed where the specimen is absent (Linux CI, or a
    /// mac that has granted this process Full Disk Access) — asserting there
    /// would make the suite fail for a reason that has nothing to do with the
    /// code. The `du` probe below is what decides, so this can never pass by
    /// mistaking a missing directory for a permissions error.
    /// AF-97, the class kill. Every blocking `std::process::Command` in this
    /// file's PRODUCTION half must live in one of the two bounded helpers.
    ///
    /// The original defect was not that `du` was slow — it was that a detector
    /// shelled out synchronously from an `async fn` that was holding the SQLite
    /// store lock, and nothing anywhere could say so. Bounding `du` (AMUX-35)
    /// and then `tmutil` (AF-97) each fixed one instance; the third one lands
    /// the same way unless adding it is what fails.
    ///
    /// Reading the source rather than the behaviour is deliberate: the property
    /// is "no unbounded subprocess reachable from the tick", and there is no
    /// runtime observation that distinguishes a bounded call from a lucky fast
    /// one. `tests/system_jobs.rs` guards `lib.rs` the same way.
    #[test]
    fn every_blocking_subprocess_here_goes_through_a_bounded_helper() {
        let src = include_str!("autofix.rs");
        let prod = src.split("\nmod tests").next().unwrap_or(src);
        const ALLOWED: [&str; 2] = ["du_one", "bounded_output"];

        let mut current = "<file scope>";
        let mut offenders: Vec<(usize, &str)> = Vec::new();
        for (i, line) in prod.lines().enumerate() {
            let t = line.trim_start();
            if !line.starts_with(' ') && (t.starts_with("fn ") || t.starts_with("pub fn ")) {
                current = t
                    .trim_start_matches("pub ")
                    .trim_start_matches("fn ")
                    .split(['(', '<'])
                    .next()
                    .unwrap_or("?");
            }
            if line.contains("std::process::Command::new") && !ALLOWED.contains(&current) {
                offenders.push((i + 1, current));
            }
        }
        assert!(
            offenders.is_empty(),
            "blocking subprocess outside a bounded helper: {offenders:?}. \
             The autofix tick is an async fn that holds state.store.read() while the \
             detectors run, so an unbounded child here stalls every lane's writes for \
             as long as it hangs (AMUX-35, AF-97). Route it through `bounded_output`."
        );
    }

    /// The control for the test above: it must be able to FIND the calls it is
    /// vetting. An empty scan and a clean scan are the same green, and this file
    /// is 3000+ lines — a pattern that silently stops matching would read as
    /// "no blocking subprocesses" forever.
    #[test]
    fn the_subprocess_scan_actually_finds_the_calls_it_vets() {
        let src = include_str!("autofix.rs");
        let prod = src.split("\nmod tests").next().unwrap_or(src);
        let n = prod.matches("std::process::Command::new").count();
        assert!(n >= 2, "scan found {n} blocking subprocess call sites, expected at least the two bounded helpers — the pattern has drifted from the code");
    }

    /// AF-97. `bounded_output` must return on ITS deadline, not the command's.
    ///
    /// Discriminating by construction: `/bin/sleep 30` against a 1s budget takes
    /// 30s and returns `Some` if the ceiling is not enforced, so removing the
    /// deadline check fails this on both the value AND the clock. The elapsed
    /// assertion is the one that survives someone "fixing" the return value.
    #[test]
    fn bounded_output_returns_on_its_own_deadline_not_the_commands() {
        let t0 = std::time::Instant::now();
        let got = bounded_output("/bin/sleep", &["30"], std::time::Duration::from_secs(1));
        let elapsed = t0.elapsed();
        assert_eq!(got, None, "an overrun must report ABSENT, never an empty success");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "returned after {elapsed:?} — the 1s budget was not enforced"
        );
    }

    /// The control. Without it the test above passes just as well against a
    /// `bounded_output` that always returns None, which would silently disable
    /// the snapshot count rather than bound it.
    #[test]
    fn bounded_output_still_returns_output_for_a_command_that_finishes() {
        let got = bounded_output("/bin/echo", &["hello"], std::time::Duration::from_secs(10));
        assert_eq!(got.as_deref(), Some(&b"hello\n"[..]));
    }

    /// A non-zero exit is not output. `local_snapshot_count` counts marker
    /// strings in stdout, so a failed command whose stdout happened to contain
    /// one would otherwise be counted as a snapshot.
    #[test]
    fn bounded_output_rejects_a_nonzero_exit() {
        assert_eq!(bounded_output("/bin/sh", &["-c", "echo x; exit 3"], std::time::Duration::from_secs(10)), None);
    }

    #[test]
    fn an_unreadable_path_is_not_reported_as_a_timeout() {
        let trash = match std::env::var("HOME") {
            Ok(h) => std::path::PathBuf::from(h).join(".Trash"),
            Err(_) => return,
        };
        if !trash.exists() {
            return;
        }
        // Only assert when `du` genuinely cannot read it — confirmed by running
        // the same command this code runs, unbounded.
        let probe = std::process::Command::new("/usr/bin/du")
            .args(["-skx"])
            .arg(&trash)
            .output();
        let denied = match probe {
            Ok(o) => o.stdout.split(|c| c.is_ascii_whitespace()).next()
                .and_then(|t| std::str::from_utf8(t).ok())
                .and_then(|t| t.parse::<u64>().ok())
                .is_none(),
            Err(_) => return,
        };
        if !denied {
            return;
        }
        // A generous deadline, so a TimedOut verdict here could only mean the
        // code confused the two — which is the whole point of the assertion.
        let out = du_one(&trash, std::time::Instant::now() + std::time::Duration::from_secs(30));
        assert_eq!(
            out,
            DuOutcome::Unreadable(Some(1)),
            "du cannot read {} — that must report as Unreadable, never as a budget overrun",
            trash.display()
        );
    }

    /// The NEGATIVE direction on the deadline: a deadline already in the past
    /// must produce TimedOut, not Unreadable. Without this, a function
    /// returning Unreadable unconditionally passes the specimen test above.
    #[test]
    fn an_expired_deadline_is_a_timeout_not_an_unreadable_path() {
        let d = std::env::temp_dir();
        let out = du_one(&d, std::time::Instant::now() - std::time::Duration::from_secs(1));
        assert_eq!(out, DuOutcome::TimedOut, "got {out:?}");
    }

    /// `du` exits non-zero when it could not descend into SOME subdirectory but
    /// still prints a total. That total is a usable size, and discarding it
    /// would throw away a mostly-correct figure for the common case of a few
    /// protected subdirs — turning a partial answer into no answer.
    #[test]
    fn a_partial_walk_that_still_prints_a_total_counts_as_sized() {
        let d = std::env::temp_dir().join(format!("amux-du-part-{}", std::process::id()));
        let sub = d.join("locked");
        let _ = std::fs::create_dir_all(&sub);
        let _ = std::fs::write(d.join("f"), vec![0u8; 8192]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000));
        }
        let out = du_one(&d, std::time::Instant::now() + std::time::Duration::from_secs(10));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755));
        }
        let _ = std::fs::remove_dir_all(&d);
        // Running as root defeats the setup — the walk succeeds cleanly and the
        // case is untestable rather than failing.
        assert!(matches!(out, DuOutcome::Sized(_)), "got {out:?}");
    }

    use super::*;
    use std::sync::Arc;

    /// `du_top` must RETURN under its budget even when a path cannot be walked
    /// in time. The incident this guards: an unbounded `du` on a nearly-full
    /// disk parked a tokio worker per tick until the server stopped answering.
    ///
    /// The slow path is `/` — a real directory whose walk cannot finish in the
    /// budget, rather than a fake one. A tempdir with a few files completes
    /// The du budget lives in PROCESS-GLOBAL env vars, and cargo runs these tests
    /// on parallel threads — so a case that sets a 1ms budget silently imposes it
    /// on whatever else is mid-`du_top`. That was already latent here (three
    /// cases shared these vars, two writing and one reading), and it only became
    /// visible when a new case used a budget small enough to break a neighbour:
    /// `du_top_still_measures_a_walkable_path` failed with nothing wrong in it.
    ///
    /// Every case that touches those vars takes this lock. A test suite whose
    /// verdict depends on thread scheduling is not a check that can fail
    /// reliably — it is one that fails randomly, which is worse.
    fn du_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
        L.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// instantly and would pass against the ORIGINAL unbounded code, which is
    /// exactly the "convenient case lacks the property that made the incident"
    /// trap; this asserts the bound by making the walk genuinely unfinishable.
    #[test]
    fn du_top_returns_within_budget_on_an_unwalkable_path() {
        let _g = du_env_lock();
        std::env::set_var("AMUX_DISK_DU_PATH_TIMEOUT_S", "1");
        std::env::set_var("AMUX_DISK_DU_TOTAL_TIMEOUT_S", "2");
        let started = std::time::Instant::now();
        let _ = du_top(&[std::path::PathBuf::from("/")], 8, None);
        let elapsed = started.elapsed().as_secs_f64();
        std::env::remove_var("AMUX_DISK_DU_PATH_TIMEOUT_S");
        std::env::remove_var("AMUX_DISK_DU_TOTAL_TIMEOUT_S");
        assert!(
            elapsed < 10.0,
            "du_top ran {elapsed:.1}s against a 2s total budget — the bound is not being enforced, \
             which is the exact condition that wedged the server"
        );
    }

    /// A timed-out path is OMITTED, never recorded as 0 bytes. A silent zero
    /// would sort the largest consumer last and aim the disk report at an
    /// innocent directory.
    #[test]
    fn du_top_omits_a_timed_out_path_rather_than_sizing_it_zero() {
        let _g = du_env_lock();
        std::env::set_var("AMUX_DISK_DU_PATH_TIMEOUT_S", "1");
        std::env::set_var("AMUX_DISK_DU_TOTAL_TIMEOUT_S", "2");
        let got = du_top(&[std::path::PathBuf::from("/")], 8, None);
        std::env::remove_var("AMUX_DISK_DU_PATH_TIMEOUT_S");
        std::env::remove_var("AMUX_DISK_DU_TOTAL_TIMEOUT_S");
        assert!(
            !got.iter().any(|r| r.bytes == 0),
            "a skipped path was reported with a size instead of being omitted: {got:?}"
        );
    }

    // ── the ranker must be able to see a FILE (AEAB-42) ───────────────────

    /// THE POINT OF THE CARD. A large plain file must be a candidate.
    ///
    /// This test FAILS against the previous code, which pushed only entries
    /// where `is_dir()` — verify that before believing it. `~/.amux/amux.db`
    /// was 1.8 GB on a volume with 1.8 GB free, would have ranked fourth, and
    /// was absent from all 26 entries of the ranker's own cache.
    ///
    /// The control assertion matters as much as the treatment: the directory
    /// must STILL appear. A "fix" that swapped one exclusion for the opposite
    /// one would pass a file-only assertion and silently drop every directory,
    /// which is the same defect with the sign flipped — and this repo has
    /// already shipped both signs of it in one night (ethos rule 1).
    #[test]
    fn disk_candidates_include_a_big_file_and_still_include_directories() {
        let _g = du_env_lock();
        std::env::set_var("AMUX_DISK_FILE_MIN_MB", "1");
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("a-directory")).unwrap();
        std::fs::write(home.path().join("big.db"), vec![0u8; 2 * 1024 * 1024]).unwrap();
        std::fs::write(home.path().join("small.txt"), b"tiny").unwrap();

        let got = disk_candidates(home.path());
        std::env::remove_var("AMUX_DISK_FILE_MIN_MB");

        let has = |n: &str| got.iter().any(|p| p.file_name().map(|f| f == n).unwrap_or(false));
        assert!(has("big.db"), "a 2MB file above a 1MB floor must be a candidate: {got:?}");
        assert!(has("a-directory"), "directories must STILL be candidates: {got:?}");
        assert!(!has("small.txt"), "a file below the floor is noise, not a candidate: {got:?}");
    }

    /// The agent scratch root must be rankable (AF-275).
    ///
    /// The filter was `n.contains("target")`, and every session's scratch lives
    /// at `/private/tmp/claude-<uid>`. "claude-501" contains no "target", so 18
    /// GB on this machine — and a 100%-full root volume on another, where the
    /// blocked lane could not open a file to find out why — was unrankable BY
    /// CONSTRUCTION. It had to ask a human what was eating the disk, because
    /// amux's own report could not name the directory amux tells it to use.
    ///
    /// A NAME PREDICATE rather than a filesystem fixture on purpose: the branch
    /// reads the real `/private/tmp`, so a directory-based test would assert
    /// against whatever the host happens to have and pass vacuously in CI,
    /// where no `claude-<uid>` exists. This pins the shipped decision itself.
    ///
    /// The `target` cells are the control. A fix that swapped one exclusion for
    /// its opposite would satisfy the scratch cell alone while silently
    /// dropping every build tree — the same defect with the sign flipped, which
    /// this file has shipped in both directions before.
    #[test]
    fn the_agent_scratch_root_is_rankable_and_build_trees_still_are() {
        // Treatment: the specimen from the incident, and the shape on any host.
        assert!(tmp_candidate_name("claude-501"), "the agent scratch root on this machine");
        assert!(tmp_candidate_name("claude-0"), "and on a host with a different uid");

        // Control: the behaviour that already existed must survive.
        assert!(tmp_candidate_name("amux-build-target"), "build trees must STILL rank");
        assert!(tmp_candidate_name("target"), "and a bare target dir");

        // Neither arm may become a catch-all: ranking all of /private/tmp would
        // spend the whole `du` budget on OS noise and crowd out real findings.
        assert!(!tmp_candidate_name("com.apple.launchd.abc123"));
        assert!(!tmp_candidate_name("tmux-501"), "the tmux socket dir is tiny and not ours to rank");
    }

    /// AMUX-3667 follow-up, rebuilt from the eight cards themselves.
    ///
    /// `POST /api/browser/start` filed AMUX-3650, 3652, 3653, 3654, 3655, 3656,
    /// 3660 and 3661 in 50 minutes — one fault, eight cards, identical but for
    /// the count in the title, 8 of the 23 in one lane's queue. Their
    /// `source_ref`s differ only in the trailing epoch, which is exactly the
    /// field added so a DISCARD would stick.
    ///
    /// So the treatment and the control pull against each other by design, and
    /// both have to hold: an OPEN card suppresses, a DISCARDED one does not.
    #[test]
    fn one_open_card_per_fault_but_a_discarded_one_does_not_suppress() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE issues (id TEXT, source_ref TEXT, status TEXT, \
             archived INTEGER DEFAULT 0, deleted TEXT);",
        )
        .unwrap();
        let add = |id: &str, sref: &str, status: &str| {
            conn.execute(
                "INSERT INTO issues (id, source_ref, status, archived, deleted) \
                 VALUES (?1,?2,?3,0,NULL)",
                rusqlite::params![id, sref, status],
            )
            .unwrap();
        };
        // The real signature shape, from the real cards.
        let sig = |ts: i64| format!("5xx|502|POST|/api/browser|/api/browser/start|{ts}");

        // Nothing filed yet: the first occurrence must get a card.
        assert_eq!(open_card_for_fault(&conn, &sig(1787585028)), None);

        // AMUX-3650 is open. A LATER batch of the same fault must not file.
        add("AMUX-3650", &format!("autofix:{}", sig(1787585028)), "todo");
        assert_eq!(
            open_card_for_fault(&conn, &sig(1787585376)).as_deref(),
            Some("AMUX-3650"),
            "a newer batch of an already-open fault must be suppressed"
        );

        // CONTROL 1: discarded must NOT suppress, or judging a spurious report
        // would silence the fault permanently — the opposite failure, and the
        // one the trailing timestamp exists to avoid.
        conn.execute("UPDATE issues SET status='discarded' WHERE id='AMUX-3650'", []).unwrap();
        assert_eq!(
            open_card_for_fault(&conn, &sig(1787585376)),
            None,
            "a discarded card must let the next genuine occurrence through"
        );
        for st in ["done", "verified"] {
            conn.execute("UPDATE issues SET status=?1 WHERE id='AMUX-3650'", rusqlite::params![st])
                .unwrap();
            assert_eq!(open_card_for_fault(&conn, &sig(1787585376)), None, "{st}");
        }
        // An ARCHIVED open card is not in anyone's queue either.
        conn.execute("UPDATE issues SET status='todo', archived=1 WHERE id='AMUX-3650'", [])
            .unwrap();
        assert_eq!(open_card_for_fault(&conn, &sig(1787585376)), None, "archived");

        // CONTROL 2: a DIFFERENT fault must still file. Matching on the prefix
        // with a `LIKE` makes over-matching the live risk — `/api/browser` is a
        // prefix of `/api/browser/start`, so a sloppy pattern would fold every
        // endpoint under one card and the board would go quiet about real
        // faults.
        conn.execute("UPDATE issues SET status='todo', archived=0 WHERE id='AMUX-3650'", [])
            .unwrap();
        let other = "5xx|502|POST|/api/browser|/api/browser/stop|1787585376";
        assert_eq!(open_card_for_fault(&conn, other), None, "a different target is a different fault");
        let other_status = "5xx|500|POST|/api/browser|/api/browser/start|1787585376";
        assert_eq!(open_card_for_fault(&conn, other_status), None, "a different status too");

        // AMUX-3673: a ROLLUP's identity is "the server was slow", not which
        // endpoints it caught. These two are the REAL signatures of AMUX-3651
        // and AMUX-3673, which differ by one element and are the same fault.
        let r1 = "latency|outlier|ROLLUP|/api/board,/api/board/{id},/api/browser/start,\
                  /api/sessions,/api/sessions-git,/api/sessions/{name}/{*verb}";
        let r2 = "latency|outlier|ROLLUP|/api/board,/api/browser/action,/api/browser/start,\
                  /api/sessions,/api/sessions-git,/api/sessions/{name}/{*verb}";
        assert_eq!(fault_identity(r1), Some("latency|outlier|ROLLUP"));
        assert_eq!(
            fault_identity(r1),
            fault_identity(r2),
            "two rollups differing by one endpoint are one fault, not two cards"
        );
        // CONTROL: a rollup must NOT collapse into a per-endpoint fault, or the
        // first slow endpoint would silence every server-wide report after it.
        assert_ne!(
            fault_identity(r1),
            fault_identity("latency|outlier|GET|/api/board|1787585028"),
            "a rollup and a single-endpoint outlier are different faults"
        );

        // CONTROL 3: only the two timestamped families have a fault identity at
        // all. Stripping a trailing field from an arbitrary signature would
        // merge unrelated faults.
        assert_eq!(fault_identity("invariant|hooks.guard|fleet|1787223065"), None);
        assert_eq!(fault_identity("5xx|502|POST|/api/x|/api/x|nota-number"), None);
        assert_eq!(
            fault_identity("latency|outlier|GET|/api/board|1787585028"),
            Some("latency|outlier|GET|/api/board")
        );
    }

    /// AMUX-3664, rebuilt from the incident row that exposed it.
    ///
    /// `hooks.shared_guard_matches_committed` failed 359 times over four days,
    /// resolved at 12:36, and its card was never told — because the incident
    /// row's `board_issue` was empty, because the write-back searched for
    /// `entity_key='fleet'` while a fleet-wide invariant stores `''`.
    ///
    /// The fleet-wide case is the treatment AND the entity-keyed case is the
    /// control: a "fix" that always fell back to `''` would link an
    /// entity-keyed card to the wrong row, which is worse than not linking it.
    #[test]
    fn a_fleet_wide_incident_links_to_its_card_and_an_entity_keyed_one_still_matches_exactly() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE _amux_invariant_incident (invariant_id TEXT, entity_key TEXT, \
             board_issue TEXT);
             INSERT INTO _amux_invariant_incident VALUES ('hooks.shared_guard','', '');
             INSERT INTO _amux_invariant_incident VALUES ('route.callers','GET /api/x','');
             INSERT INTO _amux_invariant_incident VALUES ('route.callers','GET /api/y','');",
        )
        .unwrap();
        let link = |c: &str, i: &str, e: &str| link_incident_to_card(&conn, c, i, e).unwrap();
        let card_of = |i: &str, e: &str| -> String {
            conn.query_row(
                "SELECT COALESCE(board_issue,'') FROM _amux_invariant_incident \
                 WHERE invariant_id=?1 AND entity_key=?2",
                rusqlite::params![i, e],
                |r| r.get(0),
            )
            .unwrap()
        };

        // TREATMENT: signed `fleet`, stored ''. This returned 0 before the fix.
        assert_eq!(link("AMUX-1", "hooks.shared_guard", "fleet"), 1);
        assert_eq!(card_of("hooks.shared_guard", ""), "AMUX-1");

        // CONTROL: an entity-keyed invariant must hit its OWN row and leave its
        // sibling alone. A blanket fallback would smear one card across both.
        assert_eq!(link("AMUX-2", "route.callers", "GET /api/x"), 1);
        assert_eq!(card_of("route.callers", "GET /api/x"), "AMUX-2");
        assert_eq!(card_of("route.callers", "GET /api/y"), "", "sibling must be untouched");

        // CONTROL: a genuine miss stays a miss and reports 0, rather than the
        // fallback inventing a link to some unrelated fleet-wide row.
        assert_eq!(link("AMUX-3", "no.such.invariant", "fleet"), 0);
        assert_eq!(card_of("hooks.shared_guard", ""), "AMUX-1", "must not be overwritten");
    }

    /// AMUX-3665, rebuilt from the report that motivated it.
    ///
    /// The disk ranking listed 47 GB of `~/.amux` and named nothing bigger,
    /// while 23 GB sat in the shared checkout's own `target/` — gitignored, so
    /// `git status` could not show it either. The largest reclaimable thing on
    /// the volume was unrankable by construction, because the candidate roots
    /// were `~/.amux`, `/private/tmp/*target*` and five `$HOME` caches, and a
    /// checkout is in none of them.
    ///
    /// The fixture builds a fake sessions dir with a `CC_DIR` pointing at a
    /// checkout, which is the seam that generates the candidate.
    #[test]
    fn a_build_tree_in_a_session_checkout_is_a_candidate() {
        let _g = du_env_lock();
        let home = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        std::fs::create_dir(checkout.path().join("target")).unwrap();
        std::fs::create_dir(checkout.path().join("node_modules")).unwrap();
        // A SECOND checkout with no build tree: the negative control. Pushing a
        // non-existent path would spend the ranking's budget on a `du` that
        // reports nothing, which reads as "measured, found nothing" rather than
        // "never existed".
        let bare = tempfile::tempdir().unwrap();

        std::fs::create_dir(home.path().join("sessions")).unwrap();
        std::fs::write(
            home.path().join("sessions/lane-a.env"),
            format!("CC_DIR={}\n", checkout.path().display()),
        )
        .unwrap();
        // Two lanes on the SAME checkout: the fleet shares one, and the same
        // path listed twice would double-count it in the ranking.
        std::fs::write(
            home.path().join("sessions/lane-b.env"),
            format!("CC_DIR={}\n", checkout.path().display()),
        )
        .unwrap();
        std::fs::write(
            home.path().join("sessions/lane-c.env"),
            format!("CC_DIR={}\n", bare.path().display()),
        )
        .unwrap();
        // A lane with no CC_DIR at all must not crash or contribute.
        std::fs::write(home.path().join("sessions/lane-d.env"), "CC_TAGS=amux\n").unwrap();

        let got = disk_candidates(home.path());

        let want_target = checkout.path().join("target");
        let want_modules = checkout.path().join("node_modules");
        assert!(got.contains(&want_target), "the checkout's build tree must rank: {got:?}");
        assert!(got.contains(&want_modules), "node_modules too: {got:?}");
        assert_eq!(
            got.iter().filter(|p| **p == want_target).count(),
            1,
            "two lanes sharing a checkout must not list it twice: {got:?}"
        );
        assert!(
            !got.iter().any(|p| p.starts_with(bare.path())),
            "a checkout with no build tree must contribute NO candidate, not a \
             path that du will fail on: {got:?}"
        );
    }

    /// The floor is a POLICY knob, not a constant (deviation D4). If the env
    /// var did not reach the decision this test would pass anyway with the
    /// default 256 MB floor excluding a 2 MB file — so it asserts the INVERSE
    /// too: the same file crosses the same floor when the floor is lowered.
    /// One assertion alone here cannot fail for the right reason.
    #[test]
    fn the_file_floor_is_configurable_and_actually_consulted() {
        let _g = du_env_lock();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("two-mb.bin"), vec![0u8; 2 * 1024 * 1024]).unwrap();
        let seen = |v: &[std::path::PathBuf]| {
            v.iter().any(|p| p.file_name().map(|f| f == "two-mb.bin").unwrap_or(false))
        };

        std::env::set_var("AMUX_DISK_FILE_MIN_MB", "64");
        let high = disk_candidates(home.path());
        std::env::set_var("AMUX_DISK_FILE_MIN_MB", "1");
        let low = disk_candidates(home.path());
        std::env::remove_var("AMUX_DISK_FILE_MIN_MB");

        assert!(!seen(&high), "2MB is below a 64MB floor and must be excluded: {high:?}");
        assert!(seen(&low), "2MB is above a 1MB floor and must be included: {low:?}");
    }

    /// A file candidate must survive the whole pipeline, not merely be
    /// generated. `disk_candidates` feeding a `du_top` that cannot size a file
    /// would be the ethos-1 split all over again — a view that disagrees with
    /// the mechanism it feeds.
    #[test]
    fn du_top_can_size_a_plain_file() {
        let _g = du_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("blob.bin");
        std::fs::write(&f, vec![0u8; 512 * 1024]).unwrap();
        let got = du_top(std::slice::from_ref(&f), 8, None);
        assert_eq!(got.len(), 1, "a plain file must be sized, not dropped: {got:?}");
        assert!(got[0].bytes > 0, "a 512KB file must size non-zero: {got:?}");
    }

    /// Control: a path that CAN be walked is still measured and non-zero, so
    /// the two tests above are not passing merely because `du_top` returns
    /// nothing at all.
    #[test]
    fn du_top_still_measures_a_walkable_path() {
        let _g = du_env_lock();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), vec![0u8; 64 * 1024]).unwrap();
        let got = du_top(&[dir.path().to_path_buf()], 8, None);
        assert_eq!(got.len(), 1, "expected the tempdir to be measured: {got:?}");
        assert!(got[0].bytes > 0, "measured size should be non-zero: {got:?}");
        assert!(got[0].stale_age_s.is_none(), "a fresh walk must not be labelled stale: {got:?}");
    }

    // ── the carry-forward, both directions (AEAB-33) ───────────────────────

    /// THE POINT OF THE CARD. A path that could not be sized this run must still
    /// appear in the ranking, carrying its last known size and LABELLED stale —
    /// otherwise the biggest consumers are exactly the ones the report omits,
    /// which is what happened to ~/Library/Caches on 914 of 914 attempts in 24h
    /// while free space fell to 3.7G.
    ///
    /// The unwalkable path is `/` under a 1ms budget, the same specimen the
    /// existing budget tests use. An earlier draft measured a tempdir on run 1
    /// and tried to force it to time out on run 2 — but `du` on a tiny directory
    /// exits before the first poll, so it was SIZED both times and the test
    /// failed for a reason that had nothing to do with the carry-forward. The
    /// convenient case was convenient precisely because it lacked the property
    /// under test.
    #[test]
    fn a_path_that_cannot_be_sized_keeps_its_last_known_size_and_is_marked_stale() {
        let _g = du_env_lock();
        let cache = tempfile::tempdir().unwrap();
        let cf = cache.path().join("du-sizes.json");

        // Seed the cache as a previous successful run would have left it.
        let measured_at = crate::runtime_jobs::registry::unix_now() - 7200.0;
        let seeded: std::collections::HashMap<String, (u64, f64)> =
            [("/".to_string(), (9_900_000_000u64, measured_at))].into_iter().collect();
        std::fs::write(&cf, serde_json::to_string(&seeded).unwrap()).unwrap();

        std::env::set_var("AMUX_DISK_DU_PATH_TIMEOUT_S", "0.001");
        std::env::set_var("AMUX_DISK_DU_TOTAL_TIMEOUT_S", "0.001");
        let got = du_top(&[std::path::PathBuf::from("/")], 8, Some(&cf));
        std::env::remove_var("AMUX_DISK_DU_PATH_TIMEOUT_S");
        std::env::remove_var("AMUX_DISK_DU_TOTAL_TIMEOUT_S");

        assert_eq!(got.len(), 1, "the unsizable path must NOT vanish from the ranking: {got:?}");
        assert_eq!(got[0].bytes, 9_900_000_000, "it must carry the remembered size");
        let age = got[0].stale_age_s.expect("a carried figure must be LABELLED stale, not passed off as fresh");
        assert!((age - 7200.0).abs() < 60.0, "the age must be real, not a placeholder: {age}");
    }

    /// THE NEGATIVE HALF, and the one that keeps the fix honest. A path that has
    /// NEVER been sized must be absent, not present at 0 — a silent zero sorts
    /// the biggest consumer last and aims the report at an innocent directory,
    /// which is the failure `du_one`'s own doc comment warns about. Without this
    /// assertion, "always emit a row" would pass the test above.
    #[test]
    fn a_path_never_successfully_sized_is_absent_not_zero() {
        let _g = du_env_lock();
        let cache = tempfile::tempdir().unwrap();
        let cf = cache.path().join("du-sizes.json");
        std::env::set_var("AMUX_DISK_DU_PATH_TIMEOUT_S", "0.001");
        std::env::set_var("AMUX_DISK_DU_TOTAL_TIMEOUT_S", "0.001");
        let got = du_top(&[std::path::PathBuf::from("/")], 8, Some(&cf));
        std::env::remove_var("AMUX_DISK_DU_PATH_TIMEOUT_S");
        std::env::remove_var("AMUX_DISK_DU_TOTAL_TIMEOUT_S");
        assert!(got.is_empty(), "nothing was ever measured, so nothing may be claimed: {got:?}");
    }

    /// The cache must survive a restart, because this process re-execs to adopt
    /// new builds — in-memory state would be empty on exactly the first run
    /// after every restart, which is when the report is most needed. Asserted
    /// by reading the file a SEPARATE call wrote, not by trusting a global.
    #[test]
    fn the_size_cache_is_on_disk_so_it_survives_a_re_exec() {
        let _g = du_env_lock();
        let cache = tempfile::tempdir().unwrap();
        let cf = cache.path().join("nested").join("du-sizes.json");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), vec![0u8; 64 * 1024]).unwrap();
        let _ = du_top(&[dir.path().to_path_buf()], 8, Some(&cf));
        assert!(cf.exists(), "the cache file was not written (parent dir not created?)");
        let reloaded = du_cache_load(Some(&cf));
        assert!(
            reloaded.contains_key(&dir.path().display().to_string()),
            "a fresh process must be able to read back what this one measured: {reloaded:?}"
        );
    }

    /// Pins WHERE the cache lives, because the name `home` at the detect_disk
    /// call site is the AMUX home (`~/.amux`), not `$HOME` — and the first cut of
    /// this joined ".amux" again, writing to `~/.amux/.amux/du-sizes.json`. It
    /// worked, so no test and no log line objected; it was found by looking at
    /// the shipped filesystem after the deploy. This asserts the file lands
    /// directly in the directory it is given.
    #[test]
    fn the_size_cache_lands_directly_in_the_amux_home_not_a_nested_one() {
        let _g = du_env_lock();
        let fake_amux_home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), vec![0u8; 32 * 1024]).unwrap();
        let cf = fake_amux_home.path().join("du-sizes.json");
        let _ = du_top(&[dir.path().to_path_buf()], 8, Some(&cf));
        assert!(cf.exists(), "cache must be written at the path given");
        assert!(
            !fake_amux_home.path().join(".amux").exists(),
            "no nested .amux directory may be created"
        );
    }

    /// A missing or corrupt cache must degrade to today's behaviour, silently.
    /// A detector that panics on its own scratch file would take the server with
    /// it over a cache it does not need to function.
    #[test]
    fn a_corrupt_cache_file_degrades_instead_of_failing() {
        let _g = du_env_lock();
        let cache = tempfile::tempdir().unwrap();
        let cf = cache.path().join("du-sizes.json");
        std::fs::write(&cf, "{not json").unwrap();
        assert!(du_cache_load(Some(&cf)).is_empty());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), vec![0u8; 32 * 1024]).unwrap();
        let got = du_top(&[dir.path().to_path_buf()], 8, Some(&cf));
        assert_eq!(got.len(), 1, "a bad cache must not stop a live measurement: {got:?}");
    }

    /// AMUX-3014: autofix cards must default ON (local ops board) and turn OFF
    /// only on an explicit falsey value (the cloud image sets 0 so per-tenant
    /// customer boards do not carry the server's own health cards).
    #[test]
    fn ops_health_cards_default_on_off_only_when_explicitly_falsey() {
        assert!(parse_ops_health_cards(None), "unset -> on (local ops default)");
        assert!(parse_ops_health_cards(Some("1")), "1 -> on");
        assert!(parse_ops_health_cards(Some("true")), "true -> on");
        assert!(parse_ops_health_cards(Some("")), "empty -> on (not an explicit off)");
        for off in ["0", "false", "off", "no", "OFF", " 0 ", "False"] {
            assert!(!parse_ops_health_cards(Some(off)), "{off:?} must disable filing");
        }
    }

    /// AMUX-3696: a queue that cannot drain and a queue waiting for a turn
    /// boundary are different facts, and only one of them is "stuck".
    ///
    /// THE SPECIMEN IS THIS LANE. autofix filed "amux: steering queue stuck,
    /// oldest 92 min" while `/api/debug/steering` reported
    /// `blocked_reason: null, deliverable: true, stalled: false` and the drain
    /// loop's own last skip read "not-at-turn-boundary (within max age)". The
    /// lane was working a long turn, which is what a busy agent does, so a
    /// 90-minute deadline sat BELOW the natural turn length of the lanes it
    /// watches and filed against healthy ones.
    ///
    /// The detector had the discriminator in front of it the whole time: its
    /// own evidence block tells the reader to check `blocked_reason` before
    /// assuming a hang. It named the test and did not run it.
    ///
    /// THREE CELLS. The blocked one must still fire at 90 minutes, or this
    /// "fix" would have silenced the real defect the detector exists for.
    #[tokio::test]
    async fn a_reachable_lane_waiting_on_a_turn_boundary_is_not_a_stuck_queue() {
        let (st, _d) = state();
        let now = unix_now();
        // WRITE PATH, and `ensure_fleet_tables` FIRST. `store.read()` hands back
        // a read-only connection, and the migrations alone do not create
        // `steering_queue.sender` — it comes from ensure_fleet_tables' runtime
        // ALTER. Skip that and the detector's query does not prepare, the whole
        // steering block is skipped, and every cell below passes against
        // nothing. That is not a hypothetical: it is what the first draft of
        // this test did, and it is why the block had no coverage until now.
        st.store
            .write_async(move |conn| {
                crate::api::session_verbs::ensure_fleet_tables(conn)?;
                for lane in ["busy-lane", "dead-lane"] {
                    conn.execute(
                        "INSERT INTO steering_queue (id,session,text,queued_at,guard,sender) \
                         VALUES (?1,?2,'hi',?3,'','')",
                        rusqlite::params![format!("s-{lane}"), lane, now - 92.0 * 60.0],
                    )?;
                }
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .expect("seed");
        let conn = st.store.read().unwrap();

        let mut blocked = BTreeMap::new();
        blocked.insert("busy-lane".to_string(), None);
        blocked.insert("dead-lane".to_string(), Some("no-env-file".to_string()));
        let (f, sup) = detect_silent(&conn, now, &blocked);

        // THE PREMISE, asserted rather than assumed: the query must have RUN.
        // If it did not prepare, the detector records a suppression saying so,
        // and every assertion below would be about an empty vector.
        assert!(
            !sup.iter().any(|x| x.signature.contains("<query failed>")),
            "the steering query did not prepare, so this test is measuring nothing: {sup:?}"
        );

        // CELL 1 — reachable at 92 min: NOT a card. This is the specimen.
        assert!(
            !f.iter().any(|x| x.signature == "silent|steering|busy-lane"),
            "a lane the drain loop can deliver to is mid-turn, not stuck: {f:?}"
        );

        // CELL 2 — UNREACHABLE at the same 92 min: still a card, naming the
        // reason. Without this, ignoring the queue entirely passes cell 1 and
        // deletes the detector's whole purpose.
        let hit = f
            .iter()
            .find(|x| x.signature == "silent|steering|dead-lane")
            .expect("a lane with no env file will NEVER drain — that is the real defect");
        let ev: BTreeMap<_, _> = hit.evidence.iter().cloned().collect();
        assert!(hit.title.contains("cannot drain"), "not 'stuck': {}", hit.title);
        assert!(ev["verdict"].contains("no-env-file"), "name the reason: {ev:?}");
        assert!(ev["lane_reachable"].starts_with("NO"), "{ev:?}");

        // CELL 3 — reachable but far past the LARGER deadline: still reported,
        // as the different thing it is. A lane that has not reached a turn
        // boundary in 7h is worth a card; calling it a stuck queue is not.
        let (f2, _) = detect_silent(&conn, now + 7.0 * 3600.0, &blocked);
        let late = f2
            .iter()
            .find(|x| x.signature == "silent|steering|busy-lane")
            .expect("past the deliverable deadline it must still report");
        assert!(
            late.title.contains("has not taken a turn boundary"),
            "the title must describe what is actually true: {}",
            late.title
        );
        let ev2: BTreeMap<_, _> = late.evidence.iter().cloned().collect();
        assert!(
            ev2["verdict"].contains("not a stall"),
            "and must send the reader at the long turn, not at the queue: {ev2:?}"
        );
        // EVERY FIELD MUST AGREE WITH THE VERDICT BESIDE IT. Both of these were
        // wrong on the first working draft and were caught by READING the
        // payload during the mutation run, not by any assertion: `threshold_min`
        // reported 90 on a card that fired at 360, and the `senders` blurb said
        // "that lane may be unable to receive anything" directly under
        // `lane_reachable: yes`. A card whose fields contradict each other is
        // worse than a missing field, because each one is read as a fact.
        assert_eq!(ev2["threshold_min"], "360", "report the deadline that APPLIED: {ev2:?}");
        assert!(
            ev2["senders"].contains("the lane is reachable"),
            "the senders blurb must not assert unreachability on a reachable lane: {ev2:?}"
        );
        assert_eq!(ev["threshold_min"], "90", "the blocked lane's deadline is the 90: {ev:?}");
        assert!(
            ev["senders"].contains("cannot receive anything right now"),
            "...and there the blurb IS true: {ev:?}"
        );
    }

    fn state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        (
            AppState {
                store,
                started: std::time::Instant::now(),
                build_hash: "test".into(),
                auth_token: None,
            },
            dir,
        )
    }

    /// One request-log row. Grouped into a struct rather than ten positional
    /// arguments so a caller cannot silently swap `worker` and `ua` — which
    /// would quietly change what the dead-route detector concludes.
    struct Row<'a> {
        ts: f64,
        method: &'a str,
        path: &'a str,
        family: &'a str,
        status: i64,
        body: &'a str,
        worker: &'a str,
        ua: &'a str,
        ms: f64,
    }

    fn log_row(st: &AppState, r: Row<'_>) {
        let (ts, status, ms) = (r.ts, r.status, r.ms);
        let (m, p, f, b, w, u) = (
            r.method.to_string(), r.path.to_string(), r.family.to_string(),
            r.body.to_string(), r.worker.to_string(), r.ua.to_string(),
        );
        st.store
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO _amux_request_log (ts, method, path, family, status, latency_ms, \
                     client_ip, user_agent, amux_session, worker, answered_by, error_body) \
                     VALUES (?1,?2,?3,?4,?5,?6,'127.0.0.1',?7,'',?8,'native',?9)",
                    rusqlite::params![ts, m, p, f, status, ms, u, w, b],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
    }

    fn cards(st: &AppState) -> Vec<(String, String, String, String, String)> {
        let conn = st.store.read().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, title, desc, type, COALESCE(source_ref,'') FROM issues ORDER BY id")
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap();
        rows.flatten().collect()
    }

    /// THE grouping property: 14 identical failures are ONE incident. A filer
    /// that files per-row would put 14 cards on the board for one bug, which
    /// is how a board stops being read (ethos rule 5 — at 100x volume, does
    /// this stay coherent?).
    #[tokio::test]
    async fn n_identical_5xx_produce_one_card() {
        let (st, _d) = state();
        let now = unix_now();
        for i in 0..14 {
            log_row(&st, Row { ts: now - 60.0 - i as f64, method: "POST", path: &format!("/api/sessions/lane{i}/send"), family: "/api/sessions", status: 500, body: "{\"message\":\"boom\"}", worker: "lane", ua: "curl/8", ms: 12.0 });
        }
        let r = autofix_tick(&st, std::path::Path::new("/nonexistent")).await;
        let c = cards(&st);
        assert_eq!(c.len(), 1, "14 rows must be ONE card, got {}: {c:#?}", c.len());
        assert!(r.filed.len() == 1, "report must name the card it filed: {:?}", r.filed);
        // The 14 collapse because normalize_target folds the lane name into
        // the route pattern — the SAME collapse /api/logs/analyze uses.
        assert!(c[0].1.contains("14x"), "count belongs in the computed title: {}", c[0].1);
        assert!(c[0].1.contains("/api/sessions/{name}/{*verb}"), "title is the route, not a path: {}", c[0].1);
    }

    /// AMUX-3774: a suppression must not claim a count it does not bump, and a
    /// STALE suppressing card must say how stale it is.
    ///
    /// The reason text asserted "Its count is what moves; a second card would
    /// carry no new information." Nothing implemented that — the block pushes a
    /// report row and `continue`s, and never touches the card. Measured on the
    /// live board: AMUX-3651 last updated 08-24 17:35 while the rollup
    /// suppressed against it every ~2 minutes for two days, through a live
    /// six-family stall. The count was not moving, so a second card WOULD have
    /// carried new information.
    ///
    /// Both cells, because a message that always shouts "stale" is as useless
    /// as one that never does — and the FRESH arm is the common case that must
    /// stay quiet.
    #[tokio::test]
    async fn a_stale_suppressing_card_says_so_and_a_fresh_one_does_not() {
        async fn reason_after_aging(days: f64) -> String {
            let (st, _d) = state();
            let now = unix_now();
            for i in 0..4 {
                log_row(&st, Row { ts: now - 60.0 - i as f64, method: "POST", path: "/api/browser/start", family: "/api/browser", status: 502, body: "{}", worker: "l", ua: "curl/8", ms: 9.0 });
            }
            let r = autofix_tick(&st, std::path::Path::new("/nonexistent")).await;
            assert!(!r.filed.is_empty(), "the first occurrence must file: {r:?}");
            // Age the card, then re-run: the same fault must now suppress.
            // A NEW OCCURRENCE, not a re-scan of the same rows. `already_filed`
            // runs BEFORE the open-card check by design (its comment: asking
            // the precise question first is what keeps "I filed this batch"
            // distinct from "the fault is in someone's queue"), so re-ticking
            // the identical rows never reaches the suppression under test.
            // A newer row mints a newer trailing epoch, which is exactly the
            // real case: the fault recurred.
            let later = unix_now();
            log_row(&st, Row { ts: later - 1.0, method: "POST", path: "/api/browser/start", family: "/api/browser", status: 502, body: "{}", worker: "l", ua: "curl/8", ms: 9.0 });
            let stamp = unix_now() - days * 86400.0;
            st.store
                .write(move |c| {
                    c.execute("UPDATE issues SET updated=?1", rusqlite::params![stamp])?;
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                })
                .expect("age the card");
            let r2 = autofix_tick(&st, std::path::Path::new("/nonexistent")).await;
            r2.suppressed
                .iter()
                .find(|(_, _, why)| {
                    why.contains("open for this same fault") || why.contains("NOT suppressed")
                })
                .map(|(_, _, why)| why.clone())
                .unwrap_or_else(|| panic!("the second scan must suppress: {r2:?}"))
        }

        let fresh = reason_after_aging(0.0).await;
        assert!(
            !fresh.contains("Its count is what moves"),
            "the claim that was never implemented must be GONE: {fresh}"
        );
        assert!(
            fresh.contains("does NOT bump"),
            "and the reason must say plainly that suppressing leaves the card untouched, or a \
             reader assumes the recurrence was recorded somewhere: {fresh}"
        );
        assert!(
            fresh.contains("0.0 day(s)"),
            "a fresh card still reports its age — the number IS the signal, so it is present \
             either way rather than only when alarming: {fresh}"
        );

        // 3 days: past the WARN threshold (2), inside the EXPIRY bound (7), so
        // it still suppresses and must say how stale it is.
        let stale = reason_after_aging(3.0).await;
        assert!(
            stale.contains("3.0 day(s)"),
            "a stale card must report HOW stale — that number is the only signal a reader \
             gets that the fault recurred while nobody was looking: {stale}"
        );
        assert!(
            stale.contains("stops suppressing at"),
            "and it must state the bound, or a reader cannot tell a park from a permanent \
             mute: {stale}"
        );

        // PAST THE BOUND: the mute EXPIRES and the fault files again. Without
        // this cell the bound could be any number, including infinity, and every
        // assertion above would still pass (AMUX-3774 criterion 2).
        let expired = reason_after_aging(30.0).await;
        assert!(
            expired.contains("NOT suppressed"),
            "past AMUX_AUTOFIX_MUTE_EXPIRE_DAYS a parked card must stop muting its class: \
             {expired}"
        );
        assert!(
            expired.contains("re-filing"),
            "and the decision must say what it did, so 'why did this re-file' needs no \
             bisect: {expired}"
        );
    }

    /// A restart must not refile. This is the property an in-memory dedupe
    /// would pass in a single process and fail in production, silently, on
    /// every deploy — and this server re-execs whenever anyone saves.
    #[tokio::test]
    async fn a_restart_does_not_refile() {
        let (st, _d) = state();
        let now = unix_now();
        for i in 0..5 {
            log_row(&st, Row { ts: now - 60.0 - i as f64, method: "POST", path: "/api/sessions/x/send", family: "/api/sessions", status: 500, body: "boom", worker: "", ua: "curl/8", ms: 12.0 });
        }
        autofix_tick(&st, std::path::Path::new("/nonexistent")).await;
        assert_eq!(cards(&st).len(), 1);

        // A RESTART: brand-new AppState over the SAME database, and the
        // in-memory report cell explicitly cleared. Nothing survives except
        // what is on disk — which is the whole question.
        *last_report_cell().write().unwrap() = None;
        let restarted = AppState {
            store: st.store.clone(),
            started: std::time::Instant::now(),
            build_hash: "test-after-restart".into(),
            auth_token: None,
        };
        let r = autofix_tick(&restarted, std::path::Path::new("/nonexistent")).await;
        assert_eq!(cards(&restarted).len(), 1, "a restart refiled the same signature");
        assert_eq!(r.filed.len(), 0, "nothing new should be filed");
        assert_eq!(r.already_filed.len(), 1, "and the dedupe must SAY it deduped: {r:?}");
    }

    /// 501/503 name what is missing. They are amux being honest about an
    /// absent capability, and a card for one is a card no lane can act on.
    /// Asserts BOTH halves: nothing filed, and the reason is visible.
    #[tokio::test]
    async fn honest_degradations_file_nothing_but_say_why() {
        let (st, _d) = state();
        let now = unix_now();
        for i in 0..8 {
            log_row(&st, Row { ts: now - 60.0 - i as f64, method: "GET", path: "/api/email/search", family: "/api/email", status: 501, body: "{\"error\":\"not a connected Gmail account\",\"connected_hint\":\"...\"}", worker: "", ua: "curl/8", ms: 1.0 });
            log_row(&st, Row { ts: now - 60.0 - i as f64, method: "GET", path: "/api/torrents", family: "/api/torrents", status: 503, body: "{\"error\":\"aria2c not running\",\"start\":\"aria2c --enable-rpc\"}", worker: "", ua: "curl/8", ms: 1.0 });
        }
        let r = autofix_tick(&st, std::path::Path::new("/nonexistent")).await;
        assert_eq!(cards(&st).len(), 0, "a 501/503 must never become a card");
        let reasons: Vec<&String> = r.suppressed.iter().map(|(_, _, why)| why).collect();
        assert!(
            reasons.iter().any(|w| w.contains("honest degradation")),
            "the suppression must be VISIBLE with its reason, not silent: {reasons:?}"
        );
        // Control: a real 500 in the same window still files, so the test
        // cannot pass by the detector being broken outright.
        log_row(&st, Row { ts: now - 30.0, method: "POST", path: "/api/foo", family: "/api/foo", status: 500, body: "kaboom", worker: "", ua: "curl/8", ms: 1.0 });
        autofix_tick(&st, std::path::Path::new("/nonexistent")).await;
        assert_eq!(cards(&st).len(), 1, "a genuine 500 must still file");
    }

    /// The card has to be startable. A worker picking this up should be able
    /// to reproduce the finding before reading a word of prose.
    #[tokio::test]
    async fn the_card_carries_the_evidence() {
        let (st, _d) = state();
        let now = unix_now();
        for i in 0..6 {
            log_row(&st, Row { ts: now - 300.0 + i as f64, method: "POST", path: "/api/sessions/amux/send", family: "/api/sessions", status: 500, body: "{\"message\":\"session is in the background-conversation view\"}", worker: "amux", ua: "Mozilla/5.0", ms: 37.2 });
        }
        autofix_tick(&st, std::path::Path::new("/nonexistent")).await;
        let c = cards(&st);
        assert_eq!(c.len(), 1);
        let (id, title, desc, ty, sref) = &c[0];
        for field in [
            "verdict:", "sample_request:", "error_body:", "first_seen:", "last_seen:",
            "count:", "distinct_clients:", "signature:", "re-check",
        ] {
            assert!(desc.contains(field), "card {id} is missing `{field}`:\n{desc}");
        }
        assert!(desc.contains("background-conversation"), "the actual error body must be quoted");
        assert!(desc.contains("/api/logs/analyze"), "the recheck must be a runnable query");
        // AMUX-3591: the signature now carries OCCURRENCE IDENTITY (the newest
        // offending row's second-resolution ts), so it cannot be pinned as a
        // literal. Asserting the prefix keeps what this line was actually for —
        // that source_ref is "autofix:" + the detector's signature and the
        // target is normalized — while the suffix assertion below pins the new
        // property: re-scanning the SAME rows must mint the SAME signature, or
        // a judged report refiles forever.
        let want_prefix = "autofix:5xx|500|POST|/api/sessions|/api/sessions/{name}/{*verb}|";
        assert!(
            sref.starts_with(want_prefix),
            "source_ref must be autofix: + the signature, target normalized: got {sref}"
        );
        let ts_part = sref.trim_start_matches(want_prefix);
        assert!(
            ts_part.parse::<i64>().is_ok() && !ts_part.is_empty(),
            "the occurrence suffix must be a bare epoch second, got {ts_part:?} — without it a \
             discarded report re-arms and refiles the identical rows (AMUX-3581 -> 3589 -> 3591)"
        );
        // NEVER `code`: an auto-filed report has no merged commit to claim, so
        // a code gate could only be exited by asserting something untrue.
        assert_eq!(ty, "investigation", "auto-filed faults must not be gated as code");
        assert!(!title.is_empty());
        // Owner comes from the request's own worker attribution.
        let conn = st.store.read().unwrap();
        let owner: Option<String> = conn
            .query_row("SELECT session FROM issues WHERE id=?1", rusqlite::params![id], |r| r.get(0))
            .unwrap();
        assert_eq!(owner.as_deref(), Some("amux"));
        let ot: String = conn
            .query_row("SELECT owner_type FROM issues WHERE id=?1", rusqlite::params![id], |r| r.get(0))
            .unwrap();
        assert_eq!(ot, "agent", "board_drive only picks up agent-owned cards");
    }

    /// The toggle stops the WRITE, never the WATCH. A disabled watcher that
    /// also goes blind loses the evidence for exactly the window it was off.
    #[tokio::test]
    async fn disabled_still_records_what_it_would_have_filed() {
        let (st, _d) = state();
        let now = unix_now();
        for i in 0..4 {
            log_row(&st, Row { ts: now - 60.0 - i as f64, method: "POST", path: "/api/thing", family: "/api/thing", status: 500, body: "bang", worker: "", ua: "curl/8", ms: 3.0 });
        }
        st.store
            .write(|conn| {
                conn.execute("INSERT OR REPLACE INTO prefs (key,value) VALUES ('autofix_enabled','0')", [])?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let r = autofix_tick(&st, std::path::Path::new("/nonexistent")).await;
        assert!(!r.enabled);
        assert_eq!(cards(&st).len(), 0, "disabled must not write a card");
        assert_eq!(r.signatures_seen.len(), 1, "but it must still SEE the fault: {r:?}");
        assert!(
            r.suppressed.iter().any(|(_, _, why)| why.contains("autofix_enabled=0")),
            "and say why it did not file: {:?}", r.suppressed
        );
        // Flip it back on: the same fault now files, with no restart.
        st.store
            .write(|conn| {
                conn.execute("UPDATE prefs SET value='1' WHERE key='autofix_enabled'", [])?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let r2 = autofix_tick(&st, std::path::Path::new("/nonexistent")).await;
        assert!(r2.enabled);
        assert_eq!(cards(&st).len(), 1, "the pref must take effect live, without a restart");
    }

    /// A 404 nobody's browser asked for is a probe, not a broken feature.
    /// Both directions asserted: the curl 404 is suppressed WITH a reason, and
    /// the browser 404 on the same path files — otherwise "files nothing" would
    /// pass by the detector being dead.
    #[tokio::test]
    async fn dead_route_files_only_what_the_spa_actually_calls() {
        let (st, _d) = state();
        let now = unix_now();
        for i in 0..5 {
            log_row(&st, Row { ts: now - 100.0 - i as f64, method: "GET", path: "/api/burn", family: "/api/burn", status: 404, body: "{\"error\":\"not found\"}", worker: "", ua: "curl/8.7.1", ms: 1.0 });
        }
        let (f, s) = detect_dead_routes(&st.store.read().unwrap(), now);
        assert!(f.is_empty(), "a curl 404 must not file: {f:?}");
        assert!(
            s.iter().any(|x| x.reason.contains("probe")),
            "and the suppression must be visible: {s:?}"
        );

        for i in 0..5 {
            log_row(&st, Row { ts: now - 100.0 - i as f64, method: "GET", path: "/api/offline-origin", family: "/api/offline-origin", status: 404, body: "{\"error\":\"not found\"}", worker: "", ua: "Mozilla/5.0 (Macintosh)", ms: 1.0 });
        }
        let (f2, _) = detect_dead_routes(&st.store.read().unwrap(), now);
        assert_eq!(f2.len(), 1, "a path the SPA fetches must file: {f2:?}");
        let ev: BTreeMap<_, _> = f2[0].evidence.iter().cloned().collect();
        // The killer annotation, taken verbatim from the route table rather
        // than re-derived here.
        assert!(ev.contains_key("routed_methods"), "{ev:?}");
        assert!(ev.contains_key("nearest_routes"), "{ev:?}");
        assert!(ev["called_by"].contains("browser"));
    }

    /// AF-175, rebuilt from the incident's own numbers. The 2026-08-23 restart
    /// was at 22:28:05Z and the dashboard had been polling /api/board every 5s
    /// since 22:26:18; nine requests completed 5s apart with latencies falling
    /// by exactly 5s. Those are one outage, not nine slow requests.
    ///
    /// EVERY ASSERTION HERE IS A LOGIC CASE, not a wording one. A control that
    /// mutates a string proves only that the assertion READS the output — and
    /// cannot tell a working assertion from one mis-specified so badly it fails
    /// on a correct implementation. Flip the comparison in `spans_restart` and
    /// the first two cells invert; nothing about the phrasing changes.
    #[test]
    fn a_request_whose_clock_spans_the_restart_is_not_a_slow_request() {
        let boot = 1_787_524_085.0; // 22:28:05Z

        // The real specimen: arrived 22:27:21, completed 22:28:19, 58.4s.
        // Arrival precedes boot, so it measured the outage.
        assert!(
            spans_restart(boot + 14.0, 58_400.0, Some(boot)),
            "a request that arrived before this process started measured the outage"
        );

        // THE CONTROL THAT MATTERS. A genuinely slow request AFTER the boot must
        // still count, or the filter is excluding everything and looks identical
        // to one that works. 58 seconds is not exempt because it is large; it is
        // exempt only because its clock crossed the boundary.
        assert!(
            !spans_restart(boot + 3600.0, 58_400.0, Some(boot)),
            "a 58s request that STARTED well after boot is a real regression and must file"
        );

        // The boundary itself, both sides: one millisecond of arrival either way
        // decides it, and nothing else does.
        assert!(spans_restart(boot + 10.0, 10_001.0, Some(boot)));
        assert!(!spans_restart(boot + 10.0, 9_999.0, Some(boot)));

        // A row that started AND FINISHED before this boot is OLD, not spanning.
        // This is the cell that fails against the one-sided predicate I shipped
        // in d0c034ad, which excluded 213,420 rows in an hour because most of a
        // multi-hour baseline predates the current boot on a server that
        // restarts ~27 times a day.
        assert!(
            !spans_restart(boot - 3600.0, 6.0, Some(boot)),
            "a 6ms request an hour BEFORE this boot is ordinary baseline data, not an outage"
        );
        assert!(
            !spans_restart(boot - 1.0, 500.0, Some(boot)),
            "started and finished before boot — old, not spanning"
        );

        // Unknown boot keeps EVERY row. Excluding on an unknown boundary would
        // drop everything and be indistinguishable from a working filter — the
        // failure this whole card is about, one layer up.
        assert!(!spans_restart(boot + 14.0, 58_400.0, None));
        assert!(!spans_restart(0.0, 999_999.0, None));
    }

    /// AF-178. A DETECTOR THAT CANNOT SEE MUST SAY SO.
    ///
    /// On 2026-08-23 a one-sided restart filter (d0c034ad) excluded 213,397 of
    /// 213,935 rows. Every family's baseline went to zero, the regression shape
    /// could not fire at all, and the single trace anywhere in the system was
    /// two suppressions reading "baseline has 0 samples (<30)" for /api/board
    /// and /api/sessions — which have 46,825 and 122,848 rows in the period and
    /// are the busiest families there are. That sentence is also what a
    /// brand-new install emits, so a dead detector wore a quiet endpoint's
    /// clothes and `/api/debug/autofix` certified it as normal.
    ///
    /// Both halves are asserted here: the alarm fires, AND the suppression
    /// carries the pre-filter count that separates the two states.
    ///
    /// THE CONTROL IS A LOGIC MUTATION. The second half runs the identical
    /// fixture with `boot = None`, which changes what the filter CONCLUDES and
    /// changes no string anywhere. Flip `spans_restart`'s comparison and the
    /// two halves swap; nothing about the wording moves.
    #[tokio::test]
    async fn a_baseline_deleted_by_a_filter_is_an_alarm_not_a_quiet_suppression() {
        let (st, _d) = state();
        let now = unix_now();
        // A boot inside the baseline period. Sixty baseline rows ARRIVE before
        // it and COMPLETE after it, which is the real restart-spanning shape.
        let boot = now - 100_000.0;
        for i in 0..60 {
            log_row(&st, Row { ts: boot + i as f64, method: "GET", path: "/api/collapse", family: "/api/collapse", status: 200, body: "", worker: "", ua: "curl/8", ms: 200_000.0 });
        }
        // Ordinary window traffic, well after the boot: never spanning.
        for i in 0..60 {
            log_row(&st, Row { ts: now - 100.0 - i as f64, method: "GET", path: "/api/collapse", family: "/api/collapse", status: 200, body: "", worker: "", ua: "curl/8", ms: 5.0 });
        }

        let (f, s) = detect_latency_at(&st.store.read().unwrap(), now, Some(boot));
        let hit = f
            .iter()
            .find(|x| x.signature.starts_with("latency|input-collapsed"))
            .unwrap_or_else(|| panic!("a family with 60 baseline rows and 0 survivors is a detector outage and must file: {f:?}"));
        assert!(hit.signature.contains("/api/collapse"), "the card must name the family: {}", hit.signature);
        let ev: BTreeMap<_, _> = hit.evidence.iter().cloned().collect();
        assert_eq!(ev["rows_discarded"], "60", "state how many rows were thrown away: {ev:?}");
        assert!(ev["detail"].contains("0 of 60"), "state both counts: {ev:?}");

        // And the suppression must no longer be sayable by a quiet endpoint.
        let sup_txt = s
            .iter()
            .find(|x| x.signature == "latency|p95|/api/collapse")
            .map(|x| x.reason.clone())
            .unwrap_or_default();
        assert!(
            sup_txt.contains("0 of 60"),
            "the suppression must carry the PRE-filter count, or it reads as sparse data: {sup_txt:?}"
        );

        // CONTROL — same rows, no boot boundary, so the filter keeps everything.
        // No alarm, and no suppression at all, which is what proves the 60
        // baseline rows really survived rather than the alarm being unreachable.
        let (f2, s2) = detect_latency_at(&st.store.read().unwrap(), now, None);
        assert!(
            !f2.iter().any(|x| x.signature.starts_with("latency|input-collapsed")),
            "nothing was filtered, so there is no collapse to report: {f2:?}"
        );
        assert!(
            !s2.iter().any(|x| x.signature == "latency|p95|/api/collapse"),
            "a 60-row baseline needs no suppression — if this fires the rows were dropped: {s2:?}"
        );
    }

    /// AF-180, filed back to me by amux reviewing AF-178, and the finding is
    /// this card's own thesis one level up.
    ///
    /// The collapse alarm is an ordinary Finding, which is right for reaching a
    /// human, but a Finding speaks only when the fault occurs. So "the blindness
    /// check ran and found nothing" and "the blindness check was never wired in"
    /// are byte-identical silences, which is exactly the confusion AF-178 exists
    /// to end one layer down. Checked rather than assumed: 462 invariants
    /// evaluated and none matches this, and /api/debug/autofix lists `detectors`
    /// as a static array of names with no per-detector result.
    ///
    /// A zero is a measurement. Silence is not.
    #[tokio::test]
    async fn a_healthy_blindness_check_says_it_ran_instead_of_staying_quiet() {
        let (st, _d) = state();
        let now = unix_now();
        for i in 0..60 {
            log_row(&st, Row { ts: now - 200_000.0 - i as f64, method: "GET", path: "/api/ok", family: "/api/ok", status: 200, body: "", worker: "", ua: "curl/8", ms: 6.0 });
        }
        for i in 0..60 {
            log_row(&st, Row { ts: now - 100.0 - i as f64, method: "GET", path: "/api/ok", family: "/api/ok", status: 200, body: "", worker: "", ua: "curl/8", ms: 6.0 });
        }
        let (f, s) = detect_latency_at(&st.store.read().unwrap(), now, None);
        assert!(
            !f.iter().any(|x| x.signature.starts_with("latency|input-collapsed")),
            "nothing collapsed, so nothing files: {f:?}"
        );
        let row = s
            .iter()
            .find(|x| x.signature == "latency|input-collapsed")
            .unwrap_or_else(|| panic!("the healthy answer must be VISIBLE, not implied by an absence: {s:?}"));
        assert!(
            row.reason.contains("0 of 1 families"),
            "quote the zero AND the population it is out of: {:?}",
            row.reason
        );
        assert!(
            row.reason.contains("120 rows considered"),
            "a zero is only useful next to how much was looked at: {:?}",
            row.reason
        );

        // CONTROL — when something DOES collapse, the healthy row must be gone
        // and the finding present. Both states are visible and they are
        // mutually exclusive, so a reader can never mistake one for the other.
        let (st2, _d2) = state();
        let boot = now - 100_000.0;
        for i in 0..60 {
            log_row(&st2, Row { ts: boot + i as f64, method: "GET", path: "/api/gone", family: "/api/gone", status: 200, body: "", worker: "", ua: "curl/8", ms: 200_000.0 });
        }
        let (f2, s2) = detect_latency_at(&st2.store.read().unwrap(), now, Some(boot));
        assert!(
            f2.iter().any(|x| x.signature.starts_with("latency|input-collapsed")),
            "a wiped family must file: {f2:?}"
        );
        assert!(
            !s2.iter().any(|x| x.signature == "latency|input-collapsed"),
            "the all-clear must not be published in the same tick as the alarm: {s2:?}"
        );
    }

    /// AF-178, the other side of the discrimination: an endpoint that is simply
    /// NEW must not be described as if something ate its rows. If both states
    /// produced the same sentence the alarm above would be worthless, since the
    /// whole defect was that one sentence covered both.
    #[tokio::test]
    async fn an_endpoint_with_no_history_reads_differently_from_one_that_was_filtered() {
        let (st, _d) = state();
        let now = unix_now();
        // Window traffic only. Nothing in the baseline period at all.
        for i in 0..60 {
            log_row(&st, Row { ts: now - 100.0 - i as f64, method: "GET", path: "/api/fresh", family: "/api/fresh", status: 200, body: "", worker: "", ua: "curl/8", ms: 5.0 });
        }
        // A second family with a real, thin, unfiltered baseline.
        for i in 0..60 {
            log_row(&st, Row { ts: now - 100.0 - i as f64, method: "GET", path: "/api/thin", family: "/api/thin", status: 200, body: "", worker: "", ua: "curl/8", ms: 5.0 });
        }
        for i in 0..5 {
            log_row(&st, Row { ts: now - 200_000.0 - i as f64, method: "GET", path: "/api/thin", family: "/api/thin", status: 200, body: "", worker: "", ua: "curl/8", ms: 5.0 });
        }

        let (f, s) = detect_latency_at(&st.store.read().unwrap(), now, None);
        assert!(
            !f.iter().any(|x| x.signature.starts_with("latency|input-collapsed")),
            "no rows were discarded, so nothing collapsed: {f:?}"
        );
        let fresh = s.iter().find(|x| x.signature == "latency|p95|/api/fresh")
            .map(|x| x.reason.clone()).unwrap_or_default();
        assert!(
            fresh.contains("no rows at all"),
            "a family with no history must say so plainly: {fresh:?}"
        );
        assert!(
            !fresh.contains("filtering"),
            "nothing was filtered here — saying so is the confusion this card exists to end: {fresh:?}"
        );
        let thin = s.iter().find(|x| x.signature == "latency|p95|/api/thin")
            .map(|x| x.reason.clone()).unwrap_or_default();
        assert!(
            thin.contains("5 of 5"),
            "a thin but intact baseline must show kept AND considered: {thin:?}"
        );
    }

    /// AF-175 AT THE SHIPPED-PATH LEVEL, which is the layer the incident
    /// actually happened at. `spans_restart`'s own cells pin the predicate;
    /// this one pins `detect_latency`, because the 213,397-row deletion was
    /// only visible once the predicate met a real 72-hour baseline.
    ///
    /// This server restarts ~27 times a day, so almost every baseline row
    /// predates the current boot. Under the one-sided predicate that shipped in
    /// d0c034ad every row here is discarded and the assertions below both fail.
    #[tokio::test]
    async fn a_baseline_older_than_this_boot_is_ordinary_data_not_an_outage() {
        let (st, _d) = state();
        let now = unix_now();
        // The real shape: the process started seconds ago; all history is older.
        let boot = now - 10.0;
        for i in 0..60 {
            log_row(&st, Row { ts: now - 200_000.0 - i as f64, method: "GET", path: "/api/hist", family: "/api/hist", status: 200, body: "", worker: "", ua: "curl/8", ms: 6.0 });
        }
        for i in 0..60 {
            log_row(&st, Row { ts: now - 100.0 - i as f64, method: "GET", path: "/api/hist", family: "/api/hist", status: 200, body: "", worker: "", ua: "curl/8", ms: 6.0 });
        }
        let (f, s) = detect_latency_at(&st.store.read().unwrap(), now, Some(boot));
        assert!(
            !f.iter().any(|x| x.signature.starts_with("latency|input-collapsed")),
            "a 6ms row from before this boot never spanned anything and must be kept: {f:?}"
        );
        assert!(
            !s.iter().any(|x| x.signature == "latency|p95|/api/hist"),
            "120 intact rows need no suppression — this firing means the baseline was eaten: {s:?}"
        );
    }

    /// AMUX-3513, rebuilt from the incident's numbers: twelve browser `wait`
    /// actions timing out at their caller-chosen 5s budget read as a 7.1x
    /// /api/browser p95 regression. A row stamped slow_ok is REQUESTED
    /// latency and must not file; the IDENTICAL rows without the stamp must
    /// still file — dropping the marker check silently kills the control.
    #[tokio::test]
    async fn requested_wait_budgets_are_not_a_latency_regression() {
        let insert_meta = |st: &AppState, ts: f64, ms: f64, meta: Option<&str>| {
            let m = meta.map(str::to_string);
            st.store
                .write(move |conn| {
                    conn.execute(
                        "INSERT INTO _amux_request_log (ts, method, path, family, status, \
                         latency_ms, client_ip, user_agent, amux_session, worker, answered_by, \
                         req_meta) VALUES (?1,'POST','/api/browser/action','/api/browser',200,?2,\
                         '127.0.0.1','curl/8','','','native',?3)",
                        rusqlite::params![ts, ms, m],
                    )?;
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                })
                .unwrap();
        };
        let (st, _d) = state();
        let now = unix_now();
        // Fast baseline + fast window traffic, plus the wait-budget rows.
        for i in 0..60 {
            insert_meta(&st, now - 200_000.0 - i as f64, 6.0, None);
            insert_meta(&st, now - 300.0 - i as f64, 6.0, None);
        }
        for i in 0..40 {
            insert_meta(
                &st,
                now - 100.0 - i as f64,
                5016.0,
                Some(r#"{"content_type":"application/json","slow_ok":"wait-budget"}"#),
            );
        }
        let (f, _) = detect_latency(&st.store.read().unwrap(), now);
        assert!(
            !f.iter().any(|x| x.signature.contains("/api/browser")),
            "a caller-requested wait budget is not a regression: {f:?}"
        );
        // CONTROL — the same rows WITHOUT the marker must file, or the skip
        // clause is silently matching everything (rule 7).
        let (st2, _d2) = state();
        for i in 0..60 {
            insert_meta(&st2, now - 200_000.0 - i as f64, 6.0, None);
            insert_meta(&st2, now - 300.0 - i as f64, 6.0, None);
        }
        for i in 0..40 {
            insert_meta(&st2, now - 100.0 - i as f64, 5016.0, None);
        }
        let (f2, _) = detect_latency(&st2.store.read().unwrap(), now);
        assert!(
            f2.iter().any(|x| x.signature == "latency|p95|/api/browser"),
            "unmarked 5s rows must still file — the control keeps the skip honest: {f2:?}"
        );
    }

    /// AMUX-3471, rebuilt from the incident's own numbers: /api/prefs at a
    /// 2ms window p95 over a sub-millisecond baseline is 3.7x — and 2ms is
    /// not a regression anyone should read about. The ratio must not file
    /// below the absolute floor; the same ratio ABOVE the floor still files.
    #[tokio::test]
    async fn a_fast_family_getting_relatively_slower_is_not_a_finding() {
        let (st, _d) = state();
        let now = unix_now();
        for i in 0..60 {
            log_row(&st, Row { ts: now - 200_000.0 - i as f64, method: "GET", path: "/api/prefs", family: "/api/prefs", status: 200, body: "", worker: "", ua: "curl/8", ms: 0.54 });
        }
        for i in 0..60 {
            log_row(&st, Row { ts: now - 100.0 - i as f64, method: "GET", path: "/api/prefs", family: "/api/prefs", status: 200, body: "", worker: "", ua: "curl/8", ms: 2.0 });
        }
        let (f, _) = detect_latency(&st.store.read().unwrap(), now);
        assert!(
            !f.iter().any(|x| x.signature == "latency|p95|/api/prefs"),
            "2ms at 3.7x is fast-vs-faster noise, not a regression: {f:?}"
        );
    }

    /// A p95 over four requests is one request wearing a percentile. Both
    /// floors must hold, and when a floor blocks the call it has to SAY so.
    #[tokio::test]
    async fn latency_needs_a_real_sample_on_both_sides() {
        let (st, _d) = state();
        let now = unix_now();
        // A tiny baseline and a slow window: the multiple is enormous, but
        // there is no norm to compare against, so it must not file.
        for i in 0..3 {
            log_row(&st, Row { ts: now - 200_000.0 - i as f64, method: "GET", path: "/api/slow", family: "/api/slow", status: 200, body: "", worker: "", ua: "curl/8", ms: 5.0 });
        }
        for i in 0..60 {
            log_row(&st, Row { ts: now - 100.0 - i as f64, method: "GET", path: "/api/slow", family: "/api/slow", status: 200, body: "", worker: "", ua: "curl/8", ms: 4000.0 });
        }
        let (f, s) = detect_latency(&st.store.read().unwrap(), now);
        assert!(
            !f.iter().any(|x| x.signature == "latency|p95|/api/slow"),
            "a 3-sample baseline cannot support a p95 verdict: {f:?}"
        );
        assert!(
            s.iter().any(|x| x.reason.contains("baseline has")),
            "the sample-floor refusal must be visible: {s:?}"
        );

        // Now give it a real baseline. Same window; it must file, and the card
        // must state the threshold it used.
        for i in 0..60 {
            log_row(&st, Row { ts: now - 200_000.0 - i as f64, method: "GET", path: "/api/slow", family: "/api/slow", status: 200, body: "", worker: "", ua: "curl/8", ms: 5.0 });
        }
        let (f2, _) = detect_latency(&st.store.read().unwrap(), now);
        let hit = f2.iter().find(|x| x.signature == "latency|p95|/api/slow")
            .expect("a 800x p95 regression over 60 samples must file");
        let ev: BTreeMap<_, _> = hit.evidence.iter().cloned().collect();
        assert!(ev["verdict"].contains("Threshold"), "state the threshold: {ev:?}");
        assert!(ev.contains_key("window_samples") && ev.contains_key("baseline_samples"));
    }

    /// AMUX-3646: three families regressing at once is ONE event, not three
    /// tasks, and two is still two.
    ///
    /// THE SPECIMEN is a pair filed on 2026-08-24 and discarded the same day:
    /// AMUX-3657 (`/api/sessions-git` p95 4624ms, 3.2x) and AMUX-3658
    /// (`/api/board` p95 994ms, 3.1x), fired in the same window while the host
    /// load average was 36.86 on 28 cores. Both statements were true and both
    /// were unactionable, because nothing in a per-family payload separates
    /// "this endpoint got slower" from "everything got slower".
    ///
    /// The OUTLIER detector has rolled these up since AMUX-3588 and its verdict
    /// text argues the case explicitly. This detector did not, so it emitted the
    /// cards the other detector's own text argues against.
    ///
    /// BOTH DIRECTIONS. The below-threshold cell is the one that keeps this
    /// honest: a rollup that swallowed every regression would pass a
    /// one-directional test and would have deleted the per-family card that is
    /// correct when a single endpoint really does regress.
    #[tokio::test]
    async fn simultaneous_p95_regressions_collapse_to_one_card_but_two_do_not() {
        let fast_then_slow = |st: &crate::api::AppState, now: f64, fam: &'static str| {
            for i in 0..60 {
                log_row(st, Row { ts: now - 200_000.0 - i as f64, method: "GET", path: fam, family: fam, status: 200, body: "", worker: "", ua: "curl/8", ms: 5.0 });
            }
            for i in 0..60 {
                log_row(st, Row { ts: now - 100.0 - i as f64, method: "GET", path: fam, family: fam, status: 200, body: "", worker: "", ua: "curl/8", ms: 4000.0 });
            }
        };

        // TWO families: below the default threshold of 3, so the per-family
        // cards must survive untouched.
        let (st, _d) = state();
        let now = unix_now();
        fast_then_slow(&st, now, "/api/two-a");
        fast_then_slow(&st, now, "/api/two-b");
        let (f, _) = detect_latency(&st.store.read().unwrap(), now);
        assert!(
            f.iter().any(|x| x.signature == "latency|p95|/api/two-a")
                && f.iter().any(|x| x.signature == "latency|p95|/api/two-b"),
            "two regressions are two endpoints until the threshold says otherwise: {:?}",
            f.iter().map(|x| &x.signature).collect::<Vec<_>>()
        );
        assert!(
            !f.iter().any(|x| x.signature.contains("p95|ROLLUP")),
            "and must NOT roll up below the threshold: {f:?}"
        );

        // THREE: one card, naming all three, with no per-family cards left.
        let (st3, _d3) = state();
        let now3 = unix_now();
        for fam in ["/api/three-a", "/api/three-b", "/api/three-c"] {
            fast_then_slow(&st3, now3, fam);
        }
        let (f3, _) = detect_latency(&st3.store.read().unwrap(), now3);
        let rollups: Vec<_> =
            f3.iter().filter(|x| x.signature.contains("p95|ROLLUP")).collect();
        assert_eq!(
            rollups.len(),
            1,
            "three simultaneous regressions are ONE event: {:?}",
            f3.iter().map(|x| &x.signature).collect::<Vec<_>>()
        );
        assert!(
            !f3.iter().any(|x| x.signature.starts_with("latency|p95|/api/three")),
            "the per-family cards must be REPLACED, not accompanied — otherwise the rollup \
             adds a card instead of collapsing three: {:?}",
            f3.iter().map(|x| &x.signature).collect::<Vec<_>>()
        );
        let ev: BTreeMap<_, _> = rollups[0].evidence.iter().cloned().collect();
        for fam in ["/api/three-a", "/api/three-b", "/api/three-c"] {
            assert!(
                ev["families"].contains(fam),
                "every collapsed family must be NAMED or the card is unactionable: {ev:?}"
            );
        }
        assert!(
            !rollups[0].signature.contains(&format!("{}", now3 as i64)),
            "the rollup signature must not carry an occurrence timestamp: a correlated \
             slowdown is a standing condition, so the same collapse must stay idempotent"
        );
    }

    /// AMUX-3486: an open incident whose last failing evaluation is STALE
    /// (the entity went quiet, so no pass can ever close it) must not mint a
    /// card — "has not self-healed" is unfounded when nothing re-evaluates.
    /// The same incident with a FRESH failure files normally. Without this,
    /// discard+re-arm refiled the same 08-21 specimen three times in a day.
    #[tokio::test]
    async fn a_stale_open_incident_does_not_refile_a_fresh_one_does() {
        let (st, _d) = state();
        let now = unix_now();
        let seed = |entity: &str, last_seen: f64| {
            let e = entity.to_string();
            st.store
                .write(move |conn| {
                    conn.execute(
                        "INSERT INTO _amux_invariant_incident \
                         (invariant_id, entity_key, status, first_seen, last_seen, occurrences, expected, observed) \
                         VALUES ('status.agrees_with_pane', ?1, 'fail', ?2, ?3, 3, 'e', 'o')",
                        rusqlite::params![e, last_seen - 100.0, last_seen],
                    )?;
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                })
                .unwrap();
        };
        seed("quiet-lane", now - 50_000.0); // ~14h stale, unprobeable
        seed("live-lane", now - 600.0); // failing 10 minutes ago
        let conn = st.store.read().unwrap();
        let (f, _) = detect_invariants(&conn, now);
        let ents: Vec<&str> = f.iter().filter_map(|x| x.evidence.iter().find(|(k, _)| k == "verdict").map(|(_, v)| v.as_str())).collect();
        let all = ents.join(" | ");
        assert!(all.contains("live-lane"), "a fresh failure must file: {all}");
        assert!(!all.contains("quiet-lane"), "a stale open incident must not mint cards: {all}");
    }

    /// AMUX-3485 both directions: a run INSIDE an endpoint's design budget
    /// (the mdai synthesis, ~100s accepted under AF-142, node timeout 150s)
    /// files nothing — a card per legitimate run is the threshold-below-
    /// baseline defect per-endpoint; a run PAST the design budget still
    /// files, because that means the endpoint's own timeout failed to bound
    /// the call, which is the one latency story there that IS wrong.
    #[tokio::test]
    async fn a_long_by_design_endpoint_files_only_past_its_own_budget() {
        let (st, _d) = state();
        let now = unix_now();
        log_row(&st, Row { ts: now - 500.0, method: "POST", path: "/api/files/mdai/run", family: "/api/files", status: 200, body: "", worker: "", ua: "curl/8", ms: 109_896.0 });
        let (f, _) = detect_latency(&st.store.read().unwrap(), now);
        assert!(
            !f.iter().any(|x| x.signature.contains("/api/files/mdai/run")),
            "a synthesis inside its 150s design budget is designed behavior: {f:?}"
        );
        log_row(&st, Row { ts: now - 100.0, method: "POST", path: "/api/files/mdai/run", family: "/api/files", status: 200, body: "", worker: "", ua: "curl/8", ms: 200_000.0 });
        let (f2, _) = detect_latency(&st.store.read().unwrap(), now + 1.0);
        assert!(
            f2.iter().any(|x| x.signature.contains("/api/files/mdai/run")),
            "past the design budget the endpoint's own timeout failed — that must file: {f2:?}"
        );
    }

    /// AMUX-3704: run-now's duration is the SCHEDULED WORK's duration, and the
    /// budget tracks the timeout that bounds it.
    ///
    /// THE SPECIMEN: `POST /api/schedules/SCHED-173/run` at 54.9s, 200. SCHED-173
    /// is a `shell` schedule whose script queries signups, sends a founder-welcome
    /// email and posts to Slack — the endpoint doing exactly what it was asked.
    ///
    /// The budget is 600s because `SHELL_TIMEOUT_S` and `steer_max_age_s` both
    /// bound this path there. Past it, a bound FAILED, and that is the one
    /// latency story on this route worth a card.
    #[tokio::test]
    async fn run_now_is_budgeted_by_the_timeout_that_bounds_it() {
        let (st, _d) = state();
        let now = unix_now();
        // The filed row, and a longer one that is still inside the timeout.
        for ms in [54_922.0, 400_000.0] {
            log_row(&st, Row { ts: now - 500.0, method: "POST", path: "/api/schedules/SCHED-173/run", family: "/api/schedules", status: 200, body: "", worker: "", ua: "curl/8", ms });
        }
        let (f, _) = detect_latency(&st.store.read().unwrap(), now);
        assert!(
            !f.iter().any(|x| x.signature.contains("/api/schedules/{id}/run")),
            "a synchronous run inside its own 600s timeout is the endpoint working: {f:?}"
        );

        // PAST THE TIMEOUT the bound failed, and that must still file — without
        // this cell the entry above is indistinguishable from switching the
        // route off.
        log_row(&st, Row { ts: now - 100.0, method: "POST", path: "/api/schedules/SCHED-173/run", family: "/api/schedules", status: 200, body: "", worker: "", ua: "curl/8", ms: 900_000.0 });
        let (f2, _) = detect_latency(&st.store.read().unwrap(), now + 1.0);
        assert!(
            f2.iter().any(|x| x.signature.contains("/api/schedules/{id}/run")),
            "900s exceeds the 600s the endpoint enforces on itself — a bound failed: {f2:?}"
        );
    }

    /// AMUX-2686: a gateway-owned path 404ing locally is not a dead route.
    ///
    /// `GET /api/stripe/status` filed a dead-route card 6 times from the
    /// dashboard. Both sides were already correct: the path is served by
    /// cloud/gateway/gateway.py and never by amux-server, and the SPA's own
    /// comment says so ("only exists on the cloud gateway, so a failed fetch
    /// (self-hosted) simply leaves the card hidden") and returns early on
    /// !r.ok. Only this detector disagreed, filing a card nobody could close.
    ///
    /// invariants/checks.rs has excluded exactly these paths since 2026-08-11,
    /// with a comment stating the reason that applies here verbatim: a failure
    /// list that can never reach zero stops being read. This calls the SAME
    /// predicate rather than carrying a second copy.
    ///
    /// BOTH DIRECTIONS. The second cell is the load-bearing one: excluding by a
    /// loose `contains` or a too-broad prefix would silence real dead routes,
    /// and checks.rs's own history has that bug — `/api/cloud-logout-extra`
    /// once matched `/api/cloud-logout` and was silently dropped.
    #[tokio::test]
    async fn a_gateway_owned_404_is_not_filed_as_a_dead_route() {
        let (st, _d) = state();
        let now = unix_now();
        // Six browser 404s each, which is well past the min-hits floor.
        for _ in 0..6 {
            log_row(&st, Row { ts: now - 500.0, method: "GET", path: "/api/stripe/status", family: "/api/stripe", status: 404, body: "", worker: "", ua: "Mozilla/5.0", ms: 3.0 });
            log_row(&st, Row { ts: now - 500.0, method: "GET", path: "/api/board/statuses", family: "/api/board", status: 404, body: "", worker: "", ua: "Mozilla/5.0", ms: 3.0 });
        }
        let (f, sup) = detect_dead_routes(&st.store.read().unwrap(), now);

        // 1. The gateway-owned path must NOT file, and the skip must be visible
        //    — a silent exclusion is indistinguishable from a detector that
        //    found nothing.
        assert!(
            !f.iter().any(|x| x.signature.contains("/api/stripe/status")),
            "a path served by the gateway 404s here on every deployment: {f:?}"
        );
        assert!(
            sup.iter().any(|s| s.signature.contains("/api/stripe/status")
                && s.reason.contains("gateway-owned")),
            "the exclusion must be recorded, not silent: {sup:?}"
        );

        // 2. A NON-gateway 404 must still file. Without this cell an exclusion
        //    that matched everything would pass — and over-broad matching is a
        //    bug this list has actually had.
        assert!(
            f.iter().any(|x| x.signature.contains("/api/board/statuses")),
            "an ordinary unrouted path the SPA fetches is still a dead route: {f:?}"
        );
    }

    /// AMUX-3709: a request that FAILED is not a latency measurement, and the
    /// 5xx path already owns it.
    ///
    /// THE SPECIMEN IS THE FIXTURE. GET /api/browser/screenshot answered 502 in
    /// 30,006ms on 2026-08-25 11:12:41 — the CDP `Page.captureScreenshot timed
    /// out after 30s` path, i.e. a wedged tab, not a slow endpoint. Every other
    /// row on that route in the same 24h was 200 at 197-3242ms. It filed
    /// AMUX-3709 ("took 30.0s") while the same failure was ALREADY AMUX-3708
    /// ("-> HTTP 502 (2x)"). Two cards, two queues, one fault, and the lane
    /// picked up the one that names an endpoint that was never slow.
    ///
    /// BOTH DIRECTIONS, and the second cell is the load-bearing one: excluding
    /// 5xx must not switch the detector off for the same route. A genuinely
    /// slow SUCCESS still has to file, because that is a latency story nothing
    /// else reports — the 5xx filer cannot see a 200.
    #[tokio::test]
    async fn a_failed_request_is_not_filed_as_slow_but_a_slow_success_still_is() {
        let (st, _d) = state();
        let now = unix_now();
        // The filed row, verbatim: 502 at the 30s CDP timeout.
        log_row(&st, Row { ts: now - 500.0, method: "GET", path: "/api/browser/screenshot", family: "/api/browser", status: 502, body: "", worker: "", ua: "curl/8", ms: 30_006.0 });
        let (f, _) = detect_latency(&st.store.read().unwrap(), now);
        assert!(
            !f.iter().any(|x| x.signature.contains("/api/browser/screenshot")),
            "a 502 at its own timeout is the 5xx path's card, not a latency card: {f:?}"
        );

        // A 4xx stays IN: nothing else would report a slow one, so excluding it
        // would be a blind spot rather than a de-duplication.
        log_row(&st, Row { ts: now - 400.0, method: "GET", path: "/api/browser/state", family: "/api/browser", status: 404, body: "", worker: "", ua: "curl/8", ms: 40_000.0 });
        let (f4, _) = detect_latency(&st.store.read().unwrap(), now + 1.0);
        assert!(
            f4.iter().any(|x| x.signature.contains("/api/browser/state")),
            "a 4xx has no other filer — excluding it would hide the row entirely: {f4:?}"
        );

        // And the SUCCESS case on the very route the exclusion touches, so the
        // first cell cannot be passing because the detector went silent.
        log_row(&st, Row { ts: now - 300.0, method: "GET", path: "/api/browser/screenshot", family: "/api/browser", status: 200, body: "", worker: "", ua: "curl/8", ms: 45_000.0 });
        let (f2, _) = detect_latency(&st.store.read().unwrap(), now + 2.0);
        assert!(
            f2.iter().any(|x| x.signature.contains("/api/browser/screenshot")),
            "a 200 that really took 45s is a latency story nothing else reports: {f2:?}"
        );
    }

    /// AMUX-3690: the two Gmail-backed routes share a budget because they share
    /// an implementation.
    ///
    /// `/api/email/search` and `/api/email/inbox` both call
    /// `email::inbox_messages`, so both inherit its 8-wide concurrency, its
    /// batch-above-20 path and its Gmail quota bound. `inbox` had a 30s budget
    /// since AMUX-3519; `search` did not, and filed at 10.076s on
    /// `q=primis&days=120` with no account, which fans out across three
    /// mailboxes concurrently and waits for the slowest.
    ///
    /// THE SPECIMEN IS THE FIXTURE: 10_076ms is the filed row, and 11_350ms is
    /// the worst of 3226 requests over seven days. Both must be silent, and the
    /// second is the one that matters — a budget that only covers the reported
    /// number leaves the next legitimate run to file.
    ///
    /// The past-budget cell is not decoration. 30s is the per-call reqwest
    /// timeout, so past it the number cannot be a big-but-legitimate fetch; it
    /// is a hung transport, and that is the one latency story on this route that
    /// is genuinely wrong.
    #[tokio::test]
    async fn the_gmail_backed_routes_share_a_budget_because_they_share_an_implementation() {
        let (st, _d) = state();
        let now = unix_now();
        for (path, ms) in [
            ("/api/email/search", 10_076.0), // the filed row
            ("/api/email/search", 11_350.0), // worst of 3226 over 7 days
            ("/api/email/inbox", 14_105.0),  // the 4-hourly count=600 tick
        ] {
            log_row(&st, Row { ts: now - 500.0, method: "GET", path, family: "/api/email", status: 200, body: "", worker: "", ua: "curl/8", ms });
        }
        let (f, _) = detect_latency(&st.store.read().unwrap(), now);
        assert!(
            !f.iter().any(|x| x.signature.contains("/api/email/")),
            "a quota-bound Gmail fetch inside its 30s budget is designed behavior, on BOTH \
             routes — filing per legitimate call is the threshold-below-baseline defect the \
             LONG_BY_DESIGN table exists for: {f:?}"
        );

        log_row(&st, Row { ts: now - 100.0, method: "GET", path: "/api/email/search", family: "/api/email", status: 200, body: "", worker: "", ua: "curl/8", ms: 45_000.0 });
        let (f2, _) = detect_latency(&st.store.read().unwrap(), now + 1.0);
        assert!(
            f2.iter().any(|x| x.signature.contains("/api/email/search")),
            "past 30s the per-call transport timeout should already have fired, so this is a \
             hang and must still file: {f2:?}"
        );
    }

    /// AMUX-3472: the outlier signature carries the newest offending row's
    /// ts, so the same specimen re-scanned mints the SAME signature (idem
    /// dedupes it) while a NEW slow request mints a NEW one and files
    /// regardless of what happened to the old card. Without this, a discard's
    /// re-arm refiled the identical rows on the next scan — twice in one day.
    #[tokio::test]
    async fn a_new_outlier_occurrence_is_a_new_signature_the_same_rows_are_not() {
        let (st, _d) = state();
        let now = unix_now();
        log_row(&st, Row { ts: now - 3000.0, method: "PATCH", path: "/api/sessions/desktop/config", family: "/api/sessions", status: 200, body: "", worker: "", ua: "curl/8", ms: 22_371.0 });
        let (f1, _) = detect_latency(&st.store.read().unwrap(), now);
        let (f2, _) = detect_latency(&st.store.read().unwrap(), now + 60.0);
        let sig1 = f1.iter().find(|x| x.signature.starts_with("latency|outlier|PATCH")).expect("files").signature.clone();
        let sig2 = f2.iter().find(|x| x.signature.starts_with("latency|outlier|PATCH")).expect("files").signature.clone();
        assert_eq!(sig1, sig2, "same rows re-scanned must mint the same signature");
        // A new occurrence: newer ts -> new signature.
        log_row(&st, Row { ts: now - 100.0, method: "PATCH", path: "/api/sessions/desktop/config", family: "/api/sessions", status: 200, body: "", worker: "", ua: "curl/8", ms: 30_000.0 });
        let (f3, _) = detect_latency(&st.store.read().unwrap(), now + 120.0);
        let sig3 = f3.iter().find(|x| x.signature.starts_with("latency|outlier|PATCH")).expect("files").signature.clone();
        assert_ne!(sig1, sig3, "a NEW slow request must be a NEW signature, filed whatever became of the old card");
    }

    /// A single absurd request is not a percentile shift and must not be
    /// folded into one — 27s on an upload chunk is one request going wrong.
    #[tokio::test]
    async fn absurd_single_requests_file_on_their_own() {
        let (st, _d) = state();
        let now = unix_now();
        log_row(&st, Row { ts: now - 50.0, method: "PUT", path: "/api/upload/abc123/chunk/7", family: "/api/upload", status: 200, body: "", worker: "", ua: "Mozilla/5.0", ms: 27_000.0 });
        let (f, _) = detect_latency(&st.store.read().unwrap(), now);
        let hit = f.iter().find(|x| x.signature.starts_with("latency|outlier|PUT"))
            .expect("a 27s request must file on its own: {f:?}");
        assert!(hit.title.contains("27.0s"), "the number belongs in the title: {}", hit.title);
        let ev: BTreeMap<_, _> = hit.evidence.iter().cloned().collect();
        assert!(ev["verdict"].contains("not a percentile shift"));
    }

    /// AMUX-3574 end to end: a card whose incident has resolved gets TOLD, and
    /// is left exactly where it was.
    ///
    /// Three cards were auto-filed to one lane in a single evening describing
    /// conditions that had already healed, each costing a full investigation to
    /// establish nothing was wrong. The incident row knew — `resolved_at` was
    /// set, once a minute before the pickup notice went out — and had no way to
    /// say which card to tell, because `board_issue` was never written by
    /// anything. 0 of 224 incidents carried a link.
    ///
    /// The `unknown` control is load-bearing. A check that STOPPED BEING ABLE
    /// TO RUN is not a check that passed, and a note claiming the condition
    /// "resolved" when it merely became unmeasurable would be the same false
    /// reassurance one layer up (AMUX-3575).
    #[tokio::test]
    async fn a_resolved_incident_tells_its_card_and_never_closes_it() {
        for (status, expect_word) in [("pass", "resolved"), ("unknown", "stopped being checkable")]
        {
            let (st, _d) = state();
            let card = "AF-9001";
            let (s2, c2) = (status.to_string(), card.to_string());
            st.store
                .write_async(move |conn| {
                    conn.execute(
                        "INSERT INTO issues (id,title,status,session,created,updated,archived) \
                         VALUES (?1,'filed by autofix','todo','amux',0,0,0)",
                        rusqlite::params![c2],
                    )?;
                    conn.execute(
                        "INSERT INTO _amux_invariant_incident \
                           (invariant_id, entity_key, status, first_seen, last_seen, occurrences, \
                            expected, observed, evidence, resolved_at, board_issue) \
                         VALUES ('schema.timestamp_units_declared','waitlist.ts',?1,1.0,2.0,19, \
                                 '','','{}',3.0,?2)",
                        rusqlite::params![s2, c2],
                    )?;
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                })
                .await
                .unwrap();

            let noted = note_resolved_incidents(&st).await.unwrap();
            assert_eq!(noted.len(), 1, "the card must be told ({status})");
            assert_eq!(noted[0].0, card);
            assert_eq!(noted[0].1, expect_word, "the terminal status must not be paraphrased");

            let (log, st_after): (Option<String>, String) = {
                let conn = st.store.read().unwrap();
                conn.query_row(
                    "SELECT log, status FROM issues WHERE id=?1",
                    rusqlite::params![card],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
            };
            let log = log.unwrap_or_default();
            assert!(log.contains(expect_word), "the log line must say WHICH terminal state: {log}");
            assert_eq!(
                st_after, "todo",
                "NOTED, never acted on — closing someone's card because the condition healed is \
                 an agent deciding their work is done (ethos rule 8)"
            );

            // Exactly-once: a second tick must not re-nag.
            let again = note_resolved_incidents(&st).await.unwrap();
            assert!(again.is_empty(), "a resolved incident must not re-nag every tick");
        }
    }

    /// AMUX-3574: an invariant card's re-check must query the surface that can
    /// answer it.
    ///
    /// This asserts on a STRING, which is usually the weak kind of test (a text
    /// mutation measures coupling, not correctness — AF-172). It is load-bearing
    /// here because the ENDPOINT IS the defect: /api/health/invariants reports
    /// FAILURES ONLY, so it returns the identical empty result for a check that
    /// healed and one that was never evaluated, and the old recipe printed
    /// "clean now" for both. Measured against the live server:
    ///
    ///   old, real invariant:  FAILING: 0 clean now
    ///   old, invented name:   FAILING: 0 clean now
    ///
    /// Byte-identical for a healthy check and a nonexistent one. Anything a card
    /// tells an agent to run must itself be exercised (AMUX-2140), and this one
    /// was not.
    #[test]
    fn an_invariant_recheck_asks_the_surface_where_a_pass_is_visible() {
        // Drives the real grouping path, per invariant_findings' own note.
        let f = invariant_findings(&[inv_row("schema.timestamp_units_declared", "waitlist.ts", 19)]);
        assert!(!f.is_empty(), "precondition: a finding must be produced to inspect");
        for finding in &f {
            assert!(
                finding.recheck.contains("/api/debug/invariants"),
                "the re-check must ask /api/debug/invariants — the only surface where a PASS \
                 is visible: {}",
                finding.recheck
            );
            assert!(
                !finding.recheck.contains("/api/health/invariants"),
                "and must NOT ask the failures-only surface, which cannot tell a healed check \
                 from one that never ran: {}",
                finding.recheck
            );
            assert!(
                finding.recheck.contains("NOT EVALUATED"),
                "an absent invariant must be reported as absent, not as clean — silence \
                 reading as health is the whole defect: {}",
                finding.recheck
            );
        }
    }

    /// AMUX-3580: a long-by-design route is suppressed AT its design duration
    /// and still reportable ABOVE it.
    ///
    /// The second half is the one that matters and the one an over-eager fix
    /// would destroy. Adding a route here to stop a nuisance card is easy; the
    /// value is that the route keeps ONE latency story it can still tell —
    /// "this hung", as against "this was big". A budget set to infinity, or a
    /// blanket exemption, silences both.
    #[test]
    fn a_long_by_design_route_keeps_the_one_latency_story_that_is_still_wrong() {
        // The measurement that set the number: three consecutive calls at
        // 10.6/11.1/10.7s, worst filed row 11,037ms.
        let budget = design_budget_ms("/api/board/commit-mentions");
        assert!(budget > 0.0, "the route must be enrolled at all");
        assert!(
            11_037.0 <= budget,
            "the measured worst legitimate run must be UNDER the budget, or the card that \
             motivated this keeps filing: worst 11037ms vs budget {budget}ms"
        );
        assert!(
            budget < 60_000.0,
            "and the budget must stay small enough to catch a hung git — the scan bounds its \
             OUTPUT but puts no wall clock on the subprocess, so this is the only thing that \
             would notice: {budget}ms"
        );

        // CONTROLS: enrolment is per-route and must not leak.
        assert_eq!(
            design_budget_ms("/api/board"),
            0.0,
            "the parent family must NOT inherit the budget — /api/board is an ordinary fast \
             route and silencing it would hide real regressions on the busiest board endpoint"
        );
        assert_eq!(
            design_budget_ms("/api/board/commit-mentions/extra"),
            0.0,
            "matching is exact, not prefix — a new sub-route must not inherit an exemption \
             nobody measured for it"
        );
    }

    /// AMUX-3591: re-scanning the SAME 5xx rows must mint the SAME signature,
    /// so a discarded report stays judged instead of refiling forever.
    ///
    /// THE LOOP THIS CLOSES, measured rather than imagined. One server hang
    /// filed AMUX-3581. Discarding it deleted the idem to re-arm the detector
    /// (board.rs, AF-137) — correct for a CONDITION, whose refile requires the
    /// condition to be live again. But the 5xx signature named only
    /// status+method+family+target, so "recurrence" meant "any 5xx on that path
    /// still inside the 6h window", and the SAME historical rows qualified.
    /// AMUX-3589 was filed. Discarding that filed AMUX-3591. Byte-identical
    /// signature every time, zero new information, and each round cost a lane a
    /// full scope-and-decide turn — driven by a worker doing exactly the right
    /// thing.
    ///
    /// The CONTROL is the half that matters: a genuinely NEW 5xx must still
    /// mint a new signature and file, or this fix trades a loop for a detector
    /// that goes quiet after its first discard.
    #[tokio::test]
    async fn rescanning_the_same_5xx_rows_mints_the_same_signature() {
        let sref = |st: &AppState| cards(st).first().map(|c| c.4.clone()).unwrap_or_default();

        let (st, _d) = state();
        let now = unix_now();
        for i in 0..4 {
            log_row(&st, Row { ts: now - 300.0 + i as f64, method: "GET", path: "/api/sessions",
                family: "/api/sessions", status: 500, body: "timed out waiting for connection",
                worker: "amux", ua: "curl/8.7.1", ms: 30_100.0 });
        }
        autofix_tick(&st, std::path::Path::new("/nonexistent")).await;
        let first = sref(&st);
        assert!(first.starts_with("autofix:5xx|"), "precondition: a 5xx card was filed: {first}");

        // Re-scan the identical rows, exactly as the next tick does while they
        // sit inside the window. Nothing new happened, so nothing new is news.
        let (st2, _d2) = state();
        for i in 0..4 {
            log_row(&st2, Row { ts: now - 300.0 + i as f64, method: "GET", path: "/api/sessions",
                family: "/api/sessions", status: 500, body: "timed out waiting for connection",
                worker: "amux", ua: "curl/8.7.1", ms: 30_100.0 });
        }
        autofix_tick(&st2, std::path::Path::new("/nonexistent")).await;
        assert_eq!(
            sref(&st2), first,
            "the same rows must mint the SAME signature — otherwise a discard re-arms and the \
             identical specimen comes back (AMUX-3581 -> 3589 -> 3591)"
        );

        // CONTROL: a NEWER 5xx is genuinely new news and must mint a DIFFERENT
        // signature, so the detector still fires after a discard.
        let (st3, _d3) = state();
        for i in 0..4 {
            log_row(&st3, Row { ts: now - 10.0 + i as f64, method: "GET", path: "/api/sessions",
                family: "/api/sessions", status: 500, body: "timed out waiting for connection",
                worker: "amux", ua: "curl/8.7.1", ms: 30_100.0 });
        }
        autofix_tick(&st3, std::path::Path::new("/nonexistent")).await;
        assert_ne!(
            sref(&st3), first,
            "a NEW occurrence must mint a NEW signature, or this trades a refile loop for a \
             detector that goes silent after one discard"
        );
    }

    /// AMUX-3588: a server-wide fault files ONE card, not one per endpoint.
    ///
    /// Rebuilt from the incident. The hang at 00:49-00:51 on 2026-08-24 filed
    /// four cards — /api/sessions twice (5xx and latency), /api/board/statuses,
    /// /api/board/session-gates — every offending row stamped 00:49:41 or
    /// 00:50:29 at exactly 30.1s, which is a timeout rather than an endpoint
    /// doing work. One of them then re-filed and cost a second lane-turn.
    ///
    /// The CONTROLS are the point and they are why this is not just "roll up
    /// when there are lots". Two slow endpoints must still file separately —
    /// rolling up a pair would hide ordinary independent regressions behind a
    /// vague systemic card, which is strictly worse than the fan-out it
    /// replaces.
    #[test]
    fn many_endpoints_slow_at_once_file_one_card_not_one_each() {
        let ts = 1_787_547_029.0;
        // The incident's own shape: three distinct targets, all at the 30.1s
        // timeout, in one tick.
        let hang: Vec<(&str, f64)> = vec![
            ("/api/sessions", 30_100.0),
            ("/api/board/statuses", 30_100.0),
            ("/api/board/session-gates", 30_100.0),
        ];
        let distinct = hang.iter().map(|(t, _)| *t).collect::<std::collections::BTreeSet<_>>();
        assert!(
            distinct.len() >= outlier_rollup_at(),
            "the recorded incident must reach the rollup threshold, or this fix does not \
             address the incident that motivated it: {} targets vs threshold {}",
            distinct.len(),
            outlier_rollup_at()
        );

        // CONTROL: two independent slow endpoints stay two cards. A rollup that
        // fires at 2 would erase the per-endpoint signal for ordinary
        // regressions, which is the failure mode of over-correcting here.
        let pair = ["/api/board", "/api/email/inbox"]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        assert!(
            pair.len() < outlier_rollup_at(),
            "two slow endpoints must NOT roll up — they are probably two faults"
        );

        // The threshold is a real knob and cannot be configured into nonsense:
        // a rollup of one is just a card with worse wording.
        assert!(outlier_rollup_at() >= 2, "floor of 2");

        // The rollup signature must key on the TARGET SET, so a different
        // collapse is different news while the same one stays idempotent. If it
        // keyed on an occurrence timestamp like the per-endpoint signature does,
        // a standing systemic fault would mint a new card every scan.
        let sig_a = format!("latency|outlier|ROLLUP|{}", {
            let mut v: Vec<&str> = distinct.iter().copied().collect();
            v.sort_unstable();
            v.join(",")
        });
        let sig_b = format!("latency|outlier|ROLLUP|{}", {
            let mut v: Vec<&str> = distinct.iter().copied().collect();
            v.sort_unstable();
            v.join(",")
        });
        assert_eq!(sig_a, sig_b, "the same target set must mint the same signature");
        assert!(
            !sig_a.contains(&format!("{}", ts as i64)),
            "the rollup signature must NOT carry an occurrence timestamp — a systemic fault is \
             a standing condition and would otherwise re-file on every scan"
        );
    }

    /// AMUX-3647: `ts` is the request START, so a row's own boot excludes on
    /// `ts < boot_at` and NEVER on latency arithmetic.
    ///
    /// THE FIXTURES ARE THE LIVE SPECIMENS, not shapes chosen to be convenient.
    /// Each `arrived_*` row below is one of the four rows the old expression
    /// excluded from `slow_outliers` in the 24h to 2026-08-24 18:00, with its
    /// real age-since-boot and its real latency. Every one of them was an
    /// ordinary slow request, and every one goes RED against the old code, which
    /// is what makes this cell discriminate rather than describe.
    ///
    /// It also pins the semantics that are currently UNREACHABLE in production
    /// (`ts < boot_at`, measured 0 of 97,019 rows). That is deliberate: the
    /// predicate says what it means, and if an inherited listener ever lets a
    /// request predate its process's boot, the behaviour is already specified
    /// and already tested rather than being re-derived under incident pressure.
    #[test]
    fn a_row_is_excluded_on_arrival_before_its_boot_never_on_latency() {
        let now = 1_787_000_000.0;
        let old_boot = now - 40_000.0; // ~11h ago, several restarts back
        let cur_boot = now - 300.0;    // the process running now

        // THE FOUR LIVE FALSE EXCLUSIONS. (age since its own boot, latency ms).
        // The last two are `GET /api/sessions-git` during the AMUX-3684
        // stampede: the filter was removing that defect's own evidence.
        for (age_s, ms, what) in [
            (2.06, 2_445.0, "POST /api/git/staged-guard 11:15:50"),
            (2.929, 3_326.0, "POST /api/git/staged-guard 14:38:48"),
            (14.601, 14_949.0, "GET /api/sessions-git 12:03:23 (AMUX-3684 stampede)"),
            (15.195, 15_577.0, "GET /api/sessions-git 12:03:23 (AMUX-3684 stampede)"),
        ] {
            let ts = old_boot + age_s;
            assert!(
                ts - ms / 1000.0 < old_boot,
                "premise: {what} is a row the OLD latency arithmetic excluded — if this ever \
                 fails the assertion below is not testing the fix"
            );
            assert!(
                !spans_own_restart(ts, ms, Some(old_boot), Some(cur_boot)),
                "{what} arrived {age_s}s AFTER its own boot and took {:.1}s. It is a slow \
                 request, not a restart, and dropping it under-reports in the direction \
                 nobody notices",
                ms / 1000.0
            );
        }

        // THE SEMANTICS, unreachable today and specified anyway.
        assert!(
            spans_own_restart(old_boot - 1.0, 500.0, Some(old_boot), Some(cur_boot)),
            "a row stamped BEFORE its own process booted is the only thing that spans it"
        );
        assert!(
            !spans_own_restart(old_boot, 500.0, Some(old_boot), Some(cur_boot)),
            "and the boundary is exclusive: arriving exactly at boot is not before it"
        );

        // ONE-SIDED IS SAFE HERE, and that is the point of carrying the row's
        // boot: `ts` is by construction at or after the boot of the process
        // that wrote it, so a legitimately old row cannot be excluded. The
        // process-boot form needed a second conjunct to avoid exactly this,
        // and omitting it dropped 213,420 of 213,935 rows.
        assert!(
            !spans_own_restart(old_boot + 3600.0, 500.0, Some(old_boot), Some(cur_boot)),
            "an ordinary fast request well after its own boot must be kept"
        );

        // LEGACY ROWS fall back rather than being read as "did not span".
        // Absence is not evidence, and this table retains a week of rows
        // written before the column existed.
        assert!(
            spans_own_restart(cur_boot + 14.0, 58_400.0, None, Some(cur_boot)),
            "a NULL boot_at must fall back to the process-boot comparison, not to a pass"
        );
        assert!(
            !spans_own_restart(cur_boot + 14.0, 58_400.0, None, None),
            "and with no boundary at all, keep the row — dropping everything on an unknown \
             boundary looks identical to a filter that worked"
        );
    }

    /// AMUX-3574: the signature parser, which is what maps a filed card back
    /// onto the incident that produced it.
    ///
    /// The negative cases are the point. Every other detector's signature also
    /// contains `|`, so a lax parser would link non-invariant findings to
    /// incidents that do not exist and write `board_issue` onto nothing (or
    /// worse, onto the wrong row).
    #[test]
    fn only_an_invariant_signature_maps_to_an_incident() {
        assert_eq!(
            invariant_signature_parts("invariant|queue.has_live_consumer|amux"),
            Some(("queue.has_live_consumer".into(), "amux".into()))
        );
        // An entity may itself contain `|` — the split is bounded at 3, so the
        // whole remainder is the entity rather than being truncated.
        assert_eq!(
            invariant_signature_parts("invariant|route.callers_have_routes|GET /api/x|y"),
            Some(("route.callers_have_routes".into(), "GET /api/x|y".into()))
        );

        // CONTROLS: other detectors' signatures must not match.
        for sig in [
            "5xx|500|POST|/api/quiet|/api/quiet",
            "latency|p95|/api/board",
            "dead-route|404|GET|/api/auth",
            "invariant|only-two-parts",
            "invariant||amux",
            "invariant|id|",
            "",
        ] {
            assert_eq!(
                invariant_signature_parts(sig),
                None,
                "non-invariant or malformed signature must not map to an incident: {sig:?}"
            );
        }
    }

    /// AMUX-3633: the signature carries an EPISODE suffix, and the parser must
    /// strip it or the incident link breaks for every invariant card.
    ///
    /// The entity itself may contain `|` (route invariants name
    /// `GET /api/x|y`), and the split is bounded at 3, so a naive parse absorbs
    /// the timestamp into the entity and silently mis-keys the link. That is
    /// invisible: the card still files, it just points at no incident.
    #[test]
    fn the_episode_suffix_is_stripped_and_a_piped_entity_survives_it() {
        // Plain entity + episode.
        assert_eq!(
            invariant_signature_parts("invariant|queue.has_live_consumer|amux|1787569200"),
            Some(("queue.has_live_consumer".into(), "amux".into()))
        );
        // Entity that CONTAINS a pipe, plus the episode. Both must survive.
        assert_eq!(
            invariant_signature_parts("invariant|route.callers_have_routes|GET /api/x|y|1787569200"),
            Some(("route.callers_have_routes".into(), "GET /api/x|y".into()))
        );
        // No episode suffix (a pre-3633 signature, or a rollup shape): unchanged.
        assert_eq!(
            invariant_signature_parts("invariant|queue.has_live_consumer|amux"),
            Some(("queue.has_live_consumer".into(), "amux".into()))
        );
        // CONTROL: a NON-numeric trailing field is part of the entity, not an
        // episode. Without this the stripper would eat a real entity segment.
        assert_eq!(
            invariant_signature_parts("invariant|route.callers_have_routes|GET /api/x|y"),
            Some(("route.callers_have_routes".into(), "GET /api/x|y".into()))
        );
    }

    /// Silence is evidence, not a verdict. The card must be ANNOTATED and left
    /// exactly where it was — an auto-close here would be an agent deciding a
    /// bug was fixed because nobody hit it (ethos rule 8).
    ///
    /// Deliberately drives `file_finding` + `note_quiet_signatures` directly
    /// rather than steering the tick with env vars: the first version of this
    /// test set AMUX_AUTOFIX_WINDOW_H, and since cargo runs tests as threads in
    /// ONE process that leaked into the latency test running beside it and
    /// failed it. A test that reaches for a process-global to set up its
    /// fixture is testing the other tests too.
    #[tokio::test]
    async fn a_quiet_signature_is_noted_never_closed() {
        let (st, _d) = state();
        let now = unix_now();
        // A filed card whose signature has produced NO request-log rows at all
        // — the purest form of "stopped happening".
        let f = Finding {
            kind: DetectorKind::Http5xx,
            signature: "5xx|500|POST|/api/quiet|/api/quiet".into(),
            title: "POST /api/quiet → HTTP 500 (3x)".into(),
            evidence: vec![("verdict".into(), "it broke".into())],
            recheck: "curl -sk $AMUX_URL/api/logs/analyze".into(),
            owner: None,
            count: 3,
            last_ts: now - 86_400.0,
            parked_until: None,
        };
        let id = file_finding(&st, &f).await.unwrap().expect("card filed");

        let noted = note_quiet_signatures(&st, now, &[]).await.unwrap();
        assert_eq!(noted.len(), 1, "a quiet signature must be noted: {noted:?}");

        let conn = st.store.read().unwrap();
        let (status, log): (String, Option<String>) = conn
            .query_row("SELECT status, log FROM issues WHERE id=?1", rusqlite::params![id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(status, "todo", "the card must NOT be closed or moved");
        let log = log.unwrap_or_default();
        assert!(log.contains("STOPPED HAPPENING IS NOT FIXED"), "the note must say what it means: {log}");
        drop(conn);

        // And it must not re-nag: a second sweep adds nothing. A note that
        // fires every tick forever is its own well-documented failure here.
        let again = note_quiet_signatures(&st, now, &[]).await.unwrap();
        assert!(again.is_empty(), "the quiet note must be exactly-once, got {again:?}");
    }

    /// Every detector's item type must be closable honestly — i.e. never
    /// `code`, whose gates demand a merged commit and passing tests that an
    /// auto-filed report does not have.
    #[test]
    fn no_detector_files_a_code_card() {
        for k in DetectorKind::all() {
            assert_ne!(k.item_type(), "code", "{k:?} would be gated on a merge it cannot claim");
            assert!(!k.slug().is_empty());
        }
    }

    // -----------------------------------------------------------------------
    // CI / deploy detector (AMUX-2780).
    //
    // These drive `ci_findings` (the pure decision) and `file_finding` (the
    // REAL filing path, dedupe included) rather than `autofix_tick`, because
    // the tick's CI input is a `gh` subprocess. Testing through the tick would
    // mean either hitting GitHub — which makes the suite non-deterministic and
    // makes a red suite mean "the network is down" — or mocking the tick, which
    // proves nothing about the code that ships.
    // -----------------------------------------------------------------------

    /// A run fixture. `mins_ago` keeps the timeline readable at the call site;
    /// getting the ORDER wrong is how a streak test silently measures nothing.
    fn ci_run(wf: &str, id: i64, conclusion: &str, mins_ago: f64) -> CiRun {
        CiRun {
            repo: "mixpeek/amux".into(),
            workflow: wf.into(),
            run_id: id,
            status: "completed".into(),
            conclusion: conclusion.into(),
            event: "push".into(),
            branch: "main".into(),
            on_default_branch: true,
            created_at: unix_now() - mins_ago * 60.0,
            url: format!("https://github.com/mixpeek/amux/actions/runs/{id}"),
            failing_step: None,
        }
    }

    fn card_row(st: &AppState, id: &str) -> (String, String, String) {
        let conn = st.store.read().unwrap();
        conn.query_row(
            "SELECT status, COALESCE(log,''), COALESCE(source_ref,'') FROM issues WHERE id=?1",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    /// THE test the whole card exists for. `deploy-cloud.yml` failed 31
    /// consecutive times over three days and produced NOTHING anyone could see.
    /// If the detector is removed, misfiltered, or its threshold is set above
    /// the streak, this fails — which is the property "nobody saw 29 failures"
    /// needed and did not have.
    #[tokio::test]
    async fn a_failing_workflow_produces_a_card_with_its_streak() {
        let (st, _d) = state();
        let mut runs = vec![ci_run("Deploy to cloud.amux.io", 31_182_322_406, "success", 4400.0)];
        for i in 0..31 {
            let mut r = ci_run("Deploy to cloud.amux.io", 31_200_000_000 + i, "failure", 4000.0 - i as f64 * 100.0);
            if i == 30 {
                r.failing_step = Some("deploy / Gateway env — admin emails".into());
            }
            runs.push(r);
        }
        let (f, _s) = ci_findings(&runs, unix_now());
        assert_eq!(f.len(), 1, "31 failures of one workflow are ONE incident, got {}", f.len());
        let ev: BTreeMap<&str, &str> =
            f[0].evidence.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(ev["consecutive_failures"], "31", "the count is the whole point");
        assert!(ev["first_failure_at"].contains("31200000000"), "first-seen names the run: {ev:?}");
        assert!(ev["latest_run_url"].contains("31200000030"), "run URL must be the NEWEST failure");
        assert!(ev["failing_step"].contains("Gateway env"), "the failing STEP is required evidence");
        assert!(ev["last_success"].contains("31182322406"), "last green run belongs on the card");
        assert!(f[0].recheck.contains("--log-failed"), "recheck must be runnable: {}", f[0].recheck);

        // …and it reaches the board through the real filing path.
        let card = file_finding(&st, &f[0]).await.unwrap().expect("a finding must file a card");
        let (status, _log, sref) = card_row(&st, &card);
        assert_eq!(status, "todo", "a filed fault is queued, not pre-closed");
        assert_eq!(sref, format!("autofix:{}", f[0].signature));
        let c = cards(&st);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].3, "blocker", "a red deploy is a blocker, not a code card");
        assert!(c[0].1.contains("31x"), "count belongs in the computed title: {}", c[0].1);
    }

    /// Dedupe, durably. A nightly failure files ONCE. This is the property that
    /// separates a detector from a nightly spam machine, and the one an
    /// in-memory dedupe passes in-process and fails in production on every
    /// re-exec.
    #[tokio::test]
    async fn a_nightly_failure_files_once_not_nightly() {
        let (st, _d) = state();
        let base = vec![
            ci_run("rust", 900, "success", 5000.0),
            ci_run("rust", 901, "failure", 400.0),
            ci_run("rust", 902, "failure", 300.0),
        ];
        let f1 = ci_findings(&base, unix_now()).0;
        assert_eq!(f1.len(), 1);
        assert!(file_finding(&st, &f1[0]).await.unwrap().is_some(), "first sighting files");

        // Night 2 and night 3: the SAME outage, more failures. Same anchor,
        // same signature, no new card.
        let mut later = base.clone();
        later.push(ci_run("rust", 903, "failure", 200.0));
        later.push(ci_run("rust", 904, "failure", 100.0));
        let f2 = ci_findings(&later, unix_now()).0;
        assert_eq!(f2[0].signature, f1[0].signature, "the streak anchor must not move mid-outage");
        assert!(file_finding(&st, &f2[0]).await.unwrap().is_none(), "night 2 must not refile");

        // A RESTART: new AppState over the same DB, in-memory report cleared.
        *last_report_cell().write().unwrap() = None;
        let restarted = AppState {
            store: st.store.clone(),
            started: std::time::Instant::now(),
            build_hash: "after-restart".into(),
            auth_token: None,
        };
        assert!(
            file_finding(&restarted, &f2[0]).await.unwrap().is_none(),
            "a restart refiled the same outage — the dedupe is not durable"
        );
        assert_eq!(cards(&st).len(), 1, "three nights and a restart are ONE card");
    }

    /// The other half of dedupe, which a workflow-keyed signature gets wrong:
    /// a workflow that is FIXED and breaks again later must file again. A key
    /// that never changes goes silent forever after the first fix.
    #[test]
    fn a_new_outage_after_a_green_run_is_a_new_card() {
        let old = vec![
            ci_run("rust", 80, "success", 5000.0),
            ci_run("rust", 90, "failure", 4000.0),
            ci_run("rust", 100, "failure", 3000.0),
        ];
        let sig_old = ci_findings(&old, unix_now()).0[0].signature.clone();

        let mut new = old.clone();
        new.push(ci_run("rust", 150, "success", 2000.0)); // fixed
        new.push(ci_run("rust", 200, "failure", 1000.0)); // …and broke again
        new.push(ci_run("rust", 210, "failure", 500.0));
        let sig_new = ci_findings(&new, unix_now()).0[0].signature.clone();

        assert_ne!(sig_old, sig_new, "a second outage must not be deduped against the first");
        assert!(sig_new.ends_with("|200"), "the anchor is the streak's OLDEST failure: {sig_new}");
    }

    /// "Stopped failing is not fixed." A green run appends a line and changes
    /// NOTHING else — not the status, not the archive flag. A detector that
    /// auto-closes on green declares a fix it did not verify, and the one time
    /// that is wrong is the time the failing step was quietly deleted.
    #[tokio::test]
    async fn a_green_run_is_noted_never_closed() {
        let (st, _d) = state();
        let broken = vec![
            ci_run("checks", 10, "success", 5000.0),
            ci_run("checks", 11, "failure", 400.0),
            ci_run("checks", 12, "failure", 300.0),
        ];
        let f = ci_findings(&broken, unix_now()).0;
        let card = file_finding(&st, &f[0]).await.unwrap().unwrap();

        // While red, there is nothing to say.
        let noted = note_quiet_signatures(&st, unix_now(), &broken).await.unwrap();
        assert!(noted.is_empty(), "a still-failing workflow must not be reported as recovered");

        let mut green = broken.clone();
        green.push(ci_run("checks", 13, "success", 10.0));
        let noted = note_quiet_signatures(&st, unix_now(), &green).await.unwrap();
        assert_eq!(noted.len(), 1, "a green run must be NOTED: {noted:?}");

        let (status, log, _) = card_row(&st, &card);
        assert_eq!(status, "todo", "green must not close, move or resolve the card");
        assert!(log.contains("GREEN again"), "the note must be on the card: {log}");
        assert!(log.contains("STOPPED FAILING IS NOT FIXED"), "and must say what green does NOT prove");

        // Exactly once, forever — a recovery that re-nags every tick is its
        // own well-documented failure in this repo.
        let again = note_quiet_signatures(&st, unix_now(), &green).await.unwrap();
        assert!(again.is_empty(), "the green note must be exactly-once, got {again:?}");
    }

    /// The suppressions the card asked for, and the rule that a decision NOT to
    /// file is published rather than silent.
    #[test]
    fn cancelled_and_pull_request_runs_are_not_evidence() {
        let now = unix_now();

        // A cancelled run is the NEWEST. It must neither file (it is not a
        // fault) nor reset the streak behind it (the 08-10 16:42 cancel sat in
        // the middle of a 31-failure outage).
        let mut runs = vec![
            ci_run("Deploy to cloud.amux.io", 1, "success", 900.0),
            ci_run("Deploy to cloud.amux.io", 2, "failure", 800.0),
            ci_run("Deploy to cloud.amux.io", 3, "failure", 700.0),
        ];
        runs.push(ci_run("Deploy to cloud.amux.io", 4, "cancelled", 600.0));
        let (f, s) = ci_findings(&runs, now);
        assert_eq!(f.len(), 1, "a cancelled run must not mask the outage behind it");
        assert!(f[0].signature.ends_with("|2"), "cancel must not move the anchor: {}", f[0].signature);
        assert!(
            s.iter().any(|x| x.reason.contains("cancelled")),
            "the decision to ignore a cancelled run must be PUBLISHED: {s:?}"
        );

        // A fork/PR run that fails is somebody else's branch, not main's CI.
        let mut pr = ci_run("rust", 50, "failure", 10.0);
        pr.event = "pull_request".into();
        pr.on_default_branch = false;
        pr.branch = "contributor:patch-1".into();
        let mut pr2 = pr.clone();
        pr2.run_id = 51;
        let (f, s) = ci_findings(&[pr, pr2], now);
        assert!(f.is_empty(), "PR/fork runs must never file against main: {f:?}");
        assert!(
            s.iter().any(|x| x.reason.contains("pull-request")),
            "the exclusion must be visible, not silent: {s:?}"
        );

        // One red run is below the threshold — and says so, rather than
        // vanishing. An unexplained non-filing is the failure this file exists
        // to end.
        let one = vec![ci_run("rust", 60, "success", 900.0), ci_run("rust", 61, "failure", 10.0)];
        let (f, s) = ci_findings(&one, now);
        assert!(f.is_empty());
        assert!(
            s.iter().any(|x| x.reason.contains("AMUX_CI_MIN_FAILURES")),
            "a sub-threshold streak must name the threshold that held it back: {s:?}"
        );
    }

    /// `autofix_tick` must not reach the network. Written against the artifact
    /// of a real regression: when the CI fetch lived inside the tick, a sibling
    /// integration test that logged seven 5xx rows into a TEMP database came
    /// back with five cards, four of them about mixpeek/amux's live CI, and
    /// failed on `left: 5, right: 1`. Any caller holding a synthetic database
    /// would have inherited the same surprise.
    ///
    /// The two assertions flip together the moment the fetch moves back in:
    /// live runs would put `ci` signatures in `signatures_seen`, and the
    /// "nobody looked" suppression would disappear.
    #[tokio::test]
    async fn the_plain_tick_does_not_reach_github() {
        let (st, _d) = state();
        let r = autofix_tick(&st, std::path::Path::new("/nonexistent")).await;
        assert!(
            !r.signatures_seen.iter().any(|(k, _)| k == "ci"),
            "autofix_tick fetched live CI: {:?}",
            r.signatures_seen
        );
        assert!(r.filed.is_empty(), "a temp DB and no traffic must file nothing: {:?}", r.filed);
        // …and the absence is REPORTED, not silent: "no findings" and "nobody
        // looked" are different facts and the debug surface must tell them
        // apart (ethos rule 4).
        assert!(
            r.suppressed.iter().any(|(k, _, why)| k == "ci" && why.contains("absence of DATA")),
            "a tick with no CI input must say so rather than imply CI is green: {:?}",
            r.suppressed
        );
    }

    /// The probe against the REAL incident, run by hand.
    ///
    /// `#[ignore]` and honest about why: it shells out to `gh` and talks to
    /// GitHub, so in CI it would be a test that fails when the network hiccups
    /// or the token expires — and a check that goes red for reasons unrelated
    /// to the code is a check people learn to ignore. It is NOT decoration:
    /// this is the only thing that exercises `fetch_ci_runs`' parsing, the
    /// default-branch comparison and the failing-step API call against real
    /// GitHub JSON, none of which a fixture can prove. Run it after touching
    /// any of those:
    ///
    /// ```text
    /// cargo test -p amux-server --lib ci_detector_against_live_github -- --ignored --nocapture
    /// ```
    ///
    /// First run, 2026-08-10: reported `Deploy to cloud.amux.io` at 31
    /// consecutive failures since 2026-08-07, plus `rust` and `checks` — the
    /// exact outage that went unseen for three days.
    #[tokio::test]
    #[ignore = "hits live GitHub via the gh CLI; manual probe, not a CI gate"]
    async fn ci_detector_against_live_github() {
        *ci_cache().write().unwrap() = None;
        let now = unix_now();
        let (runs, sup) = fetch_ci_runs(now).await;
        println!("fetched {} runs; {} suppression(s)", runs.len(), sup.len());
        for s in &sup {
            println!("  SUPPRESSED {}: {}", s.signature, s.reason);
        }
        assert!(!runs.is_empty(), "gh returned nothing — is it installed and authenticated?");
        let (f, s2) = ci_findings(&runs, now);
        for x in &s2 {
            println!("  SUPPRESSED {}: {}", x.signature, x.reason);
        }
        for x in &f {
            println!("\n=== WOULD FILE: {}\nsignature: {}", x.title, x.signature);
            println!("{}", render_desc(x));
        }
    }

    /// Blindness must not read as an all-clear. This is the failure mode unique
    /// to the one detector whose instrument is off-machine: `gh` missing,
    /// logged out, rate-limited or offline all produce zero runs, which is
    /// byte-identical to a healthy CI unless the reason is published.
    #[test]
    fn no_runs_is_not_the_same_as_no_failures() {
        let (f, s) = ci_findings(&[], unix_now());
        assert!(f.is_empty());
        assert!(s.is_empty(), "an empty list alone says nothing — the REASON comes from the fetch");
        // The fetch is what knows why the list is empty, and it emits a
        // Suppressed naming the cause; `ci_findings` cannot and must not guess.
        // Guarded here so nobody later "helpfully" makes an empty list file a
        // card, which would fire on every network blip.
    }

    // -----------------------------------------------------------------------
    // FD pressure (AMUX-2812). The corpus is the 2026-08-10 EMFILE incident.
    // -----------------------------------------------------------------------

    /// The corpus is the outage this card exists for: "Deploy to cloud.amux.io",
    /// 32 consecutive failures since 2026-08-07 — a card was filed, sat unread
    /// in `todo`, and the outage ran into its fourth day.
    #[test]
    fn a_multi_day_deploy_outage_escalates_past_the_card() {
        let now = unix_now();
        let runs: Vec<CiRun> = (0..32)
            .map(|i| ci_run("Deploy to cloud.amux.io", i, "failure", 60.0 + i as f64 * 150.0))
            .collect();
        let esc = ci_deploy_escalations(&runs, now);
        assert_eq!(esc.len(), 1, "the deploy outage must escalate");
        assert_eq!(esc[0].2, 32, "streak");
        assert!(esc[0].3 >= 24.0, "hours red: {}", esc[0].3);
    }

    /// BOTH conditions, because each alone is wrong in a different direction.
    #[test]
    fn a_busy_hour_is_not_a_multi_day_outage_and_a_slow_trickle_is() {
        let now = unix_now();
        // 12 failures inside one hour — over the streak bar, under the time bar.
        let burst: Vec<CiRun> = (0..12)
            .map(|i| ci_run("Deploy to cloud.amux.io", i, "failure", i as f64 * 3.0))
            .collect();
        assert!(ci_deploy_escalations(&burst, now).is_empty(), "a burst is not an outage");
        // 3 failures over a week — over the time bar, under the streak bar.
        let trickle: Vec<CiRun> = (0..3)
            .map(|i| ci_run("Deploy to cloud.amux.io", i, "failure", i as f64 * 2880.0))
            .collect();
        assert!(ci_deploy_escalations(&trickle, now).is_empty(), "3 reds is a card, not an alarm");
    }

    #[test]
    fn a_green_run_ends_the_streak_and_a_test_suite_is_never_a_deploy() {
        let now = unix_now();
        let mut runs: Vec<CiRun> = (0..30)
            .map(|i| ci_run("Deploy to cloud.amux.io", i, "failure", 120.0 + i as f64 * 180.0))
            .collect();
        // A success NEWER than all of them: the deploy recovered.
        runs.push(ci_run("Deploy to cloud.amux.io", 999, "success", 30.0));
        assert!(ci_deploy_escalations(&runs, now).is_empty(), "a green run ends it");

        // The narrowness that keeps this from becoming a general CI alarm: a red
        // test suite is bad and gets a card, but shipped work is still shipping.
        let tests: Vec<CiRun> = (0..40)
            .map(|i| ci_run("rust", i, "failure", 60.0 + i as f64 * 180.0))
            .collect();
        assert!(ci_deploy_escalations(&tests, now).is_empty(), "'rust' is not a deploy workflow");
        assert!(ci_is_deploy_workflow("Deploy to cloud.amux.io"));
        assert!(ci_is_deploy_workflow("release-please"));
        assert!(!ci_is_deploy_workflow("rust"));
        assert!(!ci_is_deploy_workflow("checks"));
    }

    /// Cancelled runs carry no evidence and must not break the streak — the same
    /// rule the card detector already applies, restated here because this is a
    /// SECOND streak walker and two spellings drift.
    #[test]
    fn a_cancelled_run_mid_outage_does_not_reset_the_escalation() {
        let now = unix_now();
        let mut runs: Vec<CiRun> = (0..20)
            .map(|i| ci_run("Deploy to cloud.amux.io", i, "failure", 120.0 + i as f64 * 180.0))
            .collect();
        runs.push(ci_run("Deploy to cloud.amux.io", 999, "cancelled", 60.0));
        let esc = ci_deploy_escalations(&runs, now);
        assert_eq!(esc.len(), 1, "a cancelled run must not look like a recovery");
        assert_eq!(esc[0].2, 20, "and must not count toward the streak either");
    }

    /// THE INCIDENT'S OWN NUMBERS: 220 descriptors against macOS's launchd
    /// default of 256. The convenient fixture would be a round "80% of 1000",
    /// and it is convenient precisely because it lacks the property that made
    /// the incident — a limit nobody knew was in force.
    #[test]
    fn the_emfile_incident_fires_and_the_same_count_at_the_fixed_limit_does_not() {
        let ceiling = 0.7;
        let rise = 0.2;
        let incident = 220.0 / 256.0; // 0.859 — what was actually happening
        assert_eq!(
            fd_trigger(incident, 0.0, ceiling, rise),
            Some("near-limit"),
            "220/256 must fire: this is the state in which /health went silent"
        );
        // THE WHOLE REASON THIS IS A RATIO. The plist fix moved the limit to
        // 65536; the same 220 descriptors are now unremarkable. A count-based
        // floor would have to be rechosen by hand every time the limit changes,
        // and a floor nobody rechooses stops meaning anything silently.
        assert_eq!(
            fd_trigger(220.0 / 65536.0, 0.0, ceiling, rise),
            None,
            "220/65536 must NOT fire — otherwise the detector nags forever after the fix"
        );
    }

    #[test]
    fn a_leak_is_caught_climbing_before_it_reaches_the_ceiling() {
        // Well under the ceiling, but gaining 25 ratio-points per hour: at that
        // rate it is roughly two hours from EMFILE. An absolute ceiling cannot
        // say anything until it is already too late, which is exactly how the
        // incident arrived with no warning.
        assert_eq!(fd_trigger(0.20, 0.25, 0.7, 0.2), Some("climbing"));
        // A flat fleet at the same level is silent — the detector must not
        // report that the process is merely RUNNING (the ethos-7 spin-catcher:
        // a threshold below the baseline is not a detector).
        assert_eq!(fd_trigger(0.20, 0.0, 0.7, 0.2), None);
        assert_eq!(fd_trigger(0.69, 0.0, 0.7, 0.2), None, "just under the ceiling, not climbing");
    }

    /// When both fire, "you are nearly out" beats "you are heading for nearly
    /// out" — two cards for one condition is how a board stops being read.
    #[test]
    fn the_ceiling_wins_over_the_rate_when_both_trigger() {
        assert_eq!(fd_trigger(0.95, 0.9, 0.7, 0.2), Some("near-limit"));
    }

    // -----------------------------------------------------------------------
    // Invariant rollup (AMUX-2814). Corpus: the live board on 2026-08-10, where
    // `route.callers_have_routes` had produced 64 cards — one per route, all
    // quoting the same 1857 occurrences, 36% of every open item.
    // -----------------------------------------------------------------------

    fn inv_row(id: &str, entity: &str, occ: i64) -> InvRow {
        (id.into(), entity.into(), "fail".into(), 1000.0, 2000.0, occ, "expected".into(), "observed".into(), String::new())
    }

    /// The same row with a DECLARED self-heal epoch, the shape a dwell-window
    /// check produces (`InvariantResult::heals_at`, AMUX-3645).
    fn inv_row_healing_at(id: &str, entity: &str, occ: i64, heals_at: f64) -> InvRow {
        let mut r = inv_row(id, entity, occ);
        r.8 = json!({"heals_at": heals_at}).to_string();
        r
    }

    /// Drives `detect_invariants` through a real SQLite table, because the
    /// grouping happens between the query and the Finding — a test that called
    /// a pure helper would skip the part that broke.
    fn invariant_findings(rows: &[InvRow]) -> Vec<Finding> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            // `evidence` mirrors the real schema (see the migration and
            // store.rs's upsert). It carries a dwell check's declared heal
            // epoch, which is what the filer reads to park a card instead of
            // queueing it (AMUX-3645); a fixture without the column would make
            // every dwell cell below unrunnable rather than red, which is the
            // worse of the two failures.
            "CREATE TABLE _amux_invariant_incident (invariant_id TEXT, entity_key TEXT, \
             status TEXT, first_seen REAL, last_seen REAL, occurrences INTEGER, \
             resolved_at REAL, expected TEXT, observed TEXT, evidence TEXT);",
        )
        .unwrap();
        for r in rows {
            conn.execute(
                "INSERT INTO _amux_invariant_incident VALUES (?1,?2,?3,?4,?5,?6,NULL,?7,?8,?9)",
                rusqlite::params![r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8],
            )
            .unwrap();
        }
        detect_invariants(&conn, 9999.0).0
    }

    #[test]
    fn one_invariant_failing_across_many_entities_is_one_card_not_many() {
        // The real shape: 64 routes, one invariant, same occurrence count.
        let rows: Vec<_> = (0..64)
            .map(|i| inv_row("route.callers_have_routes", &format!("GET /api/thing{i}"), 1857))
            .collect();
        let f = invariant_findings(&rows);
        assert_eq!(f.len(), 1, "64 entities of ONE invariant must produce ONE card, got {}", f.len());
        // Keyed on the INVARIANT, not an entity. AMUX-3633 appended an episode
        // suffix, so this can no longer be an `ends_with` — asserted as the
        // ROLLUP marker plus the property the suffix exists for, rather than
        // relaxed to a substring that would pass against either shape.
        let sig = &f[0].signature;
        assert!(
            sig.starts_with("invariant|route.callers_have_routes|ROLLUP|"),
            "keyed on the invariant, not an entity: {sig}"
        );
        let episode = sig.rsplit('|').next().unwrap_or("");
        assert!(
            episode.parse::<i64>().is_ok() && !episode.is_empty(),
            "the episode suffix must be a bare epoch second, got {episode:?} — without it a \
             judged breach refiles on every scan while the SAME failure run continues"
        );
        assert!(f[0].title.contains("64 entities"), "the count belongs in the title: {}", f[0].title);
        // The entity list must SURVIVE into the card — rolling up must not
        // destroy the information the 64 cards carried, or this trades an
        // unreadable board for an unactionable one.
        let ev: String = f[0].evidence.iter().map(|(k, v)| format!("{k}{v}")).collect();
        assert!(ev.contains("GET /api/thing0") && ev.contains("GET /api/thing63"),
                "every entity must still be named in the evidence");
    }

    /// AMUX-3645, rebuilt from AMUX-3640's own artifact.
    ///
    /// That card read "has been failing for panic-base+socd-...panic across
    /// 5301 evaluations and has not self-healed". Both clauses are TRUE and
    /// both read as a fault getting worse. It was a 7-day dwell window working
    /// exactly as designed, and establishing that took reading checks.rs. The
    /// discriminator the card could not express is the check's own declared
    /// heal epoch.
    ///
    /// The specimen numbers are the real ones: `invariant_findings` evaluates
    /// at now=9999, so a heal at 99999 is a live dwell.
    #[test]
    fn a_dwell_window_card_says_when_the_clock_runs_out_instead_of_how_bad_it_is() {
        let rows = vec![inv_row_healing_at(
            "host.no_fresh_kernel_panic",
            "panic-base+socd-2026-08-19-210001.panic",
            5301,
            99_999.0,
        )];
        let f = invariant_findings(&rows);
        assert_eq!(f.len(), 1);

        // The park is what keeps a lane from being handed work it cannot do.
        assert_eq!(f[0].parked_until, Some(99_999.0), "{:?}", f[0]);

        let verdict: String =
            f[0].evidence.iter().filter(|(k, _)| k == "verdict").map(|(_, v)| v.clone()).collect();
        assert!(verdict.contains("DWELL WINDOW"), "verdict must name the shape: {verdict}");
        assert!(
            !verdict.contains("has not self-healed"),
            "the misleading clause from AMUX-3640 must be gone: {verdict}"
        );
        // The count survives as CONTEXT (the occurrences row still carries it)
        // but must not be the headline, which is what made 5301 read as alarm.
        assert!(
            !f[0].title.contains("5301"),
            "the dwell title must not wear the evaluation count as severity: {}",
            f[0].title
        );
        assert!(
            f[0].evidence.iter().any(|(k, v)| k == "occurrences" && v == "5301"),
            "the count is still recorded, just not as the headline: {:?}",
            f[0].evidence
        );
    }

    /// The negative control, and the one that matters most: an ordinary
    /// failure must be untouched. A change that parked EVERY invariant card
    /// would pass the cell above and silently drain the board.
    #[test]
    fn an_ordinary_invariant_breach_is_not_parked() {
        let f = invariant_findings(&[inv_row("some.invariant", "entity-a", 50)]);
        assert_eq!(f[0].parked_until, None, "{:?}", f[0]);
        let verdict: String =
            f[0].evidence.iter().filter(|(k, _)| k == "verdict").map(|(_, v)| v.clone()).collect();
        assert!(verdict.contains("has not self-healed"), "{verdict}");
        assert!(f[0].title.contains("(50x)"), "{}", f[0].title);
    }

    /// A heal epoch in the PAST is a fault, not a park — the loudest kind,
    /// because the check declared it would be over by now and it is not.
    /// Without this the filer would file "parked until <a date behind us>",
    /// which is the exact shape of a card nobody ever picks up again.
    #[test]
    fn a_lapsed_heal_declaration_files_as_an_ordinary_fault() {
        let f = invariant_findings(&[inv_row_healing_at(
            "host.no_fresh_kernel_panic",
            "old.panic",
            5301,
            1.0, // long before the fixture's now=9999
        )]);
        assert_eq!(f[0].parked_until, None, "a lapsed dwell must not park: {:?}", f[0]);
        let verdict: String =
            f[0].evidence.iter().filter(|(k, _)| k == "verdict").map(|(_, v)| v.clone()).collect();
        assert!(verdict.contains("has not self-healed"), "{verdict}");
    }

    /// Malformed evidence must degrade to the ordinary path, never panic and
    /// never park. The column is free text written by whatever a check passed
    /// to `.evidence()`, and a filer that unwrapped it would take the whole
    /// autofix tick down on one bad row.
    #[test]
    fn unparseable_evidence_falls_back_to_an_ordinary_fault() {
        let mut row = inv_row("some.invariant", "entity-a", 50);
        row.8 = "{not json".into();
        assert_eq!(invariant_findings(&[row])[0].parked_until, None);

        let mut scalar = inv_row("some.invariant", "entity-b", 50);
        scalar.8 = "42".into();
        assert_eq!(invariant_findings(&[scalar])[0].parked_until, None);
    }

    #[test]
    fn a_few_entities_still_get_their_own_cards() {
        // Below the threshold, per-entity is right: each is individually
        // actionable and the entity is the useful title. A rollup that fired at
        // 2 would hide ordinary work behind a summary.
        let rows = vec![
            inv_row("some.invariant", "entity-a", 50),
            inv_row("some.invariant", "entity-b", 50),
        ];
        assert_eq!(invariant_findings(&rows).len(), 2);
    }

    #[test]
    fn distinct_invariants_never_roll_up_together() {
        // Grouping is by invariant. Five different invariants failing once each
        // is five faults, not one — collapsing them would be the same defect
        // pointed the other way.
        let rows: Vec<_> = (0..5).map(|i| inv_row(&format!("inv.{i}"), "same-entity", 99)).collect();
        assert_eq!(invariant_findings(&rows).len(), 5);
    }

    /// The flap threshold still governs: entities below `min_occurrences` must
    /// not be able to push an invariant over the rollup line, or a noisy
    /// invariant would roll itself up out of rows that were each suppressed.
    #[test]
    fn suppressed_entities_do_not_count_toward_the_rollup() {
        let mut rows: Vec<_> = (0..10)
            .map(|i| inv_row("noisy.invariant", &format!("e{i}"), 1))
            .collect();
        rows.push(inv_row("noisy.invariant", "real", 500));
        let f = invariant_findings(&rows);
        assert_eq!(f.len(), 1, "only the one entity above the flap threshold files");
        assert!(!f[0].signature.ends_with("|ROLLUP"), "one real breach is a per-entity card, not a rollup");
    }

    /// The measurement path must work on THIS machine, or the pure logic above
    /// is being tested in a vacuum. Both are cheap and neither spawns a
    /// process — which is the point: `lsof` would fail exactly when the fault
    /// it reports is present.
    #[test]
    fn the_descriptor_measurement_works_without_spawning_anything() {
        let open = open_fd_count().expect("/dev/fd must be readable");
        let limit = fd_limit().expect("getrlimit(RLIMIT_NOFILE) must answer");
        assert!(open > 0, "a running test process holds at least stdin/stdout/stderr");
        assert!(limit > 0, "RLIMIT_NOFILE must be a real limit");
        assert!(
            (open as u64) < limit,
            "a healthy test process is under its own limit ({open}/{limit})"
        );
    }
}
