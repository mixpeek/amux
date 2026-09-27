//! Chat worker execution adapter (ACW-4/7/8).
//!
//! A chat worker is a normal amux worker (env file, board identity, message
//! ledger, groups, schedules, memory) whose turns run HEADLESS instead of in a
//! terminal: each delivered message is one provider invocation
//! (`claude -p --output-format stream-json`, or `codex exec --json`) resumed
//! on the worker's persistent conversation id. No tmux pane, no worktree.
//!
//! Single source of truth (ACW-8): the transcript IS the worker's canonical
//! event stream. Every turn writes two `session_events` rows of type
//! `chat.message` (role `user`, then `assistant`), through the same
//! `emit_event` every other worker event uses; there is no chat-only table.
//! Inbound sends are also in `cmd_history` exactly as for a coding worker,
//! because they arrive through the same send path. Turn state (active/idle)
//! is reported through `native_status`, the channel provider hooks use, so
//! the fleet list, SSE projection and lease heartbeat need no chat branch.
//!
//! Streaming (AMUX-5263): provider output is parsed into typed events (text
//! and thinking deltas, tool start/args/input/result, usage, rate-limit
//! state, errors, turn start/end) by `chat_stream`, stamped with a per-lane
//! `seq` and `epoch`, and broadcast as SSE at
//! `/api/sessions/{name}/chat/stream`. The SSE `id` is `epoch:seq`, so a
//! reconnecting client (EventSource's `Last-Event-ID`, or `?after=`) gets
//! exactly the events it missed from a bounded ring, or an explicit `gap`
//! telling it to refetch history. Only the finished message is persisted; a
//! client that connects mid-turn gets the assembled partial turn and the
//! cursor it corresponds to from the history endpoint's `streaming`/`cursor`.
//!
//! Log signals: `verdict="chat_turn_completed"` / `"chat_turn_failed"` per
//! turn with duration and the provider exit, and
//! `verdict="chat_conversation_reset"` when a stale resume id is replaced.

use super::chat_stream::{parse_cursor, Assembly, Journal, Parser, Replay};
use super::session_verbs::{
    emit_event, env_path, home, load_meta, meta_i64, meta_str, parse_env, sh_quote, update_meta,
    SendOrigin,
};
use super::worker_exec::{Dispatch, ExecutionAdapter};
use super::AppState;
use amux_core::worker_type::WorkerTypeId;
use axum::extract::{Path as AxumPath, RawQuery, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

/// The `session_events.type` every chat message is stored under.
pub(crate) const CHAT_EVENT: &str = "chat.message";
const SOURCE: &str = "chat-adapter";
/// A turn that has produced nothing for this long is killed and recorded as
/// failed, so a hung provider cannot pin a worker `working` forever.
const TURN_IDLE_TIMEOUT: Duration = Duration::from_secs(900);
/// How often a running turn re-asserts `active` (well under the 120s the
/// status projection trusts a native report for).
const HEARTBEAT: Duration = Duration::from_secs(45);

pub(crate) struct ChatAdapter;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Queued {
    text: String,
    origin: String,
}

// ---------------------------------------------------------------------------
// Durability across server restarts. The builder exec()s this process on every
// commit; a tmux-hosted coding worker survives that because tmux holds the
// process, a chat turn does not. Measured 2026-09-24: a restart mid-turn left a
// user message recorded with no reply and no error, and a queued message was
// gone without a trace. So the queue lives on disk and the running turn is
// marked, and boot recovery (`recover_all`) resumes the one and reports the
// other.
// ---------------------------------------------------------------------------

fn state_dir() -> std::path::PathBuf {
    home().join("chat-state")
}

fn queue_path(name: &str) -> std::path::PathBuf {
    state_dir().join(format!("{name}.queue.json"))
}

fn persist_queue(name: &str, q: &VecDeque<Queued>) {
    let path = queue_path(name);
    if q.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let _ = std::fs::create_dir_all(state_dir());
    let tmp = path.with_extension(format!("json.{}.tmp", ulid::Ulid::new()));
    let body = serde_json::to_string(&q.iter().collect::<Vec<_>>()).unwrap_or_default();
    if std::fs::write(&tmp, body).is_err() || std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!(session = %name, measured = true, n_considered = q.len(),
            verdict = "chat_queue_persist_failed", "chat queue could not be written; a restart would drop it");
    }
}

