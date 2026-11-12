//! Named clearance signals (AMUX-5237).
//!
//! A card can wait on a NAMED signal, and any lane can raise that signal with
//! one call. Raising it records who raised it and when, clears the wait on
//! every card parked on that name, and wakes each owning lane through the
//! normal steering queue with a short message naming the signal, the raiser
//! and the note.
//!
//! WHY THIS EXISTS. Two measured stalls in the 2026-09-26 fleet review
//! (docs/fleet-stall-review-2026-09-26.md, finding 4):
//!
//! - gs-3-bucket-objects waited on mvs-infra's "30-minute clearance" by
//!   regex-polling mvs-infra's peek history, and the regex fired a false
//!   positive. A pane is prose and a clearance is a fact; scraping one for the
//!   other has no correct implementation.
//! - mixpeek-frustrations held ~145 cards for a peer approval that
//!   mixpeek-general had already given, in prose, in a message nothing parsed.
//!
//! Both are the same shape: a lane that genuinely cannot proceed until another
//! actor does something, and no durable place for "it is done" to land.
//!
//! THE WAIT IS STORED IN `blocked_on`, AS `signal:<name>`. No new column. The
//! field already means "this card is waiting for something", and every
//! consumer that matters already honours it: board-drive pickup and backlog
//! drain skip it, `ready` excludes it, a blocked `doing` card is parked to
//! backlog so it stops holding WIP. A second field would have needed every one
//! of those consumers taught about it, and the one that was missed would have
//! dispatched a waiting card. The only consumer that must treat it differently
//! is blocker recovery, which exists to talk a lane OUT of free-text peer
//! waits; a named signal has its own clearing mechanism, so recovery skips it
//! (see `board_drive::blocker_recoveries_with_policy`).
//!
//! THE WAIT IS ALWAYS ON THE WAITER'S OWN CARD. Workers must not create
//! cross-worker `depends_on` edges (docs/worker-owned-board-contract.md), and
//! this does not reopen that door: a worker may park only its own card on a
//! signal (enforced in `board::patch_item`). The raiser names a SIGNAL, never a
//! card, so raising cannot touch any card nobody chose to park on that name.
//!
//! A signal is for an event another actor produces (a clearance, an approval,
//! an external window closing). It is not a way to wait on missing
//! implementation you could build yourself; the board contract still says to
//! own that.

use super::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::db::board_store as bs;
use crate::db::{PendingEvent, WriteOutcome};
use amux_core::revision::{EntityType, MutationKind};

/// The `blocked_on` prefix that marks a named signal wait.
pub(crate) const SIGNAL_PREFIX: &str = "signal:";

/// Kill switch for the wake message (worker > group > global, process env
/// wins). Default ON. Scoped to the RECIPIENT lane: the owner of a lane that
/// should not be woken turns it off there. Clearing the wait is not gated:
/// that is the meaning of raising the signal, and a wait that a raise cannot
/// clear would leave the card parked forever.
pub(crate) const WAKE_KEY: &str = "AMUX_SIGNAL_WAKE";

/// The steering guard wake messages carry. Non-empty on purpose: it makes the
/// wake an AUTOMATED producer, so the steering chokepoint refuses it into a
/// paused or isolated lane exactly as it refuses every other automation.
pub(crate) const WAKE_GUARD: &str = "signal-wake";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_signals))
        .route("/{name}", get(get_signal).post(raise_signal))
}

pub fn subscription_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_subscriptions).post(create_subscription))
        .route(
            "/{id}",
            get(get_subscription)
                .patch(update_subscription)
                .delete(delete_subscription),
        )
        .route("/emit", axum::routing::post(emit_custom_signal))
        .route("/fires", get(list_fires))
        .route("/catalog", get(event_catalog))
        .route("/events", get(recent_events))
}

/// A signal name: 1-64 chars of `[a-z0-9._-]`, starting with a letter or
/// digit. Lowercase only, so `MVS-Reflip-Clear` and `mvs-reflip-clear` cannot
/// be two signals that each think the other was never raised.
pub(crate) fn valid_signal_name(name: &str) -> bool {
    let n = name.len();
    (1..=64).contains(&n)
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

/// Normalise a caller-supplied name: trimmed and lowercased. Validation runs
/// on the result, so the canonical form is the only one ever stored.
pub(crate) fn normalize_signal_name(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

/// What a `blocked_on` value says about signals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SignalWait {
    /// Ordinary free-text block (or empty): not a signal wait.
    NotASignal,
    /// Starts with `signal:` but the name is unusable. Refused at write time,
    /// because a malformed name is a wait no raise could ever clear.
    Invalid(String),
    /// A well-formed wait on this canonical name.
    Valid(String),
}

pub(crate) fn parse_signal_wait(blocked_on: &str) -> SignalWait {
    let t = blocked_on.trim();
    let Some(rest) = t
        .get(..SIGNAL_PREFIX.len())
        .filter(|p| p.eq_ignore_ascii_case(SIGNAL_PREFIX))
        .map(|_| &t[SIGNAL_PREFIX.len()..])
    else {
        return SignalWait::NotASignal;
    };
    let name = normalize_signal_name(rest);
    if valid_signal_name(&name) {
        SignalWait::Valid(name)
    } else {
        SignalWait::Invalid(name)
    }
}

/// The signal a card is waiting on, if its `blocked_on` is a valid signal wait.
pub(crate) fn signal_wait_name(blocked_on: Option<&str>) -> Option<String> {
    match parse_signal_wait(blocked_on.unwrap_or("")) {
        SignalWait::Valid(name) => Some(name),
        _ => None,
    }
}

/// The own-card rule, pure. `caller` is the verified worker lane ("" for the
/// owner or a verified local member, who may park any card). A worker may
/// park only a card on its own board.
pub(crate) fn wait_owner_refusal(caller: &str, card_session: Option<&str>) -> Option<Value> {
    if caller.is_empty() || card_session == Some(caller) {
        return None;
    }
    Some(json!({
        "error": "a worker may wait on a signal only on its OWN card",
        "code": "signal_wait_not_own_card",
        "caller": caller,
        "card_owner": card_session,
        "how_to_fix": "park the card on your own board that needs the signal (amux signal wait <your-card> <name>). \
                       Waiting on another worker's card is the cross-worker dependency edge the board contract forbids.",
    }))
}

/// The wake message a lane receives. Short on purpose: it has to be read
/// mid-flight by a lane that was parked, and it names the three facts that
/// lane needs (which signal, who, what they said) plus the cards it frees.
pub(crate) fn wake_message(
    signal: &str,
    raiser: &str,
    raised_at_iso: &str,
    note: &str,
    cards: &[(String, String)],
) -> String {
    let who = if raiser.is_empty() {
        "the owner"
    } else {
        raiser
    };
    let note_line = if note.trim().is_empty() {
        "No note was given.".to_string()
    } else {
        format!("Note: {}", note.trim())
    };
    let list = cards
        .iter()
        .map(|(id, title)| format!("- {id}: {title}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "[amux signal] `{signal}` was raised by {who} at {raised_at_iso}.\n{note_line}\n\n\
         The wait on {n} of your card(s) is cleared (blocked_on emptied):\n{list}\n\n\
         Pick the work back up (amux board doing <ID>). If this raise is not the event you were \
         waiting for, park the card again with amux signal wait <ID> {signal}.",
        n = cards.len(),
    )
}

/// Resolve [`WAKE_KEY`] for one recipient lane. Same ladder as
/// `board_drive::dispatch_backlog_when_idle_in`: process env wins, then
/// worker > group > global scope files, default ON.
pub(crate) fn wake_enabled_in(home: &std::path::Path, lane: &str, process: Option<&str>) -> bool {
    fn is_off(v: &str) -> bool {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    }
    if let Some(v) = process.filter(|v| !v.trim().is_empty()) {
        return !is_off(v);
    }
    crate::api::session_verbs::scoped_setting_in(home, lane, WAKE_KEY)
        .as_deref()
        .map(|v| !is_off(v))
        .unwrap_or(true)
}

fn wake_enabled(lane: &str) -> bool {
    let process = std::env::var(WAKE_KEY).ok();
    wake_enabled_in(&crate::api::session_verbs::home(), lane, process.as_deref())
}

fn actor_of(headers: &HeaderMap) -> String {
    super::org::local_member_actor(headers)
        .map(str::to_string)
        .unwrap_or_else(|| crate::api::groups::hdr_worker(headers))
        .trim()
        .chars()
        .take(64)
        .collect()
}

fn reply(code: StatusCode, v: Value) -> Response {
    (code, Json(v)).into_response()
}

fn iso(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| ts.to_string())
}

pub(crate) fn ensure_signal_table(conn: &Connection) -> rusqlite::Result<()> {
    // The migration creates it; this keeps a store opened by an older binary's
    // test fixture (or a DB restored from before 0088) from 500ing the verb.
    conn.execute_batch(include_str!("../../migrations/0089_board_signals.sql"))
}

/// Cards currently parked on `name`: live (not deleted, archived or terminal).
fn waiting_cards(conn: &Connection, name: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM issues WHERE lower(trim(blocked_on)) = ?1 AND deleted IS NULL \
         AND COALESCE(archived,0)=0 AND status NOT IN ('done','verified','discarded') \
         ORDER BY id",
    )?;
    let rows = stmt.query_map([format!("{SIGNAL_PREFIX}{name}")], |r| {
        r.get::<_, String>(0)
    })?;
    rows.collect()
}

