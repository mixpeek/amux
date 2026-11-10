//! The agent CLIs' own OpenTelemetry, collected rather than rebuilt (governance
//! plan 4.4; Vision A2, A4, A5, A9, A11).
//!
//! Claude Code already reports every tool decision and result (with the tool's
//! input when `OTEL_LOG_TOOL_DETAILS=1`), and every API request with its model,
//! tokens and cost, as OTLP log events. amux points it here instead of writing
//! hooks that re-derive the same facts (A2: when the harness offers a
//! capability, amux uses it). Measured on 2026-11-10 against Claude Code
//! 2.1.319: OTLP/HTTP with JSON bodies, headers from
//! `OTEL_EXPORTER_OTLP_HEADERS`, `/v1/logs` appended to the endpoint, a
//! self-signed server trusted through `NODE_EXTRA_CA_CERTS` (the exporter
//! ignores `OTEL_EXPORTER_OTLP_CERTIFICATE`), and no trace id on any record.
//!
//! - **Launch.** A Claude lane gets the OTel environment as configuration, the
//!   same for isolated workers (A9: environment, not a prompt). The exporter's
//!   headers carry the lane's worker token, so they ride the vault channel and
//!   never reach argv. A lane whose own environment already sets OTel is left
//!   alone and the launch says so (`cli_telemetry_user_otel_kept`).
//! - **Ingest.** `POST /api/otel/v1/logs` accepts a post only from a worker that
//!   proves itself with its token (governance PR 1); anything else is refused
//!   and counted. Tool results, rejected tool decisions, API requests and
//!   errors land in `_amux_tool_events` with the proven worker. They are
//!   activity, not decisions, so they stay out of the hash-chained ledger.
//!   Prompt and response text and the account's identity attributes are never
//!   stored; tool input passes the secret redactor and is capped.
//! - **Trace.** OTel records carry no trace id. A turn (`prompt.id`) takes the
//!   trace of the delivery that started it when the worker's trace file
//!   (`trace/<worker>.current`) was written shortly before the turn's first
//!   event; otherwise it is `unattributed`. Every row says which.
//! - **Metrics** (`POST /api/otel/v1/metrics`) are accepted and counted, not
//!   stored: their tokens and cost repeat the `api_request` events. Launch sets
//!   `OTEL_METRICS_EXPORTER=none` for that reason.
//! - **Switch.** `AMUX_CLI_TELEMETRY=off` stops wiring new launches and refuses
//!   ingest. Counts are in `GET /api/contract/counters` as `cli_telemetry`.

use super::AppState;
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const ENABLED_ENV: &str = "AMUX_CLI_TELEMETRY";
pub const HEADERS_ENV: &str = "OTEL_EXPORTER_OTLP_HEADERS";
/// The path prefix the CLI's endpoint names; it appends `/v1/logs`.
pub const ENDPOINT_PATH: &str = "/api/otel";

/// Delivery-to-turn window: a turn takes a delivery's trace only if the trace
/// file was written at most this long before the turn's first event.
const DELIVERY_WINDOW_MS: i64 = 300_000;
/// Clock slack the other way (file written just after the event's stamp).
const DELIVERY_SLACK_MS: i64 = 5_000;
const INPUT_CAP: usize = 400;

pub fn enabled() -> bool {
    enabled_from(std::env::var(ENABLED_ENV).ok().as_deref())
}

fn enabled_from(v: Option<&str>) -> bool {
    !matches!(v.map(|v| v.trim().trim_matches('"').to_ascii_lowercase()).as_deref(),
        Some("off") | Some("0") | Some("false") | Some("no"))
}

mod counters {
    use super::*;
    pub static POSTS: AtomicU64 = AtomicU64::new(0);
    pub static METRIC_POSTS: AtomicU64 = AtomicU64::new(0);
    pub static REFUSED_UNPROVEN: AtomicU64 = AtomicU64::new(0);
    pub static REFUSED_OFF: AtomicU64 = AtomicU64::new(0);
    pub static UNSUPPORTED: AtomicU64 = AtomicU64::new(0);
    pub static MALFORMED: AtomicU64 = AtomicU64::new(0);
    pub static RECORDS: AtomicU64 = AtomicU64::new(0);
    pub static STORED: AtomicU64 = AtomicU64::new(0);
    pub static MERGED: AtomicU64 = AtomicU64::new(0);
    pub static ATTRIBUTED: AtomicU64 = AtomicU64::new(0);
    pub static UNATTRIBUTED: AtomicU64 = AtomicU64::new(0);
    pub static STORE_FAILED: AtomicU64 = AtomicU64::new(0);
    pub static WIRED: AtomicU64 = AtomicU64::new(0);
    pub static USER_KEPT: AtomicU64 = AtomicU64::new(0);
    pub fn bump(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    pub fn add(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Ordering::Relaxed);
    }
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

/// When each worker last posted, since this process started.
fn last_seen() -> &'static Mutex<HashMap<String, i64>> {
    static M: std::sync::OnceLock<Mutex<HashMap<String, i64>>> = std::sync::OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

// ── Launch ──────────────────────────────────────────────────────────────────

/// What a launch does about the CLI's telemetry.
#[derive(Debug, PartialEq)]
pub enum Plan {
    /// Wire it: these go on the command line (no secrets), and the headers
    /// value rides the vault channel.
    Wire { vars: Vec<(String, String)> },
    /// The lane's own environment already configures OTel (`key`).
    UserOwns(String),
    Off,
    /// Not a CLI whose OTel this wires (Codex reads OTel from its config file).
    NotSupported,
}

/// Keys whose presence in the lane's own environment means the owner set OTel
/// up themselves; amux then sets nothing rather than clobber it.
const USER_KEYS: [&str; 4] = [
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_HEADERS",
    "CLAUDE_CODE_ENABLE_TELEMETRY",
];

fn attr_safe(v: &str) -> String {
    v.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).take(64).collect()
}