fn load_queue(name: &str) -> Vec<Queued> {
    std::fs::read_to_string(queue_path(name))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// In-memory runtime of one chat worker. Nothing here is durable state: the
/// conversation id lives in the worker meta, messages in `session_events`.
struct Lane {
    busy: AtomicBool,
    queue: Mutex<VecDeque<Queued>>,
    /// Serialized, seq-stamped events. Sent while the journal lock is held so
    /// broadcast order is seq order.
    tx: broadcast::Sender<String>,
    pid: Mutex<Option<u32>>,
    /// Ordered, resumable event log plus the assembled in-flight turn.
    journal: Mutex<Journal>,
    /// Set by the interrupt route; the running turn ends as `interrupted`.
    interrupt: AtomicBool,
    /// Last native_status sequence sent, so two reports in one millisecond
    /// are not refused as duplicates.
    status_seq: AtomicU64,
}

impl Lane {
    /// Stamp, retain and broadcast one event.
    fn publish(&self, ev: Value) {
        let mut j = self.journal.lock().unwrap();
        let s = j.push(ev);
        let _ = self.tx.send(s);
    }
}

/// Per-process epoch: a cursor from a previous server process cannot be
/// served from this one's ring, and the epoch is how the client learns that.
static EPOCH: LazyLock<String> = LazyLock::new(|| ulid::Ulid::new().to_string());

static LANES: LazyLock<Mutex<HashMap<String, Arc<Lane>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lane(name: &str) -> Arc<Lane> {
    let mut g = LANES.lock().unwrap();
    g.entry(name.to_string())
        .or_insert_with(|| {
            Arc::new(Lane {
                busy: AtomicBool::new(false),
                queue: Mutex::new(VecDeque::new()),
                tx: broadcast::channel(1024).0,
                pid: Mutex::new(None),
                journal: Mutex::new(Journal::new(&format!("{}-{name}", *EPOCH))),
                interrupt: AtomicBool::new(false),
                status_seq: AtomicU64::new(0),
            })
        })
        .clone()
}

/// Where a chat worker's turns run. The configured dir when there is one; a
/// private scratch dir otherwise, so every cwd-reading path keeps working
/// without a project or worktree.
pub(crate) fn chat_work_dir(name: &str) -> String {
    let cfg = parse_env(name);
    let dir = cfg.get_or("CC_DIR", "").trim().to_string();
    if !dir.is_empty() {
        return dir;
    }
    default_chat_dir(name)
}

pub(crate) fn default_chat_dir(name: &str) -> String {
    home().join("chat").join(name).to_string_lossy().into_owned()
}

fn provider_of(name: &str) -> String {
    parse_env(name)
        .get_or("CC_PROVIDER", "claude")
        .trim()
        .to_lowercase()
}

fn running(name: &str) -> bool {
    load_meta(name)
        .get("chat_running")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// A UUID-shaped id (v4 bits set) from a ULID: `claude --session-id` requires
/// the UUID format, and the crate has no uuid dependency.
fn new_conversation_id() -> String {
    let mut n = u128::from(ulid::Ulid::new());
    n = (n & !(0xF << 76)) | (0x4 << 76);
    n = (n & !(0x3 << 62)) | (0x2 << 62);
    let h = format!("{n:032x}");
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// Report turn state through the provider-hook channel (`native_status`).
async fn report_state(state: &AppState, name: &str, st: &str, event: &str, turn: &str) {
    report_state_with(state, name, st, event, turn, Value::Null).await
}

/// `extra` keys (tool name, model, conversation id) ride along the way a
/// provider hook's payload would carry them.
async fn report_state_with(state: &AppState, name: &str, st: &str, event: &str, turn: &str, extra: Value) {
    let meta = load_meta(name);
    let run_id = meta_str(&meta, "chat_run_id");
    if run_id.is_empty() {
        return;
    }
    let now = crate::config::now_f64();
    // Strictly increasing per lane: native_status refuses a sequence it has
    // already seen, and tool events can land in the same millisecond.
    let now_ms = (now * 1000.0) as u64;
    let seq_cell = &lane(name).status_seq;
    let prev = seq_cell
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |p| Some(now_ms.max(p + 1)))
        .unwrap_or(0);
    let sequence = now_ms.max(prev + 1);
    let mut body = json!({
        "native_status": true,
        "run_id": run_id,
        "provider": provider_of(name),
        "sequence": sequence,
        "event_ts": now,
        "state": st,
        "event": event,
        "turn_id": turn,
        "worker_type": WorkerTypeId::CHAT,
    });
    if let (Some(b), Some(x)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in x {
            b.insert(k.clone(), v.clone());
        }
    }
    let _ = super::native_status::post(state, name, &body).await;
}

#[async_trait::async_trait]
impl ExecutionAdapter for ChatAdapter {
    fn worker_type(&self) -> &'static str {
        WorkerTypeId::CHAT
    }

    async fn start(&self, state: &AppState, name: &str) -> Dispatch<(bool, String)> {
        let dir = chat_work_dir(name);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Dispatch::Handled((false, format!("chat worker dir {dir}: {e}")));
        }
        let provider = provider_of(name);
        let run_id = match super::native_status::begin_launch(name, &provider) {
            Ok((run, _)) => run,
            Err(e) => return Dispatch::Handled((false, format!("launch record: {e}"))),
        };
        let now = crate::config::now_f64() as i64;
        update_meta(
            name,
            &[
                ("chat_running", json!(true)),
                ("chat_run_id", json!(run_id)),
                ("cc_cwd", json!(dir)),
                ("last_started", json!(now)),
                ("boot_readiness_pending", json!(false)),
            ],
        );
        report_state(state, name, "idle", "SessionStart", "").await;
        emit_event(
            state,
            name,
            "session.started",
            Some(json!({"worker_type": WorkerTypeId::CHAT, "provider": provider})),
            None,
            SOURCE,
        )
        .await;
        crate::api::sessions_legacy::invalidate_sessions_cache();
        // Anything automation queued while the worker was stopped.
        let st = state.clone();
        let n = name.to_string();
        tokio::spawn(async move {
            super::session_verbs::steer_deliver_for_session(&st, &n).await;
        });
        Dispatch::Handled((true, "chat worker ready".into()))
    }

    async fn stop(&self, name: &str) -> Dispatch<(bool, String)> {
        let lane = lane(name);
        let dropped = {
            let mut q = lane.queue.lock().unwrap();
            let n = q.len();
            q.clear();
            persist_queue(name, &q);
            n
        };
        let pid = *lane.pid.lock().unwrap();
        if let Some(pid) = pid {
            // SAFETY: plain signal to a child this process spawned.
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        update_meta(name, &[("chat_running", json!(false))]);
        lane.publish(json!({"type": "stopped", "dropped_queued": dropped}));
        crate::api::sessions_legacy::invalidate_sessions_cache();
        tracing::info!(
            session = %name, dropped_queued = dropped, interrupted = pid.is_some(),
            verdict = "chat_worker_stopped", "chat worker stopped"
        );
        Dispatch::Handled((true, "stopped".into()))
    }

    async fn deliver(
        &self,
        state: &AppState,
        name: &str,
        text: &str,
        origin: SendOrigin,
    ) -> Dispatch<(bool, String)> {
        if !running(name) {
            // An owner typing into a stopped chat starts it (there is no
            // process to boot, so this costs nothing). Automation does not
            // wake a stopped worker; its steering row stays queued.
            if origin != SendOrigin::Owner {
                return Dispatch::Handled((false, "chat worker is not running".into()));
            }
            if let Dispatch::Handled((false, why)) = self.start(state, name).await {
                return Dispatch::Handled((false, why));
            }
        }
        if text.trim().is_empty() {
            return Dispatch::Handled((false, "empty message".into()));
        }
        let lane = lane(name);
        let ahead = {
            let mut q = lane.queue.lock().unwrap();
            q.push_back(Queued {
                text: text.to_string(),
                origin: match origin {
                    SendOrigin::Owner => "owner",
                    SendOrigin::Automation => "automation",
                }
                .to_string(),
            });
            persist_queue(name, &q);
            q.len() - 1
        };
        let turn_running = lane.busy.swap(true, Ordering::SeqCst);
        if !turn_running {
            let st = state.clone();
            let n = name.to_string();
            tokio::spawn(async move { pump(st, n).await });
        }
        // `waiting` = messages queued behind the running turn, which is what
        // the view shows; `ahead` alone read 0 for the first queued message.
        let waiting = if turn_running { ahead + 1 } else { 0 };
        lane.publish(json!({"type": "queued", "ahead": ahead, "waiting": waiting, "busy": turn_running}));
        Dispatch::Handled((
            true,
            if turn_running {
                format!("queued behind the running turn ({ahead} ahead)")
            } else {
                "delivered".into()
            },
        ))
    }

    fn running(&self, name: &str) -> Dispatch<bool> {
        Dispatch::Handled(running(name))
    }

    async fn interrupt(&self, name: &str) -> Dispatch<(bool, String)> {
        Dispatch::Handled(interrupt_turn(name))
    }

    async fn peek(&self, state: &AppState, name: &str, lines: i64) -> Dispatch<Value> {
        let messages = read_messages(state, name, 200, None);
        let history = render_transcript(&messages);
        let partial = lane(name)
            .journal
            .lock()
            .unwrap()
            .live
            .as_ref()
            .map(|a| a.text.clone())
            .unwrap_or_default();
        let mut all: Vec<&str> = history.lines().collect();
        let history_lines = all.len();
        if !partial.is_empty() {
            all.push("");
            all.push("assistant (streaming):");
            all.extend(partial.lines());
        }
        let keep = (lines.max(1) as usize).min(all.len());
        let output = all[all.len() - keep..].join("\n");
        Dispatch::Handled(json!({
            "name": name,
            "worker_type": WorkerTypeId::CHAT,
            "renderer": "chat",
            "history": history,
            "live": partial,
            "output": output,
            "output_lines": keep,
            "history_lines": history_lines,
            "output_is_viewport_only": false,
        }))
    }
}

/// Drain the lane's queue one turn at a time. Exactly one pump runs per lane:
/// `deliver` starts one only when it flips `busy` false->true.
async fn pump(state: AppState, name: String) {
    let lane = lane(&name);
    loop {
        let next = {
            let mut q = lane.queue.lock().unwrap();
            let item = q.pop_front();
            persist_queue(&name, &q);
            item
        };
        match next {
            Some(q) => run_turn(&state, &name, &lane, q).await,
            None => {
                lane.busy.store(false, Ordering::SeqCst);
                // A deliver that raced the store above saw busy=true and did
                // not start a pump; take its message now.
                let pending = !lane.queue.lock().unwrap().is_empty();
                if pending && !lane.busy.swap(true, Ordering::SeqCst) {
                    continue;
                }
                break;
            }
        }
    }
    // The turn boundary: the same trigger an idle hook report fires.
    super::session_verbs::steer_deliver_for_session(&state, &name).await;
}

/// What one provider invocation produced: the same assembly the stream
/// built, so the persisted message cannot disagree with what was shown.
struct TurnOutcome {
    asm: Assembly,
    conversation_id: String,
    model: String,
}

impl TurnOutcome {
    fn new(turn_id: &str) -> Self {
        TurnOutcome {
            asm: Assembly::new(turn_id),
            conversation_id: String::new(),
            model: String::new(),
        }
    }
    fn failed(turn_id: &str, why: String) -> Self {
        let mut o = Self::new(turn_id);
        o.asm.error = Some(why);
        o
    }
}

fn model_flag(flags: &str) -> Option<String> {
    let toks: Vec<&str> = flags.split_whitespace().collect();
    toks.iter().enumerate().find_map(|(i, t)| {
        if *t == "--model" {
            toks.get(i + 1).map(|m| m.trim_matches(['"', '\'']).to_string())
        } else {
            t.strip_prefix("--model=").map(str::to_string)
        }
    })
}

/// argv (after the binary) for one turn. Prompt goes on stdin, never argv.
fn turn_args(provider: &str, flags: &str, cc_model: &str, conv: &str, fresh: bool) -> Vec<String> {
    let model = model_flag(flags).or_else(|| Some(cc_model.to_string()).filter(|m| !m.is_empty()));
    if provider == "codex" {
        let mut a: Vec<String> = vec!["exec".into(), "--json".into(), "--skip-git-repo-check".into()];
        if let Some(m) = model {
            a.extend(["--model".into(), m]);
        }
        if flags.contains("--dangerously-bypass-approvals-and-sandbox") || flags.contains("--yolo") {
            a.push("--dangerously-bypass-approvals-and-sandbox".into());
        }
        if !conv.is_empty() {
            a.extend(["resume".into(), conv.to_string()]);
        }
        a.push("-".into());
        return a;
    }
    let mut a: Vec<String> = vec![
        "-p".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--include-partial-messages".into(),
    ];
    if fresh {
        a.extend(["--session-id".into(), conv.to_string()]);
    } else {
        a.extend(["--resume".into(), conv.to_string()]);
    }
    if let Some(m) = model {
        a.extend(["--model".into(), m]);
    }
    // A headless turn cannot answer a permission prompt, so without this a
    // non-YOLO chat worker is refused the amux CLI and cannot take part in
    // boards, messages or memory (seen live: `amux board add` refused while
    // answering the accountability nudge). Only the harness CLI is allowed;
    // every other tool keeps the worker's own permission setting.
    a.extend(["--allowedTools".into(), "Bash(amux:*)".into()]);
    for f in ["--dangerously-skip-permissions", "--allow-dangerously-skip-permissions"] {
        if flags.split_whitespace().any(|t| t == f) {
            a.push(f.into());
        }
    }
    a
}

fn provider_bin(provider: &str) -> String {
    let key = if provider == "codex" {
        "AMUX_CHAT_CODEX_BIN"
    } else {
        "AMUX_CHAT_CLAUDE_BIN"
    };
    std::env::var(key).unwrap_or_else(|_| provider.to_string())
}

async fn run_turn(state: &AppState, name: &str, lane: &Lane, q: Queued) {
    let turn_id = ulid::Ulid::new().to_string();
    let started = std::time::Instant::now();
    let provider = provider_of(name);
    lane.interrupt.store(false, Ordering::SeqCst);
    emit_event(
        state,
        name,
        CHAT_EVENT,
        Some(json!({"role": "user", "text": q.text, "turn_id": turn_id, "origin": q.origin})),
        Some(format!("chat:{turn_id}:user")),
        SOURCE,
    )
    .await;
    let waiting = lane.queue.lock().unwrap().len();
    lane.publish(json!({
        "type": "user", "turn_id": turn_id, "text": q.text, "origin": q.origin,
        "ts": crate::config::now_f64(), "waiting": waiting,
    }));
    update_meta(name, &[("chat_inflight_turn", json!(turn_id))]);
    report_state(state, name, "active", "UserPromptSubmit", &turn_id).await;
    update_meta(name, &[("last_send", json!(crate::config::now_f64() as i64))]);

    let mut out = execute(state, name, &provider, &q.text, lane, &turn_id, false).await;
    // A resume id the provider no longer has (conversation deleted, new
    // machine): start a fresh conversation once rather than failing forever.
    if out
        .asm
        .error
        .as_deref()
        .is_some_and(|e| e.contains("No conversation found") || e.contains("not found"))
        && out.asm.text.is_empty()
        && !lane.interrupt.load(Ordering::SeqCst)
    {
        tracing::warn!(session = %name, verdict = "chat_conversation_reset",
            reason = out.asm.error.as_deref().unwrap_or(""),
            "chat worker's stored conversation could not be resumed; starting a new one");
        update_meta(
            name,
            &[
                ("chat_conversation_id", json!("")),
                ("cc_conversation_id", json!("")),
                ("chat_codex_thread", json!("")),
            ],
        );
        // The failed attempt's error must not linger in the live view.
        lane.publish(json!({"type": "retry", "turn_id": turn_id}));
        out = execute(state, name, &provider, &q.text, lane, &turn_id, true).await;
    }
    let interrupted = lane.interrupt.swap(false, Ordering::SeqCst);
    if !out.conversation_id.is_empty() {
        let turns = meta_i64(&load_meta(name), "chat_turns");
        let id = json!(out.conversation_id);
        if provider == "codex" {
            update_meta(name, &[("chat_codex_thread", id), ("chat_turns", json!(turns + 1))]);
        } else {
            update_meta(
                name,
                &[
                    ("chat_conversation_id", id.clone()),
                    ("cc_conversation_id", id),
                    ("chat_turns", json!(turns + 1)),
                ],
            );
        }
    }
    let duration_ms = started.elapsed().as_millis() as u64;
    let a = &out.asm;
    // An owner interrupt is a choice, not a failure: the partial reply is
    // kept and marked, and the worker goes idle rather than error.
    let error = if interrupted { None } else { a.error.clone() };
    let limited = a.limited();
    let tools: Vec<&str> = a.tools.iter().map(|t| t.name.as_str()).collect();
    let msg = json!({
        "role": "assistant",
        "text": a.text,
        "turn_id": turn_id,
        "provider": provider,
        "model": out.model,
        "tools": tools,
        "tool_calls": a.tools,
        "thinking": a.thinking,
        "thinking_truncated": a.thinking_truncated,
        "usage": a.usage,
        "limit": if limited { a.limit.clone() } else { Value::Null },
        "cost_usd": a.usage["cost_usd"],
        "duration_ms": duration_ms,
        "error": error,
        "interrupted": interrupted,
        "conversation_id": out.conversation_id,
    });
    emit_event(
        state,
        name,
        CHAT_EVENT,
        Some(msg.clone()),
        Some(format!("chat:{turn_id}:assistant")),
        SOURCE,
    )
    .await;
    update_meta(name, &[("chat_inflight_turn", json!(""))]);
    lane.publish(json!({"type": "done", "turn_id": turn_id, "message": msg}));
    match &error {
        None => tracing::info!(session = %name, turn = %turn_id, duration_ms,
            chars = a.text.len(), tools = a.tools.len(), interrupted, measured = true,
            n_considered = 1, verdict = if interrupted { "chat_turn_interrupted_by_owner" } else { "chat_turn_completed" },
            "chat turn completed"),
        Some(e) => tracing::warn!(session = %name, turn = %turn_id, duration_ms, error = %e,
            limited, measured = true, n_considered = 1, verdict = "chat_turn_failed", "chat turn failed"),
    }
    // Turn end: the same edge a provider Stop hook reports, carrying the
    // conversation id so turn-end features (owner-ask, promise nudge) can
    // resolve the transcript.
    let (st, ev) = if error.is_some() {
        ("error", "StopFailure")
    } else {
        ("idle", "Stop")
    };
    let mut extra = json!({"session_id": out.conversation_id, "model": out.model});
    if limited {
        extra["limit"] = a.limit.clone();
    }
    report_state_with(state, name, st, ev, &turn_id, extra).await;
    crate::api::sessions_legacy::invalidate_sessions_cache();
}

/// Spawn one provider turn and stream its output.
async fn execute(
    state: &AppState,
    name: &str,
    provider: &str,
    text: &str,
    lane: &Lane,
    turn_id: &str,
    force_fresh: bool,
) -> TurnOutcome {
    let cfg = parse_env(name);
    let flags = cfg.get_or("CC_FLAGS", "").to_string();
    let cc_model = cfg.get_or("CC_MODEL", "").to_string();
    let meta = load_meta(name);
    let work_dir = chat_work_dir(name);
    let _ = std::fs::create_dir_all(&work_dir);
    let (conv, fresh) = if provider == "codex" {
        let t = meta_str(&meta, "chat_codex_thread");
        (if force_fresh { String::new() } else { t }, false)
    } else {
        // The adapter's own key is authoritative. `cc_conversation_id` is only
        // a mirror for the transcript readers, and terminal-lane conversation
        // management (adoption, takeover, reset) writes that key, so resuming
        // from it let an outside write start a new conversation mid-chat (seen
        // live: turn 2 minted a fresh id with chat_turns=1).
        let owned = meta_str(&meta, "chat_conversation_id");
        let mirror = meta_str(&meta, "cc_conversation_id");
        let has_turns = meta_i64(&meta, "chat_turns") > 0;
        // A chat worker from before the adapter owned this key: adopt once.
        let c = if owned.is_empty() && has_turns { mirror.clone() } else { owned };
        if !c.is_empty() && mirror != c {
            tracing::warn!(session = %name, owned = %c, mirror = %mirror, measured = true,
                n_considered = 1, verdict = "chat_conversation_mirror_repaired",
                "a chat worker's cc_conversation_id mirror was changed outside the chat adapter; restoring it");
            update_meta(name, &[("cc_conversation_id", json!(c))]);
        }
        if force_fresh || c.is_empty() || !has_turns {
            let id = if !force_fresh && !c.is_empty() {
                c
            } else {
                new_conversation_id()
            };
            (id, true)
        } else {
            (c, false)
        }
    };
    if provider != "codex" && fresh {
        // Recorded BEFORE the turn: the transcript readers resolve the
        // conversation file from this id while the turn is still running.
        update_meta(
            name,
            &[("chat_conversation_id", json!(conv)), ("cc_conversation_id", json!(conv))],
        );
    }
    let args = turn_args(provider, &flags, &cc_model, &conv, fresh);
    let prelude = super::session_verbs::headless_turn_prelude(name, provider, &work_dir);
    let script = format!("{prelude}exec {} \"$@\"", sh_quote(&provider_bin(provider)));
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(&script)
        .arg("amux-chat")
        .args(&args)
        .current_dir(&work_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let why = format!("could not start {provider}: {e}");
            lane.publish(json!({"type": "error", "turn_id": turn_id, "text": why}));
            return TurnOutcome::failed(turn_id, why);
        }
    };
    let mut out = TurnOutcome::new(turn_id);
    let mut parser = Parser::new(provider);
    *lane.pid.lock().unwrap() = child.id();
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes()).await;
        drop(stdin);
    }
    let mut stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(e) = stderr.as_mut() {
            let _ = e.read_to_string(&mut buf).await;
        }
        buf
    });
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        let mut last_output = std::time::Instant::now();
        // Status heartbeat: the fleet list trusts an `active` native report
        // for 120s only, and a long tool run or a long answer can go that
        // long with no lifecycle edge, which read as idle mid-turn.
        let mut beat = tokio::time::interval(HEARTBEAT);
        beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        beat.tick().await;
        loop {
            let next = tokio::select! {
                // `next_line` is cancel-safe, so losing the race to a tick
                // drops no bytes.
                l = lines.next_line() => Ok(l),
                _ = beat.tick() => Err(()),
            };
            let next = match next {
                Err(()) => {
                    if last_output.elapsed() >= TURN_IDLE_TIMEOUT {
                        Err(())
                    } else {
                        report_state_with(state, name, "active", "Heartbeat", turn_id,
                            json!({"silent_s": last_output.elapsed().as_secs()})).await;
                        continue;
                    }
                }
                Ok(l) => Ok(l),
            };
            match next {
                Ok(Ok(Some(line))) => {
                    last_output = std::time::Instant::now();
                    for mut ev in parser.line(&line) {
                        ev["turn_id"] = json!(turn_id);
                        out.asm.apply(&ev);
                        // Hooks-equivalent tool lifecycle, so the fleet sees a
                        // chat worker's tool activity the way a terminal
                        // worker's PreToolUse/PostToolUse hooks report it.
                        match ev["type"].as_str() {
                            Some("tool_start") => {
                                report_state_with(state, name, "active", "PreToolUse", turn_id,
                                    json!({"tool_name": ev["name"]})).await;
                            }
                            Some("tool_result") => {
                                let tool = out.asm.tools.iter().rev()
                                    .find(|t| Some(t.id.as_str()) == ev["id"].as_str())
                                    .map(|t| t.name.clone()).unwrap_or_default();
                                report_state_with(state, name, "active", "PostToolUse", turn_id,
                                    json!({"tool_name": tool, "is_error": ev["is_error"]})).await;
                            }
                            _ => {}
                        }
                        lane.publish(ev);
                    }
                }
                Ok(_) => break,
                Err(_) => {
                    let _ = child.start_kill();
                    let why = format!("no output for {}s; turn killed", TURN_IDLE_TIMEOUT.as_secs());
                    lane.publish(json!({"type": "error", "turn_id": turn_id, "text": why}));
                    out.asm.error = Some(why);
                    break;
                }
            }
        }
    }
    let status = child.wait().await.ok();
    *lane.pid.lock().unwrap() = None;
    out.conversation_id = parser.conversation_id.clone();
    out.model = parser.model.clone();
    let err_text = stderr_task.await.unwrap_or_default();
    if lane.interrupt.load(Ordering::SeqCst) {
        lane.publish(json!({"type": "interrupted", "turn_id": turn_id}));
    } else if out.asm.error.is_none() && !status.is_some_and(|s| s.success()) {
        let tail: String = err_text
            .lines()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        let why = if tail.trim().is_empty() {
            format!("{provider} exited with {:?}", status.and_then(|s| s.code()))
        } else {
            tail
        };
        lane.publish(json!({"type": "error", "turn_id": turn_id, "text": why}));
        out.asm.error = Some(why);
    }
    out
}

