-- Telegram connector: chat_id <-> session mapping, plus the poll checkpoint.
--
-- One row per LINKED Telegram chat. A chat links itself via `/link <session>`
-- sent to the bot (api/telegram.rs validates the session exists before
-- storing), so this table is never written blind from the Telegram side.
--
-- `telegram_poll_state` is a single-row checkpoint (id is CHECK'd to 1) for
-- the long-poll loop's `offset` — without a durable checkpoint, a server
-- restart mid-poll would either replay already-delivered messages (offset
-- too low) or silently skip whatever arrived during the restart window
-- (offset never advanced). Telegram's own `getUpdates` contract requires the
-- caller to track this; there is no server-side "since" the API remembers
-- for you.
CREATE TABLE IF NOT EXISTS telegram_mappings (
  chat_id INTEGER PRIMARY KEY,
  session TEXT NOT NULL,
  telegram_username TEXT,
  linked_at TEXT NOT NULL DEFAULT (datetime('now')),
  last_message_at TEXT
);

CREATE TABLE IF NOT EXISTS telegram_poll_state (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  last_update_id INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT OR IGNORE INTO telegram_poll_state (id, last_update_id) VALUES (1, 0);
