//! Schedules API (RR-0058/RR-0062): CRUD + run history + audit over the
//! LIVE `schedules` tables, field-compatible with the Python endpoints so
//! the dashboard works unchanged (`title`, `session`, `command`,
//! `schedule_expr`, `enabled`, `next_run`, ...).
//!
//! Mounted at `/api/schedules` inside the `protected` router (the
//! integrator adds the mount in api/mod.rs, following workers.rs).
//!
//! DUAL-SCHEDULER SAFETY (see runtime_jobs/mod.rs): the CRUD surface is
//! always live — both servers agree on rows and audit. Run-now is the one
//! endpoint that would FIRE, so while the Rust scheduler is in shadow mode
//! (no `AMUX_RS_SCHEDULER=1`) it answers 409
//! `{"error":"rust scheduler is in shadow mode","enable":"AMUX_RS_SCHEDULER=1"}`
//! — an honest refusal instead of a run row that delivered nothing.
//!
//! Attribution (AMUX-1765/1812/2352): every mutation records `by_who` in
//! `schedule_audit`, resolved in Python's priority order — the verified
//! `X-Amux-Worker` header (canonical spelling), then legacy
//! `X-Amux-Session`, then the request's claimed `by` (recorded as
//! `"<hdr> (claimed <by>)"` when both exist and disagree), then the cloud
//! user-email header. The Python fallback-of-last-resort is the client IP;
//! that needs `ConnectInfo` wiring at mount time, so until then the floor
//! here is the literal `unattributed-http` — visible, never blank.

use super::AppState;
use crate::db::{PendingEvent, WriteOutcome};
use crate::runtime_jobs::scheduler::{
    self, fmt_minute, get_schedule, insert_audit, insert_schedule, legacy_next_run, list_schedules,
    mint_schedule_id, record_run, skip_next_run, soft_delete_schedule, update_schedule, Deliverer,
    DurableSchedule, LiveDeliverer, RunOutcome, ScheduleExpr, AUDIT_FIELDS,
};
use amux_core::revision::{EntityType, MutationKind};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Local;
use serde::{Deserialize, Deserializer};
use crate::runtime_jobs::schedule_lifecycle as lifecycle;
use crate::runtime_jobs::schedule_lifecycle::Bounds;
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/runs", get(recent_runs))
        .route("/audit", get(audit_trail))
        .route("/{id}", get(get_one).patch(patch).delete(delete_schedule))
        .route("/{id}/run", post(run_now))
        .route("/{id}/skip", post(skip_next))
        .route("/shell", get(shell_for_session))
        .route("/{id}/output", get(shell_output))
}

// ---- the worker's Shell tab (2026-09-25) ------------------------------------
//
// Ethan: "i need to be able to see it ... maybe there should be a shell tab
// assigned to each worker". A shell schedule runs on the host, never in the
// worker, so its only visible trace used to be a 480-char note written at exit.
// The scheduler now tees each run to ~/.amux/shell-runs/<schedule>/<ms>.log;
// these two reads are what the tab shows.

fn shell_logs(id: &str) -> Vec<Value> {
    let dir = crate::runtime_jobs::scheduler::shell_runs_dir(id);
    let mut v: Vec<(String, u64)> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let size = e.metadata().ok()?.len();
                    name.ends_with(".log").then_some((name, size))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v.into_iter()
        .map(|(name, size)| {
            let started_ms: i64 = name.trim_end_matches(".log").parse().unwrap_or(0);
            json!({"name": name, "started_ms": started_ms, "size": size})
        })
        .collect()
}

#[derive(Deserialize)]
struct ShellQuery {
    session: Option<String>,
}

/// GET /api/schedules/shell?session=<name>: the worker's shell schedules, each
/// with its latest run and its saved output logs.
async fn shell_for_session(State(state): State<AppState>, Query(q): Query<ShellQuery>) -> Response {
    let session = q.session.unwrap_or_default();
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": e.to_string(), "measured": false}))).into_response(),
    };
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT id, title, command, schedule_expr, enabled FROM schedules
         WHERE session=?1 AND kind='shell' AND COALESCE(deleted,0)=0 ORDER BY title",
    ) {
        let rows = stmt.query_map([&session], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?, r.get::<_, Option<i64>>(4)?))
        });
        if let Ok(rows) = rows {
            for (id, title, command, expr, enabled) in rows.flatten() {
                let last = conn.query_row(
                    "SELECT id, ran_at, status, note, source FROM schedule_runs WHERE schedule_id=?1 ORDER BY id DESC LIMIT 1",
                    [&id],
                    |r| Ok(json!({"id": r.get::<_, i64>(0)?, "ran_at": r.get::<_, i64>(1)?, "status": r.get::<_, Option<String>>(2)?,
                                  "note": r.get::<_, Option<String>>(3)?, "source": r.get::<_, Option<String>>(4)?})),
                ).ok();
                let running = last.as_ref().is_some_and(|l| l["status"] == "running");
                out.push(json!({"id": id, "title": title, "command": command, "schedule_expr": expr,
                    "enabled": enabled.unwrap_or(0) != 0, "running": running, "last_run": last, "logs": shell_logs(&id)}));
            }
        }
    }
    let n = out.len();
    Json(json!({"session": session, "measured": true, "n_considered": n, "schedules": out})).into_response()
}

#[derive(Deserialize)]
struct OutputQuery {
    log: Option<String>,
    offset: Option<u64>,
}

/// GET /api/schedules/{id}/output?log=<name>&offset=<bytes>: a run's output
/// from `offset` (default: the newest log from 0), at most 256 KB per call, so
/// a running job can be tailed by polling with the returned `next_offset`.
async fn shell_output(State(state): State<AppState>, Path(id): Path<String>, Query(q): Query<OutputQuery>) -> Response {
    let logs = shell_logs(&id);
    let name = match q.log.filter(|l| !l.contains('/') && !l.contains("..") && l.ends_with(".log")) {
        Some(l) => l,
        None => match logs.first().and_then(|l| l["name"].as_str()) {
            Some(l) => l.to_string(),
            None => return Json(json!({"schedule": id, "measured": true, "n_considered": 0, "logs": logs,
                "text": "", "next_offset": 0, "why_empty": "no output recorded yet: runs before this change, or none since"})).into_response(),
        },
    };
    let path = crate::runtime_jobs::scheduler::shell_runs_dir(&id).join(&name);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return (StatusCode::NOT_FOUND, Json(json!({"error": format!("{name}: {e}")}))).into_response(),
    };
    let off = q.offset.unwrap_or(0).min(bytes.len() as u64) as usize;
    let end = (off + 256 * 1024).min(bytes.len());
    let running = state.store.read().ok().and_then(|c| c.query_row(
        "SELECT status FROM schedule_runs WHERE schedule_id=?1 ORDER BY id DESC LIMIT 1", [&id], |r| r.get::<_, String>(0)).ok())
        .is_some_and(|s| s == "running") && logs.first().and_then(|l| l["name"].as_str()) == Some(name.as_str());
    Json(json!({"schedule": id, "log": name, "measured": true, "n_considered": 1, "running": running,
        "size": bytes.len(), "offset": off, "next_offset": end,
        "text": String::from_utf8_lossy(&bytes[off..end]), "logs": logs})).into_response()
}

// ---- the fields this server does NOT honour (AMUX-2680) -------------------

/// Columns the `schedules` table carries, the editor used to write, and
/// NOTHING in this server reads.
///
/// Python ran a watcher thread (`_watch_schedule_response`) that polled a
/// session's pane for `done_pattern` and applied `done_action`, plus an
/// event-trigger drain (`_drain_and_fire_sched_events`) that fired schedules
/// on `session_idle`/`board` instead of a clock. Neither was ported, and the
/// write path kept accepting all seven fields — so the dashboard's Trigger
/// mode and "stop when output matches" box wrote rows that could never do
/// anything. That is the silent-absence class: the same defect as
/// `run_scheduler` having zero call sites, one layer out.
///
/// # Why the watcher was NOT ported
///
/// Measured before deciding (`~/.amux/amux.db`, 2026-08-10): **0 of 113 live
/// schedules** set any of these. Every row that ever did — 9 of them — is
/// deleted, and 4 were test fixtures (`__pwtest_loop`, `__pwtest2`,
/// `__evt_test__`, `__evt_scope_test__`). Stronger still, the feature never
/// fired under PYTHON either: `_run_schedule` writes a `schedule_runs` row on
/// every fire and `_watch_schedule_response` writes one on every match, and
/// across **20,618 run rows there are zero** with source `watch` or
/// `trigger:*`; across 531 `schedule_audit` rows, zero from
/// `watch-autodisable`/`watch-done-command`. Nobody is losing a capability
/// they had.
///
/// And the watcher half should not come back as written. `done_action` was
/// `disable` (stop nagging), `notify` (a Web Push broadcast to an EMPTY
/// `push_subscriptions` table — an alert that reached nobody), or
/// `command:<text>` (send one more line). Regex-matching a rendered terminal
/// pane to guess whether a model finished is D1's terminal-scraping in its
/// purest form: it cannot improve when the model improves, and it breaks when
/// a string changes. The model already reports its own completion through the
/// Stop hook. A loop that stops when the worker SAYS it is done is worth
/// building on `POST /api/sessions/<n>/report`, not on `tmux capture-pane`.
///
/// The event-trigger half is different and is worth building — dispatching
/// real work at the moment a lane goes idle compounds with model quality —
/// but it is a runtime job with a durable dedupe key and a named consumer,
/// not seven columns and a checkbox. Carded as AMUX-2756 rather than
/// half-ported.
pub const UNHONOURED_FIELDS: [&str; 8] = [
    "fan_out",
    "watch",
    "watch_timeout",
    "done_pattern",
    "done_action",
    "trigger_on",
    "trigger_cooldown",
    "trigger_sessions",
];

/// [`UNHONOURED_FIELDS`] as a header value. A header needs a `&'static str`,
/// so this is written out rather than joined at runtime — and
/// `unhonoured_header_lists_every_unhonoured_field` fails the build if the two
/// ever disagree, because a declaration that silently drops a field is the
/// same silent absence it exists to announce.
const UNHONOURED_FIELDS_HEADER: &str =
    "not implemented (stored, never read; AMUX-2680): fan_out, watch, watch_timeout, done_pattern, \
     done_action, trigger_on, trigger_cooldown, trigger_sessions";

/// The subset of [`UNHONOURED_FIELDS`] that ARM a dead feature, and the value
/// that means "not armed".
///
/// `watch_timeout` and `trigger_cooldown` are parameters, not switches: they
/// cannot make anything happen on their own, so refusing them would 400 an
/// edit of a legacy row carrying stale residue (SCHED-8 still holds
/// `watch_timeout=300` with `watch=0`) while closing no false promise. The
/// five below are the ones whose acceptance would tell a caller that a
/// behaviour is now armed. All seven are declared on read.
fn armed_unhonoured(body: &ScheduleBody) -> Vec<&'static str> {
    let set = |v: &Option<String>| v.as_deref().map(str::trim).is_some_and(|s| !s.is_empty());
    let mut out = Vec::new();
    if body.fan_out.unwrap_or(0) != 0 {
        out.push("fan_out");
    }
    if body.watch.unwrap_or(0) != 0 {
        out.push("watch");
    }
    if set(&body.done_pattern) {
        out.push("done_pattern");
    }
    // "disable" is the inert default the editor sends on every save.
    if body
        .done_action
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty() && s != "disable")
    {
        out.push("done_action");
    }
    if set(&body.trigger_on) {
        out.push("trigger_on");
    }
    if set(&body.trigger_sessions) {
        out.push("trigger_sessions");
    }
    out
}

/// 400 for a write that tries to arm a behaviour this server does not have.
///
/// Refusing rather than accepting-and-ignoring, because the caller cannot tell
/// those apart from a 200: python's contract said these fields did something,
/// the column still exists, and the row round-trips unchanged. A stored value
/// nobody reads reads exactly like a working feature.
fn unhonoured_refusal(fields: &[&'static str]) -> Response {
    err(
        StatusCode::BAD_REQUEST,
        json!({
            "error": format!(
                "this server does not implement {} — refusing rather than storing a value \
                 nothing reads",
                fields.join(", ")
            ),
            "unhonoured": fields,
            "unhonoured_all": UNHONOURED_FIELDS,
            "why": "the pane-polling watcher (watch/done_pattern/done_action) and the \
                    event-trigger loop (trigger_on/trigger_sessions) were not ported from the \
                    Python server; the columns survive so old rows still load",
            "instead": "for a loop that stops when the work is done, have the worker report it \
                        (POST /api/sessions/<name>/report) or disable the schedule from the \
                        session — a regex over a rendered terminal pane cannot see what the \
                        model knows",
            "card": "AMUX-2680",
        }),
    )
}

// ---- shared helpers -------------------------------------------------------

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

use super::internal;

fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, json!({ "error": "not found" }))
}

fn ev(id: &str, mutation: MutationKind) -> PendingEvent {
    PendingEvent {
        entity_type: EntityType::Schedule,
        entity_id: id.to_string(),
        mutation,
        payload: None,
    }
}

/// Park a handler-level outcome in the slot (the workers.rs idiom — the
/// closure's return type is fixed by the writer loop).
fn finish<T>(
    slot: &Mutex<Option<T>>,
    outcome: T,
    write: WriteOutcome,
) -> rusqlite::Result<WriteOutcome> {
    *slot.lock().expect("outcome slot poisoned") = Some(outcome);
    Ok(write)
}

fn no_write() -> WriteOutcome {
    WriteOutcome {
        applied: false,
        events: Vec::new(),
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Python's `_sched_mutation_by`, minus the client-IP tail (see module
/// docs). Never returns an empty string — an unattributed schedule write is
/// the AMUX-1812 forensics gap.
fn mutation_by(headers: &HeaderMap, claimed: Option<&str>) -> String {
    let hdr = header_str(headers, "x-amux-worker")
        .or_else(|| header_str(headers, "x-amux-session"))
        .map(|s| s.chars().take(64).collect::<String>());
    let claimed = claimed
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(64).collect::<String>());
    let who = match (hdr, claimed) {
        (Some(h), Some(c)) if h != c => format!("{h} (claimed {c})"),
        (Some(h), _) => h,
        (None, Some(c)) => c,
        (None, None) => header_str(headers, "x-amux-user-email")
            .map(str::to_string)
            .unwrap_or_else(|| "unattributed-http".into()),
    };
    who.chars().take(64).collect()
}

/// Python's `str()` rendering of a field value, so the two servers' audit
/// rows for the same field agree byte-for-byte where it matters (`1`/`0`
/// for enabled, `None` for null).
fn audit_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::Bool(b)) => {
            if *b {
                "True".into()
            } else {
                "False".into()
            }
        }
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Accept `true`/`false`, `1`/`0`, or `"1"`/`"true"` for flag fields (the
/// dashboard sends numbers, seed scripts send booleans).
fn de_flag<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    let v = Option::<Value>::deserialize(d)?;
    Ok(match v {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(b as i64),
        Some(Value::Number(n)) => Some((n.as_f64().unwrap_or(0.0) != 0.0) as i64),
        Some(Value::String(s)) => Some(matches!(s.as_str(), "1" | "true" | "yes" | "True") as i64),
        Some(_) => return Err(serde::de::Error::custom("expected a boolean or number")),
    })
}

/// Take a write's outcome out of its slot, dropping the guard before the
/// caller awaits anything (a held `MutexGuard` makes the handler `!Send`).
fn take_slot<T>(slot: &Mutex<Option<T>>) -> Option<T> {
    slot.lock().expect("slot").take()
}

/// Present-vs-absent for nullable PATCH fields: absent is `None`, an explicit
/// `null` is `Some(None)` (clear), a value is `Some(Some(v))`.
fn de_present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Ok(Some(Option::<T>::deserialize(d)?))
}

/// The pass condition of one create/PATCH body, validated: `None` = not
/// mentioned, `Some(None)` = clear, `Some(Some(cmd))` = set. Pass/fail on with
/// no check, or a check bash cannot parse, is refused here, so no schedule can
/// say it has a condition it does not have.
fn pass_check_input(body: &ScheduleBody) -> Result<Option<Option<String>>, Box<Response>> {
    if body.pass_fail == Some(0) {
        return Ok(Some(None));
    }
    let required = || {
        tracing::info!(verdict = "schedule_pass_check_refused", reason = "missing", measured = true, n_considered = 1,
            "pass/fail turned on without a pass check");
        err(
            StatusCode::BAD_REQUEST,
            json!({
                "error": "pass/fail needs a pass check: a shell command that exits 0 when the schedule's work is done",
                "code": "pass_check_required",
                "example": "test -s ~/reports/$(date +%F).md",
            }),
        )
    };
    match &body.pass_check {
        None if body.pass_fail == Some(1) => Err(Box::new(required())),
        None => Ok(None),
        Some(c) => match crate::runtime_jobs::schedule_check::validate(c.as_deref()) {
            Ok(None) if body.pass_fail == Some(1) => Err(Box::new(required())),
            Ok(v) => Ok(Some(v)),
            Err(e) => {
                tracing::info!(verdict = "schedule_pass_check_refused", reason = "invalid", measured = true,
                    n_considered = 1, "{e}");
                Err(Box::new(err(StatusCode::BAD_REQUEST, json!({ "error": e, "code": "pass_check_invalid" }))))
            }
        },
    }
}

/// The wake condition of one create/PATCH body, validated like a pass check:
/// `None` = not mentioned, `Some(None)` = clear, `Some(Some(cmd))` = set.
fn when_input(body: &ScheduleBody) -> Result<Option<Option<String>>, Box<Response>> {
    let Some(c) = &body.when else { return Ok(None) };
    match crate::runtime_jobs::schedule_check::validate(c.as_deref()) {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            let e = e.replace("the pass check", "the wake condition");
            tracing::info!(verdict = "schedule_when_refused", reason = "invalid", measured = true, n_considered = 1, "{e}");
            Err(Box::new(err(StatusCode::BAD_REQUEST, json!({ "error": e, "code": "when_invalid" }))))
        }
    }
}

