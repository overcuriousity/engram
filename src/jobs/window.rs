//! One window, one inference call.
//!
//! This is the unit the whole job model is built around: the smallest thing the
//! synthesizer can be asked to do. Below it there is nothing — one call returns
//! every artifact a window yields, eight on average, and a window cannot be
//! subdivided without asking for artifacts nobody has enumerated yet. Above it
//! used to be a job covering a whole document, which is what gave thirty-four
//! windows one attempt budget between them and cost a single unreadable reply
//! twelve rounds of a six-hour backoff while the other thirty-three waited.

use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::artifacts::{CorpusSpan, NewArtifact};
use crate::store::jobs::MAX_ATTEMPTS;
use crate::store::segments::SegmentState;

/// A window's address in the queue.
///
/// The corpus id is a UUID and cannot contain `#`, so the split is unambiguous
/// from the right.
pub fn unit_target(corpus_id: &str, idx: i64) -> String {
    format!("{corpus_id}#{idx}")
}

pub fn parse_target(target: &str) -> Option<(&str, i64)> {
    let (corpus_id, idx) = target.rsplit_once('#')?;
    Some((corpus_id, idx.parse().ok()?))
}

pub async fn run(core: &Core, target: &str) -> Result<()> {
    let (corpus_id, idx) = parse_target(target).ok_or(Error::NotFound)?;
    let all = core.store.segments_for_corpus(corpus_id).await?;
    let w = all
        .iter()
        .find(|s| s.idx == idx)
        // Re-segmenting can shorten a document, leaving a unit addressed to a
        // window that no longer exists. `run_one` closes a NotFound job rather
        // than retrying something that can never come back.
        .ok_or(Error::NotFound)?
        .clone();

    if w.state == SegmentState::Done {
        return Ok(());
    }

    let all_texts: Vec<&str> = all.iter().map(|s| s.text.as_str()).collect();
    let ctx = crate::infer::context::WindowContext::build(
        &all_texts,
        idx as usize,
        core.synthesizer.budget().context,
        &core.counter,
    );
    // The stored text, not a re-derivation from the line range: line numbers
    // cannot address a unit smaller than a line, so a corpus with no newlines
    // re-derived to the whole document for window 0 and to nothing at all for
    // every window after it.
    let text = w.text.clone();

    // The failure this catches is a unit retrying an over-context window against
    // the endpoint with growing backoff and no terminal state. Per-unit budgets
    // made it quieter than it used to be, not rarer: the other thirty-three
    // windows now finish and the document settles `partial`, while the one
    // window that can never fit keeps asking at the six-hour ceiling with
    // nothing in the journal naming the cause.
    //
    // The ceiling is twice the budget rather than the budget itself, because
    // that is what the splitter actually promises: it flushes once the buffer
    // has *reached* the budget, and `flush` then prepends the carried heading,
    // so a window legitimately lands somewhat over. Twice is the bound
    // `text_with_no_structure_still_splits_by_line_cap` has always asserted.
    // What must never happen is unbounded — the corpus that came back fifteen
    // times its budget.
    let window_budget = crate::infer::budget::segment_tokens(
        core.synthesizer.budget(),
        super::synthesize::prompt_overhead(core),
    );
    let window_tokens = core.counter.count(&text);
    debug_assert!(
        window_tokens <= window_budget * 2,
        "window {idx} is {window_tokens} tokens against a budget of {window_budget}"
    );
    if window_tokens > window_budget * 2 {
        tracing::error!(
            corpus_id,
            window = idx,
            window_tokens,
            window_budget,
            "window is far over its budget; the splitter did not shrink it"
        );
    }

    let permit = core.gate.background().await;
    let first = core
        .synthesizer
        .segment(crate::infer::SegmentInput {
            core: &text,
            context: &ctx,
        })
        .await;
    permit.finished();
    let mut chunks = match first {
        Ok(c) => c,
        Err(e) => {
            let reason = e.to_string();
            tracing::warn!(
                corpus_id,
                window = idx,
                lines = format!("{}-{}", w.start_line, w.end_line),
                reason,
                "window could not be segmented"
            );
            core.store
                .set_segment_state(corpus_id, idx, SegmentState::Failed, Some(&reason))
                .await?;
            settle(core, corpus_id).await?;
            return Err(e);
        }
    };

    // The model was told to keep commands, paths and flags verbatim. If it did
    // not, one more attempt sometimes gets it right; a second failure is stored
    // with a flag rather than dropped, because a visible warning beats losing
    // the chapter.
    if paraphrased(&chunks, &text) {
        tracing::warn!(
            corpus_id,
            window = idx,
            "literals missing; re-segmenting once"
        );
        let permit = core.gate.background().await;
        let second = core
            .synthesizer
            .segment(crate::infer::SegmentInput {
                core: &text,
                context: &ctx,
            })
            .await;
        permit.finished();
        match second {
            Ok(second) => chunks = second,
            // The first reply parsed; it merely paraphrased. Keeping it and
            // letting `flag_unverified` mark what went missing beats losing a
            // window we can already read.
            Err(e) => {
                tracing::warn!(
                    corpus_id,
                    window = idx,
                    error = %e,
                    "the re-segmentation failed; keeping the first reply"
                );
            }
        }
    }

    if !ctx.is_empty() {
        let before = chunks.len();
        chunks.retain(|c| !from_context_only(&c.text, &text, &ctx));
        let dropped = before - chunks.len();
        if dropped > 0 {
            // A rising count here means the configured model is ignoring the
            // prompt's context-only instruction. Better as a number in the log
            // than as duplicates in the base.
            tracing::info!(
                corpus_id,
                window = idx,
                dropped,
                "artifacts drawn from context blocks were dropped"
            );
        }
    }

    // The span is ours to compute. Without the carried heading, which is
    // prepended text from further up the document and occupies none of this
    // window's lines.
    let body: String = text
        .lines()
        .skip(w.carry_lines as usize)
        .collect::<Vec<_>>()
        .join("\n");
    for c in &mut chunks {
        c.corpus_lines = Some(resolve_span(&c.text, &body, &w, c.corpus_lines));
    }

    let written =
        write_segment_artifacts(core, corpus_id, idx, proposed_to_new(idx, chunks)).await?;
    flag_unverified(core, &written, &text).await?;
    core.store
        .set_segment_state(corpus_id, idx, SegmentState::Done, None)
        .await?;

    settle(core, corpus_id).await
}

