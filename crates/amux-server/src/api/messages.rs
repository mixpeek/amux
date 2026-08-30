//! Message API (RR-0066, Invariant 29): durable messages, delivered at turn
//! boundaries.
//!
//! `POST /api/messages` persists the message FIRST and then enqueues a
//! `WorkerCommand::DeliverMessage(MessageId)` per worker recipient with
//! `DeliveryTiming::AtTurnBoundary` — the command carries only the
//! reference; the body lives in `_amux_messages` and is resolved by the
//! pump at delivery time (orchestrator/runtime.rs). Turn-boundary delivery
//! is the invariant: a mid-turn interruption for a steering message is
//! exactly what `AtTurnBoundary` exists to prevent, and `Immediate` remains
//! reserved for Cancel/Pause/Resume.
//!
//! Group targets fan out via `amux_core::message::fan_out`: one child
//! message per member, threaded to the parent, each with its OWN delivery
//! record and its own queued command — five members means five delivery
//! records, never one optimistic "sent".
//!
//! Delivery state transitions (`/{id}/ack`, `/{id}/acted`) go through
//! `Message::advance_delivery`, which is forward-only: the delivery record
//! is audit trail (2026-07-27: a delivered message was reported swallowed;
//! the record is what settled it), so a backwards move is a 409, never a
//! rewrite.
//!
//! Mounted at `/api/messages` inside the protected router (api/mod.rs).

use super::AppState;
use crate::db::queries;
use crate::db::{PendingEvent, WriteOutcome};
use amux_core::events::Actor;
use amux_core::ids::{CommandId, GroupId, MessageId, TaskId, WorkerId};
use amux_core::message::{fan_out, DeliveryState, Message, MessageTarget};
use amux_core::protocol::{DeliveryTiming, WorkerCommand};
use amux_core::revision::{EntityType, MutationKind};
use amux_core::search::PagedResponse;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_messages).post(create_message))
        // Human-message -> tracked-work accountability (AMUX-2986). Placed
        // BEFORE "/{id}" so the literal path is not swallowed by the id capture.
        .route("/accountability", get(accountability))
        .route("/{id}", get(get_message))
        .route("/{id}/ack", post(ack_message))
        .route("/{id}/acted", post(acted_message))
}

// ---- row helpers (used by tests and the pump's body lookup contract) -----

fn corrupt(e: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
}

pub(crate) fn insert_message(conn: &Connection, m: &Message) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO _amux_messages (id, from_actor, target, body, thread, created_at, delivery)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            m.id.as_str(),
            serde_json::to_string(&m.from).map_err(corrupt)?,
            serde_json::to_string(&m.to).map_err(corrupt)?,
            m.body,
            m.thread.as_ref().map(|t| t.as_str()),
            m.created_at.to_rfc3339(),
            serde_json::to_string(&m.delivery).map_err(corrupt)?,
        ],
    )?;
    Ok(())
}

const MSG_COLS: &str = "id, from_actor, target, body, thread, created_at, delivery";

fn row_to_message(r: &rusqlite::Row) -> rusqlite::Result<Message> {
    let id: String = r.get(0)?;
    let from: String = r.get(1)?;
    let target: String = r.get(2)?;
    let thread: Option<String> = r.get(4)?;
    let created_at: String = r.get(5)?;
    let delivery: String = r.get(6)?;
    Ok(Message {
        id: MessageId::parse(&id).map_err(corrupt)?,
        from: serde_json::from_str(&from).map_err(corrupt)?,
        to: serde_json::from_str(&target).map_err(corrupt)?,
        body: r.get(3)?,
        thread: thread
            .map(|t| MessageId::parse(&t))
            .transpose()
            .map_err(corrupt)?,
        created_at: created_at.parse::<DateTime<Utc>>().map_err(corrupt)?,
        delivery: serde_json::from_str(&delivery).map_err(corrupt)?,
    })
}

pub(crate) fn message_by_id(conn: &Connection, id: &str) -> rusqlite::Result<Option<Message>> {
    conn.query_row(
        &format!("SELECT {MSG_COLS} FROM _amux_messages WHERE id = ?1"),
        params![id],
        row_to_message,
    )
    .optional()
}

/// Queue the AtTurnBoundary delivery command for one recipient. The
/// idempotency key derives from the MessageId, so a replayed create can
/// never double-queue a delivery (Invariant 9). Human-authored messages
/// carry no precondition — they always deliver (Invariant 38's rule).
fn enqueue_delivery(
    conn: &Connection,
    worker: &WorkerId,
    msg: &MessageId,
    now: DateTime<Utc>,
) -> rusqlite::Result<()> {
    crate::db::commands::enqueue(
        conn,
        CommandId::from_ulid(ulid::Ulid::new()),
        worker,
        &WorkerCommand::DeliverMessage(msg.clone()),
        &format!("deliver-{}", msg.as_str()),
        &DeliveryTiming::AtTurnBoundary,
        None,
        now,
    )?;
    Ok(())
}

/// Live (non-deleted) members of a group: workers whose `group_id` matches.
/// Group membership IS the workers table (`_amux_workers.group_id`) — no
/// second membership store to fall out of step with it.
fn group_members(conn: &Connection, group: &GroupId) -> rusqlite::Result<Vec<WorkerId>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM _amux_workers
         WHERE group_id = ?1 AND json_extract(state, '$.deleted_at') IS NULL
         ORDER BY created_at, id",
    )?;
    let rows = stmt.query_map(params![group.as_str()], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(WorkerId::parse(&row?).map_err(corrupt)?);
    }
    Ok(out)
}

