//! StructuredCliProtocol (RR-0030): [`AgentProtocol`] over headless
//! structured CLI runs — the orchestrator's ExecuteTask path.
//!
//! One prompt = one child process run in the worker's cwd, speaking the
//! provider's structured stdout format (RR-0028e coverage matrix,
//! docs/provider-coverage.csv):
//!
//! - Claude Code: `claude --print <prompt> --output-format stream-json --verbose`
//! - Gemini CLI:  `gemini -p <prompt> --output-format stream-json`
//! - Codex CLI:   `codex exec --json <prompt>`
//!
//! A reader task per child parses stdout lines via [`super::events`] and
//! broadcasts [`WorkerEvent`]s on the worker's channel. Claude and Gemini
//! streams have no explicit turn-start marker, so the reader inserts
//! `TurnStarted` before the first in-turn event; Codex's literal
//! `turn.started` is passed through and deduplicated.
//!
//! Honest limits of the headless shape, stated rather than faked:
//!
//! - `pause`/`resume` return [`ProtocolError::Rejected`]: a `--print`-style
//!   run has no suspend concept. Faking it (e.g. SIGSTOP silently) would be
//!   state the harness reports but the agent does not have.
//! - `deliver_message` mid-turn returns `Rejected`: there is no stdin
//!   session to inject into. The command queue's `AtTurnBoundary` timing is
//!   the sanctioned retry path (Invariant 34). When idle, a message delivery
//!   IS a new turn, keyed by the durable `MessageId` (Invariant 29), so a
//!   redelivered message never double-runs.
//! - `cancel` sends a real SIGINT via `/bin/kill` (graceful — the CLIs
//!   checkpoint on SIGINT per the spike), falling back to
//!   [`tokio::process::Child::start_kill`] (SIGKILL) only if that fails.
//!   nix/libc are not workspace deps and adding one for a single signal is
//!   not worth the surface; `kill(1)` is POSIX and already everywhere amux
//!   runs. `kill_on_drop(true)` reaps children if the server itself dies.
//!
//! Idempotency (Invariant 9): a `send_prompt` whose `idempotency_key` was
//! already seen for that worker returns `Ok` without re-spawning — restart
//! reconciliation replays enqueues, and this is what makes the replay safe.
//!
//! CONVERSATION CONTINUITY (AMUX-2613 gap 2): successive turns share the
//! provider's own conversation, not just a cwd. The reader captures the
//! conversation ref from the stream's init-shaped line (claude `system/init
//! session_id`, codex `thread.started thread_id`, gemini `init session_id`)
//! and the next spawn passes it back (`--resume <id>` / `exec resume <id>`).
//! All three verified LIVE on this machine 2026-08-09:
//!
//! - claude 2.1.226: `--print --resume <session_id>` continues the SAME
//!   session id (no fork); memory confirmed (pomegranate probe).
//! - gemini 0.54.4: `--resume <session_id>` works headless with `-p`;
//!   memory confirmed. (The top-level help only mentions index/"latest";
//!   the error text for a bad id names `--resume {uuid}` as supported.)
//! - codex 0.141.0: `exec resume <thread_id> --json` re-attaches the thread
//!   (same thread_id re-emitted). Memory could not be verified live — this
//!   machine's codex quota is exhausted until 2026-08-27 — so codex
//!   continuity rests on the CLI's documented resume contract.
//!
//! A DEAD resume target must not wedge the worker forever: a missing
//! claude session exits 1 emitting only an error `result` (no init), and a
//! missing gemini session exits 42 with no stream at all (both measured).
//! So: refs are only ever (re)captured from init-shaped lines, and a run
//! that was spawned WITH a resume ref, FAILED, and never emitted one is
//! taken as "the target is gone" — the ref is dropped (memory + sink) so
//! the next turn starts fresh instead of failing identically forever.
//! Refs persist in `_amux_conversations` via [`ConversationSink`]
//! (migration 0011), hydrated back at registration by backend::bootstrap,
//! so continuity survives a server restart.

use super::{events, AgentProtocol, AgentState, Prompt, ProtocolError, Result};
use amux_core::ids::{MessageId, TurnId, WorkerId};
use amux_core::protocol::{ExitStatus, Failure, TurnTrace, TurnTraceKind, WorkerEvent};
use async_trait::async_trait;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::broadcast;

/// Which structured CLI a worker speaks. Closed here (unlike the open
/// `ProviderId`) because each variant IS a concrete argv shape this module
/// owns; an unlisted provider simply is not spawnable by this protocol and
/// belongs to the terminal adapter until its structured shape is captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliProvider {
    ClaudeCode,
    GeminiCli,
    CodexCli,
}

impl CliProvider {
    /// Binary name resolved via PATH (overridable per worker for tests).
    pub fn binary(&self) -> &'static str {
        match self {
            CliProvider::ClaudeCode => "claude",
            CliProvider::GeminiCli => "gemini",
            CliProvider::CodexCli => "codex",
        }
    }

    /// The structured CLI for a provider id as stored on worker rows.
    /// `"claude"` is the workers-table default spelling (migration 0003);
    /// the provider registry's canonical id is `"claude-code"` — both map
    /// here so a worker created through either era of the API resolves.
    /// `None` means this protocol cannot drive the provider; such a worker
    /// stays terminal-hosted only and the pump never sees a live session.
    pub fn from_provider_id(id: &str) -> Option<Self> {
        match id {
            "claude" | "claude-code" => Some(CliProvider::ClaudeCode),
            "gemini" => Some(CliProvider::GeminiCli),
            // ollama workers now run `codex --oss --local-provider ollama`, so
            // they emit the same structured events as codex and are handled by
            // the same protocol pump.
            "codex" | "ollama" => Some(CliProvider::CodexCli),
            _ => None,
        }
    }

    /// Canonical family name for persistence: the provider registry's
    /// canonical id, stable across the worker-row spellings that map here
    /// ("claude" and "claude-code" are one family). `_amux_conversations`
    /// stores this so hydration can refuse to replay a ref into a DIFFERENT
    /// provider after a worker's provider changes.
    pub fn family(&self) -> &'static str {
        match self {
            CliProvider::ClaudeCode => "claude-code",
            CliProvider::GeminiCli => "gemini",
            CliProvider::CodexCli => "codex",
        }
    }

    /// The provider-native conversation id carried by one stream line, if
    /// this line is the INIT-shaped one. Deliberately ONLY the init shapes:
    /// claude's bad-resume failure emits an error `result` line that echoes
    /// the DEAD session id (measured 2026-08-09), so matching any line with
    /// a `session_id` would re-capture the very ref whose failure we need
    /// to observe.
    pub fn conversation_ref(&self, line: &str) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let ty = v.get("type").and_then(serde_json::Value::as_str)?;
        let key = match self {
            CliProvider::ClaudeCode => {
                let sub = v.get("subtype").and_then(serde_json::Value::as_str);
                if ty != "system" || sub != Some("init") {
                    return None;
                }
                "session_id"
            }
            CliProvider::GeminiCli => {
                if ty != "init" {
                    return None;
                }
                "session_id"
            }
            CliProvider::CodexCli => {
                if ty != "thread.started" {
                    return None;
                }
                "thread_id"
            }
        };
        v.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    }

    /// Argv for one headless prompt run. Flags verified against the
    /// installed binaries on 2026-08-09 (claude 2.1.226, gemini 0.54.4,
    /// codex 0.141.0) — see events.rs for the captured output they produce.
    ///
    /// `model` is the worker's configured model (tier alias or full id);
    /// all three CLIs take `--model`. None = the CLI's own default — never
    /// invent one here (the model choice is the user's, ethos rule 8).
    ///
    /// `resume` is the conversation ref a previous run reported (see the
    /// module docs' continuity section); None = a fresh conversation.
    fn args(&self, prompt: &str, model: Option<&str>, resume: Option<&str>) -> Vec<String> {
        let mut argv: Vec<String> = match self {
            // --verbose is required for stream-json with --print.
            CliProvider::ClaudeCode => vec![
                "--print".into(),
                prompt.into(),
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
            ],
            // --approval-mode auto_edit: a HEADLESS gemini turn has no human
            // to approve a write tool, so the default mode hangs FOREVER on
            // any task that edits a file (verified live, RR-0085: default
            // hung >90s on a one-line write; auto_edit finished in ~10s).
            CliProvider::GeminiCli => vec![
                "-p".into(),
                prompt.into(),
                "--output-format".into(),
                "stream-json".into(),
                "--approval-mode".into(),
                "auto_edit".into(),
            ],
            // --skip-git-repo-check: codex refuses to run outside a git repo
            // otherwise. The orchestrator chose the cwd deliberately, and a
            // headless run cannot answer the interactive refusal.
            //
            // Continuation is a SUBCOMMAND here (`exec resume <id> <prompt>`),
            // not a flag, so the whole argv reshapes when resuming.
            CliProvider::CodexCli => match resume {
                Some(thread) => vec![
                    "exec".into(),
                    "resume".into(),
                    "--json".into(),
                    "--skip-git-repo-check".into(),
                    thread.into(),
                    prompt.into(),
                ],
                None => vec![
                    "exec".into(),
                    "--json".into(),
                    "--skip-git-repo-check".into(),
                    prompt.into(),
                ],
            },
        };
        // claude + gemini share the flag spelling; codex handled above.
        if !matches!(self, CliProvider::CodexCli) {
            if let Some(r) = resume {
                argv.push("--resume".into());
                argv.push(r.into());
            }
        }
        if let Some(m) = model {
            argv.push("--model".into());
            argv.push(m.into());
        }
        argv
    }

    /// Extra env for headless runs. Gemini refuses untrusted directories
    /// with an interactive prompt a headless run cannot answer; the cwd is
    /// the worker's own workspace, chosen by the orchestrator, so trusting
    /// it is stating a fact, not bypassing a control.
    fn envs(&self) -> &'static [(&'static str, &'static str)] {
        match self {
            CliProvider::GeminiCli => &[("GEMINI_CLI_TRUST_WORKSPACE", "true")],
            _ => &[],
        }
    }

    fn translate(&self, line: &str, turn: &TurnId) -> Vec<WorkerEvent> {
        match self {
            CliProvider::ClaudeCode => events::translate_claude(line, turn),
            CliProvider::GeminiCli => events::translate_gemini(line, turn),
            CliProvider::CodexCli => events::translate_codex(line, turn),
        }
    }
}

