-- Near-duplicate detection at capture.
--
-- `content_hash` is exact, so the same chapter re-pasted with one changed byte
-- becomes a second corpus, and the two then compete for the same queries for
-- as long as the base exists. The signature is a bottom-k MinHash over word
-- shingles (`src/store/shingle.rs`), compared against every other corpus at
-- capture time — a scan, because a single-operator base holds hundreds of
-- corpora, not millions.
ALTER TABLE corpora ADD COLUMN shingles TEXT;
-- Set when capture found a near-identical corpus. The capture is stored
-- regardless and parked in `needs_review`: nothing is ever discarded on a
-- similarity score, and synthesis is not paid for until a human decides.
ALTER TABLE corpora ADD COLUMN near_dupe_of TEXT;
ALTER TABLE corpora ADD COLUMN near_dupe_score REAL;

-- Consolidation, artifact side.
--
-- The artifact this one lost to. Set by the sweep when two artifacts are near
-- identical; the loser stays stored, readable and reversible, and is hidden
-- from search by a payload flag rather than deleted. A merged rewrite would
-- put synthetic text where a stored artifact used to be, which is the one
-- failure mode this design exists to avoid.
ALTER TABLE artifacts ADD COLUMN superseded_by TEXT;
CREATE INDEX idx_artifacts_superseded ON artifacts(superseded_by);

-- Conditions under which the artifact does not apply, as stated by the source.
-- Emitted by the same synthesis call that produces the artifact, so it costs
-- output tokens rather than another call.
ALTER TABLE artifacts ADD COLUMN caveats TEXT NOT NULL DEFAULT '[]';

-- The review queue.
--
-- Pairs similar enough to be worth a person's attention but not similar enough
-- to supersede automatically. `state` is 'pending' until something resolves
-- it: 'no_conflict' when the fact-token prefilter or the judge clears it,
-- 'contradiction' when the judge finds one, 'dismissed' when an operator does.
-- `a_id` < `b_id` by string order, so the same pair found in either direction
-- is one row.
CREATE TABLE artifact_pairs (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  a_id       TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  b_id       TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  score      REAL NOT NULL,
  state      TEXT NOT NULL DEFAULT 'pending',
  detail     TEXT,
  created_at INTEGER NOT NULL,
  UNIQUE(a_id, b_id)
);
CREATE INDEX idx_pairs_state ON artifact_pairs(state, created_at DESC);
