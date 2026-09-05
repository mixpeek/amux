//! Telegram chat <-> session mapping store, over `telegram_mappings` +
//! `telegram_poll_state` (migration 0035).
//!
//! Deliberately NOT routed through the state-event journal (`PendingEvent`)
//! that `board`/`memories` use: a mapping is operator config, not a fact the
//! rest of the fleet needs to react to via SSE, and the audit weight those
//! modules carry (revisions, dedupe keys, delta sync) buys nothing here. If
//! that changes — e.g. the dashboard wants live mapping updates — promote it
//! then, don't pre-build it now (ethos rule 2).

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct TelegramMapping {
    pub chat_id: i64,
    pub session: String,
    pub telegram_username: Option<String>,
    pub linked_at: String,
    pub last_message_at: Option<String>,
    /// Where the MOST RECENT inbound message actually landed — the `/link`'d
    /// `session` by default, or an `@lane` target when the last message used
    /// one (migration 0040). NULL means "same as `session`"; callers that
    /// need "which pane is this chat's reply going to appear in" must read
    /// `routed_session()`, never `session` directly — see that method's doc.
    pub last_routed_session: Option<String>,
    /// Content hash of the last reply text actually sent to Telegram
    /// (migration 0038). The dedup gate: `telegram_relay::check_and_relay`
    /// skips sending when the freshly-extracted reply hashes the same as
    /// this. NOT a line-number/position checkpoint — `last_relayed_line`
    /// was that, and was found live 2026-08-30 to be write-only (never read
    /// back), so the relay resent the same reply every 30s tick for as long
    /// as it stayed the newest thing in the pane. A raw position is also the
    /// wrong shape for this: `tmux capture-pane -S -300` is a window sliding
    /// off the CURRENT bottom, so once a pane has >300 lines of scrollback
    /// its line count pins at 300 forever and a position-based comparison
    /// would either never fire again or always fire — a content hash has
    /// neither failure mode, since it only changes when the text to send
    /// actually does.
    pub last_relayed_hash: Option<String>,
    /// What Telegram called this chat at link time: "private", "group", or
    /// "supergroup" (migration 0054). Drives two group-only rules: only an
    /// `@lane`-addressed message is delivered (every other group message is
    /// silently ignored — the noise the group feature was blocked on), and
    /// `/link` in a group requires the target session already have a
    /// PRIVATE mapping (see `has_private_link`) — proves whoever links a
    /// group already controls a private line to that session first.
    pub chat_type: String,
}

impl TelegramMapping {
    /// The session whose pane the auto-relay job should be watching for this
    /// chat right now: the last `@lane` target if one is set, else the
    /// `/link`'d default. Reading `session` directly here is the exact bug
    /// fixed in migration 0040 — a reply typed by a lane reached only via
    /// `@mention` sits in that lane's pane forever, unseen, if the relay
    /// keeps watching the static `/link` target instead.
    pub fn routed_session(&self) -> &str {
        self.last_routed_session.as_deref().unwrap_or(&self.session)
    }

    pub fn is_group(&self) -> bool {
        self.chat_type == "group" || self.chat_type == "supergroup"
    }
}

fn row_to_mapping(r: &rusqlite::Row<'_>) -> rusqlite::Result<TelegramMapping> {
    Ok(TelegramMapping {
        chat_id: r.get(0)?,
        session: r.get(1)?,
        telegram_username: r.get(2)?,
        linked_at: r.get(3)?,
        last_message_at: r.get(4)?,
        last_routed_session: r.get(5)?,
        last_relayed_hash: r.get(6)?,
        chat_type: r.get(7)?,
    })
}

const COLS: &str = "chat_id, session, telegram_username, linked_at, last_message_at, \
                     last_routed_session, last_relayed_hash, chat_type";

pub fn list(conn: &Connection) -> rusqlite::Result<Vec<TelegramMapping>> {
    let mut stmt =
        conn.prepare(&format!("SELECT {COLS} FROM telegram_mappings ORDER BY linked_at DESC"))?;
    let rows = stmt.query_map([], row_to_mapping)?;
    rows.collect()
}

