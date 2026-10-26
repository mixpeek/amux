//! /api/social — post to social platforms and read each post's results back.
//!
//! The closed GTM loop (amux-GTM `internal/gtm/amux/PLAN.md`) needs three
//! things from each platform: publish a video or image with a caption, get the
//! platform's post id back, and read reach and engagement for that id at 24h,
//! 72h and 7d. Before this, amux's posts went out through Buffer and nobody read
//! the results back, so the loop had nothing to learn from.
//!
//! - `GET  /api/social` — every platform, whether it is built, and what gates it.
//! - `POST /api/social/posts` — upload and publish. Returns the post id.
//! - `POST /api/social/posts/track` — register a post made elsewhere (the amux
//!   browser, the platform's app) so it is measured the same way.
//! - `GET  /api/social/posts` — the ledger, each post with its window snapshots.
//! - `GET  /api/social/posts/{id}` — one post.
//! - `GET  /api/social/posts/{id}/metrics` — a live read, not stored.
//!
//! The `social-metrics` runtime job stores one snapshot per post per window
//! once the window has passed. A failed read is stored with `measured: false`
//! and its error, and retried on the next pass until it succeeds.
//!
//! Auth reuses the connector broker: YouTube is the `google-youtube` connector,
//! minted for the posting account exactly as `/api/connectors/{id}/token` mints
//! it. The token is spent here and never returned.
//!
//! Publishing publicly is the owner's switch: `AMUX_SOCIAL_PUBLIC=1` in
//! server.env. Without it a post asking for `public` is uploaded `private` and
//! the reply says why. Separately, Google keeps every upload from an unaudited
//! API project private; the reply reports the privacy the platform applied, not
//! the one asked for.

use super::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Snapshot windows, in seconds after the post.
pub const WINDOWS: &[(&str, i64)] = &[("24h", 86_400), ("72h", 259_200), ("7d", 604_800)];

/// Largest file `POST /api/social/posts` reads into memory for one upload.
const MAX_UPLOAD_BYTES: u64 = 1 << 30;

/// What each platform needs before it can post, for `GET /api/social`. Only
/// YouTube is built; the rest say what gates them so a caller can tell "not
/// built" from "broken".
const PLATFORMS: &[(&str, &str, &str)] = &[
    ("youtube", "built", "google-youtube connector. Uploads stay private until Google's YouTube API audit passes."),
    ("instagram", "not_built", "Graph API content publishing: @amux_io as a Business account linked to a Facebook Page, and a Meta app with publishing approved."),
    ("tiktok", "not_built", "Content Posting API: posts are visible only to the account until TikTok audits the app."),
    ("x", "not_built", "API v2 posting needs a paid tier, which is the owner's decision."),
    ("linkedin", "not_built", "Community Management API approval and an amux company page."),
];

type TokenFuture = Pin<Box<dyn Future<Output = Result<String, Value>> + Send>>;
type TokenSource = Arc<dyn Fn(AppState, HeaderMap, String) -> TokenFuture + Send + Sync>;
type EnvSource = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The platform endpoints plus how a bearer is obtained. Injected so the tests
/// run the real handlers against a local fake of the platform.
#[derive(Clone)]
pub struct SocialCtx {
    pub youtube_api: String,
    pub youtube_upload: String,
    pub youtube_analytics: String,
    pub token: TokenSource,
    /// Owner settings (`AMUX_SOCIAL_*`): server.env, then the process env.
    pub env: EnvSource,
    pub http: reqwest::Client,
}

impl SocialCtx {
    pub fn live() -> Self {
        SocialCtx {
            youtube_api: "https://www.googleapis.com/youtube/v3".into(),
            youtube_upload: "https://www.googleapis.com/upload/youtube/v3".into(),
            youtube_analytics: "https://youtubeanalytics.googleapis.com/v2".into(),
            token: Arc::new(|state, headers, account| {
                Box::pin(async move {
                    super::connectors::bearer_for(state, headers, "google-youtube", &account).await
                })
            }),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .unwrap_or_default(),
            env: Arc::new(|key| {
                let env = crate::config::parse_env_file(&crate::config::amux_home().join("server.env"));
                env.get(key).cloned().or_else(|| std::env::var(key).ok())
            }),
        }
    }

