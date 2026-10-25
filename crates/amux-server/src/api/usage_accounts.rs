//! Per-account Claude plan usage history (migration 0129, 2026-10-20).
//!
//! The account rotator (`scripts/claude-rotate.py`) picks the next login from
//! `~/.amux/claude-usage/accounts.json`, which holds one reading per account:
//! the latest. That answers "how full is it now" and nothing else. This table
//! keeps every distinct reading, so the rotator and a person can also ask how
//! fast an account is burning, when it last had room, and whether a reading is
//! still being refreshed at all.
//!
//! Writers:
//! - the rotator, on every accepted reading (`POST /api/usage/accounts/readings`)
//!   and on each tick for any account the table is behind on;
//! - the server itself, whenever `/api/usage` takes a fresh probe of the
//!   current login (`source = "server-probe"`).
//!
//! Readers: `GET /api/usage/accounts` (latest reading, burn rate and hours to
//! full per account, plus optional raw rows).
//!
//! Identical consecutive readings from one source are kept at most once an
//! hour, so a quiet account costs about 24 rows a day. Rows older than
//! [`RETAIN_S`] are pruned on write.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use super::AppState;

/// 90 days.
pub const RETAIN_S: f64 = 90.0 * 86400.0;
/// An unchanged reading from the same source is stored again after this long,
/// so the history shows the account was still being watched.
pub const HEARTBEAT_S: f64 = 3600.0;
/// Burn rates need at least this much time between the first and last reading.
const MIN_BURN_SPAN_S: f64 = 900.0;
/// Two reset times within this many seconds are the same window (the CLI
/// rounds to the minute, the API reports microseconds).
const SAME_WINDOW_S: i64 = 3600;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Reading {
    pub account: String,
    pub source: String,
    #[serde(default)]
    pub at: Option<f64>,
    #[serde(default)]
    pub five_hour_pct: Option<f64>,
    #[serde(default)]
    pub five_hour_resets_at: Option<String>,
    #[serde(default)]
    pub seven_day_pct: Option<f64>,
    #[serde(default)]
    pub seven_day_resets_at: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
}

pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn epoch(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts.trim())
        .ok()
        .map(|d| d.timestamp())
}

fn same_reset(a: &Option<String>, b: &Option<String>) -> bool {
    match (a.as_deref(), b.as_deref()) {
        (None, None) => true,
        (Some(x), Some(y)) => match (epoch(x), epoch(y)) {
            (Some(p), Some(q)) => (p - q).abs() < SAME_WINDOW_S,
            _ => x == y,
        },
        _ => false,
    }
}

fn same_pct(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => (x - y).abs() < 0.5,
        _ => false,
    }
}

/// Check a reading before it is stored. Returns the reason it is refused.
pub fn validate(r: &Reading, at_now: f64) -> Result<f64, String> {
    let account = r.account.trim();
    if account.is_empty() || account.len() > 200 || !account.contains('@') {
        return Err("account must be the login's email address".into());
    }
    let source = r.source.trim();
    if source.is_empty() || source.len() > 40 {
        return Err("source is required (at most 40 characters)".into());
    }
    if r.five_hour_pct.is_none() && r.seven_day_pct.is_none() {
        return Err("a reading needs five_hour_pct or seven_day_pct".into());
    }
    for (name, v) in [("five_hour_pct", r.five_hour_pct), ("seven_day_pct", r.seven_day_pct)] {
        if let Some(v) = v {
            if !(0.0..=1000.0).contains(&v) || v.is_nan() {
                return Err(format!("{name} must be a percentage, got {v}"));
            }
        }
    }
    let at = r.at.unwrap_or(at_now);
    if at > at_now + 300.0 {
        return Err("at is in the future".into());
    }
    if at < at_now - RETAIN_S {
        return Err("at is older than the 90-day retention".into());
    }
    Ok(at)
}

