//! Creating a worker starts it, except where a spawn is refused or declined.
//! Own process, because AMUX_HOME is global.
use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::{body::Body, http::Request};
use serde_json::json;
use tower::ServiceExt;

/// Create starts the worker unless the caller opts out (Ethan 2026-09-24:
/// "creating a worker should start it automatically"). Here the home is a temp
/// dir, so the AMUX-4724 guard must keep it from spawning, and the response
/// must say so rather than claim a start.
#[tokio::test]
async fn create_starts_by_default_except_where_a_spawn_is_refused_or_declined() {
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
    let post = |body: serde_json::Value| {
        let app = app.clone();
        async move {
            let req = Request::builder().method("POST").uri("/api/sessions")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string())).unwrap();
            let r = app.oneshot(req).await.unwrap();
            let bytes = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        }
    };
    // Temp home: the guard refuses, so no start is claimed.
    std::env::remove_var("AMUX_ALLOW_TMUX_SPAWN_FROM_TEST_HOME");
    assert_eq!(post(json!({"name": "auto-guarded", "dir": "/tmp"})).await["starting"], false);
    // Declined explicitly.
    assert_eq!(post(json!({"name": "auto-declined", "dir": "/tmp", "start": false})).await["starting"], false);
}
