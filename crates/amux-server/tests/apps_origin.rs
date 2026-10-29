//! Apps inside amux on their own origin (`/app/<id>/`, apps_origin.rs), through
//! the real routers: the dashboard origin hands a signed-in person a one-time
//! link, the apps origin trades it for a cookie bound to that one app, and
//! serves nothing else. Replaces tests/port_proxy.rs (`/proxy/` is gone).

use amux_server::api::{apps_router, router, AppState};
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

const TOKEN: &str = "owner-secret-for-apps-test";
const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"; // approved
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"; // pending (a worker asked)
const C: &str = "cccccccccccccccccccccccccccccccc"; // approved, another app
const D: &str = "dddddddddddddddddddddddddddddddd"; // approved, but a refused port

fn home() -> &'static std::path::Path {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let h = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AMUX_HOME", h.path());
            std::env::set_var("AMUX_APPS_URL", "https://apps.test:8443");
        }
        h
    })
    .path()
}

fn dashboard() -> axum::Router {
    home();
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

fn owner_cookie() -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(format!("amux-owner-session:{TOKEN}"));
    format!("__Host-amux_owner={}", hex::encode(h.finalize()))
}

/// A loopback app that answers with the request head it received.
async fn echo_app() -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).replace('<', "&lt;");
                let body = format!("<h1>internal app</h1><pre>{head}</pre>");
                let _ = s
                    .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nSet-Cookie: amux_member=evil; Path=/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
                    .await;
            });
        }
    });
    port
}

/// The tests share one AMUX_HOME and one links file, and each test's loopback
/// app stops when that test ends: a test that writes the file holds this
/// until it is done, so another cannot point its apps at a dead port.
async fn links_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static L: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    L.lock().await
}

fn write_links(port: u16) {
    let links = serde_json::json!({"links": [
        {"id": A, "kind": "internal", "port": port, "approved_by": "owner"},
        {"id": B, "kind": "internal", "port": port, "approved_by": ""},
        {"id": C, "kind": "internal", "port": port, "approved_by": "owner"},
        {"id": D, "kind": "internal", "port": 2019, "approved_by": "owner"},
    ]});
    std::fs::write(home().join("public-links.json"), links.to_string()).unwrap();
}

async fn open(id: &str, person: bool) -> (StatusCode, serde_json::Value) {
    let mut b = Request::builder().method("POST").uri(format!("/api/page-links/{id}/open"))
        .header(header::HOST, "sandbox.amux.io").header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    if person {
        b = b.header(header::COOKIE, owner_cookie());
    }
    let res = dashboard().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
    let st = res.status();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (st, serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null))
}

async fn app_get(path: &str, cookie: Option<&str>) -> (StatusCode, String, String, String) {
    let mut b = Request::builder().uri(path).header(header::HOST, "apps.test:8443");
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    let res = apps_router().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
    let st = res.status();
    let loc = res.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let set = res.headers().get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).collect::<Vec<_>>().join(" | ");
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (st, loc, set, String::from_utf8_lossy(&body).to_string())
}

