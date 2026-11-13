//! The ledger: one append-only, hash-chained record of every decision and
//! effect (governance plan 4.4; Vision A4, A5, A10, A11).
//!
//! It is not a second trail beside the others. `_amux_interactions` stays the
//! in-flight working state of a command (its phase moves and it keeps 14 days);
//! when the command finishes, `interactions::middleware` writes it here once.
//! Every `_amux_policy_receipts` row (policy decisions, connector token mints,
//! email gate decisions and bypasses) is copied here by a trigger in the same
//! transaction (migration 0157), so the receipt and its ledger row cannot
//! disagree; the receipt is the 30-day working view, this is the record.
//!
//! - **Who acted is proven, never claimed.** [`principal`] reads only what the
//!   server verified (owner credential, member session, policy mark, a worker's
//!   own token, an app's own token). A name a request merely asserted is kept in
//!   `claimed`, and the actor is `anonymous`.
//! - **Append-only.** Triggers refuse UPDATE and DELETE; the seal pass may fill
//!   `prev_hash`/`hash` once. Pruning opens a gate inside its own transaction and
//!   moves the anchor forward to the last pruned row's hash, so the chain stays
//!   verifiable from the anchor.
//! - **Tamper-evident.** `GET /api/ledger/verify` walks the chain from the
//!   anchor and names the first row whose hash or link does not match.
//! - **Never fails the request (A11).** A failed append logs
//!   `ledger_append_failed` and is counted; verify reports the count.
//!   `AMUX_LEDGER=off` stops appends.

use super::AppState;
use crate::db::WriteOutcome;
use axum::{
    extract::{Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub const ENABLED_ENV: &str = "AMUX_LEDGER";
pub const RETAIN_ENV: &str = "AMUX_LEDGER_RETAIN_DAYS";
/// Ethan, 2026-11-09: one year by default.
pub const DEFAULT_RETAIN_DAYS: f64 = 365.0;

const SEAL_BATCH: i64 = 500;
const MAX_LIMIT: i64 = 1000;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/ledger", get(list))
        .route("/api/ledger/verify", get(verify_route))
        .merge(super::ledger_checkpoint::routes())
}

/// `AMUX_LEDGER` is on unless it says off.
pub fn enabled() -> bool {
    enabled_from(std::env::var(ENABLED_ENV).ok().as_deref())
}

fn enabled_from(v: Option<&str>) -> bool {
    !matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("off" | "0" | "false" | "no")
    )
}

pub fn retain_days() -> f64 {
    std::env::var(RETAIN_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(DEFAULT_RETAIN_DAYS)
}

/// Counts since this process started (A5).
pub(crate) mod counters {
    use super::*;
    pub static APPENDED: AtomicU64 = AtomicU64::new(0);
    pub static APPEND_FAILED: AtomicU64 = AtomicU64::new(0);
    pub static SEALED: AtomicU64 = AtomicU64::new(0);
    pub static PRUNED: AtomicU64 = AtomicU64::new(0);
    pub fn bump(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Ordering::Relaxed);
    }
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

/// Mirror `AMUX_LEDGER` into the table the receipt trigger reads.
pub fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO _amux_ledger_config (id, enabled) VALUES (1, ?1)
         ON CONFLICT(id) DO UPDATE SET enabled=excluded.enabled",
        [enabled() as i64],
    )?;
    Ok(())
}

// ── Who acted ───────────────────────────────────────────────────────────────

/// Who the server proved acted, and on whose authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// `owner`, `member:<email>`, `policy:<name>`, `worker:<name>`,
    /// `app:<id>` or `anonymous`.
    pub actor: String,
    /// A name the request asserted but did not prove. Never the actor.
    pub claimed: Option<String>,
    pub on_behalf_of: Option<String>,
    /// What let the actor act: `owner_credential`, `member_session`,
    /// `owner_policy`, `worker_token`, `app_token`, `unproven:<why>` or `none`.
    pub authority: String,
}

impl Principal {
    pub fn anonymous(claimed: Option<String>) -> Self {
        Principal { actor: "anonymous".into(), claimed, on_behalf_of: None, authority: "none".into() }
    }
}

