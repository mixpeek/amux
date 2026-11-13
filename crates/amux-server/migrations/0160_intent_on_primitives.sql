-- Intent first runs on amux's own primitives (docs/design/intent-first.md,
-- "Built from primitives"): an outcome's routing is a post in its own Messages
-- thread (api/intent_record.rs), ideas and goals are board cards
-- (api/intent_items.rs). The two link tables from 0159 held nothing on any
-- install when this landed (2026-11-13), so they are dropped.
DROP TABLE IF EXISTS intent_outcomes;
DROP TABLE IF EXISTS intent_items;
