CREATE TABLE _amux_source_bindings (
 task_id TEXT PRIMARY KEY REFERENCES issues(id), instance_id TEXT NOT NULL,
 candidate_id INTEGER NOT NULL, fingerprint TEXT NOT NULL, snapshot TEXT NOT NULL,
 synced_at INTEGER NOT NULL, UNIQUE(instance_id,candidate_id)
);
CREATE TABLE _amux_source_operations (
 id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES issues(id),
 request TEXT NOT NULL, receipt TEXT, created_at INTEGER NOT NULL
);
CREATE TABLE _amux_source_dispatch (
 task_id TEXT NOT NULL REFERENCES issues(id), key TEXT NOT NULL UNIQUE,
 message_id TEXT NOT NULL UNIQUE, worker TEXT NOT NULL, text TEXT NOT NULL, cmd_history_id INTEGER,
 PRIMARY KEY(task_id,key)
);
-- Permits exist only within the native source writer's transaction; no HTTP API exposes them.
CREATE TABLE _amux_source_write_permits (task_id TEXT PRIMARY KEY);
CREATE TRIGGER amux_source_lifecycle_guard BEFORE UPDATE ON issues
WHEN OLD.source='workdesk'
 AND NOT EXISTS(SELECT 1 FROM _amux_source_write_permits WHERE task_id=OLD.id)
 AND (NEW.status IS NOT OLD.status OR NEW.session IS NOT OLD.session
 OR NEW.owner_type IS NOT OLD.owner_type OR NEW.source IS NOT OLD.source
 OR NEW.source_ref IS NOT OLD.source_ref OR NEW.deleted IS NOT OLD.deleted
 OR NEW.archived IS NOT OLD.archived OR NEW.lease_owner IS NOT OLD.lease_owner
 OR NEW.project_group IS NOT OLD.project_group)
BEGIN SELECT RAISE(ABORT,'source-owned task: use source actions'); END;
CREATE TRIGGER amux_source_delete_guard BEFORE DELETE ON issues
WHEN OLD.source='workdesk'
BEGIN SELECT RAISE(ABORT,'source-owned task: use source actions'); END;