// ---- shared handler helpers ----------------------------------------------

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

use super::internal;

fn no_write() -> WriteOutcome {
    WriteOutcome { applied: false, events: Vec::new() }
}

fn finish<T>(
    slot: &Mutex<Option<T>>,
    outcome: T,
    write: WriteOutcome,
) -> rusqlite::Result<WriteOutcome> {
    *slot.lock().expect("outcome slot poisoned") = Some(outcome);
    Ok(write)
}

fn ev(entity_type: EntityType, id: &str, mutation: MutationKind) -> PendingEvent {
    PendingEvent { entity_type, entity_id: id.to_string(), mutation, payload: None }
}

fn message_body(m: &Message) -> Value {
    serde_json::to_value(m).unwrap_or_else(|_| json!({ "id": m.id.as_str() }))
}

/// Resolve a recipient key — a `wrk_.../grp_...` id, the literal "human", or a
/// worker name/alias — to a MessageTarget. `Ok(None)` means a worker name that
/// does not exist. Shared by POST /api/messages and /api/env/apply's messages
/// stanza so recipients resolve through ONE path (ethos D6).
pub(crate) fn resolve_recipient(
    conn: &Connection,
    key: &str,
) -> rusqlite::Result<Option<MessageTarget>> {
    if let Ok(w) = WorkerId::parse(key) {
        Ok(Some(MessageTarget::Worker(w)))
    } else if let Ok(g) = GroupId::parse(key) {
        Ok(Some(MessageTarget::Group(g)))
    } else if key.eq_ignore_ascii_case("human") {
        Ok(Some(MessageTarget::Human))
    } else {
        queries::get_worker(conn, key)?
            .map(|row| WorkerId::parse(&row.id).map(MessageTarget::Worker).map_err(corrupt))
            .transpose()
    }
}

/// Insert one message and enqueue its AtTurnBoundary deliveries — a group target
/// fans out to per-member children (Invariant 12/29), a worker enqueues one, a
/// human queues nothing. THE delivery seam: POST /api/messages and
/// /api/env/apply both go through here, so a delivery is never spelled twice
/// (ethos D6; Invariant 9 no-double-queue holds via the derived idempotency key).
/// Returns (parent, fan-out child ids, deliveries enqueued, revision events).
pub(crate) fn insert_message_and_deliver(
    conn: &Connection,
    from: Actor,
    target: MessageTarget,
    text: String,
    thread: Option<MessageId>,
    now: DateTime<Utc>,
) -> rusqlite::Result<(Message, Vec<String>, usize, Vec<PendingEvent>)> {
    let parent = Message::new(
        MessageId::from_ulid(ulid::Ulid::new()),
        from,
        target.clone(),
        text,
        thread,
        now,
    );
    insert_message(conn, &parent)?;
    let mut events = vec![ev(EntityType::Message, parent.id.as_str(), MutationKind::Created)];
    let mut children_ids = Vec::new();
    let mut commands_enqueued = 0usize;
    match &target {
        MessageTarget::Worker(w) => {
            enqueue_delivery(conn, w, &parent.id, now)?;
            commands_enqueued += 1;
        }
        MessageTarget::Group(g) => {
            // Per-recipient children (Invariant 29/12); an empty group honestly
            // fans out to nothing — the parent row still records the send.
            let members = group_members(conn, g)?;
            let mut mint = || MessageId::from_ulid(ulid::Ulid::new());
            for child in fan_out(&parent, &members, &mut mint) {
                insert_message(conn, &child)?;
                events.push(ev(EntityType::Message, child.id.as_str(), MutationKind::Created));
                if let MessageTarget::Worker(w) = &child.to {
                    enqueue_delivery(conn, w, &child.id, now)?;
                    commands_enqueued += 1;
                }
                children_ids.push(child.id.as_str().to_string());
            }
        }
        // Human: surfaced via the message list/SSE; nothing to queue.
        MessageTarget::Human => {}
    }
    Ok((parent, children_ids, commands_enqueued, events))
}

// ---- POST /api/messages ---------------------------------------------------

/// How the caller names the recipient. A bare string is resolved
/// generously (worker id, group id, "human", or worker name/alias); a JSON
/// object must be a literal `MessageTarget`.
enum ToSpec {
    Target(MessageTarget),
    Key(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)] // Invariant 37: unknown fields rejected, not dropped
pub struct CreateMessageBody {
    /// Recipient: `"wrk_..."`, `"grp_..."`, `"human"`, a worker
    /// name/alias, or a typed MessageTarget object.
    pub to: Value,
    pub body: String,
    /// Parent message id, for replies.
    #[serde(default)]
    pub thread: Option<String>,
    /// Sender as an `Actor` object. Defaults to the owner: the bearer token
    /// IS the owner's identity on this single-user API. A worker relaying a
    /// message must pass its own actor — per-caller identity stamping lands
    /// with auth identities, and until then the default is the honest
    /// reading of an authenticated request.
    #[serde(default)]
    pub from: Option<Value>,
}

enum CreateOutcome {
    NotFound { what: String },
    Created { message: Value, fan_out: Vec<String>, commands_enqueued: usize },
}

