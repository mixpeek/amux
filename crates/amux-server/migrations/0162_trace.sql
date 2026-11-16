-- One trace id per unit of work (governance PR 8, api/trace.rs). The ledger
-- already has trace_id; a trace is a tree, so each row also names its own span
-- and the span it descends from, and how it came by its trace (minted at an
-- origin, inherited from a traceparent, or delivered to a worker). The seal
-- pass hashes the new columns only when one is set, so rows sealed before this
-- migration keep their hashes and the chain still verifies.
ALTER TABLE _amux_ledger ADD COLUMN span_id TEXT;
ALTER TABLE _amux_ledger ADD COLUMN parent_span_id TEXT;
ALTER TABLE _amux_ledger ADD COLUMN trace_source TEXT;

DROP TRIGGER IF EXISTS amux_ledger_no_update;
CREATE TRIGGER IF NOT EXISTS amux_ledger_no_update
BEFORE UPDATE OF seq, event_id, ts, kind, trace_id, parent_event_id, actor, claimed,
    on_behalf_of, authority, policy_version, action, resource, input_hash, output_hash,
    decision, outcome, cost_microusd, source_table, source_id,
    span_id, parent_span_id, trace_source ON _amux_ledger
BEGIN SELECT RAISE(ABORT, 'ledger is append-only'); END;

-- A receipt written while serving a request carries that request's trace and
-- span (the request's own span), and its ledger row descends from that span.
ALTER TABLE _amux_policy_receipts ADD COLUMN trace_id TEXT;
ALTER TABLE _amux_policy_receipts ADD COLUMN span_id TEXT;

DROP TRIGGER IF EXISTS amux_ledger_from_policy_receipt;
CREATE TRIGGER IF NOT EXISTS amux_ledger_from_policy_receipt
AFTER INSERT ON _amux_policy_receipts
WHEN COALESCE((SELECT enabled FROM _amux_ledger_config WHERE id = 1), 1) = 1
BEGIN
    INSERT OR IGNORE INTO _amux_ledger
        (event_id, ts, kind, actor, on_behalf_of, authority, action, resource,
         decision, outcome, cost_microusd, source_table, source_id,
         trace_id, span_id, parent_span_id, trace_source)
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
        NEW.id,
        NEW.trace_id,
        -- The receipt is a decision inside the request's span: its own span is
        -- derived, its parent is the request's span.
        CASE WHEN NEW.trace_id IS NOT NULL THEN substr(lower(hex(randomblob(8))), 1, 16) END,
        NEW.span_id,
        CASE WHEN NEW.trace_id IS NOT NULL THEN 'inherited' END
    );
END;
