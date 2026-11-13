//! What an auditor, a lawyer and an incident need from the ledger (governance
//! PR 19; plan 4.4 and phase 4). Nothing here changes what a worker may do
//! day to day: each power acts only when a person uses it (ethos 8).
//!
//! | need | how | who |
//! |---|---|---|
//! | read the record | `GET /api/audit/export?format=csv|ocsf|json` over `_amux_ledger`, filtered by time, actor and worker, every row marked with whether the chain verified it | owner, auditor, admin |
//! | keep it past retention | `POST /api/audit/holds`: a legal hold; a matching row cannot be deleted, so the retention prune stops at it | owner |
//! | a person's own data | `GET /api/audit/person?subject=<email>`: everything amux keeps that names them | owner, auditor, admin, the person |
//! | forget a person | `POST /api/audit/person/erase`: their name becomes `erased:<id>` in the ledger, which keeps every hash; refused under a hold | owner |
//! | stop everything now | `POST /api/audit/kill-switch`: no worker, app or policy publishes, sends email, mints a connector token or pushes until it is lifted | owner |
//!
//! The auditor is a member role (`org_members.role = 'auditor'`): reads these
//! and the ledger, traces, approvals and the at-rest report, and cannot act
//! (`org::authorize_local_member_request`).
//!
//! Every placement, lift, erasure, export and kill-switch change is a ledger
//! row of kind `governance`, so the record says who used each power (A5).
//!
//! Switches (A11). These powers are new and act only when the owner invokes
//! them, so there is nothing to report before enforcing; each has a way out:
//! `AMUX_KILL_SWITCH=off` makes the switch inert (an engaged one stops
//! refusing; engaging is refused), a hold is lifted, and an erasure is
//! dry-run first (`dry_run: true`).

use super::ledger::{self, Event, Principal};
use super::AppState;
use crate::db::WriteOutcome;
use axum::{
    body::Body,
    extract::{Path as AxPath, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const KILL_ENV: &str = "AMUX_KILL_SWITCH";
const EXPORT_MAX: i64 = 200_000;

/// Mounted under `/api/audit` beside the per-worker timeline (audit.rs).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/export", get(export))
        .route("/holds", get(list_holds).post(place_hold))
        .route("/holds/{id}/lift", post(lift_hold))
        .route("/person", get(person_export))
        .route("/person/erase", post(person_erase))
        .route("/kill-switch", get(kill_view).post(kill_set))
}

pub(crate) mod counters {
    use super::*;
    pub static EXPORTS: AtomicU64 = AtomicU64::new(0);
    pub static EXPORTED_ROWS: AtomicU64 = AtomicU64::new(0);
    pub static HOLDS_PLACED: AtomicU64 = AtomicU64::new(0);
    pub static HOLDS_LIFTED: AtomicU64 = AtomicU64::new(0);
    pub static ERASURES: AtomicU64 = AtomicU64::new(0);
    pub static ERASED_ROWS: AtomicU64 = AtomicU64::new(0);
    pub static ERASE_HELD: AtomicU64 = AtomicU64::new(0);
    pub static KILL_REFUSED: AtomicU64 = AtomicU64::new(0);
    pub static AUDITOR_REFUSED: AtomicU64 = AtomicU64::new(0);
    pub static REFUSED_NOT_OWNER: AtomicU64 = AtomicU64::new(0);
    pub fn bump(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Ordering::Relaxed);
    }
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    if getrandom::fill(&mut b).is_err() {
        // Unique enough for an id; never used as a secret.
        return format!("{:x}", now_ms());
    }
    hex::encode(b)
}

fn refuse(status: StatusCode, code: &str, error: &str) -> Response {
    (status, Json(json!({"ok": false, "code": code, "error": error}))).into_response()
}

// ── Who ─────────────────────────────────────────────────────────────────────

fn is_owner(h: &HeaderMap) -> bool {
    matches!(super::human_proof::proven(h), Some(super::human_proof::Proven::Owner))
}

/// The owner, an auditor or an admin member may read the record.
pub fn can_read(h: &HeaderMap) -> bool {
    is_owner(h) || super::org::local_member_is_auditor(h) || super::org::local_member_is_admin(h)
}

fn owner_only(h: &HeaderMap, what: &str) -> Option<Response> {
    if is_owner(h) {
        return None;
    }
    counters::bump(&counters::REFUSED_NOT_OWNER, 1);
    Some(refuse(StatusCode::FORBIDDEN, "owner_only", &format!("{what} is the owner's alone")))
}

fn principal(h: &HeaderMap) -> Principal {
    ledger::principal(&crate::config::amux_home(), h)
}

/// One `governance` row in the ledger, appended in the caller's write.
fn record(conn: &Connection, p: &Principal, action: &str, resource: &str, decision: &str, output_hash: Option<String>) -> String {
    let id = format!("gov:{action}:{}", rand_hex(8));
    ledger::append(conn, &Event {
        event_id: id.clone(),
        ts_ms: now_ms(),
        kind: "governance".into(),
        action: Some(action.into()),
        resource: Some(resource.into()),
        decision: Some(decision.into()),
        outcome: Some("applied".into()),
        output_hash,
        ..Default::default()
    }.with_principal(p));
    id
}

/// Run a write that answers with a value, not only a WriteOutcome.
async fn write_value<F>(state: &AppState, f: F) -> Result<Value, String>
where
    F: FnOnce(&Connection) -> rusqlite::Result<Value> + Send + 'static,
{
    let slot: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let s2 = slot.clone();
    state
        .store
        .write_async(move |conn| {
            let v = f(conn)?;
            let applied = v["ok"] == json!(true);
            *s2.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
            Ok(WriteOutcome { applied, events: vec![] })
        })
        .await
        .map_err(|e| e.to_string())?;
    let v = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    v.ok_or_else(|| "write returned nothing".to_string())
}

// ── Export ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default, Clone)]
pub struct ExportQuery {
    /// `csv` (default), `ocsf` (JSON lines, OCSF API Activity) or `json`.
    pub format: Option<String>,
    /// Epoch milliseconds, inclusive; or hours back with `since_h`.
    pub since: Option<i64>,
    pub since_h: Option<f64>,
    pub until: Option<i64>,
    pub actor: Option<String>,
    pub worker: Option<String>,
    pub limit: Option<i64>,
}

