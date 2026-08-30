-- Directories the scan will not descend into, and why.
--
-- Found the hard way on 2026-08-20: `~/Library/Mobile Documents` (the iCloud
-- FileProvider root) never returns from readdir on this machine. Ninety
-- seconds, zero entries, still blocked -- while `stat` on the same path answers
-- instantly with the SAME st_dev as $HOME, so the walker's cross-mount guard
-- (`md.dev() != root_dev`) has no reason to skip it. The walk thread blocks in
-- the kernel forever and `dirs_walked` freezes at whatever it had reached.
--
-- Two reasons live in this table.
--
--   'provider' is a merits-based skip, not a workaround: a FileProvider root
--   holds online-only files that occupy no local blocks, so a disk CLEANUP scan
--   has nothing to reclaim there, and enumerating one can force downloads --
--   the scan would consume disk instead of freeing it.
--
--   'stalled' is learned at runtime by the scan watchdog. Any unresponsive
--   filesystem -- a dead NFS mount, a wedged FUSE daemon, a provider root on
--   some other machine -- is recorded the first time it hangs a scan and routed
--   around by the next one, instead of wedging every scan forever.
--
-- Skips are RECORDED rather than hardcoded and forgotten because an exemption
-- that leaves no trace is invisible (ethos rule 1): the scan can report what it
-- did not look at, and a path that stops hanging can be deleted from here.
CREATE TABLE IF NOT EXISTS reclaim_skipped (
  path       TEXT PRIMARY KEY,
  reason     TEXT NOT NULL,           -- 'provider' | 'stalled'
  detail     TEXT,
  first_seen INTEGER NOT NULL,
  last_seen  INTEGER NOT NULL,
  hits       INTEGER NOT NULL DEFAULT 1
);

-- Which phase the walk thread was in when its position was last published.
-- Without this a stall is undiagnosable from outside: a blocked `read_dir` and
-- a blocked SQLite flush both freeze `dirs_walked` and both leave the row
-- claiming 'running', so the two failures are byte-identical to a reader
-- (ethos rule 4 -- the instrument could not express the discriminator).
ALTER TABLE reclaim_scans ADD COLUMN current_phase TEXT;

-- Note for anyone reading 0019's schema comment, which lists
-- `running|done|failed|cancelled`: `interrupted` (reaped after a server
-- restart) and `stalled` (ended by the watchdog because the walk thread is
-- blocked and not coming back) are also live values. Migrations are
-- append-only, so the correction lives here rather than in 0019.