/// Resolve the actor from verified facts only. Must run after the auth layer,
/// which stamps the owner proof (`human_proof`).
pub fn principal(home: &Path, headers: &HeaderMap) -> Principal {
    let lane = super::email::hdr_worker(headers);
    match super::human_proof::proven(headers) {
        Some(super::human_proof::Proven::Owner) => {
            return Principal {
                actor: "owner".into(),
                claimed: lane.map(|l| format!("worker:{l}")),
                on_behalf_of: None,
                authority: "owner_credential".into(),
            }
        }
        Some(super::human_proof::Proven::Member(m)) => {
            let actor = if m.starts_with("member:") { m } else { format!("member:{m}") };
            return Principal { actor, claimed: None, on_behalf_of: None, authority: "member_session".into() };
        }
        Some(super::human_proof::Proven::Policy(p)) => {
            return Principal {
                actor: format!("policy:{p}"),
                claimed: lane.map(|l| format!("worker:{l}")),
                on_behalf_of: Some("owner".into()),
                authority: "owner_policy".into(),
            }
        }
        None => {}
    }
    if let Some(app) = super::app_identity::actor(headers) {
        let on_behalf = headers
            .get(super::app_identity::ON_BEHALF_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        return Principal { actor: app.to_string(), claimed: None, on_behalf_of: on_behalf, authority: "app_token".into() };
    }
    let Some(lane) = lane else {
        return Principal::anonymous(None);
    };
    let token = headers
        .get(super::worker_identity::TOKEN_HEADER)
        .and_then(|v| v.to_str().ok());
    use super::worker_identity::Check;
    // `check_with(.., Enforce)` asks "is the token valid", whatever mode the
    // gate itself runs in: the ledger records proof, not policy.
    match super::worker_identity::check_with(home, &lane, token, super::worker_identity::Mode::Enforce) {
        Check::Valid => Principal {
            actor: format!("worker:{lane}"),
            claimed: None,
            on_behalf_of: None,
            authority: "worker_token".into(),
        },
        other => Principal {
            authority: format!("unproven:{}", match other {
                Check::Unminted => "unminted",
                Check::Unenforced => "unenforced",
                _ => "invalid_token",
            }),
            ..Principal::anonymous(Some(format!("worker:{lane}")))
        },
    }
}

/// Commands that are telemetry, not decisions: presence pings, status hook
/// reports, typing and read markers. On this host they were 33,000 of 37,861
/// interaction rows; recording them would bury the record (ethos 5).
pub fn records(path: &str) -> bool {
    !(path.starts_with("/api/presence/")
        || path.ends_with("/typing")
        || path.ends_with("/read")
        || path.ends_with("/report")
        || path == "/api/fs/opened")
}

/// The proven principal, carried on the response from inside the auth layer
/// out to `interactions::middleware`, which runs outside it.
#[derive(Debug, Clone)]
pub struct Stamp(pub Principal);

/// Layer just inside `auth::require_bearer`: resolve the principal once, after
/// auth has stamped its proof, for every command the ledger records.
pub async fn stamp(req: Request, next: Next) -> Response {
    if !super::interactions::tracks(req.method().as_str(), req.uri().path()) || !records(req.uri().path()) {
        return next.run(req).await;
    }
    let p = principal(&crate::config::amux_home(), req.headers());
    // A message this request delivers records who the server proved sent it
    // (instruction_provenance), not a name the body or a header claimed.
    let mut res = super::instruction_provenance::with_actor(p.actor.clone(), next.run(req)).await;
    res.extensions_mut().insert(Stamp(p));
    res
}

// ── Append, seal, prune ─────────────────────────────────────────────────────

/// One row to append. Hashes, never payloads.
#[derive(Debug, Clone, Default)]
pub struct Event {
    pub event_id: String,
    pub ts_ms: i64,
    pub kind: String,
    pub trace_id: Option<String>,
    pub parent_event_id: Option<String>,
    pub actor: String,
    pub claimed: Option<String>,
    pub on_behalf_of: Option<String>,
    pub authority: Option<String>,
    pub policy_version: Option<String>,
    pub action: Option<String>,
    pub resource: Option<String>,
    pub input_hash: Option<String>,
    pub output_hash: Option<String>,
    pub decision: Option<String>,
    pub outcome: Option<String>,
    pub cost_microusd: Option<i64>,
    pub source_table: Option<String>,
    pub source_id: Option<String>,
    /// This row's own span, the span it descends from, and how it came by its
    /// trace (`minted`, `inherited`, `delivered`); see `api/trace.rs`.
    pub span_id: Option<String>,
    pub parent_span_id: Option<String>,
    pub trace_source: Option<String>,
}

impl Event {
    pub fn with_principal(mut self, p: &Principal) -> Self {
        self.actor = p.actor.clone();
        self.claimed = p.claimed.clone();
        self.on_behalf_of = p.on_behalf_of.clone();
        self.authority = Some(p.authority.clone());
        self
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Append one event and seal what is pending. Never returns an error to the
/// caller's transaction: a failure is logged and counted (A11).
pub fn append(conn: &Connection, ev: &Event) {
    if !enabled() {
        return;
    }
    let res = conn.execute(
        "INSERT OR IGNORE INTO _amux_ledger
         (event_id, ts, kind, trace_id, parent_event_id, actor, claimed, on_behalf_of, authority,
          policy_version, action, resource, input_hash, output_hash, decision, outcome,
          cost_microusd, source_table, source_id, span_id, parent_span_id, trace_source)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
        rusqlite::params![
            ev.event_id, ev.ts_ms, ev.kind, ev.trace_id, ev.parent_event_id,
            if ev.actor.is_empty() { "anonymous" } else { ev.actor.as_str() },
            ev.claimed, ev.on_behalf_of, ev.authority, ev.policy_version, ev.action,
            ev.resource, ev.input_hash, ev.output_hash, ev.decision, ev.outcome,
            ev.cost_microusd, ev.source_table, ev.source_id,
            ev.span_id, ev.parent_span_id, ev.trace_source
        ],
    );
    match res {
        Ok(n) => counters::bump(&counters::APPENDED, n as u64),
        Err(e) => {
            counters::bump(&counters::APPEND_FAILED, 1);
            tracing::warn!(verdict = "ledger_append_failed", event_id = %ev.event_id, error = %e,
                measured = true, n_considered = 1, "a decision was not written to the ledger");
            return;
        }
    }
    if let Err(e) = seal(conn) {
        tracing::warn!(verdict = "ledger_seal_failed", error = %e, "ledger rows left unsealed; the next append retries");
    }
}

const COLS: &str = "seq, event_id, ts, kind, trace_id, parent_event_id, actor, claimed, on_behalf_of,
    authority, policy_version, action, resource, input_hash, output_hash, decision, outcome,
    cost_microusd, source_table, source_id, span_id, parent_span_id, trace_source";
const NCOLS: usize = 23;
/// The columns every row has been hashed over since migration 0157. The trace
/// columns (0158) join the hash only when one is set, so a row sealed before
/// they existed keeps its hash and the chain still verifies.
const V1_COLS: usize = 20;

fn fields(r: &rusqlite::Row) -> rusqlite::Result<Vec<Option<String>>> {
    use rusqlite::types::ValueRef;
    (0..NCOLS)
        .map(|i| {
            Ok(match r.get_ref(i)? {
                ValueRef::Null => None,
                ValueRef::Integer(n) => Some(n.to_string()),
                ValueRef::Real(f) => Some(f.to_string()),
                ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(b) => Some(sha256_hex(b)),
            })
        })
        .collect::<rusqlite::Result<Vec<_>>>()
        .map(|mut f| {
            if f[V1_COLS..].iter().all(Option::is_none) {
                f.truncate(V1_COLS);
            }
            f
        })
}

/// The hash of one row, linked to the one before it.
pub fn row_hash(prev: &str, fields: &[Option<String>]) -> String {
    let mut buf = String::with_capacity(256);
    buf.push_str(prev);
    for f in fields {
        buf.push('\u{1e}');
        match f {
            Some(v) => buf.push_str(v),
            None => buf.push('\u{0}'),
        }
    }
    sha256_hex(buf.as_bytes())
}

pub(crate) fn anchor(conn: &Connection) -> rusqlite::Result<(i64, String)> {
    Ok(conn
        .query_row("SELECT upto_seq, anchor_hash FROM _amux_ledger_anchor WHERE id=1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?
        .unwrap_or((0, "genesis".into())))
}

/// Fill `prev_hash`/`hash` on unsealed rows, in `seq` order.
pub fn seal(conn: &Connection) -> rusqlite::Result<usize> {
    let mut prev: String = match conn
        .query_row("SELECT hash FROM _amux_ledger WHERE hash IS NOT NULL ORDER BY seq DESC LIMIT 1", [], |r| r.get(0))
        .optional()?
    {
        Some(h) => h,
        None => anchor(conn)?.1,
    };
    let rows: Vec<(i64, Vec<Option<String>>)> = {
        let mut st = conn.prepare(&format!(
            "SELECT {COLS} FROM _amux_ledger WHERE hash IS NULL ORDER BY seq LIMIT {SEAL_BATCH}"
        ))?;
        let it = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, fields(r)?)))?;
        it.collect::<rusqlite::Result<_>>()?
    };
    let n = rows.len();
    for (seq, f) in rows {
        let h = row_hash(&prev, &f);
        conn.execute("UPDATE _amux_ledger SET prev_hash=?2, hash=?3 WHERE seq=?1", rusqlite::params![seq, prev, h])?;
        prev = h;
    }
    counters::bump(&counters::SEALED, n as u64);
    Ok(n)
}

/// Drop rows older than the retention window, as a prefix of the chain, and
/// move the anchor to the last dropped row's hash so verify still starts from
/// a known point. Unsealed rows are never pruned.
pub fn prune(conn: &Connection, cutoff_ms: i64) -> rusqlite::Result<usize> {
    let (from, _) = anchor(conn)?;
    let stop: Option<i64> = conn.query_row(
        "SELECT MIN(seq) FROM _amux_ledger WHERE ts >= ?1 OR hash IS NULL", [cutoff_ms], |r| r.get(0))?;
    let upto: Option<i64> = match stop {
        Some(s) => conn.query_row("SELECT MAX(seq) FROM _amux_ledger WHERE seq < ?1", [s], |r| r.get(0))?,
        None => conn.query_row("SELECT MAX(seq) FROM _amux_ledger", [], |r| r.get(0))?,
    };
    let Some(upto) = upto.filter(|u| *u > from) else {
        return Ok(0);
    };
    let anchor_hash: String =
        conn.query_row("SELECT hash FROM _amux_ledger WHERE seq=?1", [upto], |r| r.get(0))?;
    conn.execute("INSERT OR REPLACE INTO _amux_ledger_prune_gate (id, upto_seq) VALUES (1, ?1)", [upto])?;
    let n = conn.execute("DELETE FROM _amux_ledger WHERE seq <= ?1", [upto]);
    conn.execute("DELETE FROM _amux_ledger_prune_gate WHERE id=1", [])?;
    let n = n?;
    conn.execute(
        "UPDATE _amux_ledger_anchor SET upto_seq=?1, anchor_hash=?2, pruned_at=?3 WHERE id=1",
        rusqlite::params![upto, anchor_hash, chrono::Utc::now().timestamp_millis()],
    )?;
    counters::bump(&counters::PRUNED, n as u64);
    tracing::info!(verdict = "ledger_retention", pruned = n, upto_seq = upto, retain_days = retain_days(),
        measured = true, n_considered = n, "ledger rows past retention pruned; anchor moved");
    Ok(n)
}

// ── Verify and read ─────────────────────────────────────────────────────────

/// Walk the chain from the anchor. Reports the first row that does not match.
pub fn verify(conn: &Connection) -> Value {
    verify_capturing(conn, &std::collections::BTreeSet::new()).0
}

/// [`verify`], also returning the hash the chain holds at each seq in `want`
/// (the seqs signed checkpoints cover) and the last seq the walk verified, so
/// `ledger_checkpoint` checks those hashes without a second walk.
pub fn verify_capturing(
    conn: &Connection,
    want: &std::collections::BTreeSet<i64>,
) -> (Value, std::collections::BTreeMap<i64, String>, i64) {
    let mut captured = std::collections::BTreeMap::new();
    let mut walked: i64 = 0;
    let (upto, anchor_hash) = match anchor(conn) {
        Ok(a) => a,
        Err(e) => return (json!({"ok": false, "measured": false, "n_considered": 0,
            "why_unmeasured": format!("ledger unreadable: {e}")}), captured, walked),
    };
    let mut prev = anchor_hash.clone();
    let (mut n, mut sealed, mut unsealed) = (0u64, 0u64, 0u64);
    let mut bad: Option<Value> = None;
    let res: rusqlite::Result<()> = (|| {
        let mut st = conn.prepare(&format!("SELECT {COLS}, prev_hash, hash FROM _amux_ledger ORDER BY seq"))?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            n += 1;
            let seq: i64 = r.get(0)?;
            let f = fields(r)?;
            let row_prev: Option<String> = r.get(NCOLS)?;
            let row_hash_v: Option<String> = r.get(NCOLS + 1)?;
            let event_id = f[1].clone().unwrap_or_default();
            if seq <= upto {
                bad = Some(json!({"seq": seq, "event_id": event_id, "reason": "row at or below the prune anchor"}));
                break;
            }
            let Some(h) = row_hash_v else {
                unsealed += 1;
                continue;
            };
            if unsealed > 0 {
                bad = Some(json!({"seq": seq, "event_id": event_id, "reason": "sealed row after an unsealed one"}));
                break;
            }
            if row_prev.as_deref() != Some(prev.as_str()) {
                bad = Some(json!({"seq": seq, "event_id": event_id,
                    "reason": "prev_hash does not link to the row before it (a row was removed or reordered)"}));
                break;
            }
            if row_hash(&prev, &f) != h {
                bad = Some(json!({"seq": seq, "event_id": event_id,
                    "reason": "hash does not match the row's contents (the row was changed)"}));
                break;
            }
            sealed += 1;
            if want.contains(&seq) {
                captured.insert(seq, h.clone());
            }
            walked = seq;
            prev = h;
        }
        Ok(())
    })();
    if let Err(e) = res {
        return (json!({"ok": false, "measured": false, "n_considered": n,
            "why_unmeasured": format!("ledger unreadable: {e}")}), captured, walked);
    }
    let failures = counters::get(&counters::APPEND_FAILED);
    let v = json!({
        "ok": bad.is_none() && failures == 0,
        "chain_ok": bad.is_none(),
        "first_bad_event": bad.as_ref().map(|b| b["event_id"].clone()),
        "first_bad": bad,
        "sealed": sealed,
        "unsealed": unsealed,
        "anchor": {"upto_seq": upto, "hash": anchor_hash},
        "append_failures_since_start": failures,
        "enabled": enabled(),
        "retain_days": retain_days(),
        "measured": true,
        "n_considered": n,
    });
    (v, captured, walked)
}