/// Persistence seam for conversation refs (module docs' continuity
/// section). The protocol captures refs from streams; WHERE they live
/// durably (`_amux_conversations`, migration 0011) is the server's concern
/// — a trait so tests record calls without a store. Both methods are
/// fire-and-forget: continuity must never block or fail a turn.
pub trait ConversationSink: Send + Sync {
    /// The latest conversation ref a worker's stream reported.
    fn save(&self, worker: &WorkerId, provider_family: &str, conversation_ref: &str);
    /// A resume target proved dead (resumed run failed before init) — drop
    /// the persisted ref so a restart does not hydrate it back.
    fn forget(&self, worker: &WorkerId);
}

/// Per-worker spawn configuration, registered once per worker.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub provider: CliProvider,
    /// Working directory for every run — the worker's workspace.
    pub cwd: PathBuf,
    /// Override the binary path (tests point this at a fixture-replaying
    /// script; production leaves it `None` and resolves via PATH).
    pub binary: Option<PathBuf>,
    /// The worker's configured model (`_amux_workers.model`), passed as
    /// `--model` on every run. None = the CLI's own default.
    pub model: Option<String>,
    /// A persisted conversation ref to resume from (hydrated by
    /// backend::bootstrap from `_amux_conversations`). None = start fresh;
    /// the stream's own init line supersedes this the moment a run starts.
    pub conversation: Option<String>,
}

struct WorkerShared {
    /// Who this session belongs to — carried so lifecycle logging names the
    /// worker (a turn that completes invisibly is a diagnosis that cannot
    /// be made; ethos rule 4).
    worker: WorkerId,
    config: WorkerConfig,
    events: broadcast::Sender<WorkerEvent>,
    state: Mutex<AgentState>,
    /// Idempotency keys already accepted (Invariant 9). Inserted only after
    /// a successful spawn, so a failed spawn does not burn the key.
    seen_keys: Mutex<HashSet<String>>,
    /// The live child, present while a turn is running. Held for `cancel`;
    /// the reader task takes it back at EOF to reap the exit status.
    child: Mutex<Option<Child>>,
    paused: std::sync::atomic::AtomicBool,
    reading: std::sync::atomic::AtomicBool,
    /// The provider conversation the NEXT run resumes (continuity, module
    /// docs). Seeded from config, updated from each run's init line.
    conversation: Mutex<Option<String>>,
    /// Durable home for `conversation` (None in tests without a store).
    sink: Option<Arc<dyn ConversationSink>>,
}

impl WorkerShared {
    fn emit(&self, ev: WorkerEvent) {
        // No receivers is fine — broadcast keeps working when they arrive.
        let _ = self.events.send(ev);
    }

    fn set_state(&self, s: AgentState) {
        *self.state.lock().unwrap() = if self.paused.load(std::sync::atomic::Ordering::SeqCst) {
            AgentState::Paused
        } else {
            s
        };
    }
}

/// The structured-CLI implementation of [`AgentProtocol`].
#[derive(Default)]
pub struct StructuredCliProtocol {
    workers: Mutex<BTreeMap<WorkerId, Arc<WorkerShared>>>,
    /// Shared by every worker registered on this protocol instance.
    sink: Option<Arc<dyn ConversationSink>>,
}

impl StructuredCliProtocol {
    pub fn new() -> Self {
        Self::default()
    }

    /// A protocol whose captured conversation refs persist through `sink`
    /// (production: the store-backed sink built in lib.rs).
    pub fn with_conversation_sink(sink: Arc<dyn ConversationSink>) -> Self {
        Self {
            workers: Mutex::new(BTreeMap::new()),
            sink: Some(sink),
        }
    }

    /// Register a worker. Without registration every call fails with
    /// `NoSession`, matching [`super::mock::MockProtocol`]'s contract for a
    /// worker whose process is gone.
    pub fn register(&self, worker: WorkerId, config: WorkerConfig) {
        let (tx, _) = broadcast::channel(256);
        let conversation = config.conversation.clone();
        self.workers.lock().unwrap().insert(
            worker.clone(),
            Arc::new(WorkerShared {
                worker,
                config,
                events: tx,
                state: Mutex::new(AgentState::Idle),
                seen_keys: Mutex::new(HashSet::new()),
                child: Mutex::new(None),
                paused: std::sync::atomic::AtomicBool::new(false),
                reading: std::sync::atomic::AtomicBool::new(false),
                conversation: Mutex::new(conversation),
                sink: self.sink.clone(),
            }),
        );
    }

    /// Whether a worker has a live protocol session. Callers that adopt
    /// existing workers (backend::bootstrap) MUST check this before
    /// register(): re-registering replaces the WorkerShared, which would
    /// silently orphan a running turn's state.
    pub fn is_registered(&self, worker: &WorkerId) -> bool {
        self.workers.lock().unwrap().contains_key(worker)
    }

    fn shared(&self, worker: &WorkerId) -> Result<Arc<WorkerShared>> {
        self.workers
            .lock()
            .unwrap()
            .get(worker)
            .cloned()
            .ok_or_else(|| ProtocolError::NoSession(worker.to_string()))
    }

