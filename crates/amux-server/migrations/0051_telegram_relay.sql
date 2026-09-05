-- Telegram auto-relay tracking: prevent duplicate sends when sessions reply to Telegram messages.
--
-- When a session receives a Telegram-routed message, the auto-relay job monitors that session's
-- output and sends replies back to Telegram. This table tracks the last line we relayed for each
-- mapping, so we don't send the same text twice across server restarts or job re-runs.
--
-- `last_relayed_line` = the number of lines in the session's pane output when we last relayed a
--   message from this chat. Next relay will look for output AFTER this line number.
-- `last_relayed_at` = when we sent something back (informational, for observability).
-- `relay_error` = if the relay failed, the error message (cleared on success).

ALTER TABLE telegram_mappings ADD COLUMN last_relayed_line INTEGER DEFAULT 0;
ALTER TABLE telegram_mappings ADD COLUMN last_relayed_at TEXT;
ALTER TABLE telegram_mappings ADD COLUMN relay_error TEXT;
