-- Segmentation used to be one job over every window of a source: a retry
-- re-ran windows that had already succeeded, and one window exhausting its
-- attempts sent the whole source through a structural split. These rows are
-- the job's memory, so a retry resumes and a hopeless window degrades alone.
CREATE TABLE segment_windows (
  source_id  TEXT    NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
  idx        INTEGER NOT NULL,
  start_line INTEGER NOT NULL,
  end_line   INTEGER NOT NULL,
  -- The window's text as the splitter produced it.
  --
  -- Stored rather than re-derived from start_line/end_line, because line
  -- numbers cannot address a unit smaller than a line: a corpus pasted with
  -- no newlines is cut into windows the line numbers then all report as
  -- line 1, so the first re-derived to the whole document and the rest to
  -- nothing at all. The line range stays for rendering an artifact's source.
  text       TEXT    NOT NULL DEFAULT '',
  state      TEXT    NOT NULL DEFAULT 'pending',  -- pending | done | fallback
  attempts   INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  PRIMARY KEY (source_id, idx)
);
CREATE INDEX idx_windows_state ON segment_windows(source_id, state);

-- Which window produced a chunk. Chunks are replaced per window now, so this
-- is the key that scopes the delete.
ALTER TABLE chunks ADD COLUMN window_idx INTEGER;
CREATE INDEX idx_chunks_window ON chunks(source_id, window_idx);

-- Verification results. NULL flags means the chunk passed every check; the
-- detail is one human-readable line naming the first offender.
ALTER TABLE chunks ADD COLUMN flags       TEXT;
ALTER TABLE chunks ADD COLUMN flag_detail TEXT;

-- Fraction of the source's non-blank lines claimed by some chunk span.
ALTER TABLE sources ADD COLUMN coverage REAL;