    /// For tests: every platform URL under one fake origin, and a fixed bearer.
    pub fn fake(origin: &str, bearer: &str, env: &[(&str, &str)]) -> Self {
        let bearer = bearer.to_string();
        let env: std::collections::HashMap<String, String> =
            env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        SocialCtx {
            youtube_api: format!("{origin}/youtube/v3"),
            youtube_upload: format!("{origin}/upload/youtube/v3"),
            youtube_analytics: format!("{origin}/v2"),
            token: Arc::new(move |_, _, _| {
                let b = bearer.clone();
                Box::pin(async move { Ok(b) })
            }),
            env: Arc::new(move |key| env.get(key).cloned()),
            http: reqwest::Client::new(),
        }
    }
}

pub fn routes() -> Router<AppState> {
    routes_with(SocialCtx::live())
}

pub fn routes_with(ctx: SocialCtx) -> Router<AppState> {
    Router::new()
        .route("/", axum::routing::get(discover))
        .route("/posts", axum::routing::get(list).post(publish))
        .route("/posts/track", axum::routing::post(track))
        .route("/posts/{id}", axum::routing::get(one))
        .route("/posts/{id}/metrics", axum::routing::get(live_metrics))
        .layer(Extension(Arc::new(ctx)))
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({"ok": false, "error": msg.into()}))).into_response()
}

