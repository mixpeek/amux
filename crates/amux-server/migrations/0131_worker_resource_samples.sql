-- Per-worker resource usage samples, collected every 30s by the
-- worker_resources runtime job.  Retention: 7 days (the job prunes
-- on each tick).  This is the data behind GET /api/metrics/workers.

CREATE TABLE IF NOT EXISTS worker_resource_samples (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session     TEXT    NOT NULL,          -- worker/session name
    ts          TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%S','now')),
    rss_kb      INTEGER NOT NULL DEFAULT 0, -- total RSS of all worker processes (KiB)
    cpu_pct     REAL    NOT NULL DEFAULT 0, -- instantaneous %CPU across all worker processes
    n_procs     INTEGER NOT NULL DEFAULT 0, -- number of processes owned by this worker
    n_threads   INTEGER NOT NULL DEFAULT 0  -- total threads across all worker processes
);

CREATE INDEX IF NOT EXISTS idx_wrs_session_ts ON worker_resource_samples(session, ts);
CREATE INDEX IF NOT EXISTS idx_wrs_ts ON worker_resource_samples(ts);
