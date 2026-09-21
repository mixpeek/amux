-- Reconcile the two version histories that independently consumed 0082/0083.
-- Every operation is idempotent. This migration deliberately repeats both
-- branches' schema work so a database upgraded by either history converges to
-- the same shape before project execution starts.

CREATE TABLE IF NOT EXISTS board_drive_nudge_budget (
    session        TEXT NOT NULL,
    card           TEXT NOT NULL,
    kind           TEXT NOT NULL,
    n              INTEGER NOT NULL DEFAULT 0,
    first_at       REAL NOT NULL,
    last_at        REAL NOT NULL,
    next_at        REAL NOT NULL,
    status_at_last TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (session, card, kind)
);

-- ADDCOL: group_config execution_policy TEXT
-- ADDCOL: group_config execution_rev INTEGER NOT NULL DEFAULT 0
-- ADDCOL: issues project_group TEXT
-- ADDCOL: cmd_history project_group TEXT
-- ADDCOL: issues execution_state TEXT
-- ADDCOL: steering_queue sender TEXT NOT NULL DEFAULT ''

CREATE INDEX IF NOT EXISTS idx_issues_project_live ON issues(project_group,status) WHERE deleted IS NULL;
CREATE INDEX IF NOT EXISTS idx_cmd_history_project ON cmd_history(project_group,id);
CREATE INDEX IF NOT EXISTS idx_project_receipt_key ON cmd_history(project_group,json_extract(client_meta,'$.idempotency_key')) WHERE project_group IS NOT NULL;
CREATE VIEW IF NOT EXISTS legacy_execution_issues AS SELECT * FROM issues WHERE project_group IS NULL;
