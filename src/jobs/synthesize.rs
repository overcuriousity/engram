use crate::core::Core;
use crate::error::Result;
use crate::infer::budget::segment_tokens;
use crate::infer::split::{segment_text, split_into_segments};
use crate::store::artifacts::{CorpusSpan, NewArtifact};
use crate::store::corpora::CorpusStatus;
use crate::store::jobs::Stage;
use crate::store::segments::SegmentState;

/// Tokens consumed by the system prompt and scaffolding. Measured from the
/// real prompt rather than guessed.
fn prompt_overhead(core: &Core) -> usize {
    core.counter.count(crate::infer::prompt::SYNTHESIZER_SYSTEM) + 200
}

/// LLM-assisted segmentation, one window at a time.
///
/// The window rows are the job's memory. A window that succeeds is written and
/// marked `done` before the next is attempted, so an error here costs the
/// windows that had not started yet and nothing else — the job retries and
/// resumes from the first pending window.
pub async fn run(core: &Core, corpus_id: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    core.store
        .set_corpus_status(corpus_id, CorpusStatus::Segmenting)
        .await?;

    let windows = split_into_segments(
        &src.raw_text,
        &core.counter,
        segment_tokens(core.synthesizer.budget(), prompt_overhead(core)),
    );

    if windows.is_empty() {
        tracing::warn!(corpus_id, "source has no usable text");
        core.store
            .set_corpus_status(corpus_id, CorpusStatus::Failed)
            .await?;
        return Ok(());
    }

    let spans: Vec<(i64, i64)> = windows.iter().map(|w| (w.start_line, w.end_line)).collect();
    core.store.upsert_segments(corpus_id, &spans).await?;

    for w in core.store.pending_segments(corpus_id).await? {
        core.store.bump_segment_attempts(corpus_id, w.idx).await?;
        let text = segment_text(&src.raw_text, w.start_line, w.end_line);

        let mut chunks = core.synthesizer.segment(&text).await?;
        // The model was told to keep commands, paths and flags verbatim. If it
        // did not, one more attempt usually gets it right; a second failure is
        // stored with a flag rather than dropped, because a visible warning
        // beats losing the chapter.
        if paraphrased(&chunks, &text) {
            tracing::warn!(
                corpus_id,
                window = w.idx,
                "literals missing; re-segmenting once"
            );
            chunks = core.synthesizer.segment(&text).await?;
        }

        // The span is ours to compute.
        //
        // Asking the model for `corpus_lines`, checking the answer, and having
        // a third outcome for a claim that fails the check produced a flag on
        // the artifact and a button offering to re-synthesise an entire segment
        // over a line number. Since `locate_span` finds an artifact's own text
        // even where the source is hard-wrapped and synthesis reflowed it, the
        // claim is worth what it is: a hint for the case where nothing matches
        // at all. Nothing here can disagree with the artifact, so nothing here
        // has anything to report.
        for c in &mut chunks {
            let hinted = c
                .corpus_lines
                .map(|(a, b)| (a + w.start_line - 1, b + w.start_line - 1));
            let span = crate::infer::verify::locate_span(&c.text, &text, w.start_line)
                .or(hinted)
                .unwrap_or((w.start_line, w.end_line));
            // A span outside its own window would render as the wrong text.
            let clamped = (
                span.0.clamp(w.start_line, w.end_line),
                span.1.clamp(w.start_line, w.end_line),
            );
            c.corpus_lines = Some(if clamped.0 <= clamped.1 {
                clamped
            } else {
                (w.start_line, w.end_line)
            });
        }

        let written =
            write_segment_artifacts(core, corpus_id, w.idx, proposed_to_new(w.idx, chunks)).await?;
        flag_unverified(core, &written, &text).await?;
        core.store
            .set_segment_state(corpus_id, w.idx, SegmentState::Done, None)
            .await?;

        // Idle between windows if asked to. A long source is otherwise minutes
        // of unbroken generation, which on a desktop GPU is a sustained load
        // rather than a burst. The window is already committed, so a pause here
        // costs nothing if the process dies during it.
        let cooldown = core.synthesizer.cooldown();
        if !cooldown.is_zero() {
            tracing::debug!(
                secs = cooldown.as_secs(),
                "cooling down before the next window"
            );
            tokio::time::sleep(cooldown).await;
        }
    }

    finish(core, corpus_id).await
}