/// Boot recovery for one chat worker (see the durability note above).
///
/// A turn marked in flight by a previous process is NOT re-run: its tools may
/// already have acted. It gets an assistant message carrying an explicit
/// error, so the transcript says what happened instead of ending on an
/// unanswered question. Queued messages were never started, so they run.
/// Returns (interrupted turns reported, queued messages resumed).
pub async fn recover(state: &AppState, name: &str) -> (usize, usize) {
    // Recovery runs a few seconds after boot, and a message sent in that
    // window has already started a turn IN THIS PROCESS. Measured 2026-09-26
    // (AMUX-5263 e2e): that live turn was reported as "cut off by a server
    // restart", and the error row took the `chat:{turn}:assistant` dedupe key
    // the real reply needed. A busy lane is this process's own work; its
    // queue is in memory too, so reloading the disk copy would duplicate it.
    if lane(name).busy.load(Ordering::SeqCst) {
        tracing::info!(session = %name, measured = true, n_considered = 1,
            verdict = "chat_recovery_skipped_live_turn",
            "chat worker already running a turn in this process; nothing to recover");
        return (0, 0);
    }
    let meta = load_meta(name);
    let mut interrupted = 0;
    let turn = meta_str(&meta, "chat_inflight_turn");
    if !turn.is_empty() {
        let msg = json!({
            "role": "assistant",
            "text": "",
            "turn_id": turn,
            "provider": provider_of(name),
            "error": "The amux server restarted during this turn, so the reply was lost. \
                      Send the message again to retry.",
            "interrupted": true,
        });
        emit_event(state, name, CHAT_EVENT, Some(msg), Some(format!("chat:{turn}:assistant")), SOURCE)
            .await;
        update_meta(name, &[("chat_inflight_turn", json!(""))]);
        report_state(state, name, "idle", "StopFailure", &turn).await;
        tracing::warn!(session = %name, turn = %turn, measured = true, n_considered = 1,
            verdict = "chat_turn_interrupted",
            "a chat turn was cut off by a server restart; recorded as failed, not re-run");
        interrupted = 1;
    }
    let pending = load_queue(name);
    let resumed = pending.len();
    if resumed > 0 {
        if running(name) {
            let lane = lane(name);
            {
                let mut q = lane.queue.lock().unwrap();
                for item in pending.into_iter().rev() {
                    q.push_front(item);
                }
                persist_queue(name, &q);
            }
            if !lane.busy.swap(true, Ordering::SeqCst) {
                let st = state.clone();
                let n = name.to_string();
                tokio::spawn(async move { pump(st, n).await });
            }
            tracing::warn!(session = %name, measured = true, n_considered = resumed,
                verdict = "chat_queue_recovered", "resumed chat messages queued before a restart");
        } else {
            tracing::warn!(session = %name, measured = true, n_considered = resumed,
                verdict = "chat_queue_held_stopped",
                "chat messages queued before a restart are kept; the worker is stopped");
        }
    }
    (interrupted, if running(name) { resumed } else { 0 })
}

