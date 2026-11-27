-- interaction_log was the Python server's interaction trail. After the Python
-- deletion (2026-08-09) its only writer was the scope-write audit, which now
-- appends to the ledger (`_amux_ledger`, kind `scope`), and its readers (the
-- board-drive reviewer check, `/api/why/task`) read the ledger. Before this
-- runs, migrate.rs archives its rows to
-- <db dir>/archive/primitive-audit-2026-11-26/interaction_log.json and refuses
-- the migration when there are rows and the archive cannot be written.
-- Its indexes (idx_ilog_kind, idx_ilog_target, idx_ilog_ts) drop with it.
DROP TABLE IF EXISTS interaction_log;
