//! The live registry of amux's OWN background jobs — `GET /api/system-jobs`.
//!
//! # Why this exists
//!
//! On 2026-08-10 three of the server's internal loops were dead or had never
//! been spawned at all, for hours, with nothing visible anywhere: the schedule
//! firing loop had ZERO call sites (AMUX-2647), session log piping had lost
//! its writer (AMUX-2671), and the board -> worker drive loop did not exist
//! after the cutover (AMUX-2637). None of them errored, because **the failure
//! shape is pure absence** — a loop that is not running and a loop with
//! nothing to do produce byte-identical evidence. The Scheduler tab showed 113
//! user schedules and said nothing about the ten jobs that actually keep the
//! fleet moving.
//!
//! This module is the answer to ethos rule 4 ("would a wrong answer be
//! detectable from the data you keep?") applied to the server's own plumbing:
//! every internal job, its last tick, and a status that can say STALLED.
//!
//! # What this is NOT
//!
//! It is **not** a second scheduler, and it does not turn system jobs into
//! `schedules` rows. See this module's parent docs for why `DurableSchedule`
//! and `PeriodicTask` are deliberately different types with no conversion
//! between them: user schedules are owned data with history and audit; these
//! are machinery the user can neither own nor delete, and folding them into
//! the user's list would be ethos rules 3 and 8 in one move. So the endpoint
//! is a separate surface, the UI renders it as a separate section, and there
//! is no delete, no edit, and no run-now.
//!
//! # Why the registry cannot drift from the spawn sites
//!
//! A hand-maintained list of "jobs we think are running" is worth nothing: it
//! agrees with reality exactly until someone adds a loop, and the day it
//! disagrees is the day you need it. So EXISTENCE here is never declared, it
//! is observed, by two mechanisms that both sit at the spawn:
//!
//! 1. [`super::spawn_periodic_every`] is the ONLY constructor of
//!    [`super::PeriodicTask`] (its fields are private and there is no other
//!    `impl`). It calls [`register`] unconditionally and wraps the job's own
//!    closure so that every tick's start and end are recorded by the SPAWNER,
//!    not by the job. A new `PeriodicTask` is in this registry whether or not
//!    its author has ever heard of this file, and its tick timing cannot lie,
//!    because the job does not report it.
//! 2. Long-lived loops that are not `PeriodicTask`s are spawned through
//!    [`spawn_loop`] / [`adopt`] at their call site in `lib.rs`, and
//!    `tests/system_jobs.rs` reads `lib.rs` as text and fails if a bare
//!    `tokio::spawn` of a background loop appears outside that allow-list.
//!
//! [`CATALOG`] carries PROSE ONLY — the one-sentence purpose, the env var or
//! pref that controls the job, and the existing debug endpoint that holds its
//! detail. It never gates existence: a registered job with no catalog entry
//! still renders, flagged `documented: false`, so the failure mode of
//! forgetting to document a job is a visible blank, not an invisible job.
//!
//! The catalog does carry one signal existence cannot: a job that is
//! DOCUMENTED and NOT REGISTERED is the AMUX-2647 shape — a loop nobody
//! started — and it renders as `not_spawned`, loudly. When its control env var
//! says it is off, it renders as `disabled` instead. That discriminator is the
//! whole point: "off because a human said so" and "off because nobody called
//! it" looked the same for hours.
//!
//! # Outcomes come from the jobs' own reports, not a copy
//!
//! Several jobs already publish a debug surface with the real answer
//! (`/api/debug/autofix`, `/api/debug/board-drive`, `/api/debug/storage`,
//! `/api/debug/steering`). [`outcome_for`] reads those modules' own
//! `last_report()` accessors in-process — a second store of the same fact
//! would be a second thing to keep in step, and the two would eventually
//! disagree in front of someone debugging. Because it is a `match` on typed
//! accessors rather than a JSON copy, deleting a report is a COMPILE error
//! here, not a silently empty field.

use crate::api::AppState;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Job ids, as constants, because the id is used at TWO places for the
/// hand-instrumented loops (the spawn in `lib.rs` and the `tick()` inside the
/// loop) and a string typo would silently produce a second, permanently
/// stalled-looking entry. A constant makes the mismatch a compile error.
pub mod ids {
    pub const STEER_DELIVER: &str = "steer-deliver";
    pub const PIPE_RECONCILE: &str = "pipe-reconcile";
    pub const INVARIANTS: &str = "invariants-monitor";
    pub const SCHEDULER: &str = "scheduler";
    pub const ORCH_RUNTIME: &str = "orchestrator-runtime";
    pub const EVENT_PROCESSORS: &str = "event-processors";
    pub const SCAN: &str = "terminal-scan";
    pub const BOOTSTRAP: &str = "session-bootstrap";
    pub const COMMIT_NUDGE: &str = "commit-nudge";
    pub const SELF_ADOPT: &str = "self-adoption";
    // The PeriodicTask ids below are NOT referenced by any spawn site — they
    // register themselves through `spawn_periodic_every` under the name their
    // own module passes. They are listed here only so CATALOG rows and tests
    // can name them without a bare literal.
    pub const AUTOFIX: &str = "autofix";
    pub const BOARD_DRIVE: &str = "board-drive";
    pub const GHOST_RESCUE: &str = "ghost-rescue";
    pub const PANE_SIZE: &str = "pane_size";
    pub const STORAGE: &str = "storage";
    pub const HEARTBEAT: &str = "heartbeat";
    pub const TAILNET_WATCH: &str = "tailnet-watch";
}

