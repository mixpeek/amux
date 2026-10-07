-- Pre-run reviews of GS-12 proof cards' plans, one per plan hash (the
-- acceptance plus the frozen command when there is one). Their own table, not
-- card_contracts: most proof cards are typed ops and never freeze a contract,
-- and a card_contracts row changes how a card reaches verified. 0112's
-- prereview_hash and prereview_state columns on card_contracts are unused.
CREATE TABLE IF NOT EXISTS card_prereviews (
    card  TEXT PRIMARY KEY,
    hash  TEXT NOT NULL,
    state TEXT NOT NULL,
    at    REAL NOT NULL
);