pub fn plan(home: &Path, lane: &str, provider: &str, groups: &str, port: u16, enabled: bool,
            own: &dyn Fn(&str) -> Option<String>) -> Plan {
    if !enabled {
        return Plan::Off;
    }
    if provider != "claude" {
        return Plan::NotSupported;
    }
    if let Some(k) = USER_KEYS.iter().find(|k| own(k).is_some_and(|v| !v.trim().is_empty())) {
        return Plan::UserOwns(k.to_string());
    }
    let mut attrs = format!("amux.worker={}", attr_safe(lane));
    let groups: Vec<String> = groups.split(',').map(str::trim).filter(|g| !g.is_empty()).map(attr_safe).collect();
    if !groups.is_empty() {
        attrs.push_str(&format!(",amux.group={}", groups.join("|")));
    }
    let mut vars: Vec<(String, String)> = vec![
        ("CLAUDE_CODE_ENABLE_TELEMETRY".into(), "1".into()),
        ("OTEL_LOGS_EXPORTER".into(), "otlp".into()),
        ("OTEL_METRICS_EXPORTER".into(), "none".into()),
        ("OTEL_EXPORTER_OTLP_PROTOCOL".into(), "http/json".into()),
        ("OTEL_EXPORTER_OTLP_ENDPOINT".into(), format!("https://localhost:{port}{ENDPOINT_PATH}")),
        ("OTEL_LOG_TOOL_DETAILS".into(), "1".into()),
        ("OTEL_LOGS_EXPORT_INTERVAL".into(), "5000".into()),
        ("OTEL_RESOURCE_ATTRIBUTES".into(), attrs),
    ];
    // amux serves a self-signed certificate; the exporter trusts it only
    // through the runtime's extra-CA list. Additive, and skipped if the lane
    // already sets its own list.
    let cert = home.join("tls").join("cert.pem");
    if cert.exists() && own("NODE_EXTRA_CA_CERTS").is_none() {
        vars.push(("NODE_EXTRA_CA_CERTS".into(), cert.to_string_lossy().into_owned()));
    }
    Plan::Wire { vars }
}

/// The plan for a real launch: the lane's own environment is server.env, its
/// scope env files, and the server process's environment.
pub fn plan_for_launch(home: &Path, lane: &str, provider: &str, groups: &str) -> Plan {
    let own = |k: &str| -> Option<String> {
        super::contract::lane_setting(home, lane, k).or_else(|| std::env::var(k).ok())
    };
    let p = plan(home, lane, provider, groups, crate::config::canonical_port(), enabled(), &own);
    match &p {
        Plan::Wire { .. } => counters::bump(&counters::WIRED),
        Plan::UserOwns(key) => {
            counters::bump(&counters::USER_KEPT);
            tracing::info!(session = lane, key = %key, verdict = "cli_telemetry_user_otel_kept", measured = true,
                n_considered = 1, "the lane configures OpenTelemetry itself; amux sets none of it");
        }
        _ => {}
    }
    p
}

/// `KEY=value` words for the command line (no secrets in them).
pub fn command_prefix(vars: &[(String, String)]) -> String {
    vars.iter().map(|(k, v)| format!("{k}={} ", super::session_verbs::sh_quote(v))).collect()
}

/// The exporter's headers: the lane's name and its worker token.
pub fn headers_value(lane: &str, token: &str) -> String {
    format!("x-amux-session={},{}={}", attr_safe(lane), super::worker_identity::TOKEN_HEADER, token)
}

// ── Parsing ─────────────────────────────────────────────────────────────────

