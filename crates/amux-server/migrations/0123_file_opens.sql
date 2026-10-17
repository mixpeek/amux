-- When each file was last opened in the dashboard's viewer (epoch seconds),
-- keyed by its resolved absolute path. Feeds the "Opened" column of every
-- file listing (GET /api/ls) and the MDAI list's default sort.
CREATE TABLE IF NOT EXISTS file_opens (
    path TEXT PRIMARY KEY,
    opened_at REAL NOT NULL
);
