-- The append-only, hash-chained record of every decision and effect (governance
-- plan 4.4). `_amux_interactions` stays the in-flight working state of a
-- command (its phase moves; it keeps 14 days); a finished command and every
-- policy receipt are written here once and never change. `prev_hash`/`hash`
-- are filled by the server's seal pass in `seq` order (SQLite has no SHA-256),
-- so a row may be briefly unsealed; `GET /api/ledger/verify` counts those.
CREATE TABLE IF NOT EXISTS _amux_ledger (
    seq             INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id        TEXT NOT NULL UNIQUE,
    ts              INTEGER NOT NULL,
    kind            TEXT NOT NULL,
    trace_id        TEXT,
    parent_event_id TEXT,
    actor           TEXT NOT NULL DEFAULT 'anonymous',
    claimed         TEXT,
    on_behalf_of    TEXT,
    authority       TEXT,
    policy_version  TEXT,
    action          TEXT,
    resource        TEXT,
    input_hash      TEXT,
    output_hash     TEXT,
    decision        TEXT,
    outcome         TEXT,
    cost_microusd   INTEGER,
    source_table    TEXT,
    source_id       TEXT,
    prev_hash       TEXT,
    hash            TEXT
);
CREATE INDEX IF NOT EXISTS idx_amux_ledger_actor ON _amux_ledger(actor, ts);
CREATE INDEX IF NOT EXISTS idx_amux_ledger_claimed ON _amux_ledger(claimed, ts);
CREATE INDEX IF NOT EXISTS idx_amux_ledger_resource ON _amux_ledger(resource, ts);
CREATE INDEX IF NOT EXISTS idx_amux_ledger_trace ON _amux_ledger(trace_id) WHERE trace_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_amux_ledger_ts ON _amux_ledger(ts);
CREATE INDEX IF NOT EXISTS idx_amux_ledger_unsealed ON _amux_ledger(seq) WHERE hash IS NULL;

-- Where the chain starts after retention pruned its head: the hash of the last
-- pruned row. It only ever moves forward.
CREATE TABLE IF NOT EXISTS _amux_ledger_anchor (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    upto_seq    INTEGER NOT NULL DEFAULT 0,
    anchor_hash TEXT NOT NULL DEFAULT 'genesis',
    pruned_at   INTEGER
);
INSERT OR IGNORE INTO _amux_ledger_anchor (id, upto_seq, anchor_hash) VALUES (1, 0, 'genesis');

-- Set by the server from AMUX_LEDGER at startup, read by the receipt trigger.
CREATE TABLE IF NOT EXISTS _amux_ledger_config (
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    enabled INTEGER NOT NULL DEFAULT 1
);
INSERT OR IGNORE INTO _amux_ledger_config (id, enabled) VALUES (1, 1);

-- Held open only inside the retention prune's own transaction.
CREATE TABLE IF NOT EXISTS _amux_ledger_prune_gate (
    id       INTEGER PRIMARY KEY CHECK (id = 1),
    upto_seq INTEGER NOT NULL
);

-- Append-only. The seal pass may fill prev_hash/hash exactly once; nothing
-- else about a row may change, and a row may be deleted only by the prune,
-- only at or below the seq it opened the gate for.
CREATE TRIGGER IF NOT EXISTS amux_ledger_no_update
BEFORE UPDATE OF seq, event_id, ts, kind, trace_id, parent_event_id, actor, claimed,
    on_behalf_of, authority, policy_version, action, resource, input_hash, output_hash,
    decision, outcome, cost_microusd, source_table, source_id ON _amux_ledger
BEGIN SELECT RAISE(ABORT, 'ledger is append-only'); END;

CREATE TRIGGER IF NOT EXISTS amux_ledger_seal_once
BEFORE UPDATE OF prev_hash, hash ON _amux_ledger
WHEN OLD.hash IS NOT NULL
BEGIN SELECT RAISE(ABORT, 'ledger row is already sealed'); END;

CREATE TRIGGER IF NOT EXISTS amux_ledger_no_delete
BEFORE DELETE ON _amux_ledger
WHEN OLD.seq > COALESCE((SELECT upto_seq FROM _amux_ledger_prune_gate WHERE id = 1), 0)
BEGIN SELECT RAISE(ABORT, 'ledger is append-only'); END;

CREATE TRIGGER IF NOT EXISTS amux_ledger_anchor_forward
BEFORE UPDATE ON _amux_ledger_anchor
WHEN NEW.upto_seq < OLD.upto_seq
BEGIN SELECT RAISE(ABORT, 'ledger anchor only moves forward'); END;

-- Every policy receipt (policy decisions, connector token mints, email gate
-- decisions and bypasses) lands in the ledger in the same transaction, so the
-- two cannot disagree. The receipt row is the 30-day working view; this is the
-- record. INSERT OR IGNORE: the ledger must never fail the receipt's writer.
CREATE TRIGGER IF NOT EXISTS amux_ledger_from_policy_receipt
AFTER INSERT ON _amux_policy_receipts
WHEN COALESCE((SELECT enabled FROM _amux_ledger_config WHERE id = 1), 1) = 1
BEGIN
    INSERT OR IGNORE INTO _amux_ledger
        (event_id, ts, kind, actor, on_behalf_of, authority, action, resource,
         decision, outcome, cost_microusd, source_table, source_id)
    VALUES (
        'pol:' || NEW.id,
        CAST(COALESCE((julianday(NEW.created_at) - 2440587.5) * 86400000,
                      (julianday('now') - 2440587.5) * 86400000) AS INTEGER),
        'policy',
        COALESCE(NULLIF(NEW.actor, ''), 'anonymous'),
        CASE WHEN json_valid(NEW.rationale) THEN json_extract(NEW.rationale, '$.on_behalf_of') END,
        NEW.rule_id,
        NEW.action,
        NEW.resource,
        NEW.effect,
        NEW.trust,
        NEW.cost_microusd,
        '_amux_policy_receipts',
        NEW.id
    );
END;
