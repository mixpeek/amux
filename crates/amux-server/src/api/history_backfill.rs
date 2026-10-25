//! POST /api/history/backfill: rebuild `cmd_history` rows that were dropped
//! while their deliveries survived in `session_events`.
//!
//! Why it exists: until 2026-10-18 the send path kept only the newest 200
//! `cmd_history` rows per session (see `runtime_jobs/message_capture.rs`). A
//! busy worker lost owner messages from the Messages view while the delivery
//! events stayed in `session_events`. Measured on mxp-gs12: 114 owner chat
//! messages from 2026-10-08 to 2026-10-12 had a `chat.message` event and no row.
//!
//! Two event shapes carry a message's full text, so only those are sources:
//! - `chat.message` with `role=user`, `origin=owner`, stored under the Chat tab
//!   key `<session>@chat`. The live path records these as `type=chat`,
//!   `origin=chat-companion` (`chat_worker::companion_route`).
//! - `message.sent` with `human=true` under `<session>`, when its `preview`
//!   holds the WHOLE text (`chars` equals the preview's length). The live path
//!   records these as `type=user`. A preview cut at 120 chars is counted as
//!   `skipped_truncated` and never written: a row holding a fragment would read
//!   as the message the owner sent.
//!
//! Idempotent in two ways. A backfilled row carries the source event id in
//! `client_meta.backfill_event_id`, so a second run finds it. A live row that
//! survived counts as already present when it has the same type and the same
//! masked text within a window around the event (the chat event is emitted
//! when the turn starts, which can be well after the send). Each live row
//! matches at most one event, so a repeated "continue" is not swallowed.
//!
//! Text is masked with `history::redact_secrets` before it is compared or
//! stored. Logs and the response carry counts and event ids only.
//!
//! DRY RUN BY DEFAULT, owner-only `apply`, mirroring `/redact-stored`.

use super::AppState;
use crate::db::{PendingEvent, WriteOutcome};
use amux_core::revision::{EntityType, MutationKind};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// A live row may predate its event by this much (queued behind a turn).
const MATCH_BEFORE_MS: i64 = 1_800_000;
/// ...or follow it by this much (the row is written after delivery returns).
const MATCH_AFTER_MS: i64 = 120_000;
/// One call covers at most this many seconds, so a typo cannot sweep a year.
const MAX_RANGE_S: f64 = 31.0 * 86_400.0;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BackfillRow {
    pub event_id: i64,
    pub event_type: String,
    pub ts_ms: i64,
    pub text: String,
    pub htype: &'static str,
    pub origin: &'static str,
    pub delivery: Option<&'static str>,
}

/// Parse `since`/`until`: epoch seconds, RFC 3339, or the short UTC forms
/// `2026-10-08T00:00Z`, `2026-10-08T00:00:00Z` and `2026-10-08`.
pub(crate) fn parse_instant(v: &Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    let s = v.as_str()?.trim();
    if let Ok(n) = s.parse::<f64>() {
        return Some(n);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis() as f64 / 1000.0);
    }
    for fmt in ["%Y-%m-%dT%H:%MZ", "%Y-%m-%dT%H:%M:%SZ", "%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt.and_utc().timestamp() as f64);
        }
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp() as f64)
}

fn masked(text: &str) -> String {
    super::history::redact_secrets(text).0
}