/// A wake condition on a shell schedule would be a condition it does not
/// have: the fire path checks `when` only before a prompt delivery.
fn when_on_shell_refusal() -> Response {
    tracing::info!(verdict = "schedule_when_refused", reason = "shell", measured = true, n_considered = 1,
        "a wake condition was set on a shell schedule");
    err(
        StatusCode::BAD_REQUEST,
        json!({
            "error": "when applies to kind 'tmux' schedules; a shell schedule's command can test the condition itself",
            "code": "when_on_shell",
        }),
    )
}

/// The note a create/PATCH response carries for a schedule with `when`.
fn when_note(o: &serde_json::Map<String, Value>) -> Option<Value> {
    let check = o.get("when_check").and_then(Value::as_str).map(str::trim).filter(|c| !c.is_empty())?;
    let once = o.get("when_once").and_then(Value::as_i64).unwrap_or(1) != 0;
    Some(json!(format!(
        "Each scheduled fire first runs `{check}` as the owner (up to {}s). A fire whose check fails is \
         skipped: no prompt, no model turn, no max_runs spent, run status 'skipped'. So the cadence cost \
         above applies only to fires whose check passes. {} Run now skips the check.",
        crate::runtime_jobs::schedule_wake::WHEN_TIMEOUT_S,
        if once { "when_once: the schedule disables itself after the first fire that passes and delivers." }
        else { "when_once is off: it fires every time the check passes." }
    )))
}

/// The lifecycle fields of one create/PATCH body, validated. Each is
/// `None` = not mentioned, `Some(None)` = clear, `Some(Some(v))` = set.
#[derive(Default)]
struct BoundsInput {
    once: Option<String>,
    not_before: Option<Option<String>>,
    until: Option<Option<String>>,
    max_runs: Option<Option<i64>>,
}

fn bounds_refusal(error: String) -> Response {
    err(
        StatusCode::BAD_REQUEST,
        json!({
            "error": error,
            "code": "schedule_bounds_invalid",
            "accepted": {
                "once": "YYYY-MM-DDTHH:MM (local), RFC 3339, or a delay ('in 20m', '+90s', '1h30m', 'in 2 hours'; units s/m/h/d): fire one time, then done. Run times are whole minutes, so a delay rounds UP to the next minute",
                "after": "datetime or delay: no fire before it",
                "until": "datetime or delay: no fire after it; the schedule ends",
                "max_runs": "integer >= 1: scheduled fires allowed (Run now not counted)",
            },
        }),
    )
}

fn parse_bounds(body: &ScheduleBody, now: chrono::DateTime<Local>) -> Result<BoundsInput, String> {
    use crate::runtime_jobs::schedule_lifecycle::parse_when;
    let now_s = fmt_minute(now);
    let when = |name: &str, v: &Option<Option<String>>| -> Result<Option<Option<String>>, String> {
        match v {
            None => Ok(None),
            Some(None) => Ok(Some(None)),
            Some(Some(x)) if x.trim().is_empty() => Ok(Some(None)),
            Some(Some(x)) => parse_when(x)
                .map(|t| Some(Some(t)))
                .map_err(|e| format!("{name}: {e}")),
        }
    };
    let mut out = BoundsInput {
        not_before: when("after", &body.after)?,
        until: when("until", &body.until)?,
        max_runs: body.max_runs,
        once: None,
    };
    if let Some(o) = body.once.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
        let t = parse_when(o).map_err(|e| format!("once: {e}"))?;
        if body.schedule_expr.as_deref().is_some_and(|e| !e.trim().is_empty()) {
            return Err(
                "once and schedule_expr are exclusive: once fires one time, schedule_expr recurs".into(),
            );
        }
        if t < now_s {
            return Err(format!("once {t} is in the past (now {now_s})"));
        }
        out.once = Some(t);
    }
    if let Some(Some(m)) = out.max_runs {
        if m < 1 {
            return Err(format!("max_runs must be at least 1, got {m}"));
        }
    }
    if let Some(Some(u)) = &out.until {
        if u.as_str() <= now_s.as_str() {
            return Err(format!("until {u} is not in the future (now {now_s})"));
        }
        if let Some(Some(nb)) = &out.not_before {
            if u <= nb {
                return Err(format!("until {u} must be after after/not_before {nb}"));
            }
        }
    }
    Ok(out)
}

/// Write the validated bounds onto a row.
fn apply_bounds(s: &mut DurableSchedule, b: &BoundsInput) {
    let opt = |v: &Option<String>| v.clone().map(Value::from).unwrap_or(Value::Null);
    if let Some(v) = &b.not_before {
        s.set("not_before", opt(v));
    }
    if let Some(v) = &b.until {
        s.set("until", opt(v));
    }
    if let Some(v) = b.max_runs {
        s.set("max_runs", v.map(Value::from).unwrap_or(Value::Null));
    }
    if let Some(t) = &b.once {
        s.set("sched_type", json!("once"));
        s.set("run_at", json!(t));
        s.set("schedule_expr", Value::Null);
    }
}

/// The first fire a recurring row should carry now, honouring `not_before`.
fn bounded_next(s: &DurableSchedule, parsed: &ScheduleExpr, now: chrono::DateTime<Local>) -> Option<String> {
    crate::runtime_jobs::schedule_lifecycle::Bounds::of(s)
        .first_after(parsed, now)
        .map(fmt_minute)
}

/// The 409 for a worker re-enabling (or running) an owner-locked schedule.
fn owner_lock_refusal(id: &str, verb: &str, by: &str, s: &DurableSchedule) -> Response {
    tracing::warn!(schedule = id, verb, caller = by, locked_by = %s.str_field("locked_by"),
        verdict = "schedule_owner_lock_refused", measured = true, n_considered = 1,
        "a non-owner tried to re-enable or run a schedule the owner disabled");
    err(
        StatusCode::CONFLICT,
        json!({
            "error": format!("{id} was disabled by the owner and is locked; a worker cannot {verb} it"),
            "code": "schedule_owner_locked",
            "locked_by": s.str_field("locked_by"),
            "lock_reason": s.str_field("lock_reason"),
            "locked_at": s.raw().get("locked_at").cloned().unwrap_or(Value::Null),
            "how": "ask the owner; an owner PATCH {\"enabled\":1} (dashboard, or a request with no worker header) clears the lock",
        }),
    )
}

fn bad_expr(expr: &str, e: &scheduler::ExprParseError) -> Response {
    err(
        StatusCode::BAD_REQUEST,
        json!({
            "error": "unparseable schedule_expr",
            "schedule_expr": expr,
            "detail": e.to_string(),
            // Python accepted this and mis-armed it (the accepts-garbage
            // incident); refusing WITH the grammar is the honest exit.
            "accepted": [
                "every Nm/Nh/Nd", "daily at HH:MM", "every weekday at HH:MM",
                "weekly on <day> at HH:MM", "monthly on 1-28 at HH:MM",
                "MIN HOUR DOM MON DOW (5-field cron)"
            ],
            "one_off": "for a single fire pass once:'YYYY-MM-DDTHH:MM' instead of an expression",
        }),
    )
}

// ---- GET /api/schedules ---------------------------------------------------

#[derive(Deserialize)]
pub struct ListParams {
    /// Scope: only schedules bound to this session (Invariant 2's shape —
    /// a schedule bound to a session surfaces for it, not for everyone).
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

pub async fn list(State(state): State<AppState>, Query(p): Query<ListParams>) -> Response {
    let store = state.store.clone();
    let joined = crate::db::interactions::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = store.read()?;
        let rows = list_schedules(&conn, p.session.as_deref())?;
        // Measured fires/day + fleet share (Python parity: the runaway-canary
        // audit — counted from actual runs, not parsed from the expr).
        let day_ago = chrono::Utc::now().timestamp() - 86_400;
        let mut fires: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        // Failures are counted from the SAME rows and window as `fires_day`, so
        // the Scheduler page's Runs/day and Failures columns cannot disagree
        // about which runs they looked at. A failure is a run whose recorded
        // verdict is `error` (delivery or command failed) or `refused` (never
        // delivered). `skipped` is a deliberate no-op and is not a failure.
        let mut fails: std::collections::HashMap<String, (i64, Option<i64>)> =
            std::collections::HashMap::new();
        // GS-12 finding 5: `interrupted` (a restart mid-run) and failures on an
        // expired USER credential are counted apart, so 881 identical token
        // failures read as one cause and a restart does not read as a bug.
        let mut extra: std::collections::HashMap<String, (i64, i64)> =
            std::collections::HashMap::new();
        {
            let sql = format!(
                "SELECT schedule_id, COUNT(*), \
                        SUM(CASE WHEN status IN ('error','refused') THEN 1 ELSE 0 END), \
                        MAX(CASE WHEN status IN ('error','refused') THEN ran_at END), \
                        SUM(CASE WHEN status='interrupted' THEN 1 ELSE 0 END), \
                        SUM(CASE WHEN status IN ('error','refused') AND {} THEN 1 ELSE 0 END) \
                 FROM schedule_runs WHERE ran_at >= ?1 GROUP BY schedule_id",
                lifecycle::credential_sql_predicate()
            );
            let mut stmt = conn.prepare(&sql)?;
            let it = stmt.query_map([day_ago], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                ))
            })?;
            for row in it {
                let (k, v, f, last_f, intr, cred) = row?;
                fires.insert(k.clone(), v);
                fails.insert(k.clone(), (f, last_f));
                extra.insert(k, (intr, cred));
            }
        }
        // Every run row the window considered, fleet-wide: the denominator
        // that makes a 0 beside it readable as "quiet", not "never measured".
        let window_rows: i64 = fires.values().sum::<i64>();
        let total: i64 = window_rows.max(1);
        let now = Local::now();
        let out: Vec<Value> = rows
            .iter()
            .map(|s| {
                let mut v = s.to_json();
                crate::runtime_jobs::schedule_failure::expose(&mut v);
                expose_instants(&mut v);
                let f = *fires.get(s.id()).unwrap_or(&0);
                v["fires_day"] = json!(f);
                v["fleet_share"] = json!((100.0 * f as f64 / total as f64 * 10.0).round() / 10.0);
                let (nf, last_fail) = fails.get(s.id()).copied().unwrap_or((0, None));
                let (interrupted, cred) = extra.get(s.id()).copied().unwrap_or((0, 0));
                v["run_stats_24h"] = json!({
                    "runs": f,
                    "failures": nf,
                    "last_failure_at": last_fail,
                    "interrupted": interrupted,
                    "credential_failures": cred,
                    "credential_cause": if cred > 0 { json!("expired_user_credential") } else { Value::Null },
                    "window_s": 86_400,
                    "failure_statuses": ["error", "refused"],
                    "measured": true,
                    "n_considered": window_rows,
                });
                v["locked"] = json!(!s.str_field("locked_by").trim().is_empty());
                let (class, planned) = cadence_class(s.schedule_expr(), s.str_field("sched_type"));
                v["cadence_class"] = json!(class);
                v["planned_fires_day"] = planned
                    .map(|p| json!((p * 100.0).round() / 100.0))
                    .unwrap_or(Value::Null);
                // The Rust parser's own next-run, alongside the stored
                // next_run (which Python may own): shadow-comparable, and
                // never clobbers the field the dashboard renders.
                v["computed_next_run"] = s
                    .schedule_expr()
                    .and_then(|e| ScheduleExpr::parse(e).ok())
                    .and_then(|e| e.next_run_after(now))
                    .map(|t| json!(fmt_minute(t)))
                    .unwrap_or(Value::Null);
                v
            })
            .collect();
        Ok(out)
    })
    .await;
    match joined {
        Ok(Ok(out)) => {
            let total = out.len();
            let offset = p.offset.unwrap_or(0);
            let page: Vec<Value> = if offset >= out.len() {
                Vec::new()
            } else if let Some(lim) = p.limit {
                out.into_iter().skip(offset).take(lim).collect()
            } else {
                out.into_iter().skip(offset).collect()
            };
            let mut headers = HeaderMap::new();
            let put = |h: &mut HeaderMap, k: &str, v: String| {
                if let Ok(name) = k.parse::<axum::http::header::HeaderName>() {
                    if let Ok(val) = v.parse() {
                        h.insert(name, val);
                    }
                }
            };
            put(&mut headers, "x-amux-total", total.to_string());
            put(&mut headers, "x-amux-offset", offset.to_string());
            put(&mut headers, "x-amux-returned", page.len().to_string());
            // Point at the audit from the payload people actually read
            // (AMUX-2416: the trail existed, nobody found it).
            put(
                &mut headers,
                "x-amux-audit",
                "/api/schedules/audit?id=<SCHED-N>&limit=100".to_string(),
            );
            // Declared on the payload people READ, not only on the
            // write that gets refused (ethos rule 4): every row here
            // still carries these columns, and a value in a column is
            // indistinguishable from a working feature otherwise.
            put(
                &mut headers,
                "x-amux-unhonoured-fields",
                UNHONOURED_FIELDS_HEADER.to_string(),
            );
            (StatusCode::OK, headers, Json(Value::Array(page))).into_response()
        }
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

/// Which group the Scheduler page files a schedule under, decided by the SAME
/// parser that fires it (ethos rule 1: a view shares the mechanism's
/// predicate). Returns the class and the planned fires per day.
///
/// - `loop`: fires more often than hourly (an interval under 1h, or a cron
///   that averages more than 24 fires a day).
/// - `routine`: hourly to daily, including `every weekday at`.
/// - `weekly`: weekly, monthly, or an interval longer than a day.
/// - `once`: a one-shot with no recurring expression.
/// - `unknown`: an expression this parser cannot read. It will not fire
///   either, so the page must not file it as a healthy routine.
///
/// Cron is COUNTED over the next 7 days rather than 24 hours: a weekly cron
/// fires zero or one time in any given day and would otherwise be filed by
/// the weekday the page happened to be opened on.
pub(crate) fn cadence_class(expr: Option<&str>, sched_type: &str) -> (&'static str, Option<f64>) {
    let Some(expr) = expr.map(str::trim).filter(|e| !e.is_empty()) else {
        return (if sched_type == "once" { "once" } else { "unknown" }, None);
    };
    let Ok(parsed) = ScheduleExpr::parse(expr) else {
        return ("unknown", None);
    };
    // One date a year: filed apart, so 49 one-offs written this way stop
    // reading as "weekly" (GS-12 finding 5).
    if lifecycle::is_fixed_date_cron(expr) {
        return ("yearly", Some(1.0 / 365.0));
    }
    let per_day = match &parsed {
        ScheduleExpr::Cron(sched) => {
            let now = Local::now();
            let until = now + chrono::Duration::days(7);
            let n = sched.after(&now).take(20_000).take_while(|t| *t < until).count();
            (n < 20_000).then_some(n as f64 / 7.0)
        }
        other => scheduler::fires_per_day(other),
    };
    let class = match &parsed {
        ScheduleExpr::Daily { .. } | ScheduleExpr::Weekday { .. } => "routine",
        ScheduleExpr::Weekly { .. } | ScheduleExpr::Monthly { .. } => "weekly",
        _ => match per_day {
            Some(p) if p > 24.0 + 1e-9 => "loop",
            Some(p) if p >= 5.0 / 7.0 - 1e-9 => "routine",
            Some(_) => "weekly",
            // A cron too dense to count is denser than hourly.
            None => "loop",
        },
    };
    (class, per_day)
}

// ---- GET /api/schedules/{id} ----------------------------------------------

/// `next_run`/`run_at` are this server's wall clock with no zone. A browser in
/// another zone read them as its own local time, so a cloud server in UTC showed
/// an every-minute schedule as due in "3h 59m" to a customer in New York (amux
/// Cloud e2e, 2026-10-18). Publish the same instants as epoch milliseconds.
fn expose_instants(v: &mut Value) {
    let Some(o) = v.as_object_mut() else { return };
    for (field, ms_field) in [("next_run", "next_run_ms"), ("run_at", "run_at_ms")] {
        let ms = o
            .get(field)
            .and_then(Value::as_str)
            .and_then(scheduler::parse_minute)
            .map(|t| json!(t.timestamp_millis()))
            .unwrap_or(Value::Null);
        o.insert(ms_field.into(), ms);
    }
    // The wake condition under the API's own field names (stored as
    // `when_check`/`when_once`).
    let when = o.get("when_check").cloned().filter(|v| v.as_str().is_some_and(|s| !s.trim().is_empty())).unwrap_or(Value::Null);
    let once = o.get("when_once").and_then(Value::as_i64).map(|n| n != 0).unwrap_or(true);
    o.insert("when".into(), when);
    o.insert("when_once".into(), json!(once));
}

pub async fn get_one(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.clone();
    let joined = crate::db::interactions::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = store.read()?;
        Ok(get_schedule(&conn, &id)?)
    })
    .await;
    match joined {
        Ok(Ok(Some(s))) => {
            let mut v = s.to_json();
            crate::runtime_jobs::schedule_failure::expose(&mut v);
            expose_instants(&mut v);
            // A tombstone must not read as live (AMUX-3431): the raw row
            // carries enabled:1 next to deleted:<ts>, and SCHED-349's owner
            // read exactly that as a working schedule for five days. Say it
            // in words, not just a timestamp field.
            if s.is_deleted() {
                if let Some(o) = v.as_object_mut() {
                    o.insert("tombstone".into(), json!(true));
                    o.insert(
                        "note".into(),
                        json!("DELETED — this schedule never fires and the scheduler's list excludes it. PATCH {\"resurrect\": true} to restore, or POST /api/schedules to recreate."),
                    );
                }
            }
            (
                [("x-amux-unhonoured-fields", UNHONOURED_FIELDS_HEADER)],
                Json(v),
            )
                .into_response()
        }
        Ok(Ok(None)) => not_found(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- POST /api/schedules --------------------------------------------------

/// Create/patch body: Python's accepted field set, exactly. Unknown fields
/// are rejected (Invariant 37), which is also what keeps this endpoint from
/// silently dropping a typo like `schedule_exp`.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ScheduleBody {
    #[serde(default)]
    pub title: Option<String>,
    /// `worker` is the DASHBOARD's spelling for the same field — the sched
    /// editor posts `worker:` (app.js:19067) and deny_unknown_fields turned
    /// every UI create into a 422 (Ethan's repro, 2026-08-09). Alias, don't
    /// loosen: unknown-field rejection still catches real typos.
    #[serde(default, alias = "worker")]
    pub session: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    /// Snapshot a self-contained script into durable AMUX_HOME storage. The
    /// command must contain one unquoted {script} token and kind must be shell.
    #[serde(default)]
    pub script_path: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub sched_type: Option<String>,
    #[serde(default)]
    pub recurrence: Option<String>,
    #[serde(default)]
    pub run_at: Option<String>,
    #[serde(default, deserialize_with = "de_flag")]
    pub enabled: Option<i64>,
    #[serde(default)]
    pub schedule_expr: Option<String>,
    #[serde(default, deserialize_with = "de_flag")]
    pub watch: Option<i64>,
    #[serde(default)]
    pub watch_timeout: Option<i64>,
    #[serde(default)]
    pub done_pattern: Option<String>,
    #[serde(default)]
    pub done_action: Option<String>,
    #[serde(default)]
    pub trigger_on: Option<String>,
    #[serde(default)]
    pub trigger_cooldown: Option<i64>,
    #[serde(default)]
    pub trigger_sessions: Option<String>,
    #[serde(default)]
    pub exit_actions: Option<Value>,
    /// Pass/fail (runtime_jobs/schedule_check.rs): a shell command whose exit
    /// code is the verdict, 0 = pass. `null` or "" clears it.
    #[serde(default, deserialize_with = "de_present")]
    pub pass_check: Option<Option<String>>,
    /// The editor's pass/fail switch. On requires a `pass_check` in the same
    /// body; off clears the check.
    #[serde(default, deserialize_with = "de_flag")]
    pub pass_fail: Option<i64>,
    /// Wake condition (runtime_jobs/schedule_wake.rs, stored as `when_check`):
    /// a shell command run before each scheduled fire of a tmux schedule.
    /// Exit 0 delivers the prompt; anything else skips the fire. `null` or ""
    /// clears it.
    #[serde(default, deserialize_with = "de_present")]
    pub when: Option<Option<String>>,
    /// With `when`: disable the schedule after the first fire whose check
    /// passed and that delivered (default true). False keeps it firing every
    /// time the check passes.
    #[serde(default, deserialize_with = "de_flag")]
    pub when_once: Option<i64>,
    /// When set, the scheduler creates an ephemeral worktree session for each
    /// firing instead of sending the command to the named session directly.
    /// The worktree is torn down after the command completes and pushes.
    #[serde(default, deserialize_with = "de_flag")]
    pub worktree: Option<i64>,
    /// When set, the command is treated as priorities for fan-out: the scheduler
    /// calls /api/board/launch to create an epic and spin up ephemeral workers
    /// instead of sending the command as a plain message.
    #[serde(default, deserialize_with = "de_flag")]
    pub fan_out: Option<i64>,
    /// Model for fan-out workers (default "haiku").
    #[serde(default)]
    pub fan_out_model: Option<String>,
    /// Claimed attribution (weaker than the header; see `mutation_by`).
    #[serde(default)]
    pub by: Option<String>,
    /// Fire ONE time at this local datetime, then done (`sched_type=once`).
    /// The honest spelling of a one-off: 49 GS-12 one-offs were written as
    /// fixed-date yearly crons because nothing else said "once, at 09:05".
    #[serde(default)]
    pub once: Option<String>,
    /// No fire before this datetime (stored as `not_before`). `null` or ""
    /// clears it on PATCH.
    #[serde(default, alias = "not_before", deserialize_with = "de_present")]
    pub after: Option<Option<String>>,
    /// No fire after this datetime; the schedule ends with a reason. `null`
    /// or "" clears it on PATCH.
    #[serde(default, deserialize_with = "de_present")]
    pub until: Option<Option<String>>,
    /// Scheduled fires allowed (Run now is not counted), then it ends. Setting
    /// it on PATCH starts a new budget. `null` clears it.
    #[serde(default, deserialize_with = "de_present")]
    pub max_runs: Option<Option<i64>>,
    /// Why it is being disabled. An OWNER disable locks the schedule with this
    /// reason; the owning worker is told it.
    #[serde(default)]
    pub reason: Option<String>,
    /// PATCH-only: explicitly restore a soft-deleted schedule (AMUX-3431).
    /// Without this a PATCH to a tombstone is refused with 409 — silently
    /// accepting the write is how a deleted burn backstop read as live for
    /// five days while the scheduler (which reads the list) never fired it.
    #[serde(default)]
    pub resurrect: Option<bool>,
}

/// Bind a shell schedule to durable script bytes before its first fire. A
/// transient /tmp path otherwise disappears at reboot while the schedule lives.
fn prepare_script(body: &mut ScheduleBody, home: &std::path::Path) -> Result<(), String> {
    let Some(source) = body.script_path.as_deref() else { return Ok(()) };
    if body.kind.as_deref() != Some("shell") {
        return Err("script_path requires explicit kind:'shell'".into());
    }
    let command = body.command.as_deref().ok_or("script_path requires command with one unquoted {script} token")?;
    let placeholder = regex::Regex::new(r"(^|[ \t])\{script\}($|[ \t|;&])").expect("fixed pattern");
    if command.matches("{script}").count() != 1 || !placeholder.is_match(command) {
        return Err("command must contain exactly one unquoted {script} token".into());
    }
    let source = std::path::Path::new(source);
    if !source.is_file() { return Err("script_path must name an existing regular file".into()); }
    use std::io::{Read, Write};
    let mut bytes = Vec::new();
    std::fs::File::open(source).map_err(|e| e.to_string())?.take(1_048_577).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() > 1_048_576 { return Err("script_path exceeds the 1 MiB script limit".into()); }
    use sha2::{Digest, Sha256};
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let extension = source.extension().and_then(|e| e.to_str()).filter(|e| !e.is_empty() && e.bytes().all(|b| b.is_ascii_alphanumeric()));
    let folder = home.join("schedule-artifacts");
    std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let path = folder.join(format!("{hash}{}", extension.map(|e| format!(".{e}")).unwrap_or_default()));
    match std::fs::read(&path) {
        Ok(existing) if existing != bytes => return Err("durable script digest mismatch; existing evidence retained".into()),
        Ok(_) => {},
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let tmp = folder.join(format!(".{}.tmp", ulid::Ulid::new()));
            let result = (|| -> std::io::Result<()> {
                let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
                #[cfg(unix)] {
                    use std::os::unix::fs::PermissionsExt;
                    file.set_permissions(std::fs::Permissions::from_mode(0o700))?;
                }
                file.write_all(&bytes)?;
                file.sync_all()?;
                std::fs::rename(&tmp, &path)
            })();
            if result.is_err() { let _ = std::fs::remove_file(&tmp); }
            result.map_err(|e| e.to_string())?;
        },
        Err(e) => return Err(e.to_string()),
    }
    // POSIX single-quote escaping, including an AMUX_HOME containing a quote.
    let quoted = format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    body.command = Some(command.replace("{script}", &quoted));
    tracing::info!(sha256 = hash, bytes = bytes.len(), measured = true, n_considered = 1,
        verdict = "schedule_script_snapshotted", "shell schedule bound to durable script bytes");
    Ok(())
}

