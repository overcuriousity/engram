use crate::core::Core;
use crate::error::Result;
use crate::infer::budget::segment_tokens;
use crate::infer::split::{segment_text, split_into_segments};
use crate::store::artifacts::{CorpusSpan, NewArtifact};
use crate::store::corpora::CorpusStatus;
use crate::store::jobs::Stage;
use crate::store::segments::SegmentState;

fn prompt_overhead(core: &Core) -> usize {
    core.counter.count(crate::infer::prompt::SYNTHESIZER_SYSTEM) + 200
}

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
        if paraphrased(&chunks, &text) {
            tracing::warn!(
                corpus_id,
                window = w.idx,
                "literals missing; re-segmenting once"
            );
            chunks = core.synthesizer.segment(&text).await?;
        }

        for c in &mut chunks {
            let hinted = c
                .corpus_lines
                .map(|(a, b)| (a + w.start_line - 1, b + w.start_line - 1));
            let span = crate::infer::verify::locate_span(&c.text, &text, w.start_line)
                .or(hinted)
                .unwrap_or((w.start_line, w.end_line));
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

fn paraphrased(chunks: &[crate::infer::ProposedArtifact], window: &str) -> bool {
    chunks
        .iter()
        .any(|c| !crate::infer::verify::missing_literals(&c.text, &[], window).is_empty())
}

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

    let cov = recompute_coverage(core, corpus_id).await?;
    if cov < crate::infer::verify::LOW_COVERAGE {
        tracing::warn!(
            corpus_id,
            coverage = cov,
            "most of this source is unclaimed"
        );
    }

    if src.title_hint.is_none() {
        let titles: Vec<String> = chunks.iter().filter_map(|c| c.title.clone()).collect();
        match core.synthesizer.title(&src.raw_text, &titles).await {
            Ok(Some(t)) => core.store.set_title_hint(corpus_id, &t).await?,
            Ok(None) => {}
            Err(e) => tracing::warn!(corpus_id, error = %e, "could not name this corpus"),
        }
    }

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
        let core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        run(&core, &out.id).await.unwrap();

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

        core.store.claim_job().await.unwrap();
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
        let mut core = test_core().await;
        let body = multi_segment_body();
        let out = core.ingest(&body, "web", None).await.unwrap();
        assert!(segment_count(&core, &body) > 2);
        core.synthesizer = std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::failing("502"));

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
