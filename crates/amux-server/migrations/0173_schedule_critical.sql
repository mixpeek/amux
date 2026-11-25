-- `critical` = 1 lets a kind=shell schedule fire while the host guard reads
-- critical (scheduler.rs, verdict host_pressure_critical_fire): a finish-path
-- watcher on a loaded box held 25+ minutes behind the guard (mixpeek-dev,
-- 2026-11-25). The API refuses it on any other kind, so no model turn rides
-- past the guard.
ALTER TABLE schedules ADD COLUMN critical INTEGER DEFAULT 0;
