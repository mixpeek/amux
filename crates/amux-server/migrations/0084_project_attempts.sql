-- Execution checkpoint on the existing issue, not another task ledger.
-- ADDCOL: issues execution_state TEXT
-- Restart reconciliation runs before the first request initializes legacy tables.
-- ADDCOL: steering_queue sender TEXT NOT NULL DEFAULT ''
CREATE INDEX IF NOT EXISTS idx_project_receipt_key ON cmd_history(project_group,json_extract(client_meta,'$.idempotency_key')) WHERE project_group IS NOT NULL;

CREATE VIEW IF NOT EXISTS legacy_execution_issues AS SELECT * FROM issues WHERE project_group IS NULL;
