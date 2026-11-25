-- CRM gaps from the 2026-11-25 pilot (gtm-engine). Both changes are additive.
--
-- Deals carry tags (a JSON array, like crm_accounts.tags), so a cohort or play
-- tag can live on the deal it describes.
ALTER TABLE crm_deals ADD COLUMN tags TEXT NOT NULL DEFAULT '[]';

-- Activity on a company itself: a competitor change, an account-level event, a
-- watched company channel that showed something new. crm_interactions belongs
-- to one person (contact_id is NOT NULL with a foreign key), so a company row
-- has its own table; the company view merges the two.
CREATE TABLE IF NOT EXISTS crm_account_activity (
    id          TEXT PRIMARY KEY,
    account_id  TEXT NOT NULL,
    date        TEXT NOT NULL,
    type        TEXT NOT NULL DEFAULT 'other',
    direction   TEXT NOT NULL DEFAULT '',
    subject     TEXT NOT NULL DEFAULT '',
    notes       TEXT NOT NULL DEFAULT '',
    message_id  TEXT NOT NULL DEFAULT '',
    source      TEXT NOT NULL DEFAULT '',
    created     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_crm_account_activity_acct ON crm_account_activity(account_id, date);
CREATE UNIQUE INDEX IF NOT EXISTS idx_crm_account_activity_msg ON crm_account_activity(account_id, message_id) WHERE message_id <> '';