fn env_val(ctx: &SocialCtx, key: &str) -> Option<String> {
    (ctx.env)(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The account a platform posts as: the request's, else
/// `AMUX_SOCIAL_<PLATFORM>_ACCOUNT` from server.env.
fn account_for(ctx: &SocialCtx, platform: &str, requested: Option<&str>) -> Option<String> {
    requested
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(String::from)
        .or_else(|| env_val(ctx, &format!("AMUX_SOCIAL_{}_ACCOUNT", platform.to_ascii_uppercase())))
}

fn caller(headers: &HeaderMap) -> String {
    headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

async fn discover(Extension(ctx): Extension<Arc<SocialCtx>>) -> Response {
    let public = env_val(&ctx, "AMUX_SOCIAL_PUBLIC").as_deref() == Some("1");
    let platforms: Vec<Value> = PLATFORMS
        .iter()
        .map(|(id, status, gate)| {
            json!({
                "platform": id,
                "status": status,
                "gate": gate,
                "account": account_for(&ctx, id, None),
            })
        })
        .collect();
    Json(json!({
        "platforms": platforms,
        "public_posting": public,
        "public_posting_switch": "AMUX_SOCIAL_PUBLIC=1 in server.env (owner)",
        "windows": WINDOWS.iter().map(|(w, _)| *w).collect::<Vec<_>>(),
        "post": "POST /api/social/posts {platform, file, caption, title?, privacy?, tags?, account?}",
        "track": "POST /api/social/posts/track {platform, post_id, posted_at?, url?, caption?, account?}",
    }))
    .into_response()
}

#[derive(Deserialize)]
struct PublishBody {
    platform: String,
    #[serde(default)]
    account: Option<String>,
    file: String,
    #[serde(default)]
    caption: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    privacy: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

/// A YouTube title: the given one, else the caption's first line, cut to the
/// 100 characters YouTube allows. `<` and `>` are refused by the API, so they go.
fn youtube_title(title: Option<&str>, caption: &str) -> String {
    let raw = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| caption.lines().next().unwrap_or("").trim());
    raw.chars().filter(|c| *c != '<' && *c != '>').take(100).collect()
}

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        _ => "video/mp4",
    }
}

async fn publish(
    State(state): State<AppState>,
    Extension(ctx): Extension<Arc<SocialCtx>>,
    headers: HeaderMap,
    Json(body): Json<PublishBody>,
) -> Response {
    if body.platform != "youtube" {
        return not_built(&body.platform);
    }
    let Some(account) = account_for(&ctx, "youtube", body.account.as_deref()) else {
        return err(StatusCode::BAD_REQUEST, "name an account, or set AMUX_SOCIAL_YOUTUBE_ACCOUNT in server.env");
    };
    let path = std::path::Path::new(&body.file);
    let meta = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        _ => return err(StatusCode::BAD_REQUEST, format!("no such file: {}", body.file)),
    };
    let mime = mime_for(&body.file);
    if !mime.starts_with("video/") {
        return err(StatusCode::BAD_REQUEST, "YouTube's API uploads videos only; image posts are not in its API");
    }
    if meta.len() > MAX_UPLOAD_BYTES {
        return err(StatusCode::PAYLOAD_TOO_LARGE, format!("{} bytes is over the {MAX_UPLOAD_BYTES}-byte upload cap", meta.len()));
    }
    let title = youtube_title(body.title.as_deref(), &body.caption);
    if title.is_empty() {
        return err(StatusCode::BAD_REQUEST, "a YouTube video needs a title or a caption");
    }
    let requested = body.privacy.clone().unwrap_or_else(|| "public".into());
    if !matches!(requested.as_str(), "public" | "unlisted" | "private") {
        return err(StatusCode::BAD_REQUEST, "privacy is public, unlisted or private");
    }
    let public_ok = env_val(&ctx, "AMUX_SOCIAL_PUBLIC").as_deref() == Some("1");
    let sent_privacy = if requested == "private" || public_ok { requested.clone() } else { "private".to_string() };
    let token = match (ctx.token)(state.clone(), headers.clone(), account.clone()).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(platform = "youtube", %account, verdict = "social_token_unavailable", "no bearer for the posting account");
            return (StatusCode::BAD_GATEWAY, Json(json!({"ok": false, "error": "no YouTube token for this account", "token": e}))).into_response();
        }
    };
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("read {}: {e}", body.file)),
    };
    let snippet = json!({
        "snippet": {"title": title, "description": body.caption.chars().take(5000).collect::<String>(), "tags": body.tags, "categoryId": "28"},
        "status": {"privacyStatus": sent_privacy, "selfDeclaredMadeForKids": false},
    });
    let started = std::time::Instant::now();
    let video = match youtube_upload(&ctx, &token, &snippet, mime, bytes).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(platform = "youtube", %account, verdict = "social_upload_failed", error = %e, "upload failed");
            return (StatusCode::BAD_GATEWAY, Json(json!({"ok": false, "error": e}))).into_response();
        }
    };
    let Some(post_id) = video["id"].as_str().map(String::from) else {
        return (StatusCode::BAD_GATEWAY, Json(json!({"ok": false, "error": "YouTube answered without a video id"}))).into_response();
    };
    let applied = video["status"]["privacyStatus"].as_str().unwrap_or(&sent_privacy).to_string();
    let url = format!("https://www.youtube.com/watch?v={post_id}");
    let row = PostRow {
        platform: "youtube".into(),
        account: account.clone(),
        post_id: post_id.clone(),
        url: url.clone(),
        caption: body.caption.clone(),
        title: title.clone(),
        visibility: applied.clone(),
        file: body.file.clone(),
        session: caller(&headers),
        origin: "api".into(),
        posted_at: now(),
    };
    let id = match insert_post(&state, row).await {
        Ok(id) => id,
        Err(e) => {
            // The video exists; say so rather than letting a retry post it twice.
            tracing::error!(platform = "youtube", %post_id, verdict = "social_post_unrecorded", error = %e, "posted but not recorded");
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "posted": true, "post_id": post_id, "url": url, "error": format!("posted, but the ledger write failed: {e}")}))).into_response();
        }
    };
    let mut notes = Vec::new();
    if sent_privacy != requested {
        notes.push("asked for public, sent private: AMUX_SOCIAL_PUBLIC is not 1".to_string());
    }
    if applied != sent_privacy {
        notes.push(format!("YouTube applied {applied} instead of {sent_privacy}; an unaudited API project's uploads are locked private until Google's audit passes"));
    }
    tracing::info!(platform = "youtube", %account, %post_id, privacy = %applied, bytes = meta.len(),
        elapsed_ms = started.elapsed().as_millis() as u64, session = %caller(&headers), verdict = "social_posted", "social post published");
    (StatusCode::CREATED, Json(json!({
        "ok": true, "id": id, "platform": "youtube", "post_id": post_id, "url": url,
        "privacy_requested": requested, "privacy_status": applied, "notes": notes,
    }))).into_response()
}

