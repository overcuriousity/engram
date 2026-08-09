# Usable Today Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make engram trustworthy to fill and practical to read: segmentation that fails per window instead of per source, verification that literals and spans survived the rewrite, and a search workspace where the ranked list stays beside the chunk and its source lines.

**Architecture:** Segmentation gains a `segment_windows` table that acts as the job's memory, so a retry resumes at the first unfinished window and a hopeless window degrades to a structural split on its own lines. After each window the proposed chunks are checked against the raw text they came from — literals must be present, spans must be plausible — and failures are re-segmented once, then stored with a flag. The search page becomes a rail of ranked hits beside a detail pane that renders the chunk next to the source lines it claims, served by a `SourceView` trait with one implementation today.

**Tech Stack:** Rust 1.94+, axum, askama + htmx, sqlx/SQLite, Qdrant over REST, `pulldown_cmark` + `ammonia` for chunk rendering.

## Global Constraints

- Rust 1.94 is the floor (sqlx 0.9). Do not use newer language features than that.
- No new runtime dependencies. No node toolchain, no CDN, no external font or script.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` must pass at every commit.
- `cargo test` must pass without any container running. Qdrant-backed tests stay behind `--ignored`.
- Chunk text is untrusted model output. It reaches HTML only through `crate::web::markdown::render`, which sanitizes with `ammonia`. Never add a second `|safe` path.
- SQLite migrations are append-only files in `migrations/`, applied by `sqlx::migrate!`. Never edit an existing migration.
- Existing behaviour that must not regress: one batched embed job per source; "replace, never append" when writing chunks; `embed_rev` guarding `mark_embedded`.
- Spec: `docs/superpowers/specs/2026-08-09-usable-today-design.md`.

---

## File Structure

**Created**

| Path | Responsibility |
|---|---|
| `migrations/0005_segment_windows.sql` | Windows table, chunk flags, source coverage |
| `src/store/windows.rs` | CRUD for `segment_windows`, window progress |
| `src/infer/verify.rs` | Literal extraction, literal/span checks, coverage maths — pure functions, no IO |
| `src/web/source_view.rs` | `SourceView` trait and its `TextLines` implementation |
| `src/web/templates/chunk_detail.html` | Standalone chunk page (non-htmx requests) |
| `src/web/templates/_chunk_detail.html` | Same body as a fragment (htmx swaps) |
| `assets/app.js` | Query-term highlighting, snippet expand, copy buttons |

**Modified**

| Path | Change |
|---|---|
| `src/store/mod.rs` | Declare `pub mod windows;` |
| `src/store/chunks.rs` | `window_idx`, `flags`, `flag_detail`; per-window delete; renumbering; flagged-chunk query |
| `src/store/sources.rs` | `coverage` column and setter |
| `src/jobs/segment.rs` | Per-window loop with resume, verification, per-window fallback, finish step |
| `src/jobs/mod.rs` | Exhausted segment job falls back per window |
| `src/infer/split.rs` | `window_text` helper |
| `src/infer/fake.rs` | Test chunkers: fail on one window, paraphrase a literal, lie about spans |
| `src/core/mod.rs` | Query embedding cache on `Core` |
| `src/core/search.rs` | `mark` flag, cached embedding, `SearchTiming` |
| `src/web/mod.rs` | Explicit request body limit |
| `src/web/ui.rs` | Chunk detail route, workspace handlers, window/flag actions |
| `src/web/templates/search.html` | Rail + pane workspace |
| `src/web/templates/_results.html` | Rail entries, clamped |
| `src/web/templates/capture.html` | Chapter-at-a-time guidance and size warning |
| `src/web/templates/browse.html` | Window progress and coverage columns |
| `src/web/templates/ops.html` | Flagged chunks, low-coverage sources, per-window retry |
| `src/web/templates/layout.html` | Load `/assets/app.js` |
| `assets/app.css` | Workspace, rail, split pane, raw-line table, clamp, copy button, mark |
| `README.md` | Segmentation, verification and workspace sections |

---

### Task 1: Window and flag storage

**Files:**
- Create: `migrations/0005_segment_windows.sql`
- Create: `src/store/windows.rs`
- Modify: `src/store/mod.rs` (module list)
- Modify: `src/store/chunks.rs` (`NewChunk.window_idx`, `Chunk.flags`, `Chunk.flag_detail`, new queries)
- Modify: `src/store/sources.rs` (`Source.coverage`, setter)
- Test: inline `#[cfg(test)] mod tests` in `src/store/windows.rs` and `src/store/chunks.rs`

**Interfaces:**
- Consumes: `Store`, `Chunk`, `NewChunk`, `SourceSpan` as they exist today.
- Produces:
  - `store::windows::WindowState { Pending, Done, Fallback }` with `as_str()` / `parse()`
  - `store::windows::SegmentWindow { source_id: String, idx: i64, start_line: i64, end_line: i64, state: WindowState, attempts: i64, last_error: Option<String> }`
  - `Store::upsert_windows(&self, source_id: &str, spans: &[(i64, i64)]) -> Result<()>` — index is position in the slice; existing rows are left untouched.
  - `Store::windows_for_source(&self, source_id: &str) -> Result<Vec<SegmentWindow>>`
  - `Store::pending_windows(&self, source_id: &str) -> Result<Vec<SegmentWindow>>`
  - `Store::set_window_state(&self, source_id: &str, idx: i64, state: WindowState, last_error: Option<&str>) -> Result<()>`
  - `Store::bump_window_attempts(&self, source_id: &str, idx: i64) -> Result<i64>`
  - `Store::reset_window(&self, source_id: &str, idx: i64) -> Result<()>`
  - `Store::window_progress(&self, source_id: &str) -> Result<(i64, i64)>` — `(resolved, total)`
  - `Store::clear_windows(&self, source_id: &str) -> Result<()>`
  - `NewChunk.window_idx: Option<i64>`, `Chunk.window_idx: Option<i64>`, `Chunk.flags: Vec<String>`, `Chunk.flag_detail: Option<String>`
  - `Store::chunk_ids_for_window(&self, source_id: &str, window_idx: i64) -> Result<Vec<String>>`
  - `Store::renumber_chunks(&self, source_id: &str) -> Result<()>`
  - `Store::set_chunk_flags(&self, id: &str, flags: &[String], detail: Option<&str>) -> Result<()>`
  - `Store::clear_chunk_flags(&self, id: &str) -> Result<()>`
  - `Store::flagged_chunks(&self, limit: i64) -> Result<Vec<Chunk>>`
  - `Store::set_source_coverage(&self, source_id: &str, coverage: f64) -> Result<()>`, `Source.coverage: Option<f64>`

- [ ] **Step 1: Write the migration**

Create `migrations/0005_segment_windows.sql`:

```sql
-- Segmentation used to be one job over every window of a source: a retry
-- re-ran windows that had already succeeded, and one window exhausting its
-- attempts sent the whole source through a structural split. These rows are
-- the job's memory, so a retry resumes and a hopeless window degrades alone.
CREATE TABLE segment_windows (
  source_id  TEXT    NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
  idx        INTEGER NOT NULL,
  start_line INTEGER NOT NULL,
  end_line   INTEGER NOT NULL,
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
```

- [ ] **Step 2: Write the failing store tests**

