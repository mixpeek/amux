-- Projects were removed from amux (2026-10-28). This drops the project schema
-- that 0083, 0084, 0085 and 0115 added. Earlier migrations stay as they are
-- (append-only); a fresh database adds these columns and then drops them here.
--
-- Former project cards are archived first, so they remain readable history and
-- never surface on a worker board once the ownership column is gone.
UPDATE issues SET archived=1 WHERE project_group IS NOT NULL AND COALESCE(archived,0)=0;
-- Former project receipts must never enter ordinary message capture.
UPDATE cmd_history SET capture_pending=0 WHERE project_group IS NOT NULL AND capture_pending!=0;
-- Queued project packets and project-held owner notes have no deliverer any
-- more. Keep their text in history with an explicit outcome, then dequeue.
INSERT OR REPLACE INTO steering_history(id,session,text,queued_at,delivered_at,outcome,guard,sender)
  SELECT id,session,text,queued_at,strftime('%s','now'),'void:projects-removed',guard,sender
  FROM steering_queue WHERE guard IN ('project-steering','project-execution','project-lead');
DELETE FROM steering_queue WHERE guard IN ('project-steering','project-execution','project-lead');

DROP VIEW IF EXISTS legacy_execution_issues;
DROP INDEX IF EXISTS idx_issues_project_live;
DROP INDEX IF EXISTS idx_issues_project_history;
DROP INDEX IF EXISTS idx_cmd_history_project;
DROP INDEX IF EXISTS idx_project_receipt_key;

ALTER TABLE issues DROP COLUMN project_group;
ALTER TABLE issues DROP COLUMN execution_state;
ALTER TABLE cmd_history DROP COLUMN project_group;
ALTER TABLE group_config DROP COLUMN execution_policy;
ALTER TABLE group_config DROP COLUMN execution_rev;
