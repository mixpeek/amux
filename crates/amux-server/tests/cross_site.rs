//! The cross-site guard through the composed router, as an amux Cloud server
//! runs it (no owner token: the cloud door's cookie is the authority).

use amux_server::api::{router, AppState};
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

fn app() -> axum::Router {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let home = HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe { std::env::set_var("AMUX_HOME", home.path()) };
    let db = tempfile::tempdir().unwrap();
    let store = amux_server::db::Store::open(&db.path().join("t.db")).unwrap();
    std::mem::forget(db);
    router(AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    })
}

async fn call(method: &str, path: &str, headers: &[(&str, &str)]) -> (StatusCode, String) {
    let mut b = Request::builder().method(method).uri(path).header(header::HOST, "sandbox.amux.io");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = b.header(header::CONTENT_TYPE, "text/plain").body(Body::from("x")).unwrap();
    let res = app().oneshot(req).await.unwrap();
    let st = res.status();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (st, String::from_utf8_lossy(&body).to_string())
}

#[tokio::test]
async fn a_neighbouring_site_cannot_change_anything_but_the_dashboard_and_cli_can() {
    let id = "0123456789abcdef0123456789abcdef";
    // Another customer's server is the "same site" as this one to a browser.
    for site in ["same-site", "cross-site"] {
        let (st, body) = call("POST", &format!("/api/page-links/{id}/approve"), &[("sec-fetch-site", site)]).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{site}: {body}");
        assert!(body.contains("cross-site request refused"), "{body}");
    }
    let (st, body) = call("POST", "/api/page-links", &[("origin", "https://neighbour.amux.io")]).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "foreign Origin: {body}");
    // The dashboard (same origin) and header-less clients reach the handler.
    let (st, body) = call("POST", &format!("/api/page-links/{id}/approve"), &[("sec-fetch-site", "same-origin")]).await;
    assert!(!body.contains("cross-site request refused"), "same-origin was refused by the guard: {st} {body}");
    let (st, body) = call("POST", "/api/page-links", &[]).await;
    assert!(!body.contains("cross-site request refused"), "a CLI call was refused by the guard: {st} {body}");
    // Reads are not writes.
    let (st, body) = call("GET", "/api/page-links", &[("sec-fetch-site", "cross-site")]).await;
    assert_eq!(st, StatusCode::OK, "{body}");
}
