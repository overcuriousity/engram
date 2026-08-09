use crate::core::Core;
use crate::error::Result;
use crate::infer::budget::window_tokens;
use crate::infer::split::{split_into_windows, structural_chunks, window_text};
use crate::store::chunks::{NewChunk, SourceSpan};
use crate::store::jobs::Stage;
use crate::store::sources::SourceStatus;
use crate::store::windows::WindowState;

/// Tokens consumed by the system prompt and scaffolding. Measured from the
/// real prompt rather than guessed.
fn prompt_overhead(core: &Core) -> usize {
    core.counter.count(crate::infer::prompt::CHUNKER_SYSTEM) + 200
}

/// LLM-assisted segmentation, one window at a time.
///
/// The window rows are the job's memory. A window that succeeds is written and
/// marked `done` before the next is attempted, so an error here costs the
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
        // The model was told to keep commands, paths and flags verbatim. If it
        // did not, one more attempt usually gets it right; a second failure is
        // stored with a flag rather than dropped, because a visible warning
        // beats losing the chapter.
        if paraphrased(&chunks, &text) {
            tracing::warn!(
                source_id,
                window = w.idx,
                "literals missing; re-segmenting once"
            );
            chunks = core.chunker.segment(&text).await?;
        }

        // Line numbers come back relative to the window, so shift them into
        // the coordinates of the original document — and no further. A span
        // outside its own window is nonsense the detail pane would render as
        // the wrong text, so clamp it here and flag it below.
        // Only a span the model asserted can be wrong about where the chunk
        // came from. One this job derived matched by construction, and one that
        // fell back to the window claims nothing in particular — checking
        // either against the chunk's own text just invents warnings.
        let mut spans = Vec::with_capacity(chunks.len());
        for c in &mut chunks {
            let (shifted, origin) = match c.source_lines {
                Some((a, b)) => (
                    (a + w.start_line - 1, b + w.start_line - 1),
                    SpanOrigin::Model,
                ),
                // The model omits `source_lines` more often than not. The whole
                // window is an honest answer and a useless one — the pane would
                // mark every line as the span — so look for the chunk's own
                // lines in the window first.
                None => match crate::infer::verify::locate_span(&c.text, &text, w.start_line) {
                    Some(found) => (found, SpanOrigin::Derived),
                    None => ((w.start_line, w.end_line), SpanOrigin::Window),
                },
            };
            let clamped = (
                shifted.0.clamp(w.start_line, w.end_line),
                shifted.1.clamp(w.start_line, w.end_line),
            );
            // Clamping erases the evidence, so record the move before it happens.
            spans.push(if origin == SpanOrigin::Model && clamped != shifted {
                SpanOrigin::Clamped
            } else {
                origin
            });
            c.source_lines = Some(if clamped.0 <= clamped.1 {
                clamped
            } else {
                (w.start_line, w.end_line)
            });
        }

        let written =
            write_window_chunks(core, source_id, w.idx, proposed_to_new(w.idx, chunks)).await?;
        flag_unverified(core, &written, &spans, &text, &src.raw_text).await?;
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
) -> Result<Vec<crate::store::chunks::Chunk>> {
    let old = core
        .store
        .chunk_ids_for_window(source_id, window_idx)
        .await?;
    if !old.is_empty() {
        core.vectors.delete_chunks(&old).await?;
        for id in &old {
            core.store.delete_chunk(id).await?;
        }
    }
    core.store.insert_chunks(source_id, &new).await
}

/// Where a chunk's stored span came from, which decides whether it is worth
/// doubting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpanOrigin {
    /// The model said so, and nothing had to be corrected.
    Model,
    /// The model said so and named lines outside its own window.
    Clamped,
    /// Recovered here by matching the chunk's lines against the window.
    Derived,
    /// Nothing to go on; the span is the window itself.
    Window,
}

/// Did any proposed chunk lose a literal its window contains?
fn paraphrased(chunks: &[crate::infer::ProposedChunk], window: &str) -> bool {
    chunks
        .iter()
        .any(|c| !crate::infer::verify::missing_literals(&c.text, window).is_empty())
}

