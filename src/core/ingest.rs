use super::Core;
use crate::error::{Error, Result};
use crate::store::jobs::Stage;
use crate::store::sources::{SourceStatus, content_hash};

#[derive(Debug, Clone, serde::Serialize)]
pub struct IngestOutcome {
    pub id: String,
    pub status: SourceStatus,
    /// True when the text was already stored and no new source was created.
    pub duplicate: bool,
}

impl Core {
    /// Store the text and queue processing. Deliberately makes no inference
    /// call: capture must stay instant and must survive a dead endpoint.
    pub async fn ingest(
        &self,
        text: &str,
        origin: &str,
        title_hint: Option<&str>,
    ) -> Result<IngestOutcome> {
        if text.trim().is_empty() {
            return Err(Error::Validation("text is empty".into()));
        }

        if let Some(existing) = self.store.find_by_hash(&content_hash(text)).await? {
            tracing::info!(source_id = %existing.id, "duplicate ingest, returning existing source");
            return Ok(IngestOutcome {
                id: existing.id,
                status: existing.status,
                duplicate: true,
            });
        }

        let src = self.store.insert_source(text, origin, title_hint).await?;
        self.store
            .enqueue(Stage::Segment, "source", &src.id)
            .await?;
        tracing::info!(source_id = %src.id, origin, bytes = text.len(), "ingested");
        Ok(IngestOutcome {
            id: src.id,
            status: SourceStatus::Raw,
            duplicate: false,
        })
    }

    /// Vectors first: an orphaned row is invisible, but an orphaned vector is
    /// still returned by search.
    pub async fn delete_source(&self, id: &str) -> Result<()> {
        self.store.get_source(id).await?;
        self.vectors.delete_by_source(id).await?;
        self.store.delete_source(id).await?;
        tracing::info!(source_id = %id, "deleted source and its vectors");
        Ok(())
    }

    pub async fn reprocess(&self, id: &str, stage: Stage) -> Result<()> {
        let src = self.store.get_source(id).await?;
        match stage {
            Stage::Segment | Stage::Enrich => {
                // Re-segmenting replaces every chunk, so the old vectors and
                // rows go first.
                self.vectors.delete_by_source(&src.id).await?;
                for c in self.store.chunks_for_source(&src.id).await? {
                    self.store.delete_chunk(&c.id).await?;
                }
                self.store
                    .set_source_status(&src.id, SourceStatus::Raw)
                    .await?;
                self.store
                    .enqueue(Stage::Segment, "source", &src.id)
                    .await?;
            }
            Stage::Embed => {
                self.store.reset_embed_state(&src.id).await?;
                self.store.enqueue(Stage::Embed, "source", &src.id).await?;
                self.store
                    .set_source_status(&src.id, SourceStatus::Embedding)
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::core::test_support::{test_core, test_core_with_failing_chunker};
    use crate::store::jobs::Stage;
    use crate::store::sources::SourceStatus;

    #[tokio::test]
    async fn ingest_returns_immediately_and_enqueues_segmentation() {
        let core = test_core().await;
        let out = core
            .ingest("a procedure", "web", Some("title"))
            .await
            .unwrap();
        assert_eq!(out.status, SourceStatus::Raw);
        assert!(!out.duplicate);

        let src = core.store.get_source(&out.id).await.unwrap();
        assert_eq!(src.raw_text, "a procedure");
        assert!(
            core.store.claim_job().await.unwrap().is_some(),
            "segment job was not enqueued"
        );
    }

    #[tokio::test]
    async fn ingest_is_not_blocked_by_a_dead_chunker() {
        // The whole point of deferred processing: a broken inference endpoint
        // must not turn into a failed capture.
        let core = test_core_with_failing_chunker().await;
        let out = core.ingest("still accepted", "web", None).await.unwrap();
        assert_eq!(out.status, SourceStatus::Raw);
    }

    #[tokio::test]
    async fn identical_text_returns_the_existing_source() {
        let core = test_core().await;
        let a = core.ingest("same words", "web", None).await.unwrap();
        let b = core.ingest("same words", "mcp", None).await.unwrap();
        assert_eq!(a.id, b.id);
        assert!(b.duplicate);
        assert_eq!(core.store.list_sources(10, 0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn empty_text_is_rejected() {
        let core = test_core().await;
        assert!(matches!(
            core.ingest("   \n ", "web", None).await,
            Err(crate::error::Error::Validation(_))
        ));
    }

    #[tokio::test]
    async fn deleting_a_source_also_deletes_its_vectors() {
        let core = test_core().await;
        let out = core.ingest("some text", "web", None).await.unwrap();
        let chunks = core
            .store
            .insert_chunks(
                &out.id,
                &[crate::store::chunks::NewChunk {
                    ordinal: 0,
                    text: "t".into(),
                    source_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    window_idx: None,
                }],
            )
            .await
            .unwrap();
        core.vectors
            .upsert(vec![crate::vector::VectorPoint {
                sparse: Default::default(),
                vector: vec![0.1; 8],
                payload: crate::vector::VectorPayload {
                    chunk_id: chunks[0].id.clone(),
                    source_id: out.id.clone(),
                    text: "t".into(),
                    title: None,
                    category: None,
                    tags: vec![],
                    created_at: 0,
                    last_seen_at: None,
                },
            }])
            .await
            .unwrap();

        core.delete_source(&out.id).await.unwrap();
        assert_eq!(
            core.vectors.count().await.unwrap(),
            0,
            "orphaned vectors would still be searchable"
        );
        assert!(matches!(
            core.store.get_source(&out.id).await,
            Err(crate::error::Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn deleting_a_missing_source_is_not_found() {
        let core = test_core().await;
        assert!(matches!(
            core.delete_source("nope").await,
            Err(crate::error::Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn reprocess_requeues_the_requested_stage() {
        let core = test_core().await;
        let out = core.ingest("text", "web", None).await.unwrap();
        core.store.claim_job().await.unwrap(); // drain the initial segment job
        core.reprocess(&out.id, Stage::Segment).await.unwrap();
        let j = core.store.claim_job().await.unwrap().unwrap();
        assert_eq!(j.target_id, out.id);
        assert_eq!(
            j.attempts, 1,
            "reprocess must start from a clean attempt count"
        );
    }
}
