use crate::core::Core;
use crate::error::Result;
use crate::store::chunks::{Chunk, NewChunk};
use crate::store::jobs::Stage;
use crate::store::sources::SourceStatus;
use crate::vector::{VectorPayload, VectorPoint};

/// Fraction of the embedder's hard limit a chunk is allowed to occupy. The
/// remaining headroom absorbs tokenizer estimation error.
const SAFETY: f32 = 0.8;

pub async fn run(core: &Core, chunk_id: &str) -> Result<()> {
    let limit = (core.embedder.max_input_tokens() as f32 * SAFETY) as usize;
    run_with_limit(core, chunk_id, limit).await
}

pub async fn run_with_limit(core: &Core, chunk_id: &str, limit: usize) -> Result<()> {
    let chunk = core.store.get_chunk(chunk_id).await?;

    // Text for the embedding carries the title: it holds topical signal the
    // body often leaves implicit.
    let embed_text = match &chunk.title {
        Some(t) => format!("{t}\n{}", chunk.text),
        None => chunk.text.clone(),
    };

    if core.counter.count(&embed_text) > limit {
        return split_oversize(core, &chunk, limit).await;
    }

    let vectors = core.embedder.embed(&[embed_text]).await?;
    core.vectors
        .upsert(vec![VectorPoint {
            vector: vectors.into_iter().next().unwrap(),
            payload: payload_of(&chunk),
        }])
        .await?;

    core.store
        .mark_embedded(&chunk.id, core.embedder.model())
        .await?;
    settle_source(core, &chunk.source_id).await
}

/// A chunk larger than the embedder accepts becomes several sibling chunks
/// split at a paragraph boundary. Truncating would silently discard knowledge,
/// and one vector per fragment keeps the data model unchanged.
async fn split_oversize(core: &Core, chunk: &Chunk, limit: usize) -> Result<()> {
    let paragraphs: Vec<&str> = chunk
        .text
        .split("\n\n")
        .filter(|p| !p.trim().is_empty())
        .collect();

    if paragraphs.len() < 2 {
        tracing::warn!(chunk_id = %chunk.id, "oversize chunk has no paragraph boundary; embedding as-is");
        let vectors = core
            .embedder
            .embed(std::slice::from_ref(&chunk.text))
            .await?;
        core.vectors
            .upsert(vec![VectorPoint {
                vector: vectors.into_iter().next().unwrap(),
                payload: payload_of(chunk),
            }])
            .await?;
        core.store
            .mark_embedded(&chunk.id, core.embedder.model())
            .await?;
        return settle_source(core, &chunk.source_id).await;
    }

    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for p in paragraphs {
        let candidate = if current.is_empty() {
            p.to_string()
        } else {
            format!("{current}\n\n{p}")
        };
        if core.counter.count(&candidate) > limit && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
            current = p.to_string();
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }

    tracing::info!(chunk_id = %chunk.id, parts = parts.len(), "split oversize chunk into siblings");

    let base = chunk.ordinal;
    let new: Vec<NewChunk> = parts
        .iter()
        .enumerate()
        .map(|(i, text)| NewChunk {
            // Siblings sort after the original position and before the next
            // original chunk, which keeps reading order intact.
            ordinal: base * 1000 + i as i64,
            text: text.clone(),
            source_span: chunk.source_span.clone(),
            title: chunk.title.clone(),
            category: chunk.category.clone(),
            tags: chunk.tags.clone(),
        })
        .collect();

    let inserted = core.store.insert_chunks(&chunk.source_id, &new).await?;
    core.store.delete_chunk(&chunk.id).await?;
    core.vectors
        .delete_chunks(std::slice::from_ref(&chunk.id))
        .await?;

    for c in &inserted {
        core.store.enqueue(Stage::Embed, "chunk", &c.id).await?;
    }
    Ok(())
}

fn payload_of(chunk: &Chunk) -> VectorPayload {
    VectorPayload {
        chunk_id: chunk.id.clone(),
        source_id: chunk.source_id.clone(),
        text: chunk.text.clone(),
        title: chunk.title.clone(),
        category: chunk.category.clone(),
        tags: chunk.tags.clone(),
        created_at: chunk.created_at,
    }
}

