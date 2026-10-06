-- Contract A1 (AH-388): the owner notice a planned production change raises
-- before it may start. `notice` is the needsyou card addressed to the owner;
-- `base_len` is its desc length at creation, so text appended later (the
-- owner's reply) can be told from the notice's own wording.
CREATE TABLE IF NOT EXISTS prod_change_notices (
    card      TEXT PRIMARY KEY,
    notice    TEXT NOT NULL,
    raised_at REAL NOT NULL,
    base_len  INTEGER NOT NULL
);
