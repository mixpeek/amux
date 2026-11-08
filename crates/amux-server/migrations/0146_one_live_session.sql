-- One live session per worker (implicit state machines inventory 2026-10-31,
-- #5). "Live" is ended_at IS NULL and nothing enforced at most one; readers
-- broke ties by newest. End any older duplicates as replaced, then let the
-- index refuse a second live row.
UPDATE _amux_sessions SET ended_at = started_at, exit_reason = '{"reason":"replaced"}'
WHERE ended_at IS NULL AND EXISTS (
  SELECT 1 FROM _amux_sessions n
  WHERE n.worker_id = _amux_sessions.worker_id AND n.ended_at IS NULL
    AND (n.started_at > _amux_sessions.started_at
         OR (n.started_at = _amux_sessions.started_at AND n.id > _amux_sessions.id))
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_amux_sessions_one_live ON _amux_sessions(worker_id) WHERE ended_at IS NULL;