/// Every id above, enumerated. `mod ids` is a set of constants and Rust cannot
/// iterate one, so without this a new id could be added with no [`Doc`] row and
/// nothing would notice until the job rendered nameless in the UI.
/// `tests/system_jobs.rs` reads this file as text and fails if an id constant
/// is missing from this list, and the unit test below fails if this list and
/// [`CATALOG`] disagree in either direction — the two halves of "a view must
/// share the predicate of the mechanism it describes".
pub const ALL_IDS: &[&str] = &[
    ids::STEER_DELIVER,
    ids::PIPE_RECONCILE,
    ids::INVARIANTS,
    ids::SCHEDULER,
    ids::ORCH_RUNTIME,
    ids::EVENT_PROCESSORS,
    ids::SCAN,
    ids::BOOTSTRAP,
    ids::COMMIT_NUDGE,
    ids::SELF_ADOPT,
    ids::AUTOFIX,
    ids::BOARD_DRIVE,
    ids::GHOST_RESCUE,
    ids::PANE_SIZE,
    ids::STORAGE,
    ids::HEARTBEAT,
    ids::TAILNET_WATCH,
];

/// An env var this job reads at startup. It is a READOUT, never a switch: a
/// toggle writing a var the running process already read would claim an effect
/// it cannot have (ethos rule 6). The UI shows the name and the live value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvControl {
    pub var: &'static str,
    pub effect: &'static str,
    /// The value that genuinely stops the job from being spawned, if any.
    ///
    /// **Only fill this in when the code actually checks it.** `off: Some("0")`
    /// on a job whose spawn ignores 0 would paint a running job grey and
    /// switched-off — the same class of lie as an audit trail that is claimed
    /// and not implemented. [`OFF_WHEN_UNSET`] is the sentinel for vars whose
    /// ABSENCE is what disables (the legacy-port bind).
    pub off: Option<&'static str>,
}

/// Sentinel for [`EnvControl::off`]: this job is off when the var is UNSET,
/// which is the opposite of the usual "set it to 0" shape.
pub const OFF_WHEN_UNSET: &str = "\u{0}unset";

/// A `prefs` row the job re-reads on every tick — so, unlike an env var, this
/// one really is a live switch and the UI may render it as a checkbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefControl {
    pub key: &'static str,
    pub effect: &'static str,
}

/// PROSE ONLY. See the module docs: this never decides whether a job exists.
pub struct Doc {
    pub id: &'static str,
    pub name: &'static str,
    /// One sentence: what breaks if this stops.
    pub purpose: &'static str,
    /// Env vars this job reads. A job can have several, and having several is
    /// not a footnote: autofix's LOOP is switched by `AMUX_AUTOFIX_SECS` while
    /// its FILING is switched by a pref, and modelling only one of the two
    /// would report "off" for a running detector or "running" for a dead one.
    pub env: &'static [EnvControl],
    /// The one live switch, if this job has one.
    pub pref: Option<PrefControl>,
    /// The existing debug endpoint that holds this job's real detail.
    pub detail: Option<&'static str>,
}

const NO_ENV: &[EnvControl] = &[];

