-- Conversations (Messages tab): one channel per group, direct messages
-- between people, and one shared thread per worker. A post that @mentions a
-- worker is delivered through the ordinary send path, so the worker's own
-- Messages tab (cmd_history) keeps being the record of what reached it;
-- `deliveries` keeps the outcome of each of those sends beside the post.
CREATE TABLE IF NOT EXISTS conversation_posts (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    conv        TEXT NOT NULL,             -- g:<group> | p:<a>|<b> | w:<worker>
    author_key  TEXT NOT NULL,             -- person:<email|owner> | worker:<name>
    author_name TEXT NOT NULL,
    body        TEXT NOT NULL,
    mentions    TEXT NOT NULL DEFAULT '[]',
    deliveries  TEXT NOT NULL DEFAULT '[]',
    ts          INTEGER NOT NULL           -- unix millis
);
CREATE INDEX IF NOT EXISTS idx_conversation_posts_conv ON conversation_posts(conv, id);

-- Per-reader read marker, for unread and mention counts.
CREATE TABLE IF NOT EXISTS conversation_reads (
    reader  TEXT NOT NULL,
    conv    TEXT NOT NULL,
    last_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (reader, conv)
);