/// Store one reading. `Ok(None)` means stored; `Ok(Some(reason))` means it was
/// a duplicate and skipped.
pub fn record(c: &Connection, r: &Reading, at_now: f64) -> rusqlite::Result<Option<&'static str>> {
    let at = r.at.unwrap_or(at_now);
    let account = r.account.trim();
    let source = r.source.trim();
    let exact: Option<i64> = c
        .query_row(
            "SELECT seq FROM claude_account_usage WHERE account = ?1 AND source = ?2 AND abs(at - ?3) < 1.0",
            params![account, source, at],
            |row| row.get(0),
        )
        .optional()?;
    if exact.is_some() {
        return Ok(Some("already_recorded"));
    }
    type Prev = (f64, Option<f64>, Option<String>, Option<f64>, Option<String>);
    let prev: Option<Prev> = c
        .query_row(
            "SELECT at, five_hour_pct, five_hour_resets_at, seven_day_pct, seven_day_resets_at
               FROM claude_account_usage WHERE account = ?1 AND source = ?2 AND at <= ?3
              ORDER BY at DESC LIMIT 1",
            params![account, source, at],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()?;
    if let Some((p_at, p5, p5r, p7, p7r)) = prev {
        let unchanged = same_pct(p5, r.five_hour_pct)
            && same_pct(p7, r.seven_day_pct)
            && same_reset(&p5r, &r.five_hour_resets_at)
            && same_reset(&p7r, &r.seven_day_resets_at);
        if unchanged && at - p_at < HEARTBEAT_S {
            return Ok(Some("unchanged"));
        }
    }
    c.execute(
        "INSERT INTO claude_account_usage
            (at, account, source, five_hour_pct, five_hour_resets_at, seven_day_pct, seven_day_resets_at, plan)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            at,
            account,
            source,
            r.five_hour_pct,
            r.five_hour_resets_at,
            r.seven_day_pct,
            r.seven_day_resets_at,
            r.plan
        ],
    )?;
    c.execute(
        "DELETE FROM claude_account_usage WHERE at < ?1",
        params![at_now - RETAIN_S],
    )?;
    Ok(None)
}

#[derive(Debug, Clone)]
struct Row {
    at: f64,
    source: String,
    five: Option<f64>,
    five_reset: Option<String>,
    week: Option<f64>,
    week_reset: Option<String>,
    plan: Option<String>,
}

fn row_json(account: &str, r: &Row) -> Value {
    json!({
        "account": account,
        "at": r.at,
        "at_iso": chrono::DateTime::from_timestamp(r.at as i64, 0).map(|d| d.to_rfc3339()),
        "source": r.source,
        "five_hour_pct": r.five,
        "five_hour_resets_at": r.five_reset,
        "seven_day_pct": r.week,
        "seven_day_resets_at": r.week_reset,
        "plan": r.plan,
    })
}

/// Percent per hour over the latest window, from the earliest and latest
/// readings that share the latest reading's reset time. None when there is
/// not enough span to say.
fn burn(rows: &[Row], pick: fn(&Row) -> (Option<f64>, &Option<String>)) -> Option<f64> {
    let last = rows.iter().rev().find(|r| pick(r).0.is_some())?;
    let (last_pct, last_reset) = pick(last);
    let first = rows
        .iter()
        .find(|r| pick(r).0.is_some() && same_reset(pick(r).1, last_reset))?;
    let span = last.at - first.at;
    if span < MIN_BURN_SPAN_S {
        return None;
    }
    let rate = (last_pct? - pick(first).0?) / (span / 3600.0);
    Some((rate.max(0.0) * 100.0).round() / 100.0)
}

