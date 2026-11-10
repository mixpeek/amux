-- Retained goal-checker readings (Z14, the live scoreboard). goal_check kept only
-- the last run in ~/.amux/goal-check/<worker>.json, so "what was it 1 h ago",
-- "what was it this morning" and "what was the peak" had no source.
--
-- A row exists only for a run that produced a verdict AND a number: a checker
-- that could not run stores nothing, so a gap reads as unmeasured and never as
-- a zero. Retention is bounded by goal_check::SAMPLE_RETENTION_DAYS, trimmed on
-- every insert.
CREATE TABLE IF NOT EXISTS goal_samples (
    session TEXT NOT NULL,
    at      REAL NOT NULL,
    done    INTEGER NOT NULL,
    total   INTEGER,
    met     INTEGER,
    PRIMARY KEY (session, at)
) WITHOUT ROWID;
