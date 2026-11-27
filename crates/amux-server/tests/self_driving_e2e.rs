//! End-to-end proof for the self-driving control-plane primitives.
//!
//! These tests deliberately cross real subsystem boundaries: HTTP policy
//! middleware + SQLite, structured provider subprocess + event processor,
//! runtime scheduling + measured WIP state, and Git detached worktrees.

use amux_core::circuit::{FleetCircuitBreaker, FleetState};
use amux_core::ids::{CommandId, WorkerId};
use amux_core::protocol::{CommandTransition, DeliveryTiming, WorkerCommand};
use amux_server::api::{router, AppState};
use amux_server::db::board_store::{create_issue, internal_id, NewIssue};
use amux_server::db::commands;
use amux_server::db::{SharedStore, Store, WriteOutcome};
use amux_server::opencode::mock::MockProtocol;
use amux_server::opencode::structured::{CliProvider, StructuredCliProtocol, WorkerConfig};
use amux_server::opencode::{AgentProtocol, AgentState, Prompt};
use amux_server::orchestrator::events::spawn_event_processor;
use amux_server::orchestrator::runtime::Runtime;
use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use chrono::Utc;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

struct Rig {
    app: axum::Router,
    store: SharedStore,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("self-driving.db")).unwrap());
    let state = AppState {
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "self-driving-e2e".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    Rig {
        app: router(state),
        store,
        _dir: dir,
    }
}

async fn send(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, headers, value)
}

fn exact_resource(path: &str, body: Option<&Value>) -> String {
    let bytes = body.map(Value::to_string).unwrap_or_default();
    let mut hash = Sha256::new();
    hash.update(bytes.as_bytes());
    format!("{path}#sha256={}", hex::encode(hash.finalize()))
}

async fn approve_exact(
    app: &axum::Router,
    actor: &str,
    path: &str,
    body: Option<&Value>,
) -> String {
    let (_, _, approval) = send(
        app,
        "POST",
        "/api/policy/approvals",
        Some(json!({
            "actor": actor,
            "action": "deploy",
            "resource": exact_resource(path, body),
            "ttl_secs": 60
        })),
        &[("x-amux-session", "reconciliation-admin")],
    )
    .await;
    approval["approval"].as_str().unwrap().to_string()
}

fn issue(title: &str, owner: Option<&str>) -> NewIssue {
    NewIssue {
        acceptance_criteria: None,
        next_action: None,
        title: title.into(),
        desc: "self-driving E2E fixture".into(),
        status: "todo".into(),
        session: owner.map(str::to_owned),
        shepherd: None,
        item_type: "code".into(),
        creator: "self-driving-e2e".into(),
        owner_type: "agent".into(),
        due: None,
        due_time: None,
        reviewer: None,
        depends_on: vec![],
        gate: vec![],
        tags: vec![],
        ask_type: None,
        ask_question: None,
        ask_unblocks: None,
        ask_actor: None,
        source: Some("self-driving-e2e".into()),
        requested_by: None,
        callback_session: None,
        callback_prompt: None,
    }
}

