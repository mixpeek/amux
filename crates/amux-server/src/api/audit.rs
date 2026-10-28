//! `GET /api/audit?worker=<name>&since_h=<h>&kind=browser,tool,api&limit=<n>`:
//! one timeline of what a worker did, newest first, from the trails amux
//! already keeps. Nothing new is stored here.
//!
//! - `browser`: `session_events` rows of type `browser.*` (native verbs and,
//!   since 2026-10-27, every `amux browser route` verb as `browser.route.*`).
//! - `tool`: `_amux_tool_events` (Claude Code tool calls via the PostToolUse
//!   report, OpenCode tool calls via the orchestrator).
//! - `api`: `_amux_request_log` rows the worker made against amux's own API.
//!
//! Keyed by the worker NAME (the session), which is what all three trails
//! record. `why worker` resolves through `_amux_workers`, which tmux-run
//! workers are not always registered in, so it could not serve this.
//!
//! Like `why`, every source is reported with the predicate it ran and its row
//! count, including zero, so "nothing happened" and "this trail could not see
//! it" are distinguishable. What no trail can see is named in `blind_spots`.
use super::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new().route("/", get(audit))
}

#[derive(Deserialize, Default)]
pub struct AuditQuery {
    pub worker: Option<String>,
    pub since_h: Option<f64>,
    pub kind: Option<String>,
    pub limit: Option<usize>,
}

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 2000;

/// Things this endpoint cannot see, stated in every answer so an empty
/// timeline is never read as "the worker did nothing there".
const BLIND_SPOTS: &[&str] = &[
    "raw CDP traffic: a worker that connects to the browser's debugging port directly (cdp_port) bypasses amux; its navigations and evals are not recorded",
    "outbound calls a worker makes itself (curl, SDKs) appear only as the Bash command that ran them, under kind=tool",
    "tool inputs are a short summary passed through the pattern secret redactor; vault values are not matched by value",
];

async fn audit(State(state): State<AppState>, Query(q): Query<AuditQuery>) -> Response {
    let Some(worker) = q.worker.as_deref().map(str::trim).filter(|w| !w.is_empty()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "worker is required", "measured": false,
            "why_unmeasured": "no worker named", "n_considered": 0}))).into_response();
    };
    let since_h = q.since_h.filter(|h| h.is_finite() && *h > 0.0).unwrap_or(24.0);
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let kinds: Vec<String> = q.kind.as_deref().unwrap_or("browser,tool,api").split(',')
        .map(|k| k.trim().to_ascii_lowercase()).filter(|k| !k.is_empty()).collect();
    let Ok(conn) = state.store.read() else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "store unavailable", "measured": false,
            "why_unmeasured": "store unavailable", "n_considered": 0}))).into_response();
    };
    let since = crate::config::now_f64() - since_h * 3600.0;
    Json(timeline(&conn, worker, since, &kinds, limit)).into_response()
}

