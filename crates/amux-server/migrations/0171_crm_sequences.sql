-- Outbound sequences (api/crm_outbound.rs; owner 2026-11-24: "any gaps in
-- automatically making the outbound effective personal timely etc should be
-- built into the crm"). Additive.
CREATE TABLE IF NOT EXISTS crm_sequences (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    goal          TEXT NOT NULL DEFAULT '',
    steps         TEXT NOT NULL DEFAULT '[]',   -- [{day, channel, action, template?, assignee?}], day after the previous step
    stop_on_reply INTEGER NOT NULL DEFAULT 1,
    assignee      TEXT NOT NULL DEFAULT '',
    source        TEXT NOT NULL DEFAULT '',
    created       INTEGER NOT NULL,
    updated       INTEGER NOT NULL,
    deleted       INTEGER
);

CREATE TABLE IF NOT EXISTS crm_enrollments (
    sequence_id TEXT NOT NULL,
    contact_id  TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'active',  -- active, replied, finished, stopped
    step        INTEGER NOT NULL DEFAULT 0,      -- the step whose touch is open (1-based)
    enrolled_by TEXT NOT NULL DEFAULT '',
    created     INTEGER NOT NULL,
    updated     INTEGER NOT NULL,
    PRIMARY KEY (sequence_id, contact_id)
);
CREATE INDEX IF NOT EXISTS idx_crm_enrollments_contact ON crm_enrollments(contact_id, status);

-- When crm.touch.due was emitted for a touch, so it is announced once.
ALTER TABLE crm_touches ADD COLUMN due_announced_at INTEGER;
