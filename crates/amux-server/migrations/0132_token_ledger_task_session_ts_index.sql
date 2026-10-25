-- Composite index for token_ledger attribution queries.
--
-- attribute_tasks() runs UPDATE ... WHERE task='' AND session=? AND ts BETWEEN ? AND ?
-- on every cycle. The existing idx_ledger_task covers (task, ts) but post-filters
-- session across ~800K unattributed rows, producing 281-857ms per statement and
-- blocking the serialized writer long enough to trip the health probe deadline.
-- This covering index lets SQLite seek directly to (task, session, ts).

CREATE INDEX IF NOT EXISTS idx_ledger_task_session_ts
    ON token_ledger(task, session, ts);