/// The rows an export covers, oldest first, and the chain's verdict on them.
pub fn export_rows(conn: &Connection, q: &ExportQuery) -> rusqlite::Result<(Vec<Value>, Value)> {
    let _ = ledger::seal(conn);
    let (verify, _, walked) = ledger::verify_capturing(conn, &Default::default());
    let chain_ok = verify["chain_ok"] == json!(true);
    let since = q.since.unwrap_or_else(|| {
        q.since_h
            .filter(|h| h.is_finite() && *h > 0.0)
            .map(|h| now_ms() - (h * 3_600_000.0) as i64)
            .unwrap_or(0)
    });
    let until = q.until.unwrap_or(i64::MAX);
    let limit = q.limit.unwrap_or(EXPORT_MAX).clamp(1, EXPORT_MAX);
    let worker = q.worker.clone().unwrap_or_default();
    let mut st = conn.prepare(
        "SELECT seq, event_id, ts, kind, actor, claimed, on_behalf_of, authority, policy_version, action,
                resource, decision, outcome, trace_id, span_id, parent_span_id, cost_microusd,
                prev_hash, hash, redacted_at
           FROM _amux_ledger
          WHERE ts >= ?1 AND ts <= ?2
            AND (?3 = '' OR actor = ?3 OR claimed = ?3 OR on_behalf_of = ?3)
            AND (?4 = '' OR actor = 'worker:' || ?4 OR claimed = 'worker:' || ?4
                 OR resource = 'worker:' || ?4 OR resource LIKE 'worker:' || ?4 || '/%')
          ORDER BY seq LIMIT ?5",
    )?;
    let rows = st
        .query_map(rusqlite::params![since, until, q.actor.clone().unwrap_or_default(), worker, limit], |r| {
            let seq: i64 = r.get(0)?;
            let ts: i64 = r.get(2)?;
            let hash: Option<String> = r.get(18)?;
            // A row is verified when the chain walk reached it intact.
            let verified = hash.is_some() && seq <= walked;
            Ok(json!({
                "seq": seq, "event_id": r.get::<_, String>(1)?, "ts": ts,
                "at": chrono::DateTime::from_timestamp_millis(ts).map(|t| t.to_rfc3339()),
                "kind": r.get::<_, String>(3)?, "actor": r.get::<_, String>(4)?,
                "claimed": r.get::<_, Option<String>>(5)?, "on_behalf_of": r.get::<_, Option<String>>(6)?,
                "authority": r.get::<_, Option<String>>(7)?, "policy_version": r.get::<_, Option<String>>(8)?,
                "action": r.get::<_, Option<String>>(9)?, "resource": r.get::<_, Option<String>>(10)?,
                "decision": r.get::<_, Option<String>>(11)?, "outcome": r.get::<_, Option<String>>(12)?,
                "trace_id": r.get::<_, Option<String>>(13)?, "span_id": r.get::<_, Option<String>>(14)?,
                "parent_span_id": r.get::<_, Option<String>>(15)?, "cost_microusd": r.get::<_, Option<i64>>(16)?,
                "prev_hash": r.get::<_, Option<String>>(17)?, "hash": hash,
                "redacted": r.get::<_, Option<i64>>(19)?.is_some(),
                "chain_verified": verified,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let summary = json!({
        "chain_ok": chain_ok,
        "verified_upto_seq": walked,
        "first_bad": verify["first_bad"],
        "rows": rows.len(),
        "filter": {"since": since, "until": q.until, "actor": q.actor, "worker": q.worker},
        "measured": verify["measured"],
        "n_considered": verify["n_considered"],
    });
    Ok((rows, summary))
}

const CSV_COLS: &[&str] = &[
    "seq", "event_id", "ts", "at", "kind", "actor", "claimed", "on_behalf_of", "authority", "policy_version",
    "action", "resource", "decision", "outcome", "trace_id", "span_id", "parent_span_id", "cost_microusd",
    "prev_hash", "hash", "redacted", "chain_verified",
];

fn csv_cell(v: &Value) -> String {
    let s = match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    // A cell a spreadsheet would run as a formula is quoted with a leading '.
    let s = if s.starts_with(['=', '+', '-', '@']) && s.parse::<f64>().is_err() { format!("'{s}") } else { s };
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

pub fn to_csv(rows: &[Value]) -> String {
    let mut out = CSV_COLS.join(",");
    out.push('\n');
    for r in rows {
        let line: Vec<String> = CSV_COLS.iter().map(|c| csv_cell(&r[*c])).collect();
        out.push_str(&line.join(","));
        out.push('\n');
    }
    out
}

/// One OCSF 1.1 API Activity (class 6003) event per row. Fields OCSF has no
/// place for go under `unmapped.amux`, so nothing is lost.
pub fn to_ocsf(row: &Value) -> Value {
    let outcome = row["outcome"].as_str().unwrap_or("");
    let decision = row["decision"].as_str().unwrap_or("");
    let failed = matches!(decision, "deny" | "refused") || outcome.starts_with("refused") || outcome == "failed";
    json!({
        "class_uid": 6003, "class_name": "API Activity",
        "category_uid": 6, "category_name": "Application Activity",
        "type_uid": 600399, "activity_id": 99, "activity_name": row["action"],
        "time": row["ts"], "severity_id": 1, "severity": "Informational",
        "status_id": if failed { 2 } else { 1 }, "status": if failed { "Failure" } else { "Success" },
        "actor": {"user": {"name": row["actor"]}, "invoked_by": row["on_behalf_of"]},
        "api": {"operation": row["action"]},
        "resources": [{"name": row["resource"]}],
        "metadata": {"version": "1.1.0", "uid": row["event_id"], "correlation_uid": row["trace_id"],
                     "product": {"name": "amux", "vendor_name": "amux"}},
        "unmapped": {"amux": {
            "seq": row["seq"], "kind": row["kind"], "claimed": row["claimed"], "authority": row["authority"],
            "decision": row["decision"], "outcome": row["outcome"], "span_id": row["span_id"],
            "parent_span_id": row["parent_span_id"], "prev_hash": row["prev_hash"], "hash": row["hash"],
            "redacted": row["redacted"], "chain_verified": row["chain_verified"]}},
    })
}

async fn export(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<ExportQuery>) -> Response {
    if !can_read(&headers) {
        return refuse(StatusCode::FORBIDDEN, "audit_read_refused", "the owner, an auditor or an admin reads the audit record");
    }
    let p = principal(&headers);
    let q2 = q.clone();
    let res = write_value(&state, move |conn| {
        let (rows, summary) = export_rows(conn, &q2)?;
        let filter = serde_json::to_string(&summary["filter"]).unwrap_or_default();
        record(conn, &p, "audit.export", &format!("ledger?{filter}"), "allow", Some(ledger::sha256_hex(filter.as_bytes())));
        Ok(json!({"ok": true, "rows": rows, "summary": summary}))
    })
    .await;
    let v = match res {
        Ok(v) => v,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ok": false, "measured": false,
            "n_considered": 0, "why_unmeasured": e}))).into_response(),
    };
    let rows = v["rows"].as_array().cloned().unwrap_or_default();
    let summary = v["summary"].clone();
    counters::bump(&counters::EXPORTS, 1);
    counters::bump(&counters::EXPORTED_ROWS, rows.len() as u64);
    let (body, ctype, ext) = match q.format.as_deref().unwrap_or("csv") {
        "json" => (serde_json::to_string(&json!({"summary": summary, "events": rows})).unwrap_or_default(),
                   "application/json", "json"),
        "ocsf" | "jsonl" | "siem" => (rows.iter().map(|r| to_ocsf(r).to_string() + "\n").collect::<String>(),
                   "application/x-ndjson", "jsonl"),
        _ => (to_csv(&rows), "text/csv; charset=utf-8", "csv"),
    };
    let mut resp = Response::new(Body::from(body));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"amux-ledger.{ext}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    // The verdict travels with every format, so a CSV opened alone still says
    // whether the chain held (ethos 4).
    let put = |h: &mut HeaderMap, k: &'static str, v: String| {
        if let Ok(v) = HeaderValue::from_str(&v) {
            h.insert(k, v);
        }
    };
    put(h, "x-amux-ledger-chain-ok", summary["chain_ok"].to_string());
    put(h, "x-amux-ledger-verified-upto", summary["verified_upto_seq"].to_string());
    put(h, "x-amux-ledger-rows", rows.len().to_string());
    put(h, "x-amux-ledger-measured", summary["measured"].to_string());
    resp
}

// ── Legal hold ──────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct HoldBody {
    pub name: Option<String>,
    pub actor: Option<String>,
    pub worker: Option<String>,
    pub from: Option<i64>,
    pub to: Option<i64>,
    /// A hold over the whole ledger must say so.
    #[serde(default)]
    pub all: bool,
}

