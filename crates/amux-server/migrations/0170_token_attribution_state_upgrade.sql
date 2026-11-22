-- 0169 may already be recorded on databases that have the original state
-- table and claim trigger. Add the snapshot generation in a new migration so
-- those databases can detect claim races too.
-- ADDCOL: token_attribution_state generation INTEGER NOT NULL DEFAULT 0

-- Add a versioned trigger so a claim always changes the generation, including
-- when the ledger watermark is already zero. Keeping the old reset trigger is
-- harmless and preserves additive-only migration behavior.
CREATE TRIGGER IF NOT EXISTS token_attribution_revisit_claim_generation
AFTER INSERT ON session_events WHEN NEW.type = 'task.claimed'
BEGIN
    UPDATE token_attribution_state
       SET ledger_id = 0, generation = generation + 1
     WHERE id = 1;
END;