/// One OTLP attribute value as a plain JSON value.
fn any_value(v: &Value) -> Value {
    if let Some(s) = v.get("stringValue") {
        return s.clone();
    }
    if let Some(i) = v.get("intValue") {
        // OTLP JSON encodes int64 as a string.
        return i.as_str().and_then(|s| s.parse::<i64>().ok()).map(Value::from).unwrap_or_else(|| i.clone());
    }
    for k in ["doubleValue", "boolValue"] {
        if let Some(x) = v.get(k) {
            return x.clone();
        }
    }
    Value::Null
}

fn attrs(list: Option<&Value>) -> HashMap<String, Value> {
    list.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|kv| Some((kv.get("key")?.as_str()?.to_string(), any_value(kv.get("value")?))))
                .collect()
        })
        .unwrap_or_default()
}

/// The fields amux keeps from one CLI event. Nothing else is read, so prompt
/// and response text and the account's identity never reach storage.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub name: String,
    pub ts_ms: i64,
    pub prompt_id: Option<String>,
    pub session_id: Option<String>,
    pub resource_worker: Option<String>,
    pub tool: Option<String>,
    pub tool_use_id: Option<String>,
    pub detail: Value,
}

fn s(a: &HashMap<String, Value>, k: &str) -> Option<String> {
    a.get(k).and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }).filter(|s| !s.is_empty())
}

fn num(a: &HashMap<String, Value>, k: &str) -> Value {
    match a.get(k) {
        Some(Value::String(s)) => s.parse::<f64>().ok().map(|f| {
            if f.fract() == 0.0 && f.abs() < 9.0e15 { Value::from(f as i64) } else { Value::from(f) }
        }).unwrap_or(Value::Null),
        Some(v @ Value::Number(_)) => v.clone(),
        _ => Value::Null,
    }
}

fn redacted_input(a: &HashMap<String, Value>) -> Value {
    let raw = s(a, "tool_input").or_else(|| s(a, "tool_parameters"));
    match raw {
        Some(r) => Value::from(super::session_verbs::redact_secrets(&r).chars().take(INPUT_CAP).collect::<String>()),
        None => Value::Null,
    }
}

fn ts_of(rec: &Value, a: &HashMap<String, Value>) -> i64 {
    if let Some(t) = s(a, "event.timestamp").and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok()) {
        return t.timestamp_millis();
    }
    rec.get("timeUnixNano").and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_u64().map(|n| n.to_string())))
        .and_then(|n| n.parse::<i128>().ok()).map(|n| (n / 1_000_000) as i64)
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis())
}

/// The events amux keeps, from one OTLP/JSON logs body. Returns the events and
/// how many records the body held.
pub fn parse_logs(body: &Value) -> (Vec<Event>, usize) {
    let mut out = vec![];
    let mut n = 0usize;
    for rl in body.get("resourceLogs").and_then(Value::as_array).into_iter().flatten() {
        let res = attrs(rl.get("resource").and_then(|r| r.get("attributes")));
        let resource_worker = s(&res, "amux.worker");
        for sl in rl.get("scopeLogs").and_then(Value::as_array).into_iter().flatten() {
            for rec in sl.get("logRecords").and_then(Value::as_array).into_iter().flatten() {
                n += 1;
                let a = attrs(rec.get("attributes"));
                let name = s(&a, "event.name")
                    .or_else(|| rec.get("body").and_then(|b| b.get("stringValue")).and_then(Value::as_str)
                        .map(|b| b.trim_start_matches("claude_code.").to_string()))
                    .unwrap_or_default();
                let base = |tool: Option<String>, tool_use_id: Option<String>, detail: Value| Event {
                    name: name.clone(),
                    ts_ms: ts_of(rec, &a),
                    prompt_id: s(&a, "prompt.id"),
                    session_id: s(&a, "session.id"),
                    resource_worker: resource_worker.clone(),
                    tool,
                    tool_use_id,
                    detail,
                };
                match name.as_str() {
                    "tool_result" => {
                        let id = s(&a, "tool_use_id");
                        out.push(base(s(&a, "tool_name"), id.clone(), json!({
                            "source": "otel", "event": "tool_result", "tool_use_id": id,
                            "ok": s(&a, "success").map(|v| v == "true"),
                            "duration_ms": num(&a, "duration_ms"),
                            "input": redacted_input(&a),
                            "input_bytes": num(&a, "tool_input_size_bytes"),
                            "result_bytes": num(&a, "tool_result_size_bytes"),
                            "error": s(&a, "error").map(|e| e.chars().take(INPUT_CAP).collect::<String>()),
                            "request_id": s(&a, "request_id"),
                        })));
                    }
                    // An accepted decision is implied by its result; a rejected
                    // one has no result and is the fact worth keeping.
                    "tool_decision" if s(&a, "decision").as_deref() != Some("accept") => {
                        let id = s(&a, "tool_use_id");
                        out.push(base(s(&a, "tool_name"), id.clone(), json!({
                            "source": "otel", "event": "tool_decision", "tool_use_id": id,
                            "decision": s(&a, "decision"), "decided_by": s(&a, "source"),
                            "input": redacted_input(&a),
                        })));
                    }
                    "api_request" | "api_error" => {
                        out.push(base(Some(name.clone()), None, json!({
                            "source": "otel", "event": name, "model": s(&a, "model"),
                            "input_tokens": num(&a, "input_tokens"), "output_tokens": num(&a, "output_tokens"),
                            "cache_read_tokens": num(&a, "cache_read_tokens"),
                            "cache_creation_tokens": num(&a, "cache_creation_tokens"),
                            "cost_usd": num(&a, "cost_usd"), "duration_ms": num(&a, "duration_ms"),
                            "status_code": num(&a, "status_code"),
                            "error": s(&a, "error").map(|e| e.chars().take(INPUT_CAP).collect::<String>()),
                            "request_id": s(&a, "request_id"),
                        })));
                    }
                    // The turn boundary: its length, never its text.
                    "user_prompt" => {
                        out.push(base(Some("user_prompt".into()), None, json!({
                            "source": "otel", "event": "user_prompt", "prompt_length": num(&a, "prompt_length"),
                        })));
                    }
                    _ => {}
                }
            }
        }
    }
    (out, n)
}

