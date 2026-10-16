-- Scheduled delivery for a queued message (epoch seconds; 0 = none).
-- Replaces delay_after_idle_s, whose idle signal stopped being written on
-- 2026-08-09. See STEER_DUE_AT_SQL in api/session_verbs.rs.
ALTER TABLE steering_queue ADD COLUMN deliver_at REAL NOT NULL DEFAULT 0;
