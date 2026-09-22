//! Fault injection at the real HTTP middleware/dispatcher, on one runtime thread.
//! Exhausting the read pool must delay the request without freezing other HTTP work.
use amux_server::{
    api::{self, AppState},
    db::Store,
};
use axum::{body::Body, http::Request, middleware, routing::post, Router};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

async fn probe(policy: bool) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("stop.db")).unwrap());
    let state = AppState {
        store: store.clone(),
        started: Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let app = if policy {
        Router::new()
            .route("/probe", post(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                api::policy::enforce,
            ))
            .with_state(state)
    } else {
        api::session_verbs::routes().with_state(state)
    };
    let n = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(4);
    let held = (0..n).map(|_| store.read().unwrap()).collect::<Vec<_>>();
    // An OS thread releases the fault even if the one async runtime thread is
    // pinned by the regression. That makes the failure bounded and measurable.
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(900));
        drop(held);
    });
    let started = Instant::now();
    let request = tokio::spawn(async move {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri(if policy {
                    "/probe"
                } else {
                    "/api/sessions/nonexistent-stop-probe/stop"
                })
                // Explicit untrusted provenance bypasses the unrelated trust read,
                // forcing this test to exercise the subsequent role lookup.
                .header("x-amux-input-trust", "untrusted")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let elapsed = started.elapsed();
    let response = request.await.unwrap();
    release.join().unwrap();
    assert!(
        elapsed < Duration::from_millis(500),
        "HTTP runtime blocked for {elapsed:?}, policy={policy}"
    );
    assert!(
        !response.status().is_server_error(),
        "read did not recover: {}",
        response.status()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn stop_routing_yields_while_the_read_pool_is_exhausted() {
    probe(false).await;
}
#[tokio::test(flavor = "current_thread")]
async fn policy_role_lookup_yields_while_the_read_pool_is_exhausted() {
    probe(true).await;
}
