//! A 405 must SAY SO IN ITS BODY (AF-211).
//!
//! axum's method-not-allowed is `405` + `allow:` + a ZERO-LENGTH body. Correct
//! HTTP, and invisible to every recipe in this repo: CLAUDE.md's documented
//! idiom is `curl -sk <url>` with no `-i`, so a wrong method prints nothing at
//! all. Measured 2026-08-24 against the live server on `GET
//! /api/git/staged-guard` (POST-only): zero bytes out. The natural reading of
//! silence is "no such endpoint", which sends the reader hunting for a missing
//! route instead of changing one word in their command.
//!
//! That is ethos rule 7's empty-probe shape, except the server produces it —
//! and the answer was in a header the documented idiom never displays.
//!
//! This is an INTEGRATION test on purpose. The behaviour lives in a middleware
//! wrapped around the finished router, so a unit test of the handler could not
//! reach it, and the property is precisely that it applies to a route nobody
//! special-cased.

use amux_server::api::{router, AppState};
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;

fn app() -> axum::Router {
    let dir = tempfile::tempdir().unwrap();
    let store = amux_server::db::Store::open(&dir.path().join("t.db")).unwrap();
    std::mem::forget(dir);
    router(AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
    })
}

async fn call(method: Method, path: &str) -> (StatusCode, String, String) {
    let res = app()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let allow = res
        .headers()
        .get(axum::http::header::ALLOW)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, allow, String::from_utf8_lossy(&bytes).to_string())
}

/// The incident's own specimen: the exact request that printed nothing.
#[tokio::test]
async fn a_wrong_method_explains_itself_instead_of_returning_nothing() {
    let (status, allow, body) = call(Method::GET, "/api/git/staged-guard").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);

    // The whole point: a body exists at all. This is the assertion that was
    // false before the fix, and `curl -sk` shows exactly this.
    assert!(!body.is_empty(), "a 405 with an empty body is what AF-211 is about");

    let v: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("405 body must be JSON like every other error here: {e}: {body}"));
    assert_eq!(v["ok"], false);
    assert_eq!(v["method"], "GET");
    assert_eq!(v["path"], "/api/git/staged-guard");
    // The verb that WOULD have worked has to be in the body, not only in a
    // header the documented idiom never displays. Naming POST specifically,
    // because "allow is non-empty" would pass against a blank string too.
    assert!(
        v["allow"].as_str().unwrap_or("").contains("POST"),
        "the body must name the method that works: {body}"
    );
    assert!(
        v["error"].as_str().unwrap_or("").contains("POST"),
        "the human-readable line must name it too, not just a machine field: {body}"
    );
    assert!(
        v["hint"].as_str().unwrap_or("").contains("/api/debug/routes"),
        "route it to the instrument that answers 'what IS mounted here': {body}"
    );

    // The header a correct HTTP client reads must SURVIVE the rewrite — the
    // body is reconstructed from parts, and dropping `allow` would trade one
    // audience's answer for the other's.
    assert!(allow.contains("POST"), "allow header lost in the rewrite: {allow:?}");
}

/// THE CONTROL THAT ACTUALLY DISCRIMINATES: a 304 is empty ON PURPOSE.
///
/// Written after the first version of this file failed its own mutation test.
/// Deleting the `status != 405` guard left both cells below green, because
/// every response they exercise has a non-empty body and the second guard
/// (content-length 0) caught the mutation by accident. So the suite could not
/// see the over-broad direction at all, which is the direction that corrupts
/// the whole server rather than one route.
///
/// A 304 Not Modified is the discriminating fixture: non-405, and empty by
/// protocol. Filling it with JSON is a spec violation, and it is what the SPA's
/// conditional GETs would have received.
#[tokio::test]
async fn a_304_stays_empty_because_it_is_empty_on_purpose() {
    let app = app();
    let first = app
        .clone()
        .oneshot(Request::builder().uri("/api/board").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK, "fixture precondition");
    let etag = first
        .headers()
        .get(axum::http::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .expect("the board must serve an ETag for this control to exist")
        .to_string();

    let second = app
        .oneshot(
            Request::builder()
                .uri("/api/board")
                .header(axum::http::header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        second.status(),
        StatusCode::NOT_MODIFIED,
        "fixture precondition: the conditional GET must actually 304"
    );
    let bytes = axum::body::to_bytes(second.into_body(), usize::MAX).await.unwrap();
    assert!(
        bytes.is_empty(),
        "a 304 must stay empty — synthesizing a body here violates the spec and \
         breaks every conditional GET the SPA makes: {}",
        String::from_utf8_lossy(&bytes)
    );
}

/// NEGATIVE CONTROL. A layer that filled in a body unconditionally would pass
/// the test above and corrupt every other response on the server. Assert the
/// two shapes it must not touch.
#[tokio::test]
async fn a_correct_method_and_a_404_are_untouched() {
    // A route that exists and accepts this method: not a 405, real body.
    let (status, _, body) = call(Method::GET, "/health").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("amux-rust"),
        "the real handler's body must survive the layer: {body}"
    );

    // A path with no route at all is a 404, NOT a 405 — the distinction is the
    // entire diagnostic value, and a layer that answered 405 for everything
    // would erase it.
    let (status, _, _) = call(Method::POST, "/api/definitely-not-a-route-af211").await;
    assert_ne!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "an absent path must not be reported as a wrong method"
    );
}