/// The cadence note a caller sees AT CREATION (AMUX-3546).
///
/// 165 schedules were enabled on this machine when this was written, roughly
/// 1,650 turns/day from 34 rows. `every 15m` is eleven characters and reads like
/// a small choice; it is 96 turns a day, and five of them were enabled. Nothing
/// said so at the moment of the decision, which is the only moment the
/// information is free.
///
/// STATES, NEVER REFUSES — the card is explicit about this and it is right. A
/// ten-minute poll is a legitimate thing to want.
///
/// THE PLAN-TIER HALF IS NOT BUILT, and that is a finding rather than a
/// shortcut. The card asked to flag sub-hourly schedules against the plan tier
/// and to CHECK first whether the tier is available. It is not: `/api/usage`
/// returns `limit_dollars: null` on every window and `spend.limit: null`, so
/// there is no tier to compare against. Inventing a threshold instead would be
/// a tuned parameter standing in for a fact, so the note reports the rate and
/// the implied spend and leaves the judgment where it belongs.
///
/// The per-fire figure is `kind`-dependent and that is the whole point: a
/// `shell` schedule wakes no model and costs nothing per fire, which is the
/// choice AF-216's `kind_note` exists to surface. Only a `tmux` schedule gets a
/// spend line.
fn cadence_note(expr: &str, kind: &str) -> Option<Value> {
    let parsed = ScheduleExpr::parse(expr).ok()?;
    let per_day = crate::runtime_jobs::scheduler::fires_per_day(&parsed)?;
    let per_day_r = (per_day * 10.0).round() / 10.0;
    let mut note = format!("This expression fires ~{per_day_r} time(s) a day");
    if kind == "tmux" {
        // ~$6/fire, the figure AF-216 measured on 2026-08-24 and which the
        // `kind_note` beside this one already quotes. Reused rather than
        // re-derived so the two lines in one response cannot disagree.
        let usd = (per_day * 6.0 * 100.0).round() / 100.0;
        note.push_str(&format!(
            ", and kind='tmux' wakes a full model turn on each one — about ${usd}/day at the \
             ~$6/fire measured in AF-216. Nothing is refused; the number is here because \
             creation is the only moment it is free."
        ));
    } else {
        note.push_str(&format!(
            ". kind='{kind}' runs directly and wakes no model turn, so the fire rate costs \
             nothing per fire."
        ));
    }
    Some(json!(note))
}

/// A warning for a schedule whose target lane is isolated (AMUX-5241).
///
/// desktop's scheduled escalation into an isolated lane was refused and nothing
/// said so at the one moment it was cheap to hear: creation. Since 2c1d5481 a
/// schedule's OWN command is delivered into an isolated worker as owner input,
/// so the schedule itself works. What does not is everything it sets in motion
/// that is not the schedule: a peer relay, a board nudge or a callback into that
/// lane answers 403 `isolated_target`. That boundary is what the caller needs to
/// know before relying on the schedule for an escalation chain.
///
/// STATES, NEVER REFUSES, like `cadence_note`: isolated workers are allowed
/// schedules (Ethan, 2026-09-26). Pure on `isolated` so the wording is
/// testable without an AMUX_HOME.
fn isolated_target_note(session: &str, kind: &str, isolated: bool) -> Option<Value> {
    if !isolated || session.trim().is_empty() {
        return None;
    }
    let what = if kind == "shell" {
        "A shell schedule runs on the host and only reaches the lane to report a failure, \
         which is delivered."
    } else {
        "This schedule's own command IS delivered, as owner input (a schedule is standing \
         configuration of the worker)."
    };
    Some(json!({
        "code": "isolated_target",
        "session": session,
        "message": format!(
            "target '{session}' is an isolated (raw-agent) worker. {what} Anything else aimed \
             at it is refused with 403 code isolated_target: peer sends (amux send from \
             another lane), board nudges and task callbacks. If this schedule is meant to \
             start a chain that messages '{session}' from another lane, that step will fail; \
             clear CC_ISOLATED on the lane or deliver from the owner instead."
        ),
    }))
}

/// Attach [`isolated_target_note`] to a create/PATCH response, with its log line.
fn attach_isolated_note(o: &mut serde_json::Map<String, Value>, verb: &str) {
    let session = o.get("session").and_then(Value::as_str).unwrap_or("").to_string();
    let kind = o.get("kind").and_then(Value::as_str).unwrap_or("tmux").to_string();
    let isolated = !session.trim().is_empty()
        && crate::api::session_verbs::session_is_isolated(&session);
    if let Some(n) = isolated_target_note(&session, &kind, isolated) {
        let id = o.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        tracing::info!(
            schedule = %id,
            session = %session, kind = %kind, verb,
            verdict = "schedule_target_isolated",
            "schedule targets an isolated worker; warned in the response (AMUX-5241)"
        );
        o.insert("isolated_target".into(), n);
    }
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<ScheduleBody>,
) -> Response {
    if let Err(error) = prepare_script(&mut body, &crate::config::amux_home()) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": error, "code": "schedule_script_invalid"}))).into_response();
    }

    let title = match body.title.as_deref().map(str::trim) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                json!({ "error": "title is required" }),
            )
        }
    };
    // Refuse to arm a behaviour this server does not have (AMUX-2680).
    let dead = armed_unhonoured(&body);
    if !dead.is_empty() {
        return unhonoured_refusal(&dead);
    }
    let pass_in = match pass_check_input(&body) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let when_in = match when_input(&body) {
        Ok(w) => w,
        Err(r) => return *r,
    };
    if matches!(when_in, Some(Some(_))) && body.kind.as_deref() == Some("shell") {
        return when_on_shell_refusal();
    }
    let by = mutation_by(&headers, body.by.as_deref());
    let now = Local::now();
    let now_ts = chrono::Utc::now().timestamp();
    let bounds_in = match parse_bounds(&body, now) {
        Ok(b) => b,
        Err(e) => return bounds_refusal(e),
    };
    if let Some(t) = &bounds_in.once {
        body.run_at = Some(t.clone());
        body.sched_type = Some("once".into());
    }
    let run_at = body.run_at.clone().unwrap_or_else(|| fmt_minute(now));

    // next_run — prefer schedule_expr (forces recurring), like Python; but
    // an unparseable expr is a 400 naming the grammar, not a quiet mis-arm.
    let expr = body
        .schedule_expr
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mut probe_row = DurableSchedule::from_map(Map::new());
    apply_bounds(&mut probe_row, &bounds_in);
    let (sched_type, next_run) = match expr {
        Some(e) => match ScheduleExpr::parse(e) {
            Ok(parsed) => (
                "recurring".to_string(),
                bounded_next(&probe_row, &parsed, now).unwrap_or_else(|| run_at.clone()),
            ),
            Err(perr) => return bad_expr(e, &perr),
        },
        None => {
            let st = body.sched_type.clone().unwrap_or_else(|| "once".into());
            let nr = legacy_next_run(&st, body.recurrence.as_deref(), Some(&run_at), now)
                .unwrap_or_else(|| run_at.clone());
            (st, nr)
        }
    };

    let mut m = Map::new();
    m.insert("title".into(), json!(title));
    m.insert(
        "session".into(),
        json!(body.session.clone().unwrap_or_default()),
    );
    m.insert(
        "command".into(),
        json!(body.command.clone().unwrap_or_default()),
    );
    // KIND DEFAULTS TO `tmux`, WHICH IS THE EXPENSIVE ONE, AND THE CALLER IS NOW
    // TOLD (AF-216). `tmux` delivers the command to a lane as a PROMPT and wakes a
    // full turn-loop; `shell` runs it directly and costs nothing.
    //
    // Measured 2026-08-24: 53 enabled schedules, 50 `tmux` and 3 `shell`, while
    // "a schedule fired" cost $1,330 across 2,602 turns in 24h — 214 declared
    // fires/day, so ~12 turns and ~$6.20 PER FIRE. The mechanism to avoid that
    // works and reaches 3 of 53, which is ethos rule 1 and the mcp.json shape.
    //
    // NOT AUTO-SWITCHED, deliberately. A command that LOOKS like shell can still
    // need a lane — it may expect the worker's cwd, its env, or its agent. Silently
    // running such a schedule headless would break it in a way nobody would trace
    // back to here. So the default is unchanged and the response says what was
    // picked and how to change it: the caller decides, having been told.
    let kind = body.kind.clone().unwrap_or_else(|| "tmux".into());
    let kind_defaulted = body.kind.is_none();
    m.insert("kind".into(), json!(kind));
    m.insert("sched_type".into(), json!(sched_type));
    m.insert(
        "recurrence".into(),
        body.recurrence
            .clone()
            .map(Value::from)
            .unwrap_or(Value::Null),
    );
    m.insert("run_at".into(), json!(run_at));
    m.insert("next_run".into(), json!(next_run));
    m.insert("last_run".into(), Value::Null);
    // Honor an EXPLICIT enabled (the AC-139 lesson: a literal 1 silently
    // discarded seeded `enabled:0` and burned a prospect's trial budget).
    m.insert("enabled".into(), json!(body.enabled.unwrap_or(1)));
    m.insert("run_count".into(), json!(0));
    m.insert(
        "schedule_expr".into(),
        expr.map(Value::from).unwrap_or(Value::Null),
    );
    m.insert("watch".into(), json!(body.watch.unwrap_or(0)));
    m.insert(
        "watch_timeout".into(),
        json!(body.watch_timeout.unwrap_or(120)),
    );
    m.insert(
        "done_pattern".into(),
        body.done_pattern
            .clone()
            .map(Value::from)
            .unwrap_or(Value::Null),
    );
    m.insert(
        "done_action".into(),
        json!(body.done_action.clone().unwrap_or_else(|| "disable".into())),
    );
    m.insert(
        "trigger_on".into(),
        body.trigger_on
            .clone()
            .filter(|s| !s.trim().is_empty())
            .map(Value::from)
            .unwrap_or(Value::Null),
    );
    m.insert(
        "trigger_cooldown".into(),
        json!(body.trigger_cooldown.unwrap_or(120)),
    );
    m.insert(
        "trigger_sessions".into(),
        body.trigger_sessions
            .clone()
            .filter(|s| !s.trim().is_empty())
            .map(Value::from)
            .unwrap_or(Value::Null),
    );
    m.insert("worktree".into(), json!(body.worktree.unwrap_or(0)));
    m.insert("fan_out".into(), json!(body.fan_out.unwrap_or(0)));
    m.insert(
        "fan_out_model".into(),
        body.fan_out_model
            .clone()
            .map(Value::from)
            .unwrap_or(Value::Null),
    );
    m.insert(
        "exit_actions".into(),
        match &body.exit_actions {
            Some(Value::Object(o)) => Value::String(Value::Object(o.clone()).to_string()),
            Some(Value::String(s)) => Value::String(s.clone()),
            _ => Value::Null,
        },
    );
    if let Some(Some(check)) = &pass_in {
        // `probe`: the sweep runs the check once to show the current state.
        m.insert("pass_check".into(), json!(check));
        m.insert("pass_state".into(), json!("probe"));
        m.insert("pass_next_at".into(), json!(now_ts));
    }
    if let Some(Some(check)) = &when_in {
        m.insert("when_check".into(), json!(check));
    }
    m.insert("when_once".into(), json!(body.when_once.unwrap_or(1)));
    m.insert("created".into(), json!(now_ts));
    m.insert("updated".into(), json!(now_ts));
    m.insert("deleted".into(), Value::Null);
    for k in ["not_before", "until", "max_runs"] {
        if let Some(v) = probe_row.raw().get(k) {
            m.insert(k.into(), v.clone());
        }
    }
    let fixed_date = expr.is_some_and(lifecycle::is_fixed_date_cron)
        && bounds_in.max_runs.flatten().is_none()
        && bounds_in.until.clone().flatten().is_none();

    let slot: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let by_w = by.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let id = mint_schedule_id(conn)?;
            let mut m = m;
            m.insert("id".into(), json!(id));
            let row = DurableSchedule::from_map(m);
            insert_schedule(conn, &row)?;
            // Creation is a mutation too (AMUX-1812: unaudited creates were
            // half the forensics gap). created-enabled is the AC-139
            // discriminator — record it.
            let summary = json!({
                "title": row.str_field("title"),
                "session": row.str_field("session"),
                "expr": row.schedule_expr().unwrap_or(row.str_field("run_at")),
                "enabled": row.i64_field("enabled", 1),
                "kind": row.str_field("kind"),
                "not_before": row.raw().get("not_before").cloned().unwrap_or(Value::Null),
                "until": row.raw().get("until").cloned().unwrap_or(Value::Null),
                "max_runs": row.raw().get("max_runs").cloned().unwrap_or(Value::Null),
            });
            insert_audit(
                conn,
                &id,
                "created",
                "",
                &summary.to_string(),
                "api-create",
                &by_w,
            )?;
            let events = vec![ev(&id, MutationKind::Created)];
            finish(
                &slot_w,
                row.to_json(),
                WriteOutcome {
                    applied: true,
                    events,
                },
            )
        })
        .await;
    match write {
        Ok(_) => {
            let mut body = slot.lock().expect("slot").take().unwrap_or(Value::Null);
            // Say it in the RESPONSE, not only in the docs. A caller who never
            // passed `kind` has no reason to know there was a choice, and the
            // scheduler section of CLAUDE.md did not list the field either — so
            // the expensive default was unanimous and uninformed.
            if let Value::Object(o) = &mut body {
                let expr = o
                    .get("schedule_expr")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let kind = o
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("tmux")
                    .to_string();
                if let Some(n) = cadence_note(&expr, &kind) {
                    o.insert("cadence_note".into(), n);
                }
                if fixed_date {
                    let id = o.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    tracing::info!(schedule = %id, expr = %expr, verdict = "schedule_fixed_date_cron",
                        measured = true, n_considered = 1,
                        "schedule created with a fixed-date cron and no bound; told it recurs yearly");
                    o.insert("fixed_date_note".into(), json!(lifecycle::fixed_date_note(&expr)));
                }
                if let Some(n) = when_note(o) {
                    o.insert("when_note".into(), n);
                }
                attach_isolated_note(o, "create");
            }
            if kind_defaulted {
                if let Value::Object(o) = &mut body {
                    o.insert(
                        "kind_note".into(),
                        json!(
                            "kind defaulted to 'tmux': this command is delivered to the                              session as a PROMPT and wakes a full model turn on every fire                              (~$6/fire measured 2026-08-24). If it is a self-contained                              command that needs no agent, pass kind:'shell' and it runs                              directly for nothing. AF-216."
                        ),
                    );
                }
            }
            // Same absolute instants the list publishes, so a client can show "Next run" from
            // the create answer without parsing the server's zone-less wall clock.
            expose_instants(&mut body);
            (StatusCode::CREATED, Json(body)).into_response()
        }
        Err(e) => internal(e),
    }
}

