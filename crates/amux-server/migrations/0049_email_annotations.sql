-- Human judgments on ranked email, and the meta-properties that make the
-- ranking improvable (AMUX-3998).
--
-- WHY THE SCORE AND THEMES ARE STORED ALONGSIDE THE VERDICT. The verdict alone
-- ("rejected") teaches nothing: to learn, the next inference has to know WHAT
-- the ranker believed at the moment a human disagreed with it. "Rejected, and at
-- the time we scored it 14 on themes [Newsletters, Billing]" is a correction.
-- "Rejected" is an opinion about a message nobody can look up later, because the
-- themes will have been recomputed by then.
CREATE TABLE IF NOT EXISTS email_annotations (
  account               TEXT NOT NULL,
  message_id            TEXT NOT NULL,
  -- approved | rejected | NULL. NULL is genuinely different from both: it means
  -- unfiled, not "neither", so an unjudged inbox cannot be read as approved.
  verdict               TEXT,
  flagged               INTEGER NOT NULL DEFAULT 0,
  -- Manual nudge, added to the computed score. Signed: derank is negative.
  rank_delta            REAL NOT NULL DEFAULT 0,
  note                  TEXT,
  -- What the ranker thought WHEN the human judged. This is the training signal.
  score_at_annotation   REAL,
  themes_at_annotation  TEXT,
  from_addr             TEXT,
  subject               TEXT,
  created               REAL NOT NULL,
  updated               REAL NOT NULL,
  PRIMARY KEY (account, message_id)
);

CREATE INDEX IF NOT EXISTS idx_email_annotations_verdict
  ON email_annotations(verdict, updated DESC);
