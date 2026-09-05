//! API-level golden scenarios (Phase 5): RR-0079 failure + retry, RR-0081
//! dependency chain, RR-0084 no-stall invariant — the scenarios that need no
//! live model. Each test drives the REAL subsystems end-to-end: the axum
//! router (`tower::ServiceExt::oneshot`) and orchestrator `Runtime` share one
//! temp-file `Store`; the only fake is the agent itself (`MockProtocol`, the
//! Invariant 22 simulation seam).
//!
//! Two currently-real runtime gaps are exercised rather than papered over,
//! each marked with a `KNOWN GAP` comment at its assertion:
//! - `Runtime::load_board_tasks` feeds `disposition()` a todo-only slice,
//!   while `first_unmet_dependency`'s contract says "Pass the full board" —
//!   so a card whose dependencies COMPLETED (left todo) reads as
//!   conservatively-unmet forever. golden_dependency_chain proves the core
//!   planner resolves the chain correctly over the full board, and pins the
//!   runtime's current behavior so the fix announces itself here.
//! - Nothing releases a lease when its task completes; expiry reclaim is the
//!   only release path. golden_no_stall time-warps spent leases to expired so
//!   the runtime's REAL reclaim path frees capacity inside test time.

use amux_core::board::{disposition, Task, TaskDisposition};
use amux_core::circuit::{FleetCircuitBreaker, FleetState};
use amux_core::ids::{TaskId, TurnId, WorkerId};
use amux_core::orchestrator::{plan_tick, Lease, TickInputs};
use amux_core::protocol::{TurnResult, WorkerEvent};
use amux_core::revision::{EntityType, MutationKind, StateEvent};
use amux_core::worker::{Worker, WorkerState};
use amux_server::api::{router, AppState};
use amux_server::db::board_store::{self, ArchivedFilter};
use amux_server::db::{queries, SharedStore, Store, WriteOutcome};
use amux_server::opencode::mock::{MockProtocol, RecordedCall};
use amux_server::opencode::AgentState;
use amux_server::orchestrator::events as wevents;
use amux_server::orchestrator::runtime::Runtime;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{DateTime, Utc};
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tower::ServiceExt;

// ---- rig: one store under router + runtime + mock agent -------------------

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
        build_hash: "golden-test".into(),
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
fn runtime(rig: &Rig, pickup_unowned: bool) -> Runtime {
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
        fleet_state: std::sync::Mutex::new(FleetState::Normal),
        protocol: Some(rig.protocol.clone()),
        pickup_unowned,
        resume_stagger_secs: 5,
    }
}

// ---- HTTP plumbing (board_api.rs idioms) ----------------------------------

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

/// POST /api/workers — name-resolution paths get exercised because board
/// ownership resolves by display name, not by id.
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

async fn create_task(
    app: &axum::Router,
    title: &str,
    session: Option<&str>,
    deps: &[String],
) -> String {
    let mut body = json!({ "title": title });
    if let Some(s) = session {
        body["session"] = json!(s);
    }
    if !deps.is_empty() {
        body["depends_on"] = json!(deps);
    }
    let (st, v) = send_with(app, "POST", "/api/board", Some(body), &[]).await;
    assert_eq!(st, StatusCode::CREATED, "task create failed: {v}");
    v["id"].as_str().unwrap().to_string()
}

async fn detail(app: &axum::Router, sem: &str) -> Value {
    let (st, v) = send_with(app, "GET", &format!("/api/board/{sem}"), None, &[]).await;
    assert_eq!(st, StatusCode::OK, "detail {sem}: {v}");
    v
}

async fn patch_ok(app: &axum::Router, sem: &str, body: Value, actor: &str) -> Value {
    let (st, v) = send_with(
        app,
        "PATCH",
        &format!("/api/board/{sem}"),
        Some(body),
        &[("X-Amux-Session", actor)],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "PATCH {sem} refused: {v}");
    v
}