/// The timeline. Pure over a connection, so it is testable on a temp DB.
pub fn timeline(conn: &Connection, worker: &str, since: f64, kinds: &[String], limit: usize) -> Value {
    let mut events: Vec<(f64, Value)> = Vec::new();
    let mut sources = Vec::new();
    let want = |k: &str| kinds.iter().any(|x| x == k);
    if want("browser") {
        let (rows, total, err) = read(conn,
            "SELECT id, ts, type, data FROM session_events WHERE session=?1 AND type LIKE 'browser.%' AND ts>=?2 ORDER BY ts DESC LIMIT ?3",
            "SELECT COUNT(*) FROM session_events WHERE session=?1 AND type LIKE 'browser.%' AND ts>=?2",
            worker, json!(since), limit,
            |r| {
                let id: i64 = r.get(0)?;
                let ts: f64 = r.get(1)?;
                let ty: String = r.get(2)?;
                let data: String = r.get(3)?;
                let d: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
                Ok((ts, json!({"kind": "browser", "type": ty, "detail": d, "source": {"table": "session_events", "rowid": id}})))
            });
        sources.push(probe("session_events", "session=worker AND type LIKE 'browser.%' AND ts>=since (REAL seconds)", rows.len(), total, err));
        events.extend(rows);
    }
    if want("tool") {
        let since_rfc = chrono::DateTime::from_timestamp(since as i64, 0).map(|t| t.to_rfc3339()).unwrap_or_default();
        let (rows, total, err) = read(conn,
            "SELECT id, created_at, tool, detail, task_id FROM _amux_tool_events WHERE worker_id=?1 AND created_at>=?2 ORDER BY created_at DESC LIMIT ?3",
            "SELECT COUNT(*) FROM _amux_tool_events WHERE worker_id=?1 AND created_at>=?2",
            worker, json!(since_rfc), limit,
            |r| {
                let id: i64 = r.get(0)?;
                let at: String = r.get(1)?;
                let tool: String = r.get(2)?;
                let detail: Option<String> = r.get(3)?;
                let task: Option<String> = r.get(4)?;
                let ts = chrono::DateTime::parse_from_rfc3339(&at).map(|t| t.timestamp_millis() as f64 / 1000.0).unwrap_or(0.0);
                let d: Value = detail.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
                Ok((ts, json!({"kind": "tool", "type": tool, "detail": d, "task": task, "source": {"table": "_amux_tool_events", "rowid": id}})))
            });
        sources.push(probe("_amux_tool_events", "worker_id=worker AND created_at>=since (RFC 3339 text)", rows.len(), total, err));
        events.extend(rows);
    }
    if want("api") {
        let (rows, total, err) = read(conn,
            "SELECT id, ts, method, path, status, latency_ms FROM _amux_request_log WHERE amux_session=?1 AND ts>=?2 ORDER BY ts DESC LIMIT ?3",
            "SELECT COUNT(*) FROM _amux_request_log WHERE amux_session=?1 AND ts>=?2",
            worker, json!(since), limit,
            |r| {
                let id: i64 = r.get(0)?;
                let ts: f64 = r.get(1)?;
                let method: String = r.get(2)?;
                let path: String = r.get(3)?;
                let status: i64 = r.get(4)?;
                let ms: f64 = r.get::<_, Option<f64>>(5)?.unwrap_or(0.0);
                Ok((ts, json!({"kind": "api", "type": format!("{method} {path}"), "detail": {"status": status, "latency_ms": ms},
                    "source": {"table": "_amux_request_log", "rowid": id}})))
            });
        sources.push(probe("_amux_request_log", "amux_session=worker AND ts>=since (REAL seconds; bodies are never stored)", rows.len(), total, err));
        events.extend(rows);
    }
    events.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    events.truncate(limit);
    let n_considered: usize = sources.iter().map(|s| s["rows_total"].as_u64().unwrap_or(0) as usize).sum();
    let measured = sources.iter().any(|s| s["error"].is_null());
    let events: Vec<Value> = events.into_iter().map(|(ts, mut e)| {
        e["ts"] = json!(ts);
        e["at"] = json!(chrono::DateTime::from_timestamp_millis((ts * 1000.0) as i64).map(|t| t.to_rfc3339()).unwrap_or_default());
        e
    }).collect();
    json!({
        "worker": worker,
        "since": since,
        "kinds": kinds,
        "events": events,
        "truncated": n_considered > events.len(),
        "sources": sources,
        "blind_spots": BLIND_SPOTS,
        "measured": measured,
        "n_considered": n_considered,
        "why_unmeasured": if measured { Value::Null } else { json!("no requested source could be read") },
    })
}

type Row = (f64, Value);

fn read(
    conn: &Connection,
    sql: &str,
    count_sql: &str,
    worker: &str,
    since: Value,
    limit: usize,
    map: impl FnMut(&rusqlite::Row) -> rusqlite::Result<Row>,
) -> (Vec<Row>, usize, Option<String>) {
    let since: Box<dyn rusqlite::ToSql> = match since {
        Value::String(s) => Box::new(s),
        v => Box::new(v.as_f64().unwrap_or(0.0)),
    };
    let total = conn.query_row(count_sql, rusqlite::params![worker, since], |r| r.get::<_, i64>(0)).map(|n| n as usize);
    let rows = conn.prepare(sql).and_then(|mut s| {
        s.query_map(rusqlite::params![worker, since, limit as i64], map)?.collect::<rusqlite::Result<Vec<_>>>()
    });
    match (rows, total) {
        (Ok(rows), Ok(total)) => (rows, total, None),
        (Err(e), _) | (_, Err(e)) => (Vec::new(), 0, Some(e.to_string())),
    }
}

