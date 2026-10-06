-- Contract rule 1 (AH-375): a frozen verify command may be amended once by
-- the lane or its orchestrator, with a reason; this records that it was.
ALTER TABLE card_contracts ADD COLUMN amended INTEGER NOT NULL DEFAULT 0;