pub const CATALOG: &[Doc] = &[
    Doc {
        id: ids::SCHEDULER,
        name: "Schedule firing",
        purpose: "Fires every due user schedule and delivers its command to the worker; in shadow mode it only journals what it would have fired.",
        // NOT an off switch: shadow mode still runs the loop. Claiming `off`
        // here would grey out a loop that is very much ticking.
        env: &[EnvControl {
            var: "AMUX_RS_SCHEDULER",
            effect: "1 = fire for real; anything else = shadow mode (journals what it would fire)",
            off: None,
        }],
        pref: None,
        detail: None,
    },
    Doc {
        id: ids::STEER_DELIVER,
        name: "Steering delivery",
        purpose: "Hands queued inter-session messages to each worker at its next turn boundary; without it a queue only grows.",
        env: NO_ENV,
        pref: None,
        detail: Some("/api/debug/steering"),
    },
    Doc {
        id: ids::BOARD_DRIVE,
        name: "Board drive",
        purpose: "Assigns eligible todo cards to idle lanes and sends the advance nudge, through the steering queue.",
        // AF-69 unified the `_SECS=0` opt-out across every periodic job at the
        // single spawn site: 0 now DISABLES this loop (registered inert, never
        // ticks) rather than clamping to a 1s interval. Any value >= 1 is still
        // the tick period. This used to read `off: None` with a note that 0 did
        // nothing; that stopped being true when the central knob landed.
        env: &[EnvControl {
            var: "AMUX_BOARD_DRIVE_SECS",
            effect: "tick seconds; 0 disables the loop (fleet-isolation opt-out, AF-69)",
            off: Some("0"),
        }],
        pref: None,
        detail: Some("/api/debug/board-drive"),
    },
    Doc {
        id: ids::AUTOFIX,
        name: "Autofix",
        purpose: "Watches 5xx, latency, dead routes, stalled subsystems, invariants, disk and the build, and files one board card per distinct fault.",
        env: &[EnvControl {
            var: "AMUX_AUTOFIX_SECS",
            effect: "tick seconds; 0 stops the loop entirely",
            off: Some("0"),
        }],
        pref: Some(PrefControl {
            key: "autofix_enabled",
            effect: "off still detects and records every finding; only the board card is skipped",
        }),
        detail: Some("/api/debug/autofix"),
    },
    Doc {
        id: ids::HEARTBEAT,
        name: "Liveness heartbeat",
        purpose: "Stamps a row while the server runs, so the NEXT boot can name how long amux was down instead of leaving a gap in a log file that nothing counts (AEAB-29).",
        env: &[
            EnvControl {
                var: "AMUX_HEARTBEAT_SECS",
                effect: "seconds between stamps (default 15); bounds how precisely an outage's start can be named",
                // TRUE, and checked by the code that spawns, not by this row:
                // `spawn_periodic` derives the per-job switch from the job name,
                // so `AMUX_HEARTBEAT_SECS=0` really does stop this loop. The
                // same var is honoured by `record_boot`, so an isolated server
                // does not stamp the fleet's row either.
                off: Some("0"),
            },
            EnvControl {
                var: "AMUX_DOWNTIME_MIN_S",
                effect: "gap that counts as an outage rather than a restart (default 120)",
                off: None,
            },
        ],
        pref: None,
        detail: Some("/api/debug/downtime"),
    },
    Doc {
        id: ids::STORAGE,
        name: "Storage retention",
        purpose: "Prunes seven append-only tables and three cache directories on a timer, and rotates the server log.",
        env: &[EnvControl {
            var: "AMUX_STORAGE_SWEEP_SECS",
            effect: "sweep seconds; 0 stops the sweep",
            off: Some("0"),
        }],
        pref: None,
        detail: Some("/api/debug/storage"),
    },
    Doc {
        id: ids::INVARIANTS,
        name: "Invariant monitor",
        purpose: "Re-evaluates every system invariant and opens or closes incidents; a dead monitor's silence reads as health.",
        env: NO_ENV,
        pref: None,
        detail: Some("/api/debug/invariants"),
    },
    Doc {
        id: ids::GHOST_RESCUE,
        name: "Ghost rescue",
        purpose: "Presses Enter for a message that was typed into a lane's input box and never submitted — the fallback for keystroke delivery.",
        env: NO_ENV,
        pref: None,
        detail: None,
    },
    Doc {
        id: ids::PIPE_RECONCILE,
        name: "Session log piping",
        purpose: "Re-attaches tmux pipe-pane to any lane that lost its log writer; without it a live lane looks like one that never started.",
        env: NO_ENV,
        pref: None,
        detail: Some("/api/debug/logs"),
    },
    Doc {
        id: ids::PANE_SIZE,
        name: "Pane-size repair",
        purpose: "Restores a worker's tmux window width after a peek shrank it to the reader's viewport.",
        env: NO_ENV,
        pref: None,
        detail: None,
    },
    Doc {
        id: ids::COMMIT_NUDGE,
        name: "Commit nudge",
        purpose: "Nudges an idle lane that is sitting on uncommitted work it owns via the staged-guard.",
        env: &[EnvControl {
            var: "AMUX_COMMIT_NUDGE_SECS",
            effect: "sweep seconds; 0 stops the sweep",
            off: Some("0"),
        }],
        pref: None,
        detail: None,
    },
    Doc {
        id: ids::ORCH_RUNTIME,
        name: "Orchestrator runtime",
        purpose: "The reconcile/plan/execute pump: leases, the fleet circuit breaker, and every worker state transition.",
        env: &[EnvControl { var: "AMUX_RS_TICK_SECS", effect: "tick seconds (default 3)", off: None }],
        pref: None,
        detail: None,
    },
    Doc {
        id: ids::EVENT_PROCESSORS,
        name: "Worker event processors",
        purpose: "Supervises one durable subscriber per live worker so turn start/complete events reach the DB instead of only a log line.",
        env: NO_ENV,
        pref: None,
        detail: None,
    },
    Doc {
        id: ids::SCAN,
        name: "Terminal scan",
        purpose: "The fallback voice for hookless workers: captures panes to infer state, demoting any lane whose harness reports for itself.",
        env: &[EnvControl { var: "AMUX_RS_SCAN_SECS", effect: "scan seconds (default 15)", off: None }],
        pref: None,
        detail: Some("/api/debug/scan"),
    },
    Doc {
        id: ids::BOOTSTRAP,
        name: "Session bootstrap",
        purpose: "Turns durable Starting/ended worker records into real backend processes and protocol sessions.",
        env: &[EnvControl { var: "AMUX_RS_BOOTSTRAP_SECS", effect: "pass seconds (default 2)", off: None }],
        pref: None,
        detail: None,
    },
    Doc {
        id: ids::SELF_ADOPT,
        name: "Self-adoption watch",
        purpose: "Exits 0 when the installed binary changes on disk so launchd relaunches the new build instead of serving stale code.",
        env: NO_ENV,
        pref: None,
        detail: None,
    },
    Doc {
        id: ids::TAILNET_WATCH,
        name: "Tailnet watch",
        purpose: "Samples `tailscale status` for node-key expiry and reachability, and caches the verdict for /health — so the tailnet going away is visible before the day it takes remote access with it.",
        env: NO_ENV,
        pref: None,
        detail: None,
    },
];

pub fn doc_for(id: &str) -> Option<&'static Doc> {
    CATALOG.iter().find(|d| d.id == id)
}

