-- Persisted size samples for regenerable paths (DESKT-26/27 follow-up).
--
-- The sampler shipped in 80580c79 kept its previous reading in a process-local
-- static. That is fiction on this server, which re-execs to adopt new builds
-- every ~23 minutes and far more often during a commit burst. Two consequences,
-- both measured on 2026-08-28:
--
--   1. NO DELTAS. 139 log lines said "baseline" against 34 with a delta: four
--      times out of five the cache had been wiped by a re-exec, so the growth
--      series the sampler exists to produce was never produced.
--   2. A CPU FIRE. spawn_periodic fires its first tick immediately, so every
--      re-exec launched a full tree walk. Ten walks fired between 22:44:32 and
--      22:46:41 -- inside two minutes -- against a tree that had grown to 201 GB
--      / 553,847 entries, where one walk now costs 49 SECONDS (it cost 9s when
--      the design was justified). That is what put amux-server-rs at 95.7% CPU
--      on a machine already at 94% memory: a detector paying its cost in the
--      same resource it measures.
--
-- Persisting the sample fixes both. The reading survives the re-exec, so deltas
-- are real; and the walk is skipped entirely when the last PERSISTED sample is
-- younger than the interval, so a restart storm cannot trigger a walk storm.
CREATE TABLE IF NOT EXISTS regenerable_samples (
  path       TEXT NOT NULL,
  ts         REAL NOT NULL,
  bytes      INTEGER NOT NULL,
  file_count INTEGER NOT NULL,
  PRIMARY KEY (path, ts)
);

CREATE INDEX IF NOT EXISTS idx_regenerable_samples_path_ts
  ON regenerable_samples(path, ts DESC);
