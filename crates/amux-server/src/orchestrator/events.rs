//! WorkerEvent processing + turn tracking (RR-0065, Invariants 5/6/11/26/34).
//!
//! One processor task per worker with a live session: it subscribes to the
//! agent protocol's event stream and turns every `WorkerEvent` into durable
//! state — a `_amux_turns` ledger row, a worker-state write, a command
//! transition — inside ONE `Store::write_async` transaction per event, so
//! every consequence of an event shares a revision and SSE sees it within
//! one event cycle (Invariant 11: stale worker state is a bug, and a state
//! that changed without an event is unsyncable).
//!
//! The confirmation contract (Invariant 34, delivery step 5): the pump marks
//! a command `Delivered` when the protocol acks receipt; `TurnCompleted` is
//! what proves the worker actually ACTED on it, so this module — not the
//! pump — performs the `Delivered -> Confirmed` transition. A command still
//! `Dispatched` at turn end is deliberately NOT confirmed: the protocol
//! never acked receipt, so a completed turn proves nothing about it; the
//! retry/timeout path (RR-0064) owns that case.
//!
//! Lagged broadcast receives log the missed count (Invariant 26): the event
//! channel is lossy by contract ("drop oldest, gap marker"), and a consumer
//! that cannot see the size of its gap is a consumer that will trust an
//! event-derived picture with a hole in it. Recovery is NOT attempted from
//! the stream — per Invariant 34 the correct move after a gap is to re-read
//! current state from the store, which the next full event write does.

use crate::db::{PendingEvent, SharedStore, WriteOutcome};
use crate::db::{commands, queries};
use crate::opencode::AgentProtocol;
use amux_core::ids::{TaskId, WorkerId};
use amux_core::limits::AttemptRecord;
use amux_core::protocol::{
    CommandState, CommandTransition, WorkerCommand, WorkerEvent,
};
use amux_core::revision::{EntityType, MutationKind};
use amux_core::session::ExitReason;
use amux_core::worker::WorkerState;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Retry budget, matching the pump's (Invariant 34 default).
const MAX_ATTEMPTS: u32 = 3;

fn corrupt(e: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
}

