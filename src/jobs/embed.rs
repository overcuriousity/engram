use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::artifacts::{Chunk, NewArtifact};
use crate::store::corpora::CorpusStatus;
use crate::store::jobs::Stage;
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

pub async fn run(core: &Core, artifact_id: &str) -> Result<()> {
    run_with_limit(core, artifact_id, default_limit(core)).await
}

fn default_limit(core: &Core) -> usize {
    (core.embedder.max_input_tokens() as f32 * SAFETY) as usize
}

/// Does this error mean the input itself is too big, rather than the endpoint
/// being unwell?
///
/// A local inference server has a hard physical batch size — llama.cpp answers
/// `input (1030 tokens) is too large to process` — and no amount of retrying
/// changes that. Configuration is meant to keep chunks under it, but the
/// configured ceiling is a claim about the model while this is the server's
/// real limit, and the two disagree more often than not.
fn input_too_large(e: &Error) -> bool {
    let Error::Inference {
        role: "embed",
        detail,
    } = e
    else {
        return false;
    };
    let d = detail.to_ascii_lowercase();
    d.contains("too large")
        || d.contains("too long")
        || d.contains("exceeds")
        || d.contains("413")
        || d.contains("batch size")
}

pub async fn run_with_limit(core: &Core, artifact_id: &str, limit: usize) -> Result<()> {
    let chunk = core.store.get_artifact(artifact_id).await?;
    let text = embed_text(&chunk);

    if core.counter.count(&text) > limit {
        // Our own estimate, which can be wrong in either direction. Do not
        // shred the chunk on a guess.
        return split_oversize(core, &chunk, limit, false).await;
    }

    match embed_batch(core, std::slice::from_ref(&chunk), vec![text.clone()]).await {
        Ok(()) => {}
        Err(e) if input_too_large(&e) => {
            // The endpoint's real ceiling is lower than the configured one, so
            // halving the configured limit would change nothing. Halve what the
            // chunk actually measures instead: that shrinks on every refusal
            // and converges on whatever the server will take.
            let measured = core.counter.count(&text);
            let smaller = (measured / 2).max(crate::infer::budget::MIN_SEGMENT_TOKENS);
            tracing::warn!(
                artifact_id,
                measured,
                smaller,
                error = %e,
                "endpoint refused the chunk as too large; splitting instead of retrying"
            );
            // The server said no. That is a fact rather than an estimate, so
            // this split has to succeed whatever the text looks like.
            return split_oversize(core, &chunk, smaller, true).await;
        }
        Err(e) => return Err(e),
    }
    settle_corpus(core, &chunk.corpus_id).await
}

/// Embed every chunk of a source that is still waiting, in as few inference
/// calls as the batch size allows.
///
/// One call per source rather than per chunk is the whole point: the embedding
/// endpoint is the slow, rate-limited, and often paid part of ingest.
pub async fn run_corpus(core: &Core, corpus_id: &str) -> Result<()> {
    run_corpus_with_limit(core, corpus_id, default_limit(core)).await
}

pub async fn run_corpus_with_limit(core: &Core, corpus_id: &str, limit: usize) -> Result<()> {
    let pending = core.store.pending_artifacts_for_corpus(corpus_id).await?;

    // An oversize chunk becomes siblings instead of a vector, so it cannot ride
    // along in a batch. It leaves behind its own per-chunk jobs.
    let mut batch: Vec<Chunk> = Vec::with_capacity(pending.len());
    let mut texts: Vec<String> = Vec::with_capacity(pending.len());
    for chunk in pending {
        let text = embed_text(&chunk);
        if core.counter.count(&text) > limit {
            split_oversize(core, &chunk, limit, false).await?;
        } else {
            texts.push(text);
            batch.push(chunk);
        }
    }

    for (chunks, texts) in batch.chunks(BATCH).zip(texts.chunks(BATCH)) {
        match embed_batch(core, chunks, texts.to_vec()).await {
            Ok(()) => {}
            // One oversize chunk fails the whole batch, and the batch cannot
            // say which. Per-chunk jobs isolate it, and that path splits it.
            Err(e) if input_too_large(&e) => {
                tracing::warn!(corpus_id, error = %e, "batch held a chunk the endpoint will not take; isolating");
                return split_into_artifact_jobs(core, corpus_id).await;
            }
            Err(e) => return Err(e),
        }
    }
    settle_corpus(core, corpus_id).await
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
            artifact_id = %chunk.id,
            "chunk was edited while it was being embedded; leaving it pending"
        );
    }
    Ok(())
}

