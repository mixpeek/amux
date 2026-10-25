//! ACP transport for OpenCode (M2 of the opencode-provider plan).
//!
//! WHY THIS EXISTS. M1 drives OpenCode through `opencode run --format json`:
//! ONE process per turn, a flat JSONL stream, and no way for amux to say
//! anything to the agent while a turn is running. Measured against
//! `opencode 2.0.32` (2026-10-17), `opencode acp` speaks the Agent Client
//! Protocol — JSON-RPC 2.0 over stdio — and gives amux three things M1 cannot:
//!
//! 1. **Tool-level events.** `session/update` carries `tool_call` and
//!    `tool_call_update` with the tool name, `kind`, `locations`, the full
//!    `rawInput`, the tool's `content`, and — for edits — a `diff` block with
//!    `oldText`/`newText`. M1 saw only `text` and `error`.
//! 2. **Real permission asks.** When the worker's own opencode config sets a
//!    tool to `ask`, the agent sends a `session/request_permission` REQUEST
//!    (`options`: `once` / `always` / `reject`) and blocks until amux answers.
//!    M1 had to pre-approve everything with `--auto` — an invisible blanket
//!    consent. Here the ask is visible, named, and decided once per call.
//! 3. **Usage and cost.** Every turn ends with `usage_update`
//!    (`used`/`size`/`cost`) and a prompt result carrying
//!    `inputTokens`/`outputTokens`/`thoughtTokens`/`cachedReadTokens`.
//!
//! Measured limits (kept honest, not hidden — see the progress file):
//!
//! - **No mid-turn delivery.** A second `session/prompt` while a turn is
//!   running is rejected: `-32603 Session already has an active ACP prompt`.
//!   amux's contract is unchanged: a busy worker rejects and the queue's
//!   `AtTurnBoundary` timing retries. True steering needs `opencode serve`.
//! - `session/cancel` is a NOTIFICATION (no `id`); sent as a request the agent
//!   answers `-32601 Method not found`.
//! - There is no `session/set_model`; the model is a config option
//!   (`session/set_config_option`, `configId: "model"`, value `provider/model`).
//!
//! The transport is provider-agnostic on purpose: nothing here is
//! OpenCode-specific except the launch argv, so ACP for gemini/claude (a
//! natively-ACP and an adapter-backed provider, see docs/acp-spike-results.md)
//! is a second `AcpStart`, not a second client.

use amux_core::ids::TurnId;
use amux_core::protocol::{ProgressReport, ToolEvent, TurnResult, WorkerEvent};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::events::{summarize, tool_detail};
use super::{ProtocolError, Result};

/// ACP protocol version this client requests; the agent answers its own
/// (measured: `1`).
pub const PROTOCOL_VERSION: i64 = 1;

/// How long a handshake step may take. A hung handshake must fail loudly, not
/// wait forever (host-resolvable operational failures never become permanent
/// waits).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// A turn gets a generous ceiling — a long task is legitimate — but never
/// unbounded: a wedged agent becomes `Failed { retryable: true }` plus a WARN,
/// which the harness can act on, instead of a silent forever-pending turn.
pub const DEFAULT_TURN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

// ---------------------------------------------------------------------------
// Approval policy
// ---------------------------------------------------------------------------

/// One permission ask, decoded from `session/request_permission` into the shape
/// a policy needs to make a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionAsk {
    /// Tool name ("write", "shell", "read", …).
    pub tool: String,
    /// Human title (`/path/to/file`, the shell command).
    pub title: Option<String>,
    /// Tool kind ("edit", "execute", "read", …).
    pub kind: Option<String>,
    /// Paths the call touches.
    pub locations: Vec<String>,
    /// The option ids the AGENT offered. A reply may only name one of these —
    /// inventing an option is a protocol violation, so an unknown set is
    /// answered `cancelled` rather than guessed.
    pub option_ids: Vec<String>,
}

