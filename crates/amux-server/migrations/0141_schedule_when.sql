-- Wake conditions (runtime_jobs/schedule_wake.rs, `when`). `when_check` is a
-- shell command run before each scheduled fire of a tmux schedule: exit 0
-- delivers the prompt, anything else skips the fire (status `skipped`, no
-- max_runs spent). NULL means no condition. `when_once` = 1 disables the
-- schedule after the first fire whose check passed and that delivered or
-- queued, so "wake me when X" fires once.
ALTER TABLE schedules ADD COLUMN when_check TEXT;
ALTER TABLE schedules ADD COLUMN when_once INTEGER NOT NULL DEFAULT 1;