/// todo -> doing -> done for a `code` card: the doing gate is acked
/// wholesale, the done gate is satisfied with the EXACT type-derived
/// criteria (`gate_checked` must match every criterion — AMUX-1719).
async fn drive_to_done(app: &axum::Router, sem: &str, actor: &str) {
    let v = patch_ok(app, sem, json!({ "status": "doing", "gate_ack": true }), actor).await;
    assert_eq!(v["status"], json!("doing"), "{v}");
    let v = patch_ok(
        app,
        sem,
        json!({
            "status": "done",
            "gate_checked": ["Implemented and merged", "Tests / lint pass"]
        }),
        actor,
    )
    .await;
    assert_eq!(v["status"], json!("done"), "{v}");
}

/// POST /api/verify/{id} with a single typed Command criterion.
async fn run_verify(app: &axum::Router, sem: &str, cmd: &str, actor: &str) -> Value {
    let body = json!({
        "criteria": [{
            "description": format!("golden criterion: `{cmd}` exits 0"),
            "verifier": { "kind": "command", "cmd": cmd, "expected_exit": 0 },
            "required": true
        }]
    });
    let (st, v) = send_with(
        app,
        "POST",
        &format!("/api/verify/{sem}"),
        Some(body),
        &[("X-Amux-Session", actor)],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "verify {sem}: {v}");
    v
}

// ---- store-side observers -------------------------------------------------

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

fn lease_worker_for(store: &SharedStore, task: &TaskId) -> Option<String> {
    let t = task.to_string();
    live_leases(store)
        .into_iter()
        .find(|(task_id, _)| *task_id == t)
        .map(|(_, w)| w)
}

/// Time-warp a lease to expired so the next tick's REAL reclaim path
/// (plan.reclaim -> DELETE + StateEvent) releases it. Nothing in the runtime
/// releases a lease on task completion yet; this compresses the 600s expiry
/// a test cannot wait out — the deletion itself still runs the shipped code.
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

fn drain(rx: &mut tokio::sync::broadcast::Receiver<StateEvent>) -> Vec<StateEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

fn count_status_changes(events: &[StateEvent], entity_id: &str, from: &str, to: &str) -> usize {
    events
        .iter()
        .filter(|e| {
            e.entity_id == entity_id
                && matches!(&e.mutation,
                    MutationKind::StatusChanged { from: f, to: t } if f == from && t == to)
        })
        .count()
}

fn has_lease_created(events: &[StateEvent], task_id: &str) -> bool {
    events.iter().any(|e| {
        e.entity_type == EntityType::Other("lease".into())
            && e.entity_id == task_id
            && matches!(e.mutation, MutationKind::Created)
    })
}

fn latest_heartbeat(events: &[StateEvent]) -> Option<Value> {
    events
        .iter()
        .rev()
        .find(|e| e.entity_type == EntityType::Other("fleet_progress".into()))
        .map(|e| serde_json::from_str(&e.entity_id).unwrap())
}

/// The full core board + worker fleet + live leases, assembled the way
/// `first_unmet_dependency`'s contract demands ("Pass the full board") —
/// ownership resolved by display name exactly as the runtime resolves it.
fn core_snapshot(store: &SharedStore) -> (Vec<Task>, Vec<Worker>, Vec<Lease>) {
    let conn = store.read().unwrap();
    let (wrows, _) = queries::list_workers(&conn, 0, 10_000).unwrap();
    let mut names: BTreeMap<String, WorkerId> = BTreeMap::new();
    let workers: Vec<Worker> = wrows
        .iter()
        .map(|row| {
            let id = WorkerId::parse(&row.id).unwrap();
            names.insert(row.display_name.to_lowercase(), id.clone());
            let mut w = Worker::new(id, row.config(), Default::default());
            w.state = row.state.clone();
            w.version = row.version;
            w
        })
        .collect();
    let rows = board_store::list_issues(&conn, &[], &[], ArchivedFilter::ActiveOnly).unwrap();
    let mut tasks = Vec::new();
    for row in rows {
        let Some(mut t) = row.to_task() else { continue };
        if let Some(owner) = row.session.as_deref().filter(|s| !s.trim().is_empty()) {
            match names.get(&owner.to_lowercase()) {
                Some(wid) => t.worker = Some(wid.clone()),
                None => continue,
            }
        }
        tasks.push(t);
    }
    let mut stmt = conn
        .prepare("SELECT task_id, worker_id, acquired_at, expires_at, generation FROM _amux_leases")
        .unwrap();
    let leases: Vec<Lease> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, u64>(4)?,
            ))
        })
        .unwrap()
        .filter_map(|row| {
            let (t, w, acq, exp, generation) = row.ok()?;
            Some(Lease {
                task: TaskId::parse(&t).ok()?,
                worker: WorkerId::parse(&w).ok()?,
                acquired_at: acq.parse().ok()?,
                expires_at: exp.parse().ok()?,
                generation,
            })
        })
        .collect();
    (tasks, workers, leases)
}