/// Goals, planning nodes and task budgets were retired with their tables
/// (migration 0180, primitive integrity audit 2026-11-26 step 2). Through the
/// real router and its policy middleware every old path answers 410 with a
/// pointer, and a handoff can no longer be scoped to a planning node.
#[tokio::test]
async fn retired_planning_routes_answer_gone_through_the_real_router() {
    let rig = rig();
    for (method, path, body) in [
        ("POST", "/api/harness/goals", Some(json!({"id": "goal-1", "objective": "x"}))),
        ("GET", "/api/harness/goals/goal-1", None),
        ("PUT", "/api/harness/goals/goal-1", Some(json!({"objective": "y"}))),
        ("GET", "/api/harness/goals/goal-1/nodes", None),
        ("POST", "/api/harness/goals/goal-1/nodes", Some(json!({"owner": "w"}))),
        ("GET", "/api/harness/planning-nodes/pn-1", None),
        ("GET", "/api/harness/planning-nodes/pn-1/plan", None),
        ("PUT", "/api/harness/planning-nodes/pn-1/plan", Some(json!({"body": "b"}))),
        ("GET", "/api/harness/budgets/AMUX-1", None),
        ("PUT", "/api/harness/budgets/AMUX-1", Some(json!({"max_attempts": 2}))),
    ] {
        let (status, _, value) = send(&rig.app, method, path, body, &[("x-amux-session", "root-planner")]).await;
        assert_eq!(status, StatusCode::GONE, "{method} {path}: {value}");
        assert_eq!(value["code"], "route_retired", "{method} {path}: {value}");
    }
    let (status, _, value) = send(
        &rig.app,
        "POST",
        "/api/harness/handoffs/AMUX-1",
        Some(json!({
            "objective": "hand it back",
            "next_action": "review",
            "receiver": "root-planner",
            "planning_scope_id": "pn-1"
        })),
        &[("x-amux-session", "worker-a")],
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "{value}");
    assert_eq!(value["code"], "planning_scope_retired", "{value}");
    let n: i64 = rig.store.read().unwrap()
        .query_row("SELECT COUNT(*) FROM _amux_handoffs", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "a refused handoff writes nothing");
}