pub fn by_chat(conn: &Connection, chat_id: i64) -> rusqlite::Result<Option<TelegramMapping>> {
    conn.query_row(
        &format!("SELECT {COLS} FROM telegram_mappings WHERE chat_id = ?1"),
        params![chat_id],
        row_to_mapping,
    )
    .optional()
}

/// The most-recently-linked chat for a session. A session can only be the
/// TARGET of one active outbound mapping at a time in this MVP — if two
/// chats link the same session, the newer link wins for outbound sends
/// (inbound routing is unaffected: both chats still deliver in).
pub fn by_session(conn: &Connection, session: &str) -> rusqlite::Result<Option<TelegramMapping>> {
    conn.query_row(
        &format!(
            "SELECT {COLS} FROM telegram_mappings WHERE session = ?1 ORDER BY linked_at DESC LIMIT 1"
        ),
        params![session],
        row_to_mapping,
    )
    .optional()
}

pub fn upsert(
    conn: &Connection,
    chat_id: i64,
    session: &str,
    telegram_username: Option<&str>,
    chat_type: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO telegram_mappings (chat_id, session, telegram_username, linked_at, chat_type)
         VALUES (?1, ?2, ?3, datetime('now'), ?4)
         ON CONFLICT(chat_id) DO UPDATE SET
           session = excluded.session,
           telegram_username = COALESCE(excluded.telegram_username, telegram_mappings.telegram_username),
           linked_at = datetime('now'),
           chat_type = excluded.chat_type",
        params![chat_id, session, telegram_username, chat_type],
    )?;
    Ok(())
}

/// Does this session already have a PRIVATE-chat mapping? The eligibility
/// gate for linking a GROUP to it (Ethan, 2026-09-02) — a group link is only
/// accepted for a session that already has a private one, so pointing a
/// group at a session first requires controlling a private line to it.
/// Deliberately independent of `by_session` (which returns the newest
/// mapping regardless of type, for the outbound-send MVP limitation
/// documented on it) — this asks "does ANY private row exist", not "is the
/// newest row private".
pub fn has_private_link(conn: &Connection, session: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM telegram_mappings WHERE session = ?1 AND chat_type = 'private')",
        params![session],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n != 0)
}

/// Stamp which session THIS inbound message actually routed into (migration
/// 0040) — the default `session` or an `@lane` target. Always called, even
/// when the target equals the default, so `routed_session()` never has to
/// guess between "never set" and "explicitly the default".
///
/// When the target session CHANGES from what was routed last, `last_relayed_line`
/// resets to 0: it is a line-number checkpoint into a specific pane's tmux
/// capture, and a different session's pane has an unrelated line count. Carrying
/// the old number over would make the relay either resend old output (number
/// too low for the new pane) or skip real new output (number too high) —
/// wrong in the direction that depends on which pane happens to be longer, so
/// letting it ride is worse than a clean reset that watches the new pane from
/// its own start.
pub fn set_routed_session(conn: &Connection, chat_id: i64, session: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE telegram_mappings
         SET last_relayed_line = CASE
               WHEN COALESCE(last_routed_session, session) != ?1 THEN 0
               ELSE last_relayed_line
             END,
             last_relayed_hash = CASE
               WHEN COALESCE(last_routed_session, session) != ?1 THEN NULL
               ELSE last_relayed_hash
             END,
             last_routed_session = ?1
         WHERE chat_id = ?2",
        params![session, chat_id],
    )?;
    Ok(())
}

pub fn touch_last_message(conn: &Connection, chat_id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE telegram_mappings SET last_message_at = datetime('now') WHERE chat_id = ?1",
        params![chat_id],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, chat_id: i64) -> rusqlite::Result<bool> {
    let n = conn.execute("DELETE FROM telegram_mappings WHERE chat_id = ?1", params![chat_id])?;
    Ok(n > 0)
}

