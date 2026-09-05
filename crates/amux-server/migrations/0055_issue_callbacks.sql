-- Board-native worker requests and terminal callbacks.
--
-- These live on the issue rather than in provider conversation state so a
-- provider/model switch, compaction, server restart, or a different worker
-- process cannot lose the return contract.
-- ADDCOL: issues requested_by TEXT
-- ADDCOL: issues callback_session TEXT
-- ADDCOL: issues callback_prompt TEXT
-- ADDCOL: issues callback_state TEXT
-- ADDCOL: issues callback_message_id TEXT
-- ADDCOL: issues callback_fired_at INTEGER
-- ADDCOL: issues callback_error TEXT
-- ADDCOL: issues ask_actor TEXT

CREATE INDEX IF NOT EXISTS idx_issues_callback_pending
    ON issues(callback_state, updated)
    WHERE deleted IS NULL AND callback_state IN ('pending', 'dispatching');
