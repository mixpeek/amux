//! TmuxBackend (RR-0032): `SessionBackend` over the tmux CLI (fallback backend).
//!
//! One amux worker maps to one tmux session named `amux-wrk_<ulid>` (derived
//! ONLY via `super::backend_ref` — Invariant 43). Verbs:
//!
//! - spawn      -> `new-session -d -s <ref> -x 220 -y 50 -c <cwd> [-e K=V..]`,
//!   `set-option remain-on-exit on`, then `send-keys -l` + Enter
//! - terminate  -> `kill-session -t '=<ref>'`
//! - status     -> `has-session` then `list-panes #{pane_dead}...`
//! - reconcile  -> ONE `list-panes -a` census, `amux-` prefix only (MO-3622)
//! - capture    -> `capture-pane -p -S -<lines>`
//!
//! Every tmux target string is built by exactly two helpers below —
//! `session_target` / `pane_target` — never inline (L2).

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;

use super::{
    backend_ref, AttachInfo, BackendError, BackendSession, BackendStatus, ProcessRef, Result,
    SessionBackend, SessionSpec,
};

/// Per-tmux-call timeout: tmux answers in milliseconds; anything slower means
/// the server is wedged and we want the error, not the hang.
const OP_TIMEOUT: Duration = Duration::from_secs(5);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Spawn accounting (MO-3622).
//
// Every `TmuxBackend` verb is a PROCESS SPAWN, and on macOS each spawn also
// wakes trustd, syspolicyd and XProtect. `reconcile` used to cost 1 + 2N spawns
// per pass and the bootstrap loop runs it every 2s: 26 spawns/s at N=28, half
// of everything amux-server-rs forks, while the 15-minute load read 167% of the
// machine's cores. Nothing in the logs could say so, because a spawn leaves no
// trace once it exits. This is that trace: a rolling window, a per-verb tally,
// and a WARN when the rate stays high.
//
// SCOPE, stated so the number is not read as more than it is: it counts what
// `TmuxBackend::run` starts. Peek captures and the session-verb helpers spawn
// tmux from their own call sites and are NOT in this count.
// ---------------------------------------------------------------------------

/// How long a rate is averaged over before it is reported and reset.
const SPAWN_WINDOW: Duration = Duration::from_secs(60);
/// WARN above this many `TmuxBackend` spawns per second, averaged over a
/// window. Steady state after MO-3622 is well under 1/s.
/// `AMUX_TMUX_SPAWN_WARN_PER_S` overrides it.
const DEFAULT_SPAWN_WARN_PER_S: f64 = 15.0;

/// One completed accounting window.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct SpawnRate {
    pub spawns: u64,
    pub window_s: f64,
    pub per_s: f64,
    /// Highest-volume verbs first, at most five.
    pub top_verbs: Vec<(String, u64)>,
}

struct SpawnWindow {
    started: std::time::Instant,
    count: u64,
    by_verb: std::collections::BTreeMap<String, u64>,
}

impl SpawnWindow {
    fn new(now: std::time::Instant) -> Self {
        Self {
            started: now,
            count: 0,
            by_verb: Default::default(),
        }
    }

    /// Close the window when it is old enough, returning its rate. The caller
    /// decides what to do with the rate; this only measures.
    fn roll(&mut self, now: std::time::Instant) -> Option<SpawnRate> {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed < SPAWN_WINDOW {
            return None;
        }
        let mut top: Vec<(String, u64)> = std::mem::take(&mut self.by_verb).into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        top.truncate(5);
        let rate = SpawnRate {
            spawns: self.count,
            window_s: elapsed.as_secs_f64(),
            per_s: self.count as f64 / elapsed.as_secs_f64(),
            top_verbs: top,
        };
        self.started = now;
        self.count = 0;
        Some(rate)
    }

    fn note(&mut self, verb: &str, now: std::time::Instant) -> Option<SpawnRate> {
        let done = self.roll(now);
        self.count += 1;
        *self.by_verb.entry(verb.to_string()).or_default() += 1;
        done
    }
}

struct SpawnLedger {
    total: u64,
    window: Option<SpawnWindow>,
    last: Option<SpawnRate>,
}

static SPAWN_LEDGER: std::sync::Mutex<SpawnLedger> = std::sync::Mutex::new(SpawnLedger {
    total: 0,
    window: None,
    last: None,
});

