//! Promotion: synthesis armed by evidence instead of by capture.

use crate::core::Core;
use crate::error::Result;
use crate::store::artifacts::{Chunk, CorpusSpan, Provenance};
use crate::store::jobs::Stage;
use crate::store::segments::SegmentState;

/// Promote the windows of any of these passages that have earned it.
///
/// Called after an engagement bump — opened, or confirmed — and never after a
/// retrieval bump: a passage that merely keeps appearing in result lists has
/// helped nobody, and the condition "opened or confirmed at least once" is
/// *where* this is called, not a stored flag. Checked at the bump, not on a
/// sweep: a sweep reads decayed activation and the threshold would then mean
/// something different depending on when it ran.
///
/// Arms a job; calls no model. The job queue and `[pacing]` bound the load.
pub async fn maybe_promote(core: &Core, ids: &[String], at: i64) -> Result<usize> {
    let activation = core.store.activation_of(ids).await?;
    let mut armed = 0;
    for id in ids {
        let Some((value, stamp, created_at)) = activation.get(id) else {
            continue;
        };
        // Above the capture baseline, decayed to the same instant — never the
        // raw stored number. Both terms fall at the same rate, so a threshold
        // read against the raw sum means something different at every age: with
        // the baseline at `1.0` and a confirmation at `3.0`, a threshold of
        // `4.0` was reachable only by a confirmation at essentially zero
        // elapsed time, and one day after capture it already took two. What the
        // threshold is meant to name is what use added, so that is what it is
        // compared against.
        let earned = crate::store::links::engagement_at(
            *value,
            *stamp,
            *created_at,
            at,
            core.activation.half_life_days,
        );
        if earned < core.promote.activation_above {
            continue;
        }
        let Ok(c) = core.store.get_artifact(id).await else {
            continue;
        };
        if c.provenance != Provenance::Passage || !c.in_results() {
            continue;
        }
        let (Some(corpus_id), Some(idx)) = (c.corpus_id.as_deref(), c.segment_idx) else {
            continue;
        };
        // The guard against re-promotion is the segment state: a window that
        // is `done` — or already on its way — never promotes again, however
        // many of its surviving passages cross the line afterwards.
        if core.store.segment_state(corpus_id, idx).await? != Some(SegmentState::Verbatim) {
            continue;
        }
        // And the second guard, for the one window that is `verbatim` *because*
        // it was promoted: `undo_promotion` puts the state back so the window
        // can be read again, but leaves the passages the activation they
        // earned — which is the very number that armed the promotion. Without
        // this the next open of any of those passages re-promotes the window
        // and the operator's undo is undone, with the deprecated artifacts
        // still lying behind it.
        if core.store.segment_no_promote(corpus_id, idx).await? {
            continue;
        }
        // `keep_artifacts`: the window job appends rather than replaces, so the
        // passages survive to be superseded by what covers them.
        core.store.reset_segment(corpus_id, idx, true).await?;
        core.store
            .rearm_idle_seq(
                Stage::SegmentWindow,
                "segment",
                &crate::jobs::window::unit_target(corpus_id, idx),
                idx,
            )
            .await?;
        tracing::info!(
            artifact_id = %id,
            corpus_id,
            window = idx,
            activation = earned,
            "promoting a window"
        );
        armed += 1;
    }
    Ok(armed)
}

fn overlap(a: &CorpusSpan, b: &CorpusSpan) -> i64 {
    (a.end_line.min(b.end_line) - a.start_line.max(b.start_line) + 1).max(0)
}

/// Which artifact, if any, supersedes each passage: the one whose span covers
/// a **majority** of the passage's lines — per artifact, not cumulative,
/// because `supersede` names one winner and a passage hidden behind an
/// artifact holding a third of it sends the reader to the wrong text. Best
/// overlap wins. Ties on overlap go to a placed span, then to the lowest
/// ordinal. Everything else stays active, verbatim, in results: promotion can
/// only ever improve coverage.
pub fn covered_by<'a>(
    passages: &'a [(String, CorpusSpan)],
    artifacts: &'a [(String, i64, CorpusSpan)],
) -> Vec<(&'a str, &'a str)> {
    let mut out = Vec::new();
    for (pid, ps) in passages {
        let len = ps.end_line - ps.start_line + 1;
        // Best overlap; among equals a placed span before an unplaced one,
        // because an unplaced span is the whole-window fallback and says
        // nothing about *which* passage — and `supersede_covered` will not
        // read the vector for it. Then the lowest ordinal.
        let best = artifacts
            .iter()
            .map(|(aid, ord, asp)| {
                (
                    overlap(ps, asp),
                    asp.places_the_artifact(),
                    *ord,
                    aid.as_str(),
                )
            })
            .filter(|(ov, _, _, _)| 2 * ov > len)
            .max_by(|x, y| x.0.cmp(&y.0).then(x.1.cmp(&y.1)).then(y.2.cmp(&x.2)));
        if let Some((_, _, _, aid)) = best {
            out.push((pid.as_str(), aid));
        }
    }
    out
}

