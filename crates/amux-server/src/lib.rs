#![recursion_limit = "256"] // 40-key json! literals (session card parity)
//! amux server: HTTP API, SQLite store, orchestrator runtime.
//!
//! Module layout mirrors docs/rust-rebuild-plan.md §Crate structure. Modules
//! land phase by phase; each `pub mod` line appears when its RR item starts.

pub mod api;
pub mod backend;
pub mod config;
pub mod legacy_port;
pub mod log_dedupe;

/// Should this process exec itself when its binary changes on disk? (AEAB-52)
///
/// Opt-OUT: absent or empty means YES, so production keeps today's behaviour with
/// no configuration and only a caller that KNOWS it pinned a build has to say so.
/// The accepted spellings mirror the rest of amux's boolean env vars, and `0` /
/// `false` / `off` / `no` explicitly re-ENABLE — a var that disabled on any value
/// would make `AMUX_NO_SELF_ADOPT=0` mean the opposite of what it reads like,
/// which is the kind of surprise that gets discovered during an incident.
///
/// A free function rather than an inline `env::var` so the predicate can be
/// tested both ways. Inline, the only way to exercise it would be to boot a
/// server and touch its binary.
pub(crate) fn self_adopt_enabled() -> bool {
    match std::env::var("AMUX_NO_SELF_ADOPT") {
        Err(_) => true,
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "false" | "off" | "no"),
    }
}

#[cfg(test)]
mod self_adopt_tests {
    use super::self_adopt_enabled;

    /// The env var is process-global and cargo runs tests in parallel, so this
    /// takes a lock — the du-budget tests in autofix.rs were already bitten by
    /// exactly that (AEAB-33).
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
        L.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// BOTH directions. A seam that disabled self-adoption unconditionally would
    /// pass any "does not exec" assertion perfectly while silently turning the
    /// feature off for the whole fleet — the default case is the one that
    /// protects production, so it is asserted first.
    #[test]
    fn self_adoption_is_on_by_default_and_off_only_when_asked() {
        let _g = lock();
        std::env::remove_var("AMUX_NO_SELF_ADOPT");
        assert!(self_adopt_enabled(), "absent must mean ENABLED — production sets nothing");

        for on in ["1", "true", "yes", "TRUE", " 1 "] {
            std::env::set_var("AMUX_NO_SELF_ADOPT", on);
            assert!(!self_adopt_enabled(), "AMUX_NO_SELF_ADOPT={on:?} must disable");
        }
        // The reading-comprehension case: `=0` must NOT disable. A var whose name
        // is a negative and whose value is a negative is where an operator gets
        // it backwards, so it is pinned rather than left to intuition.
        for off in ["0", "false", "off", "no", ""] {
            std::env::set_var("AMUX_NO_SELF_ADOPT", off);
            assert!(self_adopt_enabled(), "AMUX_NO_SELF_ADOPT={off:?} must leave it ENABLED");
        }
        std::env::remove_var("AMUX_NO_SELF_ADOPT");
    }
}
pub mod db;
pub mod integrations;
pub mod invariants;
pub mod opencode;
pub mod orchestrator;
pub mod provider;
pub mod push;
pub mod runtime_jobs;
pub mod tls;

use std::sync::Arc;
use std::time::Instant;

/// Content hash of this binary, computed once at startup. The discriminator
/// that answers "did the server change underneath me" (CLAUDE.md workflow
/// rule; ethos rule 4). Falls back to the compile-time version when the
/// binary path is unreadable.
pub fn build_hash() -> String {
    (|| -> Option<String> {
        let exe = std::env::current_exe().ok()?;
        let bytes = std::fs::read(exe).ok()?;
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(&bytes);
        Some(hex::encode(&h.finalize()[..8]))
    })()
    .unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")))
}

pub fn run() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async_main());
}

/// Store-backed [`opencode::structured::ConversationSink`] (AMUX-2613
/// gap 2): captured conversation refs land in `_amux_conversations`
/// (migration 0011) so backend::bootstrap hydrates them back after a
/// restart. Fire-and-forget by contract — a persistence hiccup must never
/// stall a turn's reader task, so failures are logged, not propagated.
/// `applied: false` on purpose: this is protocol plumbing state with no
/// entity/SSE consumer, so it must not bump the global revision (Invariant
/// 37 gates rev on entity-visible change; the write itself still commits).
struct StoreConversationSink {
    store: db::SharedStore,
}