#[tokio::test]
async fn an_app_opens_on_its_own_origin_only_from_pages_and_only_itself() {
    let _links = links_lock().await;
    let port = echo_app().await;
    write_links(port);

    // Only a signed-in person gets a handoff, and only for an approved app.
    let (st, _) = open(A, false).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "a worker's bearer got an app handoff");
    let (st, _) = open(B, true).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "a pending app got a handoff");
    let (st, v) = open(A, true).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let url = v["url"].as_str().unwrap().to_string();
    assert!(url.starts_with(&format!("https://apps.test:8443/app/{A}/?amux_t=")), "{url}");
    let path = url.trim_start_matches("https://apps.test:8443");

    // Without the handoff or a cookie: nothing.
    let (st, _, _, body) = app_get(&format!("/app/{A}/"), None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("Apps tab"), "{body}");

    // The token is traded for a cookie bound to this app, once.
    let (st, loc, set, _) = app_get(path, None).await;
    assert_eq!((st, loc.as_str()), (StatusCode::SEE_OTHER, format!("/app/{A}/").as_str()));
    assert!(set.contains(&format!("amux_app_{A}=")) && set.contains("HttpOnly") && set.contains(&format!("Path=/app/{A}/")), "{set}");
    let session = set.split(';').next().unwrap().to_string();
    let (st, _, _, _) = app_get(path, None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "a handoff token worked twice");

    // With the cookie: the app, and nothing of amux's reaches it or comes back.
    let cookies = format!("amux_door=d; __Host-amux_owner=o; appsid=1; {session}");
    let (st, _, set, body) = app_get(&format!("/app/{A}/page?x=1"), Some(&cookies)).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body.contains("<h1>internal app</h1>") && body.contains("GET /page?x=1 "), "{body}");
    assert!(body.contains("appsid=1"), "the app's own cookie passes: {body}");
    for leaked in ["amux_door", "amux_owner", "amux_app_", TOKEN] {
        assert!(!body.contains(leaked), "{leaked} reached the app: {body}");
    }
    assert!(!set.contains("amux_member"), "the app set an amux cookie: {set}");

    // That cookie opens this app only; a planted value opens nothing.
    let other = session.replace(A, C);
    let (st, _, _, _) = app_get(&format!("/app/{C}/"), Some(&other)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "one app's session opened another");
    let (st, _, _, _) = app_get(&format!("/app/{A}/"), Some(&format!("amux_app_{A}=planted"))).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "a planted cookie opened the app");

    // A pending app, a refused port, and anything that is not /app/: never.
    let (st, _, _, _) = app_get(&format!("/app/{B}/"), Some(&session.replace(A, B))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _, _, _) = app_get(&format!("/app/{D}/"), None).await;
    assert_ne!(st, StatusCode::OK, "Caddy's admin port opened");
    for p in ["/", "/api/sessions", "/api/page-links", "/pub/x/", "/proxy/8891/"] {
        let (st, _, _, _) = app_get(p, None).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "the apps origin served {p}");
    }
}

/// `/proxy/<port>/` was removed (Ethan, 2026-10-25: "remove the proxies"): the
/// dashboard answers it like any unknown path, never by relaying an app.
#[tokio::test]
async fn the_dashboard_no_longer_serves_proxy_links() {
    let port = echo_app().await;
    let res = dashboard()
        .oneshot(Request::builder().uri(format!("/proxy/{port}/")).header(header::HOST, "localhost:8824")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}")).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("internal app"), "/proxy/ relayed an app");
}

/// A file inside amux, added to the Apps tab by the owner: one HTML file with no
/// server started, and only that file. Before this a worker served a whole
/// folder with `python3 -m http.server` to show one deck (sandbox rothco,
/// 2026-10-26), which listed its build scripts beside it.
///
/// A worker may not put a file for the owner in the Apps tab (Ethan, 2026-10-28: "it
/// is making page apps for just files"): it is refused with the file's link in
/// amux's file viewer, which is what it should hand over instead.
#[tokio::test]
async fn a_file_inside_amux_opens_from_pages_and_serves_only_itself() {
    let _links = links_lock().await;
    let port = echo_app().await;
    write_links(port);
    let home_dir = std::path::PathBuf::from(std::env::var("HOME").unwrap());
    let base = tempfile::Builder::new().prefix("appfile-").tempdir_in(&home_dir).unwrap();
    let deck = base.path().join("deck.html");
    std::fs::write(&deck, "<h1>the deck</h1>").unwrap();
    std::fs::write(base.path().join("build_deck.py"), "SECRET_SCRIPT").unwrap();

    // A worker is refused and told the file's link; the owner's request is live.
    let make = |cookie: bool| {
        let mut b = Request::builder().method("POST").uri("/api/page-links").header(header::HOST, "sandbox.amux.io")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}")).header(header::CONTENT_TYPE, "application/json")
            .header("x-amux-session", "rothco");
        if cookie {
            b = b.header(header::COOKIE, owner_cookie());
        }
        b.body(Body::from(serde_json::json!({"kind": "internal", "path": deck, "title": "Deck"}).to_string())).unwrap()
    };
    let res = dashboard().oneshot(make(false)).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(v["code"].as_str(), Some("file_not_a_page"), "{v}");
    let canon = deck.canonicalize().unwrap();
    let link = v["file_link"].as_str().unwrap();
    assert!(link.ends_with(&format!("/f{}", canon.to_string_lossy())), "the refusal names the file's link: {link}");
    assert!(v["error"].as_str().unwrap().contains(link), "{v}");
    let res = dashboard()
        .oneshot(Request::builder().uri("/api/page-links").header(header::HOST, "sandbox.amux.io")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}")).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let listed = String::from_utf8_lossy(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).to_string();
    assert!(!listed.contains("deck.html"), "a refused file was still added to Apps: {listed}");

    let res = dashboard().oneshot(make(true)).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    assert_eq!(v["pending"].as_bool(), Some(false), "{v}");
    let (st, h) = open(&id, true).await;
    assert_eq!(st, StatusCode::OK, "{h}");
    let path = h["url"].as_str().unwrap().trim_start_matches("https://apps.test:8443").to_string();
    let (_, _, set, _) = app_get(&path, None).await;
    let session = set.split(';').next().unwrap().to_string();

    let res = apps_router()
        .oneshot(Request::builder().uri(format!("/app/{id}/")).header(header::HOST, "apps.test:8443")
            .header(header::COOKIE, &session).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let csp: Vec<String> = res.headers().get_all(header::CONTENT_SECURITY_POLICY).iter().filter_map(|v| v.to_str().ok().map(str::to_string)).collect();
    assert!(res.headers().get(header::X_FRAME_OPTIONS).is_none(), "the Apps tab could not frame it");
    assert!(csp.iter().any(|c| c.starts_with("frame-ancestors ") && c.contains(" https://apps.test:")), "anyone may frame it: {csp:?}");
    let body = String::from_utf8_lossy(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).to_string();
    assert!(body.contains("the deck"), "{body}");
    for p in ["build_deck.py", "../build_deck.py", "%2e%2e/build_deck.py"] {
        let (st, _, _, body) = app_get(&format!("/app/{id}/{p}"), Some(&session)).await;
        assert!(st != StatusCode::OK && !body.contains("SECRET_SCRIPT"), "a file beside the deck was served at {p}: {st}");
    }
    // Without its session: nothing.
    let (st, _, _, body) = app_get(&format!("/app/{id}/"), None).await;
    assert!(st == StatusCode::FORBIDDEN && !body.contains("the deck"), "{st}");
}

