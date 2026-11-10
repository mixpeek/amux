-- Signed checkpoints of the ledger head (governance PR 10, api/ledger_checkpoint.rs).
-- The hash chain (0155) shows a changed row, but someone holding the database
-- file can rewrite every row from the anchor onwards and re-hash them all. A
-- checkpoint signs (seq, head hash) with an Ed25519 key kept outside the
-- database, and is exported to places the host does not control, so such a
-- rewrite no longer matches what was signed and published.
CREATE TABLE IF NOT EXISTS _amux_ledger_checkpoints (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    seq                  INTEGER NOT NULL,
    head_hash            TEXT NOT NULL,
    ts                   INTEGER NOT NULL,
    n_rows               INTEGER NOT NULL,
    prev_checkpoint_hash TEXT NOT NULL,
    host_id              TEXT NOT NULL,
    public_key           TEXT NOT NULL,
    signature            TEXT NOT NULL,
    checkpoint_hash      TEXT NOT NULL UNIQUE
);
CREATE INDEX IF NOT EXISTS idx_amux_ledger_checkpoints_seq ON _amux_ledger_checkpoints(seq);

-- Append-only, like the ledger itself.
CREATE TRIGGER IF NOT EXISTS amux_ledger_checkpoints_no_update
BEFORE UPDATE ON _amux_ledger_checkpoints
BEGIN SELECT RAISE(ABORT, 'ledger checkpoints are append-only'); END;

CREATE TRIGGER IF NOT EXISTS amux_ledger_checkpoints_no_delete
BEFORE DELETE ON _amux_ledger_checkpoints
BEGIN SELECT RAISE(ABORT, 'ledger checkpoints are append-only'); END;

-- Where each checkpoint was exported and what the sink answered. Working
-- state, not the record: a failed export is retried and its row replaced.
-- `worm` is what was measured (true, false or unknown), never assumed.
CREATE TABLE IF NOT EXISTS _amux_ledger_checkpoint_exports (
    checkpoint_hash TEXT NOT NULL,
    sink            TEXT NOT NULL,
    ok              INTEGER NOT NULL,
    worm            TEXT,
    location        TEXT,
    detail          TEXT,
    ts              INTEGER NOT NULL,
    PRIMARY KEY (checkpoint_hash, sink)
);
