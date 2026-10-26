//! GET /api/file/xlsx renders a workbook as rows (AMUX-3343). Pinned when
//! calamine moved 0.26 -> 0.37 for RUSTSEC-2026-0194/0195 (quick-xml) and
//! RUSTSEC-2026-0367..0369 (zip): the parser changed under this route.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

#[tokio::test]
async fn a_workbook_renders_as_sheets_of_rows() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState {
        store: Arc::new(Store::open(&dir.path().join("amux-test.db")).unwrap()),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let book = dir.path().join("tiny.xlsx");
    std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.xlsx"), &book).unwrap();
    let uri = format!("/api/file/xlsx?path={}", book.display());
    let res = router(state).oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
    let status = res.status();
    let v: Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["truncated"], false);
    assert_eq!(v["sheets"][0]["name"], "Leads");
    assert_eq!(v["sheets"][0]["rows"], serde_json::json!([["name", "seats"], ["Acme", "12"]]));
}
