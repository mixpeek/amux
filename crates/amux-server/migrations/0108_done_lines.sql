-- Contract A3 (AH-389): a project's done line. An epic's proof cards are
-- frozen as a set; every change after the first freeze is a new version that
-- names the owner and a reason.
CREATE TABLE IF NOT EXISTS done_lines (
    epic      TEXT PRIMARY KEY,
    cards     TEXT NOT NULL,
    version   INTEGER NOT NULL,
    by        TEXT NOT NULL,
    at        REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS done_line_revisions (
    epic      TEXT NOT NULL,
    version   INTEGER NOT NULL,
    cards     TEXT NOT NULL,
    by        TEXT NOT NULL,
    reason    TEXT NOT NULL,
    at        REAL NOT NULL,
    PRIMARY KEY (epic, version)
);
