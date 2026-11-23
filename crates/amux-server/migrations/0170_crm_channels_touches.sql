-- Agentic CRM (docs/design/crm.md, owner 2026-11-23: "a very agentic-oriented
-- CRM ... fields that allow the agent to take action ... multi-touch ...
-- LinkedIn, Twitter ... real time ... subscribe to a user ... websites ...
-- career posting"). Additive only.

-- Where a person or company can be reached or watched. A watched channel is
-- checked on its own clock: a website or careers page by the server, a social
-- profile by a worker through a signed-in browser route, which reports what it
-- saw. A change is a crm.channel.changed event.
CREATE TABLE IF NOT EXISTS crm_channels (
    id              TEXT PRIMARY KEY,
    owner_kind      TEXT NOT NULL,              -- 'contact' or 'account'
    owner_id        TEXT NOT NULL,
    kind            TEXT NOT NULL,              -- linkedin, x, instagram, website, careers, email, phone, youtube, tiktok, github, other
    handle          TEXT NOT NULL DEFAULT '',
    url             TEXT NOT NULL DEFAULT '',
    watch           INTEGER NOT NULL DEFAULT 0,
    check_every_min INTEGER NOT NULL DEFAULT 360,
    how             TEXT NOT NULL DEFAULT '',   -- 'fetch' (the server) or 'browser' (a worker); empty is chosen by kind
    last_checked_at INTEGER,
    last_changed_at INTEGER,
    last_hash       TEXT NOT NULL DEFAULT '',
    last_items      TEXT NOT NULL DEFAULT '[]', -- what the last check saw, to tell what is new
    last_error      TEXT NOT NULL DEFAULT '',
    created         INTEGER NOT NULL,
    updated         INTEGER NOT NULL,
    deleted         INTEGER
);
CREATE INDEX IF NOT EXISTS idx_crm_channels_owner ON crm_channels(owner_kind, owner_id) WHERE deleted IS NULL;
CREATE INDEX IF NOT EXISTS idx_crm_channels_watch ON crm_channels(watch, last_checked_at) WHERE deleted IS NULL AND watch = 1;

-- One planned action on one channel: the unit of multi-touch outreach. A
-- worker plans and drafts it; the owner approves anything a person outside
-- the company would read; the assigned worker does it and records the result.
CREATE TABLE IF NOT EXISTS crm_touches (
    id          TEXT PRIMARY KEY,
    contact_id  TEXT NOT NULL DEFAULT '',
    account_id  TEXT NOT NULL DEFAULT '',
    channel     TEXT NOT NULL,                  -- a channel kind: linkedin, x, email, ...
    action      TEXT NOT NULL,                  -- dm, comment, connect, email, follow, like, call, note
    body        TEXT NOT NULL DEFAULT '',
    target_url  TEXT NOT NULL DEFAULT '',
    due_date    TEXT NOT NULL DEFAULT '',
    assignee    TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'planned', -- planned, drafted, approved, done, skipped, failed
    sequence    TEXT NOT NULL DEFAULT '',
    step        INTEGER NOT NULL DEFAULT 0,
    approved_by TEXT NOT NULL DEFAULT '',
    approved_at INTEGER,
    done_at     INTEGER,
    result      TEXT NOT NULL DEFAULT '',
    result_url  TEXT NOT NULL DEFAULT '',
    source      TEXT NOT NULL DEFAULT '',
    created     INTEGER NOT NULL,
    updated     INTEGER NOT NULL,
    deleted     INTEGER
);
CREATE INDEX IF NOT EXISTS idx_crm_touches_contact ON crm_touches(contact_id) WHERE deleted IS NULL;
CREATE INDEX IF NOT EXISTS idx_crm_touches_status ON crm_touches(status, due_date) WHERE deleted IS NULL;

-- Where automatic capture last read: the event ledger's row id and the sent
-- mail log's byte offset.
CREATE TABLE IF NOT EXISTS crm_capture_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
