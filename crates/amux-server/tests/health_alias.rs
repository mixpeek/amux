//! `/api/health` answers the same thing as `/health` (2026-08-30 log sweep).
//!
//! `/health` is the only diagnostic NOT under `/api/`, and every sibling is:
//! `/api/health/invariants`, `/api/debug/*`, `/api/logs/*`. So lanes guess.
//! The sweep measured 20 x `404 GET /api/health` in 24 hours from loopback
//! curl, in irregular bursts of 3-5 rather than on a fixed interval — the
//! signature of an agent typing it by hand, not a monitor. Each one paid a
//! round trip and learned nothing.

use amux_server::api::{router, AppState};
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

fn app() -> axum::Router {
    let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let store = amux_server::db::Store::open(&dir.path().join("h.db")).unwrap();
    router(AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
    reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    })
}

async fn get(path: &str) -> (u16, serde_json::Value) {
    let res = app()
        .oneshot(Request::builder().method("GET").uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let st = res.status().as_u16();
    let b = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (st, serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null))
}

#[tokio::test]
async fn api_health_is_an_alias_and_not_a_second_implementation() {
    let (canon_status, canon) = get("/health").await;
    let (alias_status, alias) = get("/api/health").await;
    assert_eq!(canon_status, 200, "the canonical route must answer: {canon}");
    assert_eq!(alias_status, 200, "the alias must answer, not 404: {alias}");

    // SAME HANDLER, not a second one that drifts. Compare the keys rather than
    // the values, because uptime and rev move between the two calls — a value
    // comparison here would be flaky for a reason that has nothing to do with
    // what this pins.
    let keys = |v: &serde_json::Value| {
        v.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default()
    };
    assert_eq!(keys(&canon), keys(&alias), "the alias must serve the same payload shape");
    assert_eq!(alias["status"], canon["status"]);

    // CONTROL: a neighbouring made-up path under the same prefix still 404s, so
    // this test would fail if the router had started answering everything.
    let (bogus, _) = get("/api/health-not-a-route").await;
    assert_eq!(bogus, 404, "the alias must be one route, not a prefix catch-all");
}

#[tokio::test(flavor = "current_thread")]
async fn exhausted_read_pool_keeps_health_identity_and_runtime_responsive() {
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(amux_server::db::Store::open(&dir.path().join("health.db")).unwrap());
    let state = AppState {
        store: store.clone(), started: std::time::Instant::now(), build_hash: "pinned-image".into(),
        auth_token: None, reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let mut held = Vec::new();
    while let Some(conn) = store.try_read() { held.push(conn); }
    assert!(!held.is_empty());
    // Simulate fleet work borrowing every connection. A real OS thread releases
    // them even if the old synchronous health handler blocks the entire runtime.
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        drop(held);
    });
    let started = std::time::Instant::now();
    let request = amux_server::api::health::health(axum::extract::State(state.clone()));
    let heartbeat = async {
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        started.elapsed()
    };
    let ((status, axum::Json(body)), heartbeat_delay) = tokio::join!(request, heartbeat);
    let elapsed = started.elapsed();
    release.join().unwrap();
    assert!(elapsed < std::time::Duration::from_millis(250), "health waited for fleet work: {elapsed:?}");
    assert!(heartbeat_delay < std::time::Duration::from_millis(250), "health blocked the runtime: {heartbeat_delay:?}");
    assert_eq!(status.as_u16(), 503);
    assert_eq!(body.build, "pinned-image");
    assert!(!body.board.measured);
    assert_eq!(body.board.error.as_deref(), Some("read_pool_exhausted"));
    let (status, axum::Json(body)) = amux_server::api::health::health(axum::extract::State(state)).await;
    assert_eq!(status.as_u16(), 200);
    assert!(body.board.measured && body.board.ok);
}

#[tokio::test(flavor = "current_thread")]
async fn late_probe_success_remains_visible_during_the_next_slow_probe() {
    use std::{sync::{Arc, mpsc}, time::Duration};
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(amux_server::db::Store::open(&dir.path().join("progress.db")).unwrap());
    let state = AppState {
        store:store.clone(), started:std::time::Instant::now(), build_hash:"probe-progress".into(),
        auth_token:None, reconciled:Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let block_writer = || {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let db = store.clone();
        let thread = std::thread::spawn(move || db.write(move |_| {
            ready_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(amux_server::db::WriteOutcome {applied:false,events:vec![]})
        }).unwrap());
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        (release_tx,thread)
    };
    let (release, thread) = block_writer();
    let (status, axum::Json(body)) = amux_server::api::health::health(axum::extract::State(state.clone())).await;
    assert_eq!(status.as_u16(),503);
    assert_eq!(body.board.error.as_deref(),Some("probe_deadline_exceeded"));
    assert!(!body.board.measured);
    assert_eq!(body.store_probe.last_success_age_ms,None,"never claim a measurement before completion");
    assert!(body.store_probe.in_flight_age_ms.is_some());
    release.send(()).unwrap(); thread.join().unwrap();
    // The detached work must publish its success even though its HTTP caller
    // already received 503. A new healthy response alone does not prove this.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (release, thread) = block_writer();
    let (status, axum::Json(body)) = amux_server::api::health::health(axum::extract::State(state)).await;
    release.send(()).unwrap(); thread.join().unwrap();
    assert_eq!(status.as_u16(),503);
    assert!(!body.board.measured,"the current request is still unmeasured");
    assert!(body.store_probe.last_success_age_ms.is_some_and(|age| age < 5000),
        "the earlier detached probe completed successfully; a watchdog must see that progress");
}