// ---- PATCH /api/schedules/{id} --------------------------------------------

/// Who deleted this schedule, from the audit trail (best effort — the trail
/// predates nothing here: deletes have been audited since AMUX-1812).
fn deleter_of(conn: &rusqlite::Connection, id: &str) -> Option<String> {
    conn.query_row(
        "SELECT by_who FROM schedule_audit WHERE schedule_id = ?1 AND field = 'deleted' \
         ORDER BY ts DESC, id DESC LIMIT 1",
        [id],
        |r| r.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
    .filter(|s| !s.is_empty())
}

/// The refusal every write path answers on a tombstone (AMUX-3431). A deleted
/// schedule NEVER fires — the scheduler reads the list, which excludes it — so
/// accepting a write here manufactures fictional coverage: SCHED-349's owner
/// PATCHed a deleted burn backstop twice, got 200s, and the fleet's >=85%
/// pager was fiction for five days. Names the deletion (when, by whom) and
/// both honest exits. The WARN is the log-surfacing half of the two-fixes
/// rule: grep `schedule_write_on_tombstone`.
fn tombstone_refusal(id: &str, verb: &str, deleted_at: i64, deleted_by: Option<&str>) -> Response {
    tracing::warn!(
        "schedule_write_on_tombstone: {verb} {id} refused — deleted at {deleted_at} by {}",
        deleted_by.unwrap_or("?")
    );
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": format!("schedule {id} is deleted — {verb} refused"),
            "blocked": true,
            "deleted_at": deleted_at,
            "deleted_by": deleted_by,
            "how": {
                "resurrect": format!("PATCH /api/schedules/{id} with {{\"resurrect\": true}} restores it (audited), then it fires again"),
                "recreate": "POST /api/schedules creates a fresh one",
            },
        })),
    )
        .into_response()
}

pub async fn patch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(mut body): Json<ScheduleBody>,
) -> Response {
    if let Err(error) = prepare_script(&mut body, &crate::config::amux_home()) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": error, "code": "schedule_script_invalid"}))).into_response();
    }

    // Validate a caller-supplied expr OUTSIDE the write (clean 400 before
    // the writer thread is touched). A stored legacy/garbage expr on a row
    // this PATCH does not touch falls back like Python instead of bricking
    // the update.
    if let Some(e) = body
        .schedule_expr
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Err(perr) = ScheduleExpr::parse(e) {
            return bad_expr(e, &perr);
        }
    }
    // Same gate as create (AMUX-2680) — a PATCH is how the editor armed these
    // on an existing row, so guarding only create would leave the hole open.
    let dead = armed_unhonoured(&body);
    if !dead.is_empty() {
        return unhonoured_refusal(&dead);
    }
    let pass_in = match pass_check_input(&body) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let when_in = match when_input(&body) {
        Ok(w) => w,
        Err(r) => return *r,
    };
    let by = mutation_by(&headers, body.by.as_deref());
    let now = Local::now();
    let bounds_in = match parse_bounds(&body, now) {
        Ok(b) => b,
        Err(e) => return bounds_refusal(e),
    };
    // The owner is a request with no worker header (the dashboard, or the
    // owner's own shell); `is_owner_request` is the predicate the board's
    // owner gates already use.
    let owner = crate::api::standing_approvals::is_owner_request(&headers);
    let caller_worker = crate::api::session_verbs::hdr_worker(&headers);

    enum Outcome {
        NotFound,
        Tombstone {
            deleted_at: i64,
            deleted_by: Option<String>,
        },
        Locked(DurableSchedule),
        WhenOnShell,
        Applied(Value, Option<(DurableSchedule, String)>),
    }
    let slot: Arc<Mutex<Option<Outcome>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let id_w = id.clone();
    let by_outer = by.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let Some(mut s) = get_schedule(conn, &id_w)? else {
                return finish(&slot_w, Outcome::NotFound, no_write());
            };
            // A tombstone accepts NO ordinary write (AMUX-3431): the row will
            // never fire, so a 200 here is fictional coverage. The one honest
            // write is an EXPLICIT resurrect, audited as such.
            let was_deleted = s.is_deleted();
            if was_deleted && body.resurrect != Some(true) {
                return finish(
                    &slot_w,
                    Outcome::Tombstone {
                        deleted_at: s.i64_field("deleted", 0),
                        deleted_by: deleter_of(conn, s.id()),
                    },
                    no_write(),
                );
            }
            if was_deleted {
                let old = s
                    .raw()
                    .get("deleted")
                    .and_then(Value::as_i64)
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                // update_schedule's column list excludes `deleted` on purpose
                // (an ordinary write must never flip a tombstone), so the
                // clear is its own explicit statement, like the delete.
                scheduler::resurrect_schedule(conn, s.id())?;
                s.set("deleted", Value::Null);
                insert_audit(conn, s.id(), "deleted", &old, "", "api-resurrect", &by)?;
            }
            // THE OWNER LOCK (GS-12 finding 5: the owner's disable of SCHED-173
            // was undone by a worker 70 minutes later, and the worker had no
            // way to know the disable was the owner's).
            let was_enabled = s.enabled();
            let locked = !s.str_field("locked_by").trim().is_empty();
            if body.enabled == Some(1) && !was_enabled && locked && !owner {
                return finish(&slot_w, Outcome::Locked(s), no_write());
            }
            let old_lock = s.str_field("locked_by").to_string();
            let old_vals: Map<String, Value> = AUDIT_FIELDS
                .iter()
                .map(|k| {
                    (
                        k.to_string(),
                        s.raw().get(*k).cloned().unwrap_or(Value::Null),
                    )
                })
                .collect();

            // Apply provided fields (Python's writable-field list).
            if let Some(v) = &body.title {
                s.set("title", json!(v));
            }
            if let Some(v) = &body.session {
                s.set("session", json!(v));
            }
            if let Some(v) = &body.command {
                s.set("command", json!(v));
            }
            if let Some(v) = &body.kind {
                s.set("kind", json!(v));
            }
            if let Some(v) = &body.sched_type {
                s.set("sched_type", json!(v));
            }
            if let Some(v) = &body.recurrence {
                s.set("recurrence", json!(v));
            }
            if let Some(v) = &body.run_at {
                s.set("run_at", json!(v));
            }
            if let Some(v) = body.enabled {
                s.set("enabled", json!(v));
            }
            if let Some(v) = &body.schedule_expr {
                s.set("schedule_expr", json!(v));
            }
            if let Some(v) = body.watch {
                s.set("watch", json!(v));
            }
            if let Some(v) = body.watch_timeout {
                s.set("watch_timeout", json!(v));
            }
            if let Some(v) = &body.done_pattern {
                s.set("done_pattern", json!(v));
            }
            if let Some(v) = &body.done_action {
                s.set("done_action", json!(v));
            }
            if let Some(v) = &body.trigger_on {
                s.set("trigger_on", json!(v));
            }
            if let Some(v) = body.trigger_cooldown {
                s.set("trigger_cooldown", json!(v));
            }
            if let Some(v) = &body.trigger_sessions {
                s.set("trigger_sessions", json!(v));
            }
            if let Some(v) = body.worktree {
                s.set("worktree", json!(v));
            }
            if let Some(v) = body.fan_out {
                s.set("fan_out", json!(v));
            }
            if let Some(v) = &body.fan_out_model {
                s.set("fan_out_model", json!(v));
            }
            if let Some(v) = &body.exit_actions {
                let stored = match v {
                    Value::Object(o) => Value::String(Value::Object(o.clone()).to_string()),
                    other => other.clone(),
                };
                s.set("exit_actions", stored);
            }
            let mut pass_changed = false;
            if let Some(v) = &pass_in {
                pass_changed = s.str_field("pass_check").trim() != v.as_deref().unwrap_or("");
                s.set("pass_check", v.clone().map(Value::from).unwrap_or(Value::Null));
            }
            if let Some(v) = &when_in {
                s.set("when_check", v.clone().map(Value::from).unwrap_or(Value::Null));
            }
            if let Some(v) = body.when_once {
                s.set("when_once", json!(v));
            }
            if s.str_field("kind") == "shell" && !s.str_field("when_check").trim().is_empty() {
                return finish(&slot_w, Outcome::WhenOnShell, no_write());
            }
            apply_bounds(&mut s, &bounds_in);

            // Disable / enable bookkeeping: why it is off, and the owner lock.
            let now_enabled = s.enabled();
            let reason = body
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .unwrap_or("no reason given")
                .to_string();
            let mut disabled_notice: Option<String> = None;
            if body.enabled == Some(0) && owner {
                s.set("locked_by", json!(by));
                s.set("lock_reason", json!(reason));
                s.set("locked_at", json!(chrono::Utc::now().timestamp()));
                tracing::info!(schedule = %s.id(), by = %by, reason = %reason,
                    verdict = "schedule_owner_lock_set", measured = true, n_considered = 1,
                    "owner disabled a schedule; locked against worker re-enable");
            }
            if was_enabled && !now_enabled {
                s.set("disabled_reason", json!(if owner {
                    format!("disabled by the owner ({by}): {reason}")
                } else {
                    format!("disabled by {by}: {reason}")
                }));
                let session = s.str_field("session").trim().to_string();
                if !session.is_empty() && caller_worker != session {
                    disabled_notice = Some(reason.clone());
                }
            } else if !was_enabled && now_enabled {
                s.set("disabled_reason", Value::Null);
                if owner {
                    s.set("locked_by", Value::Null);
                    s.set("lock_reason", Value::Null);
                    s.set("locked_at", Value::Null);
                }
            }

            // Recompute next_run from the (possibly new) expr, Python-style.
            match s.schedule_expr().map(str::to_string) {
                Some(e) => {
                    s.set("sched_type", json!("recurring"));
                    let nr = ScheduleExpr::parse(&e)
                        .ok()
                        .and_then(|p| bounded_next(&s, &p, now))
                        .unwrap_or_else(|| s.str_field("run_at").to_string());
                    s.set("next_run", json!(nr));
                }
                None => {
                    // A recurring row whose expression was cleared, with no
                    // legacy recurrence, recurs on nothing. legacy_next_run read
                    // it as DAILY at run_at's clock time, so a PATCH of
                    // {"schedule_expr":"","run_at":"2026-10-31T20:05"} answered
                    // 200 and armed the row for 20:05 TODAY (seven GS-12
                    // one-offs, 2026-10-19, reverted before they fired). With a
                    // full date in run_at the caller meant once at that minute,
                    // which is what `once:` writes; say so in the log.
                    let run_at = s.str_field("run_at").to_string();
                    if s.str_field("sched_type") == "recurring"
                        && s.str_field("recurrence").trim().is_empty()
                        && crate::runtime_jobs::scheduler::parse_minute(&run_at).is_some()
                    {
                        s.set("sched_type", json!("once"));
                        s.set("schedule_expr", Value::Null);
                        tracing::info!(schedule = %s.id(), run_at = %run_at,
                            verdict = "schedule_expr_cleared_to_once", measured = true, n_considered = 1,
                            "schedule_expr cleared with a dated run_at: the row is now a one-shot at run_at");
                    }
                    let nr = legacy_next_run(
                        s.str_field("sched_type"),
                        Some(s.str_field("recurrence")).filter(|x| !x.is_empty()),
                        Some(s.str_field("run_at")).filter(|x| !x.is_empty()),
                        now,
                    )
                    .unwrap_or_else(|| s.str_field("run_at").to_string());
                    s.set("next_run", json!(nr));
                }
            }
            s.set("updated", json!(chrono::Utc::now().timestamp()));
            update_schedule(conn, &s)?;
            if pass_changed {
                // A new condition starts over: probe it now, no attempts used.
                let armed = !s.str_field("pass_check").trim().is_empty();
                let ts = chrono::Utc::now().timestamp();
                conn.execute(
                    "UPDATE schedules SET pass_state=?1, pass_next_at=?2, pass_attempts=0, pass_detail=NULL,
                            pass_checked_at=NULL WHERE id=?3",
                    rusqlite::params![armed.then_some("probe"), armed.then_some(ts), s.id()],
                )?;
                for (k, v) in [("pass_state", json!(armed.then_some("probe"))), ("pass_attempts", json!(0))] {
                    s.set(k, v);
                }
            }
            // A new max_runs budget, or re-enabling a row that spent its
            // budget, starts counting from zero. Otherwise re-enable would end
            // it again at its next fire with nothing having run.
            let spent = Bounds::of(&s).max_runs.is_some_and(|m| s.i64_field("cron_runs", 0) >= m);
            if bounds_in.max_runs.is_some() || (!was_enabled && now_enabled && spent) {
                conn.execute("UPDATE schedules SET cron_runs=0 WHERE id=?1", [s.id()])?;
                s.set("cron_runs", json!(0));
            }

            // Audit every changed tracked field (esp. enabled) — AMUX-1735.
            for k in AUDIT_FIELDS {
                let old = audit_str(old_vals.get(k));
                let new = audit_str(s.raw().get(k));
                if old != new {
                    insert_audit(conn, s.id(), k, &old, &new, "api-patch", &by)?;
                }
            }
            if old_lock != s.str_field("locked_by") {
                let new_lock = if s.str_field("locked_by").is_empty() {
                    String::new()
                } else {
                    format!("{} ({})", s.str_field("locked_by"), s.str_field("lock_reason"))
                };
                insert_audit(conn, s.id(), "locked_by", &old_lock, &new_lock, "api-patch", &by)?;
            }
            let events = vec![ev(s.id(), MutationKind::Updated)];
            let mut out = s.to_json();
            if was_deleted {
                if let Some(o) = out.as_object_mut() {
                    o.insert("resurrected".into(), json!(true));
                }
            }
            let notice = disabled_notice.map(|r| (s.clone(), r));
            finish(
                &slot_w,
                Outcome::Applied(out, notice),
                WriteOutcome {
                    applied: true,
                    events,
                },
            )
        })
        .await;
    match write {
        Ok(_) => match take_slot(&slot) {
            // AMUX-3546: the card says "created OR EDITED". An edit that
            // changes the cadence is the same decision as choosing it, made a
            // second time, and it is the one a schedule actually gets — nobody
            // deletes and recreates to go from hourly to every 10m.
            Some(Outcome::Applied(mut v, notice)) => {
                // Tell the owning worker its schedule was disabled by someone
                // else, so it does not re-enable a decision it cannot see.
                if let Some((sched, reason)) = notice {
                    let told = lifecycle::notify_disabled(&state.store, &sched, &by_outer, owner, &reason).await;
                    if let Value::Object(o) = &mut v {
                        o.insert("disable_notice".into(), told);
                    }
                }
                if let Value::Object(o) = &mut v {
                    let expr = o
                        .get("schedule_expr")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let kind = o
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("tmux")
                        .to_string();
                    if let Some(n) = cadence_note(&expr, &kind) {
                        o.insert("cadence_note".into(), n);
                    }
                    if let Some(n) = when_note(o) {
                        o.insert("when_note".into(), n);
                    }
                    attach_isolated_note(o, "patch");
                }
                expose_instants(&mut v);
                Json(v).into_response()
            }
            Some(Outcome::NotFound) => not_found(),
            Some(Outcome::Locked(s)) => owner_lock_refusal(&id, "re-enable", &by_outer, &s),
            Some(Outcome::WhenOnShell) => when_on_shell_refusal(),
            Some(Outcome::Tombstone {
                deleted_at,
                deleted_by,
            }) => tombstone_refusal(&id, "PATCH", deleted_at, deleted_by.as_deref()),
            None => internal("patch produced no outcome"),
        },
        Err(e) => internal(e),
    }
}

