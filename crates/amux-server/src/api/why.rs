//! RR-0109 — generalized `why` query system (Invariant 24).
//!
//! "Why did this happen?" answered from the durable trails amux already keeps,
//! never from grep over a log file: the state-event journal with its
//! post-mutation payload snapshots (`_amux_state_events`, see `db/replay.rs`),
//! the structured request log (`_amux_request_log`), board card history
//! (`issues.log`), schedule runs and audit, the turn ledger (`_amux_turns`),
//! commands, sessions, and the interaction log.
//!
//! `GET /api/why/{kind}/{id}` for `task` | `worker` | `command` | `schedule` |
//! `session` | `integration`, and `GET /api/why?since=&until=` for a window.
//! The CLI verb (`amux-rs why …`) is a printer over this endpoint and does not
//! re-implement any of the correlation — one place to be wrong is enough.
//!
//! ## This is an instrument, so it must be able to say "I cannot tell"
//!
//! Ethos rule 4: a diagnosis being impossible from the available data IS the
//! bug, and the failure mode of an explainer is to narrate confidently from
//! whatever it happened to find. Three things guard against that:
//!
//! 1. **Every timeline line cites its source** — table plus a row reference
//!    (`rowid`, `rev`) — so any claim can be re-checked with one SELECT.
//! 2. **Every source is reported even when it returned nothing**, with the
//!    predicate it ran. A zero from a probe that COULD have matched and a zero
//!    from a probe that never could look identical otherwise, and only the
//!    second is a gap.
//! 3. **`verdict` is `cannot_tell` when the evidence does not support a
//!    story**, and `gaps` names what is missing. `partial` is the answer when
//!    some trail covers the entity and another structurally cannot.
//!
//! ## Timestamp units differ per table, and getting that wrong is the classic
//! silent-wrong-answer here
//!
//! `issues.created/updated`, `schedule_runs.ran_at`, `logs.ts` are unix
//! SECONDS. `_amux_request_log.ts` and `session_events.ts` are REAL unix
//! seconds. `interaction_log.ts` is MILLISECONDS (ethos.md records two
//! sessions in one evening writing `datetime(ts,'unixepoch')` against it and
//! getting a filter ~1000x too small, which matched the entire table while
//! looking like a correct answer). Each read below converts explicitly and
//! says which unit it assumed.

use super::AppState;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(why_window))
        .route("/contract", get(contract))
        .route("/{kind}/{id}", get(why_entity))
}

/// Row shapes for the per-kind subject SELECTs. Named because a bare 9-tuple
/// in a signature is a place for two columns to swap silently.
type TaskRow = (String, String, Option<String>, String, Option<String>, i64, i64, Option<String>, i64);
type CommandRow = (String, String, String, String, i64, String, Option<String>);
type ScheduleRow = (String, String, String, i64, Option<String>, i64, i64);

/// Cap on rows pulled from any single source. Totals are counted separately
/// and reported exactly, so a truncated list never reads as a complete one.
const PER_SOURCE_CAP: usize = 200;

// ---------------------------------------------------------------------------
// Shapes
// ---------------------------------------------------------------------------

/// One thing that happened, with the row it came from.
#[derive(Debug, Clone, Serialize)]
pub struct WhyEvent {
    /// Human timestamp as recorded. `None` when the source records no
    /// resolvable time (see `card log` below) — never a guessed one.
    pub at: Option<String>,
    /// Epoch seconds, for ordering. `None` sorts to the end of the timeline
    /// and is why `ordering` exists.
    pub at_epoch: Option<i64>,
    /// `state_event`, `request`, `card_log`, `schedule_run`, `schedule_audit`,
    /// `turn`, `command`, `session`, `interaction`, `entity`.
    pub kind: String,
    /// Who or what caused it, when the row records that. `None` is honest: an
    /// unattributed write is a real thing in this system and inventing an
    /// actor for it would hide exactly the problem worth seeing.
    pub actor: Option<String>,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
    /// `table` + a reference that makes the claim checkable by hand.
    pub source: Value,
    /// `"timestamped"` or `"append-order"` — how this line's position in the
    /// timeline was decided.
    pub ordering: &'static str,
}

/// A trail that was consulted, whether or not it had anything to say.
#[derive(Debug, Clone, Serialize)]
pub struct SourceProbe {
    pub table: String,
    /// The predicate actually run, so a reader can tell a zero that means
    /// "nothing happened" from a zero that means "this table cannot be
    /// correlated to this entity".
    pub query: String,
    pub rows: usize,
    /// Exact count before `PER_SOURCE_CAP` truncation.
    pub rows_total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

struct Why {
    subject: Value,
    found: bool,
    events: Vec<WhyEvent>,
    sources: Vec<SourceProbe>,
    gaps: Vec<String>,
}

impl Why {
    fn new(kind: &str, id: &str) -> Self {
        Why {
            subject: json!({ "kind": kind, "id": id }),
            found: false,
            events: Vec::new(),
            sources: Vec::new(),
            gaps: Vec::new(),
        }
    }

    fn probe(&mut self, table: &str, query: String, rows: usize, total: usize, note: Option<String>) {
        self.sources.push(SourceProbe {
            table: table.to_string(),
            query,
            rows,
            rows_total: total,
            note,
        });
    }

    fn gap(&mut self, g: impl Into<String>) {
        self.gaps.push(g.into());
    }

    fn finish(mut self) -> Value {
        // Timestamped events first, in time order; undated ones keep their
        // append order at the end. A stable sort is what preserves that.
        self.events.sort_by_key(|e| (e.at_epoch.is_none(), e.at_epoch.unwrap_or(i64::MAX)));

        let evidence_rows: usize = self.sources.iter().map(|s| s.rows_total).sum();
        let structural_gaps = !self.gaps.is_empty();
        // Two distinct ways to reach `cannot_tell` — the subject does not
        // exist, or it exists and no trail has a row for it. They share a
        // verdict but NOT a reason, which is why `verdict_reason` below
        // branches on `self.found` separately: collapsing them into one
        // string would hide which of the two a reader is looking at.
        let verdict = if !self.found || evidence_rows == 0 {
            "cannot_tell"
        } else if structural_gaps {
            "partial"
        } else {
            "explained"
        };
        // The one sentence a reader should be able to act on. It is derived
        // from the verdict, never from a summarizing model call.
        let verdict_reason = match verdict {
            "cannot_tell" if !self.found => {
                "the subject was not found in its own table, so nothing downstream can be attributed to it"
            }
            "cannot_tell" => {
                "the subject exists but every durable trail returned zero rows for it — see `sources` for the predicates that were run"
            }
            "partial" => "some trails cover this entity and others structurally cannot — see `gaps`",
            _ => "every trail consulted covers this entity",
        };
        json!({
            "subject": self.subject,
            "found": self.found,
            "verdict": verdict,
            "verdict_reason": verdict_reason,
            "timeline": self.events,
            "sources": self.sources,
            "gaps": self.gaps,
            "per_source_cap": PER_SOURCE_CAP,
        })
    }
}

use super::internal;

fn epoch_to_rfc3339(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| format!("epoch:{secs}"))
}

/// Parse the `at` column of `_amux_state_events` (RFC3339) to epoch seconds.
fn rfc3339_to_epoch(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp())
}

// ---------------------------------------------------------------------------
// Shared collectors
// ---------------------------------------------------------------------------