async fn verify_route(State(state): State<AppState>) -> Response {
    // Seal first: a row appended by the receipt trigger since the last command
    // is pending, not broken.
    if let Err(e) = state
        .store
        .write_async(|conn| {
            seal(conn)?;
            Ok(WriteOutcome { applied: false, events: vec![] })
        })
        .await {
        tracing::warn!(error = %e, verdict = "ledger_seal_failed", "sealing before verify failed; pending rows read as unsealed");
    }
    let home = crate::config::amux_home();
    match state.store.read_async(move |conn| Ok(super::ledger_checkpoint::verify_all(conn, &home))).await {
        Ok(mut v) => {
            super::ledger_checkpoint::verify_external(&mut v).await;
            Json(v).into_response()
        }
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ok": false, "measured": false,
            "n_considered": 0, "why_unmeasured": e.to_string()}))).into_response(),
    }
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    pub actor: Option<String>,
    pub resource: Option<String>,
    pub trace_id: Option<String>,
    pub kind: Option<String>,
    /// Milliseconds since the epoch, or hours back with `since_h`.
    pub since: Option<i64>,
    pub since_h: Option<f64>,
    /// Page backwards from this seq (exclusive).
    pub before_seq: Option<i64>,
    pub limit: Option<i64>,
}

const ROW_COLS: &str = "seq, event_id, ts, kind, trace_id, parent_event_id, actor, claimed, on_behalf_of,
    authority, policy_version, action, resource, input_hash, output_hash, decision, outcome,
    cost_microusd, source_table, source_id, hash IS NOT NULL, span_id, parent_span_id, trace_source";