/// Fall back from one job per source to one job per chunk. A batch that has
/// exhausted its retries may be failing on a single chunk the embedder rejects,
/// and one bad chunk must not keep its siblings out of search.
pub async fn split_into_artifact_jobs(core: &Core, corpus_id: &str) -> Result<()> {
    let pending = core.store.pending_artifacts_for_corpus(corpus_id).await?;
    for c in &pending {
        core.store.enqueue(Stage::Embed, "artifact", &c.id).await?;
    }
    tracing::info!(
        corpus_id,
        chunks = pending.len(),
        "split batch into per-chunk embed jobs"
    );
    Ok(())
}

/// A chunk larger than the embedder accepts becomes several sibling chunks
/// split at a paragraph boundary. Truncating would silently discard knowledge,
/// and one vector per fragment keeps the data model unchanged.
async fn split_oversize(core: &Core, chunk: &Chunk, limit: usize, hard: bool) -> Result<()> {
    let paragraphs: Vec<&str> = chunk
        .text
        .split("\n\n")
        .filter(|p| !p.trim().is_empty())
        .collect();

    if paragraphs.len() < 2 {
        // No blank line to cut on. Code, tables and reference entries look like
        // this, and they are exactly what a local embedding server refuses for
        // exceeding its physical batch — so when the refusal is real, cut on
        // lines and then on characters rather than trying the same thing again.
        if hard {
            let parts = split_by_lines(&chunk.text, limit, &core.counter);
            if parts.len() > 1 {
                return replace_with_siblings(core, chunk, parts).await;
            }
        }
        // Optimism, once: our token estimate may simply be wrong, and one
        // over-long vector beats shredding a chunk on a guess. But if the
        // server refuses it, the guess is settled and the text has to be cut.
        tracing::warn!(artifact_id = %chunk.id, "oversize chunk has no paragraph boundary; embedding as-is");
        let vectors = match core.embedder.embed(std::slice::from_ref(&chunk.text)).await {
            Ok(v) => v,
            Err(e) if input_too_large(&e) => {
                let parts = split_by_lines(&chunk.text, limit, &core.counter);
                if parts.len() > 1 {
                    tracing::warn!(artifact_id = %chunk.id, parts = parts.len(), "endpoint refused it whole; cutting on lines");
                    return replace_with_siblings(core, chunk, parts).await;
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        };
        core.vectors
            .upsert(vec![VectorPoint {
                vector: vectors.into_iter().next().unwrap(),
                sparse: crate::vector::sparse::encode_document(&chunk.text),
                payload: payload_of(chunk),
            }])
            .await?;
        mark_indexed(core, chunk).await?;
        return settle_corpus(core, &chunk.corpus_id).await;
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

    replace_with_siblings(core, chunk, parts).await
}

/// Cut text that has no paragraph breaks. Lines first, and a single line that
/// still will not fit is cut on character count — the point is that this always
/// returns something smaller than it was given.
fn split_by_lines(
    text: &str,
    limit: usize,
    counter: &crate::infer::budget::TokenCounter,
) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        let candidate = if current.is_empty() {
            line.to_string()
        } else {
            format!("{current}\n{line}")
        };
        if counter.count(&candidate) > limit && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
            current = line.to_string();
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }

    // One line longer than the limit on its own: a minified blob or a very long
    // command. Characters are the last resort, and four per token is the same
    // conservative ratio the budget estimate uses.
    let max_chars = limit.saturating_mul(4).max(64);
    parts
        .into_iter()
        .flat_map(|p| {
            if counter.count(&p) <= limit {
                return vec![p];
            }
            p.chars()
                .collect::<Vec<_>>()
                .chunks(max_chars)
                .map(|c| c.iter().collect::<String>())
                .collect::<Vec<_>>()
        })
        .collect()
}

async fn replace_with_siblings(core: &Core, chunk: &Chunk, parts: Vec<String>) -> Result<()> {
    tracing::info!(artifact_id = %chunk.id, parts = parts.len(), "split oversize chunk into siblings");

    let base = chunk.ordinal;
    // Make the siblings' room before numbering them. Numbering them apart from
    // their neighbours instead — `base * 1000 + i` — sorts chunk 2's siblings
    // after chunks 3 onward rather than before them, and the next segmentation
    // pass renumbers that wrong order into place permanently.
    core.store
        .make_room_after(&chunk.corpus_id, base, parts.len() as i64 - 1)
        .await?;
    let new: Vec<NewArtifact> = parts
        .iter()
        .enumerate()
        .map(|(i, text)| NewArtifact {
            // Siblings sort after the original position and before the next
            // original chunk, which keeps reading order intact.
            ordinal: base + i as i64,
            text: text.clone(),
            corpus_span: chunk.corpus_span.clone(),
            title: chunk.title.clone(),
            category: chunk.category.clone(),
            // A caveat applies to the whole passage the parent held, so every
            // fragment of it inherits the warning rather than losing it.
            caveats: chunk.caveats.clone(),
            tags: chunk.tags.clone(),
            // Siblings belong to the window their parent came from, or a
            // re-segmentation of that window would leave them behind.
            segment_idx: chunk.segment_idx,
        })
        .collect();

    let inserted = core.store.insert_artifacts(&chunk.corpus_id, &new).await?;
    core.store.delete_artifact(&chunk.id).await?;
    core.vectors
        .delete_artifacts(std::slice::from_ref(&chunk.id))
        .await?;

    for c in &inserted {
        core.store.enqueue(Stage::Embed, "artifact", &c.id).await?;
    }
    Ok(())
}

fn payload_of(chunk: &Chunk) -> VectorPayload {
    VectorPayload {
        artifact_id: chunk.id.clone(),
        corpus_id: chunk.corpus_id.clone(),
        text: chunk.text.clone(),
        title: chunk.title.clone(),
        category: chunk.category.clone(),
        tags: chunk.tags.clone(),
        created_at: chunk.created_at,
        // Unset means "whatever is already stored": the vector store carries
        // the existing stamp forward rather than letting a re-embed make a
        // chunk look forgotten.
        last_seen_at: None,
        // Unset for the same reason, and it matters more: writing `false` here
        // would revive an artifact the sweep hid, on every re-embed.
        superseded: None,
    }
}

/// Advance the parent source once no chunk is still pending: `ready` if every
/// chunk embedded, `partial` if any gave up.
pub async fn settle_corpus(core: &Core, corpus_id: &str) -> Result<()> {
    if core.store.pending_embed_count(corpus_id).await? > 0 {
        return Ok(());
    }
    let status = if core.store.failed_embed_count(corpus_id).await? > 0 {
        CorpusStatus::Partial
    } else if core.store.get_corpus(corpus_id).await?.status == CorpusStatus::Partial {
        // A source with a window the model refused is already partial. Its
        // chunks embedding cleanly does not fill the hole those lines left,
        // and reporting `ready` would hide it.
        CorpusStatus::Partial
    } else {
        CorpusStatus::Ready
    };
    core.store.set_corpus_status(corpus_id, status).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_chunk_the_endpoint_refuses_is_split_rather_than_retried() {
        // The deployment that produced this: config claimed 8192 input tokens,
        // llama.cpp's physical batch was 1024, and the chunk in between failed
        // five identical times before anyone looked.
        let mut core = crate::core::test_support::test_core().await;
        let strict = std::sync::Arc::new(crate::infer::fake::StrictEmbedder::new(
            crate::core::test_support::TEST_DIM,
            200,
        ));
        core.embedder = strict.clone();

        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        // Several paragraphs, comfortably over the endpoint's real ceiling.
        let body = (0..40)
            .map(|i| format!("paragraph {i} with a good deal of filler text in it"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: body,
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();

        // The configured limit is the lie; the endpoint's is what bites.
        run_with_limit(&core, &made[0].id, 8192).await.unwrap();

        let chunks = core.store.artifacts_for_corpus(&src.id).await.unwrap();
        assert!(
            chunks.len() > 1,
            "the refused chunk should have become siblings, got {}",
            chunks.len()
        );
        assert!(
            chunks.iter().all(|c| c.segment_idx == Some(0)),
            "siblings must stay attached to the window that produced them"
        );
    }

    #[tokio::test]
    async fn a_chunk_with_no_paragraph_breaks_is_still_split() {
        // Code, tables and reference entries have no blank lines, and they are
        // exactly what a local embedding server refuses for exceeding its
        // physical batch. Giving up on them meant retrying forever.
        let mut core = crate::core::test_support::test_core().await;
        core.embedder = std::sync::Arc::new(crate::infer::fake::StrictEmbedder::new(
            crate::core::test_support::TEST_DIM,
            120,
        ));

        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let body = (0..60)
            .map(|i| format!("    command --flag-{i} /path/to/thing-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !body.contains("\n\n"),
            "the point is that there are no paragraphs"
        );
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: body,
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();

        run_with_limit(&core, &made[0].id, 8192).await.unwrap();

        let chunks = core.store.artifacts_for_corpus(&src.id).await.unwrap();
        assert!(chunks.len() > 1, "got {} chunks", chunks.len());
    }

    #[tokio::test]
    async fn a_refusal_during_the_as_is_attempt_still_ends_in_a_split() {
        // The trap this closes: the estimate says the chunk is oversize, there
        // is no paragraph to cut on, so it is embedded whole — and the server
        // refuses it, forever, because nothing along that path can shrink it.
        let mut core = crate::core::test_support::test_core().await;
        core.embedder = std::sync::Arc::new(crate::infer::fake::StrictEmbedder::new(
            crate::core::test_support::TEST_DIM,
            120,
        ));

        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let body = (0..60)
            .map(|i| format!("    command --flag-{i} /path/to/thing-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: body,
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();

        // A limit low enough that the proactive path runs, which is the path
        // that used to dead-end.
        run_with_limit(&core, &made[0].id, 100).await.unwrap();

        let chunks = core.store.artifacts_for_corpus(&src.id).await.unwrap();
        assert!(chunks.len() > 1, "got {} chunks", chunks.len());
    }

    #[test]
    fn splitting_by_lines_always_returns_something_smaller() {
        let counter = crate::infer::budget::TokenCounter::Estimate;
        // A single line far over the limit, with no whitespace to cut on.
        let blob = "x".repeat(4000);
        let parts = split_by_lines(&blob, 100, &counter);
        assert!(parts.len() > 1, "a long single line must still be cut");
        assert!(
            parts.iter().all(|p| counter.count(p) <= 100 * 2),
            "each part must be near the limit rather than the original size"
        );
        assert_eq!(parts.concat(), blob, "cutting must not lose text");
    }

    #[test]
    fn an_endpoint_size_refusal_is_told_apart_from_a_sick_endpoint() {
        let too_big = Error::Inference {
            role: "embed",
            detail: "input (1030 tokens) is too large to process. increase the physical \
                     batch size (current batch size: 1024)"
                .into(),
        };
        assert!(input_too_large(&too_big));

        // A transient failure must stay retryable, or a flaky local server
        // would start shredding chunks instead of waiting for it to recover.
        let flaky = Error::Inference {
            role: "embed",
            detail: "error sending request".into(),
        };
        assert!(!input_too_large(&flaky));
        let wrong_role = Error::Inference {
            role: "chunk",
            detail: "context too large".into(),
        };
        assert!(!input_too_large(&wrong_role));
    }
    use crate::core::test_support::test_core;
    use crate::store::artifacts::{EmbedState, NewArtifact};
    use crate::store::corpora::CorpusStatus;

    async fn seed(core: &crate::core::Core, texts: &[&str]) -> (String, Vec<String>) {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| NewArtifact {
                ordinal: i as i64,
                text: t.to_string(),
                corpus_span: None,
                title: Some(format!("t{i}")),
                category: Some("note".into()),
                tags: vec!["x".into()],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        let made = core.store.insert_artifacts(&src.id, &new).await.unwrap();
        core.store
            .set_corpus_status(&src.id, CorpusStatus::Embedding)
            .await
            .unwrap();
        (src.id, made.into_iter().map(|c| c.id).collect())
    }

    #[tokio::test]
    async fn embeds_a_chunk_and_writes_a_searchable_point() {
        let core = test_core().await;
        let (src_id, ids) = seed(&core, &["## A\nthe body"]).await;

        run(&core, &ids[0]).await.unwrap();

        let c = core.store.get_artifact(&ids[0]).await.unwrap();
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
        assert_eq!(hits[0].payload.corpus_id, src_id);
        assert_eq!(hits[0].payload.text, "## A\nthe body");
        assert_eq!(hits[0].payload.tags, vec!["x".to_string()]);
    }

    #[tokio::test]
    async fn source_becomes_ready_only_after_the_last_chunk() {
        let core = test_core().await;
        let (src_id, ids) = seed(&core, &["one", "two"]).await;

        run(&core, &ids[0]).await.unwrap();
        assert_eq!(
            core.store.get_corpus(&src_id).await.unwrap().status,
            CorpusStatus::Embedding
        );

        run(&core, &ids[1]).await.unwrap();
        assert_eq!(
            core.store.get_corpus(&src_id).await.unwrap().status,
            CorpusStatus::Ready
        );
    }

    #[tokio::test]
    async fn a_failed_chunk_leaves_the_source_partial() {
        let core = test_core().await;
        let (src_id, ids) = seed(&core, &["one", "two"]).await;
        run(&core, &ids[0]).await.unwrap();
        core.store.mark_embed_failed(&ids[1]).await.unwrap();
        settle_corpus(&core, &src_id).await.unwrap();
        assert_eq!(
            core.store.get_corpus(&src_id).await.unwrap().status,
            CorpusStatus::Partial
        );
    }

    #[tokio::test]
    async fn a_whole_source_is_embedded_in_one_inference_call() {
        // The embedding endpoint is the slow, rate-limited part of ingest.
        // Five chunks must cost one call, not five.
        let (core, embedder) = crate::core::test_support::test_core_counting_embed_calls().await;
        let (src_id, ids) = seed(&core, &["one", "two", "three", "four", "five"]).await;

        run_corpus(&core, &src_id).await.unwrap();

        assert_eq!(embedder.calls(), 1, "chunks were embedded one at a time");
        assert_eq!(core.vectors.count().await.unwrap(), 5);
        for id in &ids {
            assert_eq!(
                core.store.get_artifact(id).await.unwrap().embed_state,
                EmbedState::Embedded
            );
        }
        assert_eq!(
            core.store.get_corpus(&src_id).await.unwrap().status,
            CorpusStatus::Ready
        );
    }

    #[tokio::test]
    async fn a_batch_larger_than_the_request_limit_is_split_across_calls() {
        // Endpoints cap how many inputs they accept, so the batch is bounded.
        let (core, embedder) = crate::core::test_support::test_core_counting_embed_calls().await;
        let texts: Vec<String> = (0..BATCH + 5).map(|i| format!("chunk {i}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let (src_id, _) = seed(&core, &refs).await;

        run_corpus(&core, &src_id).await.unwrap();

        assert_eq!(embedder.calls(), 2, "the batch was not bounded");
        assert_eq!(core.vectors.count().await.unwrap(), (BATCH + 5) as u64);
    }

    #[tokio::test]
    async fn a_source_with_nothing_pending_still_settles() {
        // Re-running a finished job must not leave the source stuck in
        // `embedding` forever.
        let core = test_core().await;
        let (src_id, _) = seed(&core, &["one"]).await;
        run_corpus(&core, &src_id).await.unwrap();
        run_corpus(&core, &src_id).await.unwrap();
        assert_eq!(
            core.store.get_corpus(&src_id).await.unwrap().status,
            CorpusStatus::Ready
        );
    }

    #[tokio::test]
    async fn an_oversize_chunk_does_not_block_its_siblings() {
        // It becomes siblings rather than a vector, so it cannot ride along in
        // the batch. The rest of the source must still be embedded.
        let core = test_core().await;
        let big = format!("{}\n\n{}", "alpha ".repeat(400), "beta ".repeat(400));
        let (src_id, _) = seed(&core, &["small one", &big, "small two"]).await;

        run_corpus_with_limit(&core, &src_id, 200).await.unwrap();

        assert_eq!(
            core.vectors.count().await.unwrap(),
            2,
            "the two small chunks should be embedded"
        );
        let chunks = core.store.artifacts_for_corpus(&src_id).await.unwrap();
        assert!(
            chunks.len() > 3,
            "the oversize chunk should have become siblings"
        );
    }

    #[tokio::test]
    async fn a_partially_segmented_source_is_not_promoted_to_ready() {
        // `partial` records that segmentation was degraded. Every chunk
        // embedding cleanly does not undo that, and reporting `ready` would
        // hide it.
        let core = test_core().await;
        let (src_id, _) = seed(&core, &["one", "two"]).await;
        core.store
            .set_corpus_status(&src_id, CorpusStatus::Partial)
            .await
            .unwrap();

        run_corpus(&core, &src_id).await.unwrap();

        assert_eq!(
            core.store.get_corpus(&src_id).await.unwrap().status,
            CorpusStatus::Partial
        );
    }

    #[tokio::test]
    async fn oversize_chunks_are_split_into_siblings_not_truncated() {
        let core = test_core().await;
        let big = format!("{}\n\n{}", "alpha ".repeat(400), "beta ".repeat(400));
        let (src_id, ids) = seed(&core, &[&big]).await;

        run_with_limit(&core, &ids[0], 200).await.unwrap();

        let chunks = core.store.artifacts_for_corpus(&src_id).await.unwrap();
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
        let stale = core.store.get_artifact(&ids[0]).await.unwrap();

        core.store
            .update_artifact_text(&ids[0], "edited while embedding")
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
            core.store.get_artifact(&ids[0]).await.unwrap().embed_state,
            EmbedState::Pending,
            "the chunk must stay queued for the text that is actually there"
        );

        // And the retry, reading the current row, does land.
        run(&core, &ids[0]).await.unwrap();
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().embed_state,
            EmbedState::Embedded
        );
    }

    #[tokio::test]
    async fn reprocessing_a_source_outlives_a_worker_already_embedding_it() {
        // `reset_embed_state` and an in-flight batch race by construction: both
        // write the same chunk, and only the revision says which is current.
        let core = test_core().await;
        let (src_id, ids) = seed(&core, &["one", "two"]).await;
        let inflight: Vec<_> = core
            .store
            .pending_artifacts_for_corpus(&src_id)
            .await
            .unwrap();

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
    async fn re_embedding_does_not_make_a_chunk_look_forgotten() {
        // A point write replaces the payload rather than merging it, so a
        // re-embed built from the chunk row used to clear `last_seen_at` — and
        // `resurface` would offer a chunk read yesterday as forgotten.
        let core = test_core().await;
        let (_src, ids) = seed(&core, &["text"]).await;
        run(&core, &ids[0]).await.unwrap();
        core.vectors.touch(&ids[0..1], 1_700_000_000).await.unwrap();

        core.store
            .update_artifact_text(&ids[0], "edited text")
            .await
            .unwrap();
        run(&core, &ids[0]).await.unwrap();

        let forgotten = core
            .vectors
            .resurface(10, i64::MAX, 1_700_000_000)
            .await
            .unwrap();
        assert!(
            forgotten.is_empty(),
            "the re-embed dropped the stamp and the chunk now reads as unseen"
        );
    }

    #[tokio::test]
    async fn split_siblings_keep_the_reading_order_of_the_chunk_they_replace() {
        // `base * 1000 + i` put chunk 1's siblings after chunks 2 onward, and
        // the next segmentation pass renumbered that order into place for good.
        let core = test_core().await;
        let big = format!("{}\n\n{}", "alpha ".repeat(400), "beta ".repeat(400));
        let (src_id, ids) = seed(&core, &["first", &big, "last"]).await;

        run_with_limit(&core, &ids[1], 200).await.unwrap();

        let texts: Vec<String> = core
            .store
            .artifacts_for_corpus(&src_id)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.text)
            .collect();
        assert_eq!(texts.first().map(String::as_str), Some("first"));
        assert_eq!(
            texts.last().map(String::as_str),
            Some("last"),
            "the siblings sorted past the chunk that follows them: {texts:?}"
        );
    }

    #[tokio::test]
    async fn re_embedding_replaces_the_point_rather_than_adding_one() {
        let core = test_core().await;
        let (_src, ids) = seed(&core, &["text"]).await;
        run(&core, &ids[0]).await.unwrap();
        core.store
            .update_artifact_text(&ids[0], "edited text")
            .await
            .unwrap();
        run(&core, &ids[0]).await.unwrap();
        assert_eq!(core.vectors.count().await.unwrap(), 1);
    }
}