// ── Trace ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct TurnTrace {
    pub trace_id: Option<String>,
    pub parent_span_id: Option<String>,
    pub source: &'static str,
}

pub const UNATTRIBUTED: &str = "unattributed";

/// A turn's trace from the worker's trace file: the delivery's, if the file was
/// written shortly before the turn's first event; otherwise none.
pub fn correlate(first_event_ms: i64, file: Option<(i64, &str)>) -> TurnTrace {
    let none = TurnTrace { trace_id: None, parent_span_id: None, source: UNATTRIBUTED };
    let Some((written_ms, body)) = file else { return none };
    let lag = first_event_ms - written_ms;
    if !(-DELIVERY_SLACK_MS..=DELIVERY_WINDOW_MS).contains(&lag) {
        return none;
    }
    match body.lines().next().and_then(super::trace::parse) {
        Some((trace_id, span_id)) => TurnTrace { trace_id: Some(trace_id), parent_span_id: Some(span_id), source: super::trace::DELIVERED },
        None => none,
    }
}

fn read_trace_file(home: &Path, worker: &str) -> Option<(i64, String)> {
    let p = super::trace::worker_file(home, worker)?;
    let m = std::fs::metadata(&p).ok()?.modified().ok()?;
    let ms = m.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as i64;
    Some((ms, std::fs::read_to_string(&p).ok()?))
}

// ── Storing ─────────────────────────────────────────────────────────────────

