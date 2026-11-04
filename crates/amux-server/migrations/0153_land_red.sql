-- Z5: what happened after a land reached main. `kind` says a queue row is a
-- revert the queue built, `ci_state` that its push has been judged once. One
-- land_red row per repository and red sha: the UNIQUE key is what stops a
-- second revert of the same red.
-- ADDCOL: land_queue kind TEXT NOT NULL DEFAULT 'land'
-- ADDCOL: land_queue ci_state TEXT
CREATE TABLE IF NOT EXISTS land_red (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    repo           TEXT NOT NULL,
    red_sha        TEXT NOT NULL,
    base_sha       TEXT NOT NULL,
    land_id        INTEGER NOT NULL,
    lanes          TEXT NOT NULL,
    tree           TEXT,
    state          TEXT NOT NULL,
    tries          INTEGER NOT NULL DEFAULT 0,
    detail         TEXT NOT NULL DEFAULT '',
    revert_land_id INTEGER,
    cards          TEXT,
    opened_at      REAL NOT NULL,
    updated_at     REAL NOT NULL,
    told_at        REAL,
    UNIQUE (repo, red_sha)
);
CREATE INDEX IF NOT EXISTS land_queue_unjudged ON land_queue (state, ci_state);
