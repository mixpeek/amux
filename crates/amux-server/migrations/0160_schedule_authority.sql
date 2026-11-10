-- Governance PR 13: a schedule keeps its creator, proven, and every firing
-- re-checks that the creator still has authority over the worker it targets.
-- Numbered 0160 on top of the governance stack; origin/main has its own 0155,
-- so the stack's 0155..0160 are renumbered when it is restacked onto main.
ALTER TABLE schedules ADD COLUMN created_by TEXT;
ALTER TABLE schedules ADD COLUMN created_by_verified INTEGER NOT NULL DEFAULT 0;

-- Backfill from the audit trail: the first 'created' row names who made it.
-- Those names were asserted, never proven, so they stay unverified.
UPDATE schedules SET created_by = (
    SELECT a.by_who FROM schedule_audit a
     WHERE a.schedule_id = schedules.id AND a.field = 'created'
     ORDER BY a.ts ASC LIMIT 1)
 WHERE created_by IS NULL;
UPDATE schedules SET created_by = 'unattributed'
 WHERE created_by IS NULL OR TRIM(created_by) = '';

-- Who answers for a worker. A worker with no row is sponsored by the owner.
CREATE TABLE IF NOT EXISTS worker_sponsors (
    worker   TEXT PRIMARY KEY,
    sponsor  TEXT NOT NULL,            -- owner | member:<email>
    set_at   INTEGER NOT NULL,
    set_by   TEXT NOT NULL DEFAULT ''
);

-- A member who was offboarded keeps their row (their history still names
-- them); this stamps when and by whom.
ALTER TABLE org_members ADD COLUMN offboarded_at INTEGER;
ALTER TABLE org_members ADD COLUMN offboarded_by TEXT;
