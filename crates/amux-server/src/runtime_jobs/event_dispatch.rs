//! The event-subscription dispatcher: tails the event ledger and fires
//! matching subscriptions (docs/design/event-subscriptions.md).
//!
//! WHY A TAIL. Subscriptions used to be checked inside `emit_event`, but 82
//! places write `session_events` directly (every browser event among them),
//! so most activity never reached a subscription. Reading the ledger itself
//! covers every writer, now and later. The board's change log is tailed the
//! same way, as `board.card.*` events.
//!
//! RESTART. The cursors are stored (`event_dispatch_cursor`), so a restart
//! resumes where it stopped and the events written while the server was down
//! still fire. A first start has no cursor and begins at the head, so history
//! does not fire. A backlog bigger than [`MAX_REPLAY`] after a long outage is
//! skipped to the head and logged as `event_dispatch_gap_skipped` with its
//! size, because notices about hours-old events would bury the current ones.

use serde_json::Value;
use std::collections::HashMap;

use super::registry::ids;
use crate::api::AppState;
use crate::db::WriteOutcome;

const JOB: &str = ids::EVENT_DISPATCH;
const BATCH: i64 = 500;
pub(crate) const MAX_REPLAY: i64 = 5_000;
const EVENTS: &str = "session_events";
const BOARD: &str = "board_change_log";

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn head(conn: &rusqlite::Connection, table: &str) -> rusqlite::Result<i64> {
    let col = if table == BOARD { "seq" } else { "id" };
    conn.query_row(&format!("SELECT COALESCE(MAX({col}),0) FROM {table}"), [], |r| r.get(0))
}

fn stored(conn: &rusqlite::Connection, name: &str) -> rusqlite::Result<Option<i64>> {
    use rusqlite::OptionalExtension as _;
    conn.query_row("SELECT last_id FROM event_dispatch_cursor WHERE name = ?1", [name], |r| r.get(0)).optional()
}

/// Where to read from: the stored cursor, or the head on a first start or
/// after a backlog too big to replay. Returns (cursor, skipped rows).
pub(crate) fn start_at(stored: Option<i64>, head: i64) -> (i64, i64) {
    match stored {
        None => (head, 0),
        Some(c) if head - c > MAX_REPLAY => (head, head - c),
        Some(c) => (c.min(head), 0),
    }
}