/// Store one post's events for `worker`. A turn already seen keeps the trace it
/// was given; a tool call the status hook already recorded gains the trace
/// rather than a second row. Returns (stored, merged, attributed, unattributed).
pub fn store(c: &rusqlite::Connection, home: &Path, worker: &str, events: &[Event]) -> rusqlite::Result<(u64, u64, u64, u64)> {
    use rusqlite::OptionalExtension;
    let (mut stored, mut merged, mut attributed, mut unattributed) = (0, 0, 0, 0);
    let mut turns: HashMap<String, TurnTrace> = HashMap::new();
    let file = read_trace_file(home, worker);
    for e in events {
        let tt = match &e.prompt_id {
            Some(pid) => {
                if let Some(t) = turns.get(pid) {
                    t.clone()
                } else {
                    let prior: Option<(Option<String>, Option<String>, Option<String>)> = c
                        .query_row(
                            "SELECT trace_id, parent_span_id, trace_source FROM _amux_tool_events
                             WHERE worker_id=?1 AND turn_id=?2 AND source='otel' ORDER BY id LIMIT 1",
                            rusqlite::params![worker, pid],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                        )
                        .optional()?;
                    let t = match prior {
                        Some((tid, parent, src)) => TurnTrace {
                            source: if src.as_deref() == Some(super::trace::DELIVERED) { super::trace::DELIVERED } else { UNATTRIBUTED },
                            trace_id: tid,
                            parent_span_id: parent,
                        },
                        None => correlate(e.ts_ms, file.as_ref().map(|(ms, b)| (*ms, b.as_str()))),
                    };
                    turns.insert(pid.clone(), t.clone());
                    t
                }
            }
            None => TurnTrace { trace_id: None, parent_span_id: None, source: UNATTRIBUTED },
        };
        if tt.trace_id.is_some() { attributed += 1 } else { unattributed += 1 }
        let at = chrono::DateTime::from_timestamp_millis(e.ts_ms).unwrap_or_else(chrono::Utc::now).to_rfc3339();
        if let Some(id) = e.tool_use_id.as_deref().filter(|_| e.name == "tool_result") {
            let needle = format!("\"tool_use_id\":{}", json!(id));
            let hook_row: Option<i64> = c
                .query_row(
                    "SELECT id FROM _amux_tool_events WHERE worker_id=?1 AND instr(detail, ?2)>0
                     AND COALESCE(source,'hook')<>'otel' ORDER BY id DESC LIMIT 1",
                    rusqlite::params![worker, needle],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(row) = hook_row {
                c.execute(
                    "UPDATE _amux_tool_events SET turn_id=COALESCE(turn_id, ?2), trace_id=COALESCE(trace_id, ?3),
                     parent_span_id=COALESCE(parent_span_id, ?4), trace_source=COALESCE(trace_source, ?5),
                     source='hook+otel' WHERE id=?1",
                    rusqlite::params![row, e.prompt_id, tt.trace_id, tt.parent_span_id, tt.source],
                )?;
                merged += 1;
                continue;
            }
        }
        // The exporter retries a batch it is not sure landed, so an event seen
        // before is not a second row. Each kind has its own key: a tool call
        // its tool_use_id, an API call its request_id, a turn its prompt id.
        let key = match e.name.as_str() {
            "tool_result" | "tool_decision" => e.tool_use_id.as_ref().map(|id| format!("\"tool_use_id\":{}", json!(id))),
            "api_request" | "api_error" => e.detail.get("request_id").and_then(Value::as_str)
                .map(|id| format!("\"request_id\":{}", json!(id))),
            _ => None,
        };
        let dup: Option<i64> = match (&key, &e.prompt_id) {
            (Some(k), _) => c.query_row(
                "SELECT 1 FROM _amux_tool_events WHERE worker_id=?1 AND source='otel' AND event_name=?2 AND instr(detail, ?3)>0 LIMIT 1",
                rusqlite::params![worker, e.name, k], |r| r.get(0)).optional()?,
            (None, Some(pid)) if e.name == "user_prompt" => c.query_row(
                "SELECT 1 FROM _amux_tool_events WHERE worker_id=?1 AND source='otel' AND event_name='user_prompt' AND turn_id=?2 LIMIT 1",
                rusqlite::params![worker, pid], |r| r.get(0)).optional()?,
            _ => None,
        };
        if dup.is_some() {
            continue;
        }
        let mut detail = e.detail.clone();
        detail["conversation"] = json!(e.session_id);
        if let Some(rw) = e.resource_worker.as_deref().filter(|rw| *rw != worker) {
            // The proven worker wins; the claim is kept beside it.
            detail["resource_worker_claimed"] = json!(rw);
        }
        c.execute(
            "INSERT INTO _amux_tool_events(worker_id,task_id,turn_id,tool,detail,created_at,trace_id,parent_span_id,trace_source,source,event_name)
             VALUES(?1,NULL,?2,?3,?4,?5,?6,?7,?8,'otel',?9)",
            rusqlite::params![worker, e.prompt_id, e.tool.clone().unwrap_or_else(|| e.name.clone()),
                detail.to_string(), at, tt.trace_id, tt.parent_span_id, tt.source, e.name],
        )?;
        stored += 1;
    }
    Ok((stored, merged, attributed, unattributed))
}

// ── HTTP ────────────────────────────────────────────────────────────────────

fn hdr<'a>(h: &'a HeaderMap, k: &str) -> Option<&'a str> {
    h.get(k).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty())
}

fn refuse(code: StatusCode, verdict: &str, why: &str) -> Response {
    (code, Json(json!({"error": why, "verdict": verdict}))).into_response()
}

/// The worker a post proves it came from, or the refusal.
fn prove(home: &Path, h: &HeaderMap) -> Result<String, Response> {
    if !enabled() {
        counters::bump(&counters::REFUSED_OFF);
        return Err(refuse(StatusCode::SERVICE_UNAVAILABLE, "cli_telemetry_off", "AMUX_CLI_TELEMETRY is off"));
    }
    let lane = hdr(h, "x-amux-session").or_else(|| hdr(h, "x-amux-worker"));
    let token = hdr(h, super::worker_identity::TOKEN_HEADER);
    match lane {
        Some(l) if super::worker_identity::check(home, l, token) == super::worker_identity::Check::Valid => Ok(l.to_string()),
        _ => {
            counters::bump(&counters::REFUSED_UNPROVEN);
            if first_refusal(lane.unwrap_or("")) {
                tracing::warn!(lane = lane.unwrap_or(""), verdict = "cli_telemetry_unproven", measured = true, n_considered = 1,
                    "a telemetry post did not prove a worker with its token; refused");
            }
            Err(refuse(StatusCode::FORBIDDEN, "cli_telemetry_unproven",
                "telemetry is accepted only from a worker proving itself with X-Amux-Worker-Token (set at launch)"))
        }
    }
}