/// YouTube's resumable upload: open a session with the metadata, then send the
/// bytes to the URL it returns. Returns the video resource.
async fn youtube_upload(ctx: &SocialCtx, token: &str, meta: &Value, mime: &str, bytes: Vec<u8>) -> Result<Value, String> {
    let open = ctx
        .http
        .post(format!("{}/videos?uploadType=resumable&part=snippet,status", ctx.youtube_upload))
        .bearer_auth(token)
        .header("X-Upload-Content-Type", mime)
        .header("X-Upload-Content-Length", bytes.len().to_string())
        .json(meta)
        .send()
        .await
        .map_err(|e| format!("open upload session: {e}"))?;
    let status = open.status();
    let location = open.headers().get("location").and_then(|v| v.to_str().ok()).map(String::from);
    if !status.is_success() {
        return Err(format!("open upload session: HTTP {status}: {}", google_error(open.text().await.unwrap_or_default())));
    }
    let location = location.ok_or("upload session opened without a Location header")?;
    let sent = ctx
        .http
        .put(&location)
        .bearer_auth(token)
        .header("Content-Type", mime)
        .body(bytes)
        .send()
        .await
        .map_err(|e| format!("send video bytes: {e}"))?;
    let status = sent.status();
    let text = sent.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("send video bytes: HTTP {status}: {}", google_error(text)));
    }
    serde_json::from_str(&text).map_err(|e| format!("upload reply was not JSON: {e}"))
}

/// Google's error message out of an error body, or the body cut short.
fn google_error(body: String) -> String {
    serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(String::from))
        .unwrap_or_else(|| body.chars().take(300).collect())
}

fn not_built(platform: &str) -> Response {
    match PLATFORMS.iter().find(|(id, _, _)| *id == platform) {
        Some((_, _, gate)) => (StatusCode::NOT_IMPLEMENTED, Json(json!({"ok": false, "error": format!("{platform} posting is not built yet"), "gate": gate}))).into_response(),
        None => err(StatusCode::BAD_REQUEST, format!("unknown platform {platform:?}; one of youtube, instagram, tiktok, x, linkedin")),
    }
}

struct PostRow {
    platform: String,
    account: String,
    post_id: String,
    url: String,
    caption: String,
    title: String,
    visibility: String,
    file: String,
    session: String,
    origin: String,
    posted_at: i64,
}

async fn insert_post(state: &AppState, r: PostRow) -> anyhow::Result<i64> {
    let (platform, post_id) = (r.platform.clone(), r.post_id.clone());
    state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO social_posts (platform, account, post_id, url, caption, title, visibility, file, session, origin, posted_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(platform, post_id) DO NOTHING",
                rusqlite::params![r.platform, r.account, r.post_id, r.url, r.caption, r.title, r.visibility, r.file, r.session, r.origin, r.posted_at],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await?;
    let conn = state.store.read()?;
    Ok(conn.query_row(
        "SELECT id FROM social_posts WHERE platform = ?1 AND post_id = ?2",
        rusqlite::params![platform, post_id],
        |row| row.get(0),
    )?)
}

#[derive(Deserialize)]
struct TrackBody {
    platform: String,
    post_id: String,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    posted_at: Option<i64>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    caption: String,
    #[serde(default)]
    title: String,
}

async fn track(
    State(state): State<AppState>,
    Extension(ctx): Extension<Arc<SocialCtx>>,
    headers: HeaderMap,
    Json(b): Json<TrackBody>,
) -> Response {
    if !PLATFORMS.iter().any(|(id, _, _)| *id == b.platform) {
        return not_built(&b.platform);
    }
    let post_id = b.post_id.trim().to_string();
    if post_id.is_empty() {
        return err(StatusCode::BAD_REQUEST, "post_id is required");
    }
    let account = account_for(&ctx, &b.platform, b.account.as_deref()).unwrap_or_default();
    let url = b.url.unwrap_or_else(|| if b.platform == "youtube" { format!("https://www.youtube.com/watch?v={post_id}") } else { String::new() });
    let row = PostRow {
        platform: b.platform.clone(),
        account,
        post_id: post_id.clone(),
        url,
        caption: b.caption,
        title: b.title,
        visibility: String::new(),
        file: String::new(),
        session: caller(&headers),
        origin: "tracked".into(),
        posted_at: b.posted_at.unwrap_or_else(now),
    };
    match insert_post(&state, row).await {
        Ok(id) => (StatusCode::CREATED, Json(json!({"ok": true, "id": id, "platform": b.platform, "post_id": post_id,
            "measured_by": if b.platform == "youtube" { "social-metrics job at 24h, 72h, 7d" } else { "nothing yet: this platform's connector is not built" }}))).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize, Default)]
struct ListQuery {
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

const POST_COLS: &str = "id, platform, account, post_id, url, caption, title, visibility, file, session, origin, posted_at";

fn post_json(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, i64>(0)?, "platform": r.get::<_, String>(1)?, "account": r.get::<_, String>(2)?,
        "post_id": r.get::<_, String>(3)?, "url": r.get::<_, String>(4)?, "caption": r.get::<_, String>(5)?,
        "title": r.get::<_, String>(6)?, "visibility": r.get::<_, String>(7)?, "file": r.get::<_, String>(8)?,
        "session": r.get::<_, String>(9)?, "origin": r.get::<_, String>(10)?, "posted_at": r.get::<_, i64>(11)?,
    }))
}

