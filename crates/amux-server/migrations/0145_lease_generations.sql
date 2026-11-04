-- The last generation each task's orchestrator lease reached (implicit state
-- machines inventory, 2026-10-31, #7). A reclaim deletes the _amux_leases
-- row, and the claim computed the next generation from that same row, so
-- every lease was generation 0 and a dead worker's late write could not be
-- told from the live one's. The reclaim records the generation here and the
-- next claim continues from it.
CREATE TABLE IF NOT EXISTS _amux_lease_generations (
    task_id    TEXT PRIMARY KEY,
    generation INTEGER NOT NULL
);
