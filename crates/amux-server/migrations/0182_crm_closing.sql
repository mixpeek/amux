-- 0182: the CRM closes the loop (docs/design/crm-outbound-acceptance.md,
-- sections 10 to 15).
--
-- ADDITIVE ONLY (shared live DB).
--
-- WHY (owner, 2026-11-27: "it barely covers closing, and closing is the
-- point ... all 34 rows could read Pass and a deal would still never move
-- past lead"): a deal now records what it is worth each month, why it was
-- lost, and the static app that shows the opportunity; every stage change is
-- kept so the funnel counts a demo that later moved on or was lost; a reply
-- carries the intent a worker read in it; an enrollment carries its cohort;
-- and a person carries the result of the address check before a cold send.
ALTER TABLE crm_deals ADD COLUMN mrr_usd REAL;
ALTER TABLE crm_deals ADD COLUMN lost_reason TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_deals ADD COLUMN asset_url TEXT NOT NULL DEFAULT '';

CREATE TABLE IF NOT EXISTS crm_deal_stages (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    deal_id    TEXT NOT NULL,
    from_stage TEXT NOT NULL DEFAULT '',
    to_stage   TEXT NOT NULL,
    at         INTEGER NOT NULL,
    by         TEXT NOT NULL DEFAULT '',
    why        TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_crm_deal_stages_deal ON crm_deal_stages(deal_id, at);
CREATE INDEX IF NOT EXISTS idx_crm_deal_stages_to ON crm_deal_stages(to_stage);

-- Each deal's stage when the history began, so no deal reads as never staged.
INSERT INTO crm_deal_stages (deal_id, from_stage, to_stage, at, by, why)
    SELECT id, '', stage, created, source, 'the stage when history began' FROM crm_deals WHERE deleted IS NULL;

ALTER TABLE crm_interactions ADD COLUMN intent TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_interactions ADD COLUMN intent_why TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_enrollments ADD COLUMN cohort TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_contacts ADD COLUMN email_check TEXT NOT NULL DEFAULT '';
