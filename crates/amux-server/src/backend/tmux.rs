//! TmuxBackend (RR-0032): `SessionBackend` over the tmux CLI (fallback backend).
//!
//! One amux worker maps to one tmux session named `amux-wrk_<ulid>` (derived
//! ONLY via `super::backend_ref` — Invariant 43). Verbs:
//!
//! - spawn      -> `new-session -d -s <ref> -x 220 -y 50 -c <cwd> [-e K=V..]`,
//!   `set-option remain-on-exit on`, then `send-keys -l` + Enter
//! - terminate  -> `kill-session -t '=<ref>'`
//! - status     -> `has-session` then `list-panes #{pane_dead}...`
//! - reconcile  -> `list-sessions -F '#{session_name}'`, `amux-` prefix only
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

impl TmuxBackend {
    pub fn new() -> Self {
        Self { bin: "tmux".into() }
    }

    async fn run(&self, args: &[&str], timeout: Duration) -> Result<std::process::Output> {
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
        Ok(parse_pane_dead(stdout.lines().next().unwrap_or("")))
    }
}

impl Default for TmuxBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionBackend for TmuxBackend {
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
        for (k, v) in &spec.env {
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
        let out = self
            .run(&["list-sessions", "-F", "#{session_name}"], OP_TIMEOUT)
            .await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // No tmux server == hosting nothing. That is an empty answer, not
            // a failure (a fresh boot has no server until the first spawn).
            if stderr.contains("no server") || stderr.contains("No such file") {
                return Ok(Vec::new());
            }
            return Err(BackendError::CommandFailed(format!(
                "tmux list-sessions failed: {}",
                stderr.trim()
            )));
        }
        let names: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|n| n.starts_with("amux-"))
            .map(str::to_string)
            .collect();
        // Probe all sessions concurrently (was sequential — 2 tmux calls per
        // session × 50 sessions = 88s startup blackout, AMUX-3969b). Bounded
        // to 10 at a time so we do not fork-bomb the tmux server.
        use futures::stream::{self, StreamExt};
        let sessions: Vec<Result<BackendSession>> = stream::iter(names)
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
        sessions.into_iter().collect()
    }

    async fn capture(&self, proc: &ProcessRef, lines: u32) -> Result<String> {
        let pt = pane_target(&proc.backend_ref);
        // -p print to stdout; -S -<n> start <n> lines above the visible top
        // (scrollback). Pane-level command => pane target form (L2).
        let start = format!("-{lines}");
        let out = self
            .run(&["capture-pane", "-t", &pt, "-p", "-S", &start], CAPTURE_TIMEOUT)
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

/// POSIX single-quote escaping — the command line is typed into a login shell
/// via send-keys, so quoting is ours (identical rationale to herdr.rs; the
/// two backends deliberately do not share private helpers across modules).
fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b'=' | b':' | b'@' | b'%' | b'+' | b','))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn sh_quote_neutralizes_metacharacters() {
        assert_eq!(sh_quote("sleep"), "sleep");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("$(reboot)"), "'$(reboot)'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote(""), "''");
    }
}