// ---- DELETE /api/schedules/{id} -------------------------------------------

#[derive(Deserialize)]
pub struct DeleteParams {
    /// Claimed attribution (DELETE bodies are unreliable — Python reads
    /// `?by=`).
    #[serde(default)]
    pub by: Option<String>,
}

pub async fn delete_schedule(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(p): Query<DeleteParams>,
    headers: HeaderMap,
) -> Response {
    let by = mutation_by(&headers, p.by.as_deref());
    enum Outcome {
        NotFound,
        Already,
        Deleted,
    }
    let slot: Arc<Mutex<Option<Outcome>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let id_w = id.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let Some(s) = get_schedule(conn, &id_w)? else {
                return finish(&slot_w, Outcome::NotFound, no_write());
            };
            if s.is_deleted() {
                // Idempotent re-delete: no duplicate audit row (Python parity).
                return finish(&slot_w, Outcome::Already, no_write());
            }
            let now_ts = chrono::Utc::now().timestamp();
            soft_delete_schedule(conn, s.id(), now_ts)?;
            // Deletes leave the same by_who trail as PATCHes (AMUX-1812: 8
            // schedules vanished with zero attribution).
            let old = json!({
                "title": s.str_field("title"),
                "session": s.str_field("session"),
                "enabled": s.i64_field("enabled", 0),
                "expr": s.schedule_expr().unwrap_or(s.str_field("run_at")),
            });
            insert_audit(
                conn,
                s.id(),
                "deleted",
                &old.to_string(),
                &now_ts.to_string(),
                "api-delete",
                &by,
            )?;
            let events = vec![ev(s.id(), MutationKind::Deleted)];
            finish(
                &slot_w,
                Outcome::Deleted,
                WriteOutcome {
                    applied: true,
                    events,
                },
            )
        })
        .await;
    match write {
        Ok(_) => match slot.lock().expect("slot").take() {
            Some(Outcome::Deleted) => Json(json!({ "deleted": id })).into_response(),
            Some(Outcome::Already) => {
                Json(json!({ "deleted": id, "already": true })).into_response()
            }
            Some(Outcome::NotFound) => not_found(),
            None => internal("delete produced no outcome"),
        },
        Err(e) => internal(e),
    }
}

// ---- POST /api/schedules/{id}/skip ----------------------------------------

/// Skip the schedule's PENDING occurrence and resume the cadence (AMUX-2679).
///
/// The dashboard has called this since the Python days; the Rust router never
/// had the route, so the Skip button 404'd — and `skipSchedule()` renders
/// `d.error || 'Could not skip'`, so it reported a wrong reason rather than
/// nothing, which is why it read as a broken schedule instead of a missing
/// endpoint.
///
/// Semantics are python's, confirmed against `_skip_next_run` and not against
/// the button's label: `next_run` advances to the occurrence AFTER the one
/// currently armed. Nothing else moves — no run row (nothing ran), no
/// `last_run`, no `run_count`, and `enabled` is untouched, so the schedule
/// resumes on its own at the following occurrence.
///
/// The response says WHICH occurrence was dropped as well as where the
/// schedule landed. Python returned only `{"ok":true,"next_run":...}`, so the
/// toast could say where you ended up but never what you gave up — and for a
/// cadence like `every 15m` those are the same string to the eye.
pub async fn skip_next(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let by = mutation_by(&headers, None);

    enum Outcome {
        NotFound,
        Tombstone {
            deleted_at: i64,
            deleted_by: Option<String>,
        },
        Once,
        NoNext(String),
        Skipped(Value),
    }
    let slot: Arc<Mutex<Option<Outcome>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let id_w = id.clone();
    let write = state
        .store
        .write_async(move |conn| {
            // Read INSIDE the write transaction: the fire loop advances
            // `next_run` on the same row, and a skip computed from a row read
            // outside the lock could drop an occurrence that had already
            // fired — silently double-skipping.
            let Some(mut s) = get_schedule(conn, &id_w)? else {
                return finish(&slot_w, Outcome::NotFound, no_write());
            };
            if s.is_deleted() {
                // Skipping a tombstone is a write to a row that never fires —
                // name the deletion, same as PATCH (AMUX-3431).
                return finish(
                    &slot_w,
                    Outcome::Tombstone {
                        deleted_at: s.i64_field("deleted", 0),
                        deleted_by: deleter_of(conn, s.id()),
                    },
                    no_write(),
                );
            }
            // Python's guard: a one-shot has no "next occurrence" to resume
            // to, so skipping it would mean deleting it under another name.
            if s.str_field("sched_type") == "once" {
                return finish(&slot_w, Outcome::Once, no_write());
            }
            let skipped = s.str_field("next_run").to_string();
            let Some(new_next) = skip_next_run(&s) else {
                return finish(&slot_w, Outcome::NoNext(skipped), no_write());
            };
            let now_ts = chrono::Utc::now().timestamp();
            s.set("next_run", json!(new_next));
            s.set("updated", json!(now_ts));
            update_schedule(conn, &s)?;
            // Python skipped unattributed. Moving a fire time is exactly the
            // mutation AMUX-1812 was about ("why did this not run at 9?"), so
            // it leaves the same by_who trail as a PATCH.
            insert_audit(
                conn,
                s.id(),
                "next_run",
                &skipped,
                &new_next,
                "api-skip",
                &by,
            )?;
            let ms = |v: &str| scheduler::parse_minute(v).map(|t| t.timestamp_millis());
            let body = json!({
                "ok": true,
                "id": s.id(),
                "title": s.str_field("title"),
                // WHAT WAS GIVEN UP, and where it resumes.
                "skipped": skipped,
                "skipped_ms": ms(skipped.as_str()),
                "next_run": new_next,
                "next_run_ms": ms(new_next.as_str()),
                "schedule_expr": s.schedule_expr(),
                "enabled": s.i64_field("enabled", 0),
            });
            let events = vec![ev(s.id(), MutationKind::Updated)];
            finish(
                &slot_w,
                Outcome::Skipped(body),
                WriteOutcome {
                    applied: true,
                    events,
                },
            )
        })
        .await;
    match write {
        Ok(_) => match slot.lock().expect("slot").take() {
            Some(Outcome::Skipped(v)) => Json(v).into_response(),
            Some(Outcome::NotFound) => not_found(),
            Some(Outcome::Tombstone {
                deleted_at,
                deleted_by,
            }) => tombstone_refusal(&id, "skip", deleted_at, deleted_by.as_deref()),
            Some(Outcome::Once) => err(
                StatusCode::BAD_REQUEST,
                json!({
                    "error": "only recurring schedules can be skipped",
                    "detail": "a one-shot has no following occurrence to resume to — \
                               disable or delete it instead",
                }),
            ),
            // Say WHICH string could not be advanced and what the row holds.
            // "could not compute the next occurrence" alone (python's whole
            // body) cannot distinguish a blank next_run from an expression the
            // parser does not know, and those need different fixes.
            Some(Outcome::NoNext(cur)) => err(
                StatusCode::BAD_REQUEST,
                json!({
                    "error": "could not compute the next occurrence",
                    "next_run": cur,
                    "detail": if cur.trim().is_empty() {
                        "this schedule has no armed next_run to skip — enable it, or PATCH a \
                         schedule_expr so one is computed"
                    } else {
                        "next_run is set but neither schedule_expr nor recurrence yields a \
                         following occurrence — PATCH a schedule_expr (GET \
                         /api/schedules/<id> shows what the row holds)"
                    },
                }),
            ),
            None => internal("skip produced no outcome"),
        },
        Err(e) => internal(e),
    }
}

// ---- POST /api/schedules/{id}/run -----------------------------------------

/// The acknowledgement of a manual shell run. The RUN is asynchronous
/// (`status: running`, `pending`), but the COMMAND a Run now press issued,
/// "start this run", is complete once this answers, so `phase` says so. Without
/// it the dashboard's press tracker read the bare 202 as a command still in
/// flight, waited 30s for a completion nobody reports, and then toasted "Run
/// now: still waiting on the server; you can try again" over a run that had
/// already finished (scheduler e2e, 2026-10-11). The run's result has its own
/// home: the Runs tab and the run history.
fn shell_run_ack(started: bool) -> Value {
    json!({
        "ok": true,
        "pending": true,
        "deduped": !started,
        "status": "running",
        "phase": if started { "applied" } else { "noop" },
        "message": if started {
            "Run started on the host; its result appears in Runs"
        } else {
            "Already running; attached to the existing run"
        },
        "note": if started {
            "started on host; result pending"
        } else {
            "already running; attached to the existing run"
        },
        "delivery": "shell",
        "submission": Value::Null,
        "kind": "shell",
    })
}

pub async fn run_now(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    // SHADOW MODE (AMUX_RS_SCHEDULER=0): a run-now that recorded a run and
    // delivered nothing would be a lie in the run history. Refuse honestly,
    // with the way out in the body.
    if !scheduler::firing_enabled() {
        return err(
            StatusCode::CONFLICT,
            json!({
                "error": "rust scheduler is in shadow mode",
                "enable": "AMUX_RS_SCHEDULER=1",
            }),
        );
    }
    // No body is parsed on purpose (Python parity): a claimed `by` from a
    // body this endpoint never reads would be fabricated attribution.
    let by = mutation_by(&headers, None);
    let source = format!("manual:{by}");

    // READ the row first, then DELIVER outside any write lock, then RECORD what
    // delivery did. The old shape did the opposite — one write transaction that
    // recorded `status:"ok"` and returned `{"ok":true,"ran":"<title>"}` while
    // its own comment admitted nothing had been delivered. Ethan pressed Run
    // now, got a green "Ran", and no command reached the session (AMUX-2647).
    let store = state.store.clone();
    let id_r = id.clone();
    let sched = match crate::db::interactions::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = store.read()?;
        let s = get_schedule(&conn, &id_r)?;
        // Fetch the deleter alongside, so a tombstone refusal can name it
        // without a second round trip (AMUX-3431).
        let del_by = match &s {
            Some(x) if x.is_deleted() => deleter_of(&conn, x.id()),
            _ => None,
        };
        Ok((s, del_by))
    })
    .await
    {
        Ok(Ok((Some(s), _)))
            if !s.is_deleted()
                && !s.str_field("locked_by").trim().is_empty()
                && !crate::api::standing_approvals::is_owner_request(&headers) =>
        {
            return owner_lock_refusal(&id, "run", &by, &s);
        }
        Ok(Ok((Some(s), _))) if !s.is_deleted() => s,
        Ok(Ok((Some(s), del_by))) => {
            // Running a deleted schedule by hand is the same fiction as
            // PATCHing one: it LOOKS live while the scheduler will never
            // fire it. Name the deletion instead of a bare 404.
            return tombstone_refusal(&id, "run-now", s.i64_field("deleted", 0), del_by.as_deref());
        }
        Ok(Ok((None, _))) => return not_found(),
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };

    // A shell command may legitimately take minutes. Waiting for it in the
    // HTTP handler left Run Now disabled with the old last-run timestamp and
    // no run row, so repeated taps from other tabs launched the same side
    // effect again. Claim a durable provisional row atomically, return it at
    // once, and complete that same row in the background.
    if sched.str_field("kind") == "shell" {
        let claim_slot = Arc::new(Mutex::new(None));
        let claim_slot_w = claim_slot.clone();
        let sid = sched.id().to_string();
        let sid_w = sid.clone();
        let source_w = source.clone();
        let by_w = by.clone();
        let claim_write = state
            .store
            .write_async(move |conn| {
                let claim = scheduler::claim_manual_shell_run(conn, &sid_w, &source_w)?;
                let events = if claim.started() {
                    vec![
                        ev(&sid_w, MutationKind::Updated),
                        PendingEvent {
                            entity_type: EntityType::Other("schedule_manual_run".into()),
                            entity_id: sid_w.clone(),
                            mutation: MutationKind::StatusChanged {
                                from: by_w,
                                to: "running".into(),
                            },
                            payload: None,
                        },
                    ]
                } else {
                    Vec::new()
                };
                *claim_slot_w.lock().expect("shell claim slot poisoned") = Some(claim);
                Ok(WriteOutcome {
                    applied: claim.started(),
                    events,
                })
            })
            .await;
        if let Err(e) = claim_write {
            return internal(e);
        }
        let Some(claim) = claim_slot.lock().expect("shell claim slot poisoned").take() else {
            return internal("shell run claim produced no outcome");
        };

        if claim.started() {
            let state_bg = state.clone();
            let sched_bg = sched.clone();
            let source_bg = source.clone();
            let run_id = claim.run_id();
            crate::db::interactions::spawn(async move {
                let outcome = LiveDeliverer::new(state_bg.clone())
                    .deliver(&sched_bg, &source_bg)
                    .await;
                let status = outcome.status().to_string();
                let sid_done = sched_bg.id().to_string();
                let for_notice = outcome.clone();
                let update = state_bg
                    .store
                    .write_async(move |conn| {
                        scheduler::finish_manual_shell_run(conn, run_id, &outcome)?;
                        Ok(WriteOutcome {
                            applied: true,
                            events: vec![
                                ev(&sid_done, MutationKind::Updated),
                                PendingEvent {
                                    entity_type: EntityType::Other("schedule_manual_run".into()),
                                    entity_id: sid_done,
                                    mutation: MutationKind::StatusChanged {
                                        from: "running".into(),
                                        to: status,
                                    },
                                    payload: None,
                                },
                            ],
                        })
                    })
                    .await;
                if let Err(e) = update {
                    tracing::error!(schedule = %sched_bg.id(), run_id, error = %e,
                        "could not record completed manual shell run");
                } else {
                    crate::runtime_jobs::schedule_failure::on_run_outcome(
                        &state_bg.store, &sched_bg, &for_notice, &source_bg).await;
                }
            });
        }

        let mut body = shell_run_ack(claim.started());
        body["ran"] = json!(sched.str_field("title"));
        body["run_id"] = json!(claim.run_id());
        body["ran_at"] = json!(claim.ran_at());
        body["session"] = json!(sched.str_field("session"));
        body["source"] = json!(source);
        return (StatusCode::ACCEPTED, Json(body)).into_response();
    }

    let deliverer = LiveDeliverer::new(state.clone());
    let outcome = deliverer.deliver(&sched, &source).await;

    let title = sched.str_field("title").to_string();
    let sid = sched.id().to_string();
    let (o_rec, o_resp) = (outcome.clone(), outcome);
    let src = source.clone();
    let write = state
        .store
        .write_async(move |conn| {
            record_run(conn, &sid, &o_rec, &src)?;
            let events = vec![
                ev(&sid, MutationKind::Updated),
                PendingEvent {
                    entity_type: EntityType::Other("schedule_manual_run".into()),
                    entity_id: sid.clone(),
                    mutation: MutationKind::StatusChanged {
                        from: by.clone(),
                        // The event carries the VERDICT, not the bare fact that
                        // a button was pressed — a listener that saw "fired" for
                        // a refused delivery is the same lie one layer out.
                        to: o_rec.status().to_string(),
                    },
                    payload: None,
                },
            ];
            Ok(WriteOutcome {
                applied: true,
                events,
            })
        })
        .await;
    if let Err(e) = write {
        return internal(e);
    }
    crate::runtime_jobs::schedule_failure::on_run_outcome(&state.store, &sched, &o_resp, &source).await;

    // SAY WHAT HAPPENED. The dashboard toasts `status`/`note` verbatim, so an
    // empty note here is what rendered as "Ran · no output" over a dead action.
    let mut body = json!({
        // `ok` is the field a caller must read, and it is FALSE for a refusal
        // and for a failed delivery. The HTTP code stays 200 on purpose: the
        // request was processed and a run row exists — this is a verdict, not a
        // rejected request — and a 4xx here routes the dashboard into a generic
        // "the server did not accept it" toast that throws away the one thing
        // worth showing, which is WHY nothing was delivered.
        "ok": o_resp.landed() || matches!(o_resp, RunOutcome::Queued { .. }),
        "ran": title,
        "status": o_resp.status(),
        "note": o_resp.note().unwrap_or_default(),
        "delivery": o_resp.delivery(),
        "submission": o_resp.submission(),
        "session": sched.str_field("session"),
        "kind": sched.str_field("kind"),
        "source": source,
    });
    if let RunOutcome::Queued { queue_id, .. } = &o_resp {
        // The handle on a message that has not landed yet — without it "queued"
        // is a word with nothing behind it.
        body["queue_id"] = json!(queue_id);
    }
    Json(body).into_response()
}

// ---- GET /api/schedules/runs ----------------------------------------------

