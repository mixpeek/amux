//! Command/message history API: `/api/history` over the live `cmd_history`
//! table, ported from the Python handlers (GET list/counts/sessions, POST
//! append, POST import, DELETE clear).
//!
//! Python parity decisions, recorded so they are not "fixed" later:
//! - The stored `type` values are kept as-is; `kind` (human/session/schedule/
//!   amux/unknown) and `queued` are DERIVED on read, like `_msg_kind`/
//!   `_msg_is_queued`.
//!
//!   CORRECTED 2026-08-26 (AMUX-3737). This block used to record "unknown types
//!   read as human, because that is the reading that gets a message looked at
//!   rather than filtered away" as a deliberate Python-parity decision. The
//!   reasoning is about VISIBILITY and it is sound; the conclusion does not
//!   follow, because `human` is not the only visible bucket. That default
//!   silently attributed 355 machine-generated `pickup` nudges to a person, and
//!   Ethan caught it from a screenshot rather than from anything here. Unknown
//!   types now read as `unknown`: just as visible, and honest about what it
//!   does not know. Kept as a note rather than deleted, because a stale
//!   rationale is what made the default look considered.
//! - Every filter (kind, q, session, group) is applied IN SQL, before the
//!   LIMIT — the page-vs-corpus gap (AMUX-2548) is exactly what the Python
//!   comments warn about.
//! - `?group=` resolves members from the session env files
//!   (`<amux home>/sessions/*.env`, `CC_TAGS`), skipping names in
//!   `blocked-sessions.txt` — the same source Python's `list_sessions()`
//!   reads tags from. An empty group matches NOTHING (`1=0`), never
//!   everything.
//! - Secret redaction runs on the way IN on both POST paths, with the same
//!   pattern table as `_redact_secrets` (AMUX-2525), and fails OPEN: a
//!   capture that loses the prompt is worse than one that stores it.
//! - `?sessions=`/`?counts=` are Python-truthy flags: any non-empty value
//!   (including "0") selects the branch, because `parse_qs` drops empty
//!   values and a non-empty list is truthy.
//! - One deliberate deviation: a non-numeric `?limit=`/`?offset=` falls
//!   back to the default (500/0) where Python's bare `int()` answers 500.

use super::settings::amux_home;
use super::AppState;
use crate::db::{PendingEvent, WriteOutcome};
use amux_core::revision::{EntityType, MutationKind};
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_history).post(append_history).delete(clear_history))
        .route("/import", axum::routing::post(import_history))
        // Look up ONE message by its id. `/import` is a literal POST above, so
        // this GET capture never swallows it.
        .route("/{id}", get(get_history_item))
}

