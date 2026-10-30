//! Social publishers and the posting queue (routed by api/social.rs).
//!
//! A platform is one [`Publisher`]: it checks a draft without touching the
//! network, publishes it, and reads a post's results back. Adding a platform
//! is one implementation plus one line in [`publisher`]; the queue, the CLI
//! and the 24h/72h/7d metrics windows pick it up from there.
//!
//! The queue (`social_queue`) holds posts with a publish time. One request can
//! name several platforms: each gets its own row and they share a `group_id`.
//! The `social-publish` job claims a row when it falls due and moves it to
//! `posted`, `failed` (with the error, after [`MAX_ATTEMPTS`] tries for a
//! retryable failure), or `uncertain`.
//!
//! `uncertain` means the platform may have received the post: the request was
//! sent and no answer came back, or the server stopped while a row was being
//! published. Such a row is never sent again on its own, because a second send
//! is a duplicate public post. Check the platform, then cancel it or
//! reschedule it with `force`.
//!
//! Publishing publicly stays the owner's switch (`AMUX_SOCIAL_PUBLIC=1`) for
//! every platform; each publisher applies it.

use super::social::{self, SocialCtx};
use super::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Tries for a failure that reached nothing before a row is marked failed.
pub const MAX_ATTEMPTS: i64 = 3;

/// Every platform with a publisher, in the order `GET /api/social` lists them.
/// The metrics windows are read for exactly these.
pub const PUBLISHERS: &[&str] = &["youtube", "devto"];

/// What to post: the request, with the account already resolved.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Draft {
    pub platform: String,
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub caption: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub privacy: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A post the platform accepted.
pub struct Published {
    pub post_id: String,
    pub url: String,
    /// The visibility the platform applied, which can differ from the one asked.
    pub visibility: String,
    pub title: String,
    pub notes: Vec<String>,
    /// Platform-specific reply fields, merged into the response.
    pub extra: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailKind {
    /// The draft or the account cannot be posted as it is. Not retried.
    Refused,
    /// Nothing reached the platform; safe to try again.
    Retry,
    /// The request may have reached the platform. Never sent again on its own.
    Uncertain,
}

#[derive(Debug)]
pub struct Failure {
    pub kind: FailKind,
    pub status: StatusCode,
    pub error: String,
    pub detail: Value,
}

impl Failure {
    pub fn refused(status: StatusCode, error: impl Into<String>) -> Self {
        Failure { kind: FailKind::Refused, status, error: error.into(), detail: Value::Null }
    }
    pub fn retry(error: impl Into<String>) -> Self {
        Failure { kind: FailKind::Retry, status: StatusCode::BAD_GATEWAY, error: error.into(), detail: Value::Null }
    }
    pub fn uncertain(error: impl Into<String>) -> Self {
        Failure { kind: FailKind::Uncertain, status: StatusCode::BAD_GATEWAY, error: error.into(), detail: Value::Null }
    }
    pub fn with(mut self, detail: Value) -> Self {
        self.detail = detail;
        self
    }
    /// A send that failed in reqwest: one that never connected reached nothing;
    /// any other may have been received.
    pub fn from_send(what: &str, e: &reqwest::Error) -> Self {
        if e.is_connect() || e.is_builder() {
            Failure::retry(format!("{what}: {e}"))
        } else {
            Failure::uncertain(format!("{what}: {e} (the platform may have received it)"))
        }
    }
    pub fn response(&self) -> Response {
        let mut body = json!({"ok": false, "error": self.error});
        if let Value::Object(extra) = &self.detail {
            for (k, v) in extra {
                body[k] = v.clone();
            }
        }
        if self.kind == FailKind::Uncertain {
            body["uncertain"] = json!(true);
        }
        (self.status, Json(body)).into_response()
    }
}

pub trait Publisher: Send + Sync {
    fn platform(&self) -> &'static str;
    /// Checks that need no network, run when a post is queued so a bad draft is
    /// refused then and not when it falls due.
    fn check(&self, ctx: &SocialCtx, d: &Draft) -> Result<(), Failure>;
    fn publish<'a>(&'a self, ctx: &'a SocialCtx, state: &'a AppState, headers: HeaderMap, d: &'a Draft)
        -> BoxFut<'a, Result<Published, Failure>>;
    /// Current reach and engagement for a stored post.
    fn metrics<'a>(&'a self, ctx: &'a SocialCtx, state: &'a AppState, headers: HeaderMap, post: &'a Value)
        -> BoxFut<'a, Result<Value, String>>;
}

pub fn publisher(platform: &str) -> Option<&'static dyn Publisher> {
    match platform {
        "youtube" => Some(&social::YouTube),
        "devto" => Some(&super::social_devto::DevTo),
        // Not in PUBLISHERS yet: it reads no metrics, and the windows job would
        // retry a read that cannot succeed on every pass.
        "linkedin" => Some(&super::social_browser::LinkedIn),
        "x" => Some(&super::social_browser::X),
        _ => None,
    }
}

