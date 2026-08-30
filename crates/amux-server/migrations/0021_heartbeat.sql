-- Server liveness heartbeat + downtime history (AEAB-29).
--
-- amux was down for 4h26m on 2026-08-18 and the product recorded that fact
-- NOWHERE: the only evidence was a GAP between two lines in server-rs.log, and
-- an absence is not a signal. Nothing counted it, nothing named it, and the
-- restart logged the same "starting amux-rust" it logs after a routine 5-second
-- auto-adopt. The user found out because their prompts stopped working.
--
-- `server_heartbeat` is one row, stamped while the server runs. `server_downtime`
-- is the derived history: one row per boot that followed a gap, so "was it down,
-- and for how long" is answerable AFTER the fact, which is the only time anyone
-- ever asks.
CREATE TABLE IF NOT EXISTS server_heartbeat (
  id      INTEGER PRIMARY KEY CHECK (id = 1),
  beat_at REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS server_downtime (
  id        INTEGER PRIMARY KEY AUTOINCREMENT,
  -- Last moment the server is KNOWN to have been alive. The true stop is
  -- somewhere in (down_from, down_from + beat interval]; naming the last
  -- confirmed beat rather than a guess keeps the number honest.
  down_from REAL NOT NULL,
  up_at     REAL NOT NULL,
  seconds   REAL NOT NULL,
  -- Which listener noticed. Two amux-server processes share this DB, so this
  -- says which one won the race to observe the stale stamp.
  port      INTEGER
);

CREATE INDEX IF NOT EXISTS idx_server_downtime_up_at ON server_downtime(up_at);
