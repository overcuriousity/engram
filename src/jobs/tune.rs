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
//! whoever is actually waiting. It stops the moment somebody comes back —
//! between pairs, with nothing written — and the next quiet period starts it
//! over. Recomputing is the resumption: the pass is bounded, so a restart
//! costs what a pass costs, and no partial state has to be kept correct across
//! a sitting. `config.toml` is never written: the file is the operator's
//! starting point and the database holds what is live.

use crate::core::Core;
use crate::error::Result;
use crate::eval::lived::{holds_up, lived, settled};
use crate::eval::sweep;
use crate::store::generations::{Generation, GenerationParams, NewGeneration};

/// How many candidates one pass ranks the pairs under: the running
/// configuration and every rung on every axis, one knob moved at a time. A
/// bound on work rather than a setting, and deliberately not "the nearest step
/// only": a tie keeps the current value, so an improvement two rungs out
/// behind a rung that ties would never be reached at all. Sixteen vector reads
/// per pair, over a bounded number of pairs, and the pass stops when somebody
/// comes back.
pub(crate) const BUDGET: usize = 16;

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
    Ok(!core
        .store
        .activity_since(crate::store::now() - core.evolve.idle_secs.max(0))
        .await?)
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
    // The one safeguard everything else leans on. When the self-generated
    // evidence has stopped agreeing with the people using the base, the loop
    // adopts nothing, reverts nothing, keeps recording, and says so on Ops.
    // Suspension is a state, not a failure.
    if let Some(a) = crate::eval::anchor::agreement(core).await?
        && !crate::eval::anchor::trustworthy(&a)
    {
        tracing::warn!(
            agreed = a.agreed,
            disagreed = a.disagreed,
            "observations no longer agree with verdicts; the base is not moving"
        );
        return Ok(Pass::default());
    }
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

    // A live generation with a parent and a prediction is under watch, and
    // the watch comes before any new proposal. One change at a time is what
    // keeps the journal readable and the revert exact, and what stops a base
    // walking three knobs away from anything it measured.
    if let (Some(parent_id), Some(_)) = (&live.parent_id, live.predicted)
        && let Some(parent) = core.store.generation(parent_id).await?
    {
        let new = lived(core, &live.id).await?;
        let old = lived(core, &parent.id).await?;
        if !holds_up(&new, &old) {
            return revert(core, &live, &new, &old).await;
        }
        if !settled(&new, &old) {
            tracing::debug!(generation = %live.id, ?new, ?old, "under watch; nothing proposed");
            return Ok(Pass::default());
        }
    }

    propose(core, &live, current).await
}

/// Put the predecessor back, and remember the candidate that failed.
///
/// Compared against the predecessor's *lived* record, never its offline
/// number: that was computed on replayed evidence and is not the same kind of
/// quantity. The memory is the row itself — `reverted` is a state, and
/// `tried_candidates` reads it — so the same candidate is not proposed again
/// on the next quiet period. Without that the base oscillates.
async fn revert(
    core: &Core,
    live: &Generation,
    new: &crate::eval::lived::Lived,
    old: &crate::eval::lived::Lived,
) -> Result<Pass> {
    let Some(back) = core.store.revert_generation(&live.id).await? else {
        // Checked above; a parent that vanished between the two reads is a
        // base with nowhere to go back to, which stays where it is.
        return Ok(Pass::default());
    };
    *core.ranking.write().expect("ranking lock") = back.params.into();
    tracing::info!(
        reverted = %live.id,
        live = %back.id,
        predicted = live.predicted,
        ?new,
        ?old,
        "a generation did not hold what it promised; its predecessor is live again"
    );
    Ok(Pass {
        adopted: None,
        reverted: Some(live.id.clone()),
    })
}