#[tokio::test]
async fn workers_create_only_on_their_own_board_and_link_peers_explicitly() {
    let rig = rig();
    // AF-922: `depends_on` across boards is now refused at create time
    // (cross_board_dependency_forbidden, b18789f9) -- exactly the shape this
    // test's own name argues for ("link peers explicitly" via reviewer/
    // shepherd below, not via depends_on). The prerequisite this test's
    // depends_on edge points at is now owned by the SAME worker as the card
    // that depends on it, so that edge stays legal; the peer relationship the
    // test demonstrates is carried by reviewer/shepherd instead.
    let (status, _, dependency) = send(
        &rig.app,
        "POST",
        "/api/board",
        Some(json!({
            "title": "Own-board prerequisite",
            "session": "worker-a",
            "status": "backlog",
            "type": "chore"
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dependency}");
    let dependency_id = dependency["id"].as_str().unwrap().to_string();

    let (status, _, refusal) = send(
        &rig.app,
        "POST",
        "/api/board",
        Some(json!({
            "title": "Illegally placed peer work",
            "session": "worker-b",
            "status": "backlog"
        })),
        &[("x-amux-worker", "worker-a")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refusal}");
    assert_eq!(refusal["code"], "cross_board_create_forbidden");

    let (status, _, own) = send(
        &rig.app,
        "POST",
        "/api/board",
        Some(json!({
            "title": "Own work with peer relationships",
            "session": "worker-a",
            "status": "backlog",
            "type": "chore",
            "reviewer": "worker-b",
            "shepherd": "worker-c",
            "depends_on": [dependency_id]
        })),
        &[("x-amux-worker", "worker-a")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{own}");
    assert_eq!(own["session"], "worker-a");
    assert_eq!(own["reviewer"], "worker-b");
    assert_eq!(own["shepherd"], "worker-c");
    assert_eq!(own["depends_on"], json!([dependency_id]));

    let (_, _, all) = send(&rig.app, "GET", "/api/board?all=1", None, &[]).await;
    assert!(!all.as_array().unwrap().iter().any(|row| {
        row["title"] == "Illegally placed peer work" && row["session"] == "worker-b"
    }));
}

fn fixture_script(dir: &Path, long_line: &str) -> PathBuf {
    let path = dir.join("fake-claude");
    let mut file = std::fs::File::create(&path).unwrap();
    writeln!(file, "#!/bin/sh").unwrap();
    writeln!(
        file,
        "printf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"trace-session\"}}'"
    )
    .unwrap();
    writeln!(file, "printf '%s\\n' '{}'", long_line).unwrap();
    writeln!(
        file,
        "printf '%s\\n' '{{\"type\":\"user\",\"api_key\":\"fixture-secret\"}}'"
    )
    .unwrap();
    writeln!(
        file,
        "printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\",\"session_id\":\"trace-session\"}}'"
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

#[tokio::test]
async fn provider_subprocess_persists_correlated_redacted_bounded_turn_traces() {
    let rig = rig();
    let fixture_dir = tempfile::tempdir().unwrap();
    let long_line = format!(
        "{{\"type\":\"future_event\",\"blob\":\"{}\"}}",
        "x".repeat(70_000)
    );
    let script = fixture_script(fixture_dir.path(), &long_line);
    let worker = WorkerId::from_ulid(ulid::Ulid::new());
    let worker_for_write = worker.clone();
    let command = CommandId::from_ulid(ulid::Ulid::new());
    let command_for_write = command.clone();
    let task = Arc::new(std::sync::Mutex::new(None));
    let task_for_write = task.clone();
    rig.store
        .write(move |conn| {
            let row = create_issue(conn, &issue("trace a real provider process", None), Utc::now().timestamp())?;
            let internal = internal_id(&row.id);
            conn.execute(
                "INSERT INTO _amux_workers (id,display_name,created_at,updated_at)
                 VALUES (?1,'trace-worker',?2,?2)",
                rusqlite::params![worker_for_write.as_str(), Utc::now().to_rfc3339()],
            )?;
            conn.execute(
                "INSERT INTO _amux_sessions
                 (id,worker_id,backend,backend_ref,started_at)
                 VALUES ('session-trace',?1,'local','fixture-process',?2)",
                rusqlite::params![worker_for_write.as_str(), Utc::now().to_rfc3339()],
            )?;
            commands::enqueue(
                conn,
                command_for_write.clone(),
                &worker_for_write,
                &WorkerCommand::ExecuteTask(internal.clone()),
                "trace-assignment",
                &DeliveryTiming::Immediate,
                None,
                Utc::now(),
            )?;
            commands::transition(conn, &command_for_write, CommandTransition::Dispatch, 3)?;
            conn.execute(
                "INSERT INTO _amux_turn_traces
                 (id,worker_id,turn_id,kind,content,content_sha256,truncated,redactions,created_at)
                 VALUES ('expired-trace',?1,'expired-turn','prompt','old','hash',0,0,'2000-01-01T00:00:00Z')",
                [worker_for_write.as_str()],
            )?;
            *task_for_write.lock().unwrap() = Some(internal.to_string());
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .unwrap();

    let protocol = Arc::new(StructuredCliProtocol::new());
    protocol.register(
        worker.clone(),
        WorkerConfig {
            provider: CliProvider::ClaudeCode,
            cwd: fixture_dir.path().to_path_buf(),
            binary: Some(script),
            model: Some("claude-sonnet".into()),
            conversation: None,
        },
    );
    let protocol_trait: Arc<dyn AgentProtocol> = protocol.clone();
    let processor = spawn_event_processor(rig.store.clone(), protocol_trait, worker.clone());
    protocol
        .send_prompt(
            &worker,
            Prompt {
                text: "Perform the trace task; password=prompt-secret".into(),
                idempotency_key: "trace-turn-1".into(),
            },
        )
        .await
        .unwrap();

    let turn_id = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let found = {
                let conn = rig.store.read().unwrap();
                let count: u64 = conn
                    .query_row("SELECT COUNT(*) FROM _amux_turn_traces", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                if count >= 5 {
                    conn.query_row(
                        "SELECT turn_id FROM _amux_turn_traces
                         WHERE turn_id!='expired-turn' ORDER BY created_at DESC LIMIT 1",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .ok()
                } else {
                    None
                }
            };
            if let Some(turn_id) = found {
                break turn_id;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("trace events did not reach SQLite");

    let (status, _, traces) = send(
        &rig.app,
        "GET",
        &format!("/api/harness/traces/{turn_id}"),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{traces}");
    assert_eq!(traces["measured"], true);
    assert!(traces["n_considered"].as_u64().unwrap() >= 5);
    let rows = traces["items"].as_array().unwrap();
    assert!(rows.iter().any(|row| row["kind"] == "prompt"));
    assert!(rows.iter().any(|row| row["truncated"] == true));
    assert!(rows
        .iter()
        .all(|row| row["task_id"] == task.lock().unwrap().as_ref().unwrap().as_str()));
    assert!(rows.iter().all(|row| row["session_id"] == "session-trace"));
    let stored = traces.to_string();
    assert!(!stored.contains("prompt-secret"), "{stored}");
    assert!(!stored.contains("fixture-secret"), "{stored}");
    assert!(stored.contains("[REDACTED]"), "{stored}");
    let conn = rig.store.read().unwrap();
    let expired: u64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _amux_turn_traces WHERE id='expired-trace'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(expired, 0, "retention sweep must remove expired evidence");
    processor.abort();
}

#[tokio::test]
async fn measured_queue_pressure_changes_the_real_runtime_wip_and_health() {
    let rig = rig();
    let protocol = Arc::new(MockProtocol::new());
    let (status, _, worker_json) = send(
        &rig.app,
        "POST",
        "/api/workers",
        Some(json!({"display_name": "wip-worker", "cwd": "/tmp"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{worker_json}");
    let worker = WorkerId::parse(worker_json["id"].as_str().unwrap()).unwrap();
    protocol.register(worker, AgentState::Idle);
    for title in [
        "queued one",
        "queued two",
        "queued three",
        "queued four",
        "queued five",
        "queued six",
    ] {
        let (status, _, body) = send(
            &rig.app,
            "POST",
            "/api/board",
            Some(json!({"title": title, "session": "wip-worker"})),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    rig.store
        .write(|conn| {
            conn.execute(
                "UPDATE issues SET created=?1 WHERE session='wip-worker'",
                [Utc::now().timestamp() - 120],
            )?;
            conn.execute(
                "INSERT INTO _amux_work_metrics
                 (id,kind,source,duration_ms,detail,created_at)
                 VALUES('expired-metric','lock_wait','runtime',1,'expired fixture',
                        '2000-01-01T00:00:00Z')",
                [],
            )?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .unwrap();
    let (status, _, configured) = send(
        &rig.app,
        "PUT",
        "/api/harness/adaptive-wip",
        Some(json!({
            "mode": "active",
            "current_limit": 1,
            "min_limit": 1,
            "max_limit": 3
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{configured}");
    let runtime = Runtime {
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
        protocol: Some(protocol),
        pickup_unowned: false,
        resume_stagger_secs: 5,
    };
    runtime.tick_once(false).await.unwrap();
    let (_, _, expanded) = send(&rig.app, "GET", "/api/harness/adaptive-wip", None, &[]).await;
    assert_eq!(expanded["state"]["current_limit"], 2);
    assert_eq!(expanded["state"]["recommended"], 2);
    assert_eq!(expanded["state"]["sample_size"], 6);
    let conn = rig.store.read().unwrap();
    let leases: u64 = conn
        .query_row("SELECT COUNT(*) FROM _amux_leases", [], |row| row.get(0))
        .unwrap();
    assert_eq!(leases, 2, "the runtime must consume the measured WIP limit");
    let expired_metrics: u64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _amux_work_metrics WHERE id='expired-metric'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        expired_metrics, 0,
        "runtime observation must enforce metric retention"
    );
    drop(conn);

    let (_, _, health) = send(&rig.app, "GET", "/api/harness/health?days=1", None, &[]).await;
    assert_eq!(health["throughput"]["measured"], true);
    assert_eq!(health["throughput"]["waits"]["queue"]["n_considered"], 6);
    assert_eq!(health["throughput"]["adaptive_wip"]["current_limit"], 2);

    // The diagnostic ingestion endpoint is intentionally excluded from the
    // active controller: an API caller cannot manufacture capacity changes.
    let (status, _, conflict) = send(
        &rig.app,
        "POST",
        "/api/harness/work-metrics",
        Some(json!({"kind": "conflict", "detail": "real merge conflict"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{conflict}");
    assert_eq!(conflict["adaptive_wip"]["current_limit"], 2);

    // Hold the actual single-writer lane long enough for the runtime's own
    // wait measurement to cross the controller's contention band.
    let store = rig.store.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let blocker = tokio::task::spawn_blocking(move || {
        store.write(move |_conn| {
            let _ = started_tx.send(());
            std::thread::sleep(Duration::from_millis(1_500));
            Ok(WriteOutcome {
                applied: false,
                events: vec![],
            })
        })
    });
    started_rx.await.unwrap();
    runtime.tick_once(false).await.unwrap();
    blocker.await.unwrap().unwrap();
    let (_, _, contracted) = send(&rig.app, "GET", "/api/harness/adaptive-wip", None, &[]).await;
    assert_eq!(contracted["state"]["current_limit"], 1);
    assert!(contracted["state"]["reason"]
        .as_str()
        .unwrap()
        .contains("contention"));
    let (_, _, health) = send(&rig.app, "GET", "/api/harness/health?days=1", None, &[]).await;
    assert_eq!(health["throughput"]["waits"]["lock"]["n_considered"], 1);
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[tokio::test]
async fn reconciliation_runs_real_gates_in_a_detached_worktree_and_promotes_only_green() {
    let rig = rig();
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-b", "main"]);
    git(
        repo.path(),
        &["config", "user.email", "amux-e2e@example.invalid"],
    );
    git(repo.path(), &["config", "user.name", "AMUX E2E"]);
    std::fs::write(repo.path().join("README.md"), "baseline\n").unwrap();
    git(repo.path(), &["add", "README.md"]);
    git(repo.path(), &["commit", "-m", "baseline"]);
    let main_before = git(repo.path(), &["rev-parse", "main"]);
    git(repo.path(), &["checkout", "-b", "candidate"]);
    std::fs::write(repo.path().join("artifact.txt"), "ready\n").unwrap();
    git(repo.path(), &["add", "artifact.txt"]);
    git(repo.path(), &["commit", "-m", "candidate"]);
    let candidate = git(repo.path(), &["rev-parse", "HEAD"]);
    git(repo.path(), &["checkout", "main"]);

    let green_request = json!({
        "repo_path": repo.path(),
        "candidate_sha": candidate,
        "gates": [{
            "label": "artifact-is-real",
            "program": "/bin/sh",
            "args": ["-c", "test -f artifact.txt && grep -q ready artifact.txt && printf 'password=gate-secret\\n'"],
            "timeout_secs": 10
        }]
    });
    let (status, _, approval_required) = send(
        &rig.app,
        "POST",
        "/api/harness/reconciliations",
        Some(green_request.clone()),
        &[("x-amux-session", "human-verifier")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::PRECONDITION_REQUIRED,
        "{approval_required}"
    );
    assert_eq!(
        approval_required["decision"]["rule_id"],
        "builtin-reconciliation-exact-approval"
    );
    let approval = approve_exact(
        &rig.app,
        "human-verifier",
        "/api/harness/reconciliations",
        Some(&green_request),
    )
    .await;
    let (status, _, green) = send(
        &rig.app,
        "POST",
        "/api/harness/reconciliations",
        Some(green_request.clone()),
        &[
            ("x-amux-session", "human-verifier"),
            ("x-amux-approval", approval.as_str()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{green}");
    assert_eq!(green["status"], "green");
    assert_eq!(green["candidate_sha"], candidate);
    assert_eq!(green["gate_results"][0]["success"], true);
    assert!(green["gate_results"][0]["output"]
        .as_str()
        .unwrap()
        .contains("[REDACTED]"));
    assert!(!green.to_string().contains("gate-secret"));
    let (status, _, replay_refusal) = send(
        &rig.app,
        "POST",
        "/api/harness/reconciliations",
        Some(green_request),
        &[
            ("x-amux-session", "human-verifier"),
            ("x-amux-approval", approval.as_str()),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::PRECONDITION_REQUIRED,
        "{replay_refusal}"
    );
    assert_eq!(
        replay_refusal["decision"]["rule_id"],
        "builtin-reconciliation-exact-approval"
    );
    let reconciliation_count: u64 = rig
        .store
        .read()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM _amux_reconciliations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        reconciliation_count, 1,
        "a replayed approval must not run or persist a second reconciliation"
    );
    let reconciliation_id = green["id"].as_str().unwrap();
    let worktrees = git(repo.path(), &["worktree", "list", "--porcelain"]);
    assert_eq!(
        worktrees
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
        1,
        "candidate worktree must be removed after gates"
    );

    let (status, _, measured) = send(
        &rig.app,
        "GET",
        &format!("/api/harness/reconciliations/{reconciliation_id}"),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{measured}");
    assert_eq!(measured["measured"], true);
    assert_eq!(measured["item"]["status"], "green");

    let promote_path = format!("/api/harness/reconciliations/{reconciliation_id}/promote");
    let promote_approval = approve_exact(&rig.app, "human-verifier", &promote_path, None).await;
    let (status, _, promoted) = send(
        &rig.app,
        "POST",
        &promote_path,
        None,
        &[
            ("x-amux-session", "human-verifier"),
            ("x-amux-approval", promote_approval.as_str()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{promoted}");
    assert_eq!(promoted["pushed"], false);
    assert_eq!(
        git(
            repo.path(),
            &["rev-parse", "refs/heads/amux/last-known-green"]
        ),
        candidate
    );
    assert_eq!(git(repo.path(), &["rev-parse", "main"]), main_before);

    let failed_request = json!({
        "repo_path": repo.path(),
        "candidate_sha": candidate,
        "gates": [{
            "label": "real-failure",
            "program": "/bin/sh",
            "args": ["-c", "exit 7"],
            "timeout_secs": 10
        }]
    });
    let failed_approval = approve_exact(
        &rig.app,
        "human-verifier",
        "/api/harness/reconciliations",
        Some(&failed_request),
    )
    .await;
    let (status, _, failed) = send(
        &rig.app,
        "POST",
        "/api/harness/reconciliations",
        Some(failed_request),
        &[
            ("x-amux-session", "human-verifier"),
            ("x-amux-approval", failed_approval.as_str()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{failed}");
    assert_eq!(failed["status"], "failed");
    let failed_id = failed["id"].as_str().unwrap();
    let failed_promote_path = format!("/api/harness/reconciliations/{failed_id}/promote");
    let failed_promote_approval =
        approve_exact(&rig.app, "human-verifier", &failed_promote_path, None).await;
    let (status, _, refusal) = send(
        &rig.app,
        "POST",
        &failed_promote_path,
        None,
        &[
            ("x-amux-session", "human-verifier"),
            ("x-amux-approval", failed_promote_approval.as_str()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refusal}");

    let timeout_request = json!({
        "repo_path": repo.path(),
        "candidate_sha": candidate,
        "gates": [{
            "label": "bounded-hang",
            "program": "/bin/sleep",
            "args": ["5"],
            "timeout_secs": 1
        }]
    });
    let timeout_approval = approve_exact(
        &rig.app,
        "human-verifier",
        "/api/harness/reconciliations",
        Some(&timeout_request),
    )
    .await;
    let timeout_started = std::time::Instant::now();
    let (status, _, timed_out) = send(
        &rig.app,
        "POST",
        "/api/harness/reconciliations",
        Some(timeout_request),
        &[
            ("x-amux-session", "human-verifier"),
            ("x-amux-approval", timeout_approval.as_str()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{timed_out}");
    assert_eq!(timed_out["status"], "failed");
    assert_eq!(timed_out["gate_results"][0]["timed_out"], true);
    assert!(
        timeout_started.elapsed() < Duration::from_secs(3),
        "the gate timeout did not bound the real subprocess"
    );
    let conn = rig.store.read().unwrap();
    let snapshot: (String, String) = conn
        .query_row(
            "SELECT candidate_sha,reconciliation_id FROM _amux_green_snapshots",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(snapshot, (candidate, reconciliation_id.to_string()));
}