/// Work out which rows are missing. Reads only; returns the report and the rows.
pub(crate) fn plan_backfill(
    conn: &rusqlite::Connection,
    session: &str,
    since_s: f64,
    until_s: f64,
) -> rusqlite::Result<(Value, Vec<BackfillRow>)> {
    let chat_key = super::chat_worker::companion_key(session);
    let mut st = conn.prepare(
        "SELECT id, ts, type, data FROM session_events \
         WHERE ((session=?1 AND type='chat.message') OR (session=?2 AND type='message.sent')) \
           AND ts>=?3 AND ts<?4 ORDER BY ts, id",
    )?;
    let events: Vec<(i64, f64, String, Option<String>)> = st
        .query_map(rusqlite::params![chat_key, session, since_s, until_s], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    // Live rows near the range, with any backfill marker they carry.
    let lo = ((since_s * 1000.0) as i64).saturating_sub(MATCH_BEFORE_MS);
    let hi = ((until_s * 1000.0) as i64).saturating_add(MATCH_AFTER_MS);
    let mut st = conn.prepare(
        "SELECT ts, type, text, json_extract(client_meta,'$.backfill_event_id') FROM cmd_history \
         WHERE session=?1 AND ts>=?2 AND ts<=?3",
    )?;
    struct Live {
        ts: i64,
        htype: String,
        text: String,
        claimed: bool,
    }
    let mut live: Vec<Live> = Vec::new();
    let mut backfilled: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for row in st.query_map(rusqlite::params![session, lo, hi], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<i64>>(3)?))
    })? {
        let (ts, htype, text, marker) = row?;
        match marker {
            Some(id) => {
                backfilled.insert(id);
            }
            None => live.push(Live { ts, htype, text: masked(&text), claimed: false }),
        }
    }

    let (mut chat_n, mut sent_n, mut present, mut already, mut truncated, mut other, mut dup_events) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut seen_msg_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut rows: Vec<BackfillRow> = Vec::new();
    for (id, ts, etype, data) in &events {
        let d: Value = data.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
        let (text, htype, origin, delivery) = if etype == "chat.message" {
            if d.get("role").and_then(Value::as_str) != Some("user") {
                continue; // assistant replies were never cmd_history rows
            }
            if d.get("origin").and_then(Value::as_str) != Some("owner") {
                other += 1;
                continue;
            }
            if let Some(m) = d.get("msg_id").and_then(Value::as_str).filter(|m| !m.is_empty()) {
                if !seen_msg_ids.insert(m.to_string()) {
                    dup_events += 1;
                    continue;
                }
            }
            chat_n += 1;
            let Some(t) = d.get("text").and_then(Value::as_str) else {
                other += 1;
                continue;
            };
            (t.to_string(), "chat", "chat-companion", None)
        } else {
            if d.get("human").and_then(Value::as_bool) != Some(true) {
                continue; // machine sends were never recorded as messages
            }
            sent_n += 1;
            let preview = d.get("preview").and_then(Value::as_str).unwrap_or("");
            let chars = d.get("chars").and_then(Value::as_u64);
            if preview.is_empty() || chars != Some(preview.chars().count() as u64) {
                truncated += 1;
                continue;
            }
            (preview.to_string(), "user", "", Some("direct"))
        };
        if backfilled.contains(id) {
            already += 1;
            continue;
        }
        let ts_ms = (ts * 1000.0) as i64;
        let text = masked(&text);
        let best = live
            .iter_mut()
            .filter(|l| {
                !l.claimed
                    && l.htype == htype
                    && l.ts >= ts_ms - MATCH_BEFORE_MS
                    && l.ts <= ts_ms + MATCH_AFTER_MS
                    && l.text == text
            })
            .min_by_key(|l| (l.ts - ts_ms).abs());
        if let Some(l) = best {
            l.claimed = true;
            present += 1;
            continue;
        }
        rows.push(BackfillRow { event_id: *id, event_type: etype.clone(), ts_ms, text, htype, origin, delivery });
    }
    let sample: Vec<i64> = rows.iter().take(20).map(|r| r.event_id).collect();
    let report = json!({
        "measured": true,
        "session": session,
        "since": since_s,
        "until": until_s,
        "n_considered": events.len(),
        "candidates": {"chat_owner": chat_n, "sent_human": sent_n},
        "already_present": present,
        "already_backfilled": already,
        "skipped_truncated": truncated,
        "skipped_other": other,
        "duplicate_events": dup_events,
        "rows_missing": rows.len(),
        "sample_event_ids": sample,
    });
    Ok((report, rows))
}

/// Insert the planned rows. Re-checks the backfill marker inside the write so
/// two concurrent runs cannot both insert the same event.
pub(crate) fn apply_backfill(
    conn: &rusqlite::Connection,
    session: &str,
    rows: &[BackfillRow],
) -> rusqlite::Result<usize> {
    let now = chrono::Utc::now().timestamp();
    let mut n = 0usize;
    for r in rows {
        let meta = json!({
            "backfill": "session_events",
            "backfill_event_id": r.event_id,
            "backfill_event_type": r.event_type,
            "backfilled_at": now,
        })
        .to_string();
        n += conn.execute(
            "INSERT INTO cmd_history (text, type, session, ts, origin, delivery, delivered_at, capture_pending, client_meta) \
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?4, 0, ?7 \
             WHERE NOT EXISTS (SELECT 1 FROM cmd_history WHERE session=?3 \
                 AND json_extract(client_meta,'$.backfill_event_id')=?8)",
            rusqlite::params![r.text, r.htype, session, r.ts_ms, r.origin, r.delivery, meta, r.event_id],
        )?;
    }
    Ok(n)
}