fn first_refusal(lane: &str) -> bool {
    static SEEN: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    SEEN.get_or_init(Default::default).lock().map(|mut s| s.insert(lane.to_string())).unwrap_or(false)
}

fn json_body(h: &HeaderMap, body: &Bytes) -> Result<Value, Response> {
    let ct = hdr(h, "content-type").unwrap_or("application/json");
    if ct.contains("protobuf") {
        counters::bump(&counters::UNSUPPORTED);
        return Err(refuse(StatusCode::UNSUPPORTED_MEDIA_TYPE, "cli_telemetry_protobuf_unsupported",
            "send OTLP as JSON (OTEL_EXPORTER_OTLP_PROTOCOL=http/json)"));
    }
    serde_json::from_slice(body).map_err(|e| {
        counters::bump(&counters::MALFORMED);
        refuse(StatusCode::BAD_REQUEST, "cli_telemetry_malformed", &e.to_string())
    })
}

async fn post_logs(State(state): State<AppState>, h: HeaderMap, body: Bytes) -> Response {
    let home = crate::config::amux_home();
    let worker = match prove(&home, &h) { Ok(w) => w, Err(r) => return r };
    let v = match json_body(&h, &body) { Ok(v) => v, Err(r) => return r };
    counters::bump(&counters::POSTS);
    if let Ok(mut m) = last_seen().lock() {
        m.insert(worker.clone(), chrono::Utc::now().timestamp_millis());
    }
    let (events, n) = parse_logs(&v);
    counters::add(&counters::RECORDS, n as u64);
    if events.is_empty() {
        return Json(json!({})).into_response();
    }
    let w = worker.clone();
    let out = std::sync::Arc::new(Mutex::new((0u64, 0u64, 0u64, 0u64)));
    let out_w = out.clone();
    let res = state
        .store
        .write_async(move |c| {
            let r = store(c, &home, &w, &events)?;
            if let Ok(mut o) = out_w.lock() {
                *o = r;
            }
            Ok(crate::db::WriteOutcome { applied: r.0 + r.1 > 0, events: vec![] })
        })
        .await;
    let counts = out.lock().map(|o| *o).unwrap_or_default();
    match res.map(|_| counts) {
        Ok((stored, merged, attributed, unattributed)) => {
            counters::add(&counters::STORED, stored);
            counters::add(&counters::MERGED, merged);
            counters::add(&counters::ATTRIBUTED, attributed);
            counters::add(&counters::UNATTRIBUTED, unattributed);
        }
        Err(e) => {
            counters::bump(&counters::STORE_FAILED);
            tracing::warn!(worker = %worker, error = %e, verdict = "cli_telemetry_store_failed", measured = true,
                n_considered = 1, "a worker's tool events arrived but could not be stored");
        }
    }
    // OTLP: an empty ExportLogsServiceResponse is full success. The CLI is never
    // made to retry over amux's own storage trouble.
    Json(json!({})).into_response()
}

async fn post_metrics(h: HeaderMap, body: Bytes) -> Response {
    let home = crate::config::amux_home();
    let worker = match prove(&home, &h) { Ok(w) => w, Err(r) => return r };
    if let Err(r) = json_body(&h, &body) {
        return r;
    }
    counters::bump(&counters::METRIC_POSTS);
    if let Ok(mut m) = last_seen().lock() {
        m.insert(worker, chrono::Utc::now().timestamp_millis());
    }
    Json(json!({})).into_response()
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/otel/v1/logs", post(post_logs))
        .route("/api/otel/v1/metrics", post(post_metrics))
}

