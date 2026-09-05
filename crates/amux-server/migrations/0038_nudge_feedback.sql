-- AF-319: close the loop on the idle-backlog drain nudge.
--
-- MEASURED over 34 hours to 2026-08-30: 419 of 988 fleet messages (42%) are
-- board_drive nudges, 12.3/hour, 574k characters (~143k tokens) pushed into
-- worker contexts.
--
-- CORRECTED 2026-08-30. Only this COMMENT changed; the DDL below is untouched.
-- Per lane over the same 34h:
--
--   lane          nudges   card events
--   mvs-infra         72        3,682
--   backend           55          357
--   byo-ray           42          295
--   mixpeek-cicd      40          298
--   ts-gke            22           96
--   amux              18          257
--
-- The correlation is POSITIVE: the hardest-nudged lanes are the MOST active,
-- and no lane is ignoring its board.
--
-- THE FIRST VERSION OF THIS COMMENT SAID "0 cards moved" for the top five. It
-- came from joining _amux_state_events on entity_type='issue', which holds 87
-- rows fleet-wide; card events are entity_type='task' (6,777). A predicate that
-- did not match what I believed, producing a confident and exactly inverted
-- conclusion. Kept here so the method is not re-derived.
--
-- What survives, measured independently: drain nudges are 419 of 988 fleet
-- messages (42%) over 34h, 12.3/hour, 574k characters.
--
-- So this table is a backstop for a genuinely DEAD lane, and NOT the fix for
-- the measured volume: an active lane's watermark moves every tick, so it is
-- never suppressed. The volume comes from the TRIGGER
-- (doing_count == 0 && eligible == 0 && drainable_backlog > 0), which cannot
-- tell "working fast with a drained todo queue" from "stuck" — mvs-infra keeps
-- its todo empty and holds backlog, so it looks identical to a stalled lane
-- while being the busiest board user in the fleet.
-- The cause is a missing feedback term, not a wrong constant.
-- `idle_backlog_drain_cooldown_s` scales cadence UP with backlog SIZE (base 2h,
-- halving every ~25 cards, 20m floor). Frequency is therefore a function of
-- backlog, and backlog is not a function of frequency, so the largest backlogs
-- saturate at the floor and stay there forever.
--
-- This table is the missing term, and it is a TABLE rather than a HashMap
-- because the auto-builder restarts this process on every commit: in-memory
-- state would reset several times an hour and the cap would never be reached
-- (the D1 "in-memory state is fiction" deviation).
CREATE TABLE IF NOT EXISTS board_drive_nudge_state (
    session         TEXT PRIMARY KEY,
    -- Consecutive drain nudges that were followed by no card movement at all.
    unheeded        INTEGER NOT NULL DEFAULT 0,
    last_nudge_at   REAL,
    -- MAX(issues.updated) for this lane when we last nudged. The next tick
    -- compares against it: anything larger means a card moved.
    board_mark      INTEGER,
    -- The escalation card filed when we gave up nudging, so we file ONE and
    -- not one per tick.
    escalated_card  TEXT
);