/// Every live signal wait, grouped by name.
fn all_waits(conn: &Connection) -> rusqlite::Result<BTreeMap<String, Vec<Value>>> {
    let mut stmt = conn.prepare(
        "SELECT id, COALESCE(session,''), title, blocked_on FROM issues \
         WHERE lower(trim(blocked_on)) LIKE 'signal:%' AND deleted IS NULL \
         AND COALESCE(archived,0)=0 AND status NOT IN ('done','verified','discarded') ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for row in rows {
        let (id, session, title, blocked) = row?;
        if let Some(name) = signal_wait_name(Some(&blocked)) {
            out.entry(name)
                .or_default()
                .push(json!({"card": id, "session": session, "title": title}));
        }
    }
    Ok(out)
}

fn raise_rows(conn: &Connection, name: Option<&str>, limit: i64) -> rusqlite::Result<Vec<Value>> {
    let sql =
        "SELECT id, name, raised_by, raised_at, COALESCE(note,''), cleared FROM board_signals \
               WHERE (?1 IS NULL OR name = ?1) ORDER BY raised_at DESC, id DESC LIMIT ?2";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(rusqlite::params![name, limit], |r| {
        let cleared: String = r.get(5)?;
        let raised_at: i64 = r.get(3)?;
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "name": r.get::<_, String>(1)?,
            "raised_by": r.get::<_, String>(2)?,
            "raised_at": raised_at,
            "raised_at_iso": iso(raised_at),
            "note": r.get::<_, String>(4)?,
            "cleared": serde_json::from_str::<Value>(&cleared).unwrap_or(Value::Null),
        }))
    })?;
    rows.collect()
}

#[derive(serde::Deserialize, Default)]
pub struct ListQuery {
    limit: Option<i64>,
}

/// `GET /api/signals` — recent raises (newest first) and every live wait.
async fn list_signals(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    let res = state
        .store
        .read_async(move |conn| {
            ensure_signal_table(conn)?;
            let total: i64 =
                conn.query_row("SELECT COUNT(*) FROM board_signals", [], |r| r.get(0))?;
            Ok((raise_rows(conn, None, limit)?, total, all_waits(conn)?))
        })
        .await;
    match res {
        Ok((raises, total, waits)) => reply(
            StatusCode::OK,
            // `raises_total` is the population `raises` was cut from, so a
            // capped list says it is capped.
            json!({"raises": raises, "raises_total": total, "limit": limit, "waits": waits}),
        ),
        Err(e) => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": e.to_string()}),
        ),
    }
}

/// `GET /api/signals/{name}` — the raise history for one name and the cards
/// currently waiting on it. `amux signal wait` reads this to warn when the
/// signal was already raised (the event may have happened before the wait).
async fn get_signal(State(state): State<AppState>, Path(raw): Path<String>) -> Response {
    let name = normalize_signal_name(&raw);
    if !valid_signal_name(&name) {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error": "invalid signal name", "name": raw, "rule": "1-64 chars of [a-z0-9._-], starting with a letter or digit"}),
        );
    }
    let n = name.clone();
    let res = state
        .store
        .read_async(move |conn| {
            ensure_signal_table(conn)?;
            let raises = raise_rows(conn, Some(&n), 20)?;
            let waits = all_waits(conn)?.remove(&n).unwrap_or_default();
            Ok((raises, waits))
        })
        .await;
    match res {
        Ok((raises, waits)) => reply(
            StatusCode::OK,
            json!({"name": name, "last_raised": raises.first().cloned(), "raises": raises, "waiting": waits}),
        ),
        Err(e) => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": e.to_string()}),
        ),
    }
}

/// What the raise transaction produced, for the delivery step after it.
struct Raised {
    id: i64,
    at: i64,
    /// (card id, title, owning lane)
    cleared: Vec<(String, String, String)>,
}

/// `POST /api/signals/{name}` with `{"note": "..."}` — raise the signal.
async fn raise_signal(
    State(state): State<AppState>,
    Path(raw): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let name = normalize_signal_name(&raw);
    if !valid_signal_name(&name) {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error": "invalid signal name", "name": raw, "rule": "1-64 chars of [a-z0-9._-], starting with a letter or digit"}),
        );
    }
    let note: String = body
        .as_ref()
        .and_then(|Json(b)| b.get("note"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(2000)
        .collect();
    let raiser = actor_of(&headers);
    let raised = match raise_in_store(&state.store, &name, &raiser, &note).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "amux::signals", signal = %name, raiser = %raiser,
                verdict = "signal_raise_failed", error = %e, "signal raise could not be recorded");
            return reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": e.to_string()}),
            );
        }
    };

    // Group the freed cards by lane: one wake per lane per raise, never one
    // per card, so a lane with ten parked cards gets one message.
    let mut by_lane: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (id, title, lane) in &raised.cleared {
        by_lane
            .entry(lane.clone())
            .or_default()
            .push((id.clone(), title.clone()));
    }
    let at_iso = iso(raised.at);
    let mut woken = Vec::new();
    for (lane, cards) in &by_lane {
        let verdict: String = if lane.is_empty() {
            "no_owner".into()
        } else if lane == &raiser {
            // The raiser already knows; waking it would spend a turn on news.
            "raiser_is_owner".into()
        } else if !wake_enabled(lane) {
            "wake_disabled".into()
        } else {
            let text = wake_message(&name, &raiser, &at_iso, &note, cards);
            // Stable id per (raise, lane): a retried delivery or a re-run of
            // this handler for the same raise cannot queue a second copy.
            let stable = format!("signal-wake:{}:{lane}", raised.id);
            match crate::api::session_verbs::steer_enqueue_idempotent(
                &state, lane, &text, WAKE_GUARD, &raiser, &stable,
            )
            .await
            {
                Ok(_) => "queued".into(),
                Err(reason) => format!("refused:{reason}"),
            }
        };
        tracing::info!(target: "amux::signals", signal = %name, raise_id = raised.id,
            lane = %lane, cards = cards.len(), verdict = %verdict, measured = true,
            n_considered = cards.len(), "signal wake");
        woken.push(json!({
            "session": lane,
            "cards": cards.iter().map(|(id, _)| id).collect::<Vec<_>>(),
            "delivery": verdict,
        }));
    }
    tracing::info!(target: "amux::signals", signal = %name, raise_id = raised.id,
        raiser = %raiser, cleared = raised.cleared.len(), lanes = by_lane.len(),
        verdict = if raised.cleared.is_empty() { "raised_no_waiters" } else { "raised_and_cleared" },
        measured = true, n_considered = raised.cleared.len(), "signal raised");
    reply(
        StatusCode::OK,
        json!({
            "ok": true,
            "id": raised.id,
            "name": name,
            "raised_by": if raiser.is_empty() { Value::Null } else { json!(raiser) },
            "raised_at": raised.at,
            "raised_at_iso": at_iso,
            "note": note,
            "cleared": raised.cleared.iter().map(|(id, _, lane)| json!({"card": id, "session": lane})).collect::<Vec<_>>(),
            "woken": woken,
        }),
    )
}

