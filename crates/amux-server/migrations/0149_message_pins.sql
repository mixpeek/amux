-- Pinned worker messages (history.rs, pin_message): one row per pinned
-- cmd_history message. A message that is gone drops out of the list by the join.
CREATE TABLE IF NOT EXISTS message_pins (
    msg_id    INTEGER PRIMARY KEY,
    session   TEXT NOT NULL,
    pinned_by TEXT NOT NULL,
    pinned_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_message_pins_session ON message_pins(session, pinned_at);