/// Whether an artifact is traceable to a passage by meaning, for the rewrites
/// that copied no line of it.
///
/// Read against `[promote] traceable_min`, and deliberately not against
/// `[consolidate] auto_supersede`: that threshold compares two independent
/// artifacts, where a cosine cannot tell "runs on ext4" from "does not run on
/// ext4" and a judge stands behind every hide. The relation here is known by
/// construction — this artifact is the output of synthesizing this window —
/// and the only open question is which passage of that one window it came
/// from. Separating passages of a single document is a far weaker claim than
/// asserting two texts agree.
///
/// Reached only for an artifact whose span placed it in the window — see the
/// comment in `supersede_covered`. Against the whole-window fallback this
/// would be measuring something else entirely.
///
/// Free: both points are already in the store — the passages were embedded at
/// capture, and `embed_written` embedded the artifacts before this was
/// called — so this is two reads and an arithmetic mean. No model call.
///
/// Every way of not knowing answers `false`. A missing point, a store that
/// cannot be reached, a threshold set above 1.0: each leaves the passage
/// standing in results, which is the direction promotion is allowed to fail
/// in. A vector outage must not hide verbatim text.
async fn corroborated_by_vector(core: &Core, artifact_id: &str, passage_id: &str) -> bool {
    let min = core.promote.traceable_min;
    if min > 1.0 {
        return false;
    }
    let dense = |id: String| async move {
        match core.vectors.dense_of(&id).await {
            Ok(Some(v)) => Some(v),
            // Not "no vector, no claim": the passage's point is written by the
            // corpus embed job, which sits at the same class, attempts and seq
            // as the window job that reaches here — only rowid separates them,
            // and with more than one worker they are claimed at once. The
            // ordering that made this work was incidental, and nothing re-runs
            // `supersede_covered` for a window already `done`, so a point that
            // was merely late left the passage standing beside the artifact
            // written from it for good. Embedded here, the way
            // `neighbor_context` already embeds its seed passage inline.
            Ok(None) => {
                if let Err(e) = crate::jobs::embed::run(core, &id).await {
                    tracing::warn!(
                        artifact_id = %id,
                        error = %e,
                        "could not embed while judging a promotion; leaving the passage in results"
                    );
                    return None;
                }
                core.vectors.dense_of(&id).await.ok().flatten()
            }
            Err(e) => {
                tracing::warn!(
                    artifact_id = %id,
                    error = %e,
                    "could not read a vector while judging a promotion; leaving the passage in results"
                );
                None
            }
        }
    };
    let (Some(a), Some(p)) = (
        dense(artifact_id.to_string()).await,
        dense(passage_id.to_string()).await,
    ) else {
        return false;
    };
    crate::vector::cosine(&a, &p) >= min
}

