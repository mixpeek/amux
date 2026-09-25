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
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/",
            get(list_history).post(append_history).delete(clear_history),
        )
        .route("/import", axum::routing::post(import_history))
        // AMUX-4664: ask a question of these messages. A literal POST, like
        // `/import`, so the `/{id}` capture below does not take it.
        .route("/ask", axum::routing::post(super::history_ask::ask))
        // Look up ONE message by its id. `/import` is a literal POST above, so
        // this GET capture never swallows it.
        .route("/{id}", get(get_history_item))
        .route("/{id}/card", axum::routing::put(link_card))
}

/// Attach every non-deleted task in the message card's durable epic lineage.
///
/// `cmd_history.card_id` predates decomposition and can name only one task. A
/// source prompt can later become an epic plus several child cards, though,
/// and each child deliberately inherits that same source message. Returning
/// only the old scalar made card -> message work while message -> card lost all
/// but the original epic (TUBES-2474 / TUBES-2501). Keep the scalar for API
/// compatibility and add the complete lineage as `linked_cards`.
///
/// This is one batched query for the whole history page, not one query per
/// message. The CTE also resolves a scalar that already names a child back to
/// its epic root, so both old and new writers produce the same answer.
/// The lineage query for `n` message cards. One function so the handler and the
/// query-plan test run the same SQL (AMUX-4590). Its `linked.epic = root` arm
/// relies on idx_issues_epic (migration 0071); without that index SQLite scans
/// every issue once per card, which cost 9.8 s on a 500-row page.
fn linked_cards_sql(n: usize) -> String {
    let placeholders = (0..n).map(|_| "(?)").collect::<Vec<_>>().join(",");
    format!(
        "WITH message_cards(card_id) AS (VALUES {placeholders}), \
         roots(card_id, root_id) AS ( \
           SELECT mc.card_id, COALESCE(NULLIF(source.epic,''), mc.card_id) \
           FROM message_cards mc LEFT JOIN issues source ON source.id=mc.card_id \
         ) \
         SELECT roots.card_id, linked.id, linked.title, linked.status, \
                COALESCE(linked.archived,0), linked.session, roots.root_id \
         FROM roots JOIN issues linked \
           ON linked.id=roots.root_id OR linked.epic=roots.root_id \
         WHERE COALESCE(linked.deleted,0)=0 \
         ORDER BY roots.card_id, CASE WHEN linked.id=roots.root_id THEN 0 ELSE 1 END, linked.id"
    )
}

