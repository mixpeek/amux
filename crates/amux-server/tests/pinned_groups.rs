//! `/api/pinned` groups through the composed router: several files pinned at
//! once share one `group_id` (they open as tabs in one overlay), a single
//! file stays standalone, and the list reports the group for each row.

use amux_server::api::{router, AppState};
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use serde_json::{json, Value};
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

async fn call(app: &axum::Router, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path).header(header::HOST, "127.0.0.1:8824");
    let body = match body {
        Some(b) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn several_files_pinned_together_share_a_group_and_one_file_stays_standalone() {
    let app = app();
    let (st, grouped) = call(&app, Method::POST, "/api/pinned",
        Some(json!({"file_paths": ["/tmp/a.md", "/tmp/b.md", "/tmp/c.md"]}))).await;
    assert_eq!(st, StatusCode::CREATED, "{grouped}");
    let gid = grouped["group_id"].as_str().expect("a multi-file pin gets a group id").to_string();

    let (st, single) = call(&app, Method::POST, "/api/pinned", Some(json!({"file_path": "/tmp/solo.md"}))).await;
    assert_eq!(st, StatusCode::CREATED, "{single}");
    assert!(single["group_id"].is_null(), "one file stays standalone: {single}");

    let (st, none) = call(&app, Method::POST, "/api/pinned", Some(json!({}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{none}");

    let (st, list) = call(&app, Method::GET, "/api/pinned", None).await;
    assert_eq!(st, StatusCode::OK);
    let rows = list.as_array().or_else(|| list["notes"].as_array()).or_else(|| list["items"].as_array())
        .unwrap_or_else(|| panic!("list shape: {list}")).clone();
    let in_group: Vec<&str> = rows.iter().filter(|r| r["group_id"] == json!(gid))
        .filter_map(|r| r["title"].as_str()).collect();
    let mut sorted = in_group.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["a.md", "b.md", "c.md"], "every grouped file is listed under the group: {list}");
    let solo = rows.iter().find(|r| r["file_path"] == "/tmp/solo.md").expect("solo row listed");
    assert!(solo["group_id"].is_null(), "{solo}");
}
