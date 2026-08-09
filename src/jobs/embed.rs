use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::chunks::{Chunk, NewChunk};
use crate::store::jobs::Stage;
use crate::store::sources::SourceStatus;
use crate::vector::{VectorPayload, VectorPoint};

/// Fraction of the embedder's hard limit a chunk is allowed to occupy. The
/// remaining headroom absorbs tokenizer estimation error.
const SAFETY: f32 = 0.8;

/// Chunks sent to the embedder in one request. Bounded because a long source
/// can hold hundreds of chunks and endpoints cap how many inputs they accept.
const BATCH: usize = 32;

/// Text for the embedding carries the title: it holds topical signal the body
/// often leaves implicit.
fn embed_text(chunk: &Chunk) -> String {
    match &chunk.title {
        Some(t) => format!("{t}\n{}", chunk.text),
        None => chunk.text.clone(),
    }
}

pub async fn run(core: &Core, chunk_id: &str) -> Result<()> {
    run_with_limit(core, chunk_id, default_limit(core)).await
}

fn default_limit(core: &Core) -> usize {
    (core.embedder.max_input_tokens() as f32 * SAFETY) as usize
}

pub async fn run_with_limit(core: &Core, chunk_id: &str, limit: usize) -> Result<()> {
    let chunk = core.store.get_chunk(chunk_id).await?;
    let text = embed_text(&chunk);

    if core.counter.count(&text) > limit {
        return split_oversize(core, &chunk, limit).await;
    }

    embed_batch(core, std::slice::from_ref(&chunk), vec![text]).await?;
    settle_source(core, &chunk.source_id).await
}

/// Embed every chunk of a source that is still waiting, in as few inference
/// calls as the batch size allows.
///
/// One call per source rather than per chunk is the whole point: the embedding
/// endpoint is the slow, rate-limited, and often paid part of ingest.
pub async fn run_source(core: &Core, source_id: &str) -> Result<()> {
    run_source_with_limit(core, source_id, default_limit(core)).await
}

pub async fn run_source_with_limit(core: &Core, source_id: &str, limit: usize) -> Result<()> {
    let pending = core.store.pending_chunks_for_source(source_id).await?;

    // An oversize chunk becomes siblings instead of a vector, so it cannot ride
    // along in a batch. It leaves behind its own per-chunk jobs.
    let mut batch: Vec<Chunk> = Vec::with_capacity(pending.len());
    let mut texts: Vec<String> = Vec::with_capacity(pending.len());
    for chunk in pending {
        let text = embed_text(&chunk);
        if core.counter.count(&text) > limit {
            split_oversize(core, &chunk, limit).await?;
        } else {
            texts.push(text);
            batch.push(chunk);
        }
    }

    for (chunks, texts) in batch.chunks(BATCH).zip(texts.chunks(BATCH)) {
        embed_batch(core, chunks, texts.to_vec()).await?;
    }
    settle_source(core, source_id).await
}

/// One inference call and one upsert for the whole slice. Chunks are marked
/// embedded only once Qdrant has durably accepted their vectors, so a crash
/// leaves work to redo rather than a chunk that claims to be searchable.
async fn embed_batch(core: &Core, chunks: &[Chunk], texts: Vec<String>) -> Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }
    let vectors = core.embedder.embed(&texts).await?;
    if vectors.len() != chunks.len() {
        return Err(Error::Inference {
            role: "embed",
            detail: format!(
                "asked for {} embeddings and got {}; pairing them would attach vectors \
                 to the wrong chunks",
                chunks.len(),
                vectors.len()
            ),
        });
    }

    let points = chunks
        .iter()
        .zip(texts.iter())
        .zip(vectors)
        .map(|((c, text), vector)| VectorPoint {
            vector,
            // The same text the embedder saw, so the lexical and the semantic
            // half of a hit always describe the same thing.
            sparse: crate::vector::sparse::encode_document(text),
            payload: payload_of(c),
        })
        .collect();
    core.vectors.upsert(points).await?;

    for c in chunks {
        mark_indexed(core, c).await?;
    }
    Ok(())
}

/// Report a chunk indexed, unless it was edited while it was being embedded.
///
/// The revision is the one read before the inference call, so an edit that
/// landed in between wins: the mark does not apply, the chunk stays pending,
/// and the job the editor queued embeds the text that is actually there.
async fn mark_indexed(core: &Core, chunk: &Chunk) -> Result<()> {
    let landed = core
        .store
        .mark_embedded(&chunk.id, core.embedder.model(), chunk.embed_rev)
        .await?;
    if !landed {
        tracing::info!(
            chunk_id = %chunk.id,
            "chunk was edited while it was being embedded; leaving it pending"
        );
    }
    Ok(())
}