fn clean(s: &Option<String>) -> Option<String> {
    s.as_ref().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

pub fn place(conn: &Connection, b: &HoldBody, p: &Principal) -> rusqlite::Result<Value> {
    let (actor, worker) = (clean(&b.actor), clean(&b.worker));
    if actor.is_none() && worker.is_none() && b.from.is_none() && b.to.is_none() && !b.all {
        return Ok(json!({"ok": false, "code": "hold_scope_missing",
            "error": "name an actor, a worker or a time range, or say all: true"}));
    }
    let id = format!("hold_{}", rand_hex(6));
    let name = clean(&b.name).unwrap_or_else(|| id.clone());
    conn.execute(
        "INSERT INTO _amux_ledger_holds (id, name, actor, worker, from_ts, to_ts, placed_by, placed_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        rusqlite::params![id, name, actor, worker, b.from, b.to, p.actor, now_ms()],
    )?;
    record(conn, p, "legal_hold.place", &format!("hold:{id}"), "allow", None);
    counters::bump(&counters::HOLDS_PLACED, 1);
    let held = held_count(conn, &id)?;
    tracing::warn!(hold = %id, held_rows = held, verdict = "legal_hold_placed", measured = true, n_considered = held,
        "a legal hold now keeps matching ledger rows past retention");
    Ok(json!({"ok": true, "id": id, "name": name, "held_rows": held}))
}

pub fn lift(conn: &Connection, id: &str, p: &Principal) -> rusqlite::Result<Value> {
    let n = conn.execute(
        "UPDATE _amux_ledger_holds SET lifted_at=?2, lifted_by=?3 WHERE id=?1 AND lifted_at IS NULL",
        rusqlite::params![id, now_ms(), p.actor],
    )?;
    if n == 0 {
        return Ok(json!({"ok": false, "code": "hold_not_active", "error": "no active hold with that id"}));
    }
    record(conn, p, "legal_hold.lift", &format!("hold:{id}"), "allow", None);
    counters::bump(&counters::HOLDS_LIFTED, 1);
    tracing::warn!(hold = %id, verdict = "legal_hold_lifted", measured = true, n_considered = 1, "a legal hold was lifted");
    Ok(json!({"ok": true, "id": id}))
}

fn held_count(conn: &Connection, id: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM _amux_ledger l JOIN _amux_ledger_holds h ON h.id = ?1 WHERE {}", ledger::HOLD_MATCH),
        [id],
        |r| r.get(0),
    )
}

pub fn holds(conn: &Connection) -> rusqlite::Result<Value> {
    let mut st = conn.prepare(
        "SELECT id, name, actor, worker, from_ts, to_ts, placed_by, placed_at, lifted_by, lifted_at
           FROM _amux_ledger_holds ORDER BY placed_at DESC",
    )?;
    let mut items: Vec<Value> = st
        .query_map([], |r| {
            Ok(json!({"id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?,
                "actor": r.get::<_, Option<String>>(2)?, "worker": r.get::<_, Option<String>>(3)?,
                "from": r.get::<_, Option<i64>>(4)?, "to": r.get::<_, Option<i64>>(5)?,
                "placed_by": r.get::<_, String>(6)?, "placed_at": r.get::<_, i64>(7)?,
                "lifted_by": r.get::<_, Option<String>>(8)?, "lifted_at": r.get::<_, Option<i64>>(9)?}))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for h in items.iter_mut() {
        let active = h["lifted_at"].is_null();
        h["active"] = json!(active);
        if active {
            let id = h["id"].as_str().unwrap_or_default().to_string();
            h["held_rows"] = json!(held_count(conn, &id)?);
        }
    }
    let active = items.iter().filter(|h| h["active"] == json!(true)).count();
    Ok(json!({"holds": items, "active": active, "measured": true, "n_considered": items.len()}))
}

async fn list_holds(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !can_read(&headers) {
        return refuse(StatusCode::FORBIDDEN, "audit_read_refused", "the owner, an auditor or an admin reads legal holds");
    }
    match state.store.read_async(|c| Ok(holds(c)?)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"measured": false, "n_considered": 0,
            "why_unmeasured": e.to_string()}))).into_response(),
    }
}

