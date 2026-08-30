//! HerdrBackend (RR-0031): `SessionBackend` over the herdr 0.8.0 socket-API CLI.
//!
//! CLI surface discovered against herdr 0.8.0 on 2026-08-09 (`herdr --help` and
//! per-subcommand `--help`, plus live probes against a running server). herdr's
//! model is: named persistent *sessions* host *workspaces*, which contain *tabs*
//! and *panes*; every process lives in a pane's pty. There is no one-shot
//! "spawn a named agent process" verb, so this backend maps one amux worker to
//! one single-pane herdr **workspace** whose *label* is the backend ref
//! (`amux-wrk_<ulid>`, derived ONLY via `super::backend_ref` — Invariant 43):
//!
//! - spawn      -> `workspace create --cwd .. --label <ref> --env K=V --no-focus`
//!   then `pane run <pane> "cd .. && exec <cmd>"`
//! - terminate  -> `workspace close <workspace_id>` (verified: kills the pane
//!   process — a probe `sleep 300` was gone after close)
//! - status     -> workspace-with-label present? (see HERDR-GAP on exit codes)
//! - reconcile  -> `workspace list`, filter labels with the `amux-` prefix
//! - capture    -> `pane read <pane> --lines N --format text`
//! - attach     -> `workspace focus` + `session attach` (see HERDR-GAP)
//!
//! All CLI calls go through one runner with a hard timeout; responses are JSON
//! envelopes: `{"id":"cli:..","result":{..}}` or `{"id":..,"error":{"code":..,
//! "message":..}}` (exit code 1 on error).
//!
//! HERDR-GAP index (herdr 0.8.0; each gap is marked inline where it bites):
//! - GAP-EXIT-CODE: no exit status is observable. herdr reaps the pane and its
//!   (single-pane) workspace the moment the pane process exits, so `Completed`
//!   / `Crashed` can never be reported — a finished session maps to `NotFound`.
//! - GAP-DIRECT-EXEC: no verb runs a command *as* the pane process. `pane run`
//!   TYPES the line into the pane's shell (argv is joined with spaces, no
//!   quoting — verified), so we shell-quote ourselves and `exec` to replace
//!   the shell. A shell profile that reads stdin can eat the typed line.
//! - GAP-ATTACH: no per-workspace attach verb (`agent attach` only targets
//!   panes herdr *detected* as known agents). Attach is session attach +
//!   workspace focus.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use super::{
    backend_ref, AttachInfo, BackendError, BackendSession, BackendStatus, ProcessRef, Result,
    SessionBackend, SessionSpec,
};

/// Per-CLI-call timeout. Spawn/status/etc. issue several calls, each bounded.
const OP_TIMEOUT: Duration = Duration::from_secs(5);
/// Capture can move more bytes over the socket API.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
/// Prompt submission walks the agent's composer, so it gets the same budget
/// the retired agent-name path used rather than the bare op timeout.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(15);
/// Shell-readiness poll: 32 * 250ms = 8s budget (observed ~2s on this machine).
const READY_POLLS: u32 = 32;
const READY_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub struct HerdrBackend {
    bin: String,
    /// The named herdr session hosting amux workspaces. herdr's CLI is a
    /// socket-API client: every call is routed with a global `--session`
    /// flag, and fails with code `server_not_running` when that session's
    /// server is down.
    session: String,
    /// backend_ref → pane_id. Pane IDs are stable for a workspace's lifetime,
    /// so caching skips the workspace-list + pane-list round-trips (~180ms)
    /// on every capture. Invalidated on NotFound (workspace was closed).
    pane_cache: Mutex<HashMap<String, String>>,
}

/// Parse herdr's JSON envelope from whichever stream carries it (AF-102).
///
/// herdr writes its envelope to STDOUT on success and to STDERR when the server
/// is not running — stdout is empty in that case. Both call sites parsed stdout
/// alone, so the `server_not_running` arm below, written precisely to treat a
/// down herdr as an empty host rather than an error, could never be reached: the
/// parse failed first and returned "unparseable output:" with an empty string
/// interpolated. Measured 2026-08-20 on the live tail: 77 of 95 ERROR/WARN lines
/// (81%) were that one message, ~30 per minute, carrying no diagnostic detail —
/// an instrument drowning the log it is read from.
fn parse_envelope(stdout: &str, stderr: &str) -> Option<Value> {
    serde_json::from_str(stdout.trim())
        .ok()
        .or_else(|| serde_json::from_str(stderr.trim()).ok())
}