// ---------------------------------------------------------------------------
// Live state
// ---------------------------------------------------------------------------

struct Job {
    kind: &'static str,
    interval: Option<Duration>,
    spawned_at: f64,
    ticks: u64,
    last_start: Option<f64>,
    last_end: Option<f64>,
    last_ms: Option<f64>,
    /// Liveness. A `PeriodicTask`'s loop never returns, so `is_finished()` is
    /// true only after a panic or an explicit abort — i.e. it is exactly the
    /// "this job died" signal and nothing else.
    abort: Option<tokio::task::AbortHandle>,
    /// Some(switch) when this job was REGISTERED but its tick loop was
    /// deliberately NOT spawned because fleet isolation is on (AF-69: a
    /// second/test amux-server must not drive the production tmux fleet). It
    /// carries the exact switch that turned it off so [`system_jobs`] can render
    /// it `disabled` WITH a reason, instead of a loop that reads `stalled`
    /// because it never ticks. Registered-but-inert is the honest state: the
    /// fleet hazard is visible, not a silent skip (ethos rule 4).
    disabled: Option<String>,
}

fn reg() -> &'static Mutex<BTreeMap<String, Job>> {
    static R: OnceLock<Mutex<BTreeMap<String, Job>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Record a job at its spawn. Called by [`super::spawn_periodic_every`] and by
/// [`spawn_loop`] / [`adopt`]; nothing else should call it, because a
/// registration that is not a spawn is exactly the parallel list this module
/// exists to avoid.
pub fn register(
    id: &str,
    kind: &'static str,
    interval: Option<Duration>,
    abort: Option<tokio::task::AbortHandle>,
) {
    register_inner(id, kind, interval, abort, None);
}

/// Register a job whose tick loop was deliberately NOT spawned because fleet
/// isolation is on (AF-69). Called by [`super::spawn_periodic_every`] when a
/// global switch (`AMUX_ISOLATED`/`AMUX_NO_FLEET`) or the per-job
/// `AMUX_<NAME>_SECS=0` opt-out is set, so a second/test amux-server cannot press
/// Enter or resize panes in the production tmux lanes.
///
/// `reason` is the switch that fired. The job appears on `/api/system-jobs` as
/// `disabled` with that reason rather than vanishing — a suppressed fleet-driving
/// job must be inert-but-visible, or a live hazard reads as a silent skip (the
/// exact failure ethos rule 4 and this module's own docs cite). No abort handle:
/// there is no loop to stop.
pub fn register_disabled(
    id: &str,
    kind: &'static str,
    interval: Option<Duration>,
    reason: String,
) {
    register_inner(id, kind, interval, None, Some(reason));
}

fn register_inner(
    id: &str,
    kind: &'static str,
    interval: Option<Duration>,
    abort: Option<tokio::task::AbortHandle>,
    disabled: Option<String>,
) {
    if let Ok(mut m) = reg().lock() {
        m.insert(
            id.to_string(),
            Job {
                kind,
                interval,
                spawned_at: unix_now(),
                ticks: 0,
                last_start: None,
                last_end: None,
                last_ms: None,
                abort,
                disabled,
            },
        );
    }
}

/// A tick began. Recorded by the spawner's wrapper, so a job cannot forget.
pub fn tick_start(id: &str) {
    if let Ok(mut m) = reg().lock() {
        if let Some(j) = m.get_mut(id) {
            j.last_start = Some(unix_now());
        }
    }
}

/// A tick finished.
pub fn tick_end(id: &str) {
    if let Ok(mut m) = reg().lock() {
        if let Some(j) = m.get_mut(id) {
            let now = unix_now();
            j.ticks += 1;
            j.last_ms = j.last_start.map(|s| (now - s) * 1000.0);
            j.last_end = Some(now);
        }
    }
}

/// One-shot self-report, for the hand-instrumented loops that are not
/// `PeriodicTask`s. Deliberately does NOT create a missing entry: an entry
/// that appears from a tick would hide the "documented but never spawned"
/// case, which is the one that cost hours.
pub fn tick(id: &str) {
    if let Ok(mut m) = reg().lock() {
        if let Some(j) = m.get_mut(id) {
            let now = unix_now();
            j.ticks += 1;
            j.last_start = Some(now);
            j.last_end = Some(now);
        }
    }
}

/// One registry row, copied out under the lock. Exists so the endpoint never
/// holds the mutex while it reads prefs or another module's report.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: String,
    pub kind: &'static str,
    pub interval_s: Option<f64>,
    pub spawned_at: f64,
    pub ticks: u64,
    pub last_tick_at: Option<f64>,
    pub last_tick_ms: Option<f64>,
    pub in_flight_since: Option<f64>,
    pub dead: bool,
    /// Some(switch) when the job is registered-but-inert under fleet isolation
    /// (AF-69). Drives the `disabled` verdict and is surfaced verbatim so the UI
    /// can name WHY the job is off.
    pub disabled_reason: Option<String>,
}