fn answer(res: Result<Value, String>, bad: StatusCode) -> Response {
    match res {
        Ok(v) if v["ok"] == json!(true) => Json(v).into_response(),
        Ok(v) => (bad, Json(v)).into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ok": false, "error": e}))).into_response(),
    }
}

async fn place_hold(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<HoldBody>) -> Response {
    if let Some(r) = owner_only(&headers, "placing a legal hold") {
        return r;
    }
    let p = principal(&headers);
    answer(write_value(&state, move |c| place(c, &b, &p)).await, StatusCode::BAD_REQUEST)
}

async fn lift_hold(State(state): State<AppState>, headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    if let Some(r) = owner_only(&headers, "lifting a legal hold") {
        return r;
    }
    let p = principal(&headers);
    answer(write_value(&state, move |c| lift(c, &id, &p)).await, StatusCode::NOT_FOUND)
}

// ── A person's data: export and erase ───────────────────────────────────────

/// A person, as the ledger names them (`member:<email>`), and the email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub actor: String,
    pub email: String,
}

pub fn subject(raw: &str) -> Option<Subject> {
    let s = raw.trim();
    let email = s.strip_prefix("member:").unwrap_or(s).trim().to_ascii_lowercase();
    // A person is an email. `owner`, a worker or an app is not erasable here.
    let ok = email.len() >= 3 && email.contains('@') && !email.starts_with('@') && !email.ends_with('@')
        && !email.contains(char::is_whitespace);
    ok.then(|| Subject { actor: format!("member:{email}"), email })
}

/// Rows naming the person in any of the four personal columns.
const PERSON_MATCH: &str = "(instr(lower(COALESCE(actor,'')), ?1) > 0 OR instr(lower(COALESCE(claimed,'')), ?1) > 0
    OR instr(lower(COALESCE(on_behalf_of,'')), ?1) > 0 OR instr(lower(COALESCE(resource,'')), ?1) > 0)";

