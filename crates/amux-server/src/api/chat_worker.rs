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
//! turn with duration and the provider exit,
//! `verdict="chat_conversation_reset"` when a stale resume id is replaced, and
//! across a server restart `chat_turn_reattached` (boot found a detached turn
//! to follow), `chat_turn_resumed` (it finished and was recorded) or
//! `chat_turn_lost` (nothing to re-attach to, with the reason).

use super::chat_stream::{parse_cursor, Assembly, Journal, Parser, Replay};
use super::session_verbs::{emit_event, env_path, home, meta_i64, meta_str, sh_quote, SendOrigin};
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
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;

/// The `session_events.type` every chat message is stored under.
pub(crate) const CHAT_EVENT: &str = "chat.message";
const SOURCE: &str = "chat-adapter";

// ---------------------------------------------------------------------------
// COMPANION LANES: a worker's Chat tab.
//
// Ethan, 2026-09-30, on a screenshot of `mixpeek-override-chat`: "looks like u
// make a dedicated worker for the chat - this isn't correct." AMUX-5350 made
// the Chat tab a whole companion worker, `<worker>-chat`, so it showed up in
// the fleet with its own board, schedules, steering and messages, its first
// message was captured as a board card, and every worker-facing feature needed
// a special case to leave it out (board_drive, messages, the owner relay).
//
// The chat now belongs to the worker. Its lane key is `<worker>@chat`, which
// can never be a worker name (`@` is not allowed in one, see
// `valid_session_name`), so nothing that lists workers can see it. Its
// settings are derived from the worker, and its state lives in
// `chat/companions/<worker>.meta.json`, never in `sessions/`. Everything below
// that reads `parse_env`, `load_meta` or `update_meta` goes through the local
// versions here, which is the whole seam: the engine itself is unchanged.
// ---------------------------------------------------------------------------

/// Suffix that turns a worker name into its Chat tab's lane key.
pub(crate) const COMPANION_SUFFIX: &str = "@chat";

pub(crate) fn companion_key(worker: &str) -> String {
    format!("{worker}{COMPANION_SUFFIX}")
}

/// The worker a lane key is the Chat tab of, or None for any other key.
pub(crate) fn companion_parent(key: &str) -> Option<&str> {
    key.strip_suffix(COMPANION_SUFFIX)
        .filter(|w| super::session_verbs::valid_session_name(w))
}

fn companions_dir() -> std::path::PathBuf {
    home().join("chat").join("companions")
}

fn companion_meta_path(worker: &str) -> std::path::PathBuf {
    companions_dir().join(format!("{worker}.meta.json"))
}

/// A chat lane's settings. A companion's are derived from its worker: the
/// worker's directory, Claude, and the model in the worker's
/// `AMUX_CHAT_MODEL` scope setting (default sonnet, what AMUX-5350 created).
fn parse_env(name: &str) -> super::session_verbs::EnvFile {
    let Some(worker) = companion_parent(name) else {
        return super::session_verbs::parse_env(name);
    };
    let parent = super::session_verbs::parse_env(worker);
    let mut env = super::session_verbs::EnvFile::default();
    let dir = parent.get_or("CC_DIR", "").trim().to_string();
    if !dir.is_empty() {
        env.set("CC_DIR", &dir);
    }
    let model = super::session_verbs::scoped_setting_in(&home(), worker, "AMUX_CHAT_MODEL")
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "sonnet".into());
    env.set("CC_PROVIDER", "claude");
    env.set("CC_FLAGS", &format!("--model {}", model.trim()));
    env.set("CC_WORKER_TYPE", WorkerTypeId::CHAT);
    env.set("CC_COMPANION_OF", worker);
    env
}

/// A chat lane's meta: the worker meta, or a companion's own file.
pub(crate) fn load_meta(name: &str) -> serde_json::Map<String, Value> {
    let Some(worker) = companion_parent(name) else {
        return super::session_verbs::load_meta(name);
    };
    std::fs::read_to_string(companion_meta_path(worker))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn update_meta(name: &str, updates: &[(&str, Value)]) {
    let Some(worker) = companion_parent(name) else {
        return super::session_verbs::update_meta(name, updates);
    };
    let mut meta = load_meta(name);
    for (k, v) in updates {
        meta.insert((*k).to_string(), v.clone());
    }
    let path = companion_meta_path(worker);
    let _ = std::fs::create_dir_all(companions_dir());
    let tmp = path.with_extension(format!("json.{}.tmp", ulid::Ulid::new()));
    let body = serde_json::to_string_pretty(&Value::Object(meta)).unwrap_or_default();
    if std::fs::write(&tmp, body).is_err() || std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!(session = %name, verdict = "chat_companion_meta_write_failed",
            "a worker's Chat tab state could not be written");
    }
}