/// Everything that can only be decided once every window has resolved.
///
/// "Resolved" has to include a window that has spent its attempts, and that is
/// the whole subtlety here. Engram never abandons work, so a window the model
/// will not read stays queued at the six-hour ceiling forever — and if settling
/// waited for it, the thirty-three windows that came back perfectly would never
/// be embedded, never be searchable, and the document would sit in `segmenting`
/// for good. The corpus settles around such a window and reports `partial`.
///
/// If it later succeeds this runs again, which is why every step of `finish` is
/// idempotent: ordinals are renumbered, coverage recomputed, the embed job
/// re-armed for the artifacts that have appeared since.
pub(crate) async fn settle(core: &Core, corpus_id: &str) -> Result<()> {
    // Held for the whole read-then-finish, so no window's artifacts can be
    // rewritten underneath the decision or the renumbering it leads to. Taken
    // here rather than at each caller: this and `write_segment_artifacts` are the
    // only two holders, neither is reachable from inside the other, and a lock
    // acquired at call sites is one somebody eventually forgets to acquire.
    let _corpus = core.corpus_lock(corpus_id).await;
    for w in core.store.segments_for_corpus(corpus_id).await? {
        let resolved = match w.state {
            SegmentState::Done => true,
            // `jobs.attempts` rather than a counter on the segment: the unit is
            // the job now, and two counters for one thing is what made the
            // original incident so hard to read.
            SegmentState::Failed => match attempts_for(core, corpus_id, w.idx).await {
                Ok(n) => n >= MAX_ATTEMPTS,
                // A query that failed says nothing about the window, and now
                // that two workers can be in one corpus at once, `SQLITE_BUSY`
                // here is routine. Reading it as "spent" would settle a document
                // that still has retries left and report it `partial`. Deferring
                // costs one settle: the next window to resolve runs this again,
                // and the reconciliation sweep finishes the document if none
                // does.
                Err(e) => {
                    tracing::warn!(
                        corpus_id,
                        window = w.idx,
                        error = %e,
                        "could not read the window's attempts; deferring the settle"
                    );
                    false
                }
            },
            SegmentState::Pending => false,
        };
        if !resolved {
            return Ok(());
        }
    }
    crate::jobs::synthesize::finish(core, corpus_id).await
}

