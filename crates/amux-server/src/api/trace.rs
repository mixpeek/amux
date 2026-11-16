//! One trace id per unit of work (governance plan 4.4; Vision A2, A4, A5, A9,
//! A11).
//!
//! A trace starts at an origin: a request that arrives without a `traceparent`
//! (a dashboard or phone send, a Messages post, an owner's `amux send`, an app
//! call, a page ask, a webhook) or a schedule firing. It is carried:
//!
//! - **over HTTP** as a W3C `traceparent` (`00-<32 hex trace>-<16 hex span>-01`),
//!   accepted on every request and returned on every response, so PR 9's
//!   OpenTelemetry can join the same ids;
//! - **into a worker as metadata, never as text** (A2, A9): delivering a
//!   message records its trace on the `cmd_history` row and writes
//!   `~/.amux/trace/<worker>.current`; the worker's `amux` CLI and git hook read
//!   that file, so every call the worker makes after the delivery carries the
//!   delivery's span as its parent;
//! - **onto effects**: ledger rows, policy receipts (connector mints, email gate
//!   decisions), the sent-email log and commit trailers (`Amux-Trace`).
//!
//! **Honest about turn boundaries.** A worker's "current" trace is the last
//! message delivered to it, nothing more: a call it makes about something else
//! afterwards is attributed to that message. Every record says how it got its
//! trace (`minted`, `inherited` from a `traceparent`, or `delivered` via the
//! worker's file) so the attribution is never claimed beyond what is known.
//!
//! **Never fails a request (A11).** `AMUX_TRACE=off` turns all of it off; a
//! failure to write the worker file logs `trace_propagation_failed` and is
//! counted in `GET /api/contract/counters` (`trace`).

