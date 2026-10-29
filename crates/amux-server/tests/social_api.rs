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
    devto_fail: bool,
    devto_keys: Vec<String>,
    devto_posts: Vec<Value>,
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

async fn devto_create(State(f): State<Shared>, h: HeaderMap, Json(b): Json<Value>) -> axum::response::Response {
    // dev.to refuses a request with no User-Agent (403), and so does this fake.
    if !h.get("user-agent").and_then(|v| v.to_str().ok()).is_some_and(|ua| ua.starts_with("amux/")) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "forbidden: no user agent"}))).into_response();
    }
    let mut f = f.lock().unwrap();
    f.devto_keys.push(h.get("api-key").and_then(|v| v.to_str().ok()).unwrap_or("").to_string());
    if f.devto_fail {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "down for maintenance"}))).into_response();
    }
    f.devto_posts.push(b["article"].clone());
    let published = b["article"]["published"].as_bool().unwrap_or(false);
    Json(json!({"id": 555, "url": "https://dev.to/amux/eight-agents-555", "published": published})).into_response()
}

async fn devto_mine(h: HeaderMap) -> axum::response::Response {
    if h.get("user-agent").is_none() {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "forbidden: no user agent"}))).into_response();
    }
    Json(json!([{"id": 554, "page_views_count": 1}, {"id": 555, "page_views_count": 9, "public_reactions_count": 2, "comments_count": 1, "published": false, "url": "https://dev.to/amux/eight-agents-555"}])).into_response()
}

async fn fake_youtube() -> (String, Shared) {
    let shared: Shared = Arc::default();
    let app = Router::new()
        .route("/upload/youtube/v3/videos", post(open_session))
        .route("/upload/session/1", put(send_bytes))
        .route("/youtube/v3/videos", get(videos))
        .route("/v2/reports", get(reports))
        .route("/devto/api/articles", post(devto_create))
        .route("/devto/api/articles/me/all", get(devto_mine))
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
    assert_eq!(d["platforms"].as_array().unwrap().len(), 6);
    assert_eq!(d["platforms"][0]["status"], "built");
}

async fn queue_row(app: &Router, id: i64) -> Value {
    let (_, q) = call(app, "GET", "/api/social/queue?status=all", None).await;
    q["queue"].as_array().unwrap().iter().find(|r| r["id"] == id).cloned().unwrap_or(Value::Null)
}

