-- Primitive integrity audit 2026-11-26, step 2: the dormant typed model's
-- planning and budget tables go, with the legacy `tasks` table.
--
-- None has a client. Measured on the owner's database before writing this:
-- every table below held 0 rows except `tasks` (1 row, written 2026-02-20),
-- and the request log held no call to /api/harness/goals, /planning-nodes or
-- /budgets in 17.1M requests since 2026-11-13. The routes answer 410 now.
--
-- Before this runs, db/migrate.rs writes any rows these tables hold to
-- <amux home>/archive/primitive-audit-2026-11-26/<table>.json (never
-- overwriting an existing file) and refuses to run it when there are rows and
-- nowhere to write them. Their indexes and the immutability triggers on the
-- revision tables drop with them.
--
-- Kept on purpose: `_amux_leases`, `_amux_attempts` and `_amux_decompositions`
-- (the orchestrator loop still uses them when AMUX_RS_ORCH=1 turns it back on
-- for one release), `_amux_memories` (Intent feedback reads and writes it) and
-- `_amux_workers` (the record of rust-managed workers).
DROP TABLE IF EXISTS _amux_plan_revisions;
DROP TABLE IF EXISTS _amux_current_plans;
DROP TABLE IF EXISTS _amux_planning_nodes;
DROP TABLE IF EXISTS _amux_goal_contract_revisions;
DROP TABLE IF EXISTS _amux_goal_contracts;
DROP TABLE IF EXISTS _amux_task_budgets;
DROP TABLE IF EXISTS tasks;