fn attach_linked_cards(conn: &rusqlite::Connection, rows: &mut [Value]) -> rusqlite::Result<()> {
    let card_ids: BTreeSet<String> = rows
        .iter()
        .filter_map(|row| row.get("card_id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(String::from)
        .collect();
    if card_ids.is_empty() {
        for row in rows {
            row["linked_cards"] = json!([]);
        }
        return Ok(());
    }

    let sql = linked_cards_sql(card_ids.len());
    let values: Vec<rusqlite::types::Value> = card_ids
        .iter()
        .cloned()
        .map(rusqlite::types::Value::Text)
        .collect();
    let refs: Vec<&dyn rusqlite::types::ToSql> = values
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();
    let mut linked_by_card: HashMap<String, Vec<Value>> = HashMap::new();
    let mut stmt = conn.prepare(&sql)?;
    let linked = stmt.query_map(refs.as_slice(), |r| {
        let message_card: String = r.get(0)?;
        let card = json!({
            "id": r.get::<_, String>(1)?,
            "title": r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            "status": r.get::<_, Option<String>>(3)?.unwrap_or_else(|| "todo".into()),
            "archived": r.get::<_, i64>(4)? != 0,
            "session": r.get::<_, Option<String>>(5)?.unwrap_or_default(),
            "lineage_root": r.get::<_, String>(6)?,
        });
        Ok((message_card, card))
    })?;
    for result in linked {
        let (message_card, card) = result?;
        linked_by_card.entry(message_card).or_default().push(card);
    }
    for row in rows {
        let cards = row
            .get("card_id")
            .and_then(Value::as_str)
            .and_then(|id| linked_by_card.get(id))
            .cloned()
            .unwrap_or_default();
        row["linked_cards"] = Value::Array(cards);
    }
    Ok(())
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
    let raw = id
        .trim()
        .trim_start_matches("MSG-")
        .trim_start_matches("msg-")
        .trim();
    let Ok(nid) = raw.parse::<i64>() else {
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": format!("'{id}' is not a message id — expected MSG-<number> or a number") }),
        );
    };
    let store = state.store.clone();
    let joined = crate::db::interactions::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let conn = store.read()?;
        let sql = "SELECT id, text, type, session, ts, origin, card_id, \
                   delivery, queued_at, delivered_at, submit_verdict, capture_pending, queue_id, \
                   (SELECT title FROM issues WHERE issues.id=cmd_history.card_id) AS card_title, \
                   (SELECT status FROM issues WHERE issues.id=cmd_history.card_id) AS card_status, \
                   (SELECT archived FROM issues WHERE issues.id=cmd_history.card_id) AS card_archived, \
                   (SELECT deleted FROM issues WHERE issues.id=cmd_history.card_id) AS card_deleted \
                   FROM cmd_history WHERE id=?1";
        let refs: Vec<&dyn rusqlite::types::ToSql> = vec![&nid];
        let mut rows = super::calendar::query_rows_json(&conn, sql, &refs)?;
        if let Some(d) = rows.first_mut() {
            let mtype = d.get("type").and_then(Value::as_str).unwrap_or("").to_string();
            d["kind"] = json!(msg_kind(&mtype));
            // The RECORDED delivery wins over the type inference: an owner's
            // queued message is type=user, delivery=queued (MSG-68866 read
            // queued:false while it sat in the queue).
            let rec_delivery = d.get("delivery").and_then(Value::as_str).unwrap_or("").to_string();
            d["queued"] = json!(msg_is_queued(&mtype) || rec_delivery == "queued");
            // THE QUEUE ID IS EXACT. When the message was linked to its queue
            // row, answer from that row: still in steering_queue means waiting,
            // in steering_history means delivered then. The time-window join
            // below is only for rows with no link.
            let qid = d.get("queue_id").and_then(Value::as_str).unwrap_or("").to_string();
            let mut exact = false;
            if !qid.is_empty() {
                let waiting: bool = conn
                    .query_row("SELECT 1 FROM steering_queue WHERE id=?1", [&qid], |_| Ok(true))
                    .unwrap_or(false);
                let hist: Option<Option<f64>> = conn
                    .query_row("SELECT delivered_at FROM steering_history WHERE id=?1", [&qid], |r| r.get(0))
                    .ok();
                if waiting {
                    d["delivered"] = json!("waiting in queue");
                    d["delivered_source"] = json!(format!("steering_queue row {qid} — held until the worker's next idle point"));
                    d["queued"] = json!(true);
                } else if let Some(Some(t)) = hist {
                    d["delivered"] = json!("delivered");
                    d["delivered_source"] = json!(format!("steering_history row {qid} — stamped by the deliverer"));
                    d["delivered_at_actual"] = json!((t * 1000.0) as i64);
                    d["queued"] = json!(false);
                }
                // Answered exactly; the time-window guess below is skipped.
                exact = waiting || matches!(hist, Some(Some(_)));
            }
            if !exact {
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
            let submit_verdict = d.get("submit_verdict").and_then(Value::as_str);
            let (verdict, source) = delivery_truth(&delivery, submit_verdict, steering);
            if delivery == "direct" && verdict != "delivered" {
                tracing::warn!(message_id = nid, ?submit_verdict, delivered = verdict,
                    measured = true, n_considered = 1,
                    "history_delivery_not_confirmed: direct record does not prove submission");
            }
            d["delivered"] = json!(verdict);
            d["delivered_source"] = json!(source);
            if let Some(Some(t)) = steering {
                d["delivered_at_actual"] = json!((t * 1000.0) as i64);
            }
            }
        }
        attach_linked_cards(&conn, &mut rows)?;
        Ok(rows.into_iter().next())
    })
    .await;
    match joined {
        Ok(Ok(Some(row))) => Json(row).into_response(),
        Ok(Ok(None)) => err(
            StatusCode::NOT_FOUND,
            json!({ "error": format!("MSG-{nid} not found") }),
        ),
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
fn delivery_truth(
    cmd_delivery: &str,
    submit_verdict: Option<&str>,
    steering: Option<Option<f64>>,
) -> (&'static str, &'static str) {
    // Direct is a transport choice. The same durable record can say its
    // submission stuck, was unverified, or has no outcome evidence at all.
    if cmd_delivery == "direct" {
        return match submit_verdict {
            Some("confirmed" | "retried") => ("delivered", "cmd_history.submit_verdict — submission confirmed"),
            Some("stuck") => ("not delivered", "cmd_history.submit_verdict — submission remained stuck"),
            _ => ("unknown", "cmd_history.submit_verdict — submission is not confirmed; direct alone is not evidence"),
        };
    }
    match steering {
        Some(Some(_)) => (
            "delivered",
            "steering_history — stamped by the deliverer when it landed",
        ),
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

const MSG_KINDS: [&str; 6] = [
    "human",
    "session",
    "schedule",
    "amux",
    "unstamped",
    "unknown",
];

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
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if blocked.contains(name) {
            continue;
        }
        let cfg = crate::config::parse_env_file(&path);
        let tags = cfg.get("CC_TAGS").map(String::as_str).unwrap_or("");
        if tags
            .split(',')
            .map(str::trim)
            .any(|t| !t.is_empty() && t == group)
        {
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
    /// Context filters (AMUX-4695), read out of `client_meta`.
    ///
    /// SERVER-SIDE, beside `kind`, for the reason AMUX-4666 already paid for: a
    /// client-side filter runs over ONE PAGE and then reports "1 message" when
    /// the real population is 207. Every predicate here lands in SQL before the
    /// LIMIT, and the `x-amux-total` count is built from the SAME WHERE and the
    /// SAME params, so the number beside the list is a count of the filtered
    /// population rather than of the page.
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    place: Option<String>,
}

/// Python-truthy query flag: present with any non-empty value.
fn flag(v: &Option<String>) -> bool {
    v.as_deref().map(|s| !s.is_empty()).unwrap_or(false)
}

/// The UI's durable display id for a command-history row. A prefixed query is
/// an identity lookup, not prose search: clicking `MSG-123` must still find the
/// row when neither its body nor its worker happens to contain "MSG-123".
fn prefixed_message_id(raw: &str) -> Option<i64> {
    raw.trim()
        .strip_prefix("MSG-")
        .or_else(|| raw.trim().strip_prefix("msg-"))
        .and_then(|n| n.parse::<i64>().ok())
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
            requested = requested_limit,
            served = limit,
            "GET /api/history limit clamped — a caller asked for a full-table read; \
             page with &offset= instead"
        );
    }
    let joined =
        crate::db::interactions::spawn_blocking(move || -> anyhow::Result<(Value, Option<i64>)> {
            let conn = store.read()?;
            let offset: i64 = p
                .offset
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
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
                return Ok((Value::Array(rows.flatten().collect()), None));
            }

            // ?counts=1 — true totals per kind (respecting ?session=), ignoring
            // limit, so the UI's chips never read 0 for an unloaded kind.
            if flag(&p.counts) {
                let mut out: Map<String, Value> = MSG_KINDS
                    .iter()
                    .map(|k| (k.to_string(), json!(0)))
                    .collect();
                let mut count_row = |mtype: String, c: i64| {
                    let k = msg_kind(&mtype);
                    let n = out.get(k).and_then(Value::as_i64).unwrap_or(0);
                    out.insert(k.to_string(), json!(n + c));
                };
                if !session.is_empty() {
                    let mut stmt = conn.prepare(
                        "SELECT type, COUNT(*) c FROM cmd_history WHERE session=?1 GROUP BY type",
                    )?;
                    let rows = stmt.query_map([&session], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                    })?;
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
                let all: i64 = MSG_KINDS
                    .iter()
                    .map(|k| out[*k].as_i64().unwrap_or(0))
                    .sum();
                out.insert("all".into(), json!(all));

                // CONTEXT FACETS (AMUX-4695): which devices and places actually
                // occur, with their counts, scoped by the same session filter.
                //
                // DERIVED FROM THE DATA, NEVER A FIXED LIST. The UI builds its
                // filter options from this, so it can only ever offer a value that
                // selects at least one message. A hardcoded menu would offer
                // "Place" on a fleet where no message has ever carried one, which
                // is a control that looks broken to whoever clicks it, and it would
                // go stale the day a new device appears.
                //
                // The key is OMITTED when nothing has that field, rather than sent
                // as an empty object, so the client's "is there anything to filter
                // by" test is the presence of the key. Same rule as the row
                // renderer: an absence is not a value.
                for field in ["device", "place"] {
                    let mut sql = format!(
                    "SELECT json_extract(client_meta,'$.{field}') v, COUNT(*) c FROM cmd_history \
                     WHERE json_extract(client_meta,'$.{field}') IS NOT NULL"
                );
                    if !session.is_empty() {
                        sql.push_str(" AND session=?1");
                    }
                    sql.push_str(" GROUP BY v ORDER BY c DESC, v ASC");
                    let mut stmt = conn.prepare(&sql)?;
                    let map_row =
                        |r: &rusqlite::Row| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?));
                    let rows: Vec<(String, i64)> = if session.is_empty() {
                        stmt.query_map([], map_row)?.flatten().collect()
                    } else {
                        stmt.query_map([&session], map_row)?.flatten().collect()
                    };
                    if !rows.is_empty() {
                        let facet: Map<String, Value> =
                            rows.into_iter().map(|(v, c)| (v, json!(c))).collect();
                        out.insert(format!("{field}s"), Value::Object(facet));
                    }
                }
                return Ok((Value::Object(out), None));
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
                    where_cl.push(format!(
                        "session IN ({})",
                        vec!["?"; members.len()].join(",")
                    ));
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
                if let Some(message_id) = prefixed_message_id(&q) {
                    where_cl.push("id=?".into());
                    params.push(rusqlite::types::Value::Integer(message_id));
                } else {
                    where_cl.push("text LIKE ? ESCAPE '\\'".into());
                    let escaped = q
                        .replace('\\', "\\\\")
                        .replace('%', "\\%")
                        .replace('_', "\\_");
                    params.push(rusqlite::types::Value::Text(format!("%{escaped}%")));
                }
            }
            // CONTEXT FILTERS (AMUX-4695). `client_meta` is JSON text, so the
            // predicate is a json_extract, which this database already relies on in
            // 24 other places.
            //
            // AN EXACT MATCH, not a LIKE. "Mac" must not also select a device
            // called "Mac mini", because the filter's whole job is to answer "which
            // of these" and a substring match silently merges two answers into one.
            //
            // A filter for a value nothing has returns NOTHING, deliberately. No
            // row currently carries `$.place` (measured: 0 of 11,591), so
            // `?place=Office` is an empty list today rather than an error or a
            // silent no-op. An empty list is the truthful answer to "messages sent
            // from the office" when the place was never recorded; ignoring the
            // parameter would answer a different question than the one asked.
            for (field, raw) in [
                ("device", p.device.as_deref()),
                ("place", p.place.as_deref()),
            ] {
                let v = raw.unwrap_or("").trim();
                if v.is_empty() {
                    continue;
                }
                where_cl.push(format!("json_extract(client_meta,'$.{field}')=?"));
                params.push(rusqlite::types::Value::Text(v.to_string()));
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
            let mut sql = String::from(
                // delivery/queued_at/delivered_at are migration 0014. They are
                // NULL on the 12.4k pre-existing rows and must reach the client AS
                // NULL — the UI distinguishes "not recorded" from "direct", and
                // coalescing here would assert a delivery path nobody observed.
                "SELECT id, text, type, session, ts, origin, card_id, \
             delivery, queued_at, delivered_at, submit_verdict, capture_pending, client_meta, \
             (SELECT title FROM issues WHERE issues.id=cmd_history.card_id) AS card_title, \
             (SELECT status FROM issues WHERE issues.id=cmd_history.card_id) AS card_status, \
             (SELECT archived FROM issues WHERE issues.id=cmd_history.card_id) AS card_archived, \
             (SELECT deleted FROM issues WHERE issues.id=cmd_history.card_id) AS card_deleted \
             FROM cmd_history",
            );
            if !where_cl.is_empty() {
                sql.push_str(" WHERE ");
                sql.push_str(&where_cl.join(" AND "));
            }
            // AMUX-4666: the size of the population this page came from, counted
            // with the same WHERE and the same params the rows use. Counting with
            // anything else is the trap this codebase already records: a number
            // that measures the query rather than the thing.
            let mut count_sql = String::from("SELECT COUNT(*) FROM cmd_history");
            if !where_cl.is_empty() {
                count_sql.push_str(" WHERE ");
                count_sql.push_str(&where_cl.join(" AND "));
            }
            let total: i64 = {
                let refs: Vec<&dyn rusqlite::types::ToSql> = params
                    .iter()
                    .map(|p| p as &dyn rusqlite::types::ToSql)
                    .collect();
                conn.query_row(&count_sql, refs.as_slice(), |r| r.get(0))?
            };

            sql.push_str(" ORDER BY ts DESC LIMIT ? OFFSET ?");
            params.push(rusqlite::types::Value::Integer(limit));
            params.push(rusqlite::types::Value::Integer(offset));
            let refs: Vec<&dyn rusqlite::types::ToSql> = params
                .iter()
                .map(|p| p as &dyn rusqlite::types::ToSql)
                .collect();
            let mut rows = super::calendar::query_rows_json(&conn, &sql, &refs)?;
            for d in &mut rows {
                let mtype = d
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                d["kind"] = json!(msg_kind(&mtype));
                let rec = d.get("delivery").and_then(Value::as_str).unwrap_or("") == "queued";
                d["queued"] = json!(msg_is_queued(&mtype) || rec);
                // `delivery` is the RECORDED fact; `queued` above is the inference
                // from `type`. Both are sent: the inference keeps every historical
                // row classifiable, the recorded value is authoritative when
                // present, and the client prefers it. Where they disagree on a NEW
                // row that is a contradiction worth seeing, not one to smooth over.
                // `client_meta` is stored as a JSON STRING. Hand the client a parsed
                // object, and when there is nothing to hand over REMOVE THE KEY.
                //
                // Absence is not a value here (AMUX-4694). 11,526 of 11,591 rows
                // predate the capture and every client that sends none will add
                // more, so this is the common case, not the edge. A `null` would
                // reach the renderer as a present-but-empty field and tempt a
                // placeholder chip; a missing key cannot. `d.get("client_meta")` is
                // then falsy in JS for exactly one reason.
                let parsed = d
                    .get("client_meta")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .filter(Value::is_object);
                match parsed {
                    Some(obj) => d["client_meta"] = obj,
                    None => {
                        if let Some(map) = d.as_object_mut() {
                            map.remove("client_meta");
                        }
                    }
                }
                if let Some(q) = d.get("queued_at").and_then(Value::as_i64) {
                    if let Some(dl) = d.get("delivered_at").and_then(Value::as_i64) {
                        if dl > q {
                            d["queue_wait_ms"] = json!(dl - q);
                        }
                    }
                }
            }
            attach_linked_cards(&conn, &mut rows)?;
            Ok((Value::Array(rows), Some(total)))
        })
        .await;
    match joined {
        Ok(Ok((v, total))) => {
            let mut resp = Json(v).into_response();
            // The body is a bare array, so the total rides beside it. A pager
            // that guessed the count from a short page would show the wrong
            // number of pages on every filter.
            if let Some(total) = total {
                if let Ok(hv) = HeaderValue::from_str(&total.to_string()) {
                    resp.headers_mut().insert("x-amux-total", hv);
                }
            }
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

/// Attribute one reviewed original message to an existing card without
/// delivering a command or claiming historical work as newly active.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageCardLink {
    session: String,
    card_id: String,
    reason: String,
}

async fn link_card(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<MessageCardLink>,
) -> Response {
    use crate::db::board_store as bs;
    use rusqlite::OptionalExtension;
    let raw = id
        .trim()
        .strip_prefix("MSG-")
        .or_else(|| id.trim().strip_prefix("msg-"))
        .unwrap_or(id.trim());
    let Some(nid) = raw.parse::<i64>().ok().filter(|id| *id > 0) else {
        return err(
            StatusCode::BAD_REQUEST,
            json!({"error":"expected a positive message ID or MSG-<id>"}),
        );
    };
    let session = body.session.trim().to_owned();
    let card_id = body.card_id.trim().to_owned();
    let reason = body.reason.trim().to_owned();
    if session.is_empty()
        || card_id.is_empty()
        || reason.is_empty()
        || reason.chars().count() > 2000
    {
        return err(
            StatusCode::BAD_REQUEST,
            json!({"error":"session, card_id and a nonempty reason (at most 2000 characters) are required"}),
        );
    }
    let actor = super::request_log::caller_from_headers(&headers);
    let (expected_session, target, why, who) = (
        session.clone(),
        card_id.clone(),
        reason.clone(),
        actor.clone(),
    );
    let reply = Arc::new(Mutex::new(None));
    let reply_w = reply.clone();
    let result=state.store.write_async(move |conn| {
        let source=conn.query_row("SELECT session,card_id FROM cmd_history WHERE id=?1",[nid],
            |r| Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?))).optional()?;
        let mut events=vec![];
        let (status,value)=match source {
            None => (StatusCode::NOT_FOUND,json!({"error":"message not found","message_id":nid})),
            Some((actual,_)) if actual!=expected_session =>
                (StatusCode::CONFLICT,json!({"error":"message session does not match the reviewed request","message_id":nid})),
            Some((_,Some(existing))) if existing!=target =>
                (StatusCode::CONFLICT,json!({"error":"message already has a different card; reconcile that existing work first","message_id":nid,"card_id":existing})),
            Some((_,Some(existing))) =>
                (StatusCode::OK,json!({"message_id":nid,"card_id":existing,"changed":false,"applied":false,"command_resent":false})),
            Some((_,None)) => match bs::get_issue(conn,&target)? {
                None => (StatusCode::NOT_FOUND,json!({"error":"card not found","card_id":target})),
                Some(card) if card.archived!=0 =>
                    (StatusCode::CONFLICT,json!({"error":"card must be unarchived; reconcile archived work through the board first","card_id":target})),
                Some(mut card) => {
                    conn.execute("UPDATE cmd_history SET card_id=?1,capture_pending=0 WHERE id=?2 AND card_id IS NULL",rusqlite::params![target,nid])?;
                    card.log=Some(bs::append_log(card.log.as_deref(),&chrono::Local::now().format("%H:%M").to_string(),
                        &format!("Original MSG-{nid} linked by {}: {why}",if who.is_empty() {"api"} else {&who})));
                    card.updated=chrono::Utc::now().timestamp(); card.rev+=1; card.version+=1;
                    bs::save_patched(conn,&mut card)?;
                    events.push(ev(&nid.to_string(),MutationKind::Updated));
                    events.push(PendingEvent {entity_type:EntityType::Task,entity_id:card.id.clone(),mutation:MutationKind::Updated,payload:Some(card.snapshot())});
                    (StatusCode::OK,json!({"message_id":nid,"card_id":card.id,"changed":true,"applied":true,"command_resent":false}))
                }
            }
        };
        *reply_w.lock().expect("message link reply")=Some((status,value));
        Ok(WriteOutcome {applied:!events.is_empty(),events})
    }).await;
    match result {
        Ok(_) => {
            let (status, value) = reply
                .lock()
                .expect("message link reply")
                .take()
                .expect("message link result");
            tracing::info!(message_id=nid, %session, %card_id, %actor, %reason, status=status.as_u16(),
                measured=true, n_considered=1, verdict="explicit_message_card_link",
                "reviewed original message attribution handled without delivery or task status change");
            (status, Json(value)).into_response()
        }
        Err(error) => {
            tracing::warn!(message_id=nid, %card_id, %error, measured=false, n_considered=0,
                verdict="message_card_link_failed", "original message attribution transaction failed");
            internal(error)
        }
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
    let text = body
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "text required" }));
    }
    let htype = body
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("user")
        .to_string();
    let session = body
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let ts = ts_or_now(body.get("ts"));
    let origin: String = body
        .get("origin")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(80)
        .collect();
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
    let entries = body
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": "entries required" }),
        );
    }
    let slot: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let slot_w = slot.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let mut count = 0usize;
            for e in &entries {
                let text = e
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
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
            let events = if n > 0 {
                vec![ev("all", MutationKind::Deleted)]
            } else {
                vec![]
            };
            Ok(WriteOutcome {
                applied: n > 0,
                events,
            })
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
        let (router, _, dir) = app_with_state();
        (router, dir)
    }

    fn app_with_state() -> (axum::Router, AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("history-test.db")).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let router = Router::new()
            .nest("/api/history", routes())
            .with_state(state.clone());
        (router, state, dir)
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
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, v)
    }

    fn capture_log() -> (
        Arc<Mutex<Vec<u8>>>,
        tracing::subscriber::DefaultGuard,
        tracing::Dispatch,
    ) {
        #[derive(Clone)]
        struct LogBytes(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for LogBytes {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = LogBytes(bytes.clone());
        // Avoid tracing-core's single-dispatch first-use cache across parallel tests.
        let _registration_peer =
            tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let scope = tracing::subscriber::set_default(subscriber);
        (bytes, scope, _registration_peer)
    }

    #[tokio::test]
    async fn explicit_card_link_preserves_history_and_does_not_claim_or_resend() {
        let (app, state, _dir) = app_with_state();
        state.store.write(|conn| {
            super::super::session_verbs::ensure_fleet_tables(conn)?;
            conn.execute("INSERT INTO cmd_history(id,text,type,session,ts,delivery,delivered_at,submit_verdict) VALUES (1,'Implement missing parser validation','user','old-capture',1000,'direct',1000,'confirmed')",[])?;
            conn.execute("INSERT INTO issues(id,title,desc,status,session,creator,created,updated,owner_type,type) VALUES ('FIX-1','Parser validation','Reviewed original work','backlog','existing-owner','test',1,1,'agent','code')",[])?;
            Ok(WriteOutcome {applied:true,events:vec![]})
        }).unwrap();
        let request = json!({"session":"old-capture","card_id":"FIX-1","reason":"Reviewed original unlinked assignment"});
        let (status, body) = send(
            &app,
            "PUT",
            "/api/history/MSG-1/card",
            Some(request.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["changed"], true);
        assert_eq!(body["command_resent"], false);
        let (status, body) = send(&app, "PUT", "/api/history/1/card", Some(request)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["changed"], false);
        let (_, message) = send(&app, "GET", "/api/history/MSG-1", None).await;
        assert_eq!(message["card_id"], "FIX-1");
        assert_eq!(message["text"], "Implement missing parser validation");
        assert_eq!(message["ts"], 1000);
        assert_eq!(message["capture_pending"], 0);
        let conn = state.store.read().unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM cmd_history", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM issues", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let (status, desc, log): (String, String, String) = conn
            .query_row(
                "SELECT status,desc,log FROM issues WHERE id='FIX-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "backlog");
        assert_eq!(
            conn.query_row("SELECT session FROM issues WHERE id='FIX-1'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "existing-owner",
            "linking source context must not reassign work"
        );
        assert_eq!(desc, "Reviewed original work");
        assert_eq!(log.matches("Original MSG-1 linked").count(), 1);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM steering_queue", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM session_events WHERE type='task.claimed'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn explicit_card_link_refuses_mistargeting_and_rolls_back_on_failure() {
        let (app, state, _dir) = app_with_state();
        let (bytes, _scope, _peer) = capture_log();
        state.store.write(|conn| {
            conn.execute("INSERT INTO cmd_history(id,text,type,session,ts) VALUES (1,'original context','user','source-lane',1000)",[])?;
            for (id,session,archived) in [("FIX-1","source-lane",0),("FIX-2","source-lane",0),("OLD-1","source-lane",1)] {
                conn.execute("INSERT INTO issues(id,title,desc,status,session,creator,created,updated,owner_type,type,archived) VALUES (?1,'Reviewed work','Scope unchanged','todo',?2,'test',1,1,'agent','code',?3)",rusqlite::params![id,session,archived])?;
            }
            Ok(WriteOutcome {applied:true,events:vec![]})
        }).unwrap();
        for (path, session, card, reason, status) in [
            (
                "/api/history/999/card",
                "source-lane",
                "FIX-1",
                "reviewed",
                StatusCode::NOT_FOUND,
            ),
            (
                "/api/history/nope/card",
                "source-lane",
                "FIX-1",
                "reviewed",
                StatusCode::BAD_REQUEST,
            ),
            (
                "/api/history/1/card",
                "wrong-lane",
                "FIX-1",
                "reviewed",
                StatusCode::CONFLICT,
            ),
            (
                "/api/history/1/card",
                "source-lane",
                "OLD-1",
                "reviewed",
                StatusCode::CONFLICT,
            ),
            (
                "/api/history/1/card",
                "source-lane",
                "ABSENT-1",
                "reviewed",
                StatusCode::NOT_FOUND,
            ),
            (
                "/api/history/1/card",
                "source-lane",
                "FIX-1",
                "",
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let (got, body) = send(
                &app,
                "PUT",
                path,
                Some(json!({"session":session,"card_id":card,"reason":reason})),
            )
            .await;
            assert_eq!(got, status, "{path} {card}: {body}");
            assert_eq!(
                state
                    .store
                    .read()
                    .unwrap()
                    .query_row("SELECT card_id FROM cmd_history WHERE id=1", [], |r| r
                        .get::<_, Option<
                        String,
                    >>(
                        0
                    ))
                    .unwrap(),
                None
            );
        }
        state.store.write(|conn| {
            conn.execute_batch("CREATE TRIGGER refuse_link_audit BEFORE UPDATE ON issues BEGIN SELECT RAISE(ABORT,'fixture audit write unavailable'); END;")?;
            Ok(WriteOutcome {applied:true,events:vec![]})
        }).unwrap();
        let request = json!({"session":"source-lane","card_id":"FIX-1","reason":"Review complete"});
        let (status, body) = send(&app, "PUT", "/api/history/1/card", Some(request.clone())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        let failure = log
            .lines()
            .find(|line| line.contains("message_card_link_failed"))
            .expect("failed transaction must announce itself in amux logs");
        assert!(
            failure.contains("message_id=1")
                && failure.contains("card_id=FIX-1")
                && failure.contains("measured=false"),
            "{failure}"
        );
        assert_eq!(
            state
                .store
                .read()
                .unwrap()
                .query_row("SELECT card_id FROM cmd_history WHERE id=1", [], |r| r
                    .get::<_, Option<
                    String,
                >>(
                    0
                ))
                .unwrap(),
            None,
            "card link must roll back with its audit write"
        );
        state
            .store
            .write(|conn| {
                conn.execute_batch("DROP TRIGGER refuse_link_audit")?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .unwrap();
        assert_eq!(
            send(&app, "PUT", "/api/history/1/card", Some(request))
                .await
                .0,
            StatusCode::OK
        );
        let (status,body)=send(&app,"PUT","/api/history/1/card",Some(json!({"session":"source-lane","card_id":"FIX-2","reason":"Try overwriting original attribution"}))).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(
            state
                .store
                .read()
                .unwrap()
                .query_row("SELECT card_id FROM cmd_history WHERE id=1", [], |r| r
                    .get::<_, String>(
                    0
                ))
                .unwrap(),
            "FIX-1"
        );
        let (_, row) = send(&app, "GET", "/api/history/1", None).await;
        assert_eq!(row["card_id"], "FIX-1");
    }

    #[tokio::test]
    async fn explicit_card_link_has_known_receipts_through_the_full_router() {
        let (_, state, _dir) = app_with_state();
        let (bytes, _scope, _peer) = capture_log();
        state.store.write(|conn| {
            conn.execute("INSERT INTO cmd_history(id,text,type,session,ts) VALUES (1,'reviewed original','user','source-lane',1000)", [])?;
            conn.execute("INSERT INTO issues(id,title,desc,status,session,creator,created,updated,owner_type,type) VALUES ('LINK-1','Existing work','Scope unchanged','todo','existing-owner','test',1,1,'agent','code')", [])?;
            Ok(WriteOutcome {applied:true,events:vec![]})
        }).unwrap();
        let app = crate::api::router(state.clone());
        for (id, target, expected_status, expected_phase) in [
            ("link-first", "LINK-1", StatusCode::OK, "applied"),
            ("link-repeat", "LINK-1", StatusCode::OK, "noop"),
            ("link-refused", "OTHER-1", StatusCode::CONFLICT, "refused"),
        ] {
            let request = axum::http::Request::builder().method("PUT").uri("/api/history/MSG-1/card")
                .header("content-type", "application/json").header("x-amux-interaction-id", id)
                .body(Body::from(json!({"session":"source-lane","card_id":target,"reason":"Reviewed original request"}).to_string())).unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), expected_status);
            assert_eq!(response.headers()["x-amux-interaction-id"], id);
            let (status, receipt) =
                send(&app, "GET", &format!("/api/interactions/{id}"), None).await;
            assert_eq!(status, StatusCode::OK, "{receipt}");
            assert_eq!(
                receipt["phase"], expected_phase,
                "the full middleware must classify the actual endpoint response: {receipt}"
            );
            assert_eq!(receipt["measured"], true, "{receipt}");
        }
        let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(
            !log.lines().any(|line| line.contains("interaction_outcome")
                && (line.contains("link-first") || line.contains("link-repeat"))),
            "successful calls must not emit unknown-outcome warnings: {log}"
        );
        assert!(
            log.lines().any(|line| line.contains("interaction_outcome")
                && line.contains("link-refused")
                && line.contains("refused")),
            "positive control: the real refusal must reach the same collector: {log}"
        );
    }

    /// Rows carrying `client_meta`, written through the store because
    /// `POST /api/history` has no field for it: the capture path that sets it
    /// is the session send, not the history append. Seeding through the real
    /// column is the point, since the filter reads it with json_extract.
    async fn seed_context(state: &AppState) {
        for (text, session, ts, meta) in [
            (
                "from the mac",
                "ctx",
                6000i64,
                r#"{"device":"Mac","platform":"MacIntel","app_ver":"0.9.971","tz":"America/New_York"}"#,
            ),
            (
                "from the mac again",
                "ctx",
                7000,
                r#"{"device":"Mac","platform":"MacIntel","app_ver":"0.9.971","tz":"America/New_York"}"#,
            ),
            (
                "from the phone",
                "other",
                8000,
                r#"{"device":"iPhone","platform":"iPhone","app_ver":"0.9.971","tz":"America/New_York"}"#,
            ),
            (
                "from the mac mini",
                "other",
                9000,
                r#"{"device":"Mac mini"}"#,
            ),
        ] {
            let (t, se, m) = (text.to_string(), session.to_string(), meta.to_string());
            state
                .store
                .write_async(move |conn| {
                    conn.execute(
                        "INSERT INTO cmd_history (text, type, session, ts, origin, client_meta) \
                         VALUES (?1, 'direct', ?2, ?3, '', ?4)",
                        rusqlite::params![t, se, ts, m],
                    )?;
                    Ok(crate::db::WriteOutcome {
                        applied: true,
                        events: vec![],
                    })
                })
                .await
                .unwrap();
        }
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
        let texts: Vec<&str> = humans
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["text"].as_str().unwrap())
            .collect();
        assert_eq!(
            texts,
            vec!["queued steer", "hello from me"],
            "an auto-pickup nudge and an unclassified row are not human traffic"
        );
        assert!(kinds(&humans).iter().all(|k| k == "human"));

        // And selecting amux must FIND the pickup, or the row is simply lost.
        let (_, amux) = send(&app, "GET", "/api/history?kind=amux", None).await;
        let texts: Vec<&str> = amux
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["text"].as_str().unwrap())
            .collect();
        assert_eq!(
            texts,
            vec!["[amux] you went idle holding RC-53", "amux nudge"]
        );

        // `unknown` is selectable, which is how you find the types nobody
        // taught msg_kind about. A kind you cannot filter on is a kind nobody
        // will ever go looking for.
        let (_, unk) = send(&app, "GET", "/api/history?kind=unknown", None).await;
        let texts: Vec<&str> = unk
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["text"].as_str().unwrap())
            .collect();
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
        assert_eq!(
            msg_kind("pickup"),
            "amux",
            "an auto-pickup nudge is not a person"
        );
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
            assert!(
                !AMUX_TYPES.contains(&h),
                "{h:?} cannot be both human and amux"
            );
            assert!(
                !UNSTAMPED_TYPES.contains(&h),
                "{h:?} cannot be both human and unstamped"
            );
        }
        for a in AMUX_TYPES {
            assert!(
                !UNSTAMPED_TYPES.contains(&a),
                "{a:?} cannot be both amux and unstamped"
            );
        }
    }

    #[tokio::test]
    async fn direct_history_delivery_obeys_recorded_submission_verdict() {
        let (app, state, _dir) = app_with_state();
        state.store.write_async(|conn| {
            for (id, verdict) in [(1, Some("confirmed")), (2, Some("stuck")),
                (3, Some("retried")), (4, Some("unverified")), (5, None),
                (6, Some("future-state"))] {
                conn.execute("INSERT INTO cmd_history(id,text,type,session,ts,delivery,submit_verdict) VALUES(?1,'fixture','user','direct-verdict-fixture',1000,'direct',?2)", rusqlite::params![id,verdict])?;
            }
            Ok(WriteOutcome { applied: true, events: vec![] })
        }).await.unwrap();
        for (id, expected) in [
            (1, "delivered"),
            (2, "not delivered"),
            (3, "delivered"),
            (4, "unknown"),
            (5, "unknown"),
            (6, "unknown"),
        ] {
            let (status, value) = send(&app, "GET", &format!("/api/history/{id}"), None).await;
            assert_eq!(status, StatusCode::OK, "{value}");
            assert_eq!(
                value["delivered"], expected,
                "actual history endpoint row {id}: {value}"
            );
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
        let (v, src) = delivery_truth("queued", None, Some(Some(1_787_779_179.0)));
        assert_eq!(v, "delivered");
        assert!(
            src.contains("steering_history"),
            "must name the instrument that answered: {src}"
        );

        // A row the deliverer HOLDS and has not stamped is real evidence of
        // non-delivery, and must not be flattened into the unknown case.
        assert_eq!(
            delivery_truth("queued", None, Some(None)).0,
            "not delivered"
        );

        // NO ROW IS NOT A NEGATIVE. This is the assertion that stops the whole
        // class: absence of a lookup result is a fact about the lookup.
        let (v3, src3) = delivery_truth("queued", None, None);
        assert_eq!(
            v3, "unknown",
            "no steering row means we cannot tell, not that it failed"
        );
        assert_ne!(v3, "not delivered");
        assert!(
            src3.contains("NOT stamped"),
            "the reader must be told WHY the obvious column cannot be used, or they will \
             use it: {src3}"
        );

        // The three inputs must not collapse into two outputs — if any pair
        // renders alike, the join has bought nothing over reading the column.
        let all = [
            delivery_truth("queued", None, Some(Some(1.0))).0,
            delivery_truth("queued", None, Some(None)).0,
            delivery_truth("queued", None, None).0,
        ];
        let mut uniq = all.to_vec();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(
            uniq.len(),
            3,
            "three input states must yield three verdicts: {all:?}"
        );

        // A confirmed direct submission is answerable from its durable verdict,
        // without requiring an unrelated steering-history record.
        let (v4, src4) = delivery_truth("direct", Some("confirmed"), None);
        assert_eq!(v4, "delivered");
        assert!(
            src4.contains("cmd_history"),
            "and it must say which instrument: {src4}"
        );
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

    /// MSG-68866: an owner message queued behind a long turn read
    /// `queued:false, delivered:unknown` while its queue row sat waiting.
    #[tokio::test]
    async fn a_queued_owner_message_reads_as_waiting_then_delivered() {
        let (app, dir) = app();
        let db = dir.path().join("history-test.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "INSERT INTO cmd_history (id,text,type,session,ts,delivery,queue_id)
                 VALUES (7,'isabella signed up','user','ops',1790280995000,'queued','steer-1790280995138');
                 INSERT INTO steering_queue (id,session,text,queued_at)
                 VALUES ('steer-1790280995138','ops','isabella signed up',1790280995.1);",
            )
            .unwrap();
        }
        let (st, row) = send(&app, "GET", "/api/history/MSG-7", None).await;
        assert_eq!(st, StatusCode::OK, "{row}");
        assert_eq!(row["queued"], json!(true), "{row}");
        assert_eq!(row["delivered"], json!("waiting in queue"), "{row}");
        let (_, list) = send(&app, "GET", "/api/history", None).await;
        let lrow = list.as_array().unwrap().iter().find(|r| r["id"] == json!(7)).unwrap();
        assert_eq!(lrow["queued"], json!(true), "the list agrees: {lrow}");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "DELETE FROM steering_queue WHERE id='steer-1790280995138';
                 INSERT INTO steering_history (id,session,text,queued_at,delivered_at)
                 VALUES ('steer-1790280995138','ops','isabella signed up',1790280995.1,1790284000.5);",
            )
            .unwrap();
        }
        let (_, row) = send(&app, "GET", "/api/history/MSG-7", None).await;
        assert_eq!(row["delivered"], json!("delivered"), "{row}");
        assert_eq!(row["delivered_at_actual"], json!(1790284000500i64), "{row}");
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
            conn.execute(
                "INSERT INTO issues (id, title, desc, status, created, updated, archived, epic) \
                 VALUES ('AMUX-9', 'Fix parser', '', 'done', 1, 1, 0, NULL)",
                [],
            )
            .unwrap();
            conn.execute_batch(
                "INSERT INTO issues
                   (id,title,desc,status,created,updated,archived,epic,deleted)
                 VALUES
                   ('AMUX-10','Live child','', 'doing',1,1,0,'AMUX-9',0),
                   ('AMUX-11','Deleted child','', 'todo',1,1,0,'AMUX-9',1),
                   ('AMUX-12','Archived child','', 'verified',1,1,1,'AMUX-9',0),
                   ('AMUX-20','Empty epic root','', 'backlog',1,1,0,'',0);
                 INSERT INTO cmd_history (text,type,session,ts,origin,card_id)
                 VALUES ('standalone source','direct','mg',1,'orch','AMUX-20');",
            )
            .unwrap();
        }
        let (st, list) = send(&app, "GET", "/api/history", None).await;
        assert_eq!(st, StatusCode::OK, "{list}");
        let row = list
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["card_id"] == json!("AMUX-9"))
            .unwrap();
        assert_eq!(row["id"], json!(1));
        assert_eq!(row["text"], json!("fix the parser"));
        assert_eq!(row["type"], json!("steering"));
        assert_eq!(row["session"], json!("mg"));
        assert_eq!(row["ts"], json!(1753000000123i64));
        assert_eq!(row["origin"], json!("orch"));
        assert_eq!(row["card_id"], json!("AMUX-9"));
        assert_eq!(row["card_title"], json!("Fix parser"));
        assert_eq!(row["card_status"], json!("done"));
        assert_eq!(row["card_archived"], json!(0));
        assert!(row["card_deleted"].is_null());
        assert_eq!(row["kind"], json!("human"), "steering displays as human");
        assert_eq!(
            row["queued"],
            json!(true),
            "steering is the queued delivery detail"
        );
        let linked = row["linked_cards"].as_array().unwrap();
        assert_eq!(
            linked
                .iter()
                .map(|c| c["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["AMUX-9", "AMUX-10", "AMUX-12"],
            "the root and both live children are returned; deleted=1 is excluded"
        );
        assert_eq!(
            linked[1]["archived"],
            json!(false),
            "deleted=0 is a live card"
        );
        assert_eq!(
            linked[2]["archived"],
            json!(true),
            "archived lineage stays navigable"
        );
        assert!(linked.iter().all(|c| c["id"] != json!("AMUX-11")));

        let standalone = list
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["card_id"] == json!("AMUX-20"))
            .unwrap();
        assert_eq!(
            standalone["linked_cards"],
            json!([{
                "id": "AMUX-20", "title": "Empty epic root", "status": "backlog",
                "archived": false, "session": "", "lineage_root": "AMUX-20"
            }]),
            "both NULL and empty epic values resolve the source card as the lineage root"
        );

        let (st, one) = send(&app, "GET", "/api/history/MSG-1", None).await;
        assert_eq!(st, StatusCode::OK, "{one}");
        assert_eq!(one["card_title"], json!("Fix parser"));
        assert_eq!(one["card_status"], json!("done"));
        assert_eq!(one["card_archived"], json!(0));
        assert_eq!(one["linked_cards"], row["linked_cards"]);
    }

    /// AMUX-4695: filter the Messages list by the context a message was sent
    /// from, SERVER-SIDE.
    ///
    /// The card names the reason: a client-side filter runs over one page and
    /// then reports "1 message" when the population is 207. So the total is
    /// asserted here beside the rows, because a correct list with a wrong count
    /// is the exact failure AMUX-4666 already paid for once.
    #[tokio::test]
    async fn the_context_filter_selects_server_side_and_the_total_follows_it() {
        let (app, state, _dir) = app_with_state();
        seed(&app).await;
        seed_context(&state).await;

        let texts = |v: &Value| -> Vec<String> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|r| r["text"].as_str().unwrap_or("").to_string())
                .collect()
        };
        let total_for = |app: axum::Router, uri: String| async move {
            let req = axum::http::Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            let res = app.oneshot(req).await.unwrap();
            res.headers()
                .get("x-amux-total")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };

        let (_, mac) = send(&app, "GET", "/api/history?device=Mac", None).await;
        assert_eq!(texts(&mac), vec!["from the mac again", "from the mac"]);
        let (_, iph) = send(&app, "GET", "/api/history?device=iPhone", None).await;
        assert_eq!(texts(&iph), vec!["from the phone"]);

        // THE COUNT IS OF THE FILTERED POPULATION, not of the page. With
        // limit=1 the page holds one row and the total must still say two.
        assert_eq!(
            total_for(app.clone(), "/api/history?device=Mac&limit=1".into()).await,
            Some("2".into())
        );
        let (_, one) = send(&app, "GET", "/api/history?device=Mac&limit=1", None).await;
        assert_eq!(one.as_array().unwrap().len(), 1, "the page really is short");

        // A message with NO metadata is under no device, which is the whole
        // point: 11,526 of 11,591 real rows are in this state, so a filter that
        // swept them into some default bucket would be wrong about almost
        // everything.
        for uri in ["/api/history?device=Mac", "/api/history?device=iPhone"] {
            let (_, v) = send(&app, "GET", uri, None).await;
            assert!(
                !texts(&v).iter().any(|t| t == "hello from me"),
                "{uri} must not return a message that carries no metadata"
            );
        }

        // A VALUE NOTHING HAS RETURNS NOTHING. No row carries `$.place`, so
        // this is an empty list rather than an ignored parameter. Ignoring it
        // would answer a different question than the one asked.
        let (st, none) = send(&app, "GET", "/api/history?place=Office", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(none.as_array().unwrap().len(), 0);
        assert_eq!(
            total_for(app.clone(), "/api/history?place=Office".into()).await,
            Some("0".into())
        );

        // EXACT MATCH, not a prefix: "Mac" must not also select "Mac mini", or
        // the filter silently merges two answers into one.
        let (_, mini) = send(&app, "GET", "/api/history?device=Mac%20mini", None).await;
        assert_eq!(texts(&mini), vec!["from the mac mini"]);
    }

    /// The filter's options come from the DATA, so the UI can only offer a
    /// value that selects something.
    #[tokio::test]
    async fn context_facets_report_what_exists_and_omit_what_does_not() {
        let (app, state, _dir) = app_with_state();
        seed(&app).await;
        seed_context(&state).await;

        let (_, counts) = send(&app, "GET", "/api/history?counts=1", None).await;
        assert_eq!(counts["devices"]["Mac"], json!(2));
        assert_eq!(counts["devices"]["iPhone"], json!(1));
        assert_eq!(counts["devices"]["Mac mini"], json!(1));

        // THE KEY IS ABSENT, not an empty object. Nothing carries a place, and
        // a `places: {}` would render as a filter control with no options.
        assert!(
            counts.get("places").is_none(),
            "a facet nothing has must be omitted, not sent empty: {counts}"
        );

        // Scoped by session, like the kind counts beside it.
        let (_, scoped) = send(&app, "GET", "/api/history?counts=1&session=ctx", None).await;
        assert_eq!(scoped["devices"]["Mac"], json!(2));
        assert!(
            scoped["devices"].get("iPhone").is_none(),
            "the iPhone row belongs to another session: {scoped}"
        );
    }

    /// AMUX-4666: paging by PAGE NUMBER needs a page count, and a page count is
    /// only right if it counts the population the page came from. A total that
    /// ignored the active filter would show pages that do not exist, and the
    /// last page would come back empty.
    #[tokio::test]
    async fn the_page_total_counts_the_same_population_the_page_came_from() {
        let (app, _dir) = app();
        seed(&app).await;

        let total_for = |app: axum::Router, uri: &'static str| async move {
            let req = axum::http::Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            let res = app.oneshot(req).await.unwrap();
            res.headers()
                .get("x-amux-total")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };

        // A short page still reports the whole population.
        assert_eq!(
            total_for(app.clone(), "/api/history?limit=2").await,
            Some("5".into())
        );
        // ...and every filter moves it, because it is the SAME predicate.
        assert_eq!(
            total_for(app.clone(), "/api/history?kind=human&limit=1").await,
            Some("2".into())
        );
        assert_eq!(
            total_for(app.clone(), "/api/history?session=alpha&limit=1").await,
            Some("3".into())
        );
        assert_eq!(
            total_for(app.clone(), "/api/history?q=steer&limit=1").await,
            Some("1".into())
        );
        // A page past the end is empty and still says how many exist, so a
        // pager can send the reader back rather than showing a blank list.
        let (_, past) = send(&app, "GET", "/api/history?limit=2&offset=99", None).await;
        assert_eq!(past.as_array().unwrap().len(), 0);
        assert_eq!(
            total_for(app.clone(), "/api/history?limit=2&offset=99").await,
            Some("5".into())
        );

        // CONTROL: the answers that are not pages do not claim a page total.
        assert_eq!(total_for(app.clone(), "/api/history?counts=1").await, None);
        assert_eq!(
            total_for(app.clone(), "/api/history?sessions=1").await,
            None
        );
    }

    #[tokio::test]
    async fn filters_kinds_counts_sessions_pagination() {
        let (app, _dir) = app();
        seed(&app).await;

        // Full list: ts DESC.
        let (_, all) = send(&app, "GET", "/api/history", None).await;
        let texts: Vec<&str> = all
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["text"].as_str().unwrap())
            .collect();
        assert_eq!(
            texts,
            vec![
                "amux nudge",
                "cron fire",
                "session relay",
                "queued steer",
                "hello from me"
            ]
        );

        // kind=human excludes session/schedule/system.
        let (_, humans) = send(&app, "GET", "/api/history?kind=human", None).await;
        let texts: Vec<&str> = humans
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["text"].as_str().unwrap())
            .collect();
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

        // A displayed message id is an exact, stable identity. It must not be
        // treated as prose (the body does not contain its own database id).
        let (_, by_id) = send(&app, "GET", "/api/history?q=MSG-2", None).await;
        assert_eq!(by_id.as_array().unwrap().len(), 1);
        assert_eq!(by_id[0]["id"], json!(2));
        assert_eq!(by_id[0]["text"], json!("queued steer"));

        // limit/offset window.
        let (_, page) = send(&app, "GET", "/api/history?limit=2&offset=1", None).await;
        let texts: Vec<&str> = page
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["text"].as_str().unwrap())
            .collect();
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
                res.headers()
                    .get("x-amux-limit-clamped")
                    .and_then(|v| v.to_str().ok()),
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
        assert_eq!(
            hits.as_array().unwrap().len(),
            1,
            "bare %% matches only the literal"
        );
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
        assert!(
            row["ts"].as_i64().unwrap() > 1_700_000_000_000,
            "ts is milliseconds"
        );
        assert!(
            row["text"]
                .as_str()
                .unwrap()
                .contains("[REDACTED-CREDENTIAL]"),
            "credential paste redacted on the way in: {}",
            row["text"]
        );

        // Import: entries required; empty texts skipped; defaults type=direct.
        let (st, e) = send(
            &app,
            "POST",
            "/api/history/import",
            Some(json!({ "entries": [] })),
        )
        .await;
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

    /// AMUX-4590. The linked-cards join must look children up through
    /// idx_issues_epic. Without it the plan scans every issue once per message
    /// card, which cost 9.8 s on a 500-row page and 30 to 99 s on Ethan's phone.
    #[tokio::test]
    async fn linked_cards_lineage_uses_the_epic_index_instead_of_scanning_issues() {
        let (_app, state, _dir) = app_with_state();
        let conn = state.store.read().unwrap();
        let sql = linked_cards_sql(3);
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let plan: Vec<String> = stmt
            .query_map(["A-1", "A-2", "A-3"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            plan.iter().any(|d| d.contains("idx_issues_epic")),
            "the epic arm must use idx_issues_epic: {plan:#?}"
        );
        assert!(
            !plan
                .iter()
                .any(|d| d.trim_start().starts_with("SCAN linked")),
            "no full scan of issues per message card: {plan:#?}"
        );
    }
}
