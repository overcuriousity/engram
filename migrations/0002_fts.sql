-- External-content FTS5 index over chunks. `content='chunks'` keeps a single
-- copy of the text; the triggers below are what keep the index truthful.
CREATE VIRTUAL TABLE chunks_fts USING fts5(
  text,
  title,
  tags,
  content='chunks',
  content_rowid='rowid'
);

CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
  INSERT INTO chunks_fts(rowid, text, title, tags)
  VALUES (new.rowid, new.text, new.title, new.tags);
END;

-- External-content FTS5 requires these 'delete' command rows. A plain
-- DELETE FROM chunks_fts corrupts the index.
CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text, title, tags)
  VALUES ('delete', old.rowid, old.text, old.title, old.tags);
END;

CREATE TRIGGER chunks_au AFTER UPDATE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text, title, tags)
  VALUES ('delete', old.rowid, old.text, old.title, old.tags);
  INSERT INTO chunks_fts(rowid, text, title, tags)
  VALUES (new.rowid, new.text, new.title, new.tags);
END;
