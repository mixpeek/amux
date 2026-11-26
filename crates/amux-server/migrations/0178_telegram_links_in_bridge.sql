-- Telegram chat links live in bridge_links, like every other platform's
-- (bridge/mod.rs: "a new platform is an adapter, never a new subsystem").
-- telegram_mappings was a second link table beside it, left from before the
-- bridge: the poll read its routing from there while the bridge sent answers
-- from bridge_links (primitive integrity audit 2026-11-26, P1).
--
-- Telegram's poll routes each message itself: an @worker mention goes to that
-- worker, and its answer must come back, so the bridge repoints `conv` to it.
-- `default_conv` keeps where a message with no mention goes, the worker the
-- chat was /link'd to. NULL means `conv`.
ALTER TABLE bridge_links ADD COLUMN default_conv TEXT;

-- A chat the bridge already linked (it saves the link on the chat's first
-- message): keep `conv`, record the /link default and the chat's real type.
UPDATE bridge_links
   SET default_conv = NULLIF((SELECT 'w:' || m.session FROM telegram_mappings m
                              WHERE m.chat_id = CAST(bridge_links.conversation AS INTEGER)), conv),
       kind = (SELECT CASE WHEN m.chat_type IN ('group', 'supergroup') THEN 'group' ELSE 'dm' END
                 FROM telegram_mappings m WHERE m.chat_id = CAST(bridge_links.conversation AS INTEGER))
 WHERE platform = 'telegram'
   AND EXISTS (SELECT 1 FROM telegram_mappings m WHERE m.chat_id = CAST(bridge_links.conversation AS INTEGER));

-- A chat only telegram_mappings knew: link it to the worker it last routed to,
-- watermarked at today's last post so older history is not sent to the chat.
INSERT INTO bridge_links (platform, conversation, kind, conv, default_conv, last_post_id, linked_by, linked_at)
SELECT 'telegram', CAST(m.chat_id AS TEXT),
       CASE WHEN m.chat_type IN ('group', 'supergroup') THEN 'group' ELSE 'dm' END,
       'w:' || COALESCE(m.last_routed_session, m.session),
       CASE WHEN COALESCE(m.last_routed_session, m.session) != m.session THEN 'w:' || m.session END,
       COALESCE((SELECT MAX(id) FROM conversation_posts), 0),
       'telegram_poll',
       COALESCE(CAST(strftime('%s', m.linked_at) AS INTEGER) * 1000, 0)
  FROM telegram_mappings m
 WHERE NOT EXISTS (SELECT 1 FROM bridge_links l
                    WHERE l.platform = 'telegram' AND l.conversation = CAST(m.chat_id AS TEXT));

-- The poll's own links were tagged with the old table's name.
UPDATE bridge_links SET linked_by = 'telegram_poll' WHERE linked_by = 'telegram_mappings';

DROP TABLE telegram_mappings;