impl opencode::structured::ConversationSink for StoreConversationSink {
    fn save(&self, worker: &amux_core::ids::WorkerId, family: &str, conversation_ref: &str) {
        let store = self.store.clone();
        let (w, f, c) = (worker.to_string(), family.to_string(), conversation_ref.to_string());
        tokio::spawn(async move {
            let (ww, ff, cc) = (w.clone(), f, c);
            let res = store
                .write_async(move |conn| {
                    conn.execute(
                        "INSERT INTO _amux_conversations
                             (worker_id, provider, conversation_ref, updated_at)
                         VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(worker_id) DO UPDATE SET
                             provider = ?2, conversation_ref = ?3, updated_at = ?4",
                        rusqlite::params![ww, ff, cc, chrono::Utc::now().to_rfc3339()],
                    )?;
                    Ok(db::WriteOutcome { applied: false, events: vec![] })
                })
                .await;
            if let Err(e) = res {
                tracing::warn!(worker = %w, error = %e, "conversation ref persist failed");
            }
        });
    }

    fn forget(&self, worker: &amux_core::ids::WorkerId) {
        let store = self.store.clone();
        let w = worker.to_string();
        tokio::spawn(async move {
            let ww = w.clone();
            let res = store
                .write_async(move |conn| {
                    conn.execute(
                        "DELETE FROM _amux_conversations WHERE worker_id = ?1",
                        rusqlite::params![ww],
                    )?;
                    Ok(db::WriteOutcome { applied: false, events: vec![] })
                })
                .await;
            if let Err(e) = res {
                tracing::warn!(worker = %w, error = %e, "conversation ref forget failed");
            }
        });
    }
}