fn row_json(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let ts: i64 = r.get(2)?;
    Ok(json!({
        "seq": r.get::<_, i64>(0)?, "event_id": r.get::<_, String>(1)?, "ts": ts,
        "at": chrono::DateTime::from_timestamp_millis(ts).map(|t| t.to_rfc3339()),
        "kind": r.get::<_, String>(3)?, "trace_id": r.get::<_, Option<String>>(4)?,
        "parent_event_id": r.get::<_, Option<String>>(5)?, "actor": r.get::<_, String>(6)?,
        "claimed": r.get::<_, Option<String>>(7)?, "on_behalf_of": r.get::<_, Option<String>>(8)?,
        "authority": r.get::<_, Option<String>>(9)?, "policy_version": r.get::<_, Option<String>>(10)?,
        "action": r.get::<_, Option<String>>(11)?, "resource": r.get::<_, Option<String>>(12)?,
        "input_hash": r.get::<_, Option<String>>(13)?, "output_hash": r.get::<_, Option<String>>(14)?,
        "decision": r.get::<_, Option<String>>(15)?, "outcome": r.get::<_, Option<String>>(16)?,
        "cost_microusd": r.get::<_, Option<i64>>(17)?,
        "source": {"table": r.get::<_, Option<String>>(18)?, "id": r.get::<_, Option<String>>(19)?},
        "sealed": r.get::<_, bool>(20)?,
        "span_id": r.get::<_, Option<String>>(21)?, "parent_span_id": r.get::<_, Option<String>>(22)?,
        "trace_source": r.get::<_, Option<String>>(23)?,
    }))
}