/// The publish time a caller gave: epoch seconds, `now`, RFC 3339, or a local
/// `YYYY-MM-DDTHH:MM[:SS]` (also with a space).
pub fn parse_when(v: &Value) -> Result<i64, String> {
    if let Some(n) = v.as_i64() {
        return Ok(n);
    }
    let s = v.as_str().map(str::trim).ok_or("scheduled_at is epoch seconds or a time string")?;
    if s.eq_ignore_ascii_case("now") {
        return Ok(social::now());
    }
    if let Ok(n) = s.parse::<i64>() {
        return Ok(n);
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp());
    }
    for fmt in ["%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            use chrono::TimeZone;
            if let Some(t) = chrono::Local.from_local_datetime(&naive).earliest() {
                return Ok(t.timestamp());
            }
        }
    }
    Err(format!("scheduled_at {s:?} is not epoch seconds, now, RFC 3339 or YYYY-MM-DDTHH:MM"))
}

fn group_id() -> String {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    format!("sg-{}-{}", social::now(), b.iter().map(|x| format!("{x:02x}")).collect::<String>())
}

/// Queue drafts to publish at `when`. Every draft is checked first, so a bad
/// one refuses the whole request and nothing is half-queued.
pub async fn enqueue(state: &AppState, ctx: &SocialCtx, headers: &HeaderMap, drafts: Vec<Draft>, when: i64) -> Response {
    let at = social::now();
    if when < at - 60 {
        return social::err(StatusCode::BAD_REQUEST, format!("scheduled_at {when} is in the past (now {at})"));
    }
    for d in &drafts {
        let Some(p) = publisher(&d.platform) else {
            return social::not_built(&d.platform);
        };
        if let Err(mut f) = p.check(ctx, d) {
            f.detail = json!({"platform": d.platform});
            return f.response();
        }
    }
    let group = group_id();
    let session = social::caller(headers);
    let rows: Vec<(String, String, String)> = drafts
        .iter()
        .map(|d| (d.platform.clone(), d.account.clone(), serde_json::to_string(d).unwrap_or_default()))
        .collect();
    let (g, s) = (group.clone(), session.clone());
    let written = state
        .store
        .write_async(move |conn| {
            for (platform, account, draft) in &rows {
                conn.execute(
                    "INSERT INTO social_queue (group_id, platform, account, draft, scheduled_at, next_attempt_at, session, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?7)",
                    rusqlite::params![g, platform, account, draft, when, s, at],
                )?;
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    if let Err(e) = written {
        return social::err(StatusCode::INTERNAL_SERVER_ERROR, format!("queue write failed: {e}"));
    }
    let queued = match state.store.read() {
        Ok(conn) => queue_rows(&conn, &QueueQuery { group: Some(group.clone()), ..Default::default() }),
        Err(e) => return social::err(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
    };
    tracing::info!(group = %group, n = drafts.len(), scheduled_at = when, session = %session, verdict = "social_queued", "social post queued");
    (StatusCode::ACCEPTED, Json(json!({
        "ok": true, "group_id": group, "scheduled_at": when, "queued": queued,
        "published_by": "the social-publish job, within a minute of scheduled_at",
    }))).into_response()
}

#[derive(Deserialize, Default)]
pub struct QueueQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

const QUEUE_COLS: &str = "id, group_id, platform, account, draft, scheduled_at, status, attempts, next_attempt_at, error, post_row, post_id, url, session, created_at, updated_at, notes";

fn queue_json(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let draft: Draft = serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default();
    Ok(json!({
        "id": r.get::<_, i64>(0)?, "group_id": r.get::<_, String>(1)?, "platform": r.get::<_, String>(2)?,
        "account": r.get::<_, String>(3)?, "file": draft.file, "title": draft.title, "caption": draft.caption,
        "scheduled_at": r.get::<_, i64>(5)?, "status": r.get::<_, String>(6)?, "attempts": r.get::<_, i64>(7)?,
        "next_attempt_at": r.get::<_, i64>(8)?, "error": r.get::<_, Option<String>>(9)?,
        "post_row": r.get::<_, Option<i64>>(10)?, "post_id": r.get::<_, String>(11)?, "url": r.get::<_, String>(12)?,
        "session": r.get::<_, String>(13)?, "created_at": r.get::<_, i64>(14)?, "updated_at": r.get::<_, i64>(15)?,
        "notes": r.get::<_, Option<String>>(16)?,
    }))
}

fn queue_rows(conn: &rusqlite::Connection, q: &QueueQuery) -> Vec<Value> {
    let sql = format!(
        "SELECT {QUEUE_COLS} FROM social_queue WHERE (?1 IS NULL OR status = ?1) AND (?2 IS NULL OR group_id = ?2)
         ORDER BY scheduled_at, id LIMIT ?3"
    );
    let limit = q.limit.unwrap_or(200).clamp(1, 2000);
    let status = q.status.clone().filter(|s| s != "all");
    conn.prepare(&sql)
        .and_then(|mut st| Ok(st.query_map(rusqlite::params![status, q.group, limit], queue_json)?.flatten().collect()))
        .unwrap_or_default()
}

pub async fn list(State(state): State<AppState>, Query(q): Query<QueueQuery>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return social::err(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
    };
    let rows = queue_rows(&conn, &q);
    Json(json!({"measured": true, "n_considered": rows.len(), "queue": rows})).into_response()
}

fn row_status(state: &AppState, id: i64) -> Option<String> {
    state.store.read().ok()?.query_row("SELECT status FROM social_queue WHERE id = ?1", [id], |r| r.get(0)).ok()
}

/// Apply a guarded transition; on a miss, say what state the row is in.
async fn transition(state: &AppState, id: i64, sql: &'static str, params: Vec<rusqlite::types::Value>, verb: &str) -> Response {
    let res = state
        .store
        .write_async(move |conn| {
            let n = conn.execute(sql, rusqlite::params_from_iter(params))?;
            Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
        })
        .await;
    match res {
        Ok(r) if r.applied => {
            let row = state.store.read().ok().and_then(|c| {
                c.query_row(&format!("SELECT {QUEUE_COLS} FROM social_queue WHERE id = ?1"), [id], queue_json).ok()
            });
            tracing::info!(id, verb, verdict = "social_queue_changed", "social queue row changed");
            Json(json!({"ok": true, "row": row})).into_response()
        }
        Ok(_) => match row_status(state, id) {
            None => social::err(StatusCode::NOT_FOUND, format!("no queued social post {id}")),
            Some(s) => social::err(StatusCode::CONFLICT, format!("cannot {verb} a post that is {s}")),
        },
        Err(e) => social::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn cancel(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    transition(
        &state,
        id,
        "UPDATE social_queue SET status = 'cancelled', updated_at = ?2 WHERE id = ?1 AND status IN ('queued', 'failed', 'uncertain')",
        vec![id.into(), social::now().into()],
        "cancel",
    )
    .await
}

#[derive(Deserialize)]
pub struct RescheduleBody {
    scheduled_at: Value,
    /// Required to re-send an `uncertain` post, which may already be live.
    #[serde(default)]
    force: bool,
}

pub async fn reschedule(State(state): State<AppState>, Path(id): Path<i64>, Json(b): Json<RescheduleBody>) -> Response {
    let when = match parse_when(&b.scheduled_at) {
        Ok(w) => w,
        Err(e) => return social::err(StatusCode::BAD_REQUEST, e),
    };
    if row_status(&state, id).as_deref() == Some("uncertain") && !b.force {
        return social::err(StatusCode::CONFLICT, "this post may already be live (its last send got no answer); check the platform, then reschedule with force to send it again");
    }
    let states: &'static str = if b.force {
        "UPDATE social_queue SET status = 'queued', scheduled_at = ?2, next_attempt_at = ?2, attempts = 0, error = NULL, updated_at = ?3
         WHERE id = ?1 AND status IN ('queued', 'failed', 'uncertain')"
    } else {
        "UPDATE social_queue SET status = 'queued', scheduled_at = ?2, next_attempt_at = ?2, attempts = 0, error = NULL, updated_at = ?3
         WHERE id = ?1 AND status IN ('queued', 'failed')"
    };
    transition(&state, id, states, vec![id.into(), when.into(), social::now().into()], "reschedule").await
}

/// This process, so a row it claimed can be told from one a dead process left.
fn boot_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| format!("{}-{}", std::process::id(), social::now()))
}

/// Seconds to wait after failed try number `attempts + 1`: 1, 4, then 16 minutes.
fn backoff(attempts: i64) -> i64 {
    60 * 4_i64.pow(attempts.clamp(0, 2) as u32)
}

struct Claimed {
    id: i64,
    draft: Draft,
    attempts: i64,
    session: String,
}

/// One pass of the `social-publish` job: mark rows a dead process left mid-post
/// as uncertain, then publish every due row. Returns a summary for the log.
pub async fn publish_pass(ctx: &SocialCtx, state: &AppState) -> Value {
    let at = social::now();
    let me = boot_id().to_string();
    let orphaned = state
        .store
        .write_async({
            let me = me.clone();
            move |conn| {
                let n = conn.execute(
                    "UPDATE social_queue SET status = 'uncertain', updated_at = ?2,
                       error = 'the server stopped while this post was being published, so it may be live; check the platform, then cancel it or reschedule it with force'
                     WHERE status = 'publishing' AND COALESCE(claimed_by, '') != ?1",
                    rusqlite::params![me, at],
                )?;
                Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
            }
        })
        .await
        .map(|r| r.applied)
        .unwrap_or(false);
    if orphaned {
        tracing::warn!(job = "social-publish", verdict = "social_queue_uncertain", reason = "orphaned_claim", "a post left mid-publish by a stopped server was marked uncertain, not re-sent");
    }
    let due: Vec<i64> = match state.store.read().map_err(|e| e.to_string()).and_then(|c| {
        c.prepare("SELECT id FROM social_queue WHERE status = 'queued' AND next_attempt_at <= ?1 ORDER BY next_attempt_at, id LIMIT 20")
            .and_then(|mut st| Ok(st.query_map([at], |r| r.get(0))?.flatten().collect()))
            .map_err(|e| e.to_string())
    }) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(job = "social-publish", measured = false, why_unmeasured = %e, "could not list due posts");
            return json!({"measured": false, "why_unmeasured": e});
        }
    };
    let (mut posted, mut retried, mut failed, mut uncertain) = (0, 0, 0, 0);
    for id in &due {
        let Some(c) = claim(state, *id, &me).await else { continue };
        let mut headers = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&c.session) {
            headers.insert("x-amux-session", v);
        }
        let result = match publisher(&c.draft.platform) {
            Some(p) => match p.check(ctx, &c.draft) {
                Ok(()) => p.publish(ctx, state, headers.clone(), &c.draft).await,
                Err(f) => Err(f),
            },
            None => Err(Failure::refused(StatusCode::NOT_IMPLEMENTED, format!("{} has no publisher", c.draft.platform))),
        };
        match result {
            Ok(p) => {
                posted += 1;
                record_posted(state, &c, p).await;
            }
            Err(f) => {
                let attempts = c.attempts + 1;
                let (status, next) = match f.kind {
                    FailKind::Retry if attempts < MAX_ATTEMPTS => ("queued", at + backoff(c.attempts)),
                    FailKind::Uncertain => ("uncertain", at),
                    _ => ("failed", at),
                };
                match status {
                    "queued" => retried += 1,
                    "uncertain" => uncertain += 1,
                    _ => failed += 1,
                }
                let verdict = match status { "queued" => "social_queue_retry", "uncertain" => "social_queue_uncertain", _ => "social_queue_failed" };
                tracing::warn!(job = "social-publish", id = c.id, platform = %c.draft.platform, attempts, verdict, error = %f.error, "queued social post not published");
                let (id, err) = (c.id, f.error.clone());
                let _ = state.store.write_async(move |conn| {
                    conn.execute(
                        "UPDATE social_queue SET status = ?2, attempts = ?3, next_attempt_at = ?4, error = ?5, updated_at = ?6 WHERE id = ?1",
                        rusqlite::params![id, status, attempts, next, err, social::now()],
                    )?;
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                }).await;
            }
        }
    }
    if !due.is_empty() {
        tracing::info!(job = "social-publish", measured = true, n_considered = due.len(), posted, retried, failed, uncertain, "social queue pass");
    }
    json!({"measured": true, "n_considered": due.len(), "posted": posted, "retried": retried, "failed": failed, "uncertain": uncertain})
}