/// Every window for a post: its stored snapshot, or when it falls due.
fn windows_for(conn: &rusqlite::Connection, id: i64, posted_at: i64) -> Value {
    let mut out = serde_json::Map::new();
    for (w, secs) in WINDOWS {
        let snap = conn
            .query_row(
                "SELECT fetched_at, metrics, measured, error, attempts FROM social_post_metrics WHERE post_row = ?1 AND win = ?2",
                rusqlite::params![id, w],
                |r| Ok(json!({
                    "fetched_at": r.get::<_, i64>(0)?,
                    "metrics": serde_json::from_str::<Value>(&r.get::<_, String>(1)?).unwrap_or(Value::Null),
                    "measured": r.get::<_, i64>(2)? == 1,
                    "error": r.get::<_, Option<String>>(3)?,
                    "attempts": r.get::<_, i64>(4)?,
                })),
            )
            .ok();
        out.insert((*w).into(), snap.unwrap_or_else(|| json!({"measured": false, "due_at": posted_at + secs})));
    }
    Value::Object(out)
}

async fn list(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
    };
    let limit = q.limit.unwrap_or(200).clamp(1, 2000);
    let sql = format!("SELECT {POST_COLS} FROM social_posts WHERE (?1 IS NULL OR platform = ?1) ORDER BY posted_at DESC LIMIT ?2");
    let rows: Vec<Value> = match conn.prepare(&sql) {
        Ok(mut st) => st
            .query_map(rusqlite::params![q.platform, limit], post_json)
            .map(|it| it.flatten().collect())
            .unwrap_or_default(),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let posts: Vec<Value> = rows
        .into_iter()
        .map(|mut p| {
            let w = windows_for(&conn, p["id"].as_i64().unwrap_or(0), p["posted_at"].as_i64().unwrap_or(0));
            p["windows"] = w;
            p
        })
        .collect();
    Json(json!({"measured": true, "n_considered": posts.len(), "posts": posts})).into_response()
}

fn load_post(state: &AppState, id: i64) -> Result<Value, Box<Response>> {
    let conn = state.store.read().map_err(|e| Box::new(err(StatusCode::SERVICE_UNAVAILABLE, e.to_string())))?;
    let mut p = conn
        .query_row(&format!("SELECT {POST_COLS} FROM social_posts WHERE id = ?1"), [id], post_json)
        .map_err(|_| Box::new(err(StatusCode::NOT_FOUND, format!("no social post {id}"))))?;
    p["windows"] = windows_for(&conn, id, p["posted_at"].as_i64().unwrap_or(0));
    Ok(p)
}

async fn one(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match load_post(&state, id) {
        Ok(p) => Json(p).into_response(),
        Err(r) => *r,
    }
}

async fn live_metrics(
    State(state): State<AppState>,
    Extension(ctx): Extension<Arc<SocialCtx>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    let post = match load_post(&state, id) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    match fetch_metrics(&ctx, &state, headers, &post).await {
        Ok(m) => Json(json!({"ok": true, "measured": true, "post": id, "metrics": m})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"ok": false, "measured": false, "post": id, "error": e}))).into_response(),
    }
}

