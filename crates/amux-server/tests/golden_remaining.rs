//! The four remaining golden scenarios (Phase 5):
//!
//! - RR-0080 `golden_rate_limit_recovery` — deterministic (MockProtocol, the
//!   Invariant 22 simulation seam): a rate limit lands through the REAL event
//!   processor, parks the worker, starves it of deliveries, and the RR-0072
//!   tick path recovers it when the reset passes.
//! - RR-0082 `golden_scoped_gates` — deterministic: the card/type tier of
//!   gate scoping (type-derived gate, per-card `gate` override, chore's
//!   honest lighter gate). Group/worker gate tiers are RR-0051's REMAINDER
//!   and are deliberately not asserted here — see
//!   `board_store::default_gates_for`'s own docs, which name the same edge.
//! - RR-0085 `golden_multi_provider_fleet` — LIVE (`#[ignore]`): three
//!   providers (real claude-code, real gemini, MockProtocol stand-in) drive
//!   three owned tasks with zero cross-worker delivery.
//! - RR-0086 `golden_backend_interchangeability` — LIVE (`#[ignore]`): the
//!   same lifecycle traced under TmuxBackend and HerdrBackend must be
//!   IDENTICAL above the backend boundary.
//!
//! Rigs and idioms are copied from golden_scenarios.rs (router + Runtime +
//! MockProtocol over one temp store) and golden_live.rs (live guards that
//! skip LOUDLY, wrk_-shaped throwaway refs, cleanup that cannot leak a
//! session into the live fleet's namespace). Run the deterministic pair:
//!
//!   CARGO_TARGET_DIR=/tmp/amux-remaining-target \
//!     cargo test -p amux-server --test golden_remaining
//!
//! and the live pair (real tokens + real tmux/herdr sessions), once, for
//! evidence:
//!
//!   CARGO_TARGET_DIR=/tmp/amux-remaining-target \
//!     cargo test -p amux-server --test golden_remaining -- --ignored --nocapture

use amux_core::board::TaskStatus;
use amux_core::circuit::{FleetCircuitBreaker, FleetState};
use amux_core::ids::{MessageId, TurnId, WorkerId};
use amux_core::protocol::{Failure, RateLimit, RateLimitKind, TurnResult, WorkerEvent};
use amux_core::provider::ProviderId;
use amux_core::revision::{EntityType, MutationKind, StateEvent};
use amux_core::session::BackendId;
use amux_core::worker::{WorkerConfig as CoreWorkerConfig, WorkerState};
use amux_server::api::{router, AppState};
use amux_server::backend::{
    backend_ref, herdr::HerdrBackend, tmux::TmuxBackend, BackendStatus, ProcessRef,
    SessionBackend, SessionSpec,
};
use amux_server::db::board_store;
use amux_server::db::queries::{self, SessionRow, WorkerRow};
use amux_server::db::{SharedStore, Store, WriteOutcome};
use amux_server::opencode::mock::{MockProtocol, RecordedCall};
use amux_server::opencode::structured::{
    CliProvider, StructuredCliProtocol, WorkerConfig as CliWorkerConfig,
};
use amux_server::opencode::{AgentProtocol, AgentState, Prompt, ProtocolError};
use amux_server::orchestrator::events as wevents;
use amux_server::orchestrator::runtime::Runtime;
use amux_server::orchestrator::scan::ScanLoop;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{DateTime, Utc};
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Rig: one store under router + runtime + mock agent (golden_scenarios.rs)
// ---------------------------------------------------------------------------

