//! `/proxy/<port>/<page>` through the composed router. Before 2026-10-17 the
//! SPA catch-all answered these links with the dashboard, so a worker's link
//! to its dev server opened amux on the owner's phone instead of the page.

use amux_server::api::{router, AppState};
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
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
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    })
}

async fn get(path: &str) -> (StatusCode, String, String) {
    let res = app()
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::HOST, "127.0.0.1:8824")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let loc = res
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, loc, String::from_utf8_lossy(&body).to_string())
}

#[tokio::test]
async fn a_proxy_link_opens_the_dev_server_on_its_own_port() {
    let dev = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = dev.local_addr().unwrap().port();
    let (st, loc, body) = get(&format!("/proxy/{port}/?hero=texture")).await;
    assert_eq!(st, StatusCode::TEMPORARY_REDIRECT, "{body}");
    assert_eq!(loc, format!("http://127.0.0.1:{port}/?hero=texture"));
    let (st, loc, _) = get(&format!("/proxy/{port}")).await;
    assert_eq!(
        (st, loc),
        (
            StatusCode::TEMPORARY_REDIRECT,
            format!("http://127.0.0.1:{port}/")
        )
    );
}

#[tokio::test]
async fn a_proxy_link_to_a_stopped_server_says_so_and_never_serves_the_dashboard() {
    let dev = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = dev.local_addr().unwrap().port();
    drop(dev);
    let (st, loc, body) = get(&format!("/proxy/{port}/")).await;
    assert_eq!(st, StatusCode::BAD_GATEWAY);
    assert!(loc.is_empty());
    assert!(body.contains("Nothing is running on that port"), "{body}");
    assert!(!body.contains("app.js"), "the dashboard shell came back");
    let (st, _, body) = get("/proxy/8824/").await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_loopback_only_server_the_browser_cannot_reach_is_relayed_through_amux() {
    // Bound to ::1 only, so the browser's host (127.0.0.1 here) cannot reach it.
    let Ok(dev) = tokio::net::TcpListener::bind("[::1]:0").await else {
        eprintln!("no IPv6 loopback on this host; relay path not exercised");
        return;
    };
    let port = dev.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut s, _)) = dev.accept().await {
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = if req.starts_with("GET /page?x=1 ") {
                r#"<h1>relayed</h1><img src="/pixel.png">"#
            } else {
                "unexpected"
            };
            let _ = s
                .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
                .await;
        }
    });
    let (st, loc, body) = get(&format!("/proxy/{port}/page?x=1")).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(loc.is_empty(), "a loopback-only server must not be redirected to: {loc}");
    assert!(body.contains("<h1>relayed</h1>"), "{body}");
    assert!(body.contains(&format!(r#"src="/proxy/{port}/pixel.png""#)), "absolute asset not routed back: {body}");
}
