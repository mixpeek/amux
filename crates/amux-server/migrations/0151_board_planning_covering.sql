-- The orchestrator reads 16 small columns of every active card each 3 s tick,
-- and most sit behind `desc` and `log`, so each row walked their overflow
-- chains: 1,952 reads over 1 s in 10 hours, about 3.3 s each on the live board
-- (AF-1006). This index answers that read and the board's row selection, so it
-- replaces idx_issues_light_rows (AF-1144).
DROP INDEX IF EXISTS idx_issues_light_rows;
CREATE INDEX IF NOT EXISTS idx_issues_planning
  ON issues(id, status, session, archived, pinned, pos, updated, deleted, title,
            creator, created, type, depends_on, reviewer, "version", lease_owner)
  WHERE deleted IS NULL;