/// GET /api/history/{id} — look up ONE message by its id, accepting either a
/// bare integer or the `MSG-<id>` form the UI shows and people paste.
///
/// Ethan 2026-08-13: "msg api isn't obvious to workers" — a worker told to look
/// at MSG-28003 had NO endpoint to fetch it. The `MSG-<n>` id it sees is a
/// `cmd_history` ROW id with a display prefix, but `/api/messages/{id}` is a
/// DIFFERENT table (`_amux_messages`, ULID ids), so the obvious guess 404s. This
/// is the lookup that matches what the id actually is.
async fn get_history_item(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let raw = id.trim().trim_start_matches("MSG-").trim_start_matches("msg-").trim();
    let Ok(nid) = raw.parse::<i64>() else {
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": format!("'{id}' is not a message id — expected MSG-<number> or a number") }),
        );
    };
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let conn = store.read()?;
        let sql = "SELECT id, text, type, session, ts, origin, card_id, \
                   delivery, queued_at, delivered_at, submit_verdict \
                   FROM cmd_history WHERE id=?1";
        let refs: Vec<&dyn rusqlite::types::ToSql> = vec![&nid];
        let mut rows = super::calendar::query_rows_json(&conn, sql, &refs)?;
        if let Some(d) = rows.first_mut() {
            let mtype = d.get("type").and_then(Value::as_str).unwrap_or("").to_string();
            d["kind"] = json!(msg_kind(&mtype));
            d["queued"] = json!(msg_is_queued(&mtype));
            // JOIN THE INSTRUMENT THAT CAN ACTUALLY ANSWER. Matched on session
            // and a +/-10s window around `ts`, never on text: cmd_history keeps
            // the "[08:19 AM] " prefix the composer adds and steering_history
            // does not, so a text match silently misses the rows that matter.
            let delivery = d.get("delivery").and_then(Value::as_str).unwrap_or("").to_string();
            let steering = if delivery == "direct" {
                None
            } else {
                let sess = d.get("session").and_then(Value::as_str).unwrap_or("").to_string();
                let ts_s = d.get("ts").and_then(Value::as_i64).unwrap_or(0) as f64 / 1000.0;
                conn.query_row(
                    "SELECT delivered_at FROM steering_history \
                     WHERE session = ?1 AND ABS(queued_at - ?2) <= 10 \
                     ORDER BY ABS(queued_at - ?2) LIMIT 1",
                    rusqlite::params![sess, ts_s],
                    |r| r.get::<_, Option<f64>>(0),
                )
                .ok()
            };
            let (verdict, source) = delivery_truth(&delivery, steering);
            d["delivered"] = json!(verdict);
            d["delivered_source"] = json!(source);
            if let Some(Some(t)) = steering {
                d["delivered_at_actual"] = json!((t * 1000.0) as i64);
            }
        }
        Ok(rows.into_iter().next())
    })
    .await;
    match joined {
        Ok(Ok(Some(row))) => Json(row).into_response(),
        Ok(Ok(None)) => err(StatusCode::NOT_FOUND, json!({ "error": format!("MSG-{nid} not found") })),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

/// Whether a recorded message was actually DELIVERED, and which instrument says so.
///
/// `cmd_history` CANNOT answer this and has now misled in both directions on the
/// same column:
///
///   - AF-159 read "all 144 have delivered_at set, so these landed". False —
///     the column was a copy of the insert time (AMUX-3541 stopped the copy).
///   - A frustration sweep on 2026-08-27 read "92 of 92 queued rows have a NULL
///     delivered_at, so none landed" and nearly filed 92 lost messages. Also
///     false — nothing stamps the column on delivery, which AMUX-3541's own
///     comment says in as many words. steering_history showed both specimens
///     delivered in 2-3 seconds.
///
/// A NULL that means "not delivered" and a NULL that means "nobody writes here"
/// are the same bytes, so every reader has to know a fact that is not in the
/// payload. `steering_history` IS stamped by the deliverer, so it can answer —
/// and this joins the two rather than waiting for the deliverer to backfill.
///
/// `steering` encodes THREE input states, because collapsing the last two is the
/// whole defect: None = no matching row; Some(None) = row found, unstamped;
/// Some(Some(t)) = row found, delivered at t.
fn delivery_truth(cmd_delivery: &str, steering: Option<Option<f64>>) -> (&'static str, &'static str) {
    // A DIRECT send really was delivered when it was recorded — AMUX-3541 kept
    // `now_ms` there for exactly that reason, so cmd_history is authoritative
    // for this case and no join is needed.
    if cmd_delivery == "direct" {
        return ("delivered", "cmd_history — a direct send is delivered when it is recorded");
    }
    match steering {
        Some(Some(_)) => ("delivered", "steering_history — stamped by the deliverer when it landed"),
        Some(None) => (
            "not delivered",
            "steering_history — the deliverer holds this row and has not stamped it",
        ),
        // NOT "not delivered". The absence of a steering row is a fact about our
        // lookup, not about the message, and reporting it as a negative is the
        // exact misreading this function exists to stop.
        None => (
            "unknown",
            "no matching steering_history row — and cmd_history.delivered_at is NOT stamped \
             for queued rows (AMUX-3541), so its NULL is not evidence either way",
        ),
    }
}

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

use super::internal;

fn ev(id: &str, mutation: MutationKind) -> PendingEvent {
    PendingEvent {
        entity_type: EntityType::Other("cmd_history".into()),
        entity_id: id.to_string(),
        mutation,
        payload: None,
    }
}

// ---- kind derivation (_MSG_KINDS / _msg_kind / _msg_is_queued) -------------

const MSG_KINDS: [&str; 6] =
    ["human", "session", "schedule", "amux", "unstamped", "unknown"];

/// The stored types a HUMAN actually produces. An ALLOWLIST, deliberately.
///
/// The old rule was a denylist with `human` as the fallback, which is the most
/// consequential default available here: it attributes machine-generated text
/// to a person, and it does so silently for every message type amux invents
/// after the classifier was written. `pickup` is exactly that: auto-pickup
/// nudges, stamped `origin: board-drive`, 355 rows reading `Human` in the
/// Messages view.
pub(crate) const HUMAN_TYPES: [&str; 4] = [
    "direct", "steering", "user",
    // Legacy rows predating the type column. Historical human traffic, kept
    // human on purpose; this is the one case where absence really does mean a
    // person typed it.
    "",
];

/// The stored types amux itself produces. Same shape as [`HUMAN_TYPES`] and
/// read by the FILTER as well as the classifier, so the two cannot drift.
pub(crate) const AMUX_TYPES: [&str; 2] = ["system", "pickup"];

/// A send that went in via raw tmux keystrokes while the server was
/// unreachable, reconciled into the trail afterwards by the CLI.
///
/// ITS OWN KIND, not `amux` and emphatically not `human` (AMUX-2670). A person
/// probably did type it, but its delivery was never verified — keystrokes
/// reached a pane and a picker may have eaten them — and its origin is the
/// CLI's word rather than a server-side stamp. That is a different claim from
/// either bucket.
///
/// The dashboard has classified this as `unstamped` since AMUX-2670, in
/// `_msgKind`, with a comment saying an unstamped injection must not render
/// identically to an audited send. THAT BRANCH HAS NEVER RUN: `_msgKind`
/// returns the server's `kind` when the row carries one, every API row does,
/// and the server said `human`. There was also no `_MSG_KIND.unstamped` entry,
/// so even reaching it would have fallen back to the Human badge. A fix written
/// into a path nothing executes (ethos rule 1), found while fixing AMUX-3737
/// one line above it.
pub(crate) const UNSTAMPED_TYPES: [&str; 1] = ["raw-tmux-fallback"];

/// Canonical kind for a stored type.
///
/// THE UNKNOWN ARM IS THE POINT (AMUX-3737). Ethan: "youre confusing human vs
/// session messages", over a screenshot of `[amux] You went idle holding
/// RC-53...` wearing a blue `Human` badge. That row already carried
/// `origin: board-drive` and `type: pickup`; the classifier read TYPE, did not
/// recognise `pickup`, and fell through to `human`. Two fields in one row
/// disagreeing about the same fact, with the view reading the wrong one.
///
/// Adding `"pickup" => "amux"` would fix those 355 rows and leave the shape
/// intact, so the NEXT type someone adds becomes Human again in silence. An
/// explicit `unknown` makes that self-announcing instead: a kind nobody expects
/// showing up in the Messages view is the signal that a type was added without
/// teaching this function.
///
/// Note what the FIX HAD TO CHANGE: `msg_kind("legacy-weirdness") == "human"`
/// was an assertion pinning the fallback. A test can pin a bug precisely
/// because a default is indistinguishable from a decision once it is written
/// down.
///
/// `/api/usage/attribution` already had the safe shape — `human = trig ==
/// "user"`, an allowlist — so its background/human split was never wrong, and
/// the 49.7%-of-input-tokens-are-amux-initiated figure on AMUX-3759 stands.
/// Two classifiers over the same distinction, one safe and one not.
pub(crate) fn msg_kind(mtype: &str) -> &'static str {
    let t = mtype.trim().to_lowercase();
    match t.as_str() {
        "session" => "session",
        "schedule" => "schedule",
        // Machine-authored. `pickup` is board-drive's auto-pickup prompt.
        _ if AMUX_TYPES.contains(&t.as_str()) => "amux",
        _ if UNSTAMPED_TYPES.contains(&t.as_str()) => "unstamped",
        _ if HUMAN_TYPES.contains(&t.as_str()) => "human",
        _ => "unknown",
    }
}