/// Current reach and engagement for a stored post, read from the platform.
pub async fn fetch_metrics(ctx: &SocialCtx, state: &AppState, headers: HeaderMap, post: &Value) -> Result<Value, String> {
    let platform = post["platform"].as_str().unwrap_or("");
    if platform != "youtube" {
        return Err(format!("{platform} metrics are not built yet"));
    }
    let account = post["account"].as_str().filter(|a| !a.is_empty()).map(String::from)
        .or_else(|| account_for(ctx, "youtube", None))
        .ok_or("the post has no account and AMUX_SOCIAL_YOUTUBE_ACCOUNT is unset")?;
    let token = (ctx.token)(state.clone(), headers, account)
        .await
        .map_err(|e| format!("no YouTube token: {e}"))?;
    youtube_metrics(ctx, &token, post["post_id"].as_str().unwrap_or(""), post["posted_at"].as_i64().unwrap_or(0)).await
}

async fn get_json(ctx: &SocialCtx, token: &str, url: &str) -> Result<Value, String> {
    let r = ctx.http.get(url).bearer_auth(token).send().await.map_err(|e| e.to_string())?;
    let status = r.status();
    let text = r.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", google_error(text)));
    }
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// Data API counters (live) plus Analytics totals (which lag one to three days).
/// YouTube's APIs do not expose thumbnail impressions, so `impressions` is null
/// and says so; views are the reach figure.
async fn youtube_metrics(ctx: &SocialCtx, token: &str, video: &str, posted_at: i64) -> Result<Value, String> {
    let v = get_json(ctx, token, &format!("{}/videos?part=statistics,status&id={video}", ctx.youtube_api)).await?;
    let item = v["items"].get(0).ok_or_else(|| format!("YouTube returned no video {video} for this account"))?;
    let stat = |k: &str| item["statistics"][k].as_str().and_then(|s| s.parse::<i64>().ok());
    let start = chrono::DateTime::from_timestamp(posted_at, 0).unwrap_or_default().format("%Y-%m-%d");
    let end = chrono::Utc::now().format("%Y-%m-%d");
    let metrics = "views,likes,comments,shares,estimatedMinutesWatched,averageViewDuration,subscribersGained";
    let analytics = get_json(ctx, token, &format!(
        "{}/reports?ids=channel==MINE&startDate={start}&endDate={end}&metrics={metrics}&filters=video=={video}",
        ctx.youtube_analytics
    )).await;
    let (a, a_status) = match &analytics {
        Ok(rep) => match rep["rows"].get(0).and_then(Value::as_array) {
            Some(row) => {
                let names: Vec<&str> = rep["columnHeaders"].as_array().map(|h| h.iter().filter_map(|c| c["name"].as_str()).collect()).unwrap_or_default();
                let m: serde_json::Map<String, Value> = names.iter().zip(row).map(|(n, v)| (n.to_string(), v.clone())).collect();
                (Value::Object(m), json!({"measured": true}))
            }
            None => (Value::Null, json!({"measured": false, "why_unmeasured": "no Analytics rows yet; YouTube Analytics lags one to three days"})),
        },
        Err(e) => (Value::Null, json!({"measured": false, "why_unmeasured": e})),
    };
    let views = stat("viewCount");
    let likes = stat("likeCount");
    let comments = stat("commentCount");
    let shares = a["shares"].as_i64();
    let engagement = match (likes, comments) {
        (Some(l), Some(c)) => Some(l + c + shares.unwrap_or(0)),
        _ => None,
    };
    Ok(json!({
        "views": views, "likes": likes, "comments": comments, "shares": shares,
        "engagement": engagement,
        "watch_minutes": a["estimatedMinutesWatched"], "avg_view_seconds": a["averageViewDuration"],
        "subscribers_gained": a["subscribersGained"],
        "impressions": Value::Null,
        "impressions_note": "YouTube's APIs do not expose thumbnail impressions; views is the reach figure",
        "privacy_status": item["status"]["privacyStatus"],
        "age_secs": now() - posted_at,
        "sources": {"data_api": {"measured": true}, "analytics": a_status},
    }))
}

