-- The board's row selection reads seven small columns of every live card, and
-- `status` onward sit behind `desc`, so each row walked desc's overflow chain:
-- 4.7 s for 12k rows under load on a 14.7 GB database (AF-1144), 75 ms warm.
-- This index answers the selection without touching the table. `deleted` is a
-- column as well as the predicate because the query names it, and SQLite counts
-- that as a table reference otherwise.
CREATE INDEX IF NOT EXISTS idx_issues_light_rows
  ON issues(id, status, session, archived, pinned, pos, updated, deleted)
  WHERE deleted IS NULL;
