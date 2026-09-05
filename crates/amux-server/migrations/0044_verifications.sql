-- Structured verification persistence (Phase 1a of the workflow engine).
--
-- Verification is the bridge between Done (a worker's claim) and Verified (the
-- harness's conclusion). Until now, verification outcomes lived in the card's
-- append-only `log` text and `last_verified_at` timestamp. This table makes them
-- queryable: gates can evaluate `SELECT ... FROM verifications WHERE task_id = ?
-- AND verdict = 'passed'` instead of parsing log text, and the history of every
-- verification attempt (including failures and retries) is preserved as structured
-- data with actor attribution and evidence.
--
-- The core `Verification` struct (amux-core/src/verification.rs) defines the
-- shape; this table persists it.

CREATE TABLE IF NOT EXISTS _amux_verifications (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL,
    verifier    TEXT NOT NULL,       -- JSON: Actor (who reached the conclusion)
    criteria    TEXT NOT NULL,       -- JSON: array of CriterionId checked
    evidence    TEXT NOT NULL,       -- JSON: array of Evidence records
    verdict     TEXT NOT NULL,       -- 'passed' or 'failed'
    reason      TEXT,               -- failure reason (NULL when passed)
    run_detail  TEXT,               -- JSON: full CriteriaRun for replay
    actor       TEXT NOT NULL,       -- session name or 'api-anonymous'
    created_at  INTEGER NOT NULL    -- unix seconds
);

CREATE INDEX IF NOT EXISTS idx_verifications_task
    ON _amux_verifications(task_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_verifications_verdict
    ON _amux_verifications(task_id, verdict);
