//! The same owner request sent to two lanes is named on the second send
//! (AF-985, api/repeat_ask.rs).
//!
//! 2026-10-28: "make the ability in the amux Mac OS app to apply a zoom
//! in/zoom out to all windows" went to amux-frustrations as MSG-99572 and to
//! amux-admin as MSG-99574 88 seconds later. Both held it, one built it, and
//! neither was told about the other.
//!
//! Its own process: `AMUX_HOME` points at a fixture fleet of env files.

use amux_server::api::{router, AppState};
use amux_server::db::{Store, WriteOutcome};
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use tower::ServiceExt;

const AUTH: &str = "owner-request-elsewhere-token";
const ZOOM: &str = "make the ability in the amux Mac OS app to apply a zoom in/zoom out to all windows";

fn fleet_home() -> &'static std::path::Path {
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = amux_server::test_tempdirs::keep(tempfile::tempdir().expect("tempdir"));
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        for n in ["re-a", "re-b", "re-w"] {
            std::fs::write(sessions.join(format!("{n}.env")), "CC_DIR=/tmp\nCC_TAGS=alpha\n").expect("env");
        }
        std::env::set_var("AMUX_HOME", dir.path());
        dir.path().to_path_buf()
    })
    .as_path()
}

fn app() -> (axum::Router, Arc<Store>, tempfile::TempDir) {
    let _ = fleet_home();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("amux-test.db")).unwrap());
    let state = AppState {
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: Some(AUTH.into()),
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), store, dir)
}

async fn send(app: &axum::Router, to: &str, text: &str, worker: Option<&str>) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method("POST")
        .uri(format!("/api/sessions/{to}/send"))
        .header("Authorization", format!("Bearer {AUTH}"))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(w) = worker {
        b = b.header("X-Amux-Session", w);
    }
    let res = app
        .clone()
        // The dashboard records history for the owner's sends; a peer's
        // `amux send` does not, and that is the path that reads its origin.
        .oneshot(b.body(Body::from(json!({"text": text, "record_history": worker.is_none(), "no_board": true}).to_string())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_second_lane_send_names_the_first() {
    let (app, store, _d) = app();
    let then = chrono::Utc::now().timestamp_millis() - 88_000;
    store
        .write(move |c| {
            c.execute(
                "INSERT INTO cmd_history(id,text,type,session,ts,origin,delivery) VALUES(99572,?1,'user','re-a',?2,'','queued')",
                rusqlite::params![format!("[02:36 PM] {ZOOM}"), then],
            )?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .unwrap();

    let (_, r) = send(&app, "re-b", &format!("[02:37 PM] {ZOOM}"), None).await;
    let e = &r["same_request_elsewhere"];
    assert_eq!(e["id"], "MSG-99572", "the owner's second send names the first: {r}");
    assert_eq!(e["session"], "re-a", "{r}");
    assert!((80..=120).contains(&e["age_s"].as_i64().unwrap()), "about 88 s, at whole-second precision: {r}");

    // A different request, and a peer's send of the same words, name nothing.
    let (_, r) = send(&app, "re-b", "fix the editor edit button that goes away when zoomed in", None).await;
    assert!(r.get("same_request_elsewhere").is_none(), "{r}");
    let (_, r) = send(&app, "re-b", ZOOM, Some("re-w")).await;
    assert!(r.get("same_request_elsewhere").is_none(), "a peer relay is not the owner: {r}");
}
