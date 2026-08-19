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
/// Sample floor, both sides. Below this a p95 is one request wearing a
/// percentile — the "filter that matched everything" failure in slow motion.
fn latency_min_samples() -> i64 {
    env_i64("AMUX_AUTOFIX_LATENCY_MIN_SAMPLES", 30).max(5)
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
/// An invariant must stay breached across at least this many evaluations. A
/// flapping invariant is noise, and noise is how a board stops being read.
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
/// Empty (the default) means UNOWNED: the board's own pickup decides, and no
/// lane is volunteered for work nobody asked it to do.
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
            | DetectorKind::FdPressure => "blocker",
            _ => "investigation",
        }
    }

    pub fn all() -> [DetectorKind; 9] {
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
        let signature = format!("5xx|{status}|{method}|{family}|{target}");
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
        });
    }
    (out, suppressed)
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
pub fn detect_latency(conn: &Connection, now: f64) -> (Vec<Finding>, Vec<Suppressed>) {
    let w_start = now - window_h() * 3600.0;
    let b_start = now - baseline_h() * 3600.0;
    let mut out = Vec::new();
    let mut suppressed = Vec::new();
    let min_n = latency_min_samples();

    let mut per_family: BTreeMap<String, (Vec<f64>, Vec<f64>)> = BTreeMap::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT family, latency_ms, ts FROM _amux_request_log WHERE ts >= ?1 LIMIT 400000",
    ) {
        if let Ok(rows) = stmt.query_map(rusqlite::params![b_start], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?))
        }) {
            for (fam, ms, ts) in rows.flatten() {
                let e = per_family.entry(fam).or_default();
                if ts >= w_start {
                    e.0.push(ms);
                } else {
                    e.1.push(ms);
                }
            }
        }
    }
    for (fam, (mut win, mut base)) in per_family {
        if (win.len() as i64) < min_n {
            continue; // Not enough traffic to say anything. Not a suppression:
                      // there is no candidate here, only silence.
        }
        if (base.len() as i64) < min_n {
            suppressed.push(sup(
                DetectorKind::Latency,
                &format!("latency|p95|{fam}"),
                &format!(
                    "baseline has {} samples (<{min_n}) — no trailing norm to compare against yet",
                    base.len()
                ),
            ));
            continue;
        }
        win.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        base.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p95_w = rl::percentile_sorted(&win, 0.95);
        let p95_b = rl::percentile_sorted(&base, 0.95);
        let mult = latency_mult();
        if p95_b <= 0.0 || p95_w < p95_b * mult {
            continue;
        }
        let signature = format!("latency|p95|{fam}");
        let title = format!(
            "{fam} p95 {:.0}ms — {:.1}x its trailing norm",
            p95_w,
            p95_w / p95_b
        );
        out.push(Finding {
            kind: DetectorKind::Latency,
            signature,
            title,
            evidence: vec![
                ("verdict".into(), format!(
                    "{fam} p95 over the last {:.1}h is {p95_w:.0}ms against a {:.0}h trailing p95 \
                     of {p95_b:.0}ms ({:.1}x). Threshold: {mult}x with at least {min_n} samples \
                     on both sides.",
                    window_h(), baseline_h(), p95_w / p95_b
                )),
                ("window_samples".into(), win.len().to_string()),
                ("baseline_samples".into(), base.len().to_string()),
                ("window_p50_p95_max".into(), format!(
                    "{:.0} / {:.0} / {:.0} ms",
                    rl::percentile_sorted(&win, 0.5), p95_w,
                    win.last().copied().unwrap_or(0.0)
                )),
                ("baseline_p50_p95".into(), format!(
                    "{:.0} / {p95_b:.0} ms", rl::percentile_sorted(&base, 0.5)
                )),
                ("percentile_method".into(),
                 "nearest-rank over the sorted window (same function /api/logs/stats reports)".into()),
            ],
            recheck: format!(
                "curl -sk \"$AMUX_URL/api/logs/stats?since_h={}\" | python3 -c \"import json,sys; \
                 d=json.load(sys.stdin); print([f for f in d['families'] if f.get('family')=='{fam}'])\"",
                window_h() as i64
            ),
            owner: None,
            count: win.len() as u64,
            last_ts: now,
        });
    }

    // Single-request outliers: one card per (method, target), not per request.
    let mut seen: BTreeMap<(String, String), (u64, f64, f64, String)> = BTreeMap::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT method, path, latency_ms, ts, status FROM _amux_request_log \
         WHERE ts >= ?1 AND latency_ms >= ?2 ORDER BY latency_ms DESC LIMIT 2000",
    ) {
        if let Ok(rows) = stmt.query_map(rusqlite::params![w_start, outlier_ms()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        }) {
            for (method, path, ms, ts, status) in rows.flatten() {
                let target = rl::normalize_target(&path);
                let e = seen
                    .entry((method.clone(), target))
                    .or_insert((0, 0.0, ts, format!("{method} {path} → {status}")));
                e.0 += 1;
                e.1 = e.1.max(ms);
                e.2 = e.2.max(ts);
            }
        }
    }
    for ((method, target), (n, worst, last_ts, sample)) in seen {
        out.push(Finding {
            kind: DetectorKind::Latency,
            signature: format!("latency|outlier|{method}|{target}"),
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
pub fn detect_silent(conn: &Connection, now: f64) -> (Vec<Finding>, Vec<Suppressed>) {
    let mut out = Vec::new();
    let suppressed = Vec::new();

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
            });
        }
    }

    // (b) A steering queue that is not draining.
    let stale_s = steering_stale_min() * 60.0;
    if let Ok(mut stmt) = conn.prepare(
        "SELECT session, COUNT(*), MIN(queued_at), COALESCE(GROUP_CONCAT(DISTINCT sender),'') \
         FROM steering_queue GROUP BY session",
    ) {
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
                if age < stale_s {
                    continue;
                }
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
                    title: format!("{session}: steering queue stuck, oldest {:.0} min", age / 60.0),
                    evidence: vec![
                        ("verdict".into(), format!(
                            "{n} message(s) queued for {session}; the oldest has waited {:.0} min \
                             (deadline {:.0} min). Queued is not delivered — every one of these \
                             was reported to its sender as accepted.",
                            age / 60.0, steering_stale_min()
                        )),
                        ("queued".into(), n.to_string()),
                        ("oldest_queued_at".into(), rl::local_when(oldest)),
                        ("threshold_min".into(), format!("{:.0}", steering_stale_min())),
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
                                 '{session}': that lane may be unable to receive anything, which is \
                                 what this card reports."
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
type InvRow = (String, String, String, f64, f64, i64, String, String);

/// How many distinct entities one invariant must be failing for before it is
/// filed as a SINGLE rollup card instead of one card per entity.
///
/// 4 is deliberately low. The failure this guards against is not "slightly too
/// many cards", it is a board that stops being readable: one invariant produced
/// 64 cards, 36% of every open item, each quoting the same occurrence count.
/// Three related entities is a coincidence worth three cards; four is a
/// pattern, and a pattern is one piece of work.
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
    let Ok(mut stmt) = conn.prepare(
        "SELECT invariant_id, entity_key, status, first_seen, last_seen, occurrences, \
                expected, observed \
         FROM _amux_invariant_incident WHERE resolved_at IS NULL AND status = 'fail' \
         ORDER BY occurrences DESC LIMIT 200",
    ) else {
        return (out, suppressed);
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, f64>(3)?,
            r.get::<_, f64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
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
            signature: format!("invariant|{id}|ROLLUP"),
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
            // step exactly is what produces the wrong answer. Verified against
            // the live endpoint: it returns {checks, confidence, failures,
            // live_incidents, note, unknowns}, and failure rows carry
            // `invariant_id`, not `id`.
            recheck: format!(
                "curl -sk \"$AMUX_URL/api/health/invariants\" | python3 -c \"import json,sys; \
                 d=json.load(sys.stdin); f=[r for r in (d.get('failures') or []) \
                 if r.get('invariant_id')=='{id}']; \
                 print('FAILING:', len(f), f[:3] if f else 'clean now')\""
            ),
            owner: None,
            count: filing.len() as u64,
            last_ts: last_seen.max(now - 1.0),
        });
    }

    for (id, entity, _status, first, last, occ, expected, observed) in flat {
        let sig_entity = if entity.is_empty() { "fleet".to_string() } else { entity.clone() };
        let signature = format!("invariant|{id}|{sig_entity}");
        if occ < min_occ {
            suppressed.push(sup(
                DetectorKind::InvariantBreach,
                &signature,
                &format!("{occ} occurrence(s) < {min_occ} — a flapping invariant is noise, not a card"),
            ));
            continue;
        }
        out.push(Finding {
            kind: DetectorKind::InvariantBreach,
            signature,
            title: format!("invariant {id} failing ({occ}x) — {sig_entity}"),
            evidence: vec![
                ("verdict".into(), format!(
                    "Invariant `{id}` has been failing for {sig_entity} across {occ} evaluations \
                     and has not self-healed. Threshold to file: {min_occ}.",
                )),
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
            // step exactly is what produces the wrong answer. Verified against
            // the live endpoint: it returns {checks, confidence, failures,
            // live_incidents, note, unknowns}, and failure rows carry
            // `invariant_id`, not `id`.
            recheck: format!(
                "curl -sk \"$AMUX_URL/api/health/invariants\" | python3 -c \"import json,sys; \
                 d=json.load(sys.stdin); f=[r for r in (d.get('failures') or []) \
                 if r.get('invariant_id')=='{id}']; \
                 print('FAILING:', len(f), f[:3] if f else 'clean now')\""
            ),
            owner: None,
            count: occ as u64,
            last_ts: last.max(now - 1.0),
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
fn du_top(paths: &[std::path::PathBuf], limit: usize) -> Vec<(String, u64)> {
    let started = std::time::Instant::now();
    let total_budget = std::time::Duration::from_secs_f64(du_total_timeout_s());
    let per_path = std::time::Duration::from_secs_f64(du_path_timeout_s());
    let mut sized: Vec<(String, u64)> = Vec::new();
    // Two skip reasons, kept apart, because they need OPPOSITE responses: a
    // timeout is a budget story and an unreadable path is a permissions story,
    // and one message covering both sent readers at a knob that cannot help.
    let mut timed_out: Vec<String> = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();
    for p in paths.iter().filter(|p| p.exists()) {
        let remaining = total_budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            timed_out.push(p.display().to_string());
            continue;
        }
        let deadline = std::time::Instant::now() + per_path.min(remaining);
        match du_one(p, deadline) {
            DuOutcome::Sized(bytes) => sized.push((p.display().to_string(), bytes)),
            DuOutcome::TimedOut => timed_out.push(p.display().to_string()),
            DuOutcome::Unreadable(code) => unreadable.push(match code {
                Some(c) => format!("{} (du exit {c})", p.display()),
                None => format!("{} (du could not run)", p.display()),
            }),
        }
    }
    // Rule 4: a truncated ranking that does not say it was truncated reads as a
    // complete one. Whoever acts on this report must see that paths are missing
    // — AND why, since the two reasons have different fixes.
    //
    // It says it ONCE per run, naming the paths, rather than once per attempt:
    // the per-attempt spelling reported the same unchanging path thousands of
    // times and drowned the log it shares with real faults.
    //
    // The known bias is stated in the message itself rather than left for the
    // reader to infer: `du` time scales with directory size, so the paths that
    // blow the budget are disproportionately the LARGEST ones. Measured
    // 2026-08-19 on this machine, the three skipped paths held ~18.5G against
    // 3.7G free while the ranking that DID emerge listed only the also-rans. A
    // report saying merely "incomplete" reads as "here are the big ones plus a
    // few we missed", which is closer to the inverse of the truth.
    if !timed_out.is_empty() {
        tracing::warn!(
            skipped = timed_out.len(),
            paths = %timed_out.join(", "),
            "disk: size ranking is INCOMPLETE — these paths exceeded the du budget. \
             du time scales with size, so the paths missing here are LIKELIER TO BE \
             THE LARGEST than the ones ranked below"
        );
    }
    if !unreadable.is_empty() {
        tracing::warn!(
            skipped = unreadable.len(),
            paths = %unreadable.join(", "),
            "disk: size ranking is INCOMPLETE — these paths could not be READ at all \
             (permissions, e.g. macOS TCC on ~/.Trash). This is NOT a budget problem \
             and no timeout increase will fix it"
        );
    }
    sized.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
    sized.truncate(limit);
    sized
}

/// Candidate consumers. Deliberately NOT just `~/.amux`: on the night this was
/// written the top three consumers were a Trash full of screen recordings, a
/// pile of per-agent cargo target dirs under /private/tmp, and 24 hourly Time
/// Machine snapshots — none of which live under amux's home. A detector that
/// could only see its own directory would have named the innocent party.
fn disk_candidates(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(home) {
        for e in rd.flatten() {
            if e.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                v.push(e.path());
            }
        }
    }
    // Build trees, wherever sessions put them.
    if let Ok(rd) = std::fs::read_dir("/private/tmp") {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.contains("target") && e.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                v.push(e.path());
            }
        }
    }
    if let Ok(h) = std::env::var("HOME") {
        for p in ["Library/Caches", ".cache", ".claude", ".Trash", ".npm"] {
            v.push(std::path::Path::new(&h).join(p));
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
fn local_snapshot_count() -> Option<usize> {
    let out = std::process::Command::new("/usr/bin/tmutil")
        .args(["listlocalsnapshots", "/"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).matches("com.apple.TimeMachine").count())
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
    let top = du_top(&disk_candidates(home), 8);
    let listing = top
        .iter()
        .map(|(p, b)| format!("  {:>8.1} GB  {p}", *b as f64 / GB))
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
                DetectorKind::SilentSubsystem => detect_silent(&conn, now),
                DetectorKind::InvariantBreach => detect_invariants(&conn, now),
                DetectorKind::BuildDeploy => detect_build(&conn, now, home),
                DetectorKind::DiskPressure => detect_disk(now, home),
                DetectorKind::CiFailure => ci_findings(ci_runs, now),
                DetectorKind::FdPressure => detect_fd(now),
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
        to_file.push(f);
    }
    for s in suppressed {
        rep.suppressed.push((s.kind.slug().into(), s.signature, s.reason));
    }

    for f in to_file {
        match file_finding(state, &f).await {
            Ok(Some(card)) => rep.filed.push((card, f.signature.clone())),
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
                status: "todo".into(),
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
            "quiet_noted": r.quiet_noted.iter()
                .map(|(c, w)| json!({"card": c, "what": w})).collect::<Vec<_>>(),
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
    /// instantly and would pass against the ORIGINAL unbounded code, which is
    /// exactly the "convenient case lacks the property that made the incident"
    /// trap; this asserts the bound by making the walk genuinely unfinishable.
    #[test]
    fn du_top_returns_within_budget_on_an_unwalkable_path() {
        std::env::set_var("AMUX_DISK_DU_PATH_TIMEOUT_S", "1");
        std::env::set_var("AMUX_DISK_DU_TOTAL_TIMEOUT_S", "2");
        let started = std::time::Instant::now();
        let _ = du_top(&[std::path::PathBuf::from("/")], 8);
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
        std::env::set_var("AMUX_DISK_DU_PATH_TIMEOUT_S", "1");
        std::env::set_var("AMUX_DISK_DU_TOTAL_TIMEOUT_S", "2");
        let got = du_top(&[std::path::PathBuf::from("/")], 8);
        std::env::remove_var("AMUX_DISK_DU_PATH_TIMEOUT_S");
        std::env::remove_var("AMUX_DISK_DU_TOTAL_TIMEOUT_S");
        assert!(
            !got.iter().any(|(_, bytes)| *bytes == 0),
            "a skipped path was reported with a size instead of being omitted: {got:?}"
        );
    }

    /// Control: a path that CAN be walked is still measured and non-zero, so
    /// the two tests above are not passing merely because `du_top` returns
    /// nothing at all.
    #[test]
    fn du_top_still_measures_a_walkable_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), vec![0u8; 64 * 1024]).unwrap();
        let got = du_top(&[dir.path().to_path_buf()], 8);
        assert_eq!(got.len(), 1, "expected the tempdir to be measured: {got:?}");
        assert!(got[0].1 > 0, "measured size should be non-zero: {got:?}");
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
        assert_eq!(sref, "autofix:5xx|500|POST|/api/sessions|/api/sessions/{name}/{*verb}");
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
        (id.into(), entity.into(), "fail".into(), 1000.0, 2000.0, occ, "expected".into(), "observed".into())
    }

    /// Drives `detect_invariants` through a real SQLite table, because the
    /// grouping happens between the query and the Finding — a test that called
    /// a pure helper would skip the part that broke.
    fn invariant_findings(rows: &[InvRow]) -> Vec<Finding> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE _amux_invariant_incident (invariant_id TEXT, entity_key TEXT, \
             status TEXT, first_seen REAL, last_seen REAL, occurrences INTEGER, \
             resolved_at REAL, expected TEXT, observed TEXT);",
        )
        .unwrap();
        for r in rows {
            conn.execute(
                "INSERT INTO _amux_invariant_incident VALUES (?1,?2,?3,?4,?5,?6,NULL,?7,?8)",
                rusqlite::params![r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7],
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
        assert!(f[0].signature.ends_with("|ROLLUP"), "keyed on the invariant, not an entity");
        assert!(f[0].title.contains("64 entities"), "the count belongs in the title: {}", f[0].title);
        // The entity list must SURVIVE into the card — rolling up must not
        // destroy the information the 64 cards carried, or this trades an
        // unreadable board for an unactionable one.
        let ev: String = f[0].evidence.iter().map(|(k, v)| format!("{k}{v}")).collect();
        assert!(ev.contains("GET /api/thing0") && ev.contains("GET /api/thing63"),
                "every entity must still be named in the evidence");
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