/// The session routes for `<worker>@chat` (AMUX-5350 redesign). Only `send`
/// is served: the Chat tab's composer posts there through the dashboard's
/// normal outbox, so offline retries carry the same `msg_id` and are answered
/// from the dedupe list instead of running the turn twice. A GET on `send`
/// with `msg_id` is the outbox's receipt check. Anything else names the
/// worker to use instead, since the Chat tab is not a worker.
pub(crate) async fn companion_route(
    state: &AppState,
    worker: &str,
    is_post: bool,
    action: &str,
    receipt_msg_id: Option<&str>,
    body: &Value,
    caller: &str,
) -> Response {
    let key = companion_key(worker);
    if !env_path(worker).exists() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("worker '{worker}' not found")}))).into_response();
    }
    if action != "send" {
        return (StatusCode::NOT_FOUND, Json(json!({
            "error": format!("{key} is {worker}'s Chat tab, not a worker; use /api/sessions/{worker}"),
        }))).into_response();
    }
    let seen: Vec<String> = load_meta(&key)
        .get("chat_seen_msg_ids")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    if !is_post {
        let id = receipt_msg_id.unwrap_or("").trim();
        return if !id.is_empty() && seen.iter().any(|m| m == id) {
            (StatusCode::OK, Json(json!({"ok": true, "accepted": true, "msg_id": id, "id": format!("chat:{id}")}))).into_response()
        } else {
            (StatusCode::NOT_FOUND, Json(json!({"ok": false, "accepted": false, "msg_id": id}))).into_response()
        };
    }
    let msg_id: String = body.get("msg_id").and_then(Value::as_str).unwrap_or("").trim().chars().take(64).collect();
    if !msg_id.is_empty() && seen.contains(&msg_id) {
        return (StatusCode::OK, Json(json!({"ok": true, "msg_id": msg_id, "message": "already delivered", "deduped": true}))).into_response();
    }
    let text = body.get("text").and_then(Value::as_str).unwrap_or("").to_string();
    if text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": "empty message"}))).into_response();
    }
    // WHO IS ASKING (AMUX-5432, found 2026-10-01): a send carrying a lane's
    // session header is that lane, not the owner. Every Chat-tab send used to
    // be recorded as the owner's, so a helper's test message drew an
    // "[amux accountability] ... no board card" nudge as if Ethan had asked.
    // The dashboard sends with no session header and stays the owner's.
    let from_owner = caller.trim().is_empty();
    let origin = if from_owner { SendOrigin::Owner } else { SendOrigin::Automation };
    if !from_owner {
        tracing::info!(session = %key, caller, verdict = "chat_companion_send_from_lane",
            "a lane sent to a Chat tab; recorded as that lane, not the owner");
    }
    let id_for_turn = (!msg_id.is_empty()).then(|| msg_id.clone());
    let Dispatch::Handled((ok, message)) = ChatAdapter.deliver_with_id(state, &key, &text, origin, id_for_turn).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": "chat adapter did not handle the send"}))).into_response();
    };
    if !ok {
        tracing::warn!(session = %key, error = %message, verdict = "chat_companion_send_failed", "a Chat tab send was not accepted");
        return (StatusCode::CONFLICT, Json(json!({"ok": false, "error": message, "msg_id": msg_id}))).into_response();
    }
    if !msg_id.is_empty() {
        let mut keep = seen;
        keep.push(msg_id.clone());
        let start = keep.len().saturating_sub(64);
        update_meta(&key, &[("chat_seen_msg_ids", json!(keep[start..]))]);
    }
    super::session_verbs::cmd_hist_record_full(
        state,
        worker,
        &text,
        "chat",
        if from_owner { "chat-companion" } else { caller },
        true,
        super::session_verbs::DeliveryMeta::default(),
    )
    .await;
    tracing::info!(session = %key, worker, verdict = "chat_companion_send", "Chat tab message accepted");
    (StatusCode::OK, Json(json!({"ok": true, "msg_id": msg_id, "message": message, "submitted": true, "submission": "confirmed"}))).into_response()
}

/// Does this chat lane exist? A chat worker has an env file; a Chat tab
/// exists whenever its worker does.
pub(crate) fn lane_exists(name: &str) -> bool {
    match companion_parent(name) {
        Some(worker) => env_path(worker).exists(),
        None => super::session_verbs::valid_session_name(name) && env_path(name).exists(),
    }
}
/// A turn that has produced nothing for this long is killed and recorded as
/// failed, so a hung provider cannot pin a worker `working` forever.
const TURN_IDLE_TIMEOUT: Duration = Duration::from_secs(900);
/// How often a running turn re-asserts `active` (well under the 120s the
/// status projection trusts a native report for).
const HEARTBEAT: Duration = Duration::from_secs(45);

/// Queue a finished chat delegate's answer into a Chat (AMUX-5432), as a
/// message with origin `delegate`, so the Chat relays it to the owner. Only a
/// RUNNING Chat gets a turn; the caller records it in the transcript otherwise.
pub(crate) async fn deliver_delegate_result(state: &AppState, name: &str, text: &str) -> bool {
    if !running(name) || text.trim().is_empty() {
        return false;
    }
    let lane = lane(name);
    {
        let mut q = lane.queue.lock().unwrap();
        q.push_back(Queued { text: text.to_string(), origin: "delegate".to_string(), msg_id: None });
        persist_queue(name, &q);
    }
    if !lane.busy.swap(true, Ordering::SeqCst) {
        let st = state.clone();
        let n = name.to_string();
        tokio::spawn(async move { pump(st, n).await });
    }
    lane.publish(json!({"type": "queued", "ahead": 0, "waiting": 0, "busy": true}));
    true
}

pub(crate) struct ChatAdapter;

impl ChatAdapter {
    /// `deliver`, carrying the dashboard send's `msg_id` onto the recorded
    /// user message (see `Queued::msg_id`).
    pub(crate) async fn deliver_with_id(
        &self,
        state: &AppState,
        name: &str,
        text: &str,
        origin: SendOrigin,
        msg_id: Option<String>,
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
                msg_id: msg_id.filter(|m| !m.is_empty()),
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
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Queued {
    text: String,
    origin: String,
    /// The dashboard send's `msg_id`, carried onto the recorded user message so
    /// the client reconciles its pending copy BY ID (2026-10-01: the Chat tab
    /// showed the server's message and the outbox's pending copy at once).
    /// Absent on queues persisted before this field existed.
    #[serde(default)]
    msg_id: Option<String>,
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
    // A Chat tab is not a worker: it has no fleet status of its own, and its
    // live state reaches the tab through the chat stream.
    if companion_parent(name).is_some() {
        return;
    }
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
        if companion_parent(name).is_some() {
            // No launch record, no session.started event and no steering
            // sweep: those belong to workers.
            update_meta(
                name,
                &[
                    ("chat_running", json!(true)),
                    ("cc_cwd", json!(dir)),
                    ("last_started", json!(crate::config::now_f64() as i64)),
                ],
            );
            return Dispatch::Handled((true, "chat ready".into()));
        }
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
            signal_turn(pid);
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
        self.deliver_with_id(state, name, text, origin, None).await
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
    // READ-ONLY TOOLS TOO (Ethan, 2026-10-07, mixpeek-override's chat: "what
    // is the progress of gs12?" got "I couldn't pull a fresher pace reading").
    // With only Bash(amux:*) a pipe through grep and any read of a log was
    // refused, so the companion could not look things up. Reading cannot
    // change anything, so Read, Grep and Glob are allowed as well.
    a.extend(["--allowedTools".into(), "Read,Grep,Glob".into()]);
    a.extend(["--add-dir".into(), home().join("logs").to_string_lossy().into_owned()]);
    // Files the owner attaches in the Chat tab are saved under the amux
    // uploads folder, outside the worker's directory, so a headless turn was
    // refused them with no way to grant it (2026-10-04, mixpeek-override's
    // chat: "couldn't open the invoice PDF ... permission ... denied").
    a.extend(["--add-dir".into(), home().join("uploads").to_string_lossy().into_owned()]);
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
        Some(json!({"role": "user", "text": q.text, "turn_id": turn_id, "origin": q.origin, "msg_id": q.msg_id})),
        Some(format!("chat:{turn_id}:user")),
        SOURCE,
    )
    .await;
    let waiting = lane.queue.lock().unwrap().len();
    lane.publish(json!({
        "type": "user", "turn_id": turn_id, "text": q.text, "origin": q.origin, "msg_id": q.msg_id,
        "ts": crate::config::now_f64(), "waiting": waiting,
    }));
    update_meta(
        name,
        &[
            ("chat_inflight_turn", json!(turn_id)),
            // Who started this turn, and with what words: a companion's send
            // during an owner turn is delivered as the owner's (owner_relay_quote).
            ("chat_inflight_origin", json!(q.origin)),
            ("chat_inflight_text", json!(q.text.chars().take(300).collect::<String>())),
            ("chat_inflight_started", json!(crate::config::now_f64())),
        ],
    );
    report_state(state, name, "active", "UserPromptSubmit", &turn_id).await;
    update_meta(name, &[("last_send", json!(crate::config::now_f64() as i64))]);

    let mut out = execute(state, name, &provider, &q.text, lane, &turn_id, false).await;
    // A resume id the provider no longer has, or a conversation stuck in a
    // state the headless adapter cannot satisfy (deferred tool, stale marker):
    // start a fresh conversation once rather than failing forever.
    let unresumable = |e: &str| {
        e.contains("No conversation found")
            || e.contains("not found")
            || e.contains("deferred tool marker")
            || e.contains("no stdin data received")
    };
    if out
        .asm
        .error
        .as_deref()
        .is_some_and(unresumable)
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
        TurnFiles::new(&turn_id, 0).remove();
        out = execute(state, name, &provider, &q.text, lane, &turn_id, true).await;
    }
    let duration_ms = started.elapsed().as_millis() as u64;
    finish_turn(state, name, lane, &turn_id, &provider, out, duration_ms).await;
}

