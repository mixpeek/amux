-- Z15: the daily decision digest. A needsyou card can carry its worker's
-- recommendation and a declared scope; the ledger holds each card's published
-- effect date, owner veto and application receipt. Both tables are new, so
-- reverting this migration loses only digest state, never a card.
CREATE TABLE IF NOT EXISTS decision_digest_recommendations (
    card TEXT PRIMARY KEY,
    recommendation TEXT NOT NULL,
    scope TEXT NOT NULL DEFAULT '',
    set_by TEXT NOT NULL DEFAULT '',
    set_at REAL NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS decision_digest_ledger (
    card TEXT PRIMARY KEY,
    worker TEXT NOT NULL DEFAULT '',
    fingerprint TEXT NOT NULL DEFAULT '',
    log_len INTEGER NOT NULL DEFAULT 0,
    log_hash TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL,
    reason TEXT NOT NULL DEFAULT '',
    actor TEXT NOT NULL DEFAULT '',
    live INTEGER NOT NULL DEFAULT 0,
    digests INTEGER NOT NULL DEFAULT 0,
    first_digest TEXT NOT NULL DEFAULT '',
    last_digest TEXT NOT NULL DEFAULT '',
    effect_date TEXT NOT NULL DEFAULT '',
    attempts INTEGER NOT NULL DEFAULT 0,
    receipt TEXT NOT NULL DEFAULT '',
    updated_at REAL NOT NULL DEFAULT 0
);