pub fn last_update_id(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT last_update_id FROM telegram_poll_state WHERE id = 1", [], |r| r.get(0))
}

pub fn set_last_update_id(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE telegram_poll_state SET last_update_id = ?1, updated_at = datetime('now') WHERE id = 1",
        params![id],
    )?;
    Ok(())
}

/// Mark a message as successfully relayed back to Telegram. Clears any
/// previous error. `hash` is the dedup gate (see `TelegramMapping::
/// last_relayed_hash`); `line_number` is kept only as an informational
/// "how deep was the capture" breadcrumb — nothing reads it as a checkpoint.
pub fn mark_relayed(
    conn: &Connection,
    chat_id: i64,
    line_number: i64,
    hash: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE telegram_mappings
         SET last_relayed_line = ?1, last_relayed_hash = ?2,
             last_relayed_at = datetime('now'), relay_error = NULL
         WHERE chat_id = ?3",
        params![line_number, hash, chat_id],
    )?;
    Ok(())
}

/// Record a relay error for this mapping (informational, for observability).
pub fn mark_relay_error(conn: &Connection, chat_id: i64, error: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE telegram_mappings SET relay_error = ?1 WHERE chat_id = ?2",
        params![error, chat_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        crate::db::migrate::apply_all(&mut c).unwrap();
        c
    }

    #[test]
    fn upsert_defaults_and_records_chat_type() {
        let c = conn();
        upsert(&c, 111, "amux", Some("ivo"), "private").unwrap();
        let m = by_chat(&c, 111).unwrap().unwrap();
        assert_eq!(m.chat_type, "private");
        assert!(!m.is_group());

        upsert(&c, 222, "amux", None, "supergroup").unwrap();
        let g = by_chat(&c, 222).unwrap().unwrap();
        assert_eq!(g.chat_type, "supergroup");
        assert!(g.is_group());
    }

    /// AMUX group-linking gate: a session with NO private link has
    /// `has_private_link` false — this is what `link_chat` refuses a group
    /// link over.
    #[test]
    fn has_private_link_false_until_a_private_row_exists() {
        let c = conn();
        assert!(!has_private_link(&c, "amux").unwrap());

        upsert(&c, 111, "amux", None, "private").unwrap();
        assert!(has_private_link(&c, "amux").unwrap());
    }

    /// A GROUP row alone must not satisfy the gate — the whole point is that
    /// a private link has to exist FIRST, so `has_private_link` cannot be
    /// satisfied by the group row it is meant to gate.
    #[test]
    fn has_private_link_ignores_group_rows() {
        let c = conn();
        upsert(&c, 222, "amux", None, "group").unwrap();
        assert!(!has_private_link(&c, "amux").unwrap());
    }

    /// Re-`/link`ing a chat (same chat_id, different session) must not
    /// silently change its recorded chat_type — the ON CONFLICT arm updates
    /// `session` unconditionally but `chat_type` should track whatever the
    /// CALLER passes this time, since a real chat's type cannot change
    /// between calls (Telegram tells us on every message) and this is the
    /// same "re-link repoints, doesn't half-update" contract the existing
    /// session/telegram_username fields already have.
    #[test]
    fn relinking_updates_chat_type_too() {
        let c = conn();
        upsert(&c, 111, "amux", None, "private").unwrap();
        upsert(&c, 111, "amux", None, "private").unwrap(); // idempotent re-link
        let m = by_chat(&c, 111).unwrap().unwrap();
        assert_eq!(m.chat_type, "private");
    }

    #[test]
    fn a_session_can_have_both_a_private_and_a_group_link() {
        let c = conn();
        upsert(&c, 111, "amux", None, "private").unwrap();
        upsert(&c, 222, "amux", None, "supergroup").unwrap();
        let mut types: Vec<String> = list(&c).unwrap().into_iter().map(|m| m.chat_type).collect();
        types.sort();
        assert_eq!(types, vec!["private".to_string(), "supergroup".to_string()]);
    }
}
