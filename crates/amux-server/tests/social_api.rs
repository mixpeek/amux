//! /api/social against a local fake of YouTube (AA-42): the real handlers
//! upload through the resumable protocol, record the post, read its results,
//! and store window snapshots, with only the platform's origin swapped.

use amux_server::api::social::{self, SocialCtx};
use amux_server::api::AppState;
use amux_server::db::Store;
use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

#[derive(Default)]
struct Fake {
    origin: String,
    sent_privacy: Option<String>,
    upload_len: Option<usize>,
    bearer_seen: Vec<String>,
    analytics_rows: bool,
}

type Shared = Arc<Mutex<Fake>>;

fn bearer(h: &HeaderMap) -> String {
    h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

async fn open_session(State(f): State<Shared>, h: HeaderMap, Json(meta): Json<Value>) -> impl IntoResponse {
    let mut f = f.lock().unwrap();
    f.bearer_seen.push(bearer(&h));
    f.sent_privacy = meta["status"]["privacyStatus"].as_str().map(String::from);
    let loc = format!("{}/upload/session/1", f.origin);
    (StatusCode::OK, [("location", loc)], "")
}

async fn send_bytes(State(f): State<Shared>, body: Bytes) -> Json<Value> {
    let mut f = f.lock().unwrap();
    f.upload_len = Some(body.len());
    // An unaudited project: Google applies private whatever was asked.
    Json(json!({"id": "vid123", "status": {"privacyStatus": "private", "uploadStatus": "uploaded"}}))
}

async fn videos(Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    assert_eq!(q.get("id").map(String::as_str), Some("vid123"));
    Json(json!({"items": [{"statistics": {"viewCount": "42", "likeCount": "5", "commentCount": "2"}, "status": {"privacyStatus": "private"}}]}))
}

async fn reports(State(f): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    assert_eq!(q.get("filters").map(String::as_str), Some("video==vid123"));
    if !f.lock().unwrap().analytics_rows {
        return Json(json!({"columnHeaders": [], "rows": []}));
    }
    Json(json!({
        "columnHeaders": [{"name": "views"}, {"name": "likes"}, {"name": "comments"}, {"name": "shares"},
            {"name": "estimatedMinutesWatched"}, {"name": "averageViewDuration"}, {"name": "subscribersGained"}],
        "rows": [[40, 5, 2, 3, 11, 17, 1]],
    }))
}

async fn fake_youtube() -> (String, Shared) {
    let shared: Shared = Arc::default();
    let app = Router::new()
        .route("/upload/youtube/v3/videos", post(open_session))
        .route("/upload/session/1", put(send_bytes))
        .route("/youtube/v3/videos", get(videos))
        .route("/v2/reports", get(reports))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    shared.lock().unwrap().origin = origin.clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (origin, shared)
}

fn state(dir: &tempfile::TempDir) -> AppState {
    AppState {
        store: Arc::new(Store::open(&dir.path().join("amux-test.db")).unwrap()),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    }
}

async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path).header("x-amux-session", "amux-gtm");
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
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
async fn a_youtube_post_uploads_records_and_reads_back_its_results() {
    let (origin, fake) = fake_youtube().await;
    let dir = tempfile::tempdir().unwrap();
    let st = state(&dir);
    let ctx = SocialCtx::fake(&origin, "tok-1", &[("AMUX_SOCIAL_YOUTUBE_ACCOUNT", "hello@amux.io")]);
    let app = Router::new().nest("/api/social", social::routes_with(ctx.clone())).with_state(st.clone());

    let video = dir.path().join("short.mp4");
    std::fs::write(&video, vec![7u8; 4096]).unwrap();
    let (code, v) = call(&app, "POST", "/api/social/posts", Some(json!({
        "platform": "youtube", "file": video.to_str().unwrap(),
        "caption": "8 agents fixed 23 bugs overnight\namux.io", "privacy": "public",
    }))).await;
    assert_eq!(code, StatusCode::CREATED, "{v}");
    assert_eq!(v["post_id"], "vid123");
    assert_eq!(v["url"], "https://www.youtube.com/watch?v=vid123");
    assert_eq!(v["privacy_requested"], "public");
    assert_eq!(v["privacy_status"], "private");
    assert!(v["notes"][0].as_str().unwrap().contains("AMUX_SOCIAL_PUBLIC"), "{v}");
    {
        let f = fake.lock().unwrap();
        assert_eq!(f.sent_privacy.as_deref(), Some("private"), "public posting is the owner's switch");
        assert_eq!(f.upload_len, Some(4096), "every byte of the file was sent");
        assert_eq!(f.bearer_seen, ["Bearer tok-1"]);
    }
    let id = v["id"].as_i64().unwrap();

    // The ledger names the post, who posted it, and when each window falls due.
    let (_, list) = call(&app, "GET", "/api/social/posts?platform=youtube", None).await;
    assert_eq!(list["measured"], true);
    let p = &list["posts"][0];
    assert_eq!(p["session"], "amux-gtm");
    assert_eq!(p["account"], "hello@amux.io");
    assert_eq!(p["title"], "8 agents fixed 23 bugs overnight");
    assert_eq!(p["windows"]["24h"]["measured"], false);
    assert_eq!(p["windows"]["24h"]["due_at"], p["posted_at"].as_i64().unwrap() + 86_400);

    // Live read before Analytics has rows: counters are measured, Analytics says it is not.
    let (code, m) = call(&app, "GET", &format!("/api/social/posts/{id}/metrics"), None).await;
    assert_eq!(code, StatusCode::OK, "{m}");
    assert_eq!(m["metrics"]["views"], 42);
    assert_eq!(m["metrics"]["engagement"], 7);
    assert_eq!(m["metrics"]["impressions"], Value::Null);
    assert_eq!(m["metrics"]["sources"]["analytics"]["measured"], false);

    // A post made in the browser, tracked 25 hours later: the pass snapshots its 24h window.
    fake.lock().unwrap().analytics_rows = true;
    let back = chrono::Utc::now().timestamp() - 90_000;
    let (code, t) = call(&app, "POST", "/api/social/posts/track", Some(json!({"platform": "youtube", "post_id": "vid123x", "posted_at": back}))).await;
    assert_eq!(code, StatusCode::CREATED, "{t}");
    // Same video id as the fake serves, so point the tracked row at it.
    st.store
        .write_async(|c| {
            c.execute("DELETE FROM social_posts WHERE origin = 'api'", [])?;
            c.execute("UPDATE social_posts SET post_id = 'vid123' WHERE post_id = 'vid123x'", [])?;
            Ok(amux_server::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await
        .unwrap();
    let pass = social::snapshot_pass(&ctx, &st).await;
    assert_eq!(pass["n_considered"], 1, "{pass}");
    assert_eq!(pass["ok"], 1, "{pass}");
    let tid = t["id"].as_i64().unwrap();
    let (_, one) = call(&app, "GET", &format!("/api/social/posts/{tid}"), None).await;
    let snap = &one["windows"]["24h"];
    assert_eq!(snap["measured"], true, "{one}");
    assert_eq!(snap["metrics"]["shares"], 3);
    assert_eq!(snap["metrics"]["engagement"], 10);
    assert_eq!(snap["metrics"]["watch_minutes"], 11);
    assert_eq!(one["windows"]["72h"]["measured"], false, "72h is not due yet");
    let again = social::snapshot_pass(&ctx, &st).await;
    assert_eq!(again["n_considered"], 0, "a measured window is not read twice");
}

#[tokio::test]
async fn unbuilt_platforms_say_what_gates_them_and_bad_input_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = SocialCtx::fake("http://127.0.0.1:9", "t", &[("AMUX_SOCIAL_PUBLIC", "1")]);
    let app = Router::new().nest("/api/social", social::routes_with(ctx)).with_state(state(&dir));
    let (code, v) = call(&app, "POST", "/api/social/posts", Some(json!({"platform": "tiktok", "file": "/x.mp4"}))).await;
    assert_eq!(code, StatusCode::NOT_IMPLEMENTED);
    assert!(v["gate"].as_str().unwrap().contains("audits"), "{v}");
    let (code, _) = call(&app, "POST", "/api/social/posts", Some(json!({"platform": "myspace", "file": "/x.mp4"}))).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, v) = call(&app, "POST", "/api/social/posts", Some(json!({"platform": "youtube", "account": "a@b.c", "file": "/nope.mp4"}))).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
    let img = dir.path().join("card.png");
    std::fs::write(&img, b"png").unwrap();
    let (code, v) = call(&app, "POST", "/api/social/posts", Some(json!({"platform": "youtube", "account": "a@b.c", "file": img.to_str().unwrap(), "caption": "x"}))).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("videos only"), "{v}");
    let (_, d) = call(&app, "GET", "/api/social", None).await;
    assert_eq!(d["public_posting"], true);
    assert_eq!(d["platforms"].as_array().unwrap().len(), 5);
    assert_eq!(d["platforms"][0]["status"], "built");
}
