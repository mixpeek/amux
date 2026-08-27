//! Structured request log + native `/api/logs` (AMUX-2605).
//!
//! Three pieces, one file:
//!
//! 1. **The substrate**: [`middleware`] wraps the WHOLE router (outside the
//!    alias rewrite, so `path` is the RAW path the client sent) and records
//!    every API request into `_amux_request_log` (migration 0010). Rows ride
//!    a bounded channel to a background task that batch-inserts through the
//!    single-writer store — logging can never block or fail a request; on
//!    any error the row is dropped with a `tracing::warn`.
//! 2. **The Logs tab**: `GET /api/logs` + `GET /api/logs/raw`, ported from
//!    the Python server's LIVE handlers (amux-server.py:67673 — the second
//!    pair at :71933 is dead code: Python's dispatch is first-match and the
//!    :67673 block answers first). Same response shapes; where Python reads
//!    its own server.log, this serves the request log UNIONED with the
//!    tracing tail (`~/.amux/logs/server-rs.log`), each line labelled with
//!    its source.
//! 3. **Worker subset**: the `worker` column is derived from the path
//!    (`/api/sessions/{name}/*` -> name, `/api/workers/{id}/*` -> id), so a
//!    per-worker log is a FILTER over the global log (`?worker=`), never a
//!    second log to keep in step.
//!
//! Size discipline (the 25MB-dictation-upload rule): request bodies are
//! NEVER read by the logger — `req_bytes` comes from Content-Length only.
//! `error_body` (first [`ERROR_BODY_CHARS`] chars) is captured ONLY for
//! status >= 400, where bodies are small JSON by construction (and the
//! python proxy already buffers whole responses, so buffering a 4xx/5xx
//! here adds no new cost). `user_agent`/`req_meta` are char-capped.
//!
//! Retention: rows older than `AMUX_REQLOG_RETAIN_DAYS` (default 14; env >
//! server.env > default) are deleted opportunistically — every
//! [`SWEEP_EVERY`] inserted rows — and the delete COUNT is logged, so a
//! sweep that fires is visible and one that never fires is absent from the
//! log (ethos rule 4).

use super::AppState;
use crate::db::{SharedStore, WriteOutcome};
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::TimeZone;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;

/// Caps, all applied BEFORE a row enters the channel. Everything stored is
/// bounded; nothing about a request can make its row large.
const USER_AGENT_CHARS: usize = 300;
const QUERY_CHARS: usize = 500;
const CONTENT_TYPE_CHARS: usize = 120;
// AMUX-3132: 500 cut the gate-not-acknowledged 409 body mid-object. That body
// carries the DISCRIMINATOR — `gate` (required), `missing` (the unmet subset),
// and `you_sent` (the caller's own gate_checked) — but a full gate refusal with
// its `how_to_ack` block runs ~600-900 chars, so `missing`/`you_sent` were
// truncated away and a log reader saw only the required gate and concluded the
// CLIENT was right and the server refused wrongly (109 such 409s/day, and the
// next reader reaches the same wrong verdict). Widened so the whole refusal —
// including what the caller sent — survives; the response already echoes the
// submission via `you_sent`, so this needs no request-body recording (which
// would carry content and privacy that this cap-bounded log deliberately avoids).
const ERROR_BODY_CHARS: usize = 2000;
/// Bounded channel: if the writer falls behind, rows are DROPPED (with a
/// warn), never queued unboundedly and never back-pressured onto requests.
const QUEUE_CAP: usize = 10_000;
/// Max rows folded into one write transaction.
const BATCH_MAX: usize = 512;
/// Retention sweep cadence: once per this many inserted rows.
const SWEEP_EVERY: u64 = 1000;
const DEFAULT_RETAIN_DAYS: f64 = 14.0;

// ---------------------------------------------------------------------------
// Row + logger (the substrate)
// ---------------------------------------------------------------------------

/// One request, fully capped, ready to insert.
#[derive(Debug)]
pub struct LogRow {
    pub ts: f64,
    pub method: String,
    pub path: String,
    pub family: String,
    pub status: u16,
    pub latency_ms: f64,
    pub client_ip: String,
    pub user_agent: String,
    pub amux_session: String,
    pub worker: Option<String>,
    pub req_bytes: Option<i64>,
    pub resp_bytes: Option<i64>,
    pub answered_by: String,
    pub error_body: Option<String>,
    pub req_meta: Option<String>,
}

/// Cheap-to-clone handle the middleware sends rows through.
#[derive(Clone)]
pub struct RequestLogger {
    tx: tokio::sync::mpsc::Sender<LogRow>,
}

impl RequestLogger {
    pub fn spawn(store: SharedStore) -> Self {
        Self::spawn_with(store, retain_days_config(), SWEEP_EVERY)
    }

    /// Test seam: retention window + sweep cadence injectable so the sweep
    /// can be exercised without inserting 1000 rows.
    pub fn spawn_with(store: SharedStore, retain_days: f64, sweep_every: u64) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<LogRow>(QUEUE_CAP);
        // No runtime (a sync-context caller building a router for
        // inspection): rows will be dropped at send. Every production and
        // test caller runs inside tokio, so this is a guard, not a path.
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::warn!("request log: no tokio runtime at spawn — rows will be dropped");
            return Self { tx };
        }
        tokio::spawn(async move {
            let mut since_sweep: u64 = 0;
            while let Some(first) = rx.recv().await {
                let mut batch = vec![first];
                while batch.len() < BATCH_MAX {
                    match rx.try_recv() {
                        Ok(r) => batch.push(r),
                        Err(_) => break,
                    }
                }
                since_sweep += batch.len() as u64;
                let sweep = since_sweep >= sweep_every;
                if sweep {
                    since_sweep = 0;
                }
                // AF-175: the boot of the process writing this batch. Constant
                // within a process, so read once here.
                let boot = crate::runtime_jobs::heartbeat::boot_at();
                let res = store
                    .write_async(move |conn| {
                        {
                            let mut stmt = conn.prepare_cached(
                                "INSERT INTO _amux_request_log \
                                 (ts, method, path, family, status, latency_ms, client_ip, \
                                  user_agent, amux_session, worker, req_bytes, resp_bytes, \
                                  answered_by, error_body, req_meta, boot_at) \
                                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
                            )?;
                            for r in &batch {
                                stmt.execute(rusqlite::params![
                                    r.ts,
                                    r.method,
                                    r.path,
                                    r.family,
                                    r.status,
                                    r.latency_ms,
                                    r.client_ip,
                                    r.user_agent,
                                    r.amux_session,
                                    r.worker,
                                    r.req_bytes,
                                    r.resp_bytes,
                                    r.answered_by,
                                    r.error_body,
                                    r.req_meta,
                                                                    // AF-175: WHICH PROCESS logged this row.
                                    // Read once per batch below rather than
                                    // per row — it cannot change inside a
                                    // process, and re-reading it per row would
                                    // imply it could.
                                    boot,
])?;
                            }
                        }
                        if sweep {
                            let cutoff = unix_now() - retain_days * 86400.0;
                            let deleted = conn.execute(
                                "DELETE FROM _amux_request_log WHERE ts < ?1",
                                rusqlite::params![cutoff],
                            )?;
                            // The COUNT is the point (mandate: "count logged"):
                            // a sweep whose effect is invisible is a sweep
                            // nobody can verify fired.
                            tracing::info!(
                                deleted,
                                retain_days,
                                "request-log retention sweep"
                            );
                        }
                        // applied:false — deliberately NO revision bump. The
                        // request log is observability substrate, not fleet
                        // state: bumping `_amux_rev` here would publish a
                        // phantom state change to every delta-sync client on
                        // EVERY request, and the SPA's own polling would then
                        // generate revisions that trigger more polling. The
                        // transaction still commits (apply_write commits
                        // regardless of `applied`).
                        Ok(WriteOutcome { applied: false, events: vec![] })
                    })
                    .await;
                if let Err(e) = res {
                    // Never propagate: logging must not fail anything.
                    tracing::warn!(error = %e, "request-log insert failed; rows dropped");
                }
            }
        });
        Self { tx }
    }

    fn send(&self, row: LogRow) {
        if let Err(e) = self.tx.try_send(row) {
            // Full or closed: drop the row, say so. A blocked logger must
            // never become back-pressure on the request path.
            tracing::warn!(error = %e, "request-log queue rejected a row (dropped)");
        }
    }
}

/// `AMUX_REQLOG_RETAIN_DAYS`: process env wins, then server.env, then 14 —
/// the same precedence config.rs gives every other knob.
fn retain_days_config() -> f64 {
    if let Some(d) = std::env::var("AMUX_REQLOG_RETAIN_DAYS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
    {
        return d;
    }
    crate::config::parse_env_file(&super::settings::amux_home().join("server.env"))
        .get("AMUX_REQLOG_RETAIN_DAYS")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(DEFAULT_RETAIN_DAYS)
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// Wrap the finished app (AFTER `aliases::alias_layer`, so this middleware
/// runs BEFORE the alias rewrite and sees the RAW client path). Same
/// wrapping shape as `alias_layer` — outer router whose fallback is the real
/// app — so it provably applies to every route including fallbacks.
pub fn layer(app: Router, store: SharedStore) -> Router {
    layer_with(app, RequestLogger::spawn(store))
}

/// Seam for tests: exact production wiring, injectable logger.
pub fn layer_with(app: Router, logger: RequestLogger) -> Router {
    Router::new()
        .fallback_service(app)
        .layer(axum::middleware::from_fn_with_state(logger, middleware))
}

/// What gets logged: every /api request EXCEPT `/api/events` (a long-lived
/// SSE stream — Python skips it too) and `/api/debug/*` (a debugging session
/// polling the instruments must not flood the instrument). Everything
/// outside /api — the SPA shell, /health, sw.js, icons — is a static-asset
/// or liveness path and is skipped by the prefix rule. Nothing else is
/// excluded.
fn should_log(path: &str) -> bool {
    if path != "/api" && !path.starts_with("/api/") {
        return false;
    }
    if path == "/api/events" || path.starts_with("/api/events/") {
        return false;
    }
    if path == "/api/debug" || path.starts_with("/api/debug/") {
        return false;
    }
    true
}

pub async fn middleware(State(logger): State<RequestLogger>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if !should_log(&path) {
        return next.run(req).await;
    }
    let ts = unix_now();
    let started = std::time::Instant::now();
    let method = req.method().as_str().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    // Scoped block: a closure borrowing `req` may not outlive the borrow
    // into `next.run(req)` — and `&Request` is !Send (Body is !Sync), so the
    // closure must also drop before the await or the whole future loses Send.
    let (user_agent, amux_session, content_type) = {
        let hdr = |name: &str| {
            req.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        (
            truncate_chars(&hdr("user-agent"), USER_AGENT_CHARS),
            hdr("x-amux-session"),
            truncate_chars(&hdr("content-type"), CONTENT_TYPE_CHARS),
        )
    };
    // Content-Length ONLY — the logger never reads a request body (a
    // dictation upload is 25MB of audio; the SIZE is the telemetry).
    let req_bytes = req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    let client_ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_default();
    let worker = worker_of(&path, &query);
    let family = family_of(&path);

    let res = next.run(req).await;
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;

    let status = res.status().as_u16();
    let answered_by = res
        .headers()
        .get("x-amux-answered-by")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("native")
        .to_string();
    let mut resp_bytes = res
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    // error_body only for failures. Error responses in this server (and the
    // python proxy, which buffers whole bodies anyway) are small JSON, so
    // buffering one is bounded in practice; success responses — including
    // multi-GB /api/file/raw streams — are never touched.
    let (res, error_body) = if status >= 400 {
        let (parts, body) = res.into_parts();
        match axum::body::to_bytes(body, usize::MAX).await {
            Ok(bytes) => {
                resp_bytes = Some(bytes.len() as i64);
                let enc = parts
                    .headers
                    .get(axum::http::header::CONTENT_ENCODING)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let text = decoded_error_body(&bytes, enc);
                (
                    Response::from_parts(parts, axum::body::Body::from(bytes)),
                    if text.is_empty() { None } else { Some(text) },
                )
            }
            Err(e) => {
                // The underlying body stream errored — the response was
                // already undeliverable; hand back what remains.
                tracing::warn!(error = %e, %path, "error-body capture failed");
                (Response::from_parts(parts, axum::body::Body::empty()), None)
            }
        }
    } else {
        (res, None)
    };

    let mut meta = serde_json::Map::new();
    if !query.is_empty() {
        meta.insert("query".into(), json!(truncate_chars(&query, QUERY_CHARS)));
    }
    if !content_type.is_empty() {
        meta.insert("content_type".into(), json!(content_type));
    }
    // AMUX-3513: an endpoint with requested-wait semantics (browser `wait`
    // polls up to a caller-chosen budget) declares it via this response
    // header, and the latency detectors skip the row — a timed-out wait is
    // the budget the CALLER asked for, not the service getting slower.
    if let Some(v) = res.headers().get("x-amux-slow-ok").and_then(|v| v.to_str().ok()) {
        meta.insert("slow_ok".into(), json!(truncate_chars(v, 40)));
    }
    let req_meta = if meta.is_empty() {
        None
    } else {
        Some(Value::Object(meta).to_string())
    };

    logger.send(LogRow {
        ts,
        method,
        path,
        family,
        status,
        latency_ms,
        client_ip,
        user_agent,
        amux_session,
        worker,
        req_bytes,
        resp_bytes,
        answered_by,
        error_body,
        req_meta,
    });
    res
}

// ---------------------------------------------------------------------------
// Derivations
// ---------------------------------------------------------------------------

/// Family = the boundary-registry family that owns this path (longest
/// match), else the first two segments (`/api/<seg>`). Registry-derived so
/// sweep groupings share the predicate of the ownership table they report
/// on; the fallback covers python-only paths (e.g. `/api/git/...`) that a
/// client sent to this origin.
pub fn family_of(path: &str) -> String {
    let mut best: Option<&str> = None;
    let owns = |fam: &str| path == fam || (path.starts_with(fam) && path[fam.len()..].starts_with('/'));
    for (fam, _) in super::py_proxy::NATIVE_FAMILIES {
        if owns(fam) && best.is_none_or(|b| fam.len() > b.len()) {
            best = Some(fam);
        }
    }
    for f in super::py_proxy::PROXIED_FAMILIES {
        if owns(f.family) && best.is_none_or(|b| f.family.len() > b.len()) {
            best = Some(f.family);
        }
    }
    best.map(str::to_string)
        .unwrap_or_else(|| path.split('/').take(3).collect::<Vec<_>>().join("/"))
}

/// Path-derived TARGET worker: `/api/sessions/{name}/*` -> name,
/// `/api/workers/{id}/*` -> id, else None. This single derivation is what
/// makes the worker log a subset of the global log. `/api/sessions/self`
/// resolves through its `?session=` query param (that route's contract)
/// rather than recording the literal "self".
pub fn worker_of(path: &str, query: &str) -> Option<String> {
    for prefix in ["/api/sessions/", "/api/workers/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            let seg = rest.split('/').next().unwrap_or("");
            if seg.is_empty() {
                return None;
            }
            let name = percent_decode(seg);
            if name == "self" {
                return query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("session=").or_else(|| kv.strip_prefix("worker=")))
                    .map(percent_decode)
                    .filter(|s| !s.is_empty());
            }
            return Some(name);
        }
    }
    None
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Char-boundary-safe truncation (a byte slice through a UTF-8 char panics).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Decode a captured error body for storage, honouring `Content-Encoding`.
///
/// INCIDENT (AF-57, found by the 2026-08-15 log sweep). This middleware is the
/// OUTERMOST layer — deliberately, so it records the RAW path the client sent —
/// which means it runs AFTER `CompressionLayer`, so for any client sending
/// `Accept-Encoding: gzip` the bytes here are already gzipped. They were then
/// put through `String::from_utf8_lossy` and stored, which does not merely make
/// them unreadable: 875 of ~3.8KB became U+FFFD in the specimen, so `\x1f\x8b`
/// is now `\x1f\xef\xbf\xbd` and the gzip stream is UNRECOVERABLE. The field
/// exists so a 5xx can be diagnosed without a repro, and `/api/why` and autofix
/// both read it; for every compressed error response all three got noise, and
/// 2KB of destroyed bytes was written per row to hold it.
///
/// It hid because the failure is invisible from the producer's side and only
/// SOMETIMES fires: a curl without `Accept-Encoding` stores perfect JSON, so the
/// same endpoint reads fine or reads as mojibake depending on the client. Half
/// the groups in that sweep were readable, which is exactly what makes the
/// broken half look like a weird payload rather than a logging bug.
///
/// Every branch is honest: decode when we can, and when we cannot say SO in the
/// stored text rather than writing bytes that merely look like data. A marker a
/// reader can act on beats a plausible-looking string that is noise (ethos rule
/// 4 — a wrong answer must be detectable from what we keep).
fn decoded_error_body(bytes: &[u8], content_encoding: &str) -> String {
    let enc = content_encoding.trim().to_ascii_lowercase();
    let raw: std::borrow::Cow<'_, [u8]> = match enc.as_str() {
        "" | "identity" => std::borrow::Cow::Borrowed(bytes),
        "gzip" | "x-gzip" => {
            use std::io::Read;
            let mut out = Vec::new();
            match flate2::read::GzDecoder::new(bytes)
                .take(ERROR_BODY_DECODE_LIMIT)
                .read_to_end(&mut out)
            {
                Ok(_) => std::borrow::Cow::Owned(out),
                // Truncated or corrupt stream: say which, rather than storing
                // the compressed bytes and letting a reader think it is content.
                Err(e) => {
                    // WARN, not silence: this is the two-fixes rule applied to
                    // the fix itself. The marker below reaches the daily sweep
                    // (it lands in /api/logs/analyze's sample), but a sweep runs
                    // once a day and a reader has to be looking at error_body.
                    // A WARN puts the same fact where a log sweep finds it too.
                    tracing::warn!(target: "request_log",
                        "[error-body/AF-57] gzip error body failed to decode ({e}) — \
                         {} compressed bytes stored as a marker. Diagnostics for this \
                         status are degraded until it is fixed.", bytes.len());
                    return format!(
                        "<gzip error body could not be decoded: {e}; {} compressed bytes>",
                        bytes.len()
                    );
                }
            }
        }
        other => {
            // Not deduped deliberately. This fires only when a NEW Content-
            // Encoding appears on a response — i.e. someone changed the
            // compression layer — which is not a steady state. If it ever does
            // become a storm, the storm IS the signal, and a silenced one-shot
            // would hide exactly the fleet-wide change worth knowing about.
            tracing::warn!(target: "request_log",
                "[error-body/AF-57] error body is {other}-encoded and this build cannot \
                 decode it — {} bytes stored as a marker. Every error body in this \
                 encoding is now undiagnosable; teach decoded_error_body about {other}.",
                bytes.len());
            return format!(
                "<error body is {other}-encoded and this build cannot decode it; \
                 {} encoded bytes>",
                bytes.len()
            );
        }
    };
    let text = String::from_utf8_lossy(&raw);
    let n = text.chars().count();
    if n <= ERROR_BODY_CHARS {
        return text.into_owned();
    }
    // SAY that it was cut (AF-59). Error bodies are JSON, so a bare truncation
    // produces a string that is INVALID JSON and indistinguishable from a
    // malformed response — a consumer calling serde_json::from_str just fails,
    // and the natural conclusion is "the endpoint returned garbage" rather than
    // "the log trimmed it". Measured 2026-08-15: 6 of 277 bodies in 24h sat at
    // the cap, every one unparseable, including two gate-409s minutes old.
    //
    // AMUX-3132 already hit this and raised the cap 500 -> 2000, which moved the
    // threshold without changing the failure: anything over the new number is
    // silently invalid in exactly the same way. A cap that announces itself
    // stops being a trap at every size, which is why this is the fix rather than
    // a third number.
    let kept: String = text.chars().take(ERROR_BODY_CHARS).collect();
    format!("{kept}\u{2026}<truncated by the request log: kept {ERROR_BODY_CHARS} of {n} chars; \
             this string is deliberately NOT valid JSON>")
}

/// Cap on DECOMPRESSED error-body bytes. A compressed body is an amplification
/// vector — a few KB of gzip can expand to gigabytes — and this runs on every
/// 4xx/5xx, so the bound is on the output, not the input. Generous next to
/// `ERROR_BODY_CHARS` (2000) because truncation there should be what trims the
/// text; this is the safety net, not the policy.
const ERROR_BODY_DECODE_LIMIT: u64 = 1 << 20;

/// Minimal %XX decoder for path segments (session names arrive encoded from
/// the SPA). Invalid escapes pass through untouched.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            if let Some(b) = s
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// GET /api/logs + /api/logs/raw (the SPA Logs tab)
// ---------------------------------------------------------------------------

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(get_logs))
        .route("/raw", get(get_logs_raw))
        // Deterministic analysis (AMUX-2610): pure computation over the
        // request log — no model call anywhere in either handler.
        .route("/analyze", get(analyze))
        .route("/stats", get(stats))
}