/// Recover every chat worker on this server (called once at boot).
pub async fn recover_all(state: &AppState) -> (usize, usize, usize) {
    let dir = home().join("sessions");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return (0, 0, 0);
    };
    let (mut workers, mut interrupted, mut resumed) = (0, 0, 0);
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("env") {
            continue;
        }
        let Some(name) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        if super::worker_exec::worker_type_of(&name).as_str() != WorkerTypeId::CHAT {
            continue;
        }
        workers += 1;
        let (i, r) = recover(state, &name).await;
        interrupted += i;
        resumed += r;
    }
    (workers, interrupted, resumed)
}

// ---------------------------------------------------------------------------
// History: read back from the canonical event stream.
// ---------------------------------------------------------------------------

pub(crate) fn read_messages(
    state: &AppState,
    name: &str,
    limit: i64,
    before: Option<i64>,
) -> Vec<Value> {
    let Ok(conn) = state.store.read() else {
        return vec![];
    };
    read_messages_conn(&conn, name, limit, before)
}

/// Same query as [`read_messages`], taking an already-open connection — for
/// callers that hold one (the fleet-list preview builder, `build_array`)
/// rather than going through `AppState::store`.
pub(crate) fn read_messages_conn(
    conn: &rusqlite::Connection,
    name: &str,
    limit: i64,
    before: Option<i64>,
) -> Vec<Value> {
    let before = before.unwrap_or(i64::MAX);
    let Ok(mut st) = conn.prepare(
        "SELECT id, ts, data FROM session_events WHERE session=?1 AND type=?2 AND id<?3 \
         ORDER BY id DESC LIMIT ?4",
    ) else {
        return vec![];
    };
    let rows = st.query_map(rusqlite::params![name, CHAT_EVENT, before, limit], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    });
    let mut out: Vec<Value> = match rows {
        Ok(it) => it
            .flatten()
            .map(|(id, ts, data)| {
                let mut v: Value = data
                    .and_then(|d| serde_json::from_str(&d).ok())
                    .unwrap_or_else(|| json!({}));
                v["id"] = json!(id);
                v["ts"] = json!(ts);
                v
            })
            .collect(),
        Err(_) => vec![],
    };
    out.reverse();
    out
}