/// Python `_msg_is_queued`: steering = a human message queued rather than
/// sent straight through. A delivery detail, not a kind.
pub(crate) fn msg_is_queued(mtype: &str) -> bool {
    mtype.trim().to_lowercase() == "steering"
}

// ---- secret redaction (_CAPTURE_SECRET_RES / _redact_secrets) --------------

static SECRET_RES: OnceLock<Vec<Regex>> = OnceLock::new();

fn secret_res() -> &'static [Regex] {
    SECRET_RES.get_or_init(|| {
        [
            r"sk-ant-api[0-9a-zA-Z_-]{20,}",
            r"sk-(?:proj-|svcacct-)?[A-Za-z0-9_-]{20,}",
            r"AIza[0-9A-Za-z_-]{30,}",
            r"AKIA[0-9A-Z]{16}",
            r"ghp_[A-Za-z0-9]{36}",
            r"sk_(?:test|live)_[A-Za-z0-9]{20,}",
            r"xox[baprs]-[A-Za-z0-9-]{10,}",
            r"glpat-[A-Za-z0-9_-]{20,}",
            // Prefixed assignment shapes: OPENAI_API_KEY=..., LOB_API_KEY: ...
            r"(?i)\b[\w-]*(?:password|passwd|secret|api[_-]?key|token)\b\s*[:=]\s*\S{8,}",
            // A human pasting a login: email (+optional note) // password.
            r"[\w.+-]+@[\w-]+\.[\w.]+\s*(?:\([^)]*\))?\s*(?://|:|\|)\s*\S{8,}",
        ]
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect()
    })
}

/// Python `_redact_secrets`: replace every credential-shaped match, count
/// hits. Cannot fail — an empty pattern table just means zero hits.
pub(crate) fn redact_secrets(text: &str) -> (String, usize) {
    let mut out = text.to_string();
    let mut hits = 0usize;
    for rx in secret_res() {
        let n = rx.find_iter(&out).count();
        if n > 0 {
            out = rx.replace_all(&out, "[REDACTED-CREDENTIAL]").into_owned();
            hits += n;
        }
    }
    (out, hits)
}

// ---- group membership (Python list_sessions()'s tags, filesystem source) ---

/// Sessions whose env file carries `group` in CC_TAGS, minus blocked names.
/// Same inputs as Python's `list_sessions()` tag derivation:
/// `<home>/sessions/<name>.env` CC_TAGS + `<home>/blocked-sessions.txt`.
pub(crate) fn group_members(home: &Path, group: &str) -> Vec<String> {
    let blocked: std::collections::HashSet<String> =
        std::fs::read_to_string(home.join("blocked-sessions.txt"))
            .map(|s| {
                s.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
    let Ok(rd) = std::fs::read_dir(home.join("sessions")) else {
        return Vec::new();
    };
    let mut members = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("env") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        if blocked.contains(name) {
            continue;
        }
        let cfg = crate::config::parse_env_file(&path);
        let tags = cfg.get("CC_TAGS").map(String::as_str).unwrap_or("");
        if tags.split(',').map(str::trim).any(|t| !t.is_empty() && t == group) {
            members.push(name.to_string());
        }
    }
    members.sort();
    members
}

// ---- GET /api/history -------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct ListParams {
    #[serde(default)]
    limit: Option<String>,
    #[serde(default)]
    offset: Option<String>,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    counts: Option<String>,
    #[serde(default)]
    sessions: Option<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    q: Option<String>,
}

/// Python-truthy query flag: present with any non-empty value.
fn flag(v: &Option<String>) -> bool {
    v.as_deref().map(|s| !s.is_empty()).unwrap_or(false)
}

/// Default page, and the CEILING no caller can exceed (AF-213).
///
/// `limit` was unclamped: `?limit=100000` served all 8,920 rows at 19 MB, and
/// nothing in the endpoint, the client, or the logs would have said so. The
/// dashboard's own three consumers ask for 200, 200 and 500, so a 500 ceiling
/// changes no existing caller's result and makes the 19 MB response
/// unreachable — the fault is closed at the API rather than in whichever client
/// happened to ask politely.
const HISTORY_DEFAULT_LIMIT: i64 = 500;
const HISTORY_MAX_LIMIT: i64 = 500;

