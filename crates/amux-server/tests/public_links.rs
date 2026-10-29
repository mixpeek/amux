//! Public links (`/pub/<id>/`) through the composed router, with an owner
//! token set: the list and its changes need the owner, the link itself needs
//! nothing but its id, and nothing of amux's identity reaches the app.

use amux_server::api::{router, AppState};
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

const TOKEN: &str = "owner-secret-for-test";

fn app() -> axum::Router {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let home = HOME.get_or_init(|| tempfile::tempdir().unwrap());
    // Links live under the amux home; this binary owns its own.
    unsafe { std::env::set_var("AMUX_HOME", home.path()) };
    // A database per router: tests run in parallel and one file has one owner.
    let db = tempfile::tempdir().unwrap();
    let store = amux_server::db::Store::open(&db.path().join("t.db")).unwrap();
    std::mem::forget(db);
    router(AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: Some(TOKEN.into()),
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    })
}

async fn call(method: &str, path: &str, owner: bool, body: Option<&str>, cookie: Option<&str>) -> (StatusCode, String, String) {
    let mut b = Request::builder().method(method).uri(path).header(header::HOST, "sandbox.amux.io");
    if owner {
        b = b.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    }
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    let req = match body {
        Some(j) => b.header(header::CONTENT_TYPE, "application/json").body(Body::from(j.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let res = app().oneshot(req).await.unwrap();
    let status = res.status();
    let loc = res.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, loc, String::from_utf8_lossy(&body).to_string())
}

/// The owner's browser session cookie, as the server derives it from the owner
/// token (static_files.rs owner_session_value). A worker holds the bearer at
/// most, never this cookie, so this is what "a person approved it" means here.
fn owner_cookie() -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(format!("amux-owner-session:{TOKEN}"));
    format!("__Host-amux_owner={}", hex::encode(h.finalize()))
}

/// A link the bearer (a worker's kind of caller) made is pending: approve it
/// as the owner's browser.
async fn approve_as_person(id: &str) {
    // The dashboard sends the bearer AND the session cookie; a worker has the bearer only.
    let (st, _, body) = call("POST", &format!("/api/page-links/{id}/approve"), true, None, Some(&owner_cookie())).await;
    assert_eq!(st, StatusCode::OK, "{body}");
}

/// A loopback-only app that answers every request with the request head it got.
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
                if n == 0 {
                    return;
                }
                let head = String::from_utf8_lossy(&buf[..n]).replace('<', "&lt;");
                let body = format!("<h1>public app</h1><pre>{head}</pre>");
                let _ = s
                    .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
                    .await;
            });
        }
    });
    port
}

