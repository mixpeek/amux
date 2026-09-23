-- A project is a group configuration, not a second board database.
-- ADDCOL: group_config execution_policy TEXT
-- ADDCOL: group_config execution_rev INTEGER NOT NULL DEFAULT 0
-- Nullable ownership preserves the existing meaning of legacy worker boards.
-- ADDCOL: issues project_group TEXT
-- ADDCOL: cmd_history project_group TEXT
CREATE INDEX IF NOT EXISTS idx_issues_project_live ON issues(project_group,status) WHERE deleted IS NULL;
CREATE INDEX IF NOT EXISTS idx_cmd_history_project ON cmd_history(project_group,id);