/// Raw-text surrogate for the fleet-list card preview (ACW-9): a coding
/// worker's preview is a tmux capture; a chat worker has no pane at all, so
/// it gets the tail of its own transcript instead, rendered through
/// [`render_transcript`] — the SAME formatter the full chat history uses —
/// and fed into the SAME `preview_of` line-picker every other worker's
/// preview goes through (`sessions_legacy.rs`). One raw string in, one
/// mechanism, no second preview renderer to keep in sync.
pub(crate) fn preview_raw(conn: &rusqlite::Connection, name: &str) -> String {
    let messages = read_messages_conn(conn, name, 6, None);
    render_transcript(&messages)
}

fn render_transcript(messages: &[Value]) -> String {
    let mut s = String::new();
    for m in messages {
        let role = m["role"].as_str().unwrap_or("?");
        let text = m["text"].as_str().unwrap_or("");
        if !s.is_empty() {
            s.push('\n');
        }
        match (role, m["error"].as_str()) {
            ("assistant", Some(e)) => s.push_str(&format!("assistant (error): {e}\n")),
            _ => s.push_str(&format!("{role}:\n{text}\n")),
        }
    }
    s
}

fn qs_i64(q: &Option<String>, key: &str) -> Option<i64> {
    let params = super::fs::parse_qs(q.as_deref().unwrap_or(""));
    super::fs::qs_get(&params, key).and_then(|v| v.parse().ok())
}