impl PermissionAsk {
    /// Decode the request params. Never fails: a malformed ask decodes to an
    /// ask with no options, which `cancelled` answers safely.
    pub fn from_params(params: &Value) -> Self {
        let tool_call = params.get("toolCall");
        let tool = tool_call
            .and_then(|t| t.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let title = tool_call
            .and_then(|t| t.get("title"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let kind = tool_call
            .and_then(|t| t.get("kind"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let locations = tool_call
            .and_then(|t| t.get("locations"))
            .and_then(Value::as_array)
            .map(|ls| {
                ls.iter()
                    .filter_map(|l| l.get("path").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let option_ids = params
            .get("options")
            .and_then(Value::as_array)
            .map(|os| {
                os.iter()
                    .filter_map(|o| o.get("optionId").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            tool,
            title,
            kind,
            locations,
            option_ids,
        }
    }

    /// One-line, bounded description for the log (an ask that is answered
    /// invisibly is a control nobody can audit).
    fn describe(&self) -> String {
        let mut s = self.tool.clone();
        if let Some(kind) = &self.kind {
            s.push_str(&format!(" ({kind})"));
        }
        if let Some(title) = &self.title {
            s.push_str(&format!(" — {title}"));
        }
        if !self.locations.is_empty() && self.title.as_deref() != Some(self.locations[0].as_str()) {
            s.push_str(&format!(" @ {}", self.locations.join(", ")));
        }
        summarize(&s)
    }

    /// The option id that means "allow this one call", according to the
    /// ids/kinds the AGENT offered (measured: `once`). Only ever one of
    /// [`PermissionAsk::option_ids`].
    fn allow_once_id(&self) -> Option<&str> {
        self.option_ids
            .iter()
            .find(|id| id.as_str() == "once")
            .map(String::as_str)
            // A different agent may name it differently but still order the
            // offers allow-first (the measured order is once, always, reject).
            .or_else(|| self.option_ids.first().map(String::as_str))
    }

    /// The option id that means "don't". Falls back to `cancelled`, which is
    /// the protocol's own way of saying the client refused to choose.
    fn reject_id(&self) -> Option<&str> {
        self.option_ids
            .iter()
            .find(|id| id.as_str() == "reject")
            .map(String::as_str)
    }
}

/// What a policy decided about an ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    AllowOnce,
    AllowAlways,
    Reject,
}

/// Decides permission asks. The seam exists so a boundary-aware policy
/// (`boundaries.toml`) can replace the default without touching the transport.
pub trait ApprovalPolicy: Send + Sync {
    fn decide(&self, ask: &PermissionAsk) -> Approval;
}

/// M2's default policy: allow each asked call once, one ask at a time.
///
/// This is deliberately the SAME effective consent M1 gave with `--auto`
/// (blanket pre-approval), but it is now visible per call and arrives as an
/// ask amux can refuse. It never widens what the agent may do: only tools the
/// worker's own opencode config marks `ask` ever reach it.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowOncePolicy;

impl ApprovalPolicy for AllowOncePolicy {
    fn decide(&self, _ask: &PermissionAsk) -> Approval {
        Approval::AllowOnce
    }
}

/// A policy that refuses everything. Used by tests and available as an
/// explicit lockdown (the ask is answered, never ignored — a refused ask can
/// never become a hang).
#[derive(Debug, Default, Clone, Copy)]
pub struct RejectAllPolicy;

impl ApprovalPolicy for RejectAllPolicy {
    fn decide(&self, _ask: &PermissionAsk) -> Approval {
        Approval::Reject
    }
}

// ---------------------------------------------------------------------------
// Translation (pure — the measured frames are the fixtures)
// ---------------------------------------------------------------------------

/// Translate one `session/update` payload into amux events. Unknown updates
/// and malformed shapes are silent (never a panic, never an invented event).
pub fn translate_update(update: &Value, _turn: &TurnId) -> Vec<WorkerEvent> {
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        // A streamed chunk of the agent's answer. The one-line summary is the
        // heartbeat; the full text rides the raw frame in TraceObserved.
        "agent_message_chunk" => {
            let text = update
                .pointer("/content/text")
                .and_then(Value::as_str)
                .unwrap_or("");
            if text.trim().is_empty() {
                return Vec::new();
            }
            vec![WorkerEvent::Progress(ProgressReport {
                summary: summarize(text),
                tokens_used: None,
            })]
        }
        // Reasoning is progress too: the worker is working, visibly. Marked so
        // a reader can tell thinking from output.
        "agent_thought_chunk" => {
            let text = update
                .pointer("/content/text")
                .and_then(Value::as_str)
                .unwrap_or("");
            if text.trim().is_empty() {
                return Vec::new();
            }
            vec![WorkerEvent::Progress(ProgressReport {
                summary: format!("thinking: {}", summarize(text)),
                tokens_used: None,
            })]
        }
        // A tool was invoked. ACP streams a call as a state machine
        // (`tool_call` pending → `tool_call_update` in_progress → completed),
        // but amux's telemetry model has no per-call state (Invariant 30:
        // headlines, not transcripts), so emitting every frame would triple the
        // event count for one call and repeat the same headline. Emit only when
        // the frame actually SAYS something: the invocation with its input (or
        // a located call), never the terminal frame whose payload is the tool's
        // OUTPUT — that belongs in the raw trace and the logs.
        "tool_call" | "tool_call_update" => {
            let has_input = update
                .get("rawInput")
                .and_then(Value::as_object)
                .is_some_and(|o| !o.is_empty());
            let status = update.get("status").and_then(Value::as_str).unwrap_or("");
            let terminal = matches!(status, "completed" | "failed" | "cancelled");
            let tool = update
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    update
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "tool".to_string());
            // Prefer the concrete action (the shell command, the file path)
            // over the tool's display title, matching the other providers.
            // Only a REAL input object feeds `tool_detail`: its last resort is
            // the compact JSON, and `{}` is not a detail — it would turn the
            // announcement frame into a `tool_used` with the payload `{}`.
            let detail = if has_input {
                tool_detail(update.get("rawInput").unwrap_or(&Value::Null))
            } else {
                None
            }
                .or_else(|| {
                    update
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|t| *t != tool)
                        .map(str::to_string)
                })
                .or_else(|| {
                    update
                        .get("locations")
                        .and_then(Value::as_array)
                        .and_then(|ls| ls.first())
                        .and_then(|l| l.get("path"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            if terminal && !has_input {
                // The call's outcome, not a new invocation.
                return Vec::new();
            }
            if !has_input && detail.is_none() {
                // The announcement frame carried nothing to report; the next
                // update will (measured: `tool_call` arrives with `rawInput: {}`).
                return Vec::new();
            }
            vec![WorkerEvent::ToolUsed(ToolEvent { tool, detail })]
        }
        // Context/token heartbeat. `used` is the turn's running total.
        "usage_update" => {
            let used = update.get("used").and_then(Value::as_u64);
            vec![WorkerEvent::Progress(ProgressReport {
                summary: "usage".to_string(),
                tokens_used: used,
            })]
        }
        _ => Vec::new(),
    }
}

/// The turn's outcome from a `session/prompt` response. `stopReason` is the
/// agent's own word (`end_turn`, `max_tokens`, `refusal`, `cancelled`); the
/// usage is reported separately as progress.
pub fn translate_prompt_result(result: &Value, turn: &TurnId) -> TurnResult {
    let stop = result
        .get("stopReason")
        .and_then(Value::as_str)
        .unwrap_or("end_turn");
    let outcome = match stop {
        "end_turn" => "success".to_string(),
        other => other.to_string(),
    };
    TurnResult {
        turn_id: turn.clone(),
        outcome,
    }
}

/// Tokens the prompt result reported for the turn, when it reported any.
pub fn tokens_from_result(result: &Value) -> Option<u64> {
    result
        .pointer("/usage/totalTokens")
        .and_then(Value::as_u64)
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// What to start. `binary` is the executable (tests point it at a fixture);
/// `resume` is a persisted session id to continue, if any.
pub struct AcpStart {
    pub worker: String,
    pub cwd: PathBuf,
    pub binary: PathBuf,
    pub model: Option<String>,
    pub resume: Option<String>,
    pub events: broadcast::Sender<WorkerEvent>,
    pub policy: Arc<dyn ApprovalPolicy>,
    pub turn_timeout: Duration,
}

impl AcpStart {
    pub fn new(
        worker: impl Into<String>,
        cwd: PathBuf,
        binary: PathBuf,
        events: broadcast::Sender<WorkerEvent>,
    ) -> Self {
        Self {
            worker: worker.into(),
            cwd,
            binary,
            model: None,
            resume: None,
            events,
            policy: Arc::new(AllowOncePolicy),
            turn_timeout: DEFAULT_TURN_TIMEOUT,
        }
    }
}

/// The result of bringing a session up. `resume_dropped` names a persisted
/// session id that turned out to be gone — the caller forgets it so the next
/// start does not fail identically forever (the M1 dead-ref rule).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Handshake {
    pub session_id: String,
    pub resumed: bool,
    pub resume_dropped: Option<String>,
    pub model_applied: Option<String>,
}

/// A live ACP conversation with one agent process.
pub struct AcpSession {
    session_id: Mutex<String>,
    /// Frames out to the agent (requests, replies, notifications).
    out: mpsc::UnboundedSender<Value>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    next_id: AtomicI64,
    /// The child, when this session owns a process (tests with an injected
    /// transport have none).
    child: Mutex<Option<Child>>,
    closed: AtomicBool,
    events: broadcast::Sender<WorkerEvent>,
    /// The turn currently running, for translating streamed updates. Updates
    /// outside a turn are dropped (a turn is first-class; inventing one would
    /// be a lie).
    turn: Arc<Mutex<Option<TurnId>>>,
    worker: String,
    /// The transport's two tasks. `Drop` aborts them, because a session that is
    /// gone must not leave its pipes open: a peer (or an in-process test
    /// transport, where the "pipe" is a duplex and closing a direction needs the
    /// whole stream to drop) would otherwise wait for an EOF that never comes —
    /// two of this module's tests hung exactly that way the first time they were
    /// ever executed.
    writer_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    reader_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Turn ceiling in milliseconds (an AtomicU64 so `handshake(&self)` can
    /// apply `AcpStart::turn_timeout` without a `&mut self`).
    turn_timeout_ms: AtomicU64,
}

impl AcpSession {
    /// Spawn `opencode acp` (or a fixture) and complete the handshake.
    pub async fn spawn(start: AcpStart) -> Result<(Self, Handshake)> {
        let mut cmd = Command::new(&start.binary);
        // `acp` is the CLI's agent-protocol SUBCOMMAND, not a flag: without it
        // the binary starts its TUI. Measured 2026-10-25 against opencode
        // 2.1.4 — a bare `opencode` with this same stdin wrote 12 KB of ANSI to
        // stdout, answered no frame at all, and never exited, so `initialize`
        // timed out after 60s: every real ACP start failed while every fixture
        // test passed, because a shell script answers stdin whatever its argv.
        // `the_agent_is_launched_with_its_acp_subcommand` now pins this.
        cmd.arg("acp");
        cmd.current_dir(&start.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) if e.raw_os_error() == Some(26) => {
                // ETXTBSY: the multithreaded-fork/exec race on a just-written
                // binary (the same one structured.rs retries). Transient by
                // construction, bounded here too.
                tracing::warn!(program = %start.binary.display(),
                    "acp spawn hit ETXTBSY (text file busy) — retrying");
                tokio::time::sleep(Duration::from_millis(20)).await;
                cmd.spawn().map_err(|e| {
                    ProtocolError::Transport(format!("spawn {}: {e}", start.binary.display()))
                })?
            }
            Err(e) => {
                return Err(ProtocolError::Transport(format!(
                    "spawn {}: {e}",
                    start.binary.display()
                )))
            }
        };
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProtocolError::Transport("acp child stdout not captured".into()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ProtocolError::Transport("acp child stdin not captured".into()))?;
        let stderr = child.stderr.take();

        let session = Self::with_io(
            stdout,
            stdin,
            start.worker.clone(),
            start.events.clone(),
            start.policy.clone(),
        );
        *session.child.lock().unwrap() = Some(child);
        if let Some(stderr) = stderr {
            tokio::spawn(log_stderr(start.worker.clone(), stderr));
        }

        let handshake = match session.handshake(&start).await {
            Ok(h) => h,
            Err(e) => {
                // Self-announcing: the argv is the first thing to check when an
                // agent goes silent, and this line is what a log sweep reads.
                tracing::error!(
                    worker = %start.worker,
                    program = %start.binary.display(),
                    argv = "acp",
                    error = %e,
                    "acp: handshake failed — the agent never answered (argv is `<program> acp`; a bare program starts a TUI that answers nothing)"
                );
                return Err(e);
            }
        };
        Ok((session, handshake))
    }

    /// Build a session over any byte streams. This is the seam tests use to
    /// drive the protocol in-process (no process, no fixture binary), and the
    /// spawn path calls it with the child's real pipes.
    pub fn with_io<R, W>(
        reader: R,
        writer: W,
        worker: String,
        events: broadcast::Sender<WorkerEvent>,
        policy: Arc<dyn ApprovalPolicy>,
    ) -> Self
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (out, rx) = mpsc::unbounded_channel::<Value>();
        let reader_out = out.downgrade();
        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let turn: Arc<Mutex<Option<TurnId>>> = Arc::new(Mutex::new(None));
        let reader_policy = policy.clone();
        let session = Self {
            session_id: Mutex::new(String::new()),
            out,
            pending: pending.clone(),
            next_id: AtomicI64::new(1),
            child: Mutex::new(None),
            closed: AtomicBool::new(false),
            events: events.clone(),
            turn: turn.clone(),
            worker: worker.clone(),
            writer_task: Mutex::new(None),
            reader_task: Mutex::new(None),
            turn_timeout_ms: AtomicU64::new(DEFAULT_TURN_TIMEOUT.as_millis() as u64),
        };
        let writer_task = tokio::spawn(write_frames(writer, rx));
        let reader_task = tokio::spawn(read_frames(
            reader,
            pending,
            events,
            turn,
            reader_out,
            worker,
            reader_policy,
        ));
        *session.writer_task.lock().unwrap() = Some(writer_task);
        *session.reader_task.lock().unwrap() = Some(reader_task);
        session
    }

    /// The provider-native conversation id. This is the value persisted for
    /// continuity: `session/resume` continues it after a restart.
    pub fn session_id(&self) -> String {
        self.session_id.lock().unwrap().clone()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// initialize → `session/resume <id>` (or `session/new`) → model.
    async fn handshake(&self, start: &AcpStart) -> Result<Handshake> {
        // The turn ceiling travels with the start request (the process path and
        // the injected-transport path both create the session before knowing
        // it), so set it here where the caller's intent is available.
        self.turn_timeout_ms
            .store(start.turn_timeout.as_millis() as u64, Ordering::SeqCst);
        let init = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    // We serve the agent's file requests ourselves (no terminal
                    // capability): the agent runs in the worker's own cwd, and
                    // every file it touches is already the worker's business.
                    "clientCapabilities": {
                        "fs": {"readTextFile": true, "writeTextFile": true},
                        "terminal": false
                    },
                    "clientInfo": {"name": "amux", "version": env!("CARGO_PKG_VERSION")}
                }),
                HANDSHAKE_TIMEOUT,
            )
            .await?;
        let agent_version = init
            .pointer("/agentInfo/version")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        tracing::info!(worker = %self.worker, agent_version, "acp agent initialized");

        let mut out = Handshake::default();
        let cwd = start.cwd.to_string_lossy().to_string();
        let new_session = || {
            self.request(
                "session/new",
                json!({"cwd": cwd, "mcpServers": []}),
                HANDSHAKE_TIMEOUT,
            )
        };
        // Continuity first: a persisted id that still exists continues the same
        // conversation. An id the agent refuses is DROPPED (reported to the
        // caller), never retried forever.
        let created = match &start.resume {
            Some(id) if !id.is_empty() => {
                let resumed = self
                    .request(
                        "session/resume",
                        json!({"sessionId": id, "cwd": cwd, "mcpServers": []}),
                        HANDSHAKE_TIMEOUT,
                    )
                    .await;
                match resumed {
                    Ok(v) => {
                        out.resumed = true;
                        Ok(v)
                    }
                    Err(e) => {
                        tracing::warn!(worker = %self.worker, session = %id, error = %e,
                            "acp resume refused; starting a fresh session and dropping the ref");
                        out.resume_dropped = Some(id.clone());
                        new_session().await
                    }
                }
            }
            _ => new_session().await,
        }?;
        let id = created
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProtocolError::Transport("session/new returned no sessionId".to_string())
            })?
            .to_string();
        *self.session_id.lock().unwrap() = id.clone();
        out.session_id = id;

        // The worker's configured model, if it has one. Failure is loud but not
        // fatal: the session still runs on the agent's own default, which is
        // what M1 would have done with no `--model`.
        if let Some(model) = start.model.as_deref().filter(|m| !m.is_empty()) {
            match self.set_model(model).await {
                Ok(()) => out.model_applied = Some(model.to_string()),
                Err(e) => tracing::warn!(worker = %self.worker, model,
                    error = %e, "acp could not apply the worker's model; using the agent default"),
            }
        }
        Ok(out)
    }

    /// Select the model through the ACP config option (there is no
    /// `session/set_model`; measured `-32601`).
    pub async fn set_model(&self, model: &str) -> Result<()> {
        self.request(
            "session/set_config_option",
            json!({"sessionId": self.session_id(), "configId": "model", "value": model}),
            HANDSHAKE_TIMEOUT,
        )
        .await
        .map(|_| ())
    }

    /// Send one turn and wait for it to end. `turn` is recorded so streamed
    /// updates are attributed to it.
    pub async fn prompt(&self, text: &str, turn: &TurnId) -> Result<TurnResult> {
        *self.turn.lock().unwrap() = Some(turn.clone());
        let timeout = Duration::from_millis(self.turn_timeout_ms.load(Ordering::SeqCst));
        let result = self
            .request(
                "session/prompt",
                json!({
                    "sessionId": self.session_id(),
                    "prompt": [{"type": "text", "text": text}]
                }),
                timeout,
            )
            .await?;
        *self.turn.lock().unwrap() = None;
        if let Some(tokens) = tokens_from_result(&result) {
            let _ = self.events.send(WorkerEvent::Progress(ProgressReport {
                summary: "turn finished".to_string(),
                tokens_used: Some(tokens),
            }));
        }
        Ok(translate_prompt_result(&result, turn))
    }

    /// Cancel the running turn. A NOTIFICATION in ACP (no id): sent as a
    /// request the agent answers `-32601 Method not found` (measured).
    pub fn cancel(&self) -> Result<()> {
        let _ = self.out.send(json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {"sessionId": self.session_id()}
        }));
        Ok(())
    }

    /// Best-effort orderly shutdown, then reap. Never blocks a stop: the
    /// session is unusable afterwards either way.
    pub async fn close(&self) {
        let id = self.session_id();
        if !id.is_empty() {
            let _ = self
                .request(
                    "session/close",
                    json!({"sessionId": id}),
                    Duration::from_secs(5),
                )
                .await;
        }
        self.closed.store(true, Ordering::SeqCst);
        let child = self.child.lock().unwrap().take();
        if let Some(mut child) = child {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    /// One JSON-RPC request/response round trip, bounded by `timeout`.
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ProtocolError::NoSession(format!(
                "acp session for {} is closed",
                self.worker
            )));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if self.out.send(frame).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(ProtocolError::Transport("acp transport is gone".into()));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Value::Object(mut obj))) => {
                if let Some(err) = obj.remove("error") {
                    let msg = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown acp error");
                    return Err(ProtocolError::Rejected(format!("{method}: {msg}")));
                }
                Ok(obj.remove("result").unwrap_or(Value::Null))
            }
            // The reader died holding the reply (process exit, broken pipe).
            // It also settles outstanding requests with `Null`, so a non-object
            // reply is the closed transport, never a success.
            Ok(Ok(_)) | Ok(Err(_)) => Err(ProtocolError::Transport(format!(
                "acp transport closed while awaiting {method}"
            ))),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(ProtocolError::Transport(format!(
                    "acp {method} timed out after {timeout:?}"
                )))
            }
        }
    }
}

