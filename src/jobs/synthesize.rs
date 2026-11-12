use crate::core::Core;
use crate::error::Result;
use crate::infer::budget::segment_tokens;
use crate::store::artifacts::Provenance;
use crate::store::corpora::CorpusStatus;
use crate::store::jobs::Stage;
use crate::store::segments::SegmentState;

/// Measured from the prompt in the corpus's own language: the translations are comparable in bytes,
/// not tokens, and an English-sized window overflows a Russian context.
pub(super) fn prompt_overhead(core: &Core, lang: crate::infer::lang::Lang) -> usize {
    core.counter
        .count(crate::infer::prompt::synthesizer_system(lang))
        + 200
}

/// The verbatim capture: windows and passages are written and embedded first. No inference call
/// here.
pub async fn plan(core: &Core, corpus_id: &str) -> Result<()> {
    crate::jobs::passages::capture_verbatim(core, corpus_id).await
}

/// The unit promotion reads and coverage is measured against. `None` where the context cannot hold
/// a prompt beside the smallest segment ([`segment_tokens`]).
pub fn segment_budget(core: &Core, lang: crate::infer::lang::Lang) -> Option<usize> {
    match &core.synthesizer {
        Some(s) => segment_tokens(s.budget(), prompt_overhead(core, lang)),
        // The embed-only floor: sized as a modest local model would read, so a synthesizer
        // configured later can promote the windows.
        None => Some(UNSYNTHESIZED_WINDOW_TOKENS),
    }
}

pub const UNSYNTHESIZED_WINDOW_TOKENS: usize = 8192;

/// One sentence for three call sites, so they agree about whose fault it is.
pub fn no_budget_reason(core: &Core, lang: crate::infer::lang::Lang) -> String {
    let Some(b) = core.synthesizer.as_ref().map(|s| s.budget()) else {
        return "there is no [infer.synthesize] to size a window against".to_string();
    };
    format!(
        "the synthesizer's context of {} tokens does not leave room for a prompt \
         ({} tokens of instructions and context here) and a segment of at least {} \
         beside it — raise infer.synthesize.context_tokens",
        b.context_tokens,
        prompt_overhead(core, lang) + b.context.total(),
        crate::infer::budget::MIN_SEGMENT_TOKENS,
    )
}

/// Local work over stored rows only, so it can be re-run over a whole base when the measure
/// changes.
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

    // A corpus from before per-segment windows is measured as one. The note is left out: it was not
    // made from the source.
    let made = if made.is_empty() {
        vec![(
            1,
            src.raw_text.lines().count() as i64,
            chunks
                .iter()
                .filter(|c| c.provenance != Provenance::Note)
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        )]
    } else {
        made
    };

    let cov = crate::infer::verify::content_coverage(&src.raw_text, &made);
    core.store.set_corpus_coverage(corpus_id, Some(cov)).await?;
    Ok(cov)
}