/// `GET /api/sessions/{name}/chat?limit=&before=` — the chat renderer's data.
pub(crate) async fn history_route(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    RawQuery(q): RawQuery,
) -> Response {
    if !super::session_verbs::valid_session_name(&name) || !env_path(&name).exists() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "unknown worker"}))).into_response();
    }
    let limit = qs_i64(&q, "limit").unwrap_or(200).clamp(1, 1000);
    let before = qs_i64(&q, "before");
    let st = state.clone();
    let n = name.clone();
    let messages =
        tokio::task::spawn_blocking(move || read_messages(&st, &n, limit + 1, before))
            .await
            .unwrap_or_default();
    let has_more = messages.len() as i64 > limit;
    let messages: Vec<Value> = if has_more {
        messages[1..].to_vec()
    } else {
        messages
    };
    let lane = lane(&name);
    // Snapshot and cursor under one lock: the client resumes the stream from
    // exactly this point, so it neither misses nor repeats a delta.
    let (live, cursor, ring) = {
        let j = lane.journal.lock().unwrap();
        let (events, bytes) = j.retained();
        (
            j.live.clone(),
            format!("{}:{}", j.epoch, j.seq),
            json!({"retained_events": events, "retained_bytes": bytes, "evicted": j.evicted}),
        )
    };
    let queued = lane.queue.lock().unwrap().len();
    let wt = super::worker_exec::worker_type_of(&name);
    Json(json!({
        "name": name,
        "worker_type": wt,
        "renderer": wt.descriptor().renderer,
        "running": running(&name),
        "busy": lane.busy.load(Ordering::SeqCst),
        "queued": queued,
        "streaming": live.map(|a| json!(a)).unwrap_or(Value::Null),
        "cursor": cursor,
        "stream_ring": ring,
        "conversation_id": meta_str(&load_meta(&name), "chat_conversation_id"),
        "messages": messages,
        "has_more": has_more,
        "measured": true,
        "n_considered": messages.len(),
    }))
    .into_response()
}

