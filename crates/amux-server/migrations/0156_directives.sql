-- An instruction a named sender may give a worker outside its groups: it
-- interrupts the worker, and the worker has to answer it (api/directive.rs).
CREATE TABLE IF NOT EXISTS directives (
    id           TEXT PRIMARY KEY,
    session      TEXT NOT NULL,
    sender       TEXT NOT NULL,
    text         TEXT NOT NULL,
    issued_at    REAL NOT NULL,
    delivered_at REAL,
    tries        INTEGER NOT NULL DEFAULT 0,
    detail       TEXT NOT NULL DEFAULT '',
    resent_at    REAL,
    told_at      REAL,
    acked_at     REAL,
    ack_by       TEXT,
    ack_note     TEXT
);
CREATE INDEX IF NOT EXISTS directives_open ON directives (acked_at, session);