/// Rank the positive observations under the neighbouring settings, and adopt
/// the one that clears the gate, if any does.
async fn propose(
    core: &Core,
    live: &Generation,
    current: crate::core::ranking::RankingParams,
) -> Result<Pass> {
    let started = crate::store::now();
    let (pairs, skipped) = sweep::observation_pairs(core, &live.id).await?;
    if pairs.is_empty() {
        return Ok(Pass::default());
    }
    let tried = core
        .store
        .tried_candidates(&live.embed_recipe, &live.chat_model)
        .await?;
    let grid = sweep::candidates(current, &tried, BUDGET);
    let Some(scored) = sweep::score(core, &pairs, grid, current, false, Some(started)).await?
    else {
        tracing::info!(
            "somebody came back; the idle pass stopped and will start over next quiet period"
        );
        return Ok(Pass::default());
    };

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

/// Bases in each state the loop can be in, for this module's tests and the
/// insights page's.
#[cfg(test)]
pub(crate) mod test_support {
    use super::tests::{adopted_and_watching as watching, disagree_loudly};
    use crate::core::Core;

    /// A base that has just adopted a generation, with the one it replaced.
    pub(crate) async fn adopted_and_watching() -> (Core, String) {
        watching().await
    }

    /// A base whose evidence has stopped agreeing with its verdicts, with the
    /// generation that was live when it did.
    pub(crate) async fn suspended() -> (Core, String) {
        let (core, _) = watching().await;
        let live = core.store.live_generation().await.unwrap().unwrap().id;
        disagree_loudly(&core, 20).await;
        (core, live)
    }
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

    /// A base that has just adopted a generation, with the one it replaced.
    pub(crate) async fn adopted_and_watching() -> (Core, String) {
        let (mut core, parent) = seeded_with_observations().await;
        core.evolve.autonomous = true;
        run(&core).await.unwrap().expect("a candidate cleared");
        (core, parent)
    }

    /// `n` searches given up on under the live generation: the weak negative,
    /// which may take a setting back and may never bring one about.
    pub(crate) async fn observe_badly_under_live(core: &Core, n: usize) {
        let live = core.store.live_generation().await.unwrap().unwrap().id;
        for i in 0..n {
            core.store
                .record_observation(&NewObservation {
                    generation_id: live.clone(),
                    query: format!("something that did not answer {i}"),
                    query_vec: vec![0.1, 0.2, 0.3],
                    embed_model: "fake".into(),
                    artifact_id: None,
                    rank: None,
                    source: Source::GaveUp,
                })
                .await
                .unwrap();
        }
    }

    async fn eval_runs(core: &Core) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM eval_runs")
            .fetch_one(&core.store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_generation_under_watch_blocks_a_new_proposal() {
        let (core, _) = adopted_and_watching().await;
        let live_before = core.store.live_generation().await.unwrap().unwrap().id;
        let runs = eval_runs(&core).await;
        assert!(run(&core).await.unwrap().is_none(), "one change at a time");
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            live_before
        );
        assert_eq!(eval_runs(&core).await, runs, "nothing was even replayed");
    }

    #[tokio::test]
    async fn a_generation_that_lost_ground_reverts_itself() {
        let (core, parent) = adopted_and_watching().await;
        // Weak negatives, and enough of them that one observation could not
        // account for the gap between two positives out of two and sixteen
        // give-ups out of sixteen.
        observe_badly_under_live(&core, 16).await;
        let adopted = core.store.live_generation().await.unwrap().unwrap();

        assert!(run(&core).await.unwrap().is_none());
        let live = core.store.live_generation().await.unwrap().unwrap();
        assert_eq!(live.id, parent, "the base put itself back");
        assert_eq!(
            GenerationParams::from(*core.ranking.read().unwrap()),
            live.params,
            "and serves under the predecessor again"
        );
        assert_eq!(
            core.store
                .generation(&adopted.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "reverted"
        );
    }

    #[tokio::test]
    async fn a_reverted_generation_is_not_proposed_again_on_the_next_pass() {
        let (core, _) = adopted_and_watching().await;
        observe_badly_under_live(&core, 16).await;
        let reverted = core.store.live_generation().await.unwrap().unwrap().params;
        run(&core).await.unwrap();

        let next = run(&core).await.unwrap();
        if let Some(id) = next {
            let g = core.store.live_generation().await.unwrap().unwrap();
            assert_ne!(g.params, reverted, "{id} re-proposed what had just failed");
        }
    }

    #[tokio::test]
    async fn a_watch_ends_once_the_generation_has_earned_its_place() {
        // A watch that never ended would be one adoption and then silence for
        // the life of the base.
        let (mut core, order) = seeded().await;
        let generation = generation_for(&core).await;
        observe(&core, &generation, &order[3], 4).await;
        observe(&core, &generation, &order[4], 5).await;
        core.evolve.autonomous = true;
        run(&core).await.unwrap().expect("a candidate cleared");
        let live = core.store.live_generation().await.unwrap().unwrap().id;
        for artifact in &order[..3] {
            observe(&core, &live, artifact, 1).await;
        }
        let runs = eval_runs(&core).await;

        run(&core).await.unwrap();
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            live,
            "it held, so it stays"
        );
        assert_eq!(
            eval_runs(&core).await,
            runs + 1,
            "and the pass went looking again"
        );
    }

    /// `n` searches judged gaps whose queries nonetheless carry a positive
    /// observation: the evidence saying "answered" where the person said
    /// "nothing here", over and over.
    pub(crate) async fn disagree_loudly(core: &Core, n: usize) {
        use crate::store::feedback::{Door, Labeller, NewEvent, Verdict};
        let live = core.store.live_generation().await.unwrap().unwrap().id;
        for i in 0..n {
            let query = format!("a question the base could not answer {i}");
            let id = core
                .store
                .record_search(
                    NewEvent {
                        fold_onto: None,
                        query: query.clone(),
                        door: Door::Ui,
                        scope: None,
                        filters: "{}".into(),
                        query_vec: vec![0.1, 0.2],
                        embed_model: "fake".into(),
                        candidates: vec![],
                        answered: false,
                    },
                    0,
                )
                .await
                .unwrap();
            core.store
                .judge(&id, Verdict::Gap, Labeller::Deck)
                .await
                .unwrap();
            core.store
                .record_observation(&NewObservation {
                    generation_id: live.clone(),
                    query,
                    query_vec: vec![0.1, 0.2],
                    embed_model: "fake".into(),
                    artifact_id: Some("art-that-was-not-an-answer".into()),
                    rank: Some(1),
                    source: Source::Cited,
                })
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn a_base_whose_evidence_stopped_agreeing_suspends_itself() {
        let (mut core, before) = seeded_with_observations().await;
        core.evolve.autonomous = true;
        disagree_loudly(&core, 20).await;

        assert!(run(&core).await.unwrap().is_none());
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            before
        );
        assert!(
            core.store.latest_eval_run().await.unwrap().is_none(),
            "suspended means nothing is even replayed"
        );
    }

    #[tokio::test]
    async fn a_suspended_base_reverts_nothing_and_keeps_recording() {
        let (core, _) = adopted_and_watching().await;
        observe_badly_under_live(&core, 16).await;
        let live = core.store.live_generation().await.unwrap().unwrap().id;
        disagree_loudly(&core, 20).await;

        run(&core).await.unwrap();
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            live,
            "a base that cannot trust its evidence acts on none of it"
        );
        assert!(
            core.store
                .observations_for_generation(&live, 100)
                .await
                .unwrap()
                .len()
                >= 36,
            "collection carries on"
        );
    }

    /// A search stamped a moment *after* now: what a search landing mid-pass
    /// looks like to a check that reads a timestamp.
    async fn somebody_returns(core: &Core) -> String {
        let id = core
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: "back at the keyboard".into(),
                    door: crate::store::feedback::Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.1, 0.2],
                    embed_model: "fake".into(),
                    candidates: vec![],
                    answered: false,
                },
                0,
            )
            .await
            .unwrap();
        sqlx::query("UPDATE search_events SET created_at = ? WHERE id = ?")
            .bind(crate::store::now() + 5)
            .bind(&id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn a_pass_stops_when_somebody_comes_back_and_adopts_nothing() {
        // The check reads the same predicate whether the search landed before
        // the first pair or between two of them, so a search stamped a moment
        // after the pass starts stands in for one that lands mid-pass — the
        // pass cannot see the difference, and neither can this test without a
        // vector store that writes to the log on its own first read.
        let (mut core, before) = seeded_with_observations().await;
        core.evolve.autonomous = true;
        somebody_returns(&core).await;

        assert!(run(&core).await.unwrap().is_none());
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            before
        );
        assert!(
            core.store.latest_eval_run().await.unwrap().is_none(),
            "an abandoned pass writes nothing: it is never partially adopted"
        );
    }

    #[tokio::test]
    async fn the_next_quiet_period_starts_the_pass_over() {
        // Resumption is recomputation. The pass is bounded, so a restart costs
        // a pass, and no partial state has to be kept correct across a sitting.
        let (mut core, _) = seeded_with_observations().await;
        core.evolve.autonomous = true;
        let id = somebody_returns(&core).await;
        assert!(run(&core).await.unwrap().is_none(), "interrupted");

        // The sitting ends: the search is now in the past.
        sqlx::query("UPDATE search_events SET created_at = ? WHERE id = ?")
            .bind(crate::store::now() - 5_000)
            .bind(&id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        assert!(
            run(&core).await.unwrap().is_some(),
            "and the pass finds what it would have"
        );
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