fn bad(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response()
}

/// POST /api/history/backfill `{session, since, until, apply?}`.
pub(crate) async fn backfill(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|b| b.0).unwrap_or(Value::Null);
    let apply = body.get("apply").and_then(Value::as_bool).unwrap_or(false);
    let session = body.get("session").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if session.is_empty() || !super::session_verbs::valid_session_name(&session) {
        return bad("session required: the worker whose Messages lost rows");
    }
    let (Some(since), Some(until)) = (
        body.get("since").and_then(parse_instant),
        body.get("until").and_then(parse_instant),
    ) else {
        return bad("since and until required: epoch seconds or ISO-8601 UTC, e.g. 2026-10-08T00:00Z");
    };
    if until <= since || until - since > MAX_RANGE_S {
        return bad("until must be after since, and the range at most 31 days");
    }
    let caller = super::session_verbs::hdr_worker(&headers);
    if apply && !super::standing_approvals::is_owner_request(&headers) {
        tracing::warn!(caller = %caller, measured = true, n_considered = 0, verdict = "history_backfill_refused",
            "a worker asked to backfill message history; only the owner may apply this");
        return (StatusCode::FORBIDDEN, Json(json!({
            "error": "only the owner may apply a history backfill",
            "code": "owner_only",
            "how": "Run it without apply to see the counts; the owner applies it from a shell with no X-Amux-Session header.",
        }))).into_response();
    }
    let s = session.clone();
    let planned = state.store.read_async(move |conn| Ok(plan_backfill(conn, &s, since, until)?)).await;
    let (mut report, rows) = match planned {
        Ok(v) => v,
        Err(e) => return super::internal(e),
    };
    let missing = rows.len();
    let considered = report["n_considered"].as_u64().unwrap_or(0);
    if !apply {
        tracing::info!(session = %session, measured = true, n_considered = considered, rows_missing = missing,
            already_present = report["already_present"].as_u64().unwrap_or(0),
            already_backfilled = report["already_backfilled"].as_u64().unwrap_or(0),
            skipped_truncated = report["skipped_truncated"].as_u64().unwrap_or(0),
            verdict = "history_backfill_dry_run",
            "counted message history rows that session_events can rebuild; nothing changed");
        report["applied"] = json!(false);
        return Json(report).into_response();
    }
    let inserted = Arc::new(Mutex::new(0usize));
    let sink = inserted.clone();
    let s = session.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let n = apply_backfill(conn, &s, &rows)?;
            *sink.lock().unwrap_or_else(|e| e.into_inner()) = n;
            Ok(WriteOutcome {
                applied: n > 0,
                events: vec![PendingEvent {
                    entity_type: EntityType::Message,
                    entity_id: format!("backfill:{s}"),
                    mutation: MutationKind::Created,
                    payload: None,
                }],
            })
        })
        .await;
    if let Err(e) = write {
        return super::internal(e);
    }
    let n = *inserted.lock().unwrap_or_else(|e| e.into_inner());
    tracing::warn!(session = %session, measured = true, n_considered = considered, rows_missing = missing,
        rows_inserted = n, verdict = "history_backfill_applied",
        "owner applied a message history backfill from session_events");
    report["applied"] = json!(true);
    report["rows_inserted"] = json!(n);
    Json(report).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "gs-fixture";

    fn event(c: &rusqlite::Connection, session: &str, ts: f64, etype: &str, data: Value) -> i64 {
        c.execute(
            "INSERT INTO session_events (ts, session, type, data, source) VALUES (?1, ?2, ?3, ?4, 'test')",
            rusqlite::params![ts, session, etype, data.to_string()],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    fn chat(c: &rusqlite::Connection, ts: f64, text: &str) -> i64 {
        event(c, &format!("{S}@chat"), ts, "chat.message",
            json!({"role": "user", "origin": "owner", "text": text, "msg_id": format!("m{ts}")}))
    }

    fn rows(c: &rusqlite::Connection) -> Vec<(String, String, i64, String, Option<i64>)> {
        let mut st = c
            .prepare("SELECT text, type, ts, origin, json_extract(client_meta,'$.backfill_event_id') FROM cmd_history WHERE session=?1 ORDER BY ts, id")
            .unwrap();
        st.query_map([S], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// The four promises in one fixture: missing rows go in, a surviving row is
    /// not duplicated, a rerun inserts 0, and a pasted secret is masked.
    #[test]
    fn backfill_inserts_missing_skips_present_is_idempotent_and_masks() {
        let c = crate::db::migrate::test_memdb();
        let t0 = 1_791_500_000.0;
        // Survived the cap: a live row written 40s before its chat event.
        c.execute(
            "INSERT INTO cmd_history (text, type, session, ts, origin) VALUES ('keep going', 'chat', ?1, ?2, 'chat-companion')",
            rusqlite::params![S, ((t0 - 40.0) * 1000.0) as i64],
        )
        .unwrap();
        chat(&c, t0, "keep going");
        let lost = chat(&c, t0 + 100.0, "ship the parser fix");
        // The same short text again later is a SECOND message, not a match.
        let again = chat(&c, t0 + 600.0, "keep going");
        let token = ["ghp_", &"A".repeat(36)].concat();
        let secret = chat(&c, t0 + 200.0, &format!("use {token} for the push"));
        // An owner direct send whose preview is the whole text, and one cut short.
        let sent = event(&c, S, t0 + 300.0, "message.sent", json!({"chars": 5, "preview": "hello", "human": true}));
        event(&c, S, t0 + 400.0, "message.sent", json!({"chars": 300, "preview": "x".repeat(120), "human": true}));
        // Never messages: machine sends, assistant replies, out of range, other session.
        event(&c, S, t0 + 500.0, "message.sent", json!({"chars": 3, "preview": "abc", "human": false}));
        event(&c, &format!("{S}@chat"), t0 + 10.0, "chat.message", json!({"role": "assistant", "text": "ok"}));
        chat(&c, t0 + 90_000.0, "outside the range");
        event(&c, "other@chat", t0 + 5.0, "chat.message", json!({"role": "user", "origin": "owner", "text": "not mine"}));

        let (report, plan) = plan_backfill(&c, S, t0 - 10.0, t0 + 10_000.0).unwrap();
        assert_eq!(report["measured"], json!(true));
        assert_eq!(report["already_present"], json!(1), "{report}");
        assert_eq!(report["skipped_truncated"], json!(1), "{report}");
        assert_eq!(report["rows_missing"], json!(4), "{report}");
        let ids: Vec<i64> = plan.iter().map(|r| r.event_id).collect();
        assert_eq!(ids, vec![lost, secret, sent, again]);

        assert_eq!(apply_backfill(&c, S, &plan).unwrap(), 4);
        let got = rows(&c);
        assert_eq!(got.len(), 5, "{got:?}");
        let masked_row = got.iter().find(|r| r.4 == Some(secret)).unwrap();
        assert!(!masked_row.0.contains(&token) && masked_row.0.contains("REDACTED"), "secret stored: {}", masked_row.0);
        let user = got.iter().find(|r| r.4 == Some(sent)).unwrap();
        assert_eq!((user.0.as_str(), user.1.as_str()), ("hello", "user"));
        let lost_row = got.iter().find(|r| r.4 == Some(lost)).unwrap();
        assert_eq!((lost_row.1.as_str(), lost_row.3.as_str(), lost_row.2), ("chat", "chat-companion", ((t0 + 100.0) * 1000.0) as i64));
        let backlog: i64 = c.query_row("SELECT COUNT(*) FROM cmd_history WHERE capture_pending!=0", [], |r| r.get(0)).unwrap();
        assert_eq!(backlog, 0, "old messages must not enter board capture");

        // Second run: nothing missing, nothing inserted, even replaying the old plan.
        let (report, plan2) = plan_backfill(&c, S, t0 - 10.0, t0 + 10_000.0).unwrap();
        assert_eq!(report["rows_missing"], json!(0), "{report}");
        assert_eq!(report["already_backfilled"], json!(4), "{report}");
        assert!(plan2.is_empty());
        assert_eq!(apply_backfill(&c, S, &plan).unwrap(), 0, "a stale plan must not duplicate");
        assert_eq!(rows(&c).len(), 5);
    }

    #[test]
    fn parse_instant_accepts_the_short_utc_form() {
        assert_eq!(parse_instant(&json!("2026-10-08T00:00Z")), Some(1_791_417_600.0));
        assert_eq!(parse_instant(&json!("2026-10-13T00:00:00Z")), Some(1_791_849_600.0));
        assert_eq!(parse_instant(&json!(1_791_417_600)), Some(1_791_417_600.0));
        assert_eq!(parse_instant(&json!("nonsense")), None);
    }
}