async fn force_due(st: &AppState) {
    st.store
        .write_async(|c| {
            c.execute("UPDATE social_queue SET next_attempt_at = 0 WHERE status = 'queued'", [])?;
            Ok(amux_server::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn one_call_queues_a_post_for_several_platforms_and_the_job_publishes_each() {
    use amux_server::api::social_publish;
    let (origin, fake) = fake_youtube().await;
    let dir = tempfile::tempdir().unwrap();
    let st = state(&dir);
    let ctx = SocialCtx::fake(&origin, "tok-1", &[("AMUX_SOCIAL_YOUTUBE_ACCOUNT", "hello@amux.io"), ("DEVTO_API_KEY", "dk-1")]);
    let app = Router::new().nest("/api/social", social::routes_with(ctx.clone())).with_state(st.clone());
    let video = dir.path().join("short.mp4");
    std::fs::write(&video, vec![7u8; 2048]).unwrap();

    // A post for later stays queued; nothing is sent before its time.
    let later = chrono::Utc::now().timestamp() + 3600;
    let (code, v) = call(&app, "POST", "/api/social/posts", Some(json!({
        "platforms": ["youtube", "devto"], "scheduled_at": later, "file": video.to_str().unwrap(),
        "title": "Eight agents", "caption": "8 agents fixed 23 bugs overnight",
    }))).await;
    assert_eq!(code, StatusCode::ACCEPTED, "{v}");
    let group = v["group_id"].as_str().unwrap().to_string();
    let rows = v["queued"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 2, "one row per platform: {v}");
    assert!(rows.iter().all(|r| r["group_id"] == group.as_str() && r["status"] == "queued" && r["scheduled_at"] == later), "{v}");
    let pass = social_publish::publish_pass(&ctx, &st).await;
    assert_eq!(pass["n_considered"], 0, "not due yet: {pass}");
    assert!(fake.lock().unwrap().upload_len.is_none(), "nothing was sent early");

    // Moved to now, the job publishes both and records each in the ledger.
    let ids: Vec<i64> = rows.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    for id in &ids {
        let (code, r) = call(&app, "POST", &format!("/api/social/queue/{id}/reschedule"), Some(json!({"scheduled_at": "now"}))).await;
        assert_eq!(code, StatusCode::OK, "{r}");
    }
    let pass = social_publish::publish_pass(&ctx, &st).await;
    assert_eq!((pass["posted"].clone(), pass["failed"].clone()), (json!(2), json!(0)), "{pass}");
    let yt = queue_row(&app, ids[0]).await;
    let dv = queue_row(&app, ids[1]).await;
    assert_eq!((yt["status"].clone(), yt["post_id"].clone()), (json!("posted"), json!("vid123")), "{yt}");
    assert_eq!((dv["status"].clone(), dv["post_id"].clone()), (json!("posted"), json!("555")), "{dv}");
    assert!(dv["notes"].as_str().unwrap_or("").contains("draft"), "the owner switch is off, and the row says so: {dv}");
    assert_eq!(dv["error"], Value::Null, "{dv}");
    {
        let f = fake.lock().unwrap();
        assert_eq!(f.upload_len, Some(2048));
        assert_eq!(f.devto_keys, ["dk-1"], "the dev.to key goes in its api-key header");
        assert_eq!(f.devto_posts[0]["published"], false, "public posting is the owner's switch");
        assert_eq!(f.devto_posts[0]["title"], "Eight agents");
    }
    let (_, list) = call(&app, "GET", "/api/social/posts", None).await;
    assert!(list["posts"].as_array().unwrap().iter().all(|p| p["origin"] == "queue" && p["session"] == "amux-gtm"), "{list}");
    let devto_row = dv["post_row"].as_i64().unwrap();
    let (code, m) = call(&app, "GET", &format!("/api/social/posts/{devto_row}/metrics"), None).await;
    assert_eq!(code, StatusCode::OK, "{m}");
    assert_eq!((m["metrics"]["views"].clone(), m["metrics"]["engagement"].clone()), (json!(9), json!(3)), "{m}");
    let again = social_publish::publish_pass(&ctx, &st).await;
    assert_eq!(again["n_considered"], 0, "a posted row is never published twice: {again}");
}

#[tokio::test]
async fn a_queued_post_retries_then_fails_and_an_unanswered_one_is_never_resent() {
    use amux_server::api::social_publish;
    let (origin, fake) = fake_youtube().await;
    let dir = tempfile::tempdir().unwrap();
    let st = state(&dir);
    let ctx = SocialCtx::fake(&origin, "t", &[("DEVTO_API_KEY", "dk-1")]);
    let app = Router::new().nest("/api/social", social::routes_with(ctx.clone())).with_state(st.clone());

    // Refused when queued, not when due: nothing is half-queued.
    let (code, v) = call(&app, "POST", "/api/social/posts", Some(json!({"platforms": ["devto", "youtube"], "scheduled_at": "now", "caption": "x", "title": "T"}))).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "youtube has no account or file: {v}");
    assert_eq!(v["platform"], "youtube");
    let (code, _) = call(&app, "POST", "/api/social/posts", Some(json!({"platform": "devto", "scheduled_at": 1000, "caption": "x", "title": "T"}))).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "a time in the past is refused");
    let (_, q) = call(&app, "GET", "/api/social/queue?status=all", None).await;
    assert_eq!(q["n_considered"], 0, "{q}");

    // A platform that is down: retried with a backoff, then failed with its error.
    fake.lock().unwrap().devto_fail = true;
    let (code, v) = call(&app, "POST", "/api/social/posts", Some(json!({"platform": "devto", "scheduled_at": "now", "caption": "body", "title": "T"}))).await;
    assert_eq!(code, StatusCode::ACCEPTED, "{v}");
    let id = v["queued"][0]["id"].as_i64().unwrap();
    let pass = social_publish::publish_pass(&ctx, &st).await;
    assert_eq!(pass["retried"], 1, "{pass}");
    let r = queue_row(&app, id).await;
    assert_eq!((r["status"].clone(), r["attempts"].clone()), (json!("queued"), json!(1)), "{r}");
    assert!(r["next_attempt_at"].as_i64().unwrap() > chrono::Utc::now().timestamp(), "backs off: {r}");
    assert!(r["error"].as_str().unwrap().contains("maintenance"), "{r}");
    for _ in 0..2 {
        force_due(&st).await;
        social_publish::publish_pass(&ctx, &st).await;
    }
    let r = queue_row(&app, id).await;
    assert_eq!((r["status"].clone(), r["attempts"].clone()), (json!("failed"), json!(3)), "{r}");
    assert_eq!(fake.lock().unwrap().devto_keys.len(), 3, "three tries in all");

    // A failed post can be retried by rescheduling it; a cancelled one cannot.
    fake.lock().unwrap().devto_fail = false;
    let (code, _) = call(&app, "POST", &format!("/api/social/queue/{id}/cancel"), None).await;
    assert_eq!(code, StatusCode::OK);
    let (code, v) = call(&app, "POST", &format!("/api/social/queue/{id}/cancel"), None).await;
    assert_eq!(code, StatusCode::CONFLICT, "{v}");
    let (code, _) = call(&app, "POST", &format!("/api/social/queue/{id}/reschedule"), Some(json!({"scheduled_at": "now"}))).await;
    assert_eq!(code, StatusCode::CONFLICT, "a cancelled post stays cancelled");
    let (code, _) = call(&app, "POST", "/api/social/queue/99999/cancel", None).await;
    assert_eq!(code, StatusCode::NOT_FOUND);

    // A row a stopped server left mid-publish may be live: marked uncertain,
    // never re-sent on its own, and re-sent only with force.
    let (_, v) = call(&app, "POST", "/api/social/posts", Some(json!({"platform": "devto", "scheduled_at": "now", "caption": "body", "title": "T2"}))).await;
    let id = v["queued"][0]["id"].as_i64().unwrap();
    st.store
        .write_async(move |c| {
            c.execute("UPDATE social_queue SET status = 'publishing', claimed_by = 'a-dead-process' WHERE id = ?1", [id])?;
            Ok(amux_server::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await
        .unwrap();
    let sent_before = fake.lock().unwrap().devto_keys.len();
    let pass = social_publish::publish_pass(&ctx, &st).await;
    assert_eq!(pass["n_considered"], 0, "{pass}");
    let r = queue_row(&app, id).await;
    assert_eq!(r["status"], "uncertain", "{r}");
    assert_eq!(fake.lock().unwrap().devto_keys.len(), sent_before, "not re-sent");
    let (code, v) = call(&app, "POST", &format!("/api/social/queue/{id}/reschedule"), Some(json!({"scheduled_at": "now"}))).await;
    assert_eq!(code, StatusCode::CONFLICT, "{v}");
    assert!(v["error"].as_str().unwrap().contains("force"), "{v}");
    let (code, _) = call(&app, "POST", &format!("/api/social/queue/{id}/reschedule"), Some(json!({"scheduled_at": "now", "force": true}))).await;
    assert_eq!(code, StatusCode::OK);
    let pass = social_publish::publish_pass(&ctx, &st).await;
    assert_eq!(pass["posted"], 1, "{pass}");
}

#[test]
fn publish_times_parse_in_every_form_the_cli_sends() {
    use amux_server::api::social_publish::parse_when;
    assert_eq!(parse_when(&json!(1_800_000_000)), Ok(1_800_000_000));
    assert_eq!(parse_when(&json!("1800000000")), Ok(1_800_000_000));
    assert_eq!(parse_when(&json!("2027-01-15T15:00:00Z")), Ok(1_800_025_200));
    assert!(parse_when(&json!("2027-01-15T15:00")).is_ok());
    assert!(parse_when(&json!("next tuesday")).is_err());
}
