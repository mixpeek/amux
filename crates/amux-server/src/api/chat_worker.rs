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
//! Streaming: token deltas go to a per-worker broadcast channel served as SSE
//! at `/api/sessions/{name}/chat/stream`. Only the finished message is
//! persisted; a client that connects mid-turn gets the partial text from the
//! history endpoint's `streaming` field.
//!
//! Log signals: `verdict="chat_turn_completed"` / `"chat_turn_failed"` per
//! turn with duration and the provider exit, and
//! `verdict="chat_conversation_reset"` when a stale resume id is replaced.

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
use std::sync::atomic::{AtomicBool, Ordering};
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

pub(crate) struct ChatAdapter;

struct Queued {
    text: String,
    origin: &'static str,
}

/// In-memory runtime of one chat worker. Nothing here is durable state: the
/// conversation id lives in the worker meta, messages in `session_events`.
struct Lane {
    busy: AtomicBool,
    queue: Mutex<VecDeque<Queued>>,
    tx: broadcast::Sender<Value>,
    pid: Mutex<Option<u32>>,
    /// (turn_id, text so far) of the turn in flight.
    partial: Mutex<(String, String)>,
}

static LANES: LazyLock<Mutex<HashMap<String, Arc<Lane>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lane(name: &str) -> Arc<Lane> {
    let mut g = LANES.lock().unwrap();
    g.entry(name.to_string())
        .or_insert_with(|| {
            Arc::new(Lane {
                busy: AtomicBool::new(false),
                queue: Mutex::new(VecDeque::new()),
                tx: broadcast::channel(512).0,
                pid: Mutex::new(None),
                partial: Mutex::new((String::new(), String::new())),
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
    let meta = load_meta(name);
    let run_id = meta_str(&meta, "chat_run_id");
    if run_id.is_empty() {
        return;
    }
    let now = crate::config::now_f64();
    let body = json!({
        "native_status": true,
        "run_id": run_id,
        "provider": provider_of(name),
        "sequence": (now * 1000.0) as u64,
        "event_ts": now,
        "state": st,
        "event": event,
        "turn_id": turn,
        "worker_type": WorkerTypeId::CHAT,
    });
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
        let _ = lane.tx.send(json!({"type": "stopped", "dropped_queued": dropped}));
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
                },
            });
            q.len() - 1
        };
        let turn_running = lane.busy.swap(true, Ordering::SeqCst);
        if !turn_running {
            let st = state.clone();
            let n = name.to_string();
            tokio::spawn(async move { pump(st, n).await });
        }
        let _ = lane.tx.send(json!({"type": "queued", "ahead": ahead, "busy": turn_running}));
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

    async fn peek(&self, state: &AppState, name: &str, lines: i64) -> Dispatch<Value> {
        let messages = read_messages(state, name, 200, None);
        let history = render_transcript(&messages);
        let (turn, partial) = lane(name).partial.lock().unwrap().clone();
        let mut all: Vec<&str> = history.lines().collect();
        let history_lines = all.len();
        if !turn.is_empty() && !partial.is_empty() {
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
        let next = lane.queue.lock().unwrap().pop_front();
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

/// What one provider invocation produced.
#[derive(Default)]
struct TurnOutcome {
    text: String,
    tools: Vec<String>,
    conversation_id: String,
    cost_usd: Option<f64>,
    error: Option<String>,
    model: String,
}

/// Parse one stdout line into the outcome; returns a text delta to stream.
fn absorb_line(provider: &str, line: &str, out: &mut TurnOutcome, block: &mut i64) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    if provider == "codex" {
        match v["type"].as_str()? {
            "thread.started" => {
                out.conversation_id = v["thread_id"].as_str().unwrap_or("").to_string();
            }
            "item.started" | "item.completed" => {
                let item = &v["item"];
                match item["type"].as_str().unwrap_or("") {
                    "agent_message" if v["type"] == "item.completed" => {
                        let t = item["text"].as_str().unwrap_or("");
                        let delta = if out.text.is_empty() {
                            t.to_string()
                        } else {
                            format!("\n\n{t}")
                        };
                        out.text.push_str(&delta);
                        return Some(delta);
                    }
                    "command_execution" if v["type"] == "item.started" => {
                        out.tools.push("shell".into());
                    }
                    "mcp_tool_call" | "web_search" if v["type"] == "item.started" => {
                        out.tools.push(item["type"].as_str().unwrap_or("").to_string());
                    }
                    _ => {}
                }
            }
            "turn.failed" | "error" => {
                let msg = v["error"]["message"]
                    .as_str()
                    .or_else(|| v["message"].as_str())
                    .unwrap_or("codex turn failed");
                out.error = Some(msg.to_string());
            }
            _ => {}
        }
        return None;
    }
    match v["type"].as_str()? {
        "system" if v["subtype"] == "init" => {
            out.conversation_id = v["session_id"].as_str().unwrap_or("").to_string();
            out.model = v["model"].as_str().unwrap_or("").to_string();
        }
        "stream_event" => {
            let ev = &v["event"];
            match ev["type"].as_str().unwrap_or("") {
                "content_block_start" => {
                    let cb = &ev["content_block"];
                    if cb["type"] == "tool_use" {
                        out.tools.push(cb["name"].as_str().unwrap_or("tool").to_string());
                    }
                }
                "content_block_delta" if ev["delta"]["type"] == "text_delta" => {
                    let t = ev["delta"]["text"].as_str().unwrap_or("");
                    // Separate text blocks (e.g. before and after a tool call)
                    // with a blank line, the way a transcript would show them.
                    let idx = ev["index"].as_i64().unwrap_or(0);
                    let mut delta = String::new();
                    if !out.text.is_empty() && idx != *block {
                        delta.push_str("\n\n");
                    }
                    *block = idx;
                    delta.push_str(t);
                    out.text.push_str(&delta);
                    return Some(delta);
                }
                "message_start" => {
                    // New message: its block indexes restart at 0.
                    *block = -1;
                }
                _ => {}
            }
        }
        "result" => {
            if let Some(id) = v["session_id"].as_str() {
                out.conversation_id = id.to_string();
            }
            out.cost_usd = v["total_cost_usd"].as_f64();
            if v["is_error"].as_bool() == Some(true) {
                out.error = Some(
                    v["result"]
                        .as_str()
                        .unwrap_or("provider reported an error")
                        .to_string(),
                );
            } else if out.text.is_empty() {
                // Non-streaming provider build: the result carries the text.
                out.text = v["result"].as_str().unwrap_or("").to_string();
            }
        }
        _ => {}
    }
    None
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
    emit_event(
        state,
        name,
        CHAT_EVENT,
        Some(json!({"role": "user", "text": q.text, "turn_id": turn_id, "origin": q.origin})),
        Some(format!("chat:{turn_id}:user")),
        SOURCE,
    )
    .await;
    let _ = lane.tx.send(json!({
        "type": "user", "turn_id": turn_id, "text": q.text, "origin": q.origin,
        "ts": crate::config::now_f64(),
    }));
    *lane.partial.lock().unwrap() = (turn_id.clone(), String::new());
    report_state(state, name, "active", "UserPromptSubmit", &turn_id).await;
    update_meta(name, &[("last_send", json!(crate::config::now_f64() as i64))]);

    let mut out = execute(name, &provider, &q.text, lane, &turn_id, false).await;
    // A resume id the provider no longer has (conversation deleted, new
    // machine): start a fresh conversation once rather than failing forever.
    if out
        .error
        .as_deref()
        .is_some_and(|e| e.contains("No conversation found") || e.contains("not found"))
        && out.text.is_empty()
    {
        tracing::warn!(session = %name, verdict = "chat_conversation_reset",
            reason = out.error.as_deref().unwrap_or(""),
            "chat worker's stored conversation could not be resumed; starting a new one");
        update_meta(
            name,
            &[
                ("chat_conversation_id", json!("")),
                ("cc_conversation_id", json!("")),
                ("chat_codex_thread", json!("")),
            ],
        );
        out = execute(name, &provider, &q.text, lane, &turn_id, true).await;
    }
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
    let msg = json!({
        "role": "assistant",
        "text": out.text,
        "turn_id": turn_id,
        "provider": provider,
        "model": out.model,
        "tools": out.tools,
        "cost_usd": out.cost_usd,
        "duration_ms": duration_ms,
        "error": out.error,
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
    *lane.partial.lock().unwrap() = (String::new(), String::new());
    let _ = lane.tx.send(json!({"type": "done", "turn_id": turn_id, "message": msg}));
    match &out.error {
        None => tracing::info!(session = %name, turn = %turn_id, duration_ms,
            chars = out.text.len(), tools = out.tools.len(), measured = true, n_considered = 1,
            verdict = "chat_turn_completed", "chat turn completed"),
        Some(e) => tracing::warn!(session = %name, turn = %turn_id, duration_ms, error = %e,
            measured = true, n_considered = 1, verdict = "chat_turn_failed", "chat turn failed"),
    }
    let (st, ev) = if out.error.is_some() {
        ("error", "StopFailure")
    } else {
        ("idle", "Stop")
    };
    report_state(state, name, st, ev, &turn_id).await;
    crate::api::sessions_legacy::invalidate_sessions_cache();
}

/// Spawn one provider turn and stream its output.
async fn execute(
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
    let mut out = TurnOutcome::default();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            out.error = Some(format!("could not start {provider}: {e}"));
            return out;
        }
    };
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
    let mut block = -1i64;
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match tokio::time::timeout(TURN_IDLE_TIMEOUT, lines.next_line()).await {
                Ok(Ok(Some(line))) => {
                    let tools_before = out.tools.len();
                    if let Some(delta) = absorb_line(provider, &line, &mut out, &mut block) {
                        lane.partial.lock().unwrap().1.push_str(&delta);
                        let _ = lane
                            .tx
                            .send(json!({"type": "delta", "turn_id": turn_id, "text": delta}));
                    }
                    for tool in &out.tools[tools_before..] {
                        let _ = lane
                            .tx
                            .send(json!({"type": "tool", "turn_id": turn_id, "name": tool}));
                    }
                }
                Ok(_) => break,
                Err(_) => {
                    let _ = child.start_kill();
                    out.error = Some(format!(
                        "no output for {}s; turn killed",
                        TURN_IDLE_TIMEOUT.as_secs()
                    ));
                    break;
                }
            }
        }
    }
    let status = child.wait().await.ok();
    *lane.pid.lock().unwrap() = None;
    let err_text = stderr_task.await.unwrap_or_default();
    if out.error.is_none() && !status.is_some_and(|s| s.success()) {
        let tail: String = err_text
            .lines()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        out.error = Some(if tail.trim().is_empty() {
            format!("{provider} exited with {:?}", status.and_then(|s| s.code()))
        } else {
            tail
        });
    }
    out
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
    let (turn, partial) = lane.partial.lock().unwrap().clone();
    let queued = lane.queue.lock().unwrap().len();
    let wt = super::worker_exec::worker_type_of(&name);
    Json(json!({
        "name": name,
        "worker_type": wt,
        "renderer": wt.descriptor().renderer,
        "running": running(&name),
        "busy": lane.busy.load(Ordering::SeqCst),
        "queued": queued,
        "streaming": if turn.is_empty() { Value::Null } else { json!({"turn_id": turn, "text": partial}) },
        "conversation_id": meta_str(&load_meta(&name), "chat_conversation_id"),
        "messages": messages,
        "has_more": has_more,
        "measured": true,
        "n_considered": messages.len(),
    }))
    .into_response()
}