/// GET /api/logs — Python's LIVE handler shape (amux-server.py:67673):
/// `{"events": [...], "count": N}`, params `category` / `session` / `limit`
/// (SPA sends `limit=500` + optional `category`, app.js:16520). Events carry
/// the exact key set of Python's ring events (ts/type/action/target/session/
/// detail/status/ip/actor/req/resp/method/ms) plus additive fields the sweep
/// needs (family/worker/latency_ms/answered_by/level/source/...).
///
/// Additive params (not sent by the SPA today, needed by the daily sweep —
/// docs/rust-migration/log-sweep.md): `worker` (the per-worker subset),
/// `since` (unix ts), `family`, `min_status`, `answered_by`. Additive
/// response field: `total_matched` — the pre-LIMIT count, so volume
/// questions are answerable without paging (the page-vs-corpus trap).
async fn get_logs(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let limit: i64 = q
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(200)
        .clamp(1, 2000);
    let category = q.get("category").map(String::as_str).unwrap_or("");
    // This EARLY RETURN was the bug (Ethan: "these tabs in the logs dont
    // work"). It answered empty for every category except http, so five of the
    // six Logs tabs were dead — and the comment justified it by saying the
    // categories "live in python's process", which stopped being true when this
    // origin became the only server.
    //
    // The row already carries `family`, and a category is just a human grouping
    // OF families, so the filter is a family clause rather than a fabrication.
    // `http` keeps meaning "everything not in a named group", which is what the
    // tab has always shown.

    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    if !category.is_empty() {
        let fams = families_for_category(category);
        if fams.is_empty() {
            // "http" = everything NOT claimed by a named group, so it is the
            // complement rather than a list.
            let named = NAMED_CATEGORY_FAMILIES.join("','");
            clauses.push(format!("COALESCE(family,'') NOT IN ('{named}')"));
        } else {
            let holes = fams.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            clauses.push(format!("COALESCE(family,'') IN ({holes})"));
            for f in fams {
                params.push(rusqlite::types::Value::Text(f.to_string()));
            }
        }
    }
    if let Some(s) = q.get("session").filter(|s| !s.is_empty()) {
        // Python's `session` concept on http events mixes the TARGET session
        // (classified from the path) with the caller header; match either so
        // neither reading silently filters to zero.
        clauses.push("(worker = ? OR amux_session = ?)".into());
        params.push(s.clone().into());
        params.push(s.clone().into());
    }
    if let Some(w) = q.get("worker").filter(|s| !s.is_empty()) {
        clauses.push("worker = ?".into());
        params.push(w.clone().into());
    }
    if let Some(f) = q.get("family").filter(|s| !s.is_empty()) {
        clauses.push("family = ?".into());
        params.push(f.clone().into());
    }
    if let Some(ts) = q.get("since").and_then(|v| v.parse::<f64>().ok()) {
        clauses.push("ts > ?".into());
        params.push(ts.into());
    }
    if let Some(ms) = q.get("min_status").and_then(|v| v.parse::<i64>().ok()) {
        clauses.push("status >= ?".into());
        params.push(ms.into());
    }
    if let Some(a) = q.get("answered_by").filter(|s| !s.is_empty()) {
        clauses.push("answered_by = ?".into());
        params.push(a.clone().into());
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };

    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let total: i64 = match conn.query_row(
        &format!("SELECT COUNT(*) FROM _amux_request_log{where_sql}"),
        rusqlite::params_from_iter(params.iter()),
        |r| r.get(0),
    ) {
        Ok(n) => n,
        Err(e) => return internal(e),
    };
    let sql = format!(
        "SELECT ts, method, path, family, status, latency_ms, client_ip, user_agent, \
                amux_session, worker, req_bytes, resp_bytes, answered_by, error_body, req_meta \
         FROM _amux_request_log{where_sql} ORDER BY ts DESC LIMIT {limit}"
    );
    let events: Vec<Value> = match (|| -> rusqlite::Result<Vec<Value>> {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), row_to_event)?;
        rows.collect()
    })() {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    Json(json!({
        "events": events,
        "count": events.len(),
        "total_matched": total,
    }))
    .into_response()
}

/// One DB row -> one Python-ring-shaped event (+ additive fields).
fn row_to_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let ts: f64 = r.get(0)?;
    let method: String = r.get(1)?;
    let path: String = r.get(2)?;
    let family: String = r.get(3)?;
    let status: i64 = r.get(4)?;
    let latency_ms: f64 = r.get(5)?;
    let client_ip: Option<String> = r.get(6)?;
    let user_agent: Option<String> = r.get(7)?;
    let amux_session: Option<String> = r.get(8)?;
    let worker: Option<String> = r.get(9)?;
    let req_bytes: Option<i64> = r.get(10)?;
    let resp_bytes: Option<i64> = r.get(11)?;
    let answered_by: String = r.get(12)?;
    let error_body: Option<String> = r.get(13)?;
    let req_meta: Option<String> = r.get(14)?;
    let session = worker.clone().or_else(|| amux_session.clone()).unwrap_or_default();
    let level = if status >= 500 {
        "error"
    } else if status >= 400 {
        "warn"
    } else {
        "info"
    };
    Ok(json!({
        // Python ring-event keys, verbatim (amux-server.py:3809):
        "ts": ts,
        "type": "http",
        "action": method.to_lowercase(),
        "target": path,
        "session": session,
        "detail": "",
        "status": status,
        "ip": client_ip.clone().unwrap_or_default(),
        "actor": amux_session.clone().unwrap_or_default(),
        "req": req_meta.clone().unwrap_or_default(),
        "resp": error_body.clone().unwrap_or_default(),
        "method": method,
        "ms": latency_ms.round() as i64,
        // Additive (rust request log; the sweep's discriminators):
        //
        // DERIVED from the family, not hardcoded "http" (Ethan, 2026-08-10:
        // "these tabs in the logs dont work"). The Logs view offers
        // All/Board/Workers/Memory/Files/HTTP, but every row claimed "http", so
        // five of the six tabs matched ZERO events — the filter worked, the data
        // could never satisfy it. The family is already on this row, so the
        // answer was present and being overwritten with a constant.
        "category": category_of(&family),
        "level": level,
        "latency_ms": latency_ms,
        "family": family,
        "worker": worker,
        "amux_session": amux_session,
        "answered_by": answered_by,
        "req_bytes": req_bytes,
        "resp_bytes": resp_bytes,
        "user_agent": user_agent,
        "source": "request_log",
    }))
}

