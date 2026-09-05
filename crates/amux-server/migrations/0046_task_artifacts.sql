-- Artifact registry (Phase 3a of the workflow engine).
--
-- Separates artifact state from task state. A task points to artifacts (PRs,
-- commits, test runs, docs) and each artifact has its own lifecycle (draft,
-- submitted, merged, deployed). Gates can evaluate artifact state: done requires
-- an implementation artifact; verified requires a deployed one.
--
-- `kind` is open (not an enum) because artifact types grow with the workflow:
-- implementation, verification, design, doc, screenshot, config are the known
-- ones today, but a closed set here becomes a ceiling.

CREATE TABLE IF NOT EXISTS _amux_task_artifacts (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL,
    kind        TEXT NOT NULL,       -- implementation, verification, design, doc, etc.
    ref_value   TEXT NOT NULL,       -- PR-123, commit sha, file path, URL
    state       TEXT NOT NULL DEFAULT 'created',  -- created, submitted, merged, deployed
    description TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_task_artifacts_task
    ON _amux_task_artifacts(task_id);

CREATE INDEX IF NOT EXISTS idx_task_artifacts_state
    ON _amux_task_artifacts(task_id, state);