struct Rig {
    app: axum::Router,
    store: SharedStore,
    protocol: Arc<MockProtocol>,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    // These golden scenarios test the gate/lifecycle state machine, not the
    // global done-link gate (which has its own coverage), so pin it OFF.
    std::env::set_var("AMUX_DONE_LINK_REQUIRED", "0");
    std::env::set_var("AMUX_DONE_EVIDENCE_REQUIRED", "0");
    // Same reason, for the AF-317/AF-318 status gates: these rigs exercise the
    // lifecycle state machine, and each gate has its own coverage in board_api.
    std::env::set_var("AMUX_BLOCKED_NEEDS_WATCH", "0");
    std::env::set_var("AMUX_NEEDSYOU_ASK_REQUIRED", "0");
    std::env::set_var("AMUX_TODO_WIP_LIMIT", "0");
    let dir = tempfile::tempdir().unwrap();
    let store: SharedStore = Arc::new(Store::open(&dir.path().join("golden.db")).unwrap());
    let state = AppState {
        secrets: std::sync::Arc::new(amux_server::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "golden-remaining-test".into(),
        auth_token: None,
    reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    Rig {
        app: router(state),
        store,
        protocol: Arc::new(MockProtocol::new()),
        _dir: dir,
    }
}

/// The runtime under test, breaker permissive so no scenario can trip it.
fn mock_runtime(rig: &Rig) -> Runtime {
    Runtime {
        store: rig.store.clone(),
        backends: vec![],
        tick_secs: 1,
        heartbeat_every: 1000,
        breaker: FleetCircuitBreaker {
            window_budget_tokens: u64::MAX,
            window_secs: 3600,
            min_progress_per_window: 0,
            max_failures_per_window: 1000,
        },
        fleet_state: Mutex::new(FleetState::Normal),
        protocol: Some(rig.protocol.clone()),
        pickup_unowned: false,
        resume_stagger_secs: 5,
    }
}

/// Runtime over an arbitrary protocol/backends (golden_live.rs shape).
fn runtime_with(
    store: SharedStore,
    protocol: Option<Arc<dyn AgentProtocol>>,
    backends: Vec<Arc<dyn SessionBackend>>,
) -> Runtime {
    Runtime {
        store,
        backends,
        tick_secs: 1,
        heartbeat_every: 1000,
        breaker: FleetCircuitBreaker {
            window_budget_tokens: u64::MAX,
            window_secs: 3600,
            min_progress_per_window: 0,
            max_failures_per_window: 1000,
        },
        fleet_state: Mutex::new(FleetState::Normal),
        protocol,
        pickup_unowned: false,
        resume_stagger_secs: 5,
    }
}

// ---------------------------------------------------------------------------
// HTTP plumbing (board_api.rs idioms via golden_scenarios.rs)
// ---------------------------------------------------------------------------

async fn send_with(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = match body {
        Some(v) => b
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, v)
}

/// POST /api/workers + protocol registration (name-resolution path: board
/// ownership resolves by display name, not by id).
async fn register_worker(app: &axum::Router, protocol: &MockProtocol, name: &str) -> WorkerId {
    let (st, v) = send_with(
        app,
        "POST",
        "/api/workers",
        Some(json!({ "display_name": name, "cwd": "/tmp" })),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "worker create failed: {v}");
    let wid = WorkerId::parse(v["id"].as_str().unwrap()).unwrap();
    protocol.register(wid.clone(), AgentState::Idle);
    wid
}

async fn create_task_with(app: &axum::Router, body: Value) -> String {
    let (st, v) = send_with(app, "POST", "/api/board", Some(body), &[]).await;
    assert_eq!(st, StatusCode::CREATED, "task create failed: {v}");
    v["id"].as_str().unwrap().to_string()
}

async fn detail(app: &axum::Router, sem: &str) -> Value {
    let (st, v) = send_with(app, "GET", &format!("/api/board/{sem}"), None, &[]).await;
    assert_eq!(st, StatusCode::OK, "detail {sem}: {v}");
    v
}

/// PATCH that returns whatever came back — for asserting 409 bodies.
async fn patch_raw(app: &axum::Router, sem: &str, body: Value, actor: &str) -> (StatusCode, Value) {
    send_with(
        app,
        "PATCH",
        &format!("/api/board/{sem}"),
        Some(body),
        &[("X-Amux-Session", actor)],
    )
    .await
}

async fn patch_ok(app: &axum::Router, sem: &str, body: Value, actor: &str) -> Value {
    let (st, v) = patch_raw(app, sem, body, actor).await;
    assert_eq!(st, StatusCode::OK, "PATCH {sem} refused: {v}");
    v
}

/// (command JSON, state JSON) rows for a worker, FIFO.
fn command_rows(store: &SharedStore, worker: &WorkerId) -> Vec<(String, String)> {
    let conn = store.read().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT command, state FROM _amux_commands WHERE worker_id = ?1
             ORDER BY queued_at ASC, id ASC",
        )
        .unwrap();
    stmt.query_map(params![worker.as_str()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn confirmed_count(store: &SharedStore, worker: &WorkerId) -> i64 {
    let conn = store.read().unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM _amux_commands WHERE worker_id = ?1 AND state LIKE '%confirmed%'",
        params![worker.as_str()],
        |r| r.get(0),
    )
    .unwrap()
}

fn failed_count(store: &SharedStore, worker: &WorkerId) -> i64 {
    let conn = store.read().unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM _amux_commands
         WHERE worker_id = ?1 AND (state LIKE '%failed%' OR state LIKE '%dead_lettered%')",
        params![worker.as_str()],
        |r| r.get(0),
    )
    .unwrap()
}

fn worker_durable_state(store: &SharedStore, worker: &WorkerId) -> WorkerState {
    let conn = store.read().unwrap();
    queries::get_worker(&conn, worker.as_str()).unwrap().unwrap().state
}

/// Live (unexpired) leases as (task_id, worker_id).
fn live_leases(store: &SharedStore) -> Vec<(String, String)> {
    let conn = store.read().unwrap();
    let now = Utc::now();
    let mut stmt = conn
        .prepare("SELECT task_id, worker_id, expires_at FROM _amux_leases")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    rows.into_iter()
        .filter_map(|(t, w, exp)| {
            let exp: DateTime<Utc> = exp.parse().ok()?;
            (exp > now).then_some((t, w))
        })
        .collect()
}

/// Time-warp a lease to expired so the next tick's REAL reclaim path
/// (plan.reclaim -> DELETE + StateEvent) releases it — the golden_scenarios
/// idiom that compresses an expiry a test cannot wait out; the deletion
/// itself still runs the shipped code.
fn expire_lease(store: &SharedStore, task_id: &str) {
    let t = task_id.to_string();
    store
        .write(move |conn| {
            conn.execute(
                "UPDATE _amux_leases SET expires_at = ?2 WHERE task_id = ?1",
                params![t, (Utc::now() - chrono::Duration::hours(1)).to_rfc3339()],
            )?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .unwrap();
}

fn drain(rx: &mut tokio::sync::broadcast::Receiver<StateEvent>) -> Vec<StateEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

/// The worker's StatusChanged `to`-tags, in journal order.
fn worker_status_seq(events: &[StateEvent], wid: &WorkerId) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.entity_type == EntityType::Worker && e.entity_id == wid.as_str())
        .filter_map(|e| match &e.mutation {
            MutationKind::StatusChanged { to, .. } => Some(to.clone()),
            _ => None,
        })
        .collect()
}

/// Lease-entity events for one task, reduced to readable labels, in order.
fn lease_seq(events: &[StateEvent], task_id: &str) -> Vec<String> {
    events
        .iter()
        .filter(|e| {
            e.entity_type == EntityType::Other("lease".into()) && e.entity_id == task_id
        })
        .map(|e| match &e.mutation {
            MutationKind::Created => "created".to_string(),
            MutationKind::StatusChanged { to, .. } => to.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

fn sendprompt_texts(protocol: &MockProtocol) -> Vec<String> {
    protocol
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            RecordedCall::SendPrompt { prompt, .. } => Some(prompt.text),
            _ => None,
        })
        .collect()
}

async fn wait_until(mut pred: impl FnMut() -> bool) {
    for _ in 0..200 {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached within 2s");
}

// ===========================================================================
// RR-0080 — Golden scenario: rate-limit recovery (deterministic)
// ===========================================================================

#[tokio::test]
async fn golden_rate_limit_recovery() {
    let rig = rig();
    let app = &rig.app;
    // Subscribe BEFORE anything mutates so the whole journal is one stream.
    let mut rx = rig.store.subscribe();
    let mut journal: Vec<StateEvent> = Vec::new();

    let wid = register_worker(app, &rig.protocol, "limited-worker").await;
    let sem_a =
        create_task_with(app, json!({ "title": "task cut short by the weekly limit", "session": "limited-worker" })).await;
    let tid_a = board_store::internal_id(&sem_a);
    let rt = mock_runtime(&rig);

    // Tick 1: the owned todo task is planned -> lease + ExecuteTask command.
    rt.tick_once(false).await.unwrap();
    assert_eq!(
        live_leases(&rig.store),
        vec![(tid_a.to_string(), wid.as_str().to_string())],
        "tick must lease the owned task to its worker"
    );
    // Tick 2: the pump delivers through MockProtocol (WhenIdle, agent Idle).
    rt.tick_once(false).await.unwrap();
    let texts = sendprompt_texts(&rig.protocol);
    assert_eq!(texts.len(), 1, "one delivery before the limit: {texts:?}");
    assert!(texts[0].contains(tid_a.as_str()), "{}", texts[0]);

    // The agent goes MID-TASK through the real event processor.
    let processor =
        wevents::spawn_event_processor(rig.store.clone(), rig.protocol.clone(), wid.clone());
    let turn_a = TurnId::from_ulid(ulid::Ulid::new());
    rig.protocol.set_state(
        &wid,
        AgentState::Working { turn: Some(turn_a.clone()), progress: None },
        Some(WorkerEvent::TurnStarted { turn_id: turn_a.clone() }),
    );
    {
        let (store, wid) = (rig.store.clone(), wid.clone());
        wait_until(move || {
            matches!(worker_durable_state(&store, &wid), WorkerState::Active { .. })
        })
        .await;
    }

    // Mid-turn, the provider announces its WEEKLY cap with a reset 2s out.
    // The turn dies with the limit: Failed resolves the in-flight command
    // (a queue wedged on a turn that will never complete is the alternative),
    // and the RateLimited state lands LAST so it is what the fleet sees.
    let reset = Utc::now() + chrono::Duration::seconds(2);
    let rl = RateLimit {
        kind: RateLimitKind::Weekly,
        reset_at: Some(reset),
        provider: ProviderId::new("claude-code"),
        raw: Some("You've reached your weekly limit".into()),
    };
    rig.protocol.emit(
        &wid,
        WorkerEvent::Failed(Failure {
            reason: "weekly rate limit hit mid-turn".into(),
            retryable: true,
        }),
    );
    rig.protocol.set_state(
        &wid,
        AgentState::RateLimited(rl.clone()),
        Some(WorkerEvent::RateLimited(rl)),
    );
    {
        let (store, wid) = (rig.store.clone(), wid.clone());
        wait_until(move || {
            matches!(worker_durable_state(&store, &wid), WorkerState::RateLimited { .. })
        })
        .await;
    }
    // The DB state carries the exact reset instant the event reported
    // (Invariant 20: the recovery clock is the provider's, never invented).
    match worker_durable_state(&rig.store, &wid) {
        WorkerState::RateLimited { reset_at } => assert_eq!(reset_at, Some(reset)),
        other => panic!("expected RateLimited, got {other:?}"),
    }
    journal.extend(drain(&mut rx));
    // The dashboard's 2s visibility rides the StateEvent push — assert the
    // EVENT, not a browser (Invariant 35 delivery, not rendering).
    assert!(
        journal.iter().any(|e| e.entity_type == EntityType::Worker
            && e.entity_id == wid.as_str()
            && matches!(&e.mutation,
                MutationKind::StatusChanged { to, .. } if to == "rate_limited")),
        "rate_limited StateEvent missing from the journal: {journal:?}"
    );
    // The in-flight ExecuteTask failed (not confirmed, not stuck delivered).
    assert_eq!(failed_count(&rig.store, &wid), 1);
    assert_eq!(confirmed_count(&rig.store, &wid), 0);

    // Harness bookkeeping a real lane would do: the cut task is parked
    // blocked with the reason, and the (600s) lease is time-warped so the
    // next tick's REAL reclaim frees the WIP slot inside test time.
    patch_ok(
        app,
        &sem_a,
        json!({ "status": "blocked", "reason": "provider weekly limit; parked until reset" }),
        "limited-worker",
    )
    .await;
    expire_lease(&rig.store, tid_a.as_str());

    // Fresh work arrives WHILE the limit holds.
    let sem_b =
        create_task_with(app, json!({ "title": "task delivered after recovery", "session": "limited-worker" })).await;
    let tid_b = board_store::internal_id(&sem_b);

    // Guard: if the 2s window already lapsed, the next tick would measure
    // recovery, not the limit — fail naming the real cause (ethos rule 4).
    assert!(
        Utc::now() < reset,
        "test machine too slow: reset passed before the during-limit tick could run"
    );
    // Tick DURING the limit. The reclaim frees the worker's WIP slot within
    // this same plan, so the ONLY thing between task B and a lease is the
    // rate_limited state (worker_available excludes it) — and the pump has
    // nothing deliverable. No new leases, no new deliveries, no recovery.
    rt.tick_once(false).await.unwrap();
    journal.extend(drain(&mut rx));
    assert!(
        live_leases(&rig.store).is_empty(),
        "rate-limited worker must not be leased new work"
    );
    assert_eq!(
        sendprompt_texts(&rig.protocol).len(),
        1,
        "NO new deliveries to a rate-limited worker"
    );
    assert_eq!(command_rows(&rig.store, &wid).len(), 1, "no new command enqueued");
    assert!(
        matches!(worker_durable_state(&rig.store, &wid), WorkerState::RateLimited { .. }),
        "recovery must not fire before reset_at"
    );

    // Wait out the provider's window, then a tick runs the RR-0072 path:
    // the worker auto-recovers to Idle — no human, no event, just the clock
    // the provider itself supplied.
    while Utc::now() < reset {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    rt.tick_once(false).await.unwrap();
    journal.extend(drain(&mut rx));
    assert!(
        matches!(worker_durable_state(&rig.store, &wid), WorkerState::Idle { .. }),
        "RR-0072: worker must auto-recover once the reset passes"
    );
    let recoveries = journal
        .iter()
        .filter(|e| {
            e.entity_type == EntityType::Worker
                && e.entity_id == wid.as_str()
                && matches!(&e.mutation,
                    MutationKind::StatusChanged { from, to } if from == "rate_limited" && to == "idle")
        })
        .count();
    assert_eq!(recoveries, 1, "exactly one recovery edge in the journal");

    // Agent-side seam: the DURABLE recovery above is the runtime's own; the
    // protocol state is the scripted mock, standing in for a fresh provider
    // session that accepts prompts again (Invariant 22).
    rig.protocol.set_state(&wid, AgentState::Idle, None);

    // Recovery tick planned with the pre-recovery worker snapshot, so the
    // lease lands next tick, and the pump delivers it the tick after —
    // delivery RESUMES on the same loop that starved it.
    rt.tick_once(false).await.unwrap();
    assert_eq!(
        live_leases(&rig.store),
        vec![(tid_b.to_string(), wid.as_str().to_string())],
        "recovered worker leases the queued task (blocked A is Waiting, not runnable)"
    );
    rt.tick_once(false).await.unwrap();
    let texts = sendprompt_texts(&rig.protocol);
    assert_eq!(texts.len(), 2, "pump resumed delivery after recovery: {texts:?}");
    assert!(texts[1].contains(tid_b.as_str()), "{}", texts[1]);
    assert!(texts[1].contains("task delivered after recovery"), "{}", texts[1]);

    // The resumed turn completes through the real path; command B confirms.
    let turn_b = TurnId::from_ulid(ulid::Ulid::new());
    rig.protocol.set_state(
        &wid,
        AgentState::Working { turn: Some(turn_b.clone()), progress: None },
        Some(WorkerEvent::TurnStarted { turn_id: turn_b.clone() }),
    );
    rig.protocol.set_state(
        &wid,
        AgentState::Idle,
        Some(WorkerEvent::TurnCompleted(TurnResult {
            turn_id: turn_b,
            outcome: "completed".into(),
        })),
    );
    {
        let (store, wid) = (rig.store.clone(), wid.clone());
        wait_until(move || {
            confirmed_count(&store, &wid) == 1
                && matches!(worker_durable_state(&store, &wid), WorkerState::Idle { .. })
        })
        .await;
    }
    processor.abort();
    journal.extend(drain(&mut rx));

    // The FULL journal sequence. Worker state arc, in order: mid-task ->
    // turn killed -> parked -> recovered -> resumed turn -> idle.
    assert_eq!(
        worker_status_seq(&journal, &wid),
        vec!["active", "error", "rate_limited", "idle", "active", "idle"],
        "worker journal arc"
    );
    // Lease arc: A leased then reclaimed (expired during the limit); B
    // leased after recovery and still live.
    assert_eq!(lease_seq(&journal, tid_a.as_str()), vec!["created", "reclaimed"]);
    assert_eq!(lease_seq(&journal, tid_b.as_str()), vec!["created"]);
    // Command arc: A's ExecuteTask failed with the limit; B's confirmed.
    assert!(
        journal.iter().any(|e| e.entity_type == EntityType::Other("command".into())
            && matches!(&e.mutation, MutationKind::StatusChanged { to, .. } if to == "failed")),
        "command-failed event missing"
    );
    assert!(
        journal.iter().any(|e| e.entity_type == EntityType::Other("command".into())
            && matches!(&e.mutation, MutationKind::StatusChanged { to, .. } if to == "confirmed")),
        "command-confirmed event missing"
    );
    // Board arc: A was parked blocked with the reason; B stayed todo (its
    // turn completed but nothing moved the card — that is the worker's job).
    assert_eq!(detail(app, &sem_a).await["status"], json!("blocked"));
    assert_eq!(detail(app, &sem_b).await["status"], json!("todo"));
}

// ===========================================================================
// RR-0082 — Golden scenario: scoped gates (card/type tier)
// ===========================================================================

/// SCOPE NOTE: this exercises the card tier (per-card `gate` override) and
/// the type tier (type-derived defaults) — the two rungs that exist today.
/// The group/worker gate tiers are RR-0051's remainder; when they land,
/// `board_store::effective_gate` grows rungs and this test grows cases.
#[tokio::test]
async fn golden_scoped_gates() {
    let rig = rig();
    let app = &rig.app;
    let actor = "gate-runner";

    // The REAL gate tables, read from the code under test — assertions below
    // compare 409 bodies against THESE, not against re-typed assumptions.
    let code_done = board_store::default_gates_for("code", TaskStatus::Done);
    assert_eq!(
        code_done,
        vec!["Implemented and merged".to_string(), "Tests / lint pass".to_string()],
        "the fleet acks these exact strings — a drift here breaks every ack"
    );
    let chore_done = board_store::default_gates_for("chore", TaskStatus::Done);
    assert!(!chore_done.is_empty(), "chore's done gate must exist (an ungated done is a lie)");
    assert_ne!(
        chore_done, code_done,
        "chore's gate must be its own, not code's — that difference IS type scoping"
    );

    // ---- Card 1: typed `code` (the default type) ---------------------------
    let code_sem =
        create_task_with(app, json!({ "title": "code-typed card", "session": actor, "desc": "artifact: crates/amux-server/src/api/board.rs" })).await;
    // Even doing is gated for code — no ack, no move.
    let (st, v) = patch_raw(app, &code_sem, json!({ "status": "doing" }), actor).await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], json!("gate not acknowledged"), "{v}");
    let v = patch_ok(app, &code_sem, json!({ "status": "doing", "gate_ack": true }), actor).await;
    assert_eq!(v["status"], json!("doing"), "{v}");
    // done WITHOUT gate_checked: 409 naming the exact type-derived strings.
    let (st, v) = patch_raw(app, &code_sem, json!({ "status": "done" }), actor).await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], json!("gate not acknowledged"), "{v}");
    assert_eq!(v["gate"], json!(code_done), "the 409 must name the effective gate: {v}");
    assert_eq!(v["attempted_status"], json!("done"), "{v}");
    assert_eq!(v["item_type"], json!("code"), "{v}");
    // A PARTIAL ack is not an ack: matching is by exact string, per
    // criterion, and the refusal names what is missing (AMUX-1719).
    let (st, v) = patch_raw(
        app,
        &code_sem,
        json!({ "status": "done", "gate_checked": [code_done[0]] }),
        actor,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], json!("gate_checked does not match the gate"), "{v}");
    assert_eq!(v["missing"], json!([code_done[1]]), "{v}");
    // The exact strings pass.
    let v = patch_ok(
        app,
        &code_sem,
        json!({ "status": "done", "gate_checked": code_done }),
        actor,
    )
    .await;
    assert_eq!(v["status"], json!("done"), "{v}");

