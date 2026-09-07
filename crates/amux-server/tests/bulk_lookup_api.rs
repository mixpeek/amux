//! Wire-level refusal cells for the delegated bulk reader.
//! Success calls a configured external helper and is exercised through the CLI
//! harness; these cases prove bad input reaches the real route and returns the
//! measured JSON contract without spending a model call.

use amux_server::api::{router, AppState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

fn app() -> axum::Router {
    let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let store = amux_server::db::Store::open(&dir.path().join("bulk-lookup.db")).unwrap();
    router(AppState {
        secrets: std::sync::Arc::new(amux_server::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    })
}

async fn post(body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/api/lookup/bulk")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn missing_question_is_a_measured_json_refusal() {
    let (status, value) = post(json!({
        "question": "  ",
        "files": [{"path": "src/main.rs", "content": "fn main() {}"}],
    }))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(value["error"], "missing question");
    assert_eq!(value["measured"], false);
    assert_eq!(value["n_considered"], 0);
    assert!(value["why_unmeasured"].as_str().unwrap().contains("validation"));
}

#[tokio::test]
async fn decoded_content_limit_answers_before_the_helper_runs() {
    let (status, value) = post(json!({
        "question": "summarize",
        "files": [{"path": "huge.txt", "content": "x".repeat(512 * 1024 + 1)}],
    }))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(value["error"], "delegated file content exceeds 512 KiB");
    assert_eq!(value["measured"], false);
    assert_eq!(value["n_considered"], 0);
}
