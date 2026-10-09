//! Ethan, 2026-10-08: "for all new workers make sure all of these are disabled
//! by default" (the six Board-tab automation toggles). Drives the real
//! POST /api/sessions and reads the env file the handler wrote. Own process,
//! because AMUX_HOME is global.
use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::{body::Body, http::{Request, StatusCode}};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn a_new_worker_starts_with_every_board_automation_toggle_off() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join("sessions")).unwrap();
    std::env::set_var("AMUX_HOME", home.path());
    let store = std::sync::Arc::new(Store::open(&home.path().join("t.db")).unwrap());
    let app = router(AppState {
        store,
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    });
    // yolo used to switch auto-continue ON at create; it must not any more.
    let req = Request::builder().method("POST").uri("/api/sessions")
        .header("Content-Type", "application/json")
        .body(Body::from(json!({"name":"fresh-lane","dir":"/tmp","yolo":true}).to_string())).unwrap();
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::CREATED);
    let env = std::fs::read_to_string(home.path().join("sessions/fresh-lane.env")).unwrap();
    for key in ["AMUX_DISPATCH_BACKLOG_WHEN_IDLE", "CC_AUTO_PICKUP", "CC_AUTO_CONTINUE",
                "CC_STANDING_ORDERS", "AMUX_COMMAND_LIFECYCLE", "AMUX_BOARD_FORCE_ADHERENCE"] {
        assert!(env.contains(&format!("{key}=\"0\"")) || env.contains(&format!("{key}=0")),
            "{key} must be written off for a new worker, got:\n{env}");
    }
    assert!(!env.contains("CC_AUTO_CONTINUE=\"1\""), "yolo must not switch auto-continue on:\n{env}");
}
