CREATE TABLE token_attribution_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    ledger_id INTEGER NOT NULL DEFAULT 0,
    generation INTEGER NOT NULL DEFAULT 0
);
INSERT INTO token_attribution_state (id, ledger_id) VALUES (1, 0);
-- Codex can discover the owner after an initially ownerless row was indexed.
CREATE TRIGGER token_attribution_revisit_owner
AFTER UPDATE OF session ON token_ledger
WHEN OLD.session = '' AND NEW.session <> '' AND NEW.task = ''
BEGIN
    UPDATE token_attribution_state SET ledger_id = min(ledger_id, NEW.id - 1), generation = generation + 1 WHERE id = 1;
END;
CREATE TRIGGER token_attribution_revisit_claim
AFTER INSERT ON session_events WHEN NEW.type = 'task.claimed'
BEGIN
    UPDATE token_attribution_state SET ledger_id = 0, generation = generation + 1 WHERE id = 1;
END;
