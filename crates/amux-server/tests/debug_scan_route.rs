//! `GET /api/debug/scan` mount + 200 guard (AF-80).
//!
//! ethos.md and the system-jobs registry both ADVERTISE `GET /api/debug/scan`
//! (`registry.rs` carries `detail: Some("/api/debug/scan")`), and for a long
//! time it 404'd, a claimed-but-not-implemented instrument (ethos rule 6).
//! This test fails if the route regresses to unrouted: an unrouted /api path
//! lands in the SPA catch-all (HTML 404), not a 200 whose JSON carries the
//! scan report's fields. The route_table.rs walk cross-checks the same mount
//! from the ROUTE_TABLE side; this asserts the concrete 200 + payload shape.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

#[tokio::test]
async fn debug_scan_is_mounted_and_returns_200() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("t.db")).unwrap();
    let app = router(AppState {
        secrets: std::sync::Arc::new(amux_server::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        // Public route (mounted outside require_bearer), and None disables auth.
        auth_token: None,
    reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    });

    let res = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/debug/scan")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        res.status().as_u16(),
        200,
        "GET /api/debug/scan must be mounted and answer 200, not the SPA catch-all's 404"
    );

    let body = axum::body::to_bytes(res.into_body(), 256 * 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body)
        .expect("a mounted /api/debug/scan answers JSON, not the SPA HTML shell");
    // Present in BOTH the has-run and never-run branches, so this asserts the
    // handler answered (not the shell) regardless of scan-loop state.
    assert!(
        json.get("demoted_structured").is_some(),
        "the scan report must carry demoted_structured: {json}"
    );
    assert!(
        json.get("demoted_native").is_some(),
        "the scan report must carry demoted_native: {json}"
    );
}
