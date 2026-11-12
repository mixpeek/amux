-- Outbound phone calls (docs/design/phone-calls.md, api/calls.rs). One row per
-- call a worker asked for. status walks requested -> held -> approved ->
-- dialing -> in_progress -> completed | no_answer | busy | failed | canceled
-- (or declined, when the owner says no). Timestamps are epoch seconds.
-- result_token_hash is the sha256 of the per-call token the bridge must
-- present to report on this call; the token itself is never stored.
CREATE TABLE IF NOT EXISTS calls (
    id                TEXT PRIMARY KEY,
    worker            TEXT NOT NULL DEFAULT '',
    to_number         TEXT NOT NULL,
    contact           TEXT,
    goal              TEXT NOT NULL DEFAULT '',
    context           TEXT NOT NULL DEFAULT '',
    instructions      TEXT NOT NULL DEFAULT '',
    max_minutes       INTEGER NOT NULL DEFAULT 10,
    status            TEXT NOT NULL DEFAULT 'requested',
    outcome           TEXT NOT NULL DEFAULT '',
    summary           TEXT NOT NULL DEFAULT '',
    transcript        TEXT NOT NULL DEFAULT '[]',
    duration_s        INTEGER,
    provider_sid      TEXT NOT NULL DEFAULT '',
    result_token_hash TEXT NOT NULL DEFAULT '',
    approved_by       TEXT NOT NULL DEFAULT '',
    requested_at      REAL NOT NULL,
    approved_at       REAL,
    started_at        REAL,
    ended_at          REAL,
    error             TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS calls_requested_at ON calls(requested_at DESC);
CREATE INDEX IF NOT EXISTS calls_worker ON calls(worker, requested_at DESC);
CREATE INDEX IF NOT EXISTS calls_status ON calls(status);
