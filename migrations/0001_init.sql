CREATE TABLE sources (
  id           TEXT PRIMARY KEY,
  raw_text     TEXT NOT NULL,
  origin       TEXT NOT NULL,
  title_hint   TEXT,
  content_hash TEXT NOT NULL UNIQUE,
  status       TEXT NOT NULL,
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL
);
CREATE INDEX idx_sources_status  ON sources(status);
CREATE INDEX idx_sources_created ON sources(created_at DESC);

CREATE TABLE chunks (
  id          TEXT PRIMARY KEY,
  source_id   TEXT NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
  ordinal     INTEGER NOT NULL,
  text        TEXT NOT NULL,
  source_span TEXT,
  title       TEXT,
  category    TEXT,
  tags        TEXT NOT NULL DEFAULT '[]',
  embed_state TEXT NOT NULL DEFAULT 'pending',
  embed_model TEXT,
  created_at  INTEGER NOT NULL
);
CREATE INDEX idx_chunks_source ON chunks(source_id, ordinal);
CREATE INDEX idx_chunks_embed  ON chunks(embed_state);

CREATE TABLE jobs (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  stage       TEXT NOT NULL,
  target_kind TEXT NOT NULL,
  target_id   TEXT NOT NULL,
  state       TEXT NOT NULL DEFAULT 'pending',
  attempts    INTEGER NOT NULL DEFAULT 0,
  run_after   INTEGER NOT NULL DEFAULT 0,
  last_error  TEXT,
  claimed_at  INTEGER,
  UNIQUE(stage, target_id)
);
CREATE INDEX idx_jobs_ready ON jobs(state, run_after);

CREATE TABLE sessions (
  id         TEXT PRIMARY KEY,
  subject    TEXT NOT NULL,
  email      TEXT,
  expires_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX idx_sessions_expiry ON sessions(expires_at);

CREATE TABLE api_tokens (
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL,
  token_hash   TEXT NOT NULL,
  subject      TEXT NOT NULL,
  created_at   INTEGER NOT NULL,
  last_used_at INTEGER,
  revoked_at   INTEGER
);
