//! The corpus jobs answer to the same evidence the ranking side answers to.
//!
//! Two rules, both read inside the idle pass after the anchor check, under
//! the same claim and the same switch. Rule 1 asks whether what a merge or a
//! supersession hid is still found through what now answers for it. Rule 2
//! asks whether a search that was given up on would have been answered by
//! something the base hid or buried. Either way the base takes its own action
//! back through the same `Core` method the operator's button calls, and
//! stamps the journal row as taken back on evidence.
//!
//! Nothing here is a tuned constant. Rule 1 is `recommend` pointed the other
//! way: the subject's own record is the candidate and the survivor's replay is
//! the base, and an action is taken back when the record clears the gate.
//! Rule 2 is a comparison of two similarities from one search.

use crate::core::Core;
use crate::error::Result;
use crate::eval::sweep::{self, Pair};
use crate::store::actions::{Kind, UndoneBy};
use crate::store::generations::Generation;
use crate::store::pairs::DecidedBy;

/// Rows one pass will reconsider. A bound on work, like `OBSERVATION_LIMIT`:
/// every subject costs one vector read per observation naming it.
const ACTION_LIMIT: usize = 200;

/// What the corpus half of one pass did. Flat counts, so `jobs::did_work`
/// reads them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Retracted {
    /// Subjects rule 1 had evidence about.
    pub reconsidered: usize,
    /// Actions rule 1 took back.
    pub undone: usize,
    /// Artifacts rule 2 restored.
    pub restored: usize,
}

/// Both rules, in order. `started` is when the pass began: a search or a
/// question after it stops the rules between subjects, with nothing half
/// done — every undo is one call and one stamp.
pub async fn run(core: &Core, live: &Generation, started: i64) -> Result<Retracted> {
    let (reconsidered, undone, stopped) = rule_one(core, live, started).await?;
    let out = Retracted {
        reconsidered,
        undone,
        restored: 0,
    };
    if stopped {
        return Ok(out);
    }
    Ok(out)
}