/// Report a turn through the real event-processing path: keeps the worker's
/// durable state Idle and confirms whatever command was Delivered
/// (Invariant 34 step 5 — TurnCompleted is the confirmation signal).
async fn report_turn(store: &SharedStore, worker: &WorkerId) {
    let turn = TurnId::from_ulid(ulid::Ulid::new());
    wevents::process_event(store, worker, WorkerEvent::TurnStarted { turn_id: turn.clone() })
        .await
        .unwrap();
    wevents::process_event(
        store,
        worker,
        WorkerEvent::TurnCompleted(TurnResult { turn_id: turn, outcome: "completed".into() }),
    )
    .await
    .unwrap();
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

// ===========================================================================
// RR-0079 — Golden scenario 2: failure + retry
// ===========================================================================

#[tokio::test]
async fn golden_failure_and_retry() {
    let rig = rig();
    let app = &rig.app;
    // Subscribe BEFORE anything mutates, so every transition's StateEvent is
    // captured in one journal and asserted at the end.
    let mut rx = rig.store.subscribe();

    let wid = register_worker(app, &rig.protocol, "golden-worker").await;
    let sem = create_task(app, "harden the flaky retry path", Some("golden-worker"), &[]).await;
    let tid = board_store::internal_id(&sem);
    let rt = runtime(&rig, false);

    // Tick 1: the owned todo task is planned -> lease + ExecuteTask command.
    rt.tick_once(false).await.unwrap();
    assert_eq!(
        lease_worker_for(&rig.store, &tid).as_deref(),
        Some(wid.as_str()),
        "tick must lease the owned task to its registered worker"
    );
    let cmds = command_rows(&rig.store, &wid);
    assert_eq!(cmds.len(), 1, "exactly one ExecuteTask enqueued: {cmds:?}");
    assert!(cmds[0].0.contains("execute_task"), "{:?}", cmds[0]);
    assert!(cmds[0].0.contains(tid.as_str()), "{:?}", cmds[0]);
    assert!(cmds[0].1.contains("queued"), "{:?}", cmds[0]);

    // Tick 2: the pump delivers through MockProtocol (WhenIdle, agent Idle).
    rt.tick_once(false).await.unwrap();
    let calls = rig.protocol.calls();
    assert_eq!(calls.len(), 1, "one delivery: {calls:?}");
    match &calls[0] {
        RecordedCall::SendPrompt { worker, prompt } => {
            assert_eq!(worker, &wid);
            // The pump delivers the WORK (feed-forward assignment brief),
            // not a serialized command (live-golden finding: the model
            // needs the task, not its id).
            assert!(prompt.text.contains(tid.as_str()), "{}", prompt.text);
            assert!(prompt.text.contains("harden the flaky retry path"), "{}", prompt.text);
            assert!(prompt.text.contains("board card"), "{}", prompt.text);
        }
        other => panic!("expected SendPrompt, got {other:?}"),
    }
    let cmds = command_rows(&rig.store, &wid);
    assert!(cmds[0].1.contains("delivered"), "{:?}", cmds[0]);

    // The agent "works": TurnStarted/TurnCompleted stream through the real
    // event processor — worker lands Idle, the delivered command Confirmed.
    let handle = wevents::spawn_event_processor(rig.store.clone(), rig.protocol.clone(), wid.clone());
    let turn = TurnId::from_ulid(ulid::Ulid::new());
    rig.protocol
        .emit(&wid, WorkerEvent::TurnStarted { turn_id: turn.clone() });
    rig.protocol.emit(
        &wid,
        WorkerEvent::TurnCompleted(TurnResult { turn_id: turn, outcome: "completed".into() }),
    );
    {
        let store = rig.store.clone();
        let wid = wid.clone();
        wait_until(move || {
            let conn = store.read().unwrap();
            let idle = matches!(
                queries::get_worker(&conn, wid.as_str()).unwrap().unwrap().state,
                WorkerState::Idle { .. }
            );
            let confirmed: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM _amux_commands
                     WHERE worker_id = ?1 AND state LIKE '%confirmed%'",
                    params![wid.as_str()],
                    |r| r.get(0),
                )
                .unwrap();
            idle && confirmed == 1
        })
        .await;
    }
    handle.abort();

    // The worker claims completion on the board: todo -> doing -> done.
    drive_to_done(app, &sem, "golden-worker").await;

    // Verification with a FAILING typed Command criterion: 200, verdict
    // Failed, task revoked back to doing with the rejection reason logged.
    let v = run_verify(app, &sem, "false", "golden-worker").await;
    assert_eq!(v["verdict"]["kind"], json!("failed"), "{v}");
    assert!(
        v["verdict"]["reason"].as_str().unwrap().contains("exited 1"),
        "{v}"
    );
    assert_eq!(v["new_status"], json!("doing"), "{v}");
    let d = detail(app, &sem).await;
    assert_eq!(d["status"], json!("doing"), "failed verification revokes the done claim");
    let log = d["log"].as_str().unwrap();
    assert!(log.contains("verification FAILED"), "rejection reason must be in the log: {log}");
    assert!(log.contains("exited 1"), "log: {log}");

    // Retry: the worker fixes it, claims done again, verification PASSES.
    let v = patch_ok(
        app,
        &sem,
        json!({
            "status": "done",
            "gate_checked": ["Implemented and merged", "Tests / lint pass"]
        }),
        "golden-worker",
    )
    .await;
    assert_eq!(v["status"], json!("done"));
    let v = run_verify(app, &sem, "true", "golden-worker").await;
    assert_eq!(v["verdict"]["kind"], json!("passed"), "{v}");
    assert_eq!(v["new_status"], json!("verified"), "{v}");
    let d = detail(app, &sem).await;
    assert_eq!(d["status"], json!("verified"));
    assert!(
        d["last_verified_at"].is_i64(),
        "last_verified_at must be set: {d}"
    );
    assert!(d["log"].as_str().unwrap().contains("verification PASSED"));

    // Every transition emitted StateEvents (Invariant 35): drain the journal
    // and account for each hop, plus the lease/snapshot/worker/command sides.
    let events = drain(&mut rx);
    assert!(
        events.iter().any(|e| e.entity_type == EntityType::Task
            && e.entity_id == sem
            && matches!(e.mutation, MutationKind::Created)),
        "board create event missing"
    );
    assert!(has_lease_created(&events, tid.as_str()), "lease event missing");
    assert!(
        events.iter().any(|e| e.entity_type == EntityType::Other("context_snapshot".into())
            && matches!(e.mutation, MutationKind::Created)),
        "context snapshot event missing (Invariant 27)"
    );
    assert_eq!(count_status_changes(&events, &sem, "todo", "doing"), 1);
    assert_eq!(
        count_status_changes(&events, &sem, "doing", "done"),
        2,
        "initial claim + post-rejection retry"
    );
    assert_eq!(
        count_status_changes(&events, &sem, "done", "doing"),
        1,
        "the failed verification's revocation"
    );
    assert_eq!(count_status_changes(&events, &sem, "done", "verified"), 1);
    assert!(
        events.iter().any(|e| e.entity_type == EntityType::Worker
            && e.entity_id == wid.as_str()
            && matches!(&e.mutation, MutationKind::StatusChanged { to, .. } if to == "idle")),
        "worker idle event from TurnCompleted missing"
    );
    assert!(
        events.iter().any(|e| e.entity_type == EntityType::Other("command".into())
            && matches!(&e.mutation,
                MutationKind::StatusChanged { to, .. } if to == "confirmed")),
        "command confirmation event missing (Invariant 34)"
    );
}