/// The raise transaction: record the raise, then clear every card parked on
/// the name, in ONE write so a card cannot be cleared by a raise that was
/// never recorded (or recorded by one that cleared nothing it should have).
async fn raise_in_store(
    store: &crate::db::SharedStore,
    name: &str,
    raiser: &str,
    note: &str,
) -> anyhow::Result<Raised> {
    let slot: Arc<Mutex<Option<Raised>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let (name_w, raiser_w, note_w) = (name.to_string(), raiser.to_string(), note.to_string());
    store
        .write_async(move |conn| {
            ensure_signal_table(conn)?;
            let now = chrono::Utc::now().timestamp();
            let stamp = chrono::Local::now().format("%H:%M").to_string();
            let who = if raiser_w.is_empty() { "api-anonymous" } else { raiser_w.as_str() };
            let mut events = Vec::new();
            let mut cleared = Vec::new();
            for id in waiting_cards(conn, &name_w)? {
                let Some(mut row) = bs::get_issue(conn, &id)? else { continue };
                row.blocked_on = None;
                row.updated = now;
                row.rev += 1;
                row.version += 1;
                let note_part = if note_w.is_empty() { String::new() } else { format!(": {note_w}") };
                row.log = Some(bs::append_log(
                    row.log.as_deref(),
                    &stamp,
                    &format!("signal {name_w} raised by {who}{note_part}; wait cleared"),
                ));
                bs::save_patched(conn, &mut row)?;
                events.push(PendingEvent {
                    entity_type: EntityType::Task,
                    entity_id: row.id.clone(),
                    mutation: MutationKind::Updated,
                    payload: Some(row.snapshot()),
                });
                cleared.push((row.id.clone(), row.title.clone(), row.session.clone().unwrap_or_default()));
            }
            let cleared_json = json!(cleared
                .iter()
                .map(|(id, _, lane)| json!({"card": id, "session": lane}))
                .collect::<Vec<_>>())
            .to_string();
            conn.execute(
                "INSERT INTO board_signals (name, raised_by, raised_at, note, cleared) VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![name_w, who, now, note_w, cleared_json],
            )?;
            let id = conn.last_insert_rowid();
            *slot_w.lock().unwrap_or_else(|p| p.into_inner()) = Some(Raised { id, at: now, cleared });
            Ok(WriteOutcome { applied: true, events })
        })
        .await?;
    let raised = slot.lock().unwrap_or_else(|p| p.into_inner()).take();
    raised.ok_or_else(|| anyhow::anyhow!("signal raise wrote nothing"))
}

// ---------------------------------------------------------------------------
// Signal subscriptions: event-driven worker coordination
// ---------------------------------------------------------------------------

fn ensure_subscription_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(include_str!(
        "../../migrations/0130_signal_subscriptions.sql"
    ))?;
    super::event_subs::ensure_columns(conn)
}

/// A new subscription. Either the engine's own fields (`pattern` and
/// `action`), or `criteria` in plain words, which the server compiles into a
/// pattern and a filter against the event catalog (`event_subs::compile`).
/// `dry_run` compiles and backtests without saving.
#[derive(serde::Deserialize, Clone, Debug, Default)]
struct NewSubscription {
    #[serde(default)]
    name: String,
    #[serde(default)]
    pattern: String,
    #[serde(default)]
    source_filter: String,
    #[serde(default)]
    session_filter: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    action_target: String,
    #[serde(default)]
    action_template: String,
    #[serde(default = "default_dedupe")]
    dedupe_window_s: i64,
    #[serde(default)]
    criteria: String,
    #[serde(default)]
    filter: Value,
    #[serde(default)]
    judge: bool,
    #[serde(default)]
    dedupe_key: String,
    #[serde(default)]
    dry_run: bool,
}

fn default_dedupe() -> i64 {
    300
}

fn valid_action(a: &str) -> bool {
    matches!(a, "send" | "board" | "schedule" | "shell" | "notify")
}

pub(crate) fn glob_matches(pattern: &str, text: &str) -> bool {
    let pat = pattern.to_ascii_lowercase();
    let txt = text.to_ascii_lowercase();
    if pat == "*" {
        return true;
    }
    if let Some(prefix) = pat.strip_suffix(".*") {
        return txt.starts_with(prefix) || txt == prefix;
    }
    if let Some(prefix) = pat.strip_suffix('*') {
        return txt.starts_with(prefix);
    }
    pat == txt
}

const SUB_COLS: &str = "id, name, pattern, source_filter, session_filter, action, action_target, \
     action_template, created_by, created_at, enabled, dedupe_window_s, last_fired_at, \
     fire_count, criteria, filter, owner, judge, dedupe_key, notify_post";

fn sub_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let filter: String = r.get(15)?;
    Ok(json!({
        "id": r.get::<_, i64>(0)?,
        "name": r.get::<_, String>(1)?,
        "pattern": r.get::<_, String>(2)?,
        "source_filter": r.get::<_, String>(3)?,
        "session_filter": r.get::<_, String>(4)?,
        "action": r.get::<_, String>(5)?,
        "action_target": r.get::<_, String>(6)?,
        "action_template": r.get::<_, String>(7)?,
        "created_by": r.get::<_, String>(8)?,
        "created_at": r.get::<_, i64>(9)?,
        "enabled": r.get::<_, i64>(10)? != 0,
        "dedupe_window_s": r.get::<_, i64>(11)?,
        "last_fired_at": r.get::<_, Option<i64>>(12)?,
        "fire_count": r.get::<_, i64>(13)?,
        "criteria": r.get::<_, String>(14)?,
        "filter": serde_json::from_str::<Value>(&filter).unwrap_or(json!({})),
        "owner": r.get::<_, String>(16)?,
        "judge": r.get::<_, i64>(17)? != 0,
        "dedupe_key": r.get::<_, String>(18)?,
        "notify_post": r.get::<_, Option<i64>>(19)?,
    }))
}

fn sub_row_to_json(conn: &Connection, id: i64) -> rusqlite::Result<Option<Value>> {
    conn.query_row(&format!("SELECT {SUB_COLS} FROM signal_subscriptions WHERE id = ?1"), [id], sub_from_row)
        .optional()
}

fn all_sub_rows(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(&format!("SELECT {SUB_COLS} FROM signal_subscriptions ORDER BY id"))?;
    let rows = stmt.query_map([], sub_from_row)?;
    rows.collect()
}

use rusqlite::OptionalExtension as _;

#[derive(serde::Deserialize, Default)]
struct SubListQuery {
    owner: Option<String>,
}

async fn list_subscriptions(State(state): State<AppState>, Query(q): Query<SubListQuery>) -> Response {
    let res = state
        .store
        .read_async(move |conn| {
            ensure_subscription_tables(conn)?;
            Ok(all_sub_rows(conn)?)
        })
        .await;
    match res {
        Ok(mut subs) => {
            if let Some(o) = q.owner.filter(|o| !o.is_empty()) {
                subs.retain(|s| s["owner"].as_str() == Some(o.as_str()));
            }
            reply(StatusCode::OK, json!({"subscriptions": subs, "count": subs.len()}))
        }
        Err(e) => reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
    }
}

async fn get_subscription(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    let res = state
        .store
        .read_async(move |conn| {
            ensure_subscription_tables(conn)?;
            Ok(sub_row_to_json(conn, id)?)
        })
        .await;
    match res {
        Ok(Some(sub)) => reply(StatusCode::OK, sub),
        Ok(None) => reply(StatusCode::NOT_FOUND, json!({"error": "subscription not found"})),
        Err(e) => reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
    }
}

/// A short unique-enough name from plain words: the first few words, slugged.
fn name_from(criteria: &str) -> String {
    let words: Vec<String> = criteria
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(6)
        .map(str::to_ascii_lowercase)
        .collect();
    let base = if words.is_empty() { "subscription".to_string() } else { words.join("-") };
    format!("{base}-{}", chrono::Utc::now().timestamp() % 100_000)
}

/// Who may make which subscription. The owner (no worker header): anything.
/// A worker: no `shell` (a shell command run by the server on every match is
/// the owner's to set up), and `send` only to itself.
fn refusal_for(creator: &str, action: &str, target: &str) -> Option<String> {
    if creator.is_empty() {
        return None;
    }
    if action == "shell" {
        return Some("only the owner can make a shell subscription".into());
    }
    if action == "send" && target != creator {
        return Some(format!("a worker's send subscription goes to itself ({creator}), not {target}"));
    }
    None
}