/// Every job that has actually been spawned in this process.
pub fn snapshot() -> Vec<Snapshot> {
    let Ok(m) = reg().lock() else { return Vec::new() };
    m.iter()
        .map(|(id, j)| Snapshot {
            id: id.clone(),
            kind: j.kind,
            interval_s: j.interval.map(|d| d.as_secs_f64()),
            spawned_at: j.spawned_at,
            ticks: j.ticks,
            last_tick_at: j.last_end,
            last_tick_ms: j.last_ms,
            // In flight iff a start is recorded that no end has caught up to.
            in_flight_since: match (j.last_start, j.last_end) {
                (Some(s), Some(e)) if s > e => Some(s),
                (Some(s), None) => Some(s),
                _ => None,
            },
            dead: j.abort.as_ref().map(|a| a.is_finished()).unwrap_or(false),
            disabled_reason: j.disabled.clone(),
        })
        .collect()
}

/// [`tick`] for a loop whose cadence is only known INSIDE the loop (read from
/// an env var after it starts). One call so the tick and the interval it was
/// paced at can never be recorded separately — and the interval shown is by
/// construction the one the loop is actually sleeping.
pub fn tick_every(id: &str, interval: Duration) {
    if let Ok(mut m) = reg().lock() {
        if let Some(j) = m.get_mut(id) {
            let now = unix_now();
            j.interval = Some(interval);
            j.ticks += 1;
            j.last_start = Some(now);
            j.last_end = Some(now);
        }
    }
}

/// Spawn a long-lived internal loop AND register it in one call. The point is
/// that there is no way to do the first without the second.
pub fn spawn_loop<F>(id: &'static str, interval: Option<Duration>, fut: F) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let h = tokio::spawn(fut);
    register(id, "loop", interval, Some(h.abort_handle()));
    h
}

/// Register a loop somebody else already spawned (its `spawn()` owns the
/// `tokio::spawn`). Same contract as [`spawn_loop`], called at the same place.
pub fn adopt(id: &'static str, interval: Option<Duration>, h: &tokio::task::JoinHandle<()>) {
    register(id, "loop", interval, Some(h.abort_handle()));
}

// ---------------------------------------------------------------------------
// Staleness — the one rule, as a pure function so it can be tested with a
// negative control
// ---------------------------------------------------------------------------

/// Facts about one job at one instant. Split out from the registry so the
/// verdict is testable without spawning anything — a staleness check that can
/// only be exercised by waiting is a check nobody runs (ethos rule 7).
#[derive(Debug, Clone, Copy, Default)]
pub struct Facts {
    /// Present in the registry, i.e. something actually spawned it.
    pub spawned: bool,
    /// Its driving task has exited (panic or abort). Only meaningful when spawned.
    pub dead: bool,
    /// Its control says a human turned it off.
    pub disabled: bool,
    pub interval_s: Option<f64>,
    pub spawned_at: Option<f64>,
    pub last_tick_at: Option<f64>,
    /// When the in-flight tick started, if one is in flight.
    pub in_flight_since: Option<f64>,
    /// Whether ticks are observed at all. False = liveness-only; the honest
    /// verdict there is `alive`, never `ok`, because `ok` would assert a
    /// freshness nobody measured.
    pub instrumented: bool,
}

/// How long after its interval a job is STALLED.
///
/// `2.5x + 15s`: the multiplier tolerates one missed tick plus jitter (a tick
/// that overruns delays only its own next tick — `MissedTickBehavior::Delay`),
/// and the flat grace keeps fast jobs from flapping. It is deliberately NOT
/// tunable: a threshold you can turn down is a detector that gets turned down
/// the first time it is inconvenient, and the failure this catches ran for
/// HOURS, so no plausible constant in this range misses it.
pub fn stall_after_s(interval_s: f64) -> f64 {
    interval_s * 2.5 + 15.0
}

/// The verdict. Order matters: "nobody spawned it" outranks everything,
/// because a job that is not running has no ticks to be fresh or stale.
pub fn classify(f: &Facts, now: f64) -> &'static str {
    // A human's decision outranks every mechanical verdict below: a job whose
    // env var says 0 is not "dead" and not "not_spawned", it is OFF, and
    // painting it red would train everyone to ignore the red. The mirror case
    // — a job that is off because NOBODY STARTED IT — is the AMUX-2647 shape
    // that cost hours, and it is why these are two different words rather than
    // one "not running".
    if f.disabled {
        return "disabled";
    }
    if !f.spawned {
        return "not_spawned";
    }
    if f.dead {
        return "dead";
    }
    let Some(interval) = f.interval_s else {
        return if f.instrumented { "ok" } else { "alive" };
    };
    let limit = stall_after_s(interval);
    // A tick that started and never ended is a WEDGED job, not a slow one, and
    // it is invisible from `last_tick_at` alone — that field keeps reading
    // fresh right up until the hang and only goes stale a full budget later.
    // So it is checked FIRST and reported as its own word.
    if let Some(started) = f.in_flight_since {
        if now - started > limit {
            return "hung";
        }
    }
    match f.last_tick_at {
        Some(t) if now - t > limit => "stalled",
        Some(_) => "ok",
        // Never ticked. Before the budget elapses that is normal; after it,
        // this is the same fault as a stalled tick — and it is the shape a
        // loop takes when it wedges on its FIRST pass.
        //
        // `instrumented` is deliberately NOT consulted here. It is derived
        // from "have we seen a tick", so using it would conflate "this job
        // never reports ticks" with "this job has not ticked YET" — and a loop
        // wedged on its first pass falls in the second bucket, so it would
        // have read `alive` forever instead of `stalled`. A KNOWN INTERVAL is
        // the thing that makes ticks expected; that test already happened
        // above.
        None => match f.spawned_at {
            Some(s) if now - s > limit => "stalled",
            _ => "starting",
        },
    }
}