use super::AppState;
use axum::{
    extract::{Path as AxPath, Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const ENABLED_ENV: &str = "AMUX_TRACE";
pub const HEADER: &str = "traceparent";

/// How a record came by its trace.
pub const MINTED: &str = "minted";
pub const INHERITED: &str = "inherited";
pub const DELIVERED: &str = "delivered";

pub fn enabled() -> bool {
    !matches!(
        std::env::var(ENABLED_ENV).unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "off" | "0" | "false" | "no"
    )
}

pub mod counters {
    use super::*;
    pub static MINTED_N: AtomicU64 = AtomicU64::new(0);
    pub static INHERITED_N: AtomicU64 = AtomicU64::new(0);
    pub static MALFORMED_N: AtomicU64 = AtomicU64::new(0);
    pub static DELIVERIES_N: AtomicU64 = AtomicU64::new(0);
    pub static PROPAGATION_FAILED_N: AtomicU64 = AtomicU64::new(0);
    pub fn bump(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

/// The trace a piece of work runs under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceCtx {
    pub trace_id: String,
    /// This unit's own span.
    pub span_id: String,
    /// The span it descends from; `None` at an origin.
    pub parent_span_id: Option<String>,
    /// `minted`, `inherited` or `delivered`.
    pub source: &'static str,
    /// Where it started: `request`, `schedule:<id>`, ...
    pub origin: String,
}

tokio::task_local! {
    /// The trace of the request or schedule firing this task is serving.
    pub static CURRENT: TraceCtx;
}

/// The trace this task runs under, if any. Spawned tasks do not inherit it;
/// records they write carry no trace rather than a guessed one.
pub fn current() -> Option<TraceCtx> {
    if !enabled() {
        return None;
    }
    CURRENT.try_with(|c| c.clone()).ok()
}

/// Carry this task's trace into a future that will run on another task.
/// `tokio::spawn` starts a fresh task with no task-locals, so a spawned send
/// would otherwise record its delivery with no trace. Captured now, at the
/// spawn site, not when the future is first polled.
pub fn carry<F>(fut: F) -> impl std::future::Future<Output = F::Output> + Send + 'static
where
    F: std::future::Future + Send + 'static,
    F::Output: Send,
{
    let ctx = CURRENT.try_with(|c| c.clone()).ok();
    async move {
        match ctx {
            Some(c) => CURRENT.scope(c, fut).await,
            None => fut.await,
        }
    }
}

fn hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    if getrandom::fill(&mut b).is_err() {
        // A trace id is an identifier, not a secret: a clock-derived one is a
        // worse id, never a failed request.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(1);
        for (i, x) in b.iter_mut().enumerate() {
            *x = (t >> ((i % 16) * 8)) as u8 ^ i as u8;
        }
    }
    let s: String = b.iter().map(|x| format!("{x:02x}")).collect();
    if s.chars().all(|c| c == '0') { "1".repeat(n * 2) } else { s }
}

pub fn new_trace_id() -> String {
    hex(16)
}
pub fn new_span_id() -> String {
    hex(8)
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && !s.bytes().all(|b| b == b'0')
}

/// `(trace_id, parent_span_id)` from a W3C `traceparent`; `None` when malformed.
pub fn parse(tp: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = tp.trim().split('-').collect();
    if parts.len() < 4 || parts[0].len() != 2 || parts[0] == "ff" {
        return None;
    }
    let (t, s) = (parts[1].to_ascii_lowercase(), parts[2].to_ascii_lowercase());
    (is_hex(&t, 32) && is_hex(&s, 16)).then_some((t, s))
}

pub fn format(trace_id: &str, span_id: &str) -> String {
    format!("00-{trace_id}-{span_id}-01")
}

impl TraceCtx {
    /// A new trace at an origin.
    pub fn mint(origin: impl Into<String>) -> Self {
        counters::bump(&counters::MINTED_N);
        TraceCtx {
            trace_id: new_trace_id(),
            span_id: new_span_id(),
            parent_span_id: None,
            source: MINTED,
            origin: origin.into(),
        }
    }

    /// A new span under a parent the caller named.
    pub fn child(trace_id: String, parent_span_id: String, source: &'static str, origin: impl Into<String>) -> Self {
        TraceCtx { trace_id, span_id: new_span_id(), parent_span_id: Some(parent_span_id), source, origin: origin.into() }
    }

    pub fn traceparent(&self) -> String {
        format(&self.trace_id, &self.span_id)
    }
}

/// The context for one request: inherited from its `traceparent`, else minted.
pub fn for_request(header: Option<&str>, path: &str) -> TraceCtx {
    match header {
        Some(h) => match parse(h) {
            Some((t, s)) => {
                counters::bump(&counters::INHERITED_N);
                TraceCtx::child(t, s, INHERITED, "request")
            }
            None => {
                // A malformed header is the caller's bug, never a refusal.
                counters::bump(&counters::MALFORMED_N);
                tracing::debug!(verdict = "traceparent_malformed", %path, "minted a new trace");
                TraceCtx::mint("request")
            }
        },
        None => TraceCtx::mint("request"),
    }
}

/// Outermost API layer: every request runs inside its trace, and the response
/// names it, so a client can continue the same trace.
pub async fn layer(req: Request, next: Next) -> Response {
    if !enabled() {
        return next.run(req).await;
    }
    let header = req.headers().get(HEADER).and_then(|v| v.to_str().ok()).map(str::to_string);
    let ctx = for_request(header.as_deref(), req.uri().path());
    let tp = ctx.traceparent();
    let mut res = CURRENT.scope(ctx, next.run(req)).await;
    if let Ok(v) = HeaderValue::from_str(&tp) {
        res.headers_mut().insert(HEADER, v);
    }
    res
}

// ── Into a worker ───────────────────────────────────────────────────────────

pub fn worker_file(home: &Path, worker: &str) -> Option<PathBuf> {
    let safe = !worker.is_empty()
        && worker.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !worker.starts_with('.');
    safe.then(|| home.join("trace").join(format!("{worker}.current")))
}

/// The span a delivery to `worker` runs as: a child of the request or firing
/// that delivered it. Written to the worker's file so the worker's own calls
/// descend from it.
pub fn delivery(worker: &str) -> Option<TraceCtx> {
    let parent = current()?;
    let ctx = TraceCtx::child(parent.trace_id, parent.span_id, parent.source, format!("delivery:{worker}"));
    counters::bump(&counters::DELIVERIES_N);
    note_delivery(&crate::config::amux_home(), worker, &ctx);
    Some(ctx)
}

/// Write `<home>/trace/<worker>.current`: the delivery's `traceparent` on line
/// one, then how it was known. Never fails the delivery.
pub fn note_delivery(home: &Path, worker: &str, ctx: &TraceCtx) {
    let Some(path) = worker_file(home, worker) else {
        counters::bump(&counters::PROPAGATION_FAILED_N);
        tracing::warn!(verdict = "trace_propagation_failed", worker, reason = "unsafe_worker_name",
            measured = true, n_considered = 1, "the worker's next calls will start their own trace");
        return;
    };
    let body = format!("{}\nsource={}\norigin={}\n", ctx.traceparent(), DELIVERED, ctx.origin);
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("current.tmp");
        std::fs::write(&tmp, body.as_bytes())?;
        std::fs::rename(&tmp, &path)
    };
    if let Err(e) = write() {
        counters::bump(&counters::PROPAGATION_FAILED_N);
        tracing::warn!(verdict = "trace_propagation_failed", worker, error = %e,
            measured = true, n_considered = 1, "the worker's next calls will start their own trace");
    }
}

// ── Reading a trace back ────────────────────────────────────────────────────

/// Everything recorded under one trace, as a tree by span.
pub fn tree(conn: &Connection, trace_id: &str) -> rusqlite::Result<Value> {
    let mut nodes: Vec<Value> = Vec::new();
    let mut sources = vec![];

    let ledger_ok = conn
        .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='_amux_ledger'", [], |_| Ok(()))
        .is_ok();
    if ledger_ok {
        let mut st = conn.prepare(
            "SELECT event_id, ts, kind, actor, authority, action, resource, decision, outcome,
                    span_id, parent_span_id, trace_source
             FROM _amux_ledger WHERE trace_id = ?1 ORDER BY ts, seq",
        )?;
        let rows = st.query_map([trace_id], |r| {
            Ok(json!({
                "source": "ledger", "id": r.get::<_, String>(0)?, "ts": r.get::<_, i64>(1)?,
                "kind": r.get::<_, String>(2)?, "actor": r.get::<_, String>(3)?,
                "authority": r.get::<_, Option<String>>(4)?, "action": r.get::<_, Option<String>>(5)?,
                "resource": r.get::<_, Option<String>>(6)?, "decision": r.get::<_, Option<String>>(7)?,
                "outcome": r.get::<_, Option<String>>(8)?, "span_id": r.get::<_, Option<String>>(9)?,
                "parent_span_id": r.get::<_, Option<String>>(10)?, "trace_source": r.get::<_, Option<String>>(11)?,
            }))
        })?;
        let mut n = 0;
        for row in rows {
            nodes.push(row?);
            n += 1;
        }
        sources.push(json!({"name": "ledger", "consulted": true, "rows": n}));
    } else {
        sources.push(json!({"name": "ledger", "consulted": false, "why": "table absent"}));
    }

    let hist_ok = conn
        .prepare("SELECT trace_id, span_id, parent_span_id, trace_source FROM cmd_history LIMIT 0")
        .is_ok();
    if hist_ok {
        // Where each delivery's instruction came from (instruction_provenance),
        // when this database has the columns; null on an older one.
        let prov = conn.prepare("SELECT source_kind, source_ref, relayed_from FROM cmd_history LIMIT 0").is_ok();
        let cols = if prov { "source_kind, source_ref, relayed_from" } else { "NULL, NULL, NULL" };
        let mut st = conn.prepare(&format!(
            "SELECT id, ts, session, type, origin, delivery, span_id, parent_span_id, trace_source, {cols}
             FROM cmd_history WHERE trace_id = ?1 ORDER BY ts, id"
        ))?;
        let rows = st.query_map([trace_id], |r| {
            Ok(json!({
                "source": "cmd_history", "id": format!("MSG-{}", r.get::<_, i64>(0)?), "ts": r.get::<_, i64>(1)?,
                "kind": "delivery", "worker": r.get::<_, String>(2)?, "type": r.get::<_, Option<String>>(3)?,
                "origin": r.get::<_, Option<String>>(4)?, "delivery": r.get::<_, Option<String>>(5)?,
                "span_id": r.get::<_, Option<String>>(6)?, "parent_span_id": r.get::<_, Option<String>>(7)?,
                "trace_source": r.get::<_, Option<String>>(8)?,
                "source_kind": r.get::<_, Option<String>>(9)?, "source_ref": r.get::<_, Option<String>>(10)?,
                "relayed_from": r.get::<_, Option<String>>(11)?,
            }))
        })?;
        let mut n = 0;
        for row in rows {
            nodes.push(row?);
            n += 1;
        }
        sources.push(json!({"name": "cmd_history", "consulted": true, "rows": n}));
    } else {
        sources.push(json!({"name": "cmd_history", "consulted": false, "why": "trace columns absent"}));
    }
    // What the worker's CLI reported doing in a turn this trace delivered
    // (cli_telemetry.rs): tool calls and API requests, children of the
    // delivery span. Activity, not decisions, so not in the ledger.
    let tools_ok = conn.prepare("SELECT trace_id, parent_span_id, trace_source, source FROM _amux_tool_events LIMIT 0").is_ok();
    if tools_ok {
        let mut st = conn.prepare(
            "SELECT id, created_at, worker_id, tool, event_name, detail, parent_span_id, trace_source, source, turn_id
             FROM _amux_tool_events WHERE trace_id = ?1 ORDER BY created_at, id",
        )?;
        let rows = st.query_map([trace_id], |r| {
            let at: String = r.get(1)?;
            let ts = chrono::DateTime::parse_from_rfc3339(&at).map(|t| t.timestamp_millis()).unwrap_or(0);
            let detail: Option<String> = r.get(5)?;
            Ok(json!({
                "source": "tool_events", "id": format!("TOOL-{}", r.get::<_, i64>(0)?), "ts": ts,
                "kind": "tool", "worker": r.get::<_, String>(2)?, "tool": r.get::<_, String>(3)?,
                "event": r.get::<_, Option<String>>(4)?,
                "detail": detail.and_then(|d| serde_json::from_str::<Value>(&d).ok()),
                "span_id": Value::Null, "parent_span_id": r.get::<_, Option<String>>(6)?,
                "trace_source": r.get::<_, Option<String>>(7)?, "reported_by": r.get::<_, Option<String>>(8)?,
                "turn": r.get::<_, Option<String>>(9)?,
            }))
        })?;
        let mut n = 0;
        for row in rows {
            nodes.push(row?);
            n += 1;
        }
        sources.push(json!({"name": "tool_events", "consulted": true, "rows": n}));
    } else {
        sources.push(json!({"name": "tool_events", "consulted": false, "why": "trace columns absent"}));
    }
    // Named so a reader knows what this tree cannot show yet.
    sources.push(json!({"name": "conversation_posts", "consulted": false,
        "why": "posts are reached through their delivery rows; not stamped directly"}));
    sources.push(json!({"name": "email-sent.jsonl", "consulted": false,
        "why": "a file log; its rows carry trace_id and are listed by GET /api/email/log"}));

    nodes.sort_by_key(|n| n["ts"].as_i64().unwrap_or(0));
    let n_considered = nodes.len();
    let roots = nest(&nodes);
    Ok(json!({
        "trace_id": trace_id,
        "measured": true,
        "n_considered": n_considered,
        "sources": sources,
        "nodes": nodes,
        "tree": roots,
    }))
}

/// Nest nodes under the node whose span is their parent span. A node whose
/// parent span is not recorded here becomes a root, flagged `orphan`.
fn nest(nodes: &[Value]) -> Vec<Value> {
    use std::collections::HashMap;
    let span_of = |n: &Value| n["span_id"].as_str().map(str::to_string);
    let mut owner: HashMap<String, usize> = HashMap::new();
    for (i, n) in nodes.iter().enumerate() {
        if let Some(s) = span_of(n) {
            owner.entry(s).or_insert(i);
        }
    }
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut roots = vec![];
    for (i, n) in nodes.iter().enumerate() {
        let parent = n["parent_span_id"].as_str().and_then(|p| owner.get(p).copied()).filter(|&p| p != i);
        match parent {
            Some(p) => children.entry(p).or_default().push(i),
            None => roots.push(i),
        }
    }
    fn build(i: usize, nodes: &[Value], children: &std::collections::HashMap<usize, Vec<usize>>, depth: usize) -> Value {
        let mut v = nodes[i].clone();
        let kids: Vec<Value> = if depth > 64 {
            vec![]
        } else {
            children.get(&i).map(|c| c.iter().map(|&k| build(k, nodes, children, depth + 1)).collect()).unwrap_or_default()
        };
        v["children"] = json!(kids);
        v
    }
    roots
        .into_iter()
        .map(|i| {
            let mut v = build(i, nodes, &children, 0);
            if nodes[i]["parent_span_id"].as_str().is_some() {
                v["orphan"] = json!(true);
            }
            v
        })
        .collect()
}

async fn get_trace(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    let id = id.to_ascii_lowercase();
    if !is_hex(&id, 32) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "trace id is 32 lowercase hex characters",
            "measured": false, "n_considered": 0}))).into_response();
    }
    match state.store.read_async(move |c| Ok(tree(c, &id)?)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string(),
            "measured": false, "n_considered": 0}))).into_response(),
    }
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/trace/{id}", get(get_trace))
}