    // ---- Card 2: per-card `gate` override, set via PATCH -------------------
    let over_sem =
        create_task_with(app, json!({ "title": "card with its own gate", "session": actor, "desc": "artifact: crates/amux-server/src/api/board.rs" })).await;
    let custom = vec![
        "Golden custom criterion: staging soak green for 24h".to_string(),
        "Golden custom criterion: runbook updated".to_string(),
    ];
    let v = patch_ok(app, &over_sem, json!({ "gate": custom }), actor).await;
    assert_eq!(v["gate"], json!(custom), "override stored on the card: {v}");
    // The override guards every gated target for THIS card (card rung
    // outranks the type rung in the precedence chain).
    let v = patch_ok(app, &over_sem, json!({ "status": "doing", "gate_ack": true }), actor).await;
    assert_eq!(v["status"], json!("doing"), "{v}");
    let (st, v) = patch_raw(app, &over_sem, json!({ "status": "done" }), actor).await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], json!("gate not acknowledged"), "{v}");
    assert_eq!(v["gate"], json!(custom), "the 409 names ITS criteria, not the type's: {v}");
    assert!(
        !v["gate"].as_array().unwrap().iter().any(|c| c == &json!(code_done[0])),
        "the override REPLACED the type gate, it did not extend it: {v}"
    );
    // Acking the TYPE gate's strings is not acking THIS card's gate.
    let (st, v) = patch_raw(
        app,
        &over_sem,
        json!({ "status": "done", "gate_checked": code_done }),
        actor,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["missing"], json!(custom), "{v}");
    let v = patch_ok(
        app,
        &over_sem,
        json!({ "status": "done", "gate_checked": custom }),
        actor,
    )
    .await;
    assert_eq!(v["status"], json!("done"), "{v}");

    // ---- Card 3: typed `chore` — the honest lighter gate -------------------
    let chore_sem = create_task_with(
        app,
        json!({ "title": "chore-typed card", "session": actor, "type": "chore", "desc": "artifact: crates/amux-server/src/api/board.rs" }),
    )
    .await;
    let v = patch_ok(app, &chore_sem, json!({ "status": "doing", "gate_ack": true }), actor).await;
    assert_eq!(v["status"], json!("doing"), "{v}");
    let (st, v) = patch_raw(app, &chore_sem, json!({ "status": "done" }), actor).await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(
        v["gate"],
        json!(chore_done),
        "chore's 409 names chore's own gate (from default_gates_for, not an assumption): {v}"
    );
    assert_eq!(v["item_type"], json!("chore"), "{v}");
    // The honest lighter gate passes without asserting a merge that never
    // happened (ethos rule 3: fix the type, not the truth).
    let v = patch_ok(
        app,
        &chore_sem,
        json!({ "status": "done", "gate_checked": chore_done }),
        actor,
    )
    .await;
    assert_eq!(v["status"], json!("done"), "{v}");
}