async fn create_subscription(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<NewSubscription>,
) -> Response {
    let creator = actor_of(&headers);
    let mut compiled_view = Value::Null;
    if body.pattern.trim().is_empty() && !body.criteria.trim().is_empty() {
        let Some(client) = super::board_intake::model_client() else {
            return reply(StatusCode::SERVICE_UNAVAILABLE, json!({"error": "no helper model is configured to read the criteria; give pattern and filter instead"}));
        };
        let catalog = match state.store.read_async(|conn| Ok(super::event_subs::catalog(conn, 7)?)).await {
            Ok(c) => c,
            Err(e) => return reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
        };
        match super::event_subs::compile(client, body.criteria.clone(), catalog).await {
            Ok(c) => {
                tracing::info!(target: "amux::signals", criteria = %body.criteria, pattern = %c.pattern, filter = %c.filter,
                    judge = c.judge, measured = true, n_considered = 1, verdict = "subscription_compiled",
                    "plain words compiled to an event subscription");
                compiled_view = json!({"pattern": c.pattern, "filter": c.filter, "summary": c.summary, "judge": c.judge, "dedupe_key": c.dedupe_key});
                body.pattern = c.pattern;
                if body.filter.is_null() {
                    body.filter = c.filter;
                }
                body.judge = body.judge || c.judge;
                if body.dedupe_key.is_empty() {
                    body.dedupe_key = c.dedupe_key;
                }
            }
            Err(e) => {
                tracing::warn!(target: "amux::signals", criteria = %body.criteria, error = %e, verdict = "subscription_compile_failed",
                    "plain words did not compile to an event subscription");
                return reply(StatusCode::UNPROCESSABLE_ENTITY, json!({"error": e, "criteria": body.criteria}));
            }
        }
    }
    if body.pattern.trim().is_empty() {
        return reply(StatusCode::BAD_REQUEST, json!({"error": "pattern is required (or criteria in plain words)"}));
    }
    if body.action.is_empty() {
        // The owner is told; a worker wakes itself.
        body.action = if creator.is_empty() { "notify".into() } else { "send".into() };
    }
    if body.action_target.trim().is_empty() {
        body.action_target = if creator.is_empty() { "owner".into() } else { creator.clone() };
    }
    if body.name.trim().is_empty() {
        body.name = name_from(if body.criteria.is_empty() { &body.pattern } else { &body.criteria });
    }
    if !valid_action(&body.action) {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error": "action must be one of: send, board, schedule, shell, notify", "got": body.action}),
        );
    }
    if let Some(why) = refusal_for(&creator, &body.action, body.action_target.trim()) {
        return reply(StatusCode::FORBIDDEN, json!({"error": why}));
    }
    let filter = if body.filter.is_null() { json!({}) } else { body.filter.clone() };
    let (pattern, f2) = (body.pattern.trim().to_string(), filter.clone());
    let backtest = state
        .store
        .read_async(move |conn| Ok(super::event_subs::backtest(conn, &pattern, &f2, 7)?))
        .await
        .unwrap_or_else(|e| json!({"measured": false, "why_unmeasured": e.to_string()}));
    if body.dry_run {
        return reply(StatusCode::OK, json!({
            "dry_run": true, "compiled": compiled_view, "pattern": body.pattern.trim(), "filter": filter,
            "action": body.action, "action_target": body.action_target.trim(), "backtest": backtest,
        }));
    }
    let now = chrono::Utc::now().timestamp();
    let b = body.clone();
    let owner = if creator.is_empty() { "owner".to_string() } else { creator.clone() };
    let filter_s = filter.to_string();
    let slot: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let res = state
        .store
        .write_async(move |conn| {
            ensure_subscription_tables(conn)?;
            conn.execute(
                "INSERT INTO signal_subscriptions \
                 (name, pattern, source_filter, session_filter, action, action_target, \
                  action_template, created_by, created_at, dedupe_window_s, criteria, filter, owner, judge, dedupe_key) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                rusqlite::params![
                    b.name.trim(),
                    b.pattern.trim(),
                    b.source_filter.trim(),
                    b.session_filter.trim(),
                    b.action,
                    b.action_target.trim(),
                    b.action_template,
                    creator,
                    now,
                    b.dedupe_window_s.max(0),
                    b.criteria.trim(),
                    filter_s,
                    owner,
                    b.judge as i64,
                    b.dedupe_key.trim(),
                ],
            )?;
            *slot_w.lock().unwrap() = Some(conn.last_insert_rowid());
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await;
    match res {
        Ok(_) => {
            let id = slot.lock().unwrap().unwrap();
            let sub = state
                .store
                .read_async(move |conn| Ok(sub_row_to_json(conn, id)?))
                .await
                .ok()
                .flatten();
            let mut sub = sub.unwrap_or(json!({"id": id}));
            if let Some(o) = sub.as_object_mut() {
                o.insert("compiled".into(), compiled_view);
                o.insert("backtest".into(), backtest);
            }
            tracing::info!(target: "amux::signals", subscription = %body.name, pattern = %body.pattern, action = %body.action,
                owner = %sub["owner"], measured = true, n_considered = 1, verdict = "subscription_created", "event subscription created");
            reply(StatusCode::CREATED, sub)
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("UNIQUE") {
                reply(
                    StatusCode::CONFLICT,
                    json!({"error": "a subscription with that name already exists", "name": body.name}),
                )
            } else {
                reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": msg}))
            }
        }
    }
}

/// A worker may change or delete only a subscription it made, and an edit is
/// held to the same rule as a create ([`refusal_for`]). Without this, PATCH
/// re-opened what create refuses: a worker could point an owner's shell
/// subscription at a command of its own, which the server then runs outside
/// that worker's guard, or turn any send toward another worker. The owner (no
/// worker header) is unchanged. `None` means go ahead.
async fn worker_edit_refusal(
    state: &AppState,
    id: i64,
    caller: &str,
    new_target: Option<String>,
) -> Option<Response> {
    if caller.is_empty() {
        return None;
    }
    let row = state
        .store
        .read_async(move |conn| {
            ensure_subscription_tables(conn)?;
            Ok(conn
                .query_row(
                    "SELECT created_by, action, action_target FROM signal_subscriptions WHERE id = ?1",
                    [id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
                )
                .optional()?)
        })
        .await;
    let (created_by, action, target) = match row {
        Ok(Some(r)) => r,
        // Not found (or unreadable): let the handler give its own answer.
        _ => return None,
    };
    let why = if created_by != caller {
        Some(format!("subscription {id} belongs to {}, not {caller}",
            if created_by.is_empty() { "the owner" } else { created_by.as_str() }))
    } else {
        let target = new_target.map(|t| t.trim().to_string()).unwrap_or(target);
        refusal_for(caller, &action, &target)
    }?;
    tracing::warn!(target: "amux::signals", subscription_id = id, caller, measured = true, n_considered = 1,
        verdict = "subscription_edit_refused", "{why}");
    Some(reply(StatusCode::FORBIDDEN, json!({"error": why})))
}

async fn update_subscription(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<Value>,
) -> Response {
    let caller = actor_of(&headers);
    let new_target = body.get("action_target").and_then(|v| v.as_str()).map(str::to_string);
    if let Some(r) = worker_edit_refusal(&state, id, &caller, new_target).await {
        return r;
    }
    let res = state
        .store
        .write_async(move |conn| {
            ensure_subscription_tables(conn)?;
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM signal_subscriptions WHERE id = ?1",
                    [id],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if !exists {
                return Ok(WriteOutcome {
                    applied: false,
                    events: vec![],
                });
            }
            let mut sets = Vec::new();
            let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![];
            let mut idx = 1;
            macro_rules! maybe_set {
                ($field:expr, $col:expr, $ty:ty) => {
                    if let Some(v) = body.get($field) {
                        if let Some(val) = v.as_str().map(|s| s.to_string()) {
                            sets.push(format!("{} = ?{idx}", $col));
                            params.push(Box::new(val));
                            idx += 1;
                        }
                    }
                };
            }
            maybe_set!("pattern", "pattern", String);
            maybe_set!("source_filter", "source_filter", String);
            maybe_set!("session_filter", "session_filter", String);
            maybe_set!("action_target", "action_target", String);
            maybe_set!("action_template", "action_template", String);
            maybe_set!("dedupe_key", "dedupe_key", String);
            if let Some(f) = body.get("filter").filter(|f| f.is_object()) {
                sets.push(format!("filter = ?{idx}"));
                params.push(Box::new(f.to_string()));
                idx += 1;
            }
            for (key, col) in [("enabled", "enabled"), ("judge", "judge")] {
                if let Some(v) = body.get(key) {
                    let on = match v {
                        Value::Bool(b) => *b,
                        Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
                        _ => true,
                    };
                    sets.push(format!("{col} = ?{idx}"));
                    params.push(Box::new(if on { 1i64 } else { 0 }));
                    idx += 1;
                }
            }
            if let Some(v) = body.get("dedupe_window_s") {
                if let Some(n) = v.as_i64() {
                    sets.push(format!("dedupe_window_s = ?{idx}"));
                    params.push(Box::new(n.max(0)));
                    idx += 1;
                }
            }
            let _ = idx;
            if sets.is_empty() {
                return Ok(WriteOutcome {
                    applied: false,
                    events: vec![],
                });
            }
            params.push(Box::new(id));
            let sql = format!(
                "UPDATE signal_subscriptions SET {} WHERE id = ?{}",
                sets.join(", "),
                params.len()
            );
            let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
            conn.execute(&sql, param_refs.as_slice())?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await;
    match res {
        Ok(wo) if wo.applied => {
            let sub = state
                .store
                .read_async(move |conn| Ok(sub_row_to_json(conn, id)?))
                .await
                .ok()
                .flatten();
            reply(StatusCode::OK, sub.unwrap_or(json!({"ok": true})))
        }
        Ok(_) => reply(StatusCode::NOT_FOUND, json!({"error": "subscription not found or no changes"})),
        Err(e) => reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
    }
}


async fn delete_subscription(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if let Some(r) = worker_edit_refusal(&state, id, &actor_of(&headers), None).await {
        return r;
    }
    let res = state
        .store
        .write_async(move |conn| {
            ensure_subscription_tables(conn)?;
            // Its fires reference it (signal_fires.subscription_id), so a
            // subscription that ever fired could not be deleted: "FOREIGN KEY
            // constraint failed" on #4, live 2026-11-11. They go with it.
            let fires = conn.execute("DELETE FROM signal_fires WHERE subscription_id = ?1", [id])?;
            let n = conn.execute("DELETE FROM signal_subscriptions WHERE id = ?1", [id])?;
            if n > 0 {
                tracing::info!(target: "amux::signals", subscription_id = id, fires_removed = fires, measured = true,
                    n_considered = 1, verdict = "subscription_deleted", "event subscription deleted with its fire history");
            }
            Ok(WriteOutcome {
                applied: n > 0,
                events: vec![],
            })
        })
        .await;
    match res {
        Ok(wo) if wo.applied => reply(StatusCode::OK, json!({"ok": true, "deleted": id})),
        Ok(_) => reply(StatusCode::NOT_FOUND, json!({"error": "subscription not found"})),
        Err(e) => reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
    }
}


#[derive(serde::Deserialize)]
struct EmitBody {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    session: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    data: Value,
}

/// Emit an event, then run the dispatcher over the ledger right away so the
/// reply can say which subscriptions this event fired. The dispatcher is the
/// one that fires (runtime_jobs::event_dispatch); this only does not wait for
/// its next tick.
async fn emit_custom_signal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<EmitBody>,
) -> Response {
    if body.event_type.trim().is_empty() {
        return reply(StatusCode::BAD_REQUEST, json!({"error": "type is required"}));
    }
    let emitter = actor_of(&headers);
    let source = if body.source.is_empty() {
        emitter.clone()
    } else {
        body.source.clone()
    };
    let session = if body.session.is_empty() {
        emitter.clone()
    } else {
        body.session.clone()
    };
    static EMITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let idem = format!("emit:{}:{}:{}", std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        EMITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    crate::api::session_verbs::emit_event_store(
        &state.store,
        &session,
        &body.event_type,
        Some(body.data.clone()),
        Some(idem.clone()),
        &source,
    )
    .await;
    let id = state
        .store
        .read_async(move |conn| Ok(conn.query_row("SELECT id FROM session_events WHERE idem = ?1", [&idem], |r| r.get::<_, i64>(0)).optional()?))
        .await
        .ok()
        .flatten();
    let fires = crate::runtime_jobs::event_dispatch::drain_including(&state, id).await;
    let mine = id.and_then(|i| fires.get(&i).cloned()).unwrap_or_default();
    reply(
        StatusCode::OK,
        json!({
            "ok": true,
            "type": body.event_type,
            "session": session,
            "source": source,
            "event_id": id,
            "subscriptions_fired": mine,
        }),
    )
}

#[derive(serde::Deserialize, Default)]
struct CatalogQuery {
    days: Option<i64>,
}

/// Every event type a subscription can listen to (event_subs::catalog).
async fn event_catalog(State(state): State<AppState>, Query(q): Query<CatalogQuery>) -> Response {
    let days = q.days.unwrap_or(7).clamp(1, 90);
    match state.store.read_async(move |conn| Ok(super::event_subs::catalog(conn, days)?)).await {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"measured": false, "why_unmeasured": e.to_string()})),
    }
}