/// Replace the chunks of one window. Same "replace, never append" guarantee as
/// before; the key is the window rather than the whole source, so a retry of
/// window 4 cannot disturb windows 0 to 3.
async fn write_segment_artifacts(
    core: &Core,
    corpus_id: &str,
    segment_idx: i64,
    new: Vec<NewArtifact>,
) -> Result<Vec<crate::store::artifacts::Chunk>> {
    let old = core
        .store
        .artifact_ids_for_segment(corpus_id, segment_idx)
        .await?;
    if !old.is_empty() {
        core.vectors.delete_artifacts(&old).await?;
        for id in &old {
            core.store.delete_artifact(id).await?;
        }
    }
    core.store.insert_artifacts(corpus_id, &new).await
}

/// Did any proposed chunk lose a literal its window contains?
///
/// The chunk body only, deliberately — this gates a second synthesis call over
/// the whole window, the most expensive thing here. A caveat is prose the model
/// is asked to write freely ("only on `/dev/sd*` devices", "requires `sudo`"),
/// so a path it names in passing need not appear verbatim in the source, and
/// re-synthesising a window over one is paying the largest cost in the system
/// for the smallest reason. `flag_unverified` still checks caveats: a command
/// invented in one is flagged for the reader like any other.
fn paraphrased(chunks: &[crate::infer::ProposedArtifact], window: &str) -> bool {
    chunks
        .iter()
        .any(|c| !crate::infer::verify::missing_literals(&c.text, &[], window).is_empty())
}

/// Mark what verification could not vouch for. The chunk is kept — a warning
/// the reader can see beats a chapter silently missing from the base.
///
/// One check, not two. A span is derived rather than adjudicated, so there is
/// nothing left to disbelieve about it; what remains is the literal check,
/// which is about the text itself and speaks to whoever reads the artifact.
async fn flag_unverified(
    core: &Core,
    written: &[crate::store::artifacts::Chunk],
    segment_body: &str,
) -> Result<()> {
    use crate::infer::verify;

    for c in written {
        let mut flags = Vec::new();
        let mut detail: Option<String> = None;

        let missing = verify::missing_literals(&c.text, &c.caveats, segment_body);
        if let Some(first) = missing.first() {
            flags.push(verify::FLAG_LITERALS.to_string());
            detail = Some(format!("missing literal: {first}"));
            tracing::warn!(artifact_id = %c.id, literal = %first, "literal not found in source window");
        }

        if !flags.is_empty() {
            core.store
                .set_artifact_flags(&c.id, &flags, detail.as_deref())
                .await?;
        }
    }
    Ok(())
}

/// Measure how much of a corpus survived into its artifacts, and store it.
///
/// Pure local work over rows that are already there — no inference and no
/// vector call — so it can be re-run over a whole base whenever the measure
/// itself changes, rather than re-synthesising documents that are fine.
pub async fn recompute_coverage(core: &Core, corpus_id: &str) -> Result<f64> {
    let src = core.store.get_corpus(corpus_id).await?;
    let chunks = core.store.artifacts_for_corpus(corpus_id).await?;
    let segments = core.store.segments_for_corpus(corpus_id).await?;

    let made: Vec<(i64, i64, String)> = segments
        .iter()
        .map(|w| {
            let text = chunks
                .iter()
                .filter(|c| c.segment_idx == Some(w.idx))
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            (w.start_line, w.end_line, text)
        })
        .collect();

    // A corpus segmented before per-segment windows existed has no ranges to
    // group by; measure it as one.
    let made = if made.is_empty() {
        vec![(
            1,
            src.raw_text.lines().count() as i64,
            chunks
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        )]
    } else {
        made
    };

    let cov = crate::infer::verify::content_coverage(&src.raw_text, &made);
    core.store.set_corpus_coverage(corpus_id, cov).await?;
    Ok(cov)
}

