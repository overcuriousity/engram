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
    let mut out = Retracted {
        reconsidered,
        undone,
        restored: 0,
    };
    if stopped {
        return Ok(out);
    }
    let (restored, _) = rule_two(core, started).await?;
    out.restored = restored;
    let last = serde_json::json!({
        "at": crate::store::now(),
        "reconsidered": out.reconsidered,
        "undone": out.undone,
        "restored": out.restored,
    });
    core.store.meta_set(LAST_RUN, &last.to_string()).await?;
    Ok(out)
}

/// Where rule 2 has read the give-ups up to: a `created_at`, in `meta`.
const GAVE_UP_AFTER: &str = "evolve.retract.gave_up_after";

/// What the rules did the last time they ran, as JSON in `meta`, for the
/// page: `Retracted` plus `at`.
pub const LAST_RUN: &str = "evolve.retract.last";

/// Rule 2: a give-up that a hidden artifact would have answered.
///
/// Every give-up since the last pass is searched once more with hidden hits
/// included, and the graveyard is compared by cosine over what was buried by
/// the same model. When the best hidden hit the base itself hid is more
/// similar than the best live hit, the hiding cost an answer, and the base
/// restores it through the method the operator's button calls. An artifact a
/// person hid has no row and is not the base's to restore.
///
/// Returns (artifacts restored, stopped early).
pub(crate) async fn rule_two(core: &Core, started: i64) -> Result<(usize, bool)> {
    use crate::store::artifacts::ArtifactStatus;
    let current = *core.ranking.read().expect("ranking lock");
    let after: i64 = core
        .store
        .meta_get(GAVE_UP_AFTER)
        .await?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut restored = 0;
    let mut cursor = after;
    for o in core
        .store
        .gave_ups_since(after, sweep::OBSERVATION_LIMIT)
        .await?
    {
        if core.store.activity_since(started).await? {
            core.store
                .meta_set(GAVE_UP_AFTER, &cursor.to_string())
                .await?;
            return Ok((restored, true));
        }
        cursor = o.created_at;
        core.remember_query_vector(&o.query, o.query_vec.clone());
        let q = crate::core::search::SearchQuery {
            q: o.query.clone(),
            limit: sweep::LIMIT,
            tags: vec![],
            category: None,
            mark: false,
            rerank: false,
            explain: false,
            include_deprecated: true,
            include_superseded: true,
        };
        let (hits, _) = core
            .search_with_ranking(&q, current, crate::store::feedback::Door::Judge)
            .await?;
        let is_hidden = |h: &crate::core::search::SearchResult| {
            h.superseded_by.is_some() || h.status.is_some_and(|s| s != ArtifactStatus::Active)
        };
        let best_live = hits
            .iter()
            .filter(|h| !is_hidden(h))
            .filter_map(|h| h.similarity)
            .fold(0.0f32, f32::max);
        // The best hidden hit the base itself hid, with the row that says so.
        let mut best_hidden: Option<(f32, crate::store::actions::Action)> = None;
        for h in hits.iter().filter(|h| is_hidden(h)) {
            let Some(sim) = h.similarity else { continue };
            if sim <= best_live || best_hidden.as_ref().is_some_and(|(b, _)| sim <= *b) {
                continue;
            }
            for kind in [Kind::Discard, Kind::Supersede, Kind::Merge] {
                if let Some(a) = core.store.open_action_on(&h.artifact_id, kind).await? {
                    best_hidden = Some((sim, a));
                    break;
                }
            }
        }
        // And the graveyard, by cosine over what was buried by the same model.
        for (id, vec) in core.store.graveyard_vectors(&o.embed_model).await? {
            let sim = crate::vector::cosine(&o.query_vec, &vec);
            if sim <= best_live || best_hidden.as_ref().is_some_and(|(b, _)| sim <= *b) {
                continue;
            }
            if let Some(a) = core.store.open_action_on(&id, Kind::Reap).await? {
                best_hidden = Some((sim, a));
            }
        }
        let Some((sim, a)) = best_hidden else {
            continue;
        };
        let reason = format!(
            "a search given up on would have been answered by it (cosine {sim:.2} against {best_live:.2} live)"
        );
        match a.kind {
            Kind::Discard | Kind::Reap => {
                core.reactivate(&a.subject_id).await?;
                core.store
                    .undo_action_on(&a.subject_id, a.kind, UndoneBy::Evidence, &reason)
                    .await?;
            }
            Kind::Supersede => {
                core.unsupersede(&a.subject_id).await?;
                core.store
                    .undo_action_on(&a.subject_id, Kind::Supersede, UndoneBy::Evidence, &reason)
                    .await?;
            }
            Kind::Merge => {
                let survivor = a.survivor_id.clone().expect("a merge row names its merge");
                crate::jobs::merge::undo(core, &survivor, DecidedBy::Evidence).await?;
                core.store
                    .undo_actions_under(&survivor, UndoneBy::Evidence, &reason)
                    .await?;
            }
            Kind::Promote | Kind::Moment => continue,
        }
        restored += 1;
        tracing::info!(
            subject = %a.subject_id,
            kind = a.kind.as_str(),
            sim,
            best_live,
            "restored what a search given up on would have been answered by"
        );
    }
    core.store
        .meta_set(GAVE_UP_AFTER, &cursor.to_string())
        .await?;
    Ok((restored, false))
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

    /// A give-up on `QUERY` under `generation`.
    async fn gave_up(core: &Core, generation: &str) {
        let query_vec = core.embedder.embed_query(QUERY).await.unwrap();
        core.store
            .record_observation(&NewObservation {
                generation_id: generation.into(),
                query: QUERY.into(),
                query_vec,
                embed_model: core.embedder.model().to_string(),
                artifact_id: None,
                rank: None,
                source: Source::GaveUp,
                event_id: None,
            })
            .await
            .unwrap();
    }

    /// One note that is the answer to `QUERY` and one that is not, each its
    /// own source: the answer has no twin, so hiding it is a loss the base can
    /// see. `seeded()` cannot serve here — its three chunks of one source are
    /// identical, and a hidden hit with a live twin at the same similarity is
    /// no loss at all.
    async fn one_answer() -> (Core, String, String) {
        let core = crate::core::test_support::test_core().await;
        let mut ids = Vec::new();
        for (raw, text) in [("answer", QUERY), ("other", "unrelated words")] {
            let src = core.store.insert_corpus(raw, "web", None).await.unwrap();
            let new = vec![crate::store::artifacts::NewArtifact {
                ordinal: 0,
                text: text.to_string(),
                corpus_span: None,
                title: None,
                category: None,
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
            }];
            for c in core.store.insert_artifacts(&src.id, &new).await.unwrap() {
                crate::jobs::embed::run(&core, &c.id).await.unwrap();
                ids.push(c.id);
            }
        }
        let (answer, other) = (ids.remove(0), ids.remove(0));
        (core, answer, other)
    }

    fn discard_row(subject: &str) -> NewAction {
        NewAction {
            kind: Kind::Discard,
            survivor_id: None,
            ..supersede_row(subject, "")
        }
    }

    #[tokio::test]
    async fn a_give_up_that_a_discarded_artifact_would_have_topped_restores_it() {
        let (core, answer, _) = one_answer().await;
        let g = generation_for(&core).await;
        // The answer, discarded on the judge's word.
        core.deprecate_with(&answer, Some(discard_row(&answer)))
            .await
            .unwrap();
        gave_up(&core, &g.id).await;

        let out = rule_two(&core, crate::store::now()).await.unwrap();
        assert_eq!(out, (1, false));
        assert!(core.store.get_artifact(&answer).await.unwrap().in_results());
        assert!(
            core.store
                .action_was_undone(&answer, Kind::Discard)
                .await
                .unwrap()
        );
        let row = core.store.recent_actions(1).await.unwrap().remove(0);
        assert_eq!(row.undone_by, Some(UndoneBy::Evidence));
    }

    #[tokio::test]
    async fn a_give_up_the_live_list_answers_better_restores_nothing() {
        let (core, _, other) = one_answer().await;
        let g = generation_for(&core).await;
        // The unrelated note, discarded: the live answer is closer.
        core.deprecate_with(&other, Some(discard_row(&other)))
            .await
            .unwrap();
        gave_up(&core, &g.id).await;

        assert_eq!(
            rule_two(&core, crate::store::now()).await.unwrap(),
            (0, false)
        );
        assert!(!core.store.get_artifact(&other).await.unwrap().in_results());
    }

    #[tokio::test]
    async fn an_artifact_a_person_hid_is_not_the_base_s_to_restore() {
        let (core, answer, _) = one_answer().await;
        let g = generation_for(&core).await;
        core.deprecate(&answer).await.unwrap();
        gave_up(&core, &g.id).await;

        assert_eq!(
            rule_two(&core, crate::store::now()).await.unwrap(),
            (0, false)
        );
        assert!(!core.store.get_artifact(&answer).await.unwrap().in_results());
    }

    /// Bury `id` the way reap does, with its vector and a journal row.
    async fn buried(core: &Core, id: &str, embed_model: &str) {
        core.store
            .set_artifact_status(id, crate::store::artifacts::ArtifactStatus::Deprecated)
            .await
            .unwrap();
        sqlx::query("UPDATE artifacts SET retired_at = ? WHERE id = ?")
            .bind(crate::store::now() - 400 * 86_400)
            .bind(id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        let dense = core.vectors.dense_of(id).await.unwrap().expect("a point");
        core.store
            .bury(
                id,
                r#"{"reason":"covered"}"#,
                0,
                Some(&dense),
                Some(embed_model),
                &crate::jobs::reap::test_support::row(id),
            )
            .await
            .unwrap();
        core.vectors
            .delete_artifacts(std::slice::from_ref(&id.to_string()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_buried_artifact_is_exhumed_by_cosine_and_re_embedded() {
        let (core, answer, _) = one_answer().await;
        let g = generation_for(&core).await;
        let model = core.embedder.model().to_string();
        buried(&core, &answer, &model).await;
        assert!(
            core.store
                .get_artifact(&answer)
                .await
                .unwrap()
                .text
                .is_empty()
        );
        gave_up(&core, &g.id).await;

        let out = rule_two(&core, crate::store::now()).await.unwrap();
        assert_eq!(out, (1, false));
        let row = core.store.get_artifact(&answer).await.unwrap();
        assert!(row.reaped_at.is_none());
        assert_eq!(row.text, QUERY, "the text came back out of the grave");
        assert!(row.in_results());
        assert!(core.store.graveyard_row(&answer).await.unwrap().is_none());
        assert!(
            core.store
                .action_was_undone(&answer, Kind::Reap)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_buried_vector_from_another_model_is_not_compared() {
        let (core, answer, _) = one_answer().await;
        let g = generation_for(&core).await;
        buried(&core, &answer, "some-other-model").await;
        gave_up(&core, &g.id).await;

        assert_eq!(
            rule_two(&core, crate::store::now()).await.unwrap(),
            (0, false)
        );
        assert!(
            core.store
                .get_artifact(&answer)
                .await
                .unwrap()
                .reaped_at
                .is_some()
        );
    }

    #[tokio::test]
    async fn give_ups_are_read_once_and_the_cursor_moves() {
        let (core, answer, _) = one_answer().await;
        let g = generation_for(&core).await;
        gave_up(&core, &g.id).await;
        assert_eq!(
            rule_two(&core, crate::store::now()).await.unwrap(),
            (0, false)
        );
        // Now hide the answer: the give-up already read is not read again,
        // so nothing is restored until a new give-up arrives.
        core.deprecate_with(&answer, Some(discard_row(&answer)))
            .await
            .unwrap();
        assert_eq!(
            rule_two(&core, crate::store::now()).await.unwrap(),
            (0, false)
        );
        assert!(!core.store.get_artifact(&answer).await.unwrap().in_results());
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