#[derive(serde::Deserialize, Default)]
struct RecentQuery {
    pattern: Option<String>,
    after: Option<i64>,
    limit: Option<i64>,
}

/// Events after `after` whose type matches `pattern`, for `amux events tail`.
async fn recent_events(State(state): State<AppState>, Query(q): Query<RecentQuery>) -> Response {
    let pattern = q.pattern.unwrap_or_else(|| "*".into());
    let (after, limit) = (q.after.unwrap_or(0), q.limit.unwrap_or(50).clamp(1, 500));
    match state.store.read_async(move |conn| Ok(super::event_subs::recent(conn, &pattern, after, limit)?)).await {
        Ok(rows) => {
            let last = rows.last().and_then(|r| r["id"].as_i64()).unwrap_or(after);
            reply(StatusCode::OK, json!({"events": rows, "count": rows.len(), "last_id": last}))
        }
        Err(e) => reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
    }
}

#[derive(serde::Deserialize, Default)]
struct FiresQuery {
    limit: Option<i64>,
    subscription_id: Option<i64>,
}

async fn list_fires(State(state): State<AppState>, Query(q): Query<FiresQuery>) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let sub_id = q.subscription_id;
    let res = state
        .store
        .read_async(move |conn| {
            ensure_subscription_tables(conn)?;
            fn map_fire(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
                let fired_at: i64 = r.get(5)?;
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "subscription_id": r.get::<_, i64>(1)?,
                    "subscription_name": r.get::<_, String>(2)?,
                    "event_type": r.get::<_, String>(3)?,
                    "event_data": serde_json::from_str::<Value>(&r.get::<_, String>(4).unwrap_or_default()).unwrap_or(Value::Null),
                    "fired_at": fired_at,
                    "fired_at_iso": iso(fired_at),
                    "action": r.get::<_, String>(6)?,
                    "action_target": r.get::<_, String>(7)?,
                    "outcome": r.get::<_, String>(8)?,
                }))
            }
            let (sql, p1, p2): (&str, i64, i64) = if let Some(sid) = sub_id {
                (
                    "SELECT f.id, f.subscription_id, s.name, f.event_type, f.event_data, f.fired_at, \
                     f.action, f.action_target, f.outcome \
                     FROM signal_fires f JOIN signal_subscriptions s ON s.id = f.subscription_id \
                     WHERE f.subscription_id = ?1 ORDER BY f.fired_at DESC LIMIT ?2",
                    sid, limit,
                )
            } else {
                (
                    "SELECT f.id, f.subscription_id, s.name, f.event_type, f.event_data, f.fired_at, \
                     f.action, f.action_target, f.outcome \
                     FROM signal_fires f JOIN signal_subscriptions s ON s.id = f.subscription_id \
                     ORDER BY f.fired_at DESC LIMIT ?2",
                    0, limit,
                )
            };
            let mut stmt = conn.prepare(sql)?;
            let rows: Vec<Value> = stmt
                .query_map(rusqlite::params![p1, p2], map_fire)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await;
    match res {
        Ok(fires) => reply(StatusCode::OK, json!({"fires": fires, "count": fires.len()})),
        Err(e) => reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
    }
}


// ---------------------------------------------------------------------------
// Dispatcher: match events against subscriptions and fire actions
// ---------------------------------------------------------------------------

/// Fires one subscription may make in an hour. A subscription whose action
/// itself emits a matching event would otherwise loop.
pub(crate) const FIRES_PER_HOUR: i64 = 60;

/// An enabled subscription, loaded once per dispatcher batch.
#[derive(Clone, Debug)]
pub(crate) struct LiveSub {
    pub id: i64,
    pub name: String,
    pub pattern: String,
    pub source_filter: String,
    pub session_filter: String,
    pub action: String,
    pub action_target: String,
    pub action_template: String,
    pub dedupe_window_s: i64,
    pub last_fired_at: Option<i64>,
    pub criteria: String,
    pub filter: Value,
    pub judge: bool,
    pub dedupe_key: String,
    pub notify_post: Option<i64>,
}

pub(crate) fn load_live_subs(conn: &Connection) -> rusqlite::Result<Vec<LiveSub>> {
    ensure_subscription_tables(conn)?;
    let mut stmt = conn.prepare(
        "SELECT id, name, pattern, source_filter, session_filter, action, action_target, action_template, \
         dedupe_window_s, last_fired_at, criteria, filter, judge, dedupe_key, notify_post \
         FROM signal_subscriptions WHERE enabled = 1 ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        let filter: String = r.get(11)?;
        Ok(LiveSub {
            id: r.get(0)?,
            name: r.get(1)?,
            pattern: r.get(2)?,
            source_filter: r.get(3)?,
            session_filter: r.get(4)?,
            action: r.get(5)?,
            action_target: r.get(6)?,
            action_template: r.get(7)?,
            dedupe_window_s: r.get(8)?,
            last_fired_at: r.get(9)?,
            criteria: r.get(10)?,
            filter: serde_json::from_str(&filter).unwrap_or(json!({})),
            judge: r.get::<_, i64>(12)? != 0,
            dedupe_key: r.get(13)?,
            notify_post: r.get(14)?,
        })
    })?;
    rows.collect()
}

/// Whether `sub` wants this event, before dedupe: type, source and session
/// globs, then the filter over the event view.
pub(crate) fn sub_matches(sub: &LiveSub, event_type: &str, session: &str, source: &str, ev: &Value) -> bool {
    glob_matches(&sub.pattern, event_type)
        && (sub.source_filter.is_empty() || glob_matches(&sub.source_filter, source))
        && (sub.session_filter.is_empty() || glob_matches(&sub.session_filter, session))
        && super::event_subs::filter_matches(&sub.filter, ev)
}