/// Mark what verification could not vouch for. The chunk is kept — a warning
/// the reader can see beats a chapter silently missing from the base.
async fn flag_unverified(
    core: &Core,
    written: &[crate::store::chunks::Chunk],
    // Per chunk, where its stored span came from.
    spans: &[SpanOrigin],
    window_body: &str,
    raw_text: &str,
) -> Result<()> {
    use crate::infer::verify;

    for (i, c) in written.iter().enumerate() {
        let mut flags = Vec::new();
        let mut detail: Option<String> = None;

        let missing = verify::missing_literals(&c.text, window_body);
        if let Some(first) = missing.first() {
            flags.push(verify::FLAG_LITERALS.to_string());
            detail = Some(format!("missing literal: {first}"));
            tracing::warn!(chunk_id = %c.id, literal = %first, "literal not found in source window");
        }

        // A derived span matched by construction and a window span claims
        // nothing in particular; only what the model asserted can be wrong.
        let origin = spans.get(i).copied().unwrap_or(SpanOrigin::Window);
        if let Some(span) = &c.source_span
            && matches!(origin, SpanOrigin::Model | SpanOrigin::Clamped)
        {
            let claimed = window_text(raw_text, span.start_line, span.end_line);
            if origin == SpanOrigin::Clamped || !verify::span_is_plausible(&c.text, &claimed) {
                flags.push(verify::FLAG_SPAN.to_string());
                detail.get_or_insert_with(|| {
                    format!(
                        "span {}–{} does not match the chunk",
                        span.start_line, span.end_line
                    )
                });
                tracing::warn!(chunk_id = %c.id, "chunk span does not match the lines it claims");
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

/// Everything that can only be decided once every window has resolved:
/// continuous ordinals, the source's status, and the single batched embed job.
pub async fn finish(core: &Core, source_id: &str) -> Result<()> {
    let src = core.store.get_source(source_id).await?;
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

    // How much of the source ended up inside a chunk. A source where the
    // segmenter quietly dropped half a chapter used to look identical to one
    // where it did not.
    let spans: Vec<(i64, i64)> = chunks
        .iter()
        .filter_map(|c| c.source_span.as_ref().map(|s| (s.start_line, s.end_line)))
        .collect();
    let cov = crate::infer::verify::coverage(&spans, &src.raw_text);
    core.store.set_source_coverage(source_id, cov).await?;
    if cov < crate::infer::verify::LOW_COVERAGE {
        tracing::warn!(
            source_id,
            coverage = cov,
            "most of this source is unclaimed"
        );
    }

    // One job for the whole source: every chunk was just written, and embedding
    // them together is one inference call instead of `chunks.len()`.
    core.store
        .enqueue(Stage::Embed, "source", source_id)
        .await?;
    let status = if degraded {
        SourceStatus::Partial
    } else {
        SourceStatus::Embedding
    };
    core.store.set_source_status(source_id, status).await?;
    tracing::info!(source_id, chunks = chunks.len(), degraded, "segmented");
    Ok(())
}

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
                // `structural_chunks` numbers from the window's first line, so
                // the offset is the same shift the LLM path applies.
                source_span: Some(SourceSpan {
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

        // One embed job for the whole source, not one per chunk: the point of
        // batching is a single inference call.
        core.store.claim_job().await.unwrap(); // segment
        let mut embed_jobs = Vec::new();
        while let Some(j) = core.store.claim_job().await.unwrap() {
            if j.stage == Stage::Embed {
                embed_jobs.push(j);
            }
        }
        assert_eq!(embed_jobs.len(), 1, "expected one batched embed job");
        assert_eq!(embed_jobs[0].target_kind, "source");
        assert_eq!(embed_jobs[0].target_id, out.id);
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

        fallback_pending_windows(&core, &out.id, "endpoint down")
            .await
            .unwrap();
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

    const COMMAND_BODY: &str = "\
Unmount the device first.

    dd if=archlinux.iso of=/dev/sdX bs=4M oflag=sync status=progress

Then run sync.";

    #[tokio::test]
    async fn a_paraphrased_literal_is_re_segmented_once_and_then_accepted() {
        let mut core = test_core().await;
        let chunker = std::sync::Arc::new(crate::infer::fake::ParaphrasingChunker::recovering(
            "oflag=sync ",
        ));
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
        core.chunker = std::sync::Arc::new(crate::infer::fake::ParaphrasingChunker::persistent(
            "oflag=sync ",
        ));
        let out = core.ingest(COMMAND_BODY, "web", None).await.unwrap();

        run(&core, &out.id).await.unwrap();

        let chunks = core.store.chunks_for_source(&out.id).await.unwrap();
        assert!(!chunks.is_empty(), "flagged chunks are still stored");
        let flagged: Vec<_> = chunks
            .iter()
            .filter(|c| {
                c.flags
                    .iter()
                    .any(|f| f == crate::infer::verify::FLAG_LITERALS)
            })
            .collect();
        assert_eq!(flagged.len(), 1);
        assert!(
            flagged[0]
                .flag_detail
                .as_deref()
                .unwrap()
                .contains("dd if="),
            "the detail must name the literal that went missing"
        );
    }

    #[tokio::test]
    async fn a_span_outside_its_window_is_clamped_and_flagged() {
        let mut core = test_core().await;
        core.chunker = std::sync::Arc::new(crate::infer::fake::LyingSpanChunker);
        let out = core
            .ingest("first para\n\nsecond para", "web", None)
            .await
            .unwrap();

        run(&core, &out.id).await.unwrap();

        let c = &core.store.chunks_for_source(&out.id).await.unwrap()[0];
        let span = c.source_span.as_ref().unwrap();
        assert!(
            span.start_line >= 1 && span.end_line <= 3,
            "span must be clamped to the window"
        );
        assert!(c.flags.iter().any(|f| f == crate::infer::verify::FLAG_SPAN));
    }

    #[tokio::test]
    async fn coverage_is_recorded_on_the_source() {
        let core = test_core().await;
        let out = core
            .ingest("first para\n\nsecond para", "web", None)
            .await
            .unwrap();
        run(&core, &out.id).await.unwrap();
        let cov = core
            .store
            .get_source(&out.id)
            .await
            .unwrap()
            .coverage
            .unwrap();
        assert!(cov > 0.0 && cov <= 1.0);
    }

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
        // and a later one raises — the shape a flaky endpoint produces.
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
            !core
                .store
                .chunks_for_source(&out.id)
                .await
                .unwrap()
                .is_empty(),
            "chunks from the successful windows must survive the error"
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
