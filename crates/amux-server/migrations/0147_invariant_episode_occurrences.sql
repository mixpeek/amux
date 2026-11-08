-- AF-1126: the autofix filing threshold ("must stay breached across at least
-- this many evaluations") read `occurrences`, which counts every failure since
-- the pair was first seen and is never reset. An incident with 117 lifetime
-- failures that had passed for an hour filed a card on ONE failing evaluation,
-- and the card said it "has not self-healed" while it passed again 52 s later
-- (amux-gtm, session.registered_lane_is_running, 2026-11-01 00:43:34Z).
--
-- `episode_occurrences` counts failures in the current unbroken failing run:
-- the upsert adds one while the incident is open and restarts at 1 on a reopen.
-- An incident open now has an unknown current run, so it carries its lifetime
-- count, which keeps today's filing for every incident already failing.
ALTER TABLE _amux_invariant_incident ADD COLUMN episode_occurrences INTEGER NOT NULL DEFAULT 0;
UPDATE _amux_invariant_incident SET episode_occurrences = occurrences WHERE resolved_at IS NULL;
