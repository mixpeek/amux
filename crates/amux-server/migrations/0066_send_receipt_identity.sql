-- Sessions snapshots use a read-only connection. Acceptance identity must be
-- installed at startup, before the first send lazily upgrades its own tables.
-- ADDCOL: send_dedup receipt_id TEXT
CREATE INDEX IF NOT EXISTS idx_send_dedup_receipt
    ON send_dedup(session, receipt_id) WHERE receipt_id IS NOT NULL;
