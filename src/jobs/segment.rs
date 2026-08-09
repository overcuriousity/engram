use crate::core::Core;
use crate::error::Result;
use crate::infer::budget::window_tokens;
use crate::infer::split::{split_into_windows, structural_chunks};
use crate::store::chunks::{NewChunk, SourceSpan};
use crate::store::jobs::Stage;
use crate::store::sources::SourceStatus;

/// Tokens consumed by the system prompt and scaffolding. Measured from the
/// real prompt rather than guessed.
fn prompt_overhead(core: &Core) -> usize {
    core.counter.count(crate::infer::prompt::CHUNKER_SYSTEM) + 200
}

/// LLM-assisted segmentation. Windows the raw text, calls the chunker per
/// window, and replaces the source's chunks with the result.
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

    let mut proposed = Vec::new();
    for w in &windows {
        let mut chunks = core.chunker.segment(&w.text).await?;
        // Line numbers come back relative to the window, so shift them into
        // the coordinates of the original document.
        for c in &mut chunks {
            c.source_lines = c
                .source_lines
                .map(|(a, b)| (a + w.start_line - 1, b + w.start_line - 1))
                .or(Some((w.start_line, w.end_line)));
        }
        proposed.extend(chunks);
    }

    write_chunks(
        core,
        source_id,
        proposed_to_new(proposed),
        SourceStatus::Embedding,
    )
    .await
}

/// No-LLM fallback used once the chunker has exhausted its retries. Splits on
/// paragraphs without rewriting; a source is never left with zero chunks.
pub async fn run_with_fallback(core: &Core, source_id: &str) -> Result<()> {
    let src = core.store.get_source(source_id).await?;
    let new: Vec<NewChunk> = structural_chunks(&src.raw_text)
        .into_iter()
        .enumerate()
        .map(|(i, (text, start, end))| NewChunk {
            ordinal: i as i64,
            text,
            source_span: Some(SourceSpan {
                start_line: start,
                end_line: end,
            }),
            title: None,
            category: None,
            tags: vec![],
        })
        .collect();

    if new.is_empty() {
        core.store
            .set_source_status(source_id, SourceStatus::Failed)
            .await?;
        return Ok(());
    }
    tracing::warn!(
        source_id,
        chunks = new.len(),
        "segmentation fell back to a structural split"
    );
    write_chunks(core, source_id, new, SourceStatus::Partial).await
}

fn proposed_to_new(proposed: Vec<crate::infer::ProposedChunk>) -> Vec<NewChunk> {
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
        })
        .collect()
}

async fn write_chunks(
    core: &Core,
    source_id: &str,
    new: Vec<NewChunk>,
    status: SourceStatus,
) -> Result<()> {
    // Replace, never append: a retried job must not double the chunk count.
    core.vectors.delete_by_source(source_id).await?;
    for old in core.store.chunks_for_source(source_id).await? {
        core.store.delete_chunk(&old.id).await?;
    }

    let inserted = core.store.insert_chunks(source_id, &new).await?;
    for c in &inserted {
        core.store.enqueue(Stage::Embed, "chunk", &c.id).await?;
    }
    core.store.set_source_status(source_id, status).await?;
    tracing::info!(source_id, chunks = inserted.len(), "segmented");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::{test_core, test_core_with_failing_chunker};
    use crate::store::jobs::Stage;
    use crate::store::sources::SourceStatus;

    /// A body several windows long under the fake chunker's budget.
    fn multi_window_body() -> String {
        (0..400)
            .map(|i| format!("paragraph number {i} with some filler text"))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn window_count(core: &crate::core::Core, body: &str) -> usize {
        crate::infer::split::split_into_windows(
            body,
            &core.counter,
            window_tokens(core.chunker.budget(), prompt_overhead(core)),
        )
        .len()
    }

    #[tokio::test]
    async fn segments_a_source_into_chunks_and_queues_embedding() {
        let core = test_core().await;
        let out = core
            .ingest("first para\n\nsecond para", "web", None)
            .await
            .unwrap();

        run(&core, &out.id).await.unwrap();

        let chunks = core.store.chunks_for_source(&out.id).await.unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].ordinal, 0);
        assert_eq!(chunks[1].ordinal, 1);
        assert_eq!(
            core.store.get_source(&out.id).await.unwrap().status,
            SourceStatus::Embedding
        );

        core.store.claim_job().await.unwrap(); // segment
        let mut embed_jobs = 0;
        while let Some(j) = core.store.claim_job().await.unwrap() {
            if j.stage == Stage::Embed {
                embed_jobs += 1;
            }
        }
        assert_eq!(embed_jobs, 2);
    }

    #[tokio::test]
    async fn ordinals_stay_continuous_across_windows() {
        let core = test_core().await;
        // Large enough to exceed the fake chunker's window budget several
        // times over, so segmentation really does run per window.
        let body = multi_window_body();
        let out = core.ingest(&body, "web", None).await.unwrap();
        assert!(
            window_count(&core, &body) > 1,
            "test body must span multiple windows or it proves nothing"
        );

        run(&core, &out.id).await.unwrap();

        let chunks = core.store.chunks_for_source(&out.id).await.unwrap();
        assert!(chunks.len() > 1);
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.ordinal, i as i64, "ordinals must not restart per window");
        }
    }

    #[tokio::test]
    async fn unparsable_chunker_output_falls_back_to_a_structural_split() {
        let core = test_core_with_failing_chunker().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();

        let err = run(&core, &out.id).await.unwrap_err();
        assert!(
            err.retryable(),
            "a dead endpoint deserves a retry, not a fallback"
        );

        run_with_fallback(&core, &out.id).await.unwrap();
        let chunks = core.store.chunks_for_source(&out.id).await.unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "alpha para");
        assert_eq!(
            core.store.get_source(&out.id).await.unwrap().status,
            SourceStatus::Partial
        );
    }

    #[tokio::test]
    async fn re_running_segmentation_replaces_rather_than_appends() {
        let core = test_core().await;
        let out = core.ingest("one\n\ntwo", "web", None).await.unwrap();
        run(&core, &out.id).await.unwrap();
        run(&core, &out.id).await.unwrap();
        assert_eq!(
            core.store.chunks_for_source(&out.id).await.unwrap().len(),
            2,
            "a retried segment job must not double the chunks"
        );
    }

    #[tokio::test]
    async fn empty_source_is_marked_failed_not_left_pending() {
        let core = test_core().await;
        let src = core
            .store
            .insert_source("\n\n  \n", "web", None)
            .await
            .unwrap();
        run(&core, &src.id).await.unwrap();
        assert_eq!(
            core.store.get_source(&src.id).await.unwrap().status,
            SourceStatus::Failed
        );
    }

    #[tokio::test]
    async fn source_spans_are_shifted_into_document_coordinates() {
        // The chunker sees one window at a time and numbers lines from 1.
        // Without the shift, every chunk in window two would point at the
        // wrong part of the raw text.
        let core = test_core().await;
        let body = multi_window_body();
        let out = core.ingest(&body, "web", None).await.unwrap();
        assert!(window_count(&core, &body) > 1);
        run(&core, &out.id).await.unwrap();

        let chunks = core.store.chunks_for_source(&out.id).await.unwrap();
        let last = chunks.last().unwrap();
        let span = last.source_span.as_ref().expect("span must be recorded");
        assert!(
            span.start_line > 1,
            "later chunks must not all claim to start at line 1"
        );
    }
}
