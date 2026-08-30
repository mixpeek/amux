-- AMUX-3609: the board had no queryable close time.
--
-- `issues.updated` is LAST TOUCH. A card closed on 08-07 that anyone commented
-- on since reads with tonight's timestamp, so every time-window question about
-- closed cards was silently answered by "who typed most recently". Measured on
-- BACKE-3183: closed 2026-08-07, `updated` = 2026-08-24 03:18. A sweep built on
-- it returned 73 confident-looking candidates and was wrong.
--
-- `issues.log` cannot substitute. `append_log` formats `` `{hhmm}` {line} `` —
-- HH:MM, no date — so nothing in that column distinguishes 05:08 today from
-- 05:08 in June. Not lossy: impossible.
--
-- `last_verified_at` already exists for exactly this question, for exactly one
-- status out of seven. The store's own design agrees the question matters.
--
-- # Why a column rather than a derivation
--
-- `_amux_state_events` DOES record every transition properly, with `at` as
-- full ISO-8601 to the microsecond and `mutation` naming from/to. It is also
-- REAPED at 14 days (AMUX_STATE_EVENTS_RETAIN_DAYS, runtime_jobs/storage.rs)
-- because it is a delta-sync journal that grows ~3,900 rows/day. That retention
-- is right for what the table is FOR and wrong as the sole home of a permanent
-- fact: you cannot derive a permanent fact from a table with a 14-day TTL. The
-- fact was being recorded correctly and then deleted on a timer.
--
-- # What the backfill can and cannot recover, stated rather than implied
--
-- Exactly the last 14 days, precisely, from the journal. Everything older is
-- NULL permanently — roughly 10,700 of 10,905 rows at the time of writing. That
-- is not a weak backfill; it is a fact about what was retained.
--
-- SO: NULL MEANS "NOT RECORDED", NEVER "NOT CLOSED". A consumer that reads NULL
-- as still-open will be wrong about almost the entire board. Filter on
-- `closed_at IS NOT NULL` when you need a date, and never on `closed_at IS
-- NULL` to mean anything at all.
--
-- ADDCOL: issues closed_at INTEGER

-- Backfill from the surviving journal window. Scoped to cards whose CURRENT
-- status is terminal, matching the write rule (set on transition into a
-- terminal status, cleared on transition out) — so a card that closed and was
-- reopened correctly gets nothing here.
--
-- MAX(at) rather than MIN: a card that closed, reopened and closed again should
-- report its LATEST close, which is what the live write rule will record from
-- now on. Picking the first would make the backfilled rows disagree with every
-- row written after this migration.
--
-- `strftime` is what parses the microseconds and the +00:00 offset; verified
-- against the live table before relying on it. A row it cannot parse yields
-- NULL and is skipped by the WHERE, so an unparseable timestamp leaves the
-- column NULL rather than writing a wrong date — the loud direction stays the
-- default.
-- INDEX FIRST, and it is load-bearing rather than tidy. `_amux_state_events`
-- carried exactly ONE index (on `rev`), so a correlated lookup by entity is a
-- full scan of ~79,000 rows. The backfill below touches 7,281 terminal cards,
-- which without this is ~576 million row visits with two json_extract calls and
-- a strftime on each, inside the exclusive transaction a migration runs in, at
-- server startup. That is not a slow query, it is an outage.
--
-- It also fixes a hot path that was already paying the same cost: `/api/why`
-- selects `WHERE entity_type = ?1 AND entity_id = ?2` on every lineage load and
-- was full-scanning the journal every time. Surfacing that trail on the card
-- (AMUX-2393, 68d54aaf) is what made it hot.
CREATE INDEX IF NOT EXISTS idx_amux_state_events_entity
    ON _amux_state_events(entity_type, entity_id);

UPDATE issues
   SET closed_at = (
        SELECT CAST(MAX(strftime('%s', e.at)) AS INTEGER)
          FROM _amux_state_events e
         WHERE e.entity_type = 'task'
           AND e.entity_id = issues.id
           AND json_extract(e.mutation, '$.kind') = 'status_changed'
           AND json_extract(e.mutation, '$.to') IN ('done', 'verified', 'discarded')
           AND strftime('%s', e.at) IS NOT NULL
   )
 WHERE status IN ('done', 'verified', 'discarded')
   AND closed_at IS NULL;

-- The motivating query is "closed cards in a time window", which is a range
-- scan over a column that is NULL for most rows. A partial index keeps it off
-- the ~10,700 rows that can never match.
CREATE INDEX IF NOT EXISTS idx_issues_closed_at
    ON issues(closed_at) WHERE closed_at IS NOT NULL;
