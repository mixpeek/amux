-- Social posts amux published or was told about (api/social.rs), and one
-- metrics snapshot per post per window (24h, 72h, 7d) taken by the
-- social-metrics job. `measured = 0` keeps a failed read with its error, so a
-- missing number is never stored as a zero.
CREATE TABLE IF NOT EXISTS social_posts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    platform TEXT NOT NULL,
    account TEXT NOT NULL DEFAULT '',
    post_id TEXT NOT NULL,
    url TEXT NOT NULL DEFAULT '',
    caption TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL DEFAULT '',
    visibility TEXT NOT NULL DEFAULT '',
    file TEXT NOT NULL DEFAULT '',
    session TEXT NOT NULL DEFAULT '',
    origin TEXT NOT NULL DEFAULT 'api',
    posted_at INTEGER NOT NULL,
    UNIQUE (platform, post_id)
);

CREATE TABLE IF NOT EXISTS social_post_metrics (
    post_row INTEGER NOT NULL REFERENCES social_posts(id),
    win TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    metrics TEXT NOT NULL DEFAULT 'null',
    measured INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (post_row, win)
);
