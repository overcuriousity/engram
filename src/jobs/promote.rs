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
    if core.synthesis != crate::config::SynthesisMode::Earned || !core.synthesizes() {
        return Ok(0);
    }
    let activation = core.store.activation_of(ids).await?;
    let mut armed = 0;
    for id in ids {
        let Some((value, stamp)) = activation.get(id) else {
            continue;
        };
        let now_value =
            crate::store::links::decayed(*value, *stamp, at, core.activation.half_life_days);
        if now_value < core.promote.activation_above {
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
            activation = now_value,
            "promoting a window"
        );
        armed += 1;
    }
    Ok(armed)
}

/// The `eager` counterpart of promotion: an artifact shown
/// `resynthesize_after_unconfirmed` times with no confirmation recorded
/// against it is misleading, and is re-synthesised from its source segment —
/// never from itself. `keep_artifacts = 0`: replace, because the old artifacts
/// are the problem. `0` disables it and it ships disabled.
///
/// `hits` carries each artifact's retrieval count *after* this retrieval.
pub async fn maybe_resynthesize(core: &Core, hits: &[(String, i64)]) -> Result<usize> {
    let line = core.promote.resynthesize_after_unconfirmed;
    if line <= 0 || core.synthesis != crate::config::SynthesisMode::Eager || !core.synthesizes() {
        return Ok(0);
    }
    let mut armed = 0;
    for (id, count) in hits {
        if *count < line {
            continue;
        }
        let Ok(c) = core.store.get_artifact(id).await else {
            continue;
        };
        if c.provenance != Provenance::Captured || !c.in_results() {
            continue;
        }
        let (Some(corpus_id), Some(idx)) = (c.corpus_id.as_deref(), c.segment_idx) else {
            continue;
        };
        if core.store.segment_state(corpus_id, idx).await? != Some(SegmentState::Done) {
            continue;
        }
        if core.store.artifact_confirmed(id).await? {
            continue;
        }
        core.store.reset_segment(corpus_id, idx, false).await?;
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
            shown = count,
            "re-synthesising an unconfirmed window"
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
/// overlap wins; a tie goes to the lowest ordinal. Everything else stays
/// active, verbatim, in results: promotion can only ever improve coverage.
pub fn covered_by<'a>(
    passages: &'a [(String, CorpusSpan)],
    artifacts: &'a [(String, i64, CorpusSpan)],
) -> Vec<(&'a str, &'a str)> {
    let mut out = Vec::new();
    for (pid, ps) in passages {
        let len = ps.end_line - ps.start_line + 1;
        let best = artifacts
            .iter()
            .map(|(aid, ord, asp)| (overlap(ps, asp), *ord, aid.as_str()))
            .filter(|(ov, _, _)| 2 * ov > len)
            .max_by(|x, y| x.0.cmp(&y.0).then(y.1.cmp(&x.1)));
        if let Some((_, _, aid)) = best {
            out.push((pid.as_str(), aid));
        }
    }
    out
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
    // be *traceable* to the passage — at least one of its lines found in the
    // passage's text. Whatever is not traceable leaves the passage standing,
    // which is the direction promotion is allowed to fail in.
    let text_of = |id: &str| rows.iter().find(|c| c.id == id).map(|c| c.text.as_str());
    let pairs: Vec<(&str, &str)> = covered_by(&passages, &artifacts)
        .into_iter()
        .filter(|(p, a)| {
            let (Some(pt), Some(at)) = (
                text_of(p),
                written.iter().find(|c| c.id == *a).map(|c| c.text.as_str()),
            ) else {
                return false;
            };
            crate::infer::verify::locate_span(at, pt, 1).is_some()
        })
        .collect();
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
        let act = core.store.activation_of(&ids).await?;
        let carried = act
            .values()
            .map(|(v, s)| crate::store::links::decayed(*v, *s, at, half_life))
            .fold(f64::MIN, f64::max);
        if carried > f64::MIN {
            let own = core
                .store
                .activation_of(std::slice::from_ref(&winner.to_string()))
                .await?
                .get(winner)
                .map(|(v, s)| crate::store::links::decayed(*v, *s, at, half_life))
                .unwrap_or(1.0);
            core.store
                .set_activation(winner, carried.max(own), at)
                .await?;
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
    use crate::config::SynthesisMode;
    use crate::core::test_support::test_core;

    /// A core at `earned`, recording, with one verbatim corpus of one passage.
    async fn earned_with_one_passage() -> (crate::core::Core, String, String) {
        let mut core = test_core().await;
        core.synthesis = SynthesisMode::Earned;
        core.feedback.enabled = true;
        let out = core
            .ingest("a single verbatim passage", "web", None)
            .await
            .unwrap();
        crate::jobs::passages::capture_verbatim(&core, &out.id)
            .await
            .unwrap();
        let p = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();
        (core, out.id, p)
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
    async fn only_earned_with_a_synthesizer_promotes() {
        let (mut core, _corpus, p) = earned_with_one_passage().await;
        core.store
            .bump_activation(std::slice::from_ref(&p), 5.0, 14.0, 1_000)
            .await
            .unwrap();
        core.synthesis = SynthesisMode::Off;
        assert_eq!(
            maybe_promote(&core, std::slice::from_ref(&p), 1_000)
                .await
                .unwrap(),
            0
        );
        core.synthesis = SynthesisMode::Earned;
        core.synthesizer = None;
        assert_eq!(
            maybe_promote(&core, std::slice::from_ref(&p), 1_000)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn retrieval_alone_never_promotes_but_one_open_afterwards_does() {
        // The threshold is checked at the opened bump, not the retrieved one:
        // ten retrievals leave the window verbatim; the first open promotes.
        let (core, corpus, p) = earned_with_one_passage().await;
        let ids = vec![p.clone()];
        // Stamped now: `mark_artifact_seen` reads the clock, and a bump from
        // 1970 would have decayed to nothing by the time it looks.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for _ in 0..10 {
            core.store
                .bump_activation(&ids, core.activation.retrieved, 14.0, now)
                .await
                .unwrap();
        }
        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Verbatim)
        );
        core.mark_artifact_seen(&p);
        core.background.wait_idle().await;
        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Pending)
        );
    }

    use crate::store::artifacts::CorpusSpan;

    fn sp(a: i64, b: i64) -> CorpusSpan {
        CorpusSpan {
            start_line: a,
            end_line: b,
        }
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
        core.synthesis = SynthesisMode::Earned;
        core.feedback.enabled = true;
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
                    carry_lines: 0,
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
        let (a_val, a_at) = act[&written[0].id];
        let (p_val, p_at) = act[&passages[1].id];
        let expect = crate::store::links::decayed(p_val, p_at, 1_000, 14.0);
        assert!((a_val - expect).abs() < 1e-6, "got {a_val}, want {expect}");
        assert_eq!(a_at, 1_000);
        assert!(a_val > 1.0);
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

    #[tokio::test]
    async fn an_eager_artifact_shown_often_and_never_confirmed_is_re_read_from_its_segment_when_enabled()
     {
        let mut core = test_core().await;
        core.promote.resynthesize_after_unconfirmed = 3;
        let src = core
            .store
            .insert_corpus("l1\nl2", "web", None)
            .await
            .unwrap();
        core.store
            .upsert_segments(
                &src.id,
                &[crate::store::segments::NewSegment {
                    start_line: 1,
                    end_line: 2,
                    text: "l1\nl2",
                    carry_lines: 0,
                }],
            )
            .await
            .unwrap();
        core.store
            .set_segment_state(&src.id, 0, SegmentState::Done, None)
            .await
            .unwrap();
        let a = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "artifact".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        // Under the line: nothing.
        assert_eq!(
            maybe_resynthesize(&core, &[(a.clone(), 2)]).await.unwrap(),
            0
        );
        // At the line, unconfirmed: the window is re-armed to *replace*.
        assert_eq!(
            maybe_resynthesize(&core, &[(a.clone(), 3)]).await.unwrap(),
            1
        );
        assert_eq!(
            core.store.segment_state(&src.id, 0).await.unwrap(),
            Some(SegmentState::Pending)
        );
        assert!(
            !core
                .store
                .segment_keeps_artifacts(&src.id, 0)
                .await
                .unwrap(),
            "replace, not append: the old artifacts are the problem"
        );
        assert!(
            core.store
                .live_job(Stage::SegmentWindow, &unit(&src.id))
                .await
                .unwrap()
        );
        // Disabled (0) never fires.
        core.promote.resynthesize_after_unconfirmed = 0;
        core.store
            .set_segment_state(&src.id, 0, SegmentState::Done, None)
            .await
            .unwrap();
        assert_eq!(
            maybe_resynthesize(&core, &[(a.clone(), 99)]).await.unwrap(),
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