// ---------------------------------------------------------------------------
// Outcome — read from each job's OWN report, never copied
// ---------------------------------------------------------------------------

/// A one-line human summary of the job's last real outcome, pulled from the
/// module that already publishes it. A `match` on typed accessors on purpose:
/// if one of those reports is deleted or renamed, this stops compiling instead
/// of quietly rendering an empty field forever.
pub fn outcome_for(id: &str) -> Option<String> {
    match id {
        ids::AUTOFIX => super::autofix::last_report().map(|r| {
            if !r.errors.is_empty() {
                format!("{} error(s): {}", r.errors.len(), r.errors.join("; "))
            } else {
                format!(
                    "{} filed, {} suppressed, {} signature(s) seen",
                    r.filed.len(),
                    r.suppressed.len(),
                    r.signatures_seen.len()
                )
            }
        }),
        ids::BOARD_DRIVE => super::board_drive::last_report().map(|r| {
            format!("{} assigned, {} nudged across {} lane(s)", r.assigned, r.nudged, r.lanes.len())
        }),
        ids::STORAGE => super::storage::last_report().map(|r| {
            format!(
                "{} table(s) swept, {} file(s) removed, {} freed",
                r.tables.len(),
                r.files_removed,
                human_bytes(r.bytes_freed + r.rotated_bytes)
            )
        }),
        ids::SCAN => crate::orchestrator::scan::last_scan_state().map(|s| {
            format!(
                "{} scanned, {} demoted (structured), {} demoted (native), {} capture failure(s)",
                s.report.scanned.len(),
                s.report.demoted_structured.len(),
                s.report.demoted_native.len(),
                s.report.capture_failures.len(),
            )
        }),
        _ => None,
    }
}

fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

// ---------------------------------------------------------------------------
// The endpoint
// ---------------------------------------------------------------------------

/// Read a `prefs` row. Only used for `Control::Pref`, whose whole point is
/// that it is live: the job re-reads it every tick, so the value shown here is
/// the value in force.
fn pref(state: &AppState, key: &str) -> Option<String> {
    state
        .store
        .read()
        .ok()
        .and_then(|c| c.query_row("SELECT value FROM prefs WHERE key=?1", [key], |r| r.get(0)).ok())
}

/// Every env READOUT for this job, plus whether one of them says a human
/// switched it off. `off` is only consulted where the spawn code genuinely
/// checks it — see [`EnvControl::off`].
fn env_json(d: &Doc) -> (Vec<Value>, bool) {
    let mut out = Vec::new();
    let mut disabled = false;
    for e in d.env {
        let val = std::env::var(e.var).ok();
        let is_off = match e.off {
            None => false,
            Some(OFF_WHEN_UNSET) => val.as_deref().map(str::trim).unwrap_or("").is_empty(),
            Some(v) => val.as_deref().map(str::trim) == Some(v),
        };
        disabled = disabled || is_off;
        out.push(json!({
            "kind": "env",
            "var": e.var,
            "value": val,
            "effect": e.effect,
            "off_value": match e.off {
                None => Value::Null,
                Some(OFF_WHEN_UNSET) => json!("(unset)"),
                Some(v) => json!(v),
            },
            "off_now": is_off,
            // NEVER a switch: the process read this at startup, so writing it
            // from here would claim an effect it cannot have.
            "editable": false,
            "note": "read at startup — set it in ~/.amux/server.env and restart the server",
        }));
    }
    (out, disabled)
}

/// The live switch, if this job has one. A pref is re-read by the job on every
/// tick, which is exactly what makes it safe to render as a checkbox.
fn pref_json(state: &AppState, d: &Doc) -> Option<Value> {
    let p = d.pref?;
    let val = pref(state, p.key);
    let off = matches!(val.as_deref().map(str::trim), Some("0") | Some("false") | Some("off"));
    Some(json!({
        "kind": "pref",
        "key": p.key,
        "value": val,
        "effect": p.effect,
        "editable": true,
        "on": !off,
    }))
}

