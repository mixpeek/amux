-- Governance PR 19: legal hold, erasure, and what the auditor reads.
--
-- LEGAL HOLD. An active hold names an actor, a worker and/or a time range; a
-- ledger row matching any active hold cannot be deleted (retention prune or
-- otherwise). The prune stops at the first held row, so the chain stays a
-- prefix-pruned chain. Holds are never deleted; lifting one stamps it once.
CREATE TABLE IF NOT EXISTS _amux_ledger_holds (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    actor      TEXT,              -- matches actor, claimed or on_behalf_of
    worker     TEXT,              -- matches worker:<w> as actor, claimed or resource
    from_ts    INTEGER,           -- epoch milliseconds, inclusive
    to_ts      INTEGER,           -- epoch milliseconds, inclusive
    placed_by  TEXT NOT NULL,
    placed_at  INTEGER NOT NULL,  -- epoch milliseconds
    lifted_by  TEXT,
    lifted_at  INTEGER            -- epoch milliseconds
);

CREATE TRIGGER IF NOT EXISTS amux_ledger_holds_no_delete
BEFORE DELETE ON _amux_ledger_holds
BEGIN SELECT RAISE(ABORT, 'legal holds are never deleted; lift them'); END;

CREATE TRIGGER IF NOT EXISTS amux_ledger_holds_lift_once
BEFORE UPDATE ON _amux_ledger_holds
WHEN OLD.lifted_at IS NOT NULL
  OR NEW.id IS NOT OLD.id OR NEW.name IS NOT OLD.name OR NEW.actor IS NOT OLD.actor
  OR NEW.worker IS NOT OLD.worker OR NEW.from_ts IS NOT OLD.from_ts OR NEW.to_ts IS NOT OLD.to_ts
  OR NEW.placed_by IS NOT OLD.placed_by OR NEW.placed_at IS NOT OLD.placed_at
BEGIN SELECT RAISE(ABORT, 'a legal hold is only ever lifted, once'); END;

CREATE TRIGGER IF NOT EXISTS amux_ledger_hold_no_delete
BEFORE DELETE ON _amux_ledger
WHEN EXISTS (SELECT 1 FROM _amux_ledger_holds h WHERE h.lifted_at IS NULL
    AND (h.actor IS NULL OR OLD.actor = h.actor OR OLD.claimed = h.actor OR OLD.on_behalf_of = h.actor)
    AND (h.worker IS NULL OR OLD.actor = 'worker:' || h.worker OR OLD.claimed = 'worker:' || h.worker
         OR OLD.resource = 'worker:' || h.worker OR OLD.resource LIKE 'worker:' || h.worker || '/%')
    AND (h.from_ts IS NULL OR OLD.ts >= h.from_ts)
    AND (h.to_ts IS NULL OR OLD.ts <= h.to_ts))
BEGIN SELECT RAISE(ABORT, 'ledger row is under legal hold'); END;

-- ERASURE. A person's name in actor/claimed/on_behalf_of/resource is replaced
-- by `erased:<erasure id>`; the row keeps its prev_hash and hash, so the chain
-- still links and signed checkpoints still match. The erasure is itself a
-- ledger event whose output_hash is a digest of every redacted row as it now
-- stands, so a later change to a redacted row is still caught by verify.
ALTER TABLE _amux_ledger ADD COLUMN redacted_at INTEGER;  -- epoch milliseconds

CREATE TABLE IF NOT EXISTS _amux_ledger_erasures (
    id              TEXT PRIMARY KEY,
    at              INTEGER NOT NULL,  -- epoch milliseconds
    by_actor        TEXT NOT NULL,
    rows            INTEGER NOT NULL,
    digest          TEXT NOT NULL,
    ledger_event_id TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS _amux_ledger_redactions (
    seq        INTEGER PRIMARY KEY,
    erasure_id TEXT NOT NULL  -- _amux_ledger_erasures.id, written after the digest over these rows
);
CREATE INDEX IF NOT EXISTS idx_amux_ledger_redactions_erasure ON _amux_ledger_redactions(erasure_id);
CREATE TRIGGER IF NOT EXISTS amux_ledger_erasures_append_only
BEFORE UPDATE ON _amux_ledger_erasures
BEGIN SELECT RAISE(ABORT, 'erasure records are append-only'); END;
CREATE TRIGGER IF NOT EXISTS amux_ledger_redactions_append_only
BEFORE UPDATE ON _amux_ledger_redactions
BEGIN SELECT RAISE(ABORT, 'redaction records are append-only'); END;

-- Held open only inside the erase's own transaction, one row per seq.
CREATE TABLE IF NOT EXISTS _amux_ledger_erase_gate (
    seq INTEGER PRIMARY KEY
);

-- The four personal columns leave the blanket append-only trigger and get
-- their own: changed only through the erase gate, only to an `erased:` token,
-- only once, and never under a legal hold.
DROP TRIGGER IF EXISTS amux_ledger_no_update;
CREATE TRIGGER IF NOT EXISTS amux_ledger_no_update
BEFORE UPDATE OF seq, event_id, ts, kind, trace_id, parent_event_id,
    authority, policy_version, action, input_hash, output_hash,
    decision, outcome, cost_microusd, source_table, source_id,
    span_id, parent_span_id, trace_source ON _amux_ledger
BEGIN SELECT RAISE(ABORT, 'ledger is append-only'); END;

CREATE TRIGGER IF NOT EXISTS amux_ledger_erase_only
BEFORE UPDATE OF actor, claimed, on_behalf_of, resource, redacted_at ON _amux_ledger
WHEN NOT EXISTS (SELECT 1 FROM _amux_ledger_erase_gate g WHERE g.seq = OLD.seq)
  OR OLD.redacted_at IS NOT NULL
  OR NEW.redacted_at IS NULL
  OR (NEW.actor IS NOT OLD.actor AND NEW.actor NOT LIKE 'erased:%')
  OR (NEW.claimed IS NOT OLD.claimed AND COALESCE(NEW.claimed, '') NOT LIKE 'erased:%')
  OR (NEW.on_behalf_of IS NOT OLD.on_behalf_of AND COALESCE(NEW.on_behalf_of, '') NOT LIKE 'erased:%')
  OR (NEW.resource IS NOT OLD.resource AND COALESCE(NEW.resource, '') NOT LIKE 'erased:%')
BEGIN SELECT RAISE(ABORT, 'ledger is append-only'); END;

CREATE TRIGGER IF NOT EXISTS amux_ledger_erase_not_held
BEFORE UPDATE OF actor, claimed, on_behalf_of, resource ON _amux_ledger
WHEN EXISTS (SELECT 1 FROM _amux_ledger_holds h WHERE h.lifted_at IS NULL
    AND (h.actor IS NULL OR OLD.actor = h.actor OR OLD.claimed = h.actor OR OLD.on_behalf_of = h.actor)
    AND (h.worker IS NULL OR OLD.actor = 'worker:' || h.worker OR OLD.claimed = 'worker:' || h.worker
         OR OLD.resource = 'worker:' || h.worker OR OLD.resource LIKE 'worker:' || h.worker || '/%')
    AND (h.from_ts IS NULL OR OLD.ts >= h.from_ts)
    AND (h.to_ts IS NULL OR OLD.ts <= h.to_ts))
BEGIN SELECT RAISE(ABORT, 'ledger row is under legal hold'); END;