/// Take one due row for this process. A row another pass already took is skipped.
async fn claim(state: &AppState, id: i64, me: &str) -> Option<Claimed> {
    let me = me.to_string();
    let at = social::now();
    let took = state
        .store
        .write_async(move |conn| {
            let n = conn.execute(
                "UPDATE social_queue SET status = 'publishing', claimed_at = ?2, claimed_by = ?3, updated_at = ?2 WHERE id = ?1 AND status = 'queued'",
                rusqlite::params![id, at, me],
            )?;
            Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
        })
        .await
        .ok()?;
    if !took.applied {
        return None;
    }
    let conn = state.store.read().ok()?;
    conn.query_row("SELECT draft, attempts, session FROM social_queue WHERE id = ?1", [id], |r| {
        Ok(Claimed {
            id,
            draft: serde_json::from_str(&r.get::<_, String>(0)?).unwrap_or_default(),
            attempts: r.get(1)?,
            session: r.get(2)?,
        })
    })
    .ok()
}

async fn record_posted(state: &AppState, c: &Claimed, p: Published) {
    let row = social::PostRow {
        platform: c.draft.platform.clone(),
        account: c.draft.account.clone(),
        post_id: p.post_id.clone(),
        url: p.url.clone(),
        caption: c.draft.caption.clone(),
        title: p.title.clone(),
        visibility: p.visibility.clone(),
        file: c.draft.file.clone(),
        session: c.session.clone(),
        origin: "queue".into(),
        posted_at: social::now(),
    };
    let ledger = social::insert_post(state, row).await;
    let (post_row, ledger_err) = match &ledger {
        Ok(id) => (Some(*id), None),
        Err(e) => {
            tracing::error!(job = "social-publish", id = c.id, post_id = %p.post_id, verdict = "social_post_unrecorded", error = %e, "posted but not recorded in the ledger");
            (None, Some(format!("posted, but the ledger write failed: {e}")))
        }
    };
    let notes = p.notes.join("; ");
    tracing::info!(job = "social-publish", id = c.id, platform = %c.draft.platform, post_id = %p.post_id, verdict = "social_queue_posted", "queued social post published");
    let (id, post_id, url) = (c.id, p.post_id, p.url);
    let notes = (!notes.is_empty()).then_some(notes);
    let _ = state.store.write_async(move |conn| {
        conn.execute(
            "UPDATE social_queue SET status = 'posted', post_row = ?2, post_id = ?3, url = ?4, error = ?5, notes = ?6, attempts = attempts + 1, updated_at = ?7 WHERE id = ?1",
            rusqlite::params![id, post_row, post_id, url, ledger_err, notes, social::now()],
        )?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await;
}
