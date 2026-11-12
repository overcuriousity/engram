//! Promotion: synthesis armed by evidence instead of by capture.

use crate::core::Core;
use crate::error::Result;
use crate::store::artifacts::{Chunk, CorpusSpan, Provenance};
use crate::store::jobs::Stage;
use crate::store::segments::SegmentState;

/// The journal's subject for one window, and what `undo_promotion` stamps.
pub fn window_key(corpus_id: &str, idx: i64) -> String {
    format!("{corpus_id}#{idx}")
}

/// Called after an engagement bump, never a retrieval bump, and at the bump rather than on a
/// sweep, whose decayed reading would depend on when it ran. Arms a job; calls no model.
pub async fn maybe_promote(core: &Core, ids: &[String], at: i64) -> Result<usize> {
    // The embed-only floor: no synthesizer to promote to.
    if core.synthesizer.is_none() {
        return Ok(0);
    }
    // Promotion's own daily cap: it runs on ordinary reading, and a shared count would stand
    // dedupe, reap and condense down. Nothing is lost: the window is read again once the day moves.
    if !core.may_act(crate::store::actions::Job::Promote).await? {
        tracing::info!("a window is over the promotion line, but the day's budget is spent");
        return Ok(0);
    }
    let activation = core.store.activation_of(ids).await?;
    let mut armed = 0;
    for id in ids {
        let Some((value, stamp, created_at)) = activation.get(id) else {
            continue;
        };
        // What use added above the capture baseline, both decayed to one instant: against the raw
        // sum the threshold would mean something different at every age.
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
        // A window that is `done`, or on its way, never promotes again.
        if core.store.segment_state(corpus_id, idx).await? != Some(SegmentState::Verbatim) {
            continue;
        }
        // A window `undo_promotion` reset keeps its passages' activation, so without this the next
        // open would undo the undo.
        if core.store.segment_no_promote(corpus_id, idx).await? {
            continue;
        }
        // `keep_artifacts`: the window job appends, so the passages survive to be superseded.
        core.store.reset_segment(corpus_id, idx, true).await?;
        core.store
            .rearm_idle_seq(
                Stage::SegmentWindow,
                "segment",
                &crate::jobs::window::unit_target(corpus_id, idx),
                idx,
            )
            .await?;
        core.store
            .record_action(&crate::store::actions::NewAction {
                job: crate::store::actions::Job::Promote,
                kind: crate::store::actions::Kind::Promote,
                subject_id: window_key(corpus_id, idx),
                survivor_id: None,
                detail: None,
                evidence: serde_json::json!({ "passage": id, "activation": earned }),
                pair_score: None,
            })
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

/// Majority per artifact, not cumulative: `supersede` names one winner, and a passage hidden behind
/// a third of it sends the reader to the wrong text. Everything else stays active.
pub fn covered_by<'a>(
    passages: &'a [(String, CorpusSpan)],
    artifacts: &'a [(String, i64, CorpusSpan)],
) -> Vec<(&'a str, &'a str)> {
    let mut out = Vec::new();
    for (pid, ps) in passages {
        let len = ps.end_line - ps.start_line + 1;
        // A placed span before an unplaced one, which is the whole-window fallback; then the lowest
        // ordinal.
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

/// Read against `[promote] traceable_min`, not `auto_supersede`: telling passages of one
/// synthesized window apart is a weaker claim than two texts agreeing. Every way of not knowing
/// answers `false`: an outage must not hide verbatim text.
async fn corroborated_by_vector(core: &Core, artifact_id: &str, passage_id: &str) -> bool {
    let min = core.promote.traceable_min;
    if min > 1.0 {
        return false;
    }
    let dense = |id: String| async move {
        match core.vectors.dense_of(&id).await {
            Ok(Some(v)) => Some(v),
            // The passage's point may still be in flight from a concurrent embed job, and nothing
            // re-runs this for a `done` window, so embed it inline.
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

/// Activation, then links, then the supersede, which refuses a side no longer active. Returns how
/// many passages were superseded.
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
    // A span is only a guess for a rewrite, so the artifact must also be traceable: a verbatim
    // line, or, for a placed span (`Claimed` included), a cosine. An unplaced span keeps the
    // verbatim rule only: across one document every passage clears any cosine threshold.
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

    // One artifact may supersede several passages; its activation is the max over them.
    let mut by_winner: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for (p, a) in &pairs {
        by_winner.entry(a).or_default().push(p);
    }
    let mut n = 0;

    // Every supersession first, then what they earned: `try_supersede` may refuse, and nothing
    // moves off a passage still standing. Whole rather than per winner, so `all_superseded` names
    // only passages that actually went dark.
    let mut won: Vec<(&str, Vec<String>)> = Vec::new();
    for (winner, losers) in by_winner {
        let mut ids: Vec<String> = Vec::new();
        for loser in &losers {
            if crate::jobs::try_supersede(core, loser, winner, "a passage its promotion covers")
                .await
            {
                n += 1;
                ids.push((*loser).to_string());
            }
        }
        if !ids.is_empty() {
            won.push((winner, ids));
        }
    }

    let all_superseded: std::collections::HashSet<&str> = won
        .iter()
        .flat_map(|(_, ids)| ids.iter().map(String::as_str))
        .collect();

    for (winner, ids) in &won {
        let winner = *winner;
        // The earned part, not the raw sum: each baseline is anchored at its own `created_at`, so a
        // fresh winner's baseline would swallow an old passage's use.
        let act = core.store.activation_of(ids).await?;
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
            // Only ever upwards, on the winner's own baseline.
            if carried > earned {
                core.store
                    .set_activation(winner, now + (carried - earned), at)
                    .await?;
            }
        }
        for loser in ids {
            for link in core.store.links_touching(loser).await? {
                let other = if link.a_id == *loser {
                    &link.b_id
                } else {
                    &link.a_id
                };
                // A link within this promotion would become a self-link or point at a passage going
                // dark.
                if all_superseded.contains(other.as_str()) {
                    continue;
                }
                core.store
                    .carry_link(&link, loser, winner, link_half_life, at)
                    .await?;
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;

    async fn earned_with_one_passage() -> (crate::core::Core, String, String) {
        let mut core = test_core().await;
        core.learn.enabled = true;
        // Multi-window on purpose: a capture fitting one synthesis call is synthesized at capture.
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

    #[tokio::test]
    async fn an_artifact_an_answer_cited_can_promote_its_window() {
        let (mut core, corpus, p) = earned_with_one_passage().await;
        core.completer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("the answer rests on this [1]".into()),
        }));
        crate::jobs::embed::run(&core, &p).await.unwrap();
        // An ask's searches bump retrieval enough to carry a passage alone; silenced so only the
        // citation counts.
        core.activation.retrieved = 0.0;
        let now = crate::store::now();
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
        let rows = core
            .store
            .open_actions(&[crate::store::actions::Kind::Promote], 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].subject_id, window_key(&corpus, 0));
        assert!(
            rows[0].evidence_json.contains(&p),
            "{}",
            rows[0].evidence_json
        );
        assert!(
            core.store
                .live_job(Stage::SegmentWindow, &unit(&corpus))
                .await
                .unwrap()
        );
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
    async fn a_spent_budget_arms_no_promotion() {
        let (mut core, corpus, p) = earned_with_one_passage().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        core.evolve.max_actions_per_day = 0;
        let now = crate::store::now();
        core.store
            .bump_activation(
                std::slice::from_ref(&p),
                core.promote.activation_above + 1.0,
                core.activation.half_life_days,
                now,
            )
            .await
            .unwrap();

        assert_eq!(
            maybe_promote(&core, std::slice::from_ref(&p), now)
                .await
                .unwrap(),
            0,
            "a promotion was armed on a budget that is spent"
        );
        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Verbatim),
            "the window was reset for a promotion nobody could afford"
        );

        // Below "full" the corpus units are under their own switch, and the budget is not
        // consulted.
        core.evolve.autonomous = crate::config::Autonomy::Ranking;
        assert_eq!(
            maybe_promote(&core, std::slice::from_ref(&p), now)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn twenty_listings_and_one_open_never_promote_but_a_confirmation_does() {
        let (core, corpus, p) = earned_with_one_passage().await;
        let ids = vec![p.clone()];
        // Stamped now: `mark_artifact_seen` reads the clock, and a 1970 bump would decay to
        // nothing.
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
        let (core, corpus, p) = earned_with_one_passage().await;
        let ids = vec![p.clone()];
        let now = crate::store::now();
        let captured = now - 90 * 86_400;
        sqlx::query!(
            "UPDATE artifacts SET activation = 1.0, activated_at = ?, created_at = ? WHERE id = ?",
            captured,
            captured,
            &p
        )
        .execute(&core.store.pool)
        .await
        .unwrap();

        core.store
            .bump_activation(&ids, core.activation.confirmed, 14.0, now)
            .await
            .unwrap();
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
        let (core, corpus, p) = earned_with_one_passage().await;
        let ids = vec![p.clone()];
        let now = crate::store::now();
        let captured = now - 900 * 86_400;
        sqlx::query!(
            "UPDATE artifacts SET activation = 1.0, activated_at = ?, created_at = ? WHERE id = ?",
            captured,
            captured,
            &p
        )
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

    fn sp(a: i64, b: i64) -> CorpusSpan {
        CorpusSpan::located(a, b)
    }

    fn claimed(a: i64, b: i64) -> CorpusSpan {
        CorpusSpan::claimed(a, b)
    }

    fn unplaced(a: i64, b: i64) -> CorpusSpan {
        CorpusSpan::unplaced(a, b)
    }

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
        let arts = vec![
            ("a".to_string(), 0, sp(1, 6)),
            ("b".to_string(), 1, sp(7, 12)),
        ];
        assert!(covered_by(&passages, &arts).is_empty());
        let arts = vec![("a".to_string(), 0, sp(1, 10))];
        assert!(covered_by(&passages, &arts).is_empty());
        let arts = vec![
            ("z".to_string(), 5, sp(1, 20)),
            ("y".to_string(), 2, sp(1, 20)),
        ];
        assert_eq!(covered_by(&passages, &arts), vec![("p", "y")]);
    }

    #[test]
    fn on_equal_overlap_a_placed_artifact_beats_an_unplaced_one_whatever_its_ordinal() {
        let passages = vec![("p".to_string(), sp(1, 1))];
        // Ordinal 1 is the unplaced whole-window fallback; ordinal 2 claims line 1.
        let arts = vec![
            ("u".to_string(), 1, CorpusSpan::unplaced(1, 1)),
            ("c".to_string(), 2, CorpusSpan::claimed(1, 1)),
        ];
        assert_eq!(covered_by(&passages, &arts), vec![("p", "c")]);
        let arts = vec![
            ("y".to_string(), 3, CorpusSpan::claimed(1, 1)),
            ("x".to_string(), 2, CorpusSpan::claimed(1, 1)),
        ];
        assert_eq!(covered_by(&passages, &arts), vec![("p", "x")]);
    }

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
            segment_idx: Some(0),
            ..Default::default()
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
        core.store.reset_segment(&src.id, 0, true).await.unwrap();
        let written = core
            .store
            .insert_artifacts(
                &src.id,
                &[
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

    #[tokio::test]
    async fn the_artifact_inherits_what_the_passage_earned_and_not_its_raw_sum() {
        let now = crate::store::now();
        let earned_after = |captured: i64, earned: f64, core: &crate::core::Core| {
            crate::store::links::decayed(earned, captured, now, core.activation.half_life_days)
        };
        // Three half-lives: the raw sum is below the winner's own untouched baseline.
        for (age_days, earned) in [(42i64, 1.9f64), (14, 4.0)] {
            let (core, corpus, passages, written) = promoted_fixture().await;
            let captured = now - age_days * 86_400;
            sqlx::query!(
                "UPDATE artifacts SET activation = ?, activated_at = ?, created_at = ? WHERE id = ?",
                1.0 + earned,
                captured,
                captured,
                &passages[1].id
            )
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
    async fn a_link_to_a_sibling_the_supersession_was_refused_for_is_still_carried() {
        let (core, corpus, passages, mut written) = promoted_fixture().await;
        written[1].corpus_span = Some(sp(5, 6));
        core.store
            .bump_links(
                &[(passages[1].id.as_str(), passages[2].id.as_str())],
                2.0,
                Some("siblings"),
                14.0,
                1_000,
            )
            .await
            .unwrap();
        // `supersede` refuses a deprecated winner, so passage 3 stays.
        core.deprecate(&written[1].id).await.unwrap();

        let n = supersede_covered(&core, &corpus, 0, &written, 2_000)
            .await
            .unwrap();
        assert_eq!(n, 2, "A's two passages went dark and B's did not");
        assert_eq!(
            core.store
                .get_artifact(&passages[2].id)
                .await
                .unwrap()
                .superseded_by,
            None,
            "the passage B was refused for is still standing"
        );
        assert!(
            core.store
                .links_touching(&written[0].id)
                .await
                .unwrap()
                .iter()
                .any(|l| l.a_id == passages[2].id || l.b_id == passages[2].id),
            "so the link to it belongs on the artifact that took its sibling's place"
        );
    }

    #[tokio::test]
    async fn a_verbatim_window_keeps_its_passages_without_the_mark() {
        // With the mark spent, as `undo_promotion` leaves it, the writer must still keep the
        // passages.
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
        let again = crate::jobs::window::write_segment_artifacts(
            &core,
            &corpus,
            0,
            vec![crate::store::artifacts::NewArtifact {
                text: "dup".into(),
                segment_idx: Some(0),
                ..Default::default()
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

    async fn embed_as(core: &crate::core::Core, id: &str, corpus: &str, v: Vec<f32>) {
        core.vectors
            .upsert(vec![crate::vector::VectorPoint {
                vector: v,
                sparse: Default::default(),
                payload: crate::vector::VectorPayload {
                    artifact_id: id.into(),
                    corpus_id: corpus.into(),
                    ..Default::default()
                },
            }])
            .await
            .unwrap();
    }

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
                    segment_idx: Some(0),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
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
        assert_eq!(hidden.text, passages[1].text);
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
                    segment_idx: Some(0),
                    ..Default::default()
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
                    segment_idx: Some(0),
                    ..Default::default()
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
                    segment_idx: Some(0),
                    ..Default::default()
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
                    segment_idx: Some(0),
                    ..Default::default()
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
        let (core, corpus, passages, _written) = promoted_fixture().await;
        let na = |o: i64, t: &str, a: i64, b: i64| crate::store::artifacts::NewArtifact {
            ordinal: o,
            text: t.into(),
            corpus_span: Some(sp(a, b)),
            segment_idx: Some(0),
            ..Default::default()
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
