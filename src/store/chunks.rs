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
}

#[derive(Debug, Clone)]
pub struct NewChunk {
    pub ordinal: i64,
    pub text: String,
    pub source_span: Option<SourceSpan>,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
}

fn row_to_chunk(r: &sqlx::sqlite::SqliteRow) -> Chunk {
    let tags_json: String = r.get("tags");
    let span_json: Option<String> = r.get("source_span");
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
            };
            sqlx::query(
                "INSERT INTO chunks (id, source_id, ordinal, text, source_span, title, category, tags, embed_state, embed_model, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?)",
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
    pub async fn reset_embed_state(&self, source_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE chunks SET embed_state = 'pending', embed_model = NULL WHERE source_id = ?",
        )
        .bind(source_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update the fields the embedding model never sees. Deliberately does not
    /// touch `embed_state`: the stored vector is still correct.
    pub async fn update_chunk_meta(
        &self,
        id: &str,
        category: Option<&str>,
        tags: Option<&[String]>,
    ) -> Result<()> {
        if let Some(c) = category {
            sqlx::query("UPDATE chunks SET category = ? WHERE id = ?")
                .bind(c)
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        if let Some(t) = tags {
            sqlx::query("UPDATE chunks SET tags = ? WHERE id = ?")
                .bind(serde_json::to_string(t).unwrap_or_else(|_| "[]".into()))
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// The title is part of the text handed to the embedder, so changing it
    /// invalidates the vector the same way changing the body does.
    pub async fn update_chunk_title(&self, id: &str, title: &str) -> Result<()> {
        sqlx::query(
            "UPDATE chunks SET title = ?, embed_state = 'pending', embed_model = NULL WHERE id = ?",
        )
        .bind(title)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update_chunk_text(&self, id: &str, text: &str) -> Result<()> {
        let res = sqlx::query(
            "UPDATE chunks SET text = ?, embed_state = 'pending', embed_model = NULL WHERE id = ?",
        )
        .bind(text)
        .bind(id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    pub async fn mark_embedded(&self, id: &str, model: &str) -> Result<()> {
        sqlx::query("UPDATE chunks SET embed_state = 'embedded', embed_model = ? WHERE id = ?")
            .bind(model)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
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
        }
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
        s.mark_embedded(&c.id, "bge-m3").await.unwrap();
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

        s.mark_embedded(&made[0].id, "m").await.unwrap();
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
