-- Channel settings for the Messages tab (owner and admins only).
-- A channel is a group, so its settings live on the group's own row:
-- a display title, private (only people added by hand read it), archived,
-- and channel_created_at for a group created as a channel before any worker
-- carries its tag.
-- ADDCOL: group_config channel_title TEXT NOT NULL DEFAULT ''
-- ADDCOL: group_config channel_private INTEGER NOT NULL DEFAULT 0
-- ADDCOL: group_config channel_created_at INTEGER
-- ADDCOL: group_config channel_archived_at INTEGER

-- Who reads a channel starts from each member's team scope. A row here is an
-- exception to that rule: a person added outside their team ('added'), or
-- removed from a channel their team covers ('removed'). A member has exactly
-- one team, so teams cannot hold a second channel without moving them.
CREATE TABLE IF NOT EXISTS conversation_members (
    conv   TEXT NOT NULL,                 -- g:<group>
    email  TEXT NOT NULL,
    state  TEXT NOT NULL CHECK (state IN ('added','removed')),
    by_key TEXT NOT NULL DEFAULT '',      -- owner | member:<email>
    ts     INTEGER NOT NULL,              -- unix millis
    PRIMARY KEY (conv, email)
);