/// Filtered page, newest first. Pure over a connection for tests.
pub fn query(conn: &Connection, q: &ListQuery) -> rusqlite::Result<Value> {
    let since = q.since.unwrap_or_else(|| {
        q.since_h
            .filter(|h| h.is_finite() && *h > 0.0)
            .map(|h| chrono::Utc::now().timestamp_millis() - (h * 3_600_000.0) as i64)
            .unwrap_or(0)
    });
    let limit = q.limit.unwrap_or(200).clamp(1, MAX_LIMIT);
    let filter = "ts >= ?1 AND (?2 = '' OR actor = ?2 OR claimed = ?2 OR on_behalf_of = ?2)
        AND (?3 = '' OR resource = ?3 OR resource LIKE ?3 || '/%') AND (?4 = '' OR trace_id = ?4)
        AND (?5 = '' OR kind = ?5)";
    let p = rusqlite::params![
        since,
        q.actor.clone().unwrap_or_default(),
        q.resource.clone().unwrap_or_default(),
        q.trace_id.clone().unwrap_or_default(),
        q.kind.clone().unwrap_or_default(),
        q.before_seq.unwrap_or(i64::MAX),
        limit
    ];
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM _amux_ledger WHERE {filter} AND seq < ?6 AND ?7 > 0"), p, |r| r.get(0))?;
    let mut st = conn.prepare(&format!(
        "SELECT {ROW_COLS} FROM _amux_ledger WHERE {filter} AND seq < ?6 ORDER BY seq DESC LIMIT ?7"))?;
    let events: Vec<Value> = st.query_map(p, row_json)?.collect::<rusqlite::Result<_>>()?;
    let next = if (events.len() as i64) < total { events.last().map(|e| e["seq"].clone()) } else { None };
    Ok(json!({
        "events": events,
        "next_before_seq": next,
        "measured": true,
        "n_considered": total,
        "enabled": enabled(),
    }))
}