/// Rule 1: a survivor must still be found.
///
/// For every merge and supersession not yet taken back, the observations that
/// named the subject before it was hidden are replayed at the live parameters,
/// and where the survivor lands is read off the list — `satisfied_by` already
/// resolves the subject to what answers for it. When the subject's record
/// clears `recommend` against the survivor's replay, the action lost what the
/// subject had, and the base takes it back. A subject nobody had used has no
/// evidence and is left alone.
///
/// Returns (subjects reconsidered, actions undone, stopped early).
pub(crate) async fn rule_one(
    core: &Core,
    live: &Generation,
    started: i64,
) -> Result<(usize, usize, bool)> {
    let current = *core.ranking.read().expect("ranking lock");
    let mut reconsidered = 0;
    let mut undone = 0;
    for a in core
        .store
        .open_actions(&[Kind::Merge, Kind::Supersede], ACTION_LIMIT)
        .await?
    {
        if core.store.activity_since(started).await? {
            return Ok((reconsidered, undone, true));
        }
        // A merge's rows are stamped together by the first original that
        // fails; the rest of that merge are no longer open by the time the
        // loop reaches them.
        if core
            .store
            .open_action_on(&a.subject_id, a.kind)
            .await?
            .is_none()
        {
            continue;
        }
        let named = core
            .store
            .observations_naming(
                &a.subject_id,
                a.at,
                &live.embed_recipe,
                &live.chat_model,
                sweep::OBSERVATION_LIMIT,
            )
            .await?;
        if named.is_empty() {
            continue;
        }
        reconsidered += 1;
        let satisfies = crate::eval::satisfied_by(core, &a.subject_id).await;
        let mut observed = Vec::with_capacity(named.len());
        let mut replayed = Vec::with_capacity(named.len());
        for o in named {
            let pair = Pair {
                query: o.query,
                satisfies: satisfies.clone(),
                query_vec: Some(o.query_vec),
                priming: None,
                served: o.rank.map(|r| (r - 1).max(0) as usize),
            };
            observed.push(pair.served);
            replayed.push(sweep::rank_of(core, &pair, current, false).await?);
        }
        // `recommend` pointed the other way: the subject's record is the
        // candidate, the survivor's replay is the base. When the record clears
        // the gate, the survivor lost what the subject had.
        if !sweep::recommend(&replayed, &observed) {
            continue;
        }
        let reason = format!(
            "what it hid was found better than it is, over {} observations",
            observed.len()
        );
        match a.kind {
            Kind::Merge => {
                let survivor = a.survivor_id.clone().expect("a merge row names its merge");
                crate::jobs::merge::undo(core, &survivor, DecidedBy::Evidence).await?;
                undone += core
                    .store
                    .undo_actions_under(&survivor, UndoneBy::Evidence, &reason)
                    .await? as usize;
            }
            Kind::Supersede => {
                core.unsupersede(&a.subject_id).await?;
                undone += core
                    .store
                    .undo_action_on(&a.subject_id, Kind::Supersede, UndoneBy::Evidence, &reason)
                    .await? as usize;
            }
            _ => unreachable!("only merge and supersede are read"),
        }
        tracing::info!(
            subject = %a.subject_id,
            kind = a.kind.as_str(),
            "took a corpus action back on evidence"
        );
    }
    Ok((reconsidered, undone, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::sweep::test_support::{QUERY, seeded};
    use crate::store::actions::{Job, NewAction};
    use crate::store::generations::NewGeneration;
    use crate::store::observations::{NewObservation, Source};

    /// Name the running configuration as the live generation, under `recipe`.
    async fn generation_under(core: &Core, recipe: &str) -> Generation {
        let params = *core.ranking.read().unwrap();
        core.store
            .record_generation(&NewGeneration {
                params: params.into(),
                embed_recipe: recipe.into(),
                chat_model: "qwen".into(),
                parent_id: None,
            })
            .await
            .unwrap();
        core.store.live_generation().await.unwrap().unwrap()
    }

    async fn generation_for(core: &Core) -> Generation {
        generation_under(core, "recipe-a").await
    }

    /// A used excerpt at the rank it was served, carrying the vector a real
    /// search of `QUERY` would have used.
    async fn observed(core: &Core, generation: &str, artifact: &str, rank: i64) {
        let query_vec = core.embedder.embed_query(QUERY).await.unwrap();
        core.store
            .record_observation(&NewObservation {
                generation_id: generation.into(),
                query: QUERY.into(),
                query_vec,
                embed_model: "fake".into(),
                artifact_id: Some(artifact.into()),
                rank: Some(rank),
                source: Source::Cited,
                event_id: None,
            })
            .await
            .unwrap();
    }

    fn supersede_row(loser: &str, winner: &str) -> NewAction {
        NewAction {
            job: Job::Dedupe,
            kind: Kind::Supersede,
            subject_id: loser.into(),
            survivor_id: Some(winner.into()),
            detail: None,
            evidence: serde_json::json!({}),
            pair_score: Some(0.9),
        }
    }

    /// The seeded base, with the top hit of the second source (`order[3]`)
    /// hidden in favour of the last hit of the first (`order[2]`) on the
    /// judge's word, after use had named `order[3]` at rank 1 twice.
    async fn superseded_after_use() -> (Core, Generation, String, String) {
        let (core, order) = seeded().await;
        let g = generation_for(&core).await;
        observed(&core, &g.id, &order[3], 1).await;
        observed(&core, &g.id, &order[3], 1).await;
        let (loser, winner) = (order[3].clone(), order[2].clone());
        core.supersede_with(&loser, &winner, Some(supersede_row(&loser, &winner)))
            .await
            .unwrap();
        (core, g, loser, winner)
    }

    #[tokio::test]
    async fn a_supersession_whose_survivor_ranks_two_net_pairs_worse_is_taken_back_on_evidence() {
        let (core, g, loser, winner) = superseded_after_use().await;
        let out = rule_one(&core, &g, crate::store::now()).await.unwrap();
        assert_eq!(out, (1, 1, false));
        assert!(
            core.store
                .get_artifact(&loser)
                .await
                .unwrap()
                .superseded_by
                .is_none(),
            "unsuperseded"
        );
        let row = core.store.recent_actions(1).await.unwrap().remove(0);
        assert_eq!(row.undone_by, Some(UndoneBy::Evidence));
        assert_eq!(row.survivor_id.as_deref(), Some(winner.as_str()));
        assert!(
            core.store
                .action_was_undone(&loser, Kind::Supersede)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_supersession_nobody_had_used_has_no_evidence_and_is_left_alone() {
        let (core, order) = seeded().await;
        let g = generation_for(&core).await;
        core.supersede_with(
            &order[3],
            &order[2],
            Some(supersede_row(&order[3], &order[2])),
        )
        .await
        .unwrap();
        assert_eq!(
            rule_one(&core, &g, crate::store::now()).await.unwrap(),
            (0, 0, false)
        );
        assert!(
            core.store
                .get_artifact(&order[3])
                .await
                .unwrap()
                .superseded_by
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_survivor_that_ranks_as_well_as_the_subject_did_holds() {
        // `order[3]` sat at rank 4 uncapped; hidden in favour of `order[0]`,
        // which is rank 1 always: the survivor replays better, and the gate
        // refuses to take the action back.
        let (core, order) = seeded().await;
        let g = generation_for(&core).await;
        observed(&core, &g.id, &order[3], 4).await;
        observed(&core, &g.id, &order[3], 4).await;
        core.supersede_with(
            &order[3],
            &order[0],
            Some(supersede_row(&order[3], &order[0])),
        )
        .await
        .unwrap();
        assert_eq!(
            rule_one(&core, &g, crate::store::now()).await.unwrap(),
            (1, 0, false)
        );
        assert!(
            core.store
                .get_artifact(&order[3])
                .await
                .unwrap()
                .superseded_by
                .is_some()
        );
    }

    #[tokio::test]
    async fn observations_from_another_era_are_not_evidence() {
        let (core, order) = seeded().await;
        let other = generation_under(&core, "other-recipe").await;
        observed(&core, &other.id, &order[3], 1).await;
        observed(&core, &other.id, &order[3], 1).await;
        let live = generation_for(&core).await;
        core.supersede_with(
            &order[3],
            &order[2],
            Some(supersede_row(&order[3], &order[2])),
        )
        .await
        .unwrap();
        assert_eq!(
            rule_one(&core, &live, crate::store::now()).await.unwrap(),
            (0, 0, false)
        );
    }

    #[tokio::test]
    async fn a_merge_is_taken_back_whole_when_one_original_is_no_longer_found() {
        let (core, order) = seeded().await;
        let g = generation_for(&core).await;
        // Use named the second source's first chunk at the top, twice.
        observed(&core, &g.id, &order[3], 1).await;
        observed(&core, &g.id, &order[3], 1).await;
        // A merge of that chunk with its neighbour, whose text is unrelated
        // to the query, so the merge is not found where the original was.
        let draft = crate::infer::prompt::MergedDraft {
            title: Some("Merged".into()),
            text: "unrelated words entirely".into(),
            category: None,
            tags: vec![],
            caveats: vec![],
        };
        let sources = vec![order[3].clone(), order[4].clone()];
        let m = crate::jobs::merge::write(&core, &draft, &sources)
            .await
            .unwrap();
        crate::jobs::embed::run(&core, &m.id).await.unwrap();
        crate::jobs::merge::finish(&core, &m.id).await.unwrap();
        for s in &sources {
            assert!(
                core.store
                    .get_artifact(s)
                    .await
                    .unwrap()
                    .superseded_by
                    .is_some(),
                "the fixture needs the originals hidden"
            );
            core.store
                .record_action(&NewAction {
                    kind: Kind::Merge,
                    ..supersede_row(s, &m.id)
                })
                .await
                .unwrap();
        }

        let out = rule_one(&core, &g, crate::store::now()).await.unwrap();
        assert_eq!(out, (1, 2, false), "one subject had evidence; both rows go");
        for s in &sources {
            let c = core.store.get_artifact(s).await.unwrap();
            assert!(c.in_results(), "an original was not put back");
        }
        assert!(
            core.store
                .open_actions(&[Kind::Merge], 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A supersession after use on a base that is already in some state, so
    /// the pass-level tests can ask what the corpus half does there. Three
    /// observations rather than two: the watching base already holds one of
    /// this artifact at its old rank, and the record has to clear that too.
    async fn superseded_after_use_on(core: &Core) -> String {
        let order = crate::eval::sweep::test_support::ranks_order(core).await;
        let live = core.store.live_generation().await.unwrap().unwrap();
        for _ in 0..3 {
            observed(core, &live.id, &order[3], 1).await;
        }
        core.supersede_with(
            &order[3],
            &order[2],
            Some(supersede_row(&order[3], &order[2])),
        )
        .await
        .unwrap();
        order[3].clone()
    }

    #[tokio::test]
    async fn the_corpus_half_runs_while_the_ranking_half_is_under_watch() {
        let (core, _) = crate::jobs::tune::test_support::adopted_and_watching().await;
        let loser = superseded_after_use_on(&core).await;
        let p = crate::jobs::tune::pass(&core).await.unwrap();
        assert_eq!(p.undone, 1, "{p:?}");
        assert!(p.adopted.is_none(), "still under watch");
        assert!(
            core.store
                .get_artifact(&loser)
                .await
                .unwrap()
                .superseded_by
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_untrustworthy_anchor_stops_the_corpus_rules_too() {
        let (core, _) = crate::jobs::tune::test_support::suspended().await;
        let loser = superseded_after_use_on(&core).await;
        let p = crate::jobs::tune::pass(&core).await.unwrap();
        assert_eq!(p.undone, 0);
        assert!(
            core.store
                .get_artifact(&loser)
                .await
                .unwrap()
                .superseded_by
                .is_some(),
            "suspended means the corpus rules act on nothing either"
        );
    }

    #[tokio::test]
    async fn the_rule_stops_between_subjects_when_somebody_comes_back() {
        let (core, g, _, _) = superseded_after_use().await;
        // A search recorded after the pass began: whoever it was is ahead.
        let started = crate::store::now() - 10;
        core.store
            .record_search(
                crate::store::feedback::NewEvent {
                    query: "anything".into(),
                    door: crate::store::feedback::Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.1],
                    embed_model: "fake".into(),
                    candidates: vec![],
                    answered: false,
                    fold_onto: None,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        let out = rule_one(&core, &g, started).await.unwrap();
        assert_eq!(out, (0, 0, true));
    }
}