#[tokio::test]
async fn a_public_link_opens_without_sign_in_and_only_the_owner_manages_links() {
    let port = echo_app().await;

    // Managing links is the owner's.
    let (st, _, _) = call("GET", "/api/page-links", false, None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "the list must need the owner");
    let (st, _, _) = call("POST", "/api/page-links", false, Some(&format!(r#"{{"port":{port}}}"#)), None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "creating must need the owner");

    // amux itself is never published.
    let (st, _, body) = call("POST", "/api/page-links", true, Some(r#"{"port":8824}"#), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");

    let (st, _, body) = call("POST", "/api/page-links", true, Some(&format!(r#"{{"port":{port},"title":"demo"}}"#)), None).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let link: serde_json::Value = serde_json::from_str(&body).unwrap();
    let id = link["id"].as_str().unwrap().to_string();
    assert_eq!(link["path"], format!("/pub/{id}/"));
    assert_eq!(link["listening"], true);

    // Asked for with the bearer (how a worker calls): pending, served to nobody,
    // and the same caller cannot approve it.
    assert_eq!(link["pending"], true, "{body}");
    let (st, _, _) = call("GET", &format!("/pub/{id}/"), false, None, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "a pending link opened");
    let (st, _, _) = call("POST", &format!("/api/page-links/{id}/approve"), true, None, None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "the bearer approved its own public link");
    approve_as_person(&id).await;

    // Anyone with the link: no token and no cookie at all reaches the app.
    let (st, _, body) = call("GET", &format!("/pub/{id}/page?x=1"), false, None,
        Some("__Host-amux_owner=o; amux_door=d; amux_member=m; appsid=1")).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body.contains("<h1>public app</h1>"), "{body}");
    assert!(body.contains("GET /page?x=1 "), "the app sees its own path: {body}");
    for leaked in ["amux_owner", "amux_door", "amux_member", "appsid", TOKEN] {
        assert!(!body.contains(leaked), "{leaked} reached the app: {body}");
    }
    let (st, _, body) = call("GET", &format!("/pub/{id}/"), false, None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("GET / "), "the link's root is the app's root, never the dashboard: {body}");
    let (st, loc, _) = call("GET", &format!("/pub/{id}"), false, None, None).await;
    assert!(st.is_redirection() && loc == format!("/pub/{id}/"), "{st} {loc}");

    // An id nobody created, or a path trick, is a 404 and never the API.
    let (st, _, _) = call("GET", "/pub/0123456789abcdef0123456789abcdef/", false, None, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _, body) = call("GET", "/pub/../api/page-links", false, None, None).await;
    assert!(!body.contains(&id), "a path trick listed the links: {st} {body}");

    // The owner sees it, and turning it off ends it.
    let (st, _, body) = call("GET", "/api/page-links", true, None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains(&id), "{body}");
    let (st, _, _) = call("DELETE", &format!("/api/page-links/{id}"), false, None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "revoking must need the owner");
    let (st, _, _) = call("DELETE", &format!("/api/page-links/{id}"), true, None, None).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _, _) = call("GET", &format!("/pub/{id}/"), false, None, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "a revoked link still opened");
}

#[tokio::test]
async fn an_internal_app_is_never_public_and_a_shared_folder_stays_inside_itself() {
    let port = echo_app().await;
    let (st, _, body) = call("POST", "/api/page-links", true, Some(&format!(r#"{{"kind":"internal","port":{port}}}"#)), None).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let link: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(link["public"], false);
    assert_eq!(link["path"], format!("/#view=apps&open={}", link["id"].as_str().unwrap()));
    let id = link["id"].as_str().unwrap();
    let (st, _, _) = call("GET", &format!("/pub/{id}/"), false, None, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "an internal app opened publicly");

    // A folder under HOME with a visible name (hidden paths are never published).
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());
    let base = tempfile::Builder::new().prefix("pubint-").tempdir_in(&home).unwrap();
    let root = base.path().join("share");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("report.txt"), "quarterly").unwrap();
    std::fs::write(root.join(".env"), "SECRET=1").unwrap();
    std::fs::write(root.join("id_rsa"), "FAKE KEY").unwrap();
    std::fs::write(root.join("logo.svg"), "<svg xmlns='http://www.w3.org/2000/svg'><script>document.title='x'</script></svg>").unwrap();
    std::fs::write(base.path().join("outside.txt"), "outside").unwrap();
    let (st, _, body) = call("POST", "/api/page-links", true,
        Some(&serde_json::json!({"kind": "file", "path": root}).to_string()), None).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"].as_str().unwrap().to_string();
    approve_as_person(&id).await;
    let (st, _, listing) = call("GET", &format!("/pub/{id}/"), false, None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(listing.contains("report.txt") && !listing.contains(".env"), "{listing}");
    assert!(!listing.contains("id_rsa"), "a key's name was listed: {listing}");
    let (st, _, _) = call("GET", &format!("/pub/{id}/id_rsa"), false, None, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "a non-hidden key was served");
    // An SVG runs script: it must arrive sandboxed, like HTML.
    let res = app().oneshot(Request::builder().uri(format!("/pub/{id}/logo.svg")).header(header::HOST, "sandbox.amux.io")
        .body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let csp = res.headers().get(header::CONTENT_SECURITY_POLICY).and_then(|v| v.to_str().ok()).unwrap_or("");
    assert!(csp.starts_with("sandbox"), "an SVG was served without the sandbox: {csp:?}");
    let (st, _, file) = call("GET", &format!("/pub/{id}/report.txt"), false, None, None).await;
    assert_eq!((st, file.as_str()), (StatusCode::OK, "quarterly"));
    for bad in [".env", "../outside.txt", "%2e%2e/outside.txt"] {
        let (st, _, b) = call("GET", &format!("/pub/{id}/{bad}"), false, None, None).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{bad} was served: {b}");
    }
    // Hidden paths are refused at creation.
    let (st, _, _) = call("POST", "/api/page-links", true,
        Some(&serde_json::json!({"kind": "file", "path": root.join(".env")}).to_string()), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}
