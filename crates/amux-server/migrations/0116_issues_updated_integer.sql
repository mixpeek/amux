-- issues.updated is declared INTEGER, but SQLite stores a REAL that a raw
-- writer binds (a Python time.time(), for one). 35 cards carried one from
-- 2026-10-07, and every reader taking it as i64 failed: command intake held
-- 37 owner messages undelivered with "Invalid column type Real ... updated".
UPDATE issues SET updated = CAST(updated AS INTEGER) WHERE typeof(updated) = 'real';
-- Coerce at the storage boundary so the next raw writer cannot reintroduce it.
CREATE TRIGGER IF NOT EXISTS issues_updated_integer_ai AFTER INSERT ON issues
WHEN typeof(NEW.updated) = 'real' BEGIN
    UPDATE issues SET updated = CAST(NEW.updated AS INTEGER) WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER IF NOT EXISTS issues_updated_integer_au AFTER UPDATE OF updated ON issues
WHEN typeof(NEW.updated) = 'real' BEGIN
    UPDATE issues SET updated = CAST(NEW.updated AS INTEGER) WHERE rowid = NEW.rowid;
END;
