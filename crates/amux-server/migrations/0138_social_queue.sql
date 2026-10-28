-- Social posts waiting to be published (api/social_publish.rs). One row per
-- platform; a post sent to several platforms shares a group_id. The
-- social-publish job claims a row when it is due and moves it to posted,
-- failed (with the error), or uncertain: the platform may have received it, so
-- it is never sent again on its own. `draft` holds the request as JSON.
CREATE TABLE IF NOT EXISTS social_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id TEXT NOT NULL,
    platform TEXT NOT NULL,
    account TEXT NOT NULL DEFAULT '',
    draft TEXT NOT NULL,
    scheduled_at INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued',
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    error TEXT,
    notes TEXT,
    claimed_at INTEGER,
    claimed_by TEXT,
    post_row INTEGER REFERENCES social_posts(id),
    post_id TEXT NOT NULL DEFAULT '',
    url TEXT NOT NULL DEFAULT '',
    session TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS social_queue_due ON social_queue (status, next_attempt_at);
CREATE INDEX IF NOT EXISTS social_queue_group ON social_queue (group_id);
