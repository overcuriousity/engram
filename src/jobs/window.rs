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
use crate::store::artifacts::{CorpusSpan, NewArtifact, SpanSource};
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
    let synth = core.synthesizer.clone();

    let all_texts: Vec<&str> = all.iter().map(|s| s.text.as_str()).collect();
    let ctx = crate::infer::context::WindowContext::build(
        &all_texts,
        idx as usize,
        synth.budget().context,
        &core.counter,
    );
    // The stored text, not a re-derivation from the line range: line numbers
    // cannot address a unit smaller than a line, so a corpus with no newlines
    // re-derived to the whole document for window 0 and to nothing at all for
    // every window after it.
    let text = w.text.clone();

    // The judged path: a capture small enough to be one window is judged by
    // the same call that rewrites it — intent, events, links — against the
    // base's nearest artifacts. A multi-window corpus is not: a manual's
    // window is not a reminder, and its links wait for the sweeps.
    // Which of the ten system prompts this window's calls are made with. Off
    // the corpus, because that is where the door stamped it.
    let lang = crate::infer::lang::of_corpus(&core.store.get_corpus(corpus_id).await?.metadata);

    let judging = all.len() == 1;
    let neighbors = if judging {
        neighbor_context(core, corpus_id, idx).await
    } else {
        Vec::new()
    };
    let shown_ids: Vec<String> = neighbors.iter().map(|n| n.id.clone()).collect();
    let ask = if judging {
        Some(build_judge_ask(core, corpus_id, neighbors).await?)
    } else {
        None
    };

    // The failure this catches is a unit retrying an over-context window against
    // the endpoint with growing backoff and no terminal state. Per-unit budgets
    // made it quieter than it used to be, not rarer: the other thirty-three
    // windows now finish and the document settles `partial`, while the one
    // window that can never fit keeps asking at the six-hour ceiling with
    // nothing in the journal naming the cause.
    //
    // The ceiling is twice the budget rather than the budget itself: the
    // splitter aims under it but a hard-cut line can land a window somewhat
    // over, and `text_with_no_structure_still_splits_within_budget` asserts
    // the same bound. What must never happen is unbounded — the corpus that
    // came back fifteen times its budget.
    let window_budget = super::synthesize::segment_budget(core, lang);
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
    let first = synth
        .segment_judged(crate::infer::SegmentInput {
            core: &text,
            context: &ctx,
            judge: ask.as_ref(),
            lang,
        })
        .await;
    permit.finished();
    let mut reply = match first {
        Ok(r) => r,
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
    if paraphrased(&reply.artifacts, &text) {
        tracing::warn!(
            corpus_id,
            window = idx,
            "literals missing; re-segmenting once"
        );
        let permit = core.gate.background().await;
        let second = synth
            .segment_judged(crate::infer::SegmentInput {
                core: &text,
                context: &ctx,
                judge: ask.as_ref(),
                lang,
            })
            .await;
        permit.finished();
        match second {
            Ok(second) => reply = second,
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

    let judgement = reply.judgement.take();
    let mut chunks = reply.artifacts;
    let neighbor_texts: Vec<&str> = ask
        .as_ref()
        .map(|a| a.neighbors.iter().map(|n| n.text.as_str()).collect())
        .unwrap_or_default();
    if !ctx.is_empty() || !neighbor_texts.is_empty() {
        let before = chunks.len();
        chunks.retain(|c| !from_context_only(&c.text, &text, &ctx, &neighbor_texts));
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

    // The floor under the prompt's "a short note is one artifact". Logged at
    // info like the context drop above: a rising count is the configured
    // model ignoring the prompt, and a number in the journal beats a base
    // full of restated judgements.
    let allowance = artifact_allowance(window_tokens);
    if chunks.len() > allowance {
        tracing::info!(
            corpus_id,
            window = idx,
            proposed = chunks.len(),
            allowance,
            "more artifacts than the window can carry; keeping the located ones"
        );
        chunks = within_allowance(chunks, &text, allowance);
    }

    // The span is ours to compute, against the window's own text. Computed
    // beside the chunks rather than written back over `corpus_lines`: that
    // field is the model's claim, and the resolved span is what became of it —
    // one of them is evidence and the other is not, so they stopped being the
    // same value.
    let body: String = text.clone();
    let spans: Vec<CorpusSpan> = chunks
        .iter()
        .map(|c| resolve_span(&c.text, &body, &w, c.corpus_lines))
        .collect();

    // A verbatim window read by the model is definitionally a promotion:
    // its passages are the index until the artifacts land, and they are
    // superseded rather than deleted. The stored flag covers the armed path;
    // the state covers a window run directly, so no route through here can
    // ever throw verbatim text away.
    let keep = core.store.segment_keeps_artifacts(corpus_id, idx).await?
        || w.state == SegmentState::Verbatim;
    let written =
        write_segment_artifacts(core, corpus_id, idx, proposed_to_new(idx, chunks, spans)).await?;
    flag_unverified(core, &written, &text).await?;
    // What is actually searchable afterwards. On a promotion that is what
    // `embed_written` hands back and not what was written: an oversize artifact
    // is replaced by siblings and its own row goes away, so anchoring the
    // judgement to `written` could anchor it to an id that no longer exists —
    // and `apply` would lose the reminder to a foreign key.
    let mut live = written;
    if keep {
        // The replacements have to be searchable before the originals stop
        // being. `supersede_covered` takes the passages out of results, and the
        // artifacts standing in for them are still `pending` until the corpus
        // embed job settling arms below gets its turn — under a backed-up queue
        // and `[pacing] cooldown_secs` that is however long the queue takes,
        // and for all of it this window's lines are reachable from neither
        // side. Embedded here, inline, so the swap is a swap.
        //
        // A refusal leaves the passages standing and the artifacts queued, and
        // the window goes back to `failed` so that something comes for it. It
        // used to be marked `done` here with a comment claiming the next settle
        // would re-run it: nothing re-runs a `done` window — `settle` and
        // `finish` both read it as resolved and `reconcile` skips it — so one
        // embedder outage during a promotion left the verbatim passages *and*
        // their synthesized replacements in results, permanently, with
        // `keep_artifacts` spent. `write_segment_artifacts` is idempotent under
        // that flag, so the retry costs a model call and writes nothing twice.
        match embed_written(core, &live).await {
            Ok(embedded) => {
                // A promotion: what the window's artifacts cover, they
                // supersede, and the passages' access comes with them. Under
                // the corpus lock as a second locked step —
                // `write_segment_artifacts` took and released it.
                {
                    let _corpus = core.corpus_lock(corpus_id).await;
                    let n = crate::jobs::promote::supersede_covered(
                        core,
                        corpus_id,
                        idx,
                        &embedded,
                        crate::store::now(),
                    )
                    .await?;
                    tracing::info!(
                        corpus_id,
                        window = idx,
                        superseded = n,
                        "promotion superseded its covered passages"
                    );
                }
                live = embedded;
            }
            Err(e) => {
                let reason = format!("the promoted window could not be embedded: {e}");
                tracing::warn!(
                    corpus_id,
                    window = idx,
                    error = %e,
                    "the promoted window's artifacts could not be embedded; \
                     leaving its passages in results and the window for a retry"
                );
                core.store
                    .set_segment_state(corpus_id, idx, SegmentState::Failed, Some(&reason))
                    .await?;
                settle(core, corpus_id).await?;
                return Err(e);
            }
        }
    }
    // The judgement, once the artifacts it is about stand. Anchored to the
    // first live one; a judgement that cannot be applied is a warning — the
    // artifacts are already the capture.
    if let Some(j) = judgement
        && let Some(anchor) = live.iter().find(|c| c.in_results())
        && let Err(e) =
            crate::jobs::judgement::apply(core, corpus_id, &anchor.id, &j, &shown_ids).await
    {
        tracing::warn!(
            corpus_id,
            error = %e,
            "the judgement could not be applied; the artifacts stand"
        );
    }
    core.store
        .set_segment_state(corpus_id, idx, SegmentState::Done, None)
        .await?;

    settle(core, corpus_id).await
}

/// Embed one promoted window's artifacts now, rather than leaving them to the
/// corpus batch. Only a promotion needs this: it is the one path that hides
/// existing text on the strength of text it has just written.
///
/// Returns the artifacts that are actually there afterwards. An oversize one is
/// replaced by siblings and its own row goes away, so what was written is not
/// always what is now searchable — and superseding a passage behind an id that
/// no longer exists is the failure this whole step is here to avoid. The
/// siblings cover the same lines and could be traced to the same passages, but
/// working that out is `resolve_span`'s job on a re-run; dropping them leaves
/// the passages standing, which is the direction promotion may fail in.
async fn embed_written(
    core: &Core,
    written: &[crate::store::artifacts::Chunk],
) -> Result<Vec<crate::store::artifacts::Chunk>> {
    for c in written {
        crate::jobs::embed::run(core, &c.id).await?;
    }
    let ids: Vec<String> = written.iter().map(|c| c.id.clone()).collect();
    let live = core.store.artifacts_by_ids(&ids).await?;
    Ok(written
        .iter()
        .filter(|c| live.iter().any(|l| l.id == c.id))
        .cloned()
        .collect())
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
            // A verbatim window was captured as passages and is owed nothing;
            // it resolves the way a synthesized one does.
            SegmentState::Done | SegmentState::Verbatim => true,
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
    // `subject` as well as the target, like every other query on this table.
    // Corpus ids are ULIDs and a cross-tenant collision is not a thing that
    // happens today, which is exactly why an unfiltered read here would stay
    // wrong quietly: the queue is instance-wide, and the invariant the control
    // schema states — no query on it written without a tenant filter — is only
    // worth anything if it has no exceptions.
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT attempts FROM jobs
          WHERE subject = ? AND stage = 'segment_window' AND target_id = ?",
    )
    .bind(&core.store.subject)
    .bind(unit_target(corpus_id, idx))
    .fetch_optional(&core.store.control.pool)
    .await?
    .unwrap_or(MAX_ATTEMPTS))
}

/// The base's nearest artifacts to this capture, shown to the judged call so
/// it can resolve references and name relations. Best-effort throughout: any
/// failure is "no neighbors", never a failed window.
async fn neighbor_context(core: &Core, corpus_id: &str, idx: i64) -> Vec<crate::infer::Neighbor> {
    let budget = core.synthesizer.budget().context.neighbors;
    if budget == 0 {
        return Vec::new();
    }
    let Ok(rows) = core.store.artifacts_for_segment(corpus_id, idx).await else {
        return Vec::new();
    };
    let Some(seed) = rows
        .iter()
        .find(|c| c.provenance == crate::store::artifacts::Provenance::Passage)
    else {
        return Vec::new();
    };
    // The seed passage may not be embedded yet — the window unit and the
    // corpus embed job race at capture. Embedding it here is idempotent and
    // cheap next to the model call this context is for.
    let mut hits = core
        .vectors
        .neighbours(&seed.id, 8)
        .await
        .unwrap_or_default();
    if hits.is_empty() {
        let _ = crate::jobs::embed::run(core, &seed.id).await;
        hits = core
            .vectors
            .neighbours(&seed.id, 8)
            .await
            .unwrap_or_default();
    }
    let per = (budget / 5).max(64);
    let mut out = Vec::new();
    for h in hits {
        if h.payload.corpus_id == corpus_id {
            continue;
        }
        // A conservative character cut against the per-neighbor budget; the
        // fence overhead is already in `ContextBudget::total`.
        let text: String = h.payload.text.chars().take(per * 3).collect();
        out.push(crate::infer::Neighbor {
            id: h.payload.artifact_id,
            title: h.payload.title,
            text,
        });
        if out.len() == 5 {
            break;
        }
    }
    out
}

/// The clock, the zone, and what the door already said — the judged call's
/// frame of reference.
async fn build_judge_ask(
    core: &Core,
    corpus_id: &str,
    neighbors: Vec<crate::infer::Neighbor>,
) -> Result<crate::infer::JudgeAsk> {
    use chrono::TimeZone;
    let src = core.store.get_corpus(corpus_id).await?;
    let tz_name = src.metadata["tz"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(String::from)
        .unwrap_or_else(|| crate::core::moments::default_zone_name(&core.time.default_tz));
    let tz = crate::core::moments::zone(Some(&tz_name));
    let now_local = tz
        .timestamp_opt(src.created_at, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M (%A)").to_string())
        .unwrap_or_default();
    Ok(crate::infer::JudgeAsk {
        now_local,
        tz: tz.name().to_string(),
        forced_intent: src.metadata["intent"].as_str().map(String::from),
        neighbors,
    })
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
/// `body` is the window's own text: the model numbered its lines from the top
/// of what it was shown, so a hinted line k is source line
/// `start_line + k - 1`.
///
/// Which of the three roads was taken comes back with the answer, in
/// `CorpusSpan::source`. All three produce lines that address the same
/// document and render the same; they are not worth the same as evidence, and
/// `promote` reads the difference before it hides a verbatim passage.
pub(crate) fn resolve_span(
    artifact: &str,
    body: &str,
    w: &crate::store::segments::Segment,
    hint: Option<(i64, i64)>,
) -> CorpusSpan {
    let shift = w.start_line - 1;
    let hinted = hint.map(|(a, b)| (a + shift, b + shift));
    let found = crate::infer::verify::locate_span(artifact, body, w.start_line);
    let (span, source) = match (found, hinted) {
        (Some(s), _) => (s, SpanSource::Located),
        (None, Some(h)) => (h, SpanSource::Claimed),
        (None, None) => ((w.start_line, w.end_line), SpanSource::Unplaced),
    };
    // A span outside its own window would render as the wrong text. What the
    // clamp does to the *source* is the point: lines it had to move are no
    // longer the ones anybody claimed, so a hint pointing outside the window
    // comes back placing nothing rather than placing an artifact at whichever
    // edge it was dragged to.
    let clamped = (
        span.0.clamp(w.start_line, w.end_line),
        span.1.clamp(w.start_line, w.end_line),
    );
    if clamped.0 <= clamped.1 {
        CorpusSpan {
            start_line: clamped.0,
            end_line: clamped.1,
            source: if clamped == span {
                source
            } else {
                SpanSource::Unplaced
            },
        }
    } else {
        CorpusSpan::unplaced(w.start_line, w.end_line)
    }
}

/// Write the chunks of one window.
///
/// "Replace, never append" by default, keyed to the window rather than to the
/// whole source, so a retry of window 4 cannot disturb windows 0 to 3. What the
/// replacement is *for* is idempotency: the insert happens here and the window
/// is marked done afterwards, so a process that dies in between re-runs this
/// function, and without the delete that window's artifacts would be written
/// twice.
///
/// The exception is a window the operator sent back to pick up lines the first
/// read missed — `store::reset_segment` with `keep_artifacts` — where the delete
/// is not idempotency but loss. Those artifacts are the parts of the window that
/// *did* arrive; they may have been edited, retagged or verified since, and none
/// of that was the problem the button was pressed about. So that read appends,
/// and any duplicate of an already-captured claim goes to the dedupe sweep,
/// which is what the sweep is for. The mark is not spent here, though it is
/// honoured here: `set_segment_state` spends it on reaching `done`, because
/// three more writes follow this one and a failure in any of them would
/// otherwise retry the window with the mark already gone — deleting exactly the
/// artifacts it exists to protect.
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
    let keep = core
        .store
        .segment_keeps_artifacts(corpus_id, segment_idx)
        .await?;
    if keep {
        // A promotion re-run: the process died between the insert and `done`.
        // Under `keep_artifacts` the write appends, so writing again would put
        // the window's artifacts in twice. A window holding passages is a
        // promotion (an operator's re-read has none); rows in it that are not
        // passages are the earlier write — return them and insert none.
        let rows = core
            .store
            .artifacts_for_segment(corpus_id, segment_idx)
            .await?;
        let is_promotion = rows
            .iter()
            .any(|c| c.provenance == crate::store::artifacts::Provenance::Passage);
        let have: Vec<_> = rows
            .into_iter()
            .filter(|c| {
                c.provenance != crate::store::artifacts::Provenance::Passage && c.in_results()
            })
            .collect();
        if is_promotion && !have.is_empty() {
            tracing::info!(
                corpus_id,
                window = segment_idx,
                "window already written under keep_artifacts; not writing again"
            );
            return Ok(have);
        }
    }
    if !keep {
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
    }
    core.store.insert_artifacts(corpus_id, &new).await
}

/// The most artifacts a window of `input_tokens` may yield.
///
/// One per thirty tokens, never fewer than one. A 9B model told "if a passage
/// covers three techniques, emit three" and then handed a JUDGE block naming
/// three things wrote a one-sentence reminder up as six artifacts, four of
/// them restating the judgement. The prompt now forbids that; this is the
/// floor under the prompt, because a prompt is a request and a truncation is
/// not. Thirty is generous: a three-line bug report of seventy tokens still
/// gets two, and a chapter gets a hundred.
pub(crate) fn artifact_allowance(input_tokens: usize) -> usize {
    (input_tokens / 30).max(1)
}

/// The artifacts that fit the allowance, located ones first.
///
/// When the model over-delivers, what is kept is what can be traced to the
/// window: an artifact whose text locates verbatim in the source is evidence,
/// a rewrite is a claim. Among equals the model's own order stands, which is
/// what a stable sort gives.
///
/// The allowance never cuts into the evidence. Every located artifact is
/// kept even when there are more of them than the allowance permits — a
/// window that really does reproduce five of its own passages has said so
/// five times — and the allowance decides only how many rewrites ride along
/// behind them.
pub(crate) fn within_allowance(
    mut chunks: Vec<crate::infer::ProposedArtifact>,
    window: &str,
    allowance: usize,
) -> Vec<crate::infer::ProposedArtifact> {
    if chunks.len() <= allowance {
        return chunks;
    }
    chunks.sort_by_key(|c| crate::infer::verify::locate_span(&c.text, window, 1).is_none());
    let located = chunks
        .iter()
        .filter(|c| crate::infer::verify::locate_span(&c.text, window, 1).is_some())
        .count();
    chunks.truncate(allowance.max(located));
    chunks
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
///
/// `neighbors` are checked alongside the context blocks and are not optional.
/// The prompt labels that block "context only" like the rest, but neighbour
/// text lives on `JudgeAsk`, not on `WindowContext`, so `blocks()` never sees
/// it — and at the shipped `context_neighbor_tokens = 1024` it is the largest
/// context block on the prompt, larger than the opening and the overlap
/// together. The one block with no structural guard was the one most worth
/// guarding: a neighbour is an artifact the base already holds, so a model
/// restating one writes a second copy of it.
pub(crate) fn from_context_only(
    text: &str,
    core_text: &str,
    ctx: &crate::infer::context::WindowContext,
    neighbors: &[&str],
) -> bool {
    if crate::infer::verify::locate_span(text, core_text, 1).is_some() {
        return false;
    }
    ctx.blocks()
        .chain(neighbors.iter().copied())
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
    spans: Vec<CorpusSpan>,
) -> Vec<NewArtifact> {
    // Positional, so they have to be the same list: `spans` is built by
    // mapping over the chunks after the context-block drop, and nothing may
    // filter between.
    debug_assert_eq!(proposed.len(), spans.len());
    let mut spans = spans.into_iter();
    proposed
        .into_iter()
        .enumerate()
        .map(|(i, p)| NewArtifact {
            ordinal: i as i64,
            text: p.text,
            corpus_span: spans.next(),
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

    #[tokio::test]
    async fn a_windows_attempts_are_read_from_this_tenants_row_only() {
        // The queue is one instance-wide table, and this was the last
        // production query on it written without a `subject` predicate. ULID
        // corpus ids make a collision impractical, which is exactly why it
        // would have stayed wrong quietly — the invariant the control schema
        // states is only worth something if it has no exceptions.
        let core = crate::core::test_support::test_core().await;
        let src = core
            .store
            .insert_corpus("a document", "web", None)
            .await
            .unwrap();
        let target = unit_target(&src.id, 0);

        // The other tenant's row first, so an unfiltered `fetch_optional` finds
        // it before ours.
        core.store
            .control
            .provision("someone-else", None)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO jobs (subject, stage, target_kind, target_id, state, attempts, run_after, created_at, seq, class)
             VALUES ('someone-else', 'segment_window', 'segment', ?, 'pending', 4, 0, 0, 0, 0)",
        )
        .bind(&target)
        .execute(&core.store.control.pool)
        .await
        .unwrap();

        core.store
            .enqueue_seq(
                crate::store::jobs::Stage::SegmentWindow,
                "segment",
                &target,
                0,
            )
            .await
            .unwrap();
        sqlx::query("UPDATE jobs SET attempts = 2 WHERE subject = ? AND target_id = ?")
            .bind(&core.store.subject)
            .bind(&target)
            .execute(&core.store.control.pool)
            .await
            .unwrap();

        assert_eq!(attempts_for(&core, &src.id, 0).await.unwrap(), 2);
    }
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
                .all(|w| w.state == SegmentState::Verbatim),
            "a unit segmented a window that was not its own"
        );
    }

    #[tokio::test]
    async fn a_rerun_replaces_a_window_unless_it_was_sent_back_for_missed_lines() {
        // The two reasons to run a window twice want opposite answers. A retry
        // must replace, or a process killed between the insert and the "done"
        // mark writes that window's artifacts twice. A re-read for lines the
        // first pass missed must not, or it deletes the artifacts that *did*
        // arrive — with whatever an operator has edited, tagged or verified on
        // them since — for lines that were never the problem.
        let core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        run(&core, &unit_target(&out.id, 0)).await.unwrap();
        let first = core
            .store
            .artifact_ids_for_segment(&out.id, 0)
            .await
            .unwrap();
        assert!(!first.is_empty(), "the fixture must write something");

        // A plain reset: the second read stands in place of the first.
        core.store.reset_segment(&out.id, 0, false).await.unwrap();
        run(&core, &unit_target(&out.id, 0)).await.unwrap();
        let replaced = core
            .store
            .artifact_ids_for_segment(&out.id, 0)
            .await
            .unwrap();
        assert!(
            replaced.iter().all(|id| !first.contains(id)),
            "a retry left the first read's artifacts behind"
        );

        // Sent back for missed lines: the second read is added to the first.
        core.store.reset_segment(&out.id, 0, true).await.unwrap();
        run(&core, &unit_target(&out.id, 0)).await.unwrap();
        let kept = core
            .store
            .artifact_ids_for_segment(&out.id, 0)
            .await
            .unwrap();
        assert!(
            replaced.iter().all(|id| kept.contains(id)),
            "the re-read deleted what the window had already produced"
        );
        assert!(kept.len() > replaced.len(), "and it added what it read");
        assert!(
            !core
                .store
                .segment_keeps_artifacts(&out.id, 0)
                .await
                .unwrap(),
            "the mark is spent, so the next ordinary retry replaces again"
        );
    }

    #[tokio::test]
    async fn the_re_read_mark_outlives_the_write_that_honoured_it() {
        // Three DB writes follow `write_segment_artifacts` — `flag_unverified`,
        // the state change and `settle` — and `SQLITE_BUSY` among them is
        // routine now that two workers can be in one corpus. Spending the mark
        // at the write meant the retry that followed such a failure replaced,
        // deleting the very artifacts the mark exists to keep.
        let core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        run(&core, &unit_target(&out.id, 0)).await.unwrap();
        let first = core
            .store
            .artifact_ids_for_segment(&out.id, 0)
            .await
            .unwrap();

        core.store.reset_segment(&out.id, 0, true).await.unwrap();
        write_segment_artifacts(&core, &out.id, 0, vec![])
            .await
            .unwrap();
        assert!(
            core.store
                .segment_keeps_artifacts(&out.id, 0)
                .await
                .unwrap(),
            "a write is not the end of the run; the mark must survive it"
        );

        // So the retry that a later failure forces still appends.
        run(&core, &unit_target(&out.id, 0)).await.unwrap();
        let kept = core
            .store
            .artifact_ids_for_segment(&out.id, 0)
            .await
            .unwrap();
        assert!(
            first.iter().all(|id| kept.contains(id)),
            "the retry deleted what the first read had produced"
        );
        assert!(
            !core
                .store
                .segment_keeps_artifacts(&out.id, 0)
                .await
                .unwrap(),
            "and reaching `done` spends it"
        );
    }

    #[tokio::test]
    async fn a_promotion_embeds_what_it_wrote_before_it_hides_anything() {
        // The window that promotes is the one window whose artifacts cannot
        // wait for the corpus batch: `supersede_covered` takes the passages out
        // of results, and until the replacements are indexed the lines are
        // reachable from neither side. Under a backed-up queue that gap is as
        // long as the queue.
        let core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        // Capture writes the passages and arms the window with `keep`: the
        // one run below is already the promotion-shaped read.
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        run(&core, &unit_target(&out.id, 0)).await.unwrap();

        let rows = core.store.artifacts_for_segment(&out.id, 0).await.unwrap();
        let written: Vec<String> = rows
            .iter()
            .filter(|c| c.provenance != crate::store::artifacts::Provenance::Passage)
            .map(|c| c.id.clone())
            .collect();
        assert!(!written.is_empty(), "the fixture must promote something");
        for c in core.store.artifacts_by_ids(&written).await.unwrap() {
            assert_eq!(
                c.embed_state,
                crate::store::artifacts::EmbedState::Embedded,
                "the promotion superseded while {} was still {:?}",
                c.id,
                c.embed_state
            );
        }
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

    /// A window fixture for the span tests: the text is irrelevant to them,
    /// the line range is the whole point.
    fn window(start_line: i64, end_line: i64) -> crate::store::segments::Segment {
        crate::store::segments::Segment {
            corpus_id: "c".into(),
            idx: 1,
            start_line,
            end_line,
            text: String::new(),
            state: SegmentState::Pending,
            attempts: 0,
            last_error: None,
        }
    }

    #[test]
    fn an_unlocatable_artifact_reads_the_hint_against_the_window() {
        let w = window(50, 60);
        // The lines are the hint's, shifted onto the document — and they come
        // back marked as what they are: a claim, which places the artifact
        // without verifying it.
        assert_eq!(
            resolve_span("unlocatable", "a\nb", &w, Some((2, 3))),
            CorpusSpan::claimed(51, 52)
        );
    }

    #[test]
    fn the_artifacts_own_text_beats_the_hint() {
        // `locate_span` reads the artifact; the hint is only a claim about it.
        let w = window(50, 60);
        let body = "first body line\nsecond body line";
        assert_eq!(
            resolve_span("second body line", body, &w, Some((9, 9))),
            CorpusSpan::located(51, 51)
        );
    }

    #[test]
    fn a_hint_pointing_outside_the_window_falls_back_to_the_whole_window() {
        let w = window(50, 60);
        assert_eq!(
            resolve_span("unlocatable", "a\nb", &w, Some((-3, -3))),
            CorpusSpan::unplaced(50, 50)
        );
        assert_eq!(
            resolve_span("unlocatable", "a\nb", &w, None),
            CorpusSpan::unplaced(50, 60)
        );
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
            &ctx,
            &[]
        ));
        // Drawn from a context block and nowhere in the window: drop.
        assert!(from_context_only(
            "the following window describes another procedure",
            core_text,
            &ctx,
            &[]
        ));
        // Located nowhere at all — a heavily reworded artifact. Keep it, so it
        // reaches flag_unverified the way it does today instead of vanishing.
        assert!(!from_context_only(
            "an entirely reworded statement about unrelated matters",
            core_text,
            &ctx,
            &[]
        ));
    }

    #[test]
    fn the_allowance_is_one_artifact_per_thirty_tokens_and_never_zero() {
        assert_eq!(artifact_allowance(0), 1);
        assert_eq!(artifact_allowance(20), 1);
        assert_eq!(artifact_allowance(29), 1);
        assert_eq!(artifact_allowance(70), 2);
        assert_eq!(artifact_allowance(3000), 100);
    }

    #[test]
    fn over_allowance_keeps_located_artifacts_first_then_the_models_order() {
        let window = "erinnere mich an den Gastroentereologentermin, Freitag 13:45 uhr.";
        let art = |text: &str| crate::infer::ProposedArtifact {
            text: text.into(),
            title: None,
            category: None,
            tags: vec![],
            corpus_lines: None,
            caveats: vec![],
            pinned: false,
        };
        let chunks = vec![
            art("The note references a specific future event on Friday at 13:45."),
            art("erinnere mich an den Gastroentereologentermin, Freitag 13:45 uhr."),
            art("The reminder is set for Friday, 2026-09-05 at 13:45."),
        ];
        let kept = within_allowance(chunks, window, 1);
        assert_eq!(kept.len(), 1);
        assert!(kept[0].text.starts_with("erinnere mich"));

        // The evidence is never cut: three located artifacts survive an
        // allowance of one, and only the rewrites behind them are dropped.
        let chunks = vec![
            art("The reminder is set for Friday, 2026-09-05 at 13:45."),
            art("erinnere mich an den Gastroentereologentermin,"),
            art("Freitag 13:45 uhr."),
        ];
        let kept = within_allowance(chunks, window, 1);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|c| !c.text.starts_with("The reminder")));

        // Under the allowance nothing moves.
        let chunks = vec![art("b"), art("a")];
        let kept = within_allowance(chunks, window, 5);
        assert_eq!(
            kept.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            vec!["b", "a"]
        );
    }

    /// The NEIGHBORS block is on the prompt and was on no guard. A neighbour is
    /// an artifact the base already holds, so restating one is a duplicate of
    /// something already stored — the exact outcome this check exists to stop.
    #[test]
    fn an_artifact_restating_a_neighbour_is_recognised() {
        use crate::infer::context::WindowContext;

        let core_text = "the window says something quite specific here";
        let ctx = WindowContext {
            opening: None,
            before: None,
            after: None,
        };
        let neighbors = ["PUID is Microsoft's per-user identifier for a licence"];

        assert!(from_context_only(
            "PUID is Microsoft's per-user identifier for a licence",
            core_text,
            &ctx,
            &neighbors
        ));
        // The window still wins: material in both places belongs to the window.
        assert!(!from_context_only(
            "the window says something quite specific here",
            core_text,
            &ctx,
            &neighbors
        ));
    }

    #[tokio::test]
    async fn settle_treats_a_verbatim_segment_as_resolved() {
        // One promoted window done, every other window verbatim: the corpus
        // must finish — that is what arms the embed for the promoted artifacts.
        let core = test_core().await;
        let src = core
            .store
            .insert_corpus("l1\nl2\nl3\nl4", "web", None)
            .await
            .unwrap();
        core.store
            .upsert_segments(
                &src.id,
                &[
                    crate::store::segments::NewSegment {
                        start_line: 1,
                        end_line: 2,
                        text: "l1\nl2",
                    },
                    crate::store::segments::NewSegment {
                        start_line: 3,
                        end_line: 4,
                        text: "l3\nl4",
                    },
                ],
            )
            .await
            .unwrap();
        core.store.mark_segments_verbatim(&src.id).await.unwrap();
        core.store
            .set_segment_state(&src.id, 0, SegmentState::Done, None)
            .await
            .unwrap();
        core.store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "l1 l2".into(),
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
        settle(&core, &src.id).await.unwrap();
        let s = core.store.get_corpus(&src.id).await.unwrap();
        assert_eq!(
            s.status,
            crate::store::corpora::CorpusStatus::Embedding,
            "{:?}",
            s.status
        );
        assert!(
            core.store
                .live_job(crate::store::jobs::Stage::Embed, &src.id)
                .await
                .unwrap()
        );
    }
}
