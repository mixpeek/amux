//! `POST /api/git/staged-guard` must not 413 a large staged set (AF-133).
//!
//! The route took axum's DEFAULT 2MB body limit because nothing set one. The
//! hook's payload carries `paths` — the staged file list — so it scales with
//! the size of the commit, and a big enough `git add` produced
//! "Failed to buffer the request body: length limit exceeded". 17 of those
//! landed in ten minutes on 2026-08-22 (daily log sweep,
//! `GET /api/logs/analyze?since_h=24`).
//!
//! Fail-open is correct and is not the bug. WHICH commits it applied to is:
//! the hook can only read a 413 as an error, so cross-session sweep protection
//! was absent on exactly the LARGEST staged sets — the commits most likely to
//! sweep a peer's work.
//!
//! The assertion is deliberately "not 413", not "200". What the handler decides
//! about a pathological staged set is its business and may change; what must
//! not happen is the transport layer refusing before the handler ever sees it.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

fn app() -> axum::Router {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("t.db")).unwrap();
    std::mem::forget(dir); // keep the tempdir alive for the test's duration
    router(AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
    })
}

/// Build a body comfortably over axum's 2MB default so the OLD code 413s and
/// the new code does not. ~40k paths at ~64 bytes each.
fn big_body() -> String {
    let paths: Vec<String> = (0..40_000)
        .map(|i| format!("crates/amux-server/src/generated/module_{i:06}/file_{i:06}.rs"))
        .collect();
    serde_json::json!({
        "session": "af133-test",
        "dir": "/tmp/af133",
        "paths": paths,
        "op": "commit",
        "guard_version": 6,
    })
    .to_string()
}

#[tokio::test]
async fn a_large_staged_set_reaches_the_handler_instead_of_a_transport_413() {
    let body = big_body();
    assert!(
        body.len() > 2 * 1024 * 1024,
        "fixture must exceed axum's 2MB default or this test cannot fail: {} bytes",
        body.len()
    );

    let res = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/git/staged-guard")
                .header("content-type", "application/json")
                .header("x-amux-session", "af133-test")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_ne!(
        res.status().as_u16(),
        413,
        "a large staged set must reach the handler; a transport 413 makes the hook fail \
         open on exactly the commits most likely to sweep a peer's work (AF-133)"
    );
}

/// The control: without it, a passing test above proves nothing — a route that
/// 404'd would also "not 413". This pins that the route is mounted for POST.
#[tokio::test]
async fn the_route_is_mounted_for_post_so_the_limit_test_can_discriminate() {
    let res = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/git/staged-guard")
                .header("content-type", "application/json")
                .header("x-amux-session", "af133-test")
                .body(Body::from(r#"{"session":"af133-test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let s = res.status().as_u16();
    assert!(
        s != 404 && s != 405,
        "POST /api/git/staged-guard must be routed (got {s}) — otherwise the body-limit \
         test above passes for the wrong reason"
    );
}
