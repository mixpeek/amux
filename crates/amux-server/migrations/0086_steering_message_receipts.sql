-- One durable identity joins a queued human message to its steering delivery.
-- Text and timestamps are not identities: identical messages can be queued in
-- sequence, and a restart can complete delivery before the history row is linked.
-- ADDCOL: cmd_history queue_id TEXT
CREATE UNIQUE INDEX IF NOT EXISTS idx_cmd_history_queue_id
    ON cmd_history(queue_id) WHERE queue_id IS NOT NULL;

-- A delivery (or void) settles the exact Messages row. The history write and
-- receipt update are one SQLite transaction, regardless of which delivery
-- path inserted the steering_history row.
CREATE TRIGGER IF NOT EXISTS steering_receipt_after_history
AFTER INSERT ON steering_history BEGIN
    UPDATE cmd_history SET
        delivery = CASE
            WHEN NEW.outcome LIKE 'void:%' THEN 'voided'
            WHEN NEW.outcome LIKE 'interrupted:%' THEN 'uncertain'
            ELSE delivery END,
        delivered_at = CASE WHEN NEW.outcome LIKE 'sent%'
            THEN CAST(NEW.delivered_at * 1000 AS INTEGER) ELSE NULL END,
        submit_verdict = CASE
            WHEN NEW.outcome LIKE 'sent%retry%' THEN 'retried'
            WHEN NEW.outcome LIKE 'sent%' THEN 'confirmed'
            WHEN NEW.outcome LIKE 'void:%' THEN NULL
            ELSE 'unverified' END
    WHERE queue_id = NEW.id;
END;

-- A fast delivery can win the race before the Messages row is linked. Linking
-- later settles from the already-durable history with the same facts.
CREATE TRIGGER IF NOT EXISTS steering_receipt_after_link
AFTER UPDATE OF queue_id ON cmd_history WHEN NEW.queue_id IS NOT NULL BEGIN
    UPDATE cmd_history SET
        delivery = CASE
            WHEN (SELECT outcome FROM steering_history WHERE id=NEW.queue_id) LIKE 'void:%' THEN 'voided'
            WHEN (SELECT outcome FROM steering_history WHERE id=NEW.queue_id) LIKE 'interrupted:%' THEN 'uncertain'
            ELSE delivery END,
        delivered_at = CASE WHEN (SELECT outcome FROM steering_history WHERE id=NEW.queue_id) LIKE 'sent%'
            THEN CAST((SELECT delivered_at FROM steering_history WHERE id=NEW.queue_id) * 1000 AS INTEGER) ELSE NULL END,
        submit_verdict = CASE
            WHEN (SELECT outcome FROM steering_history WHERE id=NEW.queue_id) LIKE 'sent%retry%' THEN 'retried'
            WHEN (SELECT outcome FROM steering_history WHERE id=NEW.queue_id) LIKE 'sent%' THEN 'confirmed'
            WHEN (SELECT outcome FROM steering_history WHERE id=NEW.queue_id) LIKE 'void:%' THEN NULL
            WHEN EXISTS(SELECT 1 FROM steering_history WHERE id=NEW.queue_id) THEN 'unverified'
            ELSE submit_verdict END
    WHERE id = NEW.id;
END;

-- Repair old, unlinked rows only when the session, exact text and enqueue time
-- identify one message and one receipt. Ambiguous duplicates stay unknown;
-- guessing would be worse than displaying an old queued row for review.
UPDATE cmd_history SET queue_id = (
    SELECT h.id FROM steering_history h
    WHERE h.session = cmd_history.session
      AND h.text = cmd_history.text
      AND ABS(CAST(h.queued_at * 1000 AS INTEGER) - cmd_history.queued_at) <= 5000
      AND NOT EXISTS (SELECT 1 FROM cmd_history linked WHERE linked.queue_id = h.id)
) WHERE queue_id IS NULL AND delivery = 'queued' AND queued_at IS NOT NULL
  AND (SELECT COUNT(*) FROM steering_history h
       WHERE h.session = cmd_history.session AND h.text = cmd_history.text
         AND ABS(CAST(h.queued_at * 1000 AS INTEGER) - cmd_history.queued_at) <= 5000) = 1
  AND (SELECT COUNT(*) FROM cmd_history other
       WHERE other.session = cmd_history.session AND other.text = cmd_history.text
         AND other.delivery = 'queued' AND other.queued_at IS NOT NULL
         AND ABS(other.queued_at - cmd_history.queued_at) <= 5000) = 1;