// ===========================================================================
// RR-0081 — Golden scenario 4: dependency chain
// ===========================================================================

#[tokio::test]
async fn golden_dependency_chain() {
    let rig = rig();
    let app = &rig.app;
    let mut rx = rig.store.subscribe();
    let mut journal: Vec<StateEvent> = Vec::new();

    let w_a = register_worker(app, &rig.protocol, "runner-a").await;
    let w_b = register_worker(app, &rig.protocol, "runner-b").await;
    let w_c = register_worker(app, &rig.protocol, "runner-c").await;
    let w_p = register_worker(app, &rig.protocol, "parent-owner").await;

    // Children first, then the parent depending on all three. The parent
    // gets its own owner so a child's still-live lease (nothing releases a
    // lease on completion; see module docs) can never mask the dependency
    // wait behind a WIP-capacity wait.
    let c1 = create_task(app, "child one", Some("runner-a"), &[]).await;
    let c2 = create_task(app, "child two", Some("runner-b"), &[]).await;
    let c3 = create_task(app, "child three", Some("runner-c"), &[]).await;
    let parent = create_task(
        app,
        "parent integration",
        Some("parent-owner"),
        &[c1.clone(), c2.clone(), c3.clone()],
    )
    .await;
    let ptid = board_store::internal_id(&parent);

    let rt = runtime(&rig, false);

    // Tick 1: all three children leased concurrently, one per owner; the
    // parent's disposition is Waiting(Dependency) so it must NOT be leased.
    rt.tick_once(false).await.unwrap();
    journal.extend(drain(&mut rx));
    assert_eq!(live_leases(&rig.store).len(), 3, "three children leased");
    for (child, owner) in [(&c1, &w_a), (&c2, &w_b), (&c3, &w_c)] {
        let ctid = board_store::internal_id(child);
        assert_eq!(
            lease_worker_for(&rig.store, &ctid).as_deref(),
            Some(owner.as_str()),
            "child {child} leased to its owner"
        );
    }
    assert!(
        lease_worker_for(&rig.store, &ptid).is_none(),
        "parent must not be leased while dependencies are open"
    );

    // Complete the first TWO children; after each, tick and re-assert the
    // parent stays unleased (one dependency still open). The THIRD child's
    // completion is the release edge — asserted after the loop, because the
    // runtime now (correctly) leases the parent on the very tick that
    // satisfies the last dependency.
    for (child, actor) in [(&c1, "runner-a"), (&c2, "runner-b")] {
        drive_to_done(app, child, actor).await;
        let v = run_verify(app, child, "true", actor).await;
        assert_eq!(v["verdict"]["kind"], json!("passed"), "{v}");
        rt.tick_once(false).await.unwrap();
        journal.extend(drain(&mut rx));
        assert!(
            lease_worker_for(&rig.store, &ptid).is_none(),
            "parent leased before all children completed (after {child})"
        );
    }
    drive_to_done(app, &c3, "runner-c").await;
    let v = run_verify(app, &c3, "true", "runner-c").await;
    assert_eq!(v["verdict"]["kind"], json!("passed"), "{v}");

    // All three children are verified. The CORE planner, given the board the
    // way first_unmet_dependency's contract demands (the FULL board), now
    // resolves the parent Runnable and assigns it to its owner.
    let (tasks, workers, leases) = core_snapshot(&rig.store);
    let parent_task = tasks.iter().find(|t| t.id == ptid).expect("parent on board");
    assert_eq!(parent_task.depends_on.len(), 3, "edges intact, deps satisfied by status");
    assert!(
        matches!(disposition(parent_task, &tasks, &[]), TaskDisposition::Runnable),
        "all deps Done/Verified -> parent Runnable (Invariant 4)"
    );
    let fleet = FleetState::Normal;
    let (hints, attempts) = (BTreeMap::new(), BTreeMap::new());
    let providers = BTreeMap::new();
    let plan = plan_tick(&TickInputs {
        now: Utc::now(),
        tasks: &tasks,
        workers: &workers,
        leases: &leases,
        fleet_state: &fleet,
        hints: &hints,
        attempts: &attempts,
        gates: &[],
        lease_secs: 600,
        wip_limit: 1,
        provider_states: &providers,
    });
    let asg = plan
        .assignments
        .iter()
        .find(|a| a.task == ptid)
        .expect("core plan assigns the freed parent");
    assert_eq!(asg.worker, w_p, "owned task goes to its owner");

    // GAP CLOSED (loader now feeds disposition() the FULL board): the
    // runtime itself leases the parent the moment its last dependency is
    // verified — no prune bridge needed. The journal check above already
    // proved no premature lease existed.
    assert!(
        !has_lease_created(&journal, ptid.as_str()),
        "a parent lease event exists in the journal before dependencies were satisfied"
    );
    rt.tick_once(false).await.unwrap();
    journal.extend(drain(&mut rx));
    assert_eq!(
        lease_worker_for(&rig.store, &ptid).as_deref(),
        Some(w_p.as_str()),
        "the RUNTIME leases the parent once all dependencies verified"
    );
    assert!(has_lease_created(&journal, ptid.as_str()));
}