fn render_template(template: &str, event_type: &str, data: &Value) -> String {
    if template.is_empty() {
        return format!("[signal] {event_type}: {data}");
    }
    let mut out = template.replace("{{type}}", event_type);
    if let Some(obj) = data.as_object() {
        for (k, v) in obj {
            let placeholder = format!("{{{{{k}}}}}");
            let val = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out = out.replace(&placeholder, &val);
        }
    }
    out = out.replace("{{data}}", &data.to_string());
    out
}

/// The notice for a fire when the subscription has no template: what
/// happened, for whom, and where, in one readable line.
fn default_text(sub: &LiveSub, event_type: &str, ev: &Value) -> String {
    let pick = |k: &str| ev.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    let mut parts = vec![format!("{event_type}")];
    for k in ["reason", "provider", "worker", "session", "title", "status", "url"] {
        if let Some(v) = pick(k) {
            parts.push(format!("{k}: {v}"));
        }
    }
    // A blank line: a single newline runs the two together in a Messages post.
    let head = if sub.criteria.is_empty() { String::new() } else { format!("{}\n\n", sub.criteria) };
    format!("{head}{}", parts.join(" · "))
}

/// One event through every live subscription. `subs` is the batch's list and
/// is updated in place (last fire time, thread root), so a burst inside one
/// batch dedupes the same as one spread over several.
pub(crate) async fn dispatch_one(
    state: &AppState,
    subs: &mut [LiveSub],
    event_id: Option<i64>,
    event_type: &str,
    session: &str,
    source: &str,
    data: &Value,
) -> Vec<Value> {
    let ev = super::event_subs::event_view(event_type, session, source, data);
    let now = chrono::Utc::now().timestamp();
    let mut fired = Vec::new();
    for sub in subs.iter_mut() {
        if !sub_matches(sub, event_type, session, source, &ev) {
            continue;
        }
        let key = super::event_subs::fill(&sub.dedupe_key, &ev);
        let (sid, k2, window) = (sub.id, key.clone(), sub.dedupe_window_s);
        let recent = state
            .store
            .read_async(move |conn| {
                let last_same: Option<i64> = if k2.is_empty() {
                    None
                } else {
                    conn.query_row(
                        "SELECT MAX(fired_at) FROM signal_fires WHERE subscription_id = ?1 AND dedupe_key = ?2 AND outcome NOT LIKE 'judge_no%'",
                        rusqlite::params![sid, k2], |r| r.get(0))?
                };
                let hour: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM signal_fires WHERE subscription_id = ?1 AND fired_at > ?2 AND outcome NOT LIKE 'judge_no%'",
                    rusqlite::params![sid, now - 3600], |r| r.get(0))?;
                Ok((last_same, hour))
            })
            .await;
        let (last_same, hour) = recent.unwrap_or((None, 0));
        let last = if sub.dedupe_key.is_empty() { sub.last_fired_at } else { last_same };
        if window > 0 && last.is_some_and(|t| now - t < window) {
            continue;
        }
        if hour >= FIRES_PER_HOUR {
            tracing::warn!(target: "amux::signals", subscription = %sub.name, event_type, fires_last_hour = hour,
                measured = true, n_considered = 1, verdict = "subscription_rate_capped",
                "a subscription hit its hourly fire cap; this event did not fire it");
            continue;
        }
        if sub.judge {
            // A judge call takes the helper model 20-50 s. Awaited here it held
            // the whole dispatcher, so every subscription in the fleet waited on
            // one judged match (live, 2026-11-11: the cursor sat 85 s behind).
            // It runs on its own; the dispatcher moves on.
            let (st, mut s2, ev2, data2, et, key2) = (state.clone(), sub.clone(), ev.clone(), data.clone(), event_type.to_string(), key.clone());
            tokio::spawn(async move {
                let outcome = judge(&s2, &ev2, now).await;
                tracing::info!(target: "amux::signals", subscription = %s2.name, event_type = %et, verdict_judge = %outcome,
                    measured = true, n_considered = 1, verdict = "subscription_judged", "the helper model judged a match");
                if outcome == "judge_yes" {
                    act(&st, &mut s2, event_id, &et, &data2, &ev2, now, &key2).await;
                } else {
                    record_fire(&st, &s2, event_id, &et, &data2, now, "judge", &outcome, &key2).await;
                }
            });
            fired.push(json!({"subscription": sub.name, "action": "judge", "target": sub.action_target, "outcome": "judging"}));
            continue;
        }
        fired.push(act(state, sub, event_id, event_type, data, &ev, now, &key).await);
    }
    fired
}

/// Run a subscription's action for one event and record the fire.
#[allow(clippy::too_many_arguments)]
async fn act(state: &AppState, sub: &mut LiveSub, event_id: Option<i64>, event_type: &str, data: &Value, ev: &Value,
    now: i64, key: &str) -> Value {
    let text = if sub.action_template.is_empty() {
        default_text(sub, event_type, ev)
    } else {
        super::event_subs::fill(&render_template(&sub.action_template, event_type, data), ev)
    };
    let outcome = match sub.action.as_str() {
        "send" => execute_send_action(state, &sub.action_target, &text).await,
        "board" => execute_board_action(state, &sub.action_target, event_type, &text).await,
        "schedule" => execute_schedule_action(state, &sub.action_target).await,
        "shell" => execute_shell_action(&sub.action_target, &text).await,
        "notify" => {
            let (outcome, root) = super::event_subs::notify(state, &sub.name, sub.notify_post, &text, ev).await;
            if root != sub.notify_post {
                sub.notify_post = root;
                let (sid, r) = (sub.id, root);
                if let Err(e) = state.store.write_async(move |conn| {
                    conn.execute("UPDATE signal_subscriptions SET notify_post = ?1 WHERE id = ?2", rusqlite::params![r, sid])?;
                    Ok(WriteOutcome { applied: true, events: vec![] })
                }).await {
                    tracing::warn!(target: "amux::signals", subscription = %sub.name, error = %e, verdict = "subscription_thread_unsaved",
                        "the notice thread was not saved; the next fire starts a new one");
                }
            }
            outcome
        }
        _ => "unsupported_action".to_string(),
    };
    sub.last_fired_at = Some(now);
    let (action, target) = (sub.action.clone(), sub.action_target.clone());
    record_fire(state, sub, event_id, event_type, data, now, &action, &outcome, key).await;
    tracing::info!(target: "amux::signals",
        subscription = %sub.name, event_type, action = %action,
        target = %target, outcome = %outcome, event_id = ?event_id,
        measured = true, n_considered = 1, verdict = "subscription_fired", "subscription fired");
    json!({"subscription": sub.name, "action": action, "target": target, "outcome": outcome})
}

