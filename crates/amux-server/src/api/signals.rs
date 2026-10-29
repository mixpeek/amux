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
    ))
}

#[derive(serde::Deserialize, Clone, Debug)]
struct NewSubscription {
    name: String,
    pattern: String,
    #[serde(default)]
    source_filter: String,
    #[serde(default)]
    session_filter: String,
    action: String,
    action_target: String,
    #[serde(default)]
    action_template: String,
    #[serde(default = "default_dedupe")]
    dedupe_window_s: i64,
}

fn default_dedupe() -> i64 {
    300
}

fn valid_action(a: &str) -> bool {
    matches!(a, "send" | "board" | "schedule" | "shell")
}

fn glob_matches(pattern: &str, text: &str) -> bool {
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

fn sub_row_to_json(conn: &Connection, id: i64) -> rusqlite::Result<Option<Value>> {
    conn.query_row(
        "SELECT id, name, pattern, source_filter, session_filter, action, action_target, \
         action_template, created_by, created_at, enabled, dedupe_window_s, last_fired_at, \
         fire_count FROM signal_subscriptions WHERE id = ?1",
        [id],
        |r| {
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
            }))
        },
    )
    .optional()
}

fn all_sub_rows(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, pattern, source_filter, session_filter, action, action_target, \
         action_template, created_by, created_at, enabled, dedupe_window_s, last_fired_at, \
         fire_count FROM signal_subscriptions ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
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
        }))
    })?;
    rows.collect()
}

use rusqlite::OptionalExtension as _;

