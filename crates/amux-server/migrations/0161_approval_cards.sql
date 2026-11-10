-- Approvals as Needs-you cards (governance PR 12). Each pending approval
-- (held email, spend grant, publish, land, capability ask) is surfaced as ONE
-- needsyou card. This table only LINKS the item to its card: the item's own
-- record (the email draft, the grant file, the link, the land record, the
-- capability env) stays the source of whether it was approved (A10), and who
-- decided is read from the ledger row of the deciding request (span_id), not
-- copied here.
CREATE TABLE IF NOT EXISTS _amux_approval_cards (
    approval_ref TEXT PRIMARY KEY,          -- <kind>:<item id>
    kind         TEXT NOT NULL,             -- email | grant | publish | land | capability
    item_id      TEXT NOT NULL,
    card_id      TEXT,
    requester    TEXT NOT NULL DEFAULT '',  -- the worker (or member:<email>) that asked
    meta         TEXT NOT NULL DEFAULT '{}',-- kind data the card's actions need (recipients, amount, ...)
    filed_at     REAL NOT NULL,
    expires_at   REAL,                      -- the item's own TTL, when it has one
    decided_at   REAL,
    decision     TEXT,                      -- approved | rejected | expired
    reason       TEXT,
    trace_id     TEXT,                      -- the deciding request: join _amux_ledger on span_id
    span_id      TEXT,
    standing_id  INTEGER                    -- set when a standing approval decided it
);
CREATE INDEX IF NOT EXISTS idx_amux_approval_cards_card ON _amux_approval_cards(card_id);
CREATE INDEX IF NOT EXISTS idx_amux_approval_cards_open ON _amux_approval_cards(decided_at) WHERE decided_at IS NULL;
