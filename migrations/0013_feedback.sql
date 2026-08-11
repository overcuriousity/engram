-- Real searches, captured before their results were seen, so the wording is the
-- searcher's own rather than the artifact's. That ordering is the whole point:
-- a query composed while looking at an artifact reuses its vocabulary, and every
-- retrieval system passes such a pair.
--
-- `judged_at` NULL means the event is still waiting for a verdict.
CREATE TABLE search_events (
  id          TEXT PRIMARY KEY,
  query       TEXT NOT NULL,
  door        TEXT NOT NULL,
  filters     TEXT NOT NULL DEFAULT '{}',
  query_vec   BLOB NOT NULL,
  vec_dim     INTEGER NOT NULL,
  embed_model TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  judged_at   INTEGER,
  verdict     TEXT,
  expect_id   TEXT,
  skips       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_events_pending ON search_events(judged_at, skips, created_at DESC);
CREATE INDEX idx_events_verdict ON search_events(verdict);

-- What the search offered, wider than what it showed. `shown` separates the two:
-- the judging card offers the whole pool, so a hit that ranked far down can still
-- be confirmed — which is the only way a ranking failure becomes measurable.
--
-- No foreign key on `artifact_id`, deliberately: deleting an artifact must not
-- erase the record of what was once returned. Dangling ids are skipped when
-- judging and when exporting.
CREATE TABLE search_candidates (
  event_id    TEXT NOT NULL REFERENCES search_events(id) ON DELETE CASCADE,
  rank        INTEGER NOT NULL,
  artifact_id TEXT NOT NULL,
  score       REAL NOT NULL,
  similarity  REAL,
  shown       INTEGER NOT NULL,
  PRIMARY KEY (event_id, rank)
);