/// After a promoted window's artifacts are written: supersede the passages
/// they cover and carry the passages' access forward.
///
/// Activation first, links second, the supersede last — `supersede` refuses a
/// side that is no longer active, and everything before it needs the passage
/// readable. Returns how many passages were superseded.
pub async fn supersede_covered(
    core: &Core,
    corpus_id: &str,
    idx: i64,
    written: &[Chunk],
    at: i64,
) -> Result<usize> {
    let rows = core.store.artifacts_for_segment(corpus_id, idx).await?;
    let passages: Vec<(String, CorpusSpan)> = rows
        .iter()
        .filter(|c| c.provenance == Provenance::Passage && c.in_results())
        .filter_map(|c| Some((c.id.clone(), c.corpus_span.clone()?)))
        .collect();
    let artifacts: Vec<(String, i64, CorpusSpan)> = written
        .iter()
        .filter_map(|c| Some((c.id.clone(), c.ordinal, c.corpus_span.clone()?)))
        .collect();
    // A span is a claim the splitter can verify for a passage and only guess
    // for a rewrite: `resolve_span` falls back to the model's hint or the
    // whole window when nothing in the artifact locates verbatim, and a
    // heavily paraphrasing model then hands every artifact the same span. So
    // a majority by span is necessary, not sufficient: the artifact must also
    // be *traceable* to the passage. Whatever is not traceable leaves the
    // passage standing, which is the direction promotion is allowed to fail
    // in.
    //
    // Traceable two ways, and the second one is why this reads as a loop
    // rather than a filter. A line of the passage appearing verbatim in the
    // artifact is the cheap way and stays first: it costs nothing and it is
    // exact. But it is also the one thing synthesis is *for* not doing —
    // "unstructured notes come out structured", pronouns resolved, fragments
    // completed — so for a capture of prose no line ever matches, and the
    // verbatim passage stood in results beside the artifact written from it.
    // Two hits for one note, every time, on every capture that was not code.
    // The second way is the meaning: the two vectors are both already stored
    // by the time this runs, so corroboration is two reads and a cosine.
    //
    // And the meaning is only read where the span placed the artifact at all.
    // That is what `SpanSource::Unplaced` marks and the whole of what it is
    // for here. The two-part rule reads as one question asked twice — *which
    // passage of this window is this artifact?* — and the cosine answers it
    // only because the span already narrowed the window to a neighbourhood.
    // Take that away, which is exactly what the fallback does, and the
    // question the cosine is left answering is *are these two texts about the
    // same subject?* — to which two passages of one document, sharing their
    // topic and their vocabulary, answer yes at a similarity over any
    // threshold worth setting. Then every passage of the window is
    // majority-covered by the one sweeping artifact and every one of them is
    // hidden by a number that was never about them. So an unplaced span keeps
    // the verbatim rule and nothing else: a line of the passage has to appear
    // in the artifact.
    //
    // `Claimed` is thin, and it stays: the model naming lines 3–4 out of a
    // sixty-line window is a claim that can be wrong but is still *about*
    // those lines, and it is the ordinary outcome for the paraphrase this path
    // was built for — the rewrite that copies no line is precisely the one
    // `locate_span` cannot place. Refusing it would leave the cosine reachable
    // only where the verbatim rule had already answered.
    let text_of = |id: &str| rows.iter().find(|c| c.id == id).map(|c| c.text.as_str());
    let placed = |id: &str| {
        artifacts
            .iter()
            .find(|(aid, _, _)| aid == id)
            .is_some_and(|(_, _, span)| span.places_the_artifact())
    };
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    for (p, a) in covered_by(&passages, &artifacts) {
        let (Some(passage_text), Some(artifact_text)) = (
            text_of(p),
            written.iter().find(|c| c.id == a).map(|c| c.text.as_str()),
        ) else {
            continue;
        };
        if crate::infer::verify::locate_span(artifact_text, passage_text, 1).is_some()
            || (placed(a) && corroborated_by_vector(core, a, p).await)
        {
            pairs.push((p, a));
        }
    }
    if pairs.is_empty() {
        return Ok(0);
    }
    let half_life = core.activation.half_life_days;
    let link_half_life = core.associate.half_life_days;

    // Group by winner: one artifact may supersede several passages, and its
    // activation is the max over all of them.
    let mut by_winner: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for (p, a) in &pairs {
        by_winner.entry(a).or_default().push(p);
    }
    let all_superseded: std::collections::HashSet<&str> = pairs.iter().map(|(p, _)| *p).collect();
    let mut n = 0;
    for (winner, losers) in by_winner {
        let ids: Vec<String> = losers.iter().map(|s| s.to_string()).collect();
        // What moves is the engagement the passages *earned*, never the raw
        // stored sum. Activation is a capture baseline plus use, and the
        // baseline is anchored to each artifact's own `created_at`: the winner
        // was written moments ago, so its baseline stands at a full `1.0`,
        // while a passage captured six weeks back carries a baseline of almost
        // nothing under whatever it earned. Transferring the raw sum read one
        // against the other. A passage with 1.9 earned came to a raw ~2.0, and
        // the winner set to 2.0 reported 1.0 of use — half of it lost. Worse
        // below the line: 0.5 earned carried ~0.6, lost the `max` to the
        // winner's own 1.0, and the artifact that was supposed to inherit the
        // access came out at zero.
        let act = core.store.activation_of(&ids).await?;
        let carried = act
            .values()
            .map(|(v, s, c)| crate::store::links::engagement_at(*v, *s, *c, at, half_life))
            .fold(0.0f64, f64::max);
        let own = core
            .store
            .activation_of(std::slice::from_ref(&winner.to_string()))
            .await?
            .get(winner)
            .copied();
        if let Some((v, s, c)) = own {
            let now = crate::store::links::decayed(v, s, at, half_life);
            let earned = crate::store::links::engagement_at(v, s, c, at, half_life);
            // Only ever upwards, and the winner's own baseline is left under
            // it: what is written back is the same number with the larger of
            // the two engagements standing on it.
            if carried > earned {
                core.store
                    .set_activation(winner, now + (carried - earned), at)
                    .await?;
            }
        }
        for loser in &losers {
            for link in core.store.links_touching(loser).await? {
                let other = if link.a_id == *loser {
                    &link.b_id
                } else {
                    &link.a_id
                };
                // A link between two passages of this same promotion would
                // become the artifact linked to itself — or to a passage about
                // to go dark. Neither carries anything.
                if all_superseded.contains(other.as_str()) {
                    continue;
                }
                core.store
                    .carry_link(&link, loser, winner, link_half_life, at)
                    .await?;
            }
        }
        for loser in &losers {
            if crate::jobs::try_supersede(core, loser, winner, "a passage its promotion covers")
                .await
            {
                n += 1;
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;

    /// A core at `earned`, recording, with one verbatim corpus of one passage.
    async fn earned_with_one_passage() -> (crate::core::Core, String, String) {
        let mut core = test_core().await;
        core.learn.enabled = true;
        // Multi-window on purpose: a capture that fits one synthesis call is
        // synthesized at capture now, so the corpus promotion exists for is
        // one too large for that — verbatim windows, earning their call.
        let body = format!(
            "the first window speaks of one thing {}\n\nthe second window of another {}",
            "alpha filler words for sizing ".repeat(120),
            "beta filler words for sizing ".repeat(120)
        );
        let out = core.ingest(&body, "web", None).await.unwrap();
        crate::jobs::passages::capture_verbatim(&core, &out.id)
            .await
            .unwrap();
        let segs = core.store.segments_for_corpus(&out.id).await.unwrap();
        assert!(
            segs.len() > 1,
            "the fixture must be multi-window: {}",
            segs.len()
        );
        let p = core
            .store
            .artifacts_for_segment(&out.id, 0)
            .await
            .unwrap()
            .first()
            .expect("segment 0 owns a passage")
            .id
            .clone();
        (core, out.id, p)
    }

    /// The live half of what the pursuit sweep used to re-check hours later.
    ///
    /// A citation is the one signal `/ask` honestly gives about an artifact,
    /// and it arrives at the bump — against an activation that has not decayed
    /// yet, which is the whole reason the sweep's copy of this check could only
    /// ever decline where this one declined.
    #[tokio::test]
    async fn an_artifact_an_answer_cited_can_promote_its_window() {
        let (mut core, corpus, p) = earned_with_one_passage().await;
        core.completer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("the answer rests on this [1]".into()),
        }));
        crate::jobs::embed::run(&core, &p).await.unwrap();
        // The searches an ask runs bump retrieval too, and enough of it to
        // carry a passage over the line on its own — which would leave this
        // test passing with the citation deleted. Retrieval is silenced so
        // that the only thing left that can move activation is the citation.
        core.activation.retrieved = 0.0;
        let now = crate::store::now();
        // Just under the line, so the citation is what carries it over.
        core.store
            .bump_activation(
                std::slice::from_ref(&p),
                core.promote.activation_above - core.activation.cited + 0.1,
                core.activation.half_life_days,
                now,
            )
            .await
            .unwrap();
        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Verbatim),
            "the window was already promoted before the question"
        );

        core.ask(
            &crate::core::ask::AskRequest {
                q: "verbatim passage".into(),
                limit: None,
                tags: vec![],
                category: None,
            },
            crate::store::feedback::Door::Ui.by("me"),
        )
        .await
        .unwrap();
        core.background.wait_idle().await;

        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Pending),
            "the citation engaged the artifact but armed nothing"
        );
    }

    fn unit(corpus: &str) -> String {
        crate::jobs::window::unit_target(corpus, 0)
    }

    #[tokio::test]
    async fn a_passage_over_the_line_arms_its_window_once() {
        let (core, corpus, p) = earned_with_one_passage().await;
        // Baseline 1.0 plus one confirmed bump puts it at 4.0 exactly.
        core.store
            .bump_activation(std::slice::from_ref(&p), 3.0, 14.0, 1_000)
            .await
            .unwrap();
        let armed = maybe_promote(&core, std::slice::from_ref(&p), 1_000)
            .await
            .unwrap();
        assert_eq!(armed, 1);
        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Pending)
        );
        assert!(
            core.store
                .segment_keeps_artifacts(&corpus, 0)
                .await
                .unwrap()
        );
        assert!(
            core.store
                .live_job(Stage::SegmentWindow, &unit(&corpus))
                .await
                .unwrap()
        );
        // A second trigger on a window that is no longer verbatim does nothing.
        core.store
            .set_segment_state(&corpus, 0, SegmentState::Done, None)
            .await
            .unwrap();
        let again = maybe_promote(&core, std::slice::from_ref(&p), 1_000)
            .await
            .unwrap();
        assert_eq!(again, 0);
    }

    #[tokio::test]
    async fn an_undone_promotion_is_not_re_promoted_by_the_next_open() {
        // Undo puts the window back to `verbatim` so it can be read again, and
        // leaves the passage the activation it earned — which is the number
        // that armed the promotion in the first place. Without the mark the
        // next bump promotes it straight back and the operator's undo is
        // undone, with the deprecated artifacts still lying behind it.
        let (core, corpus, p) = earned_with_one_passage().await;
        core.store
            .bump_activation(std::slice::from_ref(&p), 3.0, 14.0, 1_000)
            .await
            .unwrap();
        assert_eq!(
            maybe_promote(&core, std::slice::from_ref(&p), 1_000)
                .await
                .unwrap(),
            1
        );

        core.undo_promotion(&corpus, 0).await.unwrap();
        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Verbatim),
            "the window is readable again"
        );
        // The passage still sits well over the line: nothing was decayed.
        assert_eq!(
            maybe_promote(&core, std::slice::from_ref(&p), 1_000)
                .await
                .unwrap(),
            0,
            "the undo holds"
        );
        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Verbatim)
        );
    }

    #[tokio::test]
    async fn under_the_line_nothing_is_armed() {
        let (core, corpus, p) = earned_with_one_passage().await;
        core.store
            .bump_activation(std::slice::from_ref(&p), 1.0, 14.0, 1_000)
            .await
            .unwrap();
        assert_eq!(
            maybe_promote(&core, std::slice::from_ref(&p), 1_000)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Verbatim)
        );
    }

    #[tokio::test]
    async fn twenty_listings_and_one_open_never_promote_but_a_confirmation_does() {
        // "Rewritten once you have actually used it." Exposure must not fill
        // the tank for one touch to pull the trigger: at `retrieved = 0.1`,
        // twenty listings plus one open reached the threshold, and at `1.0`
        // one listing did. Twenty retrievals and an open leave the window
        // verbatim; a confirmation — the strong signal — promotes it.
        let (core, corpus, p) = earned_with_one_passage().await;
        let ids = vec![p.clone()];
        // Stamped now: `mark_artifact_seen` reads the clock, and a bump from
        // 1970 would have decayed to nothing by the time it looks.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for _ in 0..20 {
            core.store
                .bump_activation(&ids, core.activation.retrieved, 14.0, now)
                .await
                .unwrap();
        }
        core.mark_artifact_seen(&p);
        core.background.wait_idle().await;
        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Verbatim),
            "listed twenty times and opened once, and that promoted it"
        );
        core.store
            .bump_activation(&ids, core.activation.confirmed, 14.0, now)
            .await
            .unwrap();
        assert!(maybe_promote(&core, &ids, now).await.unwrap() > 0);
        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Pending)
        );
    }

    #[tokio::test]
    async fn one_confirmation_promotes_however_long_ago_the_passage_was_captured() {
        // The threshold names what use added, and a confirmation adds the same
        // amount whenever it lands. Read against the raw stored number it did
        // not: the capture baseline decays out from under the line, so `4.0`
        // was reachable only by a confirmation at essentially zero elapsed
        // time — one day after capture it already took two, and on a passage
        // this old it would have taken two forever.
        let (core, corpus, p) = earned_with_one_passage().await;
        let ids = vec![p.clone()];
        let now = crate::store::now();
        let captured = now - 90 * 86_400;
        sqlx::query(
            "UPDATE artifacts SET activation = 1.0, activated_at = ?, created_at = ? WHERE id = ?",
        )
        .bind(captured)
        .bind(captured)
        .bind(&p)
        .execute(&core.store.pool)
        .await
        .unwrap();

        core.store
            .bump_activation(&ids, core.activation.confirmed, 14.0, now)
            .await
            .unwrap();
        // What the old reading saw: a whole confirmation, and still short of 4.
        let raw = core.store.activation_of(&ids).await.unwrap()[&p].0;
        assert!(raw < 4.0, "the raw activation was {raw}");

        assert!(maybe_promote(&core, &ids, now).await.unwrap() > 0);
        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Pending)
        );
    }

    #[tokio::test]
    async fn age_alone_never_promotes_a_passage_nobody_touched() {
        // The other direction: subtracting a decayed baseline must not turn a
        // never-touched artifact into an engaged one at any age.
        let (core, corpus, p) = earned_with_one_passage().await;
        let ids = vec![p.clone()];
        let now = crate::store::now();
        let captured = now - 900 * 86_400;
        sqlx::query(
            "UPDATE artifacts SET activation = 1.0, activated_at = ?, created_at = ? WHERE id = ?",
        )
        .bind(captured)
        .bind(captured)
        .bind(&p)
        .execute(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(maybe_promote(&core, &ids, now).await.unwrap(), 0);
        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Verbatim)
        );
    }

    use crate::store::artifacts::CorpusSpan;

    /// A span the splitter found: the ordinary case, and what a passage always
    /// has.
    fn sp(a: i64, b: i64) -> CorpusSpan {
        CorpusSpan::located(a, b)
    }

    /// The paraphrase's ordinary case: the model named lines, nothing checked
    /// them.
    fn claimed(a: i64, b: i64) -> CorpusSpan {
        CorpusSpan::claimed(a, b)
    }

    /// Nothing placed it — the whole-window fallback.
    fn unplaced(a: i64, b: i64) -> CorpusSpan {
        CorpusSpan::unplaced(a, b)
    }

    /// Two passages of one document, as an embedder actually places them:
    /// close, because they share a subject and its vocabulary — nowhere near
    /// the orthogonal pair a synthetic `[1,0]` / `[0,1]` suggests. `cos` here
    /// is 0.8, over `traceable_min`'s 0.75 and under anything a real
    /// paraphrase of the passage itself would score.
    fn same_document() -> (Vec<f32>, Vec<f32>) {
        let t: f32 = 0.8f32.acos();
        (vec![1.0, 0.0], vec![t.cos(), t.sin()])
    }

    #[test]
    fn the_majority_rule_is_per_artifact_best_overlap_ties_to_the_lowest_ordinal() {
        // passage 1–20; A claims 1–11 (11 lines, majority), B claims 12–20 (9).
        let passages = vec![("p".to_string(), sp(1, 20))];
        let arts = vec![
            ("b".to_string(), 1, sp(12, 20)),
            ("a".to_string(), 0, sp(1, 11)),
        ];
        assert_eq!(covered_by(&passages, &arts), vec![("p", "a")]);
        // 30% + 30% is not a majority: nobody claims it.
        let arts = vec![
            ("a".to_string(), 0, sp(1, 6)),
            ("b".to_string(), 1, sp(7, 12)),
        ];
        assert!(covered_by(&passages, &arts).is_empty());
        // Exactly half is not a majority either.
        let arts = vec![("a".to_string(), 0, sp(1, 10))];
        assert!(covered_by(&passages, &arts).is_empty());
        // A tie on overlap goes to the lowest ordinal.
        let arts = vec![
            ("z".to_string(), 5, sp(1, 20)),
            ("y".to_string(), 2, sp(1, 20)),
        ];
        assert_eq!(covered_by(&passages, &arts), vec![("p", "y")]);
    }

    #[test]
    fn on_equal_overlap_a_placed_artifact_beats_an_unplaced_one_whatever_its_ordinal() {
        let passages = vec![("p".to_string(), sp(1, 1))];
        // Ordinal 1 is unplaced — the fallback span that covers the whole
        // window and locates nothing. Ordinal 2 is a claim about line 1.
        let arts = vec![
            ("u".to_string(), 1, CorpusSpan::unplaced(1, 1)),
            ("c".to_string(), 2, CorpusSpan::claimed(1, 1)),
        ];
        assert_eq!(covered_by(&passages, &arts), vec![("p", "c")]);
        // Two placed: the lowest ordinal still wins.
        let arts = vec![
            ("y".to_string(), 3, CorpusSpan::claimed(1, 1)),
            ("x".to_string(), 2, CorpusSpan::claimed(1, 1)),
        ];
        assert_eq!(covered_by(&passages, &arts), vec![("p", "x")]);
    }

    /// A verbatim corpus of three passages (lines 1–2, 3–4, 5–6 of one
    /// window) with activation and a link on the middle one; then a
    /// promotion whose artifact A claims lines 1–4 and B claims line 6.
    async fn promoted_fixture() -> (
        crate::core::Core,
        String,
        Vec<crate::store::artifacts::Chunk>,
        Vec<crate::store::artifacts::Chunk>,
    ) {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let src = core
            .store
            .insert_corpus("l1\nl2\nl3\nl4\nl5\nl6", "web", None)
            .await
            .unwrap();
        core.store
            .upsert_segments(
                &src.id,
                &[crate::store::segments::NewSegment {
                    start_line: 1,
                    end_line: 6,
                    text: "l1\nl2\nl3\nl4\nl5\nl6",
                }],
            )
            .await
            .unwrap();
        core.store.mark_segments_verbatim(&src.id).await.unwrap();
        let na = |o: i64, t: &str, a: i64, b: i64| crate::store::artifacts::NewArtifact {
            ordinal: o,
            text: t.into(),
            corpus_span: Some(sp(a, b)),
            title: None,
            category: None,
            tags: vec![],
            segment_idx: Some(0),
            caveats: vec![],
        };
        let passages = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[
                    na(0, "lines one and two of the text", 1, 2),
                    na(1, "lines three and four of the text", 3, 4),
                    na(2, "lines five and six of the text", 5, 6),
                ],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();
        // Another corpus to link to.
        let other = core
            .store
            .insert_corpus("other", "web", None)
            .await
            .unwrap();
        let x = core
            .store
            .insert_artifacts(&other.id, &[na(0, "x", 1, 1)])
            .await
            .unwrap()[0]
            .id
            .clone();
        core.store
            .bump_activation(std::slice::from_ref(&passages[1].id), 4.0, 14.0, 1_000)
            .await
            .unwrap();
        core.store
            .bump_links(
                &[(passages[1].id.as_str(), x.as_str())],
                2.0,
                Some("mid"),
                14.0,
                1_000,
            )
            .await
            .unwrap();
        // The promotion's artifacts, as `write_segment_artifacts` would write them.
        core.store.reset_segment(&src.id, 0, true).await.unwrap();
        let written = core
            .store
            .insert_artifacts(
                &src.id,
                &[
                    // A's lines are traceable to passages 1 and 2; B's to 3.
                    na(
                        0,
                        "lines one and two of the text\nlines three and four of the text",
                        1,
                        4,
                    ),
                    na(1, "lines five and six of the text", 6, 6),
                ],
            )
            .await
            .unwrap();
        (core, src.id, passages, written)
    }

    #[tokio::test]
    async fn covered_passages_are_superseded_and_the_rest_stay_verbatim() {
        let (core, corpus, passages, written) = promoted_fixture().await;
        let n = supersede_covered(&core, &corpus, 0, &written, 2_000)
            .await
            .unwrap();
        assert_eq!(
            n, 2,
            "passages 1 and 2 are majority-covered by A; passage 3 is half-covered by B"
        );
        let p0 = core.store.get_artifact(&passages[0].id).await.unwrap();
        let p1 = core.store.get_artifact(&passages[1].id).await.unwrap();
        let p2 = core.store.get_artifact(&passages[2].id).await.unwrap();
        assert_eq!(p0.superseded_by.as_deref(), Some(written[0].id.as_str()));
        assert_eq!(p1.superseded_by.as_deref(), Some(written[0].id.as_str()));
        assert!(
            p2.in_results(),
            "lines 5–6: B claims one of two, not a majority"
        );
    }

    #[tokio::test]
    async fn the_artifact_takes_the_max_decayed_activation_not_one_point_zero() {
        let (core, corpus, passages, written) = promoted_fixture().await;
        supersede_covered(&core, &corpus, 0, &written, 1_000)
            .await
            .unwrap();
        let act = core
            .store
            .activation_of(&[written[0].id.clone(), passages[1].id.clone()])
            .await
            .unwrap();
        let (a_val, a_at, _) = act[&written[0].id];
        let (p_val, p_at, _) = act[&passages[1].id];
        let expect = crate::store::links::decayed(p_val, p_at, 1_000, 14.0);
        assert!((a_val - expect).abs() < 1e-6, "got {a_val}, want {expect}");
        assert_eq!(a_at, 1_000);
        assert!(a_val > 1.0);
    }

    /// What crosses is the engagement, not the raw stored sum. The winner was
    /// written moments ago, so its baseline stands at a full `1.0`, while an
    /// old passage's baseline has almost decayed away under whatever it
    /// earned: comparing the two raw numbers read one against the other. A
    /// passage six weeks old with `1.9` of earned access carried a raw `0.36`,
    /// lost the `max` to the winner's own `1.0`, and the artifact that was
    /// supposed to inherit the access it earned came out at nothing.
    #[tokio::test]
    async fn the_artifact_inherits_what_the_passage_earned_and_not_its_raw_sum() {
        let now = crate::store::now();
        let earned_after = |captured: i64, earned: f64, core: &crate::core::Core| {
            crate::store::links::decayed(earned, captured, now, core.activation.half_life_days)
        };
        // Six weeks — three half-lives — with 1.9 of earned access on it. The
        // raw sum by then is below the winner's own untouched baseline.
        for (age_days, earned) in [(42i64, 1.9f64), (14, 4.0)] {
            let (core, corpus, passages, written) = promoted_fixture().await;
            let captured = now - age_days * 86_400;
            sqlx::query(
                "UPDATE artifacts SET activation = ?, activated_at = ?, created_at = ? WHERE id = ?",
            )
            .bind(1.0 + earned)
            .bind(captured)
            .bind(captured)
            .bind(&passages[1].id)
            .execute(&core.store.pool)
            .await
            .unwrap();

            supersede_covered(&core, &corpus, 0, &written, now)
                .await
                .unwrap();

            let act = core
                .store
                .activation_of(&[written[0].id.clone()])
                .await
                .unwrap();
            let (v, s, c) = act[&written[0].id];
            let got =
                crate::store::links::engagement_at(v, s, c, now, core.activation.half_life_days);
            let want = earned_after(captured, earned, &core);
            assert!(
                (got - want).abs() < 1e-6,
                "at {age_days} days the artifact inherited {got} of the {want} the passage earned"
            );
            // And the baseline it is standing on is its own, still whole.
            assert!(
                (v - (1.0 + want)).abs() < 1e-6,
                "the winner's own baseline was overwritten: {v}"
            );
            assert_eq!(s, now);
        }
    }

    #[tokio::test]
    async fn a_link_from_a_superseded_passage_resolves_on_the_artifact_and_the_dead_row_stays() {
        let (core, corpus, passages, written) = promoted_fixture().await;
        supersede_covered(&core, &corpus, 0, &written, 1_000)
            .await
            .unwrap();
        let out = core
            .store
            .links_from(
                &[written[0].id.clone()],
                &[crate::store::links::LinkState::Learning],
                14.0,
                1_000,
                0.0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(out.len(), 1, "{out:?}");
        assert!((out[0].weight - 2.0).abs() < 1e-6);
        // The passage's own row is still there — dark, because its endpoint
        // is superseded.
        assert_eq!(
            core.store
                .links_touching(&passages[1].id)
                .await
                .unwrap()
                .len(),
            1
        );
        let from_passage = core
            .store
            .links_from(
                &[passages[1].id.clone()],
                &[crate::store::links::LinkState::Learning],
                14.0,
                1_000,
                0.0,
                10,
            )
            .await
            .unwrap();
        assert!(from_passage.is_empty());
    }

    #[tokio::test]
    async fn a_verbatim_window_keeps_its_passages_without_the_mark() {
        // `jobs::window::run` reads the mark *or* the verbatim state and goes
        // on to embed and supersede on that answer; the writer used to read
        // only the mark. With the mark spent — which is what `undo_promotion`
        // leaves — the write took the replacing branch and deleted the
        // passages the promotion exists to keep.
        let (core, corpus, passages, _written) = promoted_fixture().await;
        core.store.reset_segment(&corpus, 0, false).await.unwrap();
        core.store
            .set_segment_state(
                &corpus,
                0,
                crate::store::segments::SegmentState::Verbatim,
                None,
            )
            .await
            .unwrap();
        assert!(
            !core
                .store
                .segment_keeps_artifacts(&corpus, 0)
                .await
                .unwrap(),
            "the mark is spent; the state is the only witness left"
        );
        crate::jobs::window::write_segment_artifacts(&core, &corpus, 0, vec![])
            .await
            .unwrap();
        let live = core
            .store
            .artifact_ids_for_segment(&corpus, 0)
            .await
            .unwrap();
        for p in &passages {
            assert!(
                live.contains(&p.id),
                "a verbatim passage was deleted: {p:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_re_run_under_keep_artifacts_writes_nothing_twice() {
        let (core, corpus, _passages, written) = promoted_fixture().await;
        // `write_segment_artifacts` with keep set and non-passage rows already
        // present returns those rows and inserts none.
        let again = crate::jobs::window::write_segment_artifacts(
            &core,
            &corpus,
            0,
            vec![crate::store::artifacts::NewArtifact {
                ordinal: 0,
                text: "dup".into(),
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
        assert_eq!(
            again.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            written.iter().map(|c| c.id.clone()).collect::<Vec<_>>()
        );
        assert_eq!(
            core.store
                .artifacts_for_segment(&corpus, 0)
                .await
                .unwrap()
                .len(),
            5
        );
    }

    /// A point for one artifact, so a test can say what the store already
    /// holds by the time supersession runs: the passages were embedded at
    /// capture and `embed_written` embedded the artifacts.
    async fn embed_as(core: &crate::core::Core, id: &str, corpus: &str, v: Vec<f32>) {
        core.vectors
            .upsert(vec![crate::vector::VectorPoint {
                vector: v,
                sparse: Default::default(),
                payload: crate::vector::VectorPayload {
                    artifact_id: id.into(),
                    corpus_id: corpus.into(),
                    text: String::new(),
                    title: None,
                    category: None,
                    tags: vec![],
                    created_at: 0,
                    last_seen_at: None,
                    hit_count: None,
                    status: None,
                    last_verified_at: None,
                    superseded_by: None,
                    origin_corpora: vec![],
                    provenance: None,
                },
            }])
            .await
            .unwrap();
    }

    /// One artifact over the passage it was written from, sharing not one line
    /// with it.
    ///
    /// This is the ordinary case, not an exotic one: synthesis resolves
    /// pronouns and completes fragments, so a capture of prose comes back with
    /// nothing matching word for word. The verbatim rule alone left the
    /// passage in results beside the artifact written from it — two hits for
    /// one note, on every capture that was not code.
    #[tokio::test]
    async fn a_rewrite_that_copied_no_line_supersedes_the_passage_it_says() {
        let (core, corpus, passages, _written) = promoted_fixture().await;
        let rewrite = core
            .store
            .insert_artifacts(
                &corpus,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 9,
                    text: "Die dritte und vierte Zeile, in eigenen Worten.".into(),
                    corpus_span: Some(sp(3, 4)),
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        // What the embedder would have said: the rewrite lands on the passage
        // it rewrote.
        embed_as(&core, &rewrite[0].id, &corpus, vec![1.0, 0.0]).await;
        embed_as(&core, &passages[1].id, &corpus, vec![1.0, 0.0]).await;
        embed_as(&core, &passages[0].id, &corpus, vec![0.0, 1.0]).await;
        embed_as(&core, &passages[2].id, &corpus, vec![0.0, 1.0]).await;

        let n = supersede_covered(&core, &corpus, 0, &rewrite, 2_000)
            .await
            .unwrap();
        assert_eq!(n, 1, "the passage the rewrite covers is still in results");
        let hidden = core.store.get_artifact(&passages[1].id).await.unwrap();
        assert!(!hidden.in_results());
        assert_eq!(
            hidden.superseded_by.as_deref(),
            Some(rewrite[0].id.as_str())
        );
        // Hidden, never gone: the split pane still renders it, and the reaper
        // is what decides its fate later.
        assert_eq!(hidden.text, passages[1].text);
        // And only the one it covers.
        for other in [&passages[0], &passages[2]] {
            assert!(
                core.store
                    .get_artifact(&other.id)
                    .await
                    .unwrap()
                    .in_results(),
                "a passage the rewrite does not cover was hidden too"
            );
        }
    }

    /// The cosine's own floor: a span the model only claimed, over text the
    /// artifact says nothing about and scores nothing against. The verbatim
    /// rule caught this and so must the meaning.
    ///
    /// What this does *not* test is the floor being high enough, and no test
    /// with two orthogonal vectors can: see
    /// `a_whole_window_fallback_hides_nothing_however_similar` for the
    /// similarity a real pair of passages out of one document reaches.
    #[tokio::test]
    async fn a_rewrite_about_something_else_leaves_the_passage_standing() {
        let (core, corpus, passages, _written) = promoted_fixture().await;
        let stray = core
            .store
            .insert_artifacts(
                &corpus,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 9,
                    text: "an entirely different sentence about nothing here".into(),
                    corpus_span: Some(claimed(3, 4)),
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        embed_as(&core, &stray[0].id, &corpus, vec![1.0, 0.0]).await;
        embed_as(&core, &passages[1].id, &corpus, vec![0.0, 1.0]).await;

        let n = supersede_covered(&core, &corpus, 0, &stray, 2_000)
            .await
            .unwrap();
        assert_eq!(n, 0);
        for p in &passages {
            assert!(core.store.get_artifact(&p.id).await.unwrap().in_results());
        }
    }

    /// The fallback span, whole and entire: the failure a paraphrasing model
    /// produces on its own.
    ///
    /// `resolve_span` found nothing of the artifact in the window and handed it
    /// the window — so every passage in it is majority-covered by this one
    /// artifact, and there is nothing left between the passages and being
    /// hidden but the cosine. At the similarity passages of one document
    /// actually score against each other, which is over the threshold and
    /// nothing like the orthogonal pair a synthetic fixture suggests, all
    /// three stay: the artifact was never placed, so the number is not about
    /// them.
    #[tokio::test]
    async fn a_whole_window_fallback_hides_nothing_however_similar() {
        let (core, corpus, passages, _written) = promoted_fixture().await;
        let sweeping = core
            .store
            .insert_artifacts(
                &corpus,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 9,
                    text: "A rewrite of the whole note, in the model's own words.".into(),
                    corpus_span: Some(unplaced(1, 6)),
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        let (artifact_v, passage_v) = same_document();
        assert!(
            crate::vector::cosine(&artifact_v, &passage_v) > core.promote.traceable_min,
            "the fixture has to clear the threshold, or it proves nothing"
        );
        embed_as(&core, &sweeping[0].id, &corpus, artifact_v).await;
        for p in &passages {
            embed_as(&core, &p.id, &corpus, passage_v.clone()).await;
        }
        assert_eq!(
            covered_by(
                &passages
                    .iter()
                    .map(|p| (p.id.clone(), p.corpus_span.clone().unwrap()))
                    .collect::<Vec<_>>(),
                &[(sweeping[0].id.clone(), 9, unplaced(1, 6))],
            )
            .len(),
            3,
            "the span covers all three; only the guard may stop it"
        );
        assert_eq!(
            supersede_covered(&core, &corpus, 0, &sweeping, 2_000)
                .await
                .unwrap(),
            0
        );
        for p in &passages {
            assert!(core.store.get_artifact(&p.id).await.unwrap().in_results());
        }
    }

    /// A passage whose point has not landed yet is not a passage that fails
    /// the cosine — it is one nobody has measured. The corpus embed job and
    /// this window's job share a class, an attempts count and a seq, so with
    /// more than one worker they run at once; and nothing re-runs
    /// `supersede_covered` for a `done` window, so "not yet embedded" used to
    /// mean "left standing beside its replacement for good".
    #[tokio::test]
    async fn a_passage_with_no_point_yet_is_embedded_rather_than_written_off() {
        let (core, corpus, passages, _written) = promoted_fixture().await;
        let rewrite = core
            .store
            .insert_artifacts(
                &corpus,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 9,
                    text: "Die dritte und vierte Zeile, in eigenen Worten.".into(),
                    corpus_span: Some(sp(3, 4)),
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        embed_as(&core, &rewrite[0].id, &corpus, same_document().0).await;
        assert!(
            core.vectors
                .dense_of(&passages[1].id)
                .await
                .unwrap()
                .is_none(),
            "the fixture must start with the passage unembedded"
        );

        supersede_covered(&core, &corpus, 0, &rewrite, 2_000)
            .await
            .unwrap();

        assert!(
            core.vectors
                .dense_of(&passages[1].id)
                .await
                .unwrap()
                .is_some(),
            "the passage was judged against a point nobody had written"
        );
    }

    /// Above 1.0 is how the key says "verbatim only", and it has to reach the
    /// path that would otherwise have superseded.
    #[tokio::test]
    async fn a_threshold_over_one_leaves_only_the_verbatim_rule() {
        let (mut core, corpus, passages, _written) = promoted_fixture().await;
        core.promote.traceable_min = 1.1;
        let rewrite = core
            .store
            .insert_artifacts(
                &corpus,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 9,
                    text: "Die dritte und vierte Zeile, in eigenen Worten.".into(),
                    corpus_span: Some(sp(3, 4)),
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        embed_as(&core, &rewrite[0].id, &corpus, vec![1.0, 0.0]).await;
        embed_as(&core, &passages[1].id, &corpus, vec![1.0, 0.0]).await;

        assert_eq!(
            supersede_covered(&core, &corpus, 0, &rewrite, 2_000)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_majority_span_without_a_traceable_line_does_not_supersede() {
        // `resolve_span` can only guess a paraphrase's span — the model's hint,
        // or the whole window — and a guess must not hide verbatim text. The
        // artifact here claims the passage's lines by span but shares no line
        // of text with it: the passage stays.
        let (core, corpus, passages, _written) = promoted_fixture().await;
        let na = |o: i64, t: &str, a: i64, b: i64| crate::store::artifacts::NewArtifact {
            ordinal: o,
            text: t.into(),
            corpus_span: Some(sp(a, b)),
            title: None,
            category: None,
            tags: vec![],
            segment_idx: Some(0),
            caveats: vec![],
        };
        let paraphrase = core
            .store
            .insert_artifacts(
                &corpus,
                &[na(
                    9,
                    "an entirely different sentence about nothing here",
                    1,
                    6,
                )],
            )
            .await
            .unwrap();
        let n = supersede_covered(&core, &corpus, 0, &paraphrase, 2_000)
            .await
            .unwrap();
        assert_eq!(n, 0);
        for p in &passages {
            assert!(core.store.get_artifact(&p.id).await.unwrap().in_results());
        }
    }
}
