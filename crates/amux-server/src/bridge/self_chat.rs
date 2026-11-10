//! The owner's own chats as a line into amux Messages (Ethan, 2026-11-09:
//! "I should be able to @ a worker or person/user or # channel in iMessage or
//! WhatsApp from my phone or email and it'll send that to the messages and
//! even get it to do the actual work").
//!
//! Three chats only the owner writes in: the WhatsApp "message yourself" chat
//! (`whatsapp_app`), the iMessage thread with their own number (`imessage`),
//! and email they send to themselves (`email`). A message there that starts
//! with `@` or `#` is a post, routed by its first word
//! (`conversations::resolve_self_chat`):
//!
//! - `@worker ...` posts in #general; the mention delivers it to the worker,
//!   which does the work and answers in the thread.
//! - `#channel ...` posts the rest in that channel (an @worker in it is
//!   delivered too).
//! - `@person ...` posts in the owner's direct message with that person.
//!
//! What comes back: every reply in a thread the chat started, and in a
//! direct message, the other person's later posts. Never the rest of a
//! channel. An email reply in the same thread is a reply in the Messages
//! thread, so a follow-up needs no @ again. Anything else in these chats (a
//! note to self, amux's own digests) is left alone.
//!
//! Verdicts: bridge_self_chat_posted, bridge_self_chat_unroutable.

use super::{adapters, label, Inbound, Outcome};
use crate::api::AppState;
use rusqlite::params;

pub const PLATFORMS: &[&str] = &[super::whatsapp_app::PLATFORM, super::imessage::PLATFORM, super::email::PLATFORM];

/// Roots older than this get no more answers sent back.
pub const ANSWER_WINDOW_MS: i64 = 7 * 24 * 3600 * 1000;

pub fn is_self_chat(platform: &str) -> bool {
    PLATFORMS.contains(&platform)
}

/// A message for amux: `@name` or `#name` first.
pub fn is_command(text: &str) -> bool {
    let mut c = text.trim_start().chars();
    matches!(c.next(), Some('@' | '#')) && c.next().is_some_and(|ch| ch.is_alphanumeric())
}

/// File the owner's message: as a reply in `parent`'s thread, or as a new
/// post wherever its first word routes it. `m.conversation` is where answers
/// go back (the chat, or for email the message to reply to).
pub async fn file(state: &AppState, mut m: Inbound, parent: Option<i64>) -> Outcome {
    if super::already_posted(state, &m.platform, &m.platform_msg_id) {
        return Outcome::Duplicate;
    }
    let routed = match parent {
        Some(root) => state.store.read().ok()
            .and_then(|c| c.query_row("SELECT conv FROM conversation_posts WHERE id=?1", params![root], |r| r.get::<_, String>(0)).ok())
            .map(|conv| (conv.clone(), conv, m.text.trim().to_string()))
            .ok_or_else(|| "That thread is gone from amux Messages.".to_string()),
        None => state.store.read().map_err(|e| e.to_string())
            .and_then(|c| crate::api::conversations::resolve_self_chat(&c, &m.text)),
    };
    let (conv, place, body) = match routed {
        Ok(r) => r,
        Err(why) => {
            tracing::info!(platform = %m.platform, why = %why, measured = true, n_considered = 1,
                verdict = "bridge_self_chat_unroutable", "bridge: a message from the owner's own chat had nowhere to go");
            adapters::send(&m.platform, &m.conversation, false, &why).await.ok();
            return Outcome::Ignored { why: "unroutable" };
        }
    };
    m.text = body;
    let out = super::post_inbound_in(state, &m, true, &conv, parent).await;
    if let Outcome::Posted { delivering_to, .. } = &out {
        if parent.is_none() && conv.starts_with("p:") {
            adapters::send(&m.platform, &m.conversation, false,
                &format!("Sent to {place} in amux Messages. What they write back comes here.")).await.ok();
        }
        tracing::info!(platform = %m.platform, conv, delivering_to, reply = parent.is_some(), measured = true, n_considered = 1,
            verdict = "bridge_self_chat_posted", "bridge: the owner's message from their own {} chat was posted", label(&m.platform));
    }
    out
}

