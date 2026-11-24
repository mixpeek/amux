-- A voice in the CRM (api/crm_voice.rs; owner 2026-11-24: "a voice that we're
-- able to capture and adhere to throughout each of these engagements and
-- touchpoints"). Additive.
CREATE TABLE IF NOT EXISTS crm_voices (
    id      TEXT PRIMARY KEY,
    sender  TEXT NOT NULL DEFAULT 'owner',   -- owner, or a team voice's name
    name    TEXT NOT NULL DEFAULT '',
    rules   TEXT NOT NULL DEFAULT '{}',      -- {core, banned, booking_url, channels: {"email/email": {...}}}
    source  TEXT NOT NULL DEFAULT '',
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL,
    deleted INTEGER
);

-- The owner's edits of drafts (before, after) and texts he approved as written.
CREATE TABLE IF NOT EXISTS crm_voice_examples (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    voice_id TEXT NOT NULL,
    rule_key TEXT NOT NULL DEFAULT '',       -- channel/action, as in crm_voices.rules.channels
    kind     TEXT NOT NULL DEFAULT 'approved', -- owner_edit | approved
    before   TEXT NOT NULL DEFAULT '',
    after    TEXT NOT NULL DEFAULT '',
    touch_id TEXT NOT NULL DEFAULT '',
    created  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_crm_voice_examples_voice ON crm_voice_examples(voice_id, rule_key, created);

-- The voice check of the touch's text when it was written (JSON).
ALTER TABLE crm_touches ADD COLUMN voice_check TEXT;
