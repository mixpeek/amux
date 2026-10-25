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
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
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

async fn post_json(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("X-Amux-Session", "test-worker")
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

async fn patch_json(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("PATCH")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("X-Amux-Session", "test-worker")
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

    let (st, _) = post_json(
        &app,
        "/api/subscriptions",
        json!({"pattern": "task.*", "action": "shell", "action_target": "true"}),
    )
    .await;
    assert!(
        st == StatusCode::BAD_REQUEST || st == StatusCode::UNPROCESSABLE_ENTITY,
        "missing name should be 400 or 422, got {st}"
    );

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
