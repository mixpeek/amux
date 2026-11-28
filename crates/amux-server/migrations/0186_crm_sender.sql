-- 0186: the CRM sends approved cold email itself, and keeps the pool healthy.
--
-- ADDITIVE ONLY (shared live DB).
--
-- WHY (owner, 2026-11-27: "setup the email sequencing using the alternative
-- emails ... We should have all the capabilities as instantly that are
-- important to land in inbox and get people to see them without sacrificing
-- our own domain"): an approved email touch in a campaign with mailboxes is
-- sent by the server from its picked inbox, spaced, in the send window and
-- under the inbox's ramped cap; a follow-up goes in the same thread; a bounce
-- marks the person and can pause the inbox; each sending domain's DNS and
-- blocklist state is kept; warm-up mail between the pool's own inboxes
-- measures where it lands.
ALTER TABLE crm_touches ADD COLUMN sent_thread TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_touches ADD COLUMN sent_subject TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_touches ADD COLUMN send_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE crm_mailboxes ADD COLUMN paused_reason TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_mailboxes ADD COLUMN dns TEXT NOT NULL DEFAULT '';
ALTER TABLE crm_mailboxes ADD COLUMN dns_checked_at INTEGER;

CREATE TABLE IF NOT EXISTS crm_warmup (
    id          TEXT PRIMARY KEY,
    from_addr   TEXT NOT NULL,
    to_addr     TEXT NOT NULL,
    subject     TEXT NOT NULL DEFAULT '',
    token       TEXT NOT NULL DEFAULT '',   -- a marker in the body, searched for in the recipient's mailbox
    sent_at     INTEGER NOT NULL,
    gmail_id    TEXT NOT NULL DEFAULT '',
    thread_id   TEXT NOT NULL DEFAULT '',
    placement   TEXT NOT NULL DEFAULT '',   -- inbox, spam, missing ('' until checked)
    checked_at  INTEGER,
    rescued     INTEGER NOT NULL DEFAULT 0, -- moved out of spam and marked important
    replied_at  INTEGER
);
CREATE INDEX IF NOT EXISTS idx_crm_warmup_from ON crm_warmup(from_addr, sent_at);
CREATE INDEX IF NOT EXISTS idx_crm_warmup_open ON crm_warmup(placement, sent_at);