pub async fn create_message(
    State(state): State<AppState>,
    Json(body): Json<CreateMessageBody>,
) -> Response {
    if body.body.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "body is required" }));
    }
    let to_spec = match body.to {
        Value::String(s) => ToSpec::Key(s),
        v @ Value::Object(_) => match serde_json::from_value::<MessageTarget>(v) {
            Ok(t) => ToSpec::Target(t),
            Err(e) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": format!("unparseable target: {e}") }),
                )
            }
        },
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                json!({ "error": "to must be a string or a MessageTarget object" }),
            )
        }
    };
    let from: Actor = match body.from {
        Some(v) => match serde_json::from_value(v) {
            Ok(a) => a,
            Err(e) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": format!("unparseable from actor: {e}") }),
                )
            }
        },
        None => Actor::Human { name: "owner".into() },
    };
    let thread: Option<MessageId> = match &body.thread {
        Some(t) => match MessageId::parse(t) {
            Ok(id) => Some(id),
            Err(e) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": e.to_string(), "thread": t }),
                )
            }
        },
        None => None,
    };
    let text = body.body;

    let slot: Arc<Mutex<Option<CreateOutcome>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();

    let write = state
        .store
        .write_async(move |conn| {
            // Resolve the recipient against current state, inside the writer
            // transaction so it cannot race a delete — through the SAME resolver
            // /api/env/apply uses (ethos D6).
            let target: MessageTarget = match &to_spec {
                ToSpec::Target(t) => t.clone(),
                ToSpec::Key(s) => match resolve_recipient(conn, s)? {
                    Some(t) => t,
                    None => {
                        return finish(
                            &slot_w,
                            CreateOutcome::NotFound { what: format!("worker '{s}'") },
                            no_write(),
                        )
                    }
                },
            };
            // An object-target Worker may name a since-deleted worker; the Key
            // path above already resolved a live one.
            if let MessageTarget::Worker(w) = &target {
                if queries::get_worker(conn, w.as_str())?.is_none() {
                    return finish(
                        &slot_w,
                        CreateOutcome::NotFound { what: format!("worker {}", w.as_str()) },
                        no_write(),
                    );
                }
            }
            if let Some(t) = &thread {
                if message_by_id(conn, t.as_str())?.is_none() {
                    return finish(
                        &slot_w,
                        CreateOutcome::NotFound {
                            what: format!("thread parent {}", t.as_str()),
                        },
                        no_write(),
                    );
                }
            }

            let now = Utc::now();
            let (parent, children_ids, commands_enqueued, events) =
                insert_message_and_deliver(conn, from.clone(), target, text.clone(), thread.clone(), now)?;

            finish(
                &slot_w,
                CreateOutcome::Created {
                    message: message_body(&parent),
                    fan_out: children_ids,
                    commands_enqueued,
                },
                WriteOutcome { applied: true, events },
            )
        })
        .await;

    let reply = match write {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let outcome = slot.lock().expect("outcome slot poisoned").take();
    match outcome {
        None => internal("create produced no outcome"),
        Some(CreateOutcome::NotFound { what }) => err(
            StatusCode::NOT_FOUND,
            json!({ "error": "recipient not found", "missing": what }),
        ),
        Some(CreateOutcome::Created { message, fan_out, commands_enqueued }) => (
            StatusCode::CREATED,
            Json(json!({
                "message": message,
                "fan_out": fan_out,
                "commands_enqueued": commands_enqueued,
                "rev": reply.rev.0,
            })),
        )
            .into_response(),
    }
}

// ---- GET /api/messages ----------------------------------------------------

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default)]
    pub offset: u64,
    #[serde(default = "default_limit")]
    pub limit: u64,
}

fn default_limit() -> u64 {
    200
}

/// Newest first, PagedResponse-shaped (Invariant 40: `total`/`truncated`
/// announce what the page omits).
pub async fn list_messages(
    State(state): State<AppState>,
    Query(p): Query<ListParams>,
) -> Response {
    let offset = p.offset;
    let limit = p.limit.clamp(1, 1000);
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<Message>, u64)> {
        let conn = store.read()?;
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM _amux_messages", [], |r| r.get(0))?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {MSG_COLS} FROM _amux_messages
             ORDER BY created_at DESC, id DESC LIMIT ?1 OFFSET ?2"
        ))?;
        let rows = stmt.query_map(params![limit as i64, offset as i64], row_to_message)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok((out, total as u64))
    })
    .await;
    let (rows, total) = match joined {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };
    let items: Vec<Value> = rows.iter().map(message_body).collect();
    match PagedResponse::new(items, total, offset, limit) {
        Ok(page) => Json(serde_json::to_value(&page).unwrap_or(Value::Null)).into_response(),
        Err(e) => internal(e),
    }
}

// ---- GET /api/messages/accountability -------------------------------------
//
// "Did every human message become executed work accountable by a worker?"
// (Ethan, 2026-08-12; AMUX-2985/2986/2987). The honest answer needs a JOIN
// nothing exposed: human messages live in `cmd_history` (type='user', empty
// origin — the [HH:MM] dashboard sends; a scheduler carries origin=<title>, an
// agent carries origin=<session>), and the work lives on the board (`issues`).
//
// There is NO per-message->card link today (cmd_history.card_id is NULL for
// 100% of human messages), so this cannot claim "ask X was done by card Y".
// What it CAN do, honestly, is the PROXY: per target worker, count human
// messages in the window against cards that worker CREATED or MOVED in the same
// window, and flag a worker that received messages but produced zero board
// activity. That flag is the self-surfacing signal (ethos rule 4) the drive
// loop can nudge on — the manual cross-reference that produced the first audit
// was exactly the instrument that should not have to be hand-run.

