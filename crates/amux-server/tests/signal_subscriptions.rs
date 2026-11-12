//! Signal subscriptions: event-driven worker coordination.
//! Tests the CRUD endpoints and the dispatch/matching logic.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use std::sync::OnceLock;
use tower::ServiceExt;

fn fleet_home() -> &'static std::path::Path {
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = amux_server::test_tempdirs::keep(tempfile::tempdir().expect("tempdir"));
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        std::fs::write(
            sessions.join("test-worker.env"),
            "CC_DIR=/tmp\nCC_TAGS=test\n",
        )
        .expect("test-worker");
        std::env::set_var("AMUX_HOME", dir.path());
        dir.path().to_path_buf()
    })
    .as_path()
}

fn app() -> (axum::Router, std::sync::Arc<Store>, tempfile::TempDir) {
    let _ = fleet_home();
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(Store::open(&dir.path().join("amux-test.db")).unwrap());
    let state = AppState {
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), store, dir)
}

/// A POST from the owner (no worker header): the owner may make any
/// subscription, `shell` included.
async fn post_json(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    post_as(app, uri, body, None).await
}

async fn post_as(app: &axum::Router, uri: &str, body: Value, worker: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(w) = worker {
        req = req.header("X-Amux-Session", w);
    }
    let req = req.body(Body::from(body.to_string())).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

/// A PATCH from the owner (no worker header). Every caller here edits a
/// subscription the owner made; a worker may edit only its own
/// (`api::signals::subscription_edit_tests`).
async fn patch_json(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("PATCH")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

async fn delete(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

#[tokio::test]
async fn create_and_get_subscription() {
    let (app, _store, _dir) = app();

    let (st, v) = post_json(
        &app,
        "/api/subscriptions",
        json!({
            "name": "test-deploy-alert",
            "pattern": "session.stopped",
            "action": "shell",
            "action_target": "echo fired",
            "action_template": "Worker {{session}} stopped: {{type}}",
        }),
    )
    .await;

    assert_eq!(st, StatusCode::CREATED, "create should return 201: {v}");
    let id = v["id"].as_i64().expect("should return id");
    assert_eq!(v["name"], "test-deploy-alert");
    assert_eq!(v["pattern"], "session.stopped");
    assert!(v["enabled"].as_i64() == Some(1) || v["enabled"].as_bool() == Some(true));

    let (st, v) = get(&app, &format!("/api/subscriptions/{id}")).await;
    assert_eq!(st, StatusCode::OK, "get should return 200: {v}");
    assert_eq!(v["name"], "test-deploy-alert");
    assert_eq!(v["action"], "shell");
}

#[tokio::test]
async fn list_subscriptions() {
    let (app, _store, _dir) = app();

    post_json(
        &app,
        "/api/subscriptions",
        json!({"name": "list-test-1", "pattern": "task.*", "action": "shell", "action_target": "true"}),
    )
    .await;
    post_json(
        &app,
        "/api/subscriptions",
        json!({"name": "list-test-2", "pattern": "session.*", "action": "shell", "action_target": "true"}),
    )
    .await;

    let (st, v) = get(&app, "/api/subscriptions").await;
    assert_eq!(st, StatusCode::OK);
    let subs = v["subscriptions"].as_array().unwrap_or_else(|| {
        panic!("list should return subscriptions array, got: {v}")
    });
    assert!(subs.len() >= 2, "should have at least 2 subscriptions: {v}");
}

#[tokio::test]
async fn update_subscription() {
    let (app, _store, _dir) = app();

    let (_, v) = post_json(
        &app,
        "/api/subscriptions",
        json!({"name": "update-test", "pattern": "task.*", "action": "shell", "action_target": "true"}),
    )
    .await;
    let id = v["id"].as_i64().unwrap();

    let (st, v) = patch_json(
        &app,
        &format!("/api/subscriptions/{id}"),
        json!({"pattern": "session.*", "enabled": false}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "update should return 200: {v}");
    assert_eq!(v["pattern"], "session.*");
    assert!(v["enabled"].as_i64() == Some(0) || v["enabled"].as_bool() == Some(false),
        "enabled should be falsy: {v}");
}

#[tokio::test]
async fn delete_subscription() {
    let (app, _store, _dir) = app();

    let (_, v) = post_json(
        &app,
        "/api/subscriptions",
        json!({"name": "delete-test", "pattern": "task.*", "action": "shell", "action_target": "true"}),
    )
    .await;
    let id = v["id"].as_i64().unwrap();

    let (st, _) = delete(&app, &format!("/api/subscriptions/{id}")).await;
    assert_eq!(st, StatusCode::OK);

    let (st, _) = get(&app, &format!("/api/subscriptions/{id}")).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "deleted sub should be 404");
}

#[tokio::test]
async fn create_requires_name_pattern_action() {
    let (app, _store, _dir) = app();

    // A missing name is made from the pattern (or the criteria).
    let (st, v) = post_json(
        &app,
        "/api/subscriptions",
        json!({"pattern": "task.*", "action": "shell", "action_target": "true"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert!(v["name"].as_str().unwrap().starts_with("task-"), "{v}");

    let (st, _) = post_json(
        &app,
        "/api/subscriptions",
        json!({"name": "x", "action": "shell", "action_target": "true"}),
    )
    .await;
    assert!(
        st == StatusCode::BAD_REQUEST || st == StatusCode::UNPROCESSABLE_ENTITY,
        "missing pattern should be 400 or 422, got {st}"
    );
}

#[tokio::test]
async fn emit_fires_matching_subscription() {
    let (app, _store, _dir) = app();

    post_json(
        &app,
        "/api/subscriptions",
        json!({
            "name": "emit-test",
            "pattern": "test.event",
            "action": "shell",
            "action_target": "echo ok",
        }),
    )
    .await;

    let (st, v) = post_json(
        &app,
        "/api/subscriptions/emit",
        json!({"type": "test.event", "session": "test-worker", "data": {"key": "val"}}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "emit should return 200: {v}");
    assert_eq!(v["ok"], true);
    let fired = v["subscriptions_fired"].as_array().unwrap();
    assert_eq!(fired.len(), 1, "should fire exactly one subscription");
    assert_eq!(fired[0]["subscription"], "emit-test");
}

#[tokio::test]
async fn glob_matching_works() {
    let (app, _store, _dir) = app();

    post_json(
        &app,
        "/api/subscriptions",
        json!({
            "name": "glob-test",
            "pattern": "session.*",
            "action": "shell",
            "action_target": "true",
        }),
    )
    .await;

    // Should match session.stopped
    let (_, v) = post_json(
        &app,
        "/api/subscriptions/emit",
        json!({"type": "session.stopped", "session": "test-worker"}),
    )
    .await;
    let fired = v["subscriptions_fired"].as_array().unwrap();
    assert!(
        fired.iter().any(|f| f["subscription"] == "glob-test"),
        "session.* should match session.stopped"
    );

    // Should NOT match task.claimed
    let (_, v) = post_json(
        &app,
        "/api/subscriptions/emit",
        json!({"type": "task.claimed", "session": "test-worker"}),
    )
    .await;
    let fired = v["subscriptions_fired"].as_array().unwrap();
    assert!(
        !fired.iter().any(|f| f["subscription"] == "glob-test"),
        "session.* should NOT match task.claimed"
    );
}

#[tokio::test]
async fn source_and_session_filters() {
    let (app, _store, _dir) = app();

    post_json(
        &app,
        "/api/subscriptions",
        json!({
            "name": "filter-test",
            "pattern": "deploy.*",
            "session_filter": "prod-*",
            "source_filter": "cicd",
            "action": "shell",
            "action_target": "true",
        }),
    )
    .await;

    // Should match: session=prod-api, source=cicd
    let (_, v) = post_json(
        &app,
        "/api/subscriptions/emit",
        json!({"type": "deploy.success", "session": "prod-api", "source": "cicd"}),
    )
    .await;
    let fired = v["subscriptions_fired"].as_array().unwrap();
    assert!(
        fired.iter().any(|f| f["subscription"] == "filter-test"),
        "should match prod-api + cicd"
    );

    // Should NOT match: wrong source
    let (_, v) = post_json(
        &app,
        "/api/subscriptions/emit",
        json!({"type": "deploy.success", "session": "prod-api", "source": "manual"}),
    )
    .await;
    let fired = v["subscriptions_fired"].as_array().unwrap();
    assert!(
        !fired.iter().any(|f| f["subscription"] == "filter-test"),
        "should NOT match source=manual"
    );

    // Should NOT match: wrong session
    let (_, v) = post_json(
        &app,
        "/api/subscriptions/emit",
        json!({"type": "deploy.success", "session": "dev-api", "source": "cicd"}),
    )
    .await;
    let fired = v["subscriptions_fired"].as_array().unwrap();
    assert!(
        !fired.iter().any(|f| f["subscription"] == "filter-test"),
        "should NOT match session=dev-api"
    );
}

#[tokio::test]
async fn fires_are_recorded() {
    let (app, _store, _dir) = app();

    post_json(
        &app,
        "/api/subscriptions",
        json!({
            "name": "fire-record-test",
            "pattern": "test.fire",
            "action": "shell",
            "action_target": "echo recorded",
            "dedupe_window_s": 0,
        }),
    )
    .await;

    post_json(
        &app,
        "/api/subscriptions/emit",
        json!({"type": "test.fire", "session": "test-worker"}),
    )
    .await;

    let (st, v) = get(&app, "/api/subscriptions/fires").await;
    assert_eq!(st, StatusCode::OK);
    let fires = v["fires"].as_array().unwrap();
    assert!(
        fires.iter().any(|f| f["subscription_name"] == "fire-record-test"),
        "fire should be recorded: {v}"
    );
}

#[tokio::test]
async fn disabled_subscription_does_not_fire() {
    let (app, _store, _dir) = app();

    let (_, v) = post_json(
        &app,
        "/api/subscriptions",
        json!({
            "name": "disabled-test",
            "pattern": "test.disabled",
            "action": "shell",
            "action_target": "true",
        }),
    )
    .await;
    let id = v["id"].as_i64().unwrap();

    patch_json(
        &app,
        &format!("/api/subscriptions/{id}"),
        json!({"enabled": false}),
    )
    .await;

    let (_, v) = post_json(
        &app,
        "/api/subscriptions/emit",
        json!({"type": "test.disabled", "session": "test-worker"}),
    )
    .await;
    let fired = v["subscriptions_fired"].as_array().unwrap();
    assert!(
        fired.is_empty(),
        "disabled subscription should not fire: {v}"
    );
}

// ---- plain words, filters, the ledger tail (docs/design/event-subscriptions.md) ----

fn state_for(store: &std::sync::Arc<Store>) -> AppState {
    AppState {
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    }
}

fn raw(dir: &tempfile::TempDir) -> rusqlite::Connection {
    rusqlite::Connection::open(dir.path().join("amux-test.db")).unwrap()
}

async fn fires_of(app: &axum::Router, name: &str) -> Vec<Value> {
    let (_, v) = get(app, "/api/subscriptions/fires?limit=500").await;
    v["fires"].as_array().unwrap().iter().filter(|f| f["subscription_name"] == name).cloned().collect()
}

#[tokio::test]
async fn a_worker_subscribes_for_itself_and_cannot_run_shell() {
    let (app, _store, _dir) = app();
    let (st, v) = post_as(&app, "/api/subscriptions",
        json!({"name": "w-shell", "pattern": "x.*", "action": "shell", "action_target": "true"}), Some("test-worker")).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    let (st, v) = post_as(&app, "/api/subscriptions",
        json!({"name": "w-other", "pattern": "x.*", "action": "send", "action_target": "someone-else"}), Some("test-worker")).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    let (st, v) = post_as(&app, "/api/subscriptions", json!({"name": "w-self", "pattern": "x.*"}), Some("test-worker")).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_eq!((v["action"].as_str(), v["action_target"].as_str(), v["owner"].as_str()),
        (Some("send"), Some("test-worker"), Some("test-worker")), "a worker's subscription wakes the worker");
    let (st, v) = post_json(&app, "/api/subscriptions", json!({"name": "o-default", "pattern": "x.*"})).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_eq!((v["action"].as_str(), v["owner"].as_str()), (Some("notify"), Some("owner")), "the owner is told");
}

#[tokio::test]
async fn a_filter_and_a_dedupe_key_decide_what_fires() {
    let (app, _store, _dir) = app();
    let (st, v) = post_json(&app, "/api/subscriptions", json!({
        "name": "captchas", "pattern": "browser.blocked", "action": "shell", "action_target": "true",
        "filter": {"field": "reason", "op": "eq", "value": "captcha"},
        "dedupe_key": "{{url}}", "dedupe_window_s": 600,
    })).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let emit = |reason: &'static str, url: &'static str| {
        let app = app.clone();
        async move {
            let (_, v) = post_json(&app, "/api/subscriptions/emit",
                json!({"type": "browser.blocked", "session": "w", "data": {"reason": reason, "url": url}})).await;
            v["subscriptions_fired"].as_array().unwrap().iter().any(|f| f["subscription"] == "captchas")
        }
    };
    assert!(emit("captcha", "https://a/").await, "a captcha fires");
    assert!(!emit("captcha", "https://a/").await, "the same page again is the same occurrence");
    assert!(emit("captcha", "https://b/").await, "another page is a new one");
    assert!(!emit("login", "https://c/").await, "the filter keeps other reasons out");
    let fires = fires_of(&app, "captchas").await;
    assert_eq!(fires.len(), 2);
    assert!(fires.iter().all(|f| f["outcome"] == "ok"), "{fires:?}");
}

/// Events written straight into the ledger (most writers do that, every
/// browser event among them) reach subscriptions, and a restart resumes from
/// the stored cursor: what it missed fires once, nothing fires twice.
#[tokio::test]
async fn ledger_writers_and_restarts_reach_subscriptions() {
    let (app, store, dir) = app();
    let (st, v) = post_json(&app, "/api/subscriptions", json!({
        "name": "fanout", "pattern": "fanout.*", "action": "shell", "action_target": "true", "dedupe_window_s": 0,
    })).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let state = state_for(&store);
    amux_server::runtime_jobs::event_dispatch::drain(&state).await; // a first start stores its cursor at the head
    let insert = |n: i64| {
        raw(&dir).execute("INSERT INTO session_events (ts,session,type,data,source) VALUES (?1,'w','fanout.integrated','{}','board-drive')",
            [n as f64]).unwrap();
    };
    insert(1);
    let fired = amux_server::runtime_jobs::event_dispatch::drain(&state).await;
    assert_eq!(fired.len(), 1, "a direct ledger write fired the subscription");
    insert(2);
    insert(3);
    // A new process on the same database: it resumes, it does not start over.
    let restarted = state_for(&store);
    let fired = amux_server::runtime_jobs::event_dispatch::drain(&restarted).await;
    assert_eq!(fired.len(), 2, "both events written while it was down fired");
    assert!(amux_server::runtime_jobs::event_dispatch::drain(&restarted).await.is_empty(), "nothing fires twice");
    assert_eq!(fires_of(&app, "fanout").await.len(), 3);
    // A subscription that has fired can still be removed (its fires go with it).
    let (_, list) = get(&app, "/api/subscriptions").await;
    let id = list["subscriptions"].as_array().unwrap().iter().find(|s| s["name"] == "fanout").unwrap()["id"].as_i64().unwrap();
    let (st, v) = delete(&app, &format!("/api/subscriptions/{id}")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(fires_of(&app, "fanout").await.is_empty());
}

/// A judged match does not hold the dispatcher: the emit answers at once with
/// "judging" and the verdict is recorded when the model answers (here the
/// test home has no model, so the verdict is judge_no:no_model).
#[tokio::test]
async fn a_judged_match_runs_beside_the_dispatcher() {
    let (app, _store, _dir) = app();
    let (st, v) = post_json(&app, "/api/subscriptions", json!({
        "name": "judged", "pattern": "browser.route.*", "action": "shell", "action_target": "true",
        "judge": true, "criteria": "a sign-in page", "dedupe_window_s": 0,
    })).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let t0 = std::time::Instant::now();
    let (_, v) = post_json(&app, "/api/subscriptions/emit",
        json!({"type": "browser.route.state", "session": "w", "data": {"url": "https://x/login"}})).await;
    assert!(t0.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(v["subscriptions_fired"][0]["outcome"], "judging", "{v}");
    let mut seen = vec![];
    for _ in 0..50 {
        seen = fires_of(&app, "judged").await;
        if !seen.is_empty() { break; }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(seen.len(), 1, "the verdict is recorded once the judge answers");
    assert!(seen[0]["outcome"].as_str().unwrap().starts_with("judge_no"), "{seen:?}");
}

#[tokio::test]
async fn a_board_card_moving_fires_a_card_event() {
    let (app, store, dir) = app();
    let (st, v) = post_json(&app, "/api/subscriptions", json!({
        "name": "done-cards", "pattern": "board.card.moved", "action": "shell", "action_target": "true",
        "filter": {"field": "status", "op": "eq", "value": "done"}, "dedupe_window_s": 0,
    })).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let state = state_for(&store);
    amux_server::runtime_jobs::event_dispatch::drain(&state).await;
    let c = raw(&dir);
    c.execute("INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type) VALUES ('T-1','Ship the page','','doing','w',1,1,'agent','task')", []).unwrap();
    c.execute("UPDATE issues SET status='review', updated=2 WHERE id='T-1'", []).unwrap();
    c.execute("UPDATE issues SET status='done', updated=3 WHERE id='T-1'", []).unwrap();
    amux_server::runtime_jobs::event_dispatch::drain(&state).await;
    let fires = fires_of(&app, "done-cards").await;
    assert_eq!(fires.len(), 1, "only the move to done: {fires:?}");
    assert_eq!(fires[0]["event_type"], "board.card.moved");
    assert_eq!(fires[0]["event_data"]["title"], "Ship the page");
    assert_eq!(fires[0]["event_data"]["old_status"], "review");
    // The backtest reads the board's change log too, so the same subscription
    // made now would show this move in its last 7 days.
    let (st, v) = post_json(&app, "/api/subscriptions", json!({
        "pattern": "board.card.moved", "filter": {"field": "status", "op": "eq", "value": "done"}, "dry_run": true,
    })).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!((v["backtest"]["matched"].as_i64(), v["backtest"]["n_considered"].as_i64()), (Some(1), Some(2)), "{v}");
}

#[tokio::test]
async fn the_catalog_and_a_dry_run_show_what_a_subscription_would_see() {
    let (app, _store, _dir) = app();
    post_json(&app, "/api/subscriptions/emit",
        json!({"type": "test.catalog", "session": "w", "data": {"colour": "red"}})).await;
    let (st, v) = get(&app, "/api/subscriptions/catalog").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["measured"], true);
    let t = v["types"].as_array().unwrap().iter().find(|t| t["type"] == "test.catalog").expect("emitted type listed");
    assert_eq!(t["documented"], false);
    assert!(t["fields"].as_array().unwrap().iter().any(|f| f == "colour"), "{t}");
    let (st, v) = post_json(&app, "/api/subscriptions", json!({
        "pattern": "test.*", "filter": {"field": "colour", "op": "eq", "value": "red"}, "dry_run": true,
    })).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["backtest"]["matched"], 1, "{v}");
    let (_, v) = get(&app, "/api/subscriptions").await;
    assert_eq!(v["count"], 0, "a dry run saves nothing");
    let (_, v) = get(&app, "/api/subscriptions/events?pattern=test.*").await;
    assert!(v["events"].as_array().unwrap().iter().any(|e| e["type"] == "test.catalog"), "{v}");
}
