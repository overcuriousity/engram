-- How many model calls a pending pair has already cost.
--
-- A judgement that fails to parse, or a call that never returns, leaves the
-- pair pending deliberately: a dead endpoint must not look like a clean bill of
-- health. But the judge picks by score, so without this column the same
-- top-scoring pairs would consume every sweep's budget and the rest of the
-- queue would never be reached at all.
ALTER TABLE artifact_pairs ADD COLUMN judge_attempts INTEGER NOT NULL DEFAULT 0;