Create `src/store/windows.rs` with only this test module at the bottom (the code above it comes in step 4):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[tokio::test]
    async fn windows_are_upserted_once_and_resume_reads_only_the_pending_ones() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();

        s.upsert_windows(&src.id, &[(1, 10), (11, 20), (21, 30)])
            .await
            .unwrap();
        assert_eq!(s.windows_for_source(&src.id).await.unwrap().len(), 3);

        s.set_window_state(&src.id, 0, WindowState::Done, None)
            .await
            .unwrap();

        // A second call must not reset the window that already finished.
        s.upsert_windows(&src.id, &[(1, 10), (11, 20), (21, 30)])
            .await
            .unwrap();

        let pending = s.pending_windows(&src.id).await.unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].idx, 1);
        assert_eq!(pending[0].start_line, 11);
    }

    #[tokio::test]
    async fn progress_counts_done_and_fallback_as_resolved() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        s.upsert_windows(&src.id, &[(1, 5), (6, 10), (11, 15)])
            .await
            .unwrap();

        s.set_window_state(&src.id, 0, WindowState::Done, None)
            .await
            .unwrap();
        s.set_window_state(&src.id, 1, WindowState::Fallback, Some("endpoint down"))
            .await
            .unwrap();

        assert_eq!(s.window_progress(&src.id).await.unwrap(), (2, 3));
        let w = s.windows_for_source(&src.id).await.unwrap();
        assert_eq!(w[1].last_error.as_deref(), Some("endpoint down"));
    }

    #[tokio::test]
    async fn attempts_accumulate_and_a_reset_clears_them() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        s.upsert_windows(&src.id, &[(1, 5)]).await.unwrap();

        assert_eq!(s.bump_window_attempts(&src.id, 0).await.unwrap(), 1);
        assert_eq!(s.bump_window_attempts(&src.id, 0).await.unwrap(), 2);
        s.set_window_state(&src.id, 0, WindowState::Fallback, Some("boom"))
            .await
            .unwrap();

        s.reset_window(&src.id, 0).await.unwrap();
        let w = &s.windows_for_source(&src.id).await.unwrap()[0];
        assert_eq!(w.state, WindowState::Pending);
        assert_eq!(w.attempts, 0);
        assert_eq!(w.last_error, None);
    }

    #[tokio::test]
    async fn deleting_a_source_cascades_to_its_windows() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        s.upsert_windows(&src.id, &[(1, 5)]).await.unwrap();
        s.delete_source(&src.id).await.unwrap();
        assert!(s.windows_for_source(&src.id).await.unwrap().is_empty());
    }
}
```

Add to `src/store/chunks.rs` test module:

```rust
    #[tokio::test]
    async fn chunks_are_replaced_per_window_not_per_source() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        let mut a = nc(0, "window zero");
        a.window_idx = Some(0);
        let mut b = nc(0, "window one");
        b.window_idx = Some(1);
        s.insert_chunks(&src.id, &[a, b]).await.unwrap();

        let ids = s.chunk_ids_for_window(&src.id, 1).await.unwrap();
        assert_eq!(ids.len(), 1);
        for id in &ids {
            s.delete_chunk(id).await.unwrap();
        }

        let left = s.chunks_for_source(&src.id).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].text, "window zero");
    }

    #[tokio::test]
    async fn renumbering_orders_by_window_then_position() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        let mut second = nc(1, "second of window one");
        second.window_idx = Some(1);
        let mut first = nc(0, "first of window one");
        first.window_idx = Some(1);
        let mut zero = nc(0, "only of window zero");
        zero.window_idx = Some(0);
        s.insert_chunks(&src.id, &[second, first, zero])
            .await
            .unwrap();

        s.renumber_chunks(&src.id).await.unwrap();
        let got = s.chunks_for_source(&src.id).await.unwrap();
        assert_eq!(got[0].text, "only of window zero");
        assert_eq!(got[0].ordinal, 0);
        assert_eq!(got[1].text, "first of window one");
        assert_eq!(got[1].ordinal, 1);
        assert_eq!(got[2].ordinal, 2);
    }

    #[tokio::test]
    async fn flags_round_trip_and_list_only_flagged_chunks() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        let made = s
            .insert_chunks(&src.id, &[nc(0, "clean"), nc(1, "suspect")])
            .await
            .unwrap();

        s.set_chunk_flags(
            &made[1].id,
            &["literals_unverified".to_string()],
            Some("missing literal: --dry-run"),
        )
        .await
        .unwrap();

        let flagged = s.flagged_chunks(10).await.unwrap();
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].flags, vec!["literals_unverified".to_string()]);
        assert_eq!(
            flagged[0].flag_detail.as_deref(),
            Some("missing literal: --dry-run")
        );

        s.clear_chunk_flags(&made[1].id).await.unwrap();
        assert!(s.flagged_chunks(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn coverage_is_stored_on_the_source() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        s.set_source_coverage(&src.id, 0.42).await.unwrap();
        let got = s.get_source(&src.id).await.unwrap();
        assert!((got.coverage.unwrap() - 0.42).abs() < 1e-6);
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib store:: 2>&1 | tail -30`
Expected: compile errors — `upsert_windows` not found, `NewChunk` has no field `window_idx`, `flagged_chunks` not found.

- [ ] **Step 4: Implement the window store**

Write the body of `src/store/windows.rs` above its test module:

```rust
use super::Store;
use crate::error::Result;
use sqlx::Row;

/// Where one window of a source stands. `Fallback` means the chunker never
/// succeeded here and the lines were split structurally instead — worse than
/// an LLM split, and the reason the source ends up `partial`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowState {
    Pending,
    Done,
    Fallback,
}

impl WindowState {
    pub fn as_str(&self) -> &'static str {
        match self {
            WindowState::Pending => "pending",
            WindowState::Done => "done",
            WindowState::Fallback => "fallback",
        }
    }
    pub fn parse(s: &str) -> WindowState {
        match s {
            "done" => WindowState::Done,
            "fallback" => WindowState::Fallback,
            _ => WindowState::Pending,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SegmentWindow {
    pub source_id: String,
    pub idx: i64,
    pub start_line: i64,
    pub end_line: i64,
    pub state: WindowState,
    pub attempts: i64,
    pub last_error: Option<String>,
}

fn row_to_window(r: &sqlx::sqlite::SqliteRow) -> SegmentWindow {
    SegmentWindow {
        source_id: r.get("source_id"),
        idx: r.get("idx"),
        start_line: r.get("start_line"),
        end_line: r.get("end_line"),
        state: WindowState::parse(r.get::<String, _>("state").as_str()),
        attempts: r.get("attempts"),
        last_error: r.get("last_error"),
    }
}

impl Store {
    /// Record the windowing of a source. Idempotent by design: a retried job
    /// re-derives the same spans and must not undo the windows that finished.
    pub async fn upsert_windows(&self, source_id: &str, spans: &[(i64, i64)]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for (idx, (start, end)) in spans.iter().enumerate() {
            sqlx::query(
                "INSERT INTO segment_windows (source_id, idx, start_line, end_line)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(source_id, idx) DO NOTHING",
            )
            .bind(source_id)
            .bind(idx as i64)
            .bind(start)
            .bind(end)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn windows_for_source(&self, source_id: &str) -> Result<Vec<SegmentWindow>> {
        let rows = sqlx::query("SELECT * FROM segment_windows WHERE source_id = ? ORDER BY idx")
            .bind(source_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_window).collect())
    }

    pub async fn pending_windows(&self, source_id: &str) -> Result<Vec<SegmentWindow>> {
        let rows = sqlx::query(
            "SELECT * FROM segment_windows
             WHERE source_id = ? AND state = 'pending' ORDER BY idx",
        )
        .bind(source_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_window).collect())
    }

    pub async fn set_window_state(
        &self,
        source_id: &str,
        idx: i64,
        state: WindowState,
        last_error: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE segment_windows SET state = ?, last_error = ?
             WHERE source_id = ? AND idx = ?",
        )
        .bind(state.as_str())
        .bind(last_error)
        .bind(source_id)
        .bind(idx)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn bump_window_attempts(&self, source_id: &str, idx: i64) -> Result<i64> {
        sqlx::query(
            "UPDATE segment_windows SET attempts = attempts + 1
             WHERE source_id = ? AND idx = ?",
        )
        .bind(source_id)
        .bind(idx)
        .execute(&self.pool)
        .await?;
        let row = sqlx::query("SELECT attempts FROM segment_windows WHERE source_id = ? AND idx = ?")
            .bind(source_id)
            .bind(idx)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get("attempts"))
    }

    /// Put a window back in the queue's path. The operator's "re-segment this
    /// window" button, and nothing else, calls this.
    pub async fn reset_window(&self, source_id: &str, idx: i64) -> Result<()> {
        sqlx::query(
            "UPDATE segment_windows SET state = 'pending', attempts = 0, last_error = NULL
             WHERE source_id = ? AND idx = ?",
        )
        .bind(source_id)
        .bind(idx)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `(resolved, total)`, where resolved counts both a clean window and one
    /// that gave up and split structurally.
    pub async fn window_progress(&self, source_id: &str) -> Result<(i64, i64)> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS total,
                    SUM(CASE WHEN state <> 'pending' THEN 1 ELSE 0 END) AS resolved
             FROM segment_windows WHERE source_id = ?",
        )
        .bind(source_id)
        .fetch_one(&self.pool)
        .await?;
        let total: i64 = row.get("total");
        let resolved: Option<i64> = row.get("resolved");
        Ok((resolved.unwrap_or(0), total))
    }

    /// Drop the windowing entirely. Re-segmenting a source from scratch has to
    /// re-window it, because the text or the model's budget may have changed.
    pub async fn clear_windows(&self, source_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM segment_windows WHERE source_id = ?")
            .bind(source_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
```

Add `pub mod windows;` to the module list at the top of `src/store/mod.rs`.

- [ ] **Step 5: Extend the chunk store**

In `src/store/chunks.rs`:

Add fields to the structs and to `row_to_chunk`:

```rust
pub struct Chunk {
    // … existing fields …
    /// Which segmentation window produced this chunk. `None` for chunks
    /// written before per-window segmentation existed.
    pub window_idx: Option<i64>,
    /// Verification failures. Empty means every check passed.
    pub flags: Vec<String>,
    pub flag_detail: Option<String>,
}

pub struct NewChunk {
    // … existing fields …
    pub window_idx: Option<i64>,
}
```

```rust
fn row_to_chunk(r: &sqlx::sqlite::SqliteRow) -> Chunk {
    let tags_json: String = r.get("tags");
    let span_json: Option<String> = r.get("source_span");
    let flags_json: Option<String> = r.get("flags");
    Chunk {
        // … existing fields …
        window_idx: r.get("window_idx"),
        flags: flags_json
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        flag_detail: r.get("flag_detail"),
    }
}
```

In `insert_chunks`, carry the new column:

```rust
            sqlx::query(
                "INSERT INTO chunks (id, source_id, ordinal, text, source_span, title, category, tags, embed_state, embed_model, created_at, window_idx)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)",
            )
            // … existing binds …
            .bind(c.window_idx)
```

with `window_idx: nc.window_idx` added where the `Chunk` value is built, and `window_idx: None, flags: vec![], flag_detail: None` filled in there too.

Add the new queries:

```rust
    pub async fn chunk_ids_for_window(&self, source_id: &str, window_idx: i64) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT id FROM chunks WHERE source_id = ? AND window_idx = ? ORDER BY ordinal",
        )
        .bind(source_id)
        .bind(window_idx)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get("id")).collect())
    }

    /// Give a source one continuous ordinal sequence again.
    ///
    /// Chunks are inserted per window and numbered within it, so until this
    /// runs a source has three chunks numbered 0. Ordering by window and then
    /// by the within-window number reproduces reading order.
    pub async fn renumber_chunks(&self, source_id: &str) -> Result<()> {
        let rows = sqlx::query(
            "SELECT id FROM chunks WHERE source_id = ?
             ORDER BY COALESCE(window_idx, 0), ordinal, rowid",
        )
        .bind(source_id)
        .fetch_all(&self.pool)
        .await?;
        let mut tx = self.pool.begin().await?;
        for (n, r) in rows.iter().enumerate() {
            sqlx::query("UPDATE chunks SET ordinal = ? WHERE id = ?")
                .bind(n as i64)
                .bind(r.get::<String, _>("id"))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_chunk_flags(
        &self,
        id: &str,
        flags: &[String],
        detail: Option<&str>,
    ) -> Result<()> {
        let json = if flags.is_empty() {
            None
        } else {
            Some(serde_json::to_string(flags).unwrap_or_else(|_| "[]".into()))
        };
        sqlx::query("UPDATE chunks SET flags = ?, flag_detail = ? WHERE id = ?")
            .bind(json)
            .bind(detail)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn clear_chunk_flags(&self, id: &str) -> Result<()> {
        self.set_chunk_flags(id, &[], None).await
    }

    pub async fn flagged_chunks(&self, limit: i64) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT * FROM chunks WHERE flags IS NOT NULL ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_chunk).collect())
    }
