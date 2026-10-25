//! Tab layout presets are scoped (0133): `global` is the homepage tab bar and
//! `peek` the worker view's tabs (Ethan, 2026-10-24: the worker tab list should
//! save presets server side like the homepage one does).
//!
//! The name used to be the whole key, so the two lists could not share it.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

fn app() -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("amux-test.db")).unwrap());
    let state = AppState {
        store,
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), dir)
}

async fn call(app: &axum::Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path);
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn names(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn worker_view_presets_are_kept_apart_from_homepage_presets() {
    let (app, _d) = app();
    // Same name in both scopes: neither replaces the other.
    let (st, _) = call(&app, "POST", "/api/layout-presets",
        Some(json!({"name": "focus", "hidden": ["cost"], "tab_order": ["sessions", "board"]}))).await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, v) = call(&app, "POST", "/api/layout-presets",
        Some(json!({"name": "focus", "scope": "peek", "hidden": ["commits"], "tab_order": ["chat", "messages"]}))).await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(v["scope"], "peek");

    // No scope reads the homepage list, exactly as before scopes existed.
    let (_, global) = call(&app, "GET", "/api/layout-presets", None).await;
    assert_eq!(names(&global), ["focus"]);
    assert_eq!(global[0]["hidden"], json!(["cost"]));
    let (_, peek) = call(&app, "GET", "/api/layout-presets?scope=peek", None).await;
    assert_eq!(names(&peek), ["focus"]);
    assert_eq!(peek[0]["hidden"], json!(["commits"]));
    assert_eq!(peek[0]["tab_order"], json!(["chat", "messages"]));

    // Deleting the worker-view preset leaves the homepage one.
    let (st, _) = call(&app, "DELETE", "/api/layout-presets/focus?scope=peek", None).await;
    assert_eq!(st, StatusCode::OK);
    let (_, peek) = call(&app, "GET", "/api/layout-presets?scope=peek", None).await;
    assert!(names(&peek).is_empty(), "{peek}");
    let (_, global) = call(&app, "GET", "/api/layout-presets", None).await;
    assert_eq!(names(&global), ["focus"], "a peek delete must not reach the homepage preset");
}

#[tokio::test]
async fn an_unknown_scope_is_refused_rather_than_stored() {
    let (app, _d) = app();
    let (st, v) = call(&app, "POST", "/api/layout-presets",
        Some(json!({"name": "x", "scope": "peeek", "hidden": [], "tab_order": []}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    let (st, _) = call(&app, "GET", "/api/layout-presets?scope=peeek", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

/// The migration keeps every existing (homepage) preset, unchanged.
#[test]
fn the_scope_migration_keeps_existing_presets_as_homepage_presets() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE layout_presets (name TEXT PRIMARY KEY, hidden TEXT NOT NULL DEFAULT '[]',
             tab_order TEXT NOT NULL DEFAULT '[]', created_at INTEGER NOT NULL);
         INSERT INTO layout_presets VALUES ('default', '[\"cost\"]', '[\"sessions\"]', 1792000000);",
    )
    .unwrap();
    conn.execute_batch(include_str!("../migrations/0133_layout_presets_scope.sql")).unwrap();
    let row: (String, String, String, String, i64) = conn
        .query_row("SELECT scope, name, hidden, tab_order, created_at FROM layout_presets", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap();
    assert_eq!(row, ("global".into(), "default".into(), "[\"cost\"]".into(), "[\"sessions\"]".into(), 1792000000));
    // And the key is now (scope, name).
    conn.execute(
        "INSERT INTO layout_presets (scope, name, created_at) VALUES ('peek', 'default', 1)",
        [],
    )
    .unwrap();
}
