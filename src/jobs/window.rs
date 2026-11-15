//! One window, one inference call: the smallest thing the synthesizer can be asked to do, with its
//! own attempt budget.

use crate::core::Core;
use crate::core::judging::catalogue::{asks_reminder, has_date};
use crate::core::judging::{Asked, state};
use crate::error::{Error, Result};
use crate::store::artifacts::{CorpusSpan, NewArtifact, SpanSource};
use crate::store::jobs::MAX_ATTEMPTS;
use crate::store::segments::SegmentState;

/// The pre-read's line: either answer at or above it asks the writer for the judged schema. Low,
/// because a missed reminder is worse than a wasted schema.
pub const PRE_READ_LINE: f64 = 0.3;

/// A corpus id is a UUID and cannot contain `#`, so the split is unambiguous from the right.
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
        // `run_one` closes a NotFound job rather than retrying a window re-segmenting removed.
        .ok_or(Error::NotFound)?
        .clone();

    if w.state == SegmentState::Done {
        return Ok(());
    }
    // No synthesizer: retrying finds the same absence. Marked `verbatim`, or reconcile re-arms it
    // forever.
    let Some(synth) = core.synthesizer.clone() else {
        tracing::info!(target, "no [infer.synthesize]: window left verbatim");
        core.store
            .set_segment_state(corpus_id, idx, SegmentState::Verbatim, None)
            .await?;
        return Ok(());
    };

    let all_texts: Vec<&str> = all.iter().map(|s| s.text.as_str()).collect();
    let ctx = crate::infer::context::WindowContext::build(
        &all_texts,
        idx as usize,
        synth.budget().context,
        &core.counter,
    );
    // The stored text: line numbers cannot address a unit smaller than a line.
    let text = w.text.clone();

    // The language is the corpus's.
    let corpus = core.store.get_corpus(corpus_id).await?;
    let lang = crate::infer::lang::of_corpus(&corpus.metadata);

    // No budget is not a small budget: the call would return a non-retryable 400 with no cause
    // named.
    let Some(window_budget) = super::synthesize::segment_budget(core, lang) else {
        let reason = super::synthesize::no_budget_reason(core, lang);
        tracing::error!(corpus_id, window = idx, %reason, "no window can be synthesized");
        return Err(crate::error::Error::Validation(reason));
    };

    // Only a single-window capture is judged by the call that rewrites it, and only when the
    // pre-read finds something a moment could hang on.
    let judging = wants_judged(
        core,
        corpus_id,
        &text,
        corpus.metadata["intent"].as_str(),
        all.len() == 1,
    )
    .await?;
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

    // One retry for lost literals; a second failure is stored with a flag rather than dropped.
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
            Ok(second) => {
                // `parse_judged` returns `judgement: None` for a salvaged reply, which is not a
                // retraction.
                let first_judgement = reply.judgement.take();
                reply = second;
                // `says_something()`, not `is_some()`: every parsed reply is `Some`, often an empty
                // default.
                if !reply.judgement.as_ref().is_some_and(|j| j.says_something()) {
                    reply.judgement = first_judgement;
                }
            }
            // The first reply parsed; `flag_unverified` marks what went missing.
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
            // A rising count means the model is ignoring the prompt's context-only instruction.
            tracing::info!(
                corpus_id,
                window = idx,
                dropped,
                "artifacts drawn from context blocks were dropped"
            );
        }
    }

    // The floor under the prompt's "a short note is one artifact".
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

    // A re-run that will hand back what is already written is not checked again.
    let mut checked = written_already(core, corpus_id, idx).await?.is_none();
    let mut batch = if checked {
        faithful(core, target, &text, chunks).await
    } else {
        Batch::unasked(chunks)
    };

    // A verbatim window read by the model is a promotion: its passages are superseded, never
    // deleted.
    let keep = core.store.segment_write_appends(corpus_id, idx).await?;
    let body: String = text.clone();
    let (written, fresh) = loop {
        // Kept beside the chunks: `corpus_lines` is the model's claim, the span is what became of
        // it.
        let spans: Vec<CorpusSpan> = batch
            .kept
            .iter()
            .map(|c| resolve_span(&c.text, &body, &w, c.corpus_lines))
            .collect();
        let new = proposed_to_new(idx, batch.kept.clone(), spans);
        match write_window(core, corpus_id, idx, new, checked, batch.dropped).await? {
            Written::New(rows) => break (rows, true),
            Written::Already(rows) => break (rows, false),
            // What the first probe would have handed back is gone since: checked after all.
            Written::Unchecked => {
                batch = faithful(core, target, &text, batch.kept).await;
                checked = true;
            }
        }
    };
    // The reasons lie beside the batch, so they name rows only where the batch was inserted.
    let unchecked: &[Option<String>] = if fresh { &batch.unchecked } else { &[] };
    flag_unverified(core, &written, &text, unchecked).await?;
    // `embed_written`'s result, not `written`: an oversize artifact is replaced by siblings.
    let mut live = written;
    // A replacing write that kept the passages for a dropped artifact swaps as a promotion does.
    if keep || batch.dropped {
        // Embedded inline so the swap is a swap: the replacements must be searchable before the
        // passages stop being. On refusal the window goes back to `failed`; nothing re-runs a
        // `done` window.
        match embed_written(core, &live).await {
            Ok(embedded) => {
                // A second locked step; `write_window` took and released the corpus lock.
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
    // Anchored to the first live row, which may be the verbatim passage: a pure reminder writes
    // none.
    let anchor = if judgement.is_some() || !shown_ids.is_empty() {
        match live.iter().find(|c| c.in_results()) {
            Some(c) => Some(c.id.clone()),
            None => core
                .store
                .artifacts_for_segment(corpus_id, idx)
                .await?
                .into_iter()
                .find(|c| c.in_results())
                .map(|c| c.id),
        }
    } else {
        None
    };
    if let Some(j) = judgement {
        match &anchor {
            Some(anchor) => {
                if let Err(e) = crate::jobs::judgement::apply(core, corpus_id, anchor, &j).await {
                    tracing::warn!(
                        corpus_id,
                        error = %e,
                        "the judgement could not be applied; the artifacts stand"
                    );
                }
            }
            None => tracing::warn!(
                corpus_id,
                window = idx,
                "the judgement has nothing to hang on: the window wrote no artifact \
                 and holds no passage"
            ),
        }
    }
    core.store
        .set_segment_state(corpus_id, idx, SegmentState::Done, None)
        .await?;

    let settled = settle(core, corpus_id).await;
    // After the capture is stored and settled: a link question that fails costs no capture.
    if let Some(anchor) = &anchor
        && !shown_ids.is_empty()
        && let Err(e) = crate::jobs::associate::link_shown(core, anchor, &shown_ids).await
    {
        tracing::warn!(corpus_id, error = %e, "the shown neighbours could not be asked about");
    }
    settled
}

/// What the check left of a proposed batch.
struct Batch {
    kept: Vec<crate::infer::ProposedArtifact>,
    /// Beside `kept`: why one the decision model would not judge stands unchecked.
    unchecked: Vec<Option<String>>,
    /// Whether the check dropped any: the window's passages are then all that holds their lines.
    dropped: bool,
}

impl Batch {
    fn unasked(kept: Vec<crate::infer::ProposedArtifact>) -> Self {
        Batch {
            unchecked: vec![None; kept.len()],
            kept,
            dropped: false,
        }
    }
}

/// After the literal checks, each proposed artifact against its window: one that adds what the window
/// does not state is dropped, and the window's passages stay. A failed check never holds the capture:
/// the rest stand on the literal checks alone.
async fn faithful(
    core: &Core,
    target: &str,
    window: &str,
    chunks: Vec<crate::infer::ProposedArtifact>,
) -> Batch {
    use crate::core::judging::check::{Checked, Which, check};
    let original = [state::Note {
        title: None,
        day: None,
        text: window,
    }];
    let mut batch = Batch::unasked(Vec::with_capacity(chunks.len()));
    let mut asking = true;
    for (n, c) in chunks.into_iter().enumerate() {
        let mut unchecked = None;
        if asking {
            let rewrite = state::Note {
                title: c.title.as_deref(),
                day: None,
                text: &c.text,
            };
            // Named by its place in the proposed batch: a dropped artifact has no row of its own.
            let subject = format!("{target}#{n}");
            match check(core, &subject, &original, &rewrite, Which::AddsNothingOnly).await {
                Ok(Checked::Refused { question, yes }) => {
                    tracing::info!(
                        target,
                        question,
                        yes,
                        "the check dropped a proposed artifact"
                    );
                    batch.dropped = true;
                    continue;
                }
                // Kept and flagged: a retry is told the same, and its lines are not thrown away.
                Ok(Checked::Unjudged { detail }) => {
                    tracing::warn!(target, detail, "the check could not be put; kept, flagged");
                    unchecked = Some(detail);
                }
                Ok(Checked::Passed | Checked::TooLong) => {}
                Err(e) => {
                    tracing::warn!(target, error = %e, "the check failed; the rest stand on the literal checks");
                    asking = false;
                }
            }
        }
        batch.kept.push(c);
        batch.unchecked.push(unchecked);
    }
    batch
}

/// Whether this window is synthesized with the judged schema. Every case is kept with the capture
/// as `pre_read`: what was decided, why, and the answers where the questions were put.
pub(crate) async fn wants_judged(
    core: &Core,
    corpus_id: &str,
    text: &str,
    forced_intent: Option<&str>,
    single_window: bool,
) -> Result<bool> {
    let not_asked = |judged: bool, why: &str| serde_json::json!({"judged": judged, "why": why});
    if !single_window {
        keep_pre_read(core, corpus_id, not_asked(false, "several_windows")).await;
        return Ok(false);
    }
    if forced_intent.is_some() {
        keep_pre_read(core, corpus_id, not_asked(true, "forced_intent")).await;
        return Ok(true);
    }
    let asked = [asks_reminder(), has_date()];
    let refs: Vec<&Asked> = asked.iter().collect();
    // Not asked on cut text: the cut-off part may hold the reminder.
    let Some(shown) = state::window(text, &core.counter, state::room(core, &refs)) else {
        keep_pre_read(core, corpus_id, not_asked(true, "too_long")).await;
        return Ok(true);
    };
    // What the answers are answers about: this text, this model, these wordings.
    let of = {
        use sha2::{Digest, Sha256};
        serde_json::json!({
            "text": hex::encode(&Sha256::digest(text.as_bytes())[..8]),
            "model": core.decider.model(),
            "versions": [asked[0].version, asked[1].version],
        })
    };
    // A retry of the same window is not asked again; the kept answers meet the line as it is now.
    let recorded = core.store.get_corpus(corpus_id).await?.metadata;
    if let Some(yes) = recorded_pre_read(&recorded, &of) {
        return Ok(yes.iter().any(|y| *y >= PRE_READ_LINE));
    }
    let questions: Vec<_> = asked.iter().map(|a| a.question.clone()).collect();
    // The unit holds no permit here; the writer's turn is taken after this one is given back.
    let permit = core.gate.background_light().await;
    let got = core.decider.decide(&shown, &questions).await;
    permit.finished();
    let yes = got.and_then(|a| {
        asked
            .iter()
            .map(|q| match a.answers.get(&q.question.key) {
                Some(crate::infer::decide::Answer::Noul { yes }) => Ok(*yes),
                _ => Err(Error::InferenceRejected {
                    role: "decide",
                    detail: format!("the answer has no noul for \"{}\"", q.id),
                }),
            })
            .collect::<Result<Vec<f64>>>()
    });
    let yes = match yes {
        Ok(yes) => yes,
        // When in doubt, the judged schema: a missed reminder is worse than a wasted schema. Kept
        // as a failure, so a retry asks again.
        Err(e) => {
            tracing::warn!(corpus_id, error = %e, "the pre-read failed; asking for the judged schema");
            let failed = serde_json::json!({
                "judged": true, "why": "failed", "error": e.client_message(),
            });
            keep_pre_read(core, corpus_id, failed).await;
            return Ok(true);
        }
    };
    let judged = yes.iter().any(|y| *y >= PRE_READ_LINE);
    let answered = serde_json::json!({
        "judged": judged, "why": "asked",
        "asks_reminder": yes[0], "has_date": yes[1], "of": of,
    });
    keep_pre_read(core, corpus_id, answered).await;
    Ok(judged)
}

/// The two answers kept for exactly `of`, where the pre-read was put and answered.
fn recorded_pre_read(meta: &serde_json::Value, of: &serde_json::Value) -> Option<[f64; 2]> {
    let kept = meta.get("pre_read")?;
    if kept.get("why")?.as_str()? != "asked" || kept.get("of")? != of {
        return None;
    }
    Some([
        kept.get("asks_reminder")?.as_f64()?,
        kept.get("has_date")?.as_f64()?,
    ])
}

/// Read fresh, as `record_intent`: another writer of this column may have run since. Not being
/// able to keep it costs a repeat of the question, never the capture.
async fn keep_pre_read(core: &Core, corpus_id: &str, record: serde_json::Value) {
    let kept = async {
        let mut meta = core.store.get_corpus(corpus_id).await?.metadata;
        if !meta.is_object() {
            meta = serde_json::json!({});
        }
        // Every window of a long capture records the same; written once.
        if meta.get("pre_read") == Some(&record) {
            return Ok(());
        }
        meta["pre_read"] = record;
        core.store.set_corpus_metadata(corpus_id, &meta).await
    };
    if let Err(e) = kept.await {
        tracing::warn!(corpus_id, error = %e, "the pre-read could not be kept");
    }
}

/// Only a promotion needs this, since it hides existing text on the strength of new text. Returns
/// what is actually there: an oversize artifact is replaced by siblings.
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

/// Settles around a window that spent its attempts (reporting `partial`); every step of `finish` is
/// idempotent because a later success runs this again.
pub(crate) async fn settle(core: &Core, corpus_id: &str) -> Result<()> {
    // Held across read-then-finish; taken here, not at call sites, so no caller can forget it.
    let _corpus = core.corpus_lock(corpus_id).await;
    for w in core.store.segments_for_corpus(corpus_id).await? {
        let resolved = match w.state {
            SegmentState::Done | SegmentState::Verbatim => true,
            SegmentState::Failed => match attempts_for(core, corpus_id, w.idx).await {
                Ok(n) => n >= MAX_ATTEMPTS,
                // A failed query says nothing; `SQLITE_BUSY` is routine with two workers. Defer to
                // the next settle.
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

/// A missing row counts as spent, since nothing will try the window again; a failed query is an
/// error.
async fn attempts_for(core: &Core, corpus_id: &str, idx: i64) -> Result<i64> {
    // `subject` as well: the queue is instance-wide and every query on it is tenant-filtered.
    Ok(sqlx::query_scalar!(
        "SELECT attempts FROM jobs
          WHERE subject = ? AND stage = 'segment_window' AND target_id = ?",
        &core.store.subject,
        unit_target(corpus_id, idx)
    )
    .fetch_optional(&core.store.control.pool)
    .await?
    .unwrap_or(MAX_ATTEMPTS))
}

/// How many neighbours the reserved budget buys, and what each one gets. The floor gives way to the
/// count, never the budget: `per * slots <= budget` at every budget.
fn neighbour_shares(budget: usize) -> Option<(usize, usize)> {
    let per = (budget / 5).max(64);
    match budget / per {
        0 => None,
        slots => Some((per, slots.min(5))),
    }
}

/// `neighbours` has no floor, so distant hits would be pasted in and then dropped by
/// `from_context_only`. `None` is the store having no opinion, not a low value.
pub(crate) fn worth_showing(similarity: Option<f32>, weak_below: f32) -> bool {
    !similarity.is_some_and(|s| s < weak_below)
}

async fn neighbor_context(core: &Core, corpus_id: &str, idx: i64) -> Vec<crate::infer::Neighbor> {
    let Some(synth) = &core.synthesizer else {
        return Vec::new();
    };
    let budget = synth.budget().context.neighbors;
    if budget == 0 {
        return Vec::new();
    }
    // Before the lookup, so a budget that affords nothing costs no search.
    let Some((per, slots)) = neighbour_shares(budget) else {
        return Vec::new();
    };
    let Ok(rows) = core.store.artifacts_for_segment(corpus_id, idx).await else {
        return Vec::new();
    };
    let Some(seed) = rows
        .iter()
        .find(|c| c.provenance == crate::store::artifacts::Provenance::Passage)
    else {
        return Vec::new();
    };
    // The window unit and the corpus embed race at capture; embedding here is idempotent.
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
    let mut out = Vec::new();
    let weak_below = core.weak_below();
    for h in hits {
        if h.payload.corpus_id == corpus_id {
            continue;
        }
        if !worth_showing(h.similarity, weak_below) {
            continue;
        }
        // Measured with the budget's own counter, and the share pays for the `[id: <uuid>]` header
        // and title too; a share the header alone exhausts skips that neighbour.
        let overhead = crate::infer::context::NEIGHBOR_HEADER_TOKENS
            + h.payload
                .title
                .as_deref()
                .map(|t| core.counter.count(t))
                .unwrap_or(0);
        let Some(room) = per.checked_sub(overhead).filter(|r| *r >= 16) else {
            continue;
        };
        let text = core
            .counter
            .cut(&h.payload.text, room, true)
            .unwrap_or_default();
        out.push(crate::infer::Neighbor {
            id: h.payload.artifact_id,
            title: h.payload.title,
            text,
        });
        if out.len() == slots {
            break;
        }
    }
    out
}

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

/// The model's `corpus_lines` is only a hint for when `locate_span` finds nothing; `body` is the
/// window's text, so hinted line k is `start_line + k - 1`. `promote` reads `CorpusSpan::source`.
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
    // A hint outside the window places nothing rather than clamping to an edge.
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

/// `write_window` for a checked batch whose passages go with a replacing write.
#[cfg(test)]
pub(crate) async fn write_segment_artifacts(
    core: &Core,
    corpus_id: &str,
    segment_idx: i64,
    new: Vec<NewArtifact>,
) -> Result<Vec<crate::store::artifacts::Chunk>> {
    Ok(
        match write_window(core, corpus_id, segment_idx, new, true, false).await? {
            Written::New(rows) | Written::Already(rows) => rows,
            Written::Unchecked => Vec::new(),
        },
    )
}

/// What a window's write came to.
enum Written {
    /// Inserted now, in the order proposed.
    New(Vec<crate::store::artifacts::Chunk>),
    /// A promotion re-run: the rows an earlier run wrote, handed back.
    Already(Vec<crate::store::artifacts::Chunk>),
    /// Nothing: the batch was not checked, and no earlier rows stand in for it any more.
    Unchecked,
}

/// Replaces the window's chunks, or appends under `keep_artifacts`. A batch that skipped its check
/// on the strength of `written_already` is asked again under the lock the write holds, so nothing
/// parts the two. `passages_stay`: a replacing write leaves the passages and frees the hidden ones.
async fn write_window(
    core: &Core,
    corpus_id: &str,
    segment_idx: i64,
    new: Vec<NewArtifact>,
    checked: bool,
    passages_stay: bool,
) -> Result<Written> {
    let _corpus = core.corpus_lock(corpus_id).await;
    if let Some(have) = written_already(core, corpus_id, segment_idx).await? {
        tracing::info!(
            corpus_id,
            window = segment_idx,
            "window already written under keep_artifacts; not writing again"
        );
        return Ok(Written::Already(have));
    }
    if !checked {
        return Ok(Written::Unchecked);
    }
    // The mark or a verbatim state, as `run` asks it.
    let keep = core
        .store
        .segment_write_appends(corpus_id, segment_idx)
        .await?;
    if !keep {
        let mut old = core
            .store
            .artifact_ids_for_segment(corpus_id, segment_idx)
            .await?;
        if passages_stay {
            let passages: Vec<String> = core
                .store
                .artifacts_by_ids(&old)
                .await?
                .into_iter()
                .filter(|c| c.provenance == crate::store::artifacts::Provenance::Passage)
                .map(|c| c.id)
                .collect();
            old.retain(|id| !passages.contains(id));
        }
        if !old.is_empty() {
            core.vectors.delete_artifacts(&old).await?;
            for id in &old {
                core.store.delete_artifact(id).await?;
            }
        }
        if passages_stay {
            // A passage one of the deleted artifacts was hiding is in results again.
            core.heal_dangling_supersessions().await?;
        }
    }
    core.store
        .insert_artifacts(corpus_id, &new)
        .await
        .map(Written::New)
}

/// A promotion re-run under append: the earlier non-passage rows, which the write returns instead
/// of inserting any.
async fn written_already(
    core: &Core,
    corpus_id: &str,
    segment_idx: i64,
) -> Result<Option<Vec<crate::store::artifacts::Chunk>>> {
    if !core
        .store
        .segment_write_appends(corpus_id, segment_idx)
        .await?
    {
        return Ok(None);
    }
    let rows = core
        .store
        .artifacts_for_segment(corpus_id, segment_idx)
        .await?;
    let is_promotion = rows
        .iter()
        .any(|c| c.provenance == crate::store::artifacts::Provenance::Passage);
    let have: Vec<_> = rows
        .into_iter()
        .filter(|c| c.provenance != crate::store::artifacts::Provenance::Passage && c.in_results())
        .collect();
    Ok((is_promotion && !have.is_empty()).then_some(have))
}

/// One per thirty tokens, at least one: the floor under the prompt, because a prompt is a request.
pub(crate) fn artifact_allowance(input_tokens: usize) -> usize {
    (input_tokens / 30).max(1)
}

/// Located artifacts are evidence and always kept; the allowance only bounds the rewrites behind
/// them.
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

/// Located in context only means drop; located nowhere means a hard rewrite, kept. `neighbors` are
/// checked too: they live on `JudgeAsk`, not `WindowContext`, so `blocks()` never sees them.
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

/// The body only: this gates a second call over the whole window, and caveats are free prose.
pub(crate) fn paraphrased(chunks: &[crate::infer::ProposedArtifact], window: &str) -> bool {
    chunks
        .iter()
        .any(|c| !crate::infer::verify::missing_literals(&c.text, &[], window).is_empty())
}

/// The chunk is kept: a visible warning beats a chapter missing from the base. `unchecked` lies
/// beside `written`: why a row's check could not be put, where one could not.
pub(crate) async fn flag_unverified(
    core: &Core,
    written: &[crate::store::artifacts::Chunk],
    segment_body: &str,
    unchecked: &[Option<String>],
) -> Result<()> {
    use crate::core::judging::check::{FLAG_UNCHECKED, unchecked_detail};
    use crate::infer::verify;

    for (i, c) in written.iter().enumerate() {
        let mut flags = Vec::new();
        let mut detail: Vec<String> = Vec::new();

        let missing = verify::missing_literals(&c.text, &c.caveats, segment_body);
        if let Some(first) = missing.first() {
            flags.push(verify::FLAG_LITERALS.to_string());
            detail.push(format!("missing literal: {first}"));
            tracing::warn!(artifact_id = %c.id, literal = %first, "literal not found in source window");
        }
        if let Some(Some(why)) = unchecked.get(i) {
            flags.push(FLAG_UNCHECKED.to_string());
            detail.push(unchecked_detail(why));
        }

        if !flags.is_empty() {
            core.store
                .set_artifact_flags(&c.id, &flags, Some(&detail.join("; ")))
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
    // Positional: `spans` maps the chunks after the context drop, and nothing may filter between.
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
            // A written artifact has no category until it is read (`Stage::Read`).
            category: None,
            tags: p.tags,
            caveats: p.caveats,
            segment_idx: Some(segment_idx),
        })
        .collect()
}

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn a_leftover_window_unit_on_the_embed_only_floor_settles_verbatim() {
        let core = crate::core::test_support::test_core_without_synthesizer().await;
        let out = core.ingest("One short note.", "web", None).await.unwrap();
        assert!(crate::jobs::run_one(&core).await.unwrap());
        core.store.reset_segment(&out.id, 0, true).await.unwrap();
        assert_ne!(
            core.store.segment_state(&out.id, 0).await.unwrap(),
            Some(crate::store::segments::SegmentState::Verbatim),
            "the fixture has to start from a window something would read"
        );

        super::run(&core, &super::unit_target(&out.id, 0))
            .await
            .unwrap();
        assert_eq!(
            core.store.segment_state(&out.id, 0).await.unwrap(),
            Some(crate::store::segments::SegmentState::Verbatim)
        );
    }

    #[test]
    fn a_neighbour_below_the_line_is_not_put_in_front_of_the_model() {
        assert!(super::worth_showing(Some(0.71), 0.6));
        assert!(
            super::worth_showing(Some(0.6), 0.6),
            "the line is inclusive"
        );
        assert!(!super::worth_showing(Some(0.59), 0.6));
        // No opinion is not a low score: an exact lexical match has nothing set.
        assert!(super::worth_showing(None, 0.6));
        assert!(super::worth_showing(Some(0.0), 0.0));
    }

    #[test]
    fn the_neighbour_shares_never_add_up_to_more_than_was_reserved() {
        for budget in 0..2_000usize {
            if let Some((per, slots)) = super::neighbour_shares(budget) {
                assert!(
                    per * slots <= budget,
                    "budget {budget}: {slots} × {per} is over the reservation"
                );
                assert!((1..=5).contains(&slots), "budget {budget}: {slots} slots");
                assert!(per >= 64, "budget {budget}: {per} is not worth reading");
            }
        }
        assert_eq!(super::neighbour_shares(0), None);
        assert_eq!(super::neighbour_shares(32), None, "no share worth a fence");
        assert_eq!(super::neighbour_shares(64), Some((64, 1)));
        assert_eq!(super::neighbour_shares(200), Some((64, 3)));
        assert_eq!(super::neighbour_shares(1_024), Some((204, 5)));
    }

    #[tokio::test]
    async fn a_windows_attempts_are_read_from_this_tenants_row_only() {
        let core = crate::core::test_support::test_core().await;
        let src = core
            .store
            .insert_corpus("a document", "web", None)
            .await
            .unwrap();
        let target = unit_target(&src.id, 0);

        // The other tenant's row first, so an unfiltered `fetch_optional` finds it before ours.
        core.store
            .control
            .provision("someone-else", None)
            .await
            .unwrap();
        sqlx::query!(
            "INSERT INTO jobs (subject, stage, target_kind, target_id, state, attempts, run_after, created_at, seq, class)
             VALUES ('someone-else', 'segment_window', 'segment', ?, 'pending', 4, 0, 0, 0, 0)",
            &target
        )
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
        sqlx::query!(
            "UPDATE jobs SET attempts = 2 WHERE subject = ? AND target_id = ?",
            &core.store.subject,
            &target
        )
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
    async fn a_written_artifact_has_no_category_until_it_is_read() {
        let core = test_core().await;
        let out = core
            .ingest("The proxy listens on port 8080.", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        run(&core, &unit_target(&out.id, 0)).await.unwrap();
        let written =
            |rows: Vec<crate::store::artifacts::Chunk>| -> Vec<crate::store::artifacts::Chunk> {
                rows.into_iter()
                    .filter(|c| c.provenance != crate::store::artifacts::Provenance::Passage)
                    .collect()
            };
        let rows = written(core.store.artifacts_for_corpus(&out.id).await.unwrap());
        assert!(!rows.is_empty(), "the window wrote an artifact");
        for c in &rows {
            assert_eq!(c.category, None, "the writer gave {} a category", c.id);
        }

        crate::jobs::test_support::drain(&core).await;
        for c in written(core.store.artifacts_for_corpus(&out.id).await.unwrap()) {
            if c.in_results() {
                assert!(c.category.is_some(), "{} was never read", c.id);
                let by =
                    sqlx::query_scalar!("SELECT category_by FROM artifacts WHERE id = ?", c.id)
                        .fetch_one(&core.store.pool)
                        .await
                        .unwrap();
                assert_eq!(by.as_deref(), Some("reading"));
            }
        }
    }

    /// The check refuses any proposed artifact holding `marker`; every other request is answered 0.
    fn refusing(marker: &'static str) -> std::sync::Arc<crate::infer::fake::RuleDecider> {
        use crate::core::judging::check::test_support::answering;
        std::sync::Arc::new(crate::infer::fake::RuleDecider::new(move |state, qs| {
            let (_, rewrite) = state.rsplit_once("Rewrite: ").unwrap_or_default();
            let a = if rewrite.contains(marker) { 0.04 } else { 0.9 };
            Ok(
                answering(qs, 0.9, a).unwrap_or_else(|| crate::infer::decide::Answers {
                    answers: qs
                        .iter()
                        .map(|q| {
                            (
                                q.key.clone(),
                                crate::infer::decide::Answer::Noul { yes: 0.0 },
                            )
                        })
                        .collect(),
                    model: "rule-decider".into(),
                    input_tokens: 0,
                }),
            )
        }))
    }

    async fn written_and_passages(
        core: &Core,
        corpus_id: &str,
    ) -> (
        Vec<crate::store::artifacts::Chunk>,
        Vec<crate::store::artifacts::Chunk>,
    ) {
        core.store
            .artifacts_for_segment(corpus_id, 0)
            .await
            .unwrap()
            .into_iter()
            .filter(|c| c.in_results())
            .partition(|c| c.provenance != crate::store::artifacts::Provenance::Passage)
    }

    #[tokio::test]
    async fn a_synthesized_artifact_that_adds_a_fact_is_dropped_and_the_passages_remain() {
        let mut core = test_core().await;
        let dec = refusing("bravo");
        core.decider = dec.clone();
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();

        run(&core, &unit_target(&out.id, 0)).await.unwrap();

        let checks: Vec<_> = dec
            .requests()
            .into_iter()
            .filter(|(_, qs)| crate::core::judging::check::test_support::is_check(qs))
            .collect();
        assert_eq!(checks.len(), 2, "one check per proposed artifact");
        for (_, qs) in &checks {
            let keys: Vec<&str> = qs.iter().map(|q| q.key.as_str()).collect();
            assert_eq!(keys, ["adds_nothing"]);
        }
        let (written, passages) = written_and_passages(&core, &out.id).await;
        let texts: Vec<&str> = written.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["alpha line"], "one written");
        assert!(
            passages.iter().any(|c| c.text.contains("bravo line")),
            "the window's passage went with the dropped artifact"
        );
        assert_eq!(
            core.store.segment_state(&out.id, 0).await.unwrap(),
            Some(SegmentState::Done)
        );
    }

    async fn write_rows(core: &Core) -> Vec<String> {
        sqlx::query_scalar!(
            "SELECT subject_a FROM decisions WHERE subject_kind = 'write' ORDER BY subject_a"
        )
        .fetch_all(&core.store.pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn each_check_row_names_the_artifact_it_judged() {
        let mut core = test_core().await;
        core.decider = refusing("bravo");
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        run(&core, &unit_target(&out.id, 0)).await.unwrap();
        let t = unit_target(&out.id, 0);
        assert_eq!(
            write_rows(&core).await,
            [format!("{t}#0"), format!("{t}#1")]
        );
    }

    #[tokio::test]
    async fn a_rerun_of_a_window_already_written_is_not_checked_again() {
        let mut core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        run(&core, &unit_target(&out.id, 0)).await.unwrap();
        let before = write_rows(&core).await;
        assert!(!before.is_empty(), "the first run was checked");
        let dec = refusing("alpha");
        core.decider = dec.clone();

        core.store.reset_segment(&out.id, 0, true).await.unwrap();
        run(&core, &unit_target(&out.id, 0)).await.unwrap();

        let checks = dec
            .requests()
            .into_iter()
            .filter(|(_, qs)| crate::core::judging::check::test_support::is_check(qs))
            .count();
        assert_eq!(
            checks, 0,
            "the re-run paid for checks of what it will not write"
        );
        assert_eq!(write_rows(&core).await, before, "and logged rows for them");
    }

    #[tokio::test]
    async fn a_check_that_errs_never_holds_the_capture() {
        let mut core = test_core().await;
        core.decider = std::sync::Arc::new(crate::infer::fake::RuleDecider::new(|_, _| {
            Err(Error::Inference {
                role: "decide",
                detail: "down".into(),
            })
        }));
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();

        run(&core, &unit_target(&out.id, 0)).await.unwrap();

        let (written, _) = written_and_passages(&core, &out.id).await;
        assert_eq!(written.len(), 2, "both stand on the literal checks alone");
        assert_eq!(
            core.store.segment_state(&out.id, 0).await.unwrap(),
            Some(SegmentState::Done)
        );
    }

    /// The endpoint rejects the check of any proposed artifact holding `marker`; the rest pass.
    fn rejecting(marker: &'static str) -> std::sync::Arc<crate::infer::fake::RuleDecider> {
        use crate::core::judging::check::test_support::{answering, is_check};
        std::sync::Arc::new(crate::infer::fake::RuleDecider::new(move |state, qs| {
            let (_, rewrite) = state.rsplit_once("Rewrite: ").unwrap_or_default();
            if is_check(qs) && rewrite.contains(marker) {
                return Err(Error::InferenceRejected {
                    role: "decide",
                    detail: "HTTP 400".into(),
                });
            }
            Ok(
                answering(qs, 0.9, 0.9).unwrap_or_else(|| crate::infer::decide::Answers {
                    answers: qs
                        .iter()
                        .map(|q| {
                            (
                                q.key.clone(),
                                crate::infer::decide::Answer::Noul { yes: 0.0 },
                            )
                        })
                        .collect(),
                    model: "rule-decider".into(),
                    input_tokens: 0,
                }),
            )
        }))
    }

    #[tokio::test]
    async fn an_artifact_whose_check_is_rejected_is_kept_flagged_and_the_unit_settles() {
        let mut core = test_core().await;
        let dec = rejecting("bravo");
        core.decider = dec.clone();
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();

        run(&core, &unit_target(&out.id, 0))
            .await
            .expect("a rejection is not the queue's to retry");

        let checks = dec
            .requests()
            .into_iter()
            .filter(|(_, qs)| crate::core::judging::check::test_support::is_check(qs))
            .count();
        assert_eq!(checks, 2, "each asked once");
        let (written, _) = written_and_passages(&core, &out.id).await;
        let alpha = written.iter().find(|c| c.text == "alpha line").unwrap();
        assert!(alpha.flags.is_empty(), "{:?}", alpha.flags);
        let bravo = written
            .iter()
            .find(|c| c.text == "bravo line")
            .expect("the unjudged artifact was dropped");
        assert_eq!(
            bravo.flags,
            [crate::core::judging::check::FLAG_UNCHECKED.to_string()]
        );
        assert_eq!(bravo.flag_detail.as_deref(), Some("not checked: HTTP 400"));
        assert_eq!(
            core.store.segment_state(&out.id, 0).await.unwrap(),
            Some(SegmentState::Done)
        );
    }

    #[tokio::test]
    async fn a_replacing_rerun_that_drops_an_artifact_keeps_the_windows_passages() {
        let mut core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        run(&core, &unit_target(&out.id, 0)).await.unwrap();
        let (first, _) = written_and_passages(&core, &out.id).await;
        assert!(
            first.iter().any(|c| c.text == "bravo line"),
            "the fixture's first read wrote it"
        );

        // The mark is spent, so this re-run replaces; its check drops one.
        core.decider = refusing("bravo");
        core.store.reset_segment(&out.id, 0, false).await.unwrap();
        run(&core, &unit_target(&out.id, 0)).await.unwrap();

        let (written, passages) = written_and_passages(&core, &out.id).await;
        let texts: Vec<&str> = written.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["alpha line"]);
        assert!(
            written.iter().all(|c| first.iter().all(|f| f.id != c.id)),
            "the first read's artifacts were not replaced"
        );
        assert!(
            passages.iter().any(|c| c.text.contains("bravo line")),
            "the dropped artifact's lines are in no row a search returns"
        );
    }

    #[tokio::test]
    async fn an_unchecked_batch_is_not_written_once_the_rows_it_stood_on_are_gone() {
        let core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        run(&core, &unit_target(&out.id, 0)).await.unwrap();
        core.store.reset_segment(&out.id, 0, true).await.unwrap();
        assert!(
            written_already(&core, &out.id, 0).await.unwrap().is_some(),
            "the fixture is a re-run that would skip its check"
        );
        // After the probe and before the write, every written artifact goes to Trash.
        let (written, _) = written_and_passages(&core, &out.id).await;
        for c in &written {
            core.trash_artifact(&c.id).await.unwrap();
        }

        let new = vec![NewArtifact {
            text: "never checked".into(),
            segment_idx: Some(0),
            ..Default::default()
        }];
        let got = write_window(&core, &out.id, 0, new.clone(), false, false)
            .await
            .unwrap();
        assert!(matches!(got, Written::Unchecked));
        let rows = core.store.artifacts_for_segment(&out.id, 0).await.unwrap();
        assert!(
            rows.iter().all(|c| c.text != "never checked"),
            "an unchecked batch was written"
        );

        let got = write_window(&core, &out.id, 0, new, true, false)
            .await
            .unwrap();
        assert!(matches!(got, Written::New(rows) if rows.len() == 1));
    }

    #[tokio::test]
    async fn a_rerun_replaces_a_window_unless_it_was_sent_back_for_missed_lines() {
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
        // `SQLITE_BUSY` after the write is routine; the retry must still append.
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
        let core = test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        // Capture arms the window with `keep`, so this run is already promotion-shaped.
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
        core.synthesizer = Some(std::sync::Arc::new(
            crate::infer::fake::FakeSynthesizer::unparsable_on("alpha"),
        ));

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
        assert_eq!(
            resolve_span("unlocatable", "a\nb", &w, Some((2, 3))),
            CorpusSpan::claimed(51, 52)
        );
    }

    #[test]
    fn the_artifacts_own_text_beats_the_hint() {
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

        assert!(!from_context_only(
            "the window says something quite specific here",
            core_text,
            &ctx,
            &[]
        ));
        assert!(from_context_only(
            "the following window describes another procedure",
            core_text,
            &ctx,
            &[]
        ));
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
            tags: vec![],
            corpus_lines: None,
            caveats: vec![],
        };
        let chunks = vec![
            art("The note references a specific future event on Friday at 13:45."),
            art("erinnere mich an den Gastroentereologentermin, Freitag 13:45 uhr."),
            art("The reminder is set for Friday, 2026-09-05 at 13:45."),
        ];
        let kept = within_allowance(chunks, window, 1);
        assert_eq!(kept.len(), 1);
        assert!(kept[0].text.starts_with("erinnere mich"));

        let chunks = vec![
            art("The reminder is set for Friday, 2026-09-05 at 13:45."),
            art("erinnere mich an den Gastroentereologentermin,"),
            art("Freitag 13:45 uhr."),
        ];
        let kept = within_allowance(chunks, window, 1);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|c| !c.text.starts_with("The reminder")));

        let chunks = vec![art("b"), art("a")];
        let kept = within_allowance(chunks, window, 5);
        assert_eq!(
            kept.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            vec!["b", "a"]
        );
    }

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
        // Material in both places belongs to the window.
        assert!(!from_context_only(
            "the window says something quite specific here",
            core_text,
            &ctx,
            &neighbors
        ));
    }

    #[tokio::test]
    async fn settle_treats_a_verbatim_segment_as_resolved() {
        // The finish is what arms the embed for the promoted artifacts.
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
                    text: "l1 l2".into(),
                    segment_idx: Some(0),
                    ..Default::default()
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

    mod pre_read {
        use super::super::*;
        use crate::core::ingest::Capture;
        use crate::core::moments::Intent;
        use crate::core::test_support::test_core;
        use crate::infer::decide::{Answer, Answers, Question};
        use crate::infer::fake::{FAKE_BUDGET, RecordingSynthesizer, RuleDecider};
        use std::collections::HashMap;
        use std::sync::Arc;

        /// `asks_reminder` says `remind`, `has_date` says `date`; anything else a flat noul.
        fn pre_reader(remind: f64, date: f64) -> Arc<RuleDecider> {
            Arc::new(RuleDecider::new(move |_, qs: &[Question]| {
                let answers: HashMap<_, _> = qs
                    .iter()
                    .map(|q| {
                        let yes = match q.key.as_str() {
                            "asks_reminder" => remind,
                            "has_date" => date,
                            _ => 0.5,
                        };
                        (q.key.clone(), Answer::Noul { yes })
                    })
                    .collect();
                Ok(Answers {
                    answers,
                    model: "rule-decider".into(),
                    input_tokens: 0,
                })
            }))
        }

        fn pre_reads(d: &RuleDecider) -> Vec<(String, Vec<Question>)> {
            d.requests()
                .into_iter()
                .filter(|(_, qs)| qs.iter().any(|q| q.key == "asks_reminder"))
                .collect()
        }

        /// Runs the capture's first window; returns the corpus and whether each writer call was
        /// asked for the judged schema.
        async fn synthesized(
            core: &mut Core,
            text: &str,
            intent: Option<Intent>,
        ) -> (String, Vec<bool>) {
            let rec = Arc::new(RecordingSynthesizer::new(FAKE_BUDGET));
            core.synthesizer = Some(rec.clone());
            let out = core
                .ingest_capture(Capture::new(text, "web").with_intent(intent))
                .await
                .unwrap();
            crate::jobs::synthesize::plan(core, &out.id).await.unwrap();
            run(core, &unit_target(&out.id, 0)).await.unwrap();
            (out.id, rec.judged())
        }

        #[tokio::test]
        async fn a_note_that_asks_to_be_reminded_is_synthesized_with_the_judged_schema() {
            let mut core = test_core().await;
            let dec = pre_reader(0.97, 0.0);
            core.decider = dec.clone();
            let (_, judged) = synthesized(&mut core, "Remind me to call the dentist.", None).await;
            assert_eq!(judged, [true]);
            let asked = pre_reads(&dec);
            assert_eq!(asked.len(), 1, "one request");
            let keys: Vec<&str> = asked[0].1.iter().map(|q| q.key.as_str()).collect();
            assert_eq!(keys, ["asks_reminder", "has_date"]);
            assert_eq!(asked[0].0, "Remind me to call the dentist.");
        }

        #[tokio::test]
        async fn a_note_with_a_deadline_is_too() {
            let mut core = test_core().await;
            core.decider = pre_reader(0.0, 0.31);
            let (_, judged) =
                synthesized(&mut core, "The report is due on the 3rd of March.", None).await;
            assert_eq!(judged, [true]);
        }

        #[tokio::test]
        async fn the_line_itself_is_judged() {
            let mut core = test_core().await;
            // The number, not the constant: a moved line fails here or at 0.29 below.
            core.decider = pre_reader(0.3, 0.0);
            let (_, judged) = synthesized(&mut core, "Maybe ping Anna.", None).await;
            assert_eq!(judged, [true]);
        }

        async fn kept(core: &Core, id: &str) -> serde_json::Value {
            core.store.get_corpus(id).await.unwrap().metadata["pre_read"].clone()
        }

        #[test]
        fn kept_answers_stand_only_for_what_they_were_asked_about() {
            use serde_json::json;
            let of = json!({"text": "aa", "model": "m", "versions": [1, 1]});
            let asked = json!({"pre_read": {
                "judged": true, "why": "asked", "asks_reminder": 0.9, "has_date": 0.1, "of": of,
            }});
            assert_eq!(recorded_pre_read(&asked, &of), Some([0.9, 0.1]));
            let edited = json!({"text": "bb", "model": "m", "versions": [1, 1]});
            assert_eq!(recorded_pre_read(&asked, &edited), None);
            let reworded = json!({"text": "aa", "model": "m", "versions": [1, 2]});
            assert_eq!(recorded_pre_read(&asked, &reworded), None);
            let failed = json!({"pre_read": {"judged": true, "why": "failed"}});
            assert_eq!(recorded_pre_read(&failed, &of), None);
            assert_eq!(recorded_pre_read(&json!({}), &of), None);
        }

        #[tokio::test]
        async fn a_retried_window_reuses_the_pre_read_it_kept() {
            let mut core = test_core().await;
            let dec = pre_reader(0.29, 0.29);
            core.decider = dec.clone();
            let (id, judged) =
                synthesized(&mut core, "The proxy listens on port 8080.", None).await;
            assert_eq!(judged, [false]);

            let again = Arc::new(RecordingSynthesizer::new(FAKE_BUDGET));
            core.synthesizer = Some(again.clone());
            core.store.reset_segment(&id, 0, true).await.unwrap();
            run(&core, &unit_target(&id, 0)).await.unwrap();
            assert_eq!(pre_reads(&dec).len(), 1, "the retry asked again");
            assert_eq!(
                again.judged(),
                [false],
                "the kept answers did not choose the schema"
            );
        }

        #[tokio::test]
        async fn a_pre_read_that_failed_is_asked_again_on_the_retry() {
            let mut core = test_core().await;
            let dec = Arc::new(RuleDecider::new(|_, _| {
                Err(crate::error::Error::Inference {
                    role: "decide",
                    detail: "down".into(),
                })
            }));
            core.decider = dec.clone();
            let (id, _) = synthesized(&mut core, "The proxy listens on port 8080.", None).await;
            let failed = kept(&core, &id).await;
            assert_eq!(failed["why"], "failed");
            assert_eq!(failed["judged"], true);
            assert!(failed["error"].as_str().is_some_and(|e| e.contains("down")));

            core.store.reset_segment(&id, 0, true).await.unwrap();
            run(&core, &unit_target(&id, 0)).await.unwrap();
            assert_eq!(pre_reads(&dec).len(), 2, "a failure stood in for an answer");
        }

        #[tokio::test]
        async fn a_note_with_neither_gets_the_plain_schema() {
            let mut core = test_core().await;
            let dec = pre_reader(0.29, 0.29);
            core.decider = dec.clone();
            let (_, judged) = synthesized(&mut core, "The proxy listens on port 8080.", None).await;
            assert_eq!(judged, [false]);
            assert_eq!(pre_reads(&dec).len(), 1);
        }

        #[tokio::test]
        async fn a_forced_intent_needs_no_question() {
            let mut core = test_core().await;
            let dec = pre_reader(0.0, 0.0);
            core.decider = dec.clone();
            let (id, judged) = synthesized(
                &mut core,
                "The proxy listens on port 8080.",
                Some(Intent::Remind),
            )
            .await;
            assert_eq!(judged, [true]);
            assert_eq!(pre_reads(&dec).len(), 0);
            assert_eq!(
                kept(&core, &id).await,
                serde_json::json!({"judged": true, "why": "forced_intent"}),
                "not asked, and nothing says why"
            );
        }

        #[tokio::test]
        async fn a_window_too_long_to_ask_about_is_judged() {
            let mut core = test_core().await;
            let dec = pre_reader(0.0, 0.0);
            dec.set_context_tokens(200);
            core.decider = dec.clone();
            let text: String = (0..300).map(|i| format!("word{i} ")).collect();
            let (id, judged) = synthesized(&mut core, text.trim(), None).await;
            assert_eq!(
                core.store.segments_for_corpus(&id).await.unwrap().len(),
                1,
                "the fixture is one window"
            );
            assert_eq!(judged, [true]);
            assert_eq!(dec.calls(), 0);
            assert_eq!(
                kept(&core, &id).await,
                serde_json::json!({"judged": true, "why": "too_long"})
            );
        }

        #[tokio::test]
        async fn a_pre_read_that_fails_falls_to_the_judged_schema() {
            let mut core = test_core().await;
            let dec = Arc::new(RuleDecider::new(|_, _| {
                Err(crate::error::Error::Inference {
                    role: "decide",
                    detail: "down".into(),
                })
            }));
            core.decider = dec.clone();
            let (id, judged) =
                synthesized(&mut core, "The proxy listens on port 8080.", None).await;
            assert_eq!(judged, [true]);
            assert_eq!(pre_reads(&dec).len(), 1);
            assert_eq!(
                core.store.segment_state(&id, 0).await.unwrap(),
                Some(SegmentState::Done),
                "the capture goes on"
            );
        }

        #[tokio::test]
        async fn a_capture_of_several_windows_is_never_judged() {
            let mut core = test_core().await;
            let dec = pre_reader(0.97, 0.97);
            core.decider = dec.clone();
            let body = (0..400)
                .map(|i| format!("paragraph number {i} with some filler text"))
                .collect::<Vec<_>>()
                .join("\n\n");
            let (id, judged) = synthesized(&mut core, &body, None).await;
            assert!(core.store.segments_for_corpus(&id).await.unwrap().len() > 1);
            assert_eq!(judged, [false]);
            assert_eq!(pre_reads(&dec).len(), 0);
            assert_eq!(
                kept(&core, &id).await,
                serde_json::json!({"judged": false, "why": "several_windows"})
            );
        }

        #[tokio::test]
        async fn the_two_answers_are_kept_with_the_capture() {
            let mut core = test_core().await;
            core.decider = pre_reader(0.97, 0.25);
            let (id, _) = synthesized(&mut core, "Remind me to call the dentist.", None).await;
            let read = kept(&core, &id).await;
            assert_eq!(read["asks_reminder"], 0.97);
            assert_eq!(read["has_date"], 0.25);
            assert_eq!(read["why"], "asked");
            assert_eq!(read["judged"], true);
            assert_eq!(read["of"]["model"], "rule-decider");
        }
    }
}
