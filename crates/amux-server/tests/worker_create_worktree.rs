//! AMUX-4911: `Use worktree` must reach the env, not a refusal.
//!
//! The dashboard has always offered the checkbox, described it in the present
//! tense ("Creates a git worktree with its own working directory and branch")
//! and auto-filled a branch name when ticked. `POST /api/sessions` answered
//! 501, so the only way to learn it could not work was to spend a create
//! discovering it.
//!
//! The refusal's premise was that the bookkeeping "does not exist here yet".
//! It did: `session_verbs::start` computes
//! `worktree_enabled = fanout || CC_WORKTREE == "1"` and its NON-fanout arm
//! already does the `git worktree add`, the stale-worktree cleanup and the
//! unlock-before-remove handling AMUX-4767 added. Only this endpoint refused
//! to write the variable that reaches it, and it cannot be added afterwards:
//! by then the lane has started in the shared checkout.
//!
//! WHAT THIS PINS AND WHAT IT DOES NOT. It drives the real POST /api/sessions
//! through the real router and reads the env file the handler wrote. It does
//! NOT start a worker, so it does not prove a worktree appears on disk: that
//! is the start path's behaviour, it predates this change, and asserting it
//! here would need a git repo and a spawned process. The seam covered is the
//! one that was broken, request -> env file.
//!
//! Own process, because AMUX_HOME is global.
use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::{body::Body, http::{Request, StatusCode}};
use serde_json::json;
use tower::ServiceExt;

async fn create(app: &axum::Router, body: serde_json::Value) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri("/api/sessions")
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

fn env_of(home: &std::path::Path, name: &str) -> String {
    std::fs::read_to_string(home.join("sessions").join(format!("{name}.env")))
        .unwrap_or_else(|e| panic!("no env file for {name}: {e}"))
}

#[tokio::test]
async fn a_worktree_create_writes_the_variable_the_start_path_reads() {
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

    // The exact body the dashboard sends when the box is ticked.
    let status = create(&app, json!({"name":"wt-lane","dir":"/tmp","worktree":true})).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a worktree create must be accepted; it answered 501 for months"
    );
    let env = env_of(home.path(), "wt-lane");
    assert!(
        env.contains("CC_WORKTREE=\"1\""),
        "the create must write the variable session_verbs::start reads, got:\n{env}"
    );

    // NEGATIVE CONTROL. Without it, an implementation that writes CC_WORKTREE
    // unconditionally passes the assertion above while silently giving every
    // worker on the box an isolated checkout it never asked for.
    let status = create(&app, json!({"name":"plain-lane","dir":"/tmp"})).await;
    assert_eq!(status, StatusCode::CREATED);
    let env = env_of(home.path(), "plain-lane");
    assert!(
        !env.contains("CC_WORKTREE"),
        "absence already means shared checkout; a second spelling of the default is not written:\n{env}"
    );

    // Project executors record a repository-local worktree. The worker API
    // must expose the same path the project uses, not the individual-worker
    // default under AMUX_HOME/worktrees.
    let repository = home.path().join("repository");
    let checkout = repository.join(".worktrees/wt-lane");
    std::fs::create_dir_all(&checkout).unwrap();
    std::fs::write(checkout.join(".git"), "gitdir: fixture\n").unwrap();
    std::fs::create_dir_all(home.path().join("workspaces")).unwrap();
    std::fs::write(home.path().join("workspaces/wt-lane.json"), json!({
        "repo":repository,"path":checkout,"branch":"amux/fanout/wt-lane","base":"a".repeat(40)
    }).to_string()).unwrap();
    let response = app.oneshot(Request::builder().uri("/api/sessions").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let sessions: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let worker = sessions.as_array().unwrap().iter().find(|row| row["name"]=="wt-lane").unwrap();
    assert_eq!(worker["worktree_path"], checkout.to_string_lossy().as_ref());
    assert_eq!(worker["worktree_active"], true);
    let plain = sessions.as_array().unwrap().iter().find(|row| row["name"]=="plain-lane").unwrap();
    assert_eq!(plain["worktree_active"], false);

}
