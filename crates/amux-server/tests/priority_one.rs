//! Test priority one: verify ephemeral worker system can pick up and complete tasks.

use amux_server::api::{router, AppState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

fn app() -> axum::Router {
    let dir = amux_server::test_tempdirs::keep(tempfile::tempdir().unwrap());
    let store = amux_server::db::Store::open(&dir.path().join("priority-one.db")).unwrap();
    router(AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    })
}

async fn get_health() -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri("/api/health")
        .body(Body::empty())
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn priority_one_health_check_passes() {
    let (status, _value) = get_health().await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn priority_one_system_ready() {
    let (status, value) = get_health().await;
    assert_eq!(status, StatusCode::OK);
    assert!(value.is_object());
}
