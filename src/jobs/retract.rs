//! The corpus jobs answer to the ranking's evidence: rule 1 takes back a merge or supersession
//! whose survivor is no longer found; rule 2 restores what a given-up search would have found.

use crate::core::Core;
use crate::error::Result;
use crate::eval::sweep::{self, Pair};
use crate::store::actions::{Kind, UndoneBy};
use crate::store::generations::Generation;
use crate::store::pairs::DecidedBy;

/// A bound on work: every subject costs one vector read per observation naming it.
const ACTION_LIMIT: usize = 200;

/// A give-up's live best is usually weak, and a superseded near-duplicate sits within a hundredth
/// of its survivor, so "any better" restores on a coin toss. A restore permanently ends `dedupe`'s
/// automatic decision on the pair.
const RESTORE_MARGIN: f32 = 0.05;

/// Flat counts, so `jobs::did_work` reads them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Retracted {
    pub reconsidered: usize,
    pub undone: usize,
    pub restored: usize,
    /// Counted only: `sleep::interference` files nothing.
    pub interference: usize,
    pub condensed: usize,
}

/// `started` is when the pass began: a search after it stops the rules between subjects, nothing
/// half done.
pub async fn run(core: &Core, live: &Generation, started: i64) -> Result<Retracted> {
    let (reconsidered, undone, stopped) = rule_one(core, live, started).await?;
    let mut out = Retracted {
        reconsidered,
        undone,
        restored: 0,
        interference: 0,
        condensed: 0,
    };
    if stopped {
        return Ok(out);
    }
    let (restored, stopped) = rule_two(core, started).await?;
    out.restored = restored;
    if stopped {
        return Ok(out);
    }
    let (interference, stopped) = crate::jobs::sleep::interference(core, live, started).await?;
    out.interference = interference;
    if stopped {
        return Ok(out);
    }
    let (condensed, _) = crate::jobs::sleep::condense_candidates(core, live, started).await?;
    out.condensed = condensed;
    let last = serde_json::json!({
        "at": crate::store::now(),
        "reconsidered": out.reconsidered,
        "undone": out.undone,
        "restored": out.restored,
    });
    core.store.meta_set(LAST_RUN, &last.to_string()).await?;
    Ok(out)
}

const GAVE_UP_AFTER: &str = "evolve.retract.gave_up_after";

/// Wraps rather than only advancing: an open action stays open while the evidence agrees, so the
/// table has no end.
const ACTED_AFTER: &str = "evolve.retract.acted_after";

/// `Retracted` plus `at`, for the page.
pub const LAST_RUN: &str = "evolve.retract.last";

