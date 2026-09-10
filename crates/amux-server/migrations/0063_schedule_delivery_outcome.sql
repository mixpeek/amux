-- AF-648: a refused (or otherwise non-landed) fire still advances last_run
-- and run_count -- deliberately, see the AF-515 comment on fire_one in
-- scheduler.rs -- so GET /api/schedules has no way to say a schedule is
-- paused rather than healthy. These two columns carry the outcome of the
-- MOST RECENT attempt without touching that clock.
ALTER TABLE schedules ADD COLUMN last_delivery TEXT;
ALTER TABLE schedules ADD COLUMN last_refusal_reason TEXT;
