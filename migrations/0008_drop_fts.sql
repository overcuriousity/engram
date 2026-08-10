-- Hybrid search settled in Qdrant. Dense and sparse run as prefetch branches
-- fused by RRF in one round trip, so the lexical half is scored against the
-- same candidates as the vector half and cannot drift from them. The SQLite
-- FTS5 index was the other candidate for that job and lost: nothing in the
-- read path ever queried it, while every insert, delete and text update paid
-- three triggers to keep it current.
--
-- The index is external-content (content='artifacts'), so dropping it loses no
-- text — every row it held is still in `artifacts`. Re-deriving it later is one
-- CREATE VIRTUAL TABLE plus a 'rebuild', which is why it is cheaper to delete
-- now than to carry.
DROP TRIGGER IF EXISTS artifacts_ai;
DROP TRIGGER IF EXISTS artifacts_ad;
DROP TRIGGER IF EXISTS artifacts_au;
DROP TABLE IF EXISTS artifacts_fts;