/// How many times this window's unit has been claimed.
///
/// A missing row means the unit is gone — dropped as stale, or never armed —
/// and nothing is going to try that window again, so it counts as spent rather
/// than holding up the document forever. That default belongs to the missing
/// row alone: a failed query is not an answer, and the caller decides what to do
/// about not having one.
async fn attempts_for(core: &Core, corpus_id: &str, idx: i64) -> Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT attempts FROM jobs WHERE stage = 'segment_window' AND target_id = ?",
    )
    .bind(unit_target(corpus_id, idx))
    .fetch_optional(&core.store.pool)
    .await?
    .unwrap_or(MAX_ATTEMPTS))
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
pub(crate) fn resolve_span(
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

/// Replace the chunks of one window. Same "replace, never append" guarantee as
/// before; the key is the window rather than the whole source, so a retry of
/// window 4 cannot disturb windows 0 to 3.
///
/// The delete and the insert are one unit against the rest of the document: a
/// `settle` running between them would renumber a document that is missing a
/// window's worth of artifacts and hand out ordinals this insert then duplicates.
pub(crate) async fn write_segment_artifacts(
    core: &Core,
    corpus_id: &str,
    segment_idx: i64,
    new: Vec<NewArtifact>,
) -> Result<Vec<crate::store::artifacts::Chunk>> {
    let _corpus = core.corpus_lock(corpus_id).await;
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
pub(crate) fn from_context_only(
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
pub(crate) fn paraphrased(chunks: &[crate::infer::ProposedArtifact], window: &str) -> bool {
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
pub(crate) async fn flag_unverified(
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

pub(crate) fn proposed_to_new(
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
    use crate::core::test_support::test_core;

    #[test]
    fn a_unit_target_round_trips() {
        let t = unit_target("019ff75a-61b1-7703-aea9-f2a3ae9a0ddd", 17);
        assert_eq!(t, "019ff75a-61b1-7703-aea9-f2a3ae9a0ddd#17");
        assert_eq!(
            parse_target(&t),
            Some(("019ff75a-61b1-7703-aea9-f2a3ae9a0ddd", 17))
        );
        assert_eq!(parse_target("no-hash"), None);
        assert_eq!(parse_target("bad#notanumber"), None);
    }

    #[tokio::test]
    async fn a_unit_segments_exactly_its_own_window() {
        let core = test_core().await;
        let body = (0..400)
            .map(|i| format!("paragraph number {i} with some filler text"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let out = core.ingest(&body, "web", None).await.unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();

        run(&core, &unit_target(&out.id, 0)).await.unwrap();

        let windows = core.store.segments_for_corpus(&out.id).await.unwrap();
        assert!(windows.len() > 1, "the fixture must span several windows");
        assert_eq!(windows[0].state, SegmentState::Done);
        assert!(
            windows[1..]
                .iter()
                .all(|w| w.state == SegmentState::Pending),
            "a unit segmented a window that was not its own"
        );
    }

    #[tokio::test]
    async fn a_unit_whose_window_no_longer_exists_is_not_found() {
        // Re-segmenting can shorten a document. The stale unit must be dropped
        // by run_one's NotFound path rather than retried for six hours.
        let core = test_core().await;
        let out = core.ingest("alpha\n\nbeta", "web", None).await.unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();

        let err = run(&core, &unit_target(&out.id, 99)).await.unwrap_err();
        assert!(matches!(err, Error::NotFound));
    }

    #[tokio::test]
    async fn an_unreadable_reply_fails_only_this_window() {
        let mut core = test_core().await;
        let out = core.ingest("alpha\n\nbeta", "web", None).await.unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        core.synthesizer =
            std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::unparsable_on("alpha"));

        let err = run(&core, &unit_target(&out.id, 0)).await.unwrap_err();
        assert!(err.retryable(), "the window is still owed a call");
        let w = &core.store.segments_for_corpus(&out.id).await.unwrap()[0];
        assert_eq!(w.state, SegmentState::Failed);
        assert!(
            w.last_error
                .as_deref()
                .is_some_and(|e| e.contains("duplicate field")),
            "the window must carry the parser's own complaint"
        );
    }

    /// A window fixture for the span tests: the text is irrelevant to them, the
    /// line range and the carried heading are the whole point.
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
}