/// Latest reading, burn rates and hours to full for every account seen in the
/// window, plus up to `rows` raw readings (newest first) when asked.
pub fn summary(c: &Connection, since_s: f64, account: Option<&str>, rows: usize, at_now: f64) -> rusqlite::Result<Value> {
    let mut stmt = c.prepare(
        "SELECT account, at, source, five_hour_pct, five_hour_resets_at, seven_day_pct, seven_day_resets_at, plan
           FROM claude_account_usage
          WHERE at >= ?1 AND (?2 IS NULL OR account = ?2)
          ORDER BY account, at",
    )?;
    let mut by_account: std::collections::BTreeMap<String, Vec<Row>> = Default::default();
    let iter = stmt.query_map(params![at_now - since_s, account], |row| {
        Ok((
            row.get::<_, String>(0)?,
            Row {
                at: row.get(1)?,
                source: row.get(2)?,
                five: row.get(3)?,
                five_reset: row.get(4)?,
                week: row.get(5)?,
                week_reset: row.get(6)?,
                plan: row.get(7)?,
            },
        ))
    })?;
    let mut n = 0usize;
    for item in iter {
        let (acct, row) = item?;
        n += 1;
        by_account.entry(acct).or_default().push(row);
    }
    let mut accounts = Vec::new();
    let mut raw: Vec<Value> = Vec::new();
    for (acct, list) in &by_account {
        let latest = list.last().expect("non-empty");
        let week_burn = burn(list, |r| (r.week, &r.week_reset));
        let five_burn = burn(list, |r| (r.five, &r.five_reset));
        let hours_to_full = match (latest.week, week_burn) {
            (Some(p), Some(b)) if b > 0.0 => Some(((100.0 - p).max(0.0) / b * 10.0).round() / 10.0),
            _ => None,
        };
        let week_resets_in_h = latest
            .week_reset
            .as_deref()
            .and_then(epoch)
            .map(|e| ((e as f64 - at_now) / 360.0).round() / 10.0);
        accounts.push(json!({
            "account": acct,
            "readings": list.len(),
            "first_at": list.first().map(|r| r.at),
            "latest": row_json(acct, latest),
            "latest_age_s": (at_now - latest.at).max(0.0).round(),
            "seven_day_burn_pct_per_h": week_burn,
            "five_hour_burn_pct_per_h": five_burn,
            "seven_day_hours_to_full": hours_to_full,
            "seven_day_resets_in_h": week_resets_in_h,
        }));
        if rows > 0 {
            raw.extend(list.iter().map(|r| row_json(acct, r)));
        }
    }
    raw.sort_by(|a, b| {
        b["at"].as_f64().unwrap_or(0.0).partial_cmp(&a["at"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal)
    });
    raw.truncate(rows);
    let mut out = json!({
        "measured": true,
        "n_considered": n,
        "since_s": since_s,
        "accounts": accounts,
    });
    if rows > 0 {
        out["rows"] = json!(raw);
    }
    Ok(out)
}

/// Pull a reading out of the `/api/usage` envelope (Anthropic's body plus
/// `account_email`). None unless it is a live, available reading with an
/// account attached.
pub fn reading_from_usage(body: &Value) -> Option<Reading> {
    if body.get("available") != Some(&json!(true)) || body.get("stale") == Some(&json!(true)) {
        return None;
    }
    let account = body.get("account_email")?.as_str()?.to_string();
    let window = |k: &str| -> (Option<f64>, Option<String>) {
        let w = body.get(k);
        (
            w.and_then(|w| w.get("utilization")).and_then(Value::as_f64),
            w.and_then(|w| w.get("resets_at")).and_then(Value::as_str).map(str::to_string),
        )
    };
    let (five, five_reset) = window("five_hour");
    let (week, week_reset) = window("seven_day");
    if five.is_none() && week.is_none() {
        return None;
    }
    Some(Reading {
        account,
        source: "server-probe".into(),
        at: body.get("observed_at").and_then(Value::as_f64),
        five_hour_pct: five,
        five_hour_resets_at: five_reset,
        seven_day_pct: week,
        seven_day_resets_at: week_reset,
        plan: None,
    })
}

/// Store a reading from inside the server. Failures are logged, never raised:
/// the caller is serving a request that must not fail over history.
pub async fn record_logged(state: &AppState, r: Reading) {
    let at_now = now();
    if let Err(why) = validate(&r, at_now) {
        tracing::warn!(target: "amux::claude_usage", verdict = "claude_usage_reading_refused",
            account = %r.account, source = %r.source, reason = %why, "claude account usage reading refused");
        return;
    }
    let account = r.account.clone();
    let res = state
        .store
        .write_async(move |c| {
            record(c, &r, at_now)?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    if let Err(e) = res {
        tracing::warn!(target: "amux::claude_usage", verdict = "claude_usage_record_failed",
            account = %account, error = %e, "could not store claude account usage reading");
    }
}

pub async fn post_reading(State(state): State<AppState>, Json(r): Json<Reading>) -> Response {
    let at_now = now();
    if let Err(why) = validate(&r, at_now) {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": why}))).into_response();
    }
    let slot: std::sync::Arc<std::sync::Mutex<Option<Option<&'static str>>>> = Default::default();
    let s2 = slot.clone();
    let account = r.account.clone();
    let res = state
        .store
        .write_async(move |c| {
            let skipped = record(c, &r, at_now)?;
            *s2.lock().unwrap_or_else(|e| e.into_inner()) = Some(skipped);
            Ok(crate::db::WriteOutcome { applied: skipped.is_none(), events: vec![] })
        })
        .await;
    let skipped = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    match (res, skipped) {
        (Ok(_), Some(skipped)) => Json(json!({
            "ok": true,
            "recorded": skipped.is_none(),
            "skipped": skipped,
        }))
        .into_response(),
        (res, _) => {
            let error = res.err().map(|e| e.to_string()).unwrap_or_else(|| "no result".into());
            tracing::warn!(target: "amux::claude_usage", verdict = "claude_usage_record_failed",
                account = %account, error = %error, "could not store claude account usage reading");
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ok": false, "error": error}))).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SummaryParams {
    since_h: Option<f64>,
    account: Option<String>,
    rows: Option<usize>,
}

pub async fn get_accounts(State(state): State<AppState>, Query(p): Query<SummaryParams>) -> Response {
    let since_s = p.since_h.unwrap_or(168.0).clamp(0.1, 90.0 * 24.0) * 3600.0;
    let rows = p.rows.unwrap_or(0).min(5000);
    let account = p.account.filter(|a| !a.trim().is_empty());
    let res = state
        .store
        .read_async(move |c| Ok(summary(c, since_s, account.as_deref(), rows, now())?))
        .await;
    match res {
        Ok(v) => Json(v).into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"measured": false, "n_considered": 0, "why_unmeasured": e.to_string()})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(include_str!("../../migrations/0129_claude_account_usage.sql")).unwrap();
        c
    }

    fn reading(at: f64, week: f64, reset: &str) -> Reading {
        Reading {
            account: "a@example.com".into(),
            source: "usage-probe".into(),
            at: Some(at),
            seven_day_pct: Some(week),
            seven_day_resets_at: Some(reset.into()),
            five_hour_pct: Some(10.0),
            five_hour_resets_at: Some("2026-10-20T17:00:00+00:00".into()),
            plan: Some("Max (20x)".into()),
        }
    }

    fn count(c: &Connection) -> i64 {
        c.query_row("SELECT count(*) FROM claude_account_usage", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn unchanged_readings_are_kept_once_an_hour() {
        let c = db();
        let t = 1_792_500_000.0;
        let reset = "2026-10-24T20:59:00-04:00";
        assert_eq!(record(&c, &reading(t, 40.0, reset), t).unwrap(), None);
        assert_eq!(record(&c, &reading(t + 60.0, 40.0, reset), t + 60.0).unwrap(), Some("unchanged"));
        // Same window spelled with API precision in UTC is still unchanged.
        assert_eq!(
            record(&c, &reading(t + 120.0, 40.2, "2026-10-25T00:59:00.918+00:00"), t + 120.0).unwrap(),
            Some("unchanged")
        );
        assert_eq!(record(&c, &reading(t + HEARTBEAT_S + 1.0, 40.0, reset), t + HEARTBEAT_S + 1.0).unwrap(), None);
        assert_eq!(record(&c, &reading(t + HEARTBEAT_S + 2.0, 41.0, reset), t + HEARTBEAT_S + 2.0).unwrap(), None);
        // A re-sent reading (rotator backfill) is not stored twice.
        assert_eq!(record(&c, &reading(t, 40.0, reset), t + 5000.0).unwrap(), Some("already_recorded"));
        assert_eq!(count(&c), 3);
    }

    #[test]
    fn summary_reports_burn_rate_within_the_current_window() {
        let c = db();
        let t = 1_792_500_000.0;
        // Old window: must not count toward the burn.
        record(&c, &reading(t - 7200.0, 99.0, "2026-10-19T20:59:00-04:00"), t).unwrap();
        record(&c, &reading(t, 10.0, "2026-10-24T20:59:00-04:00"), t).unwrap();
        record(&c, &reading(t + 7200.0, 20.0, "2026-10-24T20:59:00-04:00"), t + 7200.0).unwrap();
        let s = summary(&c, 86400.0, None, 10, t + 7200.0).unwrap();
        assert_eq!(s["measured"], json!(true));
        assert_eq!(s["n_considered"], json!(3));
        let a = &s["accounts"][0];
        assert_eq!(a["account"], json!("a@example.com"));
        assert_eq!(a["seven_day_burn_pct_per_h"], json!(5.0));
        assert_eq!(a["seven_day_hours_to_full"], json!(16.0));
        assert_eq!(a["latest"]["seven_day_pct"], json!(20.0));
        assert_eq!(s["rows"].as_array().unwrap().len(), 3);
        assert_eq!(s["rows"][0]["seven_day_pct"], json!(20.0));
    }

    #[test]
    fn prunes_rows_past_retention() {
        let c = db();
        let t = 1_792_500_000.0;
        record(&c, &reading(t - RETAIN_S - 10.0, 5.0, "2026-07-01T00:00:00+00:00"), t - RETAIN_S - 10.0).unwrap();
        record(&c, &reading(t, 6.0, "2026-10-24T20:59:00-04:00"), t).unwrap();
        assert_eq!(count(&c), 1);
    }

    #[test]
    fn validate_refuses_readings_that_cannot_be_used() {
        let t = 1_792_500_000.0;
        let mut r = reading(t, 10.0, "2026-10-24T20:59:00-04:00");
        assert!(validate(&r, t).is_ok());
        r.account = "nobody".into();
        assert!(validate(&r, t).is_err());
        let mut r = reading(t + 3600.0, 10.0, "x");
        assert!(validate(&r, t).is_err());
        r.at = Some(t);
        r.five_hour_pct = None;
        r.seven_day_pct = None;
        assert!(validate(&r, t).is_err());
    }

    #[test]
    fn server_probe_envelope_becomes_a_reading() {
        let body = json!({
            "available": true,
            "account_email": "ethan@example.com",
            "observed_at": 1_792_504_126,
            "five_hour": {"utilization": 24.0, "resets_at": "2026-10-20T16:59:59.918482+00:00"},
            "seven_day": {"utilization": 7.0, "resets_at": "2026-10-26T22:59:59.918507+00:00"},
        });
        let r = reading_from_usage(&body).unwrap();
        assert_eq!(r.account, "ethan@example.com");
        assert_eq!(r.source, "server-probe");
        assert_eq!(r.seven_day_pct, Some(7.0));
        assert_eq!(r.at, Some(1_792_504_126.0));
        let mut stale = body.clone();
        stale["stale"] = json!(true);
        assert!(reading_from_usage(&stale).is_none());
        let mut anon = body;
        anon.as_object_mut().unwrap().remove("account_email");
        assert!(reading_from_usage(&anon).is_none());
    }
}
