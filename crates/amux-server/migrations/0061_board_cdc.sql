-- Change Data Capture for board mutations.
--
-- SQLite triggers write to board_change_log on every INSERT/UPDATE to the
-- issues table.  A poller (runtime_jobs::cdc_poller) tails the table by
-- seq and fans events out through SSE, and GET /api/board/changes?since_seq=N
-- lets clients catch up after a reconnect.

CREATE TABLE IF NOT EXISTS board_change_log (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    table_name TEXT    NOT NULL,
    row_id     TEXT    NOT NULL,
    operation  TEXT    NOT NULL CHECK(operation IN ('INSERT','UPDATE','DELETE')),
    changed_at REAL   NOT NULL DEFAULT (unixepoch('subsec')),
    old_status TEXT,
    new_status TEXT,
    changed_by TEXT
);

CREATE INDEX IF NOT EXISTS idx_board_cdc_seq
    ON board_change_log(seq);

-- After INSERT on issues
CREATE TRIGGER IF NOT EXISTS board_insert_cdc AFTER INSERT ON issues
BEGIN
    INSERT INTO board_change_log(table_name, row_id, operation, new_status, changed_by)
    VALUES ('issues', NEW.id, 'INSERT', NEW.status, NEW.session);
END;

-- After UPDATE on issues (status or title changed)
CREATE TRIGGER IF NOT EXISTS board_update_cdc AFTER UPDATE ON issues
WHEN OLD.status != NEW.status OR OLD.title != NEW.title
BEGIN
    INSERT INTO board_change_log(table_name, row_id, operation, old_status, new_status, changed_by)
    VALUES ('issues', NEW.id, 'UPDATE', OLD.status, NEW.status, NEW.session);
END;

-- After DELETE on issues
CREATE TRIGGER IF NOT EXISTS board_delete_cdc AFTER DELETE ON issues
BEGIN
    INSERT INTO board_change_log(table_name, row_id, operation, old_status, changed_by)
    VALUES ('issues', OLD.id, 'DELETE', OLD.status, OLD.session);
END;
