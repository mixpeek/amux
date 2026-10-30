-- Card lookups by source_ref (AF-1052). The schedule-failure resolver ran
-- `SELECT id FROM issues WHERE source_ref=? ...` inside the single writer on
-- every passing schedule fire, about one a minute, as a full SCAN of issues
-- (33,150 rows on 2026-10-30). It held the writer for 2-4.6 s a time and up
-- to 32 s, 1,496 s in total across one log, and was the largest writer_slow
-- origin while ten endpoints stalled behind it. Fourteen other source_ref
-- lookups use the same column.
CREATE INDEX IF NOT EXISTS idx_issues_source_ref ON issues(source_ref);