/// `GET /api/sessions/{name}/chat/stream` — live deltas for one worker (SSE).
pub(crate) async fn stream_route(AxumPath(name): AxumPath<String>) -> Response {
    if !super::session_verbs::valid_session_name(&name) || !env_path(&name).exists() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "unknown worker"}))).into_response();
    }
    let rx = lane(&name).tx.subscribe();
    let hello = futures::stream::once(async move {
        Ok::<_, std::convert::Infallible>(Event::default().data(json!({"type": "hello"}).to_string()))
    });
    let live = futures::stream::unfold(rx, |mut rx| async move {
        let v = match rx.recv().await {
            Ok(v) => v,
            // Told, with the count: the client refetches history.
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                json!({"type": "lagged", "missed": missed})
            }
            Err(broadcast::error::RecvError::Closed) => return None,
        };
        Some((Ok(Event::default().data(v.to_string())), rx))
    });
    Sse::new(hello.chain(live))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(10))
                .event(Event::default().data(json!({"type": "ping"}).to_string())),
        )
        .into_response()
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
    fn claude_stream_json_streams_text_and_captures_the_turn() {
        let mut out = TurnOutcome::default();
        let mut block = -1;
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"s-1","model":"claude-haiku-4-5"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start"}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"x"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hi "}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"there"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","name":"Bash"}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_start"}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"session_id":"s-1","total_cost_usd":0.01,"result":"done"}"#,
        ];
        let deltas: Vec<String> = lines
            .iter()
            .filter_map(|l| absorb_line("claude", l, &mut out, &mut block))
            .collect();
        assert_eq!(deltas, vec!["hi ", "there", "\n\ndone"]);
        assert_eq!(out.text, "hi there\n\ndone");
        assert_eq!(out.tools, vec!["Bash"]);
        assert_eq!(out.conversation_id, "s-1");
        assert_eq!(out.model, "claude-haiku-4-5");
        assert_eq!(out.cost_usd, Some(0.01));
        assert!(out.error.is_none());
    }

    #[test]
    fn claude_error_result_is_an_error_not_a_reply() {
        let mut out = TurnOutcome::default();
        let mut block = -1;
        absorb_line(
            "claude",
            r#"{"type":"result","is_error":true,"result":"No conversation found with session ID: x"}"#,
            &mut out,
            &mut block,
        );
        assert!(out.text.is_empty());
        assert!(out.error.unwrap().contains("No conversation found"));
    }

    #[test]
    fn codex_json_events_map_to_the_same_outcome() {
        let mut out = TurnOutcome::default();
        let mut block = -1;
        let lines = [
            r#"{"type":"thread.started","thread_id":"th-9"}"#,
            r#"{"type":"item.started","item":{"type":"command_execution","command":"ls"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"first"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"second"}}"#,
        ];
        for l in lines {
            absorb_line("codex", l, &mut out, &mut block);
        }
        assert_eq!(out.text, "first\n\nsecond");
        assert_eq!(out.conversation_id, "th-9");
        assert_eq!(out.tools, vec!["shell"]);
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
}
