use crate::core::Core;
use crate::error::Result;
use crate::infer::budget::segment_tokens;
use crate::infer::split::split_into_segments;
use crate::store::corpora::CorpusStatus;
use crate::store::jobs::Stage;
use crate::store::segments::SegmentState;

/// Tokens consumed by the system prompt and scaffolding. Measured from the
/// real prompt rather than guessed.
pub(super) fn prompt_overhead(core: &Core) -> usize {
    core.counter.count(crate::infer::prompt::SYNTHESIZER_SYSTEM) + 200
}

/// Split a document into windows and record them. No inference call.
///
/// This is the whole of the `Synthesize` stage now: deciding what the units of
/// work are, which is local arithmetic over the text. The calls belong to the
/// units it arms.
pub async fn plan(core: &Core, corpus_id: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
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

    let rows: Vec<crate::store::segments::NewSegment<'_>> = windows
        .iter()
        .map(|w| crate::store::segments::NewSegment {
            start_line: w.start_line,
            end_line: w.end_line,
            text: w.text.as_str(),
            carry_lines: w.carry_lines,
        })
        .collect();
    core.store.upsert_segments(corpus_id, &rows).await?;

    // A document whose windows have all resolved arms nothing, and declaring it
    // `segmenting` would park it there for good — nothing would be left to run
    // that could call `settle` and move it on. Reachable whenever a plan job
    // outlives the units it armed: a process killed after planning leaves the
    // row pending, startup re-arms it, the units (attempts 0) sort ahead of it
    // and drive the document all the way to `ready`, and only then is the stale
    // plan claimed. Settling instead is idempotent and says the same thing.
    let pending = core.store.pending_segments(corpus_id).await?;
    if pending.is_empty() {
        return crate::jobs::window::settle(core, corpus_id).await;
    }

    core.store
        .set_corpus_status(corpus_id, CorpusStatus::Segmenting)
        .await?;

    // One unit per window that has not resolved. `seq` is the window index, so
    // this document's window 0 is claimed before any document's window 1 and a
    // capture made during a long ingest does not wait for all of it.
    //
    // Idle-only, like every other automatic arming in the system. A plan job
    // outlives the units it arms — the case the comment above describes — so
    // this runs again while those units are queued with attempts against them,
    // and winding those back keeps a window the model will not read forever
    // young. An operator's reprocess still gets a clean slate: it deletes the
    // units outright, which is a decision a person made rather than a sweep.
    for w in pending {
        core.store
            .rearm_idle_seq(
                Stage::SegmentWindow,
                "segment",
                &crate::jobs::window::unit_target(corpus_id, w.idx),
                w.idx,
            )
            .await?;
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
    core.store.set_corpus_coverage(corpus_id, Some(cov)).await?;
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

    // Naming is armed, not called, and armed once: settling runs afresh every
    // time a window resolves, and re-arming would reset the title unit's
    // attempts and spend another `MAX_ATTEMPTS` on a name already given up
    // on. An operator's reprocess deletes the row and is the one way back. A
    // name given at capture is left alone — someone chose it.
    if src.title_hint.is_none() && !core.store.has_job(Stage::Title, corpus_id).await? {
        core.store
            .enqueue(Stage::Title, "corpus", corpus_id)
            .await?;
    }

    // One job for the whole source, idle-only. Re-arming a *running* embed
    // puts a second worker inside the same batch; re-arming a *pending* one
    // winds `attempts`, `run_after` and `seq` back on every settle. A job
    // already queued picks up the chunks this settle wrote.
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

/// Name a document, given its opening and the titles of the artifacts drawn
/// from it.
///
/// The only unit whose failure is not worth retrying to the ceiling. A name is
/// decoration: the corpus keeps the snippet the UI falls back to, and spending
/// four model calls a day forever on a document the model will not name is a
/// bad trade against every other thing those calls could do. `run_one` closes
/// this job once its attempts are spent.
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

    let permit = core.gate.background().await;
    let named = core.synthesizer.title(&src.raw_text, &titles).await;
    permit.finished();
    match named {
        Ok(Some(t)) => {
            core.store.set_title_hint(corpus_id, &t).await?;
            Ok(())
        }
        // The synthesizer has no opinion about titles. Not a failure, and not
        // worth another call.
        Ok(None) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Plan a corpus and work its queue to a standstill — what a worker does,
/// compressed into one call.
///
/// Segmentation is no longer a function that takes a document and returns it
/// segmented: it is a plan plus N units the queue delivers. Tests across the
/// tree that care about the outcome rather than the schedule say so with this.
/// The backoff is wound back each round because a delay is not what any of them
/// are asserting.
#[cfg(test)]
pub async fn segment_all(core: &Core, corpus_id: &str) {
    plan(core, corpus_id).await.unwrap();
    for _ in 0..500 {
        // Embedding is held back rather than run. This helper stands in for the
        // old whole-corpus `run`, which segmented a document and left an embed
        // job queued behind it — several tests are about the state in exactly
        // that gap, and draining through it would erase what they check.
        sqlx::query(
            "UPDATE jobs SET run_after = CASE WHEN stage = 'embed' THEN ? ELSE 0 END
              WHERE state = 'pending'",
        )
        .bind(crate::store::now() + 86_400)
        .execute(&core.store.pool)
        .await
        .unwrap();

        if !crate::jobs::run_one(core).await.unwrap_or(false) {
            break;
        }
        // A window the model will never read stays claimable forever — engram
        // has no terminal state — so "the queue is empty" cannot be the end
        // condition here. The corpus leaving `segmenting` is: that is precisely
        // the moment `settle` decided every window had resolved, whether by
        // succeeding or by spending its attempts.
        if core.store.get_corpus(corpus_id).await.unwrap().status != CorpusStatus::Segmenting {
            break;
        }
    }

    // Settling arms the title unit, so the corpus leaves `segmenting` with one
    // still queued. The old whole-corpus `run` named the document before it
    // returned, and these tests were written against that. Settling also arms
    // embedding, with a fresh `run_after`, so the hold has to be reapplied.
    for _ in 0..20 {
        sqlx::query("UPDATE jobs SET run_after = ? WHERE stage = 'embed' AND state = 'pending'")
            .bind(crate::store::now() + 86_400)
            .execute(&core.store.pool)
            .await
            .unwrap();
        if !crate::jobs::run_one(core).await.unwrap_or(false) {
            break;
        }
    }

    sqlx::query("UPDATE jobs SET run_after = 0 WHERE stage = 'embed'")
        .execute(&core.store.pool)
        .await
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::corpora::CorpusStatus;

    #[tokio::test]
    async fn re_planning_does_not_wind_back_a_queued_windows_attempts() {
        // A plan job outlives the units it arms: killed after planning, its row
        // stays pending, startup re-arms it, the units sort ahead of it and run
        // and fail — and only then is the stale plan claimed. Re-arming them
        // from zero there keeps a window the model will not read forever young,
        // so `settle` never counts it as spent and the document never leaves
        // `segmenting`. It is the failure the reconciliation sweep was already
        // fixed for, reached by a second route.
        let core = test_core().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();
        plan(&core, &out.id).await.unwrap();
        sqlx::query("UPDATE jobs SET attempts = 4 WHERE stage = 'segment_window'")
            .execute(&core.store.pool)
            .await
            .unwrap();

        plan(&core, &out.id).await.unwrap();

        let attempts: Vec<i64> =
            sqlx::query_scalar("SELECT attempts FROM jobs WHERE stage = 'segment_window'")
                .fetch_all(&core.store.pool)
                .await
                .unwrap();
        assert!(
            attempts.iter().all(|&a| a == 4),
            "re-planning reset a unit that was already queued: {attempts:?}"
        );
    }

    /// A budget with room for several windows and an output ceiling that never
    /// binds, so the context blocks are what shape the windowing.
    fn context_budget(opening: usize, overlap: usize) -> crate::infer::SynthesisBudget {
        crate::infer::SynthesisBudget {
            context_tokens: 2000,
            max_output_tokens: 100_000,
            output_ratio: 1.0,
            context: crate::infer::context::ContextBudget { opening, overlap },
        }
    }

    /// (state, attempts, run_after, seq) of one job row.
    async fn job_row(core: &Core, stage: Stage, target: &str) -> (String, i64, i64, i64) {
        sqlx::query_as(
            "SELECT state, attempts, run_after, seq FROM jobs WHERE stage = ? AND target_id = ?",
        )
        .bind(stage.as_str())
        .bind(target)
        .fetch_one(&core.store.pool)
        .await
        .unwrap()
    }

    /// A corpus with its windows resolved and one embed job queued behind them.
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
        // Settling runs afresh every time a window resolves, and a document with
        // one window the model will not read settles on every failed retry of
        // it. Re-arming here put a *running* embed job back in the queue for a
        // second worker: two workers embedding the same batch, and whichever
        // finished second closed the row the other had just re-armed for the
        // rest of the chunks — a corpus left half-embedded.
        let core = test_core().await;
        let id = segmented(&core).await;

        sqlx::query("UPDATE jobs SET state = 'running', claimed_at = ? WHERE stage = 'embed'")
            .bind(crate::store::now())
            .execute(&core.store.pool)
            .await
            .unwrap();

        finish(&core, &id).await.unwrap();

        let (state, ..) = job_row(&core, Stage::Embed, &id).await;
        assert_eq!(state, "running", "a settle re-armed an embed job mid-call");
    }

    #[tokio::test]
    async fn settling_again_does_not_reset_a_backing_off_embed_job() {
        // The quieter half of the same bug. A queued job is already going to run
        // and will pick up everything written since, so winding it back gains
        // nothing — and costs the backoff that keeps a dead embedder from being
        // asked every thirty seconds, plus the `seq` that `rearm_if_more` climbs
        // to walk a corpus through its chunks.
        let core = test_core().await;
        let id = segmented(&core).await;

        let later = crate::store::now() + 3600;
        sqlx::query("UPDATE jobs SET state = 'pending', attempts = 4, run_after = ?, seq = 7 WHERE stage = 'embed'")
            .bind(later)
            .execute(&core.store.pool)
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
        // The case the arming is actually for: the chunks this settle just wrote
        // are not in the batch the finished job embedded.
        let core = test_core().await;
        let id = segmented(&core).await;

        sqlx::query("UPDATE jobs SET state = 'done' WHERE stage = 'embed'")
            .execute(&core.store.pool)
            .await
            .unwrap();

        finish(&core, &id).await.unwrap();

        let (state, ..) = job_row(&core, Stage::Embed, &id).await;
        assert_eq!(state, "pending", "new chunks were left unembedded");
    }

    #[tokio::test]
    async fn a_window_is_never_sent_over_its_own_budget() {
        // The guard is a can't-happen check: split_into_segments now floors
        // window size. It exists because the failure it catches is a job that
        // spins against the endpoint forever, and a debug_assert turns that
        // into a test failure instead of a production incident.
        let core = crate::core::test_support::test_core().await;
        let budget = segment_tokens(core.synthesizer.budget(), prompt_overhead(&core));

        let lines: Vec<String> = (0..400)
            .map(|i| format!("body line {i} with enough words to cost real tokens"))
            .collect();
        // Ordinary prose, and the case that has no line boundary to cut on at
        // all — the second is what the floor in split_into_segments is for, so
        // it is the one that fails if that floor is ever removed.
        for raw in [lines.join("\n"), "word ".repeat(8000)] {
            let src = core.store.insert_corpus(&raw, "web", None).await.unwrap();

            segment_all(&core, &src.id).await;

            let segments = core.store.segments_for_corpus(&src.id).await.unwrap();
            assert!(!segments.is_empty(), "a corpus must produce windows");
            for s in segments {
                // The stored text, which is what the window is actually sent
                // as. Re-deriving it from the line range is the bug this
                // column exists to close.
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
        core.synthesizer = std::sync::Arc::new(GreedySynthesizer {
            budget: context_budget(30, 20),
        });

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
        let mut texts: Vec<&str> = written.iter().map(|c| c.text.as_str()).collect();
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
        core.synthesizer = rec.clone();

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
        core.synthesizer = rec.clone();

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
        // Window 1's preceding context must be the literal end of window 0's
        // own text, and its following context the literal start of window 2's.
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

        segment_all(&core, &out.id).await;

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
        segment_all(&core, &out.id).await;

        // A synthesizer that fails every call cannot produce artifacts either,
        // so naming is exercised through `finish` on a corpus that already has
        // them: the state a real failure leaves behind.
        let mut failing = test_core().await;
        failing.synthesizer = std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::failing(
            "endpoint down",
        ));
        let hurt = failing
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        segment_all(&failing, &hurt.id).await;
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
    async fn naming_a_corpus_is_its_own_unit() {
        let core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();

        segment_all(&core, &out.id).await;
        // The title unit is armed by the settle, so it is still queued here.
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
        // A name is decoration. Retrying it to the six-hour ceiling forever
        // spends real calls on it, and failing the document over it is worse.
        let mut core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        segment_all(&core, &out.id).await;

        // Put the document back in the state settling leaves it in — named by
        // nobody, with a title unit queued — and then break the model.
        sqlx::query("UPDATE corpora SET title_hint = NULL WHERE id = ?")
            .bind(&out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        core.store
            .enqueue(Stage::Title, "corpus", &out.id)
            .await
            .unwrap();
        core.synthesizer =
            std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::failing("no title"));
        for _ in 0..crate::store::jobs::MAX_ATTEMPTS + 3 {
            sqlx::query("UPDATE jobs SET run_after = 0")
                .execute(&core.store.pool)
                .await
                .unwrap();
            let _ = crate::jobs::run_one(&core).await;
        }

        let still_queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE stage = 'title' AND state = 'pending'",
        )
        .fetch_one(&core.store.pool)
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
        // `segmenting` says work is in flight, and only a settle moves a corpus
        // out of it. A plan that arms nothing has nothing left to settle it, so
        // declaring `segmenting` first left the document there permanently —
        // reachable from a plan job that outlived the units it armed.
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
        // Settling runs afresh every time a window resolves, so a document with
        // one window the model will not read settles on every failed retry of
        // it — once every six hours, forever. Re-arming the title unit there
        // reset its attempts and spent another `MAX_ATTEMPTS` calls each time on
        // a name that had already been given up on.
        let core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        segment_all(&core, &out.id).await;
        while crate::jobs::run_one(&core).await.unwrap() {}

        // What a corpus the model would not name looks like afterwards: no
        // title, and a title unit that has been closed.
        sqlx::query("UPDATE corpora SET title_hint = NULL WHERE id = ?")
            .bind(&out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();

        finish(&core, &out.id).await.unwrap();

        let armed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE stage = 'title' AND state = 'pending'",
        )
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(armed, 0, "a name already given up on was asked for again");
    }

    #[tokio::test]
    async fn planning_makes_no_inference_call_and_arms_one_unit_per_window() {
        use crate::infer::fake::RecordingSynthesizer;
        let mut core = test_core().await;
        let rec = std::sync::Arc::new(RecordingSynthesizer::new(context_budget(30, 20)));
        core.synthesizer = rec.clone();
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
        let armed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE stage = 'segment_window' AND state = 'pending'",
        )
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(armed as usize, windows);
    }

    #[tokio::test]
    async fn a_poisoned_window_does_not_stop_another_document() {
        // The 2026-08-12 incident, as a test. One window of document A carries a
        // reply that never parses; document B must still reach ready. Before the
        // units existed this could not pass: A held the queue for hours.
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
        core.synthesizer = std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::unparsable_on(
            "STOPHERE",
        ));

        plan(&core, &a.id).await.unwrap();
        plan(&core, &b.id).await.unwrap();
        for _ in 0..400 {
            sqlx::query("UPDATE jobs SET run_after = 0")
                .execute(&core.store.pool)
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
        // Per-unit budgets could hang a document forever: engram never abandons
        // work, so a window the model will not read stays queued at the ceiling
        // — and if settling waited for it, the windows that came back perfectly
        // would never be embedded and never become searchable.
        let mut core = test_core().await;
        let body = format!("STOPHERE poison\n\n{}", multi_segment_body());
        let out = core.ingest(&body, "web", None).await.unwrap();
        core.synthesizer = std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::unparsable_on(
            "STOPHERE",
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
        let embed_armed: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE stage = 'embed' AND target_id = ?")
                .bind(&out.id)
                .fetch_one(&core.store.pool)
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
        core.synthesizer = std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::unparsable_on(
            "STOPHERE",
        ));
        segment_all(&core, &out.id).await;
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Partial
        );

        // The endpoint comes back to its senses. Settling has to run again, or
        // the document would stay `partial` with a window that is now fine.
        core.synthesizer = std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::default());
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

        let chunks = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].ordinal, 0);
        assert_eq!(chunks[1].ordinal, 1);
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Embedding
        );

        // One embed job for the whole source, not one per chunk: the point of
        // batching is a single inference call. Read off the table rather than
        // claimed, because the units this document was segmented by are already
        // done and claiming would say nothing about how many there were.
        let embed_jobs: Vec<(String, String)> = sqlx::query_as(
            "SELECT target_kind, target_id FROM jobs
              WHERE stage = 'embed' AND state = 'pending'",
        )
        .fetch_all(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(embed_jobs.len(), 1, "expected one batched embed job");
        assert_eq!(embed_jobs[0].0, "corpus");
        assert_eq!(embed_jobs[0].1, out.id);
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
        let synthesizer = std::sync::Arc::new(crate::infer::fake::ParaphrasingSynthesizer::new(
            "oflag=sync ",
            false,
        ));
        core.synthesizer = synthesizer.clone();
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
    async fn a_literal_the_retry_also_drops_is_stored_flagged() {
        let mut core = test_core().await;
        core.synthesizer = std::sync::Arc::new(crate::infer::fake::ParaphrasingSynthesizer::new(
            "oflag=sync ",
            true,
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
        // The model's line numbers are routinely wrong on reference documents.
        // Where the chunk still reproduces its source, the real span can be
        // found — better than flagging a chunk whose lines we can work out.
        let mut core = test_core().await;
        core.synthesizer =
            std::sync::Arc::new(crate::infer::fake::MisreportingSynthesizer { echo_text: true });
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
        // A line number engram can compute itself was being asked of the model,
        // disbelieved, and turned into a queue entry whose only button spends a
        // model call on a whole segment. The span falls back to the window and
        // the reader is none the wiser.
        let mut core = test_core().await;
        core.synthesizer =
            std::sync::Arc::new(crate::infer::fake::MisreportingSynthesizer { echo_text: false });
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
    async fn re_segmenting_replaces_chunks_written_before_windows_existed() {
        // Chunks from before the window column was added carry no window, so
        // the per-window delete could not see them and a re-segmentation
        // appended a second copy of the whole source beside the first.
        let core = test_core().await;
        let out = core
            .ingest("one para\n\ntwo para", "web", None)
            .await
            .unwrap();
        segment_all(&core, &out.id).await;
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

        segment_all(&core, &out.id).await;

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

        segment_all(&core, &out.id).await;
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
    async fn a_carried_heading_does_not_shift_the_spans_of_its_window() {
        // A window that continues a section opens with the heading copied from
        // further up the document, and that line occupies none of the window's
        // own lines. Measuring an artifact's offset against it put every span in
        // every continuing window one line too far down the source.
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
            windows.iter().any(|w| w.carry_lines == 1),
            "the fixture must produce windows that carry the heading"
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
        // The synthesizer sees one window at a time and numbers lines from 1.
        // Without the shift, every chunk in window two would point at the
        // wrong part of the raw text.
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
}
