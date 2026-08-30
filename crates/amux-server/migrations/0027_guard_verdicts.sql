-- AF-127: per-verdict OUTCOME rows for the staged guard. The guard logged
-- BLOCK/ALLOW lines but never what happened NEXT, so a true positive (audited
-- override) and a false positive (reflexive ack) were byte-identical and no
-- guard change could be measured against a false-positive rate, before or
-- after. One row per verdict; outcome columns are filled by the follow-up
-- report (declared override / observed trim) and stay NULL for
-- aborted-or-walked-away, which is COMPUTED at read time from age — never
-- written by a sweep (D1: no scraper writes).
CREATE TABLE IF NOT EXISTS guard_verdicts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts REAL NOT NULL,
  session TEXT NOT NULL,
  dir TEXT NOT NULL,
  verdict TEXT NOT NULL,             -- 'block' | 'allow'
  n_foreign INTEGER NOT NULL DEFAULT 0,
  n_shared INTEGER NOT NULL DEFAULT 0,
  n_unclaimed INTEGER NOT NULL DEFAULT 0,
  paths TEXT,                        -- JSON array of blocked paths, capped
  provenance TEXT,                   -- JSON object: provenance -> count over foreign rows
  guard_version INTEGER,
  -- Outcome half (AF-127). resolution: 'proceeded' | 'trimmed' | 'reallowed'.
  -- basis: 'declared' (the committer's own override env var — the only cell
  -- false-positive arithmetic may lean on) | 'observed' (the hook compared
  -- the next staged set itself) | 'inferred' (server-side nearest-match link).
  resolution TEXT,
  override_used TEXT,                -- 'allow_foreign' | 'verified_solo'
  outcome_basis TEXT,
  outcome_ts REAL,
  outcome_elapsed_s REAL,
  outcome_link TEXT                  -- 'marker' (hook carried the id) | 'nearest'
);
CREATE INDEX IF NOT EXISTS idx_guard_verdicts_ts ON guard_verdicts(ts);
CREATE INDEX IF NOT EXISTS idx_guard_verdicts_open
  ON guard_verdicts(session, dir, ts) WHERE resolution IS NULL AND verdict='block';
