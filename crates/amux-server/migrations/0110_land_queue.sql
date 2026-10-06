-- Contract rule 5 (AH-380): the server-owned land queue. A lane enqueues a
-- commit; the server composes queued commits onto origin/main in its own
-- worktree, runs the land gate and the repo's pre-push hook, and pushes, or
-- returns the output to the lane without merging.
CREATE TABLE IF NOT EXISTS land_queue (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    repo       TEXT NOT NULL,
    lane       TEXT NOT NULL,
    sha        TEXT NOT NULL,
    priority   INTEGER NOT NULL DEFAULT 0,
    reason     TEXT,
    state      TEXT NOT NULL DEFAULT 'queued',
    merged_sha TEXT,
    output     TEXT,
    queued_at  REAL NOT NULL,
    started_at REAL,
    done_at    REAL
);
CREATE INDEX IF NOT EXISTS land_queue_state ON land_queue (state, repo);
