-- A PER-CARD BUDGET FOR THE TWO NUDGES THAT RE-ARM ON THEIR OWN DEMANDS.
--
-- The idle-with-card reminder and the blocker-recovery review are each
-- suppressed only by an identity hash of the card's state, and that hash
-- includes next_action, acceptance_criteria, evidence, gate and desc, the exact
-- fields both prompts tell the worker to write. Compliance rotates the key and
-- the same nudge re-delivers. The 3-per-24h per-card budget that used to bound
-- this was deleted on 2026-09-17 (7a56d2d4, 15b72193, e00fca78) and survived
-- only as comments. Measured on the live db: board-drive nudges went from 91 a
-- day on 09-16 to 649 on 09-19; one card took 39 blocker reviews, another 13
-- plus 34 idle reminders (MB-55).
--
-- A table, not a HashMap, for the reason board_drive_nudge_state gives: the
-- auto-builder restarts this process on every commit.
CREATE TABLE IF NOT EXISTS board_drive_nudge_budget (
    session        TEXT NOT NULL,
    card           TEXT NOT NULL,
    kind           TEXT NOT NULL,
    -- Deliveries so far for this (session, card, kind) since the card last
    -- changed status.
    n              INTEGER NOT NULL DEFAULT 0,
    first_at       REAL NOT NULL,
    last_at        REAL NOT NULL,
    -- Earliest time the next delivery is admitted: 1h, then 4h, then 24h.
    next_at        REAL NOT NULL,
    -- The card's status at the last delivery. A status change resets the
    -- budget: the worker did something, so the next nudge is about new state.
    status_at_last TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (session, card, kind)
);