    /// The one spawn path for both prompts and idle-time message delivery.
    async fn run_prompt(&self, worker: &WorkerId, text: &str, key: String) -> Result<()> {
        let shared = self.shared(worker)?;

        // Dedupe + busy-check + spawn under the per-worker key lock so two
        // concurrent sends with the same key cannot both spawn. Nothing in
        // this block awaits.
        let (stdout, stderr, turn, resumed_from) = {
            let mut keys = shared.seen_keys.lock().unwrap();
            if shared.paused.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(ProtocolError::Rejected(format!(
                    "worker {worker} is paused"
                )));
            }
            if keys.contains(&key) {
                return Ok(()); // Invariant 9: redelivery must not double-run.
            }
            if matches!(*shared.state.lock().unwrap(), AgentState::Working { .. }) {
                return Err(ProtocolError::Rejected(format!(
                    "worker {worker} has a headless turn in progress; \
                     redeliver at the turn boundary"
                )));
            }

            let cfg = &shared.config;
            // Continuity: resume the conversation the last run reported
            // (or the hydrated one). Cloned here, and passed to the reader
            // so a failed resume can be attributed to exactly this ref.
            let resumed_from = shared.conversation.lock().unwrap().clone();
            let program: PathBuf = cfg
                .binary
                .clone()
                .unwrap_or_else(|| PathBuf::from(cfg.provider.binary()));
            let mut cmd = Command::new(&program);
            cmd.args(
                cfg.provider
                    .args(text, cfg.model.as_deref(), resumed_from.as_deref()),
            )
            .current_dir(&cfg.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
            for (k, v) in cfg.provider.envs() {
                cmd.env(k, v);
            }
            // ETXTBSY retry. `cmd.spawn()` of a JUST-WRITTEN executable can fail
            // with errno 26 ("text file busy") when a CONCURRENT fork elsewhere in
            // this multithreaded process momentarily inherited an open write-fd to
            // that file — the classic multithreaded-fork/exec race. It is exactly
            // why the opencode resume test flaked only on CI (Linux) and never on
            // macOS, red-maining a commit that never touched opencode and blocking
            // the cloud deploy (amux-cloud, 2026-08-13). It is transient by
            // construction (the other fork's exec drops the inherited fd), so a
            // bounded retry clears it. WARN each retry so a NON-transient prod case
            // (a genuinely busy binary) still surfaces in the logs (two-fixes rule).
            //
            // The wait is a SYNCHRONOUS `thread::sleep`, not an `.await`: this
            // whole block runs under the per-worker key lock and must not await
            // (see the block comment above). The sleep is tiny and this path is
            // rare — usually zero retries — so briefly blocking one worker's own
            // send is an acceptable cost for not restructuring the spawn lock.
            let mut child = {
                let mut tries = 0u32;
                loop {
                    match cmd.spawn() {
                        Ok(c) => break c,
                        Err(e) if e.raw_os_error() == Some(26) && tries < 25 => {
                            tries += 1;
                            tracing::warn!(
                                program = %program.display(),
                                tries,
                                "spawn hit ETXTBSY (text file busy) — retrying"
                            );
                            std::thread::sleep(std::time::Duration::from_millis(4));
                        }
                        Err(e) => {
                            return Err(ProtocolError::Transport(format!(
                                "spawn {}: {e}",
                                program.display()
                            )));
                        }
                    }
                }
            };
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| ProtocolError::Transport("child stdout not captured".to_string()))?;
            let stderr = child.stderr.take();
            *shared.child.lock().unwrap() = Some(child);
            shared
                .reading
                .store(true, std::sync::atomic::Ordering::SeqCst);

            let turn = TurnId::from_ulid(ulid::Ulid::new());
            shared.set_state(AgentState::Working {
                turn: Some(turn.clone()),
                progress: None,
            });
            keys.insert(key);
            (stdout, stderr, turn, resumed_from)
        };

        shared.emit(WorkerEvent::TraceObserved(TurnTrace {
            turn_id: turn.clone(),
            kind: TurnTraceKind::Prompt,
            content: text.to_string(),
        }));
        tokio::spawn(read_stream(shared, stdout, stderr, turn, resumed_from));
        Ok(())
    }
}

