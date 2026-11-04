-- Pinned messages (conversations.rs, pin_post): a post pinned inside its
-- channel or DM, by whom and when. One row per pinned post; unpinning deletes
-- the row. A deleted post drops out of the pinned list (the listing joins on
-- deleted_at IS NULL) but its pin row is kept, so the record says who pinned it.
CREATE TABLE IF NOT EXISTS conversation_pins (
    post_id   INTEGER PRIMARY KEY,
    conv      TEXT NOT NULL,
    pinned_by TEXT NOT NULL,
    pinned_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_conversation_pins_conv ON conversation_pins (conv, pinned_at);