/// Restores through the operator's own method when the best hidden hit the base hid clears
/// `weak_below` and `RESTORE_MARGIN` over the best live hit. Returns (restored, stopped early).
pub(crate) async fn rule_two(core: &Core, started: i64) -> Result<(usize, bool)> {
    use crate::store::artifacts::ArtifactStatus;
    let current = *core
        .ranking
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let after = core
        .store
        .meta_get(GAVE_UP_AFTER)
        .await?
        .map(|s| crate::store::Cursor::parse(&s))
        .unwrap_or_default();
    let mut restored = 0;
    let mut cursor = after.clone();
    let live_model = core.embedder.model().to_string();
    // Read lazily, once per pass: every compared observation is under `live_model`, so the
    // graveyard need not be decoded per give-up.
    let mut buried: Option<Vec<(String, Vec<f32>)>> = None;
    for o in core
        .store
        .gave_ups_since(&after, sweep::OBSERVATION_LIMIT)
        .await?
    {
        if core.store.activity_since(started).await? {
            core.store.meta_set(GAVE_UP_AFTER, &cursor.encode()).await?;
            return Ok((restored, true));
        }
        cursor = crate::store::Cursor {
            at: o.created_at,
            id: o.id.clone(),
        };
        // Both sides of the comparison must come from one vector space.
        if o.embed_model != live_model {
            continue;
        }
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
        // With no live hit there is nothing to beat, and a fold from zero accepts nearly any grave:
        // that is a gap.
        let Some(best_live) = hits
            .iter()
            .filter(|h| !is_hidden(h))
            .filter_map(|h| h.similarity)
            .reduce(f32::max)
        else {
            continue;
        };
        // Two bars: the base's measured relevance line, and the margin over the live best.
        let bar = (best_live + RESTORE_MARGIN).max(core.weak_below());
        let mut best_hidden: Option<(f32, crate::store::actions::Action)> = None;
        for h in hits.iter().filter(|h| is_hidden(h)) {
            let Some(sim) = h.similarity else { continue };
            if sim < bar || best_hidden.as_ref().is_some_and(|(b, _)| sim <= *b) {
                continue;
            }
            for kind in [Kind::Discard, Kind::Supersede, Kind::Merge] {
                if let Some(a) = core.store.open_action_on(&h.artifact_id, kind).await? {
                    best_hidden = Some((sim, a));
                    break;
                }
            }
        }
        if buried.is_none() {
            buried = Some(core.store.graveyard_vectors(&live_model).await?);
        }
        for (id, vec) in buried.iter().flatten() {
            let sim = crate::vector::cosine(&o.query_vec, vec);
            if sim < bar || best_hidden.as_ref().is_some_and(|(b, _)| sim <= *b) {
                continue;
            }
            if let Some(a) = core.store.open_action_on(id, Kind::Reap).await? {
                best_hidden = Some((sim, a));
            }
        }
        let Some((sim, a)) = best_hidden else {
            continue;
        };
        let reason = format!(
            "a search given up on would have been answered by it (cosine {sim:.2} against {best_live:.2} live)"
        );
        // The status cannot tell an operator's Hide from a judged discard; the journal can, since
        // only `deprecate_with` writes a `Kind::Discard` row. A person's decision is not overruled
        // by an unanswered search.
        if a.kind == Kind::Reap
            && let Ok(c) = core.store.get_artifact(&a.subject_id).await
            && c.status == crate::store::artifacts::ArtifactStatus::Deprecated
            && core
                .store
                .open_action_on(&a.subject_id, Kind::Discard)
                .await?
                .is_none()
        {
            tracing::info!(
                subject = %a.subject_id,
                "a search given up on would have been answered by an artifact somebody hid \
                 by hand before it was buried; leaving that decision alone"
            );
            continue;
        }
        let restore = match a.kind {
            Kind::Discard | Kind::Reap => core.reactivate(&a.subject_id).await,
            Kind::Supersede => core.unsupersede(&a.subject_id).await,
            _ => Ok(()),
        };
        // Skipped, not raised: every `?` here returns before the cursor is stamped, so one gone row
        // would wedge the rule forever.
        if let Err(crate::error::Error::NotFound) = restore {
            tracing::info!(
                subject = %a.subject_id,
                kind = a.kind.as_str(),
                "the artifact this would have put back is gone; leaving the row alone"
            );
            continue;
        }
        // Same reason: one undo that cannot be carried out must not wedge the pass.
        if let Err(e) = restore {
            tracing::warn!(
                subject = %a.subject_id,
                kind = a.kind.as_str(),
                error = %e,
                "could not restore what a give-up would have been answered by; moving on"
            );
            continue;
        }
        match stamp_restored(core, &a, &reason).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                tracing::warn!(
                    subject = %a.subject_id,
                    kind = a.kind.as_str(),
                    error = %e,
                    "restored an artifact but could not stamp the journal; moving on"
                );
                continue;
            }
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
    core.store.meta_set(GAVE_UP_AFTER, &cursor.encode()).await?;
    Ok((restored, false))
}

/// Also undoes the merge behind a `Merge` row. `false` for a kind this rule does not take back.
async fn stamp_restored(
    core: &Core,
    a: &crate::store::actions::Action,
    reason: &str,
) -> Result<bool> {
    match a.kind {
        Kind::Discard | Kind::Reap => {
            core.store
                .undo_action_on(&a.subject_id, a.kind, UndoneBy::Evidence, reason)
                .await?;
        }
        Kind::Supersede => {
            core.store
                .undo_action_on(&a.subject_id, Kind::Supersede, UndoneBy::Evidence, reason)
                .await?;
        }
        Kind::Merge => {
            let survivor = a.survivor_id.clone().ok_or_else(|| {
                crate::error::Error::Internal("a merge row without its merge".into())
            })?;
            // Stamped only if it happened: closing rows on an undo that did nothing puts the merge
            // out of rule 1's reach (`merge::Undone`).
            if !crate::jobs::merge::undo(core, &survivor, DecidedBy::Evidence)
                .await?
                .happened()
            {
                return Ok(false);
            }
            core.store
                .undo_actions_under(&survivor, UndoneBy::Evidence, reason)
                .await?;
        }
        Kind::Promote | Kind::Moment | Kind::Condense => return Ok(false),
    }
    Ok(true)
}

