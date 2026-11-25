-- Outbound requests: a worker's direct outbound message on a channel the
-- server cannot send itself (an X reply or DM, a LinkedIn comment or DM, any
-- browser-driven post), held for the owner with its full context. Approving
-- one opens a short window in which that worker's browser may type on that
-- channel's site (api/outbound.rs). Additive only.
CREATE TABLE IF NOT EXISTS outbound_requests (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    worker       TEXT NOT NULL,
    channel      TEXT NOT NULL,
    action       TEXT NOT NULL,
    account      TEXT NOT NULL DEFAULT '',
    recipient    TEXT NOT NULL DEFAULT '',
    target_url   TEXT NOT NULL DEFAULT '',
    host         TEXT NOT NULL DEFAULT '',
    body         TEXT NOT NULL,
    context      TEXT NOT NULL DEFAULT '{}',
    crm_touch    TEXT NOT NULL DEFAULT '',
    state        TEXT NOT NULL DEFAULT 'held',
    created_at   INTEGER NOT NULL,
    decided_at   INTEGER,
    decided_by   TEXT NOT NULL DEFAULT '',
    decision_note TEXT NOT NULL DEFAULT '',
    window_until INTEGER,
    done_at      INTEGER,
    result_url   TEXT NOT NULL DEFAULT '',
    result_note  TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS outbound_requests_window ON outbound_requests (worker, host, state);