/// The A5 diagnostic, in `GET /api/contract/counters` as `cli_telemetry`.
/// `telemetry_received_at` per worker is what tells "this worker made no tool
/// calls" from "this worker's CLI is not reporting".
pub fn report() -> Value {
    let posts = counters::get(&counters::POSTS) + counters::get(&counters::METRIC_POSTS);
    let seen: serde_json::Map<String, Value> = last_seen()
        .lock()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), json!(chrono::DateTime::from_timestamp_millis(*v).map(|t| t.to_rfc3339())))).collect())
        .unwrap_or_default();
    json!({
        "enabled": enabled(),
        "measured": true,
        "n_considered": posts,
        "since": "process start",
        "launches_wired": counters::get(&counters::WIRED),
        "launches_user_otel_kept": counters::get(&counters::USER_KEPT),
        "log_posts": counters::get(&counters::POSTS),
        "metric_posts": counters::get(&counters::METRIC_POSTS),
        "records_received": counters::get(&counters::RECORDS),
        "events_stored": counters::get(&counters::STORED),
        "events_merged_with_hook_rows": counters::get(&counters::MERGED),
        "events_attributed_to_a_trace": counters::get(&counters::ATTRIBUTED),
        "events_unattributed": counters::get(&counters::UNATTRIBUTED),
        "refused_unproven": counters::get(&counters::REFUSED_UNPROVEN),
        "refused_off": counters::get(&counters::REFUSED_OFF),
        "refused_protobuf": counters::get(&counters::UNSUPPORTED),
        "malformed": counters::get(&counters::MALFORMED),
        "store_failed": counters::get(&counters::STORE_FAILED),
        "telemetry_received_at": seen,
        "providers": {"claude": "wired at launch", "codex": "not wired: Codex reads OTel from its config file, and its exporter headers would carry the worker token"},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(name: &str, attrs: &[(&str, &str)]) -> Value {
        let mut a: Vec<Value> = attrs.iter().map(|(k, v)| json!({"key": k, "value": {"stringValue": v}})).collect();
        a.push(json!({"key": "event.name", "value": {"stringValue": name}}));
        json!({"timeUnixNano": "1793000000000000000", "body": {"stringValue": format!("claude_code.{name}")}, "attributes": a})
    }

    fn body(records: Vec<Value>) -> Value {
        json!({"resourceLogs": [{"resource": {"attributes": [{"key": "amux.worker", "value": {"stringValue": "w1"}}]},
            "scopeLogs": [{"logRecords": records}]}]})
    }

    fn db() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE _amux_tool_events (id INTEGER PRIMARY KEY AUTOINCREMENT, worker_id TEXT NOT NULL, task_id TEXT,
             turn_id TEXT, tool TEXT NOT NULL, detail TEXT, created_at TEXT NOT NULL, trace_id TEXT, parent_span_id TEXT,
             trace_source TEXT, source TEXT, event_name TEXT);",
        ).unwrap();
        c
    }

    #[test]
    fn a_tool_result_keeps_its_facts_and_never_the_prompt_text_or_account() {
        let b = body(vec![
            rec("user_prompt", &[("prompt.id", "p1"), ("prompt", "secret plan"), ("prompt_length", "11"), ("user.email", "a@b.c")]),
            rec("tool_result", &[("prompt.id", "p1"), ("tool_name", "Bash"), ("tool_use_id", "toolu_1"), ("success", "true"),
                ("duration_ms", "30"), ("tool_input", "{\"command\":\"echo hi\"}"), ("user.email", "a@b.c")]),
            rec("tool_decision", &[("prompt.id", "p1"), ("tool_name", "Bash"), ("tool_use_id", "toolu_1"), ("decision", "accept")]),
            rec("assistant_response", &[("prompt.id", "p1"), ("response", "the answer")]),
            rec("api_request", &[("prompt.id", "p1"), ("model", "claude-haiku-5-5"), ("cost_usd", "0.0023"), ("input_tokens", "2")]),
        ]);
        let (ev, n) = parse_logs(&b);
        assert_eq!(n, 5);
        let names: Vec<&str> = ev.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["user_prompt", "tool_result", "api_request"], "accepted decisions and responses are not kept");
        let all = serde_json::to_string(&ev.iter().map(|e| e.detail.clone()).collect::<Vec<_>>()).unwrap();
        assert!(!all.contains("secret plan") && !all.contains("the answer") && !all.contains("a@b.c"), "{all}");
        assert_eq!(ev[1].detail["ok"], true);
        assert_eq!(ev[1].detail["duration_ms"], 30);
        assert!(ev[1].detail["input"].as_str().unwrap().contains("echo hi"));
        assert_eq!(ev[2].detail["cost_usd"], 0.0023);
        assert_eq!(ev[0].resource_worker.as_deref(), Some("w1"));
    }

    #[test]
    fn a_rejected_decision_is_kept() {
        let (ev, _) = parse_logs(&body(vec![rec("tool_decision", &[("tool_name", "Bash"), ("decision", "reject"), ("source", "hook")])]));
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].detail["decision"], "reject");
    }

    #[test]
    fn a_turn_takes_the_delivery_trace_only_inside_the_window() {
        let file = "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01\nsource=delivered\n";
        let t = correlate(10_000, Some((9_000, file)));
        assert_eq!(t.trace_id.as_deref(), Some("0123456789abcdef0123456789abcdef"));
        assert_eq!(t.parent_span_id.as_deref(), Some("0123456789abcdef"));
        assert_eq!(t.source, "delivered");
        // A file written long before the turn is some earlier delivery's.
        assert_eq!(correlate(10_000 + DELIVERY_WINDOW_MS + 1, Some((9_000, file))).source, UNATTRIBUTED);
        // A file written well after the event is the next delivery's.
        assert_eq!(correlate(1_000, Some((1_000 + DELIVERY_SLACK_MS + 1, file))).trace_id, None);
        assert_eq!(correlate(10_000, None).source, UNATTRIBUTED);
    }

    #[test]
    fn store_attributes_every_event_of_a_turn_and_merges_a_hook_row() {
        let home = tempfile::tempdir().unwrap();
        let f = super::super::trace::worker_file(home.path(), "w1").unwrap();
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01\nsource=delivered\n").unwrap();
        let c = db();
        c.execute("INSERT INTO _amux_tool_events(worker_id,tool,detail,created_at) VALUES('w1','Bash','{\"tool_use_id\":\"toolu_9\"}','x')", []).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let ev = |name: &str, tool_use: Option<&str>| Event {
            name: name.into(), ts_ms: now, prompt_id: Some("p1".into()), session_id: Some("s".into()),
            resource_worker: Some("w1".into()), tool: Some("Bash".into()), tool_use_id: tool_use.map(str::to_string),
            detail: json!({"source": "otel", "tool_use_id": tool_use}),
        };
        let (stored, merged, attributed, unattributed) =
            store(&c, home.path(), "w1", &[ev("user_prompt", None), ev("tool_result", Some("toolu_1")), ev("tool_result", Some("toolu_9"))]).unwrap();
        assert_eq!((stored, merged, attributed, unattributed), (2, 1, 3, 0));
        let traced: i64 = c.query_row("SELECT COUNT(*) FROM _amux_tool_events WHERE trace_id='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'", [], |r| r.get(0)).unwrap();
        assert_eq!(traced, 3, "both new rows and the hook row carry the turn's trace");
        // The same tool call posted again is not a second row.
        let again = store(&c, home.path(), "w1", &[ev("tool_result", Some("toolu_1"))]).unwrap();
        assert_eq!(again.0, 0);
        // A later post for the same turn keeps the turn's trace even after the
        // file moves on to another delivery.
        std::fs::write(&f, "00-cccccccccccccccccccccccccccccccc-dddddddddddddddd-01\n").unwrap();
        store(&c, home.path(), "w1", &[ev("api_request", None)]).unwrap();
        let last: String = c.query_row("SELECT trace_id FROM _amux_tool_events ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(last, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        // An API request the exporter re-sends (same request_id) is one row.
        let api = Event { name: "api_request".into(), tool: Some("api_request".into()), tool_use_id: None,
            detail: json!({"source": "otel", "request_id": "req_1"}), ..ev("api_request", None) };
        assert_eq!(store(&c, home.path(), "w1", &[api.clone()]).unwrap().0, 1);
        assert_eq!(store(&c, home.path(), "w1", &[api]).unwrap().0, 0, "a re-sent API request is not a second row");
    }

    #[test]
    fn launch_wires_a_claude_lane_and_leaves_an_owners_otel_alone() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("tls")).unwrap();
        std::fs::write(home.path().join("tls/cert.pem"), "x").unwrap();
        let none = |_: &str| None;
        let Plan::Wire { vars } = plan(home.path(), "w1", "claude", "customers, gtm", 8824, true, &none) else { panic!() };
        let get = |k: &str| vars.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
        assert_eq!(get("CLAUDE_CODE_ENABLE_TELEMETRY").as_deref(), Some("1"));
        assert_eq!(get("OTEL_EXPORTER_OTLP_ENDPOINT").as_deref(), Some("https://localhost:8824/api/otel"));
        assert_eq!(get("OTEL_RESOURCE_ATTRIBUTES").as_deref(), Some("amux.worker=w1,amux.group=customers|gtm"));
        assert!(get("NODE_EXTRA_CA_CERTS").unwrap().ends_with("tls/cert.pem"));
        assert!(vars.iter().all(|(k, _)| k != HEADERS_ENV), "the headers carry the token and never go on the command line");
        let owns = |k: &str| (k == "OTEL_EXPORTER_OTLP_ENDPOINT").then(|| "http://collector:4318".to_string());
        assert_eq!(plan(home.path(), "w1", "claude", "", 8824, true, &owns), Plan::UserOwns("OTEL_EXPORTER_OTLP_ENDPOINT".into()));
        assert_eq!(plan(home.path(), "w1", "codex", "", 8824, true, &none), Plan::NotSupported);
        assert_eq!(plan(home.path(), "w1", "claude", "", 8824, false, &none), Plan::Off);
        assert!(headers_value("w1", "tok").contains("x-amux-worker-token=tok"));
    }

    #[test]
    fn the_switch_reads_off_only_when_it_says_so() {
        assert!(enabled_from(None));
        assert!(enabled_from(Some("on")));
        assert!(!enabled_from(Some("off")));
        assert!(!enabled_from(Some("0")));
    }
}
