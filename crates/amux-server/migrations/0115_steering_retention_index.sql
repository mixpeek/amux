-- Retention must inspect timestamps without scanning stored message bodies.
CREATE INDEX IF NOT EXISTS idx_steering_history_queued_at ON steering_history(queued_at);
-- Archived/deleted owner declarations are authoritative; the live-only
-- project index cannot serve reconciliation reads over their retained history.
CREATE INDEX IF NOT EXISTS idx_issues_project_history ON issues(project_group, source, acceptance_criteria);