/// The windows that have passed for posts without a measured snapshot:
/// `(post row, window)`. Unmeasured rows are retried up to `max_attempts`.
pub fn due_windows(conn: &rusqlite::Connection, at: i64, max_attempts: i64) -> rusqlite::Result<Vec<(i64, &'static str)>> {
    let mut out = Vec::new();
    let mut st = conn.prepare("SELECT id, posted_at FROM social_posts WHERE platform = 'youtube'")?;
    let posts: Vec<(i64, i64)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.flatten().collect();
    for (id, posted_at) in posts {
        for (w, secs) in WINDOWS {
            if at < posted_at + secs {
                continue;
            }
            let prior: Option<(i64, i64)> = conn
                .query_row(
                    "SELECT measured, attempts FROM social_post_metrics WHERE post_row = ?1 AND win = ?2",
                    rusqlite::params![id, w],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            match prior {
                Some((1, _)) => {}
                Some((_, n)) if n >= max_attempts => {}
                _ => out.push((id, *w)),
            }
        }
    }
    Ok(out)
}

/// One pass of the `social-metrics` job: snapshot every due window. Returns a
/// summary for the log line.
pub async fn snapshot_pass(ctx: &SocialCtx, state: &AppState) -> Value {
    let due = match state.store.read().map_err(|e| e.to_string()).and_then(|c| due_windows(&c, now(), 12).map_err(|e| e.to_string())) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(job = "social-metrics", measured = false, why_unmeasured = %e, "could not list due windows");
            return json!({"measured": false, "why_unmeasured": e});
        }
    };
    let (mut ok, mut failed) = (0, 0);
    for (id, window) in &due {
        let post = match load_post(state, *id) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let res = fetch_metrics(ctx, state, HeaderMap::new(), &post).await;
        let (metrics, measured, error) = match res {
            Ok(m) => (m, 1, None),
            Err(e) => (Value::Null, 0, Some(e)),
        };
        if measured == 1 { ok += 1 } else { failed += 1 }
        if let Some(e) = &error {
            tracing::warn!(job = "social-metrics", post = id, window, verdict = "social_snapshot_failed", error = %e, "window snapshot failed; retried next pass");
        }
        let (id, window, m) = (*id, window.to_string(), metrics.to_string());
        let _ = state.store.write_async(move |conn| {
            conn.execute(
                "INSERT INTO social_post_metrics (post_row, win, fetched_at, metrics, measured, error, attempts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)
                 ON CONFLICT(post_row, win) DO UPDATE SET fetched_at = ?3, metrics = ?4, measured = ?5, error = ?6, attempts = attempts + 1",
                rusqlite::params![id, window, now(), m, measured, error],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).await;
    }
    if !due.is_empty() {
        tracing::info!(job = "social-metrics", measured = true, n_considered = due.len(), ok, failed, "social window snapshots taken");
    }
    json!({"measured": true, "n_considered": due.len(), "ok": ok, "failed": failed})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_comes_from_the_caption_and_fits_youtube() {
        assert_eq!(youtube_title(None, "Eight agents, one board\nmore text"), "Eight agents, one board");
        assert_eq!(youtube_title(Some("  "), "cap"), "cap");
        assert_eq!(youtube_title(Some("a <b> c"), ""), "a b c");
        assert_eq!(youtube_title(Some(&"x".repeat(150)), "").chars().count(), 100);
    }

    #[test]
    fn a_window_is_due_once_it_passes_and_stops_once_measured_or_exhausted() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../../migrations/0134_social_posts.sql")).unwrap();
        conn.execute("INSERT INTO social_posts (platform, account, post_id, posted_at) VALUES ('youtube', 'a', 'v1', 1000)", []).unwrap();
        conn.execute("INSERT INTO social_posts (platform, account, post_id, posted_at) VALUES ('tiktok', 'a', 't1', 1000)", []).unwrap();
        assert!(due_windows(&conn, 1000 + 86_399, 3).unwrap().is_empty());
        assert_eq!(due_windows(&conn, 1000 + 86_400, 3).unwrap(), vec![(1, "24h")], "unbuilt platforms are not polled");
        assert_eq!(due_windows(&conn, 1000 + 700_000, 3).unwrap().len(), 3);
        conn.execute("INSERT INTO social_post_metrics (post_row, win, fetched_at, metrics, measured, attempts) VALUES (1, '24h', 1, '{}', 1, 1)", []).unwrap();
        conn.execute("INSERT INTO social_post_metrics (post_row, win, fetched_at, metrics, measured, attempts) VALUES (1, '72h', 1, 'null', 0, 1)", []).unwrap();
        conn.execute("INSERT INTO social_post_metrics (post_row, win, fetched_at, metrics, measured, attempts) VALUES (1, '7d', 1, 'null', 0, 3)", []).unwrap();
        assert_eq!(due_windows(&conn, 1000 + 700_000, 3).unwrap(), vec![(1, "72h")], "measured and exhausted windows are skipped, a failed one retried");
    }
}