pub fn person(conn: &Connection, s: &Subject) -> rusqlite::Result<Value> {
    let _ = ledger::seal(conn);
    let ledger_rows: Vec<Value> = {
        let mut st = conn.prepare(&format!(
            "SELECT seq, event_id, ts, kind, actor, claimed, on_behalf_of, action, resource, decision, outcome, trace_id
               FROM _amux_ledger WHERE {PERSON_MATCH} ORDER BY seq"))?;
        let it = st.query_map([&s.email], |r| Ok(json!({
            "seq": r.get::<_, i64>(0)?, "event_id": r.get::<_, String>(1)?, "ts": r.get::<_, i64>(2)?,
            "kind": r.get::<_, String>(3)?, "actor": r.get::<_, String>(4)?, "claimed": r.get::<_, Option<String>>(5)?,
            "on_behalf_of": r.get::<_, Option<String>>(6)?, "action": r.get::<_, Option<String>>(7)?,
            "resource": r.get::<_, Option<String>>(8)?, "decision": r.get::<_, Option<String>>(9)?,
            "outcome": r.get::<_, Option<String>>(10)?, "trace_id": r.get::<_, Option<String>>(11)?})))?;
        it.collect::<rusqlite::Result<_>>()?
    };
    let interactions: Vec<Value> = {
        let mut st = conn.prepare(
            "SELECT id, method, path, actor, status, created_at FROM _amux_interactions
              WHERE instr(lower(actor), ?1) > 0 ORDER BY created_at")?;
        let it = st.query_map([&s.email], |r| Ok(json!({"id": r.get::<_, String>(0)?, "method": r.get::<_, String>(1)?,
            "path": r.get::<_, String>(2)?, "actor": r.get::<_, String>(3)?, "status": r.get::<_, Option<i64>>(4)?,
            "created_at": r.get::<_, i64>(5)?})))?;
        it.collect::<rusqlite::Result<_>>()?
    };
    let receipts: Vec<Value> = {
        let mut st = conn.prepare(
            "SELECT id, actor, action, resource, effect, created_at FROM _amux_policy_receipts
              WHERE instr(lower(actor), ?1) > 0 OR instr(lower(COALESCE(rationale,'')), ?1) > 0 ORDER BY created_at")?;
        let it = st.query_map([&s.email], |r| Ok(json!({"id": r.get::<_, String>(0)?, "actor": r.get::<_, String>(1)?,
            "action": r.get::<_, Option<String>>(2)?, "resource": r.get::<_, Option<String>>(3)?,
            "effect": r.get::<_, Option<String>>(4)?, "created_at": r.get::<_, Option<String>>(5)?})))?;
        it.collect::<rusqlite::Result<_>>()?
    };
    let member: Option<Value> = conn
        .query_row(
            "SELECT id, email, name, role, CAST(joined_at AS TEXT), offboarded_at FROM org_members WHERE lower(email)=?1",
            [&s.email],
            |r| Ok(json!({"id": r.get::<_, String>(0)?, "email": r.get::<_, String>(1)?,
                "name": r.get::<_, Option<String>>(2)?, "role": r.get::<_, Option<String>>(3)?,
                "joined_at": r.get::<_, Option<String>>(4)?, "offboarded_at": r.get::<_, Option<i64>>(5)?})),
        )
        .optional()?;
    let n = ledger_rows.len() + interactions.len() + receipts.len() + member.is_some() as usize;
    Ok(json!({
        "subject": s.actor,
        "ledger": ledger_rows,
        "interactions": interactions,
        "policy_receipts": receipts,
        "org_member": member,
        "not_included": not_kept_here(),
        "measured": true,
        "n_considered": n,
    }))
}

/// What amux keeps about a person that this export and erasure do not reach,
/// named so neither is mistaken for complete (ethos 1).
fn not_kept_here() -> Value {
    json!([
        "messages they wrote (Messages posts) and files they uploaded",
        "worker terminal transcripts and agent CLI session files",
        "the request log (_amux_request_log, 14 days) and server logs under logs/",
        "their org_members row: remove it with offboarding (POST /api/org/members/<id>/offboard)",
    ])
}

/// Erase a person from the ledger and the working trails. Refused, changing
/// nothing, when any matching ledger row is under an active legal hold.
pub fn erase(conn: &Connection, s: &Subject, p: &Principal, dry_run: bool) -> rusqlite::Result<Value> {
    let _ = ledger::seal(conn);
    let seqs: Vec<i64> = {
        let mut st = conn.prepare(&format!(
            "SELECT seq FROM _amux_ledger WHERE redacted_at IS NULL AND {PERSON_MATCH} ORDER BY seq"))?;
        let it = st.query_map([&s.email], |r| r.get(0))?;
        it.collect::<rusqlite::Result<_>>()?
    };
    let held: Vec<String> = {
        let mut st = conn.prepare(&format!(
            "SELECT DISTINCT h.id FROM _amux_ledger l JOIN _amux_ledger_holds h
              WHERE l.redacted_at IS NULL AND {} AND {}",
            PERSON_MATCH.replace("COALESCE(", "COALESCE(l."), ledger::HOLD_MATCH))?;
        let it = st.query_map([&s.email], |r| r.get(0))?;
        it.collect::<rusqlite::Result<_>>()?
    };
    let interactions: i64 = conn.query_row(
        "SELECT COUNT(*) FROM _amux_interactions WHERE instr(lower(actor), ?1) > 0", [&s.email], |r| r.get(0))?;
    if !held.is_empty() {
        counters::bump(&counters::ERASE_HELD, 1);
        tracing::warn!(holds = ?held, rows = seqs.len(), verdict = "erase_refused_legal_hold", measured = true,
            n_considered = seqs.len(), "an erasure was refused: the person's rows are under legal hold");
        return Ok(json!({"ok": false, "code": "legal_hold", "holds": held, "ledger_rows": seqs.len(),
            "error": "rows naming this person are under legal hold; lift the hold first"}));
    }
    if dry_run {
        return Ok(json!({"ok": true, "dry_run": true, "subject": s.actor, "ledger_rows": seqs.len(),
            "interactions": interactions, "not_erased": not_kept_here()}));
    }
    let id = format!("era_{}", rand_hex(6));
    let token = format!("erased:{id}");
    let now = now_ms();
    for seq in &seqs {
        conn.execute("INSERT INTO _amux_ledger_erase_gate (seq) VALUES (?1)", [seq])?;
    }
    let redact = |col: &str| format!("{col} = CASE WHEN instr(lower(COALESCE({col},'')), ?2) > 0 THEN ?3 ELSE {col} END");
    let sql = format!(
        "UPDATE _amux_ledger SET {}, {}, {}, {}, redacted_at = ?4 WHERE seq = ?1",
        redact("actor"), redact("claimed"), redact("on_behalf_of"), redact("resource"));
    let res: rusqlite::Result<()> = (|| {
        for seq in &seqs {
            conn.execute(&sql, rusqlite::params![seq, s.email, token, now])?;
            conn.execute("INSERT INTO _amux_ledger_redactions (seq, erasure_id) VALUES (?1, ?2)", rusqlite::params![seq, id])?;
        }
        Ok(())
    })();
    conn.execute("DELETE FROM _amux_ledger_erase_gate", [])?;
    res?;
    let (digest, n) = ledger::redaction_digest(conn, &id)?;
    let event_id = record(conn, p, "person.erase", &format!("erasure:{id}"), "allow", Some(digest.clone()));
    conn.execute(
        "INSERT INTO _amux_ledger_erasures (id, at, by_actor, rows, digest, ledger_event_id) VALUES (?1,?2,?3,?4,?5,?6)",
        rusqlite::params![id, now, p.actor, n as i64, digest, event_id],
    )?;
    // The working trails: interaction receipts are working state (14 days),
    // deleted; policy receipts keep their row with the name replaced.
    conn.execute("DELETE FROM _amux_interaction_effects WHERE interaction_id IN
        (SELECT id FROM _amux_interactions WHERE instr(lower(actor), ?1) > 0)", [&s.email])?;
    let gone = conn.execute("DELETE FROM _amux_interactions WHERE instr(lower(actor), ?1) > 0", [&s.email])?;
    let receipts = conn.execute(
        "UPDATE _amux_policy_receipts SET actor = CASE WHEN instr(lower(actor), ?1) > 0 THEN ?2 ELSE actor END,
                rationale = replace(replace(rationale, ?3, ?2), ?1, ?2)
          WHERE instr(lower(actor), ?1) > 0 OR instr(lower(COALESCE(rationale,'')), ?1) > 0",
        rusqlite::params![s.email, token, s.actor],
    )?;
    counters::bump(&counters::ERASURES, 1);
    counters::bump(&counters::ERASED_ROWS, n as u64);
    tracing::warn!(erasure = %id, ledger_rows = n, interactions = gone, receipts, verdict = "person_erased",
        measured = true, n_considered = n, "a person's name was erased from the ledger; hashes kept");
    Ok(json!({"ok": true, "erasure_id": id, "token": token, "ledger_rows": n, "interactions_deleted": gone,
        "policy_receipts_redacted": receipts, "digest": digest, "ledger_event_id": event_id,
        "not_erased": not_kept_here()}))
}

#[derive(Deserialize)]
struct PersonQuery {
    subject: String,
}

async fn person_export(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<PersonQuery>) -> Response {
    let Some(s) = subject(&q.subject) else {
        return refuse(StatusCode::BAD_REQUEST, "subject_invalid", "subject is a person's email (or member:<email>)");
    };
    // A member may always read what amux keeps about themselves.
    let own = super::org::local_member_actor(&headers).is_some_and(|a| a.eq_ignore_ascii_case(&s.actor));
    if !own && !can_read(&headers) {
        return refuse(StatusCode::FORBIDDEN, "audit_read_refused", "the owner, an auditor, an admin or the person reads this");
    }
    let p = principal(&headers);
    answer(write_value(&state, move |c| {
        let mut v = person(c, &s)?;
        record(c, &p, "person.export", &format!("subject:{}", ledger::sha256_hex(s.email.as_bytes())), "allow", None);
        v["ok"] = json!(true);
        Ok(v)
    }).await, StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Deserialize)]
struct EraseBody {
    subject: String,
    #[serde(default)]
    dry_run: bool,
}

async fn person_erase(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<EraseBody>) -> Response {
    if let Some(r) = owner_only(&headers, "erasing a person") {
        return r;
    }
    let Some(s) = subject(&b.subject) else {
        return refuse(StatusCode::BAD_REQUEST, "subject_invalid", "subject is a person's email (or member:<email>)");
    };
    let p = principal(&headers);
    answer(write_value(&state, move |c| erase(c, &s, &p, b.dry_run)).await, StatusCode::CONFLICT)
}

// ── Kill switch ─────────────────────────────────────────────────────────────

pub fn kill_enabled() -> bool {
    !matches!(
        std::env::var(KILL_ENV).ok().map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("off" | "0" | "false" | "no")
    )
}

/// The switch lives in a file so the git pre-push hook, which runs without the
/// server, reads the same fact (`scripts/git-hooks/pre-push`).
pub fn kill_path(home: &Path) -> PathBuf {
    home.join("governance").join("kill-switch.json")
}

/// The engaged switch, if it is. An unreadable file reads as engaged: a stop
/// the owner asked for must not be lifted by a corrupt file (fail closed).
pub fn kill_state(home: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(kill_path(home)).ok()?;
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) if v["engaged"] == json!(true) => Some(v),
        Ok(_) => None,
        Err(_) => Some(json!({"engaged": true, "reason": "kill-switch file unreadable; treated as engaged"})),
    }
}

/// Which outbound action a request is, if it is one the switch stops.
pub fn outbound_kind(method: &Method, path: &str) -> Option<&'static str> {
    if *method != Method::POST {
        return None;
    }
    if path == "/api/page-links" || path == "/api/page-links/site-view"
        || (path.starts_with("/api/page-links/")
            && (path.ends_with("/approve") || path.ends_with("/api") || path.ends_with("/app-token") || path.ends_with("/open")))
    {
        return Some("publish");
    }
    if path == "/api/email/send" || path == "/api/email/reply" || path.starts_with("/api/email/approve/") {
        return Some("email_send");
    }
    if path.starts_with("/api/connectors/") && path.ends_with("/token") {
        return Some("connector_mint");
    }
    if path.starts_with("/api/social/") && (path.ends_with("/publish") || path.ends_with("/post") || path.contains("/queue")) {
        return Some("social_publish");
    }
    None
}

/// The refusal for an outbound action while the switch is engaged, for any
/// caller that is not a person. The owner and members keep acting: the switch
/// stops automation, not the people who would clean up after it.
pub fn kill_refusal(kind: &str, headers: &HeaderMap) -> Option<Response> {
    if !kill_enabled() {
        return None;
    }
    let st = kill_state(&crate::config::amux_home())?;
    if matches!(super::human_proof::proven(headers),
        Some(super::human_proof::Proven::Owner | super::human_proof::Proven::Member(_)))
    {
        return None;
    }
    counters::bump(&counters::KILL_REFUSED, 1);
    let who = principal(headers);
    tracing::warn!(kind, actor = %who.actor, claimed = ?who.claimed, verdict = "kill_switch_refused", measured = true,
        n_considered = 1, "an outbound action was stopped by the kill switch");
    Some((StatusCode::FORBIDDEN, Json(json!({
        "ok": false, "code": "kill_switch_engaged", "kind": kind,
        "error": "the owner has stopped every worker's outbound actions (kill switch); nothing was sent",
        "since": st["since"], "reason": st["reason"],
        "next_step": "wait for the owner to lift it (POST /api/audit/kill-switch {\"engaged\":false})",
    }))).into_response())
}

/// Layer inside auth: stops the outbound routes in one place.
pub async fn kill_gate(req: Request, next: Next) -> Response {
    if let Some(kind) = outbound_kind(req.method(), req.uri().path()) {
        if let Some(r) = kill_refusal(kind, req.headers()) {
            return r;
        }
    }
    next.run(req).await
}

async fn kill_view(headers: HeaderMap) -> Response {
    if !can_read(&headers) {
        return refuse(StatusCode::FORBIDDEN, "audit_read_refused", "the owner, an auditor or an admin reads this");
    }
    let st = kill_state(&crate::config::amux_home());
    Json(json!({"enabled": kill_enabled(), "engaged": st.is_some(), "state": st,
        "refused_since_start": counters::get(&counters::KILL_REFUSED), "measured": true, "n_considered": 1}))
        .into_response()
}

#[derive(Deserialize)]
struct KillBody {
    engaged: bool,
    reason: Option<String>,
}

async fn kill_set(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<KillBody>) -> Response {
    if let Some(r) = owner_only(&headers, "the kill switch") {
        return r;
    }
    if b.engaged && !kill_enabled() {
        return refuse(StatusCode::CONFLICT, "kill_switch_off", "AMUX_KILL_SWITCH=off: the switch is inert on this server");
    }
    let home = crate::config::amux_home();
    let path = kill_path(&home);
    let p = principal(&headers);
    let body = json!({"engaged": b.engaged, "since": now_ms(), "by": p.actor,
        "reason": clean(&b.reason).unwrap_or_default()});
    let write = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(path.parent().unwrap_or(&home))?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&body).unwrap_or_default())?;
        std::fs::rename(&tmp, &path)
    })();
    if let Err(e) = write {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": e.to_string()}))).into_response();
    }
    let action = if b.engaged { "kill_switch.engage" } else { "kill_switch.lift" };
    let _ = state
        .store
        .write_async(move |c| {
            record(c, &p, action, "kill_switch", "allow", None);
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    tracing::warn!(engaged = b.engaged, verdict = if b.engaged { "kill_switch_engaged" } else { "kill_switch_lifted" },
        measured = true, n_considered = 1, "the owner changed the kill switch");
    Json(json!({"ok": true, "engaged": b.engaged, "state": body})).into_response()
}

/// The A5 block for `GET /api/contract/counters`.
pub fn report() -> Value {
    let engaged = kill_state(&crate::config::amux_home()).is_some();
    json!({
        "kill_switch": {"enabled": kill_enabled(), "engaged": engaged,
            "refused": counters::get(&counters::KILL_REFUSED)},
        "exports": counters::get(&counters::EXPORTS),
        "exported_rows": counters::get(&counters::EXPORTED_ROWS),
        "holds_placed": counters::get(&counters::HOLDS_PLACED),
        "holds_lifted": counters::get(&counters::HOLDS_LIFTED),
        "erasures": counters::get(&counters::ERASURES),
        "erased_rows": counters::get(&counters::ERASED_ROWS),
        "erase_refused_legal_hold": counters::get(&counters::ERASE_HELD),
        "auditor_refused": counters::get(&counters::AUDITOR_REFUSED),
        "refused_not_owner": counters::get(&counters::REFUSED_NOT_OWNER),
        "measured": true,
        "n_considered": counters::get(&counters::EXPORTS) + counters::get(&counters::HOLDS_PLACED)
            + counters::get(&counters::ERASURES) + counters::get(&counters::KILL_REFUSED),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        crate::db::migrate::test_memdb()
    }

    fn owner() -> Principal {
        Principal { actor: "owner".into(), claimed: None, on_behalf_of: None, authority: "owner_credential".into() }
    }

    fn ev(id: &str, ts: i64, actor: &str, resource: &str) -> Event {
        Event { event_id: id.into(), ts_ms: ts, kind: "interaction".into(), actor: actor.into(),
            action: Some("POST /api/x".into()), resource: Some(resource.into()), outcome: Some("applied".into()),
            ..Default::default() }
    }

    fn seed(c: &Connection) {
        ledger::append(c, &ev("a1", 100, "member:ann@x.io", "worker:wexus"));
        ledger::append(c, &ev("w1", 200, "worker:wexus", "worker:wexus/send"));
        ledger::append(c, &ev("a2", 300, "owner", "member:ann@x.io"));
        ledger::append(c, &ev("b1", 400, "member:bob@x.io", "worker:rothco"));
    }

    #[test]
    fn a_held_row_stops_the_retention_prune_and_cannot_be_deleted() {
        let c = db();
        seed(&c);
        place(&c, &HoldBody { actor: Some("worker:wexus".into()), ..Default::default() }, &owner()).unwrap();
        // Everything is past the cutoff; the prune stops before the held row.
        let n = ledger::prune(&c, 10_000).unwrap();
        assert_eq!(n, 1, "only a1, before the held w1, is pruned");
        let left: i64 = c.query_row("SELECT COUNT(*) FROM _amux_ledger WHERE event_id='w1'", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 1);
        assert_eq!(ledger::verify(&c)["chain_ok"], true);
        // Not even through the prune gate.
        c.execute("INSERT OR REPLACE INTO _amux_ledger_prune_gate (id, upto_seq) VALUES (1, 999)", []).unwrap();
        let del = c.execute("DELETE FROM _amux_ledger WHERE event_id='w1'", []);
        assert!(del.unwrap_err().to_string().contains("legal hold"));
    }

    #[test]
    fn a_lifted_hold_lets_the_prune_through_and_both_are_in_the_ledger() {
        let c = db();
        seed(&c);
        let h = place(&c, &HoldBody { worker: Some("wexus".into()), ..Default::default() }, &owner()).unwrap();
        assert_eq!(h["held_rows"], 2, "{h}");
        lift(&c, h["id"].as_str().unwrap(), &owner()).unwrap();
        assert_eq!(lift(&c, h["id"].as_str().unwrap(), &owner()).unwrap()["ok"], false, "lifted once");
        let gov: Vec<String> = c.prepare("SELECT action FROM _amux_ledger WHERE kind='governance' ORDER BY seq").unwrap()
            .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(gov, vec!["legal_hold.place", "legal_hold.lift"]);
        assert!(ledger::prune(&c, 1_000).unwrap() >= 4);
        assert!(c.execute("DELETE FROM _amux_ledger_holds", []).is_err(), "holds are never deleted");
    }

    #[test]
    fn a_hold_needs_a_scope() {
        let c = db();
        let v = place(&c, &HoldBody::default(), &owner()).unwrap();
        assert_eq!(v["code"], "hold_scope_missing");
    }

    #[test]
    fn erase_replaces_the_name_keeps_every_hash_and_the_chain_verifies() {
        let c = db();
        seed(&c);
        let before: Vec<(String, String)> = c.prepare("SELECT event_id, hash FROM _amux_ledger ORDER BY seq").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<Result<_, _>>().unwrap();
        let s = subject("Ann@X.io").unwrap();
        let v = erase(&c, &s, &owner(), false).unwrap();
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["ledger_rows"], 2, "a1 (actor) and a2 (resource)");
        let token = v["token"].as_str().unwrap().to_string();
        let (a1, a2): (String, String) = (
            c.query_row("SELECT actor FROM _amux_ledger WHERE event_id='a1'", [], |r| r.get(0)).unwrap(),
            c.query_row("SELECT resource FROM _amux_ledger WHERE event_id='a2'", [], |r| r.get(0)).unwrap());
        assert_eq!((a1.as_str(), a2.as_str()), (token.as_str(), token.as_str()));
        let left: i64 = c.query_row(&format!("SELECT COUNT(*) FROM _amux_ledger WHERE {PERSON_MATCH}"), ["ann@x.io"], |r| r.get(0)).unwrap();
        assert_eq!(left, 0, "no row names her any more");
        for (id, h) in &before {
            let now: String = c.query_row("SELECT hash FROM _amux_ledger WHERE event_id=?1", [id], |r| r.get(0)).unwrap();
            assert_eq!(&now, h, "{id} kept its hash");
        }
        let ver = ledger::verify(&c);
        assert_eq!(ver["chain_ok"], true, "{ver}");
        assert_eq!(ver["redacted"], 2, "{ver}");
        // Bob is untouched.
        let b: String = c.query_row("SELECT actor FROM _amux_ledger WHERE event_id='b1'", [], |r| r.get(0)).unwrap();
        assert_eq!(b, "member:bob@x.io");
    }

    #[test]
    fn a_redacted_row_changed_after_its_erasure_is_caught() {
        let c = db();
        seed(&c);
        erase(&c, &subject("ann@x.io").unwrap(), &owner(), false).unwrap();
        c.execute_batch("DROP TRIGGER amux_ledger_no_update;").unwrap();
        c.execute("UPDATE _amux_ledger SET decision='deny' WHERE event_id='a1'", []).unwrap();
        let v = ledger::verify(&c);
        assert_eq!(v["chain_ok"], false, "{v}");
        assert!(v["first_bad"]["reason"].as_str().unwrap().contains("erasure"), "{v}");
    }

    #[test]
    fn a_row_marked_redacted_without_an_erasure_is_caught() {
        let c = db();
        seed(&c);
        c.execute_batch("DROP TRIGGER amux_ledger_erase_only;").unwrap();
        c.execute("UPDATE _amux_ledger SET actor='erased:x', redacted_at=1 WHERE event_id='b1'", []).unwrap();
        let v = ledger::verify(&c);
        assert_eq!(v["chain_ok"], false, "{v}");
        assert_eq!(v["first_bad_event"], "b1", "{v}");
    }

    #[test]
    fn the_personal_columns_change_only_through_the_erase() {
        let c = db();
        seed(&c);
        let up = c.execute("UPDATE _amux_ledger SET actor='erased:x', redacted_at=1 WHERE event_id='a1'", []);
        assert!(up.unwrap_err().to_string().contains("append-only"));
        c.execute("INSERT INTO _amux_ledger_erase_gate (seq) SELECT seq FROM _amux_ledger WHERE event_id='a1'", []).unwrap();
        let up = c.execute("UPDATE _amux_ledger SET actor='owner', redacted_at=1 WHERE event_id='a1'", []);
        assert!(up.is_err(), "even through the gate, only to an erased: token");
    }

    #[test]
    fn erase_is_refused_under_a_legal_hold_and_changes_nothing() {
        let c = db();
        seed(&c);
        place(&c, &HoldBody { actor: Some("member:ann@x.io".into()), ..Default::default() }, &owner()).unwrap();
        let v = erase(&c, &subject("ann@x.io").unwrap(), &owner(), false).unwrap();
        assert_eq!(v["code"], "legal_hold", "{v}");
        let a1: String = c.query_row("SELECT actor FROM _amux_ledger WHERE event_id='a1'", [], |r| r.get(0)).unwrap();
        assert_eq!(a1, "member:ann@x.io");
    }

    #[test]
    fn export_marks_rows_past_a_break_as_unverified_and_filters_by_worker() {
        let c = db();
        seed(&c);
        let (rows, s) = export_rows(&c, &ExportQuery { worker: Some("wexus".into()), ..Default::default() }).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r["event_id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["a1", "w1"]);
        assert_eq!(s["chain_ok"], true);
        assert!(rows.iter().all(|r| r["chain_verified"] == json!(true)));
        c.execute_batch("DROP TRIGGER amux_ledger_no_update;").unwrap();
        c.execute("UPDATE _amux_ledger SET outcome='x' WHERE event_id='w1'", []).unwrap();
        let (rows, s) = export_rows(&c, &ExportQuery::default()).unwrap();
        assert_eq!(s["chain_ok"], false);
        let v: Vec<bool> = rows.iter().map(|r| r["chain_verified"].as_bool().unwrap()).collect();
        assert_eq!(v, vec![true, false, false, false]);
    }

    #[test]
    fn csv_quotes_and_defuses_formulas_and_ocsf_is_api_activity() {
        let row = json!({"seq": 1, "event_id": "e", "actor": "=HYPERLINK(1)", "resource": "a,b", "action": "POST /x",
            "outcome": "refused", "chain_verified": true});
        let csv = to_csv(std::slice::from_ref(&row));
        assert!(csv.contains("'=HYPERLINK(1)"), "{csv}");
        assert!(csv.contains("\"a,b\""), "{csv}");
        let o = to_ocsf(&row);
        assert_eq!(o["class_uid"], 6003);
        assert_eq!(o["status"], "Failure");
        assert_eq!(o["unmapped"]["amux"]["chain_verified"], true);
    }

    #[test]
    fn the_outbound_routes_are_named_and_reads_are_not() {
        assert_eq!(outbound_kind(&Method::POST, "/api/page-links"), Some("publish"));
        assert_eq!(outbound_kind(&Method::POST, "/api/page-links/abc/approve"), Some("publish"));
        assert_eq!(outbound_kind(&Method::POST, "/api/email/send"), Some("email_send"));
        assert_eq!(outbound_kind(&Method::POST, "/api/email/approve/apr_1"), Some("email_send"));
        assert_eq!(outbound_kind(&Method::POST, "/api/connectors/google-gmail/token"), Some("connector_mint"));
        assert_eq!(outbound_kind(&Method::GET, "/api/page-links"), None);
        assert_eq!(outbound_kind(&Method::POST, "/api/sessions/w/send"), None);
        assert_eq!(outbound_kind(&Method::DELETE, "/api/page-links/abc"), None, "unpublishing is never stopped");
    }

    #[test]
    fn a_corrupt_kill_switch_file_reads_engaged() {
        let d = tempfile::tempdir().unwrap();
        assert!(kill_state(d.path()).is_none());
        std::fs::create_dir_all(d.path().join("governance")).unwrap();
        std::fs::write(kill_path(d.path()), "{not json").unwrap();
        assert!(kill_state(d.path()).is_some());
        std::fs::write(kill_path(d.path()), r#"{"engaged":false}"#).unwrap();
        assert!(kill_state(d.path()).is_none());
    }

    #[test]
    fn an_auditor_reads_the_record_and_nothing_else() {
        use super::super::org::auditor_may;
        for p in ["/api/ledger", "/api/ledger/verify", "/api/trace/abc", "/api/audit/export", "/api/audit/holds",
                  "/api/security/at-rest", "/api/approvals/cards", "/api/contract/counters", "/api/audit"] {
            assert!(auditor_may(&Method::GET, p), "{p}");
        }
        for p in ["/api/audit/holds", "/api/audit/kill-switch", "/api/audit/person/erase", "/api/board", "/api/ledger/checkpoints"] {
            assert!(!auditor_may(&Method::POST, p), "POST {p}");
        }
        for p in ["/api/sessions", "/api/fs/list", "/api/vault", "/api/board", "/api/email/inbox"] {
            assert!(!auditor_may(&Method::GET, p), "GET {p}");
        }
    }

    #[test]
    fn a_subject_is_a_person() {
        assert_eq!(subject("member:Ann@X.io").unwrap().actor, "member:ann@x.io");
        assert!(subject("owner").is_none());
        assert!(subject("worker:wexus").is_none());
    }
}