/// GET /api/logs/raw — Python's shape (`{"lines": [...], "total": N}`,
/// param `lines`; SPA sends `lines=300`, app.js:16549). Python tails its
/// server.log; this origin's equivalents are BOTH the tracing tail
/// (`~/.amux/logs/server-rs.log`) and the request log formatted in Python's
/// own slog line format, merged by timestamp. The additive parallel
/// `sources` array names where each line came from (`server_log` /
/// `request_log`) without perturbing the lines the SPA renders.
async fn get_logs_raw(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let lines_n = q
        .get("lines")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(200)
        .clamp(1, 5000);
    let log_path = super::settings::amux_home().join("logs").join("server-rs.log");
    match raw_payload(&log_path, lines_n, &state) {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

fn raw_payload(log_path: &Path, lines_n: usize, state: &AppState) -> anyhow::Result<Value> {
    // Tracing tail. Missing file = empty, not an error (Python parity:
    // FileNotFoundError answers {"lines": [], "total": 0}).
    let text = std::fs::read_to_string(log_path).unwrap_or_default();
    let file_total = text.lines().count();
    let mut merged: Vec<(f64, String, &'static str)> = Vec::new();
    let mut last_ts = 0.0f64;
    for line in text.lines().rev().take(lines_n).collect::<Vec<_>>().into_iter().rev() {
        // tracing fmt lines start with an RFC3339 timestamp; continuation
        // lines (panics, multi-line fields) inherit the previous line's ts.
        if let Some(ts) = line
            .split_whitespace()
            .next()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        {
            last_ts = ts.timestamp() as f64 + f64::from(ts.timestamp_subsec_millis()) / 1000.0;
        }
        merged.push((last_ts, line.to_string(), "server_log"));
    }

    let conn = state.store.read()?;
    let reqlog_total: i64 =
        conn.query_row("SELECT COUNT(*) FROM _amux_request_log", [], |r| r.get(0))?;
    let mut stmt = conn.prepare(
        "SELECT ts, method, path, status, latency_ms, client_ip, amux_session, worker, answered_by \
         FROM _amux_request_log ORDER BY ts DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map([lines_n as i64], |r| {
        let ts: f64 = r.get(0)?;
        let method: String = r.get(1)?;
        let path: String = r.get(2)?;
        let status: i64 = r.get(3)?;
        let latency_ms: f64 = r.get(4)?;
        let ip: Option<String> = r.get(5)?;
        let session: Option<String> = r.get(6)?;
        let worker: Option<String> = r.get(7)?;
        let answered_by: String = r.get(8)?;
        Ok((ts, method, path, status, latency_ms, ip, session, worker, answered_by))
    })?;
    for row in rows {
        let (ts, method, path, status, latency_ms, ip, session, worker, answered_by) = row?;
        // Python's slog line format ("%Y-%m-%d %H:%M:%S [ip] METHOD path
        // status Nms"), so the SPA's raw-log styling treats both sources the
        // same; attribution fields append only when present.
        let when = chrono::Local
            .timestamp_opt(ts as i64, 0)
            .single()
            .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| format!("{ts:.0}"));
        let mut line = format!(
            "{when} [{}] {method} {path} {status} {:.0}ms",
            ip.unwrap_or_default(),
            latency_ms
        );
        if let Some(s) = session.filter(|s| !s.is_empty()) {
            line.push_str(&format!(" session={s}"));
        }
        if let Some(w) = worker.filter(|w| !w.is_empty()) {
            line.push_str(&format!(" worker={w}"));
        }
        if answered_by != "native" {
            line.push_str(&format!(" via={answered_by}"));
        }
        merged.push((ts, line, "request_log"));
    }

    merged.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let tail: Vec<_> = merged
        .into_iter()
        .rev()
        .take(lines_n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Ok(json!({
        "lines": tail.iter().map(|(_, l, _)| l.clone()).collect::<Vec<_>>(),
        "total": file_total as i64 + reqlog_total,
        "sources": tail.iter().map(|(_, _, s)| *s).collect::<Vec<_>>(),
    }))
}

use super::internal;

// ---------------------------------------------------------------------------
// ROUTE_TABLE — the routing truth (AMUX-2610)
// ---------------------------------------------------------------------------
//
// Why a hand-maintained table: axum's Router cannot enumerate its routes, and
// the alternative — a model grepping mod.rs + every module's routes() to
// answer "is PATCH routed at /api/board/statuses/{sid}?" — is exactly the
// expensive token spend AMUX-2610 exists to delete (ethos rule 2: model calls
// for judgment, computation for everything computable). The table is kept
// honest BOTH directions by tests/route_table.rs, which walks every entry
// against the real `api::router()` composition:
//   - a claimed path that is not routed fails (OPTIONS answers the SPA
//     catch-all's signature instead of the route's method router);
//   - a claimed method set that disagrees with what axum actually mounts
//     fails (the 405 `Allow` header is compared as a SET, so both an
//     over-claimed and an under-claimed method are caught), and a negative
//     twin — a method NOT listed — is fired and must 405.
//
// What is deliberately NOT a row:
//   - module-internal catch-alls whose only job is answering a JSON 404
//     (`/api/fs/{*rest}`, `/api/browser/{*rest}`, `/api/dictation/{*rest}`,
//     `/api/scope/{*rest}`, `/api/tags/{*rest}`, `/api/file/{*rest}`) — they
//     are "no such route" answerers, not capabilities;
//   - the SPA shell (`GET /` + GET-only `/{*path}`). Its GET-only-ness is
//     load-bearing for verdicts below: ANY non-GET to an unrouted path
//     answers 405 (with `Allow: GET`) from the catch-all, which reads like
//     "path exists, wrong method" and is really "no such path" — the exact
//     misdiagnosis /api/logs/analyze exists to prevent.
//
// `methods: &["*"]` = axum `any()` — the route accepts every method.

/// One routed path pattern (axum `{param}` / `{*rest}` syntax) + its methods.
pub struct RouteEntry {
    pub path: &'static str,
    pub methods: &'static [&'static str],
}

const ANY: &[&str] = &["*"];

/// Every route the composed router mounts (mod.rs + each module's routes()),
/// public and protected alike. Ordering is by mount site for diffability;
/// matching specificity is computed, not positional.
pub const ROUTE_TABLE: &[RouteEntry] = &[
    // -- public (outside require_bearer)
    RouteEntry { path: "/health", methods: &["GET"] },
    RouteEntry { path: "/manifest.json", methods: &["GET"] },
    RouteEntry { path: "/api/calendar.ics", methods: &["GET"] },
    RouteEntry { path: "/api/debug/tmux", methods: &["GET"] },
    RouteEntry { path: "/api/debug/scan", methods: &["GET"] },
    RouteEntry { path: "/api/debug/downtime", methods: &["GET"] },
    RouteEntry { path: "/api/debug/logs", methods: &["GET"] },
    RouteEntry { path: "/api/debug/boundary", methods: &["GET"] },
    RouteEntry { path: "/api/debug/legacy-port", methods: &["GET"] },
    RouteEntry { path: "/api/debug/routes", methods: &["GET"] },
    RouteEntry { path: "/api/debug/duplicate-deliveries", methods: &["GET"] },
    RouteEntry { path: "/api/system-jobs", methods: &["GET"] },
    RouteEntry { path: "/api/health/invariants", methods: &["GET"] },
    RouteEntry { path: "/api/debug/invariants", methods: &["GET"] },
    RouteEntry { path: "/api/gmail/callback", methods: &["GET"] },
    // -- core state
    RouteEntry { path: "/api/sync", methods: &["GET"] },
    RouteEntry { path: "/api/events", methods: &["GET"] },
    // -- board
    RouteEntry { path: "/api/board", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/board/statuses", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/board/statuses/reorder", methods: &["PUT"] },
    RouteEntry { path: "/api/board/statuses/{sid}", methods: &["PATCH", "DELETE"] },
    RouteEntry { path: "/api/board/session-gates", methods: &["GET", "PATCH"] },
    RouteEntry { path: "/api/board/clear-done", methods: &["POST"] },
    RouteEntry { path: "/api/board/{id}", methods: &["GET", "PATCH", "DELETE"] },
    RouteEntry { path: "/api/board/{id}/archive", methods: &["POST"] },
    RouteEntry { path: "/api/board/{id}/restore", methods: &["POST"] },
    // -- workers (+dead-letters merge)
    RouteEntry { path: "/api/workers", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/workers/{id}", methods: &["GET", "PATCH", "DELETE"] },
    RouteEntry { path: "/api/workers/{id}/start", methods: &["POST"] },
    RouteEntry { path: "/api/workers/{id}/stop", methods: &["POST"] },
    RouteEntry { path: "/api/workers/{id}/peek", methods: &["GET"] },
    RouteEntry { path: "/api/workers/{id}/send", methods: &["POST"] },
    RouteEntry { path: "/api/workers/{id}/dead-letters", methods: &["GET"] },
    // -- memories / messages / schedules / verify / prefs / criteria
    RouteEntry { path: "/api/memories", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/memories/{id}", methods: &["GET", "PATCH", "DELETE"] },
    RouteEntry { path: "/api/messages", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/messages/accountability", methods: &["GET"] },
    RouteEntry { path: "/api/messages/{id}", methods: &["GET"] },
    RouteEntry { path: "/api/messages/{id}/ack", methods: &["POST"] },
    RouteEntry { path: "/api/messages/{id}/acted", methods: &["POST"] },
    RouteEntry { path: "/api/schedules", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/schedules/runs", methods: &["GET"] },
    RouteEntry { path: "/api/schedules/audit", methods: &["GET"] },
    RouteEntry { path: "/api/schedules/{id}", methods: &["GET", "PATCH", "DELETE"] },
    RouteEntry { path: "/api/schedules/{id}/run", methods: &["POST"] },
    RouteEntry { path: "/api/verify/{id}", methods: &["POST"] },
    RouteEntry { path: "/api/prefs", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/criteria/{id}", methods: &["GET", "PUT"] },
    // -- metrics / usage / alerts / stats
    RouteEntry { path: "/api/metrics", methods: &["GET"] },
    RouteEntry { path: "/api/metrics/fleet", methods: &["GET"] },
    RouteEntry { path: "/api/metrics/replay", methods: &["GET"] },
    RouteEntry { path: "/api/reclaim/scan", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/reclaim/scan/{id}", methods: &["GET"] },
    RouteEntry { path: "/api/reclaim/scan/{id}/cancel", methods: &["POST"] },
    RouteEntry { path: "/api/reclaim/tree/{id}", methods: &["GET"] },
    RouteEntry { path: "/api/reclaim/quarantine", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/reclaim/quarantine/{id}", methods: &["DELETE"] },
    RouteEntry { path: "/api/reclaim/quarantine/{id}/restore", methods: &["POST"] },
    RouteEntry { path: "/api/reclaim/snapshots", methods: &["GET"] },
    RouteEntry { path: "/api/reclaim/skipped", methods: &["GET", "DELETE"] },
    RouteEntry { path: "/api/usage", methods: &["GET"] },
    RouteEntry { path: "/api/usage/attribution", methods: &["GET"] },
    RouteEntry { path: "/api/alert/config", methods: &["GET", "PATCH"] },
    RouteEntry { path: "/api/alert/owner", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/stats/daily", methods: &["GET"] },
    // -- branding
    RouteEntry { path: "/api/branding", methods: &["GET", "POST", "DELETE"] },
    RouteEntry { path: "/api/branding/asset/{fname}", methods: &["GET"] },
    // -- email / calendar events
    RouteEntry { path: "/api/email/send", methods: &["POST"] },
    RouteEntry { path: "/api/email/reply", methods: &["POST"] },
    RouteEntry { path: "/api/email/inbox", methods: &["GET"] },
    RouteEntry { path: "/api/email/search", methods: &["GET"] },
    RouteEntry { path: "/api/email/log", methods: &["GET"] },
    RouteEntry { path: "/api/email/approve/{id}", methods: &["POST"] },
    RouteEntry { path: "/api/email/approvals", methods: &["GET"] },
    RouteEntry { path: "/api/cal-events", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/cal-events/{id}", methods: &["PATCH", "DELETE"] },
    // -- sessions (legacy list + native per-name verbs) / identity / scope
    RouteEntry { path: "/api/sessions", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/sessions-git", methods: &["GET"] },
    // The git hooks' endpoint. Listed here because "is it routed?" was the
    // question nobody could answer for the whole cutover: the hook's
    // `except: return 0` hid the 405, so the only visible symptom was silence.
    RouteEntry { path: "/api/git/staged-guard", methods: &["POST"] },
    RouteEntry { path: "/api/git/observed-edits", methods: &["POST"] },
    RouteEntry { path: "/api/git/guard-outcome", methods: &["POST"] },
    RouteEntry { path: "/api/debug/guard-outcomes", methods: &["GET"] },
    RouteEntry { path: "/api/sessions/{name}", methods: ANY },
    RouteEntry { path: "/api/sessions/{name}/{*verb}", methods: ANY },
    RouteEntry { path: "/api/identity", methods: &["GET"] },
    RouteEntry { path: "/api/offline-origin", methods: &["GET"] },
    RouteEntry { path: "/api/scope", methods: ANY },
    // -- browser
    RouteEntry { path: "/api/browser/start", methods: &["POST"] },
    RouteEntry { path: "/api/browser/status", methods: &["GET"] },
    RouteEntry { path: "/api/browser/stop", methods: &["POST"] },
    RouteEntry { path: "/api/browser/profiles", methods: &["GET"] },
    RouteEntry { path: "/api/browser/profile/create", methods: &["POST"] },
    RouteEntry { path: "/api/browser/profile/{name}", methods: &["DELETE"] },
    RouteEntry { path: "/api/browser/navigate", methods: &["POST"] },
    RouteEntry { path: "/api/browser/screenshot", methods: &["GET"] },
    RouteEntry { path: "/api/browser/screenshot/file", methods: &["GET"] },
    RouteEntry { path: "/api/browser/state", methods: &["GET"] },
    RouteEntry { path: "/api/browser/action", methods: &["POST"] },
    RouteEntry { path: "/api/browser/inspect", methods: &["GET"] },
    RouteEntry { path: "/api/browser/inspect/clear", methods: &["POST"] },
    RouteEntry { path: "/api/browser/search", methods: &["GET"] },
    RouteEntry { path: "/api/browser/sessions", methods: &["GET"] },
    RouteEntry { path: "/api/browser/pw-profiles", methods: &["GET"] },
    RouteEntry { path: "/api/browser/save-profile", methods: &["POST"] },
    RouteEntry { path: "/api/browser/agent", methods: &["POST"] },
    // -- file viewer / files / fs
    RouteEntry { path: "/api/file", methods: ANY },
    RouteEntry { path: "/api/file/raw", methods: ANY },
    RouteEntry { path: "/api/file/xlsx", methods: ANY },
    RouteEntry { path: "/api/file/vtt", methods: ANY },
    RouteEntry { path: "/api/file/prepare", methods: ANY },
    RouteEntry { path: "/api/file/transcode", methods: ANY },
    RouteEntry { path: "/api/library", methods: ANY },
    RouteEntry { path: "/api/files", methods: &["GET"] },
    RouteEntry { path: "/api/files/download", methods: &["GET"] },
    RouteEntry { path: "/api/files/upload", methods: &["POST"] },
    RouteEntry { path: "/api/files/mdai", methods: &["GET"] },
    RouteEntry { path: "/api/files/mdai/run", methods: &["POST"] },
    RouteEntry { path: "/api/files/mdai/history", methods: &["GET"] },
    RouteEntry { path: "/api/files/mdai/connect", methods: &["POST"] },
    RouteEntry { path: "/api/fs/mkdir", methods: ANY },
    RouteEntry { path: "/api/fs/open", methods: ANY },
    RouteEntry { path: "/api/fs/upload", methods: ANY },
    RouteEntry { path: "/api/fs/rename", methods: ANY },
    RouteEntry { path: "/api/fs/read", methods: ANY },
    RouteEntry { path: "/api/fs/search", methods: ANY },
    RouteEntry { path: "/api/fs/list", methods: ANY },
    RouteEntry { path: "/api/fs/delete", methods: ANY },
    RouteEntry { path: "/api/fs/resolve", methods: ANY },
    RouteEntry { path: "/api/ls", methods: ANY },
    RouteEntry { path: "/api/autocomplete/dir", methods: ANY },
    // -- uploads
    RouteEntry { path: "/api/upload/start", methods: &["POST"] },
    RouteEntry { path: "/api/upload/{id}/chunk/{n}", methods: &["PUT"] },
    RouteEntry { path: "/api/upload/{id}/finish", methods: &["POST"] },
    RouteEntry { path: "/api/uploads/{filename}", methods: &["GET"] },
    // -- groups / tags
    RouteEntry { path: "/api/groups", methods: ANY },
    RouteEntry { path: "/api/groups/{*rest}", methods: ANY },
    RouteEntry { path: "/api/tags", methods: ANY },
    // -- journal / layout presets
    RouteEntry { path: "/api/journal", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/journal/tags", methods: &["GET"] },
    RouteEntry { path: "/api/journal/config", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/journal/import", methods: &["POST"] },
    RouteEntry { path: "/api/journal/media/{id}", methods: &["GET", "DELETE"] },
    RouteEntry { path: "/api/journal/{id}", methods: &["GET", "PATCH", "DELETE"] },
    RouteEntry { path: "/api/journal/{id}/media", methods: &["POST"] },
    RouteEntry { path: "/api/layout-presets", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/layout-presets/{name}", methods: &["DELETE"] },
    // -- New Worker / Connect modals (api/worker_create.rs, AMUX-2871)
    RouteEntry { path: "/api/templates", methods: &["GET"] },
    RouteEntry { path: "/api/git-check", methods: &["GET"] },
    RouteEntry { path: "/api/git-branches", methods: &["GET"] },
    RouteEntry { path: "/api/suggest-branch", methods: &["POST"] },
    RouteEntry { path: "/api/tmux-sessions", methods: &["GET"] },
    RouteEntry { path: "/api/iterm2/sessions", methods: &["GET"] },
    // -- saved messages / habits / token-baseline reset (AMUX-2871)
    RouteEntry { path: "/api/saved-messages", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/saved-messages/{id}", methods: &["DELETE", "PATCH"] },
    RouteEntry { path: "/api/habits", methods: &["GET", "PUT"] },
    // CRM (AMUX-2929). Mounted via .nest("/api/crm", crm::routes()), which the
    // completeness test could not see until AMUX-2917 taught it to follow
    // nests — so these answered 200 while the census called them unrouted.
    // Nested-router capabilities that were never tabled (AMUX-2937). All eight
    // answer JSON from their handlers — probed live, not the SPA catch-all's
    // HTML — so the census was calling real routes unrouted. Found once the
    // completeness test learned to follow .nest() (AMUX-2917); it previously
    // scanned only api/mod.rs's own .route() calls.
    RouteEntry { path: "/api/board/contract", methods: &["GET"] },
    RouteEntry { path: "/api/schedules/{id}/skip", methods: &["POST"] },
    RouteEntry { path: "/api/search", methods: &["GET"] },
    RouteEntry { path: "/api/search/status", methods: &["GET"] },
    RouteEntry { path: "/api/search/reindex", methods: &["POST"] },
    RouteEntry { path: "/api/why", methods: &["GET"] },
    RouteEntry { path: "/api/why/contract", methods: &["GET"] },
    RouteEntry { path: "/api/why/{kind}/{id}", methods: &["GET"] },
    RouteEntry { path: "/api/crm/contacts", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/crm/contacts/{id}", methods: &["GET", "PATCH", "DELETE"] },
    RouteEntry { path: "/api/crm/contacts/{id}/interactions", methods: &["POST"] },
    RouteEntry { path: "/api/crm/interactions/{id}", methods: &["PATCH", "DELETE"] },
    RouteEntry { path: "/api/crm/followups", methods: &["GET"] },
    // Speedtest (AMUX-2890): the Metrics tab's Run-speed-test button, unrouted
    // since the python retirement — clicks errored against the SPA catch-all.
    RouteEntry { path: "/api/speedtest/download", methods: &["GET"] },
    RouteEntry { path: "/api/speedtest/upload", methods: &["POST"] },
    RouteEntry { path: "/api/stats/reset", methods: &["POST"] },
    RouteEntry { path: "/api/observability", methods: &["GET"] },
    // Connectors (integrations): registry + status, credential paste, OAuth
    // begin/callback, live Test, and the DWD token mint (AMUX-3362). `list` is
    // GET; credentials/auth/test/token are POST; callback is the GET landing.
    RouteEntry { path: "/api/connectors", methods: &["GET"] },
    RouteEntry { path: "/api/connectors/accounts", methods: &["GET"] },
    RouteEntry { path: "/api/connectors/{id}/credentials", methods: &["POST"] },
    RouteEntry { path: "/api/connectors/{id}/auth", methods: &["POST"] },
    RouteEntry { path: "/api/connectors/{id}/test", methods: &["POST"] },
    RouteEntry { path: "/api/connectors/{id}/token", methods: &["POST"] },
    RouteEntry { path: "/api/connectors/{family}/callback", methods: &["GET"] },
    RouteEntry { path: "/api/pull", methods: &["POST"] },
    RouteEntry { path: "/api/proxies", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/proxies/{id}", methods: &["PATCH", "DELETE"] },
    RouteEntry { path: "/api/proxies/{id}/start", methods: &["POST"] },
    RouteEntry { path: "/api/proxies/{id}/stop", methods: &["POST"] },
    // The D1-exit pair. Reached by the bash CLI's own curl, which the caller
    // census does not enumerate — so these 405'd for the whole cutover while
    // every layer that mentions them kept routing sessions at them.
    RouteEntry { path: "/api/board/{id}/status-request", methods: &["POST"] },
    RouteEntry { path: "/api/board/{id}/status-update", methods: &["POST"] },
    // AMUX-3131: `amux board claim <id>` POSTs here; it was unmounted (405) and
    // the CLI exited 0 with the card untouched. Now routed to claim_card.
    RouteEntry { path: "/api/board/{id}/claim", methods: &["POST"] },
    // -- skills / slash-commands / map / history
    RouteEntry { path: "/api/mcp", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/mcp/{name}", methods: &["DELETE"] },
    // GET-only ollama model listing (workers::ollama_models) — mounted in
    // api/mod.rs on the AMUX-3145 ollama work but never tabled, so the route
    // census reported it unrouted while it answered fine (AMUX-2871 class).
    RouteEntry { path: "/api/ollama/models", methods: &["GET"] },
    // Mounted-but-untabled, all found by curling the census's "missing" list
    // against the live server (AMUX-2871). Each was reported as unrouted while
    // answering, because the census reads this table.
    RouteEntry { path: "/api/client-debug", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/memory/global", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/review/week", methods: &["GET"] },
    RouteEntry { path: "/api/review/digest", methods: &["GET"] },
    RouteEntry { path: "/api/channels", methods: &["GET"] },
    RouteEntry { path: "/api/channels/{a}/{b}/messages", methods: &["GET", "POST", "DELETE"] },
    RouteEntry { path: "/api/log-search", methods: &["GET"] },
    RouteEntry { path: "/api/sql", methods: &["POST"] },
    RouteEntry { path: "/api/sql/schema", methods: &["GET"] },
    RouteEntry { path: "/api/sql/rows", methods: &["GET"] },
    RouteEntry { path: "/api/skills", methods: &["GET"] },
    RouteEntry { path: "/api/skills/{name}", methods: &["GET", "POST", "DELETE"] },
    RouteEntry { path: "/api/slash-commands", methods: &["GET"] },
    RouteEntry { path: "/api/slash-commands/{name}", methods: &["GET"] },
    RouteEntry { path: "/api/map", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/map/pins", methods: &["POST"] },
    RouteEntry { path: "/api/map/search", methods: &["GET"] },
    RouteEntry { path: "/api/graph/fleet", methods: &["GET"] },
    RouteEntry { path: "/api/graph/{id}", methods: &["GET"] },
    RouteEntry { path: "/api/graph/{id}/import-vault", methods: &["POST"] },
    RouteEntry { path: "/api/graph/{id}/nodes/{nid}", methods: &["PATCH"] },
    RouteEntry { path: "/api/terminal/create", methods: &["POST"] },
    RouteEntry { path: "/api/terminal/{id}/input", methods: &["POST"] },
    RouteEntry { path: "/api/terminal/{id}/resize", methods: &["POST"] },
    RouteEntry { path: "/api/terminal/{id}/output", methods: &["GET"] },
    RouteEntry { path: "/api/terminal/{id}", methods: &["DELETE"] },
    RouteEntry { path: "/api/reports/types", methods: &["GET"] },
    RouteEntry { path: "/api/reports", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/reports/{id}", methods: &["DELETE", "PATCH"] },
    RouteEntry { path: "/api/reports/{id}/refresh", methods: &["POST"] },
    RouteEntry { path: "/api/reports/{id}/data", methods: &["GET"] },
    RouteEntry { path: "/api/env/apply", methods: &["POST"] },
    RouteEntry { path: "/api/env/schema", methods: &["GET"] },
    RouteEntry { path: "/api/history", methods: &["GET", "POST", "DELETE"] },
    RouteEntry { path: "/api/history/import", methods: &["POST"] },
    // Nested sub-router routes that were missing from the table (AMUX-3083): they
    // answer for real (POST /api/orchestrate/plan -> 400 transcript-required, GET
    // /api/history/{id} -> the row) while /api/debug/routes and the
    // route.callers_have_routes census read the TABLE and reported them unrouted.
    // Caught by tests/route_table.rs's completeness scan (both were named).
    RouteEntry { path: "/api/history/{id}", methods: &["GET"] },
    RouteEntry { path: "/api/orchestrate/plan", methods: &["POST"] },
    // -- logs (this module)
    RouteEntry { path: "/api/logs", methods: &["GET"] },
    // Ported in d177625. Missing from this table meant /api/debug/routes
    // reported it NOT MOUNTED while the handler was answering — the instrument
    // CLAUDE.md tells people to consult instead of grepping, lying about the
    // very route that was just added.
    RouteEntry { path: "/api/lookup", methods: &["POST"] },
    RouteEntry { path: "/api/skin", methods: &["GET"] },
    RouteEntry { path: "/api/config/export", methods: &["GET"] },
    RouteEntry { path: "/api/config/apply", methods: &["PUT"] },
    RouteEntry { path: "/api/board/themes", methods: &["GET"] },
    RouteEntry { path: "/api/board/commit-mentions", methods: &["GET"] },
    RouteEntry { path: "/api/logs/raw", methods: &["GET"] },
    RouteEntry { path: "/api/logs/analyze", methods: &["GET"] },
    RouteEntry { path: "/api/logs/stats", methods: &["GET"] },
    // -- settings / push / dictation
    RouteEntry { path: "/api/settings/default-model", methods: &["GET", "PATCH"] },
    RouteEntry { path: "/api/settings/commit-guard", methods: &["GET", "PATCH"] },
    RouteEntry { path: "/api/settings/task-guard", methods: &["GET", "PATCH"] },
    RouteEntry { path: "/api/settings/env", methods: &["GET", "PATCH"] },
    RouteEntry { path: "/api/push/public-key", methods: &["GET"] },
    RouteEntry { path: "/api/push/subscribe", methods: &["POST"] },
    RouteEntry { path: "/api/push/unsubscribe", methods: &["POST"] },
    RouteEntry { path: "/api/push/test", methods: &["POST"] },
    RouteEntry { path: "/api/push/subscriptions", methods: &["GET"] },
    RouteEntry { path: "/api/dictation/history", methods: &["GET"] },
    RouteEntry { path: "/api/dictation/history/{id}", methods: &["DELETE"] },
    RouteEntry { path: "/api/dictation/history/{id}/edit", methods: &["POST"] },
    RouteEntry { path: "/api/dictation/dict", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/dictation/dict/{id}", methods: &["PATCH", "DELETE"] },
    RouteEntry { path: "/api/dictation/config", methods: ANY },
    RouteEntry { path: "/api/dictate", methods: &["POST"] },
    RouteEntry { path: "/api/tts", methods: &["POST"] },
    RouteEntry { path: "/api/tts/voices", methods: &["GET"] },
    // -- torrents / org / gmail
    RouteEntry { path: "/api/torrents", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/torrents/config", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/torrents/{gid}", methods: &["DELETE"] },
    RouteEntry { path: "/api/torrents/{gid}/file", methods: &["GET"] },
    RouteEntry { path: "/api/torrents/{gid}/{action}", methods: &["POST"] },
    RouteEntry { path: "/api/org", methods: &["GET", "PATCH"] },
    RouteEntry { path: "/api/org/members", methods: &["GET"] },
    RouteEntry { path: "/api/org/members/{id}", methods: &["DELETE"] },
    RouteEntry { path: "/api/org/invites", methods: &["GET", "POST"] },
    RouteEntry { path: "/api/org/invites/{token}", methods: &["DELETE"] },
    RouteEntry { path: "/api/gmail/accounts", methods: &["GET"] },
    RouteEntry { path: "/api/gmail/auth", methods: &["GET"] },
    RouteEntry { path: "/api/gmail/account", methods: &["DELETE"] },
    RouteEntry { path: "/api/gmail/connect", methods: &["POST"] },
    // Mailbox half (api/gmail.rs, AMUX-2883).
    RouteEntry { path: "/api/gmail/labels", methods: &["GET"] },
    RouteEntry { path: "/api/gmail/inbox", methods: &["GET"] },
    RouteEntry { path: "/api/gmail/thread/{id}", methods: &["GET"] },
    RouteEntry { path: "/api/gmail/send", methods: &["POST"] },
    // Merged-router routes the census scanner could not see until it learned
    // to follow `.merge()` (AMUX-2883's table pass): four runtime-jobs debug
    // surfaces and the workers-spelling of the session verb dispatcher.
    RouteEntry { path: "/api/debug/steering", methods: &["GET"] },
    RouteEntry { path: "/api/debug/board-drive", methods: &["GET"] },
    RouteEntry { path: "/api/debug/autofix", methods: &["GET"] },
    RouteEntry { path: "/api/debug/storage", methods: &["GET"] },
    RouteEntry { path: "/api/workers/{name}/{*verb}", methods: &["*"] },
];

/// Match `path` against an axum-style pattern, returning a specificity score
/// (higher = more specific) or None. Scoring mirrors matchit's precedence —
/// literal (3) > `{param}` (1) > `{*wildcard}` (0) per segment — so the best
/// match here is the route axum would actually dispatch to.
fn pattern_score(pattern: &str, path: &str) -> Option<u32> {
    let pat: Vec<&str> = pattern.split('/').collect();
    let segs: Vec<&str> = path.split('/').collect();
    let mut score = 0u32;
    let mut i = 0;
    for (pi, p) in pat.iter().enumerate() {
        if p.starts_with("{*") {
            // Tail wildcard: consumes the (non-empty) remainder.
            return if segs.len() > i && pi == pat.len() - 1 { Some(score) } else { None };
        }
        let s = segs.get(i)?;
        if p.starts_with('{') {
            score += 1;
        } else if p == s {
            score += 3;
        } else {
            return None;
        }
        i += 1;
    }
    if i == segs.len() { Some(score) } else { None }
}

/// The table entry axum would dispatch `path` to (most specific match).
fn best_route(path: &str) -> Option<&'static RouteEntry> {
    ROUTE_TABLE
        .iter()
        .filter_map(|e| pattern_score(e.path, path).map(|s| (s, e)))
        .max_by_key(|(s, _)| *s)
        .map(|(_, e)| e)
}

/// Normalized grouping target: the ROUTE_TABLE pattern the path dispatches
/// to (`/api/board/AMUX-123` -> `/api/board/{id}`), so a thousand ids fold
/// into one group. Unrouted paths fall back to a conservative collapse: any
/// id-shaped segment after `/api/<family>` becomes `{id}` (digits, percent
/// escapes, or 24+ chars), literal words stay literal — `/api/stripe/status`
/// keeps its shape, `/api/foo/AMUX-9` folds.
pub fn normalize_target(path: &str) -> String {
    if let Some(e) = best_route(path) {
        return e.path.to_string();
    }
    let id_ish = |seg: &str| {
        seg.contains('%')
            || seg.len() >= 24
            || seg.chars().any(|c| c.is_ascii_digit())
    };
    path.split('/')
        .enumerate()
        .map(|(i, seg)| if i >= 3 && !seg.is_empty() && id_ish(seg) { "{id}" } else { seg })
        .collect::<Vec<_>>()
        .join("/")
}

/// The literal values sitting where the matched route declares a `{param}`.
///
/// AMUX-3573, and the specimen is this repo's own: the SPA asked
/// `/api/board/gate?item=...` for two years' worth of gate dialogs. That path IS
/// routed — `/api/board/{id}` matches it — so `route.callers_have_routes` passed
/// the whole time while every request 404'd inside the handler looking for a
/// card named "gate", and the client silently fell back to a resolver its own
/// comment calls wrong. A static segment colliding with a param route is
/// invisible to any check that asks "does a route match this".
///
/// The discriminator is the literal itself, and it is not a heuristic. Measured
/// over the live log: 404s under `/api/board/` carried `session-gates` 2281
/// times, `gate` 25, `commit-mentions` 12, `--help` 11 — against real card ids
/// at 4 and 5. Records that are genuinely missing are missing one at a time;
/// a route collision hammers one constant string.
///
/// All four of those top literals were real collisions, and three had ALREADY
/// been fixed by mounting the route (`session-gates`, `commit-mentions`) or
/// guarding the caller (`--help`); their newest hits are 14, 14 and 9 days old,
/// against `gate` at 12 minutes. So the signal is 4 for 4 on the live log, and
/// the three stale ones are the useful control: this ranks by a literal's
/// share of its group, not by recency, so a fixed collision keeps showing until
/// it ages out of the window. Read `last` before filing anything from it.
fn param_literals_of(path: &str) -> Vec<String> {
    let Some(e) = best_route(path) else { return Vec::new() };
    let want = path.split('?').next().unwrap_or(path);
    e.path
        .split('/')
        .zip(want.split('/'))
        .filter(|(decl, _)| decl.starts_with('{') || decl.starts_with(':'))
        // A wildcard tail (`{*verb}`) can swallow several segments and the zip
        // only lines up the first, so it is skipped rather than reported half
        // right — a partial literal would read as a collision that is not one.
        .filter(|(decl, lit)| !decl.contains('*') && !lit.is_empty())
        .map(|(_, lit)| lit.to_string())
        .collect()
}

/// The methods actually mounted where `path` dispatches (`["*"]` = any), or
/// empty when no route claims the path at all.
pub(crate) fn routed_methods_at(path: &str) -> Vec<&'static str> {
    best_route(path).map(|e| e.methods.to_vec()).unwrap_or_default()
}

/// Up to `n` sibling routes by shared prefix — the "did you mean" list for
/// 404s. Only prefixes extending past "/api/" count as kinship.
pub(crate) fn nearest_routes(path: &str, n: usize) -> Vec<&'static str> {
    let common = |a: &str, b: &str| a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    let mut scored: Vec<(usize, &'static str)> = ROUTE_TABLE
        .iter()
        .map(|e| (common(e.path, path), e.path))
        .filter(|(c, _)| *c > "/api/".len())
        .collect();
    // Longest shared prefix first; shorter (more general) pattern breaks ties.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())));
    scored.into_iter().take(n).map(|(_, p)| p).collect()
}

// ---------------------------------------------------------------------------
// GET /api/logs/analyze + /api/logs/stats + /api/debug/routes (AMUX-2610)
// ---------------------------------------------------------------------------

/// Bound on rows a single analyze/stats call will scan — 14 days of retained
/// traffic fits comfortably; the cap only exists so no request is unbounded.
///
/// A cap's DIRECTION is part of its correctness, not an implementation detail
/// (AF-131). Both consumers must scan `ORDER BY ts DESC` so the capped slice
/// is the TRAILING window: with no ORDER BY (stats) and an explicit ASC
/// (analyze), truncation kept the oldest rows — a "trailing norm" that was
/// actually days 8-5 fabricated a 6.46x p95 finding (honest: 1.07x), and a
/// what-is-failing instrument would have dropped the newest errors, the
/// actionable end. Anything that keys on iteration position (first/last_ts,
/// sample choice) must be order-independent, or flipping the scan direction
/// creates the inverse defect — see the min/max and sample_ts rules in the
/// group assembly.
const ANALYZE_SCAN_CAP: i64 = 200_000;
/// Groups returned by /analyze (sorted by count desc before the cut).
const ANALYZE_GROUP_CAP: usize = 200;
/// slow_outliers cap in /stats.
const OUTLIER_CAP: usize = 20;

fn since_h_of(q: &HashMap<String, String>) -> f64 {
    q.get("since_h")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(24.0)
        .clamp(0.01, 24.0 * 365.0)
}

pub(crate) fn local_when(ts: f64) -> String {
    chrono::Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| format!("{ts:.0}"))
}

/// Client identity for distinct-counting: the attributed session when the
/// caller sent X-Amux-Session, else the socket IP. On an all-localhost box
/// the IP collapses to one value, so the session header is the discriminator
/// worth having (and its absence is itself a finding — see AMUX-1812).
pub(crate) fn client_identity(session: &str, ip: &str) -> String {
    if !session.is_empty() {
        format!("session:{session}")
    } else if !ip.is_empty() {
        format!("ip:{ip}")
    } else {
        "unknown".into()
    }
}

struct ErrGroup {
    count: u64,
    first_ts: f64,
    last_ts: f64,
    /// ts of the row `sample` came from — sample choice must not depend on
    /// scan order (AF-131 flipped the scan to DESC; the old overwrite rule
    /// silently keyed "newest" to iteration position).
    sample_ts: f64,
    clients: std::collections::BTreeSet<String>,
    sample: Value,
    sample_has_body: bool,
    sample_path: String,
    method: String,
    status: i64,
    family: String,
    /// AMUX-3573. How often each LITERAL value appeared where `normalize_target`
    /// wrote a `{param}`. A 404 on `/api/board/{id}` is two completely different
    /// bugs depending on this: many DISTINCT ids is someone asking for records
    /// that are gone, one REPEATED literal is a static path colliding with the
    /// param route and never reaching a handler. Normalizing is what makes this
    /// log readable and it is also what fused those two, so the discriminator
    /// has to be kept alongside the group rather than recovered from it.
    param_literals: std::collections::HashMap<String, u64>,
}

/// GET /api/logs/analyze?since_h=24 — the diagnosis endpoint. Groups every
/// error row (status >= 400) in the window by (status, method, family,
/// normalized target) and, for 404/405 groups, annotates each with the
/// ROUTE_TABLE's answer to the question a debugging model used to burn
/// tokens deriving: which methods ARE mounted there (`routed_methods`), and
/// for 404s which routes are nearby (`nearest_routes`). `verdicts` then
/// states the 405 conclusion outright, in one computed sentence per group —
/// including the two non-obvious cells: a 405 at a path with NO route is the
/// GET-only SPA catch-all answering a non-GET (an unknown path wearing a
/// 405), and a 405 whose method IS routed in the current build means the
/// rows predate the route (the build changed since — re-run before filing).
async fn analyze(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let since_h = since_h_of(&q);
    let cutoff = unix_now() - since_h * 3600.0;
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let mut groups: std::collections::BTreeMap<(i64, String, String, String), ErrGroup> =
        Default::default();
    let mut scanned = 0i64;
    let mut oldest_scanned: Option<f64> = None;
    let res = (|| -> rusqlite::Result<()> {
        let mut stmt = conn.prepare(
            "SELECT ts, method, path, family, status, latency_ms, client_ip, user_agent, \
                    amux_session, worker, answered_by, error_body, req_meta \
             FROM _amux_request_log WHERE status >= 400 AND ts >= ?1 \
             ORDER BY ts DESC LIMIT ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![cutoff, ANALYZE_SCAN_CAP])?;
        while let Some(r) = rows.next()? {
            scanned += 1;
            let ts: f64 = r.get(0)?;
            oldest_scanned = Some(oldest_scanned.map_or(ts, |p: f64| p.min(ts)));
            let method: String = r.get(1)?;
            let path: String = r.get(2)?;
            let family: String = r.get(3)?;
            let status: i64 = r.get(4)?;
            let latency_ms: f64 = r.get(5)?;
            let client_ip: Option<String> = r.get(6)?;
            let user_agent: Option<String> = r.get(7)?;
            let amux_session: Option<String> = r.get(8)?;
            let worker: Option<String> = r.get(9)?;
            let answered_by: String = r.get(10)?;
            let error_body: Option<String> = r.get(11)?;
            let req_meta: Option<String> = r.get(12)?;
            let target = normalize_target(&path);
            let ident = client_identity(
                amux_session.as_deref().unwrap_or(""),
                client_ip.as_deref().unwrap_or(""),
            );
            let has_body = error_body.as_deref().is_some_and(|b| !b.is_empty());
            let sample = json!({
                "ts": ts, "when": local_when(ts), "method": method, "path": path,
                "status": status, "latency_ms": latency_ms,
                "client_ip": client_ip, "user_agent": user_agent,
                "amux_session": amux_session, "worker": worker,
                "answered_by": answered_by, "error_body": error_body,
                "req_meta": req_meta,
            });
            let key = (status, method.clone(), family.clone(), target.clone());
            let g = groups.entry(key).or_insert_with(|| ErrGroup {
                count: 0,
                first_ts: ts,
                last_ts: ts,
                sample_ts: ts,
                clients: Default::default(),
                sample: sample.clone(),
                sample_has_body: has_body,
                sample_path: path.clone(),
                method,
                status,
                family,
                param_literals: std::collections::HashMap::new(),
            });
            g.count += 1;
            // min/max, never positional: the scan is newest-first now
            // (AF-131), and `last_ts = ts` under DESC would report the
            // group's OLDEST hit as its most recent.
            g.first_ts = g.first_ts.min(ts);
            g.last_ts = g.last_ts.max(ts);
            if g.param_literals.len() < 200 {
                for lit in param_literals_of(&path) {
                    *g.param_literals.entry(lit).or_insert(0) += 1;
                }
            }
            if g.clients.len() < 1000 {
                g.clients.insert(ident);
            }
            // Sample = the newest row that carries an error_body (a body
            // beats a newer bodyless row; among equally-bodied rows, newest
            // ts wins). Decided on ts, never iteration position (AF-131).
            let better = (has_body && !g.sample_has_body)
                || (has_body == g.sample_has_body && ts > g.sample_ts);
            if better {
                g.sample = sample;
                g.sample_has_body = has_body;
                g.sample_path = path;
                g.sample_ts = ts;
            }
        }
        Ok(())
    })();
    if let Err(e) = res {
        return internal(e);
    }

    let mut sorted: Vec<ErrGroup> = groups.into_values().collect();
    sorted.sort_by(|a, b| b.count.cmp(&a.count).then(
        b.last_ts.partial_cmp(&a.last_ts).unwrap_or(std::cmp::Ordering::Equal),
    ));
    let groups_total = sorted.len();
    sorted.truncate(ANALYZE_GROUP_CAP);

    let mut verdicts: Vec<String> = Vec::new();
    let out: Vec<Value> = sorted
        .iter()
        .map(|g| {
            let target = normalize_target(&g.sample_path);
            let mut v = json!({
                "status": g.status, "method": g.method, "family": g.family,
                "target": target,
                "count": g.count,
                "first_ts": g.first_ts, "first": local_when(g.first_ts),
                "last_ts": g.last_ts, "last": local_when(g.last_ts),
                "distinct_clients": g.clients.len(),
                "clients": g.clients.iter().take(5).collect::<Vec<_>>(),
                "sample": g.sample,
            });
            if g.status == 404 || g.status == 405 {
                let routed = routed_methods_at(&g.sample_path);
                v["routed_methods"] = json!(routed);
                if g.status == 404 {
                    v["nearest_routes"] = json!(nearest_routes(&g.sample_path, 3));
                    // AMUX-3573: separate "records are missing" from "a static
                    // path is being eaten by the param route". Both render as
                    // the same normalized target, which is what hid this for so
                    // long, so the split is stated rather than left derivable.
                    let mut lits: Vec<(&String, &u64)> = g.param_literals.iter().collect();
                    lits.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
                    if let Some((top, n)) = lits.first().copied() {
                        v["top_param_literals"] = json!(lits
                            .iter()
                            .take(5)
                            .map(|(l, c)| json!({ "value": l, "count": c }))
                            .collect::<Vec<_>>());
                        v["distinct_param_literals"] = json!(g.param_literals.len());
                        // One literal carrying most of a multi-hit group means a
                        // caller is hammering a value that never resolves. The
                        // floor is there because 2-of-2 proves nothing.
                        //
                        // The verdict deliberately does NOT pick between the two
                        // causes. The first draft asserted "a path that is not
                        // mounted", which was true of the specimen that motivated
                        // it (`/api/board/gate`) and WRONG about three of the
                        // four groups it flagged on first run: `ollama-ui-e2e`
                        // 7135x and `amax-gtm` 686x are a dead session and a typo
                        // of `amux-gtm`, where the path shape is right and the
                        // RESOURCE is absent. Both are real bugs worth the alarm
                        // — a 686-times misspelling is exactly what this should
                        // surface — but an instrument that names a cause
                        // confidently gets believed, and naming the wrong one
                        // sends the reader to check the route table for a typo.
                        // State the observation, offer both readings, name the
                        // endpoint that decides.
                        if g.count >= 5 && *n * 2 > g.count && g.param_literals.len() > 1 {
                            v["dominant_param_literal"] = json!(top);
                            verdicts.push(format!(
                                "404 {} {}: {} of {} used the SAME literal '{}' where the route \
                                 declares a parameter — one caller hammering one value that never \
                                 resolves, not records going missing one at a time. Two causes fit \
                                 and the fix differs: '{}' was meant to be its OWN ROUTE and the \
                                 param route is swallowing it, or '{}' is a RESOURCE that does not \
                                 exist (a typo, or a reference to something deleted). \
                                 `/api/debug/routes` settles which.",
                                g.method, target, n, g.count, top, top, top
                            ));
                        }
                    }
                }
                if g.status == 405 {
                    verdicts.push(verdict_405(&g.method, &target, &routed, &g.sample_path));
                }
            }
            v
        })
        .collect();

    Json(json!({
        "since_h": since_h,
        "window_start": cutoff, "window_start_local": local_when(cutoff),
        "generated_at": unix_now(),
        "total_errors": scanned,
        "scan_truncated": scanned >= ANALYZE_SCAN_CAP,
        // AF-131: the REAL covered span. Under truncation this is smaller
        // than since_h, and saying so is the difference between "the last N
        // hours" and a confident answer about a window that was not read.
        "actual_window_h": oldest_scanned.map(|o| ((unix_now() - o) / 3600.0 * 100.0).round() / 100.0),
        "groups": out,
        "groups_total": groups_total,
        "verdicts": verdicts,
        "route_table_size": ROUTE_TABLE.len(),
    }))
    .into_response()
}

/// The computed 405 one-liner — the sentence a model used to have to derive
/// from grep + handler-reading. Three cells, one per honest state:
/// unrouted path (the catch-all trap), wrong method on a real path (the
/// classic), and method-now-routed (the build moved since the rows).
pub(crate) fn verdict_405(method: &str, target: &str, routed: &[&str], raw_path: &str) -> String {
    if routed.is_empty() {
        let near = nearest_routes(raw_path, 3);
        let near = if near.is_empty() { String::from("none") } else { near.join(", ") };
        format!(
            "{method} {target}: no route exists at this path — the 405 is the GET-only \
             SPA catch-all answering a non-GET; treat as an unknown path (404-class). \
             Nearest routes: {near}"
        )
    } else if routed.contains(&"*") || routed.contains(&method) {
        format!(
            "{method} {target}: {method} IS routed here in the CURRENT build (routed: {}) — \
             these 405 rows predate the route or hit a different build; re-run the request \
             before filing anything",
            routed.join(", ")
        )
    } else {
        format!("{method} {target}: not routed; routed there: {}", routed.join(", "))
    }
}

/// Nearest-rank percentile over an ASCENDING-sorted slice: the value at
/// 1-based rank ceil(q*n). Always an actually-observed latency — never an
/// interpolation — so p50/p95 can be grepped back to real rows.
pub(crate) fn percentile_sorted(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

struct FamAcc {
    latencies: Vec<f64>,
    error_count: u64,
    proxy_count: u64,
    origins: std::collections::BTreeMap<String, u64>,
    workers: std::collections::BTreeSet<String>,
    clients: std::collections::BTreeSet<String>,
}

/// GET /api/logs/stats?since_h=24 — per-family traffic/latency/error rollup
/// plus `slow_outliers` (rows > 5x their family's p50, capped 20 overall,
/// ranked by ratio). Percentiles: nearest-rank over the window's sorted
/// per-family latencies (see [`percentile_sorted`]); `percentile_method` in
/// the response names it so the sweep never has to guess. `proxy_count` is
/// strictly `answered_by == "python-proxy"` (the strangler-fig hop that must
/// trend to zero); the per-origin breakdown — the table carries both origins
/// since AF-36 — rides in `origins`.
async fn stats(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let since_h = since_h_of(&q);
    let cutoff = unix_now() - since_h * 3600.0;
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let mut fams: std::collections::BTreeMap<String, FamAcc> = Default::default();
    let mut scanned = 0i64;
    let mut oldest_ts: Option<f64> = None;
    // Restart-spanning rows, excluded from the latency statistics and COUNTED
    // (AF-186). Counted rather than silently dropped for AF-178's reason: a
    // detector that quietly removes rows is one nobody can audit, and a zero
    // here is a measurement where silence would not be.
    let proc_boot = crate::runtime_jobs::heartbeat::boot_at();
    let mut spanned = 0u64;
    let mut spanned_by_family: std::collections::BTreeMap<String, u64> = Default::default();
    let res = (|| -> rusqlite::Result<()> {
        let mut stmt = conn.prepare(
            // AF-131: ORDER BY ts DESC is load-bearing. With no ORDER BY,
            // SQLite returned insertion order and LIMIT kept the OLDEST rows,
            // so a truncated "trailing" window was actually days 8-5 — and
            // actual_window_h (computed from the slice) then claimed the full
            // requested span. Both lies at once: a 6.46x p95 "regression" on
            // 42k requests was 1.07x against an honest window. Newest-first
            // makes the slice the trailing window everyone assumes AND makes
            // actual_window_h truthful by construction.
            "SELECT family, status, latency_ms, answered_by, worker, amux_session, client_ip, ts, \
             boot_at \
             FROM _amux_request_log WHERE ts >= ?1 ORDER BY ts DESC LIMIT ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![cutoff, ANALYZE_SCAN_CAP])?;
        while let Some(r) = rows.next()? {
            scanned += 1;
            let family: String = r.get(0)?;
            let status: i64 = r.get(1)?;
            let latency_ms: f64 = r.get(2)?;
            let answered_by: String = r.get(3)?;
            let worker: Option<String> = r.get(4)?;
            let amux_session: Option<String> = r.get(5)?;
            let client_ip: Option<String> = r.get(6)?;
            let ts: f64 = r.get(7)?;
            let row_boot: Option<f64> = r.get(8)?;
            // A REQUEST WHOSE CLOCK SPANS A RESTART IS NOT A SLOW REQUEST (AF-186).
            //
            // `latency_ms` is wall time from arrival to completion, so a request
            // that arrived before the serving process started measured the
            // OUTAGE. autofix stopped counting these in AF-175; this endpoint —
            // the one a HUMAN opens — still did, and on 2026-08-24 the daily
            // sweep's slow_outliers was six of them at 33-58s, completing 5s
            // apart with latencies falling by exactly 5s. That is one dashboard
            // poll draining across a restart, and /api/board's real p50 in the
            // same window is 0.3ms.
            //
            // The predicate is IMPORTED from autofix rather than re-derived
            // here. Two spellings of one rule is how the outlier path kept the
            // wrong one for a day after the p95 path was fixed.
            if crate::runtime_jobs::autofix::spans_own_restart(ts, latency_ms, row_boot, proc_boot)
            {
                spanned += 1;
                *spanned_by_family.entry(family.clone()).or_insert(0u64) += 1;
                continue;
            }
            oldest_ts = Some(oldest_ts.map_or(ts, |prev: f64| prev.min(ts)));
            let f = fams.entry(family).or_insert_with(|| FamAcc {
                latencies: Vec::new(),
                error_count: 0,
                proxy_count: 0,
                origins: Default::default(),
                workers: Default::default(),
                clients: Default::default(),
            });
            f.latencies.push(latency_ms);
            if status >= 400 {
                f.error_count += 1;
            }
            // The table carries BOTH origins (AF-36, log-sweep.md):
            // `python-proxy` alone is the strangler-fig hop that must trend
            // to zero; `python` rows are the OTHER origin's own traffic, so
            // counting them as "proxied" would fake a cutover regression.
            // The full breakdown rides in `origins`.
            if answered_by == "python-proxy" {
                f.proxy_count += 1;
            }
            *f.origins.entry(answered_by).or_insert(0) += 1;
            if let Some(w) = worker.filter(|w| !w.is_empty()) {
                f.workers.insert(w);
            }
            f.clients.insert(client_identity(
                amux_session.as_deref().unwrap_or(""),
                client_ip.as_deref().unwrap_or(""),
            ));
        }
        Ok(())
    })();
    if let Err(e) = res {
        return internal(e);
    }

    // Percentiles + the outlier pass. Outlier rows are re-read per family
    // (indexed on (family, ts)) so the first pass never has to hold whole
    // rows — only latency vectors — in memory.
    let mut fam_rows: Vec<(String, Value, f64)> = Vec::new(); // (family, json, p50)
    let mut outliers: Vec<(f64, Value)> = Vec::new(); // (ratio, row)
    let mut total_count = 0u64;
    let mut total_errors = 0u64;
    let mut total_proxy = 0u64;
    let mut all_workers: std::collections::BTreeSet<String> = Default::default();
    let mut all_clients: std::collections::BTreeSet<String> = Default::default();
    let mut all_origins: std::collections::BTreeMap<String, u64> = Default::default();
    for (family, mut acc) in fams {
        acc.latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = acc.latencies.len();
        let p50 = percentile_sorted(&acc.latencies, 0.50);
        let p95 = percentile_sorted(&acc.latencies, 0.95);
        let max = acc.latencies.last().copied().unwrap_or(0.0);
        total_count += n as u64;
        total_errors += acc.error_count;
        total_proxy += acc.proxy_count;
        for (origin, c) in &acc.origins {
            *all_origins.entry(origin.clone()).or_insert(0) += c;
        }
        all_workers.extend(acc.workers.iter().cloned());
        all_clients.extend(acc.clients.iter().cloned());
        #[allow(clippy::cast_precision_loss)]
        let error_rate = if n == 0 { 0.0 } else { acc.error_count as f64 / n as f64 };
        fam_rows.push((
            family.clone(),
            json!({
                "family": family,
                "count": n,
                "p50_ms": round2(p50), "p95_ms": round2(p95), "max_ms": round2(max),
                "error_count": acc.error_count,
                "error_rate": round4(error_rate),
                // Zero is the healthy answer and it is PUBLISHED (AF-186/AF-180).
                // Silence would be indistinguishable from "the exclusion is not
                // wired in", which is the defect this endpoint had.
                "restart_spanning_excluded": spanned_by_family.get(&family).copied().unwrap_or(0),
                "proxy_count": acc.proxy_count,
                "origins": acc.origins,
                "distinct_workers": acc.workers.len(),
                "distinct_clients": acc.clients.len(),
            }),
            p50,
        ));
        // Outliers: > 5x family p50. Re-query capped per family, merged and
        // re-capped globally by ratio.
        if p50 > 0.0 {
            let threshold = 5.0 * p50;
            let r = (|| -> rusqlite::Result<()> {
                let mut stmt = conn.prepare_cached(
                    "SELECT ts, method, path, status, latency_ms, worker, boot_at \
                     FROM _amux_request_log \
                     WHERE family = ?1 AND ts >= ?2 AND latency_ms > ?3 \
                     ORDER BY latency_ms DESC LIMIT ?4",
                )?;
                let mut rows =
                    stmt.query(rusqlite::params![family, cutoff, threshold, OUTLIER_CAP as i64])?;
                while let Some(r) = rows.next()? {
                    let ts: f64 = r.get(0)?;
                    let method: String = r.get(1)?;
                    let path: String = r.get(2)?;
                    let status: i64 = r.get(3)?;
                    let latency_ms: f64 = r.get(4)?;
                    let worker: Option<String> = r.get(5)?;
                    let row_boot: Option<f64> = r.get(6)?;
                    // The same exclusion as the first pass (AF-186). Applied
                    // here TOO rather than relying on the pass above, because
                    // this is a separate re-read: on 2026-08-24 the outlier
                    // list was six restart-spanning rows at 33-58s while the
                    // family p50 was 0.3ms, and the p95 path had already been
                    // fixed in autofix while this one had not. Two scans, one
                    // rule — the second scan is exactly where the rule got lost
                    // last time.
                    if crate::runtime_jobs::autofix::spans_own_restart(
                        ts, latency_ms, row_boot, proc_boot,
                    ) {
                        continue;
                    }
                    outliers.push((
                        latency_ms / p50,
                        json!({
                            "ts": ts, "when": local_when(ts), "method": method, "path": path,
                            "status": status, "latency_ms": round2(latency_ms),
                            "family": family, "family_p50_ms": round2(p50),
                            "ratio": round2(latency_ms / p50), "worker": worker,
                        }),
                    ));
                }
                Ok(())
            })();
            if let Err(e) = r {
                return internal(e);
            }
        }
    }
    fam_rows.sort_by(|a, b| {
        let ca = a.1["count"].as_u64().unwrap_or(0);
        let cb = b.1["count"].as_u64().unwrap_or(0);
        cb.cmp(&ca)
    });
    outliers.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    outliers.truncate(OUTLIER_CAP);

    let now = unix_now();
    let actual_window_h = oldest_ts.map(|ots| (now - ots) / 3600.0);
    Json(json!({
        "since_h": since_h,
        "actual_window_h": actual_window_h.map(round2),
        "oldest_row_ts": oldest_ts,
        "oldest_row_local": oldest_ts.map(local_when),
        "window_start": cutoff, "window_start_local": local_when(cutoff),
        "generated_at": now,
        "percentile_method": "nearest-rank: value at 1-based rank ceil(q*n) of the window's \
                              sorted per-family latencies (always an observed latency, \
                              never interpolated)",
        "scan_truncated": scanned >= ANALYZE_SCAN_CAP,
        "families": fam_rows.into_iter().map(|(_, v, _)| v).collect::<Vec<_>>(),
        "totals": {
            "count": total_count,
            "error_count": total_errors,
            "proxy_count": total_proxy,
            "origins": all_origins,
            "distinct_workers": all_workers.len(),
            "distinct_clients": all_clients.len(),
            // How many rows the restart exclusion removed from every number
            // above (AF-186). `count` is post-exclusion, so a reader comparing
            // it to raw traffic needs this to reconcile — and a zero says the
            // exclusion RAN, where its absence would say nothing at all.
            "restart_spanning_excluded": spanned,
        },
        "slow_outliers": outliers.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
    }))
    .into_response()
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

/// GET /api/debug/routes — the ROUTE_TABLE as JSON, so "is X routed, with
/// which methods" is a GET, not a grep over mod.rs + every module. Owner
/// (native|proxied) derives from the boundary registry, the same source
/// /api/debug/boundary serves. Mounted in mod.rs next to its debug siblings;
/// public for the same reason boundary is (route names only, nothing secret).
/// Every family claimed by a NAMED tab. `http` is the complement of this set,
/// so the two definitions cannot disagree about what "everything else" means.
const NAMED_CATEGORY_FAMILIES: &[&str] = &[
    "/api/board", "/api/schedules", "/api/cal-events", "/api/calendar",
    "/api/sessions", "/api/workers", "/api/sessions-git", "/api/channels",
    "/api/memory", "/api/memories", "/api/scope", "/api/notes",
    "/api/fs", "/api/file", "/api/files", "/api/upload", "/api/uploads", "/api/library",
];

/// The families a tab selects. Empty for `http`, which is the complement.
///
/// Derived from [`category_of`] rather than written twice — a second list would
/// drift, and then the tab would show rows whose own `category` field disagreed
/// with the tab they arrived under.
fn families_for_category(cat: &str) -> Vec<&'static str> {
    if cat == "http" {
        return Vec::new();
    }
    NAMED_CATEGORY_FAMILIES
        .iter()
        .copied()
        .filter(|f| category_of(f) == cat)
        .collect()
}

/// Which Logs tab a request belongs to.
///
/// The tabs are a HUMAN grouping ("show me board activity"), not a URL prefix,
/// so this maps families onto them rather than exposing the family list raw —
/// a tab per API family would be 49 tabs.
///
/// Anything unmapped stays "http": an honest catch-all beats inventing a
/// category, and the All tab shows it regardless.
fn category_of(family: &str) -> &'static str {
    match family {
        "/api/board" | "/api/schedules" | "/api/cal-events" | "/api/calendar" => "board",
        "/api/sessions" | "/api/workers" | "/api/sessions-git" | "/api/channels" => "session",
        "/api/memory" | "/api/memories" | "/api/scope" | "/api/notes" => "memory",
        "/api/fs" | "/api/file" | "/api/files" | "/api/upload" | "/api/uploads"
        | "/api/library" => "files",
        _ => "http",
    }
}

pub async fn debug_routes() -> axum::Json<Value> {
    let proxied = |path: &str| {
        super::py_proxy::PROXIED_FAMILIES.iter().any(|f| {
            path == f.family || (path.starts_with(f.family) && path[f.family.len()..].starts_with('/'))
        })
    };
    axum::Json(json!({
        "count": ROUTE_TABLE.len(),
        "routes": ROUTE_TABLE.iter().map(|e| json!({
            "family": family_of(e.path),
            "path": e.path,
            "methods": e.methods,
            "owner": if proxied(e.path) { "proxied" } else { "native" },
        })).collect::<Vec<_>>(),
        "notes": {
            "any": "methods [\"*\"] = the route accepts every method (axum any())",
            "catchall": "paths NOT listed here: GET answers the SPA shell (non-API) or JSON \
                         404 (/api/*); any other method answers 405 from the GET-only \
                         catch-all — a 405 on an unlisted path means UNKNOWN PATH, not \
                         wrong method",
            "excluded": "module-internal deliberate-404 catch-alls and the SPA shell are \
                         not capabilities and not listed",
            "source": "ROUTE_TABLE in crates/amux-server/src/api/request_log.rs — kept \
                       honest both directions by tests/route_table.rs against the real \
                       router composition",
        },
    }))
}

// ---------------------------------------------------------------------------
// Tests — temp DBs only; the middleware is exercised through the SHIPPED
// wiring (layer_with), not a paraphrase of it (ethos rule 7).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// AMUX-3573, from the live specimen rather than a constructed one.
    ///
    /// The SPA called `/api/board/gate?item=...` and `/api/board/{id}` matched
    /// it, so every routing check passed while the handler 404'd looking for a
    /// card named "gate". The fix is to keep the LITERAL that landed in the
    /// param slot, because the normalized target is identical for a collision
    /// and for a record that is genuinely gone.
    ///
    /// The controls carry the weight. A wildcard tail must be skipped (it
    /// swallows several segments and a half-matched literal reads as a
    /// collision that is not one), and a path with no param must yield nothing
    /// at all — otherwise the detector fires on every 404 in the system.
    #[test]
    fn param_literals_keep_the_value_that_a_normalized_target_throws_away() {
        // The specimen: "gate" is what makes this diagnosable at all.
        let lits = super::param_literals_of("/api/board/gate?item=AMUX-1&status=done");
        assert_eq!(lits, vec!["gate".to_string()], "the param literal must survive normalization");

        // ...and it is exactly what normalize_target discards, which is the
        // whole reason this function exists.
        assert_eq!(
            super::normalize_target("/api/board/gate?item=AMUX-1"),
            super::normalize_target("/api/board/AMUX-9999"),
            "precondition: a collision and a missing card normalize IDENTICALLY, \
             so the group alone cannot tell them apart"
        );

        // A real card id lands in the same slot — the function does not judge,
        // it reports, and the count across a group is what discriminates.
        assert_eq!(super::param_literals_of("/api/board/AMUX-9999"), vec!["AMUX-9999".to_string()]);

        // CONTROL: a wildcard tail is skipped rather than half-reported.
        for p in ["/api/sessions/amux/peek", "/api/sessions/amux/send"] {
            let l = super::param_literals_of(p);
            assert!(
                !l.iter().any(|s| s == "peek" || s == "send"),
                "a {{*wildcard}} segment must not be reported as a param literal ({p} -> {l:?})"
            );
        }

        // CONTROL: a fully-static routed path has no literals, so the detector
        // is silent on the majority of traffic instead of flagging all of it.
        assert!(
            super::param_literals_of("/api/board/contract").is_empty(),
            "a static route has no param slot and must yield nothing"
        );
    }

    /// AF-116: ROUTE_TABLE is hand-maintained beside the mounts, so it drifts
    /// exactly the way the MIGRATIONS array did (AF-99: a .sql on disk was
    /// never registered; the fix was a check that fails when the two
    /// disagree). /api/connectors/accounts was mounted and answering 200
    /// while absent here — so /api/debug/routes under-reported ("routing
    /// questions are answered there, never by a grep") and
    /// route.callers_have_routes filed a FALSE failure against a live route,
    /// which trains readers to skim past the 8 real ones next to it.
    ///
    /// WHAT THIS COVERS THAT THE WALK DOES NOT, which is the only reason to
    /// keep it now that `every_directly_routed_api_path_is_in_the_table`
    /// (tests/route_table.rs) follows `.nest()`/`.merge()` and is strictly the
    /// better instrument for anything mounted. That walk starts at
    /// `api/mod.rs` and follows composition, so it can only reach a module the
    /// composition names. This one READS THE FILES — every `.rs` in src/api,
    /// mounted or not — so it still speaks for a module the walk cannot arrive
    /// at, and for a mount shape nobody has taught the walk to follow yet.
    /// Keeping both is deliberate: two instruments on different axes
    /// disagreeing is how the gmail asymmetry (AMUX-2883) was found at all.
    ///
    /// WHAT IT DOES NOT COVER — the name used to claim "every absolute route
    /// literal" and this list is why that was too much:
    ///
    /// - **Only a literal spelled at the call site.** The regex wants
    ///   `.route("` followed by a `"/api/..."` string. A path built from a
    ///   const, a `format!`, or a variable is invisible, and so is any mount
    ///   that is not spelled `.route(`.
    /// - **Only absolute `/api/` paths.** A nested router mounting `"/{id}"`
    ///   is out of scope by the prefix. Deliberate — the absolute literals are
    ///   where this class bit — but it means the walk is the ONLY check on a
    ///   relative literal.
    /// - **Only source→table.** A ROUTE_TABLE row with no mount behind it is
    ///   not this test's business; `route_table_matches_the_real_router_both_directions`
    ///   holds that direction.
    /// - **Only the shipped half of each file** (`#[cfg(test)]` onward is cut,
    ///   because test modules mount fixture paths like `/api/echo`).
    /// - **Only the top level of src/api.** The scan is a flat `read_dir`, so
    ///   if anyone ever adds a SUBDIRECTORY there its routes are skipped in
    ///   silence — `found_any` below cannot see that, since the 81 files
    ///   beside it keep the probe looking alive. There are no subdirectories
    ///   today, which is exactly why this is written down rather than fixed:
    ///   the day one appears, this is the sentence that says so.
    #[test]
    fn absolute_route_literals_in_api_files_are_tabled_even_if_never_mounted() {
        let api_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/api");
        let re = regex::Regex::new(r#"\.route\(\s*"(/api/[^"]+)""#).unwrap();
        let table: std::collections::BTreeSet<&str> =
            super::ROUTE_TABLE.iter().map(|r| r.path).collect();
        let mut missing = Vec::new();
        let mut found_any = false;
        for entry in std::fs::read_dir(&api_dir).expect("src/api readable") {
            let p = entry.expect("dir entry").path();
            if p.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let src = std::fs::read_to_string(&p).expect("source readable");
            // Test-module routers mount fixture paths (/api/echo, /api/thing)
            // that are never in the production table — scan only the shipped
            // half of each file.
            let src = src.split("#[cfg(test)]").next().unwrap_or(&src);
            for cap in re.captures_iter(src) {
                found_any = true;
                let path = cap.get(1).expect("capture").as_str();
                if !table.contains(path) {
                    missing.push(format!(
                        "{}: {}",
                        p.file_name().unwrap_or_default().to_string_lossy(),
                        path
                    ));
                }
            }
        }
        // The empty-grep trap: an extractor that matched nothing is broken,
        // not vindicated (the invariant's own rule, applied to its guard).
        assert!(found_any, "no .route(\"/api/...\") literals matched — the probe is broken");
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "mounted but ABSENT from ROUTE_TABLE — debug/routes will under-report these and \
             route.callers_have_routes will file FALSE failures against them (AF-116):\n{}",
            missing.join("\n")
        );
    }

    use axum::http::StatusCode;   // lib no longer needs it; these tests do
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use std::sync::Arc;
    use tower::ServiceExt;

    /// Live-oracle fixture: `GET https://localhost:8822/api/logs?limit=3`
    /// captured 2026-08-09 against the running Python server (build of that
    /// day). One representative ring event verbatim — the KEY SET is the
    /// contract the SPA maps over (app.js:16524-16529).
    const PYTHON_LOGS_FIXTURE: &str = r#"{
        "events": [{
            "ts": 1786315119.485713,
            "type": "http",
            "action": "get",
            "target": "/api/sessions/mixpeek-autopilot/peek",
            "session": "",
            "detail": "",
            "status": 304,
            "ip": "127.0.0.1",
            "actor": "",
            "req": "",
            "resp": "",
            "method": "GET",
            "ms": 376
        }],
        "count": 1
    }"#;

    /// `GET https://localhost:8822/api/logs/raw?lines=3`, same capture:
    /// {"lines": ["2026-08-09 18:38:39 [127.0.0.1] GET /api/... 304 377ms",
    /// ...], "total": 334708}.
    const PYTHON_RAW_KEYS: &[&str] = &["lines", "total"];

    fn store() -> (Arc<crate::db::Store>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s = crate::db::Store::open(&dir.path().join("t.db")).unwrap();
        (Arc::new(s), dir)
    }

    fn state(store: Arc<crate::db::Store>) -> AppState {
        AppState {
            store,
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        }
    }

    /// A tiny app behind the SHIPPED layer wiring: routes that let each test
    /// provoke exactly one property (proxy stamp, slow handler, big body,
    /// error body).
    fn test_app(logger: RequestLogger) -> Router {
        let inner: Router = Router::new()
            .route(
                "/api/sessions/{name}/peek",
                get(|| async {
                    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                    ([("x-amux-answered-by", "python-proxy")], "ok")
                }),
            )
            .route(
                "/api/echo",
                axum::routing::post(|b: axum::body::Bytes| async move { format!("{}", b.len()) })
                    .layer(axum::extract::DefaultBodyLimit::disable()),
            )
            .route(
                "/api/fail",
                get(|| async {
                    (StatusCode::INTERNAL_SERVER_ERROR, "E".repeat(10_000))
                }),
            )
            .route("/api/board", get(|| async { "[]" }));
        layer_with(inner, logger)
    }

    async fn hit(app: &Router, req: HttpRequest<Body>) -> (StatusCode, Vec<u8>) {
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, body.to_vec())
    }

    /// Poll until the async writer has landed `n` rows (the channel is
    /// deliberately out-of-band; tests wait on the DB, the source of truth).
    async fn wait_rows(store: &crate::db::Store, n: i64) -> i64 {
        for _ in 0..200 {
            let c = store.read().unwrap();
            let got: i64 = c
                .query_row("SELECT COUNT(*) FROM _amux_request_log", [], |r| r.get(0))
                .unwrap();
            if got >= n {
                return got;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let c = store.read().unwrap();
        c.query_row("SELECT COUNT(*) FROM _amux_request_log", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn derivations_worker_and_family() {
        assert_eq!(worker_of("/api/sessions/amux/peek", ""), Some("amux".into()));
        assert_eq!(worker_of("/api/sessions/my%20w/send", ""), Some("my w".into()));
        assert_eq!(worker_of("/api/workers/wrk_01ABC/status", ""), Some("wrk_01ABC".into()));
        assert_eq!(worker_of("/api/workers/wrk_01ABC", ""), Some("wrk_01ABC".into()));
        assert_eq!(worker_of("/api/sessions", ""), None);
        assert_eq!(worker_of("/api/board/AMUX-1", ""), None);
        // /api/sessions/self resolves through its query param, never "self".
        assert_eq!(worker_of("/api/sessions/self", "session=amux"), Some("amux".into()));
        assert_eq!(worker_of("/api/sessions/self", ""), None);

        assert_eq!(family_of("/api/board/AMUX-1"), "/api/board");
        assert_eq!(family_of("/api/sessions/amux/peek"), "/api/sessions");
        assert_eq!(family_of("/api/calendar.ics"), "/api/calendar.ics");
        // Registry miss (python-only path): first two segments.
        assert_eq!(family_of("/api/git/staged-guard"), "/api/git");
    }

    #[tokio::test]
    async fn request_becomes_row_with_attribution_latency_and_answered_by() {
        let (store, _dir) = store();
        let app = test_app(RequestLogger::spawn_with(store.clone(), 14.0, 1_000_000));
        let (st, _) = hit(
            &app,
            HttpRequest::builder()
                .uri("/api/sessions/w1/peek?lines=600")
                .header("x-amux-session", "caller-lane")
                .header("user-agent", "test-agent")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(wait_rows(&store, 1).await, 1);
        let c = store.read().unwrap();
        let (path, family, worker, sess, status, latency, answered, meta): (
            String, String, Option<String>, String, i64, f64, String, Option<String>,
        ) = c
            .query_row(
                "SELECT path, family, worker, amux_session, status, latency_ms, answered_by, req_meta \
                 FROM _amux_request_log",
                [],
                |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))
                },
            )
            .unwrap();
        assert_eq!(path, "/api/sessions/w1/peek");
        assert_eq!(family, "/api/sessions");
        assert_eq!(worker.as_deref(), Some("w1"), "worker attribution from path");
        assert_eq!(sess, "caller-lane");
        assert_eq!(status, 200);
        assert!(latency >= 10.0, "handler sleeps 15ms; measured {latency}ms");
        assert_eq!(answered, "python-proxy", "x-amux-answered-by response header");
        let meta: Value = serde_json::from_str(&meta.unwrap()).unwrap();
        assert_eq!(meta["query"], "lines=600");
    }

    #[tokio::test]
    async fn a_25mb_body_never_lands_in_the_log() {
        let (store, _dir) = store();
        let app = test_app(RequestLogger::spawn_with(store.clone(), 14.0, 1_000_000));
        let big = vec![b'a'; 25 * 1024 * 1024];
        let (st, body) = hit(
            &app,
            HttpRequest::builder()
                .method("POST")
                .uri("/api/echo")
                .header("content-type", "application/octet-stream")
                .header("content-length", big.len().to_string())
                .body(Body::from(big.clone()))
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body, format!("{}", big.len()).into_bytes(), "handler saw the full body");
        assert_eq!(wait_rows(&store, 1).await, 1);
        let c = store.read().unwrap();
        let (req_bytes, err_body, meta_len, row_bytes): (i64, Option<String>, i64, i64) = c
            .query_row(
                "SELECT req_bytes, error_body, LENGTH(COALESCE(req_meta,'')), \
                        LENGTH(COALESCE(path,''))+LENGTH(COALESCE(user_agent,''))+\
                        LENGTH(COALESCE(req_meta,''))+LENGTH(COALESCE(error_body,'')) \
                 FROM _amux_request_log",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(req_bytes, 25 * 1024 * 1024, "SIZE is recorded");
        assert_eq!(err_body, None, "success bodies are never captured");
        assert!(meta_len < 700, "req_meta stays capped: {meta_len}");
        assert!(row_bytes < 2000, "whole row stays small: {row_bytes} bytes");
    }

    #[tokio::test]
    async fn error_body_captured_capped_and_response_undamaged() {
        let (store, _dir) = store();
        let app = test_app(RequestLogger::spawn_with(store.clone(), 14.0, 1_000_000));
        let (st, body) = hit(
            &app,
            HttpRequest::builder().uri("/api/fail").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body.len(), 10_000, "client still receives the FULL error body");
        assert_eq!(wait_rows(&store, 1).await, 1);
        let c = store.read().unwrap();
        let (err_body, resp_bytes): (String, i64) = c
            .query_row("SELECT error_body, resp_bytes FROM _amux_request_log", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        // AF-59 changed this contract deliberately: the KEPT payload is still
        // exactly ERROR_BODY_CHARS, but an over-cap body now carries a marker
        // saying so, because a bare truncation is invalid JSON that reads as a
        // malformed response. Assert the kept prefix, not the total length —
        // asserting the total would make the marker's own text load-bearing.
        assert!(
            err_body.starts_with(&"E".repeat(ERROR_BODY_CHARS)),
            "exactly ERROR_BODY_CHARS of payload are kept, unaltered"
        );
        assert!(
            err_body.contains("<truncated by the request log"),
            "an over-cap body must announce the cut: {}",
            &err_body[err_body.len().saturating_sub(140)..]
        );
        assert_eq!(resp_bytes, 10_000, "exact buffered size recorded");
    }

    /// AF-57. Built from the LIVE specimen, not a convenient one: a real 503
    /// `/api/tts` row whose stored `error_body` began `1f ef bf bd` — gzip magic
    /// already mangled by `from_utf8_lossy` — with 875 U+FFFD in ~3.8KB.
    ///
    /// The pre-fix assertion is the load-bearing one. It is not enough to show
    /// the fix decodes; the point is that the OLD path destroyed the bytes
    /// irreversibly, which is why this could not be diagnosed after the fact and
    /// why no amount of care at read time would have recovered it.
    #[test]
    fn a_gzipped_error_body_is_decoded_not_stored_as_mangled_bytes() {
        use std::io::{Read, Write};
        // SIZED LIKE THE REAL ROW (2942 bytes), and that detail is load-bearing.
        // The first fixture here was the one-line JSON below on its own; deflate
        // emits a STORED block for input that small, so the text survived
        // verbatim inside the "compressed" bytes and the assertion below failed.
        // My own broken fixture was not broken — exactly the trap in ethos rule
        // 7 about verifying the specimen you built yourself. A body that really
        // compresses is what the incident had and what this needs.
        let plain_short = br#"{"error":"CDP Page.captureScreenshot timed out after 30s"}"#;
        let mut plain = Vec::new();
        plain.extend_from_slice(br#"{"error":"CDP Page.captureScreenshot timed out after 30s","#);
        plain.extend_from_slice(br#""detail":["#);
        for i in 0..60 {
            plain.extend_from_slice(
                format!(r#"{{"frame":{i},"note":"chrome cdp target detached, retrying"}},"#)
                    .as_bytes(),
            );
        }
        plain.extend_from_slice(b"null]}");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&plain).unwrap();
        let gz = enc.finish().unwrap();
        assert_eq!(&gz[..2], b"\x1f\x8b", "fixture really is gzip, not a paraphrase of one");
        assert!(gz.len() < plain.len(), "fixture must ACTUALLY compress, not store literally");

        // THE BUG, reproduced: the old code path, verbatim.
        let old = truncate_chars(&String::from_utf8_lossy(&gz), ERROR_BODY_CHARS);
        assert!(
            old.contains('\u{FFFD}'),
            "pre-fix specimen must actually be mangled or this test proves nothing"
        );
        assert!(
            !old.contains("captureScreenshot"),
            "pre-fix specimen must NOT contain the error text — that is the whole defect"
        );
        assert!(
            flate2::read::GzDecoder::new(old.as_bytes()).read_to_end(&mut Vec::new()).is_err(),
            "the lossy conversion must be IRREVERSIBLE — if this could be re-decoded, \
             the incident would have been recoverable and the fix merely cosmetic"
        );

        // THE FIX.
        let got = decoded_error_body(&gz, "gzip");
        assert!(got.contains("captureScreenshot timed out"), "gzip body must decode: {got}");
        assert!(!got.contains('\u{FFFD}'), "no replacement chars survive: {got}");

        // Uncompressed still works — the guard must not have broken the 90% case.
        assert!(decoded_error_body(plain_short, "").contains("captureScreenshot"));
        assert!(decoded_error_body(plain_short, "identity").contains("captureScreenshot"));

        // Honest failure beats plausible noise: an encoding we cannot decode,
        // and a corrupt stream, both SAY so instead of storing bytes that read
        // like content.
        let br = decoded_error_body(&gz, "br");
        assert!(br.contains("br-encoded") && br.contains("cannot decode"), "{br}");
        let bad = decoded_error_body(b"\x1f\x8b\x08garbage-not-a-stream", "gzip");
        assert!(bad.starts_with("<gzip error body could not be decoded"), "{bad}");

        // Case-insensitive: hyper does not promise a canonical casing.
        assert!(decoded_error_body(&gz, "GZIP").contains("captureScreenshot"));

        // AF-59: an over-cap body must SAY it was cut. A bare truncation yields
        // invalid JSON that reads as a malformed response, which is how
        // AMUX-3132 got "fixed" by raising the cap 500 -> 2000 — moving the
        // threshold without changing the failure. Live specimen: 6 of 277
        // bodies in 24h sat exactly at the cap, all unparseable.
        let over = format!(r#"{{"error":"{}"}}"#, "z".repeat(ERROR_BODY_CHARS + 500));
        let cut = decoded_error_body(over.as_bytes(), "");
        assert!(cut.contains("<truncated by the request log"), "must announce the cut: {}", &cut[cut.len()-120..]);
        assert!(cut.contains(&format!("kept {ERROR_BODY_CHARS} of")), "must name both sizes");
        assert!(
            serde_json::from_str::<serde_json::Value>(&cut).is_err(),
            "still not valid JSON — the marker is honest about that, it does not repair it"
        );
        // A body UNDER the cap must be untouched: no marker, and still parseable.
        let small = decoded_error_body(br#"{"error":"nope"}"#, "");
        assert_eq!(small, r#"{"error":"nope"}"#, "under-cap bodies must not gain a marker");
        assert!(serde_json::from_str::<serde_json::Value>(&small).is_ok());
    }

    #[tokio::test]
    async fn excluded_paths_never_log_and_everything_else_does() {
        let (store, _dir) = store();
        let app = test_app(RequestLogger::spawn_with(store.clone(), 14.0, 1_000_000));
        for path in ["/health", "/api/events", "/api/debug/boundary", "/app.js", "/"] {
            let _ = hit(&app, HttpRequest::builder().uri(path).body(Body::empty()).unwrap()).await;
        }
        // A logged request AFTER the excluded ones: the channel is FIFO, so
        // when this row is visible, any (wrongly) sent earlier row would be
        // too — the absence check cannot pass by racing.
        let _ = hit(&app, HttpRequest::builder().uri("/api/board").body(Body::empty()).unwrap()).await;
        assert_eq!(wait_rows(&store, 1).await, 1, "exactly the /api/board row");
        let c = store.read().unwrap();
        let path: String =
            c.query_row("SELECT path FROM _amux_request_log", [], |r| r.get(0)).unwrap();
        assert_eq!(path, "/api/board");
    }

    #[tokio::test]
    async fn retention_sweep_fires_and_deletes_old_rows() {
        let (store, _dir) = store();
        // Pre-plant two ancient rows (90 days old) straight through the
        // writer — the specimen the sweep exists to delete.
        let old_ts = unix_now() - 90.0 * 86400.0;
        store
            .write_async(move |conn| {
                for i in 0..2 {
                    conn.execute(
                        "INSERT INTO _amux_request_log \
                         (ts, method, path, family, status, latency_ms, answered_by) \
                         VALUES (?1, 'GET', ?2, '/api/board', 200, 1.0, 'native')",
                        rusqlite::params![old_ts + f64::from(i), format!("/api/board/old{i}")],
                    )?;
                }
                Ok(WriteOutcome { applied: false, events: vec![] })
            })
            .await
            .unwrap();
        // sweep_every=3: the third inserted row triggers the delete.
        let app = test_app(RequestLogger::spawn_with(store.clone(), 14.0, 3));
        for _ in 0..3 {
            let _ = hit(&app, HttpRequest::builder().uri("/api/board").body(Body::empty()).unwrap()).await;
        }
        for _ in 0..200 {
            let c = store.read().unwrap();
            let old_left: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM _amux_request_log WHERE ts < ?1",
                    [unix_now() - 14.0 * 86400.0],
                    |r| r.get(0),
                )
                .unwrap();
            let fresh: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM _amux_request_log WHERE ts > ?1",
                    [unix_now() - 3600.0],
                    |r| r.get(0),
                )
                .unwrap();
            if old_left == 0 && fresh == 3 {
                return; // swept the ancient rows, kept the fresh ones
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("retention sweep did not fire (old rows still present after 5s)");
    }

    #[tokio::test]
    async fn api_logs_matches_python_fixture_shape_and_worker_param_subsets() {
        let (store, _dir) = store();
        let app_state = state(store.clone());
        let logged = test_app(RequestLogger::spawn_with(store.clone(), 14.0, 1_000_000));
        // Two rows: one worker-scoped, one not.
        let _ = hit(
            &logged,
            HttpRequest::builder()
                .uri("/api/sessions/w1/peek")
                .header("x-amux-session", "caller")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let _ = hit(&logged, HttpRequest::builder().uri("/api/board").body(Body::empty()).unwrap()).await;
        wait_rows(&store, 2).await;

        let api: Router = Router::new()
            .nest("/api/logs", routes())
            .with_state(app_state);
        let (st, body) = hit(
            &api,
            HttpRequest::builder().uri("/api/logs?limit=500").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let ours: Value = serde_json::from_slice(&body).unwrap();
        let fixture: Value = serde_json::from_str(PYTHON_LOGS_FIXTURE).unwrap();
        // Top-level: python's exact keys, present with python's types.
        assert!(ours["events"].is_array());
        assert!(ours["count"].is_number());
        // Event keys: every key python's live ring event carries must be
        // present on our events (the SPA maps over exactly these).
        let py_event = fixture["events"][0].as_object().unwrap();
        let our_event = ours["events"][0].as_object().unwrap();
        for key in py_event.keys() {
            assert!(our_event.contains_key(key), "python event key {key:?} missing from ours");
        }
        assert_eq!(our_event["type"], "http");
        assert_eq!(our_event["action"], "get");

        // Worker subset: same endpoint, ?worker= filter.
        let (st, body) = hit(
            &api,
            HttpRequest::builder().uri("/api/logs?worker=w1").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["count"], 1, "{v}");
        assert_eq!(v["total_matched"], 1);
        assert_eq!(v["events"][0]["worker"], "w1");
        assert_eq!(v["events"][0]["target"], "/api/sessions/w1/peek");

        // `category=board` SELECTS board-family rows. This assertion used to
        // demand an EMPTY array, which pinned the bug Ethan reported ("these
        // tabs in the logs dont work"): the handler short-circuited every
        // non-http category to empty, and the test certified it. The seeded row
        // above is a /api/board request, so the honest expectation is that the
        // Board tab finds it.
        let (_, body) = hit(
            &api,
            HttpRequest::builder().uri("/api/logs?category=board").body(Body::empty()).unwrap(),
        )
        .await;
        let v: Value = serde_json::from_slice(&body).unwrap();
        let evs = v["events"].as_array().expect("events array");
        assert!(!evs.is_empty(), "the Board tab must find a /api/board request: {v}");
        assert!(
            evs.iter().all(|e| e["family"] == "/api/board"),
            "the Board tab must show ONLY board-family rows: {v}"
        );
        assert_eq!(evs[0]["category"], "board", "the row's own stamp must agree with the tab");

        // A category with no matching traffic is still honestly empty — the
        // half of the old assertion that was always right.
        let (_, body) = hit(
            &api,
            HttpRequest::builder().uri("/api/logs?category=memory").body(Body::empty()).unwrap(),
        )
        .await;
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["events"], json!([]), "no memory traffic seeded, so no rows");
    }

    #[tokio::test]
    async fn api_logs_raw_merges_both_sources_and_labels_them() {
        let (store, _dir) = store();
        // A fake tracing log with an RFC3339-stamped line + a continuation.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("logs")).unwrap();
        std::fs::write(
            home.path().join("logs/server-rs.log"),
            "2026-08-09T18:00:00.000000Z  INFO amux_server: listening\n  continuation line\n",
        )
        .unwrap();
        // One request-log row, newer than the file lines.
        store
            .write_async(|conn| {
                conn.execute(
                    "INSERT INTO _amux_request_log \
                     (ts, method, path, family, status, latency_ms, client_ip, amux_session, worker, answered_by) \
                     VALUES (?1, 'GET', '/api/board', '/api/board', 200, 12.3, '127.0.0.1', 'caller', NULL, 'python-proxy')",
                    [unix_now()],
                )?;
                Ok(WriteOutcome { applied: false, events: vec![] })
            })
            .await
            .unwrap();

        let payload =
            raw_payload(&home.path().join("logs/server-rs.log"), 300, &state(store.clone())).unwrap();
        for key in PYTHON_RAW_KEYS {
            assert!(payload.get(*key).is_some(), "python raw key {key:?} missing");
        }
        let lines: Vec<String> = payload["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let sources: Vec<String> = payload["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(lines.len(), sources.len(), "sources labels every line");
        assert_eq!(payload["total"], 3, "2 file lines + 1 request-log row");
        assert!(sources.contains(&"server_log".to_string()));
        assert!(sources.contains(&"request_log".to_string()));
        // The request-log line uses python's slog format and is the NEWEST,
        // so it merges last; proxy attribution rides the line.
        let last = lines.last().unwrap();
        assert!(last.contains("[127.0.0.1] GET /api/board 200 12ms"), "{last}");
        assert!(last.contains("session=caller"), "{last}");
        assert!(last.contains("via=python-proxy"), "{last}");
        assert!(
            regex::Regex::new(r"^\d{4}-\d{2}-\d{2} ").unwrap().is_match(last),
            "python slog date shape so SPA styling applies: {last}"
        );
        // Missing file: python's empty-shape parity, request log still served.
        let empty = raw_payload(Path::new("/nonexistent/nope.log"), 10, &state(store)).unwrap();
        assert_eq!(empty["lines"].as_array().unwrap().len(), 1);
        assert_eq!(empty["total"], 1);
    }

    // -- AMUX-2610: ROUTE_TABLE matching + the analysis endpoints -----------

    #[test]
    fn route_table_matching_and_normalization() {
        // Routed paths normalize to their table pattern.
        assert_eq!(normalize_target("/api/board/AMUX-123"), "/api/board/{id}");
        assert_eq!(normalize_target("/api/board/statuses"), "/api/board/statuses");
        assert_eq!(
            normalize_target("/api/board/statuses/review"),
            "/api/board/statuses/{sid}"
        );
        assert_eq!(normalize_target("/api/sessions/w1"), "/api/sessions/{name}");
        assert_eq!(
            normalize_target("/api/sessions/w1/peek"),
            "/api/sessions/{name}/{*verb}"
        );
        // matchit semantics: the static segment outranks {action}.
        assert_eq!(normalize_target("/api/torrents/g1/file"), "/api/torrents/{gid}/file");
        assert_eq!(
            normalize_target("/api/torrents/g1/pause"),
            "/api/torrents/{gid}/{action}"
        );
        // Unrouted paths: conservative collapse — words stay, ids fold.
        assert_eq!(normalize_target("/api/sessions-graph"), "/api/sessions-graph");
        assert_eq!(normalize_target("/api/stripe/status"), "/api/stripe/status");
        assert_eq!(normalize_target("/api/sessions-graph"), "/api/sessions-graph");
        assert_eq!(normalize_target("/api/foo/AMUX-9"), "/api/foo/{id}");

        assert_eq!(routed_methods_at("/api/board/statuses/review"), vec!["PATCH", "DELETE"]);
        // Re-pointed from /api/lookup, which became ROUTED in d177625. A
        // fixture that names a real unrouted path is worth keeping accurate
        // rather than deleting — this cell is the "no route at all" case, and
        // it needs a path that genuinely has none.
        assert_eq!(routed_methods_at("/api/sessions-graph"), Vec::<&str>::new());
        assert_eq!(routed_methods_at("/api/scope"), vec!["*"]);
        // No POST at /{gid}/file even though /{gid}/{action} routes POST —
        // the best (static) match wins, exactly as axum dispatches.
        assert_eq!(routed_methods_at("/api/torrents/g1/file"), vec!["GET"]);

        let near = nearest_routes("/api/sessions-graph", 3);
        assert!(near.contains(&"/api/sessions"), "{near:?}");
        assert!(near.len() <= 3);
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let v: Vec<f64> = (1..=10).map(f64::from).collect();
        assert_eq!(percentile_sorted(&v, 0.50), 5.0);
        assert_eq!(percentile_sorted(&v, 0.95), 10.0);
        assert_eq!(percentile_sorted(&[42.0], 0.5), 42.0);
        assert_eq!(percentile_sorted(&[], 0.5), 0.0);
        // n=5: p50 = rank ceil(2.5)=3 -> third value.
        assert_eq!(percentile_sorted(&[10.0, 10.0, 10.0, 10.0, 100.0], 0.5), 10.0);
        assert_eq!(percentile_sorted(&[10.0, 10.0, 10.0, 10.0, 100.0], 0.95), 100.0);
    }

    /// Seed one request-log row with the columns the analysis endpoints read.
    #[allow(clippy::too_many_arguments)]
    async fn seed(
        store: &crate::db::Store,
        ts: f64,
        method: &str,
        path: &str,
        status: i64,
        latency_ms: f64,
        session: &str,
        answered_by: &str,
        error_body: Option<&str>,
    ) {
        let (method, path, session, answered_by) = (
            method.to_string(),
            path.to_string(),
            session.to_string(),
            answered_by.to_string(),
        );
        let error_body = error_body.map(str::to_string);
        store
            .write_async(move |conn| {
                conn.execute(
                    "INSERT INTO _amux_request_log \
                     (ts, method, path, family, status, latency_ms, client_ip, \
                      amux_session, worker, answered_by, error_body) \
                     VALUES (?1,?2,?3,?4,?5,?6,'127.0.0.1',?7,NULL,?8,?9)",
                    rusqlite::params![
                        ts,
                        method,
                        path,
                        family_of(&path),
                        status,
                        latency_ms,
                        session,
                        answered_by,
                        error_body
                    ],
                )?;
                Ok(WriteOutcome { applied: false, events: vec![] })
            })
            .await
            .unwrap();
    }

    fn logs_api(store: Arc<crate::db::Store>) -> Router {
        Router::new().nest("/api/logs", routes()).with_state(state(store))
    }

    /// AF-131, rebuilt from the sweep's own numbers: three /api/logs/stats
    /// calls (96h/192h/336h) each scanned exactly the 200k cap over the SAME
    /// rows, yet actual_window_h reported min(requested, data_span) — and the
    /// capped slice kept the OLDEST rows, so the "trailing norm" was days 8-5
    /// and a 6.46x p95 finding was 1.07x against an honest window. Both
    /// endpoints share the scan shape, so one over-cap store pins both: the
    /// slice must be the NEWEST rows, and the window claim must shrink to
    /// what was actually read.
    #[tokio::test]
    async fn truncated_scans_keep_the_newest_rows_and_say_the_real_window() {
        let (store, _dir) = store();
        let now = unix_now();
        // 50k "old-era" rows around 90-80h ago, then 210k "new-era" rows in
        // the last 70h — 260k total against the 200k cap, so a truthful
        // truncation contains ZERO old-era rows.
        store
            .write(move |conn| {
                let tx_like = conn; // Store::write is already one transaction
                let mut stmt = tx_like.prepare(
                    "INSERT INTO _amux_request_log \
                     (ts, method, path, family, status, latency_ms, client_ip, \
                      amux_session, worker, answered_by, error_body) \
                     VALUES (?1,'GET',?2,?3,500,1.0,'127.0.0.1','lane',NULL,'native',NULL)",
                )?;
                for i in 0..50_000 {
                    let ts = now - 90.0 * 3600.0 + i as f64 * 0.5;
                    stmt.execute(rusqlite::params![ts, "/api/old-era", "old-era"])?;
                }
                for i in 0..210_000 {
                    let ts = now - 70.0 * 3600.0 + i as f64;
                    stmt.execute(rusqlite::params![ts, "/api/new-era", "new-era"])?;
                }
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let api = logs_api(store.clone());

        // STATS: the capped slice is the newest 200k (all new-era), and the
        // window claim shrinks to what was read.
        let (st, body) = hit(
            &api,
            HttpRequest::builder().uri("/api/logs/stats?since_h=96").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["scan_truncated"], true, "{v}");
        let fams: Vec<&str> = v["families"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["family"].as_str().unwrap())
            .collect();
        assert!(
            !fams.contains(&"old-era"),
            "a truncated trailing window must keep the NEWEST rows — old-era rows \
             surviving means the cap ate the recent traffic: {fams:?}"
        );
        assert!(fams.contains(&"new-era"), "{fams:?}");
        let aw = v["actual_window_h"].as_f64().unwrap();
        assert!(
            aw < 70.0,
            "actual_window_h must report the span actually READ (the newest ~200k of \
             the last 70h), never min(requested, data_span): got {aw} vs since_h=96"
        );

        // ANALYZE: same slice rule, same honest window, and the group's
        // last_ts must be its NEWEST hit even though the scan is now DESC.
        let (st, body) = hit(
            &api,
            HttpRequest::builder()
                .uri("/api/logs/analyze?since_h=96")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["scan_truncated"], true, "{v}");
        let groups = v["groups"].as_array().unwrap();
        assert!(
            groups.iter().all(|g| g["family"] != "old-era"),
            "analyze must also keep the newest under the cap: {v}"
        );
        let new_era = groups.iter().find(|g| g["family"] == "new-era").expect("new-era group");
        let last = new_era["last_ts"].as_f64().unwrap();
        // The fixture's newest new-era row is at now - (70h - 209,999s), i.e.
        // ~11.67h ago; a positional overwrite under the DESC scan would land
        // ~70h ago instead.
        let expected_newest = now - 70.0 * 3600.0 + 209_999.0;
        assert!(
            (last - expected_newest).abs() < 5.0,
            "last_ts must be the group's NEWEST hit — a positional overwrite under the \
             DESC scan reports the oldest: last_ts {last} vs expected {expected_newest}"
        );
        let aw = v["actual_window_h"].as_f64().unwrap();
        assert!(aw < 70.0, "analyze window claim must shrink too: {aw}");
    }

    #[tokio::test]
    async fn analyze_groups_annotates_and_computes_all_three_405_verdict_cells() {
        let (store, _dir) = store();
        let now = unix_now();
        // Cell 1 (the incident specimen, AMUX-2610): PATCH
        // /api/board/statuses/review 405'd on an older build; the CURRENT
        // table routes PATCH there — the verdict must say the build moved.
        seed(&store, now - 100.0, "PATCH", "/api/board/statuses/review", 405, 1.0, "lane-a", "native", None).await;
        seed(&store, now - 50.0, "PATCH", "/api/board/statuses/review", 405, 1.0, "lane-b", "native", None).await;
        // Cell 2 (the classic): PUT on a path that routes GET, POST.
        seed(&store, now - 40.0, "PUT", "/api/board", 405, 1.0, "lane-a", "native", None).await;
        // Cell 3 (the catch-all trap): POST on a path with NO route.
        seed(&store, now - 30.0, "POST", "/api/sessions-graph", 405, 1.0, "", "native", None).await;
        // 404s: one unrouted path (gets nearest_routes), one routed path
        // whose HANDLER 404'd (routed_methods shows it is a real route).
        seed(&store, now - 20.0, "GET", "/api/sessions-graph", 404, 1.0, "", "native", Some("{\"error\": \"not found\"}")).await;
        seed(&store, now - 19.0, "GET", "/api/sessions-graph", 404, 1.0, "", "native", Some("{\"error\": \"not found\"}")).await;
        seed(&store, now - 10.0, "GET", "/api/board/AMUX-9999", 404, 1.0, "lane-a", "native", Some("{\"error\":\"item not found\"}")).await;
        // Excluded: a success row, and an error outside the window.
        seed(&store, now - 5.0, "GET", "/api/board", 200, 1.0, "lane-a", "native", None).await;
        seed(&store, now - 90_000.0, "PATCH", "/api/board/statuses/review", 405, 1.0, "lane-a", "native", None).await;

        let api = logs_api(store.clone());
        let (st, body) = hit(
            &api,
            HttpRequest::builder().uri("/api/logs/analyze?since_h=24").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["total_errors"], 7, "{v}");
        assert_eq!(v["scan_truncated"], false);

        let groups = v["groups"].as_array().unwrap();
        let find = |status: i64, method: &str, target: &str| {
            groups
                .iter()
                .find(|g| g["status"] == status && g["method"] == method && g["target"] == target)
                .unwrap_or_else(|| panic!("missing group {status} {method} {target}: {v}"))
        };

        let g = find(405, "PATCH", "/api/board/statuses/{sid}");
        assert_eq!(g["count"], 2, "window bounds the old row out");
        assert_eq!(g["distinct_clients"], 2);
        assert_eq!(g["family"], "/api/board");
        assert_eq!(g["routed_methods"], json!(["PATCH", "DELETE"]));
        assert_eq!(g["sample"]["path"], "/api/board/statuses/review");

        let g = find(404, "GET", "/api/sessions-graph");
        assert_eq!(g["count"], 2);
        assert_eq!(g["routed_methods"], json!([]));
        assert!(
            g["nearest_routes"].as_array().unwrap().iter().any(|r| r == "/api/sessions"),
            "{g}"
        );
        assert_eq!(g["sample"]["error_body"], "{\"error\": \"not found\"}");

        let g = find(404, "GET", "/api/board/{id}");
        // Same stale expectation as in debug_routes below: DELETE really is
        // routed here (board.rs:58). The point of this assertion is that a 404
        // at a path WITH routed methods is the handler's own not-found, not an
        // unrouted path — which the DELETE makes no less true.
        assert_eq!(
            g["routed_methods"],
            json!(["GET", "PATCH", "DELETE"]),
            "a real route whose handler 404'd"
        );

        // The verdicts: one per 405 group, each landing in its honest cell.
        let verdicts: Vec<&str> =
            v["verdicts"].as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect();
        assert_eq!(verdicts.len(), 3, "{verdicts:?}");
        let vd = |frag: &str| {
            verdicts
                .iter()
                .find(|s| s.contains(frag))
                .unwrap_or_else(|| panic!("no verdict containing {frag:?}: {verdicts:?}"))
        };
        let cell1 = vd("PATCH /api/board/statuses/{sid}");
        assert!(cell1.contains("IS routed here in the CURRENT build"), "{cell1}");
        assert!(cell1.contains("PATCH, DELETE"), "{cell1}");
        let cell2 = vd("PUT /api/board:");
        assert!(cell2.contains("not routed; routed there: GET, POST"), "{cell2}");
        // Re-pointed from /api/lookup (routed in d177625) to a path that is
        // still genuinely unrouted, so this cell keeps testing what it names:
        // a 405 where NO route exists is the GET-only SPA catch-all.
        let cell3 = vd("POST /api/sessions-graph");
        assert!(cell3.contains("no route exists at this path"), "{cell3}");
        assert!(cell3.contains("GET-only"), "{cell3}");
    }

    /// A row whose CLOCK spans a restart is not a slow request (AF-186).
    ///
    /// autofix stopped counting these in AF-175. /api/logs/stats — the endpoint
    /// a human opens — still did, and on 2026-08-24 the daily sweep's
    /// slow_outliers was six of them at 33-58s, completing 5s apart with
    /// latencies falling by exactly 5s, against a family p50 of 0.3ms. One
    /// dashboard poll draining across a restart, rendered as six slow requests.
    ///
    /// THE CONTROL IS THE SECOND HALF and it is the one that matters: a
    /// genuinely slow request that ARRIVED AFTER the boot must still appear.
    /// Large is not exempt; only crossing the boundary is. Without that cell a
    /// filter excluding everything passes.
    #[tokio::test]
    async fn stats_does_not_report_a_restart_spanning_row_as_a_slow_one() {
        let (store, _dir) = store();
        let now = unix_now();
        let boot = now - 300.0;
        // A fast baseline so p50 is small and the threshold (5x p50) is low.
        for i in 0..8 {
            seed_boot(&store, now - 200.0 + i as f64, "/api/board", 5.0, Some(boot)).await;
        }
        // THE SPECIMEN: completed after boot, ARRIVED 100s before it.
        seed_boot(&store, boot + 20.0, "/api/board", 120_000.0, Some(boot)).await;
        // THE CONTROL: equally slow, but it arrived well AFTER the boot.
        seed_boot(&store, now - 10.0, "/api/board", 120_000.0, Some(now - 130.0)).await;

        let api = logs_api(store.clone());
        let (st, body) = hit(
            &api,
            HttpRequest::builder().uri("/api/logs/stats?since_h=24").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let v: Value = serde_json::from_slice(&body).unwrap();

        let outliers = v["slow_outliers"].as_array().unwrap();
        assert_eq!(
            outliers.len(),
            1,
            "the spanning row must be gone and the genuinely slow one must remain: {outliers:?}"
        );
        // IDENTIFY the survivor, do not bit-compare it. `assert_eq!` on a
        // serde_json f64 is an equality of BITS, and this one crosses a JSON
        // encode/decode boundary: the endpoint emits `now - 10.0` through ryu
        // and the test parses the text back. That round trip came back one ULP
        // high on a GitHub runner on 2026-08-24 (1787580761.0102837 vs
        // ...835 — 2.4e-7 apart at this magnitude, which is exactly 1 ULP) and
        // failed the run. It reproduces on no local run in 150.
        //
        // The bit-equality was never the claim. The claim is WHICH ROW
        // SURVIVED, and the candidates are 183 seconds apart at the closest
        // (the baselines sit at now-200..now-193, the spanning specimen at
        // now-280). A millisecond window is five orders of magnitude tighter
        // than it needs to be to discriminate, and it stops the test asserting
        // a property of serde_json's float parser instead of a property of
        // this endpoint.
        let got = outliers[0]["ts"].as_f64().expect("outlier ts is a number");
        assert!(
            (got - (now - 10.0)).abs() < 1e-3,
            "the survivor is the one after boot: got {got}, want {}, off by {}s ({:?})",
            now - 10.0,
            got - (now - 10.0),
            outliers[0]
        );

        // THE EXCLUSION IS PUBLISHED, NOT SILENT. A detector that quietly drops
        // rows is one nobody can audit (AF-178), and a zero here is what tells a
        // reader the exclusion RAN.
        assert_eq!(v["totals"]["restart_spanning_excluded"], json!(1), "{v}");
        let board = v["families"].as_array().unwrap().iter()
            .find(|f| f["family"] == "/api/board").unwrap();
        assert_eq!(board["restart_spanning_excluded"], json!(1), "{board}");
        // And it is out of the latency statistics too, not merely the list.
        assert_eq!(board["count"], 9, "8 fast + the control, not the spanning row: {board}");
        assert_eq!(board["max_ms"], 120_000.0, "the CONTROL is still the max — it is real: {board}");
    }

    /// Insert with an explicit `boot_at`, which `seed` does not carry.
    async fn seed_boot(
        store: &crate::db::Store,
        ts: f64,
        path: &str,
        latency_ms: f64,
        boot_at: Option<f64>,
    ) {
        let path = path.to_string();
        store
            .write_async(move |conn| {
                conn.execute(
                    "INSERT INTO _amux_request_log \
                     (ts, method, path, family, status, latency_ms, client_ip, \
                      user_agent, amux_session, worker, answered_by, boot_at) \
                     VALUES (?1,'GET',?2,?2,200,?3,'127.0.0.1','curl','lane','','native',?4)",
                    rusqlite::params![ts, path, latency_ms, boot_at],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stats_percentiles_rates_and_outliers_per_family() {
        let (store, _dir) = store();
        let now = unix_now();
        // /api/board: latencies [10,10,10,10,100] -> p50=10, p95=100, max=100.
        // One 500 (error_rate 0.2), one proxied row, worker attribution via
        // path-independent seed (worker column stays NULL; clients differ).
        for (i, lat) in [10.0, 10.0, 10.0, 10.0].iter().enumerate() {
            seed(&store, now - 60.0 + i as f64, "GET", "/api/board", 200, *lat, "lane-a", "native", None).await;
        }
        seed(&store, now - 50.0, "GET", "/api/board/AMUX-1", 500, 100.0, "lane-b", "python-proxy", Some("boom")).await;
        // /api/logs: single fast row.
        seed(&store, now - 40.0, "GET", "/api/logs", 200, 5.0, "lane-a", "native", None).await;
        // Outside window: must not skew percentiles.
        seed(&store, now - 90_000.0, "GET", "/api/board", 200, 9999.0, "lane-a", "native", None).await;

        let api = logs_api(store.clone());
        let (st, body) = hit(
            &api,
            HttpRequest::builder().uri("/api/logs/stats?since_h=24").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert!(v["percentile_method"].as_str().unwrap().contains("nearest-rank"));

        let fams = v["families"].as_array().unwrap();
        let board = fams.iter().find(|f| f["family"] == "/api/board").unwrap();
        assert_eq!(board["count"], 5);
        assert_eq!(board["p50_ms"], 10.0);
        assert_eq!(board["p95_ms"], 100.0);
        assert_eq!(board["max_ms"], 100.0);
        assert_eq!(board["error_count"], 1);
        assert_eq!(board["error_rate"], 0.2);
        assert_eq!(board["proxy_count"], 1);
        assert_eq!(board["origins"], json!({"native": 4, "python-proxy": 1}));
        assert_eq!(board["distinct_clients"], 2);
        let logs = fams.iter().find(|f| f["family"] == "/api/logs").unwrap();
        assert_eq!(logs["count"], 1);
        assert_eq!(logs["p50_ms"], 5.0);

        assert_eq!(v["totals"]["count"], 6);
        assert_eq!(v["totals"]["error_count"], 1);
        assert_eq!(v["totals"]["proxy_count"], 1);
        assert_eq!(v["totals"]["origins"], json!({"native": 5, "python-proxy": 1}));

        // The 100ms row is > 5x the family p50 (10ms) -> the one outlier.
        let outliers = v["slow_outliers"].as_array().unwrap();
        assert_eq!(outliers.len(), 1, "{v}");
        assert_eq!(outliers[0]["path"], "/api/board/AMUX-1");
        assert_eq!(outliers[0]["ratio"], 10.0);
        assert_eq!(outliers[0]["family_p50_ms"], 10.0);
    }

    #[tokio::test]
    async fn debug_routes_serves_the_table_with_owner_from_the_boundary_registry() {
        let v = debug_routes().await.0;
        assert_eq!(v["count"], ROUTE_TABLE.len());
        let routes = v["routes"].as_array().unwrap();
        assert_eq!(routes.len(), ROUTE_TABLE.len());
        let find = |p: &str| routes.iter().find(|r| r["path"] == p).unwrap();
        assert_eq!(find("/api/board/{id}")["owner"], "native");
        assert_eq!(find("/api/board/{id}")["family"], "/api/board");
        // DELETE is genuinely routed — board.rs:58 is
        // `get(get_item).patch(patch_item).delete(delete_item)` — so the
        // ROUTE_TABLE row is right and it was this EXPECTATION that went stale
        // when delete landed. Fixed the assertion, not the table: the table is
        // what /api/debug/routes serves, and editing it to match a stale test
        // would have made the endpoint lie about a method it really answers.
        assert_eq!(
            find("/api/board/{id}")["methods"],
            json!(["GET", "PATCH", "DELETE"])
        );
        // Owner derives from PROXIED_FAMILIES. The registry is EMPTY since
        // the /api/scope cutover (the last python-owned family went native),
        // so every row reads native today; the derivation stays registry-
        // driven so a re-proxied family would flip without touching this
        // module.
        assert!(routes.iter().all(|r| r["owner"] == "native"), "{v}");
        assert_eq!(find("/api/scope")["methods"], json!(["*"]));
    }
}

#[cfg(test)]
mod category_tests {
    use super::category_of;

    /// Ethan, 2026-08-10: "these tabs in the logs dont work". Five of the six
    /// Logs tabs matched ZERO events because every row was stamped "http". The
    /// tab labels are the contract — each one a user can click must be
    /// reachable by some real family.
    #[test]
    fn every_clickable_tab_is_reachable_from_some_family() {
        for (family, want) in [
            ("/api/board", "board"),
            ("/api/schedules", "board"),
            ("/api/sessions", "session"),
            ("/api/workers", "session"),
            ("/api/memory", "memory"),
            ("/api/scope", "memory"),
            ("/api/fs", "files"),
            ("/api/upload", "files"),
        ] {
            assert_eq!(category_of(family), want, "family {family}");
        }
    }

    /// An unmapped family stays "http" rather than inventing a bucket. The All
    /// tab shows it either way, so a wrong guess would only mislabel it.
    /// The two definitions must agree: every family a tab SELECTS must also be
    /// STAMPED with that tab's category. If they drift, a row shows up under a
    /// tab while its own category field says something else.
    #[test]
    fn the_selector_and_the_stamp_cannot_disagree() {
        for cat in ["board", "session", "memory", "files"] {
            let fams = super::families_for_category(cat);
            assert!(!fams.is_empty(), "tab {cat} selects no families — it would be dead");
            for f in fams {
                assert_eq!(category_of(f), cat, "{f} is selected by {cat} but stamped differently");
            }
        }
        // http is the COMPLEMENT, so it names no families by design.
        assert!(super::families_for_category("http").is_empty());
    }

    #[test]
    fn an_unmapped_family_is_honestly_http() {
        assert_eq!(category_of("/api/usage"), "http");
        assert_eq!(category_of("/health"), "http");
        assert_eq!(category_of(""), "http");
    }
}