async fn list_history(State(state): State<AppState>, Query(p): Query<ListParams>) -> Response {
    let store = state.store.clone();
    // CLAMPED OUT HERE, and the request is told. A truncated list that says
    // nothing reads as data rather than as truncation — the same failure
    // `board_contract_filters.rs` records, where a lane auditing its own board
    // got 100 rows fleet-wide with nothing in the body saying so and was one
    // step from reporting "only 4 done cards exist". So an over-limit request
    // gets `X-Amux-Limit-Clamped` naming the ceiling, and a WARN, so the next
    // occurrence is visible in the logs without anyone going to look.
    let requested_limit: i64 = p
        .limit
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(HISTORY_DEFAULT_LIMIT);
    let limit = requested_limit.clamp(1, HISTORY_MAX_LIMIT);
    let was_clamped = requested_limit > HISTORY_MAX_LIMIT;
    if was_clamped {
        tracing::warn!(
            requested = requested_limit, served = limit,
            "GET /api/history limit clamped — a caller asked for a full-table read; \
             page with &offset= instead"
        );
    }
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let conn = store.read()?;
        let offset: i64 = p.offset.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
        let session = p.session.clone().unwrap_or_default();

        // ?sessions=1 — every session with ANY history, from the STORE, not
        // the loaded page (AMUX-2548: the dropdown must see the corpus).
        if flag(&p.sessions) {
            let mut stmt = conn.prepare(
                "SELECT session, COUNT(*) c FROM cmd_history \
                 WHERE session != '' GROUP BY session ORDER BY session",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(json!({ "session": r.get::<_, String>(0)?, "count": r.get::<_, i64>(1)? }))
            })?;
            return Ok(Value::Array(rows.flatten().collect()));
        }

        // ?counts=1 — true totals per kind (respecting ?session=), ignoring
        // limit, so the UI's chips never read 0 for an unloaded kind.
        if flag(&p.counts) {
            let mut out: Map<String, Value> =
                MSG_KINDS.iter().map(|k| (k.to_string(), json!(0))).collect();
            let mut count_row = |mtype: String, c: i64| {
                let k = msg_kind(&mtype);
                let n = out.get(k).and_then(Value::as_i64).unwrap_or(0);
                out.insert(k.to_string(), json!(n + c));
            };
            if !session.is_empty() {
                let mut stmt = conn.prepare(
                    "SELECT type, COUNT(*) c FROM cmd_history WHERE session=?1 GROUP BY type",
                )?;
                let rows =
                    stmt.query_map([&session], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                for (t, c) in rows.flatten() {
                    count_row(t, c);
                }
            } else {
                let mut stmt =
                    conn.prepare("SELECT type, COUNT(*) c FROM cmd_history GROUP BY type")?;
                let rows =
                    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                for (t, c) in rows.flatten() {
                    count_row(t, c);
                }
            }
            let all: i64 = MSG_KINDS.iter().map(|k| out[*k].as_i64().unwrap_or(0)).sum();
            out.insert("all".into(), json!(all));
            return Ok(Value::Object(out));
        }

        // The list window. Every predicate lands in SQL, before the LIMIT.
        let mut where_cl: Vec<String> = Vec::new();
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        if !session.is_empty() {
            where_cl.push("session=?".into());
            params.push(rusqlite::types::Value::Text(session.clone()));
        }
        let group = p.group.as_deref().unwrap_or("").trim().to_string();
        if !group.is_empty() && session.is_empty() {
            let members = group_members(&amux_home(), &group);
            if !members.is_empty() {
                where_cl.push(format!("session IN ({})", vec!["?"; members.len()].join(",")));
                for m in members {
                    params.push(rusqlite::types::Value::Text(m));
                }
            } else {
                // An empty group must return NOTHING, not everything — the
                // whole fleet's history under a group name is a wrong answer
                // that looks like a working feature.
                where_cl.push("1=0".into());
            }
        }
        let q = p.q.as_deref().unwrap_or("").trim().to_string();
        if !q.is_empty() {
            where_cl.push("text LIKE ? ESCAPE '\\'".into());
            let escaped = q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            params.push(rusqlite::types::Value::Text(format!("%{escaped}%")));
        }
        let want: Vec<String> = p
            .kind
            .as_deref()
            .unwrap_or("")
            .split(',')
            .map(|k| k.trim().to_lowercase())
            .filter(|k| MSG_KINDS.contains(&k.as_str()))
            .collect();
        if !want.is_empty() {
            let mut ors: Vec<String> = Vec::new();
            // THE FILTER IS THE CLASSIFIER WRITTEN A SECOND TIME, so it is
            // built from the SAME lists rather than restated (AMUX-3737). It
            // used to say `type NOT IN ('session','schedule','system')` for
            // `human` — the denylist, matching msg_kind's old fallback exactly,
            // which is the problem: the two agreed, and both were wrong. A
            // filter that reproduces a misclassification is worse than one that
            // drifts from it, because the badge and the filter corroborate each
            // other.
            let inlist = |types: &[&str], params: &mut Vec<rusqlite::types::Value>| {
                for t in types {
                    params.push(rusqlite::types::Value::Text((*t).to_string()));
                }
                format!(
                    "COALESCE(type,'') IN ({})",
                    types.iter().map(|_| "?").collect::<Vec<_>>().join(",")
                )
            };
            for k in &want {
                match k.as_str() {
                    "human" => ors.push(inlist(&HUMAN_TYPES, &mut params)),
                    "amux" => ors.push(inlist(&AMUX_TYPES, &mut params)),
                    "unstamped" => ors.push(inlist(&UNSTAMPED_TYPES, &mut params)),
                    // Anything this build does not classify. Selecting it is how
                    // you FIND the types nobody taught msg_kind about, which is
                    // the whole reason `unknown` exists as a kind.
                    "unknown" => {
                        let known: Vec<&str> = HUMAN_TYPES
                            .iter()
                            .chain(AMUX_TYPES.iter())
                            .chain(UNSTAMPED_TYPES.iter())
                            .chain(["session", "schedule"].iter())
                            .copied()
                            .collect();
                        ors.push(format!("NOT {}", inlist(&known, &mut params)));
                    }
                    other => {
                        ors.push("type=?".to_string());
                        params.push(rusqlite::types::Value::Text(other.to_string()));
                    }
                }
            }
            where_cl.push(format!("({})", ors.join(" OR ")));
        }
        let mut sql =
            String::from(
            // delivery/queued_at/delivered_at are migration 0014. They are
            // NULL on the 12.4k pre-existing rows and must reach the client AS
            // NULL — the UI distinguishes "not recorded" from "direct", and
            // coalescing here would assert a delivery path nobody observed.
            "SELECT id, text, type, session, ts, origin, card_id, \
             delivery, queued_at, delivered_at, submit_verdict FROM cmd_history",
        );
        if !where_cl.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&where_cl.join(" AND "));
        }
        sql.push_str(" ORDER BY ts DESC LIMIT ? OFFSET ?");
        params.push(rusqlite::types::Value::Integer(limit));
        params.push(rusqlite::types::Value::Integer(offset));
        let refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p as &dyn rusqlite::types::ToSql).collect();
        let mut rows = super::calendar::query_rows_json(&conn, &sql, &refs)?;
        for d in &mut rows {
            let mtype = d.get("type").and_then(Value::as_str).unwrap_or("").to_string();
            d["kind"] = json!(msg_kind(&mtype));
            d["queued"] = json!(msg_is_queued(&mtype));
            // `delivery` is the RECORDED fact; `queued` above is the inference
            // from `type`. Both are sent: the inference keeps every historical
            // row classifiable, the recorded value is authoritative when
            // present, and the client prefers it. Where they disagree on a NEW
            // row that is a contradiction worth seeing, not one to smooth over.
            if let Some(q) = d.get("queued_at").and_then(Value::as_i64) {
                if let Some(dl) = d.get("delivered_at").and_then(Value::as_i64) {
                    if dl > q {
                        d["queue_wait_ms"] = json!(dl - q);
                    }
                }
            }
        }
        Ok(Value::Array(rows))
    })
    .await;
    match joined {
        Ok(Ok(v)) => {
            let mut resp = Json(v).into_response();
            if was_clamped {
                if let Ok(hv) = HeaderValue::from_str(&limit.to_string()) {
                    resp.headers_mut().insert("x-amux-limit-clamped", hv);
                }
            }
            resp
        }
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- POST /api/history ------------------------------------------------------

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A JSON number as a Python-truthy integer (0/absent/non-number -> None).
fn js_int(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_i64)
        .or_else(|| v.and_then(Value::as_f64).map(|f| f as i64))
        .filter(|&t| t != 0)
}

