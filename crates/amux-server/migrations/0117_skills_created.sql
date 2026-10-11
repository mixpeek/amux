-- When a table skill was first saved (unix seconds). The Skills tab sorts by
-- it. The command file cannot say: amux writes it as temp + rename, so its
-- birth time resets on every save. 0 means "before this column existed".
ALTER TABLE skills ADD COLUMN created INTEGER NOT NULL DEFAULT 0;