#[derive(Deserialize)]
pub struct AccountabilityParams {
    /// Look-back window in hours (default 24).
    #[serde(default = "default_since_h")]
    pub since_h: u64,
}

fn default_since_h() -> u64 {
    24
}

/// A worker that got human messages in the window but produced no board card.
pub(crate) struct Unaccounted {
    pub worker: String,
    pub human_messages: u64,
    pub latest_snippet: String,
}

/// The full accountability rollup — shared by the HTTP endpoint and the
/// automatic nudge sweep so the two cannot compute "unaccounted" differently
/// (the ethos duplication rule: one definition, two consumers).
pub(crate) struct Rollup {
    pub total_human_messages: u64,
    pub total_linked: u64,
    pub workers: Vec<Value>,
    pub unaccounted: Vec<Unaccounted>,
}

pub(crate) fn compute_rollup(conn: &Connection, since_h: u64) -> rusqlite::Result<Rollup> {
    let now_s = Utc::now().timestamp();
    let cutoff_s = now_s - (since_h as i64) * 3600;
    let cutoff_ms = cutoff_s * 1000; // cmd_history.ts is MILLISECONDS; issues.* are seconds.

    #[derive(Default)]
    struct Row {
        msgs: u64,
        linked: u64,
        latest_ts_ms: i64,
        latest_snippet: String,
        created: u64,
        moved: u64,
    }
    let mut rows: std::collections::BTreeMap<String, Row> = std::collections::BTreeMap::new();

    let mut stmt = conn.prepare(
        "SELECT session, ts, card_id, substr(text,1,80) \
         FROM cmd_history \
         WHERE type='user' AND origin='' AND ts >= ?1 \
         ORDER BY ts DESC",
    )?;
    let mut q = stmt.query(params![cutoff_ms])?;
    while let Some(r) = q.next()? {
        // session is NOT NULL in cmd_history, but read defensively — a NULL
        // anywhere here 500s the endpoint (issues.session IS nullable).
        let session: String = r.get::<_, Option<String>>(0)?.unwrap_or_default();
        if session.is_empty() {
            continue;
        }
        let ts: i64 = r.get(1)?;
        let card_id: Option<String> = r.get(2)?;
        let snippet: String = r.get::<_, String>(3)?.replace(['\n', '\r'], " ");
        let e = rows.entry(session).or_default();
        e.msgs += 1;
        if card_id.as_deref().map(|c| !c.is_empty()).unwrap_or(false) {
            e.linked += 1;
        }
        if ts > e.latest_ts_ms {
            e.latest_ts_ms = ts;
            e.latest_snippet = snippet;
        }
    }

    let mut bstmt = conn.prepare(
        "SELECT session, \
                SUM(CASE WHEN created >= ?1 THEN 1 ELSE 0 END), \
                SUM(CASE WHEN updated >= ?1 THEN 1 ELSE 0 END) \
         FROM issues \
         WHERE COALESCE(deleted,0)=0 AND (created >= ?1 OR updated >= ?1) \
         GROUP BY session",
    )?;
    let mut bq = bstmt.query(params![cutoff_s])?;
    while let Some(r) = bq.next()? {
        // issues.session is nullable — a NULL here is what 500'd the first live
        // call. Skip it: an ownerless card is not a worker's tracked work.
        let Some(session) = r.get::<_, Option<String>>(0)? else { continue };
        if let Some(e) = rows.get_mut(&session) {
            e.created = r.get::<_, i64>(1)? as u64;
            e.moved = r.get::<_, i64>(2)? as u64;
        }
    }

    let mut workers = Vec::new();
    let mut unaccounted = Vec::new();
    let mut total_human_messages = 0u64;
    let mut total_linked = 0u64;
    for (session, e) in &rows {
        total_human_messages += e.msgs;
        total_linked += e.linked;
        // A worker with messages but zero board activity has no tracked work to
        // show for the asks. It is a PROXY: a terse "continue"/"do it", or a
        // board-less personal lane, lands here legitimately — a prompt to look,
        // not a verdict of failure.
        let accounted = e.created > 0 || e.moved > 0;
        workers.push(json!({
            "worker": session,
            "human_messages": e.msgs,
            "messages_linked_to_a_card": e.linked,
            "cards_created_in_window": e.created,
            "cards_moved_in_window": e.moved,
            "latest_message_snippet": e.latest_snippet,
            "verdict": if accounted { "tracking" } else { "no-board-activity" },
        }));
        if !accounted {
            unaccounted.push(Unaccounted {
                worker: session.clone(),
                human_messages: e.msgs,
                latest_snippet: e.latest_snippet.clone(),
            });
        }
    }
    workers.sort_by(|a, b| {
        b["human_messages"].as_u64().unwrap_or(0).cmp(&a["human_messages"].as_u64().unwrap_or(0))
    });
    unaccounted.sort_by_key(|b| std::cmp::Reverse(b.human_messages));

    Ok(Rollup { total_human_messages, total_linked, workers, unaccounted })
}

