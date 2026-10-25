//! The dashboard version is derived from content and stamped at serve time.
//!
//! app.js and sw.js hold `__AMUX_ASSET_VER__` in source. Requested through the
//! real router, both must come back with the placeholder replaced by the build
//! hash, and that hash must be the one this test recomputes from `static/`
//! with an independent copy of the recipe in `crates/amux-dashboard/src/version.rs`
//! (which build.rs and the runtime override both compile). If the served
//! bytes kept the placeholder, every deploy would reuse one SW cache name and
//! no open page would ever reload onto new code.

use amux_server::api::{router, AppState};
use axum::body::Body;
use axum::http::Request;
use std::path::{Path, PathBuf};
use tower::ServiceExt;

fn app() -> axum::Router {
    let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let store = amux_server::db::Store::open(&dir.path().join("v.db")).unwrap();
    router(AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    })
}

async fn get(path: &str) -> (u16, String, String) {
    let res = app()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let content_type = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = axum::body::to_bytes(res.into_body(), 64 << 20)
        .await
        .unwrap();
    (
        status,
        content_type,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if !entry.file_name().to_string_lossy().starts_with('.') {
            out.push(path);
        }
    }
}

/// The directory the server is serving: `AMUX_DASHBOARD_DIR` when set (CI
/// runs main's compiled tests against a PR's static/ that way), else the
/// checkout's static/, which is what the binary embedded.
fn served_root() -> PathBuf {
    match std::env::var_os(amux_dashboard::DASHBOARD_DIR_ENV).filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../amux-dashboard/static"),
    }
}

/// Independent copy of the version recipe, so a drift in either is caught.
fn recompute_asset_version() -> String {
    let root = served_root();
    let mut files = Vec::new();
    collect(&root, &mut files);
    let mut keyed: Vec<(String, PathBuf)> = files
        .into_iter()
        .map(|p| {
            let rel = p
                .strip_prefix(&root)
                .unwrap()
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            (rel, p)
        })
        .collect();
    keyed.sort();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for (name, path) in keyed {
        let bytes = std::fs::read(path).unwrap();
        feed(name.as_bytes());
        feed(&[0]);
        feed(&(bytes.len() as u64).to_le_bytes());
        feed(&bytes);
    }
    format!("{h:016x}")[..12].to_string()
}

#[tokio::test]
async fn served_app_js_and_sw_js_carry_the_build_hash() {
    let ver = amux_dashboard::asset_version();
    let placeholder = amux_dashboard::ASSET_VER_PLACEHOLDER;
    assert_eq!(
        ver,
        recompute_asset_version(),
        "the served version must be the content hash of the served static/ (version.rs recipe)"
    );
    if amux_dashboard::dir_override().is_none() {
        assert_eq!(ver, amux_dashboard::ASSET_VER, "no override: the embedded hash is served");
    }

    let (status, ctype, app) = get("/app.js").await;
    assert_eq!(status, 200);
    assert!(ctype.starts_with("text/javascript"), "{ctype}");
    assert!(
        !app.contains(placeholder),
        "served app.js still holds the placeholder"
    );
    assert!(
        app.contains(&format!("const APP_VER = '{ver}';")),
        "served app.js must declare APP_VER = the build hash"
    );

    let (status, _, sw) = get("/sw.js").await;
    assert_eq!(status, 200);
    assert!(
        !sw.contains(placeholder),
        "served sw.js still holds the placeholder"
    );
    assert!(
        sw.contains(&format!("const CACHE = 'amux-v{ver}';")),
        "served sw.js must name its cache after the build hash"
    );

    // CONTROL: the raw embed really does hold the placeholder, so the two
    // assertions above prove a substitution happened rather than passing on
    // a file that never had the token.
    let raw = amux_dashboard::raw_asset("app.js").unwrap();
    assert!(String::from_utf8_lossy(&raw).contains(placeholder));
}

/// The kernel no longer bundles the registry, so the registry must be served
/// on its own at the URL state/feedback.mjs fetches and sw.js precaches.
#[tokio::test]
async fn the_control_registry_is_served_beside_the_kernel() {
    let (status, ctype, body) = get("/state/control-registry.json").await;
    assert_eq!(status, 200);
    assert_eq!(ctype, "application/json");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        v["command_handlers"]
            .as_object()
            .is_some_and(|o| !o.is_empty()),
        "registry must carry command handlers"
    );
    let (_, _, kernel) = get("/state/kernel.js").await;
    assert!(
        kernel.contains("/state/control-registry.json"),
        "kernel must fetch the registry at runtime"
    );
    let (_, _, sw) = get("/sw.js").await;
    assert!(
        sw.contains("'/state/control-registry.json'"),
        "the SW must precache the registry for offline use"
    );
}

/// Dashboard files revalidate: an ETag of the dashboard hash and no-cache on
/// the way out, a bodiless 304 when the browser already holds that version.
/// Before this every load without the service worker re-sent app.js in full.
#[tokio::test]
async fn dashboard_files_answer_304_to_the_version_the_browser_holds() {
    let res = app()
        .oneshot(Request::builder().uri("/app.js").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    let etag = res.headers().get("etag").expect("etag").to_str().unwrap().to_string();
    assert!(etag.contains(amux_dashboard::asset_version()), "{etag}");
    assert_eq!(res.headers().get("cache-control").unwrap(), "no-cache");

    let again = app()
        .oneshot(Request::builder().uri("/app.js").header("if-none-match", etag.as_str()).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(again.status().as_u16(), 304);
    let body = axum::body::to_bytes(again.into_body(), 1 << 20).await.unwrap();
    assert!(body.is_empty(), "a 304 carries no body");

    // A different version, or a path that is not a dashboard file, is a normal answer.
    let stale = app()
        .oneshot(Request::builder().uri("/app.js").header("if-none-match", "\"d-old\"").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(stale.status().as_u16(), 200);
}
