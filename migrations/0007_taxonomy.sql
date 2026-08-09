-- The domain now says what it does. A corpus is pasted; it is split into
-- segments that fit the model's context; those are synthesised into artifacts,
-- which are what gets embedded, ranked and read. The old names described the
-- mechanism — a "chunk" of text, a "window" of tokens — rather than the thing
-- the reader is actually looking for.

-- The FTS5 index goes first and whole. Its `content='chunks'` option lives
-- inside the virtual table's own definition, which a table rename does not
-- rewrite, so renaming around it would leave an index pointing at a table that
-- no longer exists. Nothing reads this index yet, so rebuilding it costs one
-- pass over the text and removes all doubt.
DROP TRIGGER chunks_ai;
DROP TRIGGER chunks_ad;
DROP TRIGGER chunks_au;
DROP TABLE chunks_fts;

ALTER TABLE sources RENAME TO corpora;
ALTER TABLE chunks RENAME TO artifacts;
ALTER TABLE segment_windows RENAME TO segments;

ALTER TABLE artifacts RENAME COLUMN source_id TO corpus_id;
ALTER TABLE artifacts RENAME COLUMN source_span TO corpus_span;
ALTER TABLE artifacts RENAME COLUMN window_idx TO segment_idx;
ALTER TABLE segments RENAME COLUMN source_id TO corpus_id;

-- The stage that calls the model is synthesis. Splitting a corpus into
-- segments is free, local and has never had a stage of its own, so naming the
-- inference stage "segment" pointed at the wrong half of the job.
UPDATE jobs SET stage = 'synthesize' WHERE stage = 'segment';
UPDATE jobs SET target_kind = 'corpus' WHERE target_kind = 'source';
UPDATE jobs SET target_kind = 'artifact' WHERE target_kind = 'chunk';
UPDATE corpora SET status = 'synthesizing' WHERE status = 'segmenting';

CREATE VIRTUAL TABLE artifacts_fts USING fts5(
  text,
  title,
  tags,
  content='artifacts',
  content_rowid='rowid'
);
INSERT INTO artifacts_fts(artifacts_fts) VALUES ('rebuild');

CREATE TRIGGER artifacts_ai AFTER INSERT ON artifacts BEGIN
  INSERT INTO artifacts_fts(rowid, text, title, tags)
  VALUES (new.rowid, new.text, new.title, new.tags);
END;

-- External-content FTS5 requires these 'delete' command rows. A plain
-- DELETE FROM artifacts_fts corrupts the index.
CREATE TRIGGER artifacts_ad AFTER DELETE ON artifacts BEGIN
  INSERT INTO artifacts_fts(artifacts_fts, rowid, text, title, tags)
  VALUES ('delete', old.rowid, old.text, old.title, old.tags);
END;

-- Scoped to the columns the index holds, so a write that touches only an
-- artifact's embedding state does not pay for a reindex of its text.
CREATE TRIGGER artifacts_au AFTER UPDATE OF text, title, tags ON artifacts BEGIN
  INSERT INTO artifacts_fts(artifacts_fts, rowid, text, title, tags)
  VALUES ('delete', old.rowid, old.text, old.title, old.tags);
  INSERT INTO artifacts_fts(rowid, text, title, tags)
  VALUES (new.rowid, new.text, new.title, new.tags);
END;