```

In `src/store/sources.rs` add `pub coverage: Option<f64>` to `Source`, `coverage: r.get("coverage")` to `row_to_source`, `coverage: None` where the struct is built in `insert_source`, and:

```rust
    pub async fn set_source_coverage(&self, source_id: &str, coverage: f64) -> Result<()> {
        sqlx::query("UPDATE sources SET coverage = ?, updated_at = ? WHERE id = ?")
            .bind(coverage)
            .bind(now())
            .bind(source_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
```

Fix every existing `NewChunk { … }` literal in the tree by adding `window_idx: None` — `src/jobs/segment.rs`, and the test helpers in `src/store/chunks.rs`, `src/core/search.rs`, `src/core/ask.rs` if present. `cargo build` names them all.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib store::`
Expected: PASS, including the four new window tests and the four new chunk tests.

- [ ] **Step 7: Commit**

```bash
git add migrations/0005_segment_windows.sql src/store
git commit -m "feat: store segmentation windows, chunk flags and source coverage"
```

---

### Task 2: Segment window by window, resuming where it stopped

**Files:**
- Modify: `src/infer/split.rs` (add `window_text`)
- Modify: `src/jobs/segment.rs` (rewrite `run`, add `write_window_chunks`, `finish`)
- Test: `src/infer/split.rs` and `src/jobs/segment.rs` test modules

**Interfaces:**
- Consumes: everything from Task 1; `split_into_windows`, `Window`, `window_tokens`, `Chunker::segment`.
- Produces:
  - `infer::split::window_text(text: &str, start_line: i64, end_line: i64) -> String`
  - `jobs::segment::run(core: &Core, source_id: &str) -> Result<()>` — unchanged signature, per-window behaviour
  - `jobs::segment::finish(core: &Core, source_id: &str) -> Result<()>` — renumber, coverage, status, enqueue embed

- [ ] **Step 1: Write the failing tests**

Add to the test module in `src/infer/split.rs`:

```rust
    #[test]
    fn window_text_returns_exactly_the_lines_a_window_claims() {
        let src = "one\ntwo\nthree\nfour\nfive";
        assert_eq!(window_text(src, 2, 4), "two\nthree\nfour");
        // Out-of-range ends clamp rather than panic: the stored window is data,
        // and data can be stale.
        assert_eq!(window_text(src, 4, 99), "four\nfive");
        assert_eq!(window_text(src, 99, 120), "");
    }
```

Add to the test module in `src/jobs/segment.rs`:

```rust
    #[tokio::test]
    async fn a_second_run_does_not_re_segment_windows_that_finished() {
        let core = test_core().await;
        let body = multi_window_body();
        let out = core.ingest(&body, "web", None).await.unwrap();
        assert!(window_count(&core, &body) > 1);

        run(&core, &out.id).await.unwrap();
        let (resolved, total) = core.store.window_progress(&out.id).await.unwrap();
        assert_eq!(resolved, total, "every window should have resolved");

        let before = core.store.chunks_for_source(&out.id).await.unwrap().len();
        // Nothing is pending, so a second run must be a no-op rather than a
        // second full pass that doubles the chunk count.
        run(&core, &out.id).await.unwrap();
        let after = core.store.chunks_for_source(&out.id).await.unwrap().len();
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn a_failing_window_leaves_earlier_windows_intact() {
        // Fails only on the window containing the marker, so window 0 succeeds
        // and window 1 raises — the shape a flaky endpoint produces.
        let mut core = test_core().await;
        let body = format!("{}\n\nSTOPHERE marker paragraph\n", multi_window_body());
        let out = core.ingest(&body, "web", None).await.unwrap();
        core.chunker = std::sync::Arc::new(crate::infer::fake::FakeChunker::failing_on("STOPHERE"));

        let err = run(&core, &out.id).await.unwrap_err();
        assert!(err.retryable(), "a chunker error must stay retryable");

        let (resolved, total) = core.store.window_progress(&out.id).await.unwrap();
        assert!(resolved > 0, "windows before the failure must be recorded");
        assert!(resolved < total, "the failing window must stay pending");
        assert!(
            !core.store.chunks_for_source(&out.id).await.unwrap().is_empty(),
            "chunks from the successful windows must survive the error"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib segment:: split:: 2>&1 | tail -20`
Expected: FAIL — `window_text` not found, `FakeChunker::failing_on` not found.

- [ ] **Step 3: Add the split helper**

In `src/infer/split.rs`:

```rust
/// The exact lines of a stored window, one-based and inclusive.
///
/// Windows live in the database between job runs, so this takes line numbers
/// rather than a `Window`: the text may have been re-read since, and clamping
/// beats panicking on a stale row.
pub fn window_text(text: &str, start_line: i64, end_line: i64) -> String {
    if start_line < 1 || end_line < start_line {
        return String::new();
    }
    text.lines()
        .skip((start_line - 1) as usize)
        .take((end_line - start_line + 1) as usize)
        .collect::<Vec<_>>()
        .join("\n")
}
```

- [ ] **Step 4: Add the test chunker**

In `src/infer/fake.rs`, replace the `FakeChunker` definition and constructors (keep the `Chunker` impl's paragraph-splitting body, adding the marker check at the top):

```rust
#[derive(Default)]
pub struct FakeChunker {
    fail_with: Option<String>,
    /// Fail only on windows containing this marker. Lets a test model the
    /// realistic case — some windows succeed, one does not.
    fail_on_marker: Option<String>,
}

impl FakeChunker {
    pub fn failing(msg: &str) -> Self {
        Self {
            fail_with: Some(msg.to_string()),
            fail_on_marker: None,
        }
    }

    pub fn failing_on(marker: &str) -> Self {
        Self {
            fail_with: None,
            fail_on_marker: Some(marker.to_string()),
        }
    }
}
```

and at the top of `segment`:

```rust
        if let Some(m) = &self.fail_with {
            return Err(Error::Inference { role: "chunk", detail: m.clone() });
        }
        if let Some(marker) = &self.fail_on_marker
            && text.contains(marker.as_str())
        {
            return Err(Error::Inference {
                role: "chunk",
                detail: format!("refusing window containing {marker}"),
            });
        }
```

- [ ] **Step 5: Rewrite the segment job**

Replace `run` and `write_chunks` in `src/jobs/segment.rs`:

```rust
/// LLM-assisted segmentation, one window at a time.
///
/// The window rows are the job's memory. A window that succeeds is written and
/// marked `done` before the next one is attempted, so an error here costs the
/// windows that had not started yet and nothing else — the job retries and
/// resumes from the first pending window.
pub async fn run(core: &Core, source_id: &str) -> Result<()> {
    let src = core.store.get_source(source_id).await?;
    core.store
        .set_source_status(source_id, SourceStatus::Segmenting)
        .await?;

    let windows = split_into_windows(
        &src.raw_text,
        &core.counter,
        window_tokens(core.chunker.budget(), prompt_overhead(core)),
    );
    if windows.is_empty() {
        tracing::warn!(source_id, "source has no usable text");
        core.store
            .set_source_status(source_id, SourceStatus::Failed)
            .await?;
        return Ok(());
    }

    let spans: Vec<(i64, i64)> = windows.iter().map(|w| (w.start_line, w.end_line)).collect();
    core.store.upsert_windows(source_id, &spans).await?;

    for w in core.store.pending_windows(source_id).await? {
        core.store.bump_window_attempts(source_id, w.idx).await?;
        let text = window_text(&src.raw_text, w.start_line, w.end_line);
        let mut chunks = core.chunker.segment(&text).await?;
        // Line numbers come back relative to the window, so shift them into
        // the coordinates of the original document.
        for c in &mut chunks {
            c.source_lines = c
                .source_lines
                .map(|(a, b)| (a + w.start_line - 1, b + w.start_line - 1))
                .or(Some((w.start_line, w.end_line)));
        }
        write_window_chunks(core, source_id, w.idx, proposed_to_new(w.idx, chunks)).await?;
        core.store
            .set_window_state(source_id, w.idx, WindowState::Done, None)
            .await?;
    }

    finish(core, source_id).await
}

/// Replace the chunks of one window. Same "replace, never append" guarantee as
/// before; the key is the window rather than the whole source, so a retry of
/// window 4 cannot disturb windows 0 to 3.
async fn write_window_chunks(
    core: &Core,
    source_id: &str,
    window_idx: i64,
    new: Vec<NewChunk>,
) -> Result<()> {
    let old = core.store.chunk_ids_for_window(source_id, window_idx).await?;
    if !old.is_empty() {
        core.vectors.delete_chunks(&old).await?;
        for id in &old {
            core.store.delete_chunk(id).await?;
        }
    }
    core.store.insert_chunks(source_id, &new).await?;
    Ok(())
}

/// Everything that can only be decided once every window has resolved:
/// continuous ordinals, the source's status, and the single batched embed job.
pub async fn finish(core: &Core, source_id: &str) -> Result<()> {
    core.store.renumber_chunks(source_id).await?;
    let windows = core.store.windows_for_source(source_id).await?;
    let degraded = windows.iter().any(|w| w.state == WindowState::Fallback);
    let chunks = core.store.chunks_for_source(source_id).await?;
    if chunks.is_empty() {
        core.store
            .set_source_status(source_id, SourceStatus::Failed)
            .await?;
        return Ok(());
    }

    core.store.enqueue(Stage::Embed, "source", source_id).await?;
    let status = if degraded {
        SourceStatus::Partial
    } else {
        SourceStatus::Embedding
    };
    core.store.set_source_status(source_id, status).await?;
    tracing::info!(source_id, chunks = chunks.len(), degraded, "segmented");
    Ok(())
}
```

Update `proposed_to_new` to take the window index and number within the window:

```rust
fn proposed_to_new(window_idx: i64, proposed: Vec<crate::infer::ProposedChunk>) -> Vec<NewChunk> {
    proposed
        .into_iter()
        .enumerate()
        .map(|(i, p)| NewChunk {
            ordinal: i as i64,
            text: p.text,
            source_span: p.source_lines.map(|(a, b)| SourceSpan {
                start_line: a,
                end_line: b,
            }),
            title: p.title,
            category: p.category,
            tags: p.tags,
            window_idx: Some(window_idx),
        })
        .collect()
}
```

Add the imports this needs: `use crate::infer::split::window_text;` and `use crate::store::windows::WindowState;`.

Leave `run_with_fallback` in place for now — Task 3 replaces it.

- [ ] **Step 6: Run the tests**

Run: `cargo test --lib segment:: split::`
Expected: PASS. The pre-existing `ordinals_stay_continuous_across_windows` test must still pass — that is what `renumber_chunks` is for.

- [ ] **Step 7: Commit**

```bash
git add src/infer/split.rs src/infer/fake.rs src/jobs/segment.rs
git commit -m "feat: segment window by window and resume where a retry left off"
```

---

### Task 3: A hopeless window degrades alone

**Files:**
- Modify: `src/jobs/segment.rs` (`fallback_pending_windows` replaces `run_with_fallback`)
- Modify: `src/jobs/mod.rs:52-66` (exhausted segment branch)
- Test: `src/jobs/segment.rs` test module

**Interfaces:**
- Consumes: `finish`, `write_window_chunks`, `WindowState`, `structural_chunks`.
- Produces: `jobs::segment::fallback_pending_windows(core: &Core, source_id: &str, reason: &str) -> Result<()>`

- [ ] **Step 1: Write the failing test**

Add to `src/jobs/segment.rs` tests:

```rust
    #[tokio::test]
    async fn only_the_unfinished_window_falls_back_to_a_structural_split() {
        let mut core = test_core().await;
        let body = format!("{}\n\nSTOPHERE marker paragraph\n", multi_window_body());
        let out = core.ingest(&body, "web", None).await.unwrap();
        core.chunker = std::sync::Arc::new(crate::infer::fake::FakeChunker::failing_on("STOPHERE"));

        // First pass records the good windows and raises on the bad one.
        assert!(run(&core, &out.id).await.is_err());
        let llm_chunks = core.store.chunks_for_source(&out.id).await.unwrap().len();

        fallback_pending_windows(&core, &out.id, "endpoint refused the window")
            .await
            .unwrap();

        let windows = core.store.windows_for_source(&out.id).await.unwrap();
        assert!(
            windows.iter().any(|w| w.state == WindowState::Done),
            "successful windows must stay done"
        );
        let fell_back: Vec<_> = windows
            .iter()
            .filter(|w| w.state == WindowState::Fallback)
            .collect();
        assert_eq!(fell_back.len(), 1);
        assert_eq!(
            fell_back[0].last_error.as_deref(),
            Some("endpoint refused the window")
        );

        assert!(
            core.store.chunks_for_source(&out.id).await.unwrap().len() > llm_chunks,
            "the fallback window must contribute its own chunks"
        );
        assert_eq!(
            core.store.get_source(&out.id).await.unwrap().status,
            SourceStatus::Partial,
            "a degraded window makes the source partial, not ready"
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib segment::tests::only_the_unfinished_window 2>&1 | tail -10`
Expected: FAIL — `fallback_pending_windows` not found.

- [ ] **Step 3: Implement the per-window fallback**

Replace `run_with_fallback` in `src/jobs/segment.rs`:

```rust
/// No-LLM fallback, used once a window has exhausted its retries.
///
/// Scoped to the windows that never finished: a structural split is worse than
/// an LLM split, and applying it to windows that already succeeded would throw
/// away good work to punish one bad one.
pub async fn fallback_pending_windows(core: &Core, source_id: &str, reason: &str) -> Result<()> {
    let src = core.store.get_source(source_id).await?;
    let pending = core.store.pending_windows(source_id).await?;
    if pending.is_empty() {
        return finish(core, source_id).await;
    }

    for w in pending {
        let text = window_text(&src.raw_text, w.start_line, w.end_line);
        let new: Vec<NewChunk> = structural_chunks(&text)
            .into_iter()
            .enumerate()
            .map(|(i, (text, start, end))| NewChunk {
                ordinal: i as i64,
                text,
                source_span: Some(SourceSpan {
                    // structural_chunks numbers from the window's first line.
                    start_line: start + w.start_line - 1,
                    end_line: end + w.start_line - 1,
                }),
                title: None,
                category: None,
                tags: vec![],
                window_idx: Some(w.idx),
            })
            .collect();
        write_window_chunks(core, source_id, w.idx, new).await?;
        core.store
            .set_window_state(source_id, w.idx, WindowState::Fallback, Some(reason))
            .await?;
        tracing::warn!(
            source_id,
            window = w.idx,
            lines = format!("{}-{}", w.start_line, w.end_line),
            "window fell back to a structural split"
        );
    }
    finish(core, source_id).await
}
```

- [ ] **Step 4: Wire it into the job runner**

In `src/jobs/mod.rs`, replace the exhausted-segment branch:

```rust
                // Out of attempts against the chunker. Only the windows that
                // never finished are split structurally; the rest keep the
                // segmentation they already earned.
                (Stage::Segment, _) if exhausted => {
                    tracing::warn!(error = %e, "segmentation exhausted retries; falling back per window");
                    match segment::fallback_pending_windows(core, &job.target_id, &e.to_string())
                        .await
                    {
                        Ok(()) => {
                            core.store.complete_job(job.id).await?;
                        }
                        Err(fe) => {
                            core.store
                                .fail_job(job.id, job.attempts, &fe.to_string())
                                .await?;
                        }
                    }
                }
```

Update any test in `src/jobs/mod.rs` that calls `run_with_fallback` to call `fallback_pending_windows(core, id, "test")`.

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib jobs::`
Expected: PASS, including the pre-existing "exhausted retries still produce chunks" test.

- [ ] **Step 6: Commit**

```bash
git add src/jobs
git commit -m "feat: degrade a hopeless window instead of the whole source"
```

---

### Task 4: Literal verification

**Files:**
- Create: `src/infer/verify.rs`
- Modify: `src/infer/mod.rs` (`pub mod verify;`)
- Modify: `src/jobs/segment.rs` (retry once, then flag)
- Modify: `src/infer/fake.rs` (a chunker that paraphrases)
- Test: `src/infer/verify.rs` and `src/jobs/segment.rs` test modules

**Interfaces:**
- Consumes: `ProposedChunk`, `Store::set_chunk_flags`.
- Produces:
  - `infer::verify::extract_literals(chunk_text: &str) -> Vec<String>`
  - `infer::verify::missing_literals(chunk_text: &str, window_text: &str) -> Vec<String>`
  - `infer::verify::FLAG_LITERALS: &str = "literals_unverified"`

- [ ] **Step 1: Write the failing unit tests**

Create `src/infer/verify.rs` containing only this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: &str = "\
### Writing the ISO

Unmount the device first.

    umount /dev/sdX*
    dd if=archlinux.iso of=/dev/sdX bs=4M oflag=sync status=progress

Use the whole device (/dev/sdX), never a partition, and pass --dry-run first.";

    #[test]
    fn fenced_code_inline_code_and_paths_are_all_literals() {
        let chunk = "Run this:\n\n```bash\ndd if=x.iso of=/dev/sdX\n```\n\nCheck `/etc/fstab` and pass --dry-run.";
        let lits = extract_literals(chunk);
        assert!(lits.iter().any(|l| l.contains("dd if=x.iso")));
        assert!(lits.iter().any(|l| l == "/etc/fstab"));
        assert!(lits.iter().any(|l| l == "--dry-run"));
    }

    #[test]
    fn a_verbatim_chunk_reports_nothing_missing() {
        let chunk = "Unmount first.\n\n```bash\ndd if=archlinux.iso of=/dev/sdX bs=4M oflag=sync status=progress\n```\n\nUse /dev/sdX with --dry-run.";
        assert!(missing_literals(chunk, WINDOW).is_empty());
    }

    #[test]
    fn a_dropped_flag_is_reported() {
        // The model rewrote the command and lost oflag=sync. This is the
        // failure the whole check exists for.
        let chunk = "```bash\ndd if=archlinux.iso of=/dev/sdX bs=4M status=progress\n```";
        let missing = missing_literals(chunk, WINDOW);
        assert_eq!(missing.len(), 1);
        assert!(missing[0].contains("status=progress"));
    }

    #[test]
    fn indentation_and_whitespace_runs_do_not_count_as_a_mismatch() {
        // The window indents the command by four spaces; the chunk fences it.
        let chunk = "```\numount   /dev/sdX*\n```";
        assert!(missing_literals(chunk, WINDOW).is_empty());
    }

    #[test]
    fn prose_alone_has_no_literals_to_check() {
        assert!(extract_literals("Just some ordinary prose about disks.").is_empty());
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib verify:: 2>&1 | tail -10`
Expected: FAIL — module not declared, functions not found.

- [ ] **Step 3: Implement the checks**

Write above that test module in `src/infer/verify.rs`:

```rust
//! Does the chunk still say what the source said?
//!
//! The chunker is instructed to reproduce commands, paths and error strings
//! verbatim while rewriting the prose around them. Nothing checked that it
//! did, and a paraphrased command is a command that later gets pasted into a
//! root shell. These are pure functions over two strings so they can be tested
//! exhaustively without a model.

/// A chunk contains a command, path or flag that its window does not.
pub const FLAG_LITERALS: &str = "literals_unverified";

/// Collapse whitespace runs so an indented source line and a fenced chunk line
/// compare equal. Anything else — a changed flag, a renamed path — still differs.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn looks_like_a_path_or_flag(token: &str) -> bool {
    let t = token.trim_matches(|c: char| matches!(c, '(' | ')' | ',' | '.' | ';' | ':' | '"' | '\''));
    if t.len() < 3 {
        return false;
    }
    t.starts_with("--") || t.starts_with('/') || t.starts_with("~/") || (t.contains('/') && !t.contains(' '))
}

/// Every string in a chunk that must have come from the source verbatim:
/// lines inside fenced code blocks, inline code spans, and bare path- or
/// flag-shaped tokens in the prose.
pub fn extract_literals(chunk_text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in chunk_text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            if !line.trim().is_empty() {
                out.push(line.trim().to_string());
            }
            continue;
        }
        // Inline code spans.
        let mut rest = line;
        while let Some(open) = rest.find('`') {
            let after = &rest[open + 1..];
            match after.find('`') {
                Some(close) => {
                    let span = after[..close].trim();
                    if !span.is_empty() {
                        out.push(span.to_string());
                    }
                    rest = &after[close + 1..];
                }
                None => break,
            }
        }
        // Bare paths and flags outside code spans.
        for token in line.split_whitespace() {
            if looks_like_a_path_or_flag(token) {
                out.push(
                    token
                        .trim_matches(|c: char| {
                            matches!(c, '(' | ')' | ',' | '.' | ';' | ':' | '"' | '\'' | '`')
                        })
                        .to_string(),
                );
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Literals present in the chunk and absent from the window it came from.
pub fn missing_literals(chunk_text: &str, window_text: &str) -> Vec<String> {
    let haystack = normalize(window_text);
    extract_literals(chunk_text)
        .into_iter()
        .filter(|lit| !haystack.contains(&normalize(lit)))
        .collect()
}
```

Add `pub mod verify;` to `src/infer/mod.rs`.

- [ ] **Step 4: Run the unit tests**

Run: `cargo test --lib verify::`
Expected: PASS (5 tests).

- [ ] **Step 5: Write the failing integration test for the retry**

Add to `src/infer/fake.rs`:

```rust
/// Drops a token from the first window it sees and reproduces it faithfully
/// afterwards. Models the case the retry exists for: a one-off paraphrase that
/// a second attempt gets right.
pub struct ParaphrasingChunker {
    drop_token: String,
    calls: std::sync::atomic::AtomicUsize,
    /// Keep paraphrasing forever rather than recovering on the retry.
    persistent: bool,
}

impl ParaphrasingChunker {
    pub fn recovering(drop_token: &str) -> Self {
        Self {
            drop_token: drop_token.to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            persistent: false,
        }
    }

    pub fn persistent(drop_token: &str) -> Self {
        Self {
            drop_token: drop_token.to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            persistent: true,
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait]
impl Chunker for ParaphrasingChunker {
    async fn segment(&self, text: &str) -> Result<Vec<ProposedChunk>> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let body = if self.persistent || n == 0 {
            text.replace(&self.drop_token, "")
        } else {
            text.to_string()
        };
        Ok(vec![ProposedChunk {
            text: body,
            title: Some("paraphrased".into()),
            category: Some("note".into()),
            tags: vec![],
            source_lines: None,
        }])
    }
    fn budget(&self) -> ChunkBudget {
        ChunkBudget {
            context_tokens: 4096,
            max_output_tokens: 1024,
            output_ratio: 1.4,
        }
    }
}
```

Add to `src/jobs/segment.rs` tests:

```rust
    const COMMAND_BODY: &str = "\
Unmount the device first.

    dd if=archlinux.iso of=/dev/sdX bs=4M oflag=sync status=progress

Then run sync.";

    #[tokio::test]
    async fn a_paraphrased_literal_is_re_segmented_once_and_then_accepted() {
        let mut core = test_core().await;
        let chunker = std::sync::Arc::new(
            crate::infer::fake::ParaphrasingChunker::recovering("oflag=sync "),
        );
        core.chunker = chunker.clone();
        let out = core.ingest(COMMAND_BODY, "web", None).await.unwrap();

        run(&core, &out.id).await.unwrap();

        assert_eq!(chunker.calls(), 2, "exactly one re-segmentation");
        let chunks = core.store.chunks_for_source(&out.id).await.unwrap();
        assert!(
            chunks.iter().all(|c| c.flags.is_empty()),
            "a clean retry must leave no flag"
        );
    }

    #[tokio::test]
    async fn a_literal_the_retry_also_drops_is_stored_flagged() {
        let mut core = test_core().await;
        core.chunker = std::sync::Arc::new(
            crate::infer::fake::ParaphrasingChunker::persistent("oflag=sync "),
        );
        let out = core.ingest(COMMAND_BODY, "web", None).await.unwrap();

        run(&core, &out.id).await.unwrap();

        let chunks = core.store.chunks_for_source(&out.id).await.unwrap();
        assert!(!chunks.is_empty(), "flagged chunks are still stored");
        let flagged: Vec<_> = chunks
            .iter()
            .filter(|c| c.flags.iter().any(|f| f == crate::infer::verify::FLAG_LITERALS))
            .collect();
        assert_eq!(flagged.len(), 1);
        assert!(
            flagged[0].flag_detail.as_deref().unwrap().contains("dd if="),
            "the detail must name the literal that went missing"
        );
    }
```

- [ ] **Step 6: Run them to verify they fail**

Run: `cargo test --lib segment::tests::a_paraphrased segment::tests::a_literal 2>&1 | tail -15`
Expected: FAIL — `ParaphrasingChunker` not found; then, once it compiles, `chunker.calls() == 1` and no flags.

- [ ] **Step 7: Verify inside the segmentation loop**

In `src/jobs/segment.rs`, replace the body of the per-window loop in `run` with:

```rust
    for w in core.store.pending_windows(source_id).await? {
        core.store.bump_window_attempts(source_id, w.idx).await?;
        let text = window_text(&src.raw_text, w.start_line, w.end_line);

        let mut chunks = core.chunker.segment(&text).await?;
        // The model was told to keep commands, paths and flags verbatim. If it
        // did not, one more attempt usually gets it right; a second failure is
        // stored with a flag rather than dropped, because a visible warning
        // beats losing the chapter.
        if !paraphrased(&chunks, &text).is_empty() {
            tracing::warn!(source_id, window = w.idx, "literals missing; re-segmenting once");
            let retry = core.chunker.segment(&text).await?;
            if paraphrased(&retry, &text).is_empty() {
                chunks = retry;
            } else {
                chunks = retry;
            }
        }

        for c in &mut chunks {
            c.source_lines = c
                .source_lines
                .map(|(a, b)| (a + w.start_line - 1, b + w.start_line - 1))
                .or(Some((w.start_line, w.end_line)));
        }

        let new = proposed_to_new(w.idx, chunks);
        let written = write_window_chunks(core, source_id, w.idx, new).await?;
        flag_unverified_literals(core, &written, &text).await?;
        core.store
            .set_window_state(source_id, w.idx, WindowState::Done, None)
            .await?;
    }
```

Change `write_window_chunks` to return what it inserted:

```rust
async fn write_window_chunks(
    core: &Core,
    source_id: &str,
    window_idx: i64,
    new: Vec<NewChunk>,
) -> Result<Vec<crate::store::chunks::Chunk>> {
    let old = core.store.chunk_ids_for_window(source_id, window_idx).await?;
    if !old.is_empty() {
        core.vectors.delete_chunks(&old).await?;
        for id in &old {
            core.store.delete_chunk(id).await?;
        }
    }
    core.store.insert_chunks(source_id, &new).await
}
```

and add the two helpers:

```rust
/// Chunks whose literals do not all appear in the window they came from.
fn paraphrased(chunks: &[crate::infer::ProposedChunk], window: &str) -> Vec<usize> {
    chunks
        .iter()
        .enumerate()
        .filter(|(_, c)| !crate::infer::verify::missing_literals(&c.text, window).is_empty())
        .map(|(i, _)| i)
        .collect()
}

async fn flag_unverified_literals(
    core: &Core,
    written: &[crate::store::chunks::Chunk],
    window: &str,
) -> Result<()> {
    for c in written {
        let missing = crate::infer::verify::missing_literals(&c.text, window);
        if let Some(first) = missing.first() {
            core.store
                .set_chunk_flags(
                    &c.id,
                    &[crate::infer::verify::FLAG_LITERALS.to_string()],
                    Some(&format!("missing literal: {first}")),
                )
                .await?;
            tracing::warn!(chunk_id = %c.id, literal = %first, "literal not found in source window");
        }
    }
    Ok(())
}
```

Note `fallback_pending_windows` now ignores `write_window_chunks`'s return value — assign it to `let _ =` there, since structural chunks are copied verbatim and cannot paraphrase anything.

- [ ] **Step 8: Run the tests**

Run: `cargo test --lib`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add src/infer src/jobs/segment.rs
git commit -m "feat: verify a chunk's literals against the window it came from"
```

---

### Task 5: Span checking and coverage

**Files:**
- Modify: `src/infer/verify.rs` (span plausibility, coverage)
- Modify: `src/jobs/segment.rs` (clamp and flag spans, store coverage in `finish`)
- Modify: `src/infer/fake.rs` (a chunker that lies about spans)
- Test: `src/infer/verify.rs`, `src/jobs/segment.rs` test modules

**Interfaces:**
- Consumes: Task 4's module, `Store::set_source_coverage`.
- Produces:
  - `infer::verify::span_is_plausible(chunk_text: &str, claimed_text: &str) -> bool`
  - `infer::verify::coverage(spans: &[(i64, i64)], raw_text: &str) -> f64`
  - `infer::verify::FLAG_SPAN: &str = "span_unverified"`
  - `infer::verify::LOW_COVERAGE: f64 = 0.6`

- [ ] **Step 1: Write the failing unit tests**

Add to `src/infer/verify.rs` tests:

```rust
    #[test]
    fn a_span_over_the_lines_the_chunk_rewrote_is_plausible() {
        let claimed = "    dd if=archlinux.iso of=/dev/sdX bs=4M oflag=sync status=progress";
        let chunk = "Write the image:\n\n```\ndd if=archlinux.iso of=/dev/sdX bs=4M oflag=sync status=progress\n```";
        assert!(span_is_plausible(chunk, claimed));
    }

    #[test]
    fn a_span_pointing_at_unrelated_lines_is_not_plausible() {
        let claimed = "The kernel keeps a page cache of recently read blocks.";
        let chunk = "```\nmkfs.ext4 /dev/sdX1\n```\nFormat the partition with mkfs.";
        assert!(!span_is_plausible(chunk, claimed));
    }

    #[test]
    fn coverage_is_the_fraction_of_non_blank_lines_claimed() {
        let raw = "one\n\ntwo\nthree\nfour";           // four non-blank lines
        assert!((coverage(&[(1, 1), (3, 3)], raw) - 0.5).abs() < 1e-6);
        assert!((coverage(&[(1, 5)], raw) - 1.0).abs() < 1e-6);
        assert_eq!(coverage(&[], raw), 0.0);
        // Overlapping spans must not push coverage above one.
        assert!((coverage(&[(1, 5), (2, 4)], raw) - 1.0).abs() < 1e-6);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib verify:: 2>&1 | tail -10`
Expected: FAIL — `span_is_plausible`, `coverage` not found.

- [ ] **Step 3: Implement**

Append to `src/infer/verify.rs`:

```rust
/// A chunk whose span points somewhere else entirely.
pub const FLAG_SPAN: &str = "span_unverified";

/// Below this fraction of a source inside some chunk, the segmenter probably
/// dropped part of the document.
pub const LOW_COVERAGE: f64 = 0.6;

fn distinctive_tokens(s: &str) -> std::collections::HashSet<String> {
    s.split(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '=')))
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| t.len() > 3)
        .collect()
}

/// Does the chunk plausibly describe the lines it claims?
///
/// The chunker rewrites prose, so this cannot demand equality — only that a
/// third of the chunk's distinctive tokens appear in the claimed range. That is
/// enough to catch a span pointing at a different section, which is the failure
/// the detail pane would otherwise render as a rendering bug.
pub fn span_is_plausible(chunk_text: &str, claimed_text: &str) -> bool {
    let chunk = distinctive_tokens(chunk_text);
    if chunk.is_empty() {
        return true;
    }
    let claimed = distinctive_tokens(claimed_text);
    let shared = chunk.iter().filter(|t| claimed.contains(*t)).count();
    shared * 3 >= chunk.len()
}

/// Fraction of the source's non-blank lines that some chunk span covers.
pub fn coverage(spans: &[(i64, i64)], raw_text: &str) -> f64 {
    let lines: Vec<&str> = raw_text.lines().collect();
    let total = lines.iter().filter(|l| !l.trim().is_empty()).count();
    if total == 0 {
        return 0.0;
    }
    let mut covered = vec![false; lines.len()];
    for (start, end) in spans {
        let s = (*start).max(1) as usize;
        let e = (*end).min(lines.len() as i64) as usize;
        for i in s..=e.max(s) {
            if i >= 1 && i <= lines.len() {
                covered[i - 1] = true;
            }
        }
    }
    let hit = lines
        .iter()
        .enumerate()
        .filter(|(i, l)| !l.trim().is_empty() && covered[*i])
        .count();
    hit as f64 / total as f64
}
```

- [ ] **Step 4: Write the failing job test**

Add to `src/infer/fake.rs`:

```rust
/// Claims every chunk came from lines far outside its window. The span check
/// exists because the model's line numbers are taken on trust.
#[derive(Default)]
pub struct LyingSpanChunker;

#[async_trait]
impl Chunker for LyingSpanChunker {
    async fn segment(&self, text: &str) -> Result<Vec<ProposedChunk>> {
        Ok(vec![ProposedChunk {
            text: text.to_string(),
            title: Some("mislabelled".into()),
            category: None,
            tags: vec![],
            source_lines: Some((9_000, 9_100)),
        }])
    }
    fn budget(&self) -> ChunkBudget {
        ChunkBudget {
            context_tokens: 4096,
            max_output_tokens: 1024,
            output_ratio: 1.4,
        }
    }
}
```

Add to `src/jobs/segment.rs` tests:

```rust
    #[tokio::test]
    async fn a_span_outside_its_window_is_clamped_and_flagged() {
        let mut core = test_core().await;
        core.chunker = std::sync::Arc::new(crate::infer::fake::LyingSpanChunker);
        let out = core.ingest("first para\n\nsecond para", "web", None).await.unwrap();

        run(&core, &out.id).await.unwrap();

        let c = &core.store.chunks_for_source(&out.id).await.unwrap()[0];
        let span = c.source_span.as_ref().unwrap();
        assert!(span.start_line >= 1 && span.end_line <= 3, "span must be clamped to the window");
        assert!(c.flags.iter().any(|f| f == crate::infer::verify::FLAG_SPAN));
    }

    #[tokio::test]
    async fn coverage_is_recorded_on_the_source() {
        let core = test_core().await;
        let out = core.ingest("first para\n\nsecond para", "web", None).await.unwrap();
        run(&core, &out.id).await.unwrap();
        let cov = core.store.get_source(&out.id).await.unwrap().coverage.unwrap();
        assert!(cov > 0.0 && cov <= 1.0);
    }
```

- [ ] **Step 5: Run to verify failure**

Run: `cargo test --lib segment::tests::a_span_outside segment::tests::coverage_is_recorded 2>&1 | tail -12`
Expected: FAIL — no clamping, no flag, `coverage` is `None`.

- [ ] **Step 6: Clamp spans and record coverage**

In `src/jobs/segment.rs`, replace the span-shifting block inside the loop:

```rust
        for c in &mut chunks {
            let shifted = c
                .source_lines
                .map(|(a, b)| (a + w.start_line - 1, b + w.start_line - 1))
                .unwrap_or((w.start_line, w.end_line));
            // The model's line numbers are taken on trust; a span outside its
            // own window is nonsense the detail pane would render as the wrong
            // text, so clamp it and say so.
            let clamped = (
                shifted.0.clamp(w.start_line, w.end_line),
                shifted.1.clamp(w.start_line, w.end_line),
            );
            c.source_lines = Some(if clamped.0 <= clamped.1 {
                clamped
            } else {
                (w.start_line, w.end_line)
            });
        }
```

Extend `flag_unverified_literals` into a general verification pass — rename it `flag_unverified` and call it with the source text as well:

```rust
async fn flag_unverified(
    core: &Core,
    written: &[crate::store::chunks::Chunk],
    window: &crate::store::windows::SegmentWindow,
    window_text_value: &str,
    raw_text: &str,
) -> Result<()> {
    for c in written {
        let mut flags = Vec::new();
        let mut detail: Option<String> = None;

        let missing = crate::infer::verify::missing_literals(&c.text, window_text_value);
        if let Some(first) = missing.first() {
            flags.push(crate::infer::verify::FLAG_LITERALS.to_string());
            detail = Some(format!("missing literal: {first}"));
            tracing::warn!(chunk_id = %c.id, literal = %first, "literal not found in source window");
        }

        if let Some(span) = &c.source_span {
            let claimed = crate::infer::split::window_text(raw_text, span.start_line, span.end_line);
            let out_of_window = span.start_line < window.start_line || span.end_line > window.end_line;
            if out_of_window || !crate::infer::verify::span_is_plausible(&c.text, &claimed) {
                flags.push(crate::infer::verify::FLAG_SPAN.to_string());
                detail.get_or_insert_with(|| {
                    format!("span {}–{} does not match the chunk", span.start_line, span.end_line)
                });
            }
        }

        if !flags.is_empty() {
            core.store
                .set_chunk_flags(&c.id, &flags, detail.as_deref())
                .await?;
        }
    }
    Ok(())
}
```

Call it as `flag_unverified(core, &written, &w, &text, &src.raw_text).await?;`.

In `finish`, record coverage before setting status:

```rust
    let spans: Vec<(i64, i64)> = chunks
        .iter()
        .filter_map(|c| c.source_span.as_ref().map(|s| (s.start_line, s.end_line)))
        .collect();
    let cov = crate::infer::verify::coverage(&spans, &src.raw_text);
    core.store.set_source_coverage(source_id, cov).await?;
```

`finish` needs the source, so start it with `let src = core.store.get_source(source_id).await?;`.

- [ ] **Step 7: Run the tests**

Run: `cargo test --lib`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/infer src/jobs/segment.rs
git commit -m "feat: check chunk spans against their window and record coverage"
```

---

### Task 6: Progress, capture guidance and an explicit body limit

**Files:**
- Modify: `src/web/ui.rs` (`BrowseRow` gains progress and coverage; ops rows)
- Modify: `src/web/templates/browse.html`
- Modify: `src/web/templates/capture.html`
- Modify: `src/web/mod.rs` (body limit layer)
- Test: `src/web/ui.rs` test module (or `tests/` integration test, following whichever the file already uses), `src/web/mod.rs`

**Interfaces:**
- Consumes: `Store::window_progress`, `Source.coverage`.
- Produces: `web::MAX_BODY_BYTES: usize = 8 * 1024 * 1024`; `BrowseRow.progress: Option<String>`, `BrowseRow.coverage: Option<String>`.

- [ ] **Step 1: Write the failing test**

Add to `src/web/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_limit_is_deliberate_and_large_enough_for_a_chapter() {
        // axum's default is 2 MB and was never chosen. A long chapter of prose
        // is well under this; a book-sized paste is refused with a message.
        assert_eq!(MAX_BODY_BYTES, 8 * 1024 * 1024);
    }
}
```

- [ ] **Step 2: Run it**

Run: `cargo test --lib web::tests 2>&1 | tail -6`
Expected: FAIL — `MAX_BODY_BYTES` not found.

- [ ] **Step 3: Set the limit**

In `src/web/mod.rs`:

```rust
/// Largest capture the server accepts.
///
/// Inherited from axum's 2 MB default until now, which was a number nobody
/// picked. Sized for a long chapter of prose with headroom, and small enough
/// that a runaway upload is refused rather than buffered.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(assets::assets_router())
        .merge(auth_routes::auth_router())
        .merge(ui::ui_router())
        .merge(crate::mcp::mcp_router(state.clone()))
        .nest("/api/v1", api::api_router())
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}
```

- [ ] **Step 4: Show window progress and coverage on Browse**

In `src/web/ui.rs`, extend `BrowseRow`:

```rust
pub struct BrowseRow {
    pub id: String,
    pub label: String,
    pub status: String,
    pub badge: &'static str,
    pub chunk_count: i64,
    pub created: String,
    /// `3/9` while segmenting, `None` once every window has resolved.
    pub progress: Option<String>,
    /// Percentage of the source that ended up inside some chunk.
    pub coverage: Option<String>,
    pub low_coverage: bool,
}
```

and in `browse`:

```rust
        let (resolved, total) = st.core.store.window_progress(&s.id).await?;
        let progress = (total > 0 && resolved < total).then(|| format!("{resolved}/{total}"));
        let low_coverage = s
            .coverage
            .is_some_and(|c| c < crate::infer::verify::LOW_COVERAGE);
        let coverage = s.coverage.map(|c| format!("{:.0}%", c * 100.0));
```

adding `progress`, `coverage`, `low_coverage` to the constructed `BrowseRow`.

In `src/web/templates/browse.html`, add a cell after the status badge:

```html
      <td>
        {% if let Some(p) = s.progress %}<span class="badge badge-accent">segmenting {{ p }}</span>{% endif %}
        {% if let Some(c) = s.coverage %}
          <span class="badge {% if s.low_coverage %}badge-warning{% else %}badge-muted{% endif %}"
                title="fraction of this source that ended up inside a chunk">{{ c }} covered</span>
        {% endif %}
      </td>
```

and a matching `<th>Progress</th>` in the header row.

- [ ] **Step 5: Say what capture expects**

In `src/web/templates/capture.html`, above the textarea:

```html
<p class="muted" style="margin:0 0 0.5rem">
  Paste a chapter at a time. Long text is split into windows and segmented one
  window per model call, so a whole book works but takes a while — and a
  chapter is what search results read best from.
</p>
<p id="size-hint" class="muted" hidden></p>
```

and at the end of the template's content block:

```html
<script>
  (function () {
    var box = document.querySelector('textarea[name="text"]');
    var hint = document.getElementById('size-hint');
    if (!box || !hint) return;
    // Rough stand-in for the tokeniser: enough to warn, never to block.
    var CHARS_PER_WINDOW = 12000;
    box.addEventListener('input', function () {
      var windows = Math.ceil(box.value.length / CHARS_PER_WINDOW);
      hint.hidden = windows < 2;
      hint.textContent = 'About ' + windows + ' windows — roughly ' + windows +
        ' model calls before this is searchable.';
    });
  })();
</script>
```

- [ ] **Step 6: Run the suite**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS with no warnings.

- [ ] **Step 7: Commit**

```bash
git add src/web assets
git commit -m "feat: show segmentation progress, guide capture size, set the body limit"
```

---

### Task 7: Ops surfaces what needs review

**Files:**
- Modify: `src/web/ui.rs` (`OpsTemplate` fields, two POST routes)
- Modify: `src/web/templates/ops.html`
- Test: `src/web/ui.rs` test module

**Interfaces:**
- Consumes: `Store::flagged_chunks`, `Store::reset_window`, `Store::clear_chunk_flags`, `Store::windows_for_source`, `Store::enqueue`.
- Produces:
  - Route `POST /ui/sources/{sid}/windows/{idx}/resegment`
  - Route `POST /ui/chunks/{cid}/reviewed`
  - `ui::FlaggedRow { chunk_id, source_id, title, detail, window_idx }`

- [ ] **Step 1: Write the failing test**

In `src/web/ui.rs` tests (following the existing pattern there for building a router with a test `AppState`; if none exists, add the test to `tests/` mirroring `tests/integration_*.rs`):

```rust
    #[tokio::test]
    async fn resegmenting_a_window_makes_it_pending_and_queues_the_job() {
        let core = crate::core::test_support::test_core().await;
        let out = core.ingest("first para\n\nsecond para", "web", None).await.unwrap();
        crate::jobs::segment::run(&core, &out.id).await.unwrap();
        core.store
            .set_window_state(&out.id, 0, crate::store::windows::WindowState::Fallback, Some("boom"))
            .await
            .unwrap();

        super::resegment_window_inner(&core, &out.id, 0).await.unwrap();

        let w = &core.store.windows_for_source(&out.id).await.unwrap()[0];
        assert_eq!(w.state, crate::store::windows::WindowState::Pending);
        assert_eq!(w.attempts, 0);

        let mut found = false;
        while let Some(j) = core.store.claim_job().await.unwrap() {
            if j.stage == crate::store::jobs::Stage::Segment && j.target_id == out.id {
                found = true;
            }
        }
        assert!(found, "a segment job must be queued for the source");
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test --lib ui::tests::resegmenting 2>&1 | tail -8`
Expected: FAIL — `resegment_window_inner` not found.

- [ ] **Step 3: Implement the actions**

In `src/web/ui.rs`:

```rust
/// The action behind "re-segment this window": put the window back in the
/// queue's path and make sure something will pick it up. Split out from the
/// handler so it can be tested without a request.
pub(crate) async fn resegment_window_inner(
    core: &crate::core::Core,
    source_id: &str,
    idx: i64,
) -> Result<()> {
    core.store.reset_window(source_id, idx).await?;
    core.store
        .enqueue(crate::store::jobs::Stage::Segment, "source", source_id)
        .await?;
    Ok(())
}

async fn resegment_window(
    State(st): State<AppState>,
    _id: Identity,
    Path((sid, idx)): Path<(String, i64)>,
) -> Result<Response> {
    resegment_window_inner(&st.core, &sid, idx).await?;
    Ok(Redirect::to("/ui/ops").into_response())
}

async fn mark_chunk_reviewed(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
) -> Result<Response> {
    st.core.store.clear_chunk_flags(&cid).await?;
    let c = st.core.store.get_chunk(&cid).await?;
    Ok(HtmlTemplate(ChunkFragment { c: chunk_view(&c) }).into_response())
}
```

Register both in `ui_router`:

```rust
        .route("/ui/sources/{sid}/windows/{idx}/resegment", post(resegment_window))
        .route("/ui/chunks/{cid}/reviewed", post(mark_chunk_reviewed))
```

Add the flagged list to `OpsTemplate`:

```rust
pub struct FlaggedRow {
    pub chunk_id: String,
    pub source_id: String,
    pub title: String,
    pub detail: String,
    pub window_idx: Option<i64>,
}
```

with `flagged: Vec<FlaggedRow>` on `OpsTemplate`, filled in `ops`:

```rust
    let flagged = st
        .core
        .store
        .flagged_chunks(50)
        .await?
        .into_iter()
        .map(|c| FlaggedRow {
            title: c.title.clone().unwrap_or_else(|| format!("Chunk {}", c.ordinal)),
            detail: c.flag_detail.clone().unwrap_or_else(|| c.flags.join(", ")),
            window_idx: c.window_idx,
            chunk_id: c.id,
            source_id: c.source_id,
        })
        .collect();
```

- [ ] **Step 4: Render it**

In `src/web/templates/ops.html`, after the failed-jobs section:

```html
<h3>Needs review</h3>
{% if flagged.is_empty() %}
  <p class="muted">Nothing flagged. Every chunk reproduced its source.</p>
{% else %}
<table class="grid">
  <tr><th>Chunk</th><th>Problem</th><th></th></tr>
  {% for f in flagged %}
  <tr>
    <td><a href="/ui/chunks/{{ f.chunk_id }}">{{ f.title }}</a></td>
    <td class="mono">{{ f.detail }}</td>
    <td>
      {% if let Some(w) = f.window_idx %}
      <form method="post" action="/ui/sources/{{ f.source_id }}/windows/{{ w }}/resegment">
        <button class="btn btn-sm" type="submit">Re-segment window</button>
      </form>
      {% endif %}
    </td>
  </tr>
  {% endfor %}
</table>
{% endif %}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/web
git commit -m "feat: list flagged chunks on Ops with a per-window re-segment action"
```

---

### Task 8: The source view seam

**Files:**
- Create: `src/web/source_view.rs`
- Modify: `src/web/mod.rs` (`pub mod source_view;`)
- Test: inline test module in `src/web/source_view.rs`

**Interfaces:**
- Consumes: `Source`, `SourceSpan`, `infer::split::window_text`.
- Produces:
  - `web::source_view::SourceSlice { lines: Vec<SourceLine>, label: String }`
  - `web::source_view::SourceLine { number: i64, text: String, in_span: bool }`
  - `web::source_view::SourceView` trait with `fn slice(&self, source: &Source, span: Option<&SourceSpan>, context: usize) -> SourceSlice`
  - `web::source_view::TextLines`
  - `web::source_view::for_source(source: &Source) -> Box<dyn SourceView>`

- [ ] **Step 1: Write the failing test**

Create `src/web/source_view.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::chunks::SourceSpan;

    async fn a_source(raw: &str) -> crate::store::sources::Source {
        let s = crate::store::Store::memory().await.unwrap();
        s.insert_source(raw, "web", None).await.unwrap()
    }

    #[tokio::test]
    async fn the_slice_marks_the_span_and_carries_context_around_it() {
        let src = a_source("l1\nl2\nl3\nl4\nl5\nl6").await;
        let slice = TextLines.slice(&src, Some(&SourceSpan { start_line: 3, end_line: 4 }), 1);

        assert_eq!(slice.label, "lines 3–4");
        assert_eq!(slice.lines.first().unwrap().number, 2);
        assert_eq!(slice.lines.last().unwrap().number, 5);
        let marked: Vec<i64> = slice.lines.iter().filter(|l| l.in_span).map(|l| l.number).collect();
        assert_eq!(marked, vec![3, 4]);
    }

    #[tokio::test]
    async fn a_chunk_without_a_span_gets_the_head_of_the_source() {
        let src = a_source("l1\nl2\nl3").await;
        let slice = TextLines.slice(&src, None, 1);
        assert_eq!(slice.label, "source");
        assert!(slice.lines.iter().all(|l| !l.in_span));
        assert_eq!(slice.lines.len(), 3);
    }

    #[tokio::test]
    async fn a_span_past_the_end_clamps_instead_of_panicking() {
        let src = a_source("l1\nl2").await;
        let slice = TextLines.slice(&src, Some(&SourceSpan { start_line: 5, end_line: 9 }), 2);
        assert!(slice.lines.iter().all(|l| l.number <= 2));
    }
}
```

- [ ] **Step 2: Run it**

Run: `cargo test --lib source_view 2>&1 | tail -8`
Expected: FAIL — module not declared.

- [ ] **Step 3: Implement**

Write above the tests:

```rust
//! How the right-hand pane gets at the text a chunk claims to come from.
//!
//! A trait rather than a function because the answer depends on what the
//! source is. Today every source is raw text and `TextLines` answers all of
//! them. A PDF source will implement the same trait — its label reads
//! `page 42` and its lines come from extracted text — and nothing in the pane
//! will need to know which implementation answered.

use crate::store::chunks::SourceSpan;
use crate::store::sources::Source;

pub struct SourceLine {
    pub number: i64,
    pub text: String,
    /// Inside the chunk's span, as opposed to the context around it.
    pub in_span: bool,
}

pub struct SourceSlice {
    pub lines: Vec<SourceLine>,
    /// What to call this range in the UI: `lines 118–141`, later `page 42`.
    pub label: String,
}

pub trait SourceView {
    fn slice(&self, source: &Source, span: Option<&SourceSpan>, context: usize) -> SourceSlice;
}

pub struct TextLines;

impl SourceView for TextLines {
    fn slice(&self, source: &Source, span: Option<&SourceSpan>, context: usize) -> SourceSlice {
        let all: Vec<&str> = source.raw_text.lines().collect();
        let total = all.len() as i64;
        let Some(span) = span else {
            return SourceSlice {
                lines: all
                    .iter()
                    .enumerate()
                    .take(40)
                    .map(|(i, t)| SourceLine {
                        number: i as i64 + 1,
                        text: (*t).to_string(),
                        in_span: false,
                    })
                    .collect(),
                label: "source".into(),
            };
        };

        let start = (span.start_line - context as i64).max(1);
        let end = (span.end_line + context as i64).min(total);
        let lines = (start..=end)
            .filter_map(|n| {
                all.get((n - 1) as usize).map(|t| SourceLine {
                    number: n,
                    text: (*t).to_string(),
                    in_span: n >= span.start_line && n <= span.end_line,
                })
            })
            .collect();

        SourceSlice {
            lines,
            label: format!("lines {}–{}", span.start_line, span.end_line),
        }
    }
}

/// The view for a source. One implementation today; the match arm is where a
/// PDF source will branch.
pub fn for_source(_source: &Source) -> Box<dyn SourceView> {
    Box::new(TextLines)
}
```

Add `pub mod source_view;` to `src/web/mod.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib source_view`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/web/source_view.rs src/web/mod.rs
git commit -m "feat: add the source view seam behind the chunk detail pane"
```

---

### Task 9: The chunk detail pane

**Files:**
- Create: `src/web/templates/_chunk_detail.html`, `src/web/templates/chunk_detail.html`
- Modify: `src/web/ui.rs` (`ChunkDetail` view model, `GET /ui/chunks/{id}`)
- Modify: `assets/app.css` (split pane, raw line table, flag banner)
- Test: `src/web/ui.rs` tests

**Interfaces:**
- Consumes: `source_view::for_source`, `Store::get_chunk`, `Store::get_source`, `markdown::render`.
- Produces:
  - `ui::ChunkDetail { id, title, html, category, tags, flags, flag_detail, source_id, window_idx, slice_label, slice_lines }`
  - `ui::chunk_detail(...)` handler on `GET /ui/chunks/{id}` (the existing `PUT` on that path stays)

- [ ] **Step 1: Write the failing test**

Add to `src/web/ui.rs` tests:

```rust
    #[tokio::test]
    async fn the_detail_view_pairs_a_chunk_with_the_lines_it_claims() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::segment::run(&core, &out.id).await.unwrap();
        let c = core.store.chunks_for_source(&out.id).await.unwrap().remove(0);

        let d = super::build_chunk_detail(&core, &c.id).await.unwrap();

        assert_eq!(d.source_id, out.id);
        assert!(d.html.contains("alpha"), "the chunk body must be rendered");
        assert!(!d.slice_lines.is_empty(), "the source slice must not be empty");
        assert!(
            d.slice_lines.iter().any(|l| l.in_span),
            "at least one line must be marked as the span"
        );
        assert!(d.slice_label.starts_with("lines "));
    }

    #[tokio::test]
    async fn a_chunk_whose_source_vanished_is_not_a_500() {
        let core = crate::core::test_support::test_core().await;
        let out = core.ingest("alpha\n\nbravo", "web", None).await.unwrap();
        crate::jobs::segment::run(&core, &out.id).await.unwrap();
        let c = core.store.chunks_for_source(&out.id).await.unwrap().remove(0);
        core.delete_source(&out.id).await.unwrap();

        let err = super::build_chunk_detail(&core, &c.id).await.unwrap_err();
        assert!(matches!(err, crate::error::Error::NotFound));
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test --lib ui::tests::the_detail_view 2>&1 | tail -8`
Expected: FAIL — `build_chunk_detail` not found.

- [ ] **Step 3: Build the view model and handler**

In `src/web/ui.rs`:

```rust
pub struct ChunkDetail {
    pub id: String,
    pub title: String,
    /// Sanitized by `markdown::render`; rendered with `|safe`.
    pub html: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub flags: Vec<String>,
    pub flag_detail: Option<String>,
    pub source_id: String,
    pub window_idx: Option<i64>,
    pub slice_label: String,
    pub slice_lines: Vec<crate::web::source_view::SourceLine>,
}

/// Everything the pane needs, in one place, so the handler is only routing.
pub(crate) async fn build_chunk_detail(
    core: &crate::core::Core,
    chunk_id: &str,
) -> Result<ChunkDetail> {
    let c = core.store.get_chunk(chunk_id).await?;
    let src = core.store.get_source(&c.source_id).await?;
    let slice = crate::web::source_view::for_source(&src).slice(&src, c.source_span.as_ref(), 3);
    Ok(ChunkDetail {
        id: c.id,
        title: c.title.unwrap_or_else(|| format!("Chunk {}", c.ordinal)),
        html: markdown::render(&c.text),
        category: c.category,
        tags: c.tags,
        flags: c.flags,
        flag_detail: c.flag_detail,
        source_id: c.source_id,
        window_idx: c.window_idx,
        slice_label: slice.label,
        slice_lines: slice.lines,
    })
}

#[derive(Template)]
#[template(path = "_chunk_detail.html")]
struct ChunkDetailFragment {
    d: ChunkDetail,
}

#[derive(Template)]
#[template(path = "chunk_detail.html")]
struct ChunkDetailPage {
    theme: String,
    d: ChunkDetail,
}

/// One route, two shapes. An htmx swap wants the pane's body; a pasted link
/// wants a page with navigation around it.
async fn chunk_detail(
    State(st): State<AppState>,
    _id: Identity,
    headers: axum::http::HeaderMap,
    Path(cid): Path<String>,
) -> Result<Response> {
    let d = build_chunk_detail(&st.core, &cid).await?;
    if headers.contains_key("hx-request") {
        return Ok(HtmlTemplate(ChunkDetailFragment { d }).into_response());
    }
    Ok(HtmlTemplate(ChunkDetailPage {
        theme: "light".into(),
        d,
    })
    .into_response())
}
```

Register it alongside the existing `PUT`:

```rust
        .route("/ui/chunks/{id}", get(chunk_detail).put(put_chunk))
```

- [ ] **Step 4: Write the templates**

`src/web/templates/_chunk_detail.html`:

```html
<div class="crumb">
  <a href="/ui/sources/{{ d.source_id }}">source</a> · {{ d.slice_label }}
</div>

{% if !d.flags.is_empty() %}
<div class="flag" role="status">
  <div aria-hidden="true">⚠</div>
  <div>
    <b>{{ d.flags.join(", ") }}</b>
    {% if let Some(detail) = d.flag_detail %}<div class="mono">{{ detail }}</div>{% endif %}
    <div class="chips">
      {% if let Some(w) = d.window_idx %}
      <form method="post" action="/ui/sources/{{ d.source_id }}/windows/{{ w }}/resegment">
        <button class="btn btn-sm" type="submit">Re-segment window</button>
      </form>
      {% endif %}
      <button class="btn btn-sm" hx-post="/ui/chunks/{{ d.id }}/reviewed"
              hx-target="closest .flag" hx-swap="outerHTML">Mark reviewed</button>
    </div>
  </div>
</div>
{% endif %}

<div class="split">
  <div>
    <div class="pane-label">Chunk</div>
    <div class="card">
      <div class="card-head"><span class="card-title">{{ d.title }}</span></div>
      <div class="md" data-copyable>{{ d.html|safe }}</div>
      <div class="chips">
        {% if let Some(c) = d.category %}<span class="badge badge-accent">{{ c }}</span>{% endif %}
        {% for t in d.tags %}<span class="badge">{{ t }}</span>{% endfor %}
      </div>
    </div>
  </div>
  <div>
    <div class="pane-label">
      {{ d.slice_label }}
      <span class="spacer"></span>
      <a href="/ui/sources/{{ d.source_id }}">open full source</a>
    </div>
    <div class="raw">
      <table>
        {% for l in d.slice_lines %}
        <tr class="{% if l.in_span %}in{% endif %}">
          <td class="ln">{{ l.number }}</td><td>{{ l.text }}</td>
        </tr>
        {% endfor %}
      </table>
    </div>
  </div>
</div>
```

`src/web/templates/chunk_detail.html`:

```html
{% extends "layout.html" %}
{% block title %}{{ d.title }} — engram{% endblock %}
{% block content %}
{% include "_chunk_detail.html" %}
{% endblock %}
```

- [ ] **Step 5: Style the pane**

Append to `assets/app.css` (tokens only — no new colour literals):

```css
.split { display: grid; grid-template-columns: 1fr 1fr; gap: 0.75rem; align-items: start; }
@media (max-width: 60rem) { .split { grid-template-columns: 1fr; } }

.pane-label {
  display: flex; gap: 0.5rem; align-items: baseline;
  font-size: 0.75rem; text-transform: uppercase; letter-spacing: 0.04em;
  color: var(--color-fg-muted); margin-bottom: 0.375rem;
}

.raw {
  background: var(--color-bg-elevated); border: 1px solid var(--color-border);
  border-radius: var(--radius-md); overflow: auto; max-height: 30rem;
}
.raw table { border-collapse: collapse; width: 100%; font-family: var(--font-mono); font-size: 0.8125rem; }
.raw td { padding: 1px 0.5rem; vertical-align: top; white-space: pre-wrap; }
.raw td.ln {
  width: 3.5rem; text-align: right; color: var(--color-fg-muted); user-select: none;
  border-right: 1px solid var(--color-border-subtle); font-variant-numeric: tabular-nums;
}
.raw tr.in td { background: var(--color-accent-dim); }
.raw tr.in td.ln { color: var(--color-accent); }

.flag {
  display: flex; gap: 0.625rem; align-items: flex-start;
  border: 1px solid var(--color-warning); background: var(--color-warning-dim);
  border-radius: var(--radius-md); padding: 0.625rem 0.75rem;
  font-size: 0.8125rem; margin-bottom: 0.75rem;
}
.flag b { color: var(--color-warning); }

.crumb { font-size: 0.8125rem; color: var(--color-fg-muted); margin-bottom: 0.75rem; }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/web assets/app.css
git commit -m "feat: chunk detail pane pairing a chunk with its source lines"
```

---

### Task 10: The search workspace

**Files:**
- Modify: `src/web/templates/search.html` (rail + pane)
- Modify: `src/web/templates/_results.html` (rail entries)
- Modify: `src/web/ui.rs` (`RenderedResult` gains `chunk_id` and a plain-text snippet; `search_page` accepts a query)
- Modify: `assets/app.css` (workspace grid, rail)
- Test: `src/web/ui.rs` tests

**Interfaces:**
- Consumes: `Core::search`, `build_chunk_detail`.
- Produces: `RenderedResult.chunk_id: String`, `RenderedResult.snippet: String`; `GET /ui/search?q=…&chunk=…` renders rail and pane server-side.

- [ ] **Step 1: Write the failing test**

Add to `src/web/ui.rs` tests:

```rust
    #[tokio::test]
    async fn a_rail_entry_carries_the_chunk_id_it_links_to() {
        let core = crate::core::test_support::test_core().await;
        let out = core.ingest("alpha line\n\nbravo line", "web", None).await.unwrap();
        crate::jobs::segment::run(&core, &out.id).await.unwrap();
        crate::jobs::embed::run_source(&core, &out.id).await.unwrap();

        let hits = core
            .search(&crate::core::search::SearchQuery {
                q: "alpha".into(),
                limit: 0,
                tags: vec![],
                category: None,
            })
            .await
            .unwrap();
        let r = super::render_hit(0, hits[0].clone());

        assert!(!r.chunk_id.is_empty(), "the rail needs a chunk id to link to");
        assert!(!r.snippet.is_empty(), "the rail shows a plain-text snippet");
        assert!(!r.snippet.contains('<'), "the snippet must not carry markup");
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test --lib ui::tests::a_rail_entry 2>&1 | tail -8`
Expected: FAIL — `RenderedResult` has no field `chunk_id`.

- [ ] **Step 3: Extend the result view model**

In `src/web/ui.rs`:

```rust
pub struct RenderedResult {
    pub chunk_id: String,
    pub title: String,
    /// Sanitized HTML from `markdown::render`. Rendered with `|safe`.
    pub html: String,
    /// Markup-free preview for the rail, where rendered HTML would not fit.
    pub snippet: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub source_id: String,
    pub rank: String,
}
```

```rust
fn render_hit(position: usize, h: crate::core::search::SearchResult) -> RenderedResult {
    RenderedResult {
        chunk_id: h.chunk_id,
        title: h.title.unwrap_or_else(|| "Untitled".into()),
        html: markdown::render(&h.text),
        snippet: markdown::snippet(&h.text, 140),
        category: h.category,
        tags: h.tags,
        source_id: h.source_id,
        rank: format!("#{}", position + 1),
    }
}
```

`_answer.html` also renders `RenderedResult`; it needs no change, but check it still compiles.

- [ ] **Step 4: Make the search page a workspace**

Replace `src/web/templates/search.html`:

```html
{% extends "layout.html" %}
{% block title %}Search — engram{% endblock %}
{% block content %}
<input class="input" type="search" name="q" placeholder="Search by meaning…" autofocus
       value="{{ q }}"
       hx-get="/ui/search/results" hx-target="#rail" hx-swap="innerHTML"
       hx-trigger="keyup changed delay:250ms, search"
       hx-indicator="#search-spinner">
<div class="row" style="margin:0.5rem 0 1rem">
  <span id="search-spinner" class="spinner">searching…</span>
  <span class="spacer"></span>
  <span id="timing" class="muted mono"></span>
</div>

<div class="workspace">
  <div id="rail" class="rail" role="listbox" aria-label="Results"></div>
  <div id="pane" class="pane">
    <p class="muted">Pick a result to see it beside the lines it came from.</p>
  </div>
</div>
{% endblock %}
```

`SearchTemplate` gains `q: String`, and `search_page` takes `Query(p): Query<UiSearchParams>` and passes `p.q` through, so a reload keeps the box filled.

Replace `src/web/templates/_results.html`:

```html
{% if results.is_empty() %}
  <p class="muted">No matches.</p>
{% else %}
  {% for r in results %}
  <a class="rail-item" role="option" href="/ui/chunks/{{ r.chunk_id }}"
     hx-get="/ui/chunks/{{ r.chunk_id }}" hx-target="#pane" hx-swap="innerHTML"
     hx-push-url="true">
    <div class="rail-head">
      <span class="rail-rank mono">{{ r.rank }}</span>
      <span class="rail-title">{{ r.title }}</span>
    </div>
    <div class="rail-snippet">{{ r.snippet }}</div>
    <div class="chips">
      {% if let Some(c) = r.category %}<span class="badge badge-accent">{{ c }}</span>{% endif %}
      {% for t in r.tags %}<span class="badge">{{ t }}</span>{% endfor %}
    </div>
  </a>
  {% endfor %}
{% endif %}
```

- [ ] **Step 5: Style the workspace**

Append to `assets/app.css`:

```css
.workspace { display: grid; grid-template-columns: 20rem 1fr; gap: 1rem; align-items: start; }
@media (max-width: 60rem) {
  .workspace { grid-template-columns: 1fr; }
  /* One region at a time on a narrow screen: the rail is the list, and
     opening a chunk replaces it. */
  .workspace.has-selection .rail { display: none; }
}

.rail { display: flex; flex-direction: column; gap: 0.5rem; max-height: 40rem; overflow-y: auto; }
.rail-item {
  display: block; text-decoration: none; color: inherit;
  background: var(--color-bg-elevated); border: 1px solid var(--color-border);
  border-radius: var(--radius-md); padding: 0.625rem 0.75rem;
}
.rail-item:hover { background: var(--color-bg-hover); }
.rail-item[aria-selected="true"] { border-color: var(--color-accent); background: var(--color-accent-dim); }
.rail-head { display: flex; gap: 0.5rem; align-items: baseline; }
.rail-rank { font-size: 0.75rem; color: var(--color-fg-muted); }
.rail-title { font-weight: 600; font-size: 0.875rem; }
.rail-snippet {
  font-size: 0.8125rem; color: var(--color-fg-secondary); margin-top: 0.25rem;
  display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden;
}
.pane { min-width: 0; }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/web assets/app.css
git commit -m "feat: search workspace with a ranked rail beside the chunk pane"
```

---

### Task 11: Highlighting, expanding and copying

**Files:**
- Create: `assets/app.js`
- Modify: `src/web/templates/layout.html` (load it)
- Modify: `src/web/ui.rs` (pass query terms to the rail)
- Modify: `src/web/templates/_results.html`, `_chunk_detail.html` (clamp wrapper, terms attribute)
- Modify: `assets/app.css` (clamp, copy button, `mark`)
- Test: `src/web/assets.rs` test module

**Interfaces:**
- Consumes: `vector::sparse::tokenize`.
- Produces: `ResultsTemplate.terms: String` (space-separated query terms); `assets/app.js` behaviours keyed on `data-terms`, `.clampable`, `[data-copyable]`.

- [ ] **Step 1: Write the failing asset tests**

Add to `src/web/assets.rs` tests:

```rust
    #[test]
    fn the_script_is_embedded_and_makes_no_external_requests() {
        let js = Assets::get("app.js").expect("app.js must be embedded");
        let js = std::str::from_utf8(js.data.as_ref()).unwrap();
        assert!(!js.contains("https://"), "external url in script");
        assert!(js.contains("data-terms"), "highlighting reads the terms attribute");
        assert!(js.contains("clipboard"), "copy buttons need the clipboard API");
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test --lib assets:: 2>&1 | tail -6`
Expected: FAIL — `app.js must be embedded`.

- [ ] **Step 3: Write the script**

Create `assets/app.js`:

```js
// Client-side because it must not touch the sanitized HTML on the server.
// Every function here walks text nodes only: it can wrap what is already
// rendered, and it can never introduce an element the sanitizer did not allow.
(function () {
  'use strict';

  function terms(root) {
    var raw = (root.getAttribute('data-terms') || '').trim();
    return raw ? raw.toLowerCase().split(/\s+/).filter(function (t) { return t.length > 1; }) : [];
  }

  function highlight(root) {
    var list = terms(root);
    if (!list.length) return;
    var walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    var nodes = [];
    while (walker.nextNode()) nodes.push(walker.currentNode);

    nodes.forEach(function (node) {
      if (node.parentNode && node.parentNode.tagName === 'MARK') return;
      var text = node.nodeValue;
      var lower = text.toLowerCase();
      var hits = [];
      list.forEach(function (term) {
        var from = 0, at;
        while ((at = lower.indexOf(term, from)) !== -1) {
          hits.push([at, at + term.length]);
          from = at + term.length;
        }
      });
      if (!hits.length) return;
      hits.sort(function (a, b) { return a[0] - b[0]; });

      var frag = document.createDocumentFragment();
      var cursor = 0;
      hits.forEach(function (h) {
        if (h[0] < cursor) return;
        frag.appendChild(document.createTextNode(text.slice(cursor, h[0])));
        var mark = document.createElement('mark');
        mark.textContent = text.slice(h[0], h[1]);
        frag.appendChild(mark);
        cursor = h[1];
      });
      frag.appendChild(document.createTextNode(text.slice(cursor)));
      node.parentNode.replaceChild(frag, node);
    });
  }

  // Clamping is visual only. The text is never truncated, so a fenced command
  // is never cut in half — expanding reveals what was always there.
  function clamp(root) {
    root.querySelectorAll('.clampable:not([data-clamped])').forEach(function (el) {
      el.setAttribute('data-clamped', 'yes');
      if (el.scrollHeight <= el.clientHeight + 4) return;
      el.classList.add('is-clamped');
      var btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'btn btn-ghost btn-sm expand';
      btn.textContent = 'Expand';
      btn.addEventListener('click', function () {
        var open = el.classList.toggle('is-clamped');
        btn.textContent = open ? 'Expand' : 'Collapse';
      });
      el.parentNode.insertBefore(btn, el.nextSibling);
    });
  }

  function copyButtons(root) {
    root.querySelectorAll('[data-copyable] pre').forEach(function (pre) {
      if (pre.parentNode.classList.contains('codewrap')) return;
      var wrap = document.createElement('div');
      wrap.className = 'codewrap';
      pre.parentNode.insertBefore(wrap, pre);
      wrap.appendChild(pre);

      var btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'copy';
      btn.textContent = 'copy';
      btn.addEventListener('click', function () {
        navigator.clipboard.writeText(pre.innerText).then(function () {
          btn.textContent = 'copied';
          setTimeout(function () { btn.textContent = 'copy'; }, 1200);
        });
      });
      wrap.appendChild(btn);
    });
  }

  function enhance(root) {
    if (!root || root.nodeType !== 1) return;
    highlight(root);
    clamp(root);
    copyButtons(root);
  }

  document.addEventListener('DOMContentLoaded', function () { enhance(document.body); });
  document.body.addEventListener('htmx:afterSwap', function (e) { enhance(e.target); });

  // Keyboard: the rail is a list, so arrows move through it and Enter opens.
  document.addEventListener('keydown', function (e) {
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
    var items = Array.prototype.slice.call(document.querySelectorAll('.rail-item'));
    if (!items.length) return;
    var active = document.activeElement;
    var i = items.indexOf(active);
    var next = e.key === 'ArrowDown' ? Math.min(i + 1, items.length - 1) : Math.max(i - 1, 0);
    if (i === -1) next = 0;
    items.forEach(function (el) { el.setAttribute('aria-selected', 'false'); });
    items[next].setAttribute('aria-selected', 'true');
    items[next].focus();
    e.preventDefault();
  });
})();
```

Load it in `src/web/templates/layout.html`, after htmx:

```html
  <script src="/assets/htmx.min.js" defer></script>
  <script src="/assets/app.js" defer></script>
```

- [ ] **Step 4: Pass the query terms and mark what clamps**

In `src/web/ui.rs`, add `terms: String` to `ResultsTemplate` and fill it in `search_results`:

```rust
    let terms = crate::vector::sparse::tokenize(p.q.trim()).join(" ");
```

(build the `SearchQuery` from a clone of `p.q` so the string is still available).

In `_results.html`, wrap the list:

```html
<div data-terms="{{ terms }}">
  … the existing loop …
</div>
```

and give the snippet div `class="rail-snippet"` as it already has — no clamp needed there, CSS handles it.

In `_chunk_detail.html`, add the clamp class to the chunk body and the terms attribute to the whole fragment:

```html
<div class="md clampable" data-copyable>{{ d.html|safe }}</div>
```

The pane inherits `data-terms` from the rail's ancestor only if it is nested, which it is not, so add `data-terms="{{ terms }}"` to `ChunkDetail` as well: `ChunkDetail.terms: String`, set from the `terms` query parameter of the detail request (`/ui/chunks/{id}?terms=dd+iso`), defaulting to empty. The rail links already carry `hx-get="/ui/chunks/{{ r.chunk_id }}?terms={{ terms|urlencode }}"`.

- [ ] **Step 5: Style the additions**

Append to `assets/app.css`:

```css
mark { background: var(--color-accent-dim); color: inherit; border-radius: 2px; padding: 0 1px; }
[data-theme="dark"] mark { background: var(--color-warning-dim); }

.clampable.is-clamped { max-height: 14rem; overflow: hidden; position: relative; }
.clampable.is-clamped::after {
  content: ""; position: absolute; left: 0; right: 0; bottom: 0; height: 3rem;
  background: linear-gradient(to bottom, transparent, var(--color-bg-elevated));
}

.codewrap { position: relative; }
.copy {
  position: absolute; top: 6px; right: 6px;
  font-family: var(--font-mono); font-size: 0.6875rem; cursor: pointer;
  padding: 3px 7px; border-radius: var(--radius-sm);
  background: var(--color-bg-elevated); color: var(--color-fg-secondary);
  border: 1px solid var(--color-border-strong);
}
.copy:hover { color: var(--color-fg-primary); background: var(--color-bg-hover); }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS, including the two pre-existing "no external urls" asset tests.

- [ ] **Step 7: Commit**

```bash
git add assets src/web
git commit -m "feat: highlight query terms, clamp long chunks, copy code blocks"
```

---

### Task 12: Typing costs less and stops draining resurface

**Files:**
- Modify: `src/core/mod.rs` (cache field, test builder)
- Modify: `src/core/search.rs` (`mark` flag, cached embedding, timing)
- Modify: `src/core/ask.rs`, `src/web/api.rs`, `src/web/ui.rs`, `src/mcp/*` (call sites)
- Test: `src/core/search.rs` test module

**Interfaces:**
- Consumes: `Embedder::embed`, `Store`, `FakeEmbedder::calls`.
- Produces:
  - `Core.query_cache: Arc<Mutex<QueryCache>>` with `QueryCache::get(&mut self, key: &str) -> Option<Vec<f32>>` and `QueryCache::put(&mut self, key: String, v: Vec<f32>)`, capacity 256
  - `SearchQuery.mark: bool` (defaults false through `#[serde(default)]`)
  - `Core::search_capped(&self, query: &SearchQuery, cap: Option<usize>) -> Result<Vec<SearchResult>>` — unchanged signature, honours `query.mark`
  - `Core::last_timing() -> (u128, u128)` is **not** added; timing rides on a new `SearchTiming` returned by `Core::search_timed(&self, query: &SearchQuery) -> Result<(Vec<SearchResult>, SearchTiming)>` with `SearchTiming { embed_ms: u128, total_ms: u128 }`

- [ ] **Step 1: Write the failing tests**

Add to `src/core/search.rs` tests:

```rust
    #[tokio::test]
    async fn an_identical_query_is_embedded_once() {
        let (core, embedder) = crate::core::test_support::test_core_counting_embed_calls().await;
        seed(&core, &[("a", "alpha text", &[])]).await;

        let q = |s: &str| SearchQuery {
            q: s.to_string(),
            limit: 0,
            tags: vec![],
            category: None,
            mark: false,
        };
        core.search(&q("dd write iso")).await.unwrap();
        let after_first = embedder.calls();
        core.search(&q("dd write iso")).await.unwrap();
        // Whitespace differences are not a different question.
        core.search(&q("  dd write iso  ")).await.unwrap();

        assert_eq!(embedder.calls(), after_first, "the query embedding must be cached");
    }

    #[tokio::test]
    async fn an_unmarked_search_does_not_stamp_last_seen() {
        let core = test_core().await;
        seed(&core, &[("a", "alpha text", &[])]).await;

        core.search(&SearchQuery {
            q: "alpha".into(),
            limit: 0,
            tags: vec![],
            category: None,
            mark: false,
        })
        .await
        .unwrap();
        core.background.drain().await;

        // Everything is old enough and unseen, so resurface still offers it.
        let old = crate::core::search::FORGOTTEN_AFTER_DAYS;
        assert!(old > 0);
        let stamped = core
            .vectors
            .resurface(10, i64::MAX, i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .filter(|h| h.payload.last_seen_at.is_some())
            .count();
        assert_eq!(stamped, 0, "typing must not stamp last_seen_at");
    }

    #[tokio::test]
    async fn a_marked_search_records_what_it_showed() {
        let core = test_core().await;
        seed(&core, &[("a", "alpha text", &[])]).await;

        core.search(&SearchQuery {
            q: "alpha".into(),
            limit: 0,
            tags: vec![],
            category: None,
            mark: true,
        })
        .await
        .unwrap();
        core.background.drain().await;

        let stamped = core
            .vectors
            .resurface(10, i64::MAX, i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .filter(|h| h.payload.last_seen_at.is_some())
            .count();
        assert!(stamped > 0, "a deliberate search still counts as seeing");
    }
```

(If `Background` has no `drain`, use whatever the existing tests use to wait for background writes — `src/core/search.rs:568` shows the pattern in use today.)

- [ ] **Step 2: Run them**

Run: `cargo test --lib core::search 2>&1 | tail -15`
Expected: FAIL — `SearchQuery` has no field `mark`; the embed call count doubles.

- [ ] **Step 3: Add the cache**

In `src/core/mod.rs`:

```rust
/// Bounded cache of query embeddings.
///
/// Search-as-you-type asks for `d`, `dd`, `dd i`, `dd if` inside one search,
/// and the same questions come back across sessions. Each of those is a remote
/// call before the vector store is touched at all, which for a local embedder
/// is the dominant term in the latency the user feels.
///
/// Insertion-ordered rather than true LRU: at this size the difference does not
/// pay for the bookkeeping.
pub struct QueryCache {
    capacity: usize,
    entries: std::collections::VecDeque<(String, Vec<f32>)>,
}

impl QueryCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: std::collections::VecDeque::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<Vec<f32>> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    pub fn put(&mut self, key: String, value: Vec<f32>) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back((key, value));
    }
}

pub const QUERY_CACHE_CAPACITY: usize = 256;
```

Add to `Core`:

```rust
    /// Shared by every clone of `Core`, like the background queue.
    pub query_cache: Arc<std::sync::Mutex<QueryCache>>,
```

Construct it wherever `Core` is built — the real assembly in `src/lib.rs` or `src/main.rs`, and `test_support::build`:

```rust
            query_cache: Arc::new(std::sync::Mutex::new(QueryCache::new(QUERY_CACHE_CAPACITY))),
```

- [ ] **Step 4: Use it, and make marking explicit**

In `src/core/search.rs`:

```rust
pub struct SearchQuery {
    pub q: String,
    #[serde(default)]
    pub limit: usize,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub category: Option<String>,
    /// Whether this search counts as having *seen* its results.
    ///
    /// Incremental UI requests pass false. Every keystroke used to stamp
    /// `last_seen_at` on whatever the prefix happened to match, which is the
    /// same field `resurface` reads — so typing quietly drained the
    /// forgotten-chunk feature. Opening, expanding and submitting pass true,
    /// and so do the API and MCP paths, which are deliberate by construction.
    #[serde(default)]
    pub mark: bool,
}
```

```rust
#[derive(Debug, Clone, Copy)]
pub struct SearchTiming {
    pub embed_ms: u128,
    pub total_ms: u128,
}
```

Inside `search_capped`, replace the embedding call:

```rust
        let key = query.q.split_whitespace().collect::<Vec<_>>().join(" ");
        let cached = self
            .query_cache
            .lock()
            .ok()
            .and_then(|c| c.get(&key));
        let started = std::time::Instant::now();
        let vector = match cached {
            Some(v) => v,
            None => {
                let v = self.embedder.embed(&[query.q.trim().to_string()]).await?.remove(0);
                if let Ok(mut c) = self.query_cache.lock() {
                    c.put(key, v.clone());
                }
                v
            }
        };
        let embed_ms = started.elapsed().as_millis();
```

and use `&vector` where `&vectors[0]` was used. Replace the unconditional stamp:

```rust
        if query.mark {
            self.mark_seen(&results);
        }
```

Add the timed variant used by the UI:

```rust
    /// Same search, plus what it cost. The UI shows these faintly so a sluggish
    /// box points at the embedder or the vector store without opening logs.
    pub async fn search_timed(
        &self,
        query: &SearchQuery,
    ) -> Result<(Vec<SearchResult>, SearchTiming)> {
        let started = std::time::Instant::now();
        let embed_started = std::time::Instant::now();
        let results = self.search(query).await?;
        let _ = embed_started;
        Ok((
            results,
            SearchTiming {
                embed_ms: 0,
                total_ms: started.elapsed().as_millis(),
            },
        ))
    }
```

Replace that stub with a real one: make `search_capped` return the timing internally by extracting its body into `search_inner(&self, query, cap) -> Result<(Vec<SearchResult>, SearchTiming)>`, have `search`, `search_capped` and `search_timed` all call it, and delete the stub above. Do not ship a timing field that is always zero.

Update every call site to set `mark`:
- `src/web/ui.rs::search_results` → `mark: false`
- `src/web/ui.rs::chunk_detail` → after building the detail, call `st.core.mark_chunk_seen(&cid)`; add that one-liner to `Core`:

```rust
    /// Opening a chunk is the deliberate act that counts as remembering it.
    pub fn mark_chunk_seen(&self, chunk_id: &str) {
        let ids = vec![chunk_id.to_string()];
        let vectors = self.vectors.clone();
        let now = now_secs();
        self.background.spawn(async move {
            if let Err(e) = vectors.touch(&ids, now).await {
                tracing::warn!(error = %e, "could not record that a chunk was opened");
            }
        });
    }
```

- `src/web/api.rs` search handler → `mark: true`
- `src/mcp` search tool → `mark: true`
- `src/core/ask.rs` → `mark: true`

- [ ] **Step 5: Show the timing**

In `search_results`, use `search_timed` and add `timing: String` to `ResultsTemplate`:

```rust
    let (hits, t) = st.core.search_timed(&q).await?;
    let timing = format!("embed {}ms · total {}ms", t.embed_ms, t.total_ms);
```

In `_results.html`, at the top of the fragment:

```html
<div id="timing-value" hx-swap-oob="innerHTML:#timing">{{ timing }}</div>
```

- [ ] **Step 6: Run the whole suite**

Run: `cargo test && cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 7: Commit**

```bash
git add src
git commit -m "feat: cache query embeddings and stop typing from draining resurface"
```

---

### Task 13: Documentation

**Files:**
- Modify: `README.md`
- Modify: `ROADMAP.md`

- [ ] **Step 1: Update the README**

Under "Inference roles", after the paragraph beginning "Ingest never calls inference", add:

```markdown
Segmentation runs one window at a time and remembers where it got to. A window
that succeeds is written before the next is attempted, so a retry resumes
rather than re-paying for the windows that already worked, and a window the
chunker cannot handle is split structurally on its own lines while the rest
keep their LLM segmentation. That source is reported `partial`, and Ops names
the window.

Each window is checked before its chunks are stored. Commands, paths and flags
in a chunk must appear in the window it came from; if they do not, the window
is segmented once more and, failing that, the chunk is stored with a flag
naming the literal that went missing. Spans are checked the same way and
clamped to their window when they are implausible. Per source, the fraction of
lines that ended up inside some chunk is recorded and shown on Browse — a
source where the segmenter dropped half a chapter no longer looks like one
where it did not.
```

Under "How search works", after the "Result scores" paragraph:

```markdown
The search page keeps the ranked list beside the result. Opening a hit fills a
detail pane with the chunk and the source lines it claims, so a paraphrase is
visible without leaving the page; `/ui/chunks/{id}` is the same view as a
standalone page, for links and new tabs.

Typing is cheap: query embeddings are cached, so a burst of keystrokes costs
one embedding call rather than one per prefix. Incremental searches do not
record what they showed — only opening, expanding or submitting does, which is
what keeps `resurface` meaningful.
```

- [ ] **Step 2: Prune the roadmap**

In `ROADMAP.md`, delete the bullets this branch implements: "Chunk detail view", "Snippet and expand", "Query term highlighting", "Copy the command", "Query embedding cache", "Do not mark partial queries as seen", "Latency budget in the response", "Literal verification", "Span verification", "Coverage report". Add under **Recall surface**:

```markdown
- **Tag and category controls.** Still API-only. The workspace has room for
  chips beside the search box; they should be built from facet counts.
```

and under a new **Ingest** heading:

```markdown
- **File upload and PDF.** The detail pane asks a `SourceView` for the lines a
  chunk claims. A PDF source implements the same trait — extracted text, a page
  map, `page 42` as the label — and the pane needs no changes. Upload comes
  first: the body limit is explicit now, at 8 MB.
```

- [ ] **Step 3: Verify the docs match the code**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS. Then read both files once for claims the branch did not deliver.

- [ ] **Step 4: Commit**

```bash
git add README.md ROADMAP.md
git commit -m "docs: describe per-window segmentation, verification and the workspace"
```

---

## Self-Review

**Spec coverage**

| Spec section | Task |
|---|---|
| §2 window rows, resume, narrower replace, one embed job | 1, 2 |
| §2 per-window failure, `partial` status | 3 |
| §2 progress, capture guidance, 8 MB body limit | 6 |
| §3 literal check, one retry, flag | 4 |
| §3 span check and clamping | 5 |
| §3 coverage and the 0.6 threshold | 5, 6 |
| §3 surfacing: badges, banner, Ops list, re-segment, mark reviewed | 7, 9 |
| §4 rail and pane, routing, fragment vs page | 9, 10 |
| §4 source view seam | 8 |
| §4 snippet clamp, copy, highlighting, narrow screens | 10, 11 |
| §5 embedding cache, `mark` flag, latency line | 12 |
| §6 testing | every task's test steps |

**Placeholder scan:** none — every code step carries the code, every test step the test.

**Type consistency check:** `NewChunk.window_idx: Option<i64>` is introduced in Task 1 and used in Tasks 2 and 3. `write_window_chunks` returns `Result<()>` in Task 2 and is deliberately widened to `Result<Vec<Chunk>>` in Task 4 — Task 4 states the change and Task 3's caller is told to bind `let _ =`. `flag_unverified_literals` (Task 4) is renamed `flag_unverified` in Task 5 with the wider signature, also stated. `SearchTiming` is introduced in Task 12 and consumed only there. `SourceLine` / `SourceSlice` are defined in Task 8 and consumed in Task 9.