/// The A5 diagnostic, served in `GET /api/contract/counters` as `trace`.
pub fn report() -> Value {
    let minted = counters::get(&counters::MINTED_N);
    let inherited = counters::get(&counters::INHERITED_N);
    json!({
        "enabled": enabled(),
        "minted": minted,
        "inherited": inherited,
        "malformed_traceparent": counters::get(&counters::MALFORMED_N),
        "deliveries_stamped": counters::get(&counters::DELIVERIES_N),
        "trace_propagation_failed": counters::get(&counters::PROPAGATION_FAILED_N),
        "measured": true,
        "n_considered": minted + inherited,
        "lookup": "GET /api/trace/{id}",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_traceparent_round_trips_and_a_malformed_one_is_refused() {
        let t = new_trace_id();
        let s = new_span_id();
        assert_eq!(t.len(), 32);
        assert_eq!(s.len(), 16);
        assert_eq!(parse(&format(&t, &s)), Some((t.clone(), s.clone())));
        assert_eq!(parse("00-zz-11-01"), None);
        assert_eq!(parse(&format!("00-{}-{s}-01", "0".repeat(32))), None, "all-zero trace id is invalid");
        assert_eq!(parse(&format!("ff-{t}-{s}-01")), None, "version ff is invalid");
    }

    #[test]
    fn a_request_inherits_a_valid_parent_and_mints_otherwise() {
        let parent = TraceCtx::mint("test");
        let child = for_request(Some(&parent.traceparent()), "/api/x");
        assert_eq!(child.trace_id, parent.trace_id);
        assert_eq!(child.parent_span_id.as_deref(), Some(parent.span_id.as_str()));
        assert_ne!(child.span_id, parent.span_id);
        assert_eq!(child.source, INHERITED);
        let fresh = for_request(Some("garbage"), "/api/x");
        assert_eq!(fresh.source, MINTED);
        assert!(fresh.parent_span_id.is_none());
        assert_ne!(fresh.trace_id, parent.trace_id);
    }

    #[tokio::test]
    async fn a_delivery_descends_from_the_request_and_writes_the_worker_file() {
        let home = tempfile::tempdir().unwrap();
        let req = TraceCtx::mint("request");
        let (r2, home_p) = (req.clone(), home.path().to_path_buf());
        let got = CURRENT
            .scope(req.clone(), async move {
                let parent = current().unwrap();
                let ctx = TraceCtx::child(parent.trace_id, parent.span_id, parent.source, "delivery:w");
                note_delivery(&home_p, "w", &ctx);
                ctx
            })
            .await;
        assert_eq!(got.trace_id, r2.trace_id);
        assert_eq!(got.parent_span_id.as_deref(), Some(r2.span_id.as_str()));
        let body = std::fs::read_to_string(home.path().join("trace/w.current")).unwrap();
        assert_eq!(body.lines().next().unwrap(), got.traceparent());
        assert!(body.contains("source=delivered"));
        assert!(worker_file(home.path(), "../etc").is_none(), "a worker name never escapes the trace dir");
    }

    #[tokio::test]
    async fn a_spawned_task_keeps_the_trace_only_when_carried() {
        let ctx = TraceCtx::mint("request");
        let (bare, carried) = CURRENT
            .scope(ctx.clone(), async {
                let bare = tokio::spawn(async { CURRENT.try_with(|c| c.trace_id.clone()).ok() }).await.unwrap();
                let carried = tokio::spawn(carry(async { CURRENT.try_with(|c| c.trace_id.clone()).ok() })).await.unwrap();
                (bare, carried)
            })
            .await;
        assert_eq!(bare, None, "a plain spawn loses the trace");
        assert_eq!(carried, Some(ctx.trace_id));
    }

    #[test]
    fn the_tree_nests_by_parent_span_and_flags_orphans() {
        let n = |id: &str, ts: i64, span: &str, parent: Option<&str>| {
            json!({"id": id, "ts": ts, "span_id": span, "parent_span_id": parent})
        };
        let nodes = vec![
            n("root", 1, "a", None),
            n("delivery", 2, "b", Some("a")),
            n("worker-call", 3, "c", Some("b")),
            n("lost", 4, "d", Some("zz")),
        ];
        let roots = nest(&nodes);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0]["id"], "root");
        assert_eq!(roots[0]["children"][0]["id"], "delivery");
        assert_eq!(roots[0]["children"][0]["children"][0]["id"], "worker-call");
        assert_eq!(roots[1]["orphan"], true);
    }
}