/// The serde tag of a WorkerState, for StatusChanged events.
fn state_tag(state: &WorkerState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.get("state").and_then(|s| s.as_str()).map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

fn ev(entity_type: EntityType, id: &str, mutation: MutationKind) -> PendingEvent {
    PendingEvent {
        entity_type,
        entity_id: id.to_string(),
        mutation,
        payload: None,
    }
}

/// Write the worker's new state and push the StatusChanged event. A write
/// that matches no row (unregistered or soft-deleted worker) is logged, not
/// silently absorbed — an event stream for a worker the store cannot see is
/// itself the diagnosis (ethos rule 4).
fn write_state(
    conn: &Connection,
    prior: Option<&WorkerState>,
    wid: &str,
    new_state: &WorkerState,
    now_s: &str,
    events: &mut Vec<PendingEvent>,
) -> rusqlite::Result<()> {
    let n = queries::update_worker_state(conn, wid, new_state, now_s)?;
    if n > 0 {
        // Post-mutation snapshot for the journal (RR-0111a): one indexed
        // read inside the same transaction. Worker state events fire every
        // turn — leaving them payload-less would advance the replay horizon
        // on every turn, making worker replay permanently unknown.
        let payload = queries::get_worker(conn, wid)?.map(|r| r.snapshot());
        events.push(PendingEvent {
            entity_type: EntityType::Worker,
            entity_id: wid.to_string(),
            mutation: MutationKind::StatusChanged {
                from: prior.map(state_tag).unwrap_or_else(|| "unknown".into()),
                to: state_tag(new_state),
            },
            payload,
        });
    } else {
        tracing::warn!(
            worker = wid,
            "worker state write matched no row — events for an unregistered/deleted worker"
        );
    }
    Ok(())
}

/// Apply one WorkerEvent inside the writer transaction. Pure with respect to
/// time (`now` is a parameter) so tests drive it deterministically; every
/// durable consequence emits a PendingEvent, and `applied` is true iff at
/// least one consequence landed — a no-op must not bump the revision
/// (Invariant 37).
pub fn apply_event(
    conn: &Connection,
    worker: &WorkerId,
    event: &WorkerEvent,
    now: DateTime<Utc>,
) -> rusqlite::Result<WriteOutcome> {
    let wid = worker.as_str();
    let now_s = now.to_rfc3339();
    let prior_state = queries::get_worker(conn, wid)?.map(|r| r.state);
    let mut events: Vec<PendingEvent> = Vec::new();

    match event {
        WorkerEvent::TurnStarted { turn_id } => {
            // The turn is a fact even when session bookkeeping is behind:
            // record it with a NULL session_id rather than dropping it.
            let ses_id = queries::live_session_for(conn, wid)?.map(|s| s.id);
            if ses_id.is_none() {
                tracing::warn!(worker = wid, turn = turn_id.as_str(),
                    "turn started with no live session row; recorded with NULL session_id");
            }
            let n = conn.execute(
                "INSERT OR IGNORE INTO _amux_turns (id, session_id, worker_id, started_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![turn_id.as_str(), ses_id, wid, now_s],
            )?;
            if n > 0 {
                events.push(ev(EntityType::Turn, turn_id.as_str(), MutationKind::Created));
            } else {
                tracing::warn!(turn = turn_id.as_str(), "duplicate TurnStarted ignored");
            }
            let state = WorkerState::Active { turn: Some(turn_id.clone()) };
            write_state(conn, prior_state.as_ref(), wid, &state, &now_s, &mut events)?;
        }

        WorkerEvent::Progress(p) => {
            // "Tokens so far this turn": the latest report supersedes, it
            // does not add. Stored as {"reported_total": N} — a shape that
            // cannot be mistaken for the per-field TokenUsage breakdown the
            // provider did NOT report (Invariant 20: never invent).
            if let Some(total) = p.tokens_used {
                let open: Option<String> = conn
                    .query_row(
                        "SELECT id FROM _amux_turns WHERE worker_id = ?1 AND ended_at IS NULL
                         ORDER BY started_at DESC LIMIT 1",
                        params![wid],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(turn_id) = open {
                    conn.execute(
                        "UPDATE _amux_turns SET tokens = ?2 WHERE id = ?1",
                        params![
                            turn_id,
                            serde_json::json!({ "reported_total": total }).to_string()
                        ],
                    )?;
                    events.push(ev(EntityType::Turn, &turn_id, MutationKind::Updated));
                }
            }
        }

        WorkerEvent::TurnCompleted(res) => {
            // End exactly once (the SQL twin of Turn::end's guard). The raw
            // outcome line is stored as-is, never re-normalized into an enum
            // the event did not state.
            let outcome_json = serde_json::json!({ "outcome": res.outcome }).to_string();
            let n = conn.execute(
                "UPDATE _amux_turns SET ended_at = ?2, outcome = ?3
                 WHERE id = ?1 AND ended_at IS NULL",
                params![res.turn_id.as_str(), now_s, outcome_json],
            )?;
            if n > 0 {
                events.push(ev(EntityType::Turn, res.turn_id.as_str(), MutationKind::Updated));
            } else {
                tracing::warn!(turn = res.turn_id.as_str(),
                    "TurnCompleted for a turn with no open ledger row (missed TurnStarted?)");
            }
            let state = WorkerState::Idle { since: now };
            write_state(conn, prior_state.as_ref(), wid, &state, &now_s, &mut events)?;

            // TurnCompleted IS the confirmation signal (Invariant 34 step 5):
            // the worker demonstrably acted, so the delivered command is done.
            if let Some(cmd) = commands::in_flight(conn, worker)? {
                if matches!(cmd.state, CommandState::Delivered) {
                    commands::transition(conn, &cmd.id, CommandTransition::Confirm, MAX_ATTEMPTS)?;
                    events.push(ev(
                        EntityType::Other("command".into()),
                        cmd.id.as_str(),
                        MutationKind::StatusChanged {
                            from: "delivered".into(),
                            to: "confirmed".into(),
                        },
                    ));
                    // Drift feedback (2026-08-09 adherence audit): the board
                    // is the ledger, so a turn spent ON a board task must be
                    // VISIBLE on that task. If the worker's whole turn passed
                    // without a single board write, note it on the card — the
                    // turn ledger alone is a store the reviewer never opens
                    // (ethos rule 4: a tag nobody reads is the same failure
                    // as no tag). Without this, "worker finished, card
                    // untouched" was observable nowhere.
                    if let WorkerCommand::ExecuteTask(task_id) = &cmd.command {
                        note_untouched_card(conn, wid, task_id, res, now, &mut events)?;
                    }
                }
                // Dispatched-but-unacked: see module docs — not ours to confirm.
            }
        }

        WorkerEvent::Waiting(w) => {
            // Invariant 11's table includes Waiting; it costs one line here
            // and its absence would leave the worker frozen at "active".
            let state = WorkerState::Waiting { reason: w.reason.clone() };
            write_state(conn, prior_state.as_ref(), wid, &state, &now_s, &mut events)?;
        }

        WorkerEvent::RateLimited(rl) => {
            let state = WorkerState::RateLimited { reset_at: rl.reset_at };
            write_state(conn, prior_state.as_ref(), wid, &state, &now_s, &mut events)?;
        }

        WorkerEvent::Failed(f) => {
            // Fail the in-flight command first so the attempt record can
            // name the task it belonged to.
            let mut task_for_attempt: Option<TaskId> = None;
            if let Some(cmd) = commands::in_flight(conn, worker)? {
                let from = cmd.state.name();
                commands::transition(
                    conn,
                    &cmd.id,
                    CommandTransition::Fail { reason: f.reason.clone() },
                    MAX_ATTEMPTS,
                )?;
                events.push(ev(
                    EntityType::Other("command".into()),
                    cmd.id.as_str(),
                    MutationKind::StatusChanged { from: from.into(), to: "failed".into() },
                ));
                if let WorkerCommand::ExecuteTask(t) = &cmd.command {
                    task_for_attempt = Some(t.clone());
                }
            }

            // Attempt ledger (Invariant 49): the record rides into the next
            // attempt's context. `_amux_attempts` is keyed by task, so a
            // failure with no identifiable task (no in-flight ExecuteTask)
            // cannot honestly produce a row — it is logged and lands on the
            // worker state instead of being pinned to an invented task.
            if let Some(task) = task_for_attempt {
                let open: Option<(String, String)> = conn
                    .query_row(
                        "SELECT started_at, tokens FROM _amux_turns
                         WHERE worker_id = ?1 AND ended_at IS NULL
                         ORDER BY started_at DESC LIMIT 1",
                        params![wid],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let (tokens_spent, wall_clock_secs) = match &open {
                    Some((started, tokens_json)) => {
                        let spent = serde_json::from_str::<serde_json::Value>(tokens_json)
                            .ok()
                            .and_then(|v| v.get("reported_total").and_then(|n| n.as_u64()))
                            .unwrap_or(0); // unreported stays 0, not invented
                        let wall = started
                            .parse::<DateTime<Utc>>()
                            .ok()
                            .map(|s| (now - s).num_seconds().max(0) as u64)
                            .unwrap_or(0);
                        (spent, wall)
                    }
                    None => (0, 0),
                };
                let prior_n: u32 = conn.query_row(
                    "SELECT COUNT(*) FROM _amux_attempts WHERE task_id = ?1 AND worker_id = ?2",
                    params![task.as_str(), wid],
                    |r| r.get(0),
                )?;
                let record = AttemptRecord {
                    attempt: prior_n + 1,
                    failure_reason: f.reason.clone(),
                    rejected_evidence: Vec::new(),
                    tokens_spent,
                    wall_clock_secs,
                    decomposition_attempted: false,
                    tree_status: None,
                    at: now,
                };
                conn.execute(
                    "INSERT INTO _amux_attempts (task_id, worker_id, attempt, record, at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        task.as_str(),
                        wid,
                        record.attempt,
                        serde_json::to_string(&record).map_err(corrupt)?,
                        now_s
                    ],
                )?;
                events.push(ev(
                    EntityType::Other("attempt".into()),
                    task.as_str(),
                    MutationKind::Created,
                ));
            } else {
                tracing::warn!(worker = wid, reason = %f.reason,
                    "worker failure with no in-flight ExecuteTask; no attempt row written");
            }

            let state = WorkerState::Error { detail: f.reason.clone() };
            write_state(conn, prior_state.as_ref(), wid, &state, &now_s, &mut events)?;
        }

        WorkerEvent::Exited(status) => {
            if let Some(ses) = queries::live_session_for(conn, wid)? {
                // Invariant 20 applied to exits: only code 0 is a clean
                // completion; anything else (including "no code reported")
                // is a crash with exactly the signal the backend saw.
                let reason = if status.code == Some(0) {
                    ExitReason::Completed
                } else {
                    ExitReason::Crashed { signal: status.signal }
                };
                let n = queries::end_session(conn, &ses.id, &reason, &now_s)?;
                if n > 0 {
                    events.push(ev(
                        EntityType::Session,
                        &ses.id,
                        MutationKind::StatusChanged { from: "running".into(), to: "ended".into() },
                    ));
                }
            }
            write_state(conn, prior_state.as_ref(), wid, &WorkerState::Stopped, &now_s, &mut events)?;
        }

        // Started: liveness, no durable consequence yet (the session row is
        // created by the start path). ToolUsed belongs in logs correlated by
        // turn (Invariant 30), TaskUpdated is the board's own write path,
        // ContextLow drives compaction (RR-0069) — each lands with its item.
        WorkerEvent::Started
        | WorkerEvent::ToolUsed(_)
        | WorkerEvent::TaskUpdated(_)
        | WorkerEvent::ContextLow(_) => {}
    }

    // `applied` mirrors the events: every real write above pushes one, so an
    // event that changed nothing does not bump the revision (Invariant 37).
    Ok(WriteOutcome { applied: !events.is_empty(), events })
}

/// If an ExecuteTask turn completed WITHOUT the worker writing its board
/// card, append a board-visible note to the card (log + rev bump + Task
/// event) so the drift is on the ledger everyone already reads.
///
/// "Untouched" is judged from the card's own `updated` stamp vs the turn's
/// `started_at`: strictly older means no board write landed during the turn.
/// A missing turn row (missed TurnStarted) makes the question unanswerable —
/// skip honestly rather than guess (Invariant 20). A vanished card is warned
/// about, never re-minted.
fn note_untouched_card(
    conn: &Connection,
    wid: &str,
    task_id: &TaskId,
    res: &amux_core::protocol::TurnResult,
    now: DateTime<Utc>,
    events: &mut Vec<PendingEvent>,
) -> rusqlite::Result<()> {
    let started: Option<String> = conn
        .query_row(
            "SELECT started_at FROM _amux_turns WHERE id = ?1",
            params![res.turn_id.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    let Some(started) = started.and_then(|s| s.parse::<DateTime<Utc>>().ok()) else {
        return Ok(()); // no turn start on record: "untouched" is unanswerable
    };
    let Some(row) = crate::orchestrator::context::issue_by_internal_id(conn, task_id)? else {
        tracing::warn!(worker = wid, task = %task_id,
            "ExecuteTask turn completed but its board card no longer resolves");
        return Ok(());
    };
    if row.status.trim().eq_ignore_ascii_case("quarantined") {
        return Ok(()); // orchestrator-terminal: nothing left for the worker to move
    }
    if row.updated >= started.timestamp() {
        return Ok(()); // the worker moved the board this turn — no nag
    }
    let who = queries::get_worker(conn, wid)?
        .map(|w| w.display_name)
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| wid.to_string());
    let mut next = row;
    let stamp = chrono::Local::now().format("%H:%M").to_string();
    next.log = Some(crate::db::board_store::append_log(
        next.log.as_deref(),
        &stamp,
        &format!(
            "runtime: {who} completed a turn (outcome: {}) with no board update — \
             the card did not move; update its status or release it",
            res.outcome
        ),
    ));
    next.rev += 1;
    next.version += 1;
    next.updated = now.timestamp();
    crate::db::board_store::save_patched(conn, &mut next)?;
    events.push(PendingEvent {
        entity_type: EntityType::Task,
        entity_id: next.id.clone(),
        mutation: MutationKind::Updated,
        payload: Some(next.snapshot()),
    });
    Ok(())
}

/// Process one event through the single-writer store.
pub async fn process_event(
    store: &SharedStore,
    worker: &WorkerId,
    event: WorkerEvent,
) -> anyhow::Result<()> {
    let worker = worker.clone();
    store
        .write_async(move |conn| apply_event(conn, &worker, &event, Utc::now()))
        .await?;
    Ok(())
}

/// One processor task per worker: subscribe to the protocol's event stream
/// and apply every event. Exits when the stream closes (worker's protocol
/// session gone); the supervisor reaps and respawns as sessions come and go.
pub fn spawn_event_processor(
    store: SharedStore,
    protocol: Arc<dyn AgentProtocol>,
    worker: WorkerId,
) -> tokio::task::JoinHandle<()> {
    // Subscribe BEFORE spawning: a broadcast channel only delivers to
    // receivers that exist at send time, so subscribing inside the task
    // races the caller's first emit — the event vanishes and the processor
    // waits forever on a turn that already started (caught by the streamed-
    // events test hanging exactly this way).
    let mut rx = protocol.events(&worker);
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Err(e) = process_event(&store, &worker, event).await {
                        tracing::warn!(worker = worker.as_str(), error = %e,
                            "worker event processing failed");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    // Invariant 26: the gap must announce its SIZE. Nothing
                    // is reconstructed from the stream — after a gap the
                    // truth is the store, not inference (Invariant 34's
                    // gap-detection rule).
                    tracing::warn!(worker = worker.as_str(), missed,
                        "worker event channel lagged: {missed} events dropped; \
                         event-derived state has a hole — trust the store, not the stream");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// One supervision cycle: spawn processors for workers that have BOTH a live
/// `_amux_sessions` row and a live protocol session, reap finished ones, and
/// abort processors whose session ended outside the event path (e.g. startup
/// reconciliation marked it interrupted). Split from the loop so tests drive
/// cycles deterministically.
pub async fn supervise_once(
    store: &SharedStore,
    protocol: &Arc<dyn AgentProtocol>,
    procs: &mut BTreeMap<WorkerId, tokio::task::JoinHandle<()>>,
) -> anyhow::Result<()> {
    let live: BTreeSet<WorkerId> = {
        let conn = store.read()?;
        let mut stmt =
            conn.prepare("SELECT DISTINCT worker_id FROM _amux_sessions WHERE ended_at IS NULL")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.filter_map(|r| r.ok().and_then(|s| WorkerId::parse(&s).ok()))
            .collect()
    };
    // Reap processors that exited on their own (event channel closed).
    procs.retain(|_, h| !h.is_finished());
    // Abort processors for workers whose session ended by another path.
    procs.retain(|w, h| {
        if live.contains(w) {
            true
        } else {
            tracing::info!(worker = w.as_str(), "session ended; stopping event processor");
            h.abort();
            false
        }
    });
    #[allow(clippy::map_entry)] // an await sits between the check and the insert
    for w in live {
        if !procs.contains_key(&w) {
            // Gate on a live PROTOCOL session: subscribing to a worker the
            // protocol does not host yields a closed channel, and spawning
            // into it would just churn spawn->exit->reap every cycle.
            if protocol.state(&w).await.is_ok() {
                let h = spawn_event_processor(store.clone(), protocol.clone(), w.clone());
                procs.insert(w, h);
            }
        }
    }
    Ok(())
}

/// Supervision cadence. pub so lib.rs registers the interval this loop
/// actually sleeps with `runtime_jobs::registry`, not a copy of the number —
/// a displayed interval that disagrees with the sleep is how a healthy job
/// reads as stalled.
pub const SUPERVISE_SECS: u64 = 2;

/// The supervisor loop: watch for workers with live sessions, keep one
/// processor per worker. Runs forever; cycle errors are logged, never fatal.
pub async fn run_event_processors(store: SharedStore, protocol: Arc<dyn AgentProtocol>) {
    let mut procs: BTreeMap<WorkerId, tokio::task::JoinHandle<()>> = BTreeMap::new();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(SUPERVISE_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        crate::runtime_jobs::registry::tick(crate::runtime_jobs::registry::ids::EVENT_PROCESSORS);
        if let Err(e) = supervise_once(&store, &protocol, &mut procs).await {
            tracing::warn!(error = %e, "event-processor supervision cycle failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::queries::{SessionRow, WorkerRow};
    use crate::db::Store;
    use crate::opencode::mock::MockProtocol;
    use crate::opencode::AgentState;
    use amux_core::ids::{CommandId, TurnId};
    use amux_core::protocol::{
        DeliveryTiming, ExitStatus, Failure, ProgressReport, TurnResult,
    };
    use amux_core::provider::ProviderId;
    use amux_core::session::BackendId;
    use amux_core::worker::WorkerConfig;

    fn store() -> SharedStore {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let s = Arc::new(Store::open(&path).unwrap());
        std::mem::forget(dir); // keep the DB alive for the test body
        s
    }

    fn wid() -> WorkerId {
        WorkerId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 42))
    }

    fn trn(n: u128) -> TurnId {
        TurnId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 900 + n))
    }

    fn cfg() -> WorkerConfig {
        WorkerConfig {
            display_name: "w".into(),
            name_aliases: vec![],
            cwd: "/tmp/w".into(),
            provider: ProviderId::new("claude"),
            model: None,
            backend: BackendId::herdr(),
            environment: Default::default(),
            permissions: vec![],
            group: None,
        }
    }

    /// Seed a worker row + a live session row; returns the session id.
    fn seed(store: &SharedStore) -> String {
        let ses_id = format!("ses_{}", ulid::Ulid::new());
        let ses_ret = ses_id.clone();
        store
            .write(move |conn| {
                let row = WorkerRow::new(&wid(), &cfg(), "2026-08-09T00:00:00+00:00");
                queries::insert_worker(conn, &row)?;
                queries::insert_session(
                    conn,
                    &SessionRow {
                        id: ses_id.clone(),
                        worker_id: wid().as_str().into(),
                        backend: "herdr".into(),
                        backend_ref: format!("amux-{}", wid()),
                        pid: None,
                        started_at: "2026-08-09T00:00:00+00:00".into(),
                        ended_at: None,
                        exit_reason: None,
                    },
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        ses_ret
    }

    fn apply(store: &SharedStore, event: WorkerEvent) -> crate::db::WriteReply {
        store
            .write(move |conn| apply_event(conn, &wid(), &event, Utc::now()))
            .unwrap()
    }

    fn worker_state(store: &SharedStore) -> WorkerState {
        let conn = store.read().unwrap();
        queries::get_worker(&conn, wid().as_str()).unwrap().unwrap().state
    }

    fn enqueue_and_deliver(store: &SharedStore, cmd: WorkerCommand) -> CommandId {
        let id = CommandId::from_ulid(ulid::Ulid::new());
        let id_w = id.clone();
        store
            .write(move |conn| {
                commands::enqueue(
                    conn, id_w.clone(), &wid(), &cmd, id_w.as_str(),
                    &DeliveryTiming::AtTurnBoundary, None, Utc::now(),
                )?;
                commands::transition(conn, &id_w, CommandTransition::Dispatch, 3)?;
                commands::transition(conn, &id_w, CommandTransition::Deliver, 3)?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        id
    }

    async fn wait_until(mut pred: impl FnMut() -> bool) {
        for _ in 0..200 {
            if pred() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("condition not reached within 2s");
    }

    #[test]
    fn turn_lifecycle_rows_and_worker_state() {
        let store = store();
        let ses = seed(&store);

        // TurnStarted -> ledger row + Active{turn}.
        let reply = apply(&store, WorkerEvent::TurnStarted { turn_id: trn(1) });
        assert!(reply.applied);
        {
            let conn = store.read().unwrap();
            let (ses_id, ended): (Option<String>, Option<String>) = conn
                .query_row(
                    "SELECT session_id, ended_at FROM _amux_turns WHERE id = ?1",
                    params![trn(1).as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(ses_id.as_deref(), Some(ses.as_str()));
            assert_eq!(ended, None, "running turn has no end");
        }
        assert_eq!(
            worker_state(&store),
            WorkerState::Active { turn: Some(trn(1)) }
        );

        // Progress with tokens -> accumulated onto the OPEN turn.
        apply(
            &store,
            WorkerEvent::Progress(ProgressReport {
                summary: "working".into(),
                tokens_used: Some(1234),
            }),
        );

        // TurnCompleted -> row closed with outcome + tokens, worker Idle.
        apply(
            &store,
            WorkerEvent::TurnCompleted(TurnResult {
                turn_id: trn(1),
                outcome: "completed".into(),
            }),
        );
        {
            let conn = store.read().unwrap();
            let (ended, outcome, tokens): (Option<String>, Option<String>, String) = conn
                .query_row(
                    "SELECT ended_at, outcome, tokens FROM _amux_turns WHERE id = ?1",
                    params![trn(1).as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert!(ended.is_some());
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&outcome.unwrap()).unwrap()["outcome"],
                serde_json::json!("completed")
            );
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&tokens).unwrap()["reported_total"],
                serde_json::json!(1234)
            );
        }
        assert!(matches!(worker_state(&store), WorkerState::Idle { .. }));

        // A second TurnCompleted must not rewrite the record (end once).
        let reply = apply(
            &store,
            WorkerEvent::TurnCompleted(TurnResult {
                turn_id: trn(1),
                outcome: "rewritten".into(),
            }),
        );
        // The worker-state write still applies (idle again), but the ledger
        // row keeps its ORIGINAL outcome.
        let conn = store.read().unwrap();
        let outcome: String = conn
            .query_row(
                "SELECT outcome FROM _amux_turns WHERE id = ?1",
                params![trn(1).as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(outcome.contains("completed"), "{outcome}");
        drop(conn);
        let _ = reply;
    }

    #[test]
    fn turn_completed_confirms_the_delivered_command() {
        let store = store();
        seed(&store);
        let cmd_id = enqueue_and_deliver(&store, WorkerCommand::Continue);

        apply(&store, WorkerEvent::TurnStarted { turn_id: trn(2) });
        apply(
            &store,
            WorkerEvent::TurnCompleted(TurnResult { turn_id: trn(2), outcome: "done".into() }),
        );

        let conn = store.read().unwrap();
        let cmd = commands::by_id(&conn, &cmd_id).unwrap().unwrap();
        assert_eq!(cmd.state, CommandState::Confirmed, "Invariant 34: TurnCompleted confirms");
    }

    #[test]
    fn failed_writes_attempt_row_and_fails_command() {
        let store = store();
        seed(&store);
        let task = TaskId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 7));
        let cmd_id = enqueue_and_deliver(&store, WorkerCommand::ExecuteTask(task.clone()));

        apply(&store, WorkerEvent::TurnStarted { turn_id: trn(3) });
        apply(
            &store,
            WorkerEvent::Progress(ProgressReport { summary: "s".into(), tokens_used: Some(500) }),
        );
        apply(
            &store,
            WorkerEvent::Failed(Failure { reason: "api blew up".into(), retryable: true }),
        );

        let conn = store.read().unwrap();
        // Command failed with the failure's reason.
        let cmd = commands::by_id(&conn, &cmd_id).unwrap().unwrap();
        assert!(
            matches!(&cmd.state, CommandState::Failed { reason } if reason == "api blew up"),
            "{:?}",
            cmd.state
        );
        // Attempt row carries the Failure fields + known tokens.
        let (attempt, record_json): (u32, String) = conn
            .query_row(
                "SELECT attempt, record FROM _amux_attempts WHERE task_id = ?1 AND worker_id = ?2",
                params![task.as_str(), wid().as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(attempt, 1);
        let record: AttemptRecord = serde_json::from_str(&record_json).unwrap();
        assert_eq!(record.failure_reason, "api blew up");
        assert_eq!(record.tokens_spent, 500, "tokens from the open turn's report");
        drop(conn);
        // Worker landed in Error.
        assert!(matches!(worker_state(&store), WorkerState::Error { .. }));
    }

    #[test]
    fn exited_ends_the_live_session_row() {
        let store = store();
        let ses = seed(&store);

        apply(&store, WorkerEvent::Exited(ExitStatus { code: Some(0), signal: None }));

        let conn = store.read().unwrap();
        let (ended, reason): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT ended_at, exit_reason FROM _amux_sessions WHERE id = ?1",
                params![ses],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(ended.is_some(), "session ended");
        assert_eq!(
            serde_json::from_str::<ExitReason>(&reason.unwrap()).unwrap(),
            ExitReason::Completed,
            "code 0 = clean completion"
        );
        drop(conn);
        assert_eq!(worker_state(&store), WorkerState::Stopped);

        // A second Exited finds no live session, so the ORIGINAL exit
        // reason survives (end exactly once — the record is the record).
        apply(&store, WorkerEvent::Exited(ExitStatus { code: None, signal: Some(9) }));
        let conn = store.read().unwrap();
        let reason: String = conn
            .query_row(
                "SELECT exit_reason FROM _amux_sessions WHERE id = ?1",
                params![ses],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<ExitReason>(&reason).unwrap(),
            ExitReason::Completed
        );
    }

    /// Seed a board card whose last write is far in the past (long before
    /// any turn this test starts). Returns the semantic id.
    fn seed_issue(store: &SharedStore, session: &str) -> String {
        let session = session.to_string();
        let out: Arc<std::sync::Mutex<String>> = Arc::default();
        let out_w = out.clone();
        store
            .write(move |conn| {
                let row = crate::db::board_store::create_issue(
                    conn,
                    &crate::db::board_store::NewIssue {
                        title: "drift specimen".into(),
                        desc: String::new(),
                        status: "doing".into(),
                        session: Some(session.clone()),
                        item_type: "code".into(),
                        creator: "test".into(),
                        owner_type: "agent".into(),
                        due: None,
                        due_time: None,
                        reviewer: None,
                        shepherd: None,
                        gate: vec![],
                        depends_on: vec![],
                        tags: vec![],
                    },
                    1_700_000_000, // 2023: well before "now"
                )?;
                *out_w.lock().unwrap() = row.id;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let sem = out.lock().unwrap().clone();
        sem
    }

    fn issue_col(store: &SharedStore, sem: &str, col: &str) -> String {
        let conn = store.read().unwrap();
        conn.query_row(
            &format!("SELECT COALESCE(CAST({col} AS TEXT), '') FROM issues WHERE id = ?1"),
            params![sem],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// M4 (drift feedback, 2026-08-09 adherence audit): an ExecuteTask turn
    /// that completes WITHOUT the worker touching its card must leave a
    /// board-visible note — otherwise "worker finished, card untouched" is
    /// observable nowhere a reviewer looks (the board), only in `_amux_turns`
    /// (a store the reader never opens is the same failure as no tag, ethos
    /// rule 4). Pre-fix, TurnCompleted confirmed the command and wrote
    /// nothing to the board: this test then finds an empty card log and
    /// fails.
    #[test]
    fn execute_task_turn_with_untouched_card_writes_a_board_note() {
        let store = store();
        seed(&store);
        let sem = seed_issue(&store, "w");
        let task = crate::db::board_store::internal_id(&sem);
        let cmd_id = enqueue_and_deliver(&store, WorkerCommand::ExecuteTask(task));

        apply(&store, WorkerEvent::TurnStarted { turn_id: trn(20) });
        apply(
            &store,
            WorkerEvent::TurnCompleted(TurnResult {
                turn_id: trn(20),
                outcome: "completed".into(),
            }),
        );

        let conn = store.read().unwrap();
        let cmd = commands::by_id(&conn, &cmd_id).unwrap().unwrap();
        assert_eq!(cmd.state, CommandState::Confirmed, "confirmation still happens");
        drop(conn);
        let log = issue_col(&store, &sem, "log");
        assert!(
            log.contains("no board update"),
            "the card must carry the runtime's drift note: {log:?}"
        );
        let rev: String = issue_col(&store, &sem, "rev");
        assert_ne!(rev, "0", "the note is a real write pollers can see");
    }

    /// The negative cell: a worker that DID move the board during its turn
    /// gets no note — the check discriminates, it does not nag (ethos
    /// rule 7: an instrument that fires on every turn discriminates
    /// nothing).
    #[test]
    fn execute_task_turn_that_touched_the_card_writes_no_note() {
        let store = store();
        seed(&store);
        let sem = seed_issue(&store, "w");
        let task = crate::db::board_store::internal_id(&sem);
        enqueue_and_deliver(&store, WorkerCommand::ExecuteTask(task));

        apply(&store, WorkerEvent::TurnStarted { turn_id: trn(21) });
        // The worker's own board write, mid-turn (updated >= turn start).
        let sem_w = sem.clone();
        store
            .write(move |conn| {
                conn.execute(
                    "UPDATE issues SET updated = ?1 WHERE id = ?2",
                    params![Utc::now().timestamp() + 5, sem_w],
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        apply(
            &store,
            WorkerEvent::TurnCompleted(TurnResult {
                turn_id: trn(21),
                outcome: "completed".into(),
            }),
        );
        let log = issue_col(&store, &sem, "log");
        assert!(
            !log.contains("no board update"),
            "a touched card gets no drift note: {log:?}"
        );
    }

    #[tokio::test]
    async fn processor_task_applies_streamed_events() {
        let store = store();
        seed(&store);
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(wid(), AgentState::Working { turn: None, progress: None });

        let handle = spawn_event_processor(store.clone(), protocol.clone(), wid());

        protocol.emit(&wid(), WorkerEvent::TurnStarted { turn_id: trn(9) });
        {
            let s = store.clone();
            wait_until(move || {
                let conn = s.read().unwrap();
                conn.query_row(
                    "SELECT COUNT(*) FROM _amux_turns WHERE id = ?1",
                    params![trn(9).as_str()],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
                    > 0
            })
            .await;
        }
        protocol.emit(
            &wid(),
            WorkerEvent::TurnCompleted(TurnResult { turn_id: trn(9), outcome: "done".into() }),
        );
        {
            let s = store.clone();
            wait_until(move || {
                matches!(
                    {
                        let conn = s.read().unwrap();
                        queries::get_worker(&conn, wid().as_str()).unwrap().unwrap().state
                    },
                    WorkerState::Idle { .. }
                )
            })
            .await;
        }
        handle.abort();
    }

    #[tokio::test]
    async fn supervisor_spawns_for_live_sessions_and_reaps_ended_ones() {
        let store = store();
        let ses = seed(&store);
        let protocol: Arc<dyn AgentProtocol> = Arc::new({
            let m = MockProtocol::new();
            m.register(wid(), AgentState::Idle);
            m
        });
        let mut procs = BTreeMap::new();

        supervise_once(&store, &protocol, &mut procs).await.unwrap();
        assert_eq!(procs.len(), 1, "live session + live protocol -> processor");
        assert!(procs.contains_key(&wid()));

        // Session ends outside the event path -> next cycle reaps.
        store
            .write(move |conn| {
                queries::end_session(conn, &ses, &ExitReason::Killed, "2026-08-09T01:00:00+00:00")?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        supervise_once(&store, &protocol, &mut procs).await.unwrap();
        assert!(procs.is_empty(), "ended session -> processor reaped");
    }
}