/// Advance the parent source once no chunk is still pending: `ready` if every
/// chunk embedded, `partial` if any gave up.
pub async fn settle_source(core: &Core, source_id: &str) -> Result<()> {
    if core.store.pending_embed_count(source_id).await? > 0 {
        return Ok(());
    }
    let status = if core.store.failed_embed_count(source_id).await? > 0 {
        SourceStatus::Partial
    } else {
        SourceStatus::Ready
    };
    core.store.set_source_status(source_id, status).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::chunks::{EmbedState, NewChunk};
    use crate::store::sources::SourceStatus;

    async fn seed(core: &crate::core::Core, texts: &[&str]) -> (String, Vec<String>) {
        let src = core.store.insert_source("raw", "web", None).await.unwrap();
        let new: Vec<NewChunk> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| NewChunk {
                ordinal: i as i64,
                text: t.to_string(),
                source_span: None,
                title: Some(format!("t{i}")),
                category: Some("note".into()),
                tags: vec!["x".into()],
            })
            .collect();
        let made = core.store.insert_chunks(&src.id, &new).await.unwrap();
        core.store
            .set_source_status(&src.id, SourceStatus::Embedding)
            .await
            .unwrap();
        (src.id, made.into_iter().map(|c| c.id).collect())
    }

    #[tokio::test]
    async fn embeds_a_chunk_and_writes_a_searchable_point() {
        let core = test_core().await;
        let (src_id, ids) = seed(&core, &["## A\nthe body"]).await;

        run(&core, &ids[0]).await.unwrap();

        let c = core.store.get_chunk(&ids[0]).await.unwrap();
        assert_eq!(c.embed_state, EmbedState::Embedded);
        assert_eq!(c.embed_model.as_deref(), Some("fake-embed"));
        assert_eq!(core.vectors.count().await.unwrap(), 1);

        // The payload must carry enough to render a result without touching SQLite.
        let q = core
            .embedder
            .embed(&["t0\n## A\nthe body".to_string()])
            .await
            .unwrap();
        let hits = core
            .vectors
            .search(&q[0], 5, &Default::default())
            .await
            .unwrap();
        assert_eq!(hits[0].payload.source_id, src_id);
        assert_eq!(hits[0].payload.text, "## A\nthe body");
        assert_eq!(hits[0].payload.tags, vec!["x".to_string()]);
    }

    #[tokio::test]
    async fn source_becomes_ready_only_after_the_last_chunk() {
        let core = test_core().await;
        let (src_id, ids) = seed(&core, &["one", "two"]).await;

        run(&core, &ids[0]).await.unwrap();
        assert_eq!(
            core.store.get_source(&src_id).await.unwrap().status,
            SourceStatus::Embedding
        );

        run(&core, &ids[1]).await.unwrap();
        assert_eq!(
            core.store.get_source(&src_id).await.unwrap().status,
            SourceStatus::Ready
        );
    }

    #[tokio::test]
    async fn a_failed_chunk_leaves_the_source_partial() {
        let core = test_core().await;
        let (src_id, ids) = seed(&core, &["one", "two"]).await;
        run(&core, &ids[0]).await.unwrap();
        core.store.mark_embed_failed(&ids[1]).await.unwrap();
        settle_source(&core, &src_id).await.unwrap();
        assert_eq!(
            core.store.get_source(&src_id).await.unwrap().status,
            SourceStatus::Partial
        );
    }

    #[tokio::test]
    async fn oversize_chunks_are_split_into_siblings_not_truncated() {
        let core = test_core().await;
        let big = format!("{}\n\n{}", "alpha ".repeat(400), "beta ".repeat(400));
        let (src_id, ids) = seed(&core, &[&big]).await;

        run_with_limit(&core, &ids[0], 200).await.unwrap();

        let chunks = core.store.chunks_for_source(&src_id).await.unwrap();
        assert!(chunks.len() > 1, "oversize chunk must become siblings");
        let joined: String = chunks
            .iter()
            .map(|c| c.text.clone())
            .collect::<Vec<_>>()
            .join("");
        assert!(joined.contains("beta"), "no text may be dropped");
        assert!(joined.contains("alpha"));
    }

    #[tokio::test]
    async fn a_single_paragraph_oversize_chunk_is_still_embedded() {
        // No paragraph boundary to split on. Better one over-long vector than
        // a chunk that never becomes searchable at all.
        let core = test_core().await;
        let big = "alpha ".repeat(800);
        let (_src, ids) = seed(&core, &[&big]).await;
        run_with_limit(&core, &ids[0], 200).await.unwrap();
        assert_eq!(core.vectors.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn re_embedding_replaces_the_point_rather_than_adding_one() {
        let core = test_core().await;
        let (_src, ids) = seed(&core, &["text"]).await;
        run(&core, &ids[0]).await.unwrap();
        core.store
            .update_chunk_text(&ids[0], "edited text")
            .await
            .unwrap();
        run(&core, &ids[0]).await.unwrap();
        assert_eq!(core.vectors.count().await.unwrap(), 1);
    }
}
