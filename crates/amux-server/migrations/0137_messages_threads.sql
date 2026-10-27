-- Messages, Slack-grade (owner, 2026-10-27): threads, edits, deletes,
-- reactions, attachments (audio notes), full-text search, profile pictures
-- and link previews. Times are epoch milliseconds.

-- A reply's thread root. NULL is a top-level post.
ALTER TABLE conversation_posts ADD COLUMN parent_id INTEGER;
ALTER TABLE conversation_posts ADD COLUMN edited_at INTEGER;
-- A deleted post keeps its row (threads and read markers point at it) and
-- loses its body.
ALTER TABLE conversation_posts ADD COLUMN deleted_at INTEGER;
-- {"👍": ["person:owner", "worker:amux"], ...}
ALTER TABLE conversation_posts ADD COLUMN reactions TEXT NOT NULL DEFAULT '{}';
-- [{"kind":"audio","path":...,"mime":...,"dur_ms":...,"transcript":...}, ...]
ALTER TABLE conversation_posts ADD COLUMN attachments TEXT NOT NULL DEFAULT '[]';
CREATE INDEX IF NOT EXISTS idx_conversation_posts_parent ON conversation_posts (parent_id, id) WHERE parent_id IS NOT NULL;

CREATE VIRTUAL TABLE IF NOT EXISTS conversation_posts_fts USING fts5(
    body,
    content='conversation_posts',
    content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);
CREATE TRIGGER IF NOT EXISTS conversation_posts_fts_ai AFTER INSERT ON conversation_posts BEGIN
    INSERT INTO conversation_posts_fts(rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER IF NOT EXISTS conversation_posts_fts_ad AFTER DELETE ON conversation_posts BEGIN
    INSERT INTO conversation_posts_fts(conversation_posts_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;
CREATE TRIGGER IF NOT EXISTS conversation_posts_fts_au AFTER UPDATE OF body ON conversation_posts BEGIN
    INSERT INTO conversation_posts_fts(conversation_posts_fts, rowid, body) VALUES ('delete', old.id, old.body);
    INSERT INTO conversation_posts_fts(rowid, body) VALUES (new.id, new.body);
END;
INSERT INTO conversation_posts_fts(conversation_posts_fts) VALUES ('rebuild');

-- A person's or a worker's picture and display name, by author key
-- (person:<email|owner>, worker:<name>).
CREATE TABLE IF NOT EXISTS profiles (
  key          TEXT PRIMARY KEY,
  avatar       TEXT,
  display_name TEXT,
  updated_at   INTEGER NOT NULL
);

-- What a link in a post unfurls to, fetched once and kept.
CREATE TABLE IF NOT EXISTS link_previews (
  url         TEXT PRIMARY KEY,
  status      INTEGER NOT NULL,
  title       TEXT,
  description TEXT,
  image       TEXT,
  site        TEXT,
  fetched_at  INTEGER NOT NULL
);
