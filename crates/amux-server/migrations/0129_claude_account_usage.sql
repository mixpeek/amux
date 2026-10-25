-- Per-account Claude plan usage over time (2026-10-20). One row per reading
-- of one account: the rotator's /usage probe and status lines, and the
-- server's own /api/usage probe of whichever login is current. The rotator's
-- accounts.json keeps only the latest reading per account; this keeps the
-- history, so burn rates and "when did this account last have room" can be
-- answered when choosing the next login. Rows older than 90 days are pruned.
CREATE TABLE IF NOT EXISTS claude_account_usage (
    seq                 INTEGER PRIMARY KEY AUTOINCREMENT,
    at                  REAL NOT NULL,
    account             TEXT NOT NULL,
    source              TEXT NOT NULL,
    five_hour_pct       REAL,
    five_hour_resets_at TEXT,
    seven_day_pct       REAL,
    seven_day_resets_at TEXT,
    plan                TEXT
);
CREATE INDEX IF NOT EXISTS claude_account_usage_account_at ON claude_account_usage(account, at);
CREATE INDEX IF NOT EXISTS claude_account_usage_at ON claude_account_usage(at);
