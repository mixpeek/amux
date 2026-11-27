-- 0183: campaigns in the CRM.
--
-- ADDITIVE ONLY (shared live DB).
--
-- WHY (owner, 2026-11-27: "Do we have the concept of a campaign?" ... "yes
-- build it"): a go-to-market push (the IP check or creative-dna focus) ran as
-- several sequences whose names carried the campaign ("amux GitHub
-- stargazers: pipeline v1" and "... X v1"; "re-engagement v1" and
-- "... (emailed 11-23)"). A campaign holds what no sequence can: the goal and
-- use case, the ICP, the opportunity's static app, the mailboxes it sends
-- from, its dates and status; its sequences and deals point to it, and the
-- funnel adds up across them.
CREATE TABLE IF NOT EXISTS crm_campaigns (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    goal       TEXT NOT NULL DEFAULT '',
    use_case   TEXT NOT NULL DEFAULT '',
    icp        TEXT NOT NULL DEFAULT '',
    asset_url  TEXT NOT NULL DEFAULT '',
    mailboxes  TEXT NOT NULL DEFAULT '[]',   -- the addresses its email goes out from
    starts     TEXT NOT NULL DEFAULT '',     -- YYYY-MM-DD
    ends       TEXT NOT NULL DEFAULT '',
    status     TEXT NOT NULL DEFAULT 'active', -- draft, active, paused, done
    owner      TEXT NOT NULL DEFAULT '',
    notes      TEXT NOT NULL DEFAULT '',
    source     TEXT NOT NULL DEFAULT '',
    created    INTEGER NOT NULL,
    updated    INTEGER NOT NULL,
    deleted    INTEGER
);

ALTER TABLE crm_sequences ADD COLUMN campaign_id TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_deals ADD COLUMN campaign_id TEXT NOT NULL DEFAULT '';
CREATE INDEX IF NOT EXISTS idx_crm_sequences_campaign ON crm_sequences(campaign_id);
CREATE INDEX IF NOT EXISTS idx_crm_deals_campaign ON crm_deals(campaign_id);