/// A dropped session STOPS its transport. `kill_on_drop` only covers the child
/// process; the two tasks are what hold the pipes, and a peer waiting on EOF
/// (the agent, or the far end of an in-process duplex) would wait forever while
/// the read half kept the "pipe" open. Aborting them is the honest shutdown:
/// after this there is nothing left that could deliver a frame.
impl Drop for AcpSession {
    fn drop(&mut self) {
        let writer = self.writer_task.lock().unwrap().take();
        if let Some(task) = writer {
            task.abort();
        }
        let reader = self.reader_task.lock().unwrap().take();
        if let Some(task) = reader {
            task.abort();
        }
    }
}

/// The reader task: demultiplex responses, notifications and the agent's own
/// requests. A request from the agent is ALWAYS answered while the session
/// lives — an unanswered permission ask would be a permanent wait in the agent.
///
/// `out` is WEAK on purpose: the reader may not be the reason a session stays
/// alive (see the `Drop` impl above — the same deadlock, one layer down).
#[allow(clippy::too_many_arguments)]
async fn read_frames<R>(
    reader: R,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    events: broadcast::Sender<WorkerEvent>,
    turn: Arc<Mutex<Option<TurnId>>>,
    out: mpsc::WeakUnboundedSender<Value>,
    worker: String,
    policy: Arc<dyn ApprovalPolicy>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            tracing::warn!(worker = %worker, len = line.len(), "acp: non-JSON frame ignored");
            continue;
        };
        let id = frame.get("id").cloned();
        let method = frame.get("method").and_then(Value::as_str);

        // Our turn's raw evidence, exactly as M1 kept it (the transcript
        // renderer and diagnosis read these), plus the translated update. Only
        // inside a turn: a turn is first-class, so an update outside one is
        // dropped rather than attributed to an invented turn.
        let active_turn = { turn.lock().unwrap().clone() };
        if let Some(active_turn) = active_turn {
            let _ = events.send(WorkerEvent::TraceObserved(
                amux_core::protocol::TurnTrace {
                    turn_id: active_turn.clone(),
                    kind: amux_core::protocol::TurnTraceKind::ProviderEvent,
                    content: line.clone(),
                },
            ));
            // The `session/update` payload lives under `params.update`; the
            // frame itself is transport envelope.
            if let Some(update) = frame.pointer("/params/update") {
                for ev in translate_update(update, &active_turn) {
                    let _ = events.send(ev);
                }
            }
        }

        match (method, id) {
            // Our response.
            (None, Some(id)) => {
                if let Some(id) = id.as_i64() {
                    if let Some(tx) = pending.lock().unwrap().remove(&id) {
                        let _ = tx.send(frame);
                    } else {
                        tracing::warn!(worker = %worker, id, "acp: response for unknown request id");
                    }
                }
            }
            // A request FROM the agent (permission, fs, elicitation).
            (Some(method), Some(id)) if !id.is_null() => {
                let reply = handle_agent_request(method, &frame, &policy, &worker);
                match out.upgrade() {
                    Some(tx) => {
                        let _ = tx.send(reply.into_frame(id));
                    }
                    // The session is gone: nothing can answer, and the agent is
                    // about to be reaped anyway. Say so instead of waiting.
                    None => tracing::warn!(worker = %worker, method,
                        "acp: session gone; agent request left unanswered"),
                }
            }
            // A notification.
            (Some(_method), _) => {}
            (None, None) => tracing::warn!(worker = %worker, "acp: frame with neither method nor id"),
        }
    }
    tracing::info!(worker = %worker, verdict = "acp_stream_ended", "acp agent stream ended");
    // Fail every outstanding request loudly rather than leaving a caller
    // waiting on a channel that can never be answered.
    let mut pending = pending.lock().unwrap();
    for (_, tx) in pending.drain() {
        let _ = tx.send(Value::Null);
    }
}