/// `GET /api/system-jobs` — every internal background job, its last tick, and
/// a status that can say STALLED.
pub async fn system_jobs(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let now = unix_now();
    let live: BTreeMap<String, Snapshot> =
        snapshot().into_iter().map(|s| (s.id.clone(), s)).collect();

    // Union of "documented" and "registered": the catalog contributes the
    // jobs that SHOULD be running (so one that is missing is loud), the
    // registry contributes everything that IS running (so an undocumented new
    // job cannot hide). Neither list alone is the truth.
    let mut all: Vec<String> = CATALOG.iter().map(|d| d.id.to_string()).collect();
    for id in live.keys() {
        if !all.iter().any(|x| x == id) {
            all.push(id.clone());
        }
    }

    let mut jobs: Vec<Value> = Vec::new();
    let mut unhealthy = 0usize;
    for id in all {
        let d = doc_for(&id);
        let (env, disabled_by_control) = match d {
            Some(d) => env_json(d),
            None => (Vec::new(), false),
        };
        let pref_ctl = d.and_then(|d| pref_json(&state, d));
        let l = live.get(&id);
        // Fleet isolation (AF-69) is decided at the spawn site, not re-derived
        // from the catalog: a job registered inert under `AMUX_ISOLATED` carries
        // its switch here. OR it with the catalog env readout so BOTH the global
        // isolation knob and a job's own `_SECS=0` render as `disabled`. This is
        // the mechanism's own verdict, so the view cannot drift from it.
        let iso_reason = l.and_then(|x| x.disabled_reason.clone());
        let f = Facts {
            spawned: l.is_some(),
            dead: l.map(|x| x.dead).unwrap_or(false),
            disabled: disabled_by_control || iso_reason.is_some(),
            interval_s: l.and_then(|x| x.interval_s),
            spawned_at: l.map(|x| x.spawned_at),
            last_tick_at: l.and_then(|x| x.last_tick_at),
            in_flight_since: l.and_then(|x| x.in_flight_since),
            // "Does this job report ticks at all?" — true once one has been
            // seen OR one is in flight. False means liveness-only, and the
            // verdict for those is `alive`, never `ok`.
            instrumented: l
                .map(|x| x.ticks > 0 || x.in_flight_since.is_some())
                .unwrap_or(false),
        };
        let status = classify(&f, now);
        if matches!(status, "stalled" | "dead" | "hung" | "not_spawned") {
            unhealthy += 1;
        }
        jobs.push(json!({
            "id": id,
            "name": d.map(|d| d.name).unwrap_or(id.as_str()),
            "purpose": d.map(|d| d.purpose),
            "documented": d.is_some(),
            "kind": l.map(|x| x.kind),
            "interval_s": f.interval_s,
            "stale_after_s": f.interval_s.map(stall_after_s),
            "spawned": f.spawned,
            "spawned_at": f.spawned_at,
            "uptime_s": f.spawned_at.map(|s| now - s),
            "ticks": l.map(|x| x.ticks).unwrap_or(0),
            "last_tick_at": f.last_tick_at,
            "last_tick_age_s": f.last_tick_at.map(|t| now - t),
            "last_tick_ms": l.and_then(|x| x.last_tick_ms),
            "in_flight": f.in_flight_since.is_some(),
            "instrumented": f.instrumented,
            "status": status,
            // The switch that suppressed this job under fleet isolation (AF-69),
            // or null. Present so a `disabled` row names WHY — which switch —
            // rather than leaving a reader to guess whether a human or the code
            // turned it off.
            "disabled_reason": iso_reason,
            // Readouts and the (at most one) live switch, kept apart on
            // purpose: the client renders them differently because they ARE
            // different — one is a fact you can act on elsewhere, one is a
            // control that takes effect now.
            "env": env,
            "pref": pref_ctl,
            "detail": d.and_then(|d| d.detail),
            "outcome": outcome_for(&id),
        }));
    }

    let body = json!({
        "note": "amux's OWN background jobs — internal plumbing, not user schedules. \
                 There is no run-now, edit or delete here on purpose: these are not \
                 owned data, they are the machinery. `not_spawned` means the catalog \
                 documents a job that nothing started (the failure that cost hours); \
                 `disabled` means a human turned it off (its `_SECS=0` opt-out or the \
                 process-wide fleet-isolation switch AMUX_ISOLATED/AMUX_NO_FLEET — see \
                 `disabled_reason`).",
        "now": now,
        "jobs": jobs,
        "count": jobs.len(),
        "unhealthy": unhealthy,
        "stall_rule": "a job is STALLED when its last tick is older than 2.5x its interval + 15s",
    });
    (axum::http::StatusCode::OK, axum::Json(body)).into_response()
}

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new().route("/api/system-jobs", axum::routing::get(system_jobs))
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: f64 = 1_000_000.0;

    fn base() -> Facts {
        Facts {
            spawned: true,
            instrumented: true,
            interval_s: Some(20.0),
            spawned_at: Some(T),
            last_tick_at: Some(T),
            ..Default::default()
        }
    }

    /// The predicate with its NEGATIVE CONTROL. A staleness check that returns
    /// "stalled" for everything is exactly as useless as one that never does,
    /// and from a screenshot of one red row you cannot tell which you have —
    /// so both directions are asserted at the same interval.
    #[test]
    fn stalled_only_past_the_budget() {
        let f = base(); // 20s interval -> stall_after = 65s
        assert_eq!(stall_after_s(20.0), 65.0);
        // NEGATIVE CONTROL: a tick one second inside the budget is NOT stalled.
        assert_eq!(classify(&f, T + 64.0), "ok");
        // ...and one second past it is.
        assert_eq!(classify(&f, T + 66.0), "stalled");
        // A fresh tick is never stalled no matter the interval.
        assert_eq!(classify(&base(), T + 1.0), "ok");
    }

    #[test]
    fn stall_budget_scales_with_the_interval() {
        // The hourly storage sweep must not read as stalled at 10 minutes...
        let f = Facts { interval_s: Some(3600.0), ..base() };
        assert_eq!(classify(&f, T + 600.0), "ok");
        // ...but does after 2.5 hours.
        assert_eq!(classify(&f, T + 9100.0), "stalled");
        // A 5s loop is stalled in well under a minute — the same rule, not a
        // special case.
        let g = Facts { interval_s: Some(5.0), ..base() };
        assert_eq!(classify(&g, T + 20.0), "ok");
        assert_eq!(classify(&g, T + 40.0), "stalled");
    }

    #[test]
    fn never_ticked_is_starting_then_stalled() {
        let f = Facts { last_tick_at: None, ..base() };
        assert_eq!(classify(&f, T + 10.0), "starting");
        assert_eq!(classify(&f, T + 100.0), "stalled");
    }

    /// The distinction that cost hours: a loop nobody started, versus one a
    /// human switched off. Both are "not running"; only one is a bug.
    #[test]
    fn unspawned_is_loud_unless_a_human_turned_it_off() {
        let f = Facts { spawned: false, ..Default::default() };
        assert_eq!(classify(&f, T), "not_spawned");
        let g = Facts { spawned: false, disabled: true, ..Default::default() };
        assert_eq!(classify(&g, T), "disabled");
        // A job whose loop was spawned and immediately returned because its
        // env var says 0 is OFF, not dead — commit-nudge's exact shape. Red
        // for a switch someone deliberately flipped teaches people to ignore
        // red.
        let h = Facts { spawned: true, dead: true, disabled: true, ..base() };
        assert_eq!(classify(&h, T + 1.0), "disabled");
    }

    /// A wedged tick keeps `last_tick_at` fresh (it is the END of the PREVIOUS
    /// tick) for a whole budget after the hang starts, so freshness alone
    /// reports a hung job as healthy. This is the case a "last run" column
    /// cannot express at all.
    #[test]
    fn a_tick_that_never_ends_reads_as_hung_not_ok() {
        let f = Facts { in_flight_since: Some(T), ..base() }; // 65s budget
        // NEGATIVE CONTROL: a tick in flight for 30s is just a slow tick.
        assert_eq!(classify(&f, T + 30.0), "ok");
        assert_eq!(classify(&f, T + 70.0), "hung");
    }

    #[test]
    fn dead_outranks_freshness() {
        // A task that panicked keeps its last (fresh) tick timestamp forever,
        // so freshness alone would report it healthy.
        let f = Facts { dead: true, ..base() };
        assert_eq!(classify(&f, T + 1.0), "dead");
    }

    #[test]
    fn uninstrumented_loops_never_claim_ok() {
        let f = Facts {
            instrumented: false,
            interval_s: None,
            last_tick_at: None,
            ..base()
        };
        assert_eq!(classify(&f, T + 100_000.0), "alive");
    }

    /// A loop that wedges on its FIRST pass has an interval, zero ticks and no
    /// last-tick time — exactly the shape of a job that simply has not reached
    /// its first tick yet. Deriving "is this instrumented" from the tick count
    /// made these two identical, and the wedged one then read `alive` forever.
    /// The known INTERVAL is what makes a tick expected.
    #[test]
    fn a_first_pass_wedge_is_stalled_not_alive() {
        let f = Facts {
            instrumented: false, // no tick has ever been seen — that IS the fault
            interval_s: Some(60.0),
            last_tick_at: None,
            spawned_at: Some(T),
            ..base()
        };
        // NEGATIVE CONTROL: inside the budget it is merely starting.
        assert_eq!(classify(&f, T + 60.0), "starting");
        assert_eq!(classify(&f, T + 200.0), "stalled");
    }

    /// Registration comes from the spawner, not from a list: spawning a
    /// `PeriodicTask` under a name nobody has ever written down must still
    /// produce a registry row with a real interval and a real tick.
    #[tokio::test]
    async fn spawning_a_periodic_task_registers_it_and_records_ticks() {
        let id = "test-autoregister-xyz";
        let t = super::super::spawn_periodic_every(id, Duration::from_millis(20), || async {});
        tokio::time::sleep(Duration::from_millis(120)).await;
        let m = reg().lock().unwrap();
        let j = m.get(id).expect("spawn_periodic_every must register the job");
        assert_eq!(j.interval, Some(Duration::from_millis(20)));
        assert!(j.ticks >= 2, "ticks recorded by the spawner, got {}", j.ticks);
        assert!(j.last_end.is_some());
        drop(m);
        t.abort();
    }

    /// Every catalog row must name a job id that some spawn site can produce.
    /// This cannot prove the spawn happens (that is what `not_spawned` is
    /// for), but it does catch a row whose id was typo'd, which would render
    /// as a permanently-missing job and send someone hunting a live loop.
    #[test]
    fn catalog_ids_are_unique_and_non_empty() {
        let mut seen = std::collections::BTreeSet::new();
        for d in CATALOG {
            assert!(!d.id.is_empty(), "catalog row with empty id");
            assert!(!d.purpose.is_empty(), "{}: purpose is the whole point", d.id);
            assert!(seen.insert(d.id), "duplicate catalog id {}", d.id);
        }
    }

    /// BOTH directions. An id with no doc renders nameless in the UI; a doc
    /// with no id is a job the page promises and no spawn site can produce,
    /// which reads as `not_spawned` forever and sends someone hunting a loop
    /// that was never meant to exist.
    #[test]
    fn every_id_has_a_doc_and_every_doc_has_an_id() {
        let ids: std::collections::BTreeSet<&str> = ALL_IDS.iter().copied().collect();
        let docs: std::collections::BTreeSet<&str> = CATALOG.iter().map(|d| d.id).collect();
        let undocumented: Vec<_> = ids.difference(&docs).collect();
        let phantom: Vec<_> = docs.difference(&ids).collect();
        assert!(undocumented.is_empty(), "job ids with no CATALOG row: {undocumented:?}");
        assert!(phantom.is_empty(), "CATALOG rows with no id constant: {phantom:?}");
    }
}
