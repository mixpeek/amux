-- Primitive integrity audit 2026-11-26, step 3: `_amux_messages` goes.
--
-- It was a second message store beside cmd_history (the record behind every
-- MSG-<n>) with no client left. Before this runs, db/migrate.rs writes its
-- rows, and the orchestrator commands that pointed at them, to
-- <amux home>/archive/primitive-audit-2026-11-26/ (and refuses to run it when
-- there are rows and nowhere to write them). Nothing is exported here.
--
-- WorkerCommand::DeliverMessage now carries a cmd_history id (a number). A
-- command still naming an `_amux_messages` id (a string) can no longer be
-- read, so it is rewritten to the number 0, which names no history row, and
-- one that had not finished is dead-lettered with the old id in its reason.
-- Finished ones keep their state.
UPDATE _amux_commands
   SET state = json_object('state', 'dead_lettered', 'reason',
         'its message was in _amux_messages, removed 2026-11-26 (archived under archive/primitive-audit-2026-11-26): ' ||
         json_extract(command, '$.data'))
 WHERE json_extract(command, '$.kind') = 'deliver_message'
   AND json_type(command, '$.data') = 'text'
   AND json_extract(state, '$.state') NOT IN ('confirmed', 'dead_lettered');
UPDATE _amux_commands
   SET command = json_object('kind', 'deliver_message', 'data', 0)
 WHERE json_extract(command, '$.kind') = 'deliver_message'
   AND json_type(command, '$.data') = 'text';

-- Its search documents go with it (the triggers on the table drop with it).
DELETE FROM search_docs WHERE entity_type = 'message';
DROP TABLE IF EXISTS _amux_messages;