/// What the client answers an agent request with. A JSON-RPC reply is either a
/// `result` or an `error` — never an error nested inside a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentReply {
    Result(Value),
    Error { code: i64, message: String },
}

impl AgentReply {
    fn into_frame(self, id: Value) -> Value {
        match self {
            AgentReply::Result(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            AgentReply::Error { code, message } => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        }
    }
}

/// Answer one agent request. Unimplemented methods get an explicit error so the
/// agent unblocks with a diagnosis instead of hanging.
fn handle_agent_request(
    method: &str,
    frame: &Value,
    policy: &Arc<dyn ApprovalPolicy>,
    worker: &str,
) -> AgentReply {
    let params = frame.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "session/request_permission" => {
            let ask = PermissionAsk::from_params(&params);
            let decision = policy.decide(&ask);
            let option = match decision {
                Approval::AllowOnce => ask.allow_once_id(),
                Approval::AllowAlways => ask
                    .option_ids
                    .iter()
                    .find(|id| id.as_str() == "always")
                    .map(String::as_str)
                    .or_else(|| ask.allow_once_id()),
                Approval::Reject => ask.reject_id(),
            };
            match option {
                // The reply names an option the AGENT offered; anything else
                // would be inventing protocol.
                Some(option_id) => {
                    tracing::info!(
                        worker = %worker,
                        tool = %ask.tool,
                        kind = ask.kind.as_deref().unwrap_or(""),
                        verdict = ?decision,
                        option = option_id,
                        request = %ask.describe(),
                        "acp permission ask answered"
                    );
                    AgentReply::Result(
                        json!({"outcome": {"outcome": "selected", "optionId": option_id}}),
                    )
                }
                // No usable option (a reject the agent did not offer, or a
                // malformed ask): `cancelled` is the protocol's own refusal and
                // still settles the ask.
                None => {
                    tracing::warn!(
                        worker = %worker,
                        tool = %ask.tool,
                        verdict = ?decision,
                        options = ?ask.option_ids,
                        "acp permission ask had no matching option; answering cancelled"
                    );
                    AgentReply::Result(json!({"outcome": {"outcome": "cancelled"}}))
                }
            }
        }
        other => {
            tracing::warn!(worker = %worker, method = %other,
                "acp: unimplemented agent request answered with an error");
            AgentReply::Error {
                code: -32601,
                message: format!("amux does not implement {other}"),
            }
        }
    }
}

