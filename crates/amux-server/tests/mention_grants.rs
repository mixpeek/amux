//! Owner @-mention grants, end to end (api/mention_grants.rs).
//!
//! Ethan, 2026-10-13: the owner can let a worker reach a worker outside its
//! groups by @-mentioning it, but "workers can't do it automatically it needs
//! to be explicitly done via a human via the input". Reachability is observed
//! through `request_to`, which answers to the same peer resolver as a send.
//!
//! Its own process: `AMUX_HOME` points at a fixture fleet of env files.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use std::sync::OnceLock;
use tower::ServiceExt;

const AUTH: &str = "mention-grant-test-token";

fn fleet_home() -> &'static std::path::Path {
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = amux_server::test_tempdirs::keep(tempfile::tempdir().expect("tempdir"));
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        for (n, g) in [("mg-a", "alpha"), ("mg-b", "beta"), ("mg-c", "beta"), ("mg-w", "alpha")] {
            std::fs::write(sessions.join(format!("{n}.env")), format!("CC_DIR=/tmp\nCC_TAGS={g}\n")).expect("env");
        }
        std::env::set_var("AMUX_HOME", dir.path());
        std::env::set_var("AMUX_APPROVAL_TYPES", "*");
        std::env::set_var("AMUX_BOARD_DELEGATION", "1");
        dir.path().to_path_buf()
    })
    .as_path()
}

fn app() -> (axum::Router, tempfile::TempDir) {
    let _ = fleet_home();
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(Store::open(&dir.path().join("amux-test.db")).unwrap());
    let state = AppState {
        store,
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: Some(AUTH.into()),
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), dir)
}

fn ui_token() -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(format!("amux-ui-guard:{AUTH}").as_bytes());
    hex::encode(h.finalize()).chars().take(40).collect()
}

async fn call(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Can `from` hand `to` work? Uses the shared peer resolver.
async fn reaches(app: &axum::Router, from: &str, to: &str) -> bool {
    let req = Request::builder()
        .method("POST")
        .uri("/api/board")
        .header("X-Amux-Worker", from)
        .header("Authorization", format!("Bearer {AUTH}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"title": format!("mention grant probe {from}->{to}"), "request_to": to}).to_string()))
        .unwrap();
    let (s, v) = call(app, req).await;
    if s.is_success() {
        return true;
    }
    assert_eq!(v["code"].as_str().or(v["error_code"].as_str()).unwrap_or(""), "peer_interaction_refused",
        "a refusal must be the peer gate, not something else: {s} {v}");
    false
}

/// A send to `to`. `worker` sets the worker header; `ui` adds the UI token.
async fn send(app: &axum::Router, to: &str, text: &str, worker: Option<&str>, ui: bool) {
    let mut b = Request::builder()
        .method("POST")
        .uri(format!("/api/sessions/{to}/send"))
        .header("Authorization", format!("Bearer {AUTH}"))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(w) = worker {
        b = b.header("X-Amux-Session", w);
    }
    if ui {
        b = b.header("X-Amux-UI-Token", ui_token());
    }
    let _ = call(app, b.body(Body::from(json!({"text": text, "record_history": true}).to_string())).unwrap()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_owners_own_input_opens_a_one_way_grant() {
    let (app, _d) = app();
    assert!(!reaches(&app, "mg-a", "mg-b").await, "different groups start refused");

    // A bare API call with no worker header is not the dashboard.
    send(&app, "mg-a", "@mg-b please check the deploy", None, false).await;
    assert!(!reaches(&app, "mg-a", "mg-b").await, "no UI token, no grant");

    // A worker mentioning someone in its own send grants nothing, even with the
    // token. mg-w shares mg-a's group, so the send itself is allowed and only
    // the grant guard can stop it.
    send(&app, "mg-a", "@mg-b hello", Some("mg-w"), true).await;
    assert!(!reaches(&app, "mg-a", "mg-b").await, "a worker's send cannot create a grant");

    // The owner's dashboard send that @-mentions mg-b opens mg-a -> mg-b.
    send(&app, "mg-a", "ask @mg-b for the deploy status", None, true).await;
    assert!(reaches(&app, "mg-a", "mg-b").await, "the owner's @ grants mg-a -> mg-b");
    assert!(!reaches(&app, "mg-b", "mg-a").await, "one way");
    assert!(!reaches(&app, "mg-a", "mg-c").await, "only the worker that was mentioned");

    let grants: Value = serde_json::from_slice(&std::fs::read(fleet_home().join("mention-grants.json")).unwrap()).unwrap();
    let pairs: Vec<(String, String)> = grants.as_array().unwrap().iter()
        .map(|g| (g["from"].as_str().unwrap().into(), g["to"].as_str().unwrap().into())).collect();
    assert_eq!(pairs, vec![("mg-a".to_string(), "mg-b".to_string())], "exactly one grant was recorded");
}
