//! Copied file and worker links (api/deep_links.rs) through the real router:
//! with no credentials, /f/<path> and /w/<worker> redirect to the dashboard's
//! own deep link and Apple's association file names the app for both, while
//! the address links are built on stays owner-only.

use amux_server::api::{router, AppState};
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

const TOKEN: &str = "owner-secret-for-deep-links-test";

fn app() -> axum::Router {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let h = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AMUX_HOME", h.path()) };
        h
    });
    let db = tempfile::tempdir().unwrap();
    let store = amux_server::db::Store::open(&db.path().join("t.db")).unwrap();
    amux_server::test_tempdirs::keep(db);
    router(AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: Some(TOKEN.into()),
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    })
}

async fn get(path: &str, bearer: bool) -> (StatusCode, String, String) {
    let mut b = Request::builder().uri(path).header(header::HOST, "sandbox.amux.io");
    if bearer {
        b = b.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    }
    let res = app().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
    let st = res.status();
    let loc = res.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (st, loc, String::from_utf8_lossy(&body).to_string())
}

#[tokio::test]
async fn copied_links_open_their_file_or_worker_and_the_app_can_claim_them() {
    let (st, loc, _) = get("/f/home/amux/rothco/deck%20v2.html", false).await;
    assert_eq!((st, loc.as_str()), (StatusCode::FOUND, "/?via=link#path=/home/amux/rothco/deck%20v2.html"));
    let (st, loc, _) = get("/w/mtp-demo", false).await;
    assert_eq!((st, loc.as_str()), (StatusCode::FOUND, "/?via=link#peek=mtp-demo"));
    let (st, loc, _) = get("/w/a%26tab%3Dx", false).await;
    assert!(st == StatusCode::NOT_FOUND && loc.is_empty(), "{st} {loc}");

    let (st, _, body) = get("/.well-known/apple-app-site-association", false).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let d = &v["applinks"]["details"][0];
    assert_eq!(d["appIDs"][0], "M225B6YA42.com.EthanSteininger.nextup");
    let paths: Vec<&str> = d["components"].as_array().unwrap().iter().filter_map(|c| c["/"].as_str()).collect();
    assert_eq!(paths, ["/f/*", "/w/*"]);

    // The base is the owner's: no credentials, no answer.
    let (st, _, _) = get("/api/deep-link", false).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _, body) = get("/api/deep-link", true).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body.contains("\"file_prefix\":\"/f\"") && body.contains("\"measured\":true"), "{body}");
}
