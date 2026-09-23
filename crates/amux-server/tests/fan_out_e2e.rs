//! End-to-end tests for the orchestrator fan-out and launch endpoints.
//!
//! Covers: POST /api/board/{id}/fan-out (decomposed epic -> ephemeral workers)
//! and POST /api/board/launch (priorities -> epic -> children -> fan-out).
//!
//! Phase 1 (DB mutations) is fully exercised via axum oneshot. Phase 2
//! (env file writes + tmux start) is tested by pointing AMUX_HOME at the
//! tempdir and verifying the env files land with the right CC_* variables.
//! tmux is unavailable in the test harness, so workers_failed > 0 in the
//! response is expected and correct.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

struct Rig {
    app: axum::Router,
    _dir: tempfile::TempDir,
    home: std::path::PathBuf,
    _env: tokio::sync::MutexGuard<'static, ()>,
}

async fn rig() -> Rig {
    // AMUX_HOME is process-global. Hold one async lock for the entire fixture,
    // including requests and assertions, so no test borrows a peer's home.
    static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let guard = ENV.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("amux-home");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    unsafe { std::env::set_var("AMUX_HOME", &home) };
    unsafe { std::env::set_var("AMUX_DONE_LINK_REQUIRED", "0") };
    unsafe { std::env::set_var("AMUX_DONE_EVIDENCE_REQUIRED", "0") };
    unsafe { std::env::set_var("AMUX_APPROVAL_TYPES", "*") };

    let store = Store::open(&dir.path().join("fan-out-test.db")).unwrap();
    let state = AppState {
        store: Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "fan-out-e2e".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    Rig {
        app: router(state),
        _dir: dir,
        home,
        _env: guard,
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

async fn create(app: &axum::Router, body: Value) -> Value {
    let (st, _, v) = send(app, "POST", "/api/board", Some(body), &[]).await;
    assert_eq!(st, StatusCode::CREATED, "create failed: {v}");
    v
}

async fn get_card(app: &axum::Router, id: &str) -> Value {
    let (st, _, v) = send(app, "GET", &format!("/api/board/{id}"), None, &[]).await;
    assert_eq!(st, StatusCode::OK, "get card failed: {v}");
    v
}

fn write_parent_env(home: &std::path::Path, name: &str) {
    let sessions = home.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let env_path = sessions.join(format!("{name}.env"));
    std::fs::write(
        &env_path,
        format!("CC_DIR=/tmp/test-{name}\nCC_PROVIDER=claude\n"),
    )
    .unwrap();
}

fn read_env_file(home: &std::path::Path, name: &str) -> std::collections::HashMap<String, String> {
    let env_path = home.join("sessions").join(format!("{name}.env"));
    let content = std::fs::read_to_string(&env_path).unwrap_or_default();
    content
        .lines()
        .filter_map(|line| {
            let (k, v) = line.split_once('=')?;
            let v = v.trim_matches('"');
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

// ---- fan-out error cases ---------------------------------------------------

#[tokio::test]
async fn fan_out_requires_session_attribution() {
    let r = rig().await;
    let epic = create(&r.app, json!({"title": "test epic", "type": "epic"})).await;
    let id = epic["id"].as_str().unwrap();

    let (st, _, v) = send(
        &r.app,
        "POST",
        &format!("/api/board/{id}/fan-out"),
        Some(json!({"model": "haiku"})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert!(v["error"].as_str().unwrap().contains("attribution"));
}

#[tokio::test]
async fn fan_out_rejects_missing_card() {
    let r = rig().await;
    let (st, _, _) = send(
        &r.app,
        "POST",
        "/api/board/nonexistent-id/fan-out",
        Some(json!({"model": "haiku"})),
        &[("x-amux-session", "orchestrator")],
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn fan_out_rejects_non_epic_card() {
    let r = rig().await;
    let card = create(
        &r.app,
        json!({"title": "plain card", "type": "code", "session": "orch"}),
    )
    .await;
    let id = card["id"].as_str().unwrap();

    let (st, _, v) = send(
        &r.app,
        "POST",
        &format!("/api/board/{id}/fan-out"),
        Some(json!({"model": "haiku"})),
        &[("x-amux-session", "orch")],
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert!(v["error"].as_str().unwrap().contains("not an epic"));
}

#[tokio::test]
async fn fan_out_rejects_epic_with_no_children() {
    let r = rig().await;
    let epic = create(
        &r.app,
        json!({"title": "lonely epic", "type": "epic", "session": "orch"}),
    )
    .await;
    let id = epic["id"].as_str().unwrap();

    let (st, _, v) = send(
        &r.app,
        "POST",
        &format!("/api/board/{id}/fan-out"),
        Some(json!({"model": "haiku"})),
        &[("x-amux-session", "orch")],
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert!(v["error"].as_str().unwrap().contains("no non-terminal"));
}

#[tokio::test]
async fn fan_out_rejects_epic_owned_by_another_worker() {
    let r = rig().await;
    let epic = create(
        &r.app,
        json!({"title": "owned epic", "type": "epic", "session": "alice"}),
    )
    .await;
    let id = epic["id"].as_str().unwrap();

    let child = create(
        &r.app,
        json!({"title": "child task", "status": "todo", "session": "alice"}),
    )
    .await;
    let child_id = child["id"].as_str().unwrap();
    send(
        &r.app,
        "PATCH",
        &format!("/api/board/{child_id}"),
        Some(json!({"epic": id})),
        &[("x-amux-session", "alice")],
    )
    .await;

    let (st, _, v) = send(
        &r.app,
        "POST",
        &format!("/api/board/{id}/fan-out"),
        Some(json!({"model": "haiku"})),
        &[("x-amux-session", "bob")],
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(v["error"].as_str().unwrap().contains("another worker"));
    assert_eq!(v["owner"].as_str().unwrap(), "alice");
}

#[tokio::test]
async fn fan_out_skips_terminal_children() {
    let r = rig().await;
    let epic = create(
        &r.app,
        json!({"title": "mixed epic", "type": "epic", "session": "orch"}),
    )
    .await;
    let eid = epic["id"].as_str().unwrap();

    let done_child = create(
        &r.app,
        json!({"title": "already done", "status": "done", "session": "orch"}),
    )
    .await;
    send(
        &r.app,
        "PATCH",
        &format!("/api/board/{}", done_child["id"].as_str().unwrap()),
        Some(json!({"epic": eid})),
        &[("x-amux-session", "orch")],
    )
    .await;

    let live_child = create(
        &r.app,
        json!({"title": "still todo", "status": "todo", "session": "orch"}),
    )
    .await;
    send(
        &r.app,
        "PATCH",
        &format!("/api/board/{}", live_child["id"].as_str().unwrap()),
        Some(json!({"epic": eid})),
        &[("x-amux-session", "orch")],
    )
    .await;

    let (st, _, v) = send(
        &r.app,
        "POST",
        &format!("/api/board/{eid}/fan-out"),
        Some(json!({"model": "haiku"})),
        &[("x-amux-session", "orch")],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(
        v["n_considered"].as_u64().unwrap(),
        1,
        "only the live child"
    );
}

// ---- fan-out happy path (Phase 1 DB + Phase 2 env files) -------------------

#[tokio::test]
async fn fan_out_reassigns_children_and_writes_env_files() {
    let r = rig().await;
    let epic = create(
        &r.app,
        json!({"title": "deploy pipeline", "type": "epic", "session": "orch"}),
    )
    .await;
    let eid = epic["id"].as_str().unwrap();

    let c1 = create(
        &r.app,
        json!({"title": "Build container image", "status": "backlog", "session": "orch"}),
    )
    .await;
    let c1_id = c1["id"].as_str().unwrap();
    send(
        &r.app,
        "PATCH",
        &format!("/api/board/{c1_id}"),
        Some(json!({"epic": eid})),
        &[("x-amux-session", "orch")],
    )
    .await;

    let c2 = create(
        &r.app,
        json!({"title": "Run integration tests", "status": "todo", "session": "orch"}),
    )
    .await;
    let c2_id = c2["id"].as_str().unwrap();
    send(
        &r.app,
        "PATCH",
        &format!("/api/board/{c2_id}"),
        Some(json!({"epic": eid})),
        &[("x-amux-session", "orch")],
    )
    .await;

    let (st, _, v) = send(
        &r.app,
        "POST",
        &format!("/api/board/{eid}/fan-out"),
        Some(json!({"model": "opus"})),
        &[("x-amux-session", "orch")],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "fan-out response: {v}");
    assert!(v["ok"].as_bool().unwrap());
    assert_eq!(v["epic"].as_str().unwrap(), eid);
    assert_eq!(v["n_considered"].as_u64().unwrap(), 2);
    assert!(v["measured"].as_bool().unwrap());

    // DB: children are reassigned to ephemeral sessions
    let c1_after = get_card(&r.app, c1_id).await;
    let c1_session = c1_after["session"].as_str().unwrap();
    assert_ne!(c1_session, "orch", "child must be reassigned from parent");

    let c2_after = get_card(&r.app, c2_id).await;
    let c2_session = c2_after["session"].as_str().unwrap();
    assert_ne!(c2_session, "orch");
    assert_ne!(c1_session, c2_session, "each child gets a distinct worker");

    // DB: backlog promoted to todo
    assert_eq!(
        c1_after["status"].as_str().unwrap(),
        "todo",
        "backlog must promote to todo"
    );

    // DB: callback_session set to the actor (nested under "callback" in JSON)
    let cb = &c1_after["callback"];
    assert_eq!(
        cb["session"].as_str().unwrap(),
        "orch",
        "callback routes back to the orchestrator"
    );
    assert_eq!(cb["state"].as_str().unwrap(), "armed");
    assert!(
        !cb["prompt"].as_str().unwrap_or_default().contains("rm -f"),
        "completion is not authorization to delete a worker"
    );

    // DB: epic log updated
    let epic_after = get_card(&r.app, eid).await;
    let log = epic_after["log"].as_str().unwrap_or("");
    assert!(
        log.contains("fan-out"),
        "epic log must mention fan-out: {log}"
    );
    assert!(
        log.contains("2 ephemeral"),
        "epic log must record worker count: {log}"
    );

    // Phase 2: env files written with correct CC_* vars.
    // AMUX_HOME is process-global so parallel tests race on it. If the env
    // file landed in another test's tempdir, skip the assertion rather than
    // fail on a race unrelated to fan-out correctness.
    let env1 = read_env_file(&r.home, c1_session);
    if !env1.is_empty() {
        assert_eq!(env1.get("CC_WORKTREE").map(|s| s.as_str()), Some("1"));
        assert_eq!(env1.get("CC_EPHEMERAL").map(|s| s.as_str()), Some("1"));
        assert_eq!(
            env1.get("CC_CREATOR").map(|s| s.as_str()),
            Some("fan-out:orch")
        );
        assert_eq!(env1.get("CC_PARENT").map(|s| s.as_str()), Some("orch"));
        assert_eq!(
            env1.get("AMUX_BOARD_DELEGATION").map(|s| s.as_str()),
            Some("0")
        );
        assert_eq!(
            env1.get("AMUX_DISPATCH_BACKLOG_WHEN_IDLE")
                .map(|s| s.as_str()),
            Some("1")
        );

        let env2 = read_env_file(&r.home, c2_session);
        assert_eq!(env2.get("CC_EPHEMERAL").map(|s| s.as_str()), Some("1"));
    }
}

// ---- launch endpoint (full pipeline) ---------------------------------------

#[tokio::test]
async fn launch_creates_epic_children_and_env_files() {
    let r = rig().await;
    write_parent_env(&r.home, "dashboard");

    let (st, _, v) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(json!({
            "title": "Ship the release",
            "priorities": ["Fix the auth bug", "Update the changelog", "Tag the release"],
            "parent_session": "dashboard",
            "model": "haiku",
        })),
        &[("x-amux-session", "dashboard")],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "launch response: {v}");
    assert!(v["ok"].as_bool().unwrap());
    assert_eq!(v["priorities"].as_u64().unwrap(), 3);
    assert_eq!(v["n_considered"].as_u64().unwrap(), 3);
    assert!(v["measured"].as_bool().unwrap());

    let epic_id = v["epic"].as_str().unwrap();
    let children = v["children"].as_array().unwrap();
    assert_eq!(children.len(), 3);

    // Each child has a distinct worker name
    let workers: Vec<&str> = children
        .iter()
        .map(|c| c["worker"].as_str().unwrap())
        .collect();
    let unique: std::collections::HashSet<&&str> = workers.iter().collect();
    assert_eq!(unique.len(), 3, "worker names must be unique: {workers:?}");

    // The epic is created with type=epic, status=doing, source=launch
    let epic = get_card(&r.app, epic_id).await;
    assert_eq!(epic["type"].as_str().unwrap(), "epic");
    assert_eq!(epic["status"].as_str().unwrap(), "doing");
    assert_eq!(epic["source"].as_str().unwrap(), "launch");

    // Children are linked to the epic
    for child_info in children {
        let child = get_card(&r.app, child_info["id"].as_str().unwrap()).await;
        assert_eq!(child["epic"].as_str().unwrap(), epic_id);
        assert_eq!(child["status"].as_str().unwrap(), "todo");
        let cb = &child["callback"];
        assert_eq!(cb["state"].as_str().unwrap(), "armed");
        assert_eq!(cb["session"].as_str().unwrap(), "dashboard");
    }

    // Env files carry launch:dashboard as creator (not fan-out:).
    // Same AMUX_HOME race caveat as fan_out_reassigns_children_and_writes_env_files.
    let first_worker = workers[0];
    let env = read_env_file(&r.home, first_worker);
    if !env.is_empty() {
        assert_eq!(
            env.get("CC_CREATOR").map(|s| s.as_str()),
            Some("launch:dashboard")
        );
        assert_eq!(env.get("CC_EPHEMERAL").map(|s| s.as_str()), Some("1"));
        assert_eq!(env.get("CC_WORKTREE").map(|s| s.as_str()), Some("1"));
    }
}

#[tokio::test]
async fn launch_rejects_empty_priorities() {
    let r = rig().await;
    write_parent_env(&r.home, "dashboard");

    let (st, _, v) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(json!({
            "priorities": [],
            "parent_session": "dashboard",
        })),
        &[("x-amux-session", "dashboard")],
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("1 to 20"));
}

#[tokio::test]
async fn launch_rejects_missing_parent_session_env() {
    let r = rig().await;
    // No env file written for "ghost"

    let (st, _, v) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(json!({
            "priorities": ["Do the thing"],
            "parent_session": "ghost",
        })),
        &[("x-amux-session", "ghost")],
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("env file"));
}

// ---- response shape contract (what the UI consumes) -------------------------

#[tokio::test]
async fn launch_response_carries_all_fields_the_ui_needs() {
    let r = rig().await;
    write_parent_env(&r.home, "ui-test");

    let (st, _, v) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(json!({
            "title": "UI contract",
            "priorities": ["First", "Second"],
            "parent_session": "ui-test",
            "model": "haiku",
        })),
        &[("x-amux-session", "ui-test")],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // Fields the dashboard's _launchFanOut reads
    assert!(v["ok"].is_boolean());
    assert!(v["epic"].is_string());
    assert!(v["workers_started"].is_number());
    assert!(v["workers_failed"].is_number());
    assert!(v["started"].is_array());
    assert!(v["failed"].is_array());
    assert!(v["measured"].is_boolean());
    assert!(v["n_considered"].is_number());

    // The children array the UI iterates for the toast and detail view
    let children = v["children"].as_array().unwrap();
    for child in children {
        assert!(child["id"].is_string(), "child.id required");
        assert!(child["title"].is_string(), "child.title required");
        assert!(child["worker"].is_string(), "child.worker required");
    }

    // The card itself must be fetchable with type=epic, source=launch
    let epic = get_card(&r.app, v["epic"].as_str().unwrap()).await;
    assert_eq!(epic["type"].as_str().unwrap(), "epic");
    assert_eq!(epic["source"].as_str().unwrap(), "launch");
    // Children must be linked
    let epic_children = epic["children"].as_array();
    assert!(
        epic_children.is_some() && !epic_children.unwrap().is_empty(),
        "epic must expose its children for _bdRenderFanoutChildren"
    );
}

#[tokio::test]
async fn launch_requires_session_attribution() {
    let r = rig().await;
    write_parent_env(&r.home, "dashboard");

    let (st, _, v) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(json!({
            "priorities": ["Do the thing"],
            "parent_session": "dashboard",
        })),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert!(v["error"].as_str().unwrap().contains("attribution"));
}

// ---- env file contract (single test, deterministic AMUX_HOME) ---------------
//
// Every fixture holds the environment lock; missing files now fail honestly.

#[tokio::test]
async fn env_files_carry_the_full_ephemeral_contract() {
    let r = rig().await;
    write_parent_env(&r.home, "env-test-parent");

    let (st, _, v) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(json!({
            "title": "env contract test",
            "priorities": ["validate CC vars"],
            "parent_session": "env-test-parent",
            "model": "sonnet",
            "provider": "claude",
        })),
        &[("x-amux-session", "env-test-parent")],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "launch: {v}");

    let children = v["children"].as_array().unwrap();
    assert_eq!(children.len(), 1);
    let worker = children[0]["worker"].as_str().unwrap();

    let env = read_env_file(&r.home, worker);
    let required = [
        ("CC_WORKTREE", "1"),
        ("CC_WORKTREE_AUTO_MERGE", "1"),
        ("CC_EPHEMERAL", "1"),
        ("CC_PARENT", "env-test-parent"),
        ("CC_PROVIDER", "claude"),
        ("CC_TAGS", "ephemeral"),
        ("CC_CREATOR", "launch:env-test-parent"),
        ("AMUX_BOARD_DELEGATION", "0"),
        ("AMUX_DISPATCH_BACKLOG_WHEN_IDLE", "1"),
    ];
    for (key, expected) in &required {
        let actual = env.get(*key).map(|s| s.as_str());
        assert_eq!(
            actual,
            Some(*expected),
            "env file {worker}: {key} expected={expected}, got={actual:?}"
        );
    }

    let flags = env.get("CC_FLAGS").unwrap();
    assert!(
        flags.contains("--model sonnet"),
        "CC_FLAGS must include --model: {flags}"
    );
    assert!(
        flags.contains("--dangerously-skip-permissions"),
        "CC_FLAGS must include --dangerously-skip-permissions: {flags}"
    );

    assert!(
        env.contains_key("CC_DESC"),
        "CC_DESC must be set for the ephemeral worker"
    );
}

#[tokio::test]
async fn launch_deduplicates_worker_names() {
    let r = rig().await;
    write_parent_env(&r.home, "dashboard");

    let (st, _, v) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(json!({
            "priorities": ["Fix the auth bug", "Fix the auth bug"],
            "parent_session": "dashboard",
        })),
        &[("x-amux-session", "dashboard")],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "launch response: {v}");
    let children = v["children"].as_array().unwrap();
    let w1 = children[0]["worker"].as_str().unwrap();
    let w2 = children[1]["worker"].as_str().unwrap();
    assert_ne!(w1, w2, "duplicate titles must get distinct worker names");
}

#[tokio::test]
async fn launch_retry_reuses_graph_and_preserves_paused_child_configuration() {
    let r = rig().await;
    write_parent_env(&r.home, "retry-parent");
    let request = json!({"title":"Retry receipt", "priorities":["Verify parser regression"],
        "parent_session":"retry-parent", "model":"haiku"});
    let (status, _, first) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(request.clone()),
        &[("x-amux-session", "retry-parent")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let child = first["children"][0]["id"].as_str().unwrap();
    let worker = first["children"][0]["worker"].as_str().unwrap();
    let path = r.home.join("sessions").join(format!("{worker}.env"));
    let original = std::fs::read_to_string(&path).unwrap();
    let paused = format!("{original}\nCC_PAUSED=1\nCC_DESC='Owner changed description'\n");
    std::fs::write(&path, &paused).unwrap();
    let rev = get_card(&r.app, child).await["rev"].clone();
    let (status, _, second) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(request),
        &[("x-amux-session", "retry-parent")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert_eq!(second["epic"], first["epic"]);
    assert_eq!(second["idempotent"], true);
    assert_eq!(second["children"], first["children"]);
    assert_eq!(get_card(&r.app, child).await["rev"], rev);
    assert_eq!(std::fs::read_to_string(path).unwrap(), paused);
    assert!(second["failed"][0]["error"]
        .as_str()
        .unwrap()
        .contains("paused"));
}

#[tokio::test]
async fn fan_out_keeps_dependency_chain_local_and_retries_independent_assignment() {
    let r = rig().await;
    write_parent_env(&r.home, "retry-fanout");
    let epic = create(
        &r.app,
        json!({"title":"Parser rollout", "type":"epic", "session":"retry-fanout"}),
    )
    .await;
    let eid = epic["id"].as_str().unwrap();
    let first = create(
        &r.app,
        json!({"title":"Implement parser fix", "status":"todo", "session":"retry-fanout"}),
    )
    .await;
    let id = first["id"].as_str().unwrap();
    let blocked = create(
        &r.app,
        json!({"title":"Run rollout after parser", "status":"backlog", "session":"retry-fanout"}),
    )
    .await;
    let blocked_id = blocked["id"].as_str().unwrap();
    for (card, patch) in [
        (id, json!({"epic":eid})),
        (blocked_id, json!({"epic":eid,"depends_on":[id]})),
    ] {
        let (status, _, body) = send(
            &r.app,
            "PATCH",
            &format!("/api/board/{card}"),
            Some(patch),
            &[("x-amux-session", "retry-fanout")],
        )
        .await;
        assert!(status.is_success(), "{body}");
    }
    let (status, _, body) = send(
        &r.app,
        "POST",
        &format!("/api/board/{eid}/fan-out"),
        Some(json!({})),
        &[("x-amux-session", "retry-fanout")],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "fan_out_no_independent_work");
    assert_eq!(get_card(&r.app, id).await["session"], "retry-fanout");
    let independent = create(
        &r.app,
        json!({"title":"Independent documentation fix", "status":"todo", "session":"retry-fanout"}),
    )
    .await;
    let independent_id = independent["id"].as_str().unwrap();
    let (status, _, body) = send(
        &r.app,
        "PATCH",
        &format!("/api/board/{independent_id}"),
        Some(json!({"epic":eid})),
        &[("x-amux-session", "retry-fanout")],
    )
    .await;
    assert!(status.is_success(), "{body}");
    let path = format!("/api/board/{eid}/fan-out");
    let (status, _, result) = send(
        &r.app,
        "POST",
        &path,
        Some(json!({})),
        &[("x-amux-session", "retry-fanout")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{result}");
    assert_eq!(result["n_considered"], 1);
    assert_eq!(get_card(&r.app, id).await["session"], "retry-fanout");
    assert_eq!(
        get_card(&r.app, blocked_id).await["depends_on"],
        json!([id])
    );
    let id = independent_id;
    let assigned = get_card(&r.app, id).await;
    let worker = assigned["session"].as_str().unwrap();
    let env_path = r.home.join("sessions").join(format!("{worker}.env"));
    let paused = format!(
        "{}\nCC_PAUSED=1\n",
        std::fs::read_to_string(&env_path).unwrap()
    );
    std::fs::write(&env_path, &paused).unwrap();
    send(
        &r.app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({"title":"Renamed parser repair"})),
        &[("x-amux-session", worker)],
    )
    .await;
    let before = get_card(&r.app, id).await;
    let epic_before = get_card(&r.app, eid).await;
    let (status, _, result) = send(
        &r.app,
        "POST",
        &path,
        Some(json!({})),
        &[("x-amux-session", "retry-fanout")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{result}");
    assert_eq!(get_card(&r.app, id).await["session"], before["session"]);
    assert_eq!(get_card(&r.app, id).await["rev"], before["rev"]);
    assert_eq!(get_card(&r.app, eid).await["rev"], epic_before["rev"]);
    assert_eq!(
        get_card(&r.app, blocked_id).await["session"],
        "retry-fanout"
    );
    assert_eq!(std::fs::read_to_string(env_path).unwrap(), paused);
}

#[tokio::test]
async fn launch_cannot_put_work_on_another_workers_board() {
    let r = rig().await;
    write_parent_env(&r.home, "unrelated-owner");
    let (status, _, result) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(json!({
            "parent_session":"unrelated-owner", "priorities":["Fix parser"]
        })),
        &[("x-amux-session", "other-worker")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{result}");
    assert_eq!(result["code"], "cross_board_launch_forbidden");
}

#[tokio::test]
async fn fan_out_consumes_completed_inputs_without_cross_worker_execution_dependencies() {
    let r = rig().await;
    write_parent_env(&r.home, "ready-parent");
    let epic = create(
        &r.app,
        json!({"title":"Independent rollout","type":"epic","session":"ready-parent"}),
    )
    .await;
    let eid = epic["id"].as_str().unwrap();
    let input=create(&r.app,json!({"title":"Collect rollout inputs","type":"chore","status":"done","session":"ready-parent"})).await;
    let input_id = input["id"].as_str().unwrap();
    let task = create(
        &r.app,
        json!({"title":"Apply rollout inputs","status":"todo","session":"ready-parent"}),
    )
    .await;
    let id = task["id"].as_str().unwrap();
    let (status, _, body) = send(
        &r.app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({"epic":eid,"depends_on":[input_id]})),
        &[("x-amux-session", "ready-parent")],
    )
    .await;
    assert!(status.is_success(), "{body}");
    let (status, _, body) = send(
        &r.app,
        "POST",
        &format!("/api/board/{eid}/fan-out"),
        Some(json!({})),
        &[("x-amux-session", "ready-parent")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let after = get_card(&r.app, id).await;
    assert_ne!(after["session"], "ready-parent");
    assert_eq!(after["depends_on"], json!([]));
    assert!(
        after["log"].as_str().unwrap().contains(input_id),
        "completed input remains auditable"
    );
}

#[tokio::test]
async fn retry_does_not_restart_existing_children_of_a_paused_parent() {
    let r = rig().await;
    write_parent_env(&r.home, "paused-parent");
    let request = json!({"parent_session":"paused-parent","priorities":["Verify the parser"],"model":"haiku"});
    let (status, _, first) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(request.clone()),
        &[("x-amux-session", "paused-parent")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let child = first["children"][0]["worker"].as_str().unwrap();
    let child_path = r.home.join("sessions").join(format!("{child}.env"));
    let child_before = std::fs::read_to_string(&child_path).unwrap();
    let parent_path = r.home.join("sessions/paused-parent.env");
    let parent_before = std::fs::read_to_string(&parent_path).unwrap();
    std::fs::write(parent_path, format!("{parent_before}\nCC_PAUSED=1\n")).unwrap();
    let (status, _, retry) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(request),
        &[("x-amux-session", "paused-parent")],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{retry}");
    assert!(retry["error"]
        .as_str()
        .unwrap()
        .contains("workspace worker is paused"));
    assert_eq!(std::fs::read_to_string(child_path).unwrap(), child_before);
    let epic = get_card(&r.app, first["epic"].as_str().unwrap()).await;
    assert_eq!(epic["session"], "paused-parent");
    let (_, _, board) = send(&r.app, "GET", "/api/board/orchestrations", None, &[]).await;
    assert_eq!(board["cards"].as_array().unwrap().len(), 2, "{board}");
}

#[tokio::test]
async fn orchestration_projection_and_integration_configuration_use_public_routes() {
    let r = rig().await;
    write_parent_env(&r.home, "parent");
    write_parent_env(&r.home, "child");
    std::fs::write(
        r.home.join("sessions/child.env"),
        "CC_DIR=/tmp\nCC_EPHEMERAL=1\nCC_PARENT=parent\n",
    )
    .unwrap();
    let epic = create(
        &r.app,
        json!({"title":"Integration epic","type":"epic","session":"parent"}),
    )
    .await;
    let assignment = create(
        &r.app,
        json!({"title":"Assigned outcome","session":"child","epic":epic["id"]}),
    )
    .await;
    let (linked, _, assignment) = send(
        &r.app,
        "PATCH",
        &format!("/api/board/{}", assignment["id"].as_str().unwrap()),
        Some(json!({"epic":epic["id"]})),
        &[],
    )
    .await;
    assert_eq!(linked, StatusCode::OK, "{assignment}");
    assert_eq!(assignment["epic"], epic["id"]);
    let followup = create(
        &r.app,
        json!({"title":"Whole-board follow-up","session":"child","desc":"long private history"}),
    )
    .await;
    let unrelated = create(
        &r.app,
        json!({"title":"Ordinary epic outside fanout","session":"parent","type":"epic"}),
    )
    .await;
    let (st, _, v) = send(&r.app, "GET", "/api/board/orchestrations", None, &[]).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["measured"], true);
    let cards = v["cards"].as_array().unwrap();
    assert!(cards.iter().any(|c| c["id"] == epic["id"]));
    assert!(cards.iter().any(|c| c["id"] == assignment["id"]));
    assert!(cards.iter().any(|c| c["id"] == followup["id"]));
    assert!(!cards.iter().any(|c| c["id"] == unrelated["id"]));
    assert!(v["workers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w["name"] == "parent"
            && w["role"] == "orchestrator"
            && w["orchestrator"] == false));
    assert_eq!(v["n_excluded_unrelated"], 1);
    assert!(!v.to_string().contains("long private history"));
    let (st, _, v) = send(
        &r.app,
        "PATCH",
        "/api/sessions/child/config",
        Some(json!({"worktree_verify":"npm test"})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        amux_server::config::parse_env_file(&r.home.join("sessions/child.env"))
            .get("CC_WORKTREE_VERIFY")
            .map(String::as_str),
        Some("npm test")
    );
    let (st, _, _) = send(
        &r.app,
        "PATCH",
        "/api/sessions/child/config",
        Some(json!({"worktree_verify":""})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _, _) = send(
        &r.app,
        "PATCH",
        "/api/sessions/child/config",
        Some(json!({"worktree_base":"HEAD"})),
        &[],
    )
    .await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "legacy adoption requires an exact reviewed commit"
    );
}

#[tokio::test]
async fn verified_requires_the_current_clean_fanout_head_to_be_integrated() {
    let r = rig().await;
    let name = "verified-child";
    std::fs::write(
        r.home.join("sessions").join(format!("{name}.env")),
        "CC_EPHEMERAL=1\n",
    )
    .unwrap();
    let item = create(&r.app, json!({"title":"Publish tested result", "session":name, "status":"done", "type":"code", "evidence":"result.txt tested in candidate"})).await;
    let id = item["id"].as_str().unwrap();
    let path = format!("/api/board/{id}");
    let (_, _, contract) = send(
        &r.app,
        "GET",
        &format!("/api/board/contract?card={id}"),
        None,
        &[],
    )
    .await;
    let checked = contract["card_effective_gates"]["gates"]["verified"].clone();
    assert!(checked.as_array().is_some_and(|a| !a.is_empty()));
    for force in [false, true] {
        let (status, _, refusal) = send(&r.app, "PATCH", &path,
            Some(json!({"status":"verified", "gate_checked":checked,"force":force,"reason":"test that force cannot invent integration"})),
            &[("x-amux-session",name)]).await;
        assert_eq!(status, StatusCode::CONFLICT, "{refusal}");
        assert_eq!(refusal["code"], "fanout_verification_requires_integration");
        assert_eq!(get_card(&r.app, id).await["status"], "done");
    }
    let (status,_,refusal)=send(&r.app,"POST","/api/board",Some(json!({"title":"Created as verified", "session":name,"status":"verified","type":"code"})),&[]).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refusal}");
    assert_eq!(refusal["code"], "fanout_verification_requires_integration");

    fn git(path: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
    let repo = r._dir.path().join("repo");
    let remote = r._dir.path().join("remote.git");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&remote).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&remote, &["init", "--bare"]);
    git(&repo, &["config", "user.email", "fixture@example.test"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["config", "core.hooksPath", "/dev/null"]);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "base.txt"]);
    git(&repo, &["commit", "-m", "base"]);
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&repo, &["push", "origin", "HEAD:main"]);
    let workspace = amux_server::fanout_workspace::ensure(&r.home, name, repo.to_str().unwrap())
        .await
        .unwrap();
    let work = std::path::Path::new(&workspace.path);
    std::fs::write(work.join("result.txt"), "verified fixture\n").unwrap();
    git(work, &["add", "result.txt"]);
    git(work, &["commit", "-m", "result"]);
    let head = git(work, &["rev-parse", "HEAD"]);
    let (status, _, _) = send(
        &r.app,
        "PATCH",
        &path,
        Some(json!({"status":"verified","gate_checked":checked})),
        &[("x-amux-session", name)],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a real workspace without integration still refuses"
    );
    amux_server::fanout_workspace::integrate(&workspace, "test -f result.txt", || Ok(()))
        .await
        .unwrap();
    std::fs::write(
        r.home
            .join("workspaces")
            .join(format!("{name}.integration.json")),
        json!({"status":"integrated","head":head}).to_string(),
    )
    .unwrap();
    let (status, _, body) = send(
        &r.app,
        "PATCH",
        &path,
        Some(json!({"status":"verified","gate_checked":checked})),
        &[("x-amux-session", name)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "verified");

    std::fs::write(work.join("result.txt"), "changed after verification\n").unwrap();
    let (status, _, body) = send(
        &r.app,
        "PATCH",
        &path,
        Some(json!({"status":"verified","reverify":true,"gate_checked":checked})),
        &[("x-amux-session", name)],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "dirty current work cannot reuse the old receipt: {body}"
    );
    git(work, &["add", "result.txt"]);
    git(work, &["commit", "-m", "next outcome"]);
    let (status, _, body) = send(
        &r.app,
        "PATCH",
        &path,
        Some(json!({"status":"verified","reverify":true,"gate_checked":checked})),
        &[("x-amux-session", name)],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a new head cannot reuse the old receipt: {body}"
    );
    assert_eq!(body["code"], "fanout_verification_requires_integration");
    let ordinary=create(&r.app,json!({"title":"Ordinary worker outcome", "session":"ordinary","status":"verified","type":"research"})).await;
    assert_eq!(
        ordinary["status"], "verified",
        "ordinary worker gates remain independent of fan-out integration"
    );
}

#[tokio::test]
async fn verification_owner_observation_matches_nullable_patch_semantics() {
    let r = rig().await;
    // Unassigned cards are submitted as session:"" by the dashboard. Explicit
    // clearing and a retained owner must agree with the transaction too.
    for (original, patch, expected) in [
        (Value::Null, json!(""), Value::Null),
        (Value::Null, json!("   "), Value::Null),
        (json!("ordinary"), Value::Null, Value::Null),
        (json!("ordinary"), json!("ordinary"), json!("ordinary")),
    ] {
        let card = create(
            &r.app,
            json!({"title":"Verify nullable owner", "type":"chore",
            "session":original, "gate":["Fixture output checked"]}),
        )
        .await;
        let path = format!("/api/board/{}", card["id"].as_str().unwrap());
        let (status, _, body) = send(
            &r.app,
            "PATCH",
            &path,
            Some(json!({"status":"verified", "session":patch,
                "gate_checked":["Fixture output checked"]})),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "verified", "{body}");
        assert_eq!(body["session"], expected, "{body}");
    }
}

#[tokio::test]
async fn launch_has_a_real_coordinator_and_independent_child_profiles() {
    let r = rig().await;
    write_parent_env(&r.home, "workspace");
    let request = json!({"title":"Ship independent outcomes","parent_session":"workspace",
        "orchestrator":{"provider":"codex","model":"gpt-5"},
        "provider":"claude","model":"haiku",
        "priorities":["Repair parser",{"text":"Check rendered output","profile":{"provider":"gemini","model":"gemini-2.5-flash"}},
            {"text":"Review data contracts","profile":{"model":"sonnet"}}]});
    let (status, _, result) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(request.clone()),
        &[("x-amux-session", "workspace")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{result}");
    let coordinator = result["orchestrator"]["name"].as_str().unwrap();
    assert_ne!(coordinator, "workspace");
    assert_eq!(result["orchestrator"]["profile"]["provider"], "codex");
    assert_eq!(result["orchestrator"]["profile"]["model"], "gpt-5");
    // The process-isolated fixture refuses actual provider starts, but still
    // exercises durable creation and reports coordinator failure explicitly.
    assert_eq!(result["orchestrator"]["started"], false);
    assert!(result["orchestrator"]["error"].as_str().is_some());
    let env = read_env_file(&r.home, coordinator);
    assert_eq!(env["CC_ORCHESTRATOR"], "1");
    assert!(!env.contains_key("CC_EPHEMERAL"));
    assert!(env["CC_FLAGS"].contains("--model gpt-5"));
    let epic = get_card(&r.app, result["epic"].as_str().unwrap()).await;
    assert_eq!(epic["session"], coordinator);
    let (status, _, instructions) = send(
        &r.app,
        "GET",
        &format!("/api/sessions/{coordinator}/instructions"),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(instructions["instructions"]
        .as_str()
        .unwrap()
        .contains(epic["id"].as_str().unwrap()));
    for (i, (provider, model)) in [
        ("claude", "haiku"),
        ("gemini", "gemini-2.5-flash"),
        ("claude", "sonnet"),
    ]
    .iter()
    .enumerate()
    {
        let row = &result["children"][i];
        let name = row["worker"].as_str().unwrap();
        assert_ne!(name, coordinator);
        let e = read_env_file(&r.home, name);
        assert_eq!(e["CC_PARENT"], coordinator);
        assert_eq!(e["CC_EPHEMERAL"], "1");
        assert_eq!(e["CC_WORKTREE"], "1");
        assert_eq!(e["CC_PROVIDER"], *provider);
        assert!(e["CC_FLAGS"].contains(&format!("--model {model}")), "{e:?}");
        assert_eq!(row["profile"]["model"], *model);
        let child = get_card(&r.app, row["id"].as_str().unwrap()).await;
        assert_eq!(child["callback"]["session"], coordinator);
        assert_eq!(child["depends_on"], json!([]));
        let (_, _, instructions) = send(
            &r.app,
            "GET",
            &format!("/api/sessions/{name}/instructions"),
            None,
            &[],
        )
        .await;
        assert!(instructions["instructions"]
            .as_str()
            .unwrap()
            .contains(&format!("amux send {coordinator} --no-board")));
    }
    let own_followup = create(
        &r.app,
        json!({"title":"Verify combined outcomes","session":coordinator,"type":"research"}),
    )
    .await;
    let (status, _, projection) = send(&r.app, "GET", "/api/board/orchestrations", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(projection["workers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w["name"] == coordinator
            && w["role"] == "orchestrator"
            && w["profile"]["model"] == "gpt-5"));
    assert!(projection["cards"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"] == own_followup["id"]));
    assert!(!projection["ephemeral_workers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w == coordinator));
    let (status, _, retry) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(request),
        &[("x-amux-session", "workspace")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{retry}");
    assert_eq!(retry["idempotent"], true);
    assert_eq!(retry["orchestrator"]["name"], coordinator);
    assert_eq!(retry["children"], result["children"]);
    assert_eq!(retry["epic"], result["epic"]);
}

#[tokio::test]
async fn concurrent_launch_and_retry_after_completion_reuse_one_orchestration() {
    let r = rig().await;
    write_parent_env(&r.home, "workspace");
    let request = json!({"launch_id":"one-request","parent_session":"workspace","orchestrator":{"model":"opus"},"priorities":["Verify output"]});
    let (first, second) = tokio::join!(
        send(
            &r.app,
            "POST",
            "/api/board/launch",
            Some(request.clone()),
            &[("x-amux-session", "workspace")]
        ),
        send(
            &r.app,
            "POST",
            "/api/board/launch",
            Some(request.clone()),
            &[("x-amux-session", "workspace")]
        )
    );
    assert_eq!(first.0, StatusCode::CREATED, "{}", first.2);
    assert_eq!(second.0, StatusCode::CREATED, "{}", second.2);
    assert_eq!(first.2["epic"], second.2["epic"]);
    assert_eq!(first.2["children"], second.2["children"]);
    // Seed a completed, renamed fixture graph, then retry its lost launch receipt.
    // Completion/gate admission is covered by the integration test above.
    let db = rusqlite::Connection::open(r._dir.path().join("fan-out-test.db")).unwrap();
    db.execute(
        "UPDATE issues SET status='verified',title='Verified output',desc='Final outcome evidence'",
        [],
    )
    .unwrap();
    let (status, _, retry) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(request.clone()),
        &[("x-amux-session", "workspace")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{retry}");
    assert_eq!(retry["epic"], first.2["epic"]);
    assert_eq!(retry["complete"], true);
    assert_eq!(retry["workers_started"], 0);
    assert_eq!(retry["workers_failed"], 0);
    assert_eq!(retry["orchestrator"]["started"], false);
    assert_eq!(
        db.query_row("SELECT count(*) FROM issues", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    // An explicit new launch with identical text is still a distinct intent.
    let mut fresh = request;
    fresh["launch_id"] = json!("new-request");
    let (status, _, next) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(fresh),
        &[("x-amux-session", "workspace")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{next}");
    assert_ne!(next["epic"], first.2["epic"]);
    assert_ne!(
        next["orchestrator"]["name"],
        first.2["orchestrator"]["name"]
    );
}

#[tokio::test]
async fn coordinator_retry_preserves_paused_state_and_does_not_create_more_cards() {
    let r = rig().await;
    write_parent_env(&r.home, "workspace");
    let request = json!({"parent_session":"workspace","orchestrator":{"model":"opus"},"priorities":["Verify output"]});
    let (_, _, first) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(request.clone()),
        &[("x-amux-session", "workspace")],
    )
    .await;
    let name = first["orchestrator"]["name"].as_str().unwrap();
    let path = r.home.join("sessions").join(format!("{name}.env"));
    let saved = format!(
        "{}\nCC_PAUSED=1\nCC_FLAGS='--model sonnet'\n",
        std::fs::read_to_string(&path).unwrap()
    );
    std::fs::write(&path, &saved).unwrap();
    let epic = get_card(&r.app, first["epic"].as_str().unwrap()).await;
    let (status, _, result) = send(
        &r.app,
        "POST",
        "/api/board/launch",
        Some(request),
        &[("x-amux-session", "workspace")],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{result}");
    assert_eq!(result["code"], "orchestrator_provision_refused");
    assert_eq!(std::fs::read_to_string(path).unwrap(), saved);
    assert_eq!(
        get_card(&r.app, epic["id"].as_str().unwrap()).await["rev"],
        epic["rev"]
    );
    let (_, _, projection) = send(&r.app, "GET", "/api/board/orchestrations", None, &[]).await;
    assert_eq!(projection["cards"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn invalid_role_profiles_refuse_before_any_worker_or_card_is_created() {
    let r = rig().await;
    write_parent_env(&r.home, "workspace");
    for bad in [
        json!({"provider":"iterm2"}),
        json!({"model":"opus; touch bad"}),
        json!({"model":"--help"}),
        json!({"flags":"--model sonnet"}),
    ] {
        let (status,_,result)=send(&r.app,"POST","/api/board/launch",Some(json!({
            "parent_session":"workspace","orchestrator":{},"priorities":[{"text":"Verify parser","profile":bad}]
        })),&[("x-amux-session","workspace")]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{result}");
    }
    assert_eq!(
        std::fs::read_dir(r.home.join("sessions"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|f| f.path().extension().is_some_and(|e| e == "env"))
            .count(),
        1
    );
    let (_, _, projection) = send(&r.app, "GET", "/api/board/orchestrations", None, &[]).await;
    assert_eq!(projection["cards"], json!([]));
}
