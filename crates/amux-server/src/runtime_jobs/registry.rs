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
    pub const HOST_METRICS: &str = "host-metrics";
    pub const TRACE_ARCHIVE: &str = "trace-archive";
    pub const HOST_GUARD: &str = "host-guard";
    pub const GOAL_KEEPER: &str = "goal-keeper";
    pub const ORCH_RUNTIME: &str = "orchestrator-runtime";
    pub const PROJECT_EXECUTION: &str = "project-execution";
    pub const EVENT_PROCESSORS: &str = "event-processors";
    pub const SCAN: &str = "terminal-scan";
    pub const BOOTSTRAP: &str = "session-bootstrap";
    pub const EMAIL_THEMES: &str = "email-themes";
    pub const COMMIT_MENTION_NOTES: &str = "commit-mention-notes";
    pub const SELF_ADOPT: &str = "self-adoption";
    pub const TUNNEL: &str = "tunnel-relay";
    pub const BROWSER_REAPER: &str = "browser-idle-reaper";
    pub const COMPUTER_REAPER: &str = "computer-sandbox-reaper";
    // The PeriodicTask ids below are NOT referenced by any spawn site — they
    // register themselves through `spawn_periodic_every` under the name their
    // own module passes. They are listed here only so CATALOG rows and tests
    // can name them without a bare literal.
    pub const AUTOFIX: &str = "autofix";
    pub const BOARD_DRIVE: &str = "board-drive";
    pub const CDC_POLLER: &str = "cdc-poller";
    pub const PANE_SIZE: &str = "pane_size";
    pub const WORKTREE_HYGIENE: &str = "worktree-hygiene";
    pub const STORAGE: &str = "storage";
    pub const HEARTBEAT: &str = "heartbeat";
    pub const TAILNET_WATCH: &str = "tailnet-watch";
    pub const TELEGRAM_POLL: &str = "telegram-poll";
    pub const TELEGRAM_RELAY: &str = "telegram-relay";
    pub const MESSAGE_CAPTURE: &str = "message-capture";
    pub const MAC_HEALTH: &str = "mac-health";
    pub const ACCOUNTABILITY_NUDGE: &str = "accountability-nudge";
    pub const CONTEXT_HEALTH: &str = "context-health";
    pub const DISK_WATCH: &str = "disk-watch";
    pub const STATUS_HISTORY: &str = "status-history";
    pub const EPISODES: &str = "episodes";
    pub const TOKEN_LEDGER: &str = "token-ledger";
    pub const BOARD_HYGIENE: &str = "board-hygiene";
    pub const RECORDINGS_TRANSCRIBE: &str = "recordings-transcribe";
    pub const MODEL_CATALOG_REFRESH: &str = "model-catalog-refresh";
    pub const CAPTURE_RECONCILE: &str = "capture-reconcile";
    pub const EMAIL_APPROVAL_DIGEST: &str = "email-approval-digest";
    pub const NEEDS_INPUT_AUTO: &str = "needs-input-auto";
    pub const CONTRACT_WATCH: &str = "contract-watch";
    pub const LAND_QUEUE: &str = "land-queue";
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
    ids::PROJECT_EXECUTION,
    ids::EVENT_PROCESSORS,
    ids::SCAN,
    ids::BOOTSTRAP,
    ids::EMAIL_THEMES,
    ids::COMMIT_MENTION_NOTES,
    ids::SELF_ADOPT,
    ids::TUNNEL,
    ids::BROWSER_REAPER,
    ids::COMPUTER_REAPER,
    ids::AUTOFIX,
    ids::BOARD_DRIVE,
    ids::CDC_POLLER,
    ids::PANE_SIZE,
    ids::WORKTREE_HYGIENE,
    ids::STORAGE,
    ids::HEARTBEAT,
    ids::TAILNET_WATCH,
    ids::TELEGRAM_POLL,
    ids::TELEGRAM_RELAY,
    ids::MESSAGE_CAPTURE,
    ids::MAC_HEALTH,
    ids::ACCOUNTABILITY_NUDGE,
    ids::CONTEXT_HEALTH,
    ids::DISK_WATCH,
    ids::HOST_METRICS,
    ids::TRACE_ARCHIVE,
    ids::HOST_GUARD,
    ids::GOAL_KEEPER,
    ids::STATUS_HISTORY,
    ids::EPISODES,
    ids::TOKEN_LEDGER,
    ids::BOARD_HYGIENE,
    ids::RECORDINGS_TRANSCRIBE,
    ids::MODEL_CATALOG_REFRESH,
    ids::CAPTURE_RECONCILE,
    ids::EMAIL_APPROVAL_DIGEST,
    ids::NEEDS_INPUT_AUTO,
    ids::CONTRACT_WATCH,
    ids::LAND_QUEUE,
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
        id: ids::EMAIL_THEMES,
        name: "Email theme inference",
        purpose: "Recomputes the owner's email themes from human message history for the ranked inbox; one meta-model call per 6h at most, and a pass skips when the stored themes are still fresh so a restart does not buy a call.",
        env: NO_ENV,
        pref: None,
        detail: Some("/api/email/themes"),
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
        env: &[
            EnvControl {
                var: "AMUX_BOARD_DRIVE_SECS",
                effect: "tick seconds; 0 disables the loop (fleet-isolation opt-out, AF-69)",
                off: Some("0"),
            },
            EnvControl {
                var: "AMUX_BLOCKER_RECOVERY_LANE_INTERVAL_S",
                effect: "at most one blocker-recovery review per lane per this many seconds (default 3600; scoped per worker/group); 0 disables blocker recovery",
                off: Some("0"),
            },
        ],
        pref: None,
        detail: Some("/api/debug/board-drive"),
    },
    Doc {
        id: ids::PROJECT_EXECUTION,
        name: "Project execution",
        purpose: "Drives project-scoped tasks through claim, delivery, verification, retained evidence, main integration, and acceptance without depending on the legacy board-drive lane sweeper.",
        env: &[EnvControl {
            var: "AMUX_PROJECT_EXECUTION_SECS",
            effect: "tick seconds; 0 disables project lifecycle driving",
            off: Some("0"),
        }],
        pref: None,
        detail: Some("/api/projects"),
    },
    Doc {
        id: ids::CDC_POLLER,
        name: "Board CDC poller",
        purpose: "Tails board_change_log every 200ms so the /api/board/changes catch-up endpoint stays current; the SSE invalidate itself comes from write_async, not from here.",
        env: &[EnvControl {
            var: "AMUX_CDC_POLLER_SECS",
            effect: "tick seconds; 0 disables the loop (fleet-isolation opt-out)",
            off: Some("0"),
        }],
        pref: None,
        detail: None,
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
        id: ids::MESSAGE_CAPTURE,
        name: "Message capture recovery",
        purpose: "Resumes durable pending message-to-card capture after interruption, using the original history row and semantic intake without resending commands. Historical unlinked messages require explicit reviewed attribution.",
        env: &[EnvControl {
            var: "AMUX_MESSAGE_CAPTURE_SECS",
            effect: "0 disables recovery; otherwise the loop runs every 90 seconds (positive values do not change its interval)",
            off: Some("0"),
        }],
        pref: None,
        detail: Some("/api/history"),
    },
    Doc {
        id: ids::STORAGE,
        name: "Storage retention",
        purpose: "Bounds append-only history, caches, diagnostic run logs and build artifacts; preserves referenced uploads and expires transcript cache entries.",
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
        id: ids::PIPE_RECONCILE,
        name: "Session log piping",
        purpose: "Re-attaches tmux pipe-pane to any lane that lost its log writer; without it a live lane looks like one that never started.",
        env: NO_ENV,
        pref: None,
        detail: Some("/api/debug/logs"),
    },
    Doc {
        id: ids::WORKTREE_HYGIENE,
        name: "Worker worktree hygiene",
        purpose: "Re-flags out-of-cone files that a sparse worker worktree shows as deleted, which otherwise refuse the worker's rebase.",
        env: NO_ENV,
        pref: None,
        detail: None,
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
        id: ids::COMMIT_MENTION_NOTES,
        name: "Commit-mention notes",
        purpose: "Tells an open card, once, that a merged commit names it. NOTED, never closed — a \
                  mention is not proof of completion, since commits also reference cards for \
                  context, partial work and reverts. Hourly rather than on the autofix tick \
                  because the scan shells out to git across every repo behind an open card and \
                  measured ~11s.",
        env: &[EnvControl {
            var: "AMUX_COMMIT_MENTION_TICK_S",
            effect: "seconds between scans (floor 60)",
            off: None,
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
        id: ids::COMPUTER_REAPER,
        name: "Computer sandbox reaper",
        purpose: "Removes amux-labelled CUA computer-use containers that are idle, exited, duplicated for one lane, or unowned. Never starts Docker; finds containers by the amux-computer label only.",
        env: &[
            EnvControl {
                var: "AMUX_COMPUTER_IDLE_S",
                effect: "seconds without an action before a sandbox is stopped (default 900 = 15 min); 0 disables this arm",
                off: None, // disables one arm, not the running job
            },
            EnvControl {
                var: "AMUX_COMPUTER_REAP_TICK_S",
                effect: "sweep interval in seconds (default 60)",
                off: None,
            },
        ],
        pref: None,
        detail: Some("/api/computer/status"),
    },
    Doc {
        id: ids::BROWSER_REAPER,
        name: "Browser idle reaper",
        purpose: "Releases browsers that are abandoned: no verb activity in 5 min (activity arm), no real page for 1 hour (idle arm), or older than 4 hours (TTL arm). Logins survive on disk; only a relaunch is lost.",
        env: &[
            EnvControl {
                var: "AMUX_BROWSER_ACTIVITY_REAP_S",
                effect: "seconds since last verb (navigate/screenshot/action) before release (default 300 = 5 min); 0 disables this arm",
                off: None, // disables one expiry arm, not the running job
            },
            EnvControl {
                var: "AMUX_BROWSER_IDLE_REAP_S",
                effect: "seconds a profile must be continuously empty (no real pages) before release (default 3600); 0 disables this arm",
                off: None, // disables one expiry arm, not the running job
            },
            EnvControl {
                var: "AMUX_BROWSER_TTL_S",
                effect: "hard age ceiling — any browser older than this is released even with open pages (default 14400 = 4 h); 0 disables",
                off: None, // disables one expiry arm, not the running job
            },
            EnvControl {
                var: "AMUX_BROWSER_REAP_TICK_S",
                effect: "how often to check (default 120)",
                off: None,
            },
        ],
        pref: None,
        detail: Some("/api/browser/status"),
    },
    Doc {
        id: ids::TUNNEL,
        name: "Tunnel relay",
        purpose: "Long-polls the amux cloud gateway and serves each public request from a local port, so a localhost app is reachable without an inbound port. Registered only while a tunnel is running — absent here means no tunnel is up, which is also the default.",
        env: &[
            EnvControl {
                var: "AMUX_TUNNEL_TOKEN",
                effect: "the amux-cloud token the gateway authenticates; without it no tunnel can start at all",
                off: Some(OFF_WHEN_UNSET),
            },
            EnvControl {
                var: "AMUX_TUNNEL_PORT",
                effect: "the local port to auto-target at boot. UNSET means no auto-start: defaulting to amux's own port would publish an unauthenticated control plane",
                off: Some(OFF_WHEN_UNSET),
            },
            EnvControl {
                var: "AMUX_TUNNEL_GATEWAY",
                effect: "gateway base URL (default https://cloud.amux.io); point at your own for the self-hosted OSS gateway",
                off: None,
            },
            EnvControl {
                var: "AMUX_TUNNEL_RATE_PER_MIN",
                effect: "public request cap per sliding minute, shed as 429 before the local app is touched (default 180)",
                off: None,
            },
            EnvControl {
                var: "AMUX_TUNNEL_MAX_CONCURRENT",
                effect: "simultaneous local fetches; excess waits 8s then gets a 503 (default 8)",
                off: None,
            },
            EnvControl {
                var: "AMUX_TUNNEL_ALLOW_SELF",
                effect: "1 = permit tunnelling amux's OWN port. Refused by default: this port has no request auth and /api/sessions/<n>/send is ungated",
                off: None,
            },
        ],
        pref: None,
        detail: Some("/api/tunnel/status"),
    },
    Doc {
        id: ids::SELF_ADOPT,
        name: "Self-adoption watch",
        purpose: "Execs the installed binary in place when it changes on disk instead of serving stale code; a test harness may deliberately disable it while pinning one build.",
        env: &[EnvControl {
            var: "AMUX_NO_SELF_ADOPT",
            effect: "truthy = do not exec a replacement binary; the inert registry row names this switch",
            // The startup branch registers its own disabled_reason because
            // this negative boolean accepts 1/true/yes/on, not one exact
            // off-value the catalog could safely re-derive.
            off: None,
        }],
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
    Doc {
        id: ids::TELEGRAM_POLL,
        name: "Telegram poll",
        purpose: "Long-polls the Telegram bot API for messages and routes them into linked amux sessions; idles (no token) rather than erroring when TELEGRAM_BOT_TOKEN is unset.",
        env: &[EnvControl {
            var: "TELEGRAM_BOT_TOKEN",
            effect: "unset = loop idles, checking every 5 minutes; set = polls continuously",
            // The loop remains spawned and instrumented while idle, so an
            // absent connector is not the same state as a disabled job.
            off: None,
        }],
        pref: None,
        detail: Some("/api/telegram/status"),
    },
    Doc {
        id: ids::TELEGRAM_RELAY,
        name: "Telegram relay",
        purpose: "Auto-relays session replies back to Telegram when a session responds to a Telegram-routed message; works for all sessions without per-session configuration.",
        env: &[EnvControl {
            var: "TELEGRAM_BOT_TOKEN",
            effect: "unset = relay idles; set = relays enabled",
            off: Some(""),
        }],
        pref: None,
        detail: Some("/api/telegram/status"),
    },
    Doc {
        id: ids::MAC_HEALTH,
        name: "Mac process health",
        purpose: "Safely reaps aged orphaned Ray, Playwright Chrome, debug rustc, and server-owned zombie children; warns on foreign zombies and excessive claude processes. Runs every 30 minutes.",
        env: &[
            EnvControl {
                var: "AMUX_MAC_HEALTH_TICK_S",
                effect: "sweep interval in seconds (default 1800 = 30 min)",
                off: None,
            },
            EnvControl {
                var: "AMUX_MAC_HEALTH_MAX_CLAUDE",
                effect: "claude process count that triggers a WARN (default 60)",
                off: None,
            },
            EnvControl {
                var: "AMUX_MAC_HEALTH_RAY_GRACE_S",
                effect: "minimum age (seconds) before an orphaned ray:: worker is killed (default 120)",
                off: None,
            },
            EnvControl {
                var: "AMUX_MAC_HEALTH_PLAYWRIGHT_GRACE_S",
                effect: "minimum age (seconds) before an orphaned Playwright Chrome is killed (default 300)",
                off: None,
            },
            EnvControl {
                var: "AMUX_MAC_HEALTH_RUSTC_GRACE_S",
                effect: "minimum age (seconds) before an orphaned debug rustc is killed (default 600)",
                off: None,
            },
            EnvControl {
                var: "AMUX_MAC_HEALTH_ZOMBIE_GRACE_S",
                effect: "minimum age (seconds) before an owned zombie child is reaped (default 60)",
                off: None,
            },
        ],
        pref: None,
        detail: None,
    },
    Doc {
        id: ids::ACCOUNTABILITY_NUDGE,
        name: "Accountability nudge",
        purpose: "Reminds non-isolated workers when recent owner messages have produced no tracked board work; the per-worker cooldown prevents repeated nagging.",
        env: &[EnvControl {
            var: "AMUX_ACCOUNTABILITY_SWEEP_SECS",
            effect: "sweep interval in seconds (default 1800 = 30 min)",
            off: None,
        }],
        pref: None,
        detail: Some("/api/messages"),
    },
    Doc {
        id: ids::CONTEXT_HEALTH,
        name: "Context health",
        purpose: "Measures worker conversation compaction generations and warns when a lane is answering from a deeply summarized context.",
        env: NO_ENV,
        pref: None,
        detail: Some("/api/debug/context-health"),
    },
    Doc {
        id: ids::DISK_WATCH,
        name: "Disk watch",
        purpose: "Measures disk pressure and regenerable cache growth, starts bounded scans when due, and reports findings without deleting user data.",
        env: &[
            EnvControl {
                var: "AMUX_DISK_WATCH_EVERY_SECS",
                effect: "minimum seconds between routine scans (default 604800 = 7 days)",
                off: None,
            },
            EnvControl {
                var: "AMUX_DISK_WATCH_LOW_FREE_EVERY_SECS",
                effect: "minimum seconds between low-space scans (default 21600 = 6 hours)",
                off: None,
            },
        ],
        pref: None,
        detail: Some("/api/reclaim/scan"),
    },
    Doc {
        id: ids::TRACE_ARCHIVE,
        name: "Trace archive",
        purpose: "Keeps every agent trace. Sets Claude Code's cleanupPeriodDays when unset (it deletes sessions after 30 days by default) and warns when it is under a year. Gzips each Claude transcript, subagent transcript and Codex rollout that has been quiet for 2 hours into ~/.amux/traces (backed up with ~/.amux), indexed by worker with message, tool-call, error and token counts; marks traces whose original was deleted. Oldest first, bounded bytes per tick, paused while the host is critical.",
        env: &[
            EnvControl { var: "AMUX_TRACE_ARCHIVE_SECS", effect: "0 disables archiving and the retention guard", off: Some("0") },
            EnvControl { var: "AMUX_TRACE_ARCHIVE_TICK_MB", effect: "source MB archived per 10-minute tick, default 400", off: None },
            EnvControl { var: "AMUX_TRACE_ARCHIVE_QUIET_S", effect: "seconds a transcript must be unchanged before archiving, default 7200", off: None },
        ],
        pref: None,
        detail: Some("/api/traces"),
    },
    Doc {
        id: ids::HOST_METRICS,
        name: "Host metrics history",
        purpose: "Samples the host analysis /api/metrics/host serves (CPU, load, memory, swap, disk, process counts) into host_metrics, so utilization over time is answerable rather than only right now. A failed probe is recorded as an unmeasured row, never as a gap.",
        env: &[EnvControl {
            var: "AMUX_HOST_METRICS_EVERY_SECS",
            effect: "seconds between samples (default 300, floored at 60; spawn_periodic clamps 0 to 1s, so this knob has no off value)",
            off: None,
        }],
        pref: None,
        detail: Some("/api/metrics/host/history"),
    },
    Doc {
        id: ids::GOAL_KEEPER,
        name: "Goal keeper",
        purpose: "Continues a Claude worker whose owner-set /goal is not met (latest transcript goal_status met:false and the footer shows /goal active) once it has been idle 2 minutes, because Claude Code's own goal loop does not survive a resume. At most one continue per 10 minutes; two continues with no tool call stop it for that goal until the goal changes or the owner writes. Reaches isolated workers as owner configuration (goal: guard). Each tick also runs the goal-loop guard's restore (AMUX-5277): a lane parked on a needsyou card gets `/goal <condition>` back once when the card leaves needsyou, and is never continued while parked.",
        env: &[
            EnvControl { var: "AMUX_GOAL_KEEPER_SECS", effect: "0 disables the keeper", off: Some("0") },
            EnvControl { var: "AMUX_GOAL_CONTINUE", effect: "0 opts a worker, group or the fleet out (scoped)", off: None },
            EnvControl { var: "AMUX_GOAL_CONTINUE_IDLE_S", effect: "idle seconds before a continue, default 120", off: None },
            EnvControl { var: "AMUX_GOAL_CONTINUE_INTERVAL_S", effect: "minimum seconds between continues, default 600", off: None },
            EnvControl { var: "AMUX_GOAL_LOOP_GUARD", effect: "0 turns off goal-loop detection, park and restore (scoped)", off: None },
        ],
        pref: None,
        detail: None,
    },
    Doc {
        id: ids::HOST_GUARD,
        name: "Host guard",
        purpose: "Every 30s computes one host pressure level (ok/strained/critical) from load per core, memory pressure and free disk. At critical, automated loops that start work (board dispatch, schedule fires, automated worker starts) skip their tick and retry on the next; owner actions never wait on it. While strained or worse, a worker's tool process that has averaged most of a core for 15+ minutes is reniced, never killed.",
        env: &[
            EnvControl { var: "AMUX_HOST_GUARD_SECS", effect: "0 disables the guard (no level, nothing deferred, nothing reniced)", off: Some("0") },
            EnvControl { var: "AMUX_HOST_LOAD_STRAINED", effect: "load per core (5-min avg) that is strained, default 1.5", off: None },
            EnvControl { var: "AMUX_HOST_LOAD_CRITICAL", effect: "load per core (5-min avg) that is critical, default 2.5", off: None },
            EnvControl { var: "AMUX_HOST_DISK_STRAINED_GB", effect: "free GiB on the amux home volume that is strained, default 40", off: None },
            EnvControl { var: "AMUX_HOST_DISK_CRITICAL_GB", effect: "free GiB that is critical, default 10", off: None },
            EnvControl { var: "AMUX_RUNAWAY_MIN_SECS", effect: "age before a worker tool process can be a runaway, default 900", off: None },
            EnvControl { var: "AMUX_RUNAWAY_AVG_CORES", effect: "lifetime average cores that makes it a runaway, default 0.8", off: None },
            EnvControl { var: "AMUX_RUNAWAY_RENICE", effect: "0 reports runaways without renicing them", off: None },
        ],
        pref: None,
        detail: Some("/api/metrics/host/pressure"),
    },
    Doc {
        id: ids::STATUS_HISTORY,
        name: "Status history",
        purpose: "Samples each live worker's derived status so time-driven and subagent-driven state changes remain explainable after the fact.",
        env: &[
            EnvControl {
                var: "AMUX_STATUS_HISTORY_SECS",
                effect: "sample interval in seconds (default 20, minimum 5)",
                off: None,
            },
            EnvControl {
                var: "AMUX_STATUS_HISTORY_DAYS",
                effect: "retention in days (default 14)",
                off: None,
            },
        ],
        pref: None,
        detail: None,
    },
    Doc {
        id: ids::EPISODES,
        name: "Episode records",
        purpose: "Writes one self-contained row per task attempt: the card, git sha, conversation and configuration at claim, then the outcome, end sha and transcripts at close, so pruning other tables cannot erase what an attempt started from.",
        env: &[EnvControl {
            var: "AMUX_EPISODES_SECS",
            effect: "capture interval in seconds (default 15, minimum 5)",
            off: None,
        }],
        pref: None,
        detail: None,
    },
    Doc {
        id: ids::TOKEN_LEDGER,
        name: "Token ledger",
        purpose: "Indexes provider transcript usage into the durable cost ledger so worker, card, and model totals do not silently read as zero.",
        env: &[EnvControl {
            var: "AMUX_LEDGER_INDEX_SECS",
            effect: "index interval in seconds; 0 disables the job",
            off: Some("0"),
        }],
        pref: None,
        detail: None,
    },
    Doc {
        id: ids::BOARD_HYGIENE,
        name: "Board hygiene",
        purpose: "Ages needsyou cards (warn at 14d, discard at 30d), discards stale autofix todos (72h), flags stale backlog (30d never promoted), and logs per-session status counts.",
        env: &[EnvControl {
            var: "AMUX_BOARD_HYGIENE_SECS",
            effect: "tick seconds; 0 disables the job",
            off: Some("0"),
        }],
        pref: None,
        detail: None,
    },
    Doc {
        id: ids::RECORDINGS_TRANSCRIBE,
        name: "Recording transcripts",
        purpose: "Transcribes recordings synced from the Record tab with a local whisper.cpp model and writes each transcript beside its audio; without it recordings sync but never become text.",
        env: &[EnvControl {
            var: "AMUX_RECORDINGS_TRANSCRIBE_SECS",
            effect: "tick seconds; 0 disables the job",
            off: Some("0"),
        }],
        pref: None,
        detail: Some("/api/recordings/config"),
    },
    Doc {
        id: ids::MODEL_CATALOG_REFRESH,
        name: "Model catalog refresh",
        purpose: "Probes each vendor's own list-models API (Anthropic, OpenAI, Gemini) when a key is configured and caches the merged result for /api/models and every ProviderAdapter::models(). Each vendor is asked at most hourly after a success; a failed probe serves the last good listing (persisted in model-catalog-live.json, so it survives restarts) and is retried on the next 5-minute tick. A vendor with no key serves the static fallback.",
        env: &[EnvControl {
            var: "AMUX_MODEL_CATALOG_REFRESH_SECS",
            effect: "tick seconds; 0 disables the job (static catalog only)",
            off: Some("0"),
        }],
        pref: None,
        detail: Some("/api/models"),
    },
    Doc {
        id: ids::CAPTURE_RECONCILE,
        name: "Capture-card reconciliation",
        purpose: "Closes an auto-captured intake card as done when a landed commit names its MSG id \
                  (evidence written onto the card), and tags capture cards still open 24h after \
                  capture as `unreconciled`. Board notes, turn-end text and cards marked done that \
                  cite the MSG id close them immediately; this tick covers commits. AMUX-5238.",
        env: &[
            EnvControl {
                var: "AMUX_CAPTURE_RECONCILE_TICK_S",
                effect: "seconds between passes (floor 60)",
                off: None,
            },
            EnvControl {
                var: "AMUX_CAPTURE_RECONCILE_SECS",
                effect: "0 disables the job",
                off: Some("0"),
            },
            EnvControl {
                var: "AMUX_CAPTURE_RECONCILE",
                effect: "scoped per lane; 0 stops closing and tagging that lane's capture cards",
                off: None,
            },
        ],
        pref: None,
        detail: None,
    },
    Doc {
        id: ids::EMAIL_APPROVAL_DIGEST,
        name: "Email approval digest",
        purpose: "Once a local day, when outbound email drafts are waiting for approval, sends \
                  the owner one list of them on web push and the owner inbox (never SMS). \
                  Approvals for email no longer expire, so this is how the queue reaches the \
                  owner away from the dashboard. AMUX-5240.",
        env: &[
            EnvControl {
                var: "AMUX_EMAIL_APPROVAL_DIGEST",
                effect: "0 stops the digest (re-read every tick)",
                off: None,
            },
            EnvControl {
                var: "AMUX_EMAIL_APPROVAL_DIGEST_HOUR",
                effect: "local hour the digest becomes due (default 9)",
                off: None,
            },
            EnvControl {
                var: "AMUX_EMAIL_APPROVAL_DIGEST_SECS",
                effect: "0 disables the job",
                off: Some("0"),
            },
        ],
        pref: None,
        detail: Some("/api/email/approvals"),
    },
    Doc {
        id: ids::NEEDS_INPUT_AUTO,
        name: "Needs-input auto-approve",
        purpose: "Every minute, approves each NEW needs-input item that its worker's policy \
                  covers, exactly as the triage sheet's Approve does, once per item. Never \
                  money, production data, outside parties, credentials or scope decisions \
                  (contract rule 11). AMUX-5301.",
        env: &[
            EnvControl {
                var: "AMUX_NEEDS_INPUT_AUTO",
                effect: "0 in server.env stops it fleet-wide; scoped per worker/group it turns \
                         one worker or group off (re-read every tick)",
                off: None,
            },
            EnvControl {
                var: "AMUX_NEEDS_INPUT_AUTO_TICK_S",
                effect: "seconds between passes (default 60, floor 30)",
                off: None,
            },
            EnvControl {
                var: "AMUX_NEEDS_INPUT_AUTO_SECS",
                effect: "0 disables the job",
                off: Some("0"),
            },
        ],
        pref: None,
        detail: Some("/api/needs-input/auto"),
    },
    Doc {
        id: ids::CONTRACT_WATCH,
        name: "Contract watch",
        purpose: "Every 5 minutes: runs the frozen post-deploy check of each deploy card whose \
                  server-verified commit production now contains (contract rule 2b), and starts \
                  a fresh read-only reviewer on a different model for each card eligible for \
                  verified (rule 3). Three failures of either reopen or escalate the card.",
        env: NO_ENV,
        pref: None,
        detail: Some("/api/contract/counters"),
    },
    Doc {
        id: ids::LAND_QUEUE,
        name: "Land queue",
        purpose: "Every 20 seconds, takes each repository's queued lands (contract rule 5) in order, \
                  composes them onto origin/main in a server worktree, runs the land gate and the \
                  repo's pre-push hook, and pushes; a red batch is bisected and a red commit goes \
                  back to its lane unmerged.",
        env: NO_ENV,
        pref: None,
        detail: Some("/api/land"),
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
    failures: u64,
    consecutive_failures: u64,
    recoveries: u64,
    last_failure_at: Option<f64>,
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

/// MANUAL TRIGGERS (AMUX-4046). Ethan: "make it so i can manually run a system
/// schedule right then and there so that i can test it."
///
/// A separate map rather than a field on `Job`, for one reason that matters:
/// `Job` lives behind a `std::sync::Mutex`, and the tick loop has to AWAIT the
/// trigger. Handing the loop an `Arc<Notify>` cloned out of the map keeps the
/// std lock strictly synchronous and never held across an await point.
///
/// `Notify::notify_one` stores a permit when nobody is waiting, so a trigger
/// fired while the job is mid-tick is not lost — the next wait returns
/// immediately. That is the behaviour you want from a "run it now" button: the
/// press always produces a run, even if it lands during one.
fn triggers() -> &'static Mutex<BTreeMap<String, std::sync::Arc<tokio::sync::Notify>>> {
    static T: OnceLock<Mutex<BTreeMap<String, std::sync::Arc<tokio::sync::Notify>>>> =
        OnceLock::new();
    T.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// The trigger handle for `id`, created on first use so a job and its trigger
/// cannot get out of step.
pub fn trigger_handle(id: &str) -> std::sync::Arc<tokio::sync::Notify> {
    let mut g = match triggers().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    g.entry(id.to_string()).or_default().clone()
}

/// Ask a job to tick NOW. `false` means there is nothing to ask: the id is not
/// registered, or it is a `loop` job that owns its own sleep and never consults
/// a trigger. Returning false rather than silently succeeding is the point —
/// a run button that reports success while nothing runs is the failure this
/// whole module exists to prevent.
pub fn trigger(id: &str) -> bool {
    if !is_triggerable(id) {
        return false;
    }
    trigger_handle(id).notify_one();
    true
}

/// Can this job be asked to tick? Only `periodic` jobs consult a trigger, and
/// only if their loop was actually spawned (fleet isolation registers a job
/// inert-but-visible, and an inert job must not claim it can run).
pub fn is_triggerable(id: &str) -> bool {
    let g = match reg().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    g.get(id)
        .map(|j| j.kind == "periodic" && j.disabled.is_none())
        .unwrap_or(false)
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

pub(crate) fn set_abort(id: &str, abort: tokio::task::AbortHandle) {
    if let Ok(mut m) = reg().lock() { if let Some(j) = m.get_mut(id) { j.abort = Some(abort); } }
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
pub fn register_disabled(id: &str, kind: &'static str, interval: Option<Duration>, reason: String) {
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
                failures: 0,
                consecutive_failures: 0,
                recoveries: 0,
                last_failure_at: None,
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
            if j.consecutive_failures > 0 { j.recoveries += 1; j.consecutive_failures = 0; }
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
            if j.consecutive_failures > 0 { j.recoveries += 1; j.consecutive_failures = 0; }
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
    pub failures: u64,
    pub consecutive_failures: u64,
    pub recoveries: u64,
    pub last_failure_at: Option<f64>,
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
    let Ok(m) = reg().lock() else {
        return Vec::new();
    };
    m.iter()
        .map(|(id, j)| Snapshot {
            id: id.clone(),
            kind: j.kind,
            interval_s: j.interval.map(|d| d.as_secs_f64()),
            spawned_at: j.spawned_at,
            ticks: j.ticks,
            failures: j.failures,
            consecutive_failures: j.consecutive_failures,
            recoveries: j.recoveries,
            last_failure_at: j.last_failure_at,
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
            if j.consecutive_failures > 0 { j.recoveries += 1; j.consecutive_failures = 0; }
            j.last_start = Some(now);
            j.last_end = Some(now);
        }
    }
}

/// Spawn a long-lived internal loop AND register it in one call. The point is
/// that there is no way to do the first without the second.
pub fn spawn_loop<F, Fut>(
    id: &'static str,
    interval: Option<Duration>,
    mut factory: F,
) -> tokio::task::JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    if let Some(reason) = super::fleet_isolation_reason(id) {
        register_disabled(id, "loop", interval, reason.clone());
        tracing::info!(
            job = id,
            switch = %reason,
            "long-lived job suppressed: fleet isolation is on, this loop will NOT run"
        );
        return tokio::spawn(async {});
    }
    register(id, "loop", interval, None);
    let h = super::executor::spawn(async move {
        let mut failures = 0u64;
        loop {
            if guard_job(id, async { super::poll_watch::watch(id, factory()).await }).await {
                // Normal completion can mean an explicit stop (the tunnel's
                // generation fence). Never resurrect an owner-stopped loop.
                break;
            }
            failures = failures.saturating_add(1);
            tokio::time::sleep(recovery_delay(interval.unwrap_or(Duration::from_secs(1)), failures)).await;
        }
    });
    set_abort(id, h.abort_handle());
    h
}

/// Catch a failed poll without losing the consumer or its cleanup. Each
/// consumer re-reads durable claims on its next pass; no side effect is replayed
/// just because its future panicked. Do not include panic payloads in logs.
pub(crate) async fn guard_job(id: &str, fut: impl std::future::Future<Output = ()>) -> bool {
    use futures::FutureExt;
    if std::panic::AssertUnwindSafe(fut).catch_unwind().await.is_ok() { return true; }
    let mut failures = 1;
    if let Ok(mut m) = reg().lock() {
        if let Some(j) = m.get_mut(id) {
            j.failures = j.failures.saturating_add(1);
            j.consecutive_failures = j.consecutive_failures.saturating_add(1);
            j.last_failure_at = Some(unix_now());
            j.last_start = None;
            failures = j.consecutive_failures;
        }
    }
    tracing::error!(job = id, failures, measured = true, n_considered = 1,
        verdict = "runtime_job_panic_recovering", "job panicked; durable state retained and recovery scheduled with bounded backoff");
    false
}

pub(crate) fn recovery_delay(interval: Duration, failures: u64) -> Duration {
    let base = interval.clamp(Duration::from_millis(10), Duration::from_secs(1));
    (base * (1u32 << failures.saturating_sub(1).min(15))).min(Duration::from_secs(300))
}

/// Register a loop somebody else already spawned (its `spawn()` owns the
/// `tokio::spawn`). Same contract as [`spawn_loop`], called at the same place.
pub fn adopt(id: &'static str, interval: Option<Duration>, h: &tokio::task::JoinHandle<()>) {
    if let Some(reason) = super::fleet_isolation_reason(id) {
        h.abort();
        register_disabled(id, "loop", interval, reason.clone());
        tracing::info!(
            job = id,
            switch = %reason,
            "adopted job suppressed: fleet isolation is on, its task was aborted"
        );
        return;
    }
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

/// Extend the liveness verdict with the duration of the last completed tick.
/// This is kept beside [`classify`] so the endpoint and autofix cannot disagree
/// about a tick that was hung long enough to exceed its budget but eventually
/// returned before either observer looked.
pub fn classify_observed(f: &Facts, last_tick_ms: Option<f64>, now: f64) -> &'static str {
    let status = classify(f, now);
    if status == "ok"
        && last_tick_ms
            .zip(f.interval_s)
            .is_some_and(|(ms, interval)| ms > stall_after_s(interval) * 1000.0)
    {
        "slow"
    } else {
        status
    }
}

/// One unhealthy system-job observation, derived from the same registry and
/// staleness predicate as `GET /api/system-jobs`. Autofix consumes this rather
/// than maintaining a second definition of "stalled" that could disagree with
/// the red row a human sees in the Scheduler tab.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthIssue {
    pub id: String,
    pub name: String,
    pub status: &'static str,
    pub interval_s: Option<f64>,
    pub ticks: u64,
    pub last_tick_age_s: Option<f64>,
    pub last_tick_ms: Option<f64>,
    pub in_flight_age_s: Option<f64>,
    pub documented: bool,
}

/// Unhealthy jobs at `now`, including catalogued jobs that were never spawned
/// and registered jobs whose documentation was forgotten. A completed tick
/// that exceeded the same budget used for `hung` is retained as `slow` until
/// the next tick, so scheduler latency is observable even after the closure
/// finally returns.
pub fn health_issues(now: f64) -> Vec<HealthIssue> {
    let live: BTreeMap<String, Snapshot> =
        snapshot().into_iter().map(|s| (s.id.clone(), s)).collect();
    let mut all: Vec<String> = CATALOG.iter().map(|d| d.id.to_string()).collect();
    for id in live.keys() {
        if !all.iter().any(|x| x == id) {
            all.push(id.clone());
        }
    }

    let mut out = Vec::new();
    for id in all {
        let d = doc_for(&id);
        let disabled_by_control = d.map(|d| env_json(d).1).unwrap_or(false);
        let l = live.get(&id);
        let f = Facts {
            spawned: l.is_some(),
            dead: l.map(|x| x.dead).unwrap_or(false),
            disabled: disabled_by_control || l.and_then(|x| x.disabled_reason.as_ref()).is_some(),
            interval_s: l.and_then(|x| x.interval_s),
            spawned_at: l.map(|x| x.spawned_at),
            last_tick_at: l.and_then(|x| x.last_tick_at),
            in_flight_since: l.and_then(|x| x.in_flight_since),
            instrumented: l
                .map(|x| x.ticks > 0 || x.in_flight_since.is_some())
                .unwrap_or(false),
        };
        let status = if !f.disabled && l.is_some_and(|x| x.consecutive_failures > 0) { "recovering" }
            else { classify_observed(&f, l.and_then(|x| x.last_tick_ms), now) };
        if !matches!(status, "stalled" | "dead" | "hung" | "not_spawned" | "slow" | "recovering") {
            continue;
        }
        out.push(HealthIssue {
            id: id.clone(),
            name: d.map(|d| d.name).unwrap_or(&id).to_string(),
            status,
            interval_s: f.interval_s,
            ticks: l.map(|x| x.ticks).unwrap_or(0),
            last_tick_age_s: f.last_tick_at.map(|t| now - t),
            last_tick_ms: l.and_then(|x| x.last_tick_ms),
            in_flight_age_s: f.in_flight_since.map(|t| now - t),
            documented: d.is_some(),
        });
    }
    out
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
                "{} table(s) swept, {} file(s) and {} directory(s) removed, {} freed, {} cache entries expired{}",
                r.tables.len(),
                r.files_removed + r.rotated_logs_removed,
                r.dirs_removed + r.run_logs.removed,
                human_bytes(r.bytes_freed + r.rotated_logs_freed + r.dir_bytes_freed + r.run_logs.bytes_freed),
                r.memory_entries_removed,
                if r.upload_refs_error.is_some() || !r.run_logs.measured || r.diagnostic_dirs.iter().any(|(_, d)| !d.measured) { "; some cleanup deferred (see storage diagnostics)" } else { "" }
            )
        }),
        ids::SCAN => crate::orchestrator::scan::last_scan_state().map(|s| {
            format!(
                "{} scanned, {} demoted (structured), {} demoted (native), {} capture failure(s), {} process exit(s), {} exit probe/apply failure(s), {} stale exit observation(s)",
                s.report.scanned.len(),
                s.report.demoted_structured.len(),
                s.report.demoted_native.len(),
                s.report.capture_failures.len(),
                s.report.process_exits.len(),
                s.report.process_exit_failures.len(),
                s.report.stale_process_exits.len(),
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
    state.store.read().ok().and_then(|c| {
        c.query_row("SELECT value FROM prefs WHERE key=?1", [key], |r| r.get(0))
            .ok()
    })
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
            "value": redact_env(e.var, val.as_deref()),
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

/// Env vars whose NAME says the value is a credential. Matched as substrings of
/// the uppercased name, so a var nobody has written yet is covered too.
const SECRET_ENV_MARKERS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "_KEY",
    "APIKEY",
    "CREDENTIAL",
];

/// Is this env var's VALUE a credential that must never be rendered?
pub fn is_secret_env(var: &str) -> bool {
    let up = var.to_ascii_uppercase();
    SECRET_ENV_MARKERS.iter().any(|m| up.contains(m))
}

/// What `/api/system-jobs` may publish for an env var (AMUX-3817).
///
/// FOUND LIVE: adding `AMUX_TUNNEL_TOKEN` to a job's CATALOG entry made this
/// endpoint print the token in plaintext, because every env control rendered
/// its raw value and until then none of them held a secret. That is a
/// credential leaving `~/.amux/server.env`, which is the one place values are
/// supposed to live, through an endpoint whose job is documentation.
///
/// A SET SECRET REPORTS AS SET, NOT AS ABSENT. The `off_now` flag beside it is
/// computed from the real value and is the fact the UI needs; blanking the
/// field to `null` would make a configured token indistinguishable from a
/// missing one, which is the ethos-4 failure and would send someone to set a
/// var that is already set.
///
/// Matched on the NAME rather than a per-entry flag on purpose: a flag is a
/// thing to remember, and the next person adding a `*_TOKEN` to a catalog entry
/// should not have to.
fn redact_env(var: &str, val: Option<&str>) -> Value {
    match val {
        None => Value::Null,
        Some(v) if is_secret_env(var) => {
            if v.trim().is_empty() {
                json!("")
            } else {
                json!(format!("(set, {} chars, redacted)", v.chars().count()))
            }
        }
        Some(v) => json!(v),
    }
}

/// The live switch, if this job has one. A pref is re-read by the job on every
/// tick, which is exactly what makes it safe to render as a checkbox.
fn pref_json(state: &AppState, d: &Doc) -> Option<Value> {
    let p = d.pref?;
    let val = pref(state, p.key);
    let off = matches!(
        val.as_deref().map(str::trim),
        Some("0") | Some("false") | Some("off")
    );
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
/// POST /api/system-jobs/{id}/run — tick a background job NOW (AMUX-4046).
///
/// Ethan: "make it so i can manually run a system schedule right then and there
/// so that i can test it." The section this serves is amux's own plumbing,
/// which is deliberately not editable — no edit, no delete, because it is
/// machinery the user cannot own. Running one is a different act: it changes
/// nothing about the job, it just stops you waiting up to an hour to find out
/// whether a change works.
///
/// REFUSES RATHER THAN LYING when it cannot deliver. A `loop` job owns its own
/// sleep and never consults a trigger, so asking it to run would do nothing at
/// all; a 409 saying so is worth more than a 200 that looks like it worked.
/// That is this view's founding rule applied to its newest button — three loops
/// were dead for hours because a job with nothing to do and a job that is not
/// running produced identical evidence.
pub async fn run_system_job(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let known = {
        let g = match reg().lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.get(&id).map(|j| (j.kind, j.disabled.clone()))
    };
    let Some((kind, disabled)) = known else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"error": format!("no system job '{id}'")})),
        )
            .into_response();
    };
    if let Some(why) = disabled {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(json!({
                "error": format!("'{id}' is disabled and its loop never spawned"),
                "disabled_by": why,
                "hint": "clear the switch above, then run it",
            })),
        )
            .into_response();
    }
    if !trigger(&id) {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(json!({
                "error": format!("'{id}' cannot be run on demand"),
                "kind": kind,
                "why": "this job owns its own sleep loop rather than ticking through the \
                        shared periodic driver, so there is nothing to signal",
            })),
        )
            .into_response();
    }
    axum::Json(json!({"ok": true, "id": id, "queued": true})).into_response()
}

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
        let status = if !f.disabled && l.is_some_and(|x| x.consecutive_failures > 0) { "recovering" }
            else { classify_observed(&f, l.and_then(|x| x.last_tick_ms), now) };
        if matches!(status, "stalled" | "dead" | "hung" | "not_spawned" | "slow" | "recovering") {
            unhealthy += 1;
        }
        jobs.push(json!({
            "id": id,
            "name": d.map(|d| d.name).unwrap_or(id.as_str()),
            "purpose": d.map(|d| d.purpose),
            "documented": d.is_some(),
            "kind": l.map(|x| x.kind),
            // CAN THIS BE RUN ON DEMAND (AMUX-4046)? Published rather than
            // inferred client-side, because the answer depends on HOW the job
            // was spawned: `periodic` jobs wait on a trigger in the shared
            // loop, while `loop` jobs own their own sleep and never consult
            // one. A UI that guessed would offer a button that silently does
            // nothing, which is the exact failure this whole view exists to
            // prevent (a dead job and a quiet one must not look alike).
            "triggerable": is_triggerable(&id),
            "interval_s": f.interval_s,
            "stale_after_s": f.interval_s.map(stall_after_s),
            "spawned": f.spawned,
            "spawned_at": f.spawned_at,
            "uptime_s": f.spawned_at.map(|s| now - s),
            "ticks": l.map(|x| x.ticks).unwrap_or(0),
            "failures": l.map(|x| x.failures).unwrap_or(0),
            "consecutive_failures": l.map(|x| x.consecutive_failures).unwrap_or(0),
            "recoveries": l.map(|x| x.recoveries).unwrap_or(0),
            "last_failure_at": l.and_then(|x| x.last_failure_at),
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
        "stall_rule": "a job is STALLED when its last tick is older than 2.5x its interval + 15s; a completed tick over the same budget is SLOW until its next result",
    });
    (axum::http::StatusCode::OK, axum::Json(body)).into_response()
}

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/api/system-jobs", axum::routing::get(system_jobs))
        .route(
            "/api/system-jobs/{id}/run",
            axum::routing::post(run_system_job),
        )
}

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn a_long_lived_consumer_recovers_a_panic_but_honors_normal_completion_and_abort() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let job = super::spawn_loop("supervision-loop-test", Some(std::time::Duration::from_millis(10)), move || {
            let counted = counted.clone();
            async move {
                if counted.fetch_add(1, Ordering::SeqCst) == 0 { panic!("injected consumer failure"); }
                super::tick("supervision-loop-test");
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), job).await.unwrap().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "normal completion is not endlessly restarted");
        let row = super::snapshot().into_iter().find(|r| r.id == "supervision-loop-test").unwrap();
        assert_eq!((row.failures, row.consecutive_failures, row.recoveries), (1, 0, 1));
        let aborted = super::spawn_loop("supervision-abort-test", None, || async { std::future::pending::<()>().await; });
        aborted.abort();
        assert!(aborted.await.unwrap_err().is_cancelled());
        assert_eq!(super::recovery_delay(std::time::Duration::from_secs(60), 1000), std::time::Duration::from_secs(300));
    }
    use super::*;

    /// AMUX-3817: `/api/system-jobs` printed AMUX_TUNNEL_TOKEN in plaintext.
    ///
    /// Every env control rendered its raw value, which was harmless until a
    /// catalog entry named a credential — then a documentation endpoint became
    /// a way to read a secret out of `~/.amux/server.env`. Caught by reading a
    /// live response, not by any test, which is why this one exists.
    #[test]
    fn a_secret_env_var_reports_as_set_without_reporting_its_value() {
        let r = redact_env("AMUX_TUNNEL_TOKEN", Some("lcjRDtvwLhyVp9wZ"));
        let s = r.as_str().unwrap_or_default();
        assert!(!s.contains("lcjRDtvw"), "the value must not appear: {r}");
        // SET, not absent. Blanking it to null would make a configured token
        // indistinguishable from a missing one and send someone to set a var
        // that is already set (ethos rule 4).
        assert!(
            s.contains("set"),
            "a configured secret must still read as configured: {r}"
        );
        assert!(
            s.contains("16"),
            "length is a useful, non-disclosing fact: {r}"
        );

        // Every shape of name that carries a credential.
        for v in [
            "AMUX_TUNNEL_TOKEN",
            "OPENAI_API_KEY",
            "DB_PASSWORD",
            "x_secret",
            "MY_CREDENTIAL",
        ] {
            assert!(is_secret_env(v), "{v} names a credential");
        }
        // THE CONTROLS. A matcher that flagged everything would pass the whole
        // block above and blank the readouts this endpoint exists for.
        for v in [
            "AMUX_TUNNEL_PORT",
            "AMUX_RS_SCHEDULER",
            "AMUX_BOARD_DRIVE_SECS",
            "AMUX_TUNNEL_GATEWAY",
        ] {
            assert!(!is_secret_env(v), "{v} is a knob, not a secret");
            assert_eq!(
                redact_env(v, Some("180")),
                json!("180"),
                "{v} must render its value"
            );
        }
        // Unset stays null and empty stays empty, for both kinds: `off_now` is
        // computed from the real value, and these two are what the UI reads to
        // tell "not configured" from "configured".
        assert_eq!(redact_env("AMUX_TUNNEL_TOKEN", None), Value::Null);
        assert_eq!(redact_env("AMUX_TUNNEL_TOKEN", Some("  ")), json!(""));
    }

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
    fn completed_tick_over_the_liveness_budget_is_slow() {
        let f = base(); // 20s interval -> 65s budget
        assert_eq!(classify_observed(&f, Some(64_999.0), T + 1.0), "ok");
        assert_eq!(classify_observed(&f, Some(65_001.0), T + 1.0), "slow");
        // A stronger liveness failure is never hidden by the duration label.
        assert_eq!(classify_observed(&f, Some(90_000.0), T + 70.0), "stalled");
    }

    #[test]
    fn stall_budget_scales_with_the_interval() {
        // The hourly storage sweep must not read as stalled at 10 minutes...
        let f = Facts {
            interval_s: Some(3600.0),
            ..base()
        };
        assert_eq!(classify(&f, T + 600.0), "ok");
        // ...but does after 2.5 hours.
        assert_eq!(classify(&f, T + 9100.0), "stalled");
        // A 5s loop is stalled in well under a minute — the same rule, not a
        // special case.
        let g = Facts {
            interval_s: Some(5.0),
            ..base()
        };
        assert_eq!(classify(&g, T + 20.0), "ok");
        assert_eq!(classify(&g, T + 40.0), "stalled");
    }

    #[test]
    fn never_ticked_is_starting_then_stalled() {
        let f = Facts {
            last_tick_at: None,
            ..base()
        };
        assert_eq!(classify(&f, T + 10.0), "starting");
        assert_eq!(classify(&f, T + 100.0), "stalled");
    }

    #[test]
    fn retired_commit_nudge_is_not_a_missing_runtime_job() {
        assert!(doc_for("commit-nudge").is_none());
        assert!(!health_issues(unix_now()).iter().any(|issue| issue.id == "commit-nudge"));
    }

    /// The distinction that cost hours: a loop nobody started, versus one a
    /// human switched off. Both are "not running"; only one is a bug.
    #[test]
    fn unspawned_is_loud_unless_a_human_turned_it_off() {
        let f = Facts {
            spawned: false,
            ..Default::default()
        };
        assert_eq!(classify(&f, T), "not_spawned");
        let g = Facts {
            spawned: false,
            disabled: true,
            ..Default::default()
        };
        assert_eq!(classify(&g, T), "disabled");
        // A job whose loop was spawned and immediately returned because its
        // env var says 0 is OFF, not dead. Red
        // for a switch someone deliberately flipped teaches people to ignore
        // red.
        let h = Facts {
            spawned: true,
            dead: true,
            disabled: true,
            ..base()
        };
        assert_eq!(classify(&h, T + 1.0), "disabled");
    }

    /// A wedged tick keeps `last_tick_at` fresh (it is the END of the PREVIOUS
    /// tick) for a whole budget after the hang starts, so freshness alone
    /// reports a hung job as healthy. This is the case a "last run" column
    /// cannot express at all.
    #[test]
    fn a_tick_that_never_ends_reads_as_hung_not_ok() {
        let f = Facts {
            in_flight_since: Some(T),
            ..base()
        }; // 65s budget
           // NEGATIVE CONTROL: a tick in flight for 30s is just a slow tick.
        assert_eq!(classify(&f, T + 30.0), "ok");
        assert_eq!(classify(&f, T + 70.0), "hung");
    }

    #[test]
    fn dead_outranks_freshness() {
        // A task that panicked keeps its last (fresh) tick timestamp forever,
        // so freshness alone would report it healthy.
        let f = Facts {
            dead: true,
            ..base()
        };
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
        let j = m
            .get(id)
            .expect("spawn_periodic_every must register the job");
        assert_eq!(j.interval, Some(Duration::from_millis(20)));
        assert!(
            j.ticks >= 2,
            "ticks recorded by the spawner, got {}",
            j.ticks
        );
        assert!(j.last_end.is_some());
        drop(m);
        t.abort();
    }

    #[tokio::test]
    async fn isolation_suppresses_spawned_and_adopted_loops_before_they_act() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        const SPAWNED: &str = "af69-isolated-spawn-loop-probe";
        const ADOPTED: &str = "af69-isolated-adopt-loop-probe";
        let spawned_var = super::super::per_job_disable_var(SPAWNED);
        let adopted_var = super::super::per_job_disable_var(ADOPTED);
        std::env::set_var(&spawned_var, "0");
        std::env::set_var(&adopted_var, "0");

        let spawned_count = Arc::new(AtomicUsize::new(0));
        let count = spawned_count.clone();
        let spawned = spawn_loop(SPAWNED, Some(Duration::from_millis(20)), move || {
            let count = count.clone();
            async move { count.fetch_add(1, Ordering::SeqCst); }
        });

        let adopted_count = Arc::new(AtomicUsize::new(0));
        let count = adopted_count.clone();
        let adopted = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            count.fetch_add(1, Ordering::SeqCst);
        });
        adopt(ADOPTED, Some(Duration::from_millis(20)), &adopted);
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(spawned_count.load(Ordering::SeqCst), 0);
        assert_eq!(adopted_count.load(Ordering::SeqCst), 0);
        for (id, reason) in [
            (SPAWNED, "AMUX_AF69_ISOLATED_SPAWN_LOOP_PROBE_SECS=0"),
            (ADOPTED, "AMUX_AF69_ISOLATED_ADOPT_LOOP_PROBE_SECS=0"),
        ] {
            let row = snapshot()
                .into_iter()
                .find(|s| s.id == id)
                .expect("suppressed loop visible");
            assert_eq!(row.disabled_reason.as_deref(), Some(reason));
            assert_eq!(row.ticks, 0);
        }

        spawned.abort();
        adopted.abort();
        std::env::remove_var(spawned_var);
        std::env::remove_var(adopted_var);
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
            assert!(
                !d.purpose.is_empty(),
                "{}: purpose is the whole point",
                d.id
            );
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
        assert!(
            undocumented.is_empty(),
            "job ids with no CATALOG row: {undocumented:?}"
        );
        assert!(
            phantom.is_empty(),
            "CATALOG rows with no id constant: {phantom:?}"
        );
    }
}
