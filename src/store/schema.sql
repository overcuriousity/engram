-- The whole database, in one place.
--
-- engram had thirteen incremental migrations. They existed to carry a running
-- instance from one shape to the next, and while the project is in testing
-- there is no such instance: every database is created from nothing. A chain
-- of diffs is then thirteen files describing a schema none of them states, so
-- this file states it instead.
--
-- Applied on every connect, which is why every object is IF NOT EXISTS. It
-- creates what is missing and leaves what is there alone; it does not alter an
-- existing table. Changing a column means changing it here and recreating the
-- database.
--
-- Because it cannot alter a table, `migrate` reads the columns back out of this
-- file and checks them against the database afterwards. A table that predates a
-- column added here would otherwise survive startup and panic later, in a
-- request, on a column nothing ever added. One column per line is what that
-- check parses, so keep it that way.

-- ── Corpora ──────────────────────────────────────────────────────────────────
-- A captured source in full. The raw text is kept verbatim and forever: every
-- artifact is a claim about a passage of it, and a claim whose source is gone
-- cannot be checked.
CREATE TABLE IF NOT EXISTS corpora (
  id              TEXT PRIMARY KEY,
  raw_text        TEXT NOT NULL,
  origin          TEXT NOT NULL,
  title_hint      TEXT,
  content_hash    TEXT NOT NULL UNIQUE,
  status          TEXT NOT NULL,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL,
  -- Fraction of the corpus that reached an artifact. A window the synthesizer
  -- never managed leaves a hole, and this is how the hole stays visible.
  coverage        REAL,
  -- Bottom-k shingle hashes of raw_text, for near-duplicate detection. Empty
  -- for corpora captured before the column existed.
  shingles        TEXT,
  near_dupe_of    TEXT,
  near_dupe_score REAL,
  restored_at     INTEGER
);
CREATE INDEX IF NOT EXISTS idx_corpora_status  ON corpora(status);
CREATE INDEX IF NOT EXISTS idx_corpora_created ON corpora(created_at DESC);