/// Split out so the caller can step over a failure. Returns the rows stamped.
async fn take_back(core: &Core, a: &crate::store::actions::Action, reason: &str) -> Result<usize> {
    match a.kind {
        Kind::Merge => {
            let survivor = a.survivor_id.clone().ok_or_else(|| {
                crate::error::Error::Internal("a merge row without its merge".into())
            })?;
            // Zero rather than an error: an undo that did nothing is ordinary here.
            if !crate::jobs::merge::undo(core, &survivor, DecidedBy::Evidence)
                .await?
                .happened()
            {
                return Ok(0);
            }
            Ok(core
                .store
                .undo_actions_under(&survivor, UndoneBy::Evidence, reason)
                .await? as usize)
        }
        Kind::Supersede => {
            core.unsupersede(&a.subject_id).await?;
            Ok(core
                .store
                .undo_action_on(&a.subject_id, Kind::Supersede, UndoneBy::Evidence, reason)
                .await? as usize)
        }
        _ => unreachable!("only merge and supersede reach here"),
    }
}

/// Results across a generation boundary compare ranking parameters, not the artifact, so both sides
/// come from one generation.
fn spanning(
    results: &[crate::store::rehearsals::RehearsalResult],
    at: i64,
) -> Option<(
    &crate::store::rehearsals::RehearsalResult,
    &crate::store::rehearsals::RehearsalResult,
)> {
    results
        .iter()
        .filter(|t| t.at > at)
        .filter_map(|t| {
            results
                .iter()
                .filter(|b| b.at <= at && b.generation_id == t.generation_id)
                .max_by_key(|b| b.at)
                .map(|b| (b, t))
        })
        .max_by_key(|(_, t)| t.at)
}

