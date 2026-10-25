-- The Map tab's pins move from the whole-document ~/.amux/map.json into rows
-- (2026-10-20). One row per pin, so an edit touches only its pin and two
-- devices no longer overwrite each other; every change is kept in
-- map_pin_history. The pin itself stays a JSON document (`doc`) because its
-- fields grow with the feature (status, rating, visits, place, eventAt).
-- map.json is imported once on first read and left on disk as the backup.
CREATE TABLE IF NOT EXISTS map_pins (
    id         TEXT PRIMARY KEY,
    doc        TEXT NOT NULL,
    lat        REAL NOT NULL,
    lng        REAL NOT NULL,
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL,
    rev        INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS map_pin_history (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    pin_id  TEXT NOT NULL,
    at      REAL NOT NULL,
    actor   TEXT NOT NULL DEFAULT '',
    action  TEXT NOT NULL,
    before  TEXT,
    after   TEXT
);
CREATE INDEX IF NOT EXISTS map_pin_history_pin ON map_pin_history(pin_id, seq);
-- tags, settings, trials, lists, tagGroups, and the one-time import marker.
CREATE TABLE IF NOT EXISTS map_meta (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at REAL NOT NULL
);
-- Read-only links to a saved list. Revoking keeps the row with revoked_at.
CREATE TABLE IF NOT EXISTS map_shares (
    token      TEXT PRIMARY KEY,
    list_id    TEXT NOT NULL,
    created_at REAL NOT NULL,
    revoked_at REAL
);
CREATE INDEX IF NOT EXISTS map_shares_list ON map_shares(list_id);