/// Recent runs across all schedules, newest first, `source` visible on
/// every row (ethos rule 4 — the consumer-facing surface of the manual/cron
/// discriminator, not just a column in a store nobody opens).
pub async fn recent_runs(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    // MF-847. The global LIMIT 50 was the only way in, and with ~192 runs a day
    // fleet-wide an every-15-min sweep rotates the visible window inside an
    // hour. So a DAILY schedule — the class whose misses matter most — was
    // never readable by the time anyone noticed a miss, and mixpeek-finances
    // reasonably concluded the RECORD was gone.
    //
    // It was not. `schedule_runs` retains everything: measured 2026-09-06,
    // 26,084 rows back to 2026-06-08, ~9k/month, and the table has carried
    // `idx_sched_runs_sched ON (schedule_id, ran_at DESC)` since the baseline
    // migration. Three months of forensics existed and no query could reach
    // them. The cap was a DISPLAY limit that read as a retention limit.
    //
    // `schedule_id` uses that index; `limit` raises the ceiling. Both are
    // rejected rather than ignored when malformed, matching audit_trail below
    // (AC-228: a filter that silently matches everything answers confidently
    // and wrongly).
    let sched_filter = q
        .get("schedule_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let limit: i64 = match q.get("limit") {
        None => 50,
        Some(v) => match v.trim().parse::<i64>() {
            Ok(n) if (1..=2000).contains(&n) => n,
            _ => {
                return err(
                    StatusCode::BAD_REQUEST,
                    json!({"error": format!("limit must be an integer 1..=2000, got {v:?}")}),
                )
            }
        },
    };
    let unknown: Vec<&String> = q
        .keys()
        .filter(|k| !matches!(k.as_str(), "schedule_id" | "limit"))
        .collect();
    if !unknown.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            json!({"error": format!("unknown query param(s): {unknown:?}"),
                   "accepted": ["schedule_id", "limit"]}),
        );
    }
    let store = state.store.clone();
    let joined = crate::db::interactions::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        let sql = format!(
            "SELECT sr.id, sr.schedule_id, sr.ran_at, sr.status, sr.note, sr.source,
                    sr.delivery, sr.submission, s.title, sr.exit_code, sr.output_tail
             FROM schedule_runs sr LEFT JOIN schedules s ON s.id = sr.schedule_id
             {} ORDER BY sr.ran_at DESC LIMIT ?1",
            if sched_filter.is_some() {
                "WHERE sr.schedule_id = ?2"
            } else {
                ""
            }
        );
        let mut stmt = conn.prepare(&sql)?;
        let bound: Vec<&dyn rusqlite::ToSql> = match &sched_filter {
            Some(id) => vec![&limit, id],
            None => vec![&limit],
        };
        let rows = stmt.query_map(bound.as_slice(), |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "schedule_id": r.get::<_, String>(1)?,
                "ran_at": r.get::<_, i64>(2)?,
                "status": r.get::<_, String>(3)?,
                "note": r.get::<_, Option<String>>(4)?,
                "source": r.get::<_, String>(5)?,
                // NULL on every row written before 0015 — "not recorded", NOT
                // "delivered". The UI must render the difference, never default
                // it (AMUX-2647).
                "delivery": r.get::<_, Option<String>>(6)?,
                "submission": r.get::<_, Option<String>>(7)?,
                "title": r.get::<_, Option<String>>(8)?,
                // AMUX-5241: shell runs only. NULL for tmux deliveries and for
                // every row before migration 0088, which means "not recorded",
                // never "exit 0".
                "exit_code": r.get::<_, Option<i64>>(9)?,
                "output_tail": r.get::<_, Option<String>>(10)?,
            }))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    })
    .await;
    match joined {
        Ok(Ok(rows)) => Json(Value::Array(rows)).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- GET /api/schedules/audit ---------------------------------------------

/// The attribution trail, reachable from the API (AMUX-2416: an audit
/// nobody can find is the same failure as no audit). Unknown query params
/// are REJECTED, not ignored (AC-228: a filter that silently matches
/// everything returns a confident wrong answer).
pub async fn audit_trail(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    const KNOWN: [&str; 4] = ["id", "field", "limit", "full"];
    let mut bad: Vec<&str> = q
        .keys()
        .map(String::as_str)
        .filter(|k| !KNOWN.contains(k))
        .collect();
    if !bad.is_empty() {
        bad.sort_unstable();
        return err(
            StatusCode::BAD_REQUEST,
            json!({
                "error": format!("unknown query param(s): {}", bad.join(", ")),
                "accepted": KNOWN,
            }),
        );
    }
    let sid = q.get("id").cloned().unwrap_or_default();
    let field = q.get("field").cloned().unwrap_or_default();
    let full = matches!(
        q.get("full").map(String::as_str),
        Some("1") | Some("true") | Some("yes")
    );
    let limit: i64 = q
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(100)
        .min(500);

    let store = state.store.clone();
    let sid2 = sid.clone();
    let joined = crate::db::interactions::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        let mut wheres: Vec<&str> = Vec::new();
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        if !sid2.is_empty() {
            wheres.push("a.schedule_id = ?");
            params.push(rusqlite::types::Value::Text(sid2.clone()));
        }
        if !field.is_empty() {
            wheres.push("a.field = ?");
            params.push(rusqlite::types::Value::Text(field.clone()));
        }
        params.push(rusqlite::types::Value::Integer(limit));
        let sql = format!(
            "SELECT a.id, a.schedule_id, a.ts, a.field, a.old_value, a.new_value, a.source, a.by_who, s.title
             FROM schedule_audit a LEFT JOIN schedules s ON s.id = a.schedule_id
             {} ORDER BY a.ts DESC LIMIT ?",
            if wheres.is_empty() { String::new() } else { format!("WHERE {}", wheres.join(" AND ")) }
        );
        let mut stmt = conn.prepare(&sql)?;
        let trim = |v: Option<String>| -> (Value, bool) {
            match v {
                Some(s) if !full && sid2.is_empty() && s.chars().count() > 200 => {
                    (json!(s.chars().take(200).collect::<String>() + "…"), true)
                }
                Some(s) => (json!(s), false),
                None => (Value::Null, false),
            }
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| {
            let (old_v, t1) = trim(r.get::<_, Option<String>>(4)?);
            let (new_v, t2) = trim(r.get::<_, Option<String>>(5)?);
            let mut v = json!({
                "id": r.get::<_, i64>(0)?,
                "schedule_id": r.get::<_, String>(1)?,
                "ts": r.get::<_, i64>(2)?,
                "field": r.get::<_, String>(3)?,
                "old_value": old_v,
                "new_value": new_v,
                "source": r.get::<_, Option<String>>(6)?,
                "by_who": r.get::<_, Option<String>>(7)?,
                "title": r.get::<_, Option<String>>(8)?,
            });
            if t1 || t2 {
                v["truncated"] = json!(true);
            }
            Ok(v)
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    })
    .await;
    match joined {
        Ok(Ok(rows)) => (
            [(
                "x-amux-audit-full",
                "add ?full=1 (or ?id=SCHED-N) for untruncated values",
            )],
            Json(Value::Array(rows)),
        )
            .into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// Tests — temp-DB stores only; the router is nested directly (the mount in
// api/mod.rs belongs to the integrator).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    #[test]
    fn script_snapshot_is_explicit_durable_and_refuses_corrupted_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home ' quoted");
        let source = temp.path().join("scratch.py");
        std::fs::write(&source, "print('first revision')\n").unwrap();
        let body = || super::ScheduleBody { kind: Some("shell".into()), command: Some("python3 {script}".into()),
            script_path: Some(source.to_string_lossy().into_owned()), ..Default::default() };
        let mut request = body();
        super::prepare_script(&mut request, &home).unwrap();
        std::fs::remove_file(&source).unwrap();
        let output = std::process::Command::new("/bin/bash").args(["-c", request.command.as_deref().unwrap()]).output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "first revision\n");
        let artifact = std::fs::read_dir(home.join("schedule-artifacts")).unwrap().next().unwrap().unwrap().path();
        std::fs::write(&artifact, "corrupted evidence").unwrap();
        std::fs::write(&source, "print('first revision')\n").unwrap();
        assert!(super::prepare_script(&mut body(), &home).unwrap_err().contains("digest mismatch"));
        assert_eq!(std::fs::read_to_string(&artifact).unwrap(), "corrupted evidence");
        for command in ["python3 '{script}'", "python3 {script} {script}", "python3 no-placeholder"] {
            let mut invalid = body(); invalid.command = Some(command.into());
            assert!(super::prepare_script(&mut invalid, &home).is_err());
        }
        let mut prompt = body(); prompt.kind = Some("tmux".into());
        assert!(super::prepare_script(&mut prompt, &home).is_err());
    }
    /// AMUX-3546: the note states the rate and the spend, and NEVER refuses.
    ///
    /// `every 15m` is the card's own specimen: eleven characters, 96 turns a
    /// day, five of them enabled on the machine that motivated this.
    #[test]
    fn an_isolated_target_is_warned_and_an_ordinary_one_is_not() {
        // AMUX-5241. The pre-fix response carried no such field at all, so the
        // absent case is the old shape and must stay absent for normal lanes.
        assert!(super::isolated_target_note("desktop", "tmux", false).is_none());
        assert!(super::isolated_target_note("", "tmux", true).is_none());
        let n = super::isolated_target_note("random", "tmux", true).expect("warned");
        assert_eq!(n["code"], "isolated_target");
        let m = n["message"].as_str().unwrap();
        assert!(m.contains("'random'") && m.contains("403") && m.contains("IS delivered"), "{m}");
        let sh = super::isolated_target_note("random", "shell", true).expect("warned");
        assert!(sh["message"].as_str().unwrap().contains("runs on the host"));
    }

    #[test]
    fn a_cadence_note_states_the_rate_and_never_refuses() {
        let n = super::cadence_note("every 15m", "tmux").expect("countable");
        let t = n.as_str().unwrap();
        assert!(t.contains("96"), "the RATE is the fact nobody had: {t}");
        assert!(
            t.contains("576"),
            "96 fires x ~$6 = $576/day, and that is the point: {t}"
        );
        assert!(
            t.contains("Nothing is refused"),
            "the card is explicit that a 10-minute poll is a legitimate thing to want: {t}"
        );

        // kind='shell' wakes no model, so a spend line would be false. This is
        // AF-216's distinction, and stating a dollar figure here would undo the
        // very choice its kind_note exists to encourage.
        let sh = super::cadence_note("every 15m", "shell").expect("countable");
        let st = sh.as_str().unwrap();
        assert!(st.contains("96"), "the rate is still worth stating: {st}");
        assert!(
            !st.contains('$'),
            "a shell schedule costs nothing per fire: {st}"
        );

        // CONTROL: an expression that cannot be parsed produces NO note rather
        // than a note with a wrong number in it. A confident zero is worse than
        // silence, because the caller acts on it.
        assert!(super::cadence_note("not an expression", "tmux").is_none());
        assert!(super::cadence_note("", "tmux").is_none());
    }

    #[test]
    fn dashboard_worker_field_aliases_to_session() {
        // The sched editor posts `worker:` (app.js:19067); before the alias
        // this whole payload 422'd at the extractor (Ethan repro 2026-08-09).
        let body: ScheduleBody = serde_json::from_value(serde_json::json!({
            "title": "t", "worker": "amux-rust", "kind": "prompt",
            "command": "noop", "sched_type": "recurring", "recurrence": null,
            "run_at": "", "schedule_expr": "daily at 9am", "watch": 0,
            "done_pattern": null, "done_action": "disable", "watch_timeout": 120,
            "trigger_on": "", "trigger_cooldown": 120, "trigger_sessions": "",
            "by": "dashboard"
        }))
        .expect("the dashboard create payload must deserialize");
        assert_eq!(body.session.as_deref(), Some("amux-rust"));
        // Real typos still refuse (the deny_unknown_fields value survives).
        assert!(serde_json::from_value::<ScheduleBody>(
            serde_json::json!({ "schedule_exp": "daily at 9am" })
        )
        .is_err());
    }
    use super::*;
    use crate::db::Store;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn app() -> (axum::Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("sched-api-test.db")).unwrap();
        let state = AppState {
            store: Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let router = Router::new()
            .nest("/api/schedules", routes())
            .with_state(state);
        (router, dir)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let req = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, v)
    }

    const SES: [(&str, &str); 1] = [("x-amux-session", "tester-session")];

    #[tokio::test]
    async fn create_lists_and_audits_with_attribution() {
        let (app, _dir) = app();
        let (st, created) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({
                "title": "Morning digest",
                "session": "alpha",
                "command": "run the digest",
                "schedule_expr": "daily at 09:00",
            })),
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{created}");
        assert_eq!(created["id"], json!("SCHED-1")); // Python's shared counter format
        assert_eq!(created["sched_type"], json!("recurring"));
        assert_eq!(created["enabled"], json!(1));
        assert!(created["next_run"].as_str().unwrap().contains("T09:00"));
        assert!(created["next_run_ms"].as_i64().is_some(), "create answers with the absolute instant: {created}");

        let (st, list) = send(&app, "GET", "/api/schedules", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        let arr = list.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        // Python field names, plus the Rust parser's own computation.
        assert_eq!(arr[0]["title"], json!("Morning digest"));
        assert_eq!(arr[0]["session"], json!("alpha"));
        assert!(arr[0]["computed_next_run"]
            .as_str()
            .unwrap()
            .contains("T09:00"));

        // The create is audited WITH attribution from the header.
        let (st, audit) = send(&app, "GET", "/api/schedules/audit?id=SCHED-1", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        let row = &audit.as_array().unwrap()[0];
        assert_eq!(row["field"], json!("created"));
        assert_eq!(row["by_who"], json!("tester-session"));
        assert_eq!(row["source"], json!("api-create"));
        assert!(row["new_value"].as_str().unwrap().contains("\"enabled\":1"));
    }

    /// Pass/fail cannot be on without a condition, the condition must parse,
    /// and a saved condition is probed at once (schedule_check.rs).
    #[tokio::test]
    async fn pass_fail_requires_a_measurable_check() {
        let (app, _dir) = app();
        let base = || json!({ "title": "report", "kind": "shell", "command": "make-report", "schedule_expr": "daily at 09:00" });
        let with = |extra: Value| {
            let mut b = base();
            for (k, v) in extra.as_object().unwrap() {
                b[k] = v.clone();
            }
            b
        };
        let (st, r) = send(&app, "POST", "/api/schedules", Some(with(json!({ "pass_fail": 1 }))), &SES).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{r}");
        assert_eq!(r["code"], json!("pass_check_required"));
        let (st, r) = send(&app, "POST", "/api/schedules", Some(with(json!({ "pass_fail": 1, "pass_check": "  " }))), &SES).await;
        assert_eq!((st, r["code"].clone()), (StatusCode::BAD_REQUEST, json!("pass_check_required")));
        let (st, r) = send(&app, "POST", "/api/schedules", Some(with(json!({ "pass_check": "if then (" }))), &SES).await;
        assert_eq!((st, r["code"].clone()), (StatusCode::BAD_REQUEST, json!("pass_check_invalid")), "{r}");

        let (st, c) = send(&app, "POST", "/api/schedules",
            Some(with(json!({ "pass_fail": 1, "pass_check": " test -s /tmp/report.md " }))), &SES).await;
        assert_eq!(st, StatusCode::CREATED, "{c}");
        assert_eq!(c["pass_check"], json!("test -s /tmp/report.md"));
        assert_eq!(c["pass_state"], json!("probe"), "a new condition is checked at once");

        // Off clears the condition and its state; the change is audited.
        let (st, p) = send(&app, "PATCH", "/api/schedules/SCHED-1", Some(json!({ "pass_fail": 0 })), &SES).await;
        assert_eq!(st, StatusCode::OK, "{p}");
        assert_eq!((p["pass_check"].clone(), p["pass_state"].clone()), (Value::Null, Value::Null));
        let (_, audit) = send(&app, "GET", "/api/schedules/audit?id=SCHED-1", None, &[]).await;
        assert!(audit.as_array().unwrap().iter().any(|r| r["field"] == json!("pass_check")
            && r["old_value"] == json!("test -s /tmp/report.md")), "{audit}");
        // On again without a check is refused on PATCH too.
        let (st, _) = send(&app, "PATCH", "/api/schedules/SCHED-1", Some(json!({ "pass_fail": 1 })), &SES).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    /// `when` (0141): validated like a pass check, refused on a shell
    /// schedule, published as `when`/`when_once`, and a relative `once` is
    /// accepted and rounded up to a whole minute.
    #[tokio::test]
    async fn a_wake_condition_is_validated_published_and_refused_on_shell() {
        let (app, _dir) = app();
        let base = json!({ "title": "ci done", "command": "CI finished; read it", "schedule_expr": "every 2m", "session": "lane" });
        let mut bad = base.clone();
        bad["when"] = json!("if then (");
        let (st, r) = send(&app, "POST", "/api/schedules", Some(bad), &SES).await;
        assert_eq!((st, r["code"].clone()), (StatusCode::BAD_REQUEST, json!("when_invalid")), "{r}");
        assert!(r["error"].as_str().unwrap().contains("wake condition"), "{r}");
        let mut shell = base.clone();
        shell["when"] = json!("true");
        shell["kind"] = json!("shell");
        let (st, r) = send(&app, "POST", "/api/schedules", Some(shell), &SES).await;
        assert_eq!((st, r["code"].clone()), (StatusCode::BAD_REQUEST, json!("when_on_shell")), "{r}");

        let mut good = base.clone();
        good["when"] = json!(" test -f /tmp/ci-done ");
        good["until"] = json!("24h");
        let (st, c) = send(&app, "POST", "/api/schedules", Some(good), &SES).await;
        assert_eq!(st, StatusCode::CREATED, "{c}");
        assert_eq!((c["when"].clone(), c["when_once"].clone()), (json!("test -f /tmp/ci-done"), json!(true)), "{c}");
        assert!(c["when_note"].as_str().unwrap().contains("skipped"), "{c}");
        assert!(c["until"].as_str().is_some_and(|u| u.len() == 16), "a relative until is stored as a minute: {c}");
        let id = c["id"].as_str().unwrap().to_string();
        let (_, g) = send(&app, "GET", &format!("/api/schedules/{id}"), None, &[]).await;
        assert_eq!(g["when"], json!("test -f /tmp/ci-done"));

        // PATCH: keep firing, then clear; switching to shell with a check is refused.
        let (st, p) = send(&app, "PATCH", &format!("/api/schedules/{id}"), Some(json!({ "when_once": false })), &SES).await;
        assert_eq!((st, p["when_once"].clone()), (StatusCode::OK, json!(false)), "{p}");
        let (st, p) = send(&app, "PATCH", &format!("/api/schedules/{id}"), Some(json!({ "kind": "shell" })), &SES).await;
        assert_eq!((st, p["code"].clone()), (StatusCode::BAD_REQUEST, json!("when_on_shell")), "{p}");
        let (st, p) = send(&app, "PATCH", &format!("/api/schedules/{id}"), Some(json!({ "when": null })), &SES).await;
        assert_eq!((st, p["when"].clone()), (StatusCode::OK, Value::Null), "{p}");
        let (_, audit) = send(&app, "GET", &format!("/api/schedules/audit?id={id}"), None, &[]).await;
        assert!(audit.as_array().unwrap().iter().any(|r| r["field"] == json!("when_check")), "{audit}");

        // A relative once.
        let (st, o) = send(&app, "POST", "/api/schedules",
            Some(json!({ "title": "later", "command": "look", "session": "lane", "once": "in 20m" })), &SES).await;
        assert_eq!(st, StatusCode::CREATED, "{o}");
        let next = crate::runtime_jobs::scheduler::parse_minute(o["next_run"].as_str().unwrap()).unwrap();
        let mins = (next - Local::now()).num_seconds();
        assert!((19 * 60..=21 * 60).contains(&mins), "in 20m, rounded up to the minute: {o}");
        let (st, r) = send(&app, "POST", "/api/schedules",
            Some(json!({ "title": "later", "command": "look", "once": "in 20" })), &SES).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{r}");
        assert!(r["accepted"]["once"].as_str().unwrap().contains("in 20m"), "{r}");
    }

    #[tokio::test]
    async fn explicit_enabled_zero_is_honored() {
        // AC-139: enabled:0 at create must not come up running.
        let (app, _dir) = app();
        let (st, created) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({ "title": "seeded", "schedule_expr": "every 5m", "enabled": 0 })),
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        assert_eq!(created["enabled"], json!(0));
    }

    #[tokio::test]
    async fn session_scope_only_surfaces_bound_schedules() {
        let (app, _dir) = app();
        for (title, session) in [("a-task", "alpha"), ("b-task", "beta")] {
            let (st, _) = send(
                &app,
                "POST",
                "/api/schedules",
                Some(json!({ "title": title, "session": session, "schedule_expr": "daily at 09:00" })),
                &SES,
            )
            .await;
            assert_eq!(st, StatusCode::CREATED);
        }
        let (st, list) = send(&app, "GET", "/api/schedules?session=alpha", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        let arr = list.as_array().unwrap();
        assert_eq!(arr.len(), 1, "beta's schedule must not surface for alpha");
        assert_eq!(arr[0]["session"], json!("alpha"));
        let (_, all) = send(&app, "GET", "/api/schedules", None, &[]).await;
        assert_eq!(all.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn clearing_the_expr_with_a_dated_run_at_makes_a_one_shot_not_a_daily() {
        // 2026-10-19: seven GS-12 one-offs were PATCHed this way, answered 200,
        // and were armed DAILY at run_at's clock time, days early.
        let (app, _dir) = app();
        let (_, created) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({ "title": "t", "session": "alpha", "schedule_expr": "5 20 31 10 *" })),
            &SES,
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        let when = (Local::now() + chrono::Duration::days(12)).format("%Y-%m-%dT%H:%M").to_string();
        let (st, patched) = send(
            &app,
            "PATCH",
            &format!("/api/schedules/{id}"),
            Some(json!({ "schedule_expr": "", "run_at": when })),
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{patched}");
        assert_eq!(patched["sched_type"], json!("once"), "{patched}");
        assert_eq!(patched["next_run"], json!(when), "{patched}");
    }

    #[tokio::test]
    async fn patch_updates_recomputes_and_audits() {
        let (app, _dir) = app();
        let (_, created) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({ "title": "t", "session": "alpha", "schedule_expr": "daily at 09:00" })),
            &SES,
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();

        // Disable + change expr in one PATCH; both audited, next_run moves.
        let (st, patched) = send(
            &app,
            "PATCH",
            &format!("/api/schedules/{id}"),
            Some(json!({ "enabled": 0, "schedule_expr": "daily at 18:00" })),
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{patched}");
        assert_eq!(patched["enabled"], json!(0));
        assert!(patched["next_run"].as_str().unwrap().contains("T18:00"));

        let (_, audit) = send(
            &app,
            "GET",
            &format!("/api/schedules/audit?id={id}"),
            None,
            &[],
        )
        .await;
        let rows = audit.as_array().unwrap();
        let enabled_row = rows
            .iter()
            .find(|r| r["field"] == json!("enabled"))
            .unwrap();
        assert_eq!(enabled_row["old_value"], json!("1"));
        assert_eq!(enabled_row["new_value"], json!("0"));
        assert_eq!(enabled_row["by_who"], json!("tester-session"));
        assert!(rows.iter().any(|r| r["field"] == json!("schedule_expr")));

        // A no-change PATCH adds no audit rows for tracked fields.
        let before = rows.len();
        let (_, _) = send(
            &app,
            "PATCH",
            &format!("/api/schedules/{id}"),
            Some(json!({ "title": "renamed (untracked field)" })),
            &SES,
        )
        .await;
        let (_, audit2) = send(
            &app,
            "GET",
            &format!("/api/schedules/audit?id={id}"),
            None,
            &[],
        )
        .await;
        assert_eq!(audit2.as_array().unwrap().len(), before);
    }

    #[tokio::test]
    async fn delete_is_soft_audited_and_idempotent() {
        let (app, _dir) = app();
        let (_, created) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({ "title": "t", "schedule_expr": "every 1h" })),
            &SES,
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();

        let (st, body) = send(&app, "DELETE", &format!("/api/schedules/{id}"), None, &SES).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["deleted"], json!(id));
        // Gone from the list; row still readable (soft delete).
        let (_, list) = send(&app, "GET", "/api/schedules", None, &[]).await;
        assert_eq!(list.as_array().unwrap().len(), 0);
        let (st, one) = send(&app, "GET", &format!("/api/schedules/{id}"), None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert!(one["deleted"].as_i64().is_some());

        // Idempotent: no second audit row.
        let (st, again) = send(&app, "DELETE", &format!("/api/schedules/{id}"), None, &SES).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(again["already"], json!(true));
        let (_, audit) = send(
            &app,
            "GET",
            &format!("/api/schedules/audit?id={id}&field=deleted"),
            None,
            &[],
        )
        .await;
        let rows = audit.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["by_who"], json!("tester-session"));
    }

    #[tokio::test]
    async fn writes_to_a_tombstone_are_refused_with_provenance_and_resurrect_is_the_exit() {
        // AMUX-3431 (SCHED-349): a deleted schedule accepted PATCHes, advanced
        // `updated`, and read back as enabled — five days of fictional burn
        // pager. Rebuilt from that specimen: delete, then write.
        let (app, _dir) = app();
        let (_, created) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({ "title": "backstop", "schedule_expr": "every 1h", "command": "orig" })),
            &SES,
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        send(&app, "DELETE", &format!("/api/schedules/{id}"), None, &SES).await;

        // The ordinary PATCH is REFUSED, naming when and by whom.
        let (st, body) = send(
            &app,
            "PATCH",
            &format!("/api/schedules/{id}"),
            Some(json!({ "command": "edited-in-vain" })),
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["blocked"], json!(true));
        assert!(body["deleted_at"].as_i64().unwrap() > 0, "{body}");
        assert_eq!(body["deleted_by"], json!("tester-session"));
        assert!(
            body["how"]["resurrect"]
                .as_str()
                .unwrap()
                .contains("resurrect"),
            "{body}"
        );

        // The refused write did NOT land, and the tombstone says what it is
        // in words — the raw row's enabled:1 next to deleted:<ts> is exactly
        // what read as live for five days.
        let (_, one) = send(&app, "GET", &format!("/api/schedules/{id}"), None, &[]).await;
        assert_eq!(one["command"], json!("orig"), "{one}");
        assert_eq!(one["tombstone"], json!(true), "{one}");
        assert!(
            one["note"].as_str().unwrap().contains("never fires"),
            "{one}"
        );

        // Skip is a write too — same refusal, not a bare 404.
        let (st, sk) = send(
            &app,
            "POST",
            &format!("/api/schedules/{id}/skip"),
            None,
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{sk}");
        assert!(sk["error"].as_str().unwrap().contains("deleted"), "{sk}");

        // The honest exit: an EXPLICIT resurrect applies the write, audited.
        let (st, res) = send(
            &app,
            "PATCH",
            &format!("/api/schedules/{id}"),
            Some(json!({ "resurrect": true, "command": "back-alive" })),
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{res}");
        assert_eq!(res["resurrected"], json!(true));
        assert_eq!(res["command"], json!("back-alive"));
        // Live again: the list (what the scheduler reads) includes it.
        let (_, list) = send(&app, "GET", "/api/schedules", None, &[]).await;
        assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
        // Audited: delete + resurrect are both rows on the `deleted` field.
        let (_, audit) = send(
            &app,
            "GET",
            &format!("/api/schedules/audit?id={id}&field=deleted"),
            None,
            &[],
        )
        .await;
        assert_eq!(audit.as_array().unwrap().len(), 2, "{audit}");
    }

    #[test]
    fn a_started_shell_run_acknowledges_the_command_as_done() {
        let started = super::shell_run_ack(true);
        assert_eq!(started["phase"], json!("applied"), "the start command completed");
        assert_eq!(started["status"], json!("running"), "the run itself is still running");
        assert_eq!(started["pending"], json!(true));
        let attached = super::shell_run_ack(false);
        assert_eq!(attached["phase"], json!("noop"));
        assert_eq!(attached["deduped"], json!(true));
    }

    /// The absolute instant travels beside the zone-less wall-clock string.
    #[test]
    fn next_run_is_published_as_epoch_ms_from_the_servers_clock() {
        let mut v = json!({"next_run": "2026-10-18T16:54", "run_at": ""});
        super::expose_instants(&mut v);
        let want = scheduler::parse_minute("2026-10-18T16:54").unwrap().timestamp_millis();
        assert_eq!(v["next_run_ms"], json!(want));
        assert_eq!(v["run_at_ms"], Value::Null, "no time, no instant");
    }

    #[tokio::test]
    async fn run_now_is_409_in_shadow_mode() {
        // Explicit opt-out: AMUX_RS_SCHEDULER=0 -> shadow mode. (No other
        // test in this crate reads the var — the firing paths are exercised
        // through scheduler_tick's explicit bool — so setting it here cannot
        // race another test.)
        std::env::set_var("AMUX_RS_SCHEDULER", "0");
        let (app, _dir) = app();
        let (_, created) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({ "title": "t", "schedule_expr": "every 1h" })),
            &SES,
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();

        let (st, body) = send(
            &app,
            "POST",
            &format!("/api/schedules/{id}/run"),
            None,
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(body["error"], json!("rust scheduler is in shadow mode"));
        assert_eq!(body["enable"], json!("AMUX_RS_SCHEDULER=1"));
        // And it recorded NOTHING: the refusal is real, not cosmetic.
        let (_, runs) = send(&app, "GET", "/api/schedules/runs", None, &[]).await;
        assert_eq!(runs.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn runs_endpoint_shows_source() {
        let (app, dir) = app();
        let (_, created) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({ "title": "t", "schedule_expr": "every 1h" })),
            &SES,
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        // Write run rows the way the two fire paths do (manual + cron-rs).
        {
            let conn = rusqlite::Connection::open(dir.path().join("sched-api-test.db")).unwrap();
            record_run(
                &conn,
                &id,
                &RunOutcome::Delivered {
                    submission: "confirmed".into(),
                    detail: "sent".into(),
                },
                "manual:tester-session",
            )
            .unwrap();
            scheduler::insert_run(
                &conn,
                &id,
                chrono::Utc::now().timestamp() + 1,
                &RunOutcome::Queued {
                    queue_id: "steer-1".into(),
                    detail: "queued (steering)".into(),
                },
                "cron-rs",
                None,
            )
            .unwrap();
        }
        let (st, runs) = send(&app, "GET", "/api/schedules/runs", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        let rows = runs.as_array().unwrap();
        let sources: Vec<&str> = rows.iter().map(|r| r["source"].as_str().unwrap()).collect();
        assert!(sources.contains(&"cron-rs"), "{sources:?}");
        assert!(sources.contains(&"manual:tester-session"), "{sources:?}");
        assert_eq!(rows[0]["title"], json!("t"));
        // The delivery verdict reaches the CONSUMER, not just the column
        // (ethos rule 4): the runs list is where a human looks, so `queued`
        // must be visibly different from `delivered` here.
        let cron = rows
            .iter()
            .find(|r| r["source"] == json!("cron-rs"))
            .unwrap();
        assert_eq!(cron["status"], json!("queued"));
        assert_eq!(cron["delivery"], json!("queued"));
        assert_eq!(cron["submission"], json!("deferred"));
        let manual = rows
            .iter()
            .find(|r| r["source"] == json!("manual:tester-session"))
            .unwrap();
        assert_eq!(manual["status"], json!("delivered"));
        assert_eq!(manual["submission"], json!("confirmed"));
    }

    /// AMUX-5241: a shell run's exit code and output tail reach the runs list.
    /// A tmux row keeps NULL in both, which is "not recorded", not "exit 0".
    #[tokio::test]
    async fn runs_endpoint_shows_shell_exit_code_and_tail() {
        let (app, dir) = app();
        let (_, created) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({ "title": "pf", "schedule_expr": "every 1h", "kind": "shell", "command": "true" })),
            &SES,
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        {
            let conn = rusqlite::Connection::open(dir.path().join("sched-api-test.db")).unwrap();
            scheduler::insert_run(
                &conn,
                &id,
                chrono::Utc::now().timestamp(),
                &RunOutcome::ShellError {
                    note: "exit 3: FAIL: gate 7".into(),
                    output: Some(scheduler::ShellOutput {
                        exit_code: 3,
                        tail: "== preflight\nFAIL: gate 7".into(),
                    }),
                },
                "cron-rs",
                None,
            )
            .unwrap();
            scheduler::insert_run(
                &conn,
                &id,
                chrono::Utc::now().timestamp() + 1,
                &RunOutcome::Queued {
                    queue_id: "steer-1".into(),
                    detail: "queued".into(),
                },
                "cron-rs",
                None,
            )
            .unwrap();
        }
        let (st, runs) = send(&app, "GET", "/api/schedules/runs", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        let rows = runs.as_array().unwrap();
        let shell = rows.iter().find(|r| r["delivery"] == json!("shell")).unwrap();
        assert_eq!(shell["exit_code"], json!(3));
        assert_eq!(shell["output_tail"], json!("== preflight\nFAIL: gate 7"));
        let tmux = rows.iter().find(|r| r["delivery"] == json!("queued")).unwrap();
        assert!(tmux["exit_code"].is_null() && tmux["output_tail"].is_null());
    }

    #[tokio::test]
    async fn bad_expr_and_unknown_fields_are_rejected() {
        let (app, _dir) = app();
        let (st, body) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({ "title": "t", "schedule_expr": "whenever i feel like it" })),
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], json!("unparseable schedule_expr"));
        assert!(body["accepted"].is_array());

        // Unknown body field: rejected, not dropped (Invariant 37).
        let (st, _) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({ "title": "t", "schedule_exp": "daily at 9am" })),
            &SES,
        )
        .await;
        assert!(st.is_client_error());

        // Unknown audit query param: rejected, not silently ignored (AC-228).
        let (st, body) = send(&app, "GET", "/api/schedules/audit?fied=enabled", None, &[]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("unknown query param"));

        // Missing title.
        let (st, _) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({ "command": "x" })),
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn attribution_disagreement_records_both() {
        let (app, _dir) = app();
        let (st, created) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({ "title": "t", "schedule_expr": "every 1h", "by": "impostor" })),
            &[("x-amux-session", "real-session")],
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let id = created["id"].as_str().unwrap();
        let (_, audit) = send(
            &app,
            "GET",
            &format!("/api/schedules/audit?id={id}"),
            None,
            &[],
        )
        .await;
        assert_eq!(
            audit.as_array().unwrap()[0]["by_who"],
            json!("real-session (claimed impostor)")
        );
    }

    // ---- AMUX-2679: the Skip button --------------------------------------

    /// THE incident: the dashboard has POSTed `/skip` since the Python days
    /// and the Rust router had no such route, so every press 404'd. Fails
    /// against the pre-fix router, where this assertion sees 404.
    ///
    /// A 404 is also what a MISSING SCHEDULE returns, which is why this test
    /// asserts on the 200 body and not merely on "not 404" — the discriminator
    /// has to be that the occurrence actually moved.
    #[tokio::test]
    async fn skip_route_exists_and_advances_exactly_one_occurrence() {
        let (app, _dir) = app();
        let (st, created) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({ "title": "daily thing", "schedule_expr": "daily at 09:00" })),
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let id = created["id"].as_str().unwrap().to_string();
        let armed = created["next_run"].as_str().unwrap().to_string();

        let (st, body) = send(
            &app,
            "POST",
            &format!("/api/schedules/{id}/skip"),
            None,
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "the Skip route must be mounted: {body}");
        // Names WHAT WAS GIVEN UP as well as where it resumes — a toast that
        // can only say "next in 15m" is unfalsifiable on a 15m cadence.
        assert_eq!(
            body["skipped"],
            json!(armed),
            "the response must name the dropped occurrence"
        );
        assert_eq!(
            body["next_run"].as_str().unwrap(),
            scheduler::skip_next_run(&DurableSchedule::from_map(
                created.as_object().unwrap().clone()
            ))
            .unwrap(),
            "exactly one occurrence, computed from the ARMED next_run"
        );
        assert_ne!(body["next_run"], json!(armed));
        // The row really moved — not just the response.
        let (_, row) = send(&app, "GET", &format!("/api/schedules/{id}"), None, &[]).await;
        assert_eq!(row["next_run"], body["next_run"]);
        // Skip does not fire, disable, or count as a run.
        assert_eq!(row["enabled"], json!(1));
        assert_eq!(row["run_count"], json!(0));
        assert!(
            row["last_run"].is_null(),
            "skip must not bump last_run — nothing ran"
        );
        let (_, runs) = send(&app, "GET", "/api/schedules/runs", None, &[]).await;
        assert!(
            runs.as_array().unwrap().is_empty(),
            "skip must not write a run row: {runs}"
        );

        // Attributed (python left this mutation unattributed — AMUX-1812).
        let (_, audit) = send(
            &app,
            "GET",
            &format!("/api/schedules/audit?id={id}&field=next_run"),
            None,
            &[],
        )
        .await;
        let a = &audit.as_array().unwrap()[0];
        assert_eq!(a["source"], json!("api-skip"));
        assert_eq!(a["by_who"], json!("tester-session"));
        assert_eq!(a["old_value"], json!(armed));
        assert_eq!(a["new_value"], body["next_run"]);
    }

    #[tokio::test]
    async fn skip_refuses_a_one_shot_and_a_missing_schedule() {
        let (app, _dir) = app();
        // Python's guard: a one-shot has no following occurrence, so skipping
        // it would be deleting it under another name.
        let (st, created) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(
                json!({ "title": "one shot", "sched_type": "once", "run_at": "2026-08-10T09:00" }),
            ),
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let id = created["id"].as_str().unwrap().to_string();
        let (st, body) = send(
            &app,
            "POST",
            &format!("/api/schedules/{id}/skip"),
            None,
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            body["error"],
            json!("only recurring schedules can be skipped")
        );
        assert!(
            body["detail"].is_string(),
            "a refusal must name the way out"
        );

        let (st, _) = send(&app, "POST", "/api/schedules/SCHED-9999/skip", None, &SES).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    // ---- AMUX-2680: fields this server does not honour --------------------

    /// The refusal must fire on the values the DASHBOARD used to send, not on
    /// a convenient synthetic one. The old save payload is reproduced verbatim
    /// from `saveSchedModal` (loop mode with a stop pattern, and trigger mode)
    /// so a check that passes here could not have passed pre-fix.
    #[tokio::test]
    async fn arming_an_unimplemented_field_is_refused_on_create_and_patch() {
        let (app, _dir) = app();
        // The editor's loop-with-stop-pattern payload.
        let loop_with_stop = json!({
            "title": "t", "worker": "alpha", "kind": "tmux", "command": "go",
            "sched_type": "recurring", "recurrence": null, "run_at": "",
            "schedule_expr": "every 30m",
            "watch": 1, "done_pattern": "all done", "done_action": "disable",
            "watch_timeout": 120, "trigger_on": "", "trigger_cooldown": 120,
            "trigger_sessions": "", "by": "dashboard"
        });
        let (st, body) = send(&app, "POST", "/api/schedules", Some(loop_with_stop), &SES).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        let dead: Vec<&str> = body["unhonoured"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(dead, vec!["watch", "done_pattern"]);
        assert_eq!(body["card"], json!("AMUX-2680"));
        assert!(
            body["instead"].is_string(),
            "a refusal must point somewhere real"
        );

        // The editor's trigger-mode payload.
        let (st, body) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({
                "title": "t", "worker": "alpha", "command": "go",
                "trigger_on": "session_idle", "trigger_cooldown": 120,
                "trigger_sessions": "backend, ts-gke", "by": "dashboard"
            })),
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            body["unhonoured"],
            json!(["trigger_on", "trigger_sessions"])
        );

        // PATCH is the OTHER way the editor armed these — guarding only
        // create would leave the hole open on every existing row.
        let (st, created) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({ "title": "t", "schedule_expr": "every 1h" })),
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let id = created["id"].as_str().unwrap().to_string();
        for arming in [
            json!({ "watch": 1 }),
            json!({ "done_pattern": "done" }),
            json!({ "done_action": "notify" }),
            json!({ "trigger_on": "board" }),
            json!({ "trigger_sessions": "alpha" }),
        ] {
            let (st, body) = send(
                &app,
                "PATCH",
                &format!("/api/schedules/{id}"),
                Some(arming.clone()),
                &SES,
            )
            .await;
            assert_eq!(
                st,
                StatusCode::BAD_REQUEST,
                "PATCH {arming} must be refused: {body}"
            );
        }
    }

    /// The inert values are the ones the dashboard sends on EVERY save,
    /// including from routine and once mode. If those tripped the gate the
    /// whole editor would 400 — a refusal that cannot be satisfied honestly is
    /// worse than the silence it replaced (ethos rule 3).
    /// MF-847. The runs endpoint had ONE way in: newest 50 across every
    /// schedule. At ~192 runs a day fleet-wide an every-15-min sweep rotates
    /// that window inside an hour, so a DAILY schedule's miss — the class whose
    /// misses matter most — was unreadable by the time anyone looked, and
    /// mixpeek-finances concluded the record had been discarded. It had not:
    /// the table retains everything (26,084 rows back to 2026-06-08 when
    /// measured). The cap was a DISPLAY limit that read as a retention limit.
    #[tokio::test]
    async fn runs_can_be_filtered_to_one_schedule_past_the_default_window() {
        let (app, _d) = app();
        // Written straight into the table: the claim under test is that a
        // per-schedule query reaches rows the global window hides, and going
        // through the fire path would test the scheduler instead.
        {
            let conn = rusqlite::Connection::open(_d.path().join("sched-api-test.db")).unwrap();
            for (sched, base) in [("SCHED-A", 1_000_000i64), ("SCHED-B", 2_000_000i64)] {
                for i in 0..60i64 {
                    conn.execute(
                        "INSERT INTO schedule_runs(schedule_id,ran_at,status,source) \
                         VALUES(?1,?2,'delivered','cron-rs')",
                        rusqlite::params![sched, base + i],
                    )
                    .unwrap();
                }
            }
        }

        // Default: the old behaviour, unchanged.
        let (_, all) = send(&app, "GET", "/api/schedules/runs", None, &[]).await;
        assert_eq!(all.as_array().unwrap().len(), 50);

        // THE CELL THIS EXISTS FOR: SCHED-A's older runs are past the global
        // window (60 B-runs are newer) and must still be reachable.
        let (_, mine) = send(
            &app,
            "GET",
            "/api/schedules/runs?schedule_id=SCHED-A&limit=100",
            None,
            &[],
        )
        .await;
        let rows = mine.as_array().unwrap();
        assert_eq!(
            rows.len(),
            60,
            "per-schedule filter did not reach past the 50 window"
        );
        assert!(
            rows.iter().all(|r| r["schedule_id"] == "SCHED-A"),
            "the filter leaked another schedule's runs"
        );
    }

    /// A malformed filter is REJECTED, not ignored. An ignored `limit` returns
    /// 50 rows and looks like an answer, which is the confident-wrong-answer
    /// shape AC-228 already pins for audit_trail.
    #[tokio::test]
    async fn a_bad_runs_filter_is_refused_rather_than_silently_dropped() {
        let (app, _d) = app();
        for bad in [
            "/api/schedules/runs?limit=abc",
            "/api/schedules/runs?limit=0",
            "/api/schedules/runs?limit=99999",
            "/api/schedules/runs?scheduleid=SCHED-A",
        ] {
            let (st, _) = send(&app, "GET", bad, None, &[]).await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "silently accepted {bad}");
        }
    }

    #[tokio::test]
    async fn inert_values_for_those_fields_still_save() {
        let (app, _dir) = app();
        let (st, body) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({
                "title": "t", "worker": "alpha", "kind": "tmux", "command": "go",
                "sched_type": "recurring", "recurrence": null, "run_at": "",
                "schedule_expr": "daily at 9am",
                "watch": 0, "done_pattern": null, "done_action": "disable",
                "watch_timeout": 300, "trigger_on": "", "trigger_cooldown": 120,
                "trigger_sessions": "", "by": "dashboard"
            })),
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{body}");
        // watch_timeout=300 is real residue on a live row (SCHED-8). It is a
        // parameter, not a switch — inert while watch=0 — so refusing it would
        // 400 an edit of that row while closing no false promise.
        let id = body["id"].as_str().unwrap().to_string();
        let (st, _) = send(
            &app,
            "PATCH",
            &format!("/api/schedules/{id}"),
            Some(json!({ "watch_timeout": 300, "trigger_cooldown": 600 })),
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
    }

    /// The declaration and the list cannot drift. A header that silently drops
    /// a field is the same silent absence it exists to announce.
    #[test]
    fn unhonoured_header_lists_every_unhonoured_field() {
        for f in UNHONOURED_FIELDS {
            assert!(
                UNHONOURED_FIELDS_HEADER.contains(f),
                "'{f}' is unhonoured but the declared header never says so"
            );
        }
        assert!(UNHONOURED_FIELDS_HEADER.contains("AMUX-2680"));
    }

    /// The Scheduler page's groups come from the firing parser, not a regex
    /// in the client. Each arm named so a broken one fails by name.
    #[test]
    fn cadence_class_files_each_expression_by_the_firing_parser() {
        let c = |e: &str| cadence_class(Some(e), "recurring").0;
        assert_eq!(c("every 10m"), "loop");
        assert_eq!(c("every 59m"), "loop");
        assert_eq!(c("*/5 * * * *"), "loop");
        assert_eq!(c("every 1h"), "routine", "hourly is a routine, not a loop");
        assert_eq!(c("daily at 09:00"), "routine");
        assert_eq!(c("every weekday at 9am"), "routine");
        assert_eq!(c("0 9,17 * * 1-5"), "routine");
        assert_eq!(c("weekly on monday at 06:00"), "weekly");
        assert_eq!(c("monthly on 1 at 9am"), "weekly");
        assert_eq!(c("0 6 * * 1"), "weekly", "a weekly cron is weekly on every day of the week");
        assert_eq!(c("every 2d"), "weekly");
        assert_eq!(c("not a schedule"), "unknown");
        assert_eq!(cadence_class(None, "once").0, "once");
        assert_eq!(cadence_class(None, "recurring").0, "unknown");
        let (_, planned) = cadence_class(Some("every 15m"), "recurring");
        assert_eq!(planned, Some(96.0));
    }

    /// Runs/day and Failures are COUNTED from schedule_runs in one window,
    /// with the denominator published beside them.
    #[tokio::test]
    async fn list_counts_runs_and_failures_from_run_history() {
        let (app, dir) = app();
        let (st, created) = send(
            &app,
            "POST",
            "/api/schedules",
            Some(json!({ "title": "counted", "session": "alpha", "schedule_expr": "every 10m" })),
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{created}");
        let id = created["id"].as_str().unwrap().to_string();
        let conn = rusqlite::Connection::open(dir.path().join("sched-api-test.db")).unwrap();
        let now = chrono::Utc::now().timestamp();
        for (age, status) in [
            (60, "ok"),
            (120, "error"),
            (180, "refused"),
            (240, "skipped"),
            (2 * 86_400, "error"), // outside the window: neither a run nor a failure
        ] {
            conn.execute(
                "INSERT INTO schedule_runs (schedule_id, ran_at, status, source) VALUES (?1, ?2, ?3, 'cron')",
                rusqlite::params![id, now - age, status],
            )
            .unwrap();
        }
        let (st, list) = send(&app, "GET", "/api/schedules", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        let row = &list.as_array().unwrap()[0];
        let stats = &row["run_stats_24h"];
        assert_eq!(stats["runs"], json!(4), "{stats}");
        assert_eq!(stats["failures"], json!(2), "error + refused, not skipped: {stats}");
        assert_eq!(stats["last_failure_at"], json!(now - 120));
        assert_eq!(stats["measured"], json!(true));
        assert_eq!(stats["n_considered"], json!(4));
        assert_eq!(row["fires_day"], json!(4), "fires_day and runs must be the same count");
        assert_eq!(row["cadence_class"], json!("loop"));
        assert_eq!(row["planned_fires_day"], json!(144.0));
    }

    // ---- GS-12 finding 5: lifecycle fields, owner lock ----------------------

    #[tokio::test]
    async fn once_and_bounds_are_validated_and_stored() {
        let (app, _dir) = app();
        let when = fmt_minute(Local::now() + chrono::Duration::days(3));
        let (st, c) = send(&app, "POST", "/api/schedules",
            Some(json!({"title": "one-off", "session": "lock-test-nobody", "kind": "shell", "command": "true", "once": when})), &SES).await;
        assert_eq!(st, StatusCode::CREATED, "{c}");
        assert_eq!(c["sched_type"], json!("once"));
        assert_eq!(c["run_at"], json!(when));
        assert_eq!(c["next_run"], json!(when), "a one-shot fires at its datetime");
        assert_eq!(c["schedule_expr"], Value::Null);

        let until = fmt_minute(Local::now() + chrono::Duration::days(10));
        let after = fmt_minute(Local::now() + chrono::Duration::days(2));
        let (st, b) = send(&app, "POST", "/api/schedules",
            Some(json!({"title": "bounded", "session": "lock-test-nobody", "kind": "shell", "command": "true",
                        "schedule_expr": "daily at 09:00", "after": after, "until": until, "max_runs": 3})), &SES).await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
        assert_eq!(b["not_before"], json!(after));
        assert_eq!(b["until"], json!(until));
        assert_eq!(b["max_runs"], json!(3));
        assert!(b["next_run"].as_str().unwrap() >= after.as_str(), "first fire honours after: {b}");

        let past = fmt_minute(Local::now() - chrono::Duration::days(1));
        for (bad, why) in [
            (json!({"once": when, "schedule_expr": "every 1h"}), "exclusive"),
            (json!({"once": past}), "in the past"),
            (json!({"once": "soon"}), "unparseable"),
            (json!({"schedule_expr": "every 1h", "max_runs": 0}), "at least 1"),
            (json!({"schedule_expr": "every 1h", "until": past}), "not in the future"),
            (json!({"schedule_expr": "every 1h", "after": until, "until": after}), "must be after"),
        ] {
            let mut body = bad.clone();
            body["title"] = json!("bad");
            let (st, e) = send(&app, "POST", "/api/schedules", Some(body), &SES).await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{bad} -> {e}");
            assert_eq!(e["code"], json!("schedule_bounds_invalid"));
            assert!(e["error"].as_str().unwrap().contains(why), "{bad}: {e}");
        }

        // PATCH clears a bound with null and starts a new max_runs budget.
        let id = b["id"].as_str().unwrap();
        let (st, p) = send(&app, "PATCH", &format!("/api/schedules/{id}"), Some(json!({"until": null, "max_runs": 5})), &SES).await;
        assert_eq!(st, StatusCode::OK, "{p}");
        assert_eq!(p["until"], Value::Null);
        assert_eq!(p["max_runs"], json!(5));
        assert_eq!(p["cron_runs"], json!(0));
    }

    #[tokio::test]
    async fn a_fixed_date_cron_is_told_it_recurs_yearly_and_filed_as_yearly() {
        let (app, _dir) = app();
        let (st, c) = send(&app, "POST", "/api/schedules",
            Some(json!({"title": "drill", "session": "lock-test-nobody", "kind": "shell", "command": "true", "schedule_expr": "5 0 3 10 *"})), &SES).await;
        assert_eq!(st, StatusCode::CREATED, "{c}");
        assert!(c["fixed_date_note"].as_str().unwrap().contains("once:"), "{c}");
        let (_, list) = send(&app, "GET", "/api/schedules", None, &[]).await;
        assert_eq!(list[0]["cadence_class"], json!("yearly"));
        // Bounded to one fire, it is a one-off and needs no note.
        let (_, c2) = send(&app, "POST", "/api/schedules",
            Some(json!({"title": "drill2", "session": "lock-test-nobody", "kind": "shell", "command": "true", "schedule_expr": "5 0 3 10 *", "max_runs": 1})), &SES).await;
        assert!(c2.get("fixed_date_note").is_none(), "{c2}");
    }

    #[tokio::test]
    async fn an_owner_disable_locks_out_a_worker_re_enable_until_the_owner_lifts_it() {
        let (app, _dir) = app();
        let (_, c) = send(&app, "POST", "/api/schedules",
            Some(json!({"title": "prod control", "session": "lock-test-nobody", "kind": "shell", "command": "true", "schedule_expr": "every 1h"})), &SES).await;
        let id = c["id"].as_str().unwrap().to_string();
        let path = format!("/api/schedules/{id}");

        // Owner (no worker header) disables with a reason.
        let (st, d) = send(&app, "PATCH", &path, Some(json!({"enabled": 0, "reason": "prod freeze"})), &[]).await;
        assert_eq!(st, StatusCode::OK, "{d}");
        assert_eq!(d["lock_reason"], json!("prod freeze"));
        assert!(!d["locked_by"].as_str().unwrap_or("").is_empty(), "{d}");
        assert!(d["disabled_reason"].as_str().unwrap().contains("owner"), "{d}");
        assert_eq!(d["disable_notice"]["route"], json!("none"), "{d}");
        assert_eq!(d["disable_notice"]["cause"], json!("owner_disable_locked"),
            "an owner's disable types nothing into the worker; the lock refusal tells it: {d}");

        // A worker cannot re-enable it, and is told why.
        let (st, r) = send(&app, "PATCH", &path, Some(json!({"enabled": 1})), &SES).await;
        assert_eq!(st, StatusCode::CONFLICT, "{r}");
        assert_eq!(r["code"], json!("schedule_owner_locked"));
        assert_eq!(r["lock_reason"], json!("prod freeze"));
        // Other edits still go through, and leave it disabled and locked.
        let (st, e) = send(&app, "PATCH", &path, Some(json!({"title": "prod control v2"})), &SES).await;
        assert_eq!(st, StatusCode::OK, "{e}");
        assert_eq!(e["enabled"], json!(0));
        assert_eq!(e["lock_reason"], json!("prod freeze"));

        // The owner lifts it by enabling.
        let (st, o) = send(&app, "PATCH", &path, Some(json!({"enabled": 1})), &[]).await;
        assert_eq!(st, StatusCode::OK, "{o}");
        assert_eq!(o["enabled"], json!(1));
        assert_eq!(o["locked_by"], Value::Null);
        assert_eq!(o["disabled_reason"], Value::Null);

        // A worker's own disable does not lock.
        let (_, w) = send(&app, "PATCH", &path, Some(json!({"enabled": 0, "reason": "done for today"})), &SES).await;
        assert_eq!(w["locked_by"], Value::Null);
        assert!(w["disabled_reason"].as_str().unwrap().contains("tester-session"), "{w}");
        let (st, _) = send(&app, "PATCH", &path, Some(json!({"enabled": 1})), &SES).await;
        assert_eq!(st, StatusCode::OK, "an unlocked schedule stays the worker's to re-enable");

        let (_, audit) = send(&app, "GET", &format!("/api/schedules/audit?id={id}&limit=100"), None, &[]).await;
        let lock_rows = audit.as_array().unwrap().iter().filter(|r| r["field"] == json!("locked_by")).count();
        assert_eq!(lock_rows, 2, "set and cleared, both audited: {audit}");
    }
}