async fn app_get_as(path: &str, cookie: Option<&str>, dest: &str) -> (StatusCode, String, String) {
    let mut b = Request::builder().uri(path).header(header::HOST, "apps.test:8443").header("sec-fetch-dest", dest);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    let res = apps_router().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
    let st = res.status();
    let loc = res.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (st, loc, String::from_utf8_lossy(&body).to_string())
}

/// A page the app opens in a tab of its own, or a page whose session lapsed
/// in the viewer, used to land on "Open this app from the Apps tab" (owner,
/// 2026-10-29: every Landing link in the GTM demos). A tab now goes to the
/// Apps tab at that page; a page in the viewer asks the viewer to reopen it.
#[tokio::test]
async fn an_app_page_without_its_session_reopens_at_that_page() {
    let _links = links_lock().await;
    let port = echo_app().await;
    write_links(port);
    let (st, loc, _) = app_get_as(&format!("/app/{A}/landing/core.html?x=1"), None, "document").await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert!(loc.ends_with(&format!("/#view=apps&open={A}&at=/landing/core.html%3Fx%3D1")), "{loc}");
    assert!(loc.starts_with("https://") && !loc.contains(":8443"), "the Apps tab is on the dashboard, not the apps origin: {loc}");
    let (st, _, body) = app_get_as(&format!("/app/{A}/landing/core.html"), None, "iframe").await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(body.contains("amux_app_reopen") && body.contains("/landing/core.html"), "{body}");
    // Every HTML page loads the viewer script, which the origin serves.
    let res = dashboard().oneshot(Request::builder().method("POST").uri(format!("/api/page-links/{A}/open"))
        .header(header::HOST, "sandbox.amux.io").header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::COOKIE, owner_cookie()).body(Body::empty()).unwrap()).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
    let path = v["url"].as_str().unwrap().trim_start_matches("https://apps.test:8443").to_string();
    let (_, _, set, _) = app_get(&path, None).await;
    let session = set.split(';').next().unwrap().to_string();
    let (st, _, _, body) = app_get(&format!("/app/{A}/"), Some(&session)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains(&format!("<script src=\"/app/{A}/.amux-viewer.js\"></script>")), "{body}");
    let (st, _, _, js) = app_get(&format!("/app/{A}/.amux-viewer.js"), None).await;
    assert!(st == StatusCode::OK && js.contains("amux_app_nav"), "{st} {js}");
}