/// Fall back from one job per source to one job per chunk. A batch that has
/// exhausted its retries may be failing on a single chunk the embedder rejects,
/// and one bad chunk must not keep its siblings out of search.
pub async fn split_into_chunk_jobs(core: &Core, source_id: &str) -> Result<()> {
    let pending = core.store.pending_chunks_for_source(source_id).await?;
    for c in &pending {
        core.store.enqueue(Stage::Embed, "chunk", &c.id).await?;
    }
    tracing::info!(
        source_id,
        chunks = pending.len(),
        "split batch into per-chunk embed jobs"
    );
    Ok(())
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
                sparse: crate::vector::sparse::encode_document(&chunk.text),
                payload: payload_of(chunk),
            }])
            .await?;
        mark_indexed(core, chunk).await?;
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
        // Left unset so re-embedding does not make a chunk look forgotten.
        last_seen_at: None,
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
    } else if core.store.get_source(source_id).await?.status == SourceStatus::Partial {
        // A source segmented by the structural fallback is already partial.
        // Its chunks embedding cleanly does not undo that degradation, and
        // reporting `ready` would hide it.
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
            .search(&q[0], &Default::default(), 5, &Default::default())
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
    async fn a_whole_source_is_embedded_in_one_inference_call() {
        // The embedding endpoint is the slow, rate-limited part of ingest.
        // Five chunks must cost one call, not five.
        let (core, embedder) = crate::core::test_support::test_core_counting_embed_calls().await;
        let (src_id, ids) = seed(&core, &["one", "two", "three", "four", "five"]).await;

        run_source(&core, &src_id).await.unwrap();

        assert_eq!(embedder.calls(), 1, "chunks were embedded one at a time");
        assert_eq!(core.vectors.count().await.unwrap(), 5);
        for id in &ids {
            assert_eq!(
                core.store.get_chunk(id).await.unwrap().embed_state,
                EmbedState::Embedded
            );
        }
        assert_eq!(
            core.store.get_source(&src_id).await.unwrap().status,
            SourceStatus::Ready
        );
    }

    #[tokio::test]
    async fn a_batch_larger_than_the_request_limit_is_split_across_calls() {
        // Endpoints cap how many inputs they accept, so the batch is bounded.
        let (core, embedder) = crate::core::test_support::test_core_counting_embed_calls().await;
        let texts: Vec<String> = (0..BATCH + 5).map(|i| format!("chunk {i}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let (src_id, _) = seed(&core, &refs).await;

        run_source(&core, &src_id).await.unwrap();

        assert_eq!(embedder.calls(), 2, "the batch was not bounded");
        assert_eq!(core.vectors.count().await.unwrap(), (BATCH + 5) as u64);
    }

    #[tokio::test]
    async fn a_source_with_nothing_pending_still_settles() {
        // Re-running a finished job must not leave the source stuck in
        // `embedding` forever.
        let core = test_core().await;
        let (src_id, _) = seed(&core, &["one"]).await;
        run_source(&core, &src_id).await.unwrap();
        run_source(&core, &src_id).await.unwrap();
        assert_eq!(
            core.store.get_source(&src_id).await.unwrap().status,
            SourceStatus::Ready
        );
    }

    #[tokio::test]
    async fn an_oversize_chunk_does_not_block_its_siblings() {
        // It becomes siblings rather than a vector, so it cannot ride along in
        // the batch. The rest of the source must still be embedded.
        let core = test_core().await;
        let big = format!("{}\n\n{}", "alpha ".repeat(400), "beta ".repeat(400));
        let (src_id, _) = seed(&core, &["small one", &big, "small two"]).await;

        run_source_with_limit(&core, &src_id, 200).await.unwrap();

        assert_eq!(
            core.vectors.count().await.unwrap(),
            2,
            "the two small chunks should be embedded"
        );
        let chunks = core.store.chunks_for_source(&src_id).await.unwrap();
        assert!(
            chunks.len() > 3,
            "the oversize chunk should have become siblings"
        );
    }

    #[tokio::test]
    async fn a_fallback_segmented_source_is_not_promoted_to_ready() {
        // `partial` records that segmentation was degraded. Every chunk
        // embedding cleanly does not undo that, and reporting `ready` would
        // hide it.
        let core = test_core().await;
        let (src_id, _) = seed(&core, &["one", "two"]).await;
        core.store
            .set_source_status(&src_id, SourceStatus::Partial)
            .await
            .unwrap();

        run_source(&core, &src_id).await.unwrap();

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
    async fn a_chunk_edited_mid_embed_is_not_reported_as_indexed() {
        // The job read the chunk, called a slow endpoint, and is about to write
        // back "indexed". An edit that landed in that window must win, or the
        // vector describes text that no longer exists and nothing says so.
        let core = test_core().await;
        let (_src, ids) = seed(&core, &["one"]).await;
        let stale = core.store.get_chunk(&ids[0]).await.unwrap();

        core.store
            .update_chunk_text(&ids[0], "edited while embedding")
            .await
            .unwrap();

        // What the in-flight job would have done, with the revision it read.
        assert!(
            !core
                .store
                .mark_embedded(&stale.id, "fake-embed", stale.embed_rev)
                .await
                .unwrap(),
            "a stale job overwrote a newer edit"
        );
        assert_eq!(
            core.store.get_chunk(&ids[0]).await.unwrap().embed_state,
            EmbedState::Pending,
            "the chunk must stay queued for the text that is actually there"
        );

        // And the retry, reading the current row, does land.
        run(&core, &ids[0]).await.unwrap();
        assert_eq!(
            core.store.get_chunk(&ids[0]).await.unwrap().embed_state,
            EmbedState::Embedded
        );
    }

    #[tokio::test]
    async fn reprocessing_a_source_outlives_a_worker_already_embedding_it() {
        // `reset_embed_state` and an in-flight batch race by construction: both
        // write the same chunk, and only the revision says which is current.
        let core = test_core().await;
        let (src_id, ids) = seed(&core, &["one", "two"]).await;
        let inflight: Vec<_> = core.store.pending_chunks_for_source(&src_id).await.unwrap();

        core.store.reset_embed_state(&src_id).await.unwrap();
        for c in &inflight {
            assert!(
                !core
                    .store
                    .mark_embedded(&c.id, "fake-embed", c.embed_rev)
                    .await
                    .unwrap()
            );
        }

        assert_eq!(
            core.store.pending_embed_count(&src_id).await.unwrap(),
            ids.len() as i64,
            "the reprocess was silently cancelled by the job it interrupted"
        );
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