/// Persist and report a finished turn: the one path a live turn and a turn
/// re-attached after a restart share, so both record the reply the same way.
async fn finish_turn(
    state: &AppState,
    name: &str,
    lane: &Lane,
    turn_id: &str,
    provider: &str,
    out: TurnOutcome,
    duration_ms: u64,
) {
    let turn_id = turn_id.to_string();
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
    let stem = meta_str(&load_meta(name), "chat_inflight_files");
    update_meta(
        name,
        &[
            ("chat_inflight_turn", json!("")),
            ("chat_inflight_origin", json!("")),
            ("chat_inflight_text", json!("")),
            ("chat_inflight_files", json!("")),
            ("chat_inflight_pgid", json!(0)),
            ("chat_inflight_provider", json!("")),
            ("chat_inflight_started", json!(0)),
        ],
    );
    // Removed only after the reply is persisted and the marker cleared, so a
    // restart between the two can still re-attach and record it. By turn id as
    // well as the marker: a worker deleted mid-turn has no meta left to name
    // them (seen live 2026-10-01: the stopped turn's files stayed behind).
    if !stem.is_empty() {
        TurnFiles::from_stem(&stem).remove();
    }
    for attempt in 0..=1 {
        TurnFiles::new(&turn_id, attempt).remove();
    }
    lane.publish(json!({"type": "done", "turn_id": turn_id, "message": msg}));
    match &error {
        None => tracing::info!(session = %name, turn = %turn_id, duration_ms,
            chars = a.text.len(), tools = a.tools.len(), interrupted, measured = true,
            n_considered = 1, verdict = if interrupted { "chat_turn_interrupted_by_owner" } else { "chat_turn_completed" },
            "chat turn completed"),
        Some(e) => tracing::warn!(session = %name, turn = %turn_id, duration_ms, error = %e,
            limited, measured = true, n_considered = 1, verdict = "chat_turn_failed", "chat turn failed"),
    }
    // A companion that ran no tool and SAYS it did not check skipped the
    // escalation ladder (primis@chat, 2026-10-06: asked why a mention had no
    // Wikidata item, it answered from general knowledge with tools=0 and
    // "I haven't checked the worker's output"). Counted so a sweep can see it.
    if error.is_none() && !interrupted && a.tools.is_empty() && reply_admits_unchecked(&a.text) {
        tracing::warn!(session = %name, turn = %turn_id, measured = true, n_considered = 1,
            verdict = "companion_answered_unchecked",
            "chat companion answered without a tool call and said it had not checked; the escalation ladder was skipped");
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

/// What a companion is told before the owner's message (AMUX-5350, Ethan
/// 2026-09-29: "a chat with core context from the worker (not the full logs
/// but anything that u think would be valuable)"; "I want to be able to ask in
/// plain text the status of work ... still be able to direct work"). Computed,
/// not summarised by a model: status, the card it is on, what waits on the
/// owner, board counts and trend, git state and its last reply. The companion
/// reaches deeper with the amux CLI and directs the worker with `amux send`.
pub(crate) async fn companion_prompt(state: &AppState, worker: &str, fresh: bool, text: &str) -> String {
    let wenv = parse_env(worker);
    let dir = wenv.get_or("CC_DIR", "").to_string();
    let mut out = String::new();
    if fresh {
        out.push_str(&format!(
            "You are the chat companion for the amux worker `{worker}` (working directory {dir}). \
             The owner talks to you in plain language about that worker's work. Answer conversationally \
             and briefly unless asked for detail. Each message starts with a [context] block amux computed \
             just now; trust it over your memory of earlier turns. For more, use the amux CLI: \
             `amux peek {worker}` shows its terminal, `amux info {worker}` its configuration, `amux board ls` \
             the board, and you can read files in its directory and in ~/.amux/logs (Read, Grep, Glob). To direct the work, send the worker an \
             instruction with `amux send {worker} --stdin` and tell the owner what you sent. Leave edits in \
             its checkout to the worker unless the owner asks you to make them.\n\n"
        ));
    }
    let running = super::session_verbs::is_running(worker).await;
    let paused = super::session_verbs::lane_is_paused(worker);
    let w = worker.to_string();
    let board = state
        .store
        .read_async(move |c| {
            let mut counts: Vec<(String, i64)> = Vec::new();
            let mut st = c.prepare(
                "SELECT status, COUNT(*) FROM issues WHERE session=?1 AND deleted IS NULL AND COALESCE(archived,0)=0 \
                 AND status IN ('doing','todo','backlog','needsyou','review','blocked') GROUP BY status",
            )?;
            for r in st.query_map([&w], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.flatten() {
                counts.push(r);
            }
            let mut st = c.prepare(
                "SELECT id, title, status, COALESCE(ask_question,'') FROM issues WHERE session=?1 AND deleted IS NULL \
                 AND COALESCE(archived,0)=0 AND status IN ('doing','needsyou') ORDER BY status, updated DESC LIMIT 6",
            )?;
            let cards = st
                .query_map([&w], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?
                .flatten()
                .collect::<Vec<_>>();
            let trend = crate::runtime_jobs::board_drain::lane_trend(c, &w, chrono::Utc::now().timestamp());
            Ok((counts, cards, trend))
        })
        .await
        .ok();
    // Recent activity log: last N prompts/messages the worker received, with
    // timestamps and origin, so the companion knows what the worker was told.
    let w2 = worker.to_string();
    let activity_log: Vec<(f64, String, String, String)> = state
        .store
        .read_async(move |c| {
            let mut st = c.prepare(
                "SELECT ts, COALESCE(type,''), COALESCE(origin,''), COALESCE(SUBSTR(text,1,200),'') \
                 FROM cmd_history WHERE session=?1 ORDER BY ts DESC LIMIT 15",
            )?;
            let rows = st
                .query_map([&w2], |r| {
                    Ok((
                        r.get::<_, f64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })?
                .flatten()
                .collect::<Vec<_>>();
            Ok(rows)
        })
        .await
        .unwrap_or_default();

    let git = |args: &[&str]| {
        let mut a: Vec<String> = vec!["-C".into(), dir.clone()];
        a.extend(args.iter().map(|s| s.to_string()));
        a
    };
    let run = |a: Vec<String>| async move {
        tokio::time::timeout(std::time::Duration::from_secs(5), tokio::process::Command::new("git").args(&a).kill_on_drop(true).output())
            .await
            .ok()
            .and_then(|r| r.ok())
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let (branch, dirty, last_commit) = if dir.is_empty() {
        (None, None, None)
    } else {
        (
            run(git(&["rev-parse", "--abbrev-ref", "HEAD"])).await,
            run(git(&["status", "--porcelain", "--untracked-files=no"])).await.map(|s| s.lines().count()),
            run(git(&["log", "-1", "--format=%h %s (%cr)"])).await,
        )
    };
    let last = super::session_verbs::last_assistant_message(worker, 1500);
    out.push_str(&format!("[context: {worker}, {}]\n", chrono::Local::now().format("%Y-%m-%d %H:%M")));
    out.push_str(&format!(
        "- process: {}\n",
        if paused { "paused by the owner" } else if running { "running" } else { "not running" }
    ));
    if let Some((counts, cards, trend)) = &board {
        let c = |k: &str| counts.iter().find(|(s, _)| s == k).map(|(_, n)| *n).unwrap_or(0);
        out.push_str(&format!(
            "- board: {} doing, {} todo, {} backlog, {} waiting on the owner, {} in review, {} blocked\n",
            c("doing"), c("todo"), c("backlog"), c("needsyou"), c("review"), c("blocked")
        ));
        for (id, title, status, ask) in cards {
            if status == "doing" {
                out.push_str(&format!("- working on {id}: {}\n", clip(title, 160)));
            } else {
                out.push_str(&format!("- waiting on the owner, {id}: {}\n", clip(if ask.is_empty() { title } else { ask }, 200)));
            }
        }
        if trend.measured && trend.opened_7d + trend.closed_7d > 0 {
            out.push_str(&format!(
                "- last 7 days: {} cards opened, {} closed; {} closed in the last 24h\n",
                trend.opened_7d, trend.closed_7d, trend.closed_24h
            ));
        }
    } else {
        out.push_str("- board: could not be read\n");
    }
    if let Some(b) = branch {
        out.push_str(&format!("- git: branch {b}, {} changed file(s) uncommitted", dirty.unwrap_or(0)));
        if let Some(l) = last_commit {
            out.push_str(&format!(", last commit {l}"));
        }
        out.push('\n');
    }
    if !activity_log.is_empty() {
        out.push_str("- recent log (newest first):\n");
        for (ts, typ, origin, text) in &activity_log {
            let when = chrono::DateTime::from_timestamp(*ts as i64, 0)
                .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
                .unwrap_or_default();
            let who = if origin.is_empty() { typ.as_str() } else { origin.as_str() };
            out.push_str(&format!("  {when} [{who}] {}\n", clip(text, 200)));
        }
    }
    if !last.trim().is_empty() {
        out.push_str(&format!("- its last reply (may be cut): {}\n", clip(last.trim(), 1500)));
    }
    if let Some(fleet) = hub_fleet_section(state, worker).await {
        out.push_str(&fleet);
    }
    // THE ESCALATION LADDER (AMUX-5432, Ethan 2026-10-01). Sent every turn,
    // not only on a fresh conversation, so existing Chats learn it too.
    out.push_str(&format!(
        "- when the question is about this worker's work (its output, data, results, a lookup it ran, why it \
         did or did not produce something), check before you answer: general knowledge is not an answer to \
         a question about what the worker did, and \"I haven't checked\" is not an acceptable reply. The same \
         applies whenever you lack a tool or the answer needs the worker's own access (web, inbox, calendar, \
         its files including uncommitted edits, or what is in the worker's head). Do not tell the owner to \
         look it up: 1) read what it produced (`amux peek {worker}`, `amux get <api path>`, the board, its last reply); \
         2) else run `amux delegate-job run --worker {worker} --wait 120 --stdin <<'EOF'` with the question: \
         a read-only background job on the worker's own agent that sees its uncommitted work and, where it can, \
         a fork of its conversation; it never touches the live worker. Say when an answer came from that fork; \
         3) if only the live worker can answer, say so and offer the owner two choices: ask the worker \
         directly, or queue the question for its next idle turn. Never send to the worker without the owner saying so.\n"
    ));
    out.push_str("\n[owner]\n");
    out.push_str(text);
    out
}

/// For a hub (other lanes name it AMUX_CONTRACT_HUB, as every GS-12 lane names
/// mixpeek-override), the lanes reporting to it and the plan's last pace
/// line, computed (Ethan, 2026-10-07: chat "should have the ability to review
/// all worker stuff asking the orchestrator of progress"). None for a worker
/// nobody reports to.
async fn hub_fleet_section(state: &AppState, worker: &str) -> Option<String> {
    let home = home();
    let lanes: Vec<String> = super::session_verbs::all_lane_names()
        .into_iter()
        .filter(|l| l != worker)
        .filter(|l| {
            crate::api::contract::lane_setting(&home, l, "AMUX_CONTRACT_HUB")
                .is_some_and(|h| h.trim().trim_matches('"') == worker)
        })
        .collect();
    let pace = std::fs::read_to_string(home.join("logs").join(format!("orch-pace-{worker}.jsonl")))
        .ok()
        .and_then(|t| t.lines().rev().find(|l| !l.trim().is_empty()).map(str::to_string))
        .and_then(|l| serde_json::from_str::<Value>(&l).ok());
    if lanes.is_empty() && pace.is_none() {
        return None;
    }
    let mut out = String::new();
    if let Some(p) = pace {
        let n = |k: &str| p.get(k).and_then(Value::as_i64).unwrap_or(0);
        let when = p.get("ts").and_then(Value::as_f64)
            .and_then(|t| chrono::DateTime::from_timestamp(t as i64, 0))
            .map(|d| d.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
            .unwrap_or_default();
        out.push_str(&format!(
            "- plan pace ({when}): proof cards {} of {} verified; cards {} of {} terminal (file: ~/.amux/logs/orch-pace-{worker}.jsonl)\n",
            n("proof_verified"), n("proof_total"), n("terminal"), n("total")
        ));
    }
    if !lanes.is_empty() {
        let ls = lanes.clone();
        let rows: Vec<(String, i64, i64, i64, String)> = state.store.read_async(move |c| {
            let mut out = Vec::new();
            for l in &ls {
                let (doing, todo, ask): (i64, i64, i64) = c.query_row(
                    "SELECT SUM(status='doing'), SUM(status='todo'), SUM(status='needsyou') FROM issues \
                     WHERE session=?1 AND deleted IS NULL AND COALESCE(archived,0)=0",
                    [l], |r| Ok((r.get::<_, Option<i64>>(0)?.unwrap_or(0), r.get::<_, Option<i64>>(1)?.unwrap_or(0), r.get::<_, Option<i64>>(2)?.unwrap_or(0))))?;
                let card: String = c.query_row(
                    "SELECT id || ' ' || title FROM issues WHERE session=?1 AND status='doing' AND deleted IS NULL ORDER BY updated DESC LIMIT 1",
                    [l], |r| r.get(0)).unwrap_or_default();
                out.push((l.clone(), doing, todo, ask, card));
            }
            Ok(out)
        }).await.unwrap_or_default();
        out.push_str(&format!("- lanes reporting to {worker} ({}): doing card, todo, waiting on the owner\n", rows.len()));
        for (l, _doing, todo, ask, card) in &rows {
            let on = if card.is_empty() { "nothing in doing".to_string() } else { clip(card, 90) };
            out.push_str(&format!("  {l}: {on}; {todo} todo; {ask} waiting on the owner\n"));
        }
        out.push_str("  For more on one lane: `amux peek <lane>`, `amux board ls --session <lane>`. Read files and logs directly (Read, Grep).\n");
    }
    Some(out)
}

/// The reply says, in so many words, that it did not look.
fn reply_admits_unchecked(text: &str) -> bool {
    let t = text.to_lowercase().replace('\u{2019}', "'");
    [
        "haven't checked",
        "have not checked",
        "didn't check",
        "did not check",
        "i can't tell you which",
        "haven't looked",
        "have not looked",
    ]
    .iter()
    .any(|p| t.contains(p))
}

#[cfg(test)]
mod unchecked_reply_tests {
    use super::reply_admits_unchecked;

    #[test]
    fn an_admission_of_not_checking_is_detected() {
        // The 2026-10-06 primis@chat reply, verbatim fragment.
        assert!(reply_admits_unchecked(
            "I haven't checked the worker's output or searched Wikidata, so I can't tell you which parents exist."
        ));
        assert!(reply_admits_unchecked("I haven\u{2019}t checked its log."));
        assert!(!reply_admits_unchecked("The worker's lookup returned no item for that label."));
    }
}

fn clip(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= n { s } else { s.chars().take(n).collect::<String>() + "…" }
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
    let mut args = turn_args(provider, &flags, &cc_model, &conv, fresh);
    // Binding rules, composed at every turn (AH-233). A Chat tab is part of its
    // worker, so it runs under the worker's rules.
    {
        let owner = companion_parent(name).unwrap_or(name);
        let isolated =
            super::session_verbs::env_flag_on(super::session_verbs::parse_env(owner).get("CC_ISOLATED"));
        let rules = super::session_verbs::worker_rules_args(owner, provider, isolated);
        if provider == "codex" {
            // `-c` is an `exec` option: it must precede `resume <id> -`.
            args.splice(1..1, rules);
        } else {
            args.extend(rules);
        }
    }
    let prelude = match companion_parent(name) {
        // The Chat tab runs with its worker's scope settings (credentials,
        // env) but under its own identity, so an `amux send` it makes is
        // stamped as the worker's Chat tab (see owner_relay_quote).
        Some(worker) => format!(
            "{}export AMUX_SESSION={key}; export AMUX_WORKER={key}; ",
            super::session_verbs::headless_turn_prelude(worker, provider, &work_dir),
            key = sh_quote(name)
        ),
        None => super::session_verbs::headless_turn_prelude(name, provider, &work_dir),
    };
    // The companion context is computed before the spawn: the prompt reaches
    // the provider through the turn's stdin file.
    let companion_of = cfg.get_or("CC_COMPANION_OF", "").trim().to_string();
    let text: String = if companion_of.is_empty() {
        text.to_string()
    } else {
        companion_prompt(state, &companion_of, fresh, text).await
    };
    let files = TurnFiles::new(turn_id, u32::from(force_fresh));
    let pgid = match spawn_detached(&files, &prelude, provider, &args, &work_dir, &text).await {
        Ok(p) => p,
        Err(why) => {
            files.remove();
            lane.publish(json!({"type": "error", "turn_id": turn_id, "text": why}));
            return TurnOutcome::failed(turn_id, why);
        }
    };
    // What a restarted server needs to re-attach to this turn (see recover).
    update_meta(
        name,
        &[
            ("chat_inflight_files", json!(files.stem.to_string_lossy())),
            ("chat_inflight_pgid", json!(pgid)),
            ("chat_inflight_provider", json!(provider)),
        ],
    );
    follow_turn(state, name, provider, lane, turn_id, &files, pgid).await
}

// ---------------------------------------------------------------------------
// DETACHED TURNS (Ethan, 2026-10-01, on "The amux server restarted during this
// turn, so the reply was lost": "this needs to be better like terminal auto").
//
// A terminal worker survives a server restart because tmux owns its process.
// A chat turn used to be a child of the server with its stdout on a pipe, so
// the builder's exec() on every commit closed the pipe and killed the turn.
// Now the provider runs in its own process group, detached from the server,
// reading its prompt from and writing its output to files under
// chat-state/turns/. The server only FOLLOWS those files, so a restarted
// server re-attaches to the same turn and the reply is persisted exactly once
// (dedupe key `chat:{turn}:assistant`).
// ---------------------------------------------------------------------------

fn turns_dir() -> std::path::PathBuf {
    state_dir().join("turns")
}

/// One turn attempt's files: `<stem>.in` (prompt), `.out` (provider stdout),
/// `.err`, and `.exit` (exit status, written last by the turn's own shell).
struct TurnFiles {
    stem: std::path::PathBuf,
}

impl TurnFiles {
    fn new(turn_id: &str, attempt: u32) -> Self {
        TurnFiles { stem: turns_dir().join(format!("{turn_id}.{attempt}")) }
    }
    fn from_stem(stem: &str) -> Self {
        TurnFiles { stem: std::path::PathBuf::from(stem) }
    }
    fn path(&self, ext: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("{}.{ext}", self.stem.to_string_lossy()))
    }
    fn exit_code(&self) -> Option<i32> {
        std::fs::read_to_string(self.path("exit")).ok().and_then(|t| t.trim().parse().ok())
    }
    fn remove(&self) {
        for ext in ["in", "out", "err", "exit", "exit.tmp"] {
            let _ = std::fs::remove_file(self.path(ext));
        }
    }
}

/// True while any process of the turn's group is alive.
pub(crate) fn group_alive(pgid: u32) -> bool {
    if pgid == 0 {
        return false;
    }
    // SAFETY: signal 0 only checks existence/permission.
    let rc = unsafe { libc::kill(-(pgid as i32), 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// SIGTERM the turn's whole process group (the provider and its shell).
pub(crate) fn signal_turn(pgid: u32) {
    if pgid == 0 {
        return;
    }
    // SAFETY: the group was created by this server for one chat turn.
    unsafe {
        if libc::kill(-(pgid as i32), libc::SIGTERM) != 0 {
            libc::kill(pgid as i32, libc::SIGTERM);
        }
    }
}

/// Start the provider detached: a launcher shell in a NEW process group starts
/// the turn in the background and exits at once, so the turn is reparented away
/// from the server and survives its restart. Returns the process group id.
async fn spawn_detached(
    files: &TurnFiles,
    prelude: &str,
    provider: &str,
    args: &[String],
    work_dir: &str,
    prompt: &str,
) -> Result<u32, String> {
    std::fs::create_dir_all(turns_dir()).map_err(|e| format!("could not start {provider}: {e}"))?;
    std::fs::write(files.path("in"), prompt).map_err(|e| format!("could not start {provider}: {e}"))?;
    let q = |ext: &str| sh_quote(&files.path(ext).to_string_lossy());
    let inner = format!(
        "{prelude}{} \"$@\"; ec=$?; printf '%s' \"$ec\" > {tmp} && mv {tmp} {exit}",
        sh_quote(&provider_bin(provider)),
        tmp = q("exit.tmp"),
        exit = q("exit"),
    );
    let launcher = format!(
        "bash -c {} amux-chat \"$@\" < {} > {} 2> {} & echo $!",
        sh_quote(&inner),
        q("in"),
        q("out"),
        q("err"),
    );
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(&launcher)
        .arg("amux-chat-launch")
        .args(args)
        .current_dir(work_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        // The LAUNCHER is owned and short-lived; the turn it starts is not.
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| format!("could not start {provider}: {e}"))?;
    let pgid = child.id().unwrap_or(0);
    let done = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
        .await
        .map_err(|_| format!("could not start {provider}: the launcher did not return"))?
        .map_err(|e| format!("could not start {provider}: {e}"))?;
    if !done.status.success() || pgid == 0 {
        return Err(format!(
            "could not start {provider}: {}",
            String::from_utf8_lossy(&done.stderr).trim()
        ));
    }
    Ok(pgid)
}

/// Follow a running (or already finished) detached turn from its files:
/// parse its output into the same events and assembly a piped turn produced,
/// report heartbeats and tool lifecycle, and enforce the idle timeout.
async fn follow_turn(
    state: &AppState,
    name: &str,
    provider: &str,
    lane: &Lane,
    turn_id: &str,
    files: &TurnFiles,
    pgid: u32,
) -> TurnOutcome {
    let mut out = TurnOutcome::new(turn_id);
    let mut parser = Parser::new(provider);
    *lane.pid.lock().unwrap() = Some(pgid);
    // The background shell creates `.out` a moment after the launcher returns.
    let mut file = None;
    for _ in 0..100 {
        if let Ok(f) = tokio::fs::File::open(files.path("out")).await {
            file = Some(f);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let Some(file) = file else {
        *lane.pid.lock().unwrap() = None;
        signal_turn(pgid);
        let why = format!("{provider} did not start (no output file)");
        lane.publish(json!({"type": "error", "turn_id": turn_id, "text": why}));
        out.asm.error = Some(why);
        return out;
    };
    let mut reader = BufReader::new(file);
    let mut pending = String::new();
    let mut last_output = std::time::Instant::now();
    let mut beat = tokio::time::interval(HEARTBEAT);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    beat.tick().await;
    let mut poll = tokio::time::interval(Duration::from_millis(100));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Draining: the turn has ended; read what is left, then stop.
    let mut draining = false;
    let mut lost = false;
    loop {
        let mut buf = String::new();
        match reader.read_line(&mut buf).await {
            Ok(0) => {
                if draining {
                    break;
                }
                if files.path("exit").exists() {
                    draining = true;
                    continue;
                }
                if !group_alive(pgid) {
                    // Ended without writing its exit status: killed outright.
                    // One more pass picks up anything flushed before it died.
                    lost = true;
                    draining = true;
                    continue;
                }
                if last_output.elapsed() >= TURN_IDLE_TIMEOUT {
                    signal_turn(pgid);
                    let why = format!("no output for {}s; turn killed", TURN_IDLE_TIMEOUT.as_secs());
                    lane.publish(json!({"type": "error", "turn_id": turn_id, "text": why}));
                    out.asm.error = Some(why);
                    break;
                }
                tokio::select! {
                    _ = poll.tick() => {}
                    _ = beat.tick() => {
                        report_state_with(state, name, "active", "Heartbeat", turn_id,
                            json!({"silent_s": last_output.elapsed().as_secs()})).await;
                    }
                }
            }
            Ok(_) => {
                pending.push_str(&buf);
                if !pending.ends_with('\n') {
                    // A partial line: the rest is still being written.
                    continue;
                }
                let line = std::mem::take(&mut pending);
                last_output = std::time::Instant::now();
                for mut ev in parser.line(line.trim_end_matches(['\n', '\r'])) {
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
            Err(e) => {
                let why = format!("could not read the turn's output: {e}");
                lane.publish(json!({"type": "error", "turn_id": turn_id, "text": why}));
                out.asm.error = Some(why);
                break;
            }
        }
    }
    *lane.pid.lock().unwrap() = None;
    out.conversation_id = parser.conversation_id.clone();
    out.model = parser.model.clone();
    let err_text = std::fs::read_to_string(files.path("err")).unwrap_or_default();
    let code = files.exit_code();
    if lane.interrupt.load(Ordering::SeqCst) {
        lane.publish(json!({"type": "interrupted", "turn_id": turn_id}));
    } else if out.asm.error.is_none() && code != Some(0) {
        let tail: String = err_text
            .lines()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        let why = if !tail.trim().is_empty() {
            tail
        } else if lost {
            format!("{provider} ended without reporting an exit status")
        } else {
            format!("{provider} exited with {code:?}")
        };
        lane.publish(json!({"type": "error", "turn_id": turn_id, "text": why}));
        out.asm.error = Some(why);
    }
    out
}

/// Re-attach a turn a previous server process started (see DETACHED TURNS):
/// follow its files to the end, then persist the reply exactly as a live turn
/// would. Runs before the lane's queue drains.
async fn reattach_turn(state: &AppState, name: &str, turn_id: &str, files: TurnFiles, pgid: u32, started_at: f64) {
    let lane = lane(name);
    let meta = load_meta(name);
    let provider = {
        let p = meta_str(&meta, "chat_inflight_provider");
        if p.is_empty() { provider_of(name) } else { p }
    };
    lane.interrupt.store(false, Ordering::SeqCst);
    lane.publish(json!({
        "type": "user", "turn_id": turn_id, "text": meta_str(&meta, "chat_inflight_text"),
        "origin": meta_str(&meta, "chat_inflight_origin"), "ts": started_at, "waiting": 0, "resumed": true,
    }));
    report_state(state, name, "active", "UserPromptSubmit", turn_id).await;
    let out = follow_turn(state, name, &provider, &lane, turn_id, &files, pgid).await;
    let duration_ms = if started_at > 0.0 {
        ((crate::config::now_f64() - started_at).max(0.0) * 1000.0) as u64
    } else {
        0
    };
    let ok = out.asm.error.is_none();
    finish_turn(state, name, &lane, turn_id, &provider, out, duration_ms).await;
    tracing::info!(session = %name, turn = %turn_id, duration_ms, ok, measured = true, n_considered = 1,
        verdict = "chat_turn_resumed", "a chat turn that outlived a server restart finished and was recorded");
}

/// Boot recovery for one chat worker (see the durability note above).
///
/// A turn marked in flight by a previous process is NOT re-run: its tools may
/// already have acted. A DETACHED turn whose files exist is re-attached and
/// followed to its end, so its reply is recorded as if nothing happened (see
/// DETACHED TURNS). Only a turn with nothing to re-attach to gets an assistant
/// message carrying an explicit error, so the transcript says what happened
/// instead of ending on an unanswered question. Queued messages were never
/// started, so they run (after a re-attached turn).
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
    let stem = meta_str(&meta, "chat_inflight_files");
    let pgid = u32::try_from(meta_i64(&meta, "chat_inflight_pgid")).unwrap_or(0);
    // A detached turn whose files exist outlived the restart (or finished
    // during it): follow it to the end instead of reporting it lost.
    let reattach = !turn.is_empty()
        && !stem.is_empty()
        && TurnFiles::from_stem(&stem).path("out").exists();
    if reattach {
        let files = TurnFiles::from_stem(&stem);
        tracing::warn!(session = %name, turn = %turn, pgid, alive = group_alive(pgid),
            finished = files.path("exit").exists(), measured = true, n_considered = 1,
            verdict = "chat_turn_reattached",
            "a chat turn outlived a server restart; re-attaching to it instead of reporting it lost");
    } else if !turn.is_empty() {
        let reason = if stem.is_empty() { "no_turn_files_recorded" } else { "turn_output_missing" };
        tracing::warn!(session = %name, turn = %turn, reason, measured = true, n_considered = 1,
            verdict = "chat_turn_lost", "a chat turn could not be re-attached after a restart");
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
        update_meta(
        name,
        &[("chat_inflight_turn", json!("")), ("chat_inflight_origin", json!("")), ("chat_inflight_text", json!(""))],
    );
        report_state(state, name, "idle", "StopFailure", &turn).await;
        tracing::warn!(session = %name, turn = %turn, measured = true, n_considered = 1,
            verdict = "chat_turn_interrupted",
            "a chat turn was cut off by a server restart; recorded as failed, not re-run");
        interrupted = 1;
    }
    let pending = load_queue(name);
    let resumed = pending.len();
    if reattach {
        // The re-attached turn finishes first, then the queue drains behind
        // it, in order, exactly as it would have without the restart.
        let lane = lane(name);
        if resumed > 0 && running(name) {
            let mut q = lane.queue.lock().unwrap();
            for item in pending.into_iter().rev() {
                q.push_front(item);
            }
            persist_queue(name, &q);
        }
        lane.busy.store(true, Ordering::SeqCst);
        let (st, n) = (state.clone(), name.to_string());
        let started_at = meta.get("chat_inflight_started").and_then(Value::as_f64).unwrap_or(0.0);
        let files = TurnFiles::from_stem(&stem);
        tokio::spawn(async move {
            reattach_turn(&st, &n, &turn, files, pgid, started_at).await;
            pump(st, n).await;
        });
        return (0, if running(name) { resumed } else { 0 });
    }
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
/// Move AMUX-5350's companion WORKERS (`<worker>-chat`, CC_COMPANION_OF) to
/// Chat-tab lanes, once, at startup: the conversation ids to the lane's meta,
/// the chat history rows to the lane key, the queue file to the lane's name,
/// then the old env and meta files aside to chat/companions/retired/ (moved,
/// not deleted). Idempotent: a moved companion has no env file left to find.
pub async fn migrate_legacy_companions(state: &AppState) -> usize {
    let Ok(rd) = std::fs::read_dir(home().join("sessions")) else {
        return 0;
    };
    let mut moved = 0;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("env") {
            continue;
        }
        let Some(old) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        let worker = super::session_verbs::parse_env(&old).get_or("CC_COMPANION_OF", "").trim().to_string();
        if worker.is_empty() || !super::session_verbs::valid_session_name(&worker) || worker == old {
            continue;
        }
        let key = companion_key(&worker);
        if !companion_meta_path(&worker).exists() {
            let old_meta = super::session_verbs::load_meta(&old);
            let carry: Vec<(&str, Value)> = ["chat_conversation_id", "cc_conversation_id", "chat_codex_thread", "chat_turns"]
                .iter()
                .filter_map(|k| old_meta.get(*k).map(|v| (*k, v.clone())))
                .collect();
            update_meta(&key, &carry);
        }
        let (o, k) = (old.clone(), key.clone());
        let rekeyed = state.store.write(move |c| {
            c.execute(
                "UPDATE session_events SET session=?1 WHERE session=?2 AND type=?3",
                rusqlite::params![k, o, CHAT_EVENT],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        });
        if let Err(error) = rekeyed {
            tracing::warn!(session = %old, worker, %error, verdict = "chat_companion_migrate_failed",
                "could not move a companion worker's chat history; left it in place");
            continue;
        }
        let q = queue_path(&old);
        if q.exists() {
            let _ = std::fs::rename(&q, queue_path(&key));
        }
        let retired = companions_dir().join("retired");
        let _ = std::fs::create_dir_all(&retired);
        for suffix in ["env", "meta.json"] {
            let from = home().join("sessions").join(format!("{old}.{suffix}"));
            if from.exists() {
                let _ = std::fs::rename(&from, retired.join(format!("{old}.{suffix}")));
            }
        }
        moved += 1;
        tracing::info!(old = %old, worker, lane = %key, measured = true, n_considered = 1,
            verdict = "chat_companion_migrated",
            "moved a companion chat worker into its worker's Chat tab; old files kept in chat/companions/retired");
    }
    if moved > 0 {
        crate::api::sessions_legacy::invalidate_sessions_cache();
    }
    moved
}

pub async fn recover_all(state: &AppState) -> (usize, usize, usize) {
    migrate_legacy_companions(state).await;
    // Chat tabs recover like chat workers: an interrupted turn is recorded,
    // a queued one resumes.
    let (mut workers, mut interrupted, mut resumed) = (0, 0, 0);
    if let Ok(rd) = std::fs::read_dir(companions_dir()) {
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().into_owned();
            let Some(worker) = fname.strip_suffix(".meta.json") else { continue };
            if !env_path(worker).exists() {
                continue;
            }
            workers += 1;
            let (i, r) = recover(state, &companion_key(worker)).await;
            interrupted += i;
            resumed += r;
        }
    }
    let dir = home().join("sessions");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return (workers, interrupted, resumed);
    };
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
    if !lane_exists(&name) {
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
    if !lane_exists(&name) {
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
    if !lane_exists(&name) {
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
    signal_turn(pid);
    tracing::info!(session = %name, pid, measured = true, n_considered = 1,
        verdict = "chat_turn_interrupt_requested", "owner interrupted a chat turn");
    (true, "turn interrupted".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_tab_key_belongs_to_its_worker_and_no_worker_can_own_it() {
        // Ethan 2026-09-30: the Chat tab is part of the worker, not a worker.
        // Its key must round-trip to the worker and be a name no real worker
        // can have, or the chat and a worker could collide.
        let key = companion_key("mixpeek-override");
        assert_eq!(key, "mixpeek-override@chat");
        assert_eq!(companion_parent(&key), Some("mixpeek-override"));
        assert!(!super::super::session_verbs::valid_session_name(&key));
        assert_eq!(companion_parent("mixpeek-override-chat"), None);
        assert_eq!(companion_parent("mixpeek-override"), None);
        assert_eq!(companion_parent("@chat"), None);
        assert_eq!(companion_parent("../x@chat"), None);
    }

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
        assert!(
            fresh.windows(2).any(|w| w == ["--allowedTools", "Read,Grep,Glob"]),
            "read-only tools let the companion look things up"
        );
        let uploads = home().join("uploads").to_string_lossy().into_owned();
        assert!(
            fresh.windows(2).any(|w| w[0] == "--add-dir" && w[1] == uploads),
            "a headless turn must be able to read the files attached in the Chat tab: {fresh:?}"
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

#[cfg(test)]
mod companion_tests {
    use super::*;

    /// Ethan, 2026-10-07: an orchestrator's chat sees the lanes reporting to
    /// it and the plan's last pace line, computed; a worker nobody reports to
    /// gets no section.
    #[tokio::test]
    async fn a_hubs_companion_sees_its_lanes_and_the_plan_pace() {
        let dir = tempfile::tempdir().unwrap();
        let h = dir.path();
        std::fs::create_dir_all(h.join("sessions")).unwrap();
        std::fs::create_dir_all(h.join("logs")).unwrap();
        let _g = crate::api::settings::test_env::set_home(h);
        std::fs::write(h.join("sessions/hubz.env"), "CC_DIR=\"/tmp\"\n").unwrap();
        std::fs::write(h.join("sessions/lane-r.env"), "AMUX_CONTRACT_HUB=hubz\n").unwrap();
        std::fs::write(h.join("sessions/lane-x.env"), "AMUX_CONTRACT_HUB=elsewhere\n").unwrap();
        std::fs::write(h.join("logs/orch-pace-hubz.jsonl"),
            "{\"ts\": 1791432943.8, \"total\": 1801, \"terminal\": 1079, \"proof_total\": 62, \"proof_verified\": 14}\n").unwrap();
        let store = crate::db::Store::open(&h.join("c.db")).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "t".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        state.store.write(|c| {
            c.execute("INSERT INTO issues (id,title,desc,status,session,created,updated,type,archived,owner_type) VALUES ('LR-1','Prove scale to zero','', 'doing','lane-r',1,1,'code',0,'agent')", [])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let s = hub_fleet_section(&state, "hubz").await.expect("a hub gets a section");
        assert!(s.contains("proof cards 14 of 62 verified"), "{s}");
        assert!(s.contains("lane-r: LR-1 Prove scale to zero"), "{s}");
        assert!(!s.contains("lane-x"), "a lane with another hub is not listed: {s}");
        assert!(hub_fleet_section(&state, "lane-x").await.is_none(), "nobody reports to lane-x");
    }

    #[tokio::test]
    async fn a_companion_turn_carries_the_workers_state_and_the_owners_words_last() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("companion.db")).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "t".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let w = "companion-test-worker-zz";
        state.store.write(move |c| {
            c.execute("INSERT INTO issues (id,title,desc,status,session,created,updated,type,archived,owner_type) VALUES ('CT-1','Ship the widget','', 'doing',?1,1,1,'code',0,'agent')", [w])?;
            c.execute("INSERT INTO issues (id,title,desc,status,session,created,updated,type,archived,owner_type,ask_question) VALUES ('CT-2','Key','', 'needsyou',?1,1,1,'code',0,'agent','Can you mint the API key?')", [w])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let p = companion_prompt(&state, w, true, "how is it going?").await;
        assert!(p.starts_with("You are the chat companion for the amux worker `companion-test-worker-zz`"), "{p}");
        assert!(p.contains("amux send companion-test-worker-zz --stdin"), "{p}");
        assert!(p.contains("- board: 1 doing, 0 todo, 0 backlog, 1 waiting on the owner"), "{p}");
        assert!(p.contains("- working on CT-1: Ship the widget"), "{p}");
        assert!(p.contains("- waiting on the owner, CT-2: Can you mint the API key?"), "{p}");
        assert!(p.ends_with("[owner]\nhow is it going?"), "{p}");
        assert!(!p.contains('\u{2014}'), "no em dashes in what the model reads back to the owner");
        // Later turns carry the context but not the role preamble again.
        let p2 = companion_prompt(&state, w, false, "and now?").await;
        assert!(p2.starts_with("[context: companion-test-worker-zz"), "{p2}");
    }
}
