//! Telegram's reads over the chat bridge's link table, plus the poll's
//! `telegram_poll_state` checkpoint (migration 0035).
//!
//! A Telegram chat's link is a `bridge_links` row with `platform =
//! 'telegram'`, the same table every other platform uses (bridge/mod.rs).
//! Writes go through `crate::bridge::save_link` and `follow_link`; this module
//! only answers the questions the poll and `/api/telegram/send` ask in
//! Telegram's terms: which worker does this chat reach, and which chat does
//! this worker answer to. Migration 0178 folded Telegram's old separate link
//! table in here.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

pub const PLATFORM: &str = "telegram";

/// One Telegram chat linked to a worker's thread.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TelegramLink {
    pub chat_id: i64,
    /// The worker a message with no `@worker` mention goes to: the one the
    /// chat was `/link`'d to (`default_conv`, else `conv`).
    pub session: String,
    /// Where the bridge sends answers now: the worker the last message went
    /// to, which an `@worker` mention can move off `session`.
    pub conv: String,
    pub is_group: bool,
}

fn row_to_link(r: &rusqlite::Row<'_>) -> rusqlite::Result<TelegramLink> {
    let conversation: String = r.get(0)?;
    let home: String = r.get(1)?;
    Ok(TelegramLink {
        chat_id: conversation.parse().unwrap_or_default(),
        session: home.strip_prefix("w:").unwrap_or(&home).to_string(),
        conv: r.get(2)?,
        is_group: r.get::<_, String>(3)? == "group",
    })
}

/// Only a link to a worker's thread routes through the poll: a chat linked to
/// a group's channel has no default worker.
const SELECT: &str = "SELECT conversation, COALESCE(default_conv, conv), conv, kind FROM bridge_links
     WHERE platform = 'telegram' AND COALESCE(default_conv, conv) LIKE 'w:%'";

pub fn by_chat(conn: &Connection, chat_id: i64) -> rusqlite::Result<Option<TelegramLink>> {
    conn.query_row(&format!("{SELECT} AND conversation = ?1"), params![chat_id.to_string()], row_to_link)
        .optional()
}

/// The most recently linked chat whose default worker is `session`. When two
/// chats link the same worker, the newer one is where a send by session goes
/// (both still deliver in).
pub fn by_session(conn: &Connection, session: &str) -> rusqlite::Result<Option<TelegramLink>> {
    conn.query_row(
        &format!("{SELECT} AND COALESCE(default_conv, conv) = 'w:' || ?1 ORDER BY linked_at DESC LIMIT 1"),
        params![session],
        row_to_link,
    )
    .optional()
}

pub fn count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM bridge_links WHERE platform = 'telegram'", [], |r| r.get(0))
}

/// Does this worker already have a PRIVATE Telegram chat linked to it? The
/// eligibility gate for linking a GROUP to it (Ethan, 2026-09-02): pointing a
/// group at a worker first requires controlling a private line to it. Asks
/// "does ANY private link exist", not "is the newest link private".
pub fn has_private_link(conn: &Connection, session: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM bridge_links WHERE platform = 'telegram' AND kind = 'dm'
                         AND COALESCE(default_conv, conv) = 'w:' || ?1)",
        params![session],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n != 0)
}

pub fn last_update_id(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT last_update_id FROM telegram_poll_state WHERE id = 1",
        [],
        |r| r.get(0),
    )
}

pub fn set_last_update_id(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE telegram_poll_state SET last_update_id = ?1, updated_at = datetime('now') WHERE id = 1",
        params![id],
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

    fn link(c: &Connection, chat: i64, kind: &str, conv: &str, default_conv: Option<&str>, at: i64) {
        c.execute(
            "INSERT INTO bridge_links (platform, conversation, kind, conv, default_conv, linked_by, linked_at)
             VALUES ('telegram', ?1, ?2, ?3, ?4, 'telegram_poll', ?5)",
            params![chat.to_string(), kind, conv, default_conv, at],
        )
        .unwrap();
    }

    #[test]
    fn a_link_reads_back_with_its_default_worker_and_where_answers_go() {
        let c = conn();
        link(&c, 111, "dm", "w:other", Some("w:amux"), 1);
        let l = by_chat(&c, 111).unwrap().unwrap();
        assert_eq!(l, TelegramLink { chat_id: 111, session: "amux".into(), conv: "w:other".into(), is_group: false });
        assert_eq!(by_session(&c, "amux").unwrap().map(|l| l.chat_id), Some(111));
        assert_eq!(by_session(&c, "other").unwrap(), None, "a mention does not make the chat the other worker's");
    }

    #[test]
    fn a_chat_linked_to_a_group_channel_is_not_a_worker_link() {
        let c = conn();
        link(&c, 333, "dm", "g:team", None, 1);
        assert_eq!(by_chat(&c, 333).unwrap(), None);
        assert_eq!(count(&c).unwrap(), 1);
    }

    #[test]
    fn has_private_link_false_until_a_private_row_exists() {
        let c = conn();
        assert!(!has_private_link(&c, "amux").unwrap());
        link(&c, 111, "dm", "w:amux", None, 1);
        assert!(has_private_link(&c, "amux").unwrap());
    }

    /// A GROUP link alone must not satisfy the gate it exists to guard.
    #[test]
    fn has_private_link_ignores_group_rows() {
        let c = conn();
        link(&c, -222, "group", "w:amux", None, 1);
        assert!(!has_private_link(&c, "amux").unwrap());
        assert!(by_chat(&c, -222).unwrap().unwrap().is_group);
    }

    /// The gate follows the /link default, not wherever the last @mention
    /// moved the chat's answers.
    #[test]
    fn has_private_link_reads_the_default_worker() {
        let c = conn();
        link(&c, 111, "dm", "w:other", Some("w:amux"), 1);
        assert!(has_private_link(&c, "amux").unwrap());
        assert!(!has_private_link(&c, "other").unwrap());
    }
}
