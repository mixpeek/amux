-- Tab layout presets get a scope (Ethan, 2026-10-24: the worker view's tab
-- list should save presets server side, like the homepage tab list does).
-- 'global' is the homepage tab bar, 'peek' the worker view. The name was the
-- whole key, so a worker-view preset named like a homepage one would have
-- replaced it; the key is now (scope, name). Existing rows are homepage
-- presets and keep their name, contents and created_at.
CREATE TABLE IF NOT EXISTS layout_presets_scoped (
    scope      TEXT NOT NULL DEFAULT 'global',
    name       TEXT NOT NULL,
    hidden     TEXT NOT NULL DEFAULT '[]',
    tab_order  TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL,
    PRIMARY KEY (scope, name)
);
INSERT OR IGNORE INTO layout_presets_scoped (scope, name, hidden, tab_order, created_at)
    SELECT 'global', name, hidden, tab_order, created_at FROM layout_presets;
DROP TABLE layout_presets;
ALTER TABLE layout_presets_scoped RENAME TO layout_presets;
