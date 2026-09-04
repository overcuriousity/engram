//! The idle pass: a quiet base moves its own ranking, and takes the move back.
//!
//! Not a new engine. The verdict-paid sweep already gathers pairs, ranks them
//! under other settings, gates the candidates and picks a winner; this is that
//! body with three changes. The candidates are drawn a step at a time rather
//! than enumerated, the pairs are the positive observations use left behind
//! rather than verdicts, and the winner becomes the live generation instead of
//! a recommendation waiting for a press. The `eval_runs` row is still written
//! — it is the journal, and the generation names it.
//!
//! Two halves, on two kinds of evidence. Adoption is counterfactual and reads
//! positives only: an excerpt that was used can be re-ranked under other
//! settings to ask where they would have put it. A negative cannot — a give-up
//! says this list did not answer, and whether another list would have is
//! unknowable, because it was never shown. So the watch reads what happened
//! instead: what the adopted generation earned while it was serving, against
//! what its predecessor earned, and the predecessor comes back when the new
//! one does not hold.
//!
//! The pass spends no inference. Every observation keeps the vector its query
//! was searched with, so the replay embeds nothing; the reranker is left out,
//! so it calls nothing; and its searches take the background lane, behind
//! whoever is actually waiting. `config.toml` is never written: the file is the
//! operator's starting point and the database holds what is live.

use crate::core::Core;
use crate::error::Result;
use crate::eval::sweep;
use crate::store::generations::{Generation, GenerationParams, NewGeneration};

/// How many candidates one pass ranks the pairs under. A bound on work rather
/// than a setting: with two axes it covers every rung of both ladders, and a
/// third axis will spend it on the nearest steps first.
const BUDGET: usize = 8;

/// What one pass did. Flat counts, so `jobs::did_work` reads them.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Pass {
    /// The generation adopted, if a candidate cleared the gate.
    pub adopted: Option<String>,
    /// The generation taken back, if the one under watch did not hold.
    pub reverted: Option<String>,
}

/// Run the pass whatever the clock says. The adopted generation's id, or
/// `None` — which is the common and correct outcome.
pub async fn run(core: &Core) -> Result<Option<String>> {
    Ok(pass(core).await?.adopted)
}

/// The pass, if the base has been quiet for `evolve.idle_secs`.
///
/// Quiet is read off the base rather than a ticker: no search recorded and no
/// question asked inside the window. What the retention unit calls.
pub async fn run_if_quiet(core: &Core) -> Result<Pass> {
    if !core.evolve.autonomous || !quiet(core).await? {
        return Ok(Pass::default());
    }
    pass(core).await
}

async fn quiet(core: &Core) -> Result<bool> {
    let since = crate::store::now() - core.evolve.idle_secs.max(0);
    let recent: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM search_events WHERE created_at > ?)
              + (SELECT COUNT(*) FROM ask_events WHERE created_at > ?)",
    )
    .bind(since)
    .bind(since)
    .fetch_one(&core.store.pool)
    .await?;
    Ok(recent == 0)
}

pub async fn pass(core: &Core) -> Result<Pass> {
    if !core.evolve.autonomous {
        return Ok(Pass::default());
    }
    let Some(live) = core.store.live_generation().await? else {
        // The boot path that names a generation has not run. Nothing to move
        // from, and nothing for an adoption to be a child of.
        return Ok(Pass::default());
    };
    // The verdict-paid sweep and this pass share one claim: two replays over
    // one base at once would each measure a baseline the other is about to
    // change.
    let Some(_claim) = sweep::Sweeping::claim(core) else {
        return Ok(Pass::default());
    };
    let current = *core.ranking.read().expect("ranking lock");
    if GenerationParams::from(current) != live.params {
        // The generation says one thing and the running parameters another.
        // Boot and the apply button both keep them in step; if something has
        // not, adopting on top of it would journal a move from settings that
        // were never measured.
        tracing::warn!(
            generation = %live.id,
            "the live generation does not describe the running parameters; the pass did nothing"
        );
        return Ok(Pass::default());
    }

    propose(core, &live, current).await
}

