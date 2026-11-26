-- Tables that used to be created at their point of use with
-- CREATE TABLE IF NOT EXISTS, moved here (primitive integrity audit
-- 2026-11-26, P1). They lived outside migrations because migrate.rs skipped a
-- version it had already applied without reading the name, so a migration
-- whose number a dirty build had consumed never ran. migrate.rs now detects
-- that mismatch and applies the registered migration, so the workaround is
-- gone. Each DDL is copied exactly from the code that used to create it, and
-- IF NOT EXISTS leaves every live table that the old code already created
-- untouched (no rewrite, no data change).
--
-- group_config (0001_baseline) and event_dispatch_cursor
-- (0157_event_subscriptions) were already created by migrations; only their
-- redundant point-of-use CREATEs were removed.

-- db/attempts.rs (RR-0052)
CREATE TABLE IF NOT EXISTS task_attempts (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    card        TEXT    NOT NULL,
    attempt     INTEGER NOT NULL,
    worker      TEXT    NOT NULL,
    generation  INTEGER NOT NULL,
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER,
    outcome     TEXT,
    to_status   TEXT,
    ended_by    TEXT,
    reason      TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_task_attempts_card_attempt ON task_attempts(card, attempt);
CREATE UNIQUE INDEX IF NOT EXISTS idx_task_attempts_running ON task_attempts(card) WHERE ended_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_task_attempts_worker ON task_attempts(worker, started_at);

-- runtime_jobs/episodes.rs
CREATE TABLE IF NOT EXISTS task_episodes (
    attempt_id       INTEGER PRIMARY KEY,
    card             TEXT    NOT NULL,
    attempt          INTEGER NOT NULL,
    worker           TEXT    NOT NULL,
    started_at       INTEGER NOT NULL,
    start_captured_at INTEGER NOT NULL,
    start_lag_s      INTEGER NOT NULL,
    start_after_end  INTEGER NOT NULL DEFAULT 0,
    card_start       TEXT    NOT NULL,
    work_dir         TEXT    NOT NULL DEFAULT '',
    start_sha        TEXT    NOT NULL DEFAULT '',
    start_branch     TEXT    NOT NULL DEFAULT '',
    start_dirty      INTEGER,
    conv_id          TEXT    NOT NULL DEFAULT '',
    provider         TEXT    NOT NULL DEFAULT '',
    model            TEXT    NOT NULL DEFAULT '',
    env_layers       TEXT    NOT NULL DEFAULT '[]',
    memory_sha256    TEXT    NOT NULL DEFAULT '',
    amux_build       TEXT    NOT NULL DEFAULT '',
    harness_version  TEXT    NOT NULL DEFAULT '',
    ended_at         INTEGER,
    end_captured_at  INTEGER,
    outcome          TEXT,
    to_status        TEXT,
    ended_by         TEXT,
    reason           TEXT,
    end_sha          TEXT,
    commits          INTEGER,
    diff_stat        TEXT,
    card_end         TEXT,
    transcripts      TEXT
);
CREATE INDEX IF NOT EXISTS idx_task_episodes_card ON task_episodes(card, attempt);
CREATE INDEX IF NOT EXISTS idx_task_episodes_worker ON task_episodes(worker, started_at);
CREATE INDEX IF NOT EXISTS idx_task_episodes_open ON task_episodes(attempt_id) WHERE ended_at IS NULL;

-- api/runner.rs (contract rule 6 budgets)
CREATE TABLE IF NOT EXISTS card_budgets (card TEXT PRIMARY KEY, lane TEXT NOT NULL, started_at REAL NOT NULL,
    nudges INTEGER NOT NULL DEFAULT 0, last_hash TEXT, repeats INTEGER NOT NULL DEFAULT 0, exhausted_at REAL);

-- api/file_visits.rs (Cmd-K files)
CREATE TABLE IF NOT EXISTS file_visits (
    path TEXT PRIMARY KEY, visits INTEGER NOT NULL DEFAULT 0, last_at REAL NOT NULL);

-- api/palette_history.rs (Cmd-K ranking)
CREATE TABLE IF NOT EXISTS nav_visits (
    kind TEXT NOT NULL, id TEXT NOT NULL, title TEXT NOT NULL DEFAULT '',
    visits INTEGER NOT NULL DEFAULT 0, last_at INTEGER NOT NULL, PRIMARY KEY (kind, id));
CREATE TABLE IF NOT EXISTS nav_picks (
    query TEXT NOT NULL, kind TEXT NOT NULL, id TEXT NOT NULL,
    picks INTEGER NOT NULL DEFAULT 0, last_at INTEGER NOT NULL, PRIMARY KEY (query, kind, id));

-- api/brex.rs (spend ledger for the card guard)
CREATE TABLE IF NOT EXISTS _amux_brex_spend (
    id INTEGER PRIMARY KEY, txn_id TEXT NOT NULL UNIQUE, card_id TEXT NOT NULL,
    amount_cents INTEGER NOT NULL, currency TEXT NOT NULL DEFAULT 'USD',
    merchant TEXT NOT NULL DEFAULT '', posted_at INTEGER NOT NULL, created_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS idx_brex_spend_card_time ON _amux_brex_spend(card_id, posted_at);
