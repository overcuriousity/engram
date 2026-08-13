use crate::core::Core;
use crate::error::Result;
use crate::infer::budget::segment_tokens;
use crate::infer::split::split_into_segments;
use crate::store::artifacts::{CorpusSpan, NewArtifact};
use crate::store::corpora::CorpusStatus;
use crate::store::jobs::Stage;
use crate::store::segments::SegmentState;

/// Unreadable replies in a row before the pass stops working through the
/// document.
///
/// One window the model will not produce readable JSON for says something about
/// that window; the pass steps over it and keeps going, which is the whole point
/// of stepping over it at all. Several in a row says something about the model
/// or the endpoint, and every further window costs a full call — the slowest,
/// most expensive thing engram does — to arrive back at the same answer. Three
/// is far enough to clear a bad patch of a document and short enough that a
/// broken model costs a handful of calls rather than one per window.
const REFUSALS_BEFORE_GIVING_UP_ON_THE_PASS: usize = 3;

/// Tokens consumed by the system prompt and scaffolding. Measured from the
/// real prompt rather than guessed.
fn prompt_overhead(core: &Core) -> usize {
    core.counter.count(crate::infer::prompt::SYNTHESIZER_SYSTEM) + 200
}

/// Where an artifact sits in the source document.
///
/// Asking the model for `corpus_lines`, checking the answer, and having a third
/// outcome for a claim that fails the check produced a flag on the artifact and
/// a button offering to re-synthesise an entire segment over a line number.
/// Since `locate_span` finds an artifact's own text even where the source is
/// hard-wrapped and synthesis reflowed it, the claim is worth what it is: a
/// hint for the case where nothing matches at all. Nothing here can disagree
/// with the artifact, so nothing here has anything to report.
///
/// `body` is the window without its carried heading — text prepended from
/// further up the document, occupying none of the window's own lines. Both
/// paths have to discount it: `locate_span` because it searches `body`, and the
/// hint because the model numbered its lines from the top of what it was shown,
/// and line 1 of that is the carried heading. Correcting only the first left
/// every hinted span in a continuing section `carry_lines` too far down.
fn resolve_span(
    artifact: &str,
    body: &str,
    w: &crate::store::segments::Segment,
    hint: Option<(i64, i64)>,
) -> (i64, i64) {
    let shift = w.start_line - 1 - w.carry_lines;
    let hinted = hint.map(|(a, b)| (a + shift, b + shift));
    let span = crate::infer::verify::locate_span(artifact, body, w.start_line)
        .or(hinted)
        .unwrap_or((w.start_line, w.end_line));
    // A span outside its own window would render as the wrong text.
    let clamped = (
        span.0.clamp(w.start_line, w.end_line),
        span.1.clamp(w.start_line, w.end_line),
    );
    if clamped.0 <= clamped.1 {
        clamped
    } else {
        (w.start_line, w.end_line)
    }
}