async fn judge(sub: &LiveSub, ev: &Value, now: i64) -> String {
    if !super::event_subs::judge_allowed(sub.id, now) {
        tracing::warn!(target: "amux::signals", subscription = %sub.name, verdict = "subscription_judge_capped",
            measured = true, n_considered = 1, "judge calls for this subscription hit the hourly cap; the event was not judged");
        return "judge_no:capped".into();
    }
    let Some(client) = super::board_intake::model_client() else {
        return "judge_no:no_model".into();
    };
    let prompt = super::session_verbs::redact_prompt_secrets(&super::event_subs::judge_prompt(&sub.criteria, ev));
    let model = super::mdai::resolve_model(None);
    match tokio::task::spawn_blocking(move || client.complete(&model, &prompt)).await {
        Ok(Ok(out)) if super::event_subs::judge_says_yes(&out) => "judge_yes".into(),
        Ok(Ok(_)) => "judge_no".into(),
        Ok(Err(e)) => format!("judge_no:error:{}", e.chars().take(120).collect::<String>()),
        Err(e) => format!("judge_no:error:{e}"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn record_fire(state: &AppState, sub: &LiveSub, event_id: Option<i64>, event_type: &str, data: &Value,
    now: i64, action: &str, outcome: &str, key: &str) {
    let (sub_id, et, data_s, action, target, outcome, key) = (sub.id, event_type.to_string(), data.to_string(),
        action.to_string(), sub.action_target.clone(), outcome.to_string(), key.to_string());
    let counts = !outcome.starts_with("judge_no");
    let _ = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO signal_fires (subscription_id, event_type, event_data, fired_at, action, action_target, outcome, event_id, dedupe_key) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                rusqlite::params![sub_id, et, data_s, now, action, target, outcome, event_id, key],
            )?;
            if counts {
                conn.execute(
                    "UPDATE signal_subscriptions SET last_fired_at = ?1, fire_count = fire_count + 1 WHERE id = ?2",
                    rusqlite::params![now, sub_id],
                )?;
            }
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .await;
}

async fn execute_send_action(state: &AppState, target: &str, text: &str) -> String {
    let home = crate::config::amux_home();
    let sessions_dir = home.join("sessions");
    if !sessions_dir.join(format!("{target}.env")).exists() {
        return format!("target_not_found:{target}");
    }
    match crate::api::session_verbs::steer_enqueue_idempotent(
        state,
        target,
        text,
        "signal-subscription",
        "signal-dispatcher",
        &format!("signal-sub:{}:{}", target, chrono::Utc::now().timestamp()),
    )
    .await
    {
        // Steering hands it to the worker at its next turn boundary; until
        // then it is queued, and saying "delivered" claimed more than happened
        // (live, 2026-11-11: a busy lane's wake sat in steering_queue).
        Ok(_) => "queued".to_string(),
        Err(reason) => format!("refused:{reason}"),
    }
}

async fn execute_board_action(
    state: &AppState,
    target: &str,
    event_type: &str,
    text: &str,
) -> String {
    let title: String = format!("[signal] {event_type}").chars().take(200).collect();
    let desc = text.to_string();
    let session = target.to_string();
    let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let row = bs::create_issue(
                conn,
                &bs::NewIssue {
                    title,
                    desc,
                    status: "todo".into(),
                    session: Some(session),
                    item_type: "signal".into(),
                    creator: "signal-dispatcher".into(),
                    owner_type: "agent".into(),
                    acceptance_criteria: None,
                    next_action: None,
                    due: None,
                    due_time: None,
                    reviewer: None,
                    shepherd: None,
                    gate: vec![],
                    depends_on: vec![],
                    tags: vec![],
                    ask_type: None,
                    ask_question: None,
                    ask_unblocks: None,
                    ask_actor: None,
                    source: Some("signal-subscription".into()),
                    requested_by: None,
                    callback_session: None,
                    callback_prompt: None,
                },
                chrono::Utc::now().timestamp(),
            )?;
            let id = row.id.clone();
            let snap = row.snapshot();
            *slot_w.lock().unwrap() = Some(id.clone());
            Ok(WriteOutcome {
                applied: true,
                events: vec![PendingEvent {
                    entity_type: EntityType::Task,
                    entity_id: id,
                    mutation: MutationKind::Created,
                    payload: Some(snap),
                }],
            })
        })
        .await;
    match res {
        Ok(_) => {
            let id = slot.lock().unwrap().clone().unwrap_or_default();
            format!("card_created:{id}")
        }
        Err(e) => format!("failed:{e}"),
    }
}

async fn execute_schedule_action(state: &AppState, schedule_id: &str) -> String {
    let url = format!("/api/schedules/{schedule_id}/run");
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(&url)
        .header("content-type", "application/json")
        .header("x-amux-session", "signal-dispatcher")
        .body(axum::body::Body::empty());
    match req {
        Ok(r) => {
            let app = crate::api::router(state.clone());
            match tower::ServiceExt::oneshot(app, r).await {
                Ok(resp) => {
                    let st = resp.status();
                    if st.is_success() {
                        "triggered".to_string()
                    } else {
                        format!("http_{}", st.as_u16())
                    }
                }
                Err(e) => format!("dispatch_error:{e}"),
            }
        }
        Err(e) => format!("request_build_error:{e}"),
    }
}