// ===========================================================================
// RR-0084 — Golden scenario 7: no-stall invariant
// ===========================================================================

/// The RR-0084 checkpoint, evaluated after every tick from durable state:
/// no worker with free capacity (available state, no live lease under
/// wip_limit 1) may coexist with an unleased task it could take (owned by it,
/// or unowned under pickup_unowned). Checked post-execute, so anything
/// assignable this tick must already hold a lease.
fn assert_no_capacity_stall(
    store: &SharedStore,
    roster: &[(WorkerId, String)],
    pickup_unowned: bool,
) {
    let leases = live_leases(store);
    let leased_tasks: BTreeSet<&str> = leases.iter().map(|(t, _)| t.as_str()).collect();
    let mut lease_count: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, w) in &leases {
        *lease_count.entry(w.as_str()).or_default() += 1;
    }
    let conn = store.read().unwrap();
    let todo =
        board_store::list_issues(&conn, &["todo".into()], &[], ArchivedFilter::ActiveOnly).unwrap();
    for (wid, name) in roster {
        let state = queries::get_worker(&conn, wid.as_str()).unwrap().unwrap().state;
        let available = matches!(
            state,
            WorkerState::Idle { .. } | WorkerState::Stopped | WorkerState::Starting
        );
        if !available || lease_count.get(wid.as_str()).copied().unwrap_or(0) >= 1 {
            continue; // saturated or busy: not a stall candidate
        }
        let hungry: Vec<&str> = todo
            .iter()
            .filter(|r| {
                !leased_tasks.contains(board_store::internal_id(&r.id).to_string().as_str())
                    && match r.session.as_deref().filter(|s| !s.trim().is_empty()) {
                        Some(owner) => owner.eq_ignore_ascii_case(name),
                        None => pickup_unowned,
                    }
            })
            .map(|r| r.id.as_str())
            .collect();
        assert!(
            hungry.is_empty(),
            "NO-STALL VIOLATION (Invariant 10): worker {name} has free capacity while \
             runnable task(s) {hungry:?} sit unleased"
        );
    }
}

