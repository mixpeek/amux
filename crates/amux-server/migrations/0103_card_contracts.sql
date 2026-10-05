-- Orchestration contract rules 1 and 2 (AH-375, AH-376): a code card's
-- acceptance criteria and verify command, frozen when it enters doing, and the
-- state of the server's verification of its done request.
CREATE TABLE IF NOT EXISTS card_contracts (
    card       TEXT PRIMARY KEY,
    acceptance TEXT NOT NULL,
    command    TEXT NOT NULL,
    hash       TEXT NOT NULL,
    frozen_at  REAL NOT NULL,
    state      TEXT NOT NULL DEFAULT 'frozen',
    sha        TEXT,
    log        TEXT,
    at         REAL
);