// ===========================================================================
// Live plumbing (golden_live.rs idioms): loud guards, throwaway refs
// ===========================================================================

fn have_bin(bin: &str) -> bool {
    std::process::Command::new("which")
        .arg(bin)
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn binary_available(bin: &str, probe_arg: &str) -> bool {
    tokio::process::Command::new(bin)
        .arg(probe_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// herdr's CLI is a socket-API client: it needs a RUNNING herdr server
/// session to target (backend_conformance.rs idiom).
async fn running_herdr_session() -> Option<String> {
    let out = tokio::process::Command::new("herdr")
        .args(["session", "list", "--json"])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let running: Vec<String> = v["sessions"]
        .as_array()?
        .iter()
        .filter(|s| s["running"].as_bool() == Some(true))
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    running
        .iter()
        .find(|n| n.as_str() == "amux")
        .or_else(|| running.first())
        .cloned()
}

// ===========================================================================
// RR-0085 — Golden scenario: multi-provider fleet (LIVE)
// ===========================================================================

/// Test-local router over per-worker protocols. The Runtime takes ONE
/// `AgentProtocol`, and this fleet spans two transports (StructuredCliProtocol
/// for the real CLIs, MockProtocol for the stand-in), so the seam is a
/// dispatch table keyed by worker — no protocol logic of its own. Every
/// SendPrompt is recorded BEFORE it is forwarded so the no-cross-delivery
/// assertion reads the actual delivery log, not an inference from side
/// effects (ethos rule 4: the discriminator lives in the data we keep).
struct FleetProtocol {
    routes: BTreeMap<WorkerId, Arc<dyn AgentProtocol>>,
    prompts: Mutex<Vec<(WorkerId, String)>>,
}

impl FleetProtocol {
    fn route(&self, worker: &WorkerId) -> amux_server::opencode::Result<&Arc<dyn AgentProtocol>> {
        self.routes
            .get(worker)
            .ok_or_else(|| ProtocolError::NoSession(worker.to_string()))
    }

    fn recorded_prompts(&self) -> Vec<(WorkerId, String)> {
        self.prompts.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl AgentProtocol for FleetProtocol {
    async fn send_prompt(&self, worker: &WorkerId, prompt: Prompt) -> amux_server::opencode::Result<()> {
        let p = self.route(worker)?;
        self.prompts
            .lock()
            .unwrap()
            .push((worker.clone(), prompt.text.clone()));
        p.send_prompt(worker, prompt).await
    }
    async fn deliver_message(
        &self,
        worker: &WorkerId,
        msg: MessageId,
        body: String,
    ) -> amux_server::opencode::Result<()> {
        self.route(worker)?.deliver_message(worker, msg, body).await
    }
    async fn cancel(&self, worker: &WorkerId) -> amux_server::opencode::Result<()> {
        self.route(worker)?.cancel(worker).await
    }
    async fn pause(&self, worker: &WorkerId) -> amux_server::opencode::Result<()> {
        self.route(worker)?.pause(worker).await
    }
    async fn resume(&self, worker: &WorkerId) -> amux_server::opencode::Result<()> {
        self.route(worker)?.resume(worker).await
    }
    async fn state(&self, worker: &WorkerId) -> amux_server::opencode::Result<AgentState> {
        self.route(worker)?.state(worker).await
    }
    fn events(&self, worker: &WorkerId) -> tokio::sync::broadcast::Receiver<WorkerEvent> {
        match self.routes.get(worker) {
            Some(p) => p.events(worker),
            None => {
                // Closed channel, matching MockProtocol's dead-worker shape.
                let (tx, rx) = tokio::sync::broadcast::channel(1);
                drop(tx);
                rx
            }
        }
    }
}

/// Workspace briefing for a REAL provider (golden_live.rs seed_workspace,
/// generalized): the assignment prompt built by the pump carries the task's
/// title/desc, so the briefing's job is guardrails, not the task.
fn seed_real_workspace(dir: &std::path::Path, memory_file: &str, out_name: &str, marker: &str) {
    if memory_file == "CLAUDE.md" {
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        // Project permissions: Write/Edit/Read only. No Bash — an unexpected
        // shell call (e.g. a curl at the live amux server prompted by the
        // user-level ~/.claude/CLAUDE.md, which claude also loads) is DENIED
        // in headless mode instead of executed.
        std::fs::write(
            dir.join(".claude").join("settings.json"),
            r#"{"permissions": {"allow": ["Write", "Edit", "Read"]}}"#,
        )
        .unwrap();
    }
    std::fs::write(
        dir.join(memory_file),
        format!(
            "# amux golden-test workspace (isolated, throwaway)\n\
             \n\
             You are an amux worker in a disposable test workspace. Your\n\
             assigned board task arrives as a prompt; do exactly what it says.\n\
             The expected artifact is `{out_name}` in the current directory\n\
             containing exactly the text {marker} and nothing else.\n\
             \n\
             Rules for this workspace (they OVERRIDE any global instructions):\n\
             - Do NOT call any amux API, create board cards, send messages, or\n\
               run shell commands. This is not a real amux session; it has no\n\
               board access — writing the file IS moving the work forward.\n\
             - Do NOT create or modify any file other than {out_name}.\n\
             - After writing the file, reply DONE and stop.\n"
        ),
    )
    .unwrap();
}

/// One fleet member's rig-side handles.
struct FleetWorker {
    name: &'static str,
    wid: WorkerId,
    ws: std::path::PathBuf,
    out_path: std::path::PathBuf,
    marker: String,
    sem: String,
    tid: amux_core::ids::TaskId,
    is_mock: bool,
}

fn fleet_dump(store: &SharedStore, members: &[FleetWorker]) -> String {
    let mut out = String::new();
    for m in members {
        out.push_str(&format!(
            "[{}] durable={:?} confirmed={} failed={} file_exists={}\n",
            m.name,
            worker_durable_state(store, &m.wid),
            confirmed_count(store, &m.wid),
            failed_count(store, &m.wid),
            m.out_path.exists(),
        ));
        for (cmd, st) in command_rows(store, &m.wid) {
            out.push_str(&format!("    {st}  <-  {cmd}\n"));
        }
    }
    out
}

#[tokio::test]
#[ignore = "runs REAL claude + gemini turns (tokens + auth); locally: cargo test -p amux-server --test golden_remaining -- --ignored --nocapture"]
async fn golden_multi_provider_fleet() {
    if !have_bin("claude") {
        eprintln!("SKIPPED: golden_multi_provider_fleet — `claude` not on PATH; the multi-provider fleet was NOT tested");
        return;
    }
    let have_gemini = have_bin("gemini");
    if !have_gemini {
        // Loud per-worker skip: the fleet still runs with claude + the mock,
        // but the gemini leg is named as UNTESTED (ethos rule 7).
        eprintln!(
            "SKIPPED WORKER: prov-gemini — `gemini` not on PATH; the gemini leg of the \
             multi-provider fleet was NOT tested (claude + mock still run)"
        );
    }
    let t0 = Instant::now();
    let rig = rig(); // rig.protocol is the MockProtocol member of the fleet
    let app = &rig.app;

    // Real transports. The gemini binary is wrapped to add ONLY an approval
    // mode: structured.rs's fixed gemini argv carries none, and a headless
    // write_file then waits forever for an approval that cannot arrive
    // (verified live 2026-08-09: default mode hung >90s on this exact task
    // shape; `--approval-mode auto_edit` wrote the file in ~10s). The
    // `binary` override is the sanctioned test seam; the stream, the model,
    // and the file write are the real gemini. The real fix belongs in
    // CliProvider::args, not here — this comment is the tripwire.
    let structured = Arc::new(StructuredCliProtocol::new());
    let shim_dir = tempfile::tempdir().unwrap();
    let gemini_shim = shim_dir.path().join("gemini-auto-edit.sh");
    std::fs::write(
        &gemini_shim,
        "#!/bin/sh\nexec gemini --approval-mode auto_edit \"$@\"\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gemini_shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Assemble the fleet: (name, provider config). `None` = MockProtocol;
    // Some((cli, binary override, memory file)) = a real structured CLI.
    type RealCli = (CliProvider, Option<std::path::PathBuf>, &'static str);
    let mut specs: Vec<(&'static str, Option<RealCli>)> = vec![
        ("prov-claude", Some((CliProvider::ClaudeCode, None, "CLAUDE.md"))),
    ];
    if have_gemini {
        specs.push((
            "prov-gemini",
            Some((CliProvider::GeminiCli, Some(gemini_shim.clone()), "GEMINI.md")),
        ));
    }
    specs.push(("prov-mock", None)); // MockProtocol stands in for a third provider

    let mut members: Vec<FleetWorker> = Vec::new();
    let mut routes: BTreeMap<WorkerId, Arc<dyn AgentProtocol>> = BTreeMap::new();
    let mut workspaces: Vec<tempfile::TempDir> = Vec::new();
    for (name, provider) in &specs {
        let ws = tempfile::tempdir().unwrap();
        let short = name.strip_prefix("prov-").unwrap();
        let out_name = format!("out-{short}.txt");
        let marker = format!("PROVIDER-{short}");
        let (st, v) = send_with(
            app,
            "POST",
            "/api/workers",
            Some(json!({
                "display_name": name,
                "cwd": ws.path().to_string_lossy(),
                "provider": match provider {
                    Some((CliProvider::ClaudeCode, _, _)) => "claude-code",
                    Some((CliProvider::GeminiCli, _, _)) => "gemini-cli",
                    _ => "mock-provider",
                },
            })),
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "worker create failed: {v}");
        let wid = WorkerId::parse(v["id"].as_str().unwrap()).unwrap();

        match provider {
            Some((cli, binary, memory_file)) => {
                seed_real_workspace(ws.path(), memory_file, &out_name, &marker);
                structured.register(
                    wid.clone(),
                    CliWorkerConfig {
                        provider: *cli,
                        cwd: ws.path().to_path_buf(),
                        binary: binary.clone(),
                        model: None, // structured::WorkerConfig grew `model` mid-flight (other lane); None = CLI default
            conversation: None, // and `conversation` with AMUX-2613; None = fresh
                    },
                );
                routes.insert(wid.clone(), structured.clone());
            }
            None => {
                rig.protocol.register(wid.clone(), AgentState::Idle);
                routes.insert(wid.clone(), rig.protocol.clone());
            }
        }

        // A live session row so turn-ledger rows carry a session id
        // (golden_live idiom — the processor warns on NULL otherwise).
        {
            let row = SessionRow {
                id: format!("ses_{}", ulid::Ulid::new()),
                worker_id: wid.as_str().to_string(),
                backend: "structured".into(),
                backend_ref: backend_ref(&wid),
                pid: None,
                started_at: Utc::now().to_rfc3339(),
                ended_at: None,
                exit_reason: None,
            };
            rig.store
                .write(move |conn| {
                    queries::insert_session(conn, &row)?;
                    Ok(WriteOutcome { applied: true, events: vec![] })
                })
                .unwrap();
        }

        // The owned board task: title AND desc carry the work — the pump's
        // assignment prompt is built from this row (feed-forward, Inv 49).
        let out_path = ws.path().join(&out_name);
        let sem = create_task_with(
            app,
            json!({
                "title": format!("write {marker} into {out_name}"),
                "desc": format!(
                    "Use your file-write tool to create {} whose entire content is exactly:\n{marker}\nThen reply DONE and stop. Do not touch any other file, and do not create board cards or call any API.",
                    out_path.display()
                ),
                "session": name,
            }),
        )
        .await;
        let tid = board_store::internal_id(&sem);
        members.push(FleetWorker {
            name,
            wid,
            ws: ws.path().to_path_buf(),
            out_path,
            marker,
            sem,
            tid,
            is_mock: provider.is_none(),
        });
        workspaces.push(ws);
    }

    let fleet = Arc::new(FleetProtocol { routes, prompts: Mutex::new(Vec::new()) });
    // One event processor per worker, through the SAME protocol the runtime
    // uses (subscribe-before-emit is inside spawn_event_processor).
    let processors: Vec<_> = members
        .iter()
        .map(|m| {
            wevents::spawn_event_processor(rig.store.clone(), fleet.clone(), m.wid.clone())
        })
        .collect();

    let rt = runtime_with(rig.store.clone(), Some(fleet.clone()), vec![]);

    // Tick 1: three owned tasks -> three leases + three ExecuteTask commands.
    rt.tick_once(false).await.unwrap();
    let leases = live_leases(&rig.store);
    assert_eq!(leases.len(), members.len(), "one lease per fleet member: {leases:?}");
    for m in &members {
        assert!(
            leases.contains(&(m.tid.to_string(), m.wid.as_str().to_string())),
            "[{}] task must be leased to its owner: {leases:?}",
            m.name
        );
    }

    // Drive ticks + pumps until every member is confirmed + artifact on disk,
    // or 240s. Terminal failures fail FAST naming the member (a hang must
    // name where — golden_live's rule).
    let deadline = Instant::now() + Duration::from_secs(240);
    let mut mock_worked = false;
    loop {
        rt.tick_once(false).await.unwrap();

        // The mock member "does the work" when its prompt arrives: the test
        // writes the artifact and streams the turn — the Invariant 22 seam
        // standing in for a third provider's model.
        if !mock_worked {
            if let Some(m) = members.iter().find(|m| m.is_mock) {
                let delivered = fleet
                    .recorded_prompts()
                    .iter()
                    .any(|(w, _)| w == &m.wid);
                if delivered {
                    std::fs::write(&m.out_path, format!("{}\n", m.marker)).unwrap();
                    let turn = TurnId::from_ulid(ulid::Ulid::new());
                    rig.protocol.set_state(
                        &m.wid,
                        AgentState::Working { turn: Some(turn.clone()), progress: None },
                        Some(WorkerEvent::TurnStarted { turn_id: turn.clone() }),
                    );
                    rig.protocol.set_state(
                        &m.wid,
                        AgentState::Idle,
                        Some(WorkerEvent::TurnCompleted(TurnResult {
                            turn_id: turn,
                            outcome: "completed".into(),
                        })),
                    );
                    mock_worked = true;
                }
            }
        }

        for m in &members {
            let state = worker_durable_state(&rig.store, &m.wid);
            let failed = failed_count(&rig.store, &m.wid);
            if failed > 0 || matches!(state, WorkerState::Error { .. }) {
                panic!(
                    "[{}] provider turn FAILED after {:.0}s; fleet state:\n{}",
                    m.name,
                    t0.elapsed().as_secs_f32(),
                    fleet_dump(&rig.store, &members)
                );
            }
        }
        let done = members
            .iter()
            .all(|m| confirmed_count(&rig.store, &m.wid) >= 1 && m.out_path.exists());
        if done {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fleet did not converge within 240s; state:\n{}",
            fleet_dump(&rig.store, &members)
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    for p in processors {
        p.abort();
    }
    eprintln!(
        "[multi-provider] all {} members confirmed at t+{:.0}s",
        members.len(),
        t0.elapsed().as_secs_f32()
    );

    // Each artifact was written BY its own provider in its own cwd.
    for m in &members {
        let content = std::fs::read_to_string(&m.out_path).unwrap_or_else(|e| {
            panic!("[{}] {} missing ({e})", m.name, m.out_path.display())
        });
        assert!(
            content.contains(&m.marker),
            "[{}] artifact lacks {}: {content:?}",
            m.name,
            m.marker
        );
        // Belt and braces: nobody else's marker landed in this workspace.
        for other in members.iter().filter(|o| o.name != m.name) {
            assert!(
                !m.ws.join(other.out_path.file_name().unwrap()).exists(),
                "[{}] {}'s artifact leaked into {}'s workspace",
                m.name,
                other.name,
                m.name
            );
        }
    }

    // NO cross-worker prompt delivery: every recorded SendPrompt names its
    // own worker's task and no other's, exactly once per member.
    let prompts = fleet.recorded_prompts();
    assert_eq!(
        prompts.len(),
        members.len(),
        "exactly one delivery per member (idempotency, Invariant 9): {prompts:?}"
    );
    for (worker, text) in &prompts {
        let named: Vec<&FleetWorker> = members
            .iter()
            .filter(|m| text.contains(m.tid.as_str()))
            .collect();
        assert_eq!(named.len(), 1, "prompt names exactly one task: {text}");
        assert_eq!(
            &named[0].wid, worker,
            "SendPrompt worker must match the task owner (delivered {} work to {worker})",
            named[0].name
        );
    }

    // Drive all tasks to VERIFIED through the board + typed verification:
    // done on the gate, verified on file_exists against the provider's own
    // artifact — the full loop closed per member.
    for m in &members {
        let v = patch_ok(app, &m.sem, json!({ "status": "doing", "gate_ack": true }), m.name).await;
        assert_eq!(v["status"], json!("doing"), "{v}");
        let v = patch_ok(
            app,
            &m.sem,
            json!({
                "status": "done",
                "gate_checked": ["Implemented and merged", "Tests / lint pass"]
            }),
            m.name,
        )
        .await;
        assert_eq!(v["status"], json!("done"), "{v}");
        let (st, v) = send_with(
            app,
            "POST",
            &format!("/api/verify/{}", m.sem),
            Some(json!({
                "criteria": [{
                    "description": format!("{}'s artifact exists on disk", m.name),
                    "verifier": { "kind": "file_exists", "path": m.out_path.to_string_lossy() },
                    "required": true
                }]
            })),
            &[("X-Amux-Session", m.name)],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "[{}] verify: {v}", m.name);
        assert_eq!(v["verdict"]["kind"], json!("passed"), "[{}] {v}", m.name);
        assert_eq!(v["new_status"], json!("verified"), "[{}] {v}", m.name);
        assert_eq!(detail(app, &m.sem).await["status"], json!("verified"));
    }

    eprintln!(
        "[multi-provider] PASS in {:.0}s — members: {}; deliveries: {:?}",
        t0.elapsed().as_secs_f32(),
        members
            .iter()
            .map(|m| format!("{}({})", m.name, if m.is_mock { "mock" } else { "real" }))
            .collect::<Vec<_>>()
            .join(", "),
        prompts
            .iter()
            .map(|(w, t)| {
                let head: String = t.chars().take(60).collect();
                format!("{w} <- {}…", head.replace('\n', " "))
            })
            .collect::<Vec<_>>(),
    );
}

// ===========================================================================
// RR-0086 — Golden scenario: backend interchangeability (LIVE)
// ===========================================================================

/// Markers only the RUNNING claude TUI paints (golden_live.rs) — none appear
/// in the echoed spawn command line, so a match proves the UI is up.
const CLAUDE_UI_MARKERS: &[&str] = &[
    "? for shortcuts",
    "welcome to claude",
    "bypass permissions",
    "esc to interrupt",
    "no, exit",
];

async fn wait_for_claude_ui(
    backend: &Arc<dyn SessionBackend>,
    proc: &ProcessRef,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut last_capture = String::new();
    loop {
        match backend.status(proc).await {
            Ok(BackendStatus::Running) => {}
            Ok(other) => {
                return Err(format!(
                    "claude session ended before the UI appeared (status {other:?}); \
                     last capture:\n{last_capture}"
                ))
            }
            Err(e) => return Err(format!("status probe errored while waiting for UI: {e}")),
        }
        match backend.capture(proc, 100).await {
            Ok(frame) => {
                let lower = frame.to_lowercase();
                if CLAUDE_UI_MARKERS.iter().any(|m| lower.contains(m)) {
                    return Ok(());
                }
                last_capture = frame;
            }
            Err(e) => last_capture = format!("<capture error: {e}>"),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "claude UI did not appear within {timeout:?}; last capture:\n{last_capture}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Everything the orchestrator SEES from one lifecycle, with backend-scoped
/// identities (worker/session ids, refs) normalized away. Two backends are
/// interchangeable exactly when these compare equal.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LifecycleTrace {
    /// StateEvent kinds in emission order, ids elided: "session:running->interrupted".
    event_kinds: Vec<String>,
    scan_scanned_self: bool,
    scan_demoted: usize,
    scan_capture_failures: usize,
    scan_events_applied: usize,
    reconcile_interrupted_self: bool,
    reconcile_probe_failures: usize,
    session_ended: bool,
    /// Exit-reason CLASSIFICATION from the session row ("crashed", ...).
    exit_reason: String,
}

fn normalize_event(e: &StateEvent) -> String {
    let ty = match &e.entity_type {
        EntityType::Other(s) => s.clone(),
        t => format!("{t:?}").to_lowercase(),
    };
    let mu = match &e.mutation {
        MutationKind::Created => "created".to_string(),
        MutationKind::Updated => "updated".to_string(),
        MutationKind::Deleted => "deleted".to_string(),
        MutationKind::StatusChanged { from, to } => format!("{from}->{to}"),
    };
    format!("{ty}:{mu}")
}

/// The SAME lifecycle golden_live.rs runs (spawn interactive claude -> UI up
/// -> scan_once reaches it -> terminate -> reconcile marks interrupted),
/// instrumented to return what the orchestrator observed. Safety rules are
/// copied from backend_conformance.rs verbatim: wrk_-shaped refs only, the
/// guard re-checked on the exact value passed to terminate, cleanup that
/// always runs, and the live fleet's sessions only ever COUNTED.
async fn traced_live_backend_lifecycle(
    backend: Arc<dyn SessionBackend>,
    label: &str,
) -> LifecycleTrace {
    let t0 = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let store: SharedStore = Arc::new(Store::open(&dir.path().join("live.db")).unwrap());
    // Subscribe BEFORE any write: the trace must contain every event.
    let mut rx = store.subscribe();
    let ws = tempfile::tempdir().unwrap();

    let wid = WorkerId::from_ulid(ulid::Ulid::new());
    let ref_ = backend_ref(&wid);
    assert!(
        ref_.contains("wrk_"),
        "[{label}] test ref {ref_:?} is not worker-shaped; refusing to run against a live fleet"
    );

    // Durable rows the scan + reconciliation read.
    {
        let (wid2, ref2, backend_name) = (wid.clone(), ref_.clone(), label.to_string());
        let cwd = ws.path().to_string_lossy().into_owned();
        store
            .write(move |conn| {
                let cfg = CoreWorkerConfig {
                    display_name: format!("live-{backend_name}"),
                    name_aliases: vec![],
                    cwd: cwd.clone(),
                    provider: ProviderId::new("claude"),
                    model: None,
                    backend: if backend_name == "tmux" {
                        BackendId::tmux()
                    } else {
                        BackendId::herdr()
                    },
                    environment: Default::default(),
                    permissions: vec![],
                    group: None,
                };
                let row = WorkerRow::new(&wid2, &cfg, &Utc::now().to_rfc3339());
                queries::insert_worker(conn, &row)?;
                queries::insert_session(
                    conn,
                    &SessionRow {
                        id: format!("ses_{}", ulid::Ulid::new()),
                        worker_id: wid2.as_str().to_string(),
                        backend: backend_name.clone(),
                        backend_ref: ref2.clone(),
                        pid: None,
                        started_at: Utc::now().to_rfc3339(),
                        ended_at: None,
                        exit_reason: None,
                    },
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
    }

    // Spawn the REAL interactive claude session.
    let spec = SessionSpec {
        worker: wid.clone(),
        command: vec!["claude".into(), "--dangerously-skip-permissions".into()],
        cwd: ws.path().to_string_lossy().into_owned(),
        env: BTreeMap::from([("AMUX_GOLDEN_LIVE".to_string(), "1".to_string())]),
        human_label: None,
    };
    let proc = backend
        .spawn(&spec)
        .await
        .unwrap_or_else(|e| panic!("[{label}] spawn failed: {e}"));
    assert_eq!(proc.backend_ref, ref_, "[{label}] spawn must return the canonical ref");

    // Everything until terminate is collected, never panicked (guard idiom):
    // a mid-lifecycle failure must not leak a live claude session.
    let mid: Result<amux_server::orchestrator::scan::ScanReport, String> = async {
        wait_for_claude_ui(&backend, &proc, Duration::from_secs(60)).await?;
        eprintln!("[{label}] claude UI up at t+{:.1}s", t0.elapsed().as_secs_f32());

        // The scan loop: no structured protocol session, so this worker must
        // be SCANNED (the scraper is its only voice), never demoted.
        let scan = ScanLoop::new(store.clone(), vec![backend.clone()], None);
        let report = scan
            .scan_once()
            .await
            .map_err(|e| format!("scan_once errored: {e}"))?;
        if !report.scanned.iter().any(|s| s == wid.as_str()) {
            return Err(format!(
                "scan did not reach the worker: scanned={:?} demoted={:?} failures={:?}",
                report.scanned, report.demoted_structured, report.capture_failures
            ));
        }
        Ok(report)
    }
    .await;

    // SAFETY GUARD: checked on the exact value passed to the kill.
    assert!(
        proc.backend_ref.contains("wrk_"),
        "[{label}] REFUSING terminate: {:?} is not a throwaway ref",
        proc.backend_ref
    );
    let term = backend.terminate(&proc).await;

    let scan_report = match mid {
        Ok(ok) => ok,
        Err(msg) => panic!("[{label}] {msg}"),
    };
    term.unwrap_or_else(|e| panic!("[{label}] terminate failed: {e}"));

    // The host must report the session gone (zero leaked sessions).
    match backend.status(&proc).await {
        Ok(BackendStatus::NotFound) | Ok(BackendStatus::Completed { .. }) => {}
        Ok(other) => {
            panic!("[{label}] status after terminate: expected NotFound/Completed, got {other:?}")
        }
        Err(e) => panic!("[{label}] status after terminate errored: {e}"),
    }

    // Startup reconciliation: DB says live, backend says gone -> interrupted.
    // The live fleet's own amux-* sessions surface as stale_backend — READ
    // ONLY, counted, never touched (ethos rule 8).
    let rt = runtime_with(store.clone(), None, vec![backend.clone()]);
    let report = rt
        .reconcile_on_startup()
        .await
        .unwrap_or_else(|e| panic!("[{label}] reconcile_on_startup failed: {e}"));
    let (ended, exit_reason): (Option<String>, Option<String>) = {
        let conn = store.read().unwrap();
        conn.query_row(
            "SELECT ended_at, exit_reason FROM _amux_sessions WHERE worker_id = ?1",
            params![wid.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    let exit_class = exit_reason
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v["reason"].as_str().map(str::to_string))
        .unwrap_or_else(|| "<none>".into());

    let trace = LifecycleTrace {
        event_kinds: drain(&mut rx).iter().map(normalize_event).collect(),
        scan_scanned_self: scan_report.scanned.iter().any(|s| s == wid.as_str()),
        scan_demoted: scan_report.demoted_structured.len(),
        scan_capture_failures: scan_report.capture_failures.len(),
        scan_events_applied: scan_report.events_applied,
        reconcile_interrupted_self: report.interrupted.iter().any(|w| w == wid.as_str()),
        reconcile_probe_failures: report.backend_probe_failures.len(),
        session_ended: ended.is_some(),
        exit_reason: exit_class,
    };
    eprintln!(
        "[{label}] lifecycle done in {:.0}s — ref {ref_}; stale_backend fleet sessions seen \
         (untouched): {}; trace: {trace:?}",
        t0.elapsed().as_secs_f32(),
        report.stale_backend.len(),
    );
    trace
}

#[tokio::test]
#[ignore = "spawns REAL interactive claude under tmux AND herdr; locally: cargo test -p amux-server --test golden_remaining -- --ignored --nocapture"]
async fn golden_backend_interchangeability() {
    if !have_bin("claude") {
        eprintln!("SKIPPED: golden_backend_interchangeability — `claude` not on PATH; interchangeability was NOT tested");
        return;
    }
    if !binary_available("tmux", "-V").await {
        eprintln!("SKIPPED: golden_backend_interchangeability — `tmux` not found on PATH; interchangeability was NOT tested");
        return;
    }
    if !binary_available("herdr", "--version").await {
        eprintln!("SKIPPED: golden_backend_interchangeability — `herdr` not found on PATH; interchangeability (the whole point) was NOT tested");
        return;
    }
    let Some(session) = running_herdr_session().await else {
        eprintln!(
            "SKIPPED: golden_backend_interchangeability — herdr installed but no herdr server \
             session running (start `herdr --session amux`); interchangeability was NOT tested"
        );
        return;
    };

    // The SAME lifecycle, once per backend.
    let tmux_trace =
        traced_live_backend_lifecycle(Arc::new(TmuxBackend::new()), "tmux").await;
    let herdr_trace =
        traced_live_backend_lifecycle(Arc::new(HerdrBackend::new(session)), "herdr").await;

    // Each run individually did what the lifecycle demands. These per-run
    // asserts exist so the equality below cannot green-lie on two traces
    // that are identical because BOTH observed nothing (ethos rule 7: a
    // check must be able to fail).
    for (label, t) in [("tmux", &tmux_trace), ("herdr", &herdr_trace)] {
        assert!(t.scan_scanned_self, "[{label}] scan must reach the worker: {t:?}");
        assert_eq!(t.scan_demoted, 0, "[{label}] nothing to demote (no protocol wired): {t:?}");
        assert_eq!(t.scan_capture_failures, 0, "[{label}] capture must succeed: {t:?}");
        assert_eq!(t.reconcile_probe_failures, 0, "[{label}] probe must answer: {t:?}");
        assert!(t.reconcile_interrupted_self, "[{label}] reconcile must mark it interrupted: {t:?}");
        assert!(t.session_ended, "[{label}] session row must be ended: {t:?}");
        assert!(
            t.event_kinds.contains(&"session:running->interrupted".to_string()),
            "[{label}] the interruption StateEvent must be in the trace: {t:?}"
        );
    }

    // THE assertion: above the backend boundary the two runs are the same
    // run — same event kinds in the same order, same scan shape, same exit
    // classification. A backend swap changes nothing the orchestrator sees
    // (Invariant 33; the split that makes backends interchangeable).
    assert_eq!(
        tmux_trace, herdr_trace,
        "backend swap leaked through the boundary: the orchestrator observed different \
         lifecycles under tmux vs herdr"
    );
    eprintln!(
        "[interchangeability] PASS — identical traces above the backend boundary: {tmux_trace:?}"
    );
}