/// A survivor must still be found: observations naming the subject before it was hidden are
/// replayed at the live parameters, and `recommend` decides. Reads `ACTION_LIMIT` rows from the
/// cursor, wrapping, so later rows are reached. Returns (reconsidered, undone, stopped early).
pub(crate) async fn rule_one(
    core: &Core,
    live: &Generation,
    started: i64,
) -> Result<(usize, usize, bool)> {
    let current = *core
        .ranking
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut reconsidered = 0;
    let mut undone = 0;
    let after = core
        .store
        .meta_get(ACTED_AFTER)
        .await?
        .map(|s| crate::store::Cursor::parse(&s))
        .unwrap_or_default();
    let kinds = [Kind::Merge, Kind::Supersede, Kind::Condense];
    let mut batch = core
        .store
        .open_actions_after(&kinds, &after, ACTION_LIMIT)
        .await?;
    // The end of a lap: start over, as the rows before the cursor are open still.
    if batch.is_empty() && after != crate::store::Cursor::default() {
        batch = core
            .store
            .open_actions_after(&kinds, &crate::store::Cursor::default(), ACTION_LIMIT)
            .await?;
    }
    let mut cursor = after;
    for a in batch {
        if core.store.activity_since(started).await? {
            core.store.meta_set(ACTED_AFTER, &cursor.encode()).await?;
            return Ok((reconsidered, undone, true));
        }
        cursor = crate::store::Cursor {
            at: a.at,
            id: a.id.clone(),
        };
        // A merge's rows are stamped together by the first original that fails.
        if core
            .store
            .open_action_on(&a.subject_id, a.kind)
            .await?
            .is_none()
        {
            continue;
        }
        // Same `recommend` shape: the probes' record before is the candidate, the replay after is
        // the base.
        if a.kind == Kind::Condense {
            // `recommend` reads index *i* on both sides as the same query, so pairing is per probe
            // (see `spanning`); a probe missing either side sits out.
            let mut before = Vec::new();
            let mut after = Vec::new();
            for p in core.store.rehearsals_of(&a.subject_id).await? {
                let results = core
                    .store
                    .results_of(&p.id, sweep::OBSERVATION_LIMIT)
                    .await?;
                let rank = |r: &crate::store::rehearsals::RehearsalResult| {
                    r.rank.map(|n| (n - 1).max(0) as usize)
                };
                if let Some((b, t)) = spanning(&results, a.at) {
                    before.push(rank(b));
                    after.push(rank(t));
                }
            }
            if before.is_empty() {
                continue;
            }
            reconsidered += 1;
            if !sweep::recommend(&after, &before) {
                continue;
            }
            core.uncondense(&a.id, UndoneBy::Evidence).await?;
            undone += 1;
            tracing::info!(
                subject = %a.subject_id,
                "took a condensation back on evidence: its probes stopped finding it"
            );
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
                served_rank: o.rank,
                // Rule 1 reads `served_rank` and never the rerank axis.
                served_reranked: false,
                // A person's query, not an artifact's text.
                exclude: Vec::new(),
            };
            // Measured where its replay is measured (`sweep::served_at`).
            observed.push(pair.served());
            replayed.push(sweep::rank_of(core, &pair, current, false).await?);
        }
        if !sweep::recommend(&replayed, &observed) {
            continue;
        }
        let reason = format!(
            "what it hid was found better than it is, over {} observations",
            observed.len()
        );
        // Stepped over, not raised: one failing undo would otherwise block the rest of the idle
        // pass forever. Left unstamped, so the next lap retries it.
        match take_back(core, &a, &reason).await {
            Ok(n) => undone += n,
            Err(e) => {
                tracing::warn!(
                    subject = %a.subject_id,
                    kind = a.kind.as_str(),
                    error = %e,
                    "could not take a corpus action back; leaving the row open and moving on"
                );
                continue;
            }
        }
        tracing::info!(
            subject = %a.subject_id,
            kind = a.kind.as_str(),
            "took a corpus action back on evidence"
        );
    }
    core.store.meta_set(ACTED_AFTER, &cursor.encode()).await?;
    Ok((reconsidered, undone, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::sweep::test_support::{QUERY, seeded};
    use crate::store::actions::{Job, NewAction};
    use crate::store::generations::NewGeneration;
    use crate::store::observations::{NewObservation, Source};

    async fn generation_under(core: &Core, recipe: &str) -> Generation {
        let params = *core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        core.store
            .record_generation(&NewGeneration {
                params: params.into(),
                embed_recipe: recipe.into(),
                chat_model: "qwen".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        core.store.live_generation().await.unwrap().unwrap()
    }

    async fn generation_for(core: &Core) -> Generation {
        generation_under(core, "recipe-a").await
    }

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

    #[tokio::test]
    async fn rule_one_takes_a_condensation_back_when_its_probes_stop_finding_it() {
        let (mut core, order) = seeded().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let g = generation_for(&core).await;
        let id = order[0].clone();
        let before = core.store.get_artifact(&id).await.unwrap();
        // `recommend` counts pairs, and three results of one query are one pair.
        let probes = probes_on(&core, &id, &g, 3).await;
        let (action, _) = core
            .store
            .condense_artifact(&id, None, "shorter", None, &[], serde_json::json!({}))
            .await
            .unwrap()
            .expect("no revision was pinned");
        let at = core.store.action(&action).await.unwrap().unwrap().at;
        for p in &probes {
            result_at(&core, p, &g, Some(1), at - 10).await;
            result_at(&core, p, &g, None, at + 10).await;
        }

        let (reconsidered, undone, _) = rule_one(&core, &g, crate::store::now()).await.unwrap();
        assert_eq!((reconsidered, undone), (1, 1));
        let back = core.store.get_artifact(&id).await.unwrap();
        assert_eq!(back.text, before.text);
        assert!(
            core.store
                .action_was_undone(&id, Kind::Condense)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_condensation_is_judged_on_probes_that_span_it_and_no_others() {
        let (mut core, order) = seeded().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let g = generation_for(&core).await;
        let id = order[0].clone();
        let text = core.store.get_artifact(&id).await.unwrap().text;
        let old_probes = probes_on(&core, &id, &g, 4).await;
        let (action, _) = core
            .store
            .condense_artifact(&id, None, "shorter", None, &[], serde_json::json!({}))
            .await
            .unwrap()
            .expect("no revision was pinned");
        let at = core.store.action(&action).await.unwrap().unwrap().at;
        for p in &old_probes {
            result_at(&core, p, &g, Some(1), at - 10).await;
            core.store.retire_rehearsal(p, at).await.unwrap();
        }
        for p in &probes_on(&core, &id, &g, 4).await {
            result_at(&core, p, &g, Some(5), at + 10).await;
        }

        let (reconsidered, undone, _) = rule_one(&core, &g, crate::store::now()).await.unwrap();
        assert_eq!(
            (reconsidered, undone),
            (0, 0),
            "no probe spans the condensation, so there is nothing to compare"
        );
        assert_ne!(
            core.store.get_artifact(&id).await.unwrap().text,
            text,
            "the condensation stands"
        );
        assert!(
            !core
                .store
                .action_was_undone(&id, Kind::Condense)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_condensation_is_not_judged_across_a_generation_boundary() {
        let (mut core, order) = seeded().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let g = generation_for(&core).await;
        let id = order[0].clone();
        let text = core.store.get_artifact(&id).await.unwrap().text;
        let probes = probes_on(&core, &id, &g, 3).await;
        let (action, _) = core
            .store
            .condense_artifact(&id, None, "shorter", None, &[], serde_json::json!({}))
            .await
            .unwrap()
            .expect("no revision was pinned");
        let at = core.store.action(&action).await.unwrap().unwrap().at;
        for p in &probes {
            result_at(&core, p, &g, Some(1), at - 10).await;
        }
        let adopted = generation_under(&core, "recipe-a").await;
        for p in &probes {
            result_at(&core, p, &adopted, None, at + 10).await;
        }

        let (reconsidered, undone, _) = rule_one(&core, &adopted, crate::store::now())
            .await
            .unwrap();
        assert_eq!(
            (reconsidered, undone),
            (0, 0),
            "a rank under one generation was compared with a rank under another"
        );
        assert_ne!(
            core.store.get_artifact(&id).await.unwrap().text,
            text,
            "the condensation stands"
        );
    }

    async fn probes_on(core: &Core, artifact: &str, g: &Generation, n: usize) -> Vec<String> {
        let query_vec = core.embedder.embed_query(QUERY).await.unwrap();
        let mut out = Vec::new();
        for i in 0..n {
            out.push(
                core.store
                    .record_rehearsal(&crate::store::rehearsals::NewRehearsal {
                        class: crate::store::rehearsals::Class::Cue,
                        query: format!("q{i}-{}", g.id),
                        query_vec: query_vec.clone(),
                        embed_model: core.embedder.model().to_string(),
                        artifact_id: artifact.to_string(),
                        source_id: None,
                    })
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        out
    }

    async fn result_at(core: &Core, probe: &str, g: &Generation, rank: Option<i64>, at: i64) {
        let rid = core
            .store
            .record_rehearsal_result(&crate::store::rehearsals::NewResult {
                rehearsal_id: probe.to_string(),
                generation_id: g.id.clone(),
                rank,
                outranked_by: vec![],
            })
            .await
            .unwrap();
        sqlx::query!("UPDATE rehearsal_results SET at = ? WHERE id = ?", at, &rid)
            .execute(&core.store.pool)
            .await
            .unwrap();
    }

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
    async fn the_rule_gets_back_to_an_action_it_has_already_walked_past() {
        // An open row must come round again: neither stuck on the oldest rows nor passed for good.
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
        let cursor = core.store.meta_get(ACTED_AFTER).await.unwrap().unwrap();
        assert_ne!(
            crate::store::Cursor::parse(&cursor),
            crate::store::Cursor::default(),
            "the pass left a position behind"
        );
        assert_eq!(
            rule_one(&core, &g, crate::store::now()).await.unwrap(),
            (1, 0, false),
            "the lap ended, so it starts over"
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
        observed(&core, &g.id, &order[3], 1).await;
        observed(&core, &g.id, &order[3], 1).await;
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
    async fn learning_curates_the_corpus_and_holds_ranking_still() {
        let (mut core, _) = crate::jobs::tune::test_support::adopted_and_watching().await;
        crate::jobs::tune::test_support::observe_badly_under_live(&core, 16).await;
        let before = core.store.live_generation().await.unwrap().unwrap().id;
        core.learn.mode = crate::config::LearnMode::Learning;
        let loser = superseded_after_use_on(&core).await;

        let p = crate::jobs::tune::pass(&core).await.unwrap();

        assert_eq!(p.undone, 1, "the corpus half did not curate: {p:?}");
        assert!(
            core.store
                .get_artifact(&loser)
                .await
                .unwrap()
                .superseded_by
                .is_none()
        );
        assert!(
            p.adopted.is_none() && p.reverted.is_none(),
            "learning moved ranking: {p:?}"
        );
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            before,
            "ranking moved under learning"
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

    async fn one_answer() -> (Core, String, String) {
        let core = crate::core::test_support::test_core().await;
        let mut ids = Vec::new();
        for (raw, text) in [("answer", QUERY), ("other", "unrelated words")] {
            let src = core.store.insert_corpus(raw, "web", None).await.unwrap();
            let new = vec![crate::store::artifacts::NewArtifact {
                text: text.to_string(),
                ..Default::default()
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
    async fn a_give_up_with_nothing_live_to_beat_restores_nothing() {
        let (core, answer, other) = one_answer().await;
        let g = generation_for(&core).await;
        core.deprecate_with(&answer, Some(discard_row(&answer)))
            .await
            .unwrap();
        core.deprecate_with(&other, Some(discard_row(&other)))
            .await
            .unwrap();
        gave_up(&core, &g.id).await;

        assert_eq!(
            rule_two(&core, crate::store::now()).await.unwrap(),
            (0, false)
        );
        assert!(!core.store.get_artifact(&answer).await.unwrap().in_results());
    }

    #[tokio::test]
    async fn a_give_up_recorded_under_another_embedder_is_not_read() {
        let (core, answer, _) = one_answer().await;
        let g = generation_for(&core).await;
        core.deprecate_with(&answer, Some(discard_row(&answer)))
            .await
            .unwrap();
        let query_vec = core.embedder.embed_query(QUERY).await.unwrap();
        core.store
            .record_observation(&NewObservation {
                generation_id: g.id.clone(),
                query: QUERY.into(),
                query_vec,
                embed_model: "some-other-model".into(),
                artifact_id: None,
                rank: None,
                source: Source::GaveUp,
                event_id: None,
            })
            .await
            .unwrap();

        assert_eq!(
            rule_two(&core, crate::store::now()).await.unwrap(),
            (0, false)
        );
        assert!(!core.store.get_artifact(&answer).await.unwrap().in_results());
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

    #[tokio::test]
    async fn an_artifact_a_person_hid_stays_hidden_after_it_is_buried() {
        let (core, answer, _) = one_answer().await;
        let g = generation_for(&core).await;
        let model = core.embedder.model().to_string();
        // No journal row: that is what distinguishes a hand Hide from a judged discard.
        core.deprecate(&answer).await.unwrap();
        sqlx::query!(
            "UPDATE artifacts SET retired_at = ? WHERE id = ?",
            crate::store::now() - 400 * 86_400,
            &answer
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        let dense = core
            .vectors
            .dense_of(&answer)
            .await
            .unwrap()
            .expect("a point");
        core.store
            .bury(
                &answer,
                r#"{"reason":"covered"}"#,
                0,
                Some(&dense),
                Some(&model),
                &crate::jobs::reap::test_support::row(&answer),
            )
            .await
            .unwrap();
        core.vectors
            .delete_artifacts(std::slice::from_ref(&answer))
            .await
            .unwrap();
        gave_up(&core, &g.id).await;

        assert_eq!(
            rule_two(&core, crate::store::now()).await.unwrap(),
            (0, false),
            "a give-up overturned somebody's decision to hide this"
        );
        let row = core.store.get_artifact(&answer).await.unwrap();
        assert!(row.reaped_at.is_some(), "the grave was opened anyway");
        assert!(!row.in_results());
        assert!(
            !core
                .store
                .action_was_undone(&answer, Kind::Reap)
                .await
                .unwrap(),
            "an undo was stamped for a restore that did not happen"
        );
    }

    async fn buried(core: &Core, id: &str, embed_model: &str) {
        core.deprecate_with(id, Some(discard_row(id)))
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE artifacts SET retired_at = ? WHERE id = ?",
            crate::store::now() - 400 * 86_400,
            id
        )
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
    async fn a_grave_whose_artifact_is_gone_neither_stops_the_rule_nor_moves_the_cursor_back() {
        let (core, answer, _) = one_answer().await;
        let g = generation_for(&core).await;
        let model = core.embedder.model().to_string();
        buried(&core, &answer, &model).await;
        // The grave and the open `Reap` row stay, as in production.
        let corpus = core
            .store
            .get_artifact(&answer)
            .await
            .unwrap()
            .corpus_id
            .expect("a buried stub still names its corpus");
        core.store.delete_corpus(&corpus).await.unwrap();
        assert!(
            matches!(
                core.store.get_artifact(&answer).await,
                Err(crate::error::Error::NotFound)
            ),
            "the premise: the stub left with its corpus"
        );
        assert!(
            core.store.graveyard_row(&answer).await.unwrap().is_some(),
            "the grave is permanent by design"
        );
        assert!(
            core.store
                .open_action_on(&answer, Kind::Reap)
                .await
                .unwrap()
                .is_some(),
            "and so is its journal row"
        );
        gave_up(&core, &g.id).await;

        let out = rule_two(&core, crate::store::now()).await.unwrap();
        assert_eq!(out, (0, false), "an orphan is not something to restore");
        assert!(
            core.store.meta_get(GAVE_UP_AFTER).await.unwrap().is_some(),
            "the pass died before it could stamp where it had read to"
        );
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
