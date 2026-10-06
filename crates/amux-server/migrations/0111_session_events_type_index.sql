-- board_drive::reconcile_child_task_claim reads a lane's newest task markers
-- (session = ? AND type IN (...) ORDER BY ts DESC, id DESC LIMIT 128) inside
-- a write transaction. With only idx_sev_session it sorted every event of the
-- session: 29 s on 753k rows under load, holding the single writer 47-74 s and
-- wedging every board write fleet-wide (2026-10-06 12:40Z). This index answers
-- it in 0.08 s. Already created by hand on the live box; IF NOT EXISTS.
CREATE INDEX IF NOT EXISTS idx_sev_session_type_ts ON session_events(session, type, ts DESC, id DESC);
