-- The runs still marked running (AF-1082). The scheduler's startup reconcile
-- (run_scheduler) finds orphaned runs with three WHERE status='running'
-- statements inside the single writer, and schedule_runs had no index on
-- status: each was a full SCAN of 149,741 rows (3.1 s read-only on a quiet
-- host). At boot, with a cold cache on a loaded Mac, the reconcile held the
-- writer up to 42 s, once per server start. Only a handful of rows are ever
-- running, so a partial index makes those lookups touch only them.
CREATE INDEX IF NOT EXISTS idx_schedule_runs_running
    ON schedule_runs(schedule_id) WHERE status = 'running';
