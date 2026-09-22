//! A nested API root must answer the same with or without a trailing slash
//! (AMUX-4753).
//!
//! Measured on build b971a3e9, against the running server: 11 of 12 nested
//! roots answered 200 bare and 404 on the slash, because they are mounted as
//! `.nest("/api/x", Router::new().route("/", ..))` and axum 0.6 dropped
//! automatic trailing-slash redirects.
//!
//! Real clients send the slash form (12 GETs to `/api/board-lifecycle/` from
//! Python-urllib/3.11 on 2026-09-15, every one a 404), and amux disagreed with
//! itself about it: `match_route_full` trims slashes, so
//! `route.mounted_routes_answer` judged the slash form as a mounted route and
//! failed it while axum refused to serve it.
//!
//! WHY THE ASSERTION IS AGREEMENT RATHER THAN 200. A root can answer 500 in a
//! test process for its own reasons (no daemon, empty store), and pinning 200
//! would make this file red about something else entirely. The property the
//! card is about is that the two spellings reach one handler, which is true
//! whatever that handler answers.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

/// The roots the card measured, plus the five more this file found.
const NESTED_ROOTS: &[&str] = &[
    "/api/board-lifecycle",
    "/api/board",
    "/api/workers",
    "/api/schedules",
    "/api/memories",
    "/api/groups",
    "/api/messages",
    "/api/search",
    "/api/why",
    "/api/policy",
    "/api/prefs",
];

fn app() -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("amux-test.db")).unwrap();
    let state = AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), dir)
}

async fn status_of(app: &axum::Router, path: &str) -> StatusCode {
    app.clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn every_nested_api_root_answers_the_same_with_or_without_a_trailing_slash() {
    let (app, _dir) = app();
    let mut disagreed = Vec::new();
    for root in NESTED_ROOTS {
        let bare = status_of(&app, root).await;
        let slash = status_of(&app, &format!("{root}/")).await;
        if bare != slash {
            disagreed.push(format!("{root}: bare={bare} slash={slash}"));
        }
        // The pre-fix symptom, stated so the cell cannot pass by every root
        // 404ing on both spellings.
        assert_ne!(
            bare,
            StatusCode::NOT_FOUND,
            "{root} does not answer at all, so this file is measuring the wrong thing"
        );
    }
    assert!(
        disagreed.is_empty(),
        "these roots answer differently depending on a trailing slash: {disagreed:?}"
    );
}

/// THE CONTROL. The fix is "a declared route tolerates the slash", not "a
/// trailing slash never 404s". A path nothing declares must keep its 404, with
/// the catalog and nearest_routes it already gets.
#[tokio::test]
async fn an_undeclared_path_still_404s_with_or_without_the_slash() {
    let (app, _dir) = app();
    for path in [
        "/api/definitely-not-a-route",
        "/api/definitely-not-a-route/",
    ] {
        assert_eq!(
            status_of(&app, path).await,
            StatusCode::NOT_FOUND,
            "{path} is not declared and must not be rescued by slash handling"
        );
    }
}

/// The root itself is never rewritten: stripping its slash leaves the empty
/// path. It serves the SPA shell, so anything other than a 404 proves it was
/// routed normally.
#[tokio::test]
async fn the_bare_root_is_left_alone() {
    let (app, _dir) = app();
    assert_ne!(status_of(&app, "/").await, StatusCode::NOT_FOUND);
}