async fn execute_shell_action(command: &str, _text: &str) -> String {
    match tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
    {
        Ok(output) => {
            let code = output.status.code().unwrap_or(-1);
            if code == 0 {
                "ok".to_string()
            } else {
                let stderr: String = String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(200)
                    .collect();
                format!("exit_{code}:{stderr}")
            }
        }
        Err(e) => format!("spawn_error:{e}"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn names_are_lowercase_slugs() {
        assert!(valid_signal_name("mvs-reflip-clear"));
        assert!(valid_signal_name("gs3.approval_2"));
        assert!(!valid_signal_name(""));
        assert!(!valid_signal_name("-leading-dash"));
        assert!(!valid_signal_name("has space"));
        assert!(!valid_signal_name("Upper"));
        assert!(!valid_signal_name(&"a".repeat(65)));
        assert_eq!(
            normalize_signal_name("  MVS-Reflip-Clear "),
            "mvs-reflip-clear"
        );
    }

    /// The pre-fix shape of `blocked_on` is free text, and it must stay
    /// exactly that: only the explicit prefix turns a block into a signal wait.
    #[test]
    fn only_the_prefix_makes_a_signal_wait() {
        assert_eq!(
            parse_signal_wait("waiting on the KubeRay answer"),
            SignalWait::NotASignal
        );
        assert_eq!(
            parse_signal_wait("mvs-infra 30-minute clearance"),
            SignalWait::NotASignal
        );
        assert_eq!(parse_signal_wait(""), SignalWait::NotASignal);
        assert_eq!(
            parse_signal_wait("signal:mvs-reflip-clear"),
            SignalWait::Valid("mvs-reflip-clear".into())
        );
        assert_eq!(
            parse_signal_wait(" Signal: MVS-Reflip-Clear "),
            SignalWait::Valid("mvs-reflip-clear".into())
        );
        assert!(matches!(
            parse_signal_wait("signal:"),
            SignalWait::Invalid(_)
        ));
        assert!(matches!(
            parse_signal_wait("signal:two words"),
            SignalWait::Invalid(_)
        ));
        assert_eq!(signal_wait_name(Some("signal:x")), Some("x".into()));
        assert_eq!(signal_wait_name(None), None);
    }

    #[test]
    fn a_worker_may_park_only_its_own_card() {
        assert!(
            wait_owner_refusal("", Some("backend")).is_none(),
            "owner may park any card"
        );
        assert!(wait_owner_refusal("gs-3", Some("gs-3")).is_none());
        let refused = wait_owner_refusal("gs-3", Some("mvs-infra")).expect("refused");
        assert_eq!(refused["code"], "signal_wait_not_own_card");
        assert!(
            wait_owner_refusal("gs-3", None).is_some(),
            "an unowned card is not yours"
        );
    }

    #[test]
    fn the_wake_names_signal_raiser_note_and_cards() {
        let m = wake_message(
            "mvs-reflip-clear",
            "mvs-infra",
            "2026-09-26T14:02:11Z",
            "shards 3-7 are back, 30 min soak passed",
            &[("GS-12".into(), "promote bucket objects".into())],
        );
        assert!(m.contains("`mvs-reflip-clear`"));
        assert!(m.contains("raised by mvs-infra at 2026-09-26T14:02:11Z"));
        assert!(m.contains("Note: shards 3-7 are back"));
        assert!(m.contains("- GS-12: promote bucket objects"));
        assert!(!m.contains('\u{2014}'), "no em-dashes in produced text");
        let bare = wake_message("x", "", "t", "  ", &[]);
        assert!(bare.contains("raised by the owner"));
        assert!(bare.contains("No note was given."));
    }

    #[test]
    fn the_wake_switch_resolves_worker_over_global_and_defaults_on() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        assert!(wake_enabled_in(home, "lane-a", None), "default ON");
        std::fs::write(home.join("amux.env"), "AMUX_SIGNAL_WAKE=0\n").unwrap();
        assert!(!wake_enabled_in(home, "lane-a", None), "global off");
        std::fs::write(home.join("sessions/lane-a.env"), "AMUX_SIGNAL_WAKE=1\n").unwrap();
        assert!(wake_enabled_in(home, "lane-a", None), "worker beats global");
        assert!(
            !wake_enabled_in(home, "lane-a", Some("off")),
            "process env wins"
        );
    }

    pub(crate) fn fixture() -> (AppState, crate::db::SharedStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            Arc::new(crate::db::Store::open(&dir.path().join("signals.db")).expect("open store"));
        crate::test_tempdirs::keep(dir);
        let state = AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "signals-test".into(),
            auth_token: None,
            reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (state, store)
    }

    pub(crate) fn seed(
        store: &crate::db::SharedStore,
        session: &str,
        status: &str,
        blocked_on: Option<&str>,
    ) -> String {
        let slot = Arc::new(Mutex::new(None));
        let slot_w = slot.clone();
        let (session, status, blocked) = (
            session.to_string(),
            status.to_string(),
            blocked_on.map(str::to_string),
        );
        store
            .write(move |conn| {
                let mut row = bs::create_issue(
                    conn,
                    &bs::NewIssue {
                        acceptance_criteria: None,
                        next_action: None,
                        title: format!("card for {session}"),
                        desc: "fixture".into(),
                        status,
                        session: Some(session),
                        item_type: "code".into(),
                        creator: "test".into(),
                        owner_type: "agent".into(),
                        due: None,
                        due_time: None,
                        reviewer: None,
                        shepherd: None,
                        gate: vec![],
                        depends_on: vec![],
                        tags: vec![],
                        ask_type: None,
                        ask_question: None,
                        ask_unblocks: None,
                        ask_actor: None,
                        source: Some("test".into()),
                        requested_by: None,
                        callback_session: None,
                        callback_prompt: None,
                    },
                    1_700_000_000,
                )?;
                row.blocked_on = blocked;
                bs::save_patched(conn, &mut row)?;
                *slot_w.lock().unwrap() = Some(row.id);
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .expect("seed");
        let id = slot.lock().unwrap().clone().unwrap();
        id
    }

    pub(crate) fn row(store: &crate::db::SharedStore, id: &str) -> bs::IssueRow {
        bs::get_issue(&store.read().unwrap(), id).unwrap().unwrap()
    }

    async fn patch(
        state: &AppState,
        id: &str,
        caller: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut h = HeaderMap::new();
        if let Some(c) = caller {
            h.insert("x-amux-session", c.parse().unwrap());
        }
        let resp = crate::api::board::patch_item(
            State(state.clone()),
            Path(id.to_string()),
            h,
            Json(body),
        )
        .await;
        let code = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// The wait goes through the ordinary board PATCH, so the rules live there:
    /// own card only, a valid name, and the canonical spelling is what is stored.
    #[tokio::test]
    async fn a_signal_wait_is_own_card_only_valid_and_canonical() {
        let (state, store) = fixture();
        let mine = seed(&store, "gs-3-bucket-objects", "backlog", None);
        let theirs = seed(&store, "mvs-infra", "backlog", None);

        let (code, body) = patch(
            &state,
            &theirs,
            Some("gs-3-bucket-objects"),
            json!({"blocked_on": "signal:mvs-reflip-clear"}),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["code"], "signal_wait_not_own_card");
        assert_eq!(
            row(&store, &theirs).blocked_on,
            None,
            "refused write stored nothing"
        );

        let (code, body) = patch(
            &state,
            &mine,
            Some("gs-3-bucket-objects"),
            json!({"blocked_on": "signal:not valid"}),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "invalid_signal_name");

        let (code, body) = patch(
            &state,
            &mine,
            Some("gs-3-bucket-objects"),
            json!({"blocked_on": " Signal:MVS-Reflip-Clear "}),
        )
        .await;
        assert!(code.is_success(), "{code} {body}");
        assert_eq!(
            row(&store, &mine).blocked_on.as_deref(),
            Some("signal:mvs-reflip-clear")
        );

        // The owner (no worker header) may park any card; free text is untouched.
        let (code, _) = patch(
            &state,
            &theirs,
            None,
            json!({"blocked_on": "waiting on KubeRay"}),
        )
        .await;
        assert!(code.is_success());
        assert_eq!(
            row(&store, &theirs).blocked_on.as_deref(),
            Some("waiting on KubeRay")
        );
    }

    /// End to end over the store: only live cards parked on THIS name clear;
    /// free-text blocks, other names and terminal cards are untouched, and the
    /// raise is recorded with its raiser, note and cleared set.
    #[tokio::test]
    async fn a_raise_clears_exactly_the_live_waiters_and_records_itself() {
        let (state, store) = fixture();
        let waiting = seed(
            &store,
            "gs-3-bucket-objects",
            "backlog",
            Some("signal:mvs-reflip-clear"),
        );
        let waiting2 = seed(
            &store,
            "mixpeek-frustrations",
            "todo",
            Some("signal:mvs-reflip-clear"),
        );
        let other = seed(
            &store,
            "gs-3-bucket-objects",
            "backlog",
            Some("signal:another"),
        );
        let prose = seed(
            &store,
            "gs-3-bucket-objects",
            "backlog",
            Some("mvs-reflip-clear from mvs-infra"),
        );
        let closed = seed(
            &store,
            "gs-3-bucket-objects",
            "done",
            Some("signal:mvs-reflip-clear"),
        );

        let resp = raise_signal(
            State(state.clone()),
            Path("MVS-Reflip-Clear".into()),
            {
                let mut h = HeaderMap::new();
                h.insert("x-amux-session", "mvs-infra".parse().unwrap());
                h
            },
            Some(Json(json!({"note": "30 min soak passed"}))),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["name"], "mvs-reflip-clear");
        assert_eq!(body["raised_by"], "mvs-infra");
        assert_eq!(body["cleared"].as_array().unwrap().len(), 2, "{body}");
        // Two lanes, one wake entry each. No env file exists in the test home,
        // so the steering chokepoint refuses delivery; the verdict SAYS so
        // rather than claiming a wake that did not happen.
        let woken = body["woken"].as_array().unwrap();
        assert_eq!(woken.len(), 2, "{body}");
        for w in woken {
            let d = w["delivery"].as_str().unwrap();
            assert!(
                d == "queued" || d == "wake_disabled" || d.starts_with("refused:"),
                "every lane gets a stated delivery verdict: {w}"
            );
        }

        assert_eq!(row(&store, &waiting).blocked_on, None);
        assert_eq!(row(&store, &waiting2).blocked_on, None);
        assert!(row(&store, &waiting)
            .log
            .unwrap_or_default()
            .contains("signal mvs-reflip-clear raised by mvs-infra: 30 min soak passed"));
        assert_eq!(
            row(&store, &other).blocked_on.as_deref(),
            Some("signal:another")
        );
        assert_eq!(
            row(&store, &prose).blocked_on.as_deref(),
            Some("mvs-reflip-clear from mvs-infra")
        );
        assert_eq!(
            row(&store, &closed).blocked_on.as_deref(),
            Some("signal:mvs-reflip-clear")
        );

        // A second raise finds nobody waiting: no clears, no wakes, so a raise
        // repeated in a loop can never re-wake anyone.
        let again = raise_in_store(&store, "mvs-reflip-clear", "mvs-infra", "")
            .await
            .unwrap();
        assert!(again.cleared.is_empty());

        let conn = store.read().unwrap();
        let rows = raise_rows(&conn, Some("mvs-reflip-clear"), 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["note"], "30 min soak passed");
        assert_eq!(rows[1]["raised_by"], "mvs-infra");
        assert_eq!(
            all_waits(&conn).unwrap().get("another").map(Vec::len),
            Some(1)
        );
    }
}

#[cfg(test)]
mod subscription_edit_tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{json, Value};
    use tower::ServiceExt;

    async fn call(app: &axum::Router, method: &str, uri: &str, worker: Option<&str>, body: Value) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(uri).header("content-type", "application/json");
        if let Some(w) = worker {
            b = b.header("x-amux-session", w);
        }
        let res = app.clone().oneshot(b.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let st = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (st, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// Create refuses a worker a shell subscription and a send to anyone but
    /// itself; PATCH and DELETE used to take no caller at all, so a worker
    /// could retarget the owner's shell subscription or turn its own send
    /// toward another worker. The owner keeps every edit.
    #[tokio::test]
    async fn a_worker_edits_only_its_own_subscription_and_within_the_create_rule() {
        let (state, _store) = super::tests::fixture();
        let app = super::subscription_routes().with_state(state);
        let (st, owner_sub) = call(&app, "POST", "/", None,
            json!({"name": "owner-shell", "pattern": "board.*", "action": "shell", "action_target": "true"})).await;
        assert_eq!(st, StatusCode::CREATED, "{owner_sub}");
        let (st, own) = call(&app, "POST", "/", Some("lane-a"),
            json!({"name": "lane-a-send", "pattern": "board.*", "action": "send", "action_target": "lane-a"})).await;
        assert_eq!(st, StatusCode::CREATED, "{own}");
        let (o, a) = (owner_sub["id"].as_i64().unwrap(), own["id"].as_i64().unwrap());

        for (method, id, body) in [
            ("PATCH", o, json!({"action_target": "echo changed"})),
            ("DELETE", o, json!({})),
            ("PATCH", a, json!({"action_target": "lane-b"})),
        ] {
            let (st, v) = call(&app, method, &format!("/{id}"), Some("lane-a"), body.clone()).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{method} {id} {body} as lane-a: {v}");
        }
        let (st, v) = call(&app, "GET", &format!("/{o}"), None, json!({})).await;
        assert_eq!((st, v["action_target"].as_str()), (StatusCode::OK, Some("true")), "refused edit stored nothing: {v}");

        let (st, v) = call(&app, "PATCH", &format!("/{a}"), Some("lane-a"), json!({"pattern": "board.moved"})).await;
        assert_eq!(st, StatusCode::OK, "a worker edits its own: {v}");
        let (st, v) = call(&app, "PATCH", &format!("/{o}"), None, json!({"action_target": "true # owner"})).await;
        assert_eq!(st, StatusCode::OK, "the owner edits any: {v}");
        let (st, _) = call(&app, "DELETE", &format!("/{a}"), Some("lane-a"), json!({})).await;
        assert_eq!(st, StatusCode::OK, "a worker deletes its own");
    }
}
