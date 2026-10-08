-- Provider capacity is an execution hold, never a failed quality review.
ALTER TABLE card_contracts ADD COLUMN review_retry_at REAL;
ALTER TABLE card_contracts ADD COLUMN review_capacity_model TEXT;
ALTER TABLE card_contracts ADD COLUMN review_capacity_refunded_at REAL;
ALTER TABLE card_prereviews ADD COLUMN retry_at REAL;
ALTER TABLE card_prereviews ADD COLUMN capacity_model TEXT;