/// Rank the positive observations under the neighbouring settings, and adopt
/// the one that clears the gate, if any does.
async fn propose(
    core: &Core,
    live: &Generation,
    current: crate::core::ranking::RankingParams,
) -> Result<Pass> {
    let (pairs, skipped) = sweep::observation_pairs(core, &live.id).await?;
    if pairs.is_empty() {
        return Ok(Pass::default());
    }
    let tried = core
        .store
        .tried_candidates(&live.embed_recipe, &live.chat_model)
        .await?;
    let grid = sweep::candidates(current, &tried, BUDGET);
    let scored = sweep::score(core, &pairs, grid, current, false).await?;

    // Same guard as the sweep, for the same reason: an apply landing while
    // this ran means the baseline it measured against is no longer running.
    if *core.ranking.read().expect("ranking lock") != current {
        tracing::info!("ranking changed while the idle pass ran; its results were discarded");
        return Ok(Pass::default());
    }

    let judged = core.store.feedback_stats(core.weak_below()).await?.judged;
    let run_id = core
        .store
        .record_eval_run(&scored.eval_run(&pairs, judged, skipped))
        .await?;
    let Some(winner) = scored.winner() else {
        return Ok(Pass::default());
    };
    let predicted = scored.predicted().unwrap_or(0.0);
    let id = core
        .store
        .adopt_generation(
            &NewGeneration {
                params: winner.into(),
                embed_recipe: live.embed_recipe.clone(),
                chat_model: live.chat_model.clone(),
                parent_id: Some(live.id.clone()),
            },
            &run_id,
            predicted,
        )
        .await?;
    *core.ranking.write().expect("ranking lock") = winner;
    // Stamped, or the insights page would offer an Apply button for settings
    // that are already running.
    core.store.mark_eval_run_applied(&run_id).await?;
    tracing::info!(
        generation = %id,
        recency_weight = winner.recency_weight,
        per_source_cap = ?winner.per_source_cap,
        predicted,
        pairs = pairs.len(),
        "adopted a generation"
    );
    Ok(Pass {
        adopted: Some(id),
        reverted: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::sweep::test_support::{QUERY, seeded};
    use crate::store::observations::{NewObservation, Source};

    /// Name the running configuration as the live generation.
    async fn generation_for(core: &Core) -> String {
        let params = *core.ranking.read().unwrap();
        core.store
            .record_generation(&NewGeneration {
                params: params.into(),
                embed_recipe: "recipe-a".into(),
                chat_model: "qwen".into(),
                parent_id: None,
            })
            .await
            .unwrap()
    }

    /// A used excerpt, at the rank the uncapped list gave it, carrying the
    /// vector a real search of `QUERY` would have used.
    pub(crate) async fn observe(core: &Core, generation: &str, artifact: &str, rank: i64) {
        let query_vec = core.embedder.embed_query(QUERY).await.unwrap();
        core.store
            .record_observation(&NewObservation {
                generation_id: generation.to_string(),
                query: QUERY.into(),
                query_vec,
                embed_model: "fake".into(),
                artifact_id: Some(artifact.to_string()),
                rank: Some(rank),
                source: Source::Cited,
            })
            .await
            .unwrap();
    }

    /// A base whose second source's first two chunks were the ones an answer
    /// drew on — buried behind the leading source uncapped, and promoted the
    /// moment a cap displaces its tail. Autonomy off, as shipped.
    pub(crate) async fn seeded_with_observations() -> (Core, String) {
        let (core, order) = seeded().await;
        let generation = generation_for(&core).await;
        observe(&core, &generation, &order[3], 4).await;
        observe(&core, &generation, &order[4], 5).await;
        (core, generation)
    }

    /// The same base, with the observations already at the top of the list.
    async fn seeded_with_nothing_to_gain() -> (Core, String) {
        let (core, order) = seeded().await;
        let generation = generation_for(&core).await;
        observe(&core, &generation, &order[0], 1).await;
        observe(&core, &generation, &order[1], 2).await;
        (core, generation)
    }

    #[tokio::test]
    async fn a_pass_with_autonomy_off_changes_nothing() {
        let (core, before) = seeded_with_observations().await;
        assert!(run(&core).await.unwrap().is_none());
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            before
        );
        assert!(
            core.store.latest_eval_run().await.unwrap().is_none(),
            "off means off: not even a journal row"
        );
    }

    #[tokio::test]
    async fn a_candidate_that_clears_the_gate_becomes_the_live_generation() {
        let (mut core, before) = seeded_with_observations().await;
        core.evolve.autonomous = true;
        let adopted = run(&core).await.unwrap().expect("a candidate cleared");

        let live = core.store.live_generation().await.unwrap().unwrap();
        assert_eq!(live.id, adopted);
        assert_ne!(live.id, before, "the base moved");
        assert_eq!(live.parent_id.as_deref(), Some(before.as_str()));
        assert!(live.predicted.is_some(), "it must say what it promised");
        assert!(
            live.params.per_source_cap.is_some(),
            "the improvement here is a cap, and the generation must carry it"
        );
        assert_eq!(
            GenerationParams::from(*core.ranking.read().unwrap()),
            live.params,
            "and serve under it"
        );
        let run = core.store.latest_eval_run().await.unwrap().unwrap();
        assert_eq!(live.run_id.as_deref(), Some(run.id.as_str()));
        assert!(run.recommended);
        assert!(
            run.applied_at.is_some(),
            "a run the base already took must not be offered as an open recommendation"
        );
    }

    #[tokio::test]
    async fn a_pass_that_finds_nothing_better_leaves_the_generation_alone() {
        let (mut core, before) = seeded_with_nothing_to_gain().await;
        core.evolve.autonomous = true;
        assert!(run(&core).await.unwrap().is_none());
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            before
        );
        let run = core.store.latest_eval_run().await.unwrap().unwrap();
        assert!(!run.recommended, "the quiet pass is still journaled");
    }

    #[tokio::test]
    async fn the_pass_embeds_nothing() {
        // Every observation keeps the vector its query was searched with. A
        // pass that embedded anyway would be inference spent on a base that is
        // supposed to be asleep.
        let (mut core, embedder) =
            crate::core::test_support::test_core_counting_embed_calls().await;
        core.evolve.autonomous = true;
        let generation = generation_for(&core).await;
        observe(&core, &generation, "art-1", 4).await;
        observe(&core, &generation, "art-2", 5).await;
        let before = embedder.calls();

        run(&core).await.unwrap();
        assert_eq!(embedder.calls(), before, "the pass embedded a query");
    }

    #[test]
    fn the_pass_never_writes_the_operators_config_file() {
        // The file is the starting point and the envelope. A loop that rewrote
        // it every quiet period would turn a commented file into a machine's.
        // The pass has no path to write to, and this is the rule kept in a
        // form that fails the moment somebody hands it one.
        let source = include_str!("tune.rs");
        let body = source.split("#[cfg(test)]").next().unwrap();
        assert!(
            !body.contains("write_ranking"),
            "the pass writes config.toml"
        );
        assert!(
            !body.contains("config_path"),
            "the pass knows where config.toml is"
        );
    }
}