/// `GET /api/sessions/{name}/chat/stream?after=<epoch:seq>`: live events
/// for one worker (SSE). Each event's SSE `id` is its `epoch:seq` cursor, so
/// a browser's automatic reconnect sends `Last-Event-ID` and resumes here
/// without duplicates. `?after=` does the same for a client that reconnects
/// by hand (the history endpoint returns the cursor its snapshot matches).
/// A cursor this process cannot serve yields a `gap` event: the client
/// refetches history instead of silently missing events.
pub(crate) async fn stream_route(
    AxumPath(name): AxumPath<String>,
    RawQuery(q): RawQuery,
    headers: axum::http::HeaderMap,
) -> Response {
    if !super::session_verbs::valid_session_name(&name) || !env_path(&name).exists() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "unknown worker"}))).into_response();
    }
    let params = super::fs::parse_qs(q.as_deref().unwrap_or(""));
    // Last-Event-ID wins: on an automatic reconnect the browser sends the
    // last event it applied, while the URL still carries the older `after`
    // the connection was first opened with.
    let cursor = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| super::fs::qs_get(&params, "after").map(str::to_string))
        .and_then(|c| parse_cursor(&c));
    let lane = lane(&name);
    // Subscribe and read the ring under the journal lock: nothing can be
    // published between the replay and the live subscription.
    let (rx, replay, head, epoch) = {
        let j = lane.journal.lock().unwrap();
        let rx = lane.tx.subscribe();
        let replay = match &cursor {
            Some((epoch, after)) => j.replay_after(epoch, *after),
            None => Replay::Events(vec![]),
        };
        (rx, replay, j.seq, j.epoch.clone())
    };
    let mut first: Vec<Event> = vec![Event::default()
        .data(json!({"type": "hello", "cursor": format!("{epoch}:{head}")}).to_string())];
    match replay {
        Replay::Events(evs) => {
            if !evs.is_empty() {
                tracing::debug!(session = %name, replayed = evs.len(), measured = true,
                    n_considered = evs.len(), verdict = "chat_stream_resumed",
                    "chat stream reconnect replayed missed events");
            }
            first.extend(evs.into_iter().map(|e| sse_event(&e)));
        }
        Replay::Gap(reason) => {
            tracing::info!(session = %name, reason, head, measured = true, n_considered = 1,
                verdict = "chat_stream_resume_gap",
                "chat stream reconnect could not be replayed; client told to refetch history");
            first.push(Event::default().data(json!({"type": "gap", "reason": reason}).to_string()));
        }
    }
    let n = name.clone();
    let live = futures::stream::unfold(rx, move |mut rx| {
        let n = n.clone();
        async move {
            let ev = match rx.recv().await {
                Ok(s) => sse_event(&s),
                // Told, with the count: the client refetches history.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(session = %n, missed, measured = true, n_considered = missed,
                        verdict = "chat_stream_lagged", "chat stream subscriber fell behind");
                    Event::default()
                        .data(json!({"type": "gap", "reason": "lagged", "missed": missed}).to_string())
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            };
            Some((Ok::<_, std::convert::Infallible>(ev), rx))
        }
    });
    let head = futures::stream::iter(first.into_iter().map(Ok::<_, std::convert::Infallible>));
    Sse::new(head.chain(live))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(10))
                .event(Event::default().data(json!({"type": "ping"}).to_string())),
        )
        .into_response()
}

