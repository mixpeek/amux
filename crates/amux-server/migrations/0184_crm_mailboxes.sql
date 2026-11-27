-- 0184: the sending pool, and LinkedIn connection accepts.
--
-- ADDITIVE ONLY (shared live DB).
--
-- WHY (owner, 2026-11-27: "Do it", on the cold-email setup from
-- @coldemailchris: Google inboxes only, 2 inboxes per domain, 15 sends a day
-- per inbox, 14 day warm up, 20% of inboxes in reserve; and on @draprints'
-- LinkedIn playbook, whose first check is "under 1 in 5 accepting ur request
-- means the list is wrong"): each sending inbox is a row with its daily cap and
-- warm-up; an approved cold email is given the inbox that sends it; a
-- LinkedIn connect records when it was accepted.
CREATE TABLE IF NOT EXISTS crm_mailboxes (
    address        TEXT PRIMARY KEY,        -- lower case
    domain         TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'warming', -- warming, active, reserve, paused
    daily_cap      INTEGER NOT NULL DEFAULT 15,
    warmup_started TEXT NOT NULL DEFAULT '',  -- YYYY-MM-DD
    warmup_days    INTEGER NOT NULL DEFAULT 14,
    notes          TEXT NOT NULL DEFAULT '',
    created        INTEGER NOT NULL,
    updated        INTEGER NOT NULL
);

ALTER TABLE crm_touches ADD COLUMN mailbox TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_touches ADD COLUMN accepted_at INTEGER;
CREATE INDEX IF NOT EXISTS idx_crm_touches_mailbox ON crm_touches(mailbox, status);