fn spawn_warn_threshold() -> f64 {
    std::env::var("AMUX_TMUX_SPAWN_WARN_PER_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_SPAWN_WARN_PER_S)
}

/// The verb of a tmux argv: the first word that is not a flag (`-N`, `-L x` is
/// not used here). Bounded so a pathological argv cannot grow the tally.
fn tmux_verb(args: &[&str]) -> String {
    args.iter()
        .find(|a| !a.starts_with('-'))
        .map(|a| a.chars().take(32).collect())
        .unwrap_or_else(|| "?".into())
}

fn note_tmux_spawn(args: &[&str]) {
    let now = std::time::Instant::now();
    let verb = tmux_verb(args);
    let Ok(mut ledger) = SPAWN_LEDGER.lock() else {
        return;
    };
    ledger.total += 1;
    let window = ledger.window.get_or_insert_with(|| SpawnWindow::new(now));
    if let Some(rate) = window.note(&verb, now) {
        if rate.per_s > spawn_warn_threshold() {
            tracing::warn!(
                target: "amux::tmux",
                verdict = "tmux_spawn_rate_high",
                measured = true,
                n_considered = rate.spawns,
                per_s = rate.per_s,
                window_s = rate.window_s,
                top_verbs = ?rate.top_verbs,
                "TmuxBackend is starting tmux processes faster than a steady fleet needs; \
                 each one also wakes trustd/syspolicyd/XProtect. Read top_verbs for the loop."
            );
        }
        ledger.last = Some(rate);
    }
}

/// `TmuxBackend` spawn accounting for `GET /api/debug/tmux`.
///
/// `last_window` is `null` until a full window has elapsed since the process
/// started (a restart resets it), and `scope` says what is and is not counted,
/// so a low number is never read as a statement about the whole server.
pub fn tmux_spawn_stats() -> serde_json::Value {
    let ledger = SPAWN_LEDGER.lock().ok();
    serde_json::json!({
        "scope": "TmuxBackend::run only; peek captures and session-verb helpers spawn tmux \
                  from their own call sites and are not counted here",
        "total_since_start": ledger.as_ref().map(|l| l.total),
        "last_window": ledger.as_ref().and_then(|l| l.last.clone()),
        "warn_per_s": spawn_warn_threshold(),
    })
}

// ---------------------------------------------------------------------------
// L2 — the two tmux target forms, encoded in ONE place each.
//
// tmux has two addressing grains and they are NOT interchangeable:
//
// * SESSION-level commands (`has-session`, `kill-session`, `attach-session`)
//   take `=<name>` — the `=` demands an exact match instead of tmux's default
//   prefix matching (without it, killing `amux-wrk_A` could match
//   `amux-wrk_AB`).
//
// * PANE/WINDOW-level commands (`send-keys`, `capture-pane`, `list-panes`,
//   `set-option -w`) take `=<name>:` — the trailing colon means "the active
//   window of exactly this session". Session-level commands tolerate the
//   bare form, pane-level commands SILENTLY FAIL with it.
//
// This exact asymmetry took down the Python fleet on 2026-08-08: `=<name>`
// was shipped to pane-level commands, the session-level verbs kept working,
// and every capture/send across 62 sessions failed silently (rust-rebuild
// plan, lesson L2). Both forms live only here so the split can never be
// re-introduced call-site by call-site.
// ---------------------------------------------------------------------------

/// Target for SESSION-level tmux commands: `=<ref>` (exact match, no colon).
/// pub(crate): api/session_verbs.rs builds fleet targets through these two
/// helpers so the L2 format lives in exactly one place.
pub(crate) fn session_target(backend_ref: &str) -> String {
    format!("={backend_ref}")
}

/// Target for PANE/WINDOW-level tmux commands: `=<ref>:` (exact match, active
/// window — the trailing colon is load-bearing, see L2 block above).
pub(crate) fn pane_target(backend_ref: &str) -> String {
    format!("={backend_ref}:")
}

pub struct TmuxBackend {
    bin: String,
}

/// May this `KEY=VALUE` pair travel as a process argument? (AMUX-4803)
///
/// Empty values are always safe: `ANTHROPIC_API_KEY=` carries no secret and is
/// exactly how an OAuth worker SUPPRESSES an inherited key, so a blanket ban on
/// the name would break that.
///
/// Name-shaped rather than value-shaped on purpose. Guessing "does this look
/// like a credential" from the VALUE is how a guard both misses a short key and
/// blocks an innocent path; the names are a closed, boring set that the people
/// adding new ones already follow.
pub(crate) fn env_pair_is_argv_safe(key: &str, value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let k = key.to_ascii_uppercase();
    !["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "PASSWD"]
        .iter()
        .any(|needle| k.contains(needle))
}

impl TmuxBackend {
    pub fn new() -> Self {
        Self { bin: "tmux".into() }
    }

    async fn run(&self, args: &[&str], timeout: Duration) -> Result<std::process::Output> {
        note_tmux_spawn(args);
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(out) => Ok(out?),
            Err(_) => Err(BackendError::CommandFailed(format!(
                "tmux {} timed out after {:?}",
                args.join(" "),
                timeout
            ))),
        }
    }

    /// Run and require exit 0; non-zero maps to CommandFailed with stderr.
    async fn run_ok(&self, args: &[&str], timeout: Duration) -> Result<std::process::Output> {
        let out = self.run(args, timeout).await?;
        if !out.status.success() {
            return Err(BackendError::CommandFailed(format!(
                "tmux {} failed (exit {:?}): {}",
                args.join(" "),
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(out)
    }

    async fn status_by_ref(&self, backend_ref: &str) -> Result<BackendStatus> {
        // Session-level existence check: exit 0 = exists. Any non-zero exit
        // (unknown session, or no tmux server at all) means no such process
        // host — NotFound, not an error.
        let st = session_target(backend_ref);
        let has = self.run(&["has-session", "-t", &st], OP_TIMEOUT).await?;
        if !has.status.success() {
            return Ok(BackendStatus::NotFound);
        }
        // Pane-level: with remain-on-exit (set at spawn) a finished command
        // leaves a dead pane behind, so the exit status is observable instead
        // of the whole session vanishing.
        let pt = pane_target(backend_ref);
        let out = self
            .run(
                &[
                    "list-panes",
                    "-t",
                    &pt,
                    "-F",
                    // ':' not '\t': in a LANG-less launchd env tmux sanitizes
                    // non-printable chars to '_' (2026-08-09 fleet incident),
                    // and none of these three fields can contain ':'.
                    "#{pane_dead}:#{pane_dead_status}:#{pane_dead_signal}",
                ],
                OP_TIMEOUT,
            )
            .await?;
        if !out.status.success() {
            // Session disappeared between the two calls (raced a kill).
            return Ok(BackendStatus::NotFound);
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let status = parse_pane_dead(stdout.lines().next().unwrap_or(""));
        if status != (BackendStatus::Crashed { signal: None }) {
            return Ok(status);
        }
        // AMUX-4636: tmux marked the pane dead but recorded neither status nor
        // signal. Measured on tmux 3.4 (ubuntu CI): the pane's process sat as a
        // zombie under the tmux server, never reaped, so tmux had nothing to
        // report. The kernel still holds that zombie's exit status.
        let pids = self
            .run(
                &["list-panes", "-t", &pt, "-F", "#{pane_pid}:#{pid}"],
                OP_TIMEOUT,
            )
            .await?;
        let line = String::from_utf8_lossy(&pids.stdout);
        if let Some((pane_pid, server_pid)) = parse_pid_pair(line.lines().next().unwrap_or("")) {
            if let Some(measured) =
                zombie_exit_status(std::path::Path::new("/proc"), &pane_pid, &server_pid)
            {
                tracing::info!(
                    target: "amux::tmux",
                    verdict = "tmux_unreaped_exit_measured_from_proc",
                    backend_ref,
                    pane_pid = %pane_pid,
                    status = ?measured,
                    "tmux reported a dead pane with no exit status; read it from the zombie in /proc"
                );
                return Ok(measured);
            }
        }
        Ok(status)
    }
}

impl Default for TmuxBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionBackend for TmuxBackend {
    async fn process_exits(
        &self,
    ) -> Result<std::collections::BTreeMap<String, amux_core::protocol::ExitStatus>> {
        let out = self
            .run(
                &[
                    "list-panes",
                    "-a",
                    "-F",
                    "#{session_name}:#{pane_dead}:#{pane_dead_status}:#{pane_dead_signal}",
                ],
                OP_TIMEOUT,
            )
            .await?;
        if !out.status.success() {
            let error = String::from_utf8_lossy(&out.stderr);
            if error.contains("no server") || error.contains("No such file") {
                return Ok(std::collections::BTreeMap::new());
            }
            return Err(BackendError::CommandFailed(format!(
                "tmux process-exit census: {error}"
            )));
        }
        let mut exits = parse_process_exits(&String::from_utf8_lossy(&out.stdout))?;
        if !exits
            .values()
            .any(|e| e.code.is_none() && e.signal.is_none())
        {
            return Ok(exits);
        }
        // AMUX-4636: see status_by_ref. Only a session with exactly one dead
        // pane is filled in; several dead panes still prove cessation without
        // one shared exit code, exactly as parse_process_exits decides.
        let census = self
            .run(
                &[
                    "list-panes",
                    "-a",
                    "-F",
                    "#{session_name}:#{pane_dead}:#{pane_pid}:#{pid}",
                ],
                OP_TIMEOUT,
            )
            .await?;
        if !census.status.success() {
            return Ok(exits);
        }
        let pids = dead_pane_pids(&String::from_utf8_lossy(&census.stdout));
        for (name, exit) in exits.iter_mut() {
            if exit.code.is_some() || exit.signal.is_some() {
                continue;
            }
            let Some([(pane_pid, server_pid)]) = pids.get(name).map(Vec::as_slice) else {
                continue;
            };
            let measured = zombie_exit_status(std::path::Path::new("/proc"), pane_pid, server_pid);
            let filled = match measured {
                Some(BackendStatus::Completed { exit_code }) => amux_core::protocol::ExitStatus {
                    code: Some(exit_code),
                    signal: None,
                },
                Some(BackendStatus::Crashed { signal: Some(sig) }) => {
                    amux_core::protocol::ExitStatus {
                        code: None,
                        signal: Some(sig),
                    }
                }
                _ => continue,
            };
            tracing::info!(
                target: "amux::tmux",
                verdict = "tmux_unreaped_exit_measured_from_proc",
                session = %name,
                pane_pid = %pane_pid,
                code = ?filled.code,
                signal = ?filled.signal,
                "tmux census had a dead pane with no exit status; read it from the zombie in /proc"
            );
            *exit = filled;
        }
        Ok(exits)
    }

    fn name(&self) -> &'static str {
        "tmux"
    }

    async fn spawn(&self, spec: &SessionSpec) -> Result<ProcessRef> {
        if spec.command.is_empty() {
            return Err(BackendError::SpawnFailed("empty command".into()));
        }
        let ref_ = backend_ref(&spec.worker);
        let st = session_target(&ref_);
        let pt = pane_target(&ref_);

        // Refuse to double-spawn: `new-session -s` on an existing name errors
        // anyway, but checking first gives a spawn error instead of a generic
        // command failure.
        let has = self.run(&["has-session", "-t", &st], OP_TIMEOUT).await?;
        if has.status.success() {
            return Err(BackendError::SpawnFailed(format!(
                "tmux session {ref_} already exists"
            )));
        }

        // -x/-y give the detached pane a real geometry so full-screen TUIs
        // (claude, etc.) render sanely before any client attaches.
        // -e per env var (tmux >= 3.2; this repo targets tmux 3.x).
        let create_server = super::tmux_health::may_create_server()
            .await
            .map_err(BackendError::SpawnFailed)?;
        let mut args: Vec<String> = vec![
            "new-session".into(),
            "-d".into(),
            "-s".into(),
            ref_.clone(),
            "-x".into(),
            "220".into(),
            "-y".into(),
            "50".into(),
            "-c".into(),
            spec.cwd.clone(),
        ];
        if !create_server {
            args.insert(0, "-N".into());
        }
        for (k, v) in &spec.env {
            // NEVER A SECRET VALUE IN ARGV (AMUX-4803). Process arguments are
            // world-readable on macOS and a tmux SERVER keeps the argv of the
            // new-session that created it for its whole lifetime, so a key put
            // here is readable by every process on the box for days.
            //
            // No caller currently routes a credential through `spec.env`, which
            // is why this is a guard rather than a repair: the live leak was the
            // worker-spawn path in api/session_verbs.rs, and it now defers
            // secrets to `tmux set-environment` after the session exists. This
            // stops the same mistake arriving here later, and it is LOUD rather
            // than silent because a dropped variable that nobody notices is its
            // own outage.
            if !env_pair_is_argv_safe(k, v) {
                tracing::error!(
                    target: "amux::backend",
                    verdict = "secret_refused_in_argv", key = %k, session = %ref_,
                    measured = true, n_considered = 1,
                    "refusing to put a secret-shaped value in tmux argv; pass it with \
                     `tmux set-environment` after the session exists and import it in the \
                     pane, the way api/session_verbs.rs does"
                );
                continue;
            }
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run_ok(&args_ref, OP_TIMEOUT)
            .await
            .map_err(|e| BackendError::SpawnFailed(e.to_string()))?;

        // Everything after new-session cleans up the half-spawned session on
        // failure — a name-squatting session with no command would block every
        // future spawn of this worker.
        let result: Result<Option<u32>> = async {
            // remain-on-exit BEFORE the command is sent: without it tmux
            // destroys the pane (and the single-window session) the instant
            // the process exits, which would collapse Completed/Crashed into
            // NotFound and lose the exit status. Set first so even an
            // instantly-exiting command leaves its corpse. Window-level
            // option => pane-level target form (L2).
            self.run_ok(
                &["set-option", "-w", "-t", &pt, "remain-on-exit", "on"],
                OP_TIMEOUT,
            )
            .await?;

            // `-l` = literal: without it send-keys translates tokens like
            // "Enter"/"Space"/"C-c" inside the command into KEYS — the exact
            // send-keys mangling the Python fleet fought for 250 lines. The
            // Enter keypress is a separate send-keys WITHOUT -l so it does
            // resolve as a key.
            //
            // The line `cd .. && exec ..` because the pane runs the user's
            // login shell: `cd` explicitly since shell profiles can cd away
            // from `-c` (observed on this machine), and `exec` replaces the
            // shell so pane_dead reports the COMMAND's exit status (not the
            // wrapper shell's) and #{pane_pid} IS the command's pid.
            //
            // Known hazard (mirrors HERDR-GAP-DIRECT-EXEC in herdr.rs): the
            // line is typed into a pty; a shell profile that reads stdin
            // during init can eat it. tmux exposes no cheap "shell is at a
            // prompt" probe (herdr's process-info equivalent would be a ps(1)
            // walk), so this backend accepts the same exposure the Python
            // fleet ran with; the pty buffers keystrokes until the shell
            // reads them.
            let line = format!(
                "cd {} && exec {}",
                sh_quote(&spec.cwd),
                spec.command
                    .iter()
                    .map(|a| sh_quote(a))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            self.run_ok(&["send-keys", "-t", &pt, "-l", &line], OP_TIMEOUT)
                .await?;
            self.run_ok(&["send-keys", "-t", &pt, "Enter"], OP_TIMEOUT)
                .await?;

            // Shell pid now; the command inherits it via exec.
            let out = self
                .run_ok(&["list-panes", "-t", &pt, "-F", "#{pane_pid}"], OP_TIMEOUT)
                .await?;
            Ok(String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .and_then(|l| l.trim().parse::<u32>().ok()))
        }
        .await;

        match result {
            Ok(pid) => Ok(ProcessRef {
                backend_ref: ref_,
                pid,
            }),
            Err(e) => {
                let _ = self.run(&["kill-session", "-t", &st], OP_TIMEOUT).await;
                Err(BackendError::SpawnFailed(e.to_string()))
            }
        }
    }

    async fn terminate(&self, proc: &ProcessRef) -> Result<()> {
        // Scope guard: this backend only owns refs it minted (`amux-…` via
        // backend_ref). kill-session against anything else is refused
        // outright — this machine hosts a live fleet of tmux sessions, and a
        // bug above this layer must not be able to aim a kill at one of them
        // (ethos rule 8).
        if !proc.backend_ref.starts_with("amux-") {
            return Err(BackendError::CommandFailed(format!(
                "refusing to terminate {:?}: outside the amux- namespace",
                proc.backend_ref
            )));
        }
        let st = session_target(&proc.backend_ref);
        let out = self.run(&["kill-session", "-t", &st], OP_TIMEOUT).await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // "can't find session" / "no server running" — nothing to kill.
            if stderr.contains("can't find") || stderr.contains("no server") {
                return Err(BackendError::NotFound(proc.backend_ref.clone()));
            }
            return Err(BackendError::CommandFailed(format!(
                "tmux kill-session failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    async fn status(&self, proc: &ProcessRef) -> Result<BackendStatus> {
        self.status_by_ref(&proc.backend_ref).await
    }

    async fn attach_info(&self, proc: &ProcessRef) -> Result<AttachInfo> {
        match self.status_by_ref(&proc.backend_ref).await? {
            BackendStatus::NotFound => Err(BackendError::NotFound(proc.backend_ref.clone())),
            _ => Ok(AttachInfo {
                // attach-session is session-level: no trailing colon (L2).
                // Quoted for the human's shell: `=` is safe but the ref is
                // user-visible copy/paste material.
                command: format!(
                    "tmux attach-session -t '{}'",
                    session_target(&proc.backend_ref)
                ),
            }),
        }
    }

    async fn reconcile(&self) -> Result<Vec<BackendSession>> {
        // READ-ONLY sweep: reconcile reports what exists under the amux-
        // prefix; acting on it (killing strays, adopting orphans) is the
        // orchestrator's decision, not the backend's.
        //
        // ONE spawn for the whole fleet (MO-3622). This used to run
        // `list-sessions` and then `has-session` + `list-panes` per session:
        // 1 + 2N processes per pass, on a loop that fires every 2s. At N=28
        // that was 26 spawns/s of pure probing, and the bootstrap sweep that
        // calls it reads only the ref, never the status. `list-panes -a` names
        // every pane of every session with its dead/status/signal, which is
        // exactly what `status_by_ref` asked for one session at a time.
        let out = self
            .run(
                &["list-panes", "-a", "-F", RECONCILE_CENSUS_FORMAT],
                OP_TIMEOUT,
            )
            .await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // No tmux server == hosting nothing. That is an empty answer, not
            // a failure (a fresh boot has no server until the first spawn).
            if stderr.contains("no server") || stderr.contains("No such file") {
                return Ok(Vec::new());
            }
            return Err(BackendError::CommandFailed(format!(
                "tmux list-panes -a failed: {}",
                stderr.trim()
            )));
        }
        let census = parse_reconcile_census(&String::from_utf8_lossy(&out.stdout))?;

        let mut hosted = Vec::with_capacity(census.len());
        let mut needs_probe = Vec::new();
        for (name, status) in census {
            match status {
                // A dead pane with neither status nor signal is the AMUX-4636
                // case: only `status_by_ref` knows how to read the exit code
                // from the unreaped zombie. A session with no active-window
                // pane in the census is not something to guess about either.
                // Both are rare, and both keep the exact old per-session path.
                Some(status) if status != (BackendStatus::Crashed { signal: None }) => {
                    hosted.push(BackendSession {
                        backend_ref: name,
                        status,
                    })
                }
                _ => needs_probe.push(name),
            }
        }
        if !needs_probe.is_empty() {
            tracing::debug!(
                target: "amux::tmux",
                verdict = "tmux_reconcile_per_session_probe",
                measured = true,
                n_considered = hosted.len() + needs_probe.len(),
                probed = needs_probe.len(),
                "reconcile census could not settle these sessions; probing each"
            );
        }
        // Bounded to 10 at a time so we do not fork-bomb the tmux server.
        use futures::stream::{self, StreamExt};
        let probed: Vec<Result<BackendSession>> = stream::iter(needs_probe)
            .map(|name| async move {
                let status = self.status_by_ref(&name).await?;
                Ok(BackendSession {
                    backend_ref: name,
                    status,
                })
            })
            .buffer_unordered(10)
            .collect()
            .await;
        for session in probed {
            hosted.push(session?);
        }
        Ok(hosted)
    }

    async fn capture(&self, proc: &ProcessRef, lines: u32) -> Result<String> {
        let pt = pane_target(&proc.backend_ref);
        // -p print to stdout; -S -<n> start <n> lines above the visible top
        // (scrollback). Pane-level command => pane target form (L2).
        let start = format!("-{lines}");
        let out = self
            .run(
                &["capture-pane", "-t", &pt, "-p", "-S", &start],
                CAPTURE_TIMEOUT,
            )
            .await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("can't find") || stderr.contains("no server") {
                return Err(BackendError::NotFound(proc.backend_ref.clone()));
            }
            return Err(BackendError::CommandFailed(format!(
                "tmux capture-pane failed: {}",
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Parse one `#{pane_dead}:#{pane_dead_status}:#{pane_dead_signal}` line.
///
/// dead=0 -> Running. dead=1: a recorded signal wins (Crashed), else a
/// recorded exit status (Completed), else the process ended without either —
/// reported as Crashed(None) because claiming an exit code nobody measured
/// would be theatre (ethos rule 7).
fn parse_pane_dead(line: &str) -> BackendStatus {
    let mut parts = line.split(':');
    let dead = parts.next().unwrap_or("").trim();
    let status = parts.next().unwrap_or("").trim();
    let signal = parts.next().unwrap_or("").trim();
    if dead != "1" {
        return BackendStatus::Running;
    }
    if let Ok(sig) = signal.parse::<i32>() {
        return BackendStatus::Crashed { signal: Some(sig) };
    }
    if let Ok(code) = status.parse::<i32>() {
        return BackendStatus::Completed { exit_code: code };
    }
    BackendStatus::Crashed { signal: None }
}

/// `list-panes -a` format for `reconcile`. `:`-separated for the reason
/// `status_by_ref` gives (a LANG-less launchd env turns tabs into `_`); tmux
/// never lets a session name contain `:`, so the name is whatever is left when
/// the four trailing fields are split off the right.
const RECONCILE_CENSUS_FORMAT: &str =
    "#{session_name}:#{window_active}:#{pane_dead}:#{pane_dead_status}:#{pane_dead_signal}";

/// Parse a `RECONCILE_CENSUS_FORMAT` census into one entry per `amux-` session,
/// in first-seen order.
///
/// The status is that of the FIRST pane of the session's ACTIVE window, which
/// is the line `list-panes -t =<name>:` printed first and the one
/// `parse_pane_dead(stdout.lines().next())` read in `status_by_ref`. `None`
/// means the census showed no active-window pane for the session.
///
/// Foreign (non-`amux-`) sessions are skipped before they are parsed, as
/// `reconcile` always did. A malformed `amux-` line is an error rather than a
/// missing session: a session that silently vanished from the census would read
/// as "gone" to the sweep that reaps and interrupts.
fn parse_reconcile_census(output: &str) -> Result<Vec<(String, Option<BackendStatus>)>> {
    let mut order: Vec<String> = Vec::new();
    let mut first_active: std::collections::BTreeMap<String, Option<BackendStatus>> =
        Default::default();
    for line in output.lines() {
        if !line.starts_with("amux-") {
            continue;
        }
        // Right to left: signal, status, dead, window_active, then the name.
        let fields: Vec<_> = line.rsplitn(5, ':').collect();
        if fields.len() != 5
            || fields[4].is_empty()
            || !matches!(fields[3], "0" | "1")
            || !matches!(fields[2], "0" | "1")
        {
            return Err(BackendError::CommandFailed(format!(
                "malformed tmux reconcile census line: {line:?}"
            )));
        }
        let name = fields[4];
        if !first_active.contains_key(name) {
            order.push(name.to_string());
            first_active.insert(name.to_string(), None);
        }
        if fields[3] == "1" {
            let slot = first_active.get_mut(name).expect("inserted above");
            if slot.is_none() {
                *slot = Some(parse_pane_dead(&format!(
                    "{}:{}:{}",
                    fields[2], fields[1], fields[0]
                )));
            }
        }
    }
    Ok(order
        .into_iter()
        .map(|name| {
            let status = first_active.remove(&name).flatten();
            (name, status)
        })
        .collect())
}

fn parse_process_exits(
    output: &str,
) -> Result<std::collections::BTreeMap<String, amux_core::protocol::ExitStatus>> {
    use amux_core::protocol::ExitStatus;
    use std::collections::BTreeMap;
    let mut panes: BTreeMap<String, Vec<BackendStatus>> = BTreeMap::new();
    for line in output.lines() {
        let fields: Vec<_> = line.rsplitn(4, ':').collect();
        if fields.len() != 4 || fields[3].is_empty() || !matches!(fields[2], "0" | "1") {
            return Err(BackendError::CommandFailed(
                "malformed tmux process-exit census".into(),
            ));
        }
        if fields[3].starts_with("amux-") {
            let status = parse_pane_dead(&format!("{}:{}:{}", fields[2], fields[1], fields[0]));
            panes.entry(fields[3].into()).or_default().push(status);
        }
    }
    let mut exits = BTreeMap::new();
    for (name, statuses) in panes {
        if statuses.iter().any(|s| matches!(s, BackendStatus::Running)) {
            continue; // A completed side pane cannot stop a live lane.
        }
        // A single pane (or agreeing panes) supplies an exact exit status.
        // Different dead panes prove cessation but not one shared exit code.
        let status = if statuses.iter().all(|s| s == &statuses[0]) {
            match statuses[0] {
                BackendStatus::Completed { exit_code } => ExitStatus {
                    code: Some(exit_code),
                    signal: None,
                },
                BackendStatus::Crashed { signal } => ExitStatus { code: None, signal },
                _ => unreachable!("only confirmed dead panes remain"),
            }
        } else {
            ExitStatus {
                code: None,
                signal: None,
            }
        };
        exits.insert(name, status);
    }
    Ok(exits)
}

/// `#{pane_pid}:#{pid}` -> (pane pid, tmux server pid), both numeric.
fn parse_pid_pair(line: &str) -> Option<(String, String)> {
    let (pane, server) = line.trim().split_once(':')?;
    (pane.parse::<u32>().is_ok() && server.parse::<u32>().is_ok())
        .then(|| (pane.to_string(), server.to_string()))
}

/// Dead panes per session from a `#{session_name}:#{pane_dead}:#{pane_pid}:#{pid}`
/// census. Live panes and malformed lines are left out.
fn dead_pane_pids(census: &str) -> std::collections::BTreeMap<String, Vec<(String, String)>> {
    let mut out: std::collections::BTreeMap<String, Vec<(String, String)>> = Default::default();
    for line in census.lines() {
        let fields: Vec<_> = line.rsplitn(4, ':').collect();
        if fields.len() != 4 || fields[3].is_empty() || fields[2] != "1" {
            continue;
        }
        if let Some(pair) = parse_pid_pair(&format!("{}:{}", fields[1], fields[0])) {
            out.entry(fields[3].to_string()).or_default().push(pair);
        }
    }
    out
}

/// The exit status the kernel holds for an unreaped child, or None.
///
/// AMUX-4636. `<proc_root>/<pid>/stat` field 3 is the state, field 4 the parent
/// pid and field 52 `exit_code` (Linux 3.5+, in waitpid encoding). Accepted
/// only for a zombie ('Z') whose parent is the tmux server, so a recycled pid
/// or an unrelated process can never supply an exit. `comm` (field 2) may hold
/// spaces and parentheses, so fields are counted after its LAST ')'. Anything
/// unreadable is None, which keeps the honest Crashed { signal: None }.
fn zombie_exit_status(
    proc_root: &std::path::Path,
    pid: &str,
    parent: &str,
) -> Option<BackendStatus> {
    pid.parse::<u32>().ok()?;
    let stat = std::fs::read_to_string(proc_root.join(pid).join("stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // fields[0] is field 3 (state), so field N is fields[N - 3].
    if fields.first() != Some(&"Z") || fields.get(1) != Some(&parent) {
        return None;
    }
    let raw: i32 = fields.get(52 - 3)?.parse().ok()?;
    if raw & 0x7f == 0 {
        Some(BackendStatus::Completed {
            exit_code: (raw >> 8) & 0xff,
        })
    } else {
        Some(BackendStatus::Crashed {
            signal: Some(raw & 0x7f),
        })
    }
}

/// POSIX single-quote escaping — the command line is typed into a login shell
/// via send-keys, so quoting is ours (identical rationale to herdr.rs; the
/// two backends deliberately do not share private helpers across modules).
fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'_' | b'-' | b'.' | b'/' | b'=' | b':' | b'@' | b'%' | b'+' | b','
                )
        })
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn process_exit_census_requires_every_pane_to_be_dead() {
        let exits = super::parse_process_exits(
            "amux-dead:1:1:\namux-live:0::\namux-mixed:1:1:\namux-mixed:0::\namux-signal:1::9\namux-ambiguous:1:1:\namux-ambiguous:1:2:\nforeign:1:1:\n"
        ).unwrap();
        assert_eq!(exits.len(), 3);
        assert_eq!(exits["amux-dead"].code, Some(1));
        assert_eq!(exits["amux-signal"].signal, Some(9));
        assert_eq!(exits["amux-ambiguous"].code, None);
        assert_eq!(exits["amux-ambiguous"].signal, None);
        assert!(!exits.contains_key("amux-live"));
        assert!(!exits.contains_key("amux-mixed"));
        for unreadable in ["amux-dead:1", "amux-dead:?:1:", ":1:1:"] {
            assert!(super::parse_process_exits(unreadable).is_err());
        }
        assert!(super::parse_process_exits("").unwrap().is_empty());
    }
    use super::*;

    /// The L2 regression test: the two target forms must differ by exactly
    /// the trailing colon, and both must carry the exact-match `=`.
    #[test]
    fn l2_target_forms() {
        assert_eq!(session_target("amux-wrk_01ABC"), "=amux-wrk_01ABC");
        assert_eq!(pane_target("amux-wrk_01ABC"), "=amux-wrk_01ABC:");
        assert!(
            !session_target("x").ends_with(':'),
            "session-level targets must not carry the pane-level colon"
        );
        assert!(
            pane_target("x").ends_with(':'),
            "pane-level targets require the trailing colon (L2, 2026-08-08)"
        );
    }

    #[test]
    fn pane_dead_parsing() {
        assert_eq!(parse_pane_dead("0::"), BackendStatus::Running);
        assert_eq!(parse_pane_dead("0"), BackendStatus::Running);
        assert_eq!(
            parse_pane_dead("1:0:"),
            BackendStatus::Completed { exit_code: 0 }
        );
        assert_eq!(
            parse_pane_dead("1:137:"),
            BackendStatus::Completed { exit_code: 137 }
        );
        assert_eq!(
            parse_pane_dead("1::9"),
            BackendStatus::Crashed { signal: Some(9) }
        );
        assert_eq!(
            parse_pane_dead("1::"),
            BackendStatus::Crashed { signal: None }
        );
        // Defensive: garbage line from a future tmux — never invent an exit.
        assert_eq!(parse_pane_dead(""), BackendStatus::Running);
    }

    /// A /proc/<pid>/stat line with `comm`, state, parent and exit_code set
    /// and every other field zero, 52 fields in all.
    fn fake_stat(pid: &str, comm: &str, state: &str, parent: &str, exit_code: i32) -> String {
        let mut rest = vec!["0".to_string(); 50];
        rest[0] = state.into();
        rest[1] = parent.into();
        rest[52 - 3] = exit_code.to_string();
        format!("{pid} ({comm}) {}\n", rest.join(" "))
    }

    fn write_stat(root: &std::path::Path, pid: &str, line: &str) {
        std::fs::create_dir_all(root.join(pid)).unwrap();
        std::fs::write(root.join(pid).join("stat"), line).unwrap();
    }

    /// AMUX-4636: the zombie record supplies the exit only for a zombie child
    /// of the tmux server, and is decoded the way waitpid encodes it.
    #[test]
    fn zombie_exit_status_reads_only_a_zombie_child_of_the_server() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        write_stat(r, "100", &fake_stat("100", "sh", "Z", "50", 256));
        assert_eq!(
            zombie_exit_status(r, "100", "50"),
            Some(BackendStatus::Completed { exit_code: 1 })
        );
        write_stat(r, "101", &fake_stat("101", "sh", "Z", "50", 9));
        assert_eq!(
            zombie_exit_status(r, "101", "50"),
            Some(BackendStatus::Crashed { signal: Some(9) })
        );
        write_stat(r, "102", &fake_stat("102", "a b) (c", "Z", "50", 0));
        assert_eq!(
            zombie_exit_status(r, "102", "50"),
            Some(BackendStatus::Completed { exit_code: 0 })
        );
        // Controls: not a zombie, a foreign parent, no record, a non-numeric pid.
        write_stat(r, "103", &fake_stat("103", "sh", "S", "50", 256));
        assert_eq!(zombie_exit_status(r, "103", "50"), None);
        assert_eq!(zombie_exit_status(r, "100", "51"), None);
        assert_eq!(zombie_exit_status(r, "999", "50"), None);
        assert_eq!(zombie_exit_status(r, "../100", "50"), None);
    }

    #[test]
    fn dead_pane_pids_keeps_dead_panes_with_numeric_pids() {
        let pids = dead_pane_pids(
            "amux-a:1:100:50\namux-b:0:101:50\namux-c:1:x:50\nweird:name:1:102:50\n",
        );
        assert_eq!(pids["amux-a"], vec![("100".to_string(), "50".to_string())]);
        assert!(!pids.contains_key("amux-b"));
        assert!(!pids.contains_key("amux-c"));
        assert_eq!(
            pids["weird:name"],
            vec![("102".to_string(), "50".to_string())]
        );
        assert_eq!(parse_pid_pair("100:50"), Some(("100".into(), "50".into())));
        assert_eq!(parse_pid_pair("100:"), None);
    }

    #[test]
    fn sh_quote_neutralizes_metacharacters() {
        assert_eq!(sh_quote("sleep"), "sleep");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("$(reboot)"), "'$(reboot)'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote(""), "''");
    }

    // ---- MO-3622: reconcile is ONE spawn, and the accounting can fail -------

    #[test]
    fn reconcile_census_reads_the_first_pane_of_the_active_window() {
        let census = parse_reconcile_census(
            "amux-live:1:0::\n\
             amux-done:1:1:3:\n\
             amux-sig:1:1::9\n\
             amux-bare:1:1::\n\
             amux-multi:0:1:7:\n\
             amux-multi:1:0::\n\
             amux-multi:1:1:5:\n\
             amux-inactive-only:0:0::\n\
             foreign:1:1:1:\n",
        )
        .unwrap();
        let by_name: std::collections::BTreeMap<_, _> = census.iter().cloned().collect();
        assert_eq!(by_name["amux-live"], Some(BackendStatus::Running));
        assert_eq!(
            by_name["amux-done"],
            Some(BackendStatus::Completed { exit_code: 3 })
        );
        assert_eq!(
            by_name["amux-sig"],
            Some(BackendStatus::Crashed { signal: Some(9) })
        );
        assert_eq!(
            by_name["amux-bare"],
            Some(BackendStatus::Crashed { signal: None })
        );
        // The dead pane in the INACTIVE window must not speak for the session,
        // and only the first pane of the active window counts, exactly as
        // `list-panes -t =name:` + `lines().next()` did.
        assert_eq!(by_name["amux-multi"], Some(BackendStatus::Running));
        // No active-window pane in the census: unknown, never guessed.
        assert_eq!(by_name["amux-inactive-only"], None);
        assert!(!by_name.contains_key("foreign"));
        assert_eq!(census.len(), 6, "one entry per amux- session");
        // First-seen order is kept, so the census is deterministic.
        assert_eq!(census[0].0, "amux-live");
    }

    #[test]
    fn reconcile_census_fails_loudly_on_a_malformed_amux_line() {
        for bad in ["amux-x:1:0:", "amux-x:2:0::", "amux-x:1:9::", "amux-x"] {
            assert!(
                parse_reconcile_census(bad).is_err(),
                "{bad:?} must be an error, not a session that quietly vanished"
            );
        }
        // Foreign lines are never parsed, so their shape cannot break the sweep.
        assert!(parse_reconcile_census("weird line\nother:junk\n:1:0::\n")
            .unwrap()
            .is_empty());
        assert!(parse_reconcile_census("").unwrap().is_empty());
    }

    /// A `tmux` that logs every invocation and answers only the census (plus the
    /// per-session probes when `probe` is set). Returns the backend and the log.
    fn fake_tmux(
        dir: &std::path::Path,
        census: &str,
        probe: bool,
    ) -> (TmuxBackend, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let log = dir.join("calls.log");
        let script = dir.join("tmux");
        let probes = if probe {
            "  \"has-session\"*) exit 0 ;;\n  \"list-panes -t\"*\"pane_pid\"*) echo 1:1 ;;\n  \"list-panes -t\"*) echo 1:: ;;\n"
        } else {
            ""
        };
        let text = format!(
            "#!/bin/sh\necho \"$*\" >> '{log}'\ncase \"$*\" in\n  \"list-panes -a\"*)\ncat <<'EOF'\n{census}EOF\n    ;;\n{probes}  *) echo \"unexpected tmux call: $*\" >&2; exit 1 ;;\nesac\n",
            log = log.display(),
        );
        std::fs::write(&script, text).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (
            TmuxBackend {
                bin: script.display().to_string(),
            },
            log,
        )
    }

    /// `cmd.spawn()` of a just-written executable can hit ETXTBSY when another
    /// test thread forks in the window (see opencode::structured). Retry that
    /// one error; anything else is a real failure.
    async fn reconcile_retrying(b: &TmuxBackend) -> Vec<BackendSession> {
        for _ in 0..20 {
            match b.reconcile().await {
                Ok(v) => return v,
                Err(e) if e.to_string().contains("Text file busy") => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(e) => panic!("reconcile failed: {e}"),
            }
        }
        panic!("reconcile kept hitting ETXTBSY");
    }

    fn calls(log: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// THE REGRESSION. Before MO-3622 a fleet of N sessions cost 1 + 2N spawns
    /// per reconcile, every 2s. It is one, whatever N is.
    #[tokio::test]
    async fn reconcile_costs_one_spawn_for_the_whole_fleet() {
        let dir = tempfile::tempdir().unwrap();
        let mut census = String::new();
        for i in 0..40 {
            census.push_str(&format!("amux-lane{i}:1:0::\n"));
        }
        census.push_str("amux-done:1:1:0:\nforeign:1:0::\n");
        let (backend, log) = fake_tmux(dir.path(), &census, false);

        let hosted = reconcile_retrying(&backend).await;

        let seen = calls(&log);
        assert_eq!(seen.len(), 1, "one spawn for 41 sessions, got: {seen:?}");
        assert!(seen[0].starts_with("list-panes -a"), "{seen:?}");
        assert_eq!(hosted.len(), 41, "every amux- session, no foreign one");
        assert!(hosted.iter().any(|s| s.backend_ref == "amux-done"
            && s.status == BackendStatus::Completed { exit_code: 0 }));
        assert_eq!(
            hosted
                .iter()
                .filter(|s| s.status == BackendStatus::Running)
                .count(),
            40
        );
    }

    /// The census cannot read an exit code out of a dead pane that recorded
    /// none (AMUX-4636), so ONLY that session pays for the per-session probe.
    #[tokio::test]
    async fn reconcile_probes_only_the_session_the_census_cannot_settle() {
        let dir = tempfile::tempdir().unwrap();
        let census = "amux-a:1:0::\namux-b:1:0::\namux-bare:1:1::\namux-c:1:0::\n";
        let (backend, log) = fake_tmux(dir.path(), census, true);

        let hosted = reconcile_retrying(&backend).await;

        let seen = calls(&log);
        assert_eq!(
            seen.iter()
                .filter(|c| c.starts_with("list-panes -a"))
                .count(),
            1
        );
        let probes: Vec<_> = seen
            .iter()
            .filter(|c| !c.starts_with("list-panes -a"))
            .collect();
        assert!(!probes.is_empty(), "the unsettled session must be probed");
        assert!(
            probes.iter().all(|c| c.contains("amux-bare")),
            "only amux-bare may be probed, got: {probes:?}"
        );
        assert_eq!(hosted.len(), 4);
        assert!(hosted
            .iter()
            .any(|s| s.backend_ref == "amux-bare"
                && s.status == BackendStatus::Crashed { signal: None }));
    }

    #[tokio::test]
    async fn reconcile_with_no_server_is_an_empty_answer() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("tmux");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'no server running on /tmp/tmux-501/default' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let backend = TmuxBackend {
            bin: script.display().to_string(),
        };
        assert!(reconcile_retrying(&backend).await.is_empty());
    }

    #[test]
    fn spawn_window_reports_rate_and_verbs_only_after_it_closes() {
        let t0 = std::time::Instant::now();
        let mut w = SpawnWindow::new(t0);
        // 130 spawns inside the window: nothing is reported yet.
        for i in 0..130u64 {
            let verb = if i % 2 == 0 {
                "has-session"
            } else {
                "list-panes"
            };
            assert_eq!(w.note(verb, t0 + Duration::from_millis(i * 100)), None);
        }
        w.note("capture-pane", t0 + Duration::from_secs(1));
        // The next spawn after the window elapsed closes it.
        let rate = w
            .note("list-sessions", t0 + SPAWN_WINDOW + Duration::from_secs(1))
            .expect("window must close once it is old enough");
        assert_eq!(rate.spawns, 131);
        assert!((rate.window_s - 61.0).abs() < 0.01);
        assert!((rate.per_s - 131.0 / 61.0).abs() < 0.01);
        assert_eq!(rate.top_verbs[0], ("has-session".to_string(), 65));
        assert_eq!(rate.top_verbs[1], ("list-panes".to_string(), 65));
        // The closing spawn opens the next window rather than being lost.
        assert_eq!(w.count, 1);
    }

    #[test]
    fn tmux_verb_skips_leading_flags() {
        assert_eq!(
            tmux_verb(&["-N", "display-message", "-p"]),
            "display-message"
        );
        assert_eq!(tmux_verb(&["list-panes", "-a"]), "list-panes");
        assert_eq!(tmux_verb(&[]), "?");
    }
}

/// AMUX-4803: secrets must not reach a process argument list.
#[cfg(test)]
mod argv_secret_tests {
    use super::*;

    /// The keys actually observed in the leak, plus the shapes around them.
    /// `ps -axo command` showed `-e OPENAI_API_KEY=<full key>` on a tmux server
    /// that had been up 3d22h.
    #[test]
    fn credential_shaped_names_are_refused_from_argv() {
        for key in [
            "OPENAI_API_KEY",
            "GOOGLE_API_KEY",
            "ANTHROPIC_API_KEY",
            "GEMINI_API_KEY",
            "GITHUB_TOKEN",
            "SLACK_CLIENT_SECRET",
            "MATTERMOST_PASSWORD",
        ] {
            assert!(
                !env_pair_is_argv_safe(key, "sk-live-value"),
                "{key} carries a credential and must not go in argv"
            );
        }
        // Case is not a defence: argv does not care how the caller spelled it.
        assert!(!env_pair_is_argv_safe("openai_api_key", "v"));
        assert!(!env_pair_is_argv_safe("MyApiKeyThing", "v"));
    }

    /// The ordinary configuration that SHOULD keep travelling in argv. A guard
    /// that blocks these breaks worker spawn, which is worse than the leak it
    /// was meant to stop.
    #[test]
    fn ordinary_configuration_still_travels() {
        for key in [
            "TMUX_SESSION_NAME",
            "AMUX_SESSION",
            "AMUX_WORKER",
            "AMUX_URL",
            "ANTHROPIC_API_BASE",
            "GOOGLE_CLOUD_PROJECT",
            "GOOGLE_CLOUD_LOCATION",
            "TERM",
            "GOOGLE_GENAI_USE_VERTEXAI",
        ] {
            assert!(
                env_pair_is_argv_safe(key, "some-value"),
                "{key} is not a secret"
            );
        }
    }

    /// AN EMPTY VALUE IS ALWAYS SAFE, and this is load-bearing rather than a
    /// nicety: `ANTHROPIC_API_KEY=` is how an OAuth worker SUPPRESSES an
    /// inherited key. Banning the name outright would delete that mechanism and
    /// silently hand OAuth workers a key they are supposed to run without.
    #[test]
    fn an_empty_value_is_not_a_secret() {
        assert!(env_pair_is_argv_safe("ANTHROPIC_API_KEY", ""));
        assert!(env_pair_is_argv_safe("OPENAI_API_KEY", ""));
    }
}