/// The Files tab shows an HTML file as part of its folder's website: sibling
/// CSS and scripts load and links between pages work, without adding the
/// folder to Apps (owner, 2026-10-29). Before this the preview's sibling
/// requests carried no sign-in and the page rendered unstyled.
#[tokio::test]
async fn a_file_opens_as_its_folders_site_and_nothing_outside_it() {
    home();
    let home_dir = std::path::PathBuf::from(std::env::var("HOME").unwrap());
    let base = tempfile::Builder::new().prefix("sitefiles-").tempdir_in(&home_dir).unwrap();
    let site = base.path().join("demos");
    for d in ["landing", "design", "shared"] {
        std::fs::create_dir_all(site.join(d)).unwrap();
    }
    std::fs::write(base.path().join("outside.txt"), "OUTSIDE_SECRET").unwrap();
    std::fs::write(site.join("index.html"), "<!doctype html><html><head><title>hub</title></head><body><a href=\"landing/core.html\">L</a></body></html>").unwrap();
    std::fs::write(site.join("landing/core.html"), "<!doctype html><html><head><link rel=\"stylesheet\" href=\"../design/a.css\"></head><body>core page</body></html>").unwrap();
    std::fs::write(site.join("design/a.css"), "body{color:red}").unwrap();
    std::fs::write(site.join(".env"), "HIDDEN").unwrap();

    let mint = |cookie: bool, path: &std::path::Path| {
        let mut b = Request::builder().method("POST").uri("/api/page-links/site-view").header(header::HOST, "sandbox.amux.io")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}")).header(header::CONTENT_TYPE, "application/json");
        if cookie {
            b = b.header(header::COOKIE, owner_cookie());
        }
        b.body(Body::from(serde_json::json!({"path": path}).to_string())).unwrap()
    };
    let res = dashboard().oneshot(mint(false, &site.join("landing/core.html"))).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "a worker's bearer got a site view");
    // Opened from a page one folder down, the site is the folder its ../ links need.
    let res = dashboard().oneshot(mint(true, &site.join("landing/core.html"))).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(v["root"].as_str(), Some(site.canonicalize().unwrap().to_str().unwrap()), "{v}");
    let url = v["url"].as_str().unwrap();
    assert!(url.ends_with("/landing/core.html"), "{url}");
    let path = url.trim_start_matches("https://apps.test:8443").to_string();
    let id = v["id"].as_str().unwrap().to_string();

    let res = apps_router().oneshot(Request::builder().uri(&path).header(header::HOST, "apps.test:8443").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let csp: Vec<String> = res.headers().get_all(header::CONTENT_SECURITY_POLICY).iter().filter_map(|v| v.to_str().ok().map(str::to_string)).collect();
    assert!(csp.iter().any(|c| c.starts_with("sandbox allow-scripts")) && !csp.iter().any(|c| c.contains("allow-same-origin")), "a site page is not sandboxed: {csp:?}");
    assert!(csp.iter().any(|c| c.starts_with("frame-ancestors ") && c.contains(" https://apps.test:")), "{csp:?}");
    let body = String::from_utf8_lossy(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).to_string();
    assert!(body.contains("core page") && body.contains(&format!("/app/{id}/.amux-viewer.js")), "{body}");
    let (st, _, _, css) = app_get(&format!("/app/{id}/design/a.css"), None).await;
    assert!(st == StatusCode::OK && css.contains("color:red"), "the sibling stylesheet did not load: {st} {css}");
    // As text/css: under nosniff a text/plain stylesheet is refused by the browser.
    let res = apps_router().oneshot(Request::builder().uri(format!("/app/{id}/design/a.css")).header(header::HOST, "apps.test:8443").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()), Some("text/css; charset=utf-8"));
    let (st, _, _, hub) = app_get(&format!("/app/{id}/"), None).await;
    assert!(st == StatusCode::OK && hub.contains("<title>hub</title>"), "{st}");
    for p in ["../outside.txt", "%2e%2e/outside.txt", ".env", "landing/../../outside.txt"] {
        let (st, _, _, body) = app_get(&format!("/app/{id}/{p}"), None).await;
        assert!(st != StatusCode::OK && !body.contains("OUTSIDE_SECRET") && !body.contains("HIDDEN"), "{p} was served: {st}");
    }
    // A view nobody minted asks the viewer to reopen it; it serves nothing.
    let (st, _, body) = app_get_as(&format!("/app/{}/landing/core.html", "e".repeat(32)), None, "iframe").await;
    assert!(st == StatusCode::FORBIDDEN && body.contains("amux_app_reopen") && !body.contains("core page"), "{st} {body}");
}