async fn async_main() {
    // rustls refuses to guess when both ring and aws-lc-rs are in the
    // dependency graph (reqwest pulls one, axum-server the other). Pin ring
    // explicitly or the first TLS handshake panics the accept loop.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Config first: the log-file path below needs amux_home.
    let cfg = config::ServerConfig::from_process_env();

    // Tracing tees to stdout AND ~/.amux/logs/server-rs.log (AMUX-2605):
    // the file is what GET /api/logs/raw tails — python parity, where the
    // Logs tab's raw view reads the server's own log. ANSI off so the file
    // (and the SPA's raw view) gets clean text. If the file cannot be
    // opened, stdout-only — logging setup must never stop the server.
    let env_filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())
    };
    /// Is fd 1 the very file we just opened? See the call site for why this is
    /// (dev, ino) and not a path comparison. Non-unix has no `/dev/fd`, and no
    /// launchd either, so the tee is correct there.
    fn stdout_is_same_file(f: &std::fs::File) -> bool {
        #[cfg(unix)]
        {
            use std::os::fd::AsFd;
            use std::os::unix::fs::MetadataExt;
            // fstat the DESCRIPTOR, never stat("/dev/fd/1"): on macOS that path
            // stats the /dev entry itself and reports a character device, so it
            // never matches a regular file. The first cut of this fix did
            // exactly that, shipped, and reported tee_to_stdout=true against a
            // log that was provably still doubling — a probe guessing where the
            // answer lived and missing by one layer.
            //
            // try_clone_to_owned() dups fd 1; dropping the dup does not close
            // stdout, so this stays safe with no `unsafe` and no mem::forget.
            let Ok(a) = f.metadata() else { return false };
            let same = std::io::stdout()
                .as_fd()
                .try_clone_to_owned()
                .map(std::fs::File::from)
                .and_then(|sf| sf.metadata())
                .map(|b| a.dev() == b.dev() && a.ino() == b.ino())
                // Unknown: keep the tee. Over-logging is recoverable; losing the
                // log because a stat failed is not.
                .unwrap_or(false);
            same
        }
        #[cfg(not(unix))]
        {
            let _ = f;
            false
        }
    }
    let log_file = {
        let dir = cfg.amux_home.join("logs");
        std::fs::create_dir_all(&dir).ok();
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("server-rs.log"))
            .ok()
    };
    match log_file {
        Some(f) => {
            use tracing_subscriber::fmt::writer::MakeWriterExt;
            // ...but NOT when stdout ALREADY IS that file, which under launchd
            // it always is: com.amux.server-rs.plist sets both StandardOutPath
            // and StandardErrorPath to this exact path. Teeing then writes every
            // line twice — measured 2026-08-11 on the live log, 1,485 unique
            // lines out of 3,000.
            //
            // That is not merely wasted bytes, it is a CORRUPT INSTRUMENT: this
            // file is what `GET /api/logs/raw` tails and what the SPA's Logs tab
            // renders, so every count taken from it — by a human, a sweep, or an
            // autofix loop — was inflated ~2x, and inflated silently, because
            // duplicate log lines look exactly like a thing that really happened
            // twice. It cost me a wrong first reading of my own AMUX-2909 probe
            // (one delivery, reported by the log as two).
            //
            // Compared by (dev, ino) rather than by path: launchd hands us an
            // already-open fd and the path could be a symlink, a relative spelling
            // or a rotated file, so string comparison would silently miss.
            let dup = stdout_is_same_file(&f);
            let sub = tracing_subscriber::fmt().with_env_filter(env_filter()).with_ansi(false);
            if dup {
                sub.with_writer(Arc::new(f)).init()
            } else {
                sub.with_writer(std::io::stdout.and(Arc::new(f))).init()
            }
            // Say which way it went, once, so "why is my log duplicated / empty"
            // is answerable from the log itself instead of from this comment.
            tracing::info!(
                tee_to_stdout = !dup,
                "log writer configured (stdout suppressed when it is the same file — AMUX-2906)"
            );
        }
        None => tracing_subscriber::fmt().with_env_filter(env_filter()).init(),
    }

    tracing::info!(port = cfg.port, db = %cfg.db_path.display(), "starting amux-rust");

    // WAS THIS RESTART ANNOUNCED? (AF-176)
    //
    // A planned self-adoption sets AMUX_SELF_ADOPTED on the exec'd image. Its
    // ABSENCE means the previous process stopped without saying so — a death,
    // a SIGKILL, or a launchd stop — and until now nothing distinguished those
    // from the 27 routine self-adoptions that happen on a busy day. Measured
    // 2026-08-23: 28 starts, 27 announced, ONE not, and that one was a
    // 107-second outage with ~50 lanes attached that surfaced only because the
    // latency detector misfiled it as slow requests (AMUX-3570).
    //
    // The outage length needs no new state: the request log's newest row is
    // when the fleet last got an answer. `env::var` is read BEFORE the runtime
    // starts so nothing can have cleared it.
    let self_adopted = std::env::var("AMUX_SELF_ADOPTED").is_ok();
    std::env::remove_var("AMUX_SELF_ADOPTED");
    // SAY IT AT BOOT, not only when a duration threshold happens to trip. This
    // is one line rather than a new detector because heartbeat.rs already owns
    // outage reporting and is being extended by another session right now; a
    // second reporter beside it is how two spellings of one alarm start
    // drifting. The fact belongs on the record either way — an unannounced stop
    // is worth knowing at ANY length, and the 2026-08-23 22:28 one was 107s
    // against a 120s threshold, so nothing said anything at all.
    if self_adopted {
        tracing::info!("boot: self-adoption (the previous process exec'd this one deliberately)");
    } else {
        tracing::warn!(
            "boot: UNANNOUNCED — no self-adoption marker, so the previous process stopped \
             without saying so (death, SIGKILL, or a launchd stop). A planned rebuild sets \
             AMUX_SELF_ADOPTED; its absence is the discriminator (AF-176). Duration, if any, \
             is reported separately by the heartbeat's downtime check."
        );
    }

    let store = match db::Store::open(&cfg.db_path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::error!(error = %e, "store open failed");
            std::process::exit(1);
        }
    };

    // AEAB-29: name the outage that preceded this boot, BEFORE anything else
    // reports a duration. amux was down 4h26m on 2026-08-18 and the only
    // evidence anywhere was a gap between two log lines — an absence nothing
    // counted. This runs before the background loops so a stall/rot/schedule
    // reader that starts up cannot report an age spanning the outage without
    // the outage itself already being on the record.
    runtime_jobs::heartbeat::record_boot(&store, cfg.port);

    // Migration-rehearsal mode (Phase 11): open + migrate + report + exit.
    // Lets docs/rust-migration/migration-rehearsal.sh exercise the EXACT production
    // migration path against a DB copy without binding ports.
    if cfg.env.get("AMUX_RS_MIGRATE_ONLY").map(|v| v == "1").unwrap_or(false) {
        let conn = store.read().expect("read after migrate");
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(-1);
        let migrations: i64 = conn
            .query_row("SELECT COUNT(*) FROM _amux_migrations", [], |r| r.get(0))
            .unwrap_or(-1);
        println!(
            "{}",
            serde_json::json!({
                "migrate_only": true,
                "tables": tables,
                "migrations_applied": migrations,
            })
        );
        return;
    }

    // AMUX_AUTH_TOKEN parity (amux-server.py:701): a non-empty env value IS
    // the token, the literal "none" disables auth, otherwise the token file
    // shared with the Python server.
    let auth_token = match cfg.env.get("AMUX_AUTH_TOKEN").map(|s| s.trim()) {
        Some(v) if v.eq_ignore_ascii_case("none") => None,
        Some(v) if !v.is_empty() => Some(v.to_string()),
        _ => api::auth::load_or_create_token(&cfg.auth_token_path()).ok(),
    };

    // RR-0092: at boot no amux-launched Chrome exists, so Singleton* locks in
    // amux-owned profile dirs are stale by definition and would block the next
    // launch (AMUX-2070). Only touches ~/.amux/playwright-auth — never the
    // user's real Chrome dir. Logs WHAT was cleaned, not just that it ran.
    for (dir, removed) in integrations::browser::reconcile_locks_at_startup(&cfg.amux_home) {
        tracing::info!(dir = %dir.display(), locks = ?removed, "cleaned stale Chrome profile locks");
    }

    let state = api::AppState {
        store: store.clone(),
        started: Instant::now(),
        build_hash: build_hash(),
        auth_token,
    };
    // EVERY BACKGROUND LOOP BELOW GOES THROUGH `registry::spawn_loop`, and
    // that is not a style preference. Three of these were dead or had never
    // been spawned at all — for hours, silently, because a loop that is not
    // running and a loop with nothing to do are byte-identical from outside.
    // `spawn_loop` makes the spawn AND the visibility one call, so the two
    // cannot come apart; `runtime_jobs::registry` explains why the list is
    // derived here rather than declared somewhere, and tests/system_jobs.rs
    // fails if a bare `tokio::spawn` of a background loop reappears in this
    // file. The whole set is readable at `GET /api/system-jobs` and in the
    // dashboard's Scheduler tab under "System".
    use runtime_jobs::registry as jobs;
    let secs = std::time::Duration::from_secs;

    // Steering DELIVERY (AMUX-2617). `steer_enqueue` had three call sites and no
    // consumer: queued messages were stored durably and never handed to the lane,
    // so a busy worker's queue only grew (amux-rust: IDLE, 9 QUEUED, oldest 2h6m).
    // Spawned before the router takes `state` by value.
    jobs::spawn_loop(
        jobs::ids::STEER_DELIVER,
        Some(secs(api::session_verbs::STEER_TICK_SECS)),
        api::session_verbs::steer_deliver_loop(state.clone()),
    );

    // pipe-pane reconciler (AMUX-2671). `pipe-pane` is attached in
    // start_session and nowhere else, so a pane that loses its writer stays
    // unlogged forever — indistinguishable from a lane that was never started.
    jobs::spawn_loop(
        jobs::ids::PIPE_RECONCILE,
        Some(secs(api::session_verbs::PIPE_RECONCILE_SECS)),
        api::session_verbs::pipe_reconcile_loop(),
    );

    // Continuous invariant checking (AMUX-2622). Spawned before the router
    // takes `state` by value. Runs forever; a panic in one pass is caught
    // inside so the monitor cannot die quietly — a dead monitor's silence
    // reads as health, which is the failure this module exists to prevent.
    jobs::spawn_loop(
        jobs::ids::INVARIANTS,
        Some(secs(invariants::monitor::TICK_SECS)),
        invariants::monitor::run(state.clone()),
    );

    // Ghost-rescue (AMUX-2629): the FALLBACK sweep for the keystroke delivery
    // path — it presses Enter for an amux message that was typed into a lane's
    // input box and never submitted. Every rescue logs at WARN because a
    // rescue means the send path failed. It retires when interactive lanes are
    // protocol-driven; see runtime_jobs::ghost_rescue for the exit condition.
    // The handle is dropped on purpose — a PeriodicTask is NOT aborted on drop
    // (runtime_jobs' contract: an internal maintenance loop outlives the handle
    // that spawned it, and is stopped only by an explicit `abort`).
    drop(runtime_jobs::ghost_rescue::spawn(state.clone()));

    // Board -> worker drive loop (AMUX-2637): auto-pickup + the advance nudge.
    // Python owned this entire loop and the cutover left it behind, so no card
    // was assigned and no nudge was sent to any of the fleet's python-owned
    // lanes — and NOTHING errored, because the failure is pure absence. It
    // delivers through the steering queue, so every nudge lands at a turn
    // boundary and survives a restart; `/api/debug/board-drive` is the trace
    // whose absence is why the outage went unnoticed for hours.
    drop(runtime_jobs::board_drive::spawn(state.clone()));

    // Automatic accountability (AMUX-2990, Ethan: "the accountability shit needs
    // to be automatic"). Sweeps for lanes with human messages but no board card
    // and steers each to open one — server-side, so it reaches any group. One
    // nudge per lane per 24h. Held like board-drive so the loop is not dropped.
    drop(api::messages::accountability_spawn(state.clone()));

    // Pane-size restoration (AMUX-2634): a peek resizes the worker's tmux
    // window to the READER's viewport and tmux pins `window-size manual`, so
    // one phone glance used to narrow that worker's output permanently — the
    // fleet was found running at 50/94/102 columns instead of 220. Python had
    // this restorer (py:4443) and the cutover dropped it. Runs a one-shot
    // repair at boot, then holds the line on a 20s sweep against an
    // expiring viewer lease.
    drop(runtime_jobs::pane_size::spawn());
    // The idle uncommitted-work nudge (AMUX-2638). Ownership comes from the
    // staged-guard, never from the dirty tree — see the module docs for the
    // three sweeps that rule exists to prevent. It owns its own tokio::spawn
    // (it decides whether to run at all from AMUX_COMMIT_NUDGE_SECS), so it is
    // `adopt`ed rather than spawned here — same contract, same call site.
    {
        let h = runtime_jobs::commit_nudge::spawn(state.clone());
        jobs::adopt(jobs::ids::COMMIT_NUDGE, None, &h);
    }

    // AUTOFIX (AMUX-2681) — notice, file, hand off. Runs in the SERVER, on
    // purpose: the thing that watches for breakage must not share fate with
    // the thing that breaks, so nothing in it touches a pane, a send or a turn
    // boundary. It reads SQLite and writes a board card; `board_drive` above
    // then hands that card to a lane through the delivery path that already
    // exists. If every worker in the fleet is dead, the cards still get filed
    // and wait. Noticing is infrastructure; fixing is work.
    drop(runtime_jobs::autofix::spawn(state.clone()));
    // AMUX-3579: enrol /api/board/commit-mentions, which had zero consumers.
    drop(runtime_jobs::commit_mention_notes::spawn(state.clone()));

    // STORAGE RETENTION (AMUX-2700). Seven append-only tables and three cache
    // directories had no retention at all — not leaking, just working as
    // written, forever. Driven by time rather than traffic on purpose: the
    // prune logic media-cache and uploads already had was correct and only ran
    // while those directories were GROWING, so a fleet that stopped transcoding
    // never evicted a transcode.
    drop(runtime_jobs::heartbeat::spawn(store.clone()));
    drop(runtime_jobs::storage::spawn(state.clone()));
    drop(runtime_jobs::disk_watch::spawn(state.clone()));
    drop(runtime_jobs::tailnet_watch::spawn());
    // Compaction-generation watch (AMUX-3742): the reason "amux claude performs
    // worse than raw claude" was invisible for months is that nothing counted
    // how many times a lane's conversation had been summarized away.
    drop(runtime_jobs::context_health::spawn());
    // AMUX-3761: a durable record of WHICH RULE decided each lane's status,
    // so "was that badge accurate?" is answerable after the screenshot arrives.
    drop(runtime_jobs::status_history::spawn(state.clone()));
    // The token_ledger WRITER. Every reader of that table was ported at the
    // cutover and this was not, so /api/stats/daily served a confident
    // total_tokens: 0 for 36 hours (AMUX-2892).
    drop(runtime_jobs::token_ledger::spawn(state.clone()));

    // THE SCHEDULE FIRING LOOP (AMUX-2647). `run_scheduler` existed, was
    // documented, was gated behind `AMUX_RS_SCHEDULER=1` — and had ZERO call
    // sites, so setting the gate armed a loop nobody started. Nothing errored,
    // because the failure is pure absence: the last cron fire on this fleet was
    // 19:41, the moment the python server stopped, and six schedules were
    // silently overdue by the time anyone looked. A capability that never
    // reaches the runtime does not exist (ethos rule 1).
    //
    // Shadow mode still runs: it journals what it WOULD have fired, and that
    // journal is the only evidence that the loop is alive at all when firing is
    // off. Delivery goes through `LiveDeliverer` — the one implementation, and
    // the same send path a human's message takes.
    {
        let firing = runtime_jobs::firing_enabled();
        let deliverer: std::sync::Arc<dyn runtime_jobs::scheduler::Deliverer> =
            std::sync::Arc::new(runtime_jobs::scheduler::LiveDeliverer::new(state.clone()));
        jobs::spawn_loop(
            jobs::ids::SCHEDULER,
            Some(secs(runtime_jobs::SCHEDULER_TICK_SECS)),
            runtime_jobs::run_scheduler(store.clone(), firing, deliverer),
        );
    }

    let app = api::router(state);

    // SNI dual-cert: Tailscale LE cert for the tailnet hostname, self-signed
    // for localhost/IPs — Python-server parity (its _sni_cb), so the PWA's
    // service worker registers over https://<host>.ts.net:<port>.
    let server_config = tls::build_server_config(&cfg.tls_dir()).expect("tls material");
    let rustls_cfg =
        axum_server::tls_rustls::RustlsConfig::from_config(std::sync::Arc::new(server_config));

    // Terminal backends (RR-0031/0032): tmux always; herdr joins when
    // AMUX_HERDR_SESSION names the herdr session hosting amux workspaces
    // (backend::backends_from_env — constructing it against a missing herdr
    // server would make every probe a failure report).
    let backends = backend::backends_from_env(&cfg.env);
    // Publish the backend set for API handlers (worker peek, AMUX-2613
    // gap 4): AppState is constructed at 40+ sites, so the accessor is a
    // process-wide slot rather than a new required field on every one.
    backend::set_process_backends(backends.clone());
    // ONE protocol instance shared by the runtime pump, the scan demotion
    // check, and the bootstrap registrar — two instances would disagree
    // about which workers have live sessions (ethos rule 4).
    //
    // Conversation refs the protocol captures persist to
    // `_amux_conversations` (AMUX-2613 gap 2) so bootstrap re-hydrates them
    // after a restart — an in-memory-only ref is fiction (the D1 report-
    // table lesson: this process re-execs on every deploy).
    let protocol = Arc::new(opencode::structured::StructuredCliProtocol::with_conversation_sink(
        Arc::new(StoreConversationSink { store: store.clone() }),
    ));

    // Orchestrator runtime: reconcile once, then tick (RR-0041).
    let runtime = Arc::new(orchestrator::runtime::Runtime {
        store: store.clone(),
        backends: backends.clone(),
        tick_secs: cfg
            .env
            .get("AMUX_RS_TICK_SECS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(3),
        heartbeat_every: 10,
        breaker: amux_core::circuit::FleetCircuitBreaker {
            // Spend trip disabled until the token ledger wires in (Phase 4)
            // — 0 budget with 0 accounting would trip instantly on lies.
            window_budget_tokens: u64::MAX,
            window_secs: 3600,
            min_progress_per_window: 0, // no-progress trip opt-in via config later
            max_failures_per_window: 50,
        },
        fleet_state: std::sync::Mutex::new(amux_core::circuit::FleetState::Normal),
        protocol: Some(protocol.clone() as Arc<dyn opencode::AgentProtocol>),
        pickup_unowned: cfg.env.get("AMUX_RS_PICKUP_UNOWNED").map(|v| v == "1").unwrap_or(false),
        // RR-0044b: staggered un-park interval after a provider rate-limit
        // reset (thundering-herd prevention).
        resume_stagger_secs: cfg
            .env
            .get("AMUX_RS_RESUME_STAGGER_SECS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(amux_core::provider_fleet::DEFAULT_RESUME_STAGGER_SECS),
    });
    match runtime.reconcile_on_startup().await {
        Ok(report) => tracing::info!(
            interrupted = report.interrupted.len(),
            stale = report.stale_backend.len(),
            probe_failures = report.backend_probe_failures.len(),
            "startup reconciliation complete"
        ),
        Err(e) => tracing::warn!(error = %e, "startup reconciliation failed"),
    }
    crate::api::reclaim::reap_orphaned_scans(&store);
    let orch_tick_secs = runtime.tick_secs.max(1);
    jobs::spawn_loop(jobs::ids::ORCH_RUNTIME, Some(secs(orch_tick_secs)), runtime.clone().run());

    // Worker event processors (RR-0065, AMUX-2613 gap 1): the durable
    // subscriber to protocol.events(). Without this spawn the whole event
    // module was test-only plumbing — a headless turn ran, completed, and
    // the worker's DB row said `idle` throughout, with turn outcomes
    // visible only in a tracing line (a store the reader never opens is
    // the same failure as no tag, ethos rule 4). One processor per worker
    // with a live session; TurnStarted -> Active{turn}, TurnCompleted ->
    // Idle + command confirmation + the drift-note path, all journaled
    // with payloads (RR-0111a). The scan loop stays the FALLBACK voice:
    // it already demotes any worker whose protocol session is live.
    jobs::spawn_loop(
        jobs::ids::EVENT_PROCESSORS,
        Some(secs(orchestrator::events::SUPERVISE_SECS)),
        orchestrator::events::run_event_processors(
            store.clone(),
            runtime.protocol.clone().expect("protocol constructed above"),
        ),
    );

    // Terminal scan loop (RR-0067): the fallback voice for hookless
    // interactive workers, with structured-session demotion built in.
    let scan = Arc::new(orchestrator::scan::ScanLoop::new(
        store.clone(),
        runtime.backends.clone(),
        runtime.protocol.clone(),
    ));
    let scan_secs = cfg
        .env
        .get("AMUX_RS_SCAN_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);
    jobs::spawn_loop(jobs::ids::SCAN, Some(secs(scan_secs.max(5))), scan.run(scan_secs));

    // Session bootstrap (backend::bootstrap): the spawn/registration glue
    // that turns the API's durable Starting/ended records into real backend
    // processes and structured-protocol sessions. Without it a started
    // worker never gets a terminal and the pump sees NoSession forever —
    // see the module docs for the incident.
    let boot = Arc::new(backend::bootstrap::Bootstrap {
        store: store.clone(),
        backends,
        registry: Arc::new(provider::default_registry()),
        registrar: protocol,
    });
    let boot_secs = cfg
        .env
        .get("AMUX_RS_BOOTSTRAP_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    jobs::spawn_loop(jobs::ids::BOOTSTRAP, Some(secs(boot_secs.max(1))), boot.run(boot_secs));

    // Self-adoption (parity with the Python server's own-mtime watch): when
    // the INSTALLED binary changes underneath us — the builder agent just
    // installed a new build — replace this process with the new code. The
    // binary is the unit of deploy; a server that keeps running stale code
    // after a deploy is the Python shared-checkout staleness incident wearing
    // a compiled coat.
    //
    // exec(), NOT exit (AMUX-3458, gtm-ticker's capacity signal): exiting for
    // launchd's KeepAlive relaunch paid launchd's ThrottleInterval on every
    // adoption — measured at a flat 10.09s exit-to-start across six restarts
    // in one evening, even after 25 minutes of clean uptime — and with the
    // high commit cadence of a busy day that was a connection-refused window
    // per commit, which cost real consumer reads (/api/email/inbox mid-scan,
    // twice). exec replaces the process image in place: same PID, launchd
    // uninvolved, no throttle, and the refusal window shrinks to the new
    // binary's boot (~1s). Crash respawns still go through launchd and KEEP
    // the 10s damping, which is correct — a crash loop should damp; a
    // deliberate deploy should not. exit(0) stays as the fallback if exec
    // itself fails (missing/unreadable binary), because the old behavior is
    // strictly better than a server running stale code.
    // AEAB-52: self-adoption is right in PRODUCTION and wrong in a TEST HARNESS.
    //
    // `e2e/serve-head.sh` exists to pin a SPECIFIC build, and playwright.config.ts
    // starts three servers from it (desktop 18823, mobile 18833, ios 18843). Each
    // one builds, and every build rewrites the shared binary — so each server
    // already running sees its own mtime move and exec's, refusing connections
    // for ~1s. Whichever specs are mid-`page.goto` at that instant fail with
    // ERR_CONNECTION_REFUSED, which is what three days of "flaky" desktop
    // failures were: 3 failures, then 9 on a re-run of the SAME commit, then 2,
    // then 2.
    //
    // The signature is why this is structural rather than random. Execs per port
    // in run 32645871348, against server start order:
    //     18823 desktop, started 1st -> 2 execs
    //     18833 mobile,  started 2nd -> 1 exec
    //     18843 ios,     started 3rd -> 0 execs
    // Each server exec's once per server that starts AFTER it; the last never
    // does, because nothing rebuilds after it. Only WHICH tests get caught is
    // random.
    //
    // A server that hot-swaps itself mid-suite is also running a different binary
    // from the one the suite chose, so this is a correctness problem for the
    // RESULTS and not only noise.
    //
    // Opt-OUT, not opt-in: production keeps today's behaviour with no
    // configuration, and only the harness that knows it pinned a build says so.
    if !self_adopt_enabled() {
        // NOT an early return: everything after this block is the rest of server
        // startup (the port binds, the router). Skipping it would trade a flaky
        // suite for a server that never listens.
        tracing::info!(
            "self-adoption DISABLED by AMUX_NO_SELF_ADOPT — this process will not exec on a \
             binary change (AEAB-52: a test harness pins its build on purpose)"
        );
    } else {
        jobs::spawn_loop(jobs::ids::SELF_ADOPT, Some(secs(5)), async {
            let Ok(exe) = std::env::current_exe() else { return };
            let Ok(meta) = std::fs::metadata(&exe) else { return };
            let initial = meta.modified().ok();
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                jobs::tick(jobs::ids::SELF_ADOPT);
                let current = std::fs::metadata(&exe).ok().and_then(|m| m.modified().ok());
                if current.is_some() && current != initial {
                    tracing::info!(
                        "binary changed on disk — exec'ing the new build in place (self-adoption, \
                         AMUX-3458: no exit means no launchd throttle window)"
                    );
                    // TELL THE SUCCESSOR THIS WAS PLANNED (AF-176). exec()
                    // replaces the image, so the new server cannot otherwise
                    // know whether it arrived by self-adoption or because the
                    // previous process died — and those look identical in the
                    // log: 28 starts on 2026-08-23, 27 of them routine.
                    let err = std::os::unix::process::CommandExt::exec(
                        std::process::Command::new(&exe).env("AMUX_SELF_ADOPTED", "1"),
                    );
                    // Only reachable when exec FAILED. Fall back to the old
                    // exit-for-relaunch path — slower (throttle) but correct.
                    tracing::warn!(%err, "exec failed — falling back to exit-for-relaunch");
                    std::process::exit(0);
                }
            }
        });
    }

    // THE LEGACY 8822 BIND IS GONE (Ethan, 2026-08-11: "no more 8822 just rust").
    //
    // It existed because the python retirement stranded every running lane:
    // AMUX_URL was baked into live process envs that cannot be rotated, so the
    // rust server answered the old port too — same router, same TLS, same auth.
    // It was always a countdown rather than a feature, and this is the owner
    // calling it, ahead of the automatic 7-day-quiet exit.
    //
    // WHAT REPLACED IT, so nobody re-adds the bind to fix the symptom: sessions
    // are launched with AMUX_URL derived from `config::canonical_port()`
    // (session_verbs.rs), the CLI defaults to the canonical port, and
    // `publish_endpoint` below writes the real address for the one client class
    // that can be told no other way — a hook spawned by a pre-cutover process,
    // which inherits the stale variable and whose DEFAULT therefore never
    // fires. Any lane still holding the old address gets connection-refused,
    // which is loud and fixed by restarting that lane.
    //
    // tests/legacy_port_guard.rs fails the build if the retired address
    // reappears in code, scripts or e2e.
    legacy_port::publish_endpoint(&cfg.amux_home, cfg.port, None);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], cfg.port));
    // HOW LONG AFTER `record_boot` DID WE ACTUALLY START SERVING (AMUX-3647).
    //
    // The card proposed a `ready_at` COLUMN on `_amux_request_log` so the
    // restart filter could ask "did this request arrive before the server was
    // serving". It cannot: nothing accepts a connection before this bind, so
    // `ts < ready_at` is false for every row, and the column would have bought
    // a schema change and no rows. The load-bearing assumption is that this gap
    // is small, and asserting that in a comment is exactly the shape this repo
    // keeps getting wrong, so it is MEASURED on every boot instead. If it is
    // ever seconds rather than milliseconds, the number is already in the log
    // and the column argument can be reopened with evidence.
    //
    // Note what it does NOT include: migrations. Those run inside
    // `Store::open`, before `record_boot`, with the port closed. A 186-second
    // migration (0031, 2026-08-24) shows up as downtime and connection-refused,
    // never as a slow request.
    if let Some(b) = runtime_jobs::heartbeat::boot_at() {
        tracing::info!(
            ready_after_boot_ms = ((runtime_jobs::registry::unix_now() - b) * 1000.0).round(),
            "ready: binding the listener; no request can be stamped before this instant"
        );
    }
    tracing::info!(%addr, "listening (https, plain-http redirected)");
    let acceptor = tls::RedirectingAcceptor::new(
        axum_server::tls_rustls::RustlsAcceptor::new(rustls_cfg),
        format!("localhost:{}", cfg.port),
    );
    axum_server::bind(addr)
        .acceptor(acceptor)
        // with_connect_info: the auth middleware's localhost bypass (Python
        // parity) needs the PEER address; without this every request looks
        // remote and local tokenless CLI calls 401.
        .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .await
        .expect("server run");
}
