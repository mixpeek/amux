-- 0181: a schedule the harness keeps in step with a configuration key.
--
-- ADDITIVE ONLY (shared live DB).
--
-- WHY (primitive integrity audit 2026-11-26, "The goal checker reimplements
-- pass-condition schedules"): AMUX_GOAL_CHECK ran on a private 600 s clock
-- inside the goal keeper, with no run history, no audit row and no place in
-- the Schedules tab. It is now an ordinary kind=shell schedule owned by the
-- worker. `managed_by` names what keeps the row in step (`goal-check` for
-- AMUX_GOAL_CHECK), so the reconcile finds its own row, never a person's, and
-- the API can refuse an edit the next reconcile would undo.
--
-- NULL means an ordinary schedule. One live managed row per (session, managed_by).

-- ADDCOL: schedules managed_by TEXT
CREATE UNIQUE INDEX IF NOT EXISTS idx_schedules_managed
    ON schedules (session, managed_by)
    WHERE managed_by IS NOT NULL AND deleted IS NULL;
