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
    self, fmt_minute, get_schedule, insert_audit, insert_schedule, legacy_next_run,
    list_schedules, mint_schedule_id, record_run, skip_next_run, soft_delete_schedule,
    update_schedule, Deliverer, DurableSchedule, LiveDeliverer, RunOutcome, ScheduleExpr,
    AUDIT_FIELDS,
};
use amux_core::revision::{EntityType, MutationKind};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Local;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/runs", get(recent_runs))
        .route("/audit", get(audit_trail))
        .route(
            "/{id}",
            get(get_one).patch(patch).delete(delete_schedule),
        )
        .route("/{id}/run", post(run_now))
        .route("/{id}/skip", post(skip_next))
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
pub const UNHONOURED_FIELDS: [&str; 7] = [
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
    "not implemented (stored, never read; AMUX-2680): watch, watch_timeout, done_pattern, \
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
    if body.watch.unwrap_or(0) != 0 {
        out.push("watch");
    }
    if set(&body.done_pattern) {
        out.push("done_pattern");
    }
    // "disable" is the inert default the editor sends on every save.
    if body.done_action.as_deref().map(str::trim).is_some_and(|s| !s.is_empty() && s != "disable") {
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
    WriteOutcome { applied: false, events: Vec::new() }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty())
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
        Some(Value::Bool(b)) => if *b { "True".into() } else { "False".into() },
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
        Some(Value::String(s)) => {
            Some(matches!(s.as_str(), "1" | "true" | "yes" | "True") as i64)
        }
        Some(_) => return Err(serde::de::Error::custom("expected a boolean or number")),
    })
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
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = store.read()?;
        let rows = list_schedules(&conn, p.session.as_deref())?;
        // Measured fires/day + fleet share (Python parity: the runaway-canary
        // audit — counted from actual runs, not parsed from the expr).
        let day_ago = chrono::Utc::now().timestamp() - 86_400;
        let mut fires: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT schedule_id, COUNT(*) FROM schedule_runs WHERE ran_at >= ?1 GROUP BY schedule_id",
            )?;
            let it = stmt.query_map([day_ago], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for row in it {
                let (k, v) = row?;
                fires.insert(k, v);
            }
        }
        let total: i64 = fires.values().sum::<i64>().max(1);
        let now = Local::now();
        let out: Vec<Value> = rows
            .iter()
            .map(|s| {
                let mut v = s.to_json();
                let f = *fires.get(s.id()).unwrap_or(&0);
                v["fires_day"] = json!(f);
                v["fleet_share"] = json!((100.0 * f as f64 / total as f64 * 10.0).round() / 10.0);
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

// ---- GET /api/schedules/{id} ----------------------------------------------

pub async fn get_one(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = store.read()?;
        Ok(get_schedule(&conn, &id)?)
    })
    .await;
    match joined {
        Ok(Ok(Some(s))) => {
            let mut v = s.to_json();
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
            ([("x-amux-unhonoured-fields", UNHONOURED_FIELDS_HEADER)], Json(v)).into_response()
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
    /// Claimed attribution (weaker than the header; see `mutation_by`).
    #[serde(default)]
    pub by: Option<String>,
    /// PATCH-only: explicitly restore a soft-deleted schedule (AMUX-3431).
    /// Without this a PATCH to a tombstone is refused with 409 — silently
    /// accepting the write is how a deleted burn backstop read as live for
    /// five days while the scheduler (which reads the list) never fired it.
    #[serde(default)]
    pub resurrect: Option<bool>,
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

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ScheduleBody>,
) -> Response {
    let title = match body.title.as_deref().map(str::trim) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => return err(StatusCode::BAD_REQUEST, json!({ "error": "title is required" })),
    };
    // Refuse to arm a behaviour this server does not have (AMUX-2680).
    let dead = armed_unhonoured(&body);
    if !dead.is_empty() {
        return unhonoured_refusal(&dead);
    }
    let by = mutation_by(&headers, body.by.as_deref());
    let now = Local::now();
    let now_ts = chrono::Utc::now().timestamp();
    let run_at = body.run_at.clone().unwrap_or_else(|| fmt_minute(now));

    // next_run — prefer schedule_expr (forces recurring), like Python; but
    // an unparseable expr is a 400 naming the grammar, not a quiet mis-arm.
    let expr = body.schedule_expr.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let (sched_type, next_run) = match expr {
        Some(e) => match ScheduleExpr::parse(e) {
            Ok(parsed) => (
                "recurring".to_string(),
                parsed.next_run_after(now).map(fmt_minute).unwrap_or_else(|| run_at.clone()),
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
    m.insert("session".into(), json!(body.session.clone().unwrap_or_default()));
    m.insert("command".into(), json!(body.command.clone().unwrap_or_default()));
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
    m.insert("recurrence".into(), body.recurrence.clone().map(Value::from).unwrap_or(Value::Null));
    m.insert("run_at".into(), json!(run_at));
    m.insert("next_run".into(), json!(next_run));
    m.insert("last_run".into(), Value::Null);
    // Honor an EXPLICIT enabled (the AC-139 lesson: a literal 1 silently
    // discarded seeded `enabled:0` and burned a prospect's trial budget).
    m.insert("enabled".into(), json!(body.enabled.unwrap_or(1)));
    m.insert("run_count".into(), json!(0));
    m.insert("schedule_expr".into(), expr.map(Value::from).unwrap_or(Value::Null));
    m.insert("watch".into(), json!(body.watch.unwrap_or(0)));
    m.insert("watch_timeout".into(), json!(body.watch_timeout.unwrap_or(120)));
    m.insert("done_pattern".into(), body.done_pattern.clone().map(Value::from).unwrap_or(Value::Null));
    m.insert("done_action".into(), json!(body.done_action.clone().unwrap_or_else(|| "disable".into())));
    m.insert("trigger_on".into(), body.trigger_on.clone().filter(|s| !s.trim().is_empty()).map(Value::from).unwrap_or(Value::Null));
    m.insert("trigger_cooldown".into(), json!(body.trigger_cooldown.unwrap_or(120)));
    m.insert("trigger_sessions".into(), body.trigger_sessions.clone().filter(|s| !s.trim().is_empty()).map(Value::from).unwrap_or(Value::Null));
    m.insert(
        "exit_actions".into(),
        match &body.exit_actions {
            Some(Value::Object(o)) => Value::String(Value::Object(o.clone()).to_string()),
            Some(Value::String(s)) => Value::String(s.clone()),
            _ => Value::Null,
        },
    );
    m.insert("created".into(), json!(now_ts));
    m.insert("updated".into(), json!(now_ts));
    m.insert("deleted".into(), Value::Null);

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
            });
            insert_audit(conn, &id, "created", "", &summary.to_string(), "api-create", &by_w)?;
            let events = vec![ev(&id, MutationKind::Created)];
            finish(&slot_w, row.to_json(), WriteOutcome { applied: true, events })
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
                let expr = o.get("schedule_expr").and_then(Value::as_str).unwrap_or("").to_string();
                let kind = o.get("kind").and_then(Value::as_str).unwrap_or("tmux").to_string();
                if let Some(n) = cadence_note(&expr, &kind) {
                    o.insert("cadence_note".into(), n);
                }
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
    Json(body): Json<ScheduleBody>,
) -> Response {
    // Validate a caller-supplied expr OUTSIDE the write (clean 400 before
    // the writer thread is touched). A stored legacy/garbage expr on a row
    // this PATCH does not touch falls back like Python instead of bricking
    // the update.
    if let Some(e) = body.schedule_expr.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
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
    let by = mutation_by(&headers, body.by.as_deref());
    let now = Local::now();

    enum Outcome {
        NotFound,
        Tombstone { deleted_at: i64, deleted_by: Option<String> },
        Applied(Value),
    }
    let slot: Arc<Mutex<Option<Outcome>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let id_w = id.clone();
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
            let old_vals: Map<String, Value> = AUDIT_FIELDS
                .iter()
                .map(|k| (k.to_string(), s.raw().get(*k).cloned().unwrap_or(Value::Null)))
                .collect();

            // Apply provided fields (Python's writable-field list).
            if let Some(v) = &body.title { s.set("title", json!(v)); }
            if let Some(v) = &body.session { s.set("session", json!(v)); }
            if let Some(v) = &body.command { s.set("command", json!(v)); }
            if let Some(v) = &body.kind { s.set("kind", json!(v)); }
            if let Some(v) = &body.sched_type { s.set("sched_type", json!(v)); }
            if let Some(v) = &body.recurrence { s.set("recurrence", json!(v)); }
            if let Some(v) = &body.run_at { s.set("run_at", json!(v)); }
            if let Some(v) = body.enabled { s.set("enabled", json!(v)); }
            if let Some(v) = &body.schedule_expr { s.set("schedule_expr", json!(v)); }
            if let Some(v) = body.watch { s.set("watch", json!(v)); }
            if let Some(v) = body.watch_timeout { s.set("watch_timeout", json!(v)); }
            if let Some(v) = &body.done_pattern { s.set("done_pattern", json!(v)); }
            if let Some(v) = &body.done_action { s.set("done_action", json!(v)); }
            if let Some(v) = &body.trigger_on { s.set("trigger_on", json!(v)); }
            if let Some(v) = body.trigger_cooldown { s.set("trigger_cooldown", json!(v)); }
            if let Some(v) = &body.trigger_sessions { s.set("trigger_sessions", json!(v)); }
            if let Some(v) = &body.exit_actions {
                let stored = match v {
                    Value::Object(o) => Value::String(Value::Object(o.clone()).to_string()),
                    other => other.clone(),
                };
                s.set("exit_actions", stored);
            }

            // Recompute next_run from the (possibly new) expr, Python-style.
            match s.schedule_expr().map(str::to_string) {
                Some(e) => {
                    s.set("sched_type", json!("recurring"));
                    let nr = ScheduleExpr::parse(&e)
                        .ok()
                        .and_then(|p| p.next_run_after(now))
                        .map(fmt_minute)
                        .unwrap_or_else(|| s.str_field("run_at").to_string());
                    s.set("next_run", json!(nr));
                }
                None => {
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

            // Audit every changed tracked field (esp. enabled) — AMUX-1735.
            for k in AUDIT_FIELDS {
                let old = audit_str(old_vals.get(k));
                let new = audit_str(s.raw().get(k));
                if old != new {
                    insert_audit(conn, s.id(), k, &old, &new, "api-patch", &by)?;
                }
            }
            let events = vec![ev(s.id(), MutationKind::Updated)];
            let mut out = s.to_json();
            if was_deleted {
                if let Some(o) = out.as_object_mut() {
                    o.insert("resurrected".into(), json!(true));
                }
            }
            finish(&slot_w, Outcome::Applied(out), WriteOutcome { applied: true, events })
        })
        .await;
    match write {
        Ok(_) => match slot.lock().expect("slot").take() {
            // AMUX-3546: the card says "created OR EDITED". An edit that
            // changes the cadence is the same decision as choosing it, made a
            // second time, and it is the one a schedule actually gets — nobody
            // deletes and recreates to go from hourly to every 10m.
            Some(Outcome::Applied(mut v)) => {
                if let Value::Object(o) = &mut v {
                    let expr =
                        o.get("schedule_expr").and_then(Value::as_str).unwrap_or("").to_string();
                    let kind =
                        o.get("kind").and_then(Value::as_str).unwrap_or("tmux").to_string();
                    if let Some(n) = cadence_note(&expr, &kind) {
                        o.insert("cadence_note".into(), n);
                    }
                }
                Json(v).into_response()
            }
            Some(Outcome::NotFound) => not_found(),
            Some(Outcome::Tombstone { deleted_at, deleted_by }) => {
                tombstone_refusal(&id, "PATCH", deleted_at, deleted_by.as_deref())
            }
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
            insert_audit(conn, s.id(), "deleted", &old.to_string(), &now_ts.to_string(), "api-delete", &by)?;
            let events = vec![ev(s.id(), MutationKind::Deleted)];
            finish(&slot_w, Outcome::Deleted, WriteOutcome { applied: true, events })
        })
        .await;
    match write {
        Ok(_) => match slot.lock().expect("slot").take() {
            Some(Outcome::Deleted) => Json(json!({ "deleted": id })).into_response(),
            Some(Outcome::Already) => Json(json!({ "deleted": id, "already": true })).into_response(),
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
        Tombstone { deleted_at: i64, deleted_by: Option<String> },
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
            insert_audit(conn, s.id(), "next_run", &skipped, &new_next, "api-skip", &by)?;
            let body = json!({
                "ok": true,
                "id": s.id(),
                "title": s.str_field("title"),
                // WHAT WAS GIVEN UP, and where it resumes.
                "skipped": skipped,
                "next_run": new_next,
                "schedule_expr": s.schedule_expr(),
                "enabled": s.i64_field("enabled", 0),
            });
            let events = vec![ev(s.id(), MutationKind::Updated)];
            finish(&slot_w, Outcome::Skipped(body), WriteOutcome { applied: true, events })
        })
        .await;
    match write {
        Ok(_) => match slot.lock().expect("slot").take() {
            Some(Outcome::Skipped(v)) => Json(v).into_response(),
            Some(Outcome::NotFound) => not_found(),
            Some(Outcome::Tombstone { deleted_at, deleted_by }) => {
                tombstone_refusal(&id, "skip", deleted_at, deleted_by.as_deref())
            }
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

pub async fn run_now(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    // DUAL-SCHEDULER: while Python owns firing, a Rust run-now that recorded
    // a run and delivered nothing would be a lie in the run history. Refuse
    // honestly, with the way out in the body.
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
    let sched = match tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
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
            Ok(WriteOutcome { applied: true, events })
        })
        .await;
    if let Err(e) = write {
        return internal(e);
    }

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
pub async fn recent_runs(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        let mut stmt = conn.prepare(
            "SELECT sr.id, sr.schedule_id, sr.ran_at, sr.status, sr.note, sr.source,
                    sr.delivery, sr.submission, s.title
             FROM schedule_runs sr LEFT JOIN schedules s ON s.id = sr.schedule_id
             ORDER BY sr.ran_at DESC LIMIT 50",
        )?;
        let rows = stmt.query_map([], |r| {
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
    let mut bad: Vec<&str> =
        q.keys().map(String::as_str).filter(|k| !KNOWN.contains(k)).collect();
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
    let full = matches!(q.get("full").map(String::as_str), Some("1") | Some("true") | Some("yes"));
    let limit: i64 = q.get("limit").and_then(|s| s.parse().ok()).unwrap_or(100).min(500);

    let store = state.store.clone();
    let sid2 = sid.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
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
            [("x-amux-audit-full", "add ?full=1 (or ?id=SCHED-N) for untruncated values")],
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
    /// AMUX-3546: the note states the rate and the spend, and NEVER refuses.
    ///
    /// `every 15m` is the card's own specimen: eleven characters, 96 turns a
    /// day, five of them enabled on the machine that motivated this.
    #[test]
    fn a_cadence_note_states_the_rate_and_never_refuses() {
        let n = super::cadence_note("every 15m", "tmux").expect("countable");
        let t = n.as_str().unwrap();
        assert!(t.contains("96"), "the RATE is the fact nobody had: {t}");
        assert!(t.contains("576"), "96 fires x ~$6 = $576/day, and that is the point: {t}");
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
        assert!(!st.contains('$'), "a shell schedule costs nothing per fire: {st}");

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
        };
        let router = Router::new().nest("/api/schedules", routes()).with_state(state);
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
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
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

        let (st, list) = send(&app, "GET", "/api/schedules", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        let arr = list.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        // Python field names, plus the Rust parser's own computation.
        assert_eq!(arr[0]["title"], json!("Morning digest"));
        assert_eq!(arr[0]["session"], json!("alpha"));
        assert!(arr[0]["computed_next_run"].as_str().unwrap().contains("T09:00"));

        // The create is audited WITH attribution from the header.
        let (st, audit) = send(&app, "GET", "/api/schedules/audit?id=SCHED-1", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        let row = &audit.as_array().unwrap()[0];
        assert_eq!(row["field"], json!("created"));
        assert_eq!(row["by_who"], json!("tester-session"));
        assert_eq!(row["source"], json!("api-create"));
        assert!(row["new_value"].as_str().unwrap().contains("\"enabled\":1"));
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

        let (_, audit) = send(&app, "GET", &format!("/api/schedules/audit?id={id}"), None, &[]).await;
        let rows = audit.as_array().unwrap();
        let enabled_row = rows.iter().find(|r| r["field"] == json!("enabled")).unwrap();
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
        let (_, audit2) = send(&app, "GET", &format!("/api/schedules/audit?id={id}"), None, &[]).await;
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
        assert!(body["how"]["resurrect"].as_str().unwrap().contains("resurrect"), "{body}");

        // The refused write did NOT land, and the tombstone says what it is
        // in words — the raw row's enabled:1 next to deleted:<ts> is exactly
        // what read as live for five days.
        let (_, one) = send(&app, "GET", &format!("/api/schedules/{id}"), None, &[]).await;
        assert_eq!(one["command"], json!("orig"), "{one}");
        assert_eq!(one["tombstone"], json!(true), "{one}");
        assert!(one["note"].as_str().unwrap().contains("never fires"), "{one}");

        // Skip is a write too — same refusal, not a bare 404.
        let (st, sk) = send(&app, "POST", &format!("/api/schedules/{id}/skip"), None, &SES).await;
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
        let (_, audit) =
            send(&app, "GET", &format!("/api/schedules/audit?id={id}&field=deleted"), None, &[])
                .await;
        assert_eq!(audit.as_array().unwrap().len(), 2, "{audit}");
    }

    #[tokio::test]
    async fn run_now_is_409_in_shadow_mode() {
        // Default deployment: AMUX_RS_SCHEDULER unset -> shadow mode. (No
        // test in this crate ever SETS the var — the firing paths are
        // exercised through scheduler_tick's explicit bool — so clearing it
        // here cannot race another test.)
        std::env::remove_var("AMUX_RS_SCHEDULER");
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

        let (st, body) = send(&app, "POST", &format!("/api/schedules/{id}/run"), None, &SES).await;
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
                &RunOutcome::Delivered { submission: "confirmed".into(), detail: "sent".into() },
                "manual:tester-session",
            )
            .unwrap();
            scheduler::insert_run(
                &conn,
                &id,
                chrono::Utc::now().timestamp() + 1,
                &RunOutcome::Queued { queue_id: "steer-1".into(), detail: "queued (steering)".into() },
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
        let cron = rows.iter().find(|r| r["source"] == json!("cron-rs")).unwrap();
        assert_eq!(cron["status"], json!("queued"));
        assert_eq!(cron["delivery"], json!("queued"));
        assert_eq!(cron["submission"], json!("deferred"));
        let manual = rows.iter().find(|r| r["source"] == json!("manual:tester-session")).unwrap();
        assert_eq!(manual["status"], json!("delivered"));
        assert_eq!(manual["submission"], json!("confirmed"));
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
        assert!(body["error"].as_str().unwrap().contains("unknown query param"));

        // Missing title.
        let (st, _) = send(&app, "POST", "/api/schedules", Some(json!({ "command": "x" })), &SES).await;
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
        let (_, audit) = send(&app, "GET", &format!("/api/schedules/audit?id={id}"), None, &[]).await;
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

        let (st, body) =
            send(&app, "POST", &format!("/api/schedules/{id}/skip"), None, &SES).await;
        assert_eq!(st, StatusCode::OK, "the Skip route must be mounted: {body}");
        // Names WHAT WAS GIVEN UP as well as where it resumes — a toast that
        // can only say "next in 15m" is unfalsifiable on a 15m cadence.
        assert_eq!(body["skipped"], json!(armed), "the response must name the dropped occurrence");
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
        assert!(row["last_run"].is_null(), "skip must not bump last_run — nothing ran");
        let (_, runs) = send(&app, "GET", "/api/schedules/runs", None, &[]).await;
        assert!(runs.as_array().unwrap().is_empty(), "skip must not write a run row: {runs}");

        // Attributed (python left this mutation unattributed — AMUX-1812).
        let (_, audit) =
            send(&app, "GET", &format!("/api/schedules/audit?id={id}&field=next_run"), None, &[])
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
            Some(json!({ "title": "one shot", "sched_type": "once", "run_at": "2026-08-10T09:00" })),
            &SES,
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let id = created["id"].as_str().unwrap().to_string();
        let (st, body) =
            send(&app, "POST", &format!("/api/schedules/{id}/skip"), None, &SES).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], json!("only recurring schedules can be skipped"));
        assert!(body["detail"].is_string(), "a refusal must name the way out");

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
        let (st, body) =
            send(&app, "POST", "/api/schedules", Some(loop_with_stop), &SES).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        let dead: Vec<&str> =
            body["unhonoured"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(dead, vec!["watch", "done_pattern"]);
        assert_eq!(body["card"], json!("AMUX-2680"));
        assert!(body["instead"].is_string(), "a refusal must point somewhere real");

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
        assert_eq!(body["unhonoured"], json!(["trigger_on", "trigger_sessions"]));

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
            let (st, body) =
                send(&app, "PATCH", &format!("/api/schedules/{id}"), Some(arming.clone()), &SES)
                    .await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "PATCH {arming} must be refused: {body}");
        }
    }

    /// The inert values are the ones the dashboard sends on EVERY save,
    /// including from routine and once mode. If those tripped the gate the
    /// whole editor would 400 — a refusal that cannot be satisfied honestly is
    /// worse than the silence it replaced (ethos rule 3).
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
}
