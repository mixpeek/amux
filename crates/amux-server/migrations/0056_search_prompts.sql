-- Index the human's own prompts into universal search (AF-499).
--
-- `cmd_history` records everything sent TO a worker; `/api/search` indexes
-- cards, schedules, journal, memories, messages and workers, and not this.
-- Measured on the live DB, 2026-09-04: 13,277 indexed docs (12,933 task, 180
-- schedule, 136 journal, 25 message, 3 worker) beside 10,569 cmd_history rows
-- that were reachable only by knowing the MSG id you wanted. Searching
-- "HubSpot" returned 2 hits, both cards, while the conversation that discussed
-- HubSpot at length was invisible.
--
-- Reported by Ethan in a live onboarding session: "when you ask it to verify
-- something or you ask it a question, it's not going to do anything, it's just
-- gonna answer your question. And then the answer to that question is gonna be
-- lost in the ether."
--
-- ONLY type='user' — 1,646 of the 10,569 rows, 7.1MB. The rest are machine
-- traffic (auto-pickup dispatches, peer relays, scheduler commands) which is a
-- LOG, and indexing a log is how a searchable corpus stops discriminating
-- (ethos rule 5). A human's own words are the thing nobody can reconstruct.
--
-- SCOPE, stated because the entity name invites the wrong reading: this indexes
-- the QUESTION, not the ANSWER. amux does not store worker output anywhere, so
-- there is nothing to index yet. See AF-499 for that half, which is a decision
-- about storing model output and is Ethan's to make.
--
-- Maintenance is a trigger, matching every other family in 0013_search.sql:
-- cmd_history rows are INSERT-only and never updated, so one insert trigger and
-- one delete trigger is the whole contract.

CREATE TRIGGER IF NOT EXISTS search_prompt_ai AFTER INSERT ON cmd_history
WHEN new.type = 'user' BEGIN
    INSERT INTO search_docs (doc_id, entity_type, entity_id, title, body, scope, task_id, worker_id, link, meta, updated_at)
    VALUES ('prompt:'||new.id, 'prompt', new.id,
            substr(replace(new.text, char(10), ' '), 1, 80), new.text,
            new.session, new.card_id, new.session, '#history/'||new.id,
            json_object('session', new.session, 'origin', new.origin, 'card_id', new.card_id),
            new.ts)
    ON CONFLICT(doc_id) DO UPDATE SET
        title=excluded.title, body=excluded.body, scope=excluded.scope,
        task_id=excluded.task_id, worker_id=excluded.worker_id,
        link=excluded.link, meta=excluded.meta, updated_at=excluded.updated_at;
END;

CREATE TRIGGER IF NOT EXISTS search_prompt_ad AFTER DELETE ON cmd_history BEGIN
    DELETE FROM search_docs WHERE doc_id = 'prompt:'||old.id;
END;

-- Backfill what is already there. `INSERT OR IGNORE` so a re-run is a no-op;
-- the count is reported by GET /api/search/status, which is the only thing that
-- can print it (a migration cannot).
INSERT OR IGNORE INTO search_docs (doc_id, entity_type, entity_id, title, body, scope, task_id, worker_id, link, meta, updated_at)
SELECT 'prompt:'||id, 'prompt', id,
       substr(replace(text, char(10), ' '), 1, 80), text,
       session, card_id, session, '#history/'||id,
       json_object('session', session, 'origin', origin, 'card_id', card_id),
       ts
FROM cmd_history WHERE type = 'user';