async fn list(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    match state.store.read_async(move |conn| Ok(query(conn, &q)?)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "ledger unavailable",
            "measured": false, "n_considered": 0, "why_unmeasured": e.to_string()}))).into_response(),
    }
}

/// The A5 block for `GET /api/contract/counters`.
pub fn report() -> Value {
    json!({
        "enabled": enabled(),
        "retain_days": retain_days(),
        "appended": counters::get(&counters::APPENDED),
        "append_failed": counters::get(&counters::APPEND_FAILED),
        "sealed": counters::get(&counters::SEALED),
        "pruned": counters::get(&counters::PRUNED),
        "measured": true,
        "n_considered": counters::get(&counters::APPENDED) + counters::get(&counters::APPEND_FAILED),
        "verify": "GET /api/ledger/verify",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        crate::db::migrate::test_memdb()
    }

    fn ev(id: &str, ts: i64, actor: &str) -> Event {
        Event { event_id: id.into(), ts_ms: ts, kind: "interaction".into(), actor: actor.into(),
            action: Some("POST /api/x".into()), resource: Some("worker:w".into()),
            outcome: Some("applied".into()), ..Default::default() }
    }

    #[test]
    fn trace_columns_join_the_hash_only_when_set_and_a_changed_one_is_caught() {
        let c = db();
        append(&c, &ev("old", 1, "owner"));
        let mut traced = ev("new", 2, "worker:w");
        traced.trace_id = Some("a".repeat(32));
        traced.span_id = Some("b".repeat(16));
        traced.parent_span_id = Some("c".repeat(16));
        traced.trace_source = Some("inherited".into());
        append(&c, &traced);
        // A row with no trace columns hashes exactly as rows did before the
        // columns existed, so a chain sealed before migration 0158 verifies.
        let (prev, h): (String, String) = c
            .query_row("SELECT prev_hash, hash FROM _amux_ledger WHERE event_id='old'", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        let f = c
            .query_row(&format!("SELECT {COLS} FROM _amux_ledger WHERE event_id='old'"), [], fields)
            .unwrap();
        assert_eq!(f.len(), V1_COLS);
        assert_eq!(row_hash(&prev, &f), h);
        assert_eq!(verify(&c)["chain_ok"], true);
        c.execute_batch("DROP TRIGGER amux_ledger_no_update").unwrap();
        c.execute("UPDATE _amux_ledger SET parent_span_id='dddddddddddddddd' WHERE event_id='new'", []).unwrap();
        let v = verify(&c);
        assert_eq!(v["chain_ok"], false, "{v}");
        assert_eq!(v["first_bad_event"], "new");
    }

    #[test]
    fn appended_rows_are_sealed_into_a_chain_that_verifies() {
        let c = db();
        for i in 0..5 {
            append(&c, &ev(&format!("e{i}"), 1000 + i, "owner"));
        }
        let v = verify(&c);
        assert_eq!(v["chain_ok"], true, "{v}");
        assert_eq!(v["sealed"], 5, "{v}");
        assert_eq!(v["unsealed"], 0, "{v}");
        assert_eq!(v["measured"], true);
    }

    #[test]
    fn an_update_or_delete_of_a_ledger_row_is_refused() {
        let c = db();
        append(&c, &ev("e1", 1, "worker:w"));
        let up = c.execute("UPDATE _amux_ledger SET actor='owner' WHERE event_id='e1'", []);
        assert!(up.unwrap_err().to_string().contains("append-only"));
        let reseal = c.execute("UPDATE _amux_ledger SET hash='x' WHERE event_id='e1'", []);
        assert!(reseal.unwrap_err().to_string().contains("already sealed"));
        let del = c.execute("DELETE FROM _amux_ledger WHERE event_id='e1'", []);
        assert!(del.unwrap_err().to_string().contains("append-only"));
    }

    #[test]
    fn a_row_changed_behind_the_triggers_is_named_by_verify() {
        let c = db();
        for i in 0..4 {
            append(&c, &ev(&format!("e{i}"), 10 + i, "worker:w"));
        }
        c.execute_batch("DROP TRIGGER amux_ledger_no_update;").unwrap();
        c.execute("UPDATE _amux_ledger SET actor='owner' WHERE event_id='e2'", []).unwrap();
        let v = verify(&c);
        assert_eq!(v["chain_ok"], false, "{v}");
        assert_eq!(v["first_bad_event"], "e2", "{v}");
        assert!(v["first_bad"]["reason"].as_str().unwrap().contains("changed"), "{v}");
    }

    #[test]
    fn a_row_removed_behind_the_triggers_is_named_by_verify() {
        let c = db();
        for i in 0..4 {
            append(&c, &ev(&format!("e{i}"), 10 + i, "worker:w"));
        }
        c.execute_batch("DROP TRIGGER amux_ledger_no_delete;").unwrap();
        c.execute("DELETE FROM _amux_ledger WHERE event_id='e1'", []).unwrap();
        let v = verify(&c);
        assert_eq!(v["first_bad_event"], "e2", "{v}");
        assert!(v["first_bad"]["reason"].as_str().unwrap().contains("removed"), "{v}");
    }

    #[test]
    fn pruning_keeps_the_rest_of_the_chain_verifiable_from_the_anchor() {
        let c = db();
        for i in 0..6 {
            append(&c, &ev(&format!("e{i}"), 100 * i, "owner"));
        }
        let n = prune(&c, 250).unwrap();
        assert_eq!(n, 3, "rows at ts 0, 100, 200 are past the cutoff");
        let v = verify(&c);
        assert_eq!(v["chain_ok"], true, "{v}");
        assert_eq!(v["sealed"], 3, "{v}");
        assert_eq!(v["anchor"]["upto_seq"], 3, "{v}");
        // Appending after a prune links to the chain, not to the anchor.
        append(&c, &ev("e6", 600, "owner"));
        assert_eq!(verify(&c)["chain_ok"], true);
        // The prune gate is closed again.
        let gate: i64 = c.query_row("SELECT COUNT(*) FROM _amux_ledger_prune_gate", [], |r| r.get(0)).unwrap();
        assert_eq!(gate, 0);
        assert!(c.execute("DELETE FROM _amux_ledger WHERE event_id='e5'", []).is_err());
    }

    #[test]
    fn a_receipt_written_inside_a_trace_lands_under_the_request_span() {
        let c = db();
        let (t, s) = ("e".repeat(32), "f".repeat(16));
        c.execute(
            "INSERT INTO _amux_policy_receipts (id,actor,action,resource,trust,reversible,effect,rule_id,rationale,cost_microusd,created_at,trace_id,span_id)
             VALUES ('pol_t','worker:w','connector_token_mint','connector:google-gmail','trusted',1,'allow','connector-token-scope','{}',0,
                     '2026-11-10T12:00:00Z',?1,?2)", rusqlite::params![t, s]).unwrap();
        let (trace, span, parent, source): (String, String, String, String) = c.query_row(
            "SELECT trace_id, span_id, parent_span_id, trace_source FROM _amux_ledger WHERE event_id='pol:pol_t'", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
        assert_eq!(trace, t);
        assert_eq!(parent, s, "the receipt descends from the request's span");
        assert_eq!(span.len(), 16);
        assert_ne!(span, s);
        assert_eq!(source, "inherited");
        // A receipt outside any trace carries none.
        c.execute(
            "INSERT INTO _amux_policy_receipts (id,actor,action,resource,trust,reversible,effect,rule_id,rationale,cost_microusd,created_at)
             VALUES ('pol_n','owner','x','y','trusted',1,'allow','r','{}',0,'2026-11-10T12:00:00Z')", []).unwrap();
        let none: Option<String> = c.query_row("SELECT span_id FROM _amux_ledger WHERE event_id='pol:pol_n'", [], |r| r.get(0)).unwrap();
        assert!(none.is_none());
        seal(&c).unwrap();
        assert_eq!(verify(&c)["chain_ok"], true);
    }

    #[test]
    fn a_policy_receipt_lands_in_the_ledger_in_the_same_write() {
        let c = db();
        c.execute(
            "INSERT INTO _amux_policy_receipts (id,actor,action,resource,trust,reversible,effect,rule_id,rationale,cost_microusd,created_at)
             VALUES ('pol_1','worker:w','connector_token_mint','connector:google-gmail','untrusted',1,'deny','connector-token-scope',
                     '{\"on_behalf_of\":\"job:social\"}',0,'2026-11-10T12:00:00.123456789+00:00')", []).unwrap();
        seal(&c).unwrap();
        let (actor, decision, behalf, ts): (String, String, String, i64) = c.query_row(
            "SELECT actor, decision, on_behalf_of, ts FROM _amux_ledger WHERE event_id='pol:pol_1'", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
        assert_eq!((actor.as_str(), decision.as_str(), behalf.as_str()), ("worker:w", "deny", "job:social"));
        assert!((ts - 1_794_312_000_123).abs() <= 1, "created_at is parsed to milliseconds: {ts}");
        assert_eq!(verify(&c)["chain_ok"], true);
    }

    #[test]
    fn the_ledger_switch_stops_receipt_copies() {
        let c = db();
        c.execute("UPDATE _amux_ledger_config SET enabled=0 WHERE id=1", []).unwrap();
        c.execute(
            "INSERT INTO _amux_policy_receipts (id,actor,action,resource,trust,reversible,effect,rule_id,rationale,cost_microusd,created_at)
             VALUES ('pol_2','owner','x','y','trusted',1,'allow',NULL,'r',0,'2026-11-10T12:00:00+00:00')", []).unwrap();
        let n: i64 = c.query_row("SELECT COUNT(*) FROM _amux_ledger", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn a_header_alone_never_makes_an_actor() {
        let home = tempfile::tempdir().unwrap();
        let mut h = HeaderMap::new();
        h.insert("x-amux-session", "w1".parse().unwrap());
        let p = principal(home.path(), &h);
        assert_eq!(p.actor, "anonymous");
        assert_eq!(p.claimed.as_deref(), Some("worker:w1"));
        assert!(p.authority.starts_with("unproven:"), "{p:?}");
        let p = principal(home.path(), &HeaderMap::new());
        assert_eq!((p.actor.as_str(), p.authority.as_str()), ("anonymous", "none"));
    }

    #[test]
    fn a_worker_with_its_own_token_is_the_actor() {
        let home = tempfile::tempdir().unwrap();
        let token = super::super::worker_identity::mint(home.path(), "w1").expect("minted");
        let mut h = HeaderMap::new();
        h.insert("x-amux-session", "w1".parse().unwrap());
        h.insert(super::super::worker_identity::TOKEN_HEADER, token.parse().unwrap());
        let p = principal(home.path(), &h);
        assert_eq!((p.actor.as_str(), p.authority.as_str()), ("worker:w1", "worker_token"));
        assert_eq!(p.claimed, None);
    }

    #[test]
    fn query_filters_by_actor_and_pages_newest_first() {
        let c = db();
        for i in 0..5 {
            append(&c, &ev(&format!("e{i}"), 10 + i, if i % 2 == 0 { "owner" } else { "worker:w" }));
        }
        let v = query(&c, &ListQuery { actor: Some("owner".into()), limit: Some(2), ..Default::default() }).unwrap();
        assert_eq!(v["n_considered"], 3);
        let ids: Vec<&str> = v["events"].as_array().unwrap().iter().map(|e| e["event_id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["e4", "e2"]);
        let next = v["next_before_seq"].as_i64().unwrap();
        let v2 = query(&c, &ListQuery { actor: Some("owner".into()), before_seq: Some(next), ..Default::default() }).unwrap();
        assert_eq!(v2["events"][0]["event_id"], "e0");
    }

    #[test]
    fn telemetry_commands_are_not_recorded_and_decisions_are() {
        for p in ["/api/presence/where", "/api/sessions/w/typing", "/api/sessions/w/report",
                  "/api/conversations/c/read", "/api/fs/opened"] {
            assert!(!records(p), "{p}");
        }
        for p in ["/api/sessions/w/send", "/api/grants/g1/decide", "/api/connectors/google-gmail/token",
                  "/api/sessions/w", "/api/page-links/x/api"] {
            assert!(records(p), "{p}");
        }
    }

    #[test]
    fn the_switch_reads_off_only_when_it_says_so() {
        assert!(enabled_from(None));
        assert!(enabled_from(Some("on")));
        assert!(!enabled_from(Some("off")));
        assert!(!enabled_from(Some(" OFF ")));
    }
}