fn probe(table: &str, query: &str, rows: usize, total: usize, error: Option<String>) -> Value {
    json!({"table": table, "query": query, "rows": rows, "rows_total": total, "error": error})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE session_events (id INTEGER PRIMARY KEY, ts REAL, session TEXT, type TEXT, data TEXT, source TEXT);
             CREATE TABLE _amux_tool_events (id INTEGER PRIMARY KEY, worker_id TEXT, task_id TEXT, turn_id TEXT, tool TEXT, detail TEXT, created_at TEXT);
             CREATE TABLE _amux_request_log (id INTEGER PRIMARY KEY, ts REAL, method TEXT, path TEXT, status INTEGER, latency_ms REAL, amux_session TEXT);",
        ).unwrap();
        c
    }

    #[test]
    fn a_worker_timeline_merges_browser_tool_and_api_newest_first_and_names_every_source() {
        let c = db();
        let now = 1_800_000_000.0;
        c.execute("INSERT INTO session_events (ts,session,type,data,source) VALUES (?1,'w','browser.route.action','{\"action\":\"click\",\"url_after\":\"https://x/\"}','browser-api')", [now - 30.0]).unwrap();
        c.execute("INSERT INTO session_events (ts,session,type,data,source) VALUES (?1,'other','browser.action','{}','browser-api')", [now - 10.0]).unwrap();
        c.execute("INSERT INTO session_events (ts,session,type,data,source) VALUES (?1,'w','board.moved','{}','x')", [now - 5.0]).unwrap();
        let at = chrono::DateTime::from_timestamp((now - 20.0) as i64, 0).unwrap().to_rfc3339();
        c.execute("INSERT INTO _amux_tool_events (worker_id,tool,detail,created_at) VALUES ('w','Bash','{\"input\":\"ls\"}',?1)", [at]).unwrap();
        c.execute("INSERT INTO _amux_request_log (ts,method,path,status,latency_ms,amux_session) VALUES (?1,'POST','/api/browser/routing/request',200,12.0,'w')", [now - 40.0]).unwrap();
        c.execute("INSERT INTO _amux_request_log (ts,method,path,status,latency_ms,amux_session) VALUES (?1,'GET','/api/x',200,1.0,'w')", [now - 99_999.0]).unwrap();
        let kinds: Vec<String> = ["browser", "tool", "api"].iter().map(|s| s.to_string()).collect();
        let v = timeline(&c, "w", now - 3600.0, &kinds, 50);
        let types: Vec<&str> = v["events"].as_array().unwrap().iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(types, vec!["Bash", "browser.route.action", "POST /api/browser/routing/request"],
            "newest first; other workers, non-browser events and rows before `since` excluded");
        assert_eq!(v["n_considered"], 3);
        assert_eq!(v["measured"], true);
        assert_eq!(v["sources"].as_array().unwrap().len(), 3, "every source reported");
        assert!(v["blind_spots"].as_array().unwrap().iter().any(|b| b.as_str().unwrap().contains("raw CDP")));

        let only_tool = timeline(&c, "w", now - 3600.0, &["tool".to_string()], 50);
        assert_eq!(only_tool["events"].as_array().unwrap().len(), 1);
        let quiet = timeline(&c, "nobody", now - 3600.0, &kinds, 50);
        assert_eq!((quiet["measured"].as_bool(), quiet["n_considered"].as_u64()), (Some(true), Some(0)),
            "an empty timeline says it looked");
        let limited = timeline(&c, "w", now - 3600.0, &kinds, 1);
        assert_eq!((limited["events"].as_array().unwrap().len(), limited["truncated"].as_bool()), (1, Some(true)));
    }

    #[test]
    fn a_missing_table_is_reported_not_read_as_quiet() {
        let c = Connection::open_in_memory().unwrap();
        let v = timeline(&c, "w", 0.0, &["tool".to_string()], 10);
        assert_eq!(v["measured"], false);
        assert!(v["sources"][0]["error"].as_str().unwrap().contains("no such table"));
    }
}