-- ── Artifacts ────────────────────────────────────────────────────────────────
-- One atomic piece of knowledge, rewritten to stand alone. Superseding hides
-- an artifact and names its replacement; nothing is ever merged or rewritten
-- in place.
CREATE TABLE IF NOT EXISTS artifacts (
  id               TEXT PRIMARY KEY,
  corpus_id        TEXT NOT NULL REFERENCES corpora(id) ON DELETE CASCADE,
  ordinal          INTEGER NOT NULL,
  text             TEXT NOT NULL,
  -- Line range in the corpus this came from, as JSON.
  corpus_span      TEXT,
  title            TEXT,
  category         TEXT,
  tags             TEXT NOT NULL DEFAULT '[]',
  embed_state      TEXT NOT NULL DEFAULT 'pending',
  embed_model      TEXT,
  created_at       INTEGER NOT NULL,
  -- Bumped on every edit. An embed job carries the revision it read, so a
  -- chunk edited mid-embed is not reported as indexed.
  embed_rev        INTEGER NOT NULL DEFAULT 0,
  -- Which window of the corpus produced it. Artifacts are replaced per
  -- window, so a retry of one window cannot disturb the others.
  segment_idx      INTEGER,
  -- What verification could not vouch for, and why.
  flags            TEXT,
  flag_detail      TEXT,
  superseded_by    TEXT,
  caveats          TEXT NOT NULL DEFAULT '[]',
  status           TEXT NOT NULL DEFAULT 'active',
  last_verified_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_artifacts_corpus     ON artifacts(corpus_id, ordinal);
CREATE INDEX IF NOT EXISTS idx_artifacts_embed      ON artifacts(embed_state);
CREATE INDEX IF NOT EXISTS idx_artifacts_window     ON artifacts(corpus_id, segment_idx);
CREATE INDEX IF NOT EXISTS idx_artifacts_superseded ON artifacts(superseded_by);
CREATE INDEX IF NOT EXISTS idx_artifacts_status     ON artifacts(status);

-- ── Segments ─────────────────────────────────────────────────────────────────
-- The windows a corpus was split into for synthesis. One window is one
-- inference call and one queue unit, so these rows say what the units are and
-- which of them have resolved; the attempt count and the backoff belong to the
-- job, not here.
CREATE TABLE IF NOT EXISTS segments (
  corpus_id  TEXT    NOT NULL REFERENCES corpora(id) ON DELETE CASCADE,
  idx        INTEGER NOT NULL,
  -- Where the window came from. Used to render an artifact's source, and only
  -- for that: it cannot reproduce the window, because the splitter cuts inside
  -- a line when a corpus has no line boundaries.
  start_line INTEGER NOT NULL,
  end_line   INTEGER NOT NULL,
  -- The window itself, as the splitter produced it. Authoritative.
  text       TEXT    NOT NULL DEFAULT '',
  -- How many leading lines of `text` come from outside start_line..end_line:
  -- the heading the splitter carries into a window that continues a section.
  -- An offset measured inside the window is that much too high without it.
  carry_lines INTEGER NOT NULL DEFAULT 0,
  state      TEXT    NOT NULL DEFAULT 'pending',  -- pending | done | failed
  -- Dead since 2026-08-13. A window is its own queue unit now, so `jobs.attempts`
  -- is the count that governs its backoff and its settling, and two counters for
  -- one thing is exactly what made the incident behind that change so hard to
  -- read. Left in place because `migrate` cannot drop a column; remove it
  -- whenever the database is next recreated, and do not start writing to it.
  attempts   INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  PRIMARY KEY (corpus_id, idx)
);
CREATE INDEX IF NOT EXISTS idx_segments_state ON segments(corpus_id, state);

-- ── Jobs ─────────────────────────────────────────────────────────────────────
-- The work queue. There is no terminal state: attempts past the maximum only
-- widen the backoff, because an endpoint that was loading a model says nothing
-- about the work.
CREATE TABLE IF NOT EXISTS jobs (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  stage       TEXT NOT NULL,
  target_kind TEXT NOT NULL,
  target_id   TEXT NOT NULL,
  state       TEXT NOT NULL DEFAULT 'pending',
  attempts    INTEGER NOT NULL DEFAULT 0,
  run_after   INTEGER NOT NULL DEFAULT 0,
  last_error  TEXT,
  claimed_at  INTEGER,
  created_at  INTEGER NOT NULL DEFAULT 0,
  -- Position within the batch of units armed together: the window index, the
  -- judge pair's index, the embed batch number. Zero for singletons. Claiming
  -- orders by it, so every document's first window runs before any document's
  -- second — which is what stops a large ingest starving a small one behind it.
  seq         INTEGER NOT NULL DEFAULT 0,
  UNIQUE(stage, target_id)
);
-- Ready work in the order `claim_job` takes it: least-tried first, then oldest.
--
-- The column order is the query's, not the filter's. `run_after` last looks
-- wrong until you try it the other way round: an inequality ends an index's
-- usable ordering, so `(state, run_after, attempts, id)` finds the ready rows
-- and then sorts them in a temp B-tree on every poll. This walks `state`,
-- `attempts`, `id` in claim order, tests `run_after` on each entry, and stops
-- at the first row that is ready — covering, and no sort.
--
-- Dropped by its old name rather than widened in place. `migrate` applies this
-- file to every database on every start, and `CREATE INDEX IF NOT EXISTS` on a
-- name that already exists is a silent no-op, so a deployment carrying an
-- earlier version of this index would have kept it. The drop is how an existing
-- base actually picks the new one up.
DROP INDEX IF EXISTS idx_jobs_ready;
DROP INDEX IF EXISTS idx_jobs_claim;
CREATE INDEX IF NOT EXISTS idx_jobs_claim2  ON jobs(state, attempts, seq, id, run_after);
CREATE INDEX IF NOT EXISTS idx_jobs_created ON jobs(created_at);

-- ── Consolidation ────────────────────────────────────────────────────────────
-- Two artifacts that may say the same thing differently. The only question a
-- person is ever asked.
CREATE TABLE IF NOT EXISTS artifact_pairs (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  a_id           TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  b_id           TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  score          REAL NOT NULL,
  state          TEXT NOT NULL DEFAULT 'pending',
  detail         TEXT,
  created_at     INTEGER NOT NULL,
  judge_attempts INTEGER NOT NULL DEFAULT 0,
  obsolete_id    TEXT REFERENCES artifacts(id),
  UNIQUE(a_id, b_id)
);
CREATE INDEX IF NOT EXISTS idx_pairs_state ON artifact_pairs(state, created_at DESC);

-- ── Relevance feedback ───────────────────────────────────────────────────────
-- A search, its query vector, and what came back — so a judgement made later
-- can be scored against the ranking that produced it.
CREATE TABLE IF NOT EXISTS search_events (
  id          TEXT PRIMARY KEY,
  query       TEXT NOT NULL,
  door        TEXT NOT NULL,
  -- Who searched, where the door knows. Coalescing needs it: a typing burst
  -- belongs to one person, and folding keyed on the door alone let one
  -- operator's half-typed query swallow another's finished one.
  scope       TEXT,
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
CREATE INDEX IF NOT EXISTS idx_events_pending ON search_events(judged_at, skips, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_events_verdict ON search_events(verdict);

CREATE TABLE IF NOT EXISTS search_candidates (
  event_id    TEXT NOT NULL REFERENCES search_events(id) ON DELETE CASCADE,
  rank        INTEGER NOT NULL,
  artifact_id TEXT NOT NULL,
  score       REAL NOT NULL,
  similarity  REAL,
  shown       INTEGER NOT NULL,
  PRIMARY KEY (event_id, rank)
);

-- ── Auth ─────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS sessions (
  id         TEXT PRIMARY KEY,
  subject    TEXT NOT NULL,
  email      TEXT,
  expires_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_expiry ON sessions(expires_at);

CREATE TABLE IF NOT EXISTS api_tokens (
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL,
  token_hash   TEXT NOT NULL,
  subject      TEXT NOT NULL,
  created_at   INTEGER NOT NULL,
  last_used_at INTEGER,
  revoked_at   INTEGER
);