/// The state-event journal for one entity. This is the highest-quality trail
/// in the system: revisioned, ordered, and (since migration 0008) carrying the
/// post-mutation snapshot, so a transition can be stated as `field: a -> b`
/// rather than "something changed".
///
/// `created_epoch` is the entity's own birth time, and it is what makes the
/// retention horizon expressible. The journal is REAPED — 14 days by default
/// (`AMUX_STATE_EVENTS_RETAIN_DAYS`, `runtime_jobs/storage.rs`) — so its floor
/// is a MOVING horizon, not the beginning of time. Pass `None` only when the
/// entity genuinely has no creation timestamp; passing it means the difference
/// between "here is this card's history" and "here is the surviving tail of
/// it", which have identical shapes and opposite meanings.
fn collect_state_events(
    conn: &Connection,
    w: &mut Why,
    entity_tag: &str,
    entity_id: &str,
    created_epoch: Option<i64>,
) -> rusqlite::Result<()> {
    let q = format!("entity_type='{entity_tag}' AND entity_id=?");
    let total: usize = conn.query_row(
        "SELECT COUNT(*) FROM _amux_state_events WHERE entity_type = ?1 AND entity_id = ?2",
        rusqlite::params![entity_tag, entity_id],
        |r| r.get::<_, i64>(0).map(|n| n as usize),
    )?;
    let mut stmt = conn.prepare(
        "SELECT id, rev, mutation, at, payload FROM _amux_state_events
         WHERE entity_type = ?1 AND entity_id = ?2 ORDER BY rev ASC, id ASC LIMIT ?3",
    )?;
    let rows: Vec<(i64, i64, String, String, Option<String>)> = stmt
        .query_map(rusqlite::params![entity_tag, entity_id, PER_SOURCE_CAP as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut prev: Option<Value> = None;
    let n = rows.len();
    let mut payloadless = 0usize;
    for (rowid, rev, mutation_raw, at, payload_raw) in rows {
        let mutation = serde_json::from_str::<Value>(&mutation_raw)
            .ok()
            .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
            .unwrap_or(mutation_raw);
        let payload: Option<Value> = payload_raw.as_deref().and_then(|p| serde_json::from_str(p).ok());
        if payload.is_none() {
            payloadless += 1;
        }
        // What actually changed, named field by field against the previous
        // snapshot. Without payloads this degrades to "it changed", which is
        // stated as such rather than dressed up.
        let (summary, detail) = match (&prev, &payload) {
            (Some(a), Some(b)) => {
                let changed = changed_fields(a, b);
                if changed.is_empty() {
                    (format!("{mutation} (no field differed from the previous snapshot)"), None)
                } else {
                    // The SUMMARY is a line a human reads; the DETAIL carries
                    // the untruncated values. A real card's `desc` runs to
                    // thousands of characters (measured on AMUX-2628: one
                    // status update was 4KB), and inlining it turns the
                    // timeline into a wall that hides the other events —
                    // which is the failure mode of an explainer that
                    // accumulates instead of discriminating.
                    let desc = changed
                        .iter()
                        .map(|(k, from, to)| format!("{k}: {} -> {}", abbrev(from), abbrev(to)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    (
                        format!("{mutation}: {desc}"),
                        Some(json!(changed
                            .iter()
                            .map(|(k, f, t)| json!({"field": k, "from": f, "to": t}))
                            .collect::<Vec<_>>())),
                    )
                }
            }
            (None, Some(b)) => (
                format!("{mutation} (first recorded snapshot)"),
                Some(b.clone()),
            ),
            _ => (
                format!("{mutation} (no snapshot recorded — what it changed INTO is not in the journal)"),
                None,
            ),
        };
        // The journal records the entity and the revision; it does not record
        // a caller. Attribution for a mutation comes from the request log,
        // correlated by time below.
        w.events.push(WhyEvent {
            at_epoch: rfc3339_to_epoch(&at),
            at: Some(at),
            kind: "state_event".into(),
            actor: None,
            summary,
            detail,
            source: json!({"table": "_amux_state_events", "rowid": rowid, "rev": rev}),
            ordering: "timestamped",
        });
        if payload.is_some() {
            prev = payload;
        }
    }

    // The journal's floor. Reported as a DATE, not only as a rev: a rev number
    // is unfalsifiable to a reader — it cannot be compared against anything they
    // know — while a date can be held against the card in front of them.
    let floor: Option<(i64, String)> = conn
        .query_row("SELECT MIN(rev), MIN(at) FROM _amux_state_events", [], |r| {
            Ok(match (r.get::<_, Option<i64>>(0)?, r.get::<_, Option<String>>(1)?) {
                (Some(rev), Some(at)) => Some((rev, at)),
                _ => None,
            })
        })
        .unwrap_or(None);
    let predates_floor = match (created_epoch, floor.as_ref()) {
        (Some(c), Some((_, at))) => rfc3339_to_epoch(at).is_some_and(|f| c < f),
        _ => false,
    };

    // A GAP is a hole in the evidence; a receipt is not. Both are reported, but
    // only the holes go in `gaps`, or the field stops meaning anything.
    let (gap_note, extra) = if total == 0 {
        // Distinguish "nothing ever happened" from "the journal does not go
        // back that far", which are different answers with the same row count.
        (
            Some(match &floor {
                Some((r, at)) => format!(
                    "no journal events for this entity; the journal is reaped and retains only from {at} (rev {r}), so anything before that is not enumerable"
                ),
                None => "the state-event journal is empty on this database".to_string(),
            }),
            None,
        )
    } else if predates_floor {
        // THE CASE THIS FUNCTION USED TO MISS, and the common one. The zero-row
        // arm above has always been careful; every other arm treated the rows it
        // got as the entity's whole history. For anything older than the
        // retention window that is false in the most misleading available way:
        // the trail renders, it is internally consistent, and it silently begins
        // partway through. A card created in June and closed in July shows its
        // last fortnight of touches and reads as though that is all that ever
        // happened to it.
        let (r, at) = floor.as_ref().expect("predates_floor implies a floor");
        (
            Some(format!(
                "these {n} events are the surviving TAIL of this entity's journal, not its history: the journal retains only from {at} (rev {r}) and this entity predates that floor. Everything older was reaped (AMUX_STATE_EVENTS_RETAIN_DAYS, default 14d) and is not recoverable from here"
            )),
            None,
        )
    } else {
        // A receipt: the entity was born inside the retained window, so for once
        // the journal really is complete. Worth stating — without it, a reader
        // who has learned to distrust the horizon cannot tell a complete trail
        // from a truncated one, and the fix for the silent case would make every
        // trail equally suspect.
        (
            None,
            floor.as_ref().map(|(_, at)| {
                format!("journal retained from {at}; this entity postdates that floor, so its journal history is complete")
            }),
        )
    };
    let payload_note = (payloadless > 0).then(|| format!(
        "{payloadless} of {n} events carry no post-mutation snapshot (pre-migration-0008 rows, or a write site that does not journal payloads) — those lines can say THAT the entity changed, not into what"
    ));
    // The payloadless caveat used to be an `else if`, so it was SUPPRESSED on
    // the zero-row arm and would now be suppressed on the truncation arm too.
    // Two independent caveats about the same source are two caveats.
    if let Some(nt) = &gap_note {
        w.gap(format!("_amux_state_events: {nt}"));
    }
    if let Some(nt) = &payload_note {
        w.gap(format!("_amux_state_events: {nt}"));
    }
    let note = match (gap_note, payload_note, extra) {
        (Some(a), Some(b), _) => Some(format!("{a}. {b}")),
        (Some(a), None, _) | (None, Some(a), _) => Some(a),
        (None, None, e) => e,
    };
    w.probe("_amux_state_events", q, n, total, note);
    Ok(())
}

/// Render a snapshot field for a one-line summary: short values verbatim,
/// long ones truncated WITH their full length stated, so the reader knows the
/// line was cut and by how much rather than silently seeing a prefix. The
/// untruncated value is always in the event's `detail`.
fn abbrev(v: &Value) -> String {
    const MAX: usize = 90;
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Null => "(absent)".to_string(),
        other => other.to_string(),
    };
    let flat = s.replace('\n', "⏎");
    if flat.chars().count() <= MAX {
        return format!("{flat:?}");
    }
    let head: String = flat.chars().take(MAX).collect();
    format!("{head:?}… ({} chars total, full value in `detail`)", s.chars().count())
}

/// Field-level diff of two snapshots, for the "a -> b" lines above.
fn changed_fields(a: &Value, b: &Value) -> Vec<(String, Value, Value)> {
    let (Some(a), Some(b)) = (a.as_object(), b.as_object()) else {
        return Vec::new();
    };
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        // `updated`/`rev` move on every write by construction; listing them
        // would bury the field that actually explains the change.
        .filter(|k| !matches!(k.as_str(), "updated" | "updated_at" | "rev" | "version"))
        .map(|k| {
            (
                k.clone(),
                a.get(k).cloned().unwrap_or(Value::Null),
                b.get(k).cloned().unwrap_or(Value::Null),
            )
        })
        .collect()
}

/// Requests that touched a path. This is the trail that carries ATTRIBUTION —
/// `amux_session` is the X-Amux-Session stamp, and its absence on a mutating
/// call is itself a finding worth surfacing, not something to paper over.
fn collect_requests(conn: &Connection, w: &mut Why, like: &str, label: &str) -> rusqlite::Result<()> {
    let q = format!("path LIKE '{like}'");
    let total: usize = conn.query_row(
        "SELECT COUNT(*) FROM _amux_request_log WHERE path LIKE ?1",
        [like],
        |r| r.get::<_, i64>(0).map(|n| n as usize),
    )?;
    let mut stmt = conn.prepare(
        "SELECT id, ts, method, path, status, amux_session, worker, answered_by, error_body, latency_ms
         FROM _amux_request_log WHERE path LIKE ?1 ORDER BY ts ASC LIMIT ?2",
    )?;
    let rows: Vec<WhyEvent> = stmt
        .query_map(rusqlite::params![like, PER_SOURCE_CAP as i64], |r| {
            let id: i64 = r.get(0)?;
            // `_amux_request_log.ts` is REAL unix SECONDS.
            let ts: f64 = r.get(1)?;
            let method: String = r.get(2)?;
            let path: String = r.get(3)?;
            let status: i64 = r.get(4)?;
            let session: Option<String> = r.get(5)?;
            let worker: Option<String> = r.get(6)?;
            let answered_by: String = r.get(7)?;
            let error_body: Option<String> = r.get(8)?;
            let latency: f64 = r.get(9)?;
            let secs = ts as i64;
            let mut summary = format!("{method} {path} -> {status}");
            if let Some(e) = &error_body {
                summary.push_str(&format!(" — {}", e.chars().take(160).collect::<String>()));
            }
            Ok(WhyEvent {
                at: Some(epoch_to_rfc3339(secs)),
                at_epoch: Some(secs),
                kind: "request".into(),
                // Unattributed calls stay None on purpose: "who did this" with
                // no answer is the finding.
                actor: session.clone().filter(|s| !s.is_empty()),
                summary,
                detail: Some(json!({
                    "method": method, "path": path, "status": status,
                    "amux_session": session, "worker": worker,
                    "answered_by": answered_by, "latency_ms": latency,
                })),
                source: json!({"table": "_amux_request_log", "rowid": id}),
                ordering: "timestamped",
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let n = rows.len();
    w.events.extend(rows);
    let note = if total == 0 {
        Some(format!(
            "no requests recorded for {label}; the request log only covers calls served by THIS server since migration 0010, so anything older, or a mutation made directly against the database, leaves no row here"
        ))
    } else {
        None
    };
    if let Some(nt) = &note {
        w.gap(format!("_amux_request_log: {nt}"));
    }
    w.probe("_amux_request_log", q, n, total, note);
    Ok(())
}

// ---------------------------------------------------------------------------
// task
// ---------------------------------------------------------------------------

fn why_task(conn: &Connection, id: &str) -> rusqlite::Result<Value> {
    let mut w = Why::new("task", id);
    let row: Option<TaskRow> = conn
        .query_row(
            "SELECT title, status, session, creator, log, created, updated, gate, archived
             FROM issues WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?,
                    r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?,
                ))
            },
        )
        .ok();

    let Some((title, status, session, creator, log, created, updated, gate, archived)) = row else {
        w.gap(format!("no row in `issues` with id {id} — it never existed, or it was hard-deleted (the board soft-deletes, so a hard delete means something outside the API removed it)"));
        w.probe("issues", format!("id='{id}'"), 0, 0, Some("subject not found".into()));
        return Ok(w.finish());
    };
    w.found = true;
    w.subject = json!({
        "kind": "task", "id": id, "title": title, "status": status,
        "session": session, "creator": creator, "gate": gate, "archived": archived,
    });
    w.probe("issues", format!("id='{id}'"), 1, 1, None);

    // `issues.created/updated` are unix SECONDS.
    w.events.push(WhyEvent {
        at: Some(epoch_to_rfc3339(created)),
        at_epoch: Some(created),
        kind: "entity".into(),
        actor: Some(creator.clone()).filter(|s| !s.is_empty()),
        summary: format!("card created: {title}"),
        detail: Some(json!({"creator": creator, "session": session})),
        source: json!({"table": "issues", "column": "created", "id": id}),
        ordering: "timestamped",
    });
    w.events.push(WhyEvent {
        at: Some(epoch_to_rfc3339(updated)),
        at_epoch: Some(updated),
        kind: "entity".into(),
        actor: None,
        summary: format!("last write to the row (status now `{status}`)"),
        detail: None,
        source: json!({"table": "issues", "column": "updated", "id": id}),
        ordering: "timestamped",
    });

    // The card log: the human-facing history. Append-only, one line each,
    // formatted `` `HH:MM` text `` — note there is NO DATE in it.
    let log_lines: Vec<&str> = log.as_deref().unwrap_or("").lines().filter(|l| !l.trim().is_empty()).collect();
    for (i, line) in log_lines.iter().take(PER_SOURCE_CAP).enumerate() {
        let (hhmm, text) = parse_log_line(line);
        w.events.push(WhyEvent {
            at: hhmm.map(|h| h.to_string()),
            // Deliberately None: the column stores HH:MM and no date, so an
            // epoch here would be invented. Reporting the gap is the honest
            // move; guessing "today" would make an old line look like a
            // recent one, which is precisely the kind of confident-wrong
            // ordering this endpoint exists to prevent.
            at_epoch: None,
            kind: "card_log".into(),
            actor: None,
            summary: text.to_string(),
            detail: None,
            source: json!({"table": "issues", "column": "log", "id": id, "line": i + 1}),
            ordering: "append-order",
        });
    }
    let log_note = if log_lines.is_empty() {
        Some("the card has no history lines".to_string())
    } else {
        Some(format!(
            "{} history lines; `issues.log` records HH:MM only, so these lines carry no date and are placed in append order at the end of the timeline, not interleaved by time",
            log_lines.len()
        ))
    };
    if let Some(nt) = &log_note {
        w.gap(format!("issues.log: {nt}"));
    }
    w.probe("issues.log", format!("id='{id}'"), log_lines.len().min(PER_SOURCE_CAP), log_lines.len(), log_note);

    collect_state_events(conn, &mut w, "task", id, Some(created))?;
    collect_requests(conn, &mut w, &format!("/api/board/{id}%"), &format!("card {id}"))?;

    // Turn ledger: which agent turns claimed this task.
    let total: usize = conn
        .query_row("SELECT COUNT(*) FROM _amux_turns WHERE task_id = ?1", [id], |r| {
            r.get::<_, i64>(0).map(|n| n as usize)
        })
        .unwrap_or(0);
    let mut stmt = conn.prepare(
        "SELECT id, worker_id, session_id, started_at, ended_at, outcome
         FROM _amux_turns WHERE task_id = ?1 ORDER BY started_at ASC LIMIT ?2",
    )?;
    let turns: Vec<WhyEvent> = stmt
        .query_map(rusqlite::params![id, PER_SOURCE_CAP as i64], |r| {
            let tid: String = r.get(0)?;
            let worker: String = r.get(1)?;
            let session: Option<String> = r.get(2)?;
            let started: String = r.get(3)?;
            let ended: Option<String> = r.get(4)?;
            let outcome: Option<String> = r.get(5)?;
            Ok(WhyEvent {
                at_epoch: rfc3339_to_epoch(&started),
                at: Some(started),
                kind: "turn".into(),
                actor: Some(worker.clone()),
                summary: match (&ended, &outcome) {
                    (Some(_), Some(o)) => format!("turn {tid} by {worker} ended: {o}"),
                    (Some(_), None) => format!("turn {tid} by {worker} ended with no recorded outcome"),
                    _ => format!("turn {tid} by {worker} is still running"),
                },
                detail: Some(json!({"turn_id": tid, "worker_id": worker, "session_id": session})),
                source: json!({"table": "_amux_turns", "id": tid}),
                ordering: "timestamped",
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let n = turns.len();
    w.events.extend(turns);
    let note = (total == 0).then(|| {
        "no turns reference this task; the turn ledger is written by the orchestrator, so work done by a session that was not driving this card through the orchestrator leaves nothing here".to_string()
    });
    if let Some(nt) = &note {
        w.gap(format!("_amux_turns: {nt}"));
    }
    w.probe("_amux_turns", format!("task_id='{id}'"), n, total, note);

    collect_interactions(conn, &mut w, id)?;
    Ok(w.finish())
}

/// `` `HH:MM` text `` -> (Some("HH:MM"), "text"). Anything else is returned
/// whole with no time, rather than being coerced into the expected shape.
fn parse_log_line(line: &str) -> (Option<&str>, &str) {
    let t = line.trim();
    if let Some(rest) = t.strip_prefix('`') {
        if let Some((hhmm, tail)) = rest.split_once('`') {
            return (Some(hhmm), tail.trim());
        }
    }
    (None, t)
}

/// `interaction_log` — the UI/agent interaction trail. **ts is MILLISECONDS**
/// on this table (unlike every other timestamp here).
fn collect_interactions(conn: &Connection, w: &mut Why, target: &str) -> rusqlite::Result<()> {
    let total: usize = conn
        .query_row("SELECT COUNT(*) FROM interaction_log WHERE target = ?1", [target], |r| {
            r.get::<_, i64>(0).map(|n| n as usize)
        })
        .unwrap_or(0);
    let mut stmt = conn.prepare(
        "SELECT id, ts, kind, actor, action, detail, ok, result FROM interaction_log
         WHERE target = ?1 ORDER BY ts ASC LIMIT ?2",
    )?;
    let rows: Vec<WhyEvent> = stmt
        .query_map(rusqlite::params![target, PER_SOURCE_CAP as i64], |r| {
            let id: i64 = r.get(0)?;
            let ts_ms: i64 = r.get(1)?;
            let kind: String = r.get(2)?;
            let actor: String = r.get(3)?;
            let action: String = r.get(4)?;
            let detail: String = r.get(5)?;
            let ok: i64 = r.get(6)?;
            let result: String = r.get(7)?;
            // MILLISECONDS -> seconds. Doing this in the wrong direction is
            // the documented way to produce a filter that matches everything.
            let secs = ts_ms / 1000;
            Ok(WhyEvent {
                at: Some(epoch_to_rfc3339(secs)),
                at_epoch: Some(secs),
                kind: "interaction".into(),
                actor: Some(actor).filter(|s| !s.is_empty()),
                summary: format!(
                    "{kind} {action}{}",
                    if ok == 1 { String::new() } else { format!(" FAILED: {result}") }
                ),
                detail: (!detail.is_empty()).then(|| json!({ "detail": detail })),
                source: json!({"table": "interaction_log", "rowid": id}),
                ordering: "timestamped",
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let n = rows.len();
    w.events.extend(rows);
    // AMUX-3526 / AF-154: a zero here is a claim about the CHANNEL far more
    // often than about the entity. Nothing has written kind='board' since
    // 2026-08-09 (792ce1f, the Python deletion) and 'scope' is the only kind
    // still produced, so for anything created since, this query returns an
    // empty set — and "no interaction rows target this entity" states that as
    // a fact about the entity. That is the wrong subject: an explanation
    // built from a dead source should say the source is dead, not that
    // nothing happened. Compounded by the reaper — storage.rs sweeps this
    // table at 14 days, so the surviving 08-09 rows are expiring now, after
    // which the table is EMPTY and this note would be wrong about everything.
    let note = if total > 0 {
        None
    } else {
        let (rows_anywhere, newest_ms): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(MAX(ts), 0) FROM interaction_log",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((0, 0));
        if rows_anywhere == 0 {
            Some(
                "interaction_log is EMPTY — this is a fact about the SOURCE, not about this \
                 entity. Nothing has written it since the Python server was deleted \
                 (2026-08-09) and the storage reaper expires what remained."
                    .to_string(),
            )
        } else {
            let newest = epoch_to_rfc3339(newest_ms / 1000);
            Some(format!(
                "no interaction rows target this entity — but the newest row in the whole \
                 table is {newest}, so this channel is stale and a zero here is weak \
                 evidence about the entity (AMUX-3526)."
            ))
        }
    };
    w.probe(
        "interaction_log",
        format!("target='{target}' (ts is MILLISECONDS on this table)"),
        n,
        total,
        note,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// worker
// ---------------------------------------------------------------------------

fn why_worker(conn: &Connection, key: &str) -> rusqlite::Result<Value> {
    let mut w = Why::new("worker", key);
    // Accept an id or a display name — `amux why worker backend` from the spec
    // is a NAME. A rename history lives in name_aliases, so an old name still
    // resolves (Invariant 17/43: renames must not orphan history).
    let resolved: Option<(String, String, String, String, String)> = conn
        .query_row(
            "SELECT id, display_name, cwd, provider, backend FROM _amux_workers
             WHERE id = ?1 OR display_name = ?1 OR name_aliases LIKE '%\"'||?1||'\"%' LIMIT 1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .ok();
    let Some((id, name, cwd, provider, backend)) = resolved else {
        w.gap(format!(
            "no worker matches `{key}` by id, display_name, or a recorded alias — the name may belong to a tmux-only session that was never registered in `_amux_workers`"
        ));
        w.probe("_amux_workers", format!("id/display_name/alias = '{key}'"), 0, 0, Some("subject not found".into()));
        return Ok(w.finish());
    };
    w.found = true;
    w.subject = json!({"kind": "worker", "id": id, "display_name": name, "cwd": cwd, "provider": provider, "backend": backend});
    w.probe("_amux_workers", format!("id/display_name/alias = '{key}'"), 1, 1, None);

    // `_amux_workers` records no creation time, so the truncation caveat cannot
    // be computed for a worker and the journal's floor is reported without a
    // verdict about it. Stating the limit rather than passing a guess: a
    // fabricated birth time would turn "unknown" into a confident "complete".
    collect_state_events(conn, &mut w, "worker", &id, None)?;

    // Sessions: every time this worker was actually running.
    let total: usize = conn
        .query_row("SELECT COUNT(*) FROM _amux_sessions WHERE worker_id = ?1", [&id], |r| {
            r.get::<_, i64>(0).map(|n| n as usize)
        })
        .unwrap_or(0);
    let mut stmt = conn.prepare(
        "SELECT id, backend, backend_ref, pid, started_at, ended_at, exit_reason
         FROM _amux_sessions WHERE worker_id = ?1 ORDER BY started_at ASC LIMIT ?2",
    )?;
    let rows: Vec<WhyEvent> = stmt
        .query_map(rusqlite::params![&id, PER_SOURCE_CAP as i64], |r| {
            let sid: String = r.get(0)?;
            let backend: String = r.get(1)?;
            let bref: String = r.get(2)?;
            let pid: Option<i64> = r.get(3)?;
            let started: String = r.get(4)?;
            let ended: Option<String> = r.get(5)?;
            let exit: Option<String> = r.get(6)?;
            Ok(WhyEvent {
                at_epoch: rfc3339_to_epoch(&started),
                at: Some(started),
                kind: "session".into(),
                actor: None,
                summary: match (&ended, &exit) {
                    (Some(e), Some(x)) => format!("session {sid} on {backend} ran until {e}, exit {x}"),
                    (Some(e), None) => format!("session {sid} on {backend} ended {e} with no recorded exit reason"),
                    _ => format!("session {sid} on {backend} is still live"),
                },
                detail: Some(json!({"backend_ref": bref, "pid": pid})),
                source: json!({"table": "_amux_sessions", "id": sid}),
                ordering: "timestamped",
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let n = rows.len();
    w.events.extend(rows);
    w.probe("_amux_sessions", format!("worker_id='{id}'"), n, total,
        (total == 0).then(|| "this worker has no recorded session; it was never started under this server, or it runs on a backend that does not register sessions".to_string()));

    collect_worker_commands(conn, &mut w, &id)?;
    collect_worker_turns(conn, &mut w, &id)?;
    collect_requests(conn, &mut w, &format!("/api/workers/{id}%"), &format!("worker {id}"))?;
    Ok(w.finish())
}

fn collect_worker_commands(conn: &Connection, w: &mut Why, worker_id: &str) -> rusqlite::Result<()> {
    let total: usize = conn
        .query_row("SELECT COUNT(*) FROM _amux_commands WHERE worker_id = ?1", [worker_id], |r| {
            r.get::<_, i64>(0).map(|n| n as usize)
        })
        .unwrap_or(0);
    let mut stmt = conn.prepare(
        "SELECT id, command, state, queued_at, attempts FROM _amux_commands
         WHERE worker_id = ?1 ORDER BY queued_at ASC LIMIT ?2",
    )?;
    let rows: Vec<WhyEvent> = stmt
        .query_map(rusqlite::params![worker_id, PER_SOURCE_CAP as i64], |r| {
            Ok(command_event(r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let n = rows.len();
    w.events.extend(rows);
    w.probe("_amux_commands", format!("worker_id='{worker_id}'"), n, total,
        (total == 0).then(|| "no commands were queued to this worker through the command table".to_string()));
    Ok(())
}

fn command_event(id: String, command: String, state: String, queued_at: String, attempts: i64) -> WhyEvent {
    let cmd_kind = serde_json::from_str::<Value>(&command)
        .ok()
        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
        .unwrap_or_else(|| "command".into());
    let state_kind = serde_json::from_str::<Value>(&state)
        .ok()
        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
        .unwrap_or_else(|| state.clone());
    WhyEvent {
        at_epoch: rfc3339_to_epoch(&queued_at),
        at: Some(queued_at),
        kind: "command".into(),
        actor: None,
        summary: format!("command {id} ({cmd_kind}) is {state_kind} after {attempts} attempt(s)"),
        detail: Some(json!({"command": command, "state": state})),
        source: json!({"table": "_amux_commands", "id": id}),
        ordering: "timestamped",
    }
}

fn collect_worker_turns(conn: &Connection, w: &mut Why, worker_id: &str) -> rusqlite::Result<()> {
    let total: usize = conn
        .query_row("SELECT COUNT(*) FROM _amux_turns WHERE worker_id = ?1", [worker_id], |r| {
            r.get::<_, i64>(0).map(|n| n as usize)
        })
        .unwrap_or(0);
    let mut stmt = conn.prepare(
        "SELECT id, task_id, started_at, ended_at, outcome, tokens FROM _amux_turns
         WHERE worker_id = ?1 ORDER BY started_at DESC LIMIT ?2",
    )?;
    let rows: Vec<WhyEvent> = stmt
        .query_map(rusqlite::params![worker_id, PER_SOURCE_CAP as i64], |r| {
            let tid: String = r.get(0)?;
            let task: Option<String> = r.get(1)?;
            let started: String = r.get(2)?;
            let ended: Option<String> = r.get(3)?;
            let outcome: Option<String> = r.get(4)?;
            let tokens: String = r.get(5)?;
            Ok(WhyEvent {
                at_epoch: rfc3339_to_epoch(&started),
                at: Some(started),
                kind: "turn".into(),
                actor: Some(worker_id.to_string()),
                summary: format!(
                    "turn {tid}{} {}",
                    task.as_ref().map(|t| format!(" on {t}")).unwrap_or_default(),
                    match (&ended, &outcome) {
                        (Some(_), Some(o)) => format!("ended: {o}"),
                        (Some(_), None) => "ended with no recorded outcome".into(),
                        _ => "is still running".into(),
                    }
                ),
                detail: Some(json!({"task_id": task, "tokens": tokens})),
                source: json!({"table": "_amux_turns", "id": tid}),
                ordering: "timestamped",
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let n = rows.len();
    w.events.extend(rows);
    w.probe("_amux_turns", format!("worker_id='{worker_id}'"), n, total,
        (total == 0).then(|| "no turns recorded for this worker".to_string()));
    Ok(())
}

// ---------------------------------------------------------------------------
// command
// ---------------------------------------------------------------------------

fn why_command(conn: &Connection, id: &str) -> rusqlite::Result<Value> {
    let mut w = Why::new("command", id);
    let row: Option<CommandRow> = conn
        .query_row(
            "SELECT worker_id, command, state, queued_at, attempts, timing, precondition
             FROM _amux_commands WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )
        .ok();
    let Some((worker_id, command, state, queued_at, attempts, timing, precondition)) = row else {
        w.gap(format!("no row in `_amux_commands` with id {id}"));
        w.probe("_amux_commands", format!("id='{id}'"), 0, 0, Some("subject not found".into()));
        return Ok(w.finish());
    };
    w.found = true;
    w.subject = json!({"kind": "command", "id": id, "worker_id": worker_id, "state": state});
    w.probe("_amux_commands", format!("id='{id}'"), 1, 1, None);
    // Read before the move: `queued_at` is RFC3339 TEXT here (not the millis that
    // `cmd_history.ts` carries), and it is this command's birth time.
    let queued_epoch = rfc3339_to_epoch(&queued_at);
    w.events.push(command_event(id.to_string(), command, state, queued_at, attempts));
    w.events.push(WhyEvent {
        at: None,
        at_epoch: None,
        kind: "command".into(),
        actor: None,
        summary: "delivery timing and precondition as recorded on the row".into(),
        detail: Some(json!({"timing": timing, "precondition": precondition})),
        source: json!({"table": "_amux_commands", "id": id}),
        ordering: "append-order",
    });
    collect_state_events(conn, &mut w, "command", id, queued_epoch)?;
    collect_worker_turns(conn, &mut w, &worker_id)?;
    w.gap(
        "commands carry no per-attempt history table: `_amux_commands.state` holds only the CURRENT state, so intermediate transitions of this command are not recoverable — the state-event journal is the only place a transition could have been recorded".to_string(),
    );
    Ok(w.finish())
}

// ---------------------------------------------------------------------------
// schedule
// ---------------------------------------------------------------------------

fn why_schedule(conn: &Connection, id: &str) -> rusqlite::Result<Value> {
    let mut w = Why::new("schedule", id);
    let row: Option<ScheduleRow> = conn
        .query_row(
            "SELECT title, session, command, enabled, schedule_expr, created, updated
             FROM schedules WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )
        .ok();
    let Some((title, session, command, enabled, expr, created, updated)) = row else {
        w.gap(format!("no row in `schedules` with id {id} — a deleted schedule leaves its runs and audit rows behind, so try `schedule_runs`/`schedule_audit` directly for a schedule that no longer exists"));
        w.probe("schedules", format!("id='{id}'"), 0, 0, Some("subject not found".into()));
        // The runs/audit rows can still explain a deleted schedule; collect
        // them anyway rather than stopping at "not found".
        collect_schedule_runs(conn, &mut w, id)?;
        collect_schedule_audit(conn, &mut w, id)?;
        return Ok(w.finish());
    };
    w.found = true;
    w.subject = json!({"kind": "schedule", "id": id, "title": title, "session": session, "enabled": enabled, "schedule_expr": expr});
    w.probe("schedules", format!("id='{id}'"), 1, 1, None);
    w.events.push(WhyEvent {
        at: Some(epoch_to_rfc3339(created)),
        at_epoch: Some(created),
        kind: "entity".into(),
        actor: None,
        summary: format!("schedule created: {title} -> session {session}"),
        detail: Some(json!({"command": command})),
        source: json!({"table": "schedules", "column": "created", "id": id}),
        ordering: "timestamped",
    });
    w.events.push(WhyEvent {
        at: Some(epoch_to_rfc3339(updated)),
        at_epoch: Some(updated),
        kind: "entity".into(),
        actor: None,
        summary: format!("last write to the row (enabled={enabled})"),
        detail: None,
        source: json!({"table": "schedules", "column": "updated", "id": id}),
        ordering: "timestamped",
    });
    collect_schedule_runs(conn, &mut w, id)?;
    collect_schedule_audit(conn, &mut w, id)?;
    collect_state_events(conn, &mut w, "schedule", id, Some(created))?;
    collect_requests(conn, &mut w, &format!("/api/schedules/{id}%"), &format!("schedule {id}"))?;
    Ok(w.finish())
}

fn collect_schedule_runs(conn: &Connection, w: &mut Why, id: &str) -> rusqlite::Result<()> {
    let total: usize = conn
        .query_row("SELECT COUNT(*) FROM schedule_runs WHERE schedule_id = ?1", [id], |r| {
            r.get::<_, i64>(0).map(|n| n as usize)
        })
        .unwrap_or(0);
    let mut stmt = conn.prepare(
        "SELECT id, ran_at, status, note, source FROM schedule_runs
         WHERE schedule_id = ?1 ORDER BY ran_at DESC LIMIT ?2",
    )?;
    let rows: Vec<WhyEvent> = stmt
        .query_map(rusqlite::params![id, PER_SOURCE_CAP as i64], |r| {
            let rowid: i64 = r.get(0)?;
            // `ran_at` is unix SECONDS.
            let ran_at: i64 = r.get(1)?;
            let status: String = r.get(2)?;
            let note: Option<String> = r.get(3)?;
            // `source` exists BECAUSE a manual Run-now and a cron fire were
            // byte-identical rows once (ethos rule 4's own example). Naming it
            // on every line is the whole point of the column.
            let source: String = r.get(4)?;
            Ok(WhyEvent {
                at: Some(epoch_to_rfc3339(ran_at)),
                at_epoch: Some(ran_at),
                kind: "schedule_run".into(),
                actor: Some(source.clone()),
                summary: format!(
                    "ran ({source}) -> {status}{}",
                    note.as_ref().map(|n| format!(": {n}")).unwrap_or_default()
                ),
                detail: None,
                source: json!({"table": "schedule_runs", "rowid": rowid}),
                ordering: "timestamped",
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let n = rows.len();
    w.events.extend(rows);
    w.probe("schedule_runs", format!("schedule_id='{id}'"), n, total,
        (total == 0).then(|| "this schedule has never recorded a run".to_string()));
    Ok(())
}

fn collect_schedule_audit(conn: &Connection, w: &mut Why, id: &str) -> rusqlite::Result<()> {
    let total: usize = conn
        .query_row("SELECT COUNT(*) FROM schedule_audit WHERE schedule_id = ?1", [id], |r| {
            r.get::<_, i64>(0).map(|n| n as usize)
        })
        .unwrap_or(0);
    let mut stmt = conn.prepare(
        "SELECT id, ts, field, old_value, new_value, source, by_who FROM schedule_audit
         WHERE schedule_id = ?1 ORDER BY ts ASC LIMIT ?2",
    )?;
    let rows: Vec<WhyEvent> = stmt
        .query_map(rusqlite::params![id, PER_SOURCE_CAP as i64], |r| {
            let rowid: i64 = r.get(0)?;
            let ts: i64 = r.get(1)?;
            let field: String = r.get(2)?;
            let old: Option<String> = r.get(3)?;
            let new: Option<String> = r.get(4)?;
            let source: Option<String> = r.get(5)?;
            let by_who: Option<String> = r.get(6)?;
            Ok(WhyEvent {
                at: Some(epoch_to_rfc3339(ts)),
                at_epoch: Some(ts),
                kind: "schedule_audit".into(),
                actor: by_who.clone().filter(|s| !s.is_empty()),
                summary: format!(
                    "{field}: {} -> {}",
                    old.as_deref().unwrap_or("(unset)"),
                    new.as_deref().unwrap_or("(unset)")
                ),
                detail: Some(json!({"source": source, "by_who": by_who})),
                source: json!({"table": "schedule_audit", "rowid": rowid}),
                ordering: "timestamped",
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let n = rows.len();
    w.events.extend(rows);
    w.probe("schedule_audit", format!("schedule_id='{id}'"), n, total,
        (total == 0).then(|| "no audited field changes for this schedule".to_string()));
    Ok(())
}

// ---------------------------------------------------------------------------
// session
// ---------------------------------------------------------------------------

fn why_session(conn: &Connection, name: &str) -> rusqlite::Result<Value> {
    let mut w = Why::new("session", name);
    let req_total: usize = conn
        .query_row("SELECT COUNT(*) FROM _amux_request_log WHERE amux_session = ?1", [name], |r| {
            r.get::<_, i64>(0).map(|n| n as usize)
        })
        .unwrap_or(0);
    let ev_total: usize = conn
        .query_row("SELECT COUNT(*) FROM session_events WHERE session = ?1", [name], |r| {
            r.get::<_, i64>(0).map(|n| n as usize)
        })
        .unwrap_or(0);
    let cards: i64 = conn
        .query_row("SELECT COUNT(*) FROM issues WHERE session = ?1 AND deleted IS NULL", [name], |r| r.get(0))
        .unwrap_or(0);
    w.found = req_total > 0 || ev_total > 0 || cards > 0;
    w.subject = json!({"kind": "session", "id": name, "board_cards": cards});
    if !w.found {
        w.gap(format!(
            "`{name}` appears in no request-log attribution, no session_events row, and owns no board card — either the name is wrong, or everything it did was written without the X-Amux-Session stamp (an unattributed write is invisible to this query by construction)"
        ));
    }

    let mut stmt = conn.prepare(
        "SELECT id, ts, method, path, status FROM _amux_request_log
         WHERE amux_session = ?1 ORDER BY ts DESC LIMIT ?2",
    )?;
    let rows: Vec<WhyEvent> = stmt
        .query_map(rusqlite::params![name, PER_SOURCE_CAP as i64], |r| {
            let id: i64 = r.get(0)?;
            let ts: f64 = r.get(1)?;
            let method: String = r.get(2)?;
            let path: String = r.get(3)?;
            let status: i64 = r.get(4)?;
            let secs = ts as i64;
            Ok(WhyEvent {
                at: Some(epoch_to_rfc3339(secs)),
                at_epoch: Some(secs),
                kind: "request".into(),
                actor: Some(name.to_string()),
                summary: format!("{method} {path} -> {status}"),
                detail: None,
                source: json!({"table": "_amux_request_log", "rowid": id}),
                ordering: "timestamped",
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let n = rows.len();
    w.events.extend(rows);
    w.probe("_amux_request_log", format!("amux_session='{name}'"), n, req_total,
        (req_total == 0).then(|| "no attributed requests; calls made without the X-Amux-Session header cannot be tied to a session here".to_string()));

    let mut stmt = conn.prepare(
        "SELECT id, ts, type, source, data FROM session_events
         WHERE session = ?1 ORDER BY ts DESC LIMIT ?2",
    )?;
    let rows: Vec<WhyEvent> = stmt
        .query_map(rusqlite::params![name, PER_SOURCE_CAP as i64], |r| {
            let id: i64 = r.get(0)?;
            // `session_events.ts` is REAL unix SECONDS.
            let ts: f64 = r.get(1)?;
            let ty: String = r.get(2)?;
            let source: String = r.get(3)?;
            let data: Option<String> = r.get(4)?;
            let secs = ts as i64;
            Ok(WhyEvent {
                at: Some(epoch_to_rfc3339(secs)),
                at_epoch: Some(secs),
                kind: "session".into(),
                actor: (!source.is_empty()).then(|| source.clone()),
                summary: format!("{ty} (reported by {})", if source.is_empty() { "unrecorded source" } else { &source }),
                detail: data.map(|d| json!({ "data": d })),
                source: json!({"table": "session_events", "rowid": id}),
                ordering: "timestamped",
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let n = rows.len();
    w.events.extend(rows);
    w.probe("session_events", format!("session='{name}'"), n, ev_total,
        (ev_total == 0).then(|| "this session's harness never reported a lifecycle event (no Stop/UserPromptSubmit hook, or a provider without hooks)".to_string()));
    Ok(w.finish())
}

// ---------------------------------------------------------------------------
// integration
// ---------------------------------------------------------------------------

/// `amux why integration gmail`. There is no integrations table: an
/// integration's durable trail is whatever its API family wrote to the request
/// log, plus (for email) the email tables. When neither has anything, this
/// says so explicitly instead of narrating from nothing.
fn why_integration(conn: &Connection, name: &str) -> rusqlite::Result<Value> {
    let mut w = Why::new("integration", name);
    let like = format!("/api/{name}%");
    let fam_total: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM _amux_request_log WHERE family = ?1 OR path LIKE ?2",
            rusqlite::params![name, &like],
            |r| r.get::<_, i64>(0).map(|n| n as usize),
        )
        .unwrap_or(0);
    w.found = fam_total > 0;

    let mut stmt = conn.prepare(
        "SELECT id, ts, method, path, status, amux_session, error_body FROM _amux_request_log
         WHERE family = ?1 OR path LIKE ?2 ORDER BY ts DESC LIMIT ?3",
    )?;
    let rows: Vec<WhyEvent> = stmt
        .query_map(rusqlite::params![name, &like, PER_SOURCE_CAP as i64], |r| {
            let id: i64 = r.get(0)?;
            let ts: f64 = r.get(1)?;
            let method: String = r.get(2)?;
            let path: String = r.get(3)?;
            let status: i64 = r.get(4)?;
            let session: Option<String> = r.get(5)?;
            let err: Option<String> = r.get(6)?;
            let secs = ts as i64;
            Ok(WhyEvent {
                at: Some(epoch_to_rfc3339(secs)),
                at_epoch: Some(secs),
                kind: "request".into(),
                actor: session.filter(|s| !s.is_empty()),
                summary: format!(
                    "{method} {path} -> {status}{}",
                    err.map(|e| format!(" — {}", e.chars().take(160).collect::<String>())).unwrap_or_default()
                ),
                detail: None,
                source: json!({"table": "_amux_request_log", "rowid": id}),
                ordering: "timestamped",
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let n = rows.len();
    w.events.extend(rows);
    w.probe("_amux_request_log", format!("family='{name}' OR path LIKE '{like}'"), n, fam_total,
        (fam_total == 0).then(|| format!("no request-log rows for family `{name}`; either nothing has called it since the request log started, or the integration is not exposed under /api/{name}")));

    // Email integrations have a second, richer trail.
    let email_total: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM email_events WHERE account_id IN (SELECT id FROM email_accounts WHERE provider = ?1 OR email LIKE '%'||?1||'%')",
            [name],
            |r| r.get::<_, i64>(0).map(|n| n as usize),
        )
        .unwrap_or(0);
    if email_total > 0 {
        w.found = true;
    }
    w.probe(
        "email_events",
        format!("account provider='{name}' or address contains '{name}'"),
        0,
        email_total,
        (email_total == 0).then(|| format!("no email_events rows attributable to `{name}`")),
    );

    if !w.found {
        w.gap(format!(
            "amux keeps no integrations registry: there is no table that records `{name}` being configured, enabled, or failing. The only durable trails are the request log (per API family) and, for email, email_events — both empty here. This question is not answerable from the data amux currently keeps, and the fix is an integration state table, not a better query."
        ));
    }
    Ok(w.finish())
}

// ---------------------------------------------------------------------------
// window mode
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct WindowParams {
    /// Epoch seconds, or an RFC3339 timestamp.
    since: Option<String>,
    until: Option<String>,
    limit: Option<usize>,
}

fn parse_when(s: &str) -> Option<i64> {
    s.parse::<i64>()
        .ok()
        .or_else(|| chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp()))
}

/// "What happened between T1 and T2" across the journal, the request log, and
/// schedule runs — the three trails that are complete enough to answer a
/// window question without cherry-picking.
fn why_window_inner(conn: &Connection, since: i64, until: i64, limit: usize) -> rusqlite::Result<Value> {
    let mut w = Why::new("window", &format!("{since}..{until}"));
    w.found = true;
    w.subject = json!({
        "kind": "window",
        "since": epoch_to_rfc3339(since),
        "until": epoch_to_rfc3339(until),
        "since_epoch": since,
        "until_epoch": until,
    });

    // State events store `at` as RFC3339 text, so the window is compared as
    // text against RFC3339 bounds — not as an epoch, which would silently
    // match nothing.
    let since_s = epoch_to_rfc3339(since);
    let until_s = epoch_to_rfc3339(until);
    let ev_total: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM _amux_state_events WHERE at >= ?1 AND at <= ?2",
            rusqlite::params![&since_s, &until_s],
            |r| r.get::<_, i64>(0).map(|n| n as usize),
        )
        .unwrap_or(0);
    let mut stmt = conn.prepare(
        "SELECT id, rev, entity_type, entity_id, mutation, at FROM _amux_state_events
         WHERE at >= ?1 AND at <= ?2 ORDER BY rev ASC LIMIT ?3",
    )?;
    let rows: Vec<WhyEvent> = stmt
        .query_map(rusqlite::params![&since_s, &until_s, limit as i64], |r| {
            let id: i64 = r.get(0)?;
            let rev: i64 = r.get(1)?;
            let etype: String = r.get(2)?;
            let eid: String = r.get(3)?;
            let mutation: String = r.get(4)?;
            let at: String = r.get(5)?;
            Ok(WhyEvent {
                at_epoch: rfc3339_to_epoch(&at),
                at: Some(at),
                kind: "state_event".into(),
                actor: None,
                summary: format!("{etype} {eid}: {mutation}"),
                detail: None,
                source: json!({"table": "_amux_state_events", "rowid": id, "rev": rev}),
                ordering: "timestamped",
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let n = rows.len();
    w.events.extend(rows);
    w.probe("_amux_state_events", format!("at BETWEEN '{since_s}' AND '{until_s}'"), n, ev_total, None);

    let req_total: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM _amux_request_log WHERE ts >= ?1 AND ts <= ?2 AND status >= 400",
            rusqlite::params![since as f64, until as f64],
            |r| r.get::<_, i64>(0).map(|n| n as usize),
        )
        .unwrap_or(0);
    // Only failures: a window's worth of 200s is a log, not an explanation
    // (ethos rule 5 — accumulate vs discriminate).
    let mut stmt = conn.prepare(
        "SELECT id, ts, method, path, status, amux_session, error_body FROM _amux_request_log
         WHERE ts >= ?1 AND ts <= ?2 AND status >= 400 ORDER BY ts ASC LIMIT ?3",
    )?;
    let rows: Vec<WhyEvent> = stmt
        .query_map(rusqlite::params![since as f64, until as f64, limit as i64], |r| {
            let id: i64 = r.get(0)?;
            let ts: f64 = r.get(1)?;
            let method: String = r.get(2)?;
            let path: String = r.get(3)?;
            let status: i64 = r.get(4)?;
            let session: Option<String> = r.get(5)?;
            let err: Option<String> = r.get(6)?;
            let secs = ts as i64;
            Ok(WhyEvent {
                at: Some(epoch_to_rfc3339(secs)),
                at_epoch: Some(secs),
                kind: "request".into(),
                actor: session.filter(|s| !s.is_empty()),
                summary: format!(
                    "{method} {path} -> {status}{}",
                    err.map(|e| format!(" — {}", e.chars().take(160).collect::<String>())).unwrap_or_default()
                ),
                detail: None,
                source: json!({"table": "_amux_request_log", "rowid": id}),
                ordering: "timestamped",
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let n = rows.len();
    w.events.extend(rows);
    w.probe("_amux_request_log", format!("ts BETWEEN {since} AND {until} AND status >= 400 (FAILURES ONLY)"), n, req_total,
        Some("successful requests in the window are deliberately not listed — this view answers 'what went wrong', and a window of 200s is a log rather than an explanation".into()));

    let run_total: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM schedule_runs WHERE ran_at >= ?1 AND ran_at <= ?2",
            rusqlite::params![since, until],
            |r| r.get::<_, i64>(0).map(|n| n as usize),
        )
        .unwrap_or(0);
    let mut stmt = conn.prepare(
        "SELECT id, schedule_id, ran_at, status, note, source FROM schedule_runs
         WHERE ran_at >= ?1 AND ran_at <= ?2 ORDER BY ran_at ASC LIMIT ?3",
    )?;
    let rows: Vec<WhyEvent> = stmt
        .query_map(rusqlite::params![since, until, limit as i64], |r| {
            let id: i64 = r.get(0)?;
            let sched: String = r.get(1)?;
            let ran_at: i64 = r.get(2)?;
            let status: String = r.get(3)?;
            let note: Option<String> = r.get(4)?;
            let source: String = r.get(5)?;
            Ok(WhyEvent {
                at: Some(epoch_to_rfc3339(ran_at)),
                at_epoch: Some(ran_at),
                kind: "schedule_run".into(),
                actor: Some(source.clone()),
                summary: format!(
                    "{sched} ran ({source}) -> {status}{}",
                    note.map(|n| format!(": {n}")).unwrap_or_default()
                ),
                detail: None,
                source: json!({"table": "schedule_runs", "rowid": id}),
                ordering: "timestamped",
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let n = rows.len();
    w.events.extend(rows);
    w.probe("schedule_runs", format!("ran_at BETWEEN {since} AND {until}"), n, run_total, None);

    if ev_total == 0 && req_total == 0 && run_total == 0 {
        w.found = false;
        w.gap(format!(
            "no journal event, failed request, or schedule run falls in {since_s}..{until_s} — the window may predate the state-event journal or the request log (migration 0010), both of which start when this server first ran"
        ));
    }
    Ok(w.finish())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn why_entity(State(st): State<AppState>, AxPath((kind, id)): AxPath<(String, String)>) -> Response {
    let conn = match st.store.read() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let res = match kind.as_str() {
        // `card` is the SPA's word for the same thing; both spellings answer.
        "task" | "card" | "issue" => why_task(&conn, &id),
        "worker" => why_worker(&conn, &id),
        "command" => why_command(&conn, &id),
        "schedule" => why_schedule(&conn, &id),
        "session" => why_session(&conn, &id),
        "integration" => why_integration(&conn, &id),
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("unknown subject kind `{other}`"),
                    "kinds": ["task", "worker", "command", "schedule", "session", "integration"],
                    "contract": "GET /api/why/contract",
                })),
            )
                .into_response()
        }
    };
    match res {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

async fn why_window(State(st): State<AppState>, Query(p): Query<WindowParams>) -> Response {
    let now = chrono::Utc::now().timestamp();
    let until = p.until.as_deref().and_then(parse_when).unwrap_or(now);
    // Default window is the last hour: long enough to contain the thing you
    // just noticed, short enough that the answer is not a log.
    let since = p.since.as_deref().and_then(parse_when).unwrap_or(until - 3600);
    let limit = p.limit.unwrap_or(100).min(PER_SOURCE_CAP);
    if since > until {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "since is after until", "since": since, "until": until})),
        )
            .into_response();
    }
    let conn = match st.store.read() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    match why_window_inner(&conn, since, until, limit) {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

/// What this endpoint can answer and from which tables. Published so an agent
/// does not have to guess which subject kinds exist, and so the list of
/// consulted trails is checkable against the code that consults them.
async fn contract() -> Json<Value> {
    Json(json!({
        "endpoint": "GET /api/why/{kind}/{id}",
        "window": "GET /api/why?since=<epoch|rfc3339>&until=<epoch|rfc3339>&limit=N",
        "kinds": {
            "task":        ["issues", "issues.log", "_amux_state_events", "_amux_request_log", "_amux_turns", "interaction_log"],
            "worker":      ["_amux_workers", "_amux_state_events", "_amux_sessions", "_amux_commands", "_amux_turns", "_amux_request_log"],
            "command":     ["_amux_commands", "_amux_turns"],
            "schedule":    ["schedules", "schedule_runs", "schedule_audit", "_amux_state_events", "_amux_request_log"],
            "session":     ["_amux_request_log", "session_events", "issues"],
            "integration": ["_amux_request_log", "email_events"]
        },
        "verdicts": {
            "explained":   "every trail consulted covers this entity",
            "partial":     "some trails cover it and others structurally cannot; see `gaps`",
            "cannot_tell": "the subject was not found, or no trail has a row for it"
        },
        "guarantee": "every timeline line cites the table and row it came from; every consulted source is listed with the predicate it ran, including the ones that returned nothing"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_line_parses_the_python_format_and_leaves_anything_else_alone() {
        assert_eq!(parse_log_line("`12:01` a: todo -> doing"), (Some("12:01"), "a: todo -> doing"));
        // A line that is not in the expected shape is returned whole rather
        // than half-parsed into a wrong timestamp.
        assert_eq!(parse_log_line("no timestamp here"), (None, "no timestamp here"));
    }

    #[test]
    fn abbrev_truncates_but_says_it_did_and_how_much() {
        let long = Value::String("x".repeat(4000));
        let s = abbrev(&long);
        assert!(s.len() < 200, "a 4000-char field must not land in a summary line");
        assert!(s.contains("4000 chars total"), "the cut must state the real length: {s}");
        assert!(s.contains("full value in `detail`"), "{s}");
        // Short values are untouched.
        assert_eq!(abbrev(&Value::String("todo".into())), "\"todo\"");
        assert_eq!(abbrev(&Value::Null), "\"(absent)\"");
    }

    #[test]
    fn changed_fields_ignores_bookkeeping_columns() {
        let a = json!({"status": "todo", "updated": 1, "rev": 1, "title": "x"});
        let b = json!({"status": "doing", "updated": 2, "rev": 2, "title": "x"});
        let d = changed_fields(&a, &b);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].0, "status");
    }

    /// AMUX-3526. A zero from a DEAD source must not be reported as a fact
    /// about the entity. interaction_log has had no writer since 2026-08-09,
    /// so "no interaction rows target this entity" was true of the table and
    /// false as an explanation — and the storage reaper is expiring what
    /// remains, after which it would be wrong about everything.
    #[test]
    fn an_empty_interaction_log_is_reported_as_a_dead_source_not_a_quiet_entity() {
        let conn = Connection::open_in_memory().expect("db");
        conn.execute_batch(
            "CREATE TABLE interaction_log (id INTEGER PRIMARY KEY, ts INTEGER, kind TEXT, \
             actor TEXT, target TEXT, action TEXT, detail TEXT, ok INTEGER, result TEXT);",
        )
        .expect("schema");

        // (a) TABLE COMPLETELY EMPTY — the post-reaper shape.
        let mut w = Why::new("task", "AMUX-1");
        collect_interactions(&conn, &mut w, "AMUX-1").expect("collect");
        let note = format!("{:?}", w.sources);
        assert!(note.contains("EMPTY"), "must name the SOURCE as empty: {note}");
        assert!(
            !note.contains("no interaction rows target this entity"),
            "must not state a dead channel as a fact about the entity: {note}"
        );

        // (b) TABLE HAS ROWS, none for this target — the note must say the
        // channel is STALE and name the newest row, not imply the entity was
        // quiet. Without this branch the assertion above passes for a probe
        // that shouts EMPTY unconditionally.
        conn.execute(
            "INSERT INTO interaction_log (ts,kind,actor,target,action,detail,ok,result) \
             VALUES (?1,'board','someone','OTHER-9','patch','',1,'')",
            rusqlite::params![1786233600000i64],
        )
        .expect("seed");
        let mut w2 = Why::new("task", "AMUX-1");
        collect_interactions(&conn, &mut w2, "AMUX-1").expect("collect");
        let note2 = format!("{:?}", w2.sources);
        assert!(note2.contains("stale"), "a populated-but-old table must read as stale: {note2}");
        assert!(note2.contains("2026-08-09"), "the note must name the newest row: {note2}");

        // (c) CONTROL — a row that DOES target the entity produces no note at
        // all, or every explanation would carry a caveat and the caveat would
        // stop being read.
        conn.execute(
            "INSERT INTO interaction_log (ts,kind,actor,target,action,detail,ok,result) \
             VALUES (?1,'board','someone','AMUX-1','patch','',1,'')",
            rusqlite::params![1786233600000i64],
        )
        .expect("seed2");
        let mut w3 = Why::new("task", "AMUX-1");
        collect_interactions(&conn, &mut w3, "AMUX-1").expect("collect");
        let note3 = format!("{:?}", w3.sources);
        assert!(!note3.contains("EMPTY"), "a real hit must not be caveated: {note3}");
        assert!(!note3.contains("stale"), "a real hit must not be caveated: {note3}");
    }

    #[test]
    fn when_parses_both_epoch_and_rfc3339() {
        assert_eq!(parse_when("1700000000"), Some(1700000000));
        // Cross-checked against `date -u -r 1786233600` rather than computed
        // in my head — the first spelling of this constant was a day out and
        // the failing test was the instrument, not the code.
        assert_eq!(parse_when("2026-08-09T00:00:00Z"), Some(1786233600));
        assert_eq!(parse_when("not a time"), None);
    }
}