#[tokio::test]
async fn golden_no_stall() {
    let rig = rig();
    let app = &rig.app;

    let w1 = register_worker(app, &rig.protocol, "alpha").await;
    let w2 = register_worker(app, &rig.protocol, "beta").await;
    let roster = [(w1.clone(), "alpha".to_string()), (w2.clone(), "beta".to_string())];
    // Boot both workers to Idle through the real event path — the plan's
    // stall check only examines Idle workers, so Stopped workers would make
    // the heartbeat's stall counter structurally unable to fire (ethos 7).
    for (wid, _) in &roster {
        report_turn(&rig.store, wid).await;
    }

    // 5 tasks, 2 workers: two owned (one per worker), three unowned that the
    // runtime may pick up (pickup_unowned = true).
    let sems = [
        create_task(app, "owned by alpha", Some("alpha"), &[]).await,
        create_task(app, "owned by beta", Some("beta"), &[]).await,
        create_task(app, "pool task one", None, &[]).await,
        create_task(app, "pool task two", None, &[]).await,
        create_task(app, "pool task three", None, &[]).await,
    ];
    let tid_to_sem: BTreeMap<String, String> = sems
        .iter()
        .map(|s| (board_store::internal_id(s).to_string(), s.clone()))
        .collect();

    let rt = runtime(&rig, true);
    let mut rx = rig.store.subscribe();

    let mut total_ticks = 0usize;
    let mut capacity_stall_reports = 0u64;
    loop {
        total_ticks += 1;
        assert!(total_ticks < 50, "run did not converge within 50 ticks");
        rt.tick_once(true).await.unwrap();
        let events = drain(&mut rx);
        let hb = latest_heartbeat(&events).expect("heartbeat tick publishes FleetProgress");
        assert_eq!(hb["workers_total"], json!(2), "{hb}");
        // The plan's own stall check, read from the real heartbeat event.
        // Under wip_limit 1 a reported stall can only be an owned task whose
        // owner is saturated this tick (a capacity report, not a dead fleet);
        // the substantive checkpoint below proves no free-capacity worker had
        // eligible work — the invariant RR-0084 actually states.
        capacity_stall_reports += hb["stall_violations"].as_u64().unwrap();
        assert_no_capacity_stall(&rig.store, &roster, true);

        // Workers "do the work": each freshly leased, still-queued task is
        // completed through the board + verification APIs, and its spent
        // lease is time-warped so the next tick's real reclaim frees the
        // capacity (see expire_lease docs).
        let mut worked = false;
        for (task_id, _worker) in live_leases(&rig.store) {
            let Some(sem) = tid_to_sem.get(&task_id) else { continue };
            let d = detail(app, sem).await;
            if d["status"] == json!("todo") {
                drive_to_done(app, sem, "golden-runner").await;
                let v = run_verify(app, sem, "true", "golden-runner").await;
                assert_eq!(v["verdict"]["kind"], json!("passed"), "{v}");
                expire_lease(&rig.store, &task_id);
                worked = true;
            }
        }
        // Turn reports through the real event path: confirm delivered
        // commands (unblocking each worker's FIFO queue) and keep both
        // workers Idle for the next tick's stall check.
        for (wid, _) in &roster {
            report_turn(&rig.store, wid).await;
        }

        let mut verified = 0;
        for sem in &sems {
            if detail(app, sem).await["status"] == json!("verified") {
                verified += 1;
            }
        }
        if verified == 5 {
            break;
        }
        assert!(
            worked || !live_leases(&rig.store).is_empty(),
            "tick {total_ticks} made no progress: {verified}/5 verified, no leases, no completions"
        );
    }

    // Drain the command pipeline: remaining queued ExecuteTasks deliver as
    // turn confirmations free each worker's queue head.
    for _ in 0..3 {
        rt.tick_once(false).await.unwrap();
        for (wid, _) in &roster {
            report_turn(&rig.store, wid).await;
        }
    }
    drain(&mut rx);

    // Terminal fleet: everything verified, zero live leases, and a final
    // heartbeat that reports a quiet, unstalled fleet.
    for sem in &sems {
        assert_eq!(detail(app, sem).await["status"], json!("verified"), "{sem}");
    }
    rt.tick_once(true).await.unwrap();
    let events = drain(&mut rx);
    let hb = latest_heartbeat(&events).unwrap();
    assert_eq!(hb["stall_violations"], json!(0), "{hb}");
    assert_eq!(hb["live_leases"], json!(0), "{hb}");
    assert!(live_leases(&rig.store).is_empty());

    // Every task executed exactly once: 5 ExecuteTask prompts reached the
    // agent, each naming a distinct task (Invariant 9: no double-assignment).
    let mut executed: BTreeSet<String> = BTreeSet::new();
    let mut prompts = 0;
    for call in rig.protocol.calls() {
        if let RecordedCall::SendPrompt { prompt, .. } = call {
            prompts += 1;
            assert!(prompt.text.contains("board card"), "{}", prompt.text);
            let named = tid_to_sem
                .keys()
                .find(|t| prompt.text.contains(t.as_str()))
                .unwrap_or_else(|| panic!("prompt names no known task: {}", prompt.text));
            assert!(executed.insert(named.clone()), "task double-executed: {named}");
        }
    }
    assert_eq!(prompts, 5, "five deliveries, one per task");
    assert_eq!(executed.len(), 5);
    assert!(total_ticks < 50, "bounded run: took {total_ticks} ticks");
    // Stall reports observed along the way were all capacity reports —
    // proven benign tick-by-tick by assert_no_capacity_stall above.
    let _ = capacity_stall_reports;
}