async fn save(state: &AppState, name: &'static str, id: i64) {
    let now = chrono::Utc::now().timestamp();
    let saved = state
        .store
        .write_async(move |conn| {
            crate::api::event_subs::ensure_columns(conn)?;
            conn.execute(
                "INSERT INTO event_dispatch_cursor (name, last_id, updated_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(name) DO UPDATE SET last_id = excluded.last_id, updated_at = excluded.updated_at",
                rusqlite::params![name, id, now],
            )?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    // An unsaved cursor costs a replay after the next restart, never a lost
    // event; say so, because the replay would look like duplicate fires.
    if let Err(e) = saved {
        tracing::warn!(job = JOB, table = name, last_id = id, error = %e, verdict = "event_dispatch_cursor_unsaved",
            "event dispatcher could not store its cursor; a restart will replay from the last saved one");
    }
}

struct Row {
    id: i64,
    event_type: String,
    session: String,
    source: String,
    data: Value,
}

/// Fire every subscription for every ledger row after the cursors. Returns the
/// fires per `session_events` id, for the emit route's reply.
pub async fn drain(state: &AppState) -> HashMap<i64, Vec<Value>> {
    drain_including(state, None).await
}

/// [`drain`], making sure event `id` is read even when no cursor is stored
/// yet (a first start begins at the head, which would be just past it).
pub async fn drain_including(state: &AppState, id: Option<i64>) -> HashMap<i64, Vec<Value>> {
    let _g = LOCK.lock().await;
    let mut out: HashMap<i64, Vec<Value>> = HashMap::new();
    let read = state
        .store
        .read_async(|conn| {
            crate::api::event_subs::ensure_columns(conn)?;
            let subs = crate::api::signals::load_live_subs(conn)?;
            let ev_head = head(conn, EVENTS)?;
            let bd_head = head(conn, BOARD).unwrap_or(0);
            Ok((subs, ev_head, bd_head, stored(conn, EVENTS).unwrap_or(None), stored(conn, BOARD).unwrap_or(None)))
        })
        .await;
    let Ok((mut subs, ev_head, bd_head, ev_stored, bd_stored)) = read else {
        return out;
    };
    for (name, h, s) in [(EVENTS, ev_head, ev_stored), (BOARD, bd_head, bd_stored)] {
        let (mut from, skipped) = start_at(s, h);
        if name == EVENTS && s.is_none() {
            if let Some(i) = id {
                from = from.min(i - 1);
            }
        }
        if skipped > 0 {
            tracing::warn!(job = JOB, table = name, skipped, measured = true, n_considered = skipped,
                verdict = "event_dispatch_gap_skipped",
                "event dispatcher found a backlog too big to replay after a stop; these events fired no subscription");
        }
        if s != Some(from) {
            save(state, name, from).await;
        }
        if subs.is_empty() {
            // Nothing listens: move the cursor without reading the rows.
            if from < h {
                save(state, name, h).await;
            }
            continue;
        }
        let mut cursor = from;
        while cursor < h {
            let after = cursor;
            let rows = state
                .store
                .read_async(move |conn| Ok(read_rows(conn, name, after)?))
                .await
                .unwrap_or_default();
            if rows.is_empty() {
                break;
            }
            for r in &rows {
                if r.event_type.is_empty() {
                    cursor = r.id;
                    continue;
                }
                let fired = crate::api::signals::dispatch_one(state, &mut subs, (name == EVENTS).then_some(r.id),
                    &r.event_type, &r.session, &r.source, &r.data).await;
                if name == EVENTS && !fired.is_empty() {
                    out.insert(r.id, fired);
                }
                cursor = r.id;
            }
            save(state, name, cursor).await;
        }
    }
    out
}

/// seq, card id, operation, old status, new status, changed by.
type BoardRow = (i64, String, String, Option<String>, Option<String>, Option<String>);

fn read_rows(conn: &rusqlite::Connection, table: &str, after: i64) -> rusqlite::Result<Vec<Row>> {
    if table == BOARD {
        let mut stmt = conn.prepare(
            "SELECT seq, row_id, operation, old_status, new_status, changed_by FROM board_change_log \
             WHERE seq > ?1 AND table_name = 'issues' ORDER BY seq ASC LIMIT ?2",
        )?;
        let raw: Vec<BoardRow> = stmt
            .query_map(rusqlite::params![after, BATCH], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<Result<_, _>>()?;
        // Rows of other tables still advance the cursor.
        let last_any: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq), ?1) FROM (SELECT seq FROM board_change_log WHERE seq > ?1 ORDER BY seq ASC LIMIT ?2)",
            rusqlite::params![after, BATCH], |r| r.get(0))?;
        let mut rows: Vec<Row> = raw
            .into_iter()
            .map(|(seq, id, op, old, new, by)| {
                let (t, session, data) = crate::api::event_subs::board_event(conn, &id, &op, old.as_deref(), new.as_deref(), by.as_deref());
                Row { id: seq, event_type: t, session, source: "board".into(), data }
            })
            .collect();
        if rows.last().map(|r| r.id).unwrap_or(after) < last_any {
            rows.push(Row { id: last_any, event_type: String::new(), session: String::new(), source: String::new(), data: Value::Null });
        }
        return Ok(rows);
    }
    let mut stmt = conn.prepare(
        "SELECT id, type, session, source, data FROM session_events WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![after, BATCH], |r| {
        let data: Option<String> = r.get(4)?;
        Ok(Row {
            id: r.get(0)?,
            event_type: r.get(1)?,
            session: r.get(2)?,
            source: r.get(3)?,
            data: data.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null),
        })
    })?;
    rows.collect()
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic_every(JOB, std::time::Duration::from_secs(1), move || {
        let state = state.clone();
        async move {
            drain(&state).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restart_resumes_and_a_huge_backlog_skips_to_the_head() {
        assert_eq!(start_at(None, 900), (900, 0), "first start: history does not fire");
        assert_eq!(start_at(Some(850), 900), (850, 0), "a restart replays what it missed");
        assert_eq!(start_at(Some(10), 10 + MAX_REPLAY + 1), (10 + MAX_REPLAY + 1, MAX_REPLAY + 1), "too far behind: skip and say how much");
        assert_eq!(start_at(Some(950), 900), (900, 0), "a cursor past the head (a restored database) comes back to it");
    }
}
