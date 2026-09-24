-- 0087_worker_type.sql — first-class worker type (ACW-1/ACW-2).
--
-- A worker type selects the execution adapter and the primary output
-- renderer (`coding` = terminal session + peek, `chat` = headless provider
-- turns + chat transcript). Every existing row is a coding worker, which is
-- exactly what the DEFAULT gives it; no data is rewritten. Open string, not a
-- CHECK-constrained enum (Invariant 8): a new type is a registry entry, not a
-- schema change. Env-file workers carry the same fact as CC_WORKER_TYPE.
ALTER TABLE _amux_workers ADD COLUMN worker_type TEXT NOT NULL DEFAULT 'coding';
