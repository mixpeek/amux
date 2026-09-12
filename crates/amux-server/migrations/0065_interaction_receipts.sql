CREATE TABLE IF NOT EXISTS _amux_interactions (
    id TEXT PRIMARY KEY,
    command_kind TEXT NOT NULL,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    actor TEXT NOT NULL,
    target_kind TEXT NOT NULL,
    target_id TEXT NOT NULL,
    phase TEXT NOT NULL,
    status INTEGER,
    acknowledgement TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 1,
    applied_writes INTEGER NOT NULL DEFAULT 0,
    unjournaled_writes INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_interactions_updated ON _amux_interactions(updated_at);
CREATE INDEX IF NOT EXISTS idx_interactions_actor ON _amux_interactions(actor, updated_at);
CREATE TABLE IF NOT EXISTS _amux_interaction_effects (
    id INTEGER PRIMARY KEY,
    interaction_id TEXT NOT NULL REFERENCES _amux_interactions(id),
    event_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    entity_kind TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    rev INTEGER NOT NULL,
    UNIQUE(interaction_id, event_id)
);
CREATE INDEX IF NOT EXISTS idx_interaction_effects_interaction ON _amux_interaction_effects(interaction_id, id);
CREATE INDEX IF NOT EXISTS idx_request_log_interaction ON _amux_request_log(json_extract(req_meta, '$.interaction_id')) WHERE req_meta IS NOT NULL;
