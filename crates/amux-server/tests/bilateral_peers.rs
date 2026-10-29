//! `CC_PEERS`: bilateral peer-to-peer channels that bypass both isolation and
//! group boundaries. Both sides must name each other; a unilateral listing is
//! not enough.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use std::sync::OnceLock;
use tower::ServiceExt;

fn fleet_home() -> &'static std::path::Path {
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = amux_server::test_tempdirs::keep(tempfile::tempdir().expect("tempdir"));
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");

        // Two isolated workers that are bilateral peers.
        std::fs::write(
            sessions.join("iso-a.env"),
            "CC_DIR=/tmp\nCC_ISOLATED=1\nCC_TAGS=alpha\nCC_PEERS=\"iso-b\"\n",
        )
        .expect("iso-a");
        std::fs::write(
            sessions.join("iso-b.env"),
            "CC_DIR=/tmp\nCC_ISOLATED=1\nCC_TAGS=beta\nCC_PEERS=\"iso-a\"\n",
        )
        .expect("iso-b");

        // An isolated worker that names iso-a but iso-a does NOT name it back.
        std::fs::write(
            sessions.join("iso-unilateral.env"),
            "CC_DIR=/tmp\nCC_ISOLATED=1\nCC_TAGS=gamma\nCC_PEERS=\"iso-a\"\n",
        )
        .expect("iso-unilateral");

        // A non-isolated worker in a different group, bilateral peer with iso-a.
        std::fs::write(
            sessions.join("open-peer.env"),
            "CC_DIR=/tmp\nCC_TAGS=delta\nCC_PEERS=\"iso-a\"\n",
        )
        .expect("open-peer");
        // iso-a also names open-peer (update its line).
        std::fs::write(
            sessions.join("iso-a.env"),
            "CC_DIR=/tmp\nCC_ISOLATED=1\nCC_TAGS=alpha\nCC_PEERS=\"iso-b,open-peer\"\n",
        )
        .expect("iso-a updated");

        // A plain worker with no peers and no shared group with the above.
        std::fs::write(
            sessions.join("stranger.env"),
            "CC_DIR=/tmp\nCC_TAGS=zeta\n",
        )
        .expect("stranger");

        std::env::set_var("AMUX_HOME", dir.path());
        dir.path().to_path_buf()
    })
    .as_path()
}

fn app() -> (axum::Router, std::sync::Arc<Store>, tempfile::TempDir) {
    let _ = fleet_home();
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(Store::open(&dir.path().join("amux-test.db")).unwrap());
    let state = AppState {
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), store, dir)
}

async fn send_as(app: &axum::Router, from: &str, to: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/sessions/{to}/send"))
        .header("X-Amux-Worker", from)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"text": "hello"}).to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

#[tokio::test]
async fn bilateral_peers_bypass_isolation_and_group_boundary() {
    let (app, _store, _dir) = app();

    // iso-a -> iso-b: bilateral peers, both isolated, different groups. Should succeed.
    let (st, v) = send_as(&app, "iso-a", "iso-b").await;
    assert_ne!(
        st,
        StatusCode::FORBIDDEN,
        "bilateral peers should bypass isolation + group boundary: {v}"
    );
}

#[tokio::test]
async fn bilateral_peers_work_in_both_directions() {
    let (app, _store, _dir) = app();

    // iso-b -> iso-a: the reverse direction should also work.
    let (st, v) = send_as(&app, "iso-b", "iso-a").await;
    assert_ne!(
        st,
        StatusCode::FORBIDDEN,
        "bilateral peers should work in both directions: {v}"
    );
}

#[tokio::test]
async fn unilateral_peer_listing_is_refused() {
    let (app, _store, _dir) = app();

    // iso-unilateral -> iso-a: unilateral names iso-a, but iso-a does not name
    // unilateral back. Should be refused.
    let (st, v) = send_as(&app, "iso-unilateral", "iso-a").await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "unilateral peer listing should still be refused: {v}"
    );
}

#[tokio::test]
async fn non_peer_stranger_is_refused_by_isolated_target() {
    let (app, _store, _dir) = app();

    // stranger -> iso-a: no peer relationship, no shared group. Refused.
    let (st, v) = send_as(&app, "stranger", "iso-a").await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "a stranger should not reach an isolated worker: {v}"
    );
}

#[tokio::test]
async fn bilateral_peers_cross_group_non_isolated() {
    let (app, _store, _dir) = app();

    // open-peer -> iso-a: bilateral peers, different groups, target is isolated.
    let (st, v) = send_as(&app, "open-peer", "iso-a").await;
    assert_ne!(
        st,
        StatusCode::FORBIDDEN,
        "bilateral peers should bypass isolation from a non-isolated sender: {v}"
    );
}
