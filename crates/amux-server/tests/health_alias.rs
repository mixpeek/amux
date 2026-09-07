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
        secrets: std::sync::Arc::new(amux_server::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
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