/// Everything that can only be decided once every window has resolved:
/// continuous ordinals, the source's status, and the single batched embed job.
pub async fn finish(core: &Core, corpus_id: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    core.store.renumber_artifacts(corpus_id).await?;
    let windows = core.store.segments_for_corpus(corpus_id).await?;
    let degraded = windows.iter().any(|w| w.state != SegmentState::Done);
    let chunks = core.store.artifacts_for_corpus(corpus_id).await?;
    if chunks.is_empty() {
        core.store
            .set_corpus_status(corpus_id, CorpusStatus::Failed)
            .await?;
        return Ok(());
    }

    // How much of the source ended up inside a chunk. A source where the
    // segmenter quietly dropped half a chapter used to look identical to one
    // where it did not.
    let cov = recompute_coverage(core, corpus_id).await?;
    if cov < crate::infer::verify::LOW_COVERAGE {
        tracing::warn!(
            corpus_id,
            coverage = cov,
            "most of this source is unclaimed"
        );
    }

    // Named here rather than at capture, which makes no inference call by
    // design. The artifact titles are the cheapest description of what the
    // document turned out to be about, and they only exist now.
    //
    // A failure is logged and dropped: the corpus keeps the snippet the UI
    // falls back to, and losing a document over a missing name would be a bad
    // trade. A name given at capture is left alone — someone chose it.
    if src.title_hint.is_none() {
        let titles: Vec<String> = chunks.iter().filter_map(|c| c.title.clone()).collect();
        match core.synthesizer.title(&src.raw_text, &titles).await {
            Ok(Some(t)) => core.store.set_title_hint(corpus_id, &t).await?,
            Ok(None) => {}
            Err(e) => tracing::warn!(corpus_id, error = %e, "could not name this corpus"),
        }
    }

    // One job for the whole source: every chunk was just written, and embedding
    // them together is one inference call instead of `chunks.len()`.
    core.store
        .enqueue(Stage::Embed, "corpus", corpus_id)
        .await?;
    let status = if degraded {
        CorpusStatus::Partial
    } else {
        CorpusStatus::Embedding
    };
    core.store.set_corpus_status(corpus_id, status).await?;
    tracing::info!(corpus_id, chunks = chunks.len(), degraded, "segmented");
    Ok(())
}