/// Keep the last few stderr lines so a nonzero exit is diagnosable from the
/// Failure it produces (ethos rule 4: never discard the only evidence).
async fn stderr_tail(stderr: Option<ChildStderr>) -> String {
    let Some(stderr) = stderr else {
        return String::new();
    };
    let mut lines = BufReader::new(stderr).lines();
    let mut tail: VecDeque<String> = VecDeque::with_capacity(5);
    while let Ok(Some(line)) = lines.next_line().await {
        if tail.len() == 5 {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    tail.into_iter().collect::<Vec<_>>().join(" | ")
}

/// Reader task: translate stdout lines into events, then reap the child and
/// settle the worker's state. `resumed_from` is the conversation ref this
/// run was spawned with (None = fresh) — see the module docs' continuity
/// section for the dead-target rule it feeds.
async fn read_stream(
    shared: Arc<WorkerShared>,
    stdout: ChildStdout,
    stderr: Option<ChildStderr>,
    turn: TurnId,
    resumed_from: Option<String>,
) {
    struct Reading(Arc<WorkerShared>);
    impl Drop for Reading {
        fn drop(&mut self) {
            self.0
                .reading
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let _reading = Reading(shared.clone());
    let stderr_task = tokio::spawn(stderr_tail(stderr));

    let mut lines = BufReader::new(stdout).lines();
    let mut turn_started = false;
    let mut turn_completed = false;
    let mut saw_conversation_ref = false;
    while let Ok(Some(line)) = lines.next_line().await {
        shared.emit(WorkerEvent::TraceObserved(TurnTrace {
            turn_id: turn.clone(),
            kind: TurnTraceKind::ProviderEvent,
            content: line.clone(),
        }));
        // Continuity capture, BEFORE translation (an init line is also a
        // Started event; the two concerns stay independent).
        if let Some(cid) = shared.config.provider.conversation_ref(&line) {
            saw_conversation_ref = true;
            let changed = {
                let mut cur = shared.conversation.lock().unwrap();
                if cur.as_deref() != Some(cid.as_str()) {
                    *cur = Some(cid.clone());
                    true
                } else {
                    false
                }
            };
            if changed {
                tracing::info!(worker = %shared.worker, conversation = %cid,
                    "captured conversation ref for continuity");
                if let Some(sink) = &shared.sink {
                    sink.save(&shared.worker, shared.config.provider.family(), &cid);
                }
            }
        }
        for ev in shared.config.provider.translate(&line, &turn) {
            match &ev {
                WorkerEvent::Started => {}
                WorkerEvent::TurnStarted { .. } => {
                    if turn_started {
                        continue; // provider marker after our insertion
                    }
                    turn_started = true;
                }
                _ => {
                    // Claude/Gemini have no explicit turn-start marker: the
                    // first in-turn event implies it (Invariant 6 — the turn
                    // is first-class, so its start must be an event).
                    if !turn_started {
                        turn_started = true;
                        shared.emit(WorkerEvent::TurnStarted {
                            turn_id: turn.clone(),
                        });
                    }
                }
            }
            match &ev {
                WorkerEvent::Progress(p) => shared.set_state(AgentState::Working {
                    turn: Some(turn.clone()),
                    progress: Some(p.clone()),
                }),
                WorkerEvent::TurnCompleted(r) => {
                    turn_completed = true;
                    // The one place a headless turn's outcome is visible
                    // server-side today (nothing durable subscribes to the
                    // broadcast yet) — log it, or the turn completes
                    // invisibly (ethos rule 4).
                    tracing::info!(
                        worker = %shared.worker,
                        turn = %r.turn_id,
                        outcome = %r.outcome,
                        "structured turn completed"
                    );
                    shared.set_state(AgentState::Idle);
                }
                _ => {}
            }
            shared.emit(ev);
        }
    }

    // stdout closed: reap the child for the real exit status.
    let child = shared.child.lock().unwrap().take();
    let status = match child {
        Some(mut c) => c.wait().await.ok(),
        None => None, // cancel() raced us and the child is being torn down
    };
    let tail = stderr_task.await.unwrap_or_default();
    if !tail.is_empty() {
        shared.emit(WorkerEvent::TraceObserved(TurnTrace {
            turn_id: turn.clone(),
            kind: TurnTraceKind::CommandOutput,
            content: tail.clone(),
        }));
    }

    if shared.paused.load(std::sync::atomic::Ordering::SeqCst) {
        // Intentional interruption is not evidence that the resume target died.
        // Keep its reference, and do not spend the task's failure budget.
        shared.set_state(AgentState::Paused);
        tracing::info!(worker = %shared.worker, verdict = "protocol_turn_paused", "structured turn stopped by worker pause");
        return;
    }

    match status {
        Some(st) if st.success() => {
            if !turn_completed {
                // Exit 0 with no result event (e.g. a clean SIGINT). The
                // turn still ENDED — leaving it open would hang the harness
                // on a boundary that already happened.
                if !turn_started {
                    shared.emit(WorkerEvent::TurnStarted {
                        turn_id: turn.clone(),
                    });
                }
                shared.emit(WorkerEvent::TurnCompleted(
                    amux_core::protocol::TurnResult {
                        turn_id: turn.clone(),
                        outcome: "stream ended without a result event".to_string(),
                    },
                ));
            }
            shared.set_state(AgentState::Idle);
        }
        Some(st) => {
            let code = st.code();
            #[cfg(unix)]
            let signal = std::os::unix::process::ExitStatusExt::signal(&st);
            #[cfg(not(unix))]
            let signal = None;
            let reason = if tail.is_empty() {
                format!("{} exited with {st}", shared.config.provider.binary())
            } else {
                format!(
                    "{} exited with {st}; stderr: {tail}",
                    shared.config.provider.binary()
                )
            };
            tracing::warn!(worker = %shared.worker, reason = %reason, "structured turn failed");
            // Dead resume target (module docs): the run was told to resume,
            // failed, and never emitted an init line — measured shape of a
            // vanished claude session (exit 1, error result only) and a
            // vanished gemini session (exit 42, empty stream). Drop the ref
            // so the next turn starts fresh instead of failing identically
            // forever. Guarded on the ref being UNCHANGED so a concurrent
            // capture is never clobbered. An API failure mid-turn has
            // already re-emitted init, so it never lands here.
            if let Some(from) = &resumed_from {
                if !saw_conversation_ref {
                    let mut cur = shared.conversation.lock().unwrap();
                    if cur.as_deref() == Some(from.as_str()) {
                        *cur = None;
                        tracing::warn!(worker = %shared.worker, conversation = %from,
                            "resume target appears gone; dropping conversation ref \
                             (next turn starts a fresh conversation)");
                        if let Some(sink) = &shared.sink {
                            sink.forget(&shared.worker);
                        }
                    }
                }
            }
            shared.emit(WorkerEvent::Failed(Failure {
                reason,
                retryable: true,
            }));
            shared.emit(WorkerEvent::Exited(ExitStatus { code, signal }));
            shared.set_state(AgentState::Exited { code });
        }
        None => {
            // We could not observe the exit (Invariant 20: never invent a
            // code that was not reported).
            shared.emit(WorkerEvent::Exited(ExitStatus {
                code: None,
                signal: None,
            }));
            shared.set_state(AgentState::Exited { code: None });
        }
    }
}

#[async_trait]
impl AgentProtocol for StructuredCliProtocol {
    async fn send_prompt(&self, worker: &WorkerId, prompt: Prompt) -> Result<()> {
        self.run_prompt(worker, &prompt.text, prompt.idempotency_key)
            .await
    }

    /// Deliver a durable message. Headless CLIs have no mid-turn stdin, so
    /// an idle worker gets the message as a new turn (keyed by the durable
    /// `MessageId`, so redelivery is idempotent) and a busy worker rejects —
    /// the queue's `AtTurnBoundary` timing retries at the boundary.
    async fn deliver_message(&self, worker: &WorkerId, msg: MessageId, body: String) -> Result<()> {
        self.run_prompt(worker, &body, msg.as_str().to_string())
            .await
    }

    /// Graceful SIGINT via `/bin/kill`; hard-kill fallback. A cancel with
    /// nothing running is Ok — the absence of a turn IS the cancelled state.
    async fn cancel(&self, worker: &WorkerId) -> Result<()> {
        let shared = self.shared(worker)?;
        let pid = shared.child.lock().unwrap().as_ref().and_then(|c| c.id());
        let Some(pid) = pid else {
            return Ok(());
        };
        let sigint_ok = matches!(
            Command::new("kill")
                .args(["-INT", &pid.to_string()])
                .status()
                .await,
            Ok(st) if st.success()
        );
        if !sigint_ok {
            if let Some(child) = shared.child.lock().unwrap().as_mut() {
                child
                    .start_kill()
                    .map_err(|e| ProtocolError::Transport(format!("kill: {e}")))?;
            }
        }
        Ok(())
    }

    /// Pause terminates current work and retains the conversation reference.
    async fn pause(&self, worker: &WorkerId) -> Result<()> {
        let shared = self.shared(worker)?;
        let pid = {
            let _keys = shared.seen_keys.lock().unwrap();
            shared
                .paused
                .store(true, std::sync::atomic::Ordering::SeqCst);
            shared.child.lock().unwrap().as_ref().and_then(|c| c.id())
        };
        shared.set_state(AgentState::Paused);
        if let Some(pid) = pid {
            crate::api::session_verbs::terminate_owned_tree(pid as i32, true)
                .await
                .map_err(|e| ProtocolError::Transport(e.to_string()))?;
            // The stream reader owns reaping. Do not acknowledge until the
            // previous turn's process is gone, even if EOF raced the kill.
            for _ in 0..100 {
                if !shared.reading.load(std::sync::atomic::Ordering::SeqCst)
                    && unsafe { libc::kill(pid as i32, 0) } != 0
                {
                    tracing::info!(worker = %worker, verdict = "protocol_pause_stopped", "paused structured worker process stopped");
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            return Err(ProtocolError::Transport(
                "could not confirm paused protocol process stopped".into(),
            ));
        }
        Ok(())
    }

    async fn resume(&self, worker: &WorkerId) -> Result<()> {
        let shared = self.shared(worker)?;
        let _keys = shared.seen_keys.lock().unwrap();
        if shared.reading.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ProtocolError::Rejected(
                "previous turn is still stopping".into(),
            ));
        }
        shared
            .paused
            .store(false, std::sync::atomic::Ordering::SeqCst);
        shared.set_state(AgentState::Idle);
        Ok(())
    }

    async fn state(&self, worker: &WorkerId) -> Result<AgentState> {
        Ok(self.shared(worker)?.state.lock().unwrap().clone())
    }

    fn events(&self, worker: &WorkerId) -> broadcast::Receiver<WorkerEvent> {
        match self.workers.lock().unwrap().get(worker) {
            Some(shared) => shared.events.subscribe(),
            None => {
                // Same contract as MockProtocol: a dead worker yields a
                // closed channel, so the subscriber sees Closed immediately
                // instead of hanging.
                let (tx, rx) = broadcast::channel(1);
                drop(tx);
                rx
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Conformance tests (RR-0030). The fake-binary tests replay the REAL
// captured provider lines from events.rs through the full spawn -> read ->
// translate -> broadcast path, so they exercise the shipped code path (ethos
// rule 7) without needing provider auth. The real-claude integration test is
// #[ignore]d for CI (no claude auth there); run it locally with:
//   cargo test -p amux-server opencode -- --ignored
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    fn worker_id(tag: &str) -> WorkerId {
        // Distinct fixed ids per test for determinism.
        let base = "01JGXV000000000000000";
        WorkerId::from_ulid(format!("{base}{tag}").parse().unwrap())
    }

    /// Write an executable script that logs each run (for idempotency
    /// assertions), records its argv (for continuity assertions — one
    /// NUL-joined line per run in `<name>.argv`), and replays a fixture
    /// stream on stdout.
    fn fixture_script(
        dir: &std::path::Path,
        name: &str,
        lines: &[&str],
        exit_code: i32,
    ) -> PathBuf {
        let path = dir.join(name);
        let runs = dir.join(format!("{name}.runs"));
        let argv = dir.join(format!("{name}.argv"));
        let mut body = String::from("#!/bin/sh\n");
        body.push_str(&format!("printf x >> '{}'\n", runs.display()));
        body.push_str(&format!(
            "{{ printf '%s\\000' \"$@\"; printf '\\n'; }} >> '{}'\n",
            argv.display()
        ));
        body.push_str("cat <<'FIXTURE_EOF'\n");
        for line in lines {
            body.push_str(line);
            body.push('\n');
        }
        body.push_str("FIXTURE_EOF\n");
        body.push_str(&format!("exit {exit_code}\n"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn run_count(dir: &std::path::Path, name: &str) -> usize {
        std::fs::read(dir.join(format!("{name}.runs")))
            .map(|b| b.len())
            .unwrap_or(0)
    }

    /// The argv of each recorded run, in order.
    fn recorded_argvs(dir: &std::path::Path, name: &str) -> Vec<Vec<String>> {
        let raw = std::fs::read_to_string(dir.join(format!("{name}.argv"))).unwrap_or_default();
        raw.lines()
            .map(|l| {
                l.split('\0')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .collect()
    }

    async fn collect_until_terminal(
        rx: &mut broadcast::Receiver<WorkerEvent>,
        deadline: Duration,
    ) -> Vec<WorkerEvent> {
        let mut out = Vec::new();
        let end = tokio::time::Instant::now() + deadline;
        loop {
            let now = tokio::time::Instant::now();
            if now >= end {
                break;
            }
            match tokio::time::timeout(end - now, rx.recv()).await {
                Ok(Ok(ev)) => {
                    let terminal =
                        matches!(ev, WorkerEvent::TurnCompleted(_) | WorkerEvent::Exited(_));
                    out.push(ev);
                    if terminal {
                        break;
                    }
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Err(_) => break,
            }
        }
        out
    }

    async fn wait_for_state(
        proto: &StructuredCliProtocol,
        worker: &WorkerId,
        pred: impl Fn(&AgentState) -> bool,
    ) -> AgentState {
        for _ in 0..100 {
            let s = proto.state(worker).await.unwrap();
            if pred(&s) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        proto.state(worker).await.unwrap()
    }

    fn prompt(key: &str) -> Prompt {
        Prompt {
            text: "say hi".to_string(),
            idempotency_key: key.to_string(),
        }
    }

    fn kind(ev: &WorkerEvent) -> &'static str {
        match ev {
            WorkerEvent::Started => "started",
            WorkerEvent::TurnStarted { .. } => "turn_started",
            WorkerEvent::Progress(_) => "progress",
            WorkerEvent::Waiting(_) => "waiting",
            WorkerEvent::ToolUsed(_) => "tool_used",
            WorkerEvent::TraceObserved(_) => "trace_observed",
            WorkerEvent::TaskUpdated(_) => "task_updated",
            WorkerEvent::TurnCompleted(_) => "turn_completed",
            WorkerEvent::RateLimited(_) => "rate_limited",
            WorkerEvent::ContextLow(_) => "context_low",
            WorkerEvent::Failed(_) => "failed",
            WorkerEvent::Exited(_) => "exited",
        }
    }

    fn lifecycle_kinds(events: &[WorkerEvent]) -> Vec<&'static str> {
        events
            .iter()
            .filter(|event| !matches!(event, WorkerEvent::TraceObserved(_)))
            .map(kind)
            .collect()
    }

    fn trace_count(events: &[WorkerEvent]) -> usize {
        events
            .iter()
            .filter(|event| matches!(event, WorkerEvent::TraceObserved(_)))
            .count()
    }

    #[test]
    fn model_flag_rides_into_argv_only_when_configured() {
        for provider in [
            CliProvider::ClaudeCode,
            CliProvider::GeminiCli,
            CliProvider::CodexCli,
        ] {
            let with = provider.args("hi", Some("haiku"), None);
            assert!(
                with.windows(2).any(|w| w == ["--model", "haiku"]),
                "{provider:?}: missing --model haiku in {with:?}"
            );
            let without = provider.args("hi", None, None);
            assert!(
                !without.iter().any(|a| a == "--model"),
                "{provider:?}: invented a model in {without:?}"
            );
        }
    }

    #[test]
    fn resume_ref_rides_into_argv_in_each_providers_own_shape() {
        // claude + gemini: a --resume flag (both verified live 2026-08-09).
        for provider in [CliProvider::ClaudeCode, CliProvider::GeminiCli] {
            let with = provider.args("hi", None, Some("abc-123"));
            assert!(
                with.windows(2).any(|w| w == ["--resume", "abc-123"]),
                "{provider:?}: missing --resume in {with:?}"
            );
            let fresh = provider.args("hi", None, None);
            assert!(
                !fresh.iter().any(|a| a == "--resume"),
                "{provider:?}: invented a resume target in {fresh:?}"
            );
        }
        // codex: continuation is the `exec resume <id> <prompt>` subcommand
        // (argv shape verified live against codex 0.141.0 — thread.started
        // re-emitted the resumed thread id).
        let with = CliProvider::CodexCli.args("hi", None, Some("thr-1"));
        assert_eq!(
            with,
            vec![
                "exec",
                "resume",
                "--json",
                "--skip-git-repo-check",
                "thr-1",
                "hi"
            ]
        );
        let fresh = CliProvider::CodexCli.args("hi", None, None);
        assert_eq!(fresh, vec!["exec", "--json", "--skip-git-repo-check", "hi"]);
    }

    #[test]
    fn conversation_refs_come_only_from_init_shaped_lines() {
        // The three real init shapes carry their ids.
        assert_eq!(
            CliProvider::ClaudeCode.conversation_ref(events::tests::CLAUDE_INIT),
            Some("b7e8023e-530b-47ac-8515-a4e181bb7d58".to_string())
        );
        assert_eq!(
            CliProvider::CodexCli.conversation_ref(events::tests::CODEX_THREAD_STARTED),
            Some("019fe715-0c25-72c3-878b-7728e09afc96".to_string())
        );
        assert_eq!(
            CliProvider::GeminiCli.conversation_ref(events::tests::GEMINI_INIT),
            Some("d1f03c4d-470b-4894-a540-3fd31e12be65".to_string())
        );
        // The bad-resume error result ALSO carries a session_id (the dead
        // one — measured live); it must NOT be captured, or the dead ref
        // would re-arm itself on every failure.
        let bad_resume_result = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":0,"session_id":"00000000-0000-0000-0000-000000000000","errors":["No conversation found with session ID: 00000000-0000-0000-0000-000000000000"]}"#;
        assert_eq!(
            CliProvider::ClaudeCode.conversation_ref(bad_resume_result),
            None
        );
        // Cross-provider lines do not cross-match.
        assert_eq!(
            CliProvider::ClaudeCode.conversation_ref(events::tests::GEMINI_INIT),
            None
        );
    }

    #[test]
    fn provider_id_mapping_covers_worker_row_spellings() {
        // "claude" is the workers-table DEFAULT (migration 0003); a mapping
        // that only knows "claude-code" strands every default-created worker.
        assert_eq!(
            CliProvider::from_provider_id("claude"),
            Some(CliProvider::ClaudeCode)
        );
        assert_eq!(
            CliProvider::from_provider_id("claude-code"),
            Some(CliProvider::ClaudeCode)
        );
        assert_eq!(
            CliProvider::from_provider_id("gemini"),
            Some(CliProvider::GeminiCli)
        );
        assert_eq!(
            CliProvider::from_provider_id("codex"),
            Some(CliProvider::CodexCli)
        );
        // ollama workers now run codex --oss --local-provider ollama, so they
        // are handled by the same structured protocol as codex.
        assert_eq!(
            CliProvider::from_provider_id("ollama"),
            Some(CliProvider::CodexCli)
        );
        // A genuinely unknown provider still returns None.
        assert_eq!(CliProvider::from_provider_id("iterm2"), None);
    }

    #[tokio::test]
    async fn codex_shaped_stream_full_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(
            dir.path(),
            "fake-codex",
            &[
                events::tests::CODEX_THREAD_STARTED,
                events::tests::CODEX_TURN_STARTED,
                events::tests::CODEX_AGENT_MESSAGE,
                events::tests::CODEX_TURN_COMPLETED,
            ],
            0,
        );
        let proto = StructuredCliProtocol::new();
        let w = worker_id("00001");
        proto.register(
            w.clone(),
            WorkerConfig {
                provider: CliProvider::CodexCli,
                cwd: dir.path().to_path_buf(),
                binary: Some(script),
                model: None,
                conversation: None,
            },
        );
        let mut rx = proto.events(&w);
        proto.send_prompt(&w, prompt("k1")).await.unwrap();

        let evs = collect_until_terminal(&mut rx, Duration::from_secs(10)).await;
        let kinds = lifecycle_kinds(&evs);
        assert_eq!(
            kinds,
            vec!["started", "turn_started", "progress", "turn_completed"],
            "{evs:?}"
        );
        assert_eq!(trace_count(&evs), 5, "prompt plus every provider line");
        let settled = wait_for_state(&proto, &w, |s| *s == AgentState::Idle).await;
        assert_eq!(settled, AgentState::Idle);
    }

    #[tokio::test]
    async fn turn_started_inserted_for_streams_without_marker() {
        // Gemini's stream has no turn-start line; the reader must insert it
        // before the first in-turn event (Invariant 6).
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(
            dir.path(),
            "fake-gemini",
            &[
                events::tests::GEMINI_INIT,
                events::tests::GEMINI_ASSISTANT_DELTA,
                events::tests::GEMINI_RESULT,
            ],
            0,
        );
        let proto = StructuredCliProtocol::new();
        let w = worker_id("00002");
        proto.register(
            w.clone(),
            WorkerConfig {
                provider: CliProvider::GeminiCli,
                cwd: dir.path().to_path_buf(),
                binary: Some(script),
                model: None,
                conversation: None,
            },
        );
        let mut rx = proto.events(&w);
        proto.send_prompt(&w, prompt("k1")).await.unwrap();

        let evs = collect_until_terminal(&mut rx, Duration::from_secs(10)).await;
        let kinds = lifecycle_kinds(&evs);
        assert_eq!(
            kinds,
            vec![
                "started",
                "turn_started",
                "progress", // assistant delta
                "progress", // final token accounting
                "turn_completed"
            ],
            "{evs:?}"
        );
        assert_eq!(trace_count(&evs), 4, "prompt plus every provider line");
    }

    #[tokio::test]
    async fn claude_shaped_stream_translates_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(
            dir.path(),
            "fake-claude",
            &[
                events::tests::CLAUDE_INIT,
                events::tests::CLAUDE_TEXT,
                events::tests::CLAUDE_RESULT,
            ],
            0,
        );
        let proto = StructuredCliProtocol::new();
        let w = worker_id("00003");
        proto.register(
            w.clone(),
            WorkerConfig {
                provider: CliProvider::ClaudeCode,
                cwd: dir.path().to_path_buf(),
                binary: Some(script),
                model: None,
                conversation: None,
            },
        );
        let mut rx = proto.events(&w);
        proto.send_prompt(&w, prompt("k1")).await.unwrap();

        let evs = collect_until_terminal(&mut rx, Duration::from_secs(10)).await;
        let kinds = lifecycle_kinds(&evs);
        assert_eq!(
            kinds,
            vec!["started", "turn_started", "progress", "turn_completed"],
            "{evs:?}"
        );
        assert_eq!(trace_count(&evs), 4, "prompt plus every provider line");
        match evs.last().unwrap() {
            WorkerEvent::TurnCompleted(r) => assert_eq!(r.outcome, "success: done"),
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn idempotent_send_prompt_does_not_respawn() {
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(
            dir.path(),
            "fake-codex",
            &[
                events::tests::CODEX_THREAD_STARTED,
                events::tests::CODEX_TURN_STARTED,
                events::tests::CODEX_TURN_COMPLETED,
            ],
            0,
        );
        let proto = StructuredCliProtocol::new();
        let w = worker_id("00004");
        proto.register(
            w.clone(),
            WorkerConfig {
                provider: CliProvider::CodexCli,
                cwd: dir.path().to_path_buf(),
                binary: Some(script),
                model: None,
                conversation: None,
            },
        );
        let mut rx = proto.events(&w);
        proto.send_prompt(&w, prompt("dup")).await.unwrap();
        collect_until_terminal(&mut rx, Duration::from_secs(10)).await;
        wait_for_state(&proto, &w, |s| *s == AgentState::Idle).await;
        assert_eq!(run_count(dir.path(), "fake-codex"), 1);

        // Same key again: Ok, no second run (Invariant 9).
        proto.send_prompt(&w, prompt("dup")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(run_count(dir.path(), "fake-codex"), 1);

        // A fresh key runs again.
        let mut rx2 = proto.events(&w);
        proto.send_prompt(&w, prompt("fresh")).await.unwrap();
        collect_until_terminal(&mut rx2, Duration::from_secs(10)).await;
        wait_for_state(&proto, &w, |s| *s == AgentState::Idle).await;
        assert_eq!(run_count(dir.path(), "fake-codex"), 2);
    }

    #[tokio::test]
    async fn nonzero_exit_emits_failed_then_exited() {
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(
            dir.path(),
            "fake-broken",
            &[events::tests::CODEX_THREAD_STARTED],
            3,
        );
        let proto = StructuredCliProtocol::new();
        let w = worker_id("00005");
        proto.register(
            w.clone(),
            WorkerConfig {
                provider: CliProvider::CodexCli,
                cwd: dir.path().to_path_buf(),
                binary: Some(script),
                model: None,
                conversation: None,
            },
        );
        let mut rx = proto.events(&w);
        proto.send_prompt(&w, prompt("k1")).await.unwrap();

        let evs = collect_until_terminal(&mut rx, Duration::from_secs(10)).await;
        let kinds = lifecycle_kinds(&evs);
        assert_eq!(kinds, vec!["started", "failed", "exited"], "{evs:?}");
        assert_eq!(trace_count(&evs), 2, "prompt plus the provider line");
        match evs.last().unwrap() {
            WorkerEvent::Exited(st) => assert_eq!(st.code, Some(3)),
            other => panic!("expected Exited, got {other:?}"),
        }
        let settled = wait_for_state(&proto, &w, |s| matches!(s, AgentState::Exited { .. })).await;
        assert_eq!(settled, AgentState::Exited { code: Some(3) });
    }

    #[tokio::test]
    async fn pause_and_resume_stop_headless_work_and_preserve_conversation() {
        use std::os::unix::fs::PermissionsExt;
        for (i, provider) in [
            CliProvider::ClaudeCode,
            CliProvider::CodexCli,
            CliProvider::GeminiCli,
        ]
        .into_iter()
        .enumerate()
        {
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("provider");
            std::fs::write(
                &script,
                "#!/bin/sh\nsleep 120 &\necho $! > tool.pid.tmp\nmv tool.pid.tmp tool.pid\nwait\n",
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
            let proto = StructuredCliProtocol::new();
            let w = worker_id(&format!("{:05}", i + 60));
            proto.register(
                w.clone(),
                WorkerConfig {
                    provider,
                    cwd: dir.path().into(),
                    binary: Some(script),
                    model: None,
                    conversation: Some("saved-conversation".into()),
                },
            );
            proto.send_prompt(&w, prompt("before-pause")).await.unwrap();
            // Wait for the fixture's atomic readiness signal, not a one-second
            // host scheduling assumption. Always stop its process group before
            // reporting failure, so a failed readiness check cannot leak work.
            let tool = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match std::fs::read_to_string(dir.path().join("tool.pid")) {
                        Ok(pid) => break Ok(pid),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => break Err(e),
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            proto.pause(&w).await.unwrap();
            let tool = tool
                .expect("fake provider did not publish tool readiness within 10s")
                .expect("fake provider readiness could not be read");
            assert_eq!(proto.state(&w).await.unwrap(), AgentState::Paused);
            assert!(matches!(
                proto.send_prompt(&w, prompt("while-paused")).await,
                Err(ProtocolError::Rejected(_))
            ));
            let status = Command::new("ps")
                .args(["-p", tool.trim(), "-o", "stat="])
                .output()
                .await
                .unwrap();
            let stat = String::from_utf8_lossy(&status.stdout);
            assert!(
                stat.trim().is_empty() || stat.trim().starts_with('Z'),
                "tool survived pause: {stat}"
            );
            assert_eq!(
                proto
                    .shared(&w)
                    .unwrap()
                    .conversation
                    .lock()
                    .unwrap()
                    .as_deref(),
                Some("saved-conversation")
            );
            proto.resume(&w).await.unwrap();
            assert_eq!(proto.state(&w).await.unwrap(), AgentState::Idle);
            // A rejected prompt did not consume the key; it can be sent after Resume.
            proto.send_prompt(&w, prompt("while-paused")).await.unwrap();
            proto.pause(&w).await.unwrap();
        }
        let proto = StructuredCliProtocol::new();
        assert!(matches!(
            proto.pause(&worker_id("00007")).await,
            Err(ProtocolError::NoSession(_))
        ));
    }

    #[tokio::test]
    async fn unregistered_worker_is_no_session_and_closed_channel() {
        let proto = StructuredCliProtocol::new();
        let ghost = worker_id("00008");
        assert!(matches!(
            proto.send_prompt(&ghost, prompt("k")).await,
            Err(ProtocolError::NoSession(_))
        ));
        assert!(matches!(
            proto.state(&ghost).await,
            Err(ProtocolError::NoSession(_))
        ));
        assert!(matches!(
            proto.cancel(&ghost).await,
            Err(ProtocolError::NoSession(_))
        ));
        let mut rx = proto.events(&ghost);
        assert!(matches!(
            rx.recv().await,
            Err(broadcast::error::RecvError::Closed)
        ));
    }

    #[tokio::test]
    async fn cancel_with_nothing_running_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let proto = StructuredCliProtocol::new();
        let w = worker_id("00009");
        proto.register(
            w.clone(),
            WorkerConfig {
                provider: CliProvider::CodexCli,
                cwd: dir.path().to_path_buf(),
                binary: None,
                model: None,
                conversation: None,
            },
        );
        proto.cancel(&w).await.unwrap();
    }

    // ---- conversation continuity (AMUX-2613 gap 2) ------------------------

    #[derive(Default)]
    struct RecordingSink {
        saves: Mutex<Vec<(WorkerId, String, String)>>,
        forgets: Mutex<Vec<WorkerId>>,
    }

    impl ConversationSink for RecordingSink {
        fn save(&self, worker: &WorkerId, family: &str, cref: &str) {
            self.saves
                .lock()
                .unwrap()
                .push((worker.clone(), family.to_string(), cref.to_string()));
        }
        fn forget(&self, worker: &WorkerId) {
            self.forgets.lock().unwrap().push(worker.clone());
        }
    }

    /// Turn 2 must resume the conversation turn 1's stream reported —
    /// pre-fix, every turn was a fresh fork+exec and this finds no
    /// `--resume` in run 2's argv. Also pins the sink persistence call.
    #[tokio::test]
    async fn second_run_resumes_the_conversation_the_first_run_reported() {
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(
            dir.path(),
            "fake-claude",
            &[
                events::tests::CLAUDE_INIT, // session_id b7e8023e-…
                events::tests::CLAUDE_TEXT,
                events::tests::CLAUDE_RESULT,
            ],
            0,
        );
        let sink = Arc::new(RecordingSink::default());
        let proto = StructuredCliProtocol::with_conversation_sink(sink.clone());
        let w = worker_id("0000B");
        proto.register(
            w.clone(),
            WorkerConfig {
                provider: CliProvider::ClaudeCode,
                cwd: dir.path().to_path_buf(),
                binary: Some(script),
                model: None,
                conversation: None,
            },
        );
        let mut rx = proto.events(&w);
        proto.send_prompt(&w, prompt("t1")).await.unwrap();
        collect_until_terminal(&mut rx, Duration::from_secs(10)).await;
        wait_for_state(&proto, &w, |s| *s == AgentState::Idle).await;

        let mut rx2 = proto.events(&w);
        proto.send_prompt(&w, prompt("t2")).await.unwrap();
        collect_until_terminal(&mut rx2, Duration::from_secs(10)).await;
        wait_for_state(&proto, &w, |s| *s == AgentState::Idle).await;

        let argvs = recorded_argvs(dir.path(), "fake-claude");
        assert_eq!(argvs.len(), 2, "{argvs:?}");
        assert!(
            !argvs[0].iter().any(|a| a == "--resume"),
            "turn 1 must start fresh: {:?}",
            argvs[0]
        );
        assert!(
            argvs[1]
                .windows(2)
                .any(|p| p == ["--resume", "b7e8023e-530b-47ac-8515-a4e181bb7d58"]),
            "turn 2 must resume turn 1's session: {:?}",
            argvs[1]
        );
        // The ref was persisted once (unchanged on turn 2's re-observation),
        // under the canonical family name.
        let saves = sink.saves.lock().unwrap();
        assert_eq!(saves.len(), 1, "{saves:?}");
        assert_eq!(saves[0].1, "claude-code");
        assert_eq!(saves[0].2, "b7e8023e-530b-47ac-8515-a4e181bb7d58");
    }

    /// A hydrated ref (config.conversation, from `_amux_conversations`)
    /// rides into the FIRST run's argv — continuity across server restarts.
    #[tokio::test]
    async fn hydrated_conversation_rides_into_the_first_run() {
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(
            dir.path(),
            "fake-claude",
            &[events::tests::CLAUDE_INIT, events::tests::CLAUDE_RESULT],
            0,
        );
        let proto = StructuredCliProtocol::new();
        let w = worker_id("0000C");
        proto.register(
            w.clone(),
            WorkerConfig {
                provider: CliProvider::ClaudeCode,
                cwd: dir.path().to_path_buf(),
                binary: Some(script),
                model: None,
                conversation: Some("persisted-ref-1".into()),
            },
        );
        let mut rx = proto.events(&w);
        proto.send_prompt(&w, prompt("t1")).await.unwrap();
        collect_until_terminal(&mut rx, Duration::from_secs(10)).await;
        let argvs = recorded_argvs(dir.path(), "fake-claude");
        assert!(
            argvs[0]
                .windows(2)
                .any(|p| p == ["--resume", "persisted-ref-1"]),
            "{argvs:?}"
        );
    }

    /// The dead-target rule: a resumed run that fails without emitting an
    /// init line drops the ref (memory + sink) so the next turn starts
    /// fresh — the measured shape of `claude --resume <vanished id>`
    /// (exit 1, error result only). Without this, every subsequent turn
    /// fails identically forever.
    #[tokio::test]
    async fn failed_resume_without_init_drops_the_dead_ref() {
        let dir = tempfile::tempdir().unwrap();
        // The measured bad-resume stream: ONE error result line, exit 1.
        let script = fixture_script(
            dir.path(),
            "fake-claude",
            &[
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":0,"session_id":"dead-ref-9","errors":["No conversation found with session ID: dead-ref-9"]}"#,
            ],
            1,
        );
        let sink = Arc::new(RecordingSink::default());
        let proto = StructuredCliProtocol::with_conversation_sink(sink.clone());
        let w = worker_id("0000D");
        proto.register(
            w.clone(),
            WorkerConfig {
                provider: CliProvider::ClaudeCode,
                cwd: dir.path().to_path_buf(),
                binary: Some(script),
                model: None,
                conversation: Some("dead-ref-9".into()),
            },
        );
        let mut rx = proto.events(&w);
        proto.send_prompt(&w, prompt("t1")).await.unwrap();
        // The error result line closes the turn (collection stops there);
        // the exit-1 consequences (Failed/Exited + the ref drop) land right
        // after, observed via the settled state.
        let evs = collect_until_terminal(&mut rx, Duration::from_secs(10)).await;
        assert!(
            evs.iter()
                .any(|e| matches!(e, WorkerEvent::TurnCompleted(_))),
            "{evs:?}"
        );
        let settled = wait_for_state(&proto, &w, |s| matches!(s, AgentState::Exited { .. })).await;
        assert!(
            matches!(settled, AgentState::Exited { code: Some(1) }),
            "{settled:?}"
        );

        let argvs = recorded_argvs(dir.path(), "fake-claude");
        assert!(
            argvs[0].windows(2).any(|p| p == ["--resume", "dead-ref-9"]),
            "run 1 tried the dead ref: {argvs:?}"
        );
        assert_eq!(
            *sink.forgets.lock().unwrap(),
            vec![w.clone()],
            "the dead ref must be forgotten durably"
        );
        assert!(
            sink.saves.lock().unwrap().is_empty(),
            "nothing re-captured the dead id"
        );
        // A fresh key runs again — WITHOUT the dead ref.
        let mut rx2 = proto.events(&w);
        proto.send_prompt(&w, prompt("t2")).await.unwrap();
        collect_until_terminal(&mut rx2, Duration::from_secs(10)).await;
        let argvs = recorded_argvs(dir.path(), "fake-claude");
        assert_eq!(argvs.len(), 2, "{argvs:?}");
        assert!(
            !argvs[1].iter().any(|a| a == "--resume"),
            "turn 2 must start fresh after the drop: {:?}",
            argvs[1]
        );
    }

    /// THE continuity acceptance (AMUX-2613): two real claude turns, and
    /// turn 2 must remember turn 1's word — the difference between a
    /// protocol and a fork+exec. #[ignore]d for CI (no claude auth there);
    /// locally:
    ///   cargo test -p amux-server pomegranate -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires installed+authenticated claude CLI; run with -- --ignored"]
    async fn real_claude_conversation_remembers_pomegranate() {
        let have_claude = std::process::Command::new("which")
            .arg("claude")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !have_claude {
            eprintln!("skipping: claude not on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let proto = StructuredCliProtocol::new();
        let w = worker_id("0000E");
        proto.register(
            w.clone(),
            WorkerConfig {
                provider: CliProvider::ClaudeCode,
                cwd: dir.path().to_path_buf(),
                binary: None,
                model: Some("haiku".into()),
                conversation: None,
            },
        );

        let mut rx = proto.events(&w);
        proto
            .send_prompt(
                &w,
                Prompt {
                    text: "Remember the word: pomegranate. Reply with just: OK".into(),
                    idempotency_key: "pom-1".into(),
                },
            )
            .await
            .unwrap();
        let t1 = collect_until_terminal(&mut rx, Duration::from_secs(120)).await;
        assert!(
            t1.iter()
                .any(|e| matches!(e, WorkerEvent::TurnCompleted(_))),
            "turn 1 did not complete: {t1:?}"
        );
        wait_for_state(&proto, &w, |s| *s == AgentState::Idle).await;

        let mut rx2 = proto.events(&w);
        proto
            .send_prompt(
                &w,
                Prompt {
                    text: "What word did I ask you to remember? Reply with just that word.".into(),
                    idempotency_key: "pom-2".into(),
                },
            )
            .await
            .unwrap();
        let t2 = collect_until_terminal(&mut rx2, Duration::from_secs(120)).await;
        let outcome = t2
            .iter()
            .find_map(|e| match e {
                WorkerEvent::TurnCompleted(r) => Some(r.outcome.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("turn 2 did not complete: {t2:?}"));
        eprintln!("turn 2 outcome: {outcome}");
        assert!(
            outcome.to_lowercase().contains("pomegranate"),
            "turn 2 forgot the word — continuity is broken: {outcome:?}"
        );
    }

    /// Real end-to-end run against the installed claude CLI. #[ignore]d
    /// because CI has no claude binary or auth; locally:
    ///   cargo test -p amux-server opencode -- --ignored
    #[tokio::test]
    #[ignore = "requires installed+authenticated claude CLI; run with -- --ignored"]
    async fn real_claude_turn_end_to_end() {
        // Gate on `which claude` so an --ignored sweep on a claude-less
        // machine skips instead of failing on spawn.
        let have_claude = std::process::Command::new("which")
            .arg("claude")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !have_claude {
            eprintln!("skipping: claude not on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let proto = StructuredCliProtocol::new();
        let w = worker_id("0000A");
        proto.register(
            w.clone(),
            WorkerConfig {
                provider: CliProvider::ClaudeCode,
                cwd: dir.path().to_path_buf(),
                binary: None,
                model: None,
                conversation: None,
            },
        );
        let mut rx = proto.events(&w);
        proto
            .send_prompt(
                &w,
                Prompt {
                    text: "Reply with exactly: ok".to_string(),
                    idempotency_key: "real-claude-1".to_string(),
                },
            )
            .await
            .unwrap();

        let evs = collect_until_terminal(&mut rx, Duration::from_secs(120)).await;
        let kinds: Vec<_> = evs.iter().map(kind).collect();
        let pos = |k: &str| kinds.iter().position(|x| *x == k);
        let (started, turn_started, turn_completed) =
            (pos("started"), pos("turn_started"), pos("turn_completed"));
        assert!(started.is_some(), "no Started in {kinds:?}");
        assert!(turn_started.is_some(), "no TurnStarted in {kinds:?}");
        assert!(
            turn_completed.is_some(),
            "no TurnCompleted within 120s: {kinds:?}"
        );
        assert!(
            started < turn_started && turn_started < turn_completed,
            "{kinds:?}"
        );
        let settled = wait_for_state(&proto, &w, |s| *s == AgentState::Idle).await;
        assert_eq!(settled, AgentState::Idle);
    }
}