/// Serialize frames out. One writer task so concurrent callers never interleave
/// half-frames on the pipe.
async fn write_frames<W>(mut writer: W, mut rx: mpsc::UnboundedReceiver<Value>)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    while let Some(frame) = rx.recv().await {
        let mut line = frame.to_string();
        line.push('\n');
        if writer.write_all(line.as_bytes()).await.is_err() {
            break;
        }
        if writer.flush().await.is_err() {
            break;
        }
    }
}

/// Keep the agent's stderr visible in amux's logs: it is the only place a
/// protocol-level complaint shows up (ethos rule 4).
async fn log_stderr(worker: String, stderr: ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::warn!(worker = %worker, "acp stderr: {}", summarize(&line));
    }
}

// ---------------------------------------------------------------------------
// Tests. The JSON below is COPIED FROM THE MEASURED FRAMES (opencode 2.0.32,
// 2026-10-17) — see docs/opencode-provider-plan.md (M2 section) for the measured
// protocol facts. Nothing here is invented: if the agent changes shape, these
// fixtures keep passing while the live path misbehaves, so they are paired with
// the fixture-binary test that exercises the whole transport.
// ---------------------------------------------------------------------------

/// Write an executable that speaks just enough ACP to complete a turn, and logs
/// each start (`<name>.runs`) and its argv (`<name>.argv`). It REFUSES
/// `session/resume`, so the handshake's dead-ref rule is exercised too, and it
/// emits a tool call plus a message chunk so the M2 event path is visible.
/// Shared with `structured.rs`'s protocol-pump test.
#[cfg(test)]
pub(crate) fn acp_fixture_script(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    let runs = dir.join(format!("{name}.runs"));
    let argv = dir.join(format!("{name}.argv"));
    let config = dir.join(format!("{name}.config"));
    let mut body = String::from("#!/bin/sh\n");
    body.push_str(&format!("printf x >> '{}'\n", runs.display()));
    body.push_str(&format!(
        "{{ printf '%s\\000' \"$@\"; printf '\\n'; }} >> '{}'\n",
        argv.display()
    ));
    body.push_str(
        r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentInfo":{"name":"OpenCode","version":"2.0.32"}}}\n' "$id" ;;
    *'"method":"session/resume"'*) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"session not found"}}\n' "$id" ;;
    *'"method":"session/new"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"ses_fixture"}}\n' "$id" ;;
    *'"method":"session/set_config_option"'*) printf '%s\n' "$line" >> 'CONFIGFILE'; printf '{"jsonrpc":"2.0","id":%s,"result":{"configOptions":[]}}\n' "$id" ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_fixture","update":{"sessionUpdate":"tool_call","toolCallId":"call-1","name":"write","title":"write","kind":"edit","status":"pending","locations":[],"rawInput":{}}}}\n'
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_fixture","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-1","status":"in_progress","kind":"edit","title":"write","locations":[{"path":"%s/hello.txt"}],"rawInput":{"path":"%s/hello.txt","content":"hi"}}}}\n' "$PWD" "$PWD"
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_fixture","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-1","status":"completed","locations":[{"path":"%s/hello.txt"}],"content":[{"type":"content","content":{"type":"text","text":"Created file successfully: hello.txt"}}],"rawOutput":{"metadata":{"truncated":false}}}}}\n' "$PWD"
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_fixture","update":{"sessionUpdate":"agent_message_chunk","messageId":"m1","content":{"type":"text","text":"fixture answer"}}}}\n'
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_fixture","update":{"sessionUpdate":"usage_update","used":6138,"size":1000000,"cost":{"amount":0.01,"currency":"USD"}}}}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn","usage":{"totalTokens":42}}}\n' "$id" ;;
    *'"method":"session/close"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
  esac
