-- Event subscriptions in plain words (docs/design/event-subscriptions.md):
-- the words as written, the filter they compiled to, who the subscription
-- belongs to, whether the helper model judges each match, what makes two
-- events the same occurrence, and the #general thread its notices go to.
ALTER TABLE signal_subscriptions ADD COLUMN criteria TEXT NOT NULL DEFAULT '';
ALTER TABLE signal_subscriptions ADD COLUMN filter TEXT NOT NULL DEFAULT '';
ALTER TABLE signal_subscriptions ADD COLUMN owner TEXT NOT NULL DEFAULT '';
ALTER TABLE signal_subscriptions ADD COLUMN judge INTEGER NOT NULL DEFAULT 0;
ALTER TABLE signal_subscriptions ADD COLUMN dedupe_key TEXT NOT NULL DEFAULT '';
ALTER TABLE signal_subscriptions ADD COLUMN notify_post INTEGER;
ALTER TABLE signal_fires ADD COLUMN event_id INTEGER;
ALTER TABLE signal_fires ADD COLUMN dedupe_key TEXT NOT NULL DEFAULT '';

-- Where the dispatcher has read up to, per ledger, so a restart resumes.
CREATE TABLE IF NOT EXISTS event_dispatch_cursor (
    name       TEXT PRIMARY KEY,
    last_id    INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
