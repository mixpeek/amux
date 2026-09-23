//! A column sweep must be able to say who did it (AMUX-4755).
//!
//! Measured from the live issues table before this shipped: 373 cards
//! discarded in one minute on 2026-09-15, 123 on 09-16, 202 across six
//! minutes, 25, 29 — every one recorded as `api-anonymous`, because the
//! dashboard is not a worker and sends no `X-Amux-Session`. The cost was a
//! wrong diagnosis: two bursts landing in the same minute on consecutive days,
//! with no actor on either, read as a runaway daily job, and a peer lane
//! re-filed on that basis and was preparing to hunt for the job.
//!
//! These drive the REAL router, so what they pin is the shipped path: the
//! handler's resolution order, what it writes on each card, and what it
//! refuses to claim when there is no credential to verify.
//!
//! Its own process, because `require_bearer` is a router-wide layer and this
//! file needs one app with an owner token configured and one without.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

const OWNER: &str = "owner-bearer-for-this-test";

fn app(auth_token: Option<&str>) -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("amux-test.db")).unwrap();
    let state = AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: auth_token.map(String::from),
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), dir)
}

async fn send(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = match body {
        Some(v) => b
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, value)
}

/// Fill a column and return the created ids.
async fn seed_backlog(app: &axum::Router, headers: &[(&str, &str)], n: usize) -> Vec<String> {
    let mut ids = Vec::new();
    for i in 0..n {
        let (st, v) = send(
            app,
            "POST",
            "/api/board",
            Some(json!({
                "title": format!("sweep specimen {i}"),
                "status": "backlog",
                "session": "some-lane",
                "type": "chore",
            })),
            headers,
        )
        .await;
        assert!(st.is_success(), "seeding a backlog card: {st} {v}");
        ids.push(
            v["id"]
                .as_str()
                .expect("created card has an id")
                .to_string(),
        );
    }
    ids
}

async fn card_log(app: &axum::Router, id: &str, headers: &[(&str, &str)]) -> String {
    let (st, v) = send(app, "GET", &format!("/api/board/{id}"), None, headers).await;
    assert_eq!(st, StatusCode::OK, "reading back {id}: {v}");
    v["log"].as_str().unwrap_or_default().to_string()
}

/// THE CARD'S SUBJECT. A browser presenting the owner bearer and nothing else
/// must be named, on the response and on every card it moved.
///
/// The bearer is not a decoration here: `static_files::inject_bootstrap` puts
/// the owner token into the served shell and `app.js::_authHeaders` sends it on
/// every API call, so this is the request the dashboard actually makes. The
/// credential was already arriving; the handler was not reading it.
#[tokio::test]
async fn a_dashboard_column_sweep_names_the_owner_on_every_card_it_moved() {
    let (app, _dir) = app(Some(OWNER));
    let auth: &[(&str, &str)] = &[("authorization", &format!("Bearer {OWNER}"))];
    let ids = seed_backlog(&app, auth, 3).await;

    let (st, v) = send(
        &app,
        "POST",
        "/api/board/bulk-migrate",
        Some(json!({"from": "backlog", "to": "discarded"})),
        auth,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["moved"], json!(3), "{v}");
    assert_eq!(
        v["actor"], "owner-token",
        "the response must say who the sweep was recorded as: {v}"
    );

    for id in &ids {
        let log = card_log(&app, id, auth).await;
        assert!(
            log.contains("bulk-migrated backlog -> discarded by owner-token"),
            "{id} must name the actor on the card, got: {log}"
        );
        // THE COUNT IS THE DISCRIMINATOR. Without it the line reads exactly
        // like a decision made about this one card, which is how AF-398 and
        // AF-640 were both read as individually retired.
        assert!(
            log.contains("(column sweep, 3 cards considered)"),
            "{id} must say it was one of a sweep, and how many: {log}"
        );
    }
}

/// A WORKER KEEPS ITS OWN NAME. The one CLI site that sends this bearer sends
/// `X-Amux-Session` with it, so an upgrade that fired on every owner-credentialed
/// request would rename real lanes to "the owner" in their own audit trail.
///
/// This is the cell that pins the ORDER rather than the lookup, and it fails on
/// any implementation that checks the bearer first.
#[tokio::test]
async fn a_worker_that_also_holds_the_owner_bearer_is_still_the_worker() {
    let (app, _dir) = app(Some(OWNER));
    let bearer = format!("Bearer {OWNER}");
    let as_worker: &[(&str, &str)] = &[("authorization", &bearer), ("x-amux-session", "some-lane")];
    let ids = seed_backlog(&app, as_worker, 2).await;

    let (st, v) = send(
        &app,
        "POST",
        "/api/board/bulk-migrate",
        Some(json!({"from": "backlog", "to": "discarded"})),
        as_worker,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        v["actor"], "some-lane",
        "the worker's own name must win: {v}"
    );
    let log = card_log(&app, &ids[0], as_worker).await;
    assert!(
        log.contains("by some-lane"),
        "the card must record the worker, not the owner: {log}"
    );
    assert!(
        !log.contains("owner-token"),
        "a named caller must not be relabelled as the owner: {log}"
    );
}

/// THE TRUTHFUL PATH, and the control that makes the two cells above mean
/// something. On a server with no auth token configured there is no owner
/// credential to present, so there is no owner to name and the sweep stays
/// anonymous. `api-anonymous` is the honest answer there; a name would be an
/// actor nobody could check.
///
/// It is also the leg that would catch a fix implemented as "call it the owner
/// whenever nothing else answered", which passes both cells above.
#[tokio::test]
async fn with_no_owner_credential_configured_the_sweep_stays_anonymous() {
    let (app, _dir) = app(None);
    let ids = seed_backlog(&app, &[], 2).await;

    let (st, v) = send(
        &app,
        "POST",
        "/api/board/bulk-migrate",
        Some(json!({"from": "backlog", "to": "discarded"})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["moved"], json!(2), "{v}");
    assert_eq!(
        v["actor"], "api-anonymous",
        "with nothing to verify, the sweep must not claim an owner: {v}"
    );
    let log = card_log(&app, &ids[0], &[]).await;
    assert!(
        !log.contains("owner-token"),
        "an unverifiable caller must not be named the owner: {log}"
    );
    // The sweep is still declared as a sweep. The count is not conditional on
    // knowing who ran it.
    assert!(
        log.contains("(column sweep, 2 cards considered)"),
        "an anonymous sweep still has to say it was a sweep: {log}"
    );
}