/// One stamped event as SSE, with its cursor as the event id.
fn sse_event(serialized: &str) -> Event {
    let id = serde_json::from_str::<Value>(serialized)
        .ok()
        .map(|v| format!("{}:{}", v["epoch"].as_str().unwrap_or(""), v["seq"].as_u64().unwrap_or(0)));
    let ev = Event::default().data(serialized);
    match id {
        Some(id) => ev.id(id),
        None => ev,
    }
}

/// `POST /api/sessions/{name}/chat/interrupt`: stop the running turn and
/// keep the worker (and its queue). The chat equivalent of pressing Escape in
/// a terminal worker. The partial reply is kept and marked interrupted.
pub(crate) async fn interrupt_route(AxumPath(name): AxumPath<String>) -> Response {
    if !super::session_verbs::valid_session_name(&name) || !env_path(&name).exists() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "unknown worker"}))).into_response();
    }
    let (interrupted, detail) = interrupt_turn(&name);
    Json(json!({"ok": true, "interrupted": interrupted, "detail": detail})).into_response()
}

/// Signal the running turn's provider; the turn ends as `interrupted`.
fn interrupt_turn(name: &str) -> (bool, String) {
    let lane = lane(name);
    let pid = *lane.pid.lock().unwrap();
    let Some(pid) = pid else {
        return (false, "no turn running".into());
    };
    lane.interrupt.store(true, Ordering::SeqCst);
    // SAFETY: plain signal to a child this process spawned.
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    tracing::info!(session = %name, pid, measured = true, n_considered = 1,
        verdict = "chat_turn_interrupt_requested", "owner interrupted a chat turn");
    (true, "turn interrupted".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_id_is_uuid_v4_shaped() {
        let id = new_conversation_id();
        let re = regex::Regex::new(
            r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$",
        )
        .unwrap();
        assert!(re.is_match(&id), "{id}");
    }

    #[test]
    fn turn_args_resume_and_prompt_never_in_argv() {
        let fresh = turn_args("claude", "--model claude-haiku-4-5 --dangerously-skip-permissions", "", "c-1", true);
        assert!(fresh.windows(2).any(|w| w == ["--session-id", "c-1"]));
        assert!(fresh.windows(2).any(|w| w == ["--model", "claude-haiku-4-5"]));
        assert!(fresh.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(
            fresh.windows(2).any(|w| w == ["--allowedTools", "Bash(amux:*)"]),
            "the harness CLI must be usable from a headless turn"
        );
        let resumed = turn_args("claude", "", "", "c-1", false);
        assert!(resumed.windows(2).any(|w| w == ["--resume", "c-1"]));
        let codex = turn_args("codex", "--model gpt-5", "", "th-1", false);
        assert_eq!(codex.last().unwrap(), "-");
        assert!(codex.windows(2).any(|w| w == ["resume", "th-1"]));
    }

    /// A bare `session_events` table with only the columns `read_messages_conn`
    /// actually reads — the fixture stays self-contained rather than pulling in
    /// the full migration set for one indexed SELECT.
    fn chat_events_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session_events (id INTEGER PRIMARY KEY, ts REAL, session TEXT, type TEXT, data TEXT);",
        )
        .unwrap();
        conn
    }

    fn insert_chat_event(conn: &rusqlite::Connection, session: &str, ts: f64, data: &Value) {
        conn.execute(
            "INSERT INTO session_events(ts, session, type, data) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![ts, session, CHAT_EVENT, data.to_string()],
        )
        .unwrap();
    }

    #[test]
    fn read_messages_conn_returns_oldest_first() {
        let conn = chat_events_conn();
        insert_chat_event(&conn, "w1", 2.0, &json!({"role": "assistant", "text": "second"}));
        insert_chat_event(&conn, "w1", 1.0, &json!({"role": "user", "text": "first"}));
        // A different session's row must never leak into this worker's tail.
        insert_chat_event(&conn, "w2", 3.0, &json!({"role": "user", "text": "not mine"}));
        let msgs = read_messages_conn(&conn, "w1", 10, None);
        let texts: Vec<&str> = msgs.iter().map(|m| m["text"].as_str().unwrap()).collect();
        // Rows are inserted DESC-id, so id 1 ("second") precedes id 2
        // ("first") in insertion order but the QUERY is `ORDER BY id DESC`
        // then reversed — chronological (insertion) order out, id 1 first.
        assert_eq!(texts, vec!["second", "first"], "{msgs:?}");
    }

    #[test]
    fn preview_raw_renders_the_transcript_tail_for_the_card_preview() {
        // ACW-9: a chat worker has no tmux pane, so its fleet-list card
        // preview comes from here instead of a pane capture — this is the
        // raw text `preview_of` (sessions_legacy.rs) then picks a line from,
        // exactly the same as any terminal worker's captured pane text.
        let conn = chat_events_conn();
        insert_chat_event(&conn, "chatty", 1.0, &json!({"role": "user", "text": "how many LOC?"}));
        insert_chat_event(
            &conn,
            "chatty",
            2.0,
            &json!({"role": "assistant", "text": "About 400,000 lines."}),
        );
        let raw = preview_raw(&conn, "chatty");
        assert!(raw.contains("user:\nhow many LOC?"), "{raw}");
        assert!(raw.contains("assistant:\nAbout 400,000 lines."), "{raw}");
        // Empty for a worker with no chat history at all — the caller
        // (`sessions_legacy.rs`) treats blank as "nothing to show", not as a
        // worker that failed to answer.
        assert_eq!(preview_raw(&conn, "no-such-worker"), "");
    }
}
