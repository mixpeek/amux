//! `AMUX_DASHBOARD_DIR` serves the dashboard from a directory instead of the
//! embed, with the asset version computed from that directory.
//!
//! CI relies on this to run a server and test binaries main already compiled
//! against a pull request's `static/` (rust.yml server-bin and test-build
//! caches). If the override silently served the embed, a dashboard PR would be
//! tested against main's dashboard and go green on bytes it never shipped, so
//! this pins both halves: the bytes come from the directory, and the version
//! is the directory's hash, not the embedded one.
//!
//! One test in this binary on purpose: the override is resolved once per
//! process, so the variable must be set before anything reads the dashboard.

use amux_server::api::{router, AppState};
use axum::body::Body;
use axum::http::Request;
use std::path::PathBuf;
use tower::ServiceExt;

async fn get(path: &str) -> (u16, String) {
    let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let store = amux_server::db::Store::open(&dir.path().join("o.db")).unwrap();
    let app = router(AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    });
    let res = app
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = axum::body::to_bytes(res.into_body(), 64 << 20).await.unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn a_dashboard_dir_override_serves_its_own_bytes_and_version() {
    let embedded_static = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../amux-dashboard/static");
    let tmp = tempfile::tempdir().unwrap();
    let marker = "/* dashboard_dir_override marker 7c1e */";
    let app_js = std::fs::read_to_string(embedded_static.join("app.js")).unwrap();
    std::fs::write(tmp.path().join("app.js"), format!("{marker}\n{app_js}")).unwrap();
    std::fs::copy(embedded_static.join("sw.js"), tmp.path().join("sw.js")).unwrap();
    std::env::set_var(amux_dashboard::DASHBOARD_DIR_ENV, tmp.path());

    let o = amux_dashboard::dir_override().expect("the override is active");
    assert_eq!(o.dir, tmp.path());
    let expected = amux_dashboard::dir_asset_version(tmp.path()).unwrap();
    assert_eq!(o.version, expected);
    assert_eq!(amux_dashboard::asset_version(), expected);
    assert_ne!(
        expected,
        amux_dashboard::ASSET_VER,
        "a modified app.js must move the version away from the embedded one"
    );

    let (status, app) = get("/app.js").await;
    assert_eq!(status, 200);
    assert!(app.starts_with(marker), "app.js must come from the directory");
    assert!(app.contains(&format!("const APP_VER = '{expected}';")));
    assert!(!app.contains(amux_dashboard::ASSET_VER_PLACEHOLDER));

    let (status, sw) = get("/sw.js").await;
    assert_eq!(status, 200);
    assert!(sw.contains(&format!("const CACHE = 'amux-v{expected}';")));

    // Raw bytes come from the directory too, placeholder intact.
    let raw = amux_dashboard::raw_asset("app.js").unwrap();
    let raw = String::from_utf8_lossy(&raw);
    assert!(raw.starts_with(marker));
    assert!(raw.contains(amux_dashboard::ASSET_VER_PLACEHOLDER));

    // A file absent from the directory is absent, not quietly taken from the
    // embed: mixing sources would make the version describe neither.
    assert!(amux_dashboard::asset("app.css").is_none());
    assert!(amux_dashboard::asset("../Cargo.toml").is_none());
}