/// Answers owed to the owner's own chats: replies in the threads they
/// started, and the other person's posts in a direct message they wrote in,
/// not yet sent (or failed under the cap, or approved).
pub(super) fn pending(conn: &rusqlite::Connection, max_attempts: i64, now: i64) -> rusqlite::Result<Vec<super::Pending>> {
    let list = PLATFORMS.iter().map(|p| format!("'{p}'")).collect::<Vec<_>>().join(",");
    let since = now - ANSWER_WINDOW_MS;
    let sql = format!(
        "SELECT p.id, p.author_key, p.author_name, p.body, p.conv, b.platform, b.conversation, o.attempts, o.state
         FROM conversation_posts p
         JOIN bridge_inbound b ON b.post_id = CASE WHEN p.parent_id IS NOT NULL THEN p.parent_id ELSE
             (SELECT MAX(r.post_id) FROM bridge_inbound r JOIN conversation_posts rp ON rp.id = r.post_id
              WHERE p.conv LIKE 'p:%' AND rp.conv = p.conv AND rp.parent_id IS NULL AND r.post_id < p.id
                AND r.platform IN ({list})) END
         LEFT JOIN bridge_outbound o ON o.id = p.id || '|' || b.platform || '|' || b.conversation
         WHERE p.id > (SELECT COALESCE(MIN(post_id), 0) FROM bridge_inbound WHERE platform IN ({list}) AND received_at > ?2)
           AND b.platform IN ({list}) AND b.received_at > ?2
           AND p.author_key <> 'person:owner'
           AND NOT EXISTS (SELECT 1 FROM bridge_inbound x WHERE x.post_id = p.id)
           AND (o.id IS NULL OR o.state = 'approved' OR (o.state = 'failed' AND o.attempts < ?1))
         ORDER BY p.id LIMIT 100");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![max_attempts, since], |r| {
        let (pid, platform, conversation): (i64, String, String) = (r.get(0)?, r.get(5)?, r.get(6)?);
        Ok(super::Pending {
            id: format!("{pid}|{platform}|{conversation}"), post_id: pid, author_key: r.get(1)?, author_name: r.get(2)?,
            body: r.get(3)?, conv: r.get(4)?, platform, conversation, is_group: false,
            attempts: r.get::<_, Option<i64>>(7)?.unwrap_or(0), state: r.get(8)?,
        })
    })?;
    Ok(rows.flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_starts_with_a_mention_or_a_channel() {
        assert!(is_command("@amux status?"));
        assert!(is_command("  #ops deploy went out"));
        assert!(!is_command("buy milk"));
        assert!(!is_command("Daily digest, Mon Nov 9"));
        assert!(!is_command("# heading"), "a markdown heading is not a channel");
        assert!(!is_command("@ 5pm"), "an at-sign alone is not a mention");
        assert!(!is_command("hello @amux"), "only the first word routes");
    }

    fn seed(store: &crate::db::Store, sql: String) {
        store.write(move |c| { c.execute_batch(&sql)?; Ok(crate::db::WriteOutcome { applied: true, events: vec![] }) }).unwrap();
    }

    /// From the owner's own chats, only answers come back: replies in the
    /// threads they started, and the other person's posts in a direct message
    /// they wrote in. Not the channel, not their own posts, not the chat's own
    /// messages, not someone else's thread, and nothing for an old root.
    #[test]
    fn only_answers_to_the_owners_own_chats_come_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("b.db")).unwrap();
        let now = super::super::now_ms();
        let old = now - ANSWER_WINDOW_MS - 1;
        seed(&store, format!(
            "INSERT INTO conversation_posts (id, conv, author_key, author_name, body, ts, parent_id) VALUES
               (1, 'g:general', 'person:owner', 'owner', '@amux status?', 0, NULL),
               (2, 'g:general', 'worker:amux', 'amux', 'all green', 0, 1),
               (3, 'g:general', 'worker:ops', 'ops', 'unrelated channel post', 0, NULL),
               (4, 'g:general', 'worker:ops', 'ops', 'in someone else''s thread', 0, 3),
               (5, 'p:doron@x.com|owner', 'person:owner', 'owner', '@doron deck?', 0, NULL),
               (6, 'p:doron@x.com|owner', 'person:doron@x.com', 'Doron', 'sent it', 0, NULL),
               (7, 'g:general', 'person:owner', 'owner', 'from the dashboard', 0, 1),
               (8, 'g:general', 'person:owner', 'owner', 'email follow-up', 0, 1),
               (9, 'g:general', 'worker:amux', 'amux', 'answer to the follow-up', 0, 1),
               (10, 'g:ops', 'person:owner', 'owner', '#ops old', 0, NULL),
               (11, 'g:ops', 'worker:ops', 'ops', 'late answer to an old root', 0, 10);
             INSERT INTO bridge_inbound (post_id, platform, conversation, platform_msg_id, sender_id, sender_name, is_owner, received_at) VALUES
               (1, 'imessage', '+1555', 'g1', '+1555', 'owner', 1, {now}),
               (5, 'email', '<m5@x>', 'e5', 'me', 'owner', 1, {now}),
               (8, 'email', '<m8@x>', 'e8', 'me', 'owner', 1, {now}),
               (10, 'whatsapp-app', '9@lid', 'w10', '1555', 'owner', 1, {old});"));
        let c = store.read().unwrap();
        let got: Vec<(i64, String)> = pending(&c, 8, now).unwrap().into_iter().map(|p| (p.post_id, p.conversation)).collect();
        assert_eq!(got, vec![(2, "+1555".to_string()), (6, "<m5@x>".to_string()), (9, "+1555".to_string())]);
    }
}
