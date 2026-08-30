-- Backfill AF-99's discriminator for rows filed before 0022.
--
-- Separate file because the migration runner executes plain SQL BEFORE applying
-- `-- ADDCOL:` directives, so an UPDATE in 0022 would run against a column that
-- does not exist yet.
--
-- This is what corrects the live false record: the 2026-08-19 row claiming a
-- 759-minute outage gets the request count that was served during it, and every
-- reader that filters on `requests_during = 0` stops treating it as downtime.
UPDATE server_downtime
   SET requests_during = (
         SELECT COUNT(*) FROM _amux_request_log
          WHERE ts > server_downtime.down_from
            AND ts < server_downtime.up_at
       )
 WHERE requests_during IS NULL;