pub async fn accountability(
    State(state): State<AppState>,
    Query(p): Query<AccountabilityParams>,
) -> Response {
    let since_h = p.since_h.clamp(1, 24 * 30);
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let conn = store.read()?;
        let r = compute_rollup(&conn, since_h)?;
        let unaccounted: Vec<Value> = r
            .unaccounted
            .iter()
            .map(|u| json!({"worker": u.worker, "human_messages": u.human_messages,
                "latest_message_snippet": u.latest_snippet}))
            .collect();
        Ok(json!({
            "since_h": since_h,
            "total_human_messages": r.total_human_messages,
            "total_linked_to_a_card": r.total_linked,
            "linkage_note": "cmd_history.card_id is the ONLY hard link from an ask to its work; \
                             it is ~0 today, so `verdict` is a board-activity PROXY, not proof a \
                             specific ask was executed. The durable fix is stamping card_id when a \
                             worker opens a card from a message (AMUX-2986).",
            "workers": r.workers,
            "unaccounted": unaccounted,
            "unaccounted_count": r.unaccounted.len(),
        }))
    })
    .await;
    match joined {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- automatic accountability nudge (AMUX-2990) ---------------------------
//
// Ethan, 2026-08-12: "the accountability shit needs to be automatic." A server
// background sweep finds unaccounted lanes and STEERS each one directly to open
// a card and pursue it — server-side delivery reaches any lane regardless of
// group, which `amux` (a worker) cannot do. Deduped to at most one nudge per
// lane per cooldown (default 24h, Ethan's chosen cadence) via a persisted
// `accountability_nudged` prefs map, so a standing gap is one nudge/day, not one
// every tick.

const NUDGE_PREFS_KEY: &str = "accountability_nudged";

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// One sweep: nudge every lane that has been unaccounted longer than the
/// cooldown. Public(crate) so a test can drive it against a seeded store.
pub(crate) async fn accountability_tick(state: &AppState) {
    let since_h = env_u64("AMUX_ACCOUNTABILITY_WINDOW_H", 24);
    let cooldown_s = env_u64("AMUX_ACCOUNTABILITY_NUDGE_COOLDOWN_H", 24) as i64 * 3600;
    let now = Utc::now().timestamp();

    let store = state.store.clone();
    let gaps = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<(String, u64, String)>> {
        let conn = store.read()?;
        let r = compute_rollup(&conn, since_h)?;
        Ok(r
            .unaccounted
            .into_iter()
            .map(|u| (u.worker, u.human_messages, u.latest_snippet))
            .collect())
    })
    .await;
    let gaps = match gaps {
        Ok(Ok(g)) => g,
        Ok(Err(e)) => {
            tracing::warn!(error=%e, "[accountability] sweep query failed");
            return;
        }
        Err(e) => {
            tracing::warn!(error=%e, "[accountability] sweep task panicked");
            return;
        }
    };
    if gaps.is_empty() {
        return;
    }

    // The dedup map: {worker -> last nudge unix-seconds}. A missing worker means
    // never nudged. Read once; write once at the end with the new stamps.
    let mut nudged: std::collections::HashMap<String, i64> = {
        let store = state.store.clone();
        tokio::task::spawn_blocking(move || {
            store
                .read()
                .ok()
                .and_then(|c| {
                    c.query_row(
                        "SELECT value FROM prefs WHERE key=?1",
                        params![NUDGE_PREFS_KEY],
                        |r| r.get::<_, String>(0),
                    )
                    .ok()
                })
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default()
    };

    let mut sent = 0usize;
    for (worker, msgs, snippet) in gaps {
        // ISOLATED (AMUX-3232): a raw agent leaves no board trace on purpose and
        // has no session/URL to open a card, so the accountability nudge would be
        // both impossible to satisfy and harness noise inside a lane meant to run
        // untouched. Skip it, reading the same source of truth the spawn path reads.
        if crate::api::session_verbs::session_is_isolated(&worker) {
            continue;
        }
        let last = nudged.get(&worker).copied().unwrap_or(0);
        if now - last < cooldown_s {
            continue; // within cooldown — one nudge/day, not one/tick
        }
        let text = format!(
            "[amux accountability] You have {msgs} message(s) from Ethan in the last {since_h}h with \
             no board card created or moved — the work isn't tracked yet. Please open a board card \
             for the ask (owned by you) and pursue it. Most recent: \"{snippet}\"",
        );
        let _ = crate::api::session_verbs::steer_enqueue(state, &worker, &text, "accountability", "").await;
        nudged.insert(worker.clone(), now);
        sent += 1;
        tracing::info!(worker=%worker, human_messages=msgs, "[accountability] nudged unaccounted lane");
    }
    if sent == 0 {
        return;
    }

    // Persist the updated stamps so the next sweep respects the cooldown.
    let body = serde_json::to_string(&nudged).unwrap_or_else(|_| "{}".into());
    let _ = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO prefs (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![NUDGE_PREFS_KEY, body],
            )?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    tracing::info!(nudged = sent, "[accountability] sweep nudged unaccounted lanes");
}

/// Register the periodic sweep. Interval default 30m; the per-lane cooldown
/// (24h) is what actually bounds how often any one lane hears from it.
pub fn accountability_spawn(state: AppState) -> crate::runtime_jobs::PeriodicTask {
    let secs = env_u64("AMUX_ACCOUNTABILITY_SWEEP_SECS", 1800);
    crate::runtime_jobs::spawn_periodic("accountability-nudge", secs, move || {
        let state = state.clone();
        async move {
            accountability_tick(&state).await;
        }
    })
}

// ---- GET /api/messages/{id} -----------------------------------------------

pub async fn get_message(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    let store = state.store.clone();
    let k = key.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Message>> {
        let conn = store.read()?;
        Ok(message_by_id(&conn, &k)?)
    })
    .await;
    match joined {
        Ok(Ok(Some(m))) => Json(message_body(&m)).into_response(),
        Ok(Ok(None)) => err(
            StatusCode::NOT_FOUND,
            json!({ "error": "message not found", "key": key }),
        ),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- POST /api/messages/{id}/ack + /acted ---------------------------------

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ActedBody {
    /// The task the recipient opened because of this message, if any —
    /// closes the loop between a steer and the work it caused.
    #[serde(default)]
    pub task: Option<String>,
}

enum AdvanceOutcome {
    NotFound,
    /// Backwards/sideways move refused by the core (forward-only record).
    Refused { from: &'static str, to: &'static str },
    Applied { delivery: Value },
}

async fn advance(
    state: AppState,
    key: String,
    next: DeliveryState,
) -> Response {
    let slot: Arc<Mutex<Option<AdvanceOutcome>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let key_w = key.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let Some(mut msg) = message_by_id(conn, &key_w)? else {
                return finish(&slot_w, AdvanceOutcome::NotFound, no_write());
            };
            let from_name = msg.delivery.name();
            match msg.advance_delivery(next) {
                Err(e) => finish(
                    &slot_w,
                    AdvanceOutcome::Refused { from: e.from, to: e.to },
                    no_write(),
                ),
                Ok(()) => {
                    conn.execute(
                        "UPDATE _amux_messages SET delivery = ?2 WHERE id = ?1",
                        params![
                            msg.id.as_str(),
                            serde_json::to_string(&msg.delivery).map_err(corrupt)?
                        ],
                    )?;
                    let events = vec![ev(
                        EntityType::Message,
                        msg.id.as_str(),
                        MutationKind::StatusChanged {
                            from: from_name.into(),
                            to: msg.delivery.name().into(),
                        },
                    )];
                    finish(
                        &slot_w,
                        AdvanceOutcome::Applied {
                            delivery: serde_json::to_value(&msg.delivery)
                                .unwrap_or(Value::Null),
                        },
                        WriteOutcome { applied: true, events },
                    )
                }
            }
        })
        .await;
    let reply = match write {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let outcome = slot.lock().expect("outcome slot poisoned").take();
    match outcome {
        None => internal("advance produced no outcome"),
        Some(AdvanceOutcome::NotFound) => err(
            StatusCode::NOT_FOUND,
            json!({ "error": "message not found", "key": key }),
        ),
        Some(AdvanceOutcome::Refused { from, to }) => err(
            StatusCode::CONFLICT,
            json!({
                "error": "delivery state only moves forward",
                "from": from,
                "to": to,
            }),
        ),
        Some(AdvanceOutcome::Applied { delivery }) => Json(json!({
            "applied": true,
            "delivery": delivery,
            "rev": reply.rev.0,
        }))
        .into_response(),
    }
}

pub async fn ack_message(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    advance(state, key, DeliveryState::Acknowledged { at: Utc::now() }).await
}

pub async fn acted_message(
    State(state): State<AppState>,
    Path(key): Path<String>,
    body: Option<Json<ActedBody>>,
) -> Response {
    let task = match body.and_then(|Json(b)| b.task) {
        Some(t) => match TaskId::parse(&t) {
            Ok(id) => Some(id),
            Err(e) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": e.to_string(), "task": t }),
                )
            }
        },
        None => None,
    };
    advance(state, key, DeliveryState::ActedOn { at: Utc::now(), task }).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{router, AppState};
    use crate::db::{SharedStore, Store};
    use crate::opencode::mock::{MockProtocol, RecordedCall};
    use crate::opencode::AgentState;
    use amux_core::protocol::CommandState;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    fn app() -> (axum::Router, SharedStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("amux-test.db")).unwrap());
        let state = AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        (router(state), store, dir)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let b = Request::builder().method(method).uri(path);
        let req = match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, v)
    }

    async fn create_worker(app: &axum::Router, name: &str, group: Option<&str>) -> String {
        let mut body = json!({ "display_name": name, "cwd": "/tmp/w" });
        if let Some(g) = group {
            body["group"] = json!(g);
        }
        let (st, v) = send(app, "POST", "/api/workers", Some(body)).await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        v["id"].as_str().unwrap().to_string()
    }

    fn pump_runtime(
        store: SharedStore,
        protocol: Arc<MockProtocol>,
    ) -> crate::orchestrator::runtime::Runtime {
        crate::orchestrator::runtime::Runtime {
            store,
            backends: vec![],
            tick_secs: 3,
            heartbeat_every: 1000,
            breaker: amux_core::circuit::FleetCircuitBreaker {
                window_budget_tokens: u64::MAX,
                window_secs: 3600,
                min_progress_per_window: 0,
                max_failures_per_window: 1000,
            },
            fleet_state: std::sync::Mutex::new(amux_core::circuit::FleetState::Normal),
            protocol: Some(protocol),
            pickup_unowned: false,
            resume_stagger_secs: 5,
        }
    }

    #[tokio::test]
    async fn post_to_worker_enqueues_and_pump_delivers_the_real_body() {
        let (app, store, _dir) = app();
        let wid = create_worker(&app, "w", None).await;

        // The body carries shell metacharacters ON PURPOSE: it must arrive
        // byte-identical, never evaluated or re-derived (AMUX-1888's class).
        let text = "review AR-42 `now` $(please)";
        let (st, v) = send(
            &app,
            "POST",
            "/api/messages",
            Some(json!({ "to": wid, "body": text })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        let msg_id = v["message"]["id"].as_str().unwrap().to_string();
        assert_eq!(v["message"]["delivery"]["state"], json!("queued"));
        assert_eq!(v["commands_enqueued"], json!(1));
        assert_eq!(v["fan_out"], json!([]));

        // The queued command references the message, not the text.
        let worker = WorkerId::parse(&wid).unwrap();
        {
            let conn = store.read().unwrap();
            let head = crate::db::commands::next_deliverable(&conn, &worker)
                .unwrap()
                .expect("a DeliverMessage command is queued");
            assert_eq!(
                head.command,
                WorkerCommand::DeliverMessage(MessageId::parse(&msg_id).unwrap())
            );
            assert_eq!(head.timing, DeliveryTiming::AtTurnBoundary);
        }

        // Pump at a turn boundary: the protocol receives the REAL body.
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(worker.clone(), AgentState::Idle);
        let rt = pump_runtime(store.clone(), protocol.clone());
        rt.pump_commands(Utc::now(), &std::collections::BTreeMap::new()).await.unwrap();
        let calls = protocol.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        match &calls[0] {
            RecordedCall::DeliverMessage { worker: w, msg, body } => {
                assert_eq!(w, &worker);
                assert_eq!(msg.as_str(), msg_id);
                assert_eq!(body, text, "the durable body, byte-identical");
            }
            other => panic!("expected DeliverMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pump_fails_delivery_when_message_row_is_missing() {
        // A DeliverMessage command whose message row does not exist must
        // FAIL, not deliver an empty body (the silent-blank-email shape).
        let (app, store, _dir) = app();
        let wid = create_worker(&app, "w", None).await;
        let worker = WorkerId::parse(&wid).unwrap();
        let ghost = MessageId::from_ulid(ulid::Ulid::new());
        let cmd_id = CommandId::from_ulid(ulid::Ulid::new());
        {
            let (worker, ghost, cmd_id) = (worker.clone(), ghost.clone(), cmd_id.clone());
            store
                .write(move |conn| {
                    crate::db::commands::enqueue(
                        conn,
                        cmd_id,
                        &worker,
                        &WorkerCommand::DeliverMessage(ghost.clone()),
                        "ghost-key",
                        &DeliveryTiming::AtTurnBoundary,
                        None,
                        Utc::now(),
                    )?;
                    Ok(WriteOutcome { applied: true, events: vec![] })
                })
                .unwrap();
        }
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(worker.clone(), AgentState::Idle);
        let rt = pump_runtime(store.clone(), protocol.clone());
        rt.pump_commands(Utc::now(), &std::collections::BTreeMap::new()).await.unwrap();

        assert!(protocol.calls().is_empty(), "nothing must reach the agent");
        let conn = store.read().unwrap();
        let cmd = crate::db::commands::by_id(&conn, &cmd_id).unwrap().unwrap();
        assert!(
            matches!(&cmd.state, CommandState::Failed { reason }
                if reason.contains("message body lookup failed")),
            "{:?}",
            cmd.state
        );
    }

    #[tokio::test]
    async fn group_target_fans_out_per_member_with_commands() {
        let (app, store, _dir) = app();
        let grp = GroupId::from_ulid(ulid::Ulid::new());
        let w1 = create_worker(&app, "alpha", Some(grp.as_str())).await;
        let w2 = create_worker(&app, "beta", Some(grp.as_str())).await;
        let _lone = create_worker(&app, "outsider", None).await;

        let (st, v) = send(
            &app,
            "POST",
            "/api/messages",
            Some(json!({ "to": grp.as_str(), "body": "standup in 5" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        let parent_id = v["message"]["id"].as_str().unwrap().to_string();
        let children = v["fan_out"].as_array().unwrap();
        assert_eq!(children.len(), 2, "one child per member, outsider excluded");
        assert_eq!(v["commands_enqueued"], json!(2));

        // Each child is threaded to the parent with its own delivery record,
        // and each member worker has a queued DeliverMessage.
        let conn = store.read().unwrap();
        for child_id in children {
            let m = message_by_id(&conn, child_id.as_str().unwrap()).unwrap().unwrap();
            assert_eq!(m.thread.as_ref().map(|t| t.as_str()), Some(parent_id.as_str()));
            assert_eq!(m.body, "standup in 5");
            assert_eq!(m.delivery, DeliveryState::Queued);
        }
        for w in [&w1, &w2] {
            let worker = WorkerId::parse(w).unwrap();
            assert!(
                crate::db::commands::next_deliverable(&conn, &worker).unwrap().is_some(),
                "member {w} has a queued delivery"
            );
        }
        // Total rows: parent + 2 children.
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM _amux_messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
    }

    #[tokio::test]
    async fn ack_and_acted_advance_forward_only() {
        let (app, _store, _dir) = app();
        let (st, v) = send(
            &app,
            "POST",
            "/api/messages",
            Some(json!({ "to": "human", "body": "fyi" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let id = v["message"]["id"].as_str().unwrap().to_string();

        // Queued -> Acknowledged (skipping Delivered is legal: an ack seen
        // out-of-band implies delivery).
        let (st, v) = send(&app, "POST", &format!("/api/messages/{id}/ack"), None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["delivery"]["state"], json!("acknowledged"));

        // Acknowledged -> ActedOn.
        let (st, v) = send(&app, "POST", &format!("/api/messages/{id}/acted"), None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["delivery"]["state"], json!("acted_on"));

        // Backwards: 409 naming both sides, record unchanged.
        let (st, v) = send(&app, "POST", &format!("/api/messages/{id}/ack"), None).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(v["from"], json!("acted_on"));
        assert_eq!(v["to"], json!("acknowledged"));
        let (_, back) = send(&app, "GET", &format!("/api/messages/{id}"), None).await;
        assert_eq!(back["delivery"]["state"], json!("acted_on"));
    }

    #[tokio::test]
    async fn list_pages_and_get_resolves() {
        let (app, _store, _dir) = app();
        for i in 0..3 {
            let (st, _) = send(
                &app,
                "POST",
                "/api/messages",
                Some(json!({ "to": "human", "body": format!("note {i}") })),
            )
            .await;
            assert_eq!(st, StatusCode::CREATED);
        }
        let (st, v) = send(&app, "GET", "/api/messages?limit=2", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["total"], json!(3));
        assert_eq!(v["items"].as_array().unwrap().len(), 2);
        assert_eq!(v["truncated"], json!(true));

        let id = v["items"][0]["id"].as_str().unwrap().to_string();
        let (st, m) = send(&app, "GET", &format!("/api/messages/{id}"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(m["id"], json!(id));

        let (st, _) = send(&app, "GET", "/api/messages/msg_missing", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn accountability_tick_nudges_the_uncooled_lane_and_skips_the_cooled_one() {
        let (_app, store, _dir) = app();
        let state = AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        let now_s = Utc::now().timestamp();
        let now_ms = now_s * 1000;
        // AF-188: the enqueue refuses a target with no env file, so both lanes
        // must be REGISTERED for this test to exercise nudging rather than
        // refusal. Before the widening it queued for lanes that never existed,
        // which is precisely the shape being stopped.
        let _home = crate::api::settings::test_env::set_home(_dir.path());
        std::fs::create_dir_all(_dir.path().join("sessions")).unwrap();
        for w in ["w-gap", "w-cooled"] {
            std::fs::write(_dir.path().join(format!("sessions/{w}.env")), "CC_DIR=/tmp\n").unwrap();
        }
        // Both lanes are unaccounted (a human message, no board card). w-cooled
        // was "already nudged just now" via the prefs stamp; w-gap never was.
        store
            .write(move |conn| {
                for w in ["w-gap", "w-cooled"] {
                    conn.execute(
                        "INSERT INTO cmd_history (text,type,session,ts,origin) VALUES ('do the thing','user',?1,?2,'')",
                        params![w, now_ms],
                    )
                    .unwrap();
                }
                conn.execute(
                    "INSERT INTO prefs (key,value) VALUES (?1,?2)",
                    params![NUDGE_PREFS_KEY, format!("{{\"w-cooled\":{now_s}}}")],
                )
                .unwrap();
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();

        accountability_tick(&state).await;

        let steers = |w: &str| -> i64 {
            store
                .read()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM steering_queue WHERE session=?1",
                    params![w],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(steers("w-gap"), 1, "an un-nudged unaccounted lane must be steered");
        assert_eq!(steers("w-cooled"), 0, "a lane nudged within the cooldown must be skipped");
    }

    #[tokio::test]
    async fn accountability_flags_a_worker_with_messages_but_no_board_activity() {
        let (app, store, _dir) = app();
        let now_s = Utc::now().timestamp();
        let now_ms = now_s * 1000;
        // w-gap: got a human message, produced no board card. w-ok: got a human
        // message AND created a card in-window. w-sched: a SCHEDULER message
        // (origin set) that must be excluded from the human count.
        store
            .write(move |conn| {
                let ins_msg = |session: &str, origin: &str, ty: &str, ts: i64| {
                    conn.execute(
                        "INSERT INTO cmd_history (text, type, session, ts, origin) VALUES (?1,?2,?3,?4,?5)",
                        params![format!("ask for {session}"), ty, session, ts, origin],
                    )
                    .unwrap();
                };
                ins_msg("w-gap", "", "user", now_ms - 1000);
                ins_msg("w-ok", "", "user", now_ms - 1000);
                ins_msg("w-sched", "Daily thing", "user", now_ms - 1000); // origin set -> not human
                // A board card w-ok created just now; w-gap has none.
                conn.execute(
                    "INSERT INTO issues (id,title,status,session,created,updated) VALUES ('I-OK','t','todo','w-ok',?1,?1)",
                    params![now_s],
                )
                .unwrap();
                // An OWNERLESS card (session NULL) in-window — this 500'd the
                // first live call (issues.session is nullable). The endpoint must
                // skip it, not choke on the NULL.
                conn.execute(
                    "INSERT INTO issues (id,title,status,session,created,updated) VALUES ('I-NULL','t','todo',NULL,?1,?1)",
                    params![now_s],
                )
                .unwrap();
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();

        let (st, v) = send(&app, "GET", "/api/messages/accountability?since_h=24", None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        // Only the two human messages count; the scheduler one is excluded.
        assert_eq!(v["total_human_messages"], json!(2), "{v}");
        let unacc: Vec<&str> = v["unaccounted"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|u| u["worker"].as_str())
            .collect();
        assert_eq!(unacc, vec!["w-gap"], "only the worker with no board activity is flagged: {v}");
        // And w-ok reads as tracking, not flagged.
        let ok_row = v["workers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["worker"] == "w-ok")
            .unwrap();
        assert_eq!(ok_row["verdict"], json!("tracking"));
        assert_eq!(ok_row["cards_created_in_window"], json!(1));
    }

    #[tokio::test]
    async fn unknown_worker_recipient_is_404_and_nothing_persists() {
        let (app, store, _dir) = app();
        let (st, v) = send(
            &app,
            "POST",
            "/api/messages",
            Some(json!({ "to": "nobody-here", "body": "hello" })),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
        let conn = store.read().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM _amux_messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "a refused create leaves no row behind");
    }
}