async fn list_subscriptions(State(state): State<AppState>) -> Response {
    let res = state
        .store
        .read_async(move |conn| {
            ensure_subscription_tables(conn)?;
            Ok(all_sub_rows(conn)?)
        })
        .await;
    match res {
        Ok(subs) => reply(StatusCode::OK, json!({"subscriptions": subs, "count": subs.len()})),
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

async fn create_subscription(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<NewSubscription>,
) -> Response {
    if body.name.trim().is_empty() {
        return reply(StatusCode::BAD_REQUEST, json!({"error": "name is required"}));
    }
    if body.pattern.trim().is_empty() {
        return reply(StatusCode::BAD_REQUEST, json!({"error": "pattern is required"}));
    }
    if !valid_action(&body.action) {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error": "action must be one of: send, board, schedule, shell", "got": body.action}),
        );
    }
    if body.action_target.trim().is_empty() {
        return reply(StatusCode::BAD_REQUEST, json!({"error": "action_target is required"}));
    }
    let creator = actor_of(&headers);
    let now = chrono::Utc::now().timestamp();
    let b = body.clone();
    let slot: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let res = state
        .store
        .write_async(move |conn| {
            ensure_subscription_tables(conn)?;
            conn.execute(
                "INSERT INTO signal_subscriptions \
                 (name, pattern, source_filter, session_filter, action, action_target, \
                  action_template, created_by, created_at, dedupe_window_s) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
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
            reply(StatusCode::CREATED, sub.unwrap_or(json!({"id": id})))
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

async fn update_subscription(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<Value>,
) -> Response {
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
            if let Some(v) = body.get("enabled") {
                let on = match v {
                    Value::Bool(b) => *b,
                    Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
                    _ => true,
                };
                sets.push(format!("enabled = ?{idx}"));
                params.push(Box::new(if on { 1i64 } else { 0 }));
                idx += 1;
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
    Path(id): Path<i64>,
) -> Response {
    let res = state
        .store
        .write_async(move |conn| {
            ensure_subscription_tables(conn)?;
            let n = conn.execute("DELETE FROM signal_subscriptions WHERE id = ?1", [id])?;
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
    crate::api::session_verbs::emit_event_store(
        &state.store,
        &session,
        &body.event_type,
        Some(body.data.clone()),
        None,
        &source,
    )
    .await;
    let fires = dispatch_subscriptions(
        &state,
        &body.event_type,
        &session,
        &source,
        &body.data,
    )
    .await;
    reply(
        StatusCode::OK,
        json!({
            "ok": true,
            "type": body.event_type,
            "session": session,
            "source": source,
            "subscriptions_fired": fires,
        }),
    )
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

struct MatchedSub {
    id: i64,
    name: String,
    action: String,
    action_target: String,
    action_template: String,
}

fn matching_subscriptions(
    conn: &Connection,
    event_type: &str,
    session: &str,
    source: &str,
) -> rusqlite::Result<Vec<MatchedSub>> {
    ensure_subscription_tables(conn)?;
    let now = chrono::Utc::now().timestamp();
    let mut stmt = conn.prepare(
        "SELECT id, name, pattern, source_filter, session_filter, action, action_target, \
         action_template, dedupe_window_s, last_fired_at \
         FROM signal_subscriptions WHERE enabled = 1",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, i64>(8)?,
            r.get::<_, Option<i64>>(9)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, name, pattern, src_f, sess_f, action, target, template, dedupe, last) = row?;
        if !glob_matches(&pattern, event_type) {
            continue;
        }
        if !src_f.is_empty() && !glob_matches(&src_f, source) {
            continue;
        }
        if !sess_f.is_empty() && !glob_matches(&sess_f, session) {
            continue;
        }
        if dedupe > 0 {
            if let Some(last_ts) = last {
                if now - last_ts < dedupe {
                    continue;
                }
            }
        }
        out.push(MatchedSub {
            id,
            name,
            action,
            action_target: target,
            action_template: template,
        });
    }
    Ok(out)
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

pub(crate) async fn dispatch_subscriptions(
    state: &AppState,
    event_type: &str,
    session: &str,
    source: &str,
    data: &Value,
) -> Vec<Value> {
    let et = event_type.to_string();
    let sess = session.to_string();
    let src = source.to_string();
    let matched = state
        .store
        .read_async(move |conn| Ok(matching_subscriptions(conn, &et, &sess, &src)?))
        .await;
    let subs = match matched {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(target: "amux::signals", event_type, error = %e, "subscription dispatch read failed");
            return vec![];
        }
    };
    let mut fired = Vec::new();
    for sub in subs {
        let text = render_template(&sub.action_template, event_type, data);
        let outcome = match sub.action.as_str() {
            "send" => {
                execute_send_action(state, &sub.action_target, &text).await
            }
            "board" => {
                execute_board_action(state, &sub.action_target, event_type, &text).await
            }
            "schedule" => {
                execute_schedule_action(state, &sub.action_target).await
            }
            "shell" => {
                execute_shell_action(&sub.action_target, &text).await
            }
            _ => "unsupported_action".to_string(),
        };
        let now = chrono::Utc::now().timestamp();
        let sub_id = sub.id;
        let et2 = event_type.to_string();
        let data_s = data.to_string();
        let action = sub.action.clone();
        let target = sub.action_target.clone();
        let outcome_w = outcome.clone();
        let _ = state
            .store
            .write_async(move |conn| {
                conn.execute(
                    "INSERT INTO signal_fires (subscription_id, event_type, event_data, fired_at, action, action_target, outcome) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    rusqlite::params![sub_id, et2, data_s, now, action, target, outcome_w],
                )?;
                conn.execute(
                    "UPDATE signal_subscriptions SET last_fired_at = ?1, fire_count = fire_count + 1 WHERE id = ?2",
                    rusqlite::params![now, sub_id],
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await;
        tracing::info!(target: "amux::signals",
            subscription = %sub.name, event_type, action = %sub.action,
            target = %sub.action_target, outcome = %outcome,
            measured = true, "subscription fired");
        fired.push(json!({
            "subscription": sub.name,
            "action": sub.action,
            "target": sub.action_target,
            "outcome": outcome,
        }));
    }
    fired
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
        Ok(_) => "delivered".to_string(),
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

// ---------------------------------------------------------------------------
// Hook: called from emit_event to check subscriptions
// ---------------------------------------------------------------------------

pub(crate) async fn on_event(
    state: &AppState,
    event_type: &str,
    session: &str,
    source: &str,
    data: Option<&Value>,
) {
    let empty = Value::Null;
    let data = data.unwrap_or(&empty);
    let _ = dispatch_subscriptions(state, event_type, session, source, data).await;
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
