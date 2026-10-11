-- Group multiple pinned files into a single tabbed overlay.
-- All rows sharing the same non-NULL group_id launch as tabs in one window.
-- NULL group_id = standalone pin (backwards compatible).
ALTER TABLE pinned_notes ADD COLUMN group_id TEXT;
CREATE INDEX IF NOT EXISTS idx_pinned_notes_group ON pinned_notes(group_id) WHERE group_id IS NOT NULL;
