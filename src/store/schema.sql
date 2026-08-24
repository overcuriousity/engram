-- The whole database, in one place.
--
-- One statement of what the schema is, rather than a chain of diffs describing
-- how it came to be. Every database is created from nothing by this file.
--
-- Applied on every connect, which is why every object is IF NOT EXISTS. It
-- creates what is missing and leaves what is there alone; it does not alter an
-- existing table. Changing a column means changing it here and recreating the
-- database.

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
  -- Bottom-k shingle hashes of raw_text, for near-duplicate detection.
  shingles        TEXT,
  near_dupe_of    TEXT,
  near_dupe_score REAL,
  -- Where this text was read, when it was read somewhere. `origin` is the
  -- channel it arrived through and this is the location it came from; one
  -- column cannot be both without losing the channel.
  source_url      TEXT,
  restored_at     INTEGER,
  -- What a door knew about the capture beyond the text: a note, file facts,
  -- EXIF. Namespaced JSON, '{}' when nothing was recorded.
  metadata        TEXT NOT NULL DEFAULT '{}'
);

-- The bytes an image corpus was captured from. `bytes` is the upload exactly
-- as it arrived — the verbatim source, as `raw_text` is for a paste — and
-- `preview` is the one derived copy: orientation applied, downscaled, JPEG.
CREATE TABLE IF NOT EXISTS attachments (
  id         INTEGER PRIMARY KEY,
  corpus_id  TEXT    NOT NULL REFERENCES corpora(id) ON DELETE CASCADE,
  kind       TEXT    NOT NULL,
  mime       TEXT    NOT NULL,
  filename   TEXT,
  bytes      BLOB    NOT NULL,
  preview    BLOB    NOT NULL,
  width      INTEGER,
  height     INTEGER,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS attachments_corpus ON attachments(corpus_id);
CREATE INDEX IF NOT EXISTS idx_corpora_status  ON corpora(status);
CREATE INDEX IF NOT EXISTS idx_corpora_created ON corpora(created_at DESC);

-- ── Artifacts ────────────────────────────────────────────────────────────────
-- One atomic piece of knowledge, rewritten to stand alone. Superseding hides
-- an artifact and names its replacement; a captured artifact's text is never
-- rewritten in place. The dedupe pass may write a *new* artifact out of several
-- others — see `provenance` and `artifact_sources` — but it never edits one,
-- and the artifacts it was written from stay stored and one write from active.
CREATE TABLE IF NOT EXISTS artifacts (
  id               TEXT PRIMARY KEY,
  -- NULL for a merged artifact, which belongs to no single corpus. Claiming a
  -- corpus it did not come from would put the wrong lines beside it in the
  -- detail pane, which is the one dishonesty merging must not commit.
  corpus_id        TEXT REFERENCES corpora(id) ON DELETE CASCADE,
  -- 'passage' | 'captured' | 'merged' | 'synthesized'. The discriminator every
  -- consumer branches on, rather
  -- than `corpus_id IS NULL`: a null is an absence, and the failure modes
  -- merging can produce want to hang off an assertion.
  provenance       TEXT NOT NULL DEFAULT 'captured',
  -- How many captured roots a merge was written from — the number of
  -- `artifact_sources` rows it started with, not the number of arguments it was
  -- called with, which for a merge of a merge is fewer. Compared against the
  -- surviving rows to notice a source that has since been deleted; without it
  -- "lost a source" cannot be told from "only ever had two".
  source_count     INTEGER NOT NULL DEFAULT 0,
  -- Set in the same UPDATE that changes status/superseded_by, cleared once the
  -- payload write is acknowledged. The lifecycle repair reads this instead of
  -- scanning, so its cost is the open writes rather than the set of hidden
  -- artifacts — which merging makes grow without bound.
  lifecycle_dirty  INTEGER NOT NULL DEFAULT 0,
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
  -- Bumped in the same UPDATE that bumps `embed_rev`. Unrelated to
  -- recommendation, and the one question the base could not previously answer
  -- about itself: when did this artifact last change. `created_at` answers when
  -- it arrived, and `last_verified_at` answers when someone vouched for it;
  -- neither says whether the text on screen is the text that was captured.
  updated_at       INTEGER NOT NULL DEFAULT 0,
  -- Which window of the corpus produced it. Artifacts are replaced per
  -- window, so a retry of one window cannot disturb the others.
  segment_idx      INTEGER,
  -- What verification could not vouch for, and why.
  flags            TEXT,
  flag_detail      TEXT,
  superseded_by    TEXT,
  caveats          TEXT NOT NULL DEFAULT '[]',
  status           TEXT NOT NULL DEFAULT 'active',
  last_verified_at INTEGER,
  activation       REAL    NOT NULL DEFAULT 1.0,
  -- Current accessibility above is raised by being captured, retrieved,
  -- opened and confirmed; read through the same lazy decay as a link's
  -- weight. In SQLite rather than the vector payload because the query path
  -- already needs one SQLite read for links, and the same read returns this
  -- — one crossing.
  activated_at     INTEGER NOT NULL DEFAULT 0,
  -- For a synthesized artifact: the questions it was written for, JSON list.
  cues             TEXT    NOT NULL DEFAULT '[]'
);
CREATE INDEX IF NOT EXISTS idx_artifacts_corpus     ON artifacts(corpus_id, ordinal);
CREATE INDEX IF NOT EXISTS idx_artifacts_embed      ON artifacts(embed_state);
CREATE INDEX IF NOT EXISTS idx_artifacts_window     ON artifacts(corpus_id, segment_idx);
CREATE INDEX IF NOT EXISTS idx_artifacts_superseded ON artifacts(superseded_by);
CREATE INDEX IF NOT EXISTS idx_artifacts_status     ON artifacts(status);
-- Partial: the repair's work list is the open writes, which is almost always
-- empty. A full index on a column that is 0 for every row but a handful would
-- be paid for on every lifecycle write and read nothing back.
CREATE INDEX IF NOT EXISTS idx_artifacts_dirty      ON artifacts(lifecycle_dirty) WHERE lifecycle_dirty = 1;
CREATE INDEX IF NOT EXISTS idx_artifacts_provenance ON artifacts(provenance);

-- ── Lineage ──────────────────────────────────────────────────────────────────
-- What a merged artifact is made of, as resolved captured roots rather than as
-- parent edges. `root_id` always names source text — `captured` or `note`, the
-- test in `roots_of` being `!is_model_written()` rather than one literal — so
-- a re-merge reads the leaves in one query and is never written from text a
-- model produced — which is what keeps information loss one generation deep
-- however many times a group is merged. That sentence is about *merging*: a
-- `synthesized` artifact may be written from another synthesized one, and then
-- `root_id` still names source text while `via_id` names the intermediate.
--
-- The closure duplicates what edges would imply. That is the trade: the fan-in
-- cap bounds how much, and it buys a hot-path read with no recursive CTE.
CREATE TABLE IF NOT EXISTS artifact_sources (
  child_id   TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  root_id    TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  -- The direct parent through which root_id entered this child; equal to
  -- root_id for a first-generation merge. Rendering only. SET NULL rather than
  -- CASCADE because a deleted intermediate does not invalidate the root.
  via_id     TEXT REFERENCES artifacts(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  -- Set when an operator explicitly restored this root out of the merge. The
  -- unfinished-merge repair must never hide such a root again; a *new* merge
  -- decision writes fresh rows with 0, which is new evidence rather than a
  -- repair of old state.
  restored   INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (child_id, root_id)
);
CREATE INDEX IF NOT EXISTS idx_sources_root ON artifact_sources(root_id);

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
  state      TEXT    NOT NULL DEFAULT 'pending',  -- pending | done | failed | verbatim
  -- Set when this window is being read again to pick up lines the first read
  -- missed, and cleared once the window reaches `done`. It is what tells
  -- `window::write_segment_artifacts` to append rather than replace: see there
  -- for why the two reasons to re-run a window want opposite answers.
  keep_artifacts INTEGER NOT NULL DEFAULT 0,
  -- Set when an operator undid this window's promotion. The passages keep the
  -- activation that earned the promotion in the first place, so `verbatim`
  -- alone would let the very next open promote it again and undo the undo.
  -- Cleared when the window is re-split: that is a different window.
  no_promote INTEGER NOT NULL DEFAULT 0,
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
  -- Is someone waiting on this? 0 = foreground (the capture pipeline the
  -- operator is watching move raw → ready), 1 = background (work nobody is
  -- standing in front of). One distinction and not a scale: a priority the
  -- operator can set wrong presents as "the capture is hanging", with nothing
  -- anywhere saying why. Default 0 because a row written before this column
  -- existed is foreground, which is the safe direction to be wrong in; the
  -- backfill in `migrate` puts the sweeps where they belong.
  class       INTEGER NOT NULL DEFAULT 0,
  UNIQUE(stage, target_id)
);
-- Ready work in the order `claim_job` takes it: what someone is waiting on
-- first, then least-tried, then oldest.
--
-- The column order is the query's, not the filter's. `run_after` last looks
-- wrong until you try it the other way round: an inequality ends an index's
-- usable ordering, so `(state, run_after, attempts, id)` finds the ready rows
-- and then sorts them in a temp B-tree on every poll. This walks `state`,
-- `class`, `attempts`, `id` in claim order, tests `run_after` on each entry,
-- and stops at the first row that is ready — covering, and no sort. `class`
-- leads for the same reason `run_after` trails: it is an equality the walk can
-- carry, so priority costs the hot path nothing. Ageing is a written column
-- rather than a computed age precisely so that stays true — see
-- `Store::age_background`.
--
-- Superseded index dropped by name: `CREATE INDEX IF NOT EXISTS` leaves an
-- existing index's columns alone, so a base that already had `idx_jobs_claim2`
-- would keep claiming through the old order — silently, and on exactly the
-- installs the new one exists to serve.
DROP INDEX IF EXISTS idx_jobs_claim2;
CREATE INDEX IF NOT EXISTS idx_jobs_claim3  ON jobs(state, class, attempts, seq, id, run_after);
CREATE INDEX IF NOT EXISTS idx_jobs_created ON jobs(created_at);

-- One row per completed run of a periodic unit: what the memory did while
-- nobody was looking.
--
-- There is no "night" to group these by. Units that reschedule themselves on
-- their own periods do not line up into one cycle, and inventing a cycle
-- identity to group them by would be inventing it. What Ops shows instead is
-- the last day, and under it this history -- which is the thing a single
-- overwritten summary could never give: whether a sweep started going wrong
-- yesterday or has been going wrong for a week.
CREATE TABLE IF NOT EXISTS sweep_runs (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  stage      TEXT NOT NULL,
  started_at INTEGER NOT NULL,
  ended_at   INTEGER NOT NULL,
  -- 'ok' | 'failed'. A sweep that failed is exactly the run an operator needs
  -- to see, so it is recorded like any other rather than only logged.
  outcome    TEXT NOT NULL,
  -- What it did, JSON: the counts each sweep already returns.
  detail     TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_sweep_runs_at ON sweep_runs(started_at DESC);

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
  -- Of those attempts, the ones the endpoint answered and the answer could not
  -- be read. Counted apart from `judge_attempts` because only this half says
  -- anything about the pair: a call an outage ate says something about the
  -- endpoint, and shelving a pair for that would empty the review queue every
  -- time the model is down.
  judge_unreadable INTEGER NOT NULL DEFAULT 0,
  obsolete_id    TEXT REFERENCES artifacts(id),
  -- Which merged artifact answered this pair, when the settlement was an
  -- applied merge. The stranded-merge reap reopens pairs by it.
  merged_into    TEXT,
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
  skips       INTEGER NOT NULL DEFAULT 0,
  -- Set when the operator says a `gap` search has since been covered.
  dismissed_at INTEGER,
  -- A synthesized artifact led the list above `weak_below`: the base
  -- answered, and the pursuit this lands in closes satisfied.
  answered    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_events_pending ON search_events(judged_at, skips, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_events_verdict ON search_events(verdict);
-- The association sweep's own read: `created_at > watermark AND < cutoff`,
-- ordered by the same column. `idx_events_pending` cannot serve it — it leads
-- with `judged_at` — so without this the sweep full-scans and sorts the whole
-- log every `associate.interval_mins`, and with `feedback.retain_days = 0` that
-- log is never trimmed.
CREATE INDEX IF NOT EXISTS idx_events_created ON search_events(created_at);

CREATE TABLE IF NOT EXISTS search_candidates (
  event_id    TEXT NOT NULL REFERENCES search_events(id) ON DELETE CASCADE,
  rank        INTEGER NOT NULL,
  artifact_id TEXT NOT NULL,
  score       REAL NOT NULL,
  similarity  REAL,
  shown       INTEGER NOT NULL,
  PRIMARY KEY (event_id, rank)
);

-- ── Tuning sweeps ────────────────────────────────────────────────────────────
-- One row per background sweep over the judged pairs: what the running
-- configuration scored, the best the grid found, and whether the gate let that
-- become a recommendation. A number recorded without the configuration that
-- produced it cannot be compared against anything, so the settings are stored
-- beside the figures rather than left to a commit message to remember.
--
-- `diff` holds query prefixes and ranks. No artifact text is written here, for
-- the same reason the harness never prints any.
CREATE TABLE IF NOT EXISTS eval_runs (
  id            TEXT PRIMARY KEY,
  created_at    INTEGER NOT NULL,
  -- Verdicts given when this ran: what the next sweep measures its distance
  -- from, so a re-sweep is paced by new judgements rather than by the clock.
  judged_count  INTEGER NOT NULL,
  pairs_used    INTEGER NOT NULL,
  -- Pairs whose artifact is gone. Housekeeping, not a ranking result, and
  -- counted rather than scored as a miss.
  pairs_skipped INTEGER NOT NULL,
  base_params   TEXT NOT NULL,
  base_recall   REAL NOT NULL,
  base_mrr      REAL NOT NULL,
  -- Equal to the baseline when nothing passed the gate, which is what a quiet
  -- sweep is: recorded, so the silence can be explained.
  best_params   TEXT NOT NULL,
  best_recall   REAL NOT NULL,
  best_mrr      REAL NOT NULL,
  diff          TEXT NOT NULL,
  recommended   INTEGER NOT NULL,
  applied_at    INTEGER
);
CREATE INDEX IF NOT EXISTS idx_eval_runs_open
  ON eval_runs(recommended, applied_at, created_at DESC);

-- ── Ask feedback ─────────────────────────────────────────────────────────────
-- A question asked on the page, the answer it got and the excerpts the model
-- was shown — so a verdict given later can be scored against exactly what
-- happened. Only the UI door records; see `Core::ask`.
CREATE TABLE IF NOT EXISTS ask_events (
  id           TEXT PRIMARY KEY,
  question     TEXT NOT NULL,
  scope        TEXT,
  filters      TEXT NOT NULL DEFAULT '{}',
  -- Stored so a "nothing here" can be clustered with other gaps later without
  -- paying for the embedding again.
  query_vec    BLOB NOT NULL,
  vec_dim      INTEGER NOT NULL,
  embed_model  TEXT NOT NULL,
  answer       TEXT NOT NULL,
  abstained    INTEGER NOT NULL,
  dropped      INTEGER NOT NULL,
  truncated    INTEGER NOT NULL,
  created_at   INTEGER NOT NULL,
  judged_at    INTEGER,
  verdict      TEXT,
  -- Set when the operator says a "nothing here" gap has since been covered.
  dismissed_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_asks_verdict ON ask_events(verdict, dismissed_at);
CREATE INDEX IF NOT EXISTS idx_asks_created ON ask_events(created_at);

CREATE TABLE IF NOT EXISTS ask_citations (
  event_id    TEXT NOT NULL REFERENCES ask_events(id) ON DELETE CASCADE,
  -- The [n] the model was shown, 1-based, in the order it was shown.
  n           INTEGER NOT NULL,
  artifact_id TEXT NOT NULL,
  score       REAL NOT NULL,
  -- The operator said this excerpt carried the answer.
  carried     INTEGER NOT NULL DEFAULT 0,
  -- The answer actually referenced this [n]. Being shown to the model is not
  -- engagement: the pursuit sweep scores what the answer drew on, and an
  -- abstention draws on nothing.
  used        INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (event_id, n)
);

-- ── Knowledge gaps ───────────────────────────────────────────────────────────
-- Unanswered questions and gap searches, grouped by their stored vectors and
-- named once. Membership is identity: a group whose members change is a new
-- row with a new name, so the same members are never named twice.
CREATE TABLE IF NOT EXISTS gap_clusters (
  key         TEXT PRIMARY KEY,
  label       TEXT NOT NULL,
  labelled_by TEXT NOT NULL,
  members     TEXT NOT NULL,
  created_at  INTEGER NOT NULL
);

-- A gap the base has since answered, and the capture that answered it.
--
-- Its own table rather than a column on the source row, which is deliberately
-- left untouched: nothing an automatic score decides should overwrite what a
-- person judged, and an operator who disagrees reopens the gap by deleting the
-- row here rather than by re-judging anything. The cascades are that
-- reversibility for free — delete the capture that closed a gap and the gap
-- comes back.
CREATE TABLE IF NOT EXISTS gap_coverage (
  -- The `GapKind` and the id of the row it came from: an ask event, a search
  -- event, a pursuit. Not a foreign key, because it names one of three tables
  -- — so nothing cascades from the row it points at, and retention deletes
  -- those rows routinely. `Store::trim_gap_coverage`, on the repair pass, is
  -- the collection this cannot have declaratively.
  kind        TEXT NOT NULL,
  gap_id      TEXT NOT NULL,
  corpus_id   TEXT NOT NULL REFERENCES corpora(id) ON DELETE CASCADE,
  artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  -- Similarity of the best new hit. Kept so the page can say how strong a
  -- claim this was; a hit at exactly `weak_below` is a weak one.
  score       REAL NOT NULL,
  covered_at  INTEGER NOT NULL,
  PRIMARY KEY (kind, gap_id)
);
CREATE INDEX IF NOT EXISTS idx_gap_coverage_corpus ON gap_coverage(corpus_id);

-- ── Association ──────────────────────────────────────────────────────────────
-- Two artifacts that keep being retrieved by the same searches. The other half
-- of relatedness: `artifact_pairs` is about two texts saying the same thing,
-- this is about two texts being needed together. A pair can be both — filed by
-- `Relate` at 0.89 and judged distinct, and co-retrieved and related — and one
-- row cannot hold two verdicts, so they are separate tables.
CREATE TABLE IF NOT EXISTS artifact_links (
  a_id        TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  b_id        TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  -- Strength as of `bumped_at`. Read through decay; never decayed in place, so
  -- learning is one UPDATE and forgetting costs no writes at all.
  weight      REAL NOT NULL,
  bumped_at   INTEGER NOT NULL,
  -- Distinct normalised query texts that bound this pair. What separates a
  -- link from one search typed twice.
  queries     INTEGER NOT NULL DEFAULT 1,
  -- Up to three binding queries with counts, JSON: [{"q":..,"n":..}].
  cues        TEXT NOT NULL DEFAULT '[]',
  -- 'learning' | 'related' | 'unrelated' | 'dismissed'
  state       TEXT NOT NULL DEFAULT 'learning',
  -- The judge's one line, for `related`.
  reason      TEXT,
  -- Revisions the judge read. A re-embed of either side reopens the verdict:
  -- the text changed under it.
  judged_rev_a INTEGER,
  judged_rev_b INTEGER,
  judge_attempts INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY (a_id, b_id),
  CHECK (a_id < b_id)
);
CREATE INDEX IF NOT EXISTS idx_links_b ON artifact_links(b_id);
CREATE INDEX IF NOT EXISTS idx_links_state ON artifact_links(state, weight DESC);

-- ── Pursuits ─────────────────────────────────────────────────────────────────
-- What happened after a result list rendered. Joined to a pursuit through
-- time and scope at analysis, never by a stored pursuit id: the clustering
-- decides, and re-clustering never has to rewrite these.
CREATE TABLE IF NOT EXISTS interaction_events (
  id          INTEGER PRIMARY KEY,
  artifact_id TEXT REFERENCES artifacts(id) ON DELETE CASCADE,
  -- 'opened' | 'pivoted' | 'dwell'
  kind        TEXT NOT NULL,
  -- The artifact this was reached from, for 'pivoted'.
  via         TEXT,
  -- Seconds, for 'dwell'. Read on Ops; the sweep parses it.
  detail      TEXT,
  scope       TEXT,
  at          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_interactions_at ON interaction_events(at);

-- ── The situation a page view happened in ────────────────────────────────────
-- Joined to `search_events` and `interaction_events` through `scope` and `at`,
-- never through a stored id — the same rule `interaction_events` states just
-- above for pursuits. The clustering decides what belongs together, and
-- re-clustering never has to rewrite these.
CREATE TABLE IF NOT EXISTS context_events (
  id          INTEGER PRIMARY KEY,
  scope       TEXT,
  at          INTEGER NOT NULL,
  -- The whole bundle as received, including fields the encoder ignores. That
  -- is what makes a new block cheap: a reindex plus a sweep, rather than the
  -- loss of every situation recorded before it existed.
  bundle      TEXT NOT NULL,
  -- Hash over the stable fields only: platform, UA family, screen dimensions,
  -- hardwareConcurrency, deviceMemory, language. Not canvas, WebGL or fonts —
  -- those are what identify a device across a population, and here the
  -- population is one authenticated person, so they are constant and say
  -- nothing about *which situation* this is. They are also randomised per
  -- session and origin by a hardened browser, so every day would look like a
  -- new device.
  device_key  TEXT,
  -- Denormalised for whoever opens this table with `sqlite3`: "what does my
  -- Friday afternoon look like" should not require decoding a JSON bundle per
  -- row. The sweep does not read them — it re-derives all three through the
  -- encoder, which is the only reader that must agree with itself.
  --
  -- REAL, not INTEGER: the encoder keeps the fractional hour on purpose, so
  -- that 14:55 costs almost nothing against a 15:00 pattern. Truncating here
  -- would make 14:05 and 14:55 the same row and quietly disagree with the
  -- vector beside it.
  local_hour  REAL,
  weekday     INTEGER,
  tz          TEXT
);
CREATE INDEX IF NOT EXISTS idx_context_scope_at ON context_events(scope, at);

-- The situations one artifact is opened in, agglomerated. The centroids
-- themselves live in the vector store as the `ctx` multivector; this is the
-- bookkeeping, for two reasons: Qdrant holds numbers and cannot produce a
-- reason, and this table survives a `--reindex` while the vectors are rewritten.
CREATE TABLE IF NOT EXISTS context_clusters (
  id              INTEGER PRIMARY KEY,
  scope           TEXT,
  artifact_id     TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  -- Position of this centroid within the point's `ctx` multivector. Unique per
  -- artifact and NOT per (scope, artifact): the multivector is one array on one
  -- point, shared by every scope that has opened this artifact, so a slot
  -- numbered per scope would have two owners writing index 0.
  slot            INTEGER NOT NULL,
  centroid        BLOB NOT NULL,
  weight          REAL NOT NULL,
  -- How many events this cluster was built from, undecayed. `weight` answers
  -- "how much does this still count", which is the right question for ranking
  -- and the wrong one for the line under the offer: nobody can read 1.9 and
  -- know it means twice. This is what the wording says out loud.
  events          INTEGER NOT NULL DEFAULT 0,
  last_at         INTEGER NOT NULL,
  -- What layout `centroid` was written under. A reader that does not recognise
  -- it skips the cluster rather than explaining a hit with the wrong blocks.
  encoder_version INTEGER NOT NULL,
  -- The member nearest the centroid, as `{"at": <unix>, "bundle": {…}}` — what
  -- the display quotes. The stamp is carried with it because a bundle does not
  -- contain one and the line says "like 08.08., 15:04".
  representative  TEXT NOT NULL,
  UNIQUE (artifact_id, slot)
);
CREATE INDEX IF NOT EXISTS idx_context_clusters_artifact ON context_clusters(artifact_id);

-- A coherent thing that was wanted: its queries, and what came of it.
CREATE TABLE IF NOT EXISTS pursuits (
  id           TEXT PRIMARY KEY,
  opened_at    INTEGER NOT NULL,
  closed_at    INTEGER,
  -- open | satisfied | unsatisfied | generated | dismissed
  state        TEXT NOT NULL DEFAULT 'open',
  -- Why it closed, in one line. Read on Ops; never parsed.
  reason       TEXT,
  -- The clustered queries, JSON. Becomes the artifact's `cues` on generation.
  queries      TEXT NOT NULL DEFAULT '[]',
  -- The engaged artifact ids, JSON, in engagement order. What generation reads.
  sources      TEXT NOT NULL DEFAULT '[]',
  -- The generated artifact, once there is one.
  artifact_id  TEXT REFERENCES artifacts(id) ON DELETE SET NULL,
  -- The leading clustered query's vector, carried forward when the pursuit is
  -- written. A pursuit that closes unsatisfied is a gap, and a gap is a
  -- question plus the vector it was found by — `queries` holds the words and
  -- the words alone, and re-embedding them to group the gap would be a call
  -- spent on a vector that was already computed. Null on a pursuit written
  -- before this column existed, which is why `vec_dim > 0` is the test: an
  -- uncomparable vector is exactly what `open_gaps` already leaves out.
  query_vec    BLOB,
  vec_dim      INTEGER NOT NULL DEFAULT 0,
  embed_model  TEXT
);
CREATE INDEX IF NOT EXISTS idx_pursuits_state ON pursuits(state, opened_at);

-- Cursors that have no row to live on. Three keys so far:
-- `associate.events_after`, `associate.judged_after`, `pursuit.events_after`.
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
