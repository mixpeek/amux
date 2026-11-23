-- CRM phase 1 (docs/design/crm.md, owner 2026-11-23): companies, deals,
-- lookalikes, and on each person the fields GTM work needs: who or what added
-- them, their status, ICP grade with reasons, location, outbound stops, and
-- the ids of the same record in Lightfield, Instantly and the Mixpeek gtm
-- namespace. Every change here is additive: no column is dropped or
-- rewritten, and the backfill below only creates companies and links people
-- to them by the company name already on each row.

ALTER TABLE crm_contacts ADD COLUMN account_id TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_contacts ADD COLUMN source TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_contacts ADD COLUMN status TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_contacts ADD COLUMN icp_grade TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_contacts ADD COLUMN icp_reasons TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_contacts ADD COLUMN icp_scored_at INTEGER;
ALTER TABLE crm_contacts ADD COLUMN location TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_contacts ADD COLUMN unsubscribed_at INTEGER;
ALTER TABLE crm_contacts ADD COLUMN bounced_at INTEGER;
ALTER TABLE crm_contacts ADD COLUMN external TEXT NOT NULL DEFAULT '{}';
CREATE INDEX IF NOT EXISTS idx_crm_contacts_account ON crm_contacts(account_id) WHERE deleted IS NULL;
CREATE INDEX IF NOT EXISTS idx_crm_contacts_email ON crm_contacts(lower(email)) WHERE deleted IS NULL AND email <> '';

CREATE TABLE IF NOT EXISTS crm_accounts (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    domain        TEXT NOT NULL DEFAULT '',
    industry      TEXT NOT NULL DEFAULT '',
    size          TEXT NOT NULL DEFAULT '',
    location      TEXT NOT NULL DEFAULT '',
    website       TEXT NOT NULL DEFAULT '',
    linkedin      TEXT NOT NULL DEFAULT '',
    notes         TEXT NOT NULL DEFAULT '',
    tags          TEXT NOT NULL DEFAULT '[]',
    status        TEXT NOT NULL DEFAULT '',
    icp_grade     TEXT NOT NULL DEFAULT '',
    icp_reasons   TEXT NOT NULL DEFAULT '',
    icp_scored_at INTEGER,
    source        TEXT NOT NULL DEFAULT '',
    external      TEXT NOT NULL DEFAULT '{}',
    created       INTEGER NOT NULL,
    updated       INTEGER NOT NULL,
    deleted       INTEGER
);
-- The dedupe key: one live company per domain.
CREATE UNIQUE INDEX IF NOT EXISTS idx_crm_accounts_domain ON crm_accounts(lower(domain)) WHERE domain <> '' AND deleted IS NULL;
CREATE INDEX IF NOT EXISTS idx_crm_accounts_upd ON crm_accounts(updated) WHERE deleted IS NULL;

CREATE TABLE IF NOT EXISTS crm_deals (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    account_id  TEXT NOT NULL DEFAULT '',
    contact_ids TEXT NOT NULL DEFAULT '[]',
    stage       TEXT NOT NULL DEFAULT 'lead',
    amount      REAL,
    currency    TEXT NOT NULL DEFAULT 'USD',
    close_date  TEXT NOT NULL DEFAULT '',
    next_step   TEXT NOT NULL DEFAULT '',
    owner       TEXT NOT NULL DEFAULT '',
    notes       TEXT NOT NULL DEFAULT '',
    source      TEXT NOT NULL DEFAULT '',
    external    TEXT NOT NULL DEFAULT '{}',
    created     INTEGER NOT NULL,
    updated     INTEGER NOT NULL,
    deleted     INTEGER
);
CREATE INDEX IF NOT EXISTS idx_crm_deals_account ON crm_deals(account_id) WHERE deleted IS NULL;

-- A company (or person) like another: found by a worker or a search, with why.
CREATE TABLE IF NOT EXISTS crm_lookalikes (
    kind     TEXT NOT NULL,            -- 'account' or 'contact'
    from_id  TEXT NOT NULL,
    to_id    TEXT NOT NULL,
    score    REAL NOT NULL DEFAULT 0,
    reason   TEXT NOT NULL DEFAULT '',
    found_by TEXT NOT NULL DEFAULT '',
    found_at INTEGER NOT NULL,
    PRIMARY KEY (kind, from_id, to_id)
);

ALTER TABLE crm_interactions ADD COLUMN direction TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_interactions ADD COLUMN subject TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_interactions ADD COLUMN message_id TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_interactions ADD COLUMN deal_id TEXT NOT NULL DEFAULT '';
CREATE UNIQUE INDEX IF NOT EXISTS idx_crm_ix_message ON crm_interactions(contact_id, message_id) WHERE message_id <> '';

-- Backfill: one company per distinct company name already on a live person,
-- and each person linked to it. Ids continue from the counter afterwards.
INSERT INTO crm_accounts (id, name, source, created, updated)
SELECT 'ACCT-' || ROW_NUMBER() OVER (ORDER BY first_seen, lname), name, 'backfill:contact-company', first_seen, first_seen
FROM (
    SELECT MIN(TRIM(company)) AS name, LOWER(TRIM(company)) AS lname, MIN(created) AS first_seen
    FROM crm_contacts
    WHERE deleted IS NULL AND TRIM(company) <> ''
    GROUP BY LOWER(TRIM(company))
);
UPDATE crm_contacts
SET account_id = (SELECT a.id FROM crm_accounts a WHERE LOWER(a.name) = LOWER(TRIM(crm_contacts.company)) AND a.deleted IS NULL)
WHERE deleted IS NULL AND TRIM(company) <> '' AND account_id = '';
INSERT OR REPLACE INTO issue_counters (prefix, next_n) VALUES ('ACCT', (SELECT COUNT(*) + 1 FROM crm_accounts));