/// LLM-assisted segmentation, one window at a time.
///
/// The window rows are the job's memory. A window that succeeds is written and
/// marked `done` before the next is attempted, so the job retries and resumes
/// from the first window that has not resolved.
///
/// What a failure costs depends on whose failure it is. A reply the parser
/// cannot read is the window's own problem — the pass records it against that
/// window and carries on through the document, and the job ends in an error so
/// the queue brings it back for another try. An endpoint that will not answer
/// at all is everyone's problem, and the pass stops on the spot rather than
/// spending a timeout per remaining window to learn the same thing again.
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

    // Every window of the corpus, in order, for the neighbouring context. The
    // rows are authoritative rather than the freshly split `windows`: a corpus
    // whose windows were written by an earlier run keeps the text that run
    // produced, which is what makes a retry reproduce the same prompt.
    let all = core.store.segments_for_corpus(corpus_id).await?;
    let all_texts: Vec<&str> = all.iter().map(|s| s.text.as_str()).collect();

    // The first unreadable reply of this pass, kept so the job still ends in an
    // error and comes back for the windows it could not segment. The rest of
    // the document is worked through first.
    let mut refused: Option<crate::error::Error> = None;
    let mut in_a_row = 0usize;

    for w in core.store.pending_segments(corpus_id).await? {
        core.store.bump_segment_attempts(corpus_id, w.idx).await?;
        // The stored text, not a re-derivation from the line range: line
        // numbers cannot address a unit smaller than a line, so a corpus with
        // no newlines re-derived to the whole document for window 0 and to
        // nothing at all for every window after it.
        let text = w.text.clone();

        // The failure this catches is a job retrying an over-context window
        // against the endpoint with growing backoff and no terminal state, so
        // it is worth an assertion even though it cannot fire today.
        //
        // The ceiling is twice the budget rather than the budget itself,
        // because that is what the splitter actually promises: it flushes once
        // the buffer has *reached* the budget, and `flush` then prepends the
        // carried heading, so a window legitimately lands somewhat over. Twice
        // is the bound `text_with_no_structure_still_splits_by_line_cap` has
        // always asserted. What must never happen is unbounded — the corpus
        // that came back fifteen times its budget.
        let window_budget = segment_tokens(core.synthesizer.budget(), prompt_overhead(core));
        let window_tokens = core.counter.count(&text);
        debug_assert!(
            window_tokens <= window_budget * 2,
            "window {} is {window_tokens} tokens against a budget of {window_budget}",
            w.idx
        );
        if window_tokens > window_budget * 2 {
            tracing::error!(
                corpus_id,
                window = w.idx,
                window_tokens,
                window_budget,
                "window is far over its budget; the splitter did not shrink it"
            );
        }

        let ctx = crate::infer::context::WindowContext::build(
            &all_texts,
            w.idx as usize,
            core.synthesizer.budget().context,
            &core.counter,
        );
        let mut chunks = match core
            .synthesizer
            .segment(crate::infer::SegmentInput {
                core: &text,
                context: &ctx,
            })
            .await
        {
            Ok(chunks) => chunks,
            // The model answered and we could not read the answer. That is a
            // property of this window's text, not of the endpoint, so the
            // windows after it are still worth calling for — and before this,
            // they were not called at all until the bad one got through. One
            // duplicate JSON key cost a real document thirty untried windows
            // and twelve rounds of backoff, hours of waiting for work that was
            // never going to be attempted in those passes anyway.
            Err(e @ crate::error::Error::MalformedLlmOutput(_)) => {
                let reason = e.to_string();
                tracing::warn!(
                    corpus_id,
                    window = w.idx,
                    lines = format!("{}-{}", w.start_line, w.end_line),
                    reason,
                    "window could not be segmented; moving on to the next one"
                );
                core.store
                    .set_segment_state(corpus_id, w.idx, SegmentState::Failed, Some(&reason))
                    .await?;
                in_a_row += 1;
                refused.get_or_insert(e);
                // Carrying on past one bad window is what this arm is for.
                // Carrying on past a run of them is just paying a full call to
                // learn the same thing again: a model garbling this many in a
                // row is garbling all of them, and the windows left untried
                // stay pending for a pass that can actually use them.
                if in_a_row >= REFUSALS_BEFORE_GIVING_UP_ON_THE_PASS {
                    tracing::warn!(
                        corpus_id,
                        windows = in_a_row,
                        "the model is refusing every window; stopping this pass"
                    );
                    break;
                }
                continue;
            }
            // Anything else — a timeout, a 502, a refused connection — says the
            // endpoint is unwell. Putting thirty more windows to it costs one
            // timeout each and learns nothing, so the pass still stops here.
            Err(e) => return Err(e),
        };
        // A window the model read fine says the last refusal was about that
        // window rather than about the model, so the run starts over.
        in_a_row = 0;

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
            match core
                .synthesizer
                .segment(crate::infer::SegmentInput {
                    core: &text,
                    context: &ctx,
                })
                .await
            {
                Ok(second) => chunks = second,
                // The first reply parsed; it merely paraphrased. Keeping it and
                // letting `flag_unverified` mark what went missing beats losing
                // a window we can already read.
                Err(e) => tracing::warn!(
                    corpus_id,
                    window = w.idx,
                    error = %e,
                    "the re-segmentation failed; keeping the first reply"
                ),
            }
        }

        if !ctx.is_empty() {
            let before = chunks.len();
            chunks.retain(|c| !from_context_only(&c.text, &text, &ctx));
            let dropped = before - chunks.len();
            if dropped > 0 {
                // A rising count here means the configured model is ignoring
                // the prompt's context-only instruction. Better as a number in
                // the log than as duplicates in the base.
                tracing::info!(
                    corpus_id,
                    window = w.idx,
                    dropped,
                    "artifacts drawn from context blocks were dropped"
                );
            }
        }

        // The span is ours to compute.
        //
        // Without the carried heading, which is prepended text from further up
        // the document and occupies none of the window's lines.
        let body: String = text
            .lines()
            .skip(w.carry_lines as usize)
            .collect::<Vec<_>>()
            .join("\n");
        for c in &mut chunks {
            c.corpus_lines = Some(resolve_span(&c.text, &body, &w, c.corpus_lines));
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

    // Windows the model refused are still owed a call, so the job has to fail
    // for the queue to bring it back. `finish` is left to the caller's
    // exhausted path, which settles the source once the attempts run out.
    if let Some(e) = refused {
        return Err(e);
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

/// Did this artifact come from a context block rather than from the window?
///
/// The prompt says not to extract from context, and a small local model obeys
/// that unevenly, so the check is structural. Three outcomes matter and only
/// the middle one is a duplicate: located in the window, keep; located only in
/// context, drop, because the window that owns the material will emit it
/// properly; located nowhere, keep — that is an artifact the model reworded
/// hard, which flag_unverified has always handled and which must not start
/// silently disappearing.
fn from_context_only(
    text: &str,
    core_text: &str,
    ctx: &crate::infer::context::WindowContext,
) -> bool {
    if crate::infer::verify::locate_span(text, core_text, 1).is_some() {
        return false;
    }
    ctx.blocks()
        .any(|b| crate::infer::verify::locate_span(text, b, 1).is_some())
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

            run(&core, &src.id).await.unwrap();

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

    #[test]
    fn an_artifact_found_only_in_context_is_recognised() {
        use crate::infer::context::WindowContext;

        let core_text = "the window says something quite specific here\nand more of it";
        let ctx = WindowContext {
            opening: Some("the document opening states the version clearly".into()),
            before: None,
            after: Some("the following window describes another procedure".into()),
        };

        // Drawn from the window itself: keep.
        assert!(!from_context_only(
            "the window says something quite specific here",
            core_text,
            &ctx
        ));
        // Drawn from a context block and nowhere in the window: drop.
        assert!(from_context_only(
            "the following window describes another procedure",
            core_text,
            &ctx
        ));
        // Located nowhere at all — a heavily reworded artifact. Keep it, so it
        // reaches flag_unverified the way it does today instead of vanishing.
        assert!(!from_context_only(
            "an entirely reworded statement about unrelated matters",
            core_text,
            &ctx
        ));
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

        run(&core, &src.id).await.unwrap();

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

        run(&core, &src.id).await.unwrap();

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

        run(&core, &src.id).await.unwrap();

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
    async fn a_window_the_parser_chokes_on_does_not_hold_up_the_rest_of_the_document() {
        // The production failure this exists for: one window's reply carried a
        // duplicate JSON key, and because the pass stopped at the first error,
        // the thirty windows after it were not attempted for over an hour —
        // they waited out the job's backoff twelve times over. A reply we
        // cannot read says something about that window's text, not about the
        // endpoint, so the rest of the document still deserves its calls.
        let mut core = test_core().await;
        let body = format!("STOPHERE marker paragraph\n\n{}", multi_segment_body());
        let out = core.ingest(&body, "web", None).await.unwrap();
        assert!(segment_count(&core, &body) > 2);
        core.synthesizer = std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::unparsable_on(
            "STOPHERE",
        ));

        let err = run(&core, &out.id).await.unwrap_err();
        assert!(err.retryable(), "the refused window is still owed a call");

        let windows = core.store.segments_for_corpus(&out.id).await.unwrap();
        let refused: Vec<_> = windows
            .iter()
            .filter(|w| w.state == SegmentState::Failed)
            .collect();
        assert_eq!(refused.len(), 1, "only the unreadable window may fail");
        assert_eq!(refused[0].idx, 0);
        assert!(
            refused[0]
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("duplicate field")),
            "the window must carry the parser's own complaint"
        );

        // The point of the change: every window after the bad one was tried in
        // this same pass rather than waiting for the next.
        assert!(
            windows
                .iter()
                .skip(1)
                .all(|w| w.state == SegmentState::Done),
            "windows after the refused one were left unattempted: {:?}",
            windows.iter().map(|w| (w.idx, w.state)).collect::<Vec<_>>()
        );
        assert!(
            !core
                .store
                .artifacts_for_corpus(&out.id)
                .await
                .unwrap()
                .is_empty(),
            "the readable windows must have produced chunks"
        );
    }

    #[tokio::test]
    async fn a_model_refusing_everything_costs_a_handful_of_calls_not_one_per_window() {
        // Stepping over a bad window must not turn a broken model into a full
        // document's worth of calls every pass. Inference is the scarcest thing
        // here, and three refusals in a row already answer the question.
        let mut core = test_core().await;
        let body = multi_segment_body();
        let out = core.ingest(&body, "web", None).await.unwrap();
        let windows = segment_count(&core, &body);
        assert!(
            windows > REFUSALS_BEFORE_GIVING_UP_ON_THE_PASS + 2,
            "the fixture must have more windows than the pass will try"
        );
        // Unparsable for every window, since every window contains the marker.
        core.synthesizer = std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::unparsable_on(
            "paragraph",
        ));

        assert!(run(&core, &out.id).await.is_err());

        let tried = core
            .store
            .segments_for_corpus(&out.id)
            .await
            .unwrap()
            .iter()
            .filter(|w| w.attempts > 0)
            .count();
        assert_eq!(
            tried, REFUSALS_BEFORE_GIVING_UP_ON_THE_PASS,
            "the pass kept calling a model that had already refused three times"
        );
    }

    #[tokio::test]
    async fn a_refused_window_is_retried_on_the_next_pass_and_can_still_succeed() {
        // `pending_segments` covers failed windows too, so the next pass owes
        // the refused one another call — and a window that fails only because
        // the endpoint garbled one reply must be able to recover.
        let mut core = test_core().await;
        let body = format!("STOPHERE marker paragraph\n\n{}", multi_segment_body());
        let out = core.ingest(&body, "web", None).await.unwrap();
        core.synthesizer = std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::unparsable_on(
            "STOPHERE",
        ));
        assert!(run(&core, &out.id).await.is_err());

        // The endpoint comes back to its senses.
        core.synthesizer = std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::default());
        run(&core, &out.id).await.unwrap();

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

    fn window(start_line: i64, end_line: i64, carry_lines: i64) -> crate::store::segments::Segment {
        crate::store::segments::Segment {
            corpus_id: "c".into(),
            idx: 1,
            start_line,
            end_line,
            text: String::new(),
            carry_lines,
            state: SegmentState::Pending,
            attempts: 0,
            last_error: None,
        }
    }

    #[test]
    fn a_hinted_span_discounts_the_carried_heading_too() {
        // The window covers source lines 50-60 and opens with one heading
        // carried from further up, so the model's line 2 is source line 50.
        // The artifact is reworded past recognition, which is exactly when the
        // hint is all there is — and the path that used it was the one place
        // the carried heading was still being counted.
        let w = window(50, 60, 1);
        let body = "first body line\nsecond body line";
        assert_eq!(
            resolve_span(
                "nothing here matches the source at all",
                body,
                &w,
                Some((2, 3))
            ),
            (50, 51)
        );
    }

    #[test]
    fn a_window_carrying_nothing_reads_the_hint_straight_through() {
        let w = window(50, 60, 0);
        assert_eq!(
            resolve_span("unlocatable", "a\nb", &w, Some((2, 3))),
            (51, 52)
        );
    }

    #[test]
    fn the_artifacts_own_text_beats_the_hint() {
        // `locate_span` reads the artifact; the hint is only a claim about it.
        let w = window(50, 60, 1);
        let body = "first body line\nsecond body line";
        assert_eq!(
            resolve_span("second body line", body, &w, Some((9, 9))),
            (51, 51)
        );
    }

    #[test]
    fn a_hint_pointing_outside_the_window_falls_back_to_the_whole_window() {
        let w = window(50, 60, 1);
        // Discounting the carry can push a hint of line 1 — the heading
        // itself — below the window's first line. Clamping keeps it inside.
        assert_eq!(
            resolve_span("unlocatable", "a\nb", &w, Some((1, 1))),
            (50, 50)
        );
        assert_eq!(resolve_span("unlocatable", "a\nb", &w, None), (50, 60));
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

        run(&core, &out.id).await.unwrap();

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