/// Python `body.get("ts") or now_ms` — falsy (absent/null/0) means now.
fn ts_or_now(v: Option<&Value>) -> i64 {
    js_int(v).unwrap_or_else(now_ms)
}

async fn append_history(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let text = body.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if text.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "text required" }));
    }
    let htype = body.get("type").and_then(Value::as_str).unwrap_or("user").to_string();
    let session = body.get("session").and_then(Value::as_str).unwrap_or("").to_string();
    let ts = ts_or_now(body.get("ts"));
    let origin: String =
        body.get("origin").and_then(Value::as_str).unwrap_or("").chars().take(80).collect();
    let (text, hits) = redact_secrets(&text);
    if hits > 0 {
        tracing::info!(
            "[capture] {}: redacted {} suspected credential(s) from a pushed history row (AMUX-2525)",
            if session.is_empty() { "api" } else { &session },
            hits
        );
    }
    let slot: Arc<Mutex<i64>> = Arc::new(Mutex::new(0));
    let slot_w = slot.clone();
    let write = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO cmd_history (text, type, session, ts, origin) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![text, htype, session, ts, origin],
            )?;
            let id = conn.last_insert_rowid();
            *slot_w.lock().expect("slot") = id;
            Ok(WriteOutcome {
                applied: true,
                events: vec![ev(&id.to_string(), MutationKind::Created)],
            })
        })
        .await;
    match write {
        Ok(_) => {
            let id = *slot.lock().expect("slot");
            Json(json!({ "ok": true, "id": id })).into_response()
        }
        Err(e) => internal(e),
    }
}

// ---- POST /api/history/import ----------------------------------------------

async fn import_history(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let entries = body.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
    if entries.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "entries required" }));
    }
    let slot: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let slot_w = slot.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let mut count = 0usize;
            for e in &entries {
                let text = e.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
                if text.is_empty() {
                    continue;
                }
                let (text, _hits) = redact_secrets(&text);
                let htype = e.get("type").and_then(Value::as_str).unwrap_or("direct");
                let session = e.get("session").and_then(Value::as_str).unwrap_or("");
                // Python: e.get("time") or e.get("ts") or now — falsy chain.
                let ts = js_int(e.get("time"))
                    .or_else(|| js_int(e.get("ts")))
                    .unwrap_or_else(now_ms);
                conn.execute(
                    "INSERT INTO cmd_history (text, type, session, ts) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![text, htype, session, ts],
                )?;
                count += 1;
            }
            *slot_w.lock().expect("slot") = count;
            Ok(WriteOutcome {
                applied: count > 0,
                events: vec![ev("import", MutationKind::Created)],
            })
        })
        .await;
    match write {
        Ok(_) => {
            let count = *slot.lock().expect("slot");
            Json(json!({ "ok": true, "imported": count })).into_response()
        }
        Err(e) => internal(e),
    }
}

// ---- DELETE /api/history ----------------------------------------------------

