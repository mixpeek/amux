-- The board change feed missed most edits. board_update_cdc fired only when
-- status or title changed, so archiving, reassigning, pinning, reordering or
-- editing a card's description wrote no change row: the dashboard's change
-- feed reported nothing and the board kept showing the old card until some
-- later full read. Fire on every column a board card renders, and on `rev`,
-- which each board mutation bumps. Lease heartbeats touch none of these, so
-- they still write nothing.
DROP TRIGGER IF EXISTS board_update_cdc;
CREATE TRIGGER board_update_cdc AFTER UPDATE ON issues
WHEN OLD.status IS NOT NEW.status OR OLD.title IS NOT NEW.title
  OR OLD.rev IS NOT NEW.rev OR OLD.archived IS NOT NEW.archived
  OR OLD.deleted IS NOT NEW.deleted OR OLD.session IS NOT NEW.session
  OR OLD.pinned IS NOT NEW.pinned OR OLD.pos IS NOT NEW.pos
  OR OLD.type IS NOT NEW.type OR OLD.due IS NOT NEW.due
  OR OLD.gate IS NOT NEW.gate OR OLD.reviewer IS NOT NEW.reviewer
  OR OLD."desc" IS NOT NEW."desc" OR OLD.lease_owner IS NOT NEW.lease_owner
BEGIN
    INSERT INTO board_change_log(table_name, row_id, operation, old_status, new_status, changed_by)
    VALUES ('issues', NEW.id, 'UPDATE', OLD.status, NEW.status, NEW.session);
END;
