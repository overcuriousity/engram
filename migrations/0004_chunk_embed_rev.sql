-- A chunk's vector revision.
--
-- Embedding reads a chunk, calls a slow remote endpoint, and only then writes
-- back "this is indexed". Anything that edits the text or the title in that
-- window — a PATCH, a reprocess — leaves the job about to mark a chunk
-- embedded on the strength of a vector computed from text that no longer
-- exists. Nothing in the row said so, because the edit set the same
-- `embed_state = 'pending'` the job was about to clear.
--
-- So every vector-invalidating edit bumps this, the embed job carries the
-- revision it read, and marking a chunk embedded only lands while the two
-- still match. A losing job leaves the chunk pending, which is exactly the
-- state that gets it embedded again.
ALTER TABLE chunks ADD COLUMN embed_rev INTEGER NOT NULL DEFAULT 0;

-- The FTS index only mirrors text, title and tags, but this trigger fired on
-- every column — so each `embed_state` write, and now each revision bump,
-- deleted and reinserted three FTS rows for no change in what they hold.
-- Scoping it makes the index cost track edits rather than job traffic.
DROP TRIGGER chunks_au;
CREATE TRIGGER chunks_au AFTER UPDATE OF text, title, tags ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text, title, tags)
  VALUES ('delete', old.rowid, old.text, old.title, old.tags);
  INSERT INTO chunks_fts(rowid, text, title, tags)
  VALUES (new.rowid, new.text, new.title, new.tags);
END;
