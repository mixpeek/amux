-- Intent first (docs/design/intent-first.md). No new primitive: an outcome is
-- a Messages thread (`o:<id>` in conversation_posts) routed to a worker; an
-- idea is a suggested request; a goal is a versioned goal contract
-- (_amux_goal_contracts) worked on by a worker through a schedule. These rows
-- hold only what links those together and what the owner sees about them.

-- One outcome: the conversation unit the owner sees instead of a worker.
CREATE TABLE IF NOT EXISTS intent_outcomes (
    id          TEXT PRIMARY KEY,               -- the conversation key is o:<id>
    title       TEXT NOT NULL,
    worker      TEXT NOT NULL DEFAULT '',       -- who it is routed to now
    decision    TEXT NOT NULL DEFAULT '{}',     -- the router's decision, as returned
    sources     TEXT NOT NULL DEFAULT '[]',     -- posts and messages it came from
    overrides   TEXT NOT NULL DEFAULT '[]',     -- each "Change": from, to, when
    item_id     TEXT,                           -- the idea or goal that started it
    created_ms  INTEGER NOT NULL,
    updated_ms  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_intent_outcomes_updated ON intent_outcomes (updated_ms);

-- Ideas and goals.
CREATE TABLE IF NOT EXISTS intent_items (
    id           TEXT PRIMARY KEY,
    kind         TEXT NOT NULL,                  -- idea | goal
    tier         TEXT NOT NULL DEFAULT '',       -- goal: tracking (finite, a deadline) | standing
    title        TEXT NOT NULL,
    body         TEXT NOT NULL DEFAULT '',       -- what it can do, from what, and the result
    request      TEXT NOT NULL DEFAULT '',       -- what starting it sends through the router
    status       TEXT NOT NULL DEFAULT 'offered',-- idea: offered|started|dismissed; goal: active|paused|done|dismissed
    why          TEXT NOT NULL DEFAULT '',
    sources      TEXT NOT NULL DEFAULT '[]',     -- [{kind: message|email|event|outcome, id, text}]
    due          TEXT,                           -- YYYY-MM-DD for tracking
    status_line  TEXT NOT NULL DEFAULT '',
    status_at    INTEGER,
    worker       TEXT NOT NULL DEFAULT '',
    outcome_id   TEXT,
    goal_id      TEXT,                           -- _amux_goal_contracts.id
    schedule_id  TEXT,                           -- the scheduled check
    outside_ok   INTEGER NOT NULL DEFAULT 0,     -- owner approved its first outside action
    fingerprint  TEXT NOT NULL DEFAULT '',       -- normalized title, so a pass never offers one twice
    created_ms   INTEGER NOT NULL,
    updated_ms   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_intent_items_kind ON intent_items (kind, status);
CREATE UNIQUE INDEX IF NOT EXISTS idx_intent_items_fp ON intent_items (kind, fingerprint) WHERE fingerprint <> '';
