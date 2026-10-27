-- The chat bridge (bridge/mod.rs): an external conversation (WhatsApp, Slack,
-- Telegram, Discord) mirrored onto an amux Messages conversation, a worker's
-- thread (w:<worker>) or a group's channel (g:<group>). Times are epoch
-- milliseconds.

-- One external conversation linked to one Messages conversation. Posts in
-- `conv` newer than `last_post_id` are forwarded to the chat, so history from
-- before the link is not.
CREATE TABLE IF NOT EXISTS bridge_links (
  platform     TEXT NOT NULL,
  conversation TEXT NOT NULL,
  kind         TEXT NOT NULL DEFAULT 'dm',
  conv         TEXT NOT NULL,
  last_post_id INTEGER NOT NULL DEFAULT 0,
  linked_by    TEXT NOT NULL,
  linked_at    INTEGER NOT NULL,
  PRIMARY KEY (platform, conversation)
);
CREATE INDEX IF NOT EXISTS idx_bridge_links_conv ON bridge_links (conv);

-- Every platform message the bridge posted, by its Messages post id. The
-- platform id makes a webhook the platform retries a no-op, and the post id
-- keeps the bridge from echoing a chat's own message back to it.
CREATE TABLE IF NOT EXISTS bridge_inbound (
  post_id         INTEGER PRIMARY KEY,
  platform        TEXT NOT NULL,
  conversation    TEXT NOT NULL,
  platform_msg_id TEXT NOT NULL,
  sender_id       TEXT NOT NULL,
  sender_name     TEXT NOT NULL DEFAULT '',
  is_owner        INTEGER NOT NULL DEFAULT 0,
  received_at     INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_bridge_inbound_pmid ON bridge_inbound (platform, platform_msg_id);
CREATE INDEX IF NOT EXISTS idx_bridge_inbound_conv ON bridge_inbound (platform, conversation);

-- Every post forwarded (or waiting to be) to one linked chat: sent, held for
-- the owner (the chat includes someone else), or failed and retried.
CREATE TABLE IF NOT EXISTS bridge_outbound (
  id              TEXT PRIMARY KEY,            -- <post>|<platform>|<conversation>
  post_id         INTEGER NOT NULL,
  platform        TEXT NOT NULL,
  conversation    TEXT NOT NULL,
  state           TEXT NOT NULL,
  attempts        INTEGER NOT NULL DEFAULT 0,
  error           TEXT,
  platform_msg_id TEXT,
  card            TEXT,
  created_at      INTEGER NOT NULL,
  sent_at         INTEGER
);
CREATE INDEX IF NOT EXISTS idx_bridge_outbound_state ON bridge_outbound (state);

-- Telegram links made before the bridge carry over, to the worker's thread,
-- watermarked at today's last post.
INSERT OR IGNORE INTO bridge_links (platform, conversation, kind, conv, last_post_id, linked_by, linked_at)
  SELECT 'telegram', CAST(chat_id AS TEXT), CASE WHEN chat_id < 0 THEN 'group' ELSE 'dm' END, 'w:' || session,
         COALESCE((SELECT MAX(id) FROM conversation_posts), 0),
         'telegram_mappings', CAST(strftime('%s', linked_at) AS INTEGER) * 1000
  FROM telegram_mappings;
