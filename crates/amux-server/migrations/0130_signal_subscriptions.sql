-- Signal subscriptions: event-driven worker coordination.
--
-- A subscription binds an event pattern to an action. When an event matching
-- the pattern fires, the action executes (send a message, create a board card,
-- trigger a schedule, run a shell command).

CREATE TABLE IF NOT EXISTS signal_subscriptions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT    NOT NULL UNIQUE,
    pattern         TEXT    NOT NULL,
    source_filter   TEXT    NOT NULL DEFAULT '',
    session_filter  TEXT    NOT NULL DEFAULT '',
    action          TEXT    NOT NULL,
    action_target   TEXT    NOT NULL,
    action_template TEXT    NOT NULL DEFAULT '',
    created_by      TEXT    NOT NULL DEFAULT '',
    created_at      INTEGER NOT NULL,
    enabled         INTEGER NOT NULL DEFAULT 1,
    dedupe_window_s INTEGER NOT NULL DEFAULT 300,
    last_fired_at   INTEGER,
    fire_count      INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS signal_fires (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    subscription_id INTEGER NOT NULL REFERENCES signal_subscriptions(id),
    event_type      TEXT    NOT NULL,
    event_data      TEXT,
    fired_at        INTEGER NOT NULL,
    action          TEXT    NOT NULL,
    action_target   TEXT    NOT NULL,
    outcome         TEXT    NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_signal_subs_pattern ON signal_subscriptions(pattern);
CREATE INDEX IF NOT EXISTS idx_signal_subs_enabled ON signal_subscriptions(enabled);
CREATE INDEX IF NOT EXISTS idx_signal_fires_sub    ON signal_fires(subscription_id, fired_at DESC);
CREATE INDEX IF NOT EXISTS idx_signal_fires_ts     ON signal_fires(fired_at DESC);
