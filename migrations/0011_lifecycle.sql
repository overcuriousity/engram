-- Artifact lifecycle: freshness and deprecation.
--
-- `status` makes "hidden because a duplicate won" (superseded, already
-- tracked via `superseded_by`) and "flagged stale with no replacement"
-- (deprecated) distinguishable, instead of overloading one boolean for both.
-- `last_verified_at` is the recency input search ranking decays against —
-- separate from `created_at`, because an artifact confirmed accurate last
-- week should outrank one merely written last week and never looked at
-- since.
ALTER TABLE artifacts ADD COLUMN status TEXT NOT NULL DEFAULT 'active';
ALTER TABLE artifacts ADD COLUMN last_verified_at INTEGER;

UPDATE artifacts SET status = 'superseded' WHERE superseded_by IS NOT NULL;
UPDATE artifacts SET last_verified_at = created_at WHERE last_verified_at IS NULL;

CREATE INDEX idx_artifacts_status ON artifacts(status);

-- Which side of a judged pair the model believes is obsolete, so the review
-- UI can offer "apply supersede" without asking the model again. Set only
-- when the judge names a direction with confidence; NULL for an ordinary
-- contradiction with no clear winner.
ALTER TABLE artifact_pairs ADD COLUMN obsolete_id TEXT REFERENCES artifacts(id);