/// Settle the windows a spent job leaves behind.
///
/// The model is a hard dependency: a window it will not segment stays
/// unsegmented and records why. There is no structural split to fall back on,
/// because paragraphs stored verbatim are not what the rest of the system means
/// by a chunk — no title, no category, no tags, and not rewritten to stand
/// alone — and they would compete for queries against chunks that are.
///
/// Only windows that have actually been tried get a verdict. A local endpoint
/// fails in bursts — the model is loading, or something else took the VRAM —
/// and the job's attempt count is shared by every window, so an outage during
/// window 1 must not condemn windows 2 onward that the model never saw. Those
/// go back in the queue instead. "Tried at least once" is the line rather than
/// "spent every attempt", because the attempt count belongs to the job, which
/// covers the whole source.
///
/// Returns whether windows are still waiting for their first attempt, which the
/// caller answers with a fresh job. It cannot be enqueued here: the caller's own
/// job row is keyed `(stage, target_id)`, so enqueuing the same source would
/// reuse that row and the `complete_job` that follows would close it again — the
/// untried windows would be left with nothing to come back to.
///
/// Either way the source is settled for now: whatever windows did succeed are
/// embedded and the corpus reports `partial`. Settled is not finished — a failed
/// window is still owed a model call, and the caller queues one at the backoff's
/// distance.
pub async fn fail_pending_segments(core: &Core, corpus_id: &str, reason: &str) -> Result<bool> {
    let pending = core.store.pending_segments(corpus_id).await?;
    if pending.is_empty() {
        finish(core, corpus_id).await?;
        return Ok(false);
    }

    let (tried, untried): (Vec<_>, Vec<_>) = pending
        .into_iter()
        .partition(|w| w.attempts > 0 || w.state == SegmentState::Failed);

    if !untried.is_empty() {
        tracing::info!(
            corpus_id,
            windows = untried.len(),
            "leaving untried windows queued rather than failing them"
        );
    }

    if tried.is_empty() {
        // Nothing has earned a verdict yet; the caller queues another attempt.
        return Ok(true);
    }

    for w in tried {
        core.store
            .set_segment_state(corpus_id, w.idx, SegmentState::Failed, Some(reason))
            .await?;
        tracing::warn!(
            corpus_id,
            window = w.idx,
            lines = format!("{}-{}", w.start_line, w.end_line),
            reason,
            "window could not be segmented; its lines have no chunk"
        );
    }
    // Windows still waiting for their first attempt mean the source is not
    // settled yet; finishing here would enqueue embedding for half a document.
    // A window already marked failed does not hold it up — it is owed another
    // call, not a first one, and the next job brings that.
    let untried_left = core
        .store
        .pending_segments(corpus_id)
        .await?
        .into_iter()
        .any(|w| w.state == SegmentState::Pending);
    if !untried_left {
        finish(core, corpus_id).await?;
        return Ok(false);
    }
    Ok(true)
}