async fn clear_history(State(state): State<AppState>) -> Response {
    let write = state
        .store
        .write_async(move |conn| {
            let n = conn.execute("DELETE FROM cmd_history", [])?;
            let events =
                if n > 0 { vec![ev("all", MutationKind::Deleted)] } else { vec![] };
            Ok(WriteOutcome { applied: n > 0, events })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// Tests — temp-DB stores; the group test pins AMUX_HOME to a temp dir under
// the shared env lock (settings::test_env), never the live ~/.amux.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn app() -> (axum::Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("history-test.db")).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        let router = Router::new().nest("/api/history", routes()).with_state(state);
        (router, dir)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let b = Request::builder().method(method).uri(path);
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
        let v = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, v)
    }

    async fn seed(app: &axum::Router) {
        for (text, htype, session, ts) in [
            ("hello from me", "direct", "alpha", 1000),
            ("queued steer", "steering", "alpha", 2000),
            ("session relay", "session", "beta", 3000),
            ("cron fire", "schedule", "beta", 4000),
            ("amux nudge", "system", "alpha", 5000),
        ] {
            let (st, _) = send(
                app,
                "POST",
                "/api/history",
                Some(json!({ "text": text, "type": htype, "session": session, "ts": ts })),
            )
            .await;
            assert_eq!(st, StatusCode::OK);
        }
    }

    /// The two rows the pre-AMUX-3737 fixture could not express.
    ///
    /// `seed` above holds one row per type the classifier already knew, so
    /// `kind=human` returned the same two rows under the old denylist and the
    /// new allowlist and the filter test was green across the bug's whole life.
    /// A fixture that cannot contain the defect cannot detect it.
    async fn seed_unclassified(app: &axum::Router) {
        for (text, htype, ts) in [
            // The specimen: an auto-pickup nudge, which read `Human`.
            ("[amux] you went idle holding RC-53", "pickup", 6000),
            // A type this build has never heard of, standing in for the next
            // one somebody adds.
            ("from the future", "some-new-type", 7000),
        ] {
            let (st, _) = send(
                app,
                "POST",
                "/api/history",
                Some(json!({ "text": text, "type": htype, "session": "alpha", "ts": ts })),
            )
            .await;
            assert_eq!(st, StatusCode::OK);
        }
    }

    /// The FILTER half of AMUX-3737, which is the classifier written a second
    /// time in SQL and therefore the half that can silently disagree with it.
    #[tokio::test]
    async fn the_kind_filter_agrees_with_the_classifier_about_machine_messages() {
        let (app, _dir) = app();
        seed(&app).await;
        seed_unclassified(&app).await;

        let kinds = |v: &Value| -> Vec<String> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|r| r["kind"].as_str().unwrap_or("").to_string())
                .collect()
        };

        // The badge. `pickup` must not wear `Human`.
        let (_, all) = send(&app, "GET", "/api/history", None).await;
        let by_text: std::collections::HashMap<&str, &str> = all
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["text"].as_str().unwrap(), r["kind"].as_str().unwrap()))
            .collect();
        assert_eq!(by_text["[amux] you went idle holding RC-53"], "amux");
        assert_eq!(by_text["from the future"], "unknown");

        // The filter must reach the same verdict. Selecting Human must not
        // return either of them.
        let (_, humans) = send(&app, "GET", "/api/history?kind=human", None).await;
        let texts: Vec<&str> =
            humans.as_array().unwrap().iter().map(|r| r["text"].as_str().unwrap()).collect();
        assert_eq!(
            texts,
            vec!["queued steer", "hello from me"],
            "an auto-pickup nudge and an unclassified row are not human traffic"
        );
        assert!(kinds(&humans).iter().all(|k| k == "human"));

        // And selecting amux must FIND the pickup, or the row is simply lost.
        let (_, amux) = send(&app, "GET", "/api/history?kind=amux", None).await;
        let texts: Vec<&str> =
            amux.as_array().unwrap().iter().map(|r| r["text"].as_str().unwrap()).collect();
        assert_eq!(texts, vec!["[amux] you went idle holding RC-53", "amux nudge"]);

        // `unknown` is selectable, which is how you find the types nobody
        // taught msg_kind about. A kind you cannot filter on is a kind nobody
        // will ever go looking for.
        let (_, unk) = send(&app, "GET", "/api/history?kind=unknown", None).await;
        let texts: Vec<&str> =
            unk.as_array().unwrap().iter().map(|r| r["text"].as_str().unwrap()).collect();
        assert_eq!(texts, vec!["from the future"]);

        // Every row lands in exactly one kind: the four buckets must partition
        // the table, or a message is invisible under every filter.
        let mut seen = 0usize;
        for k in ["human", "session", "schedule", "amux", "unknown"] {
            let (_, v) = send(&app, "GET", &format!("/api/history?kind={k}"), None).await;
            seen += v.as_array().unwrap().len();
        }
        assert_eq!(seen, 7, "5 seeded + 2 unclassified, each counted once");
    }

    /// AMUX-3737. Ethan: "youre confusing human vs session messages", over a
    /// screenshot of `[amux] You went idle holding RC-53...` wearing a blue
    /// `Human` badge.
    ///
    /// THIS TEST USED TO ASSERT THE BUG. Its last line was
    /// `assert_eq!(msg_kind("legacy-weirdness"), "human")`, with the comment
    /// "unknown provenance reads as human — the reading that gets looked at",
    /// and the module header recorded the same thing as a deliberate parity
    /// decision. The reasoning is about VISIBILITY and it is sound; the
    /// conclusion does not follow, because `human` is not the only visible
    /// bucket. Writing a default down is what makes it indistinguishable from a
    /// decision, and both the doc and the test then defended it.
    ///
    /// Measured on the live table before the change: 355 `pickup` rows from
    /// `origin: board-drive` reading Human, 4.0% of 8,993 messages.
    #[test]
    fn an_unrecognised_type_is_unknown_never_human() {
        for t in ["direct", "steering", "user", ""] {
            assert_eq!(msg_kind(t), "human", "{t:?} is typed by a person");
        }
        assert_eq!(msg_kind("SESSION "), "session", "trimmed and lowercased");
        assert_eq!(msg_kind("schedule"), "schedule");
        for t in ["system", "pickup"] {
            assert_eq!(msg_kind(t), "amux", "{t:?} is machine-authored");
        }
        // AMUX-2670's kind, made real. Not `human` (its delivery was never
        // verified) and not `amux` (a person probably did type it).
        assert_eq!(msg_kind("raw-tmux-fallback"), "unstamped");
        // THE SPECIMEN: the type on MSG-33250, the row in Ethan's screenshot.
        assert_eq!(msg_kind("pickup"), "amux", "an auto-pickup nudge is not a person");
        // THE SHAPE, which is what actually matters. Fixing only `pickup` would
        // pass every line above and leave the next new type reading Human in
        // silence.
        assert_eq!(
            msg_kind("some-type-invented-next-year"),
            "unknown",
            "an unrecognised type must announce that it is unrecognised"
        );
        assert!(msg_is_queued("steering"));
        assert!(!msg_is_queued("direct"));

        // The two lists the SQL filter is built from must stay disjoint, or a
        // type would match both `kind=human` and `kind=amux`.
        for h in HUMAN_TYPES {
            assert!(!AMUX_TYPES.contains(&h), "{h:?} cannot be both human and amux");
            assert!(!UNSTAMPED_TYPES.contains(&h), "{h:?} cannot be both human and unstamped");
        }
        for a in AMUX_TYPES {
            assert!(!UNSTAMPED_TYPES.contains(&a), "{a:?} cannot be both amux and unstamped");
        }
    }

    /// The sweep instrument that misled in both directions must now be unable to.
    ///
    /// Both historical misreadings were of the SAME column and pointed opposite
    /// ways, which is the tell that the column cannot answer the question at
    /// all: AF-159 concluded "delivered" from a non-NULL that was a copy of the
    /// insert time; a 2026-08-27 sweep nearly concluded "92 lost messages" from
    /// a NULL that only means nobody writes there.
    #[test]
    fn an_unstamped_column_is_never_read_as_a_delivery_verdict() {
        // The case that nearly produced a false finding: queued, cmd_history
        // silent, and the deliverer's own table says it landed in 2 seconds.
        let (v, src) = delivery_truth("queued", Some(Some(1_787_779_179.0)));
        assert_eq!(v, "delivered");
        assert!(src.contains("steering_history"), "must name the instrument that answered: {src}");

        // A row the deliverer HOLDS and has not stamped is real evidence of
        // non-delivery, and must not be flattened into the unknown case.
        assert_eq!(delivery_truth("queued", Some(None)).0, "not delivered");

        // NO ROW IS NOT A NEGATIVE. This is the assertion that stops the whole
        // class: absence of a lookup result is a fact about the lookup.
        let (v3, src3) = delivery_truth("queued", None);
        assert_eq!(v3, "unknown", "no steering row means we cannot tell, not that it failed");
        assert_ne!(v3, "not delivered");
        assert!(
            src3.contains("NOT stamped"),
            "the reader must be told WHY the obvious column cannot be used, or they will \
             use it: {src3}"
        );

        // The three inputs must not collapse into two outputs — if any pair
        // renders alike, the join has bought nothing over reading the column.
        let all = [
            delivery_truth("queued", Some(Some(1.0))).0,
            delivery_truth("queued", Some(None)).0,
            delivery_truth("queued", None).0,
        ];
        let mut uniq = all.to_vec();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), 3, "three input states must yield three verdicts: {all:?}");

        // A direct send is honestly answerable from cmd_history alone (AMUX-3541
        // kept `now_ms` there deliberately), so it must NOT be reported as
        // unknown merely because no steering row was looked up.
        let (v4, src4) = delivery_truth("direct", None);
        assert_eq!(v4, "delivered");
        assert!(src4.contains("cmd_history"), "and it must say which instrument: {src4}");
    }

    #[test]
    fn redaction_matches_python_families() {
        // The probe key is ASSEMBLED at runtime so the repo's secret
        // scanner never matches source (the CI self-test's own trick) —
        // a redaction test whose fixture trips the scanner can never land.
        let probe = format!("key sk-ant-{}03-abcdefghijklmnopqrstuvwx here", "api");
        let (out, hits) = redact_secrets(&probe);
        assert_eq!(hits, 1);
        assert!(!out.contains("sk-ant-api03"), "{out}");
        assert!(out.contains("[REDACTED-CREDENTIAL]"));
        let (out, hits) = redact_secrets("OPENAI_API_KEY=abcd1234efgh5678");
        assert_eq!(hits, 1, "{out}");
        let (out, hits) = redact_secrets("hello@amux.io (godmode) // qrP3LW7QPiUn4Hk");
        assert_eq!(hits, 1, "{out}");
        let (out, hits) = redact_secrets("no secrets in this friendly text");
        assert_eq!(hits, 0);
        assert_eq!(out, "no secrets in this friendly text");
    }

    #[tokio::test]
    async fn python_shaped_row_round_trips_column_by_column() {
        let (app, dir) = app();
        {
            let conn = rusqlite::Connection::open(dir.path().join("history-test.db")).unwrap();
            conn.execute(
                "INSERT INTO cmd_history (text, type, session, ts, origin, card_id) \
                 VALUES ('fix the parser', 'steering', 'mg', 1753000000123, 'orch', 'AMUX-9')",
                [],
            )
            .unwrap();
        }
        let (st, list) = send(&app, "GET", "/api/history", None).await;
        assert_eq!(st, StatusCode::OK, "{list}");
        let row = &list.as_array().unwrap()[0];
        assert_eq!(row["id"], json!(1));
        assert_eq!(row["text"], json!("fix the parser"));
        assert_eq!(row["type"], json!("steering"));
        assert_eq!(row["session"], json!("mg"));
        assert_eq!(row["ts"], json!(1753000000123i64));
        assert_eq!(row["origin"], json!("orch"));
        assert_eq!(row["card_id"], json!("AMUX-9"));
        assert_eq!(row["kind"], json!("human"), "steering displays as human");
        assert_eq!(row["queued"], json!(true), "steering is the queued delivery detail");
    }

    #[tokio::test]
    async fn filters_kinds_counts_sessions_pagination() {
        let (app, _dir) = app();
        seed(&app).await;

        // Full list: ts DESC.
        let (_, all) = send(&app, "GET", "/api/history", None).await;
        let texts: Vec<&str> =
            all.as_array().unwrap().iter().map(|r| r["text"].as_str().unwrap()).collect();
        assert_eq!(texts, vec!["amux nudge", "cron fire", "session relay", "queued steer", "hello from me"]);

        // kind=human excludes session/schedule/system.
        let (_, humans) = send(&app, "GET", "/api/history?kind=human", None).await;
        let texts: Vec<&str> =
            humans.as_array().unwrap().iter().map(|r| r["text"].as_str().unwrap()).collect();
        assert_eq!(texts, vec!["queued steer", "hello from me"]);
        // Comma-separated kinds OR together.
        let (_, some) = send(&app, "GET", "/api/history?kind=schedule,amux", None).await;
        assert_eq!(some.as_array().unwrap().len(), 2);
        // Unknown kinds are dropped from the filter (Python whitelist).
        let (_, all2) = send(&app, "GET", "/api/history?kind=bogus", None).await;
        assert_eq!(all2.as_array().unwrap().len(), 5);

        // session filter.
        let (_, alpha) = send(&app, "GET", "/api/history?session=alpha", None).await;
        assert_eq!(alpha.as_array().unwrap().len(), 3);

        // q searches text server-side.
        let (_, hits) = send(&app, "GET", "/api/history?q=steer", None).await;
        assert_eq!(hits.as_array().unwrap().len(), 1);
        assert_eq!(hits[0]["text"], json!("queued steer"));

        // limit/offset window.
        let (_, page) = send(&app, "GET", "/api/history?limit=2&offset=1", None).await;
        let texts: Vec<&str> =
            page.as_array().unwrap().iter().map(|r| r["text"].as_str().unwrap()).collect();
        assert_eq!(texts, vec!["cron fire", "session relay"]);

        // AF-213: `limit` IS CLAMPED, and an over-limit request is TOLD.
        //
        // The row count cannot discriminate here — the fixture has 5 rows and
        // the ceiling is 500, so "asked for 100000, got 5" holds just as well
        // against no clamp at all. The header is the only observable that
        // separates them, which is why the assertion is on the header and the
        // control below is a request that must NOT carry it.
        //
        // Measured on the live store before the clamp: ?limit=100000 served all
        // 8,920 rows, 19 MB, silently.
        {
            let over = axum::http::Request::builder()
                .method("GET")
                .uri("/api/history?limit=100000")
                .body(Body::empty())
                .unwrap();
            let res = app.clone().oneshot(over).await.unwrap();
            assert_eq!(
                res.headers().get("x-amux-limit-clamped").and_then(|v| v.to_str().ok()),
                Some("500"),
                "an over-limit request must be told the ceiling it was cut to — a silent \
                 truncation reads as data, not as truncation"
            );

            // CONTROL: a request inside the ceiling must NOT claim it was clamped.
            // Without this, a handler that stamps the header unconditionally passes.
            let ok = axum::http::Request::builder()
                .method("GET")
                .uri("/api/history?limit=2")
                .body(Body::empty())
                .unwrap();
            let res2 = app.clone().oneshot(ok).await.unwrap();
            assert!(
                res2.headers().get("x-amux-limit-clamped").is_none(),
                "a request within the ceiling must not be labelled clamped"
            );
        }

        // counts=1: true totals per kind + all, ignoring limit.
        let (_, counts) = send(&app, "GET", "/api/history?counts=1&limit=1", None).await;
        assert_eq!(counts["human"], json!(2));
        assert_eq!(counts["session"], json!(1));
        assert_eq!(counts["schedule"], json!(1));
        assert_eq!(counts["amux"], json!(1));
        assert_eq!(counts["all"], json!(5));
        // counts respects ?session=.
        let (_, counts) = send(&app, "GET", "/api/history?counts=1&session=alpha", None).await;
        assert_eq!(counts["human"], json!(2));
        assert_eq!(counts["amux"], json!(1));
        assert_eq!(counts["all"], json!(3));

        // sessions=1: dropdown derived from the STORE (AMUX-2548).
        let (_, sess) = send(&app, "GET", "/api/history?sessions=1", None).await;
        assert_eq!(
            sess,
            json!([{ "session": "alpha", "count": 3 }, { "session": "beta", "count": 2 }])
        );
    }

    #[tokio::test]
    async fn like_wildcards_in_q_are_escaped() {
        let (app, _dir) = app();
        for text in ["progress 100%", "plain text"] {
            send(&app, "POST", "/api/history", Some(json!({ "text": text }))).await;
        }
        // A literal % must not become a match-everything wildcard.
        let (_, hits) = send(&app, "GET", "/api/history?q=100%25", None).await;
        assert_eq!(hits.as_array().unwrap().len(), 1);
        assert_eq!(hits[0]["text"], json!("progress 100%"));
        let (_, hits) = send(&app, "GET", "/api/history?q=%25", None).await;
        assert_eq!(hits.as_array().unwrap().len(), 1, "bare %% matches only the literal");
    }

    #[tokio::test]
    async fn post_defaults_redaction_and_import() {
        let (app, _dir) = app();
        // text required.
        let (st, e) = send(&app, "POST", "/api/history", Some(json!({ "text": "  " }))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(e["error"], json!("text required"));

        // Defaults: type=user, session="", ts=now(ms), origin truncated to 80.
        let long_origin = "x".repeat(120);
        let (st, r) = send(
            &app,
            "POST",
            "/api/history",
            Some(json!({ "text": "password: hunter2hunter2", "origin": long_origin })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r["ok"], json!(true));
        assert_eq!(r["id"], json!(1));
        let (_, list) = send(&app, "GET", "/api/history", None).await;
        let row = &list.as_array().unwrap()[0];
        assert_eq!(row["type"], json!("user"));
        assert_eq!(row["session"], json!(""));
        assert_eq!(row["origin"].as_str().unwrap().len(), 80);
        assert!(row["ts"].as_i64().unwrap() > 1_700_000_000_000, "ts is milliseconds");
        assert!(row["text"].as_str().unwrap().contains("[REDACTED-CREDENTIAL]"),
                "credential paste redacted on the way in: {}", row["text"]);

        // Import: entries required; empty texts skipped; defaults type=direct.
        let (st, e) = send(&app, "POST", "/api/history/import", Some(json!({ "entries": [] }))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(e["error"], json!("entries required"));
        let (st, r) = send(
            &app,
            "POST",
            "/api/history/import",
            Some(json!({ "entries": [
                { "text": "old one", "time": 111 },
                { "text": "", "ts": 222 },
                { "text": "new one", "ts": 333, "type": "session", "session": "mg" }
            ] })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r, json!({ "ok": true, "imported": 2 }));
        let (_, list) = send(&app, "GET", "/api/history?q=one", None).await;
        let rows = list.as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["ts"], json!(111), "`time` wins over `ts`");
        assert_eq!(rows[1]["type"], json!("direct"));
        assert_eq!(rows[0]["type"], json!("session"));

        // DELETE clears everything.
        let (st, r) = send(&app, "DELETE", "/api/history", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r, json!({ "ok": true }));
        let (_, list) = send(&app, "GET", "/api/history", None).await;
        assert_eq!(list.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn group_filter_resolves_tags_and_never_leaks_the_fleet() {
        let home = tempfile::tempdir().unwrap();
        let sessions = home.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("alpha.env"), "CC_TAGS=sales,us\n").unwrap();
        std::fs::write(sessions.join("beta.env"), "CC_TAGS=eng\n").unwrap();
        std::fs::write(sessions.join("gamma.env"), "CC_TAGS=sales\n").unwrap();
        std::fs::write(home.path().join("blocked-sessions.txt"), "gamma\n").unwrap();
        let _env = crate::api::settings::test_env::set_home(home.path());

        // Helper level: members resolved from CC_TAGS, blocked excluded.
        assert_eq!(group_members(home.path(), "sales"), vec!["alpha"]);
        assert_eq!(group_members(home.path(), "eng"), vec!["beta"]);
        assert!(group_members(home.path(), "nope").is_empty());

        let (app, _dir) = app();
        seed(&app).await;
        let (_, sales) = send(&app, "GET", "/api/history?group=sales", None).await;
        assert_eq!(sales.as_array().unwrap().len(), 3, "alpha's rows only");
        // An unknown group returns NOTHING — not the whole fleet.
        let (_, none) = send(&app, "GET", "/api/history?group=marketing", None).await;
        assert_eq!(none.as_array().unwrap().len(), 0);
        // ?session= wins over ?group= (Python: group applies only without session).
        let (_, beta) = send(&app, "GET", "/api/history?group=sales&session=beta", None).await;
        assert_eq!(beta.as_array().unwrap().len(), 2);
    }
}