/// Continuous ordinals, the source's status, and the single batched embed job, once every window
/// has resolved.
pub async fn finish(core: &Core, corpus_id: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    core.store.renumber_artifacts(corpus_id).await?;
    let windows = core.store.segments_for_corpus(corpus_id).await?;
    let degraded = windows
        .iter()
        .any(|w| !matches!(w.state, SegmentState::Done | SegmentState::Verbatim));
    let chunks = core.store.artifacts_for_corpus(corpus_id).await?;
    // The note is the operator's own sentence, not something read; counting it would settle an
    // unread scan `ready`.
    if chunks.iter().all(|c| c.provenance == Provenance::Note) {
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

    // Armed once: settling runs per resolved window, and re-arming would reset the title unit's
    // attempts. A name given at capture is left alone.
    if src.title_hint.is_none() && !core.store.has_job(Stage::Title, corpus_id).await? {
        core.store
            .enqueue(Stage::Title, "corpus", corpus_id)
            .await?;
    }

    // Idle-only: re-arming a running embed puts a second worker in the batch; a pending one would
    // wind back its backoff and `seq`.
    core.store
        .rearm_idle_seq(Stage::Embed, "corpus", corpus_id, 0)
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

/// A name is decoration: the corpus keeps its fallback snippet, so `run_one` closes this job once
/// its attempts are spent.
pub async fn run_title(core: &Core, corpus_id: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    if src.title_hint.is_some() {
        return Ok(());
    }
    let titles: Vec<String> = core
        .store
        .artifacts_for_corpus(corpus_id)
        .await?
        .iter()
        .filter_map(|c| c.title.clone())
        .collect();

    let Some(synth) = &core.synthesizer else {
        return Ok(());
    };
    let permit = core.gate.background().await;
    let named = synth
        .title(
            &src.raw_text,
            &titles,
            crate::infer::lang::of_corpus(&src.metadata),
        )
        .await;
    permit.finished();
    match named {
        Ok(Some(t)) => {
            core.store.set_title_hint(corpus_id, &t).await?;
            Ok(())
        }
        // No opinion about titles is not a failure, nor worth another call.
        Ok(None) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Plan a corpus and work its queue to a standstill; the backoff is wound back each round.
#[cfg(test)]
pub async fn segment_all(core: &Core, corpus_id: &str) {
    plan(core, corpus_id).await.unwrap();
    // Capture arms synthesis only for one-window corpora, so arm every verbatim window as promotion
    // would.
    for w in core.store.segments_for_corpus(corpus_id).await.unwrap() {
        if w.state == SegmentState::Verbatim {
            core.store
                .reset_segment(corpus_id, w.idx, true)
                .await
                .unwrap();
            core.store
                .rearm_idle_seq(
                    Stage::SegmentWindow,
                    "segment",
                    &crate::jobs::window::unit_target(corpus_id, w.idx),
                    w.idx,
                )
                .await
                .unwrap();
        }
    }
    for _ in 0..500 {
        // Embedding is held back: several tests check the state before it runs.
        sqlx::query!(
            "UPDATE jobs SET run_after = CASE WHEN stage = 'embed' THEN ? ELSE 0 END
              WHERE state = 'pending'",
            crate::store::now() + 86_400
        )
        .execute(&core.store.control.pool)
        .await
        .unwrap();

        if !crate::jobs::run_one(core).await.unwrap_or(false) {
            break;
        }
        // A failed window keeps its unit until its budget is gone; only then has `settle` had its
        // last word.
        let pending: i64 = sqlx::query_scalar!(
            "SELECT count(*) FROM jobs WHERE stage = 'segment_window' AND state != 'done'"
        )
        .fetch_one(&core.store.control.pool)
        .await
        .unwrap_or(0);
        if pending == 0 {
            break;
        }
    }

    // Settling arms the title unit and a fresh embed `run_after`, so the hold is reapplied.
    for _ in 0..20 {
        sqlx::query!(
            "UPDATE jobs SET run_after = ? WHERE stage = 'embed' AND state = 'pending'",
            crate::store::now() + 86_400
        )
        .execute(&core.store.control.pool)
        .await
        .unwrap();
        if !crate::jobs::run_one(core).await.unwrap_or(false) {
            break;
        }
    }

    sqlx::query!("UPDATE jobs SET run_after = 0 WHERE stage = 'embed'")
        .execute(&core.store.control.pool)
        .await
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::corpora::CorpusStatus;

    #[tokio::test]
    async fn the_window_is_measured_against_the_prompt_its_language_will_be_sent() {
        let core = test_core().await;
        for l in crate::infer::lang::Lang::ALL {
            assert_eq!(
                prompt_overhead(&core, l),
                core.counter
                    .count(crate::infer::prompt::synthesizer_system(l))
                    + 200,
                "{} was measured against another language's prompt",
                l.tag()
            );
        }
        // The estimator counts characters; a real tokenizer widens the gap further.
        assert_ne!(
            prompt_overhead(&core, crate::infer::lang::Lang::Ru),
            prompt_overhead(&core, crate::infer::lang::Lang::En),
            "the Russian prompt and the English one measured the same"
        );
    }

    #[tokio::test]
    async fn re_planning_does_not_wind_back_a_queued_windows_attempts() {
        // A stale plan re-arming its spent units from zero would keep a window forever young, and
        // the document in `segmenting`.
        let core = test_core().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();
        plan(&core, &out.id).await.unwrap();
        sqlx::query!("UPDATE jobs SET attempts = 4 WHERE stage = 'segment_window'")
            .execute(&core.store.control.pool)
            .await
            .unwrap();

        plan(&core, &out.id).await.unwrap();

        let attempts: Vec<i64> =
            sqlx::query_scalar!("SELECT attempts FROM jobs WHERE stage = 'segment_window'")
                .fetch_all(&core.store.control.pool)
                .await
                .unwrap();
        assert!(
            attempts.iter().all(|&a| a == 4),
            "re-planning reset a unit that was already queued: {attempts:?}"
        );
    }

    fn context_budget(opening: usize, overlap: usize) -> crate::infer::SynthesisBudget {
        crate::infer::SynthesisBudget {
            context_tokens: 4096,
            max_output_tokens: 100_000,
            output_ratio: 1.0,
            context: crate::infer::context::ContextBudget {
                opening,
                overlap,
                neighbors: 0,
            },
        }
    }

    async fn job_row(core: &Core, stage: Stage, target: &str) -> (String, i64, i64, i64) {
        sqlx::query_as(
            "SELECT state, attempts, run_after, seq FROM jobs WHERE stage = ? AND target_id = ?",
        )
        .bind(stage.as_str())
        .bind(target)
        .fetch_one(&core.store.control.pool)
        .await
        .unwrap()
    }

    async fn segmented(core: &Core) -> String {
        let lines: Vec<String> = (0..40)
            .map(|i| format!("body line {i} with enough words to cost real tokens"))
            .collect();
        let src = core
            .store
            .insert_corpus(&lines.join("\n"), "web", None)
            .await
            .unwrap();
        segment_all(core, &src.id).await;
        src.id
    }

    #[tokio::test]
    async fn settling_again_does_not_disturb_an_embed_job_a_worker_is_inside() {
        // Re-arming a running embed put a second worker on the same batch and left the corpus
        // half-embedded.
        let core = test_core().await;
        let id = segmented(&core).await;

        sqlx::query!(
            "UPDATE jobs SET state = 'running', claimed_at = ? WHERE stage = 'embed'",
            crate::store::now()
        )
        .execute(&core.store.control.pool)
        .await
        .unwrap();

        finish(&core, &id).await.unwrap();

        let (state, ..) = job_row(&core, Stage::Embed, &id).await;
        assert_eq!(state, "running", "a settle re-armed an embed job mid-call");
    }

    #[tokio::test]
    async fn settling_again_does_not_reset_a_backing_off_embed_job() {
        // Winding a queued job back gains nothing and costs its backoff and `seq`.
        let core = test_core().await;
        let id = segmented(&core).await;

        let later = crate::store::now() + 3600;
        sqlx::query!(
            "UPDATE jobs SET state = 'pending', attempts = 4, run_after = ?, seq = 7 WHERE stage = 'embed'",
            later
        )
            .execute(&core.store.control.pool)
            .await
            .unwrap();

        finish(&core, &id).await.unwrap();

        let (state, attempts, run_after, seq) = job_row(&core, Stage::Embed, &id).await;
        assert_eq!(
            (state.as_str(), attempts, run_after, seq),
            ("pending", 4, later, 7),
            "a settle wound a queued embed job back to the front"
        );
    }

    #[tokio::test]
    async fn settling_after_an_embed_finished_arms_it_again() {
        let core = test_core().await;
        let id = segmented(&core).await;

        sqlx::query!("UPDATE jobs SET state = 'done' WHERE stage = 'embed'")
            .execute(&core.store.control.pool)
            .await
            .unwrap();

        finish(&core, &id).await.unwrap();

        let (state, ..) = job_row(&core, Stage::Embed, &id).await;
        assert_eq!(state, "pending", "new chunks were left unembedded");
    }

    #[tokio::test]
    async fn a_window_is_never_sent_over_its_own_budget() {
        // A can't-happen check: `split_into_segments` floors window size, and the failure would
        // spin against the endpoint forever.
        let core = crate::core::test_support::test_core().await;
        let budget = segment_budget(&core, crate::infer::lang::Lang::En).expect("a window");

        let lines: Vec<String> = (0..400)
            .map(|i| format!("body line {i} with enough words to cost real tokens"))
            .collect();
        // The second input has no line boundary to cut on: it fails if the floor is removed.
        for raw in [lines.join("\n"), "word ".repeat(8000)] {
            let src = core.store.insert_corpus(&raw, "web", None).await.unwrap();

            segment_all(&core, &src.id).await;

            let segments = core.store.segments_for_corpus(&src.id).await.unwrap();
            assert!(!segments.is_empty(), "a corpus must produce windows");
            for s in segments {
                // The stored text is what the window is sent as; re-deriving it from the line range
                // is wrong.
                assert!(
                    core.counter.count(&s.text) <= budget * 2,
                    "window {} is {} tokens against a budget of {budget}",
                    s.idx,
                    core.counter.count(&s.text)
                );
                assert!(!s.text.is_empty(), "window {} was stored empty", s.idx);
            }
        }
    }

    #[tokio::test]
    async fn a_model_that_extracts_from_context_does_not_duplicate_artifacts() {
        use crate::infer::fake::GreedySynthesizer;

        let mut core = crate::core::test_support::test_core().await;
        core.synthesizer = Some(std::sync::Arc::new(GreedySynthesizer {
            budget: context_budget(30, 20),
        }));

        let lines: Vec<String> = (0..400)
            .map(|i| format!("body line {i} with enough words to cost real tokens"))
            .collect();
        let src = core
            .store
            .insert_corpus(&lines.join("\n"), "web", None)
            .await
            .unwrap();

        segment_all(&core, &src.id).await;

        let written = core.store.artifacts_for_corpus(&src.id).await.unwrap();
        // Superseded passages legitimately share text with what replaced them.
        let mut texts: Vec<&str> = written
            .iter()
            .filter(|c| c.in_results())
            .map(|c| c.text.as_str())
            .collect();
        texts.sort_unstable();
        let before = texts.len();
        texts.dedup();
        assert_eq!(
            texts.len(),
            before,
            "the same line was stored as an artifact more than once"
        );
    }

    #[tokio::test]
    async fn every_window_after_the_first_is_given_the_document_opening() {
        use crate::infer::fake::RecordingSynthesizer;

        let mut core = crate::core::test_support::test_core().await;
        let rec = std::sync::Arc::new(RecordingSynthesizer::new(context_budget(30, 20)));
        core.synthesizer = Some(rec.clone());

        let mut lines = vec!["# Backup Guide".to_string(), "PBS 3.x on Debian 12.".into()];
        for i in 0..400 {
            lines.push(format!(
                "body line {i} with enough words to cost real tokens"
            ));
        }
        let src = core
            .store
            .insert_corpus(&lines.join("\n"), "web", None)
            .await
            .unwrap();

        segment_all(&core, &src.id).await;

        let seen = rec.seen.lock().unwrap();
        assert!(seen.len() > 1, "the fixture must produce several windows");
        assert_eq!(
            seen[0].1.opening, None,
            "window 0 already holds the opening"
        );
        assert_eq!(seen[0].1.before, None);
        for (i, (_, ctx)) in seen.iter().enumerate().skip(1) {
            assert!(
                ctx.opening.as_deref().unwrap().contains("# Backup Guide"),
                "window {i} lost the document opening"
            );
            assert!(
                ctx.before.is_some(),
                "window {i} lost its preceding context"
            );
        }
        assert_eq!(
            seen.last().unwrap().1.after,
            None,
            "the last window has nothing after it"
        );
    }

    #[tokio::test]
    async fn a_windows_context_is_the_text_of_its_neighbours() {
        use crate::infer::fake::RecordingSynthesizer;

        let mut core = crate::core::test_support::test_core().await;
        let rec = std::sync::Arc::new(RecordingSynthesizer::new(context_budget(30, 20)));
        core.synthesizer = Some(rec.clone());

        let lines: Vec<String> = (0..400)
            .map(|i| format!("body line {i} with enough words to cost real tokens"))
            .collect();
        let src = core
            .store
            .insert_corpus(&lines.join("\n"), "web", None)
            .await
            .unwrap();

        segment_all(&core, &src.id).await;

        let seen = rec.seen.lock().unwrap();
        let w0_tail_line = seen[0].0.lines().last().unwrap();
        assert!(
            seen[1].1.before.as_deref().unwrap().ends_with(w0_tail_line),
            "preceding context is not the previous window's tail"
        );
        let w2_head_line = seen[2].0.lines().next().unwrap();
        assert!(
            seen[1]
                .1
                .after
                .as_deref()
                .unwrap()
                .starts_with(w2_head_line),
            "following context is not the next window's head"
        );
    }

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

        segment_all(&core, &out.id).await;

        let named = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(named.title_hint.as_deref(), Some("alpha line"));
    }

    #[tokio::test]
    async fn a_name_that_was_given_at_capture_is_not_overwritten() {
        let core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", Some("My own label"))
            .await
            .unwrap();

        segment_all(&core, &out.id).await;

        let got = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(got.title_hint.as_deref(), Some("My own label"));
    }

    #[tokio::test]
    async fn a_context_too_small_to_synthesize_says_so_instead_of_failing_every_window() {
        use crate::infer::fake::RecordingSynthesizer;
        let mut core = test_core().await;
        let rec = std::sync::Arc::new(RecordingSynthesizer::new(crate::infer::SynthesisBudget {
            context_tokens: 1600,
            max_output_tokens: 100_000,
            output_ratio: 1.0,
            context: crate::infer::context::ContextBudget::default(),
        }));
        core.synthesizer = Some(rec.clone());
        assert_eq!(
            segment_budget(&core, crate::infer::lang::Lang::En),
            None,
            "the fixture is no longer the configuration this test is about"
        );

        let out = core
            .ingest(&multi_segment_body(), "web", None)
            .await
            .unwrap();
        plan(&core, &out.id).await.unwrap();

        assert_eq!(
            core.store.segments_for_corpus(&out.id).await.unwrap().len(),
            0,
            "windows were cut to a budget no call can honour"
        );
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            crate::store::corpora::CorpusStatus::Failed,
            "the corpus was left looking like it was on its way"
        );
        assert_eq!(rec.seen.lock().unwrap().len(), 0, "a call was spent anyway");

        let reason = no_budget_reason(&core, crate::infer::lang::Lang::En);
        assert!(reason.contains("context_tokens"), "{reason}");
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
            segment_budget(core, crate::infer::lang::Lang::En).expect("a window"),
        )
        .len()
    }

    #[tokio::test]
    async fn naming_a_corpus_is_its_own_unit() {
        // The model unit only exists for a corpus with no name at settle.
        let core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        segment_all(&core, &out.id).await;
        sqlx::query!("UPDATE corpora SET title_hint = NULL WHERE id = ?", &out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        finish(&core, &out.id).await.unwrap();
        while crate::jobs::run_one(&core).await.unwrap() {}

        assert_eq!(
            core.store
                .get_corpus(&out.id)
                .await
                .unwrap()
                .title_hint
                .as_deref(),
            Some("Fake title: alpha line")
        );
    }

    #[tokio::test]
    async fn a_corpus_the_model_will_not_name_still_reaches_ready() {
        let mut core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        segment_all(&core, &out.id).await;

        sqlx::query!("UPDATE corpora SET title_hint = NULL WHERE id = ?", &out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        core.store
            .enqueue(Stage::Title, "corpus", &out.id)
            .await
            .unwrap();
        core.synthesizer = Some(std::sync::Arc::new(
            crate::infer::fake::FakeSynthesizer::failing("no title"),
        ));
        for _ in 0..crate::store::jobs::MAX_ATTEMPTS + 3 {
            sqlx::query!("UPDATE jobs SET run_after = 0")
                .execute(&core.store.control.pool)
                .await
                .unwrap();
            let _ = crate::jobs::run_one(&core).await;
        }

        let still_queued: i64 = sqlx::query_scalar!(
            "SELECT count(*) FROM jobs WHERE stage = 'title' AND state = 'pending'"
        )
        .fetch_one(&core.store.control.pool)
        .await
        .unwrap();
        assert_eq!(still_queued, 0, "a cosmetic failure is retried forever");
        assert!(
            core.store
                .get_corpus(&out.id)
                .await
                .unwrap()
                .title_hint
                .is_none()
        );
        assert!(
            !core
                .store
                .artifacts_for_corpus(&out.id)
                .await
                .unwrap()
                .is_empty(),
            "the document itself must be unharmed by an unnameable title"
        );
    }

    #[tokio::test]
    async fn planning_a_document_whose_windows_all_finished_does_not_park_it() {
        // Only a settle moves a corpus out of `segmenting`, so a plan that arms nothing must not
        // declare it.
        let core = test_core().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();
        segment_all(&core, &out.id).await;
        let (resolved, total) = core.store.segment_progress(&out.id).await.unwrap();
        assert_eq!(resolved, total, "the fixture must start fully segmented");

        plan(&core, &out.id).await.unwrap();

        assert_ne!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Segmenting,
            "a finished document was parked in segmenting with nothing to run"
        );
    }

    #[tokio::test]
    async fn a_name_already_given_up_on_is_not_asked_for_again() {
        let core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        segment_all(&core, &out.id).await;
        sqlx::query!("UPDATE corpora SET title_hint = NULL WHERE id = ?", &out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        finish(&core, &out.id).await.unwrap();
        while crate::jobs::run_one(&core).await.unwrap() {}

        sqlx::query!("UPDATE corpora SET title_hint = NULL WHERE id = ?", &out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();

        finish(&core, &out.id).await.unwrap();

        let armed: i64 = sqlx::query_scalar!(
            "SELECT count(*) FROM jobs WHERE stage = 'title' AND state = 'pending'"
        )
        .fetch_one(&core.store.control.pool)
        .await
        .unwrap();
        assert_eq!(armed, 0, "a name already given up on was asked for again");
    }

    #[tokio::test]
    async fn planning_makes_no_inference_call_and_arms_one_unit_per_window() {
        use crate::infer::fake::RecordingSynthesizer;
        let mut core = test_core().await;
        let rec = std::sync::Arc::new(RecordingSynthesizer::new(context_budget(30, 20)));
        core.synthesizer = Some(rec.clone());
        let body = multi_segment_body();
        let out = core.ingest(&body, "web", None).await.unwrap();

        plan(&core, &out.id).await.unwrap();

        assert_eq!(
            rec.seen.lock().unwrap().len(),
            0,
            "planning called the model; it is meant to be arithmetic over text"
        );
        let windows = core.store.segments_for_corpus(&out.id).await.unwrap().len();
        assert!(windows > 2, "the fixture must span several windows");
        let armed: i64 = sqlx::query_scalar!(
            "SELECT count(*) FROM jobs WHERE stage = 'segment_window' AND state = 'pending'"
        )
        .fetch_one(&core.store.control.pool)
        .await
        .unwrap();
        assert_eq!(armed, 0);
    }

    #[tokio::test]
    async fn a_poisoned_window_does_not_stop_another_document() {
        // One poisoned window of A must not hold the queue from B.
        let mut core = test_core().await;
        let a = core
            .ingest(
                &format!("STOPHERE poison\n\n{}", multi_segment_body()),
                "web",
                None,
            )
            .await
            .unwrap();
        let b = core
            .ingest("bravo one\n\nbravo two", "web", None)
            .await
            .unwrap();
        core.synthesizer = Some(std::sync::Arc::new(
            crate::infer::fake::FakeSynthesizer::unparsable_on("STOPHERE"),
        ));

        plan(&core, &a.id).await.unwrap();
        plan(&core, &b.id).await.unwrap();
        for w in core.store.segments_for_corpus(&a.id).await.unwrap() {
            core.store.reset_segment(&a.id, w.idx, true).await.unwrap();
            core.store
                .rearm_idle_seq(
                    Stage::SegmentWindow,
                    "segment",
                    &crate::jobs::window::unit_target(&a.id, w.idx),
                    w.idx,
                )
                .await
                .unwrap();
        }
        for _ in 0..400 {
            sqlx::query!("UPDATE jobs SET run_after = 0")
                .execute(&core.store.control.pool)
                .await
                .unwrap();
            if !crate::jobs::run_one(&core).await.unwrap_or(false) {
                break;
            }
        }

        assert_eq!(
            core.store.get_corpus(&b.id).await.unwrap().status,
            CorpusStatus::Ready,
            "the healthy document waited on the poisoned one"
        );
        assert_eq!(
            core.store.get_corpus(&a.id).await.unwrap().status,
            CorpusStatus::Partial,
            "a document with one refused window settles partial, not failed"
        );
    }

    #[tokio::test]
    async fn a_corpus_settles_around_a_window_that_will_not_resolve() {
        // A window the model will not read stays queued; settling must not wait for it.
        let mut core = test_core().await;
        let body = format!("STOPHERE poison\n\n{}", multi_segment_body());
        let out = core.ingest(&body, "web", None).await.unwrap();
        core.synthesizer = Some(std::sync::Arc::new(
            crate::infer::fake::FakeSynthesizer::unparsable_on("STOPHERE"),
        ));

        segment_all(&core, &out.id).await;

        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Partial
        );
        assert!(
            !core
                .store
                .artifacts_for_corpus(&out.id)
                .await
                .unwrap()
                .is_empty(),
            "the good windows' artifacts were never written"
        );
        let embed_armed: i64 = sqlx::query_scalar!(
            "SELECT count(*) FROM jobs WHERE stage = 'embed' AND target_id = ?",
            &out.id
        )
        .fetch_one(&core.store.control.pool)
        .await
        .unwrap();
        assert_eq!(
            embed_armed, 1,
            "the good artifacts were never queued to embed"
        );
    }

    #[tokio::test]
    async fn a_recovered_window_settles_the_corpus_again() {
        let mut core = test_core().await;
        let body = format!("STOPHERE poison\n\n{}", multi_segment_body());
        let out = core.ingest(&body, "web", None).await.unwrap();
        core.synthesizer = Some(std::sync::Arc::new(
            crate::infer::fake::FakeSynthesizer::unparsable_on("STOPHERE"),
        ));
        segment_all(&core, &out.id).await;
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Partial
        );

        core.synthesizer = Some(std::sync::Arc::new(
            crate::infer::fake::FakeSynthesizer::default(),
        ));
        segment_all(&core, &out.id).await;

        let windows = core.store.segments_for_corpus(&out.id).await.unwrap();
        assert!(
            windows.iter().all(|w| w.state == SegmentState::Done),
            "the retried window never recovered"
        );
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Embedding
        );
    }

    #[tokio::test]
    async fn segments_a_source_into_chunks_and_queues_embedding() {
        let core = test_core().await;
        let out = core
            .ingest("first para\n\nsecond para", "web", None)
            .await
            .unwrap();

        segment_all(&core, &out.id).await;

        let chunks: Vec<_> = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|c| c.in_results())
            .collect();
        // No single artifact holds a majority of the joint passage, so it stays live.
        assert_eq!(chunks.len(), 3);
        assert!(
            chunks.windows(2).all(|p| p[0].ordinal < p[1].ordinal),
            "ordinals must stay ordered"
        );
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Embedding
        );

        // Read off the table: the units are done, so claiming says nothing about how many there
        // were.
        let embed_jobs: Vec<(String, String)> = sqlx::query_as(
            "SELECT target_kind, target_id FROM jobs
              WHERE stage = 'embed' AND state = 'pending'",
        )
        .fetch_all(&core.store.control.pool)
        .await
        .unwrap();
        assert_eq!(embed_jobs.len(), 1, "expected one batched embed job");
        assert_eq!(embed_jobs[0].0, "corpus");
        assert_eq!(embed_jobs[0].1, out.id);
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

        segment_all(&core, &out.id).await;

        let chunks = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(chunks.len() > 1);
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.ordinal, i as i64, "ordinals must not restart per window");
        }
    }

    #[tokio::test]
    async fn re_running_segmentation_replaces_rather_than_appends() {
        let core = test_core().await;
        let out = core.ingest("one\n\ntwo", "web", None).await.unwrap();
        segment_all(&core, &out.id).await;
        segment_all(&core, &out.id).await;
        // "one\n\ntwo" allows one artifact, and neither proposal is short enough to locate as
        // evidence.
        assert_eq!(
            core.store
                .artifacts_for_corpus(&out.id)
                .await
                .unwrap()
                .iter()
                .filter(|c| c.in_results())
                .count(),
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
        let synthesizer = std::sync::Arc::new(crate::infer::fake::ParaphrasingSynthesizer::new(
            "oflag=sync ",
            false,
        ));
        core.synthesizer = Some(synthesizer.clone());
        let out = core.ingest(COMMAND_BODY, "web", None).await.unwrap();

        segment_all(&core, &out.id).await;

        assert_eq!(synthesizer.calls(), 2, "exactly one re-segmentation");
        let chunks = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(
            chunks.iter().all(|c| c.flags.is_empty()),
            "a clean retry must leave no flag"
        );
    }

    #[tokio::test]
    async fn a_re_segmentation_keeps_the_judgement_the_first_reply_gave() {
        let mut core = test_core().await;
        let synthesizer = std::sync::Arc::new(crate::infer::fake::JudgingParaphraser::new(
            // Drops a path component, so the artifact carries a literal the window does not.
            "backup/",
            crate::infer::Judgement {
                intent: Some("remind".into()),
                when: Some("2099-09-04T09:00".into()),
                rule: None,
                events: vec![],
                links: vec![],
            },
        ));
        core.synthesizer = Some(synthesizer.clone());
        let out = core
            .ingest(
                "erinnere mich Freitag, /mnt/backup/nightly.sh prüfen",
                "web",
                None,
            )
            .await
            .unwrap();

        crate::jobs::test_support::drain(&core).await;

        assert_eq!(synthesizer.calls(), 2, "exactly one re-segmentation");
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(
            rows.len(),
            1,
            "the reminder the capture was made for: {rows:?}"
        );
        assert!(rows[0].moment.at.is_some());
        let chunks = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(
            chunks
                .iter()
                .any(|c| c.text.contains("/mnt/backup/nightly.sh")),
            "the literal came back on the retry: {chunks:?}"
        );
    }

    #[tokio::test]
    async fn a_re_segmentation_that_says_nothing_does_not_retract_the_judgement() {
        let mut core = test_core().await;
        let synthesizer = std::sync::Arc::new(
            crate::infer::fake::JudgingParaphraser::new(
                "backup/",
                crate::infer::Judgement {
                    intent: Some("remind".into()),
                    when: Some("2099-09-04T09:00".into()),
                    rule: None,
                    events: vec![],
                    links: vec![],
                },
            )
            .answering_an_empty_judgement_on_retry(),
        );
        core.synthesizer = Some(synthesizer.clone());
        let out = core
            .ingest(
                "erinnere mich Freitag, /mnt/backup/nightly.sh prüfen",
                "web",
                None,
            )
            .await
            .unwrap();

        crate::jobs::test_support::drain(&core).await;

        assert_eq!(synthesizer.calls(), 2, "exactly one re-segmentation");
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(
            rows.len(),
            1,
            "an empty JUDGE block is not a retraction: {rows:?}"
        );
        assert!(rows[0].moment.at.is_some());
        let chunks = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(
            chunks
                .iter()
                .any(|c| c.text.contains("/mnt/backup/nightly.sh")),
            "and the retry's literal is still what is stored: {chunks:?}"
        );
    }

    #[tokio::test]
    async fn a_literal_the_retry_also_drops_is_stored_flagged() {
        let mut core = test_core().await;
        core.synthesizer = Some(std::sync::Arc::new(
            crate::infer::fake::ParaphrasingSynthesizer::new("oflag=sync ", true),
        ));
        let out = core.ingest(COMMAND_BODY, "web", None).await.unwrap();

        segment_all(&core, &out.id).await;

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
        // The model's line numbers are often wrong on reference documents; a verbatim chunk can be
        // located.
        let mut core = test_core().await;
        core.synthesizer = Some(std::sync::Arc::new(
            crate::infer::fake::MisreportingSynthesizer { echo_text: true },
        ));
        let out = core
            .ingest("first paragraph here\n\nsecond paragraph here", "web", None)
            .await
            .unwrap();

        segment_all(&core, &out.id).await;

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
        core.synthesizer = Some(std::sync::Arc::new(
            crate::infer::fake::MisreportingSynthesizer { echo_text: false },
        ));
        let out = core
            .ingest("first paragraph here\n\nsecond paragraph here", "web", None)
            .await
            .unwrap();

        segment_all(&core, &out.id).await;

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
        segment_all(&core, &out.id).await;
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
    async fn a_second_run_does_not_re_segment_windows_that_finished() {
        let core = test_core().await;
        let body = multi_segment_body();
        let out = core.ingest(&body, "web", None).await.unwrap();
        assert!(segment_count(&core, &body) > 1);

        segment_all(&core, &out.id).await;
        let (resolved, total) = core.store.segment_progress(&out.id).await.unwrap();
        assert_eq!(resolved, total, "every window should have resolved");

        let before = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .len();
        segment_all(&core, &out.id).await;
        let after = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .len();
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn empty_source_is_marked_failed_not_left_pending() {
        let core = test_core().await;
        let src = core
            .store
            .insert_corpus("\n\n  \n", "web", None)
            .await
            .unwrap();
        segment_all(&core, &src.id).await;
        assert_eq!(
            core.store.get_corpus(&src.id).await.unwrap().status,
            CorpusStatus::Failed
        );
    }

    #[tokio::test]
    async fn every_artifacts_span_reads_back_its_own_text() {
        let core = test_core().await;
        let mut lines = vec!["## Section one".to_string(), String::new()];
        for i in 0..400 {
            lines.push(format!("paragraph number {i} with some filler text"));
            lines.push(String::new());
        }
        let body = lines.join("\n");
        let out = core.ingest(&body, "web", None).await.unwrap();

        segment_all(&core, &out.id).await;

        let windows = core.store.segments_for_corpus(&out.id).await.unwrap();
        assert!(
            windows.len() > 1,
            "the fixture must produce several windows"
        );

        let raw = core.store.get_corpus(&out.id).await.unwrap().raw_text;
        for c in core.store.artifacts_for_corpus(&out.id).await.unwrap() {
            let needle = c.text.lines().next_back().unwrap();
            let span = c.corpus_span.expect("every artifact keeps a span");
            let claimed = crate::infer::split::segment_text(&raw, span.start_line, span.end_line);
            assert!(
                claimed.contains(needle),
                "artifact claims lines {}-{}, which read {claimed:?}, not {needle:?}",
                span.start_line,
                span.end_line
            );
        }
    }

    #[tokio::test]
    async fn source_spans_are_shifted_into_document_coordinates() {
        // The synthesizer numbers lines from 1 per window.
        let core = test_core().await;
        let body = multi_segment_body();
        let out = core.ingest(&body, "web", None).await.unwrap();
        assert!(segment_count(&core, &body) > 1);
        segment_all(&core, &out.id).await;

        let chunks = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        let last = chunks.last().unwrap();
        let span = last.corpus_span.as_ref().expect("span must be recorded");
        assert!(
            span.start_line > 1,
            "later chunks must not all claim to start at line 1"
        );
    }

    #[tokio::test]
    async fn a_small_capture_arms_its_synthesis_at_capture_keeping_its_passages() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("remind me friday to send the invoice", "web", None)
            .await
            .unwrap();
        plan(&core, &out.id).await.unwrap();
        let rows = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(
            rows.iter()
                .any(|c| c.provenance == crate::store::artifacts::Provenance::Passage)
        );
        let target = crate::jobs::window::unit_target(&out.id, 0);
        assert!(
            core.store
                .live_job(Stage::SegmentWindow, &target)
                .await
                .unwrap()
        );
        assert!(
            core.store
                .segment_keeps_artifacts(&out.id, 0)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_large_capture_arms_no_synthesis_and_waits_for_promotion() {
        let core = crate::core::test_support::test_core().await;
        let body = multi_segment_body();
        let out = core.ingest(&body, "web", None).await.unwrap();
        plan(&core, &out.id).await.unwrap();
        let segs = core.store.segments_for_corpus(&out.id).await.unwrap();
        assert!(
            segs.len() > 1,
            "must be multi-window for this test: {}",
            segs.len()
        );
        for w in &segs {
            assert_eq!(w.state, SegmentState::Verbatim);
            let target = crate::jobs::window::unit_target(&out.id, w.idx);
            assert!(
                !core
                    .store
                    .live_job(Stage::SegmentWindow, &target)
                    .await
                    .unwrap(),
                "window {} must wait for promotion",
                w.idx
            );
        }
        let rows = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(!rows.is_empty());
        assert!(
            rows.iter()
                .all(|c| c.provenance == crate::store::artifacts::Provenance::Passage)
        );
    }

    #[tokio::test]
    async fn a_document_that_produced_only_its_note_still_parks_as_failed() {
        let core = test_core().await;
        let out = core
            .ingest_capture(
                crate::core::ingest::Capture::new("a page nothing could be made of", "upload")
                    .with_note(Some("the survey nobody can OCR".into())),
            )
            .await
            .unwrap();
        plan(&core, &out.id).await.unwrap();
        sqlx::query!(
            "DELETE FROM artifacts WHERE corpus_id = ? AND provenance != 'note'",
            &out.id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        sqlx::query!(
            "UPDATE segments SET state = 'done' WHERE corpus_id = ?",
            &out.id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();

        finish(&core, &out.id).await.unwrap();

        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Failed,
            "a corpus holding nothing but its note was reported as read"
        );
    }

    #[tokio::test]
    async fn the_windowless_coverage_fallback_does_not_count_the_note() {
        let core = test_core().await;
        let src = core
            .store
            .insert_corpus(
                "quarterly revenue projections attached\nthe Reinhardt lease expires November",
                "web",
                None,
            )
            .await
            .unwrap();
        let new = |text: &str| crate::store::artifacts::NewArtifact {
            text: text.to_string(),
            ..Default::default()
        };
        core.store
            .insert_artifacts_with_provenance(
                &src.id,
                &[new("quarterly revenue projections attached")],
                crate::store::artifacts::Provenance::Captured,
            )
            .await
            .unwrap();
        core.store
            .insert_artifacts_with_provenance(
                &src.id,
                &[new("the Reinhardt lease expires November")],
                crate::store::artifacts::Provenance::Note,
            )
            .await
            .unwrap();

        let cov = recompute_coverage(&core, &src.id).await.unwrap();

        assert_eq!(
            cov, 0.5,
            "the note was measured as if it had been read out of the document"
        );
    }
}