done
"#,
    );
    let mut f = std::fs::File::create(&path).unwrap();
    use std::io::Write;
    let body = body.replace("CONFIGFILE", &config.display().to_string());
    f.write_all(body.as_bytes()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn turn() -> TurnId {
        TurnId::from_ulid(ulid::Ulid::new())
    }

    /// Box a policy into the object the transport takes. (A bare
    /// `Arc::new(X)` does not coerce at a `&Arc<dyn Trait>` parameter site.)
    fn policy(p: impl ApprovalPolicy + 'static) -> Arc<dyn ApprovalPolicy> {
        Arc::new(p)
    }

    /// The exact `tool_call` frame measured for a file write.
    const TOOL_CALL: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_1","update":{"sessionUpdate":"tool_call","toolCallId":"call-648a041b","name":"write","title":"write","kind":"edit","status":"pending","locations":[],"rawInput":{}}}}"#;
    const TOOL_CALL_PROGRESS: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_1","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-648a041b","status":"in_progress","kind":"edit","title":"write","locations":[{"path":"/tmp/ws/hello.txt"}],"rawInput":{"path":"/tmp/ws/hello.txt","content":"hi"}}}}"#;
    const TOOL_CALL_DONE: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_1","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-648a041b","status":"completed","locations":[{"path":"/tmp/ws/hello.txt"}],"content":[{"type":"content","content":{"type":"text","text":"Created file successfully: hello.txt"}}],"rawOutput":{"metadata":{"truncated":false}}}}}"#;
    const MESSAGE_CHUNK: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_1","update":{"sessionUpdate":"agent_message_chunk","messageId":"msg_1","content":{"type":"text","text":"done"}}}}"#;
    const THOUGHT_CHUNK: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_1","update":{"sessionUpdate":"agent_thought_chunk","messageId":"msg_1:reasoning:0","content":{"type":"text","text":"The user wants one word."}}}}"#;
    const USAGE_UPDATE: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_1","update":{"sessionUpdate":"usage_update","used":6138,"size":1000000,"cost":{"amount":0.001113102,"currency":"USD"}}}}"#;
    const PERMISSION_ASK: &str = r#"{"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{"sessionId":"ses_1","toolCall":{"toolCallId":"call-574fe502","name":"write","title":"/tmp/ws/perm.txt","kind":"edit","status":"pending","locations":[{"path":"/tmp/ws/perm.txt"}],"rawInput":{"path":"/tmp/ws/perm.txt","content":"hi"},"content":[{"type":"diff","path":"/tmp/ws/perm.txt","oldText":null,"newText":"hi"}]},"options":[{"optionId":"once","kind":"allow_once","name":"Allow once"},{"optionId":"always","kind":"allow_always","name":"Always allow"},{"optionId":"reject","kind":"reject_once","name":"Reject"}]}}"#;
    const PROMPT_RESULT: &str = r#"{"jsonrpc":"2.0","id":5,"result":{"stopReason":"end_turn","usage":{"inputTokens":5335,"outputTokens":401,"totalTokens":6138,"thoughtTokens":18,"cachedReadTokens":384}}}"#;

    fn update_of(frame: &str) -> Value {
        let v: Value = serde_json::from_str(frame).unwrap();
        v.pointer("/params/update").cloned().unwrap()
    }

    #[test]
    fn a_write_tool_call_is_a_tool_used_with_the_path() {
        let t = turn();
        // The announcement frame carries `rawInput: {}` — nothing to report, so
        // it is NOT an event (the next update names the path).
        assert!(translate_update(&update_of(TOOL_CALL), &t).is_empty());
        // The in-progress frame carries the real input: the path is the summary
        // the harness wants, and it is the ONLY tool event for this call.
        assert_eq!(
            translate_update(&update_of(TOOL_CALL_PROGRESS), &t),
            vec![WorkerEvent::ToolUsed(ToolEvent {
                tool: "write".into(),
                detail: Some("/tmp/ws/hello.txt".into()),
            })]
        );
        // The completed frame is the tool's OUTPUT (logs + raw trace), not a
        // second invocation.
        assert!(translate_update(&update_of(TOOL_CALL_DONE), &t).is_empty());
    }

    #[test]
    fn message_thought_and_usage_chunks_become_progress() {
        let t = turn();
        assert_eq!(
            translate_update(&update_of(MESSAGE_CHUNK), &t),
            vec![WorkerEvent::Progress(ProgressReport {
                summary: "done".into(),
                tokens_used: None,
            })]
        );
        assert_eq!(
            translate_update(&update_of(THOUGHT_CHUNK), &t),
            vec![WorkerEvent::Progress(ProgressReport {
                summary: "thinking: The user wants one word.".into(),
                tokens_used: None,
            })]
        );
        assert_eq!(
            translate_update(&update_of(USAGE_UPDATE), &t),
            vec![WorkerEvent::Progress(ProgressReport {
                summary: "usage".into(),
                tokens_used: Some(6138),
            })]
        );
    }

    #[test]
    fn unknown_and_malformed_updates_are_silent() {
        let t = turn();
        // The measured frames this client deliberately ignores.
        for frame in [
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"available_commands_update","availableCommands":[]}}}"#,
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"config_option_update","configOptions":[]}}}"#,
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"session_info_update"}}}"#,
        ] {
            assert!(translate_update(&update_of(frame), &t).is_empty());
        }
        assert!(translate_update(&serde_json::json!({}), &t).is_empty());
        assert!(translate_update(&Value::Null, &t).is_empty());
        // A chunk with no text is not a heartbeat.
        assert!(translate_update(
            &serde_json::json!({"sessionUpdate": "agent_message_chunk"}),
            &t
        )
        .is_empty());
    }

    #[test]
    fn prompt_result_names_the_stop_reason_and_the_tokens() {
        let t = turn();
        let v: Value = serde_json::from_str(PROMPT_RESULT).unwrap();
        let result = v.get("result").unwrap();
        assert_eq!(
            translate_prompt_result(result, &t),
            TurnResult {
                turn_id: t.clone(),
                outcome: "success".into(),
            }
        );
        assert_eq!(tokens_from_result(result), Some(6138));
        assert_eq!(
            translate_prompt_result(&serde_json::json!({"stopReason": "refusal"}), &t),
            TurnResult {
                turn_id: t,
                outcome: "refusal".into(),
            }
        );
        assert_eq!(tokens_from_result(&serde_json::json!({})), None);
    }

    #[test]
    fn the_permission_ask_decodes_the_measured_frame() {
        let v: Value = serde_json::from_str(PERMISSION_ASK).unwrap();
        let ask = PermissionAsk::from_params(v.get("params").unwrap());
        assert_eq!(ask.tool, "write");
        assert_eq!(ask.kind.as_deref(), Some("edit"));
        assert_eq!(ask.title.as_deref(), Some("/tmp/ws/perm.txt"));
        assert_eq!(ask.locations, vec!["/tmp/ws/perm.txt"]);
        assert_eq!(ask.option_ids, vec!["once", "always", "reject"]);
        assert_eq!(ask.allow_once_id(), Some("once"));
        assert_eq!(ask.reject_id(), Some("reject"));
        assert_eq!(ask.describe(), "write (edit) — /tmp/ws/perm.txt");
    }

    #[test]
    fn an_ask_with_no_offers_is_never_answered_with_an_invented_option() {
        let ask = PermissionAsk::from_params(&serde_json::json!({}));
        assert_eq!(ask.tool, "unknown");
        assert!(ask.option_ids.is_empty());
        assert_eq!(ask.allow_once_id(), None);
        assert_eq!(ask.reject_id(), None);
        let reply = handle_agent_request(
            "session/request_permission",
            &serde_json::json!({"params": {}}),
            &policy(RejectAllPolicy),
            "w",
        );
        assert_eq!(
            reply,
            AgentReply::Result(serde_json::json!({"outcome": {"outcome": "cancelled"}}))
        );
    }

    #[test]
    fn a_rejecting_policy_picks_the_agents_reject_option() {
        let v: Value = serde_json::from_str(PERMISSION_ASK).unwrap();
        let reply = handle_agent_request(
            "session/request_permission",
            &v,
            &policy(RejectAllPolicy),
            "w",
        );
        assert_eq!(
            reply,
            AgentReply::Result(
                serde_json::json!({"outcome": {"outcome": "selected", "optionId": "reject"}})
            )
        );
        // The default allows exactly one call.
        let reply =
            handle_agent_request("session/request_permission", &v, &policy(AllowOncePolicy), "w");
        assert_eq!(
            reply,
            AgentReply::Result(
                serde_json::json!({"outcome": {"outcome": "selected", "optionId": "once"}})
            )
        );
    }

    #[test]
    fn an_unimplemented_agent_request_is_answered_not_ignored() {
        // An unanswered request would hang the agent forever. The reply must be
        // a JSON-RPC ERROR, not an error nested in a result.
        let reply = handle_agent_request(
            "fs/read_text_file",
            &serde_json::json!({"params": {"path": "/etc/hosts"}}),
            &policy(AllowOncePolicy),
            "w",
        );
        assert!(matches!(reply, AgentReply::Error { code: -32601, .. }));
        let frame = reply.into_frame(json!(9));
        assert_eq!(frame.get("error").and_then(|e| e.get("code")), Some(&json!(-32601)));
        assert!(frame.get("result").is_none());
    }

    // -- transport-level tests over an in-process scripted agent -------------

    /// What the scripted agent observed from the client.
    #[derive(Default, Debug)]
    struct Observed {
        permission_reply: Option<Value>,
        unimplemented_reply: Option<Value>,
    }

    fn reply(id: i64, result: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "result": result})
    }

    async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, v: Value) {
        let mut line = v.to_string();
        line.push('\n');
        w.write_all(line.as_bytes()).await.unwrap();
        w.flush().await.unwrap();
    }

    /// Drive a full turn against a scripted agent. The agent emits the measured
    /// updates, asks for permission, and returns the measured prompt result.
    async fn scripted_turn(policy: Arc<dyn ApprovalPolicy>) -> (Result<TurnResult>, Vec<WorkerEvent>, Observed) {
        let (client, agent) = tokio::io::duplex(1 << 20);
        let (c_r, c_w) = tokio::io::split(client);
        let (mut a_r, mut a_w) = tokio::io::split(agent);
        let (events_tx, _events_rx) = broadcast::channel(256);

        let observed = Arc::new(Mutex::new(Observed::default()));
        let agent_observed = observed.clone();
        let script = tokio::spawn(async move {
            let mut lines = BufReader::new(&mut a_r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let id = v.get("id").and_then(Value::as_i64).unwrap_or(-1);
                match v.get("method").and_then(Value::as_str) {
                    Some("initialize") => {
                        write_frame(&mut a_w, reply(id, json!({"protocolVersion": 1,
                            "agentInfo": {"name": "OpenCode", "version": "2.0.32"}}))).await;
                    }
                    Some("session/new") => {
                        write_frame(&mut a_w, reply(id, json!({"sessionId": "ses_scripted"}))).await;
                    }
                    Some("session/set_config_option") => {
                        write_frame(&mut a_w, reply(id, json!({"configOptions": []}))).await;
                    }
                    Some("session/prompt") => {
                        // The measured stream for a tool-using turn.
                        for frame in [MESSAGE_CHUNK, TOOL_CALL, TOOL_CALL_PROGRESS, TOOL_CALL_DONE, USAGE_UPDATE] {
                            let v: Value = serde_json::from_str(frame).unwrap();
                            write_frame(&mut a_w, v).await;
                        }
                        // …then a permission ask, and WAIT for the client's
                        // answer before closing the turn.
                        let ask: Value = serde_json::from_str(PERMISSION_ASK).unwrap();
                        write_frame(&mut a_w, ask).await;
                        while let Ok(Some(line)) = lines.next_line().await {
                            let Ok(reply_frame) = serde_json::from_str::<Value>(&line) else {
                                continue;
                            };
                            if reply_frame.get("id").and_then(Value::as_i64) == Some(0)
                                && reply_frame.get("method").is_none()
                            {
                                agent_observed.lock().unwrap().permission_reply =
                                    reply_frame.get("result").cloned();
                                break;
                            }
                        }
                        // A request amux does not implement: it must be
                        // answered, never ignored.
                        write_frame(&mut a_w, json!({"jsonrpc": "2.0", "id": 9,
                            "method": "fs/read_text_file", "params": {"path": "/etc/hosts"}})).await;
                        while let Ok(Some(line)) = lines.next_line().await {
                            let Ok(reply_frame) = serde_json::from_str::<Value>(&line) else {
                                continue;
                            };
                            if reply_frame.get("id").and_then(Value::as_i64) == Some(9)
                                && reply_frame.get("method").is_none()
                            {
                                agent_observed.lock().unwrap().unimplemented_reply =
                                    Some(reply_frame);
                                break;
                            }
                        }
                        // Answer with the CLIENT's request id (the fixture
                        // const carries the id measured live, 5; a scripted
                        // agent must not guess it — a mismatched id is a reply
                        // the client can never match, and the turn then "times
                        // out" for a reason that has nothing to do with ACP).
                        let mut result: Value = serde_json::from_str(PROMPT_RESULT).unwrap();
                        result["id"] = json!(id);
                        write_frame(&mut a_w, result).await;
                    }
                    Some("session/close") => {
                        write_frame(&mut a_w, reply(id, json!({}))).await;
                    }
                    _ => {}
                }
            }
        });

        let session = AcpSession::with_io(
            c_r,
            c_w,
            "test-worker".to_string(),
            events_tx.clone(),
            policy,
        );
        let mut start = AcpStart::new(
            "test-worker",
            PathBuf::from("/tmp"),
            PathBuf::from("unused"),
            events_tx,
        );
        // A short ceiling so a protocol mistake fails the test loudly instead of
        // hanging the suite (the production default is 30 minutes).
        start.turn_timeout = Duration::from_secs(10);
        let handshake = session.handshake(&start).await.unwrap();
        assert_eq!(handshake.session_id, "ses_scripted");
        let t = turn();
        let mut rx = session.events.subscribe();
        let result = session.prompt("create hello.txt", &t).await;
        // Collect until the turn closes.
        let mut events = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            match tokio::time::timeout(deadline - now, rx.recv()).await {
                Ok(Ok(ev)) => {
                    let terminal = matches!(ev, WorkerEvent::TurnCompleted(_));
                    events.push(ev);
                    if terminal {
                        break;
                    }
                }
                _ => break,
            }
        }
        // Drop the session BEFORE awaiting the scripted agent: the agent's read
        // loop only sees EOF when the client's writer goes away, which happens
        // when the session (and its out-channel) drops. Awaiting first would
        // wait forever — the exact class of hang this transport must never have.
        drop(session);
        let _ = script.await;
        let observed = observed.lock().unwrap();
        let observed = Observed {
            permission_reply: observed.permission_reply.clone(),
            unimplemented_reply: observed.unimplemented_reply.clone(),
        };
        (result, events, observed)
    }

    #[tokio::test]
    async fn a_scripted_turn_streams_tools_and_answers_the_permission_ask() {
        let (result, events, observed) = scripted_turn(policy(AllowOncePolicy)).await;
        let turn_id = result.as_ref().map(|r| r.turn_id.clone()).unwrap();
        assert_eq!(
            result.unwrap(),
            TurnResult {
                turn_id: turn_id.clone(),
                outcome: "success".into(),
            }
        );
        let kinds: Vec<&'static str> = events
            .iter()
            .map(|e| match e {
                WorkerEvent::Progress(_) => "progress",
                WorkerEvent::ToolUsed(_) => "tool_used",
                WorkerEvent::TurnCompleted(_) => "turn_completed",
                WorkerEvent::TraceObserved(_) => "trace",
                WorkerEvent::Failed(_) => "failed",
                WorkerEvent::Started => "started",
                WorkerEvent::TurnStarted { .. } => "turn_started",
                _ => "other",
            })
            .collect();
        // The measured stream translated, in order, with the raw frames kept.
        assert!(kinds.contains(&"progress"), "{kinds:?}");
        assert!(kinds.contains(&"tool_used"), "{kinds:?}");
        // Layering, pinned: `TurnCompleted` is the WORKER layer's event
        // (`structured::run_acp_prompt`), and the turn's terminal signal at THIS
        // layer is the `session/prompt` result asserted above. Emitting it here
        // too would double it in the worker's event stream; expecting it here
        // was the only thing standing between this test and green.
        assert!(!kinds.contains(&"turn_completed"), "{kinds:?}");
        // The write is visible as a tool event with its path.
        assert!(events.iter().any(|e| matches!(
            e,
            WorkerEvent::ToolUsed(t) if t.tool == "write" && t.detail.as_deref() == Some("/tmp/ws/hello.txt")
        )));
        // Token usage rides the usage_update AND the final result.
        assert!(events.iter().any(|e| matches!(
            e,
            WorkerEvent::Progress(p) if p.tokens_used == Some(6138)
        )));
        // The ask was answered with an option the AGENT offered.
        assert_eq!(
            observed.permission_reply,
            Some(json!({"outcome": {"outcome": "selected", "optionId": "once"}}))
        );
        // And an unimplemented request was answered, not left hanging.
        assert!(observed
            .unimplemented_reply
            .as_ref()
            .and_then(|r| r.pointer("/error/message"))
            .is_some());
    }

    #[tokio::test]
    async fn a_rejecting_policy_refuses_the_scripted_ask() {
        let (_result, _events, observed) = scripted_turn(policy(RejectAllPolicy)).await;
        assert_eq!(
            observed.permission_reply,
            Some(json!({"outcome": {"outcome": "selected", "optionId": "reject"}}))
        );
    }

    #[tokio::test]
    async fn a_dead_agent_fails_the_turn_instead_of_hanging_it() {
        let (client, agent) = tokio::io::duplex(1 << 16);
        let (c_r, c_w) = tokio::io::split(client);
        let (mut a_r, mut a_w) = tokio::io::split(agent);
        let (events_tx, _rx) = broadcast::channel(64);
        let script = tokio::spawn(async move {
            let mut lines = BufReader::new(&mut a_r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let id = v.get("id").and_then(Value::as_i64).unwrap_or(-1);
                match v.get("method").and_then(Value::as_str) {
                    Some("initialize") => {
                        write_frame(&mut a_w, reply(id, json!({"protocolVersion": 1}))).await
                    }
                    Some("session/new") => {
                        write_frame(&mut a_w, reply(id, json!({"sessionId": "ses_dying"}))).await
                    }
                    // The agent dies mid-turn: no reply, the stream just ends.
                    Some("session/prompt") => return,
                    _ => {}
                }
            }
        });
        let session = AcpSession::with_io(
            c_r,
            c_w,
            "dying".to_string(),
            events_tx.clone(),
            policy(AllowOncePolicy),
        );
        let start = AcpStart::new("dying", PathBuf::from("/tmp"), PathBuf::from("x"), events_tx);
        session.handshake(&start).await.unwrap();
        let err = session
            .prompt(
                "hi",
                &turn(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProtocolError::Transport(_)),
            "expected a transport error, got {err:?}"
        );
        let _ = script.await;
    }

    // -- spawn path: a fixture ACP binary -----------------------------------

    /// The spawn argv is `<program> acp`, and that is load-bearing: a bare
    /// `opencode` starts its TUI, which answers no frame at all. The fixture
    /// records its argv, so this is the regression that the 60s handshake
    /// timeout could not be (2026-10-25: every live start failed, every fixture
    /// test passed, because a script reads stdin whatever its argv says).
    #[tokio::test]
    async fn the_agent_is_launched_with_its_acp_subcommand() {
        let dir = std::env::temp_dir().join(format!("amux-acp-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = acp_fixture_script(&dir, "argv-fixture");
        let (events_tx, _rx) = broadcast::channel(64);
        let start = AcpStart::new("argv-worker", dir.clone(), binary, events_tx);
        let (session, _handshake) = AcpSession::spawn(start).await.unwrap();
        session.close().await;
        let argv = std::fs::read_to_string(dir.join("argv-fixture.argv")).unwrap_or_default();
        let args: Vec<&str> = argv
            .split('\0')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .collect();
        assert_eq!(args, ["acp"], "spawn argv was {args:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_real_process_completes_a_turn_and_drops_a_dead_resume_ref() {
        let dir = std::env::temp_dir().join(format!("amux-acp-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = acp_fixture_script(&dir, "fixture-acp");
        let (events_tx, mut rx) = broadcast::channel(256);
        let mut start = AcpStart::new("fixture-worker", dir.clone(), binary, events_tx);
        start.model = Some("opencode-go/deepseek-v4.1-flash".to_string());
        start.resume = Some("ses_gone".to_string());
        let (session, handshake) = AcpSession::spawn(start).await.unwrap();
        // The refused resume is reported (so the caller can forget it) and a
        // fresh session took over.
        assert_eq!(handshake.resume_dropped.as_deref(), Some("ses_gone"));
        assert!(!handshake.resumed);
        assert_eq!(handshake.session_id, "ses_fixture");
        assert_eq!(session.session_id(), "ses_fixture");
        assert_eq!(
            handshake.model_applied.as_deref(),
            Some("opencode-go/deepseek-v4.1-flash")
        );

        let t = turn();
        let result = session.prompt("hi", &t).await.unwrap();
        assert_eq!(result.outcome, "success");
        let mut saw_progress = false;
        let mut saw_tokens = false;
        while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            match ev {
                WorkerEvent::Progress(p) => {
                    if p.summary == "fixture answer" {
                        saw_progress = true;
                    }
                    if p.tokens_used == Some(42) {
                        saw_tokens = true;
                    }
                }
                WorkerEvent::TurnCompleted(_) => break,
                _ => {}
            }
        }
        assert!(saw_progress, "the fixture's message chunk surfaced as progress");
        assert!(saw_tokens, "the prompt result's tokens surfaced as progress");
        session.close().await;
        // Exactly one process was started for the whole conversation.
        let runs = std::fs::read_to_string(dir.join("fixture-acp.runs")).unwrap_or_default();
        assert_eq!(runs.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real thing, on a box with an authenticated `opencode`:
    ///   cargo test -p amux-server opencode::acp -- --ignored --nocapture
    /// Ignored in CI (no opencode auth there), like the real-claude tests.
    /// Proves the two M2 claims the fixtures cannot: a live agent streams TOOL
    /// events, and two turns share ONE session (no new process, same id).
    ///
    /// The model is the fleet default ([`crate::opencode::default_model`]) —
    /// the same one a registered worker resolves — and `AMUX_ACP_LIVE_MODEL`
    /// overrides it for a box whose default differs. Measured 2026-10-25 on
    /// this box: the AGENT's own default model answered every prompt with
    /// `503 Upstream request failed: Endpoint is unavailable` (a provider
    /// problem, not an amux one — bare `opencode acp` does the same with no amux
    /// in the loop), while the fleet default returned `end_turn` and wrote the
    /// file. That is why the fleet default is pinned rather than inherited.
    #[tokio::test]
    #[ignore = "requires installed+authenticated opencode CLI; run with -- --ignored"]
    async fn real_opencode_acp_turn_end_to_end() {
        let dir = std::env::temp_dir().join(format!("amux-acp-live-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, mut rx) = broadcast::channel(4096);
        let mut start = AcpStart::new("real-acp", dir.clone(), PathBuf::from("opencode"), tx);
        start.model = std::env::var("AMUX_ACP_LIVE_MODEL")
            .ok()
            .filter(|m| !m.trim().is_empty())
            .or_else(|| Some(crate::opencode::default_model()));
        start.turn_timeout = Duration::from_secs(240);
        let (session, handshake) = AcpSession::spawn(start).await.unwrap();
        assert!(!handshake.session_id.is_empty(), "a live session has an id");

        let t = turn();
        let result = session
            .prompt(
                "Create a file named acp-proof.txt containing exactly the text hi. \
                 Then reply with the single word done.",
                &t,
            )
            .await
            .unwrap();
        assert_eq!(result.outcome, "success");

        let mut tools = Vec::new();
        let mut tokens = None;
        while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            match ev {
                WorkerEvent::ToolUsed(t) => tools.push(format!("{} {:?}", t.tool, t.detail)),
                WorkerEvent::Progress(p) => {
                    if let Some(used) = p.tokens_used {
                        tokens = Some(used);
                    }
                }
                _ => {}
            }
        }
        assert!(
            tools.iter().any(|t| t.starts_with("write")),
            "a live tool-calling turn must surface a tool event: {tools:?}"
        );
        assert!(tokens.unwrap_or(0) > 0, "usage must be reported");

        // Turn two reuses the same session: same id, and only one process.
        let t2 = turn();
        let result2 = session
            .prompt("Reply with exactly one word: again", &t2)
            .await
            .unwrap();
        assert_eq!(result2.outcome, "success");
        assert_eq!(session.session_id(), handshake.session_id);
        let written = dir.join("acp-proof.txt");
        assert!(written.exists(), "the agent's write landed in the worker cwd");
        session.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