fn proposed_to_new(
    segment_idx: i64,
    proposed: Vec<crate::infer::ProposedArtifact>,
) -> Vec<NewArtifact> {
    proposed
        .into_iter()
        .enumerate()
        .map(|(i, p)| NewArtifact {
            ordinal: i as i64,
            text: p.text,
            corpus_span: p.corpus_lines.map(|(a, b)| CorpusSpan {
                start_line: a,
                end_line: b,
            }),
            title: p.title,
            category: p.category,
            tags: p.tags,
            caveats: p.caveats,
            segment_idx: Some(segment_idx),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::{test_core, test_core_with_failing_synthesizer};
    use crate::store::corpora::CorpusStatus;
    use crate::store::jobs::{MAX_ATTEMPTS, Stage};

    #[tokio::test]
    async fn synthesis_names_the_corpus() {
        let core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        assert!(
            core.store
                .get_corpus(&out.id)
                .await
                .unwrap()
                .title_hint
                .is_none()
        );

        run(&core, &out.id).await.unwrap();

        let named = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(named.title_hint.as_deref(), Some("Fake title: alpha line"));
    }

    #[tokio::test]
    async fn a_name_that_was_given_at_capture_is_not_overwritten() {
        // The API still accepts a title, and a name someone chose outranks one
        // the model would have written.
        let core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", Some("My own label"))
            .await
            .unwrap();

        run(&core, &out.id).await.unwrap();

        let got = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(got.title_hint.as_deref(), Some("My own label"));
    }

    #[tokio::test]
    async fn a_capture_survives_a_synthesizer_that_will_not_name_it() {
        // The title is a nicety. Losing the document because the model would
        // not name it would be a bad trade, so the failure is logged and the
        // corpus keeps its fallback.
        let core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        run(&core, &out.id).await.unwrap();

        // A synthesizer that fails every call cannot produce artifacts either,
        // so naming is exercised through `finish` on a corpus that already has
        // them: the state a real failure leaves behind.
        let failing = test_core_with_failing_synthesizer().await;
        let hurt = failing
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        let _ = run(&failing, &hurt.id).await;
        assert!(
            failing
                .store
                .get_corpus(&hurt.id)
                .await
                .unwrap()
                .title_hint
                .is_none(),
            "a corpus the model would not name simply stays unnamed"
        );
    }

    /// A body several windows long under the fake synthesizer's budget.
    fn multi_segment_body() -> String {
        (0..400)
            .map(|i| format!("paragraph number {i} with some filler text"))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn segment_count(core: &crate::core::Core, body: &str) -> usize {
        crate::infer::split::split_into_segments(
            body,
            &core.counter,
            segment_tokens(core.synthesizer.budget(), prompt_overhead(core)),
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

        let chunks = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].ordinal, 0);
        assert_eq!(chunks[1].ordinal, 1);
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Embedding
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
        assert_eq!(embed_jobs[0].target_kind, "corpus");
        assert_eq!(embed_jobs[0].target_id, out.id);
    }

    #[tokio::test]
    async fn ordinals_stay_continuous_across_windows() {
        let core = test_core().await;
        // Large enough to exceed the fake synthesizer's window budget several
        // times over, so segmentation really does run per window.
        let body = multi_segment_body();
        let out = core.ingest(&body, "web", None).await.unwrap();
        assert!(
            segment_count(&core, &body) > 1,
            "test body must span multiple windows or it proves nothing"
        );

        run(&core, &out.id).await.unwrap();

        let chunks = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(chunks.len() > 1);
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.ordinal, i as i64, "ordinals must not restart per window");
        }
    }

    #[tokio::test]
    async fn a_segment_the_endpoint_refused_is_queued_again() {
        // The failure that lost a quarter of a document: the endpoint was
        // loading a model and returned 502 for ten minutes, the job spent its
        // attempts inside the first minute, and nothing ever tried the segment
        // again. `failed` has to mean "waiting to be tried", not "gone".
        let core = test_core_with_failing_synthesizer().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();

        for _ in 0..MAX_ATTEMPTS + 2 {
            sqlx::query("UPDATE jobs SET run_after = 0")
                .execute(&core.store.pool)
                .await
                .unwrap();
            crate::jobs::run_one(&core).await.unwrap();
        }

        assert!(
            core.store.failed_jobs(10).await.unwrap().is_empty(),
            "the corpus was abandoned"
        );
        // Directly, because `finish` also queues an embed job for the corpus
        // and `claim_job` may hand that one over first.
        let queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs
              WHERE stage = 'synthesize' AND target_id = ? AND state = 'pending'",
        )
        .bind(&out.id)
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(queued, 1, "no job is left to retry the segment");
    }

    #[tokio::test]
    async fn a_window_the_model_refuses_is_marked_failed_not_split() {
        let core = test_core_with_failing_synthesizer().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();

        let err = run(&core, &out.id).await.unwrap_err();
        assert!(
            err.retryable(),
            "a dead endpoint deserves a retry, not a verdict"
        );

        let requeue = fail_pending_segments(&core, &out.id, "endpoint down")
            .await
            .unwrap();

        assert!(!requeue, "nothing is left waiting when every window failed");
        let w = &core.store.segments_for_corpus(&out.id).await.unwrap()[0];
        assert_eq!(w.state, SegmentState::Failed);
        assert_eq!(w.last_error.as_deref(), Some("endpoint down"));

        // The point of the change: no paragraph-shaped debris competing for
        // queries against chunks that were actually written to stand alone.
        assert!(
            core.store
                .artifacts_for_corpus(&out.id)
                .await
                .unwrap()
                .is_empty(),
            "a refused window must produce no chunks at all"
        );
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Failed
        );
    }

    #[tokio::test]
    async fn re_running_segmentation_replaces_rather_than_appends() {
        let core = test_core().await;
        let out = core.ingest("one\n\ntwo", "web", None).await.unwrap();
        run(&core, &out.id).await.unwrap();
        run(&core, &out.id).await.unwrap();
        assert_eq!(
            core.store
                .artifacts_for_corpus(&out.id)
                .await
                .unwrap()
                .len(),
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
        let synthesizer = std::sync::Arc::new(
            crate::infer::fake::ParaphrasingSynthesizer::recovering("oflag=sync "),
        );
        core.synthesizer = synthesizer.clone();
        let out = core.ingest(COMMAND_BODY, "web", None).await.unwrap();

        run(&core, &out.id).await.unwrap();

        assert_eq!(synthesizer.calls(), 2, "exactly one re-segmentation");
        let chunks = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(
            chunks.iter().all(|c| c.flags.is_empty()),
            "a clean retry must leave no flag"
        );
    }

    #[tokio::test]
    async fn a_literal_the_retry_also_drops_is_stored_flagged() {
        let mut core = test_core().await;
        core.synthesizer = std::sync::Arc::new(
            crate::infer::fake::ParaphrasingSynthesizer::persistent("oflag=sync "),
        );
        let out = core.ingest(COMMAND_BODY, "web", None).await.unwrap();

        run(&core, &out.id).await.unwrap();

        let chunks = core.store.artifacts_for_corpus(&out.id).await.unwrap();
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
    async fn a_wrong_span_is_replaced_by_one_recovered_from_the_text() {
        // The model's line numbers are routinely wrong on reference documents.
        // Where the chunk still reproduces its source, the real span can be
        // found — better than flagging a chunk whose lines we can work out.
        let mut core = test_core().await;
        core.synthesizer = std::sync::Arc::new(crate::infer::fake::LyingSpanSynthesizer);
        let out = core
            .ingest("first paragraph here\n\nsecond paragraph here", "web", None)
            .await
            .unwrap();

        run(&core, &out.id).await.unwrap();

        let c = &core.store.artifacts_for_corpus(&out.id).await.unwrap()[0];
        let span = c.corpus_span.as_ref().unwrap();
        assert!(
            span.start_line >= 1 && span.end_line <= 3,
            "the recovered span must lie inside the window"
        );
        assert!(
            c.flags.is_empty(),
            "a span we corrected ourselves is not a warning for the reader"
        );
    }

    #[tokio::test]
    async fn a_wrong_span_is_never_a_review_task() {
        // A line number engram can compute itself was being asked of the model,
        // disbelieved, and turned into a queue entry whose only button spends a
        // model call on a whole segment. The span falls back to the window and
        // the reader is none the wiser.
        let mut core = test_core().await;
        core.synthesizer = std::sync::Arc::new(crate::infer::fake::HallucinatingSynthesizer);
        let out = core
            .ingest("first paragraph here\n\nsecond paragraph here", "web", None)
            .await
            .unwrap();

        run(&core, &out.id).await.unwrap();

        for c in core.store.artifacts_for_corpus(&out.id).await.unwrap() {
            assert!(
                !c.flags.iter().any(|f| f == "span_unverified"),
                "a span produced a review task: {:?}",
                c.flags
            );
            let span = c.corpus_span.expect("every artifact keeps a span");
            assert!(
                span.start_line >= 1 && span.end_line >= span.start_line,
                "{span:?}"
            );
        }
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
            .get_corpus(&out.id)
            .await
            .unwrap()
            .coverage
            .unwrap();
        assert!(cov > 0.0 && cov <= 1.0);
    }

    #[tokio::test]
    async fn a_burst_of_endpoint_failures_does_not_condemn_untried_windows() {
        // The job's attempt count is shared by every window of a source, so an
        // outage while window 0 is running used to condemn the whole rest of
        // the document without ever calling the model for it. Locally that
        // outage is usually the model still loading.
        let mut core = test_core().await;
        let body = multi_segment_body();
        let out = core.ingest(&body, "web", None).await.unwrap();
        assert!(segment_count(&core, &body) > 2);
        core.synthesizer = std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::failing("502"));

        // The endpoint refuses while the first window is running; the rest of
        // the source never gets a call at all.
        assert!(run(&core, &out.id).await.is_err());
        let requeue = fail_pending_segments(&core, &out.id, "502 Bad Gateway")
            .await
            .unwrap();

        let windows = core.store.segments_for_corpus(&out.id).await.unwrap();
        assert_eq!(
            windows
                .iter()
                .filter(|w| w.state == SegmentState::Failed)
                .count(),
            1,
            "only the window that spent its attempts may be given a verdict"
        );
        assert!(
            windows
                .iter()
                .filter(|w| w.state == SegmentState::Pending)
                .count()
                > 1,
            "untried windows must stay queued for the model"
        );

        assert!(
            requeue,
            "the untried windows need a job to come back to, and only the \
             caller can enqueue it without its own row being closed underneath"
        );
    }

    #[tokio::test]
    async fn a_source_with_untried_windows_still_has_a_job_after_a_failure() {
        // Settling the windows used to enqueue the retry itself. The queue is keyed by
        // (stage, target), so that reused the very row the worker was running,
        // and the `complete_job` that followed closed it again: the untried
        // windows were abandoned and the source sat in `segmenting` forever.
        let mut core = test_core().await;
        let body = multi_segment_body();
        let out = core.ingest(&body, "web", None).await.unwrap();
        assert!(segment_count(&core, &body) > 2);
        core.synthesizer = std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::failing("502"));

        for _ in 0..=crate::store::jobs::MAX_ATTEMPTS {
            sqlx::query("UPDATE jobs SET run_after = 0")
                .execute(&core.store.pool)
                .await
                .unwrap();
            let _ = crate::jobs::run_one(&core).await;
        }

        let windows = core.store.segments_for_corpus(&out.id).await.unwrap();
        assert!(
            windows.iter().any(|w| w.state == SegmentState::Pending),
            "this test only proves anything while windows are still untried"
        );
        // Past the backoff the last failure set, which is a delay rather than
        // the question here.
        sqlx::query("UPDATE jobs SET run_after = 0")
            .execute(&core.store.pool)
            .await
            .unwrap();
        let job = core.store.claim_job().await.unwrap();
        let job = job.expect("the untried windows were left with no job at all");
        assert_eq!(job.stage, Stage::Synthesize);
        assert_eq!(job.target_id, out.id);
    }

    #[tokio::test]
    async fn windows_that_succeeded_keep_their_chunks_when_a_later_one_fails() {
        let mut core = test_core().await;
        let body = format!("{}\n\nSTOPHERE marker paragraph\n", multi_segment_body());
        let out = core.ingest(&body, "web", None).await.unwrap();
        core.synthesizer =
            std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::failing_on("STOPHERE"));

        // First pass records the good windows and raises on the bad one.
        assert!(run(&core, &out.id).await.is_err());
        let llm_artifacts = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .len();
        assert!(llm_artifacts > 0);

        fail_pending_segments(&core, &out.id, "endpoint refused the window")
            .await
            .unwrap();

        let windows = core.store.segments_for_corpus(&out.id).await.unwrap();
        assert!(
            windows.iter().any(|w| w.state == SegmentState::Done),
            "successful windows must stay done"
        );
        let failed: Vec<_> = windows
            .iter()
            .filter(|w| w.state == SegmentState::Failed)
            .collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(
            failed[0].last_error.as_deref(),
            Some("endpoint refused the window")
        );

        assert_eq!(
            core.store
                .artifacts_for_corpus(&out.id)
                .await
                .unwrap()
                .len(),
            llm_artifacts,
            "a failed window must not disturb the chunks another window earned"
        );
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Partial,
            "a window with no chunks makes the source partial, not ready"
        );
    }

    #[tokio::test]
    async fn a_cooldown_paces_the_windows_it_segments() {
        let mut core = test_core().await;
        let body = multi_segment_body();
        let out = core.ingest(&body, "web", None).await.unwrap();
        let windows = segment_count(&core, &body);
        assert!(windows > 1);

        let pause = std::time::Duration::from_millis(40);
        core.synthesizer = std::sync::Arc::new(crate::infer::fake::PacedSynthesizer::new(pause));

        let started = std::time::Instant::now();
        run(&core, &out.id).await.unwrap();
        assert!(
            started.elapsed() >= pause * (windows as u32 - 1),
            "each window but the last should have been followed by a pause"
        );
    }

    #[tokio::test]
    async fn re_segmenting_replaces_chunks_written_before_windows_existed() {
        // Chunks from before the window column was added carry no window, so
        // the per-window delete could not see them and a re-segmentation
        // appended a second copy of the whole source beside the first.
        let core = test_core().await;
        let out = core
            .ingest("one para\n\ntwo para", "web", None)
            .await
            .unwrap();
        run(&core, &out.id).await.unwrap();
        let before = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .len();

        // What an older database holds: chunks with no window, and no window
        // rows to resume from.
        sqlx::query("UPDATE artifacts SET segment_idx = NULL WHERE corpus_id = ?")
            .bind(&out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM segments WHERE corpus_id = ?")
            .bind(&out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();

        run(&core, &out.id).await.unwrap();

        assert_eq!(
            core.store
                .artifacts_for_corpus(&out.id)
                .await
                .unwrap()
                .len(),
            before,
            "the pre-window chunks were left in place and duplicated"
        );
    }

    #[tokio::test]
    async fn a_second_run_does_not_re_segment_windows_that_finished() {
        let core = test_core().await;
        let body = multi_segment_body();
        let out = core.ingest(&body, "web", None).await.unwrap();
        assert!(segment_count(&core, &body) > 1);

        run(&core, &out.id).await.unwrap();
        let (resolved, total) = core.store.segment_progress(&out.id).await.unwrap();
        assert_eq!(resolved, total, "every window should have resolved");

        let before = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .len();
        // Nothing is pending, so a second run must be a no-op rather than a
        // second full pass that doubles the chunk count.
        run(&core, &out.id).await.unwrap();
        let after = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .len();
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn a_failing_window_leaves_earlier_windows_intact() {
        // Fails only on the window containing the marker, so window 0 succeeds
        // and a later one raises — the shape a flaky endpoint produces.
        let mut core = test_core().await;
        let body = format!("{}\n\nSTOPHERE marker paragraph\n", multi_segment_body());
        let out = core.ingest(&body, "web", None).await.unwrap();
        core.synthesizer =
            std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::failing_on("STOPHERE"));

        let err = run(&core, &out.id).await.unwrap_err();
        assert!(err.retryable(), "a synthesizer error must stay retryable");

        let (resolved, total) = core.store.segment_progress(&out.id).await.unwrap();
        assert!(resolved > 0, "windows before the failure must be recorded");
        assert!(resolved < total, "the failing window must stay pending");
        assert!(
            !core
                .store
                .artifacts_for_corpus(&out.id)
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
            .insert_corpus("\n\n  \n", "web", None)
            .await
            .unwrap();
        run(&core, &src.id).await.unwrap();
        assert_eq!(
            core.store.get_corpus(&src.id).await.unwrap().status,
            CorpusStatus::Failed
        );
    }

    #[tokio::test]
    async fn source_spans_are_shifted_into_document_coordinates() {
        // The synthesizer sees one window at a time and numbers lines from 1.
        // Without the shift, every chunk in window two would point at the
        // wrong part of the raw text.
        let core = test_core().await;
        let body = multi_segment_body();
        let out = core.ingest(&body, "web", None).await.unwrap();
        assert!(segment_count(&core, &body) > 1);
        run(&core, &out.id).await.unwrap();

        let chunks = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        let last = chunks.last().unwrap();
        let span = last.corpus_span.as_ref().expect("span must be recorded");
        assert!(
            span.start_line > 1,
            "later chunks must not all claim to start at line 1"
        );
    }
}