impl HerdrBackend {
    pub fn new(herdr_session: impl Into<String>) -> Self {
        Self {
            bin: "herdr".into(),
            session: herdr_session.into(),
            pane_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Session name from `AMUX_HERDR_SESSION`, defaulting to `amux` (the
    /// fleet's long-running herdr session on this machine).
    pub fn from_env() -> Self {
        Self::new(std::env::var("AMUX_HERDR_SESSION").unwrap_or_else(|_| "amux".into()))
    }

    /// Run `herdr --session <s> <args..>` with a hard timeout. `kill_on_drop`
    /// ensures a timed-out CLI process does not linger.
    async fn run_raw(&self, args: &[&str], timeout: Duration) -> Result<std::process::Output> {
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.arg("--session")
            .arg(&self.session)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(out) => Ok(out?),
            Err(_) => Err(BackendError::CommandFailed(format!(
                "herdr {} timed out after {:?}",
                args.join(" "),
                timeout
            ))),
        }
    }

    /// Run a JSON-envelope verb; map `{"error":{..}}` to a typed error.
    async fn run_json(&self, args: &[&str], timeout: Duration) -> Result<Value> {
        let out = self.run_raw(args, timeout).await?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let v: Value = parse_envelope(&stdout, &stderr).ok_or_else(|| {
            BackendError::CommandFailed(format!(
                "herdr {}: unparseable output (exit {:?}): {} {}",
                args.join(" "),
                out.status.code(),
                stdout.trim(),
                stderr.trim(),
            ))
        })?;
        if let Some((code, message)) = envelope_error(&v) {
            return Err(match code.as_str() {
                "workspace_not_found" | "pane_not_found" | "tab_not_found" => {
                    BackendError::NotFound(message)
                }
                _ => BackendError::CommandFailed(format!("{code}: {message}")),
            });
        }
        Ok(v)
    }

    /// Resolve a backend ref (workspace label) to a live workspace id.
    /// `ProcessRef` carries only the ref (its shape is fixed by the trait), so
    /// every operation re-resolves; `workspace list` is one cheap socket call.
    async fn find_workspace(&self, backend_ref: &str) -> Result<Option<String>> {
        let v = self.run_json(&["workspace", "list"], OP_TIMEOUT).await?;
        Ok(parse_workspaces(&v)
            .into_iter()
            .find(|(label, _)| label == backend_ref)
            .map(|(_, id)| id))
    }

    /// The single pane of a spawned workspace. Spawn only ever creates
    /// single-pane workspaces, so the first pane is the process's pane.
    async fn root_pane(&self, workspace_id: &str) -> Result<String> {
        let v = self
            .run_json(&["pane", "list", "--workspace", workspace_id], OP_TIMEOUT)
            .await?;
        v["result"]["panes"]
            .as_array()
            .and_then(|panes| panes.first())
            .and_then(|p| p["pane_id"].as_str())
            .map(str::to_string)
            .ok_or_else(|| BackendError::NotFound(format!("no pane in workspace {workspace_id}")))
    }

    /// Wait for the pane's shell to be idle at a prompt before typing.
    ///
    /// Why (GAP-DIRECT-EXEC): `pane run` types into the shell. On this machine
    /// the bash profile runs a pipeline (`nc | tail -1`) that READS STDIN
    /// during init — observed 2026-08-09 eating a line typed 0.5s after
    /// workspace create. Ready = process-info shows exactly the shell in the
    /// foreground, twice in a row (a single read can land between profile
    /// commands). Returns the shell pid (== the command's pid after `exec`).
    /// If readiness never arrives we type anyway — the pty buffers keystrokes —
    /// and this remains a best-effort mitigation, not a guarantee.
    async fn wait_shell_ready(&self, pane_id: &str) -> Option<u32> {
        let mut consecutive = 0u32;
        let mut shell_pid: Option<u32> = None;
        for _ in 0..READY_POLLS {
            if let Ok(v) = self
                .run_json(&["pane", "process-info", "--pane", pane_id], OP_TIMEOUT)
                .await
            {
                let info = &v["result"]["process_info"];
                let sp = info["shell_pid"].as_u64();
                shell_pid = sp.map(|p| p as u32);
                let fg = info["foreground_processes"].as_array();
                let ready = match (sp, fg) {
                    (Some(sp), Some(fg)) => {
                        fg.len() == 1 && fg[0]["pid"].as_u64() == Some(sp)
                    }
                    _ => false,
                };
                if ready {
                    consecutive += 1;
                    if consecutive >= 2 {
                        return shell_pid;
                    }
                } else {
                    consecutive = 0;
                }
            }
            tokio::time::sleep(READY_POLL_INTERVAL).await;
        }
        shell_pid
    }

    /// Resolve pane id, using the cache to skip workspace-list + pane-list
    /// (~180ms saved). On NotFound, evicts the cache entry and retries once
    /// cold in case the workspace was recreated with a new pane id.
    async fn resolve_pane(&self, backend_ref: &str) -> Result<String> {
        if let Some(cached) = self.pane_cache.lock().unwrap().get(backend_ref) {
            return Ok(cached.clone());
        }
        let ws = self
            .find_workspace(backend_ref)
            .await?
            .ok_or_else(|| BackendError::NotFound(backend_ref.to_string()))?;
        let pane = self.root_pane(&ws).await?;
        self.pane_cache
            .lock()
            .unwrap()
            .insert(backend_ref.to_string(), pane.clone());
        Ok(pane)
    }

    fn evict_pane(&self, backend_ref: &str) {
        self.pane_cache.lock().unwrap().remove(backend_ref);
    }

    async fn do_pane_read(&self, pane: &str, lines: u32) -> Result<String> {
        let lines_s = lines.to_string();
        let out = self
            .run_raw(
                &[
                    "pane", "read", pane, "--lines", &lines_s, "--format", "text",
                ],
                CAPTURE_TIMEOUT,
            )
            .await?;
        if !out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if let Ok(v) = serde_json::from_str::<Value>(stdout.trim()) {
                if let Some((code, message)) = envelope_error(&v) {
                    return Err(match code.as_str() {
                        "pane_not_found" | "workspace_not_found" => {
                            BackendError::NotFound(message)
                        }
                        _ => BackendError::CommandFailed(format!("{code}: {message}")),
                    });
                }
            }
            return Err(BackendError::CommandFailed(format!(
                "herdr pane read failed: {} {}",
                stdout.trim(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Clear the composer, then submit `text` as a prompt to the pane's agent.
    ///
    /// Two verbs with different success shapes, so they cannot share a runner:
    /// `pane send-keys` prints NOTHING on success (exit 0, empty stdout —
    /// measured against herdr 0.8.0) and only emits an envelope when it fails,
    /// while `agent prompt` answers with a normal `{"result":..}` envelope.
    ///
    /// The clear is best-effort: it is a hygiene step, and failing the send
    /// because a keystroke was refused would turn a probably-deliverable
    /// prompt into a hard error.
    async fn do_pane_prompt(&self, pane: &str, text: &str) -> Result<()> {
        let clear = self
            .run_raw(&["pane", "send-keys", pane, "ctrl+u"], OP_TIMEOUT)
            .await;
        if let Ok(out) = &clear {
            if !out.status.success() {
                tracing::debug!(
                    pane,
                    "herdr pane send-keys ctrl+u did not clear the composer: {}",
                    String::from_utf8_lossy(&out.stdout).trim()
                );
            }
        }
        self.run_json(&["agent", "prompt", pane, text], PROMPT_TIMEOUT).await?;
        Ok(())
    }
}

#[async_trait]
impl SessionBackend for HerdrBackend {
    fn name(&self) -> &'static str {
        "herdr"
    }

    async fn spawn(&self, spec: &SessionSpec) -> Result<ProcessRef> {
        if spec.command.is_empty() {
            return Err(BackendError::SpawnFailed("empty command".into()));
        }
        let ref_ = backend_ref(&spec.worker);
        // herdr labels are not unique; the backend enforces one workspace per ref.
        if self.find_workspace(&ref_).await?.is_some() {
            return Err(BackendError::SpawnFailed(format!(
                "backend ref {ref_} already exists in herdr session {}",
                self.session
            )));
        }

        // Env goes through `--env` (structured, no quoting hazards in the
        // typed line). A shell profile that re-exports the same variable would
        // win; accepted — same exposure as any terminal-hosted process.
        let mut env_args: Vec<String> = Vec::new();
        for (k, v) in &spec.env {
            env_args.push("--env".into());
            env_args.push(format!("{k}={v}"));
        }
        let mut args: Vec<&str> = vec![
            "workspace",
            "create",
            "--cwd",
            &spec.cwd,
            "--label",
            &ref_,
            "--no-focus", // never steal focus in a live herdr session
        ];
        args.extend(env_args.iter().map(String::as_str));
        let v = self
            .run_json(&args, OP_TIMEOUT)
            .await
            .map_err(|e| BackendError::SpawnFailed(e.to_string()))?;
        let pane_id = v["result"]["root_pane"]["pane_id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| {
                BackendError::SpawnFailed("workspace create returned no root_pane".into())
            })?;

        let pid = self.wait_shell_ready(&pane_id).await;

        // GAP-DIRECT-EXEC: `pane run` joins argv with spaces and types it into
        // the shell (verified: argv ["echo","a b"] arrives as `echo a b`), so
        // quoting is ours. `cd` explicitly because shell profiles can cd away
        // from the workspace `--cwd` (observed: this machine's profile cds to
        // ~/Dev). `exec` replaces the shell with the command so the pane holds
        // exactly the process we mean: its pid is the shell pid, and its exit
        // reaps the pane (see GAP-EXIT-CODE in status()).
        let line = format!(
            "cd {} && exec {}",
            sh_quote(&spec.cwd),
            spec.command
                .iter()
                .map(|a| sh_quote(a))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let out = self
            .run_raw(&["pane", "run", &pane_id, &line], OP_TIMEOUT)
            .await
            .map_err(|e| BackendError::SpawnFailed(e.to_string()))?;
        if !out.status.success() {
            // Half-spawned workspace must not leak: close it, best effort.
            if let Ok(Some(ws)) = self.find_workspace(&ref_).await {
                let _ = self.run_json(&["workspace", "close", &ws], OP_TIMEOUT).await;
            }
            return Err(BackendError::SpawnFailed(format!(
                "pane run failed: {} {}",
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }

        // Display metadata (AMUX-2613 gap 5 — herdr capability the backend
        // under-used): the workspace LABEL is the machine ref
        // (`amux-wrk_<ulid>`), opaque to a human attached to the session.
        // `workspace report-metadata` tokens show alongside it in herdr's
        // UI/`workspace get` (`tokens: {worker: <name>}` — verified live
        // 2026-08-09; note the CLI wants the positional ID BEFORE flags).
        // Display-only and best-effort: a metadata miss must never fail a
        // spawn that succeeded.
        if let Some(name) = spec.human_label.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(ws_id) = v["result"]["root_pane"]["workspace_id"].as_str() {
                let token = format!("worker={name}");
                if let Err(e) = self
                    .run_json(
                        &["workspace", "report-metadata", ws_id, "--source", "amux", "--token", &token],
                        OP_TIMEOUT,
                    )
                    .await
                {
                    tracing::debug!(backend_ref = %ref_, error = %e,
                        "herdr workspace metadata report failed (display-only; ignored)");
                }
            }
        }

        Ok(ProcessRef {
            backend_ref: ref_,
            pid,
        })
    }

    async fn terminate(&self, proc: &ProcessRef) -> Result<()> {
        // Scope guard: this backend only owns refs it minted (`amux-…`).
        // Refusing anything else means a bug above us can never close a
        // human's workspace (ethos rule 8).
        if !proc.backend_ref.starts_with("amux-") {
            return Err(BackendError::CommandFailed(format!(
                "refusing to terminate {:?}: outside the amux- namespace",
                proc.backend_ref
            )));
        }
        let ws = self
            .find_workspace(&proc.backend_ref)
            .await?
            .ok_or_else(|| BackendError::NotFound(proc.backend_ref.clone()))?;
        // Verified 2026-08-09: `workspace close` kills the pane's processes
        // (probe `sleep 300` was gone after close), it does not orphan them.
        self.run_json(&["workspace", "close", &ws], OP_TIMEOUT)
            .await?;
        Ok(())
    }

    async fn status(&self, proc: &ProcessRef) -> Result<BackendStatus> {
        // GAP-EXIT-CODE: herdr 0.8.0 reaps the pane — and its single-pane
        // workspace — the moment the pane's process exits (verified 2026-08-09:
        // `exec sleep 3` -> workspace gone from `workspace list` 1s after
        // exit, pane_get -> pane_not_found), and it exposes no exit status.
        // So `Completed`/`Crashed` are unobservable here: present == Running,
        // absent == NotFound, and a finished session is indistinguishable from
        // one that never existed. We report NotFound rather than fake a
        // `Completed {{ exit_code: 0 }}` nobody measured (ethos rule 7: a
        // status that cannot be wrong is theatre).
        //
        // Residual hazard of GAP-DIRECT-EXEC: if the typed command line was
        // eaten by a stdin-reading shell profile, the pane holds an idle shell
        // forever and this honestly reports Running — the *shell* is the pane
        // process. wait_shell_ready() in spawn() is the mitigation.
        //
        // If the herdr server itself is down this returns Err (CommandFailed
        // "server_not_running: …") — "cannot answer" is not "not found".
        match self.find_workspace(&proc.backend_ref).await? {
            Some(_) => Ok(BackendStatus::Running),
            None => Ok(BackendStatus::NotFound),
        }
    }

    async fn attach_info(&self, proc: &ProcessRef) -> Result<AttachInfo> {
        let ws = self
            .find_workspace(&proc.backend_ref)
            .await?
            .ok_or_else(|| BackendError::NotFound(proc.backend_ref.clone()))?;
        // GAP-ATTACH: herdr 0.8.0 has no "attach to this workspace" verb —
        // `session attach` grabs the whole session and `agent attach` only
        // resolves panes herdr detected as known agents (arbitrary commands
        // show agent_status "unknown"). Closest honest composition: focus the
        // workspace over the socket API, then attach the client.
        Ok(AttachInfo {
            command: format!(
                "herdr --session {s} workspace focus {ws} && herdr session attach {s}",
                s = self.session
            ),
        })
    }

    async fn reconcile(&self) -> Result<Vec<BackendSession>> {
        let out = self.run_raw(&["workspace", "list"], OP_TIMEOUT).await?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        // Both streams, and BOTH streams in the failure message — the old text
        // interpolated only stdout, which is empty in exactly the case that fires,
        // so the log line ended in a colon and told the reader nothing (AF-102).
        let v: Value = parse_envelope(&stdout, &stderr).ok_or_else(|| {
            BackendError::CommandFailed(format!(
                "herdr workspace list: unparseable output (exit {:?}): stdout={:?} stderr={:?}",
                out.status.code(),
                stdout.trim(),
                stderr.trim()
            ))
        })?;
        if let Some((code, message)) = envelope_error(&v) {
            if code == "server_not_running" {
                // The herdr server hosts every pane pty; with it down, no
                // amux process is running under this backend. For process
                // reconciliation (Invariant 9) that is an empty host, not an
                // error — the store's sessions get marked dead, which is true.
                return Ok(Vec::new());
            }
            return Err(BackendError::CommandFailed(format!("{code}: {message}")));
        }
        Ok(parse_workspaces(&v)
            .into_iter()
            .filter(|(label, _)| label.starts_with("amux-"))
            .map(|(label, _)| BackendSession {
                backend_ref: label,
                // Present implies Running: herdr reaps exited panes
                // (GAP-EXIT-CODE above).
                status: BackendStatus::Running,
            })
            .collect())
    }

    async fn capture(&self, proc: &ProcessRef, lines: u32) -> Result<String> {
        let pane = self.resolve_pane(&proc.backend_ref).await?;
        let result = self.do_pane_read(&pane, lines).await;
        match &result {
            Err(BackendError::NotFound(_)) => {
                self.evict_pane(&proc.backend_ref);
                let pane = self.resolve_pane(&proc.backend_ref).await?;
                self.do_pane_read(&pane, lines).await
            }
            _ => result,
        }
    }

    /// `agent prompt <pane> <text>` — herdr's own prompt-submission verb,
    /// addressed by PANE ID (verified against herdr 0.8.0: `agent get w2:p1`
    /// resolves, so a pane is a valid agent target).
    ///
    /// Not `pane send-text`: a `\n` inside that verb is delivered as Enter
    /// (measured 2026-08-20 — `"echo AAA\necho BBB"` ran AAA and left BBB in
    /// the composer), so a multi-line prompt would submit its first line and
    /// strand the rest. `agent prompt` is the verb that knows how to hand a
    /// whole prompt to a recognized agent.
    ///
    /// A pane whose process herdr does NOT recognize as an agent (a bare
    /// shell — GAP-ATTACH) fails here rather than typing into it blind. That
    /// is the honest answer: the provider is not accepting prompts.
    ///
    /// `ctrl+u` first, so a prompt never concatenates onto whatever was left
    /// in the composer. Pane-not-found retries once cold like `capture` — the
    /// pane id is cached and a restarted worker gets a new one.
    async fn send_text(&self, proc: &ProcessRef, text: &str) -> Result<()> {
        let pane = self.resolve_pane(&proc.backend_ref).await?;
        match self.do_pane_prompt(&pane, text).await {
            Err(BackendError::NotFound(_)) => {
                self.evict_pane(&proc.backend_ref);
                let pane = self.resolve_pane(&proc.backend_ref).await?;
                self.do_pane_prompt(&pane, text).await
            }
            other => other,
        }
    }

    /// #84: one `workspace list` for the whole fleet, returning
    /// `(backend_ref, agent_status)` for every `amux-` workspace whose pane
    /// herdr has recognized as a known agent (`idle`/`working`/`blocked`/
    /// `done`; arbitrary commands read `unknown` and are omitted — the
    /// scrape stays their voice). Verified live on herdr 0.7.5 (2026-08-15):
    /// `pane run "claude"` is auto-recognized and the workspace carries
    /// `agent_status: "idle"`; a `workspace list` already returns it, so no
    /// per-lane probe is needed. A read failure is `Err`, NOT an empty map.
    async fn agent_states(&self) -> Result<std::collections::BTreeMap<String, String>> {
        let v = self.run_json(&["workspace", "list"], OP_TIMEOUT).await?;
        Ok(parse_workspace_statuses(&v).into_iter().collect())
    }
}

/// `{"error":{"code":..,"message":..}}` -> (code, message).
fn envelope_error(v: &Value) -> Option<(String, String)> {
    let err = v.get("error")?;
    Some((
        err["code"].as_str().unwrap_or("unknown").to_string(),
        err["message"].as_str().unwrap_or("").to_string(),
    ))
}

/// `workspace list` envelope -> [(label, workspace_id)].
fn parse_workspaces(v: &Value) -> Vec<(String, String)> {
    v["result"]["workspaces"]
        .as_array()
        .map(|ws| {
            ws.iter()
                .filter_map(|w| {
                    Some((
                        w["label"].as_str()?.to_string(),
                        w["workspace_id"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `workspace list` envelope -> [(label, agent_status)] for amux-owned
/// workspaces whose pane herdr recognized as a known agent. `unknown`
/// (arbitrary commands — GAP-ATTACH) and agentless entries are omitted:
/// for them the scrape is the only voice, and an `unknown` key in the map
/// would read as "herdr answered" to a caller that only checks presence.
fn parse_workspace_statuses(v: &Value) -> Vec<(String, String)> {
    v["result"]["workspaces"]
        .as_array()
        .map(|ws| {
            ws.iter()
                .filter_map(|w| {
                    let label = w["label"].as_str()?;
                    let status = w["agent_status"].as_str()?;
                    if !label.starts_with("amux-") || status == "unknown" {
                        return None;
                    }
                    Some((label.to_string(), status.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// POSIX single-quote escaping: safe against spaces, `$(..)`, backticks, `;`.
/// Needed because the command line is TYPED into a shell (GAP-DIRECT-EXEC) —
/// herdr does no quoting of its own.
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

    /// AF-102. The exact bytes `herdr workspace list` produced on this machine on
    /// 2026-08-20 with no herdr server running: envelope on STDERR, stdout EMPTY,
    /// exit 1. Captured from the real CLI, not composed — the old code parsed
    /// stdout alone, so this specimen is what made the `server_not_running` arm
    /// in `reconcile` unreachable and put 77 of 95 ERROR/WARN lines in the log.
    const REAL_STDERR_WHEN_SERVER_DOWN: &str = r#"{"id":"cli:workspace:list","error":{"code":"server_not_running","message":"no herdr server is running at /Users/ethan/.config/herdr/herdr.sock; run `herdr` to start or attach it"}}"#;

    #[test]
    fn the_envelope_is_found_on_stderr_when_stdout_is_empty() {
        let v = super::parse_envelope("", REAL_STDERR_WHEN_SERVER_DOWN)
            .expect("the real down-server specimen must parse");
        let (code, _msg) = super::envelope_error(&v).expect("it is an error envelope");
        assert_eq!(code, "server_not_running",
            "reconcile keys its empty-host arm on this exact code; if it does not \
             surface, that arm stays unreachable and the WARN storm returns");
    }

    #[test]
    fn stdout_still_wins_when_it_carries_the_envelope() {
        let v = super::parse_envelope(r#"{"id":"x","workspaces":[]}"#, "ignored noise")
            .expect("normal success path");
        assert!(v.get("workspaces").is_some(), "stdout must take precedence over stderr");
    }

    /// The control. Without it, a `parse_envelope` that returned Some(Null) for
    /// anything would pass both tests above and silently swallow real breakage.
    #[test]
    fn genuinely_unparseable_output_is_still_none() {
        assert!(super::parse_envelope("", "").is_none());
        assert!(super::parse_envelope("not json", "also not json").is_none());
    }

    use super::*;

    #[test]
    fn sh_quote_passes_safe_strings_through() {
        assert_eq!(sh_quote("sleep"), "sleep");
        assert_eq!(sh_quote("/tmp/x-y_z.0"), "/tmp/x-y_z.0");
    }

    #[test]
    fn sh_quote_neutralizes_shell_metacharacters() {
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(sh_quote("`date`"), "'`date`'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn parses_workspace_list_envelope() {
        // Shape captured from a live herdr 0.8.0 probe (2026-08-09).
        let v: Value = serde_json::from_str(
            r#"{"id":"cli:workspace:list","result":{"type":"workspace_list",
                "workspaces":[
                  {"active_tab_id":"w7:t1","agent_status":"unknown","focused":true,
                   "label":"amux-wrk_01ABC","number":1,"pane_count":1,"tab_count":1,
                   "workspace_id":"w7"},
                  {"label":"scratch","workspace_id":"w9"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_workspaces(&v),
            vec![
                ("amux-wrk_01ABC".to_string(), "w7".to_string()),
                ("scratch".to_string(), "w9".to_string())
            ]
        );
        assert!(envelope_error(&v).is_none());
    }

    #[test]
    fn parses_native_agent_statuses() {
        // Shape captured from a live herdr 0.7.5 probe (2026-08-15): a
        // `pane run "claude"` lane reads `idle`, an arbitrary command reads
        // `unknown`, and a foreign workspace is not ours to report.
        let v: Value = serde_json::from_str(
            r#"{"id":"cli:workspace:list","result":{"type":"workspace_list",
                "workspaces":[
                  {"agent_status":"unknown","label":"amux-wrk-spike","workspace_id":"w1"},
                  {"agent_status":"idle","label":"amux-wrk-claude","workspace_id":"w2"},
                  {"agent_status":"blocked","label":"amux-wrk-stuck","workspace_id":"w3"},
                  {"agent_status":"done","label":"amux-wrk-finished","workspace_id":"w4"},
                  {"agent_status":"idle","label":"scratch","workspace_id":"w9"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_workspace_statuses(&v),
            vec![
                ("amux-wrk-claude".to_string(), "idle".to_string()),
                ("amux-wrk-stuck".to_string(), "blocked".to_string()),
                ("amux-wrk-finished".to_string(), "done".to_string()),
            ]
        );
    }

    #[test]
    fn parses_error_envelope() {
        let v: Value = serde_json::from_str(
            r#"{"error":{"code":"workspace_not_found","message":"workspace w999 not found"},
                "id":"cli:workspace:get"}"#,
        )
        .unwrap();
        assert_eq!(
            envelope_error(&v),
            Some((
                "workspace_not_found".to_string(),
                "workspace w999 not found".to_string()
            ))
        );
    }
}
