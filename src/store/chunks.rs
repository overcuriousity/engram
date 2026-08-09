use super::{Store, new_id, now};
use crate::error::{Error, Result};
use sqlx::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbedState {
    Pending,
    Embedded,
    Failed,
}

impl EmbedState {
    pub fn as_str(&self) -> &'static str {
        match self {
            EmbedState::Pending => "pending",
            EmbedState::Embedded => "embedded",
            EmbedState::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> EmbedState {
        match s {
            "embedded" => EmbedState::Embedded,
            "failed" => EmbedState::Failed,
            _ => EmbedState::Pending,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SourceSpan {
    pub start_line: i64,
    pub end_line: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Chunk {
    pub id: String,
    pub source_id: String,
    pub ordinal: i64,
    pub text: String,
    pub source_span: Option<SourceSpan>,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub embed_state: EmbedState,
    pub embed_model: Option<String>,
    pub created_at: i64,
    /// Bumped by every edit that invalidates the stored vector. Internal
    /// bookkeeping between the editor and the embed job, so it is not part of
    /// what the API hands out.
    #[serde(skip)]
    pub embed_rev: i64,
    /// Which segmentation window produced this chunk. `None` for chunks
    /// written before per-window segmentation existed.
    pub window_idx: Option<i64>,
    /// Verification failures. Empty means every check passed.
    pub flags: Vec<String>,
    pub flag_detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewChunk {
    pub ordinal: i64,
    pub text: String,
    pub source_span: Option<SourceSpan>,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub window_idx: Option<i64>,
}

fn row_to_chunk(r: &sqlx::sqlite::SqliteRow) -> Chunk {
    let tags_json: String = r.get("tags");
    let span_json: Option<String> = r.get("source_span");
    let flags_json: Option<String> = r.get("flags");
    Chunk {
        id: r.get("id"),
        source_id: r.get("source_id"),
        ordinal: r.get("ordinal"),
        text: r.get("text"),
        source_span: span_json.and_then(|s| serde_json::from_str(&s).ok()),
        title: r.get("title"),
        category: r.get("category"),
        tags: serde_json::from_str(&tags_json).unwrap_or_default(),
        embed_state: EmbedState::parse(r.get::<String, _>("embed_state").as_str()),
        embed_model: r.get("embed_model"),
        created_at: r.get("created_at"),
        embed_rev: r.get("embed_rev"),
        window_idx: r.get("window_idx"),
        flags: flags_json
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        flag_detail: r.get("flag_detail"),
    }
}

impl Store {
    pub async fn insert_chunks(&self, source_id: &str, chunks: &[NewChunk]) -> Result<Vec<Chunk>> {
        let mut tx = self.pool.begin().await?;
        let mut out = Vec::with_capacity(chunks.len());
        for nc in chunks {
            let c = Chunk {
                id: new_id(),
                source_id: source_id.to_string(),
                ordinal: nc.ordinal,
                text: nc.text.clone(),
                source_span: nc.source_span.clone(),
                title: nc.title.clone(),
                category: nc.category.clone(),
                tags: nc.tags.clone(),
                embed_state: EmbedState::Pending,
                embed_model: None,
                created_at: now(),
                embed_rev: 0,
                window_idx: nc.window_idx,
                flags: vec![],
                flag_detail: None,
            };
            sqlx::query(
                "INSERT INTO chunks (id, source_id, ordinal, text, source_span, title, category, tags, embed_state, embed_model, created_at, window_idx)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)",
            )
            .bind(&c.id)
            .bind(&c.source_id)
            .bind(c.ordinal)
            .bind(&c.text)
            .bind(c.source_span.as_ref().map(|s| serde_json::to_string(s).unwrap()))
            .bind(&c.title)
            .bind(&c.category)
            .bind(serde_json::to_string(&c.tags).unwrap())
            .bind(c.embed_state.as_str())
            .bind(c.created_at)
            .bind(c.window_idx)
            .execute(&mut *tx)
            .await?;
            out.push(c);
        }
        tx.commit().await?;
        Ok(out)
    }

    pub async fn get_chunk(&self, id: &str) -> Result<Chunk> {
        let row = sqlx::query("SELECT * FROM chunks WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound)?;
        Ok(row_to_chunk(&row))
    }

    pub async fn chunks_for_source(&self, source_id: &str) -> Result<Vec<Chunk>> {
        let rows = sqlx::query("SELECT * FROM chunks WHERE source_id = ? ORDER BY ordinal")
            .bind(source_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_chunk).collect())
    }

    /// Chunks of a source still waiting for a vector. The embed job batches
    /// these into one inference call, so it needs them as rows, not a count.
    pub async fn pending_chunks_for_source(&self, source_id: &str) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT * FROM chunks WHERE source_id = ? AND embed_state = 'pending' ORDER BY ordinal",
        )
        .bind(source_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_chunk).collect())
    }

    /// Put every chunk of a source back in the embed queue's path. Re-embedding
    /// only happens for rows that say they still need it, so asking for it has
    /// to say so first.
    ///
    /// The revision bump is what makes this safe to run while a worker is
    /// mid-batch on the same source: that worker's `mark_embedded` no longer
    /// matches, so it cannot clear the pending state this just set.
    pub async fn reset_embed_state(&self, source_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE chunks
             SET embed_state = 'pending', embed_model = NULL, embed_rev = embed_rev + 1
             WHERE source_id = ?",
        )
        .bind(source_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set or clear the category. Deliberately does not touch `embed_state`:
    /// the embedding model is never shown a category, so the stored vector is
    /// still correct.
    pub async fn update_chunk_category(&self, id: &str, category: Option<&str>) -> Result<()> {
        self.expect_updated(
            sqlx::query("UPDATE chunks SET category = ? WHERE id = ?")
                .bind(category)
                .bind(id)
                .execute(&self.pool)
                .await?,
        )
    }

    /// Replace the tag list. An empty list is a clear, not a no-op.
    pub async fn update_chunk_tags(&self, id: &str, tags: &[String]) -> Result<()> {
        self.expect_updated(
            sqlx::query("UPDATE chunks SET tags = ? WHERE id = ?")
                .bind(serde_json::to_string(tags).unwrap_or_else(|_| "[]".into()))
                .bind(id)
                .execute(&self.pool)
                .await?,
        )
    }

    /// The title is part of the text handed to the embedder, so setting or
    /// clearing it invalidates the vector the same way changing the body does.
    pub async fn update_chunk_title(&self, id: &str, title: Option<&str>) -> Result<()> {
        self.expect_updated(
            sqlx::query(
                "UPDATE chunks
                 SET title = ?, embed_state = 'pending', embed_model = NULL,
                     embed_rev = embed_rev + 1
                 WHERE id = ?",
            )
            .bind(title)
            .bind(id)
            .execute(&self.pool)
            .await?,
        )
    }

    pub async fn update_chunk_text(&self, id: &str, text: &str) -> Result<()> {
        self.expect_updated(
            sqlx::query(
                "UPDATE chunks
                 SET text = ?, embed_state = 'pending', embed_model = NULL,
                     embed_rev = embed_rev + 1
                 WHERE id = ?",
            )
            .bind(text)
            .bind(id)
            .execute(&self.pool)
            .await?,
        )
    }

    fn expect_updated(&self, res: sqlx::sqlite::SqliteQueryResult) -> Result<()> {
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Report a chunk indexed, but only if it has not been edited since the
    /// embed job read it.
    ///
    /// Returns whether the mark landed. `false` means a newer revision exists
    /// and the vector just written describes text that is already stale; the
    /// chunk stays pending, so it will be embedded again from the current row.
    ///
    /// That relies on an invariant worth keeping: whoever bumps the revision
    /// also queues the work. `update_chunk_text`, `update_chunk_title` and
    /// `reset_embed_state` are only ever called alongside an `enqueue`, so a
    /// chunk left pending here always has a job coming for it.
    pub async fn mark_embedded(&self, id: &str, model: &str, rev: i64) -> Result<bool> {
        let res = sqlx::query(
            "UPDATE chunks SET embed_state = 'embedded', embed_model = ?
             WHERE id = ? AND embed_rev = ?",
        )
        .bind(model)
        .bind(id)
        .bind(rev)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn mark_embed_failed(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE chunks SET embed_state = 'failed' WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_chunk(&self, id: &str) -> Result<()> {
        let res = sqlx::query("DELETE FROM chunks WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// The chunks a window's next write replaces.
    ///
    /// Chunks with no window at all are included, because a source segmented
    /// before windows existed has nothing else to key on: leaving them out
    /// would append the new segmentation beside the old one instead of
    /// replacing it. They are swept by whichever window writes first, and there
    /// are none left by the second.
    pub async fn chunk_ids_for_window(
        &self,
        source_id: &str,
        window_idx: i64,
    ) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT id FROM chunks WHERE source_id = ?
               AND (window_idx = ? OR window_idx IS NULL)
             ORDER BY ordinal",
        )
        .bind(source_id)
        .bind(window_idx)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get("id")).collect())
    }

    /// Open a gap of `by` ordinals after `ordinal`, so chunks inserted into it
    /// keep reading order without renumbering the whole source.
    pub async fn make_room_after(&self, source_id: &str, ordinal: i64, by: i64) -> Result<()> {
        sqlx::query("UPDATE chunks SET ordinal = ordinal + ? WHERE source_id = ? AND ordinal > ?")
            .bind(by)
            .bind(source_id)
            .bind(ordinal)
            .execute(&self.pool)
            .await?;
        Ok(())
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

    /// Record what verification found. An empty list clears the flags, so a
    /// re-checked chunk does not keep a warning it no longer earns.
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

    async fn count_by_embed_state(&self, source_id: &str, state: &str) -> Result<i64> {
        let row =
            sqlx::query("SELECT COUNT(*) AS n FROM chunks WHERE source_id = ? AND embed_state = ?")
                .bind(source_id)
                .bind(state)
                .fetch_one(&self.pool)
                .await?;
        Ok(row.get("n"))
    }

    pub async fn pending_embed_count(&self, source_id: &str) -> Result<i64> {
        self.count_by_embed_state(source_id, "pending").await
    }

    pub async fn failed_embed_count(&self, source_id: &str) -> Result<i64> {
        self.count_by_embed_state(source_id, "failed").await
    }

    /// Exact-token search over chunk text, titles and tags. Vector search is
    /// the default path; this exists for error codes, CLI flags and paths,
    /// which embeddings match poorly.
    pub async fn keyword_search(&self, query: &str, limit: i64) -> Result<Vec<Chunk>> {
        let sanitized = fts_quote(query);
        if sanitized.is_empty() {
            return Ok(vec![]);
        }
        let rows = sqlx::query(
            "SELECT c.* FROM chunks_fts f
             JOIN chunks c ON c.rowid = f.rowid
             WHERE chunks_fts MATCH ?
             ORDER BY bm25(chunks_fts) LIMIT ?",
        )
        .bind(&sanitized)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_chunk).collect())
    }
}

/// FTS5 has its own query grammar, and user input is not written in it. Each
/// whitespace-separated term is wrapped as a quoted phrase so stray operators
/// and quotes become literal text instead of syntax errors.
fn fts_quote(query: &str) -> String {
    query
        .split_whitespace()
        .map(|t| t.replace('"', ""))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn nc(ord: i64, text: &str) -> NewChunk {
        NewChunk {
            ordinal: ord,
            text: text.to_string(),
            source_span: Some(SourceSpan {
                start_line: 1,
                end_line: 4,
            }),
            title: Some(format!("title {ord}")),
            category: Some("procedure".into()),
            tags: vec!["forensics".into(), "windows".into()],
            window_idx: None,
        }
    }

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

    #[tokio::test]
    async fn insert_and_read_back_chunks() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        let made = s
            .insert_chunks(&src.id, &[nc(0, "## A\nfirst"), nc(1, "## B\nsecond")])
            .await
            .unwrap();
        assert_eq!(made.len(), 2);

        let got = s.chunks_for_source(&src.id).await.unwrap();
        assert_eq!(got[0].ordinal, 0);
        assert_eq!(got[1].text, "## B\nsecond");
        assert_eq!(
            got[0].tags,
            vec!["forensics".to_string(), "windows".to_string()]
        );
        assert_eq!(got[0].source_span.as_ref().unwrap().end_line, 4);
        assert_eq!(got[0].embed_state, EmbedState::Pending);
    }

    #[tokio::test]
    async fn deleting_a_source_cascades_to_its_chunks() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        s.insert_chunks(&src.id, &[nc(0, "x")]).await.unwrap();
        s.delete_source(&src.id).await.unwrap();
        assert!(s.chunks_for_source(&src.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn editing_text_resets_embed_state() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        let c = s
            .insert_chunks(&src.id, &[nc(0, "x")])
            .await
            .unwrap()
            .remove(0);
        assert!(s.mark_embedded(&c.id, "bge-m3", c.embed_rev).await.unwrap());
        assert_eq!(
            s.get_chunk(&c.id).await.unwrap().embed_state,
            EmbedState::Embedded
        );

        s.update_chunk_text(&c.id, "## x\nedited").await.unwrap();
        let after = s.get_chunk(&c.id).await.unwrap();
        assert_eq!(after.text, "## x\nedited");
        assert_eq!(
            after.embed_state,
            EmbedState::Pending,
            "edited text must not keep a stale vector"
        );
    }

    #[tokio::test]
    async fn counts_track_embed_progress() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        let made = s
            .insert_chunks(&src.id, &[nc(0, "a"), nc(1, "b")])
            .await
            .unwrap();
        assert_eq!(s.pending_embed_count(&src.id).await.unwrap(), 2);

        s.mark_embedded(&made[0].id, "m", made[0].embed_rev)
            .await
            .unwrap();
        s.mark_embed_failed(&made[1].id).await.unwrap();
        assert_eq!(s.pending_embed_count(&src.id).await.unwrap(), 0);
        assert_eq!(s.failed_embed_count(&src.id).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn fts_matches_exact_technical_strings() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        s.insert_chunks(
            &src.id,
            &[
                nc(0, "Run `robocopy /MIR` to mirror the tree."),
                nc(1, "Check the registry at HKLM\\SYSTEM\\CurrentControlSet."),
            ],
        )
        .await
        .unwrap();

        let hits = s.keyword_search("robocopy", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.contains("robocopy"));

        let hits = s.keyword_search("HKLM", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn fts_stays_in_sync_on_update_and_delete() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        let c = s
            .insert_chunks(&src.id, &[nc(0, "original vanishingword")])
            .await
            .unwrap()
            .remove(0);

        s.update_chunk_text(&c.id, "replaced entirely")
            .await
            .unwrap();
        assert!(
            s.keyword_search("vanishingword", 10)
                .await
                .unwrap()
                .is_empty(),
            "stale term still indexed after update"
        );
        assert_eq!(s.keyword_search("replaced", 10).await.unwrap().len(), 1);

        s.delete_chunk(&c.id).await.unwrap();
        assert!(s.keyword_search("replaced", 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fts_is_cleaned_up_when_a_source_cascades() {
        // SQLite only fires delete triggers for rows removed by a foreign-key
        // cascade when recursive_triggers is enabled. Without it the chunk rows
        // vanish but their FTS entries survive, and deleted text stays
        // searchable forever.
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        s.insert_chunks(&src.id, &[nc(0, "cascadingsentinel term")])
            .await
            .unwrap();
        assert_eq!(
            s.keyword_search("cascadingsentinel", 10)
                .await
                .unwrap()
                .len(),
            1
        );

        s.delete_source(&src.id).await.unwrap();

        // Assert against chunks_fts directly. `keyword_search` joins back to
        // `chunks`, so it would report an empty result even if the index still
        // held orphaned rows — that check cannot distinguish clean from stale.
        let orphans: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'cascadingsentinel'",
        )
        .fetch_one(&s.pool)
        .await
        .unwrap();
        assert_eq!(orphans, 0, "cascade left orphaned rows in the fts index");

        // FTS5's own consistency check. Orphaned entries in an external-content
        // index eventually surface as "database disk image is malformed".
        sqlx::query("INSERT INTO chunks_fts(chunks_fts) VALUES('integrity-check')")
            .execute(&s.pool)
            .await
            .expect("fts index failed its integrity check");
    }

    #[tokio::test]
    async fn fts_query_syntax_errors_do_not_crash() {
        let s = Store::memory().await.unwrap();
        // A bare quote is invalid FTS5 syntax; a user typing it must not 500.
        let hits = s.keyword_search("broken\" AND", 10).await.unwrap();
        assert!(hits.is_empty());
    }
}
