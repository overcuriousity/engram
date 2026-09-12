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
//! The pass spends inference in exactly one case. Every observation keeps the
//! vector its query was searched with, so the replay embeds nothing; the
//! ladder replays with the reranker off, so it calls nothing; and its searches
//! take the background lane, behind whoever is actually waiting. The one case
//! is the rerank flip on a base whose live generation runs without a
//! configured reranker: one call per observation, to ask what the reranker
//! would have changed, spent because the operator configured it.
//!
//! Four kinds of move. The ladder is counterfactual: a replay under every
//! neighbouring rung of five knobs. The rerank flip is counterfactual with its
//! own base, the rank that was actually served. The band is lived: it widens
//! or narrows on whether it was used more than the ranked tail beside it,
//! asked only when the other two propose nothing. The review threshold is
//! lived too, and last: it moves on what the pairs just above it earned and
//! what was taken back, read off the corpus journal.
//!
//! And a corpus half, before any of that: `jobs::retract` reads the corpus
//! journal against the same observations and takes the base's own merges,
//! replacements, discards and burials back where the evidence says so. Same
//! switch, same claim, same anchor. It stops the moment somebody comes back —
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
/// behind a rung that ties would never be reached at all. Twenty vector reads
/// per pair at the widest — the running configuration, every other rung of
/// five ladders, and the sitting flip, which the chooser offers only above a
/// zero lift — over a bounded number of pairs, and the pass stops when
/// somebody comes back.
pub(crate) const BUDGET: usize = 20;

/// What one pass did. Flat counts, so `jobs::did_work` reads them.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Pass {
    /// The generation adopted, if a candidate cleared the gate.
    pub adopted: Option<String>,
    /// The generation taken back, if the one under watch did not hold.
    pub reverted: Option<String>,
    /// Corpus actions rule 1 took back: a merge or a supersession whose
    /// survivor was no longer found where the original was.
    pub undone: usize,
    /// Artifacts rule 2 restored for a search given up on.
    pub restored: usize,
    /// Interferers rule 3 observed. Counted only: retrieval competition is a
    /// fact about ranking, and `sleep::interference` files nothing.
    pub interference: usize,
    /// Condensations armed.
    pub condensed: usize,
    /// What the integrate phase filed before the pass.
    pub integrated: crate::jobs::sleep::Integrated,
    /// What the rehearse phase replayed.
    pub replayed: crate::jobs::sleep::Replayed,
    /// The candidate refused on the base's own probes, if one was.
    pub refused: Option<String>,
    /// Why the pass ended early, where it did: `suspended`, `no_evidence`,
    /// `activity`. Empty for a pass that ran to the end.
    pub stopped: &'static str,
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
    if !quiet(core).await? {
        return Ok(Pass::default());
    }
    // Bookkeeping whatever the stage: one neighbour read per new artifact, no
    // inference, and what lets the page say which artifacts nothing has asked
    // for. Under "off" this is the whole of what a quiet base does.
    let started = crate::store::now();
    // Before anything reads the record. An observation naming an artifact that
    // has left — with its corpus, usually — is not evidence about ordering any
    // more, and `lived` counting it as a hit props up the parent a watched
    // generation is judged against. One statement, and it belongs beside the
    // rest of the bookkeeping a quiet base does at every stage.
    match core.store.exclude_orphaned_observations().await {
        Ok(n) if n > 0 => tracing::info!(excluded = n, "observations whose artifact is gone"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "could not exclude orphaned observations"),
    }
    let integrated = crate::jobs::sleep::integrate(core, started).await?;
    let p = if !core.evolve.autonomous.moves_ranking() || integrated.stopped {
        Pass {
            integrated,
            stopped: if integrated.stopped { "activity" } else { "" },
            ..Default::default()
        }
    } else {
        let mut p = pass(core).await?;
        p.integrated = integrated;
        p
    };
    journal(core, started, &p).await?;
    Ok(p)
}

/// One row in `sleep_runs`, whatever the sleep did. Written here and not in
/// `pass`, because this is where the phases converge — and a base under
/// "off", which only integrates, still slept.
async fn journal(core: &Core, started: i64, p: &Pass) -> Result<()> {
    let Some(live) = core.store.live_generation().await? else {
        return Ok(());
    };
    let budget = if core.evolve.autonomous.acts_on_corpus() {
        core.budget(crate::store::actions::Job::Sleep).await?
    } else {
        crate::core::Budget { used: 0, cap: 0 }
    };
    core.store
        .record_sleep_run(&crate::store::sleep_runs::SleepRun {
            id: crate::store::new_id(),
            started,
            ended: crate::store::now(),
            stopped: if p.stopped.is_empty() {
                "finished".into()
            } else {
                p.stopped.into()
            },
            generation_id: live.id,
            integrated: p.integrated.integrated as i64,
            novel: p.integrated.novel as i64,
            known: p.integrated.known as i64,
            conflicts: p.integrated.conflicts as i64,
            rehearsed: p.replayed.rehearsed as i64,
            found: p.replayed.found as i64,
            adopted: p.adopted.clone(),
            reverted: p.reverted.clone(),
            refused: p.refused.clone(),
            undone: p.undone as i64,
            restored: p.restored as i64,
            interference: p.interference as i64,
            condensed: p.condensed as i64,
            budget_used: i64::from(budget.used),
            budget: i64::from(budget.cap),
            detail: "{}".into(),
        })
        .await
}

async fn quiet(core: &Core) -> Result<bool> {
    Ok(!core
        .store
        .activity_since(crate::store::now() - core.evolve.idle_secs.max(0))
        .await?)
}

pub async fn pass(core: &Core) -> Result<Pass> {
    if !core.evolve.autonomous.moves_ranking() {
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
    // The one safeguard everything else leans on, on two sides. Human
    // verdicts, where any judged search has an observation beside it, can
    // suspend: when the self-generated evidence has stopped agreeing with
    // the people using the base, the loop adopts nothing, reverts nothing,
    // keeps recording, and says so on Ops. Suspension is a state, not a
    // failure. Rehearsal, where any probe has been replayed, is the second
    // side, read below.
    let agreement = crate::eval::anchor::agreement(core).await?;
    if let Some(a) = agreement
        && !crate::eval::anchor::trustworthy(&a)
    {
        tracing::warn!(
            agreed = a.agreed,
            disagreed = a.disagreed,
            "observations no longer agree with verdicts; the base is not moving"
        );
        return Ok(Pass {
            stopped: "suspended",
            ..Default::default()
        });
    }

    // Before anything measures or acts, not just before the adoption at the
    // end. The generation says one thing and the running parameters another;
    // boot and the apply button both keep them in step, and where something
    // has not, every number this pass produces is stamped with `live.id` while
    // being measured under settings that generation does not describe —
    // probe results the watch will read back, and corpus actions the rules
    // take from them. Adopting on top of it would journal a move from settings
    // that were never measured; measuring on top of it poisons the record the
    // next pass reads.
    let current = *core.ranking.read().expect("ranking lock");
    if GenerationParams::from(current) != live.params {
        tracing::warn!(
            generation = %live.id,
            "the live generation does not describe the running parameters; the pass did nothing"
        );
        return Ok(Pass::default());
    }

    // Rehearse before anything reads the record: the corpus rules and the
    // watch below read what this writes. Pure vector reads.
    let started = crate::store::now();
    let replayed = crate::jobs::sleep::rehearse(core, &live, started).await?;
    if replayed.stopped {
        return Ok(Pass {
            replayed,
            stopped: "activity",
            ..Default::default()
        });
    }

    // Nothing on either side is the case this used to walk past: no judged
    // search with an observation beside it, and no probe ever replayed. A
    // base with nothing to measure a move against does not move.
    let probes = crate::eval::rehearsed::probe_set(core, &live.id).await?;
    if agreement.is_none() && probes.is_empty() {
        tracing::warn!("no evidence on either side; the base is not moving");
        return Ok(Pass {
            replayed,
            stopped: "no_evidence",
            ..Default::default()
        });
    }

    // The corpus half, before the ranking half's own gates: a base under
    // watch, or one whose parameters drifted, still answers for what it hid.
    // Same switch, same claim, same anchor — the rules read the same
    // observations the ladder does. A corpus rule runs where the others
    // will: under "full". A file that said `true` reads as "full", so nothing
    // an operator turned on turns off.
    let retracted = if core.evolve.autonomous.acts_on_corpus() {
        crate::jobs::retract::run(core, &live, started).await?
    } else {
        crate::jobs::retract::Retracted::default()
    };
    let mut out = Pass {
        undone: retracted.undone,
        restored: retracted.restored,
        interference: retracted.interference,
        condensed: retracted.condensed,
        replayed,
        ..Default::default()
    };

    // A live generation with a parent and a prediction is under watch, and
    // the watch comes before any new proposal. One change at a time is what
    // keeps the journal readable and the revert exact, and what stops a base
    // walking three knobs away from anything it measured.
    if let (Some(parent_id), Some(_)) = (&live.parent_id, live.predicted)
        && let Some(parent) = core.store.generation(parent_id).await?
    {
        let new = lived(core, &live.id).await?;
        let old = lived(core, &parent.id).await?;
        // The second side of the watch: both parameter sets replayed on one
        // probe set, now. A generation that loses on probes is taken back
        // with no observations at all, and two that cannot be told apart on
        // enough probes end the watch on a base nobody searches.
        //
        // Only where a replay can see the difference at all. Probes replay
        // through `Door::Judge` with no priming, so a move of the band, the
        // lift, the sitting or the review threshold replays identically on
        // both sides by construction — and ten identical replays are
        // `indistinguishable`. That ended the watch on a `spread_max` or
        // `review_min` move on the first pass after it was adopted, before a
        // single lived observation, and let a child be adopted on top of it.
        let mut rehearsal_lost = false;
        let mut rehearsal_settled = false;
        if !probes.is_empty()
            && crate::eval::rehearsed::probes_tell_apart(live.params.into(), parent.params.into())
        {
            // Reranked only where the two generations disagree about it. Held
            // constant it cancels out of the verdict and costs a reranker call
            // per probe to do so; where it *is* what separates them, replaying
            // without it made the two sides identical by construction.
            let axis = live.params.rerank != parent.params.rerank;
            let r_new = crate::eval::rehearsed::rehearsed_under(
                core,
                live.params.into(),
                &probes,
                Some(started),
                axis && live.params.rerank,
            )
            .await?;
            let r_old = crate::eval::rehearsed::rehearsed_under(
                core,
                parent.params.into(),
                &probes,
                Some(started),
                axis && parent.params.rerank,
            )
            .await?;
            match (r_new, r_old) {
                (Some(n), Some(o)) => {
                    rehearsal_lost = n.loses_to(&o);
                    // And only once the live evidence has said something at
                    // all. `indistinguishable` is true whenever two parameter
                    // sets score within noise on the probes, which is the
                    // ordinary case for a one-rung move — so on any base with
                    // ten probes the watch ended on the very next idle pass,
                    // with zero observations under the new generation, and the
                    // next proposal followed immediately. Probes saying "these
                    // two are the same" is not the live evidence having spoken.
                    // The same asymmetry as everywhere else here: probes refuse
                    // and revert, they do not license the next move.
                    rehearsal_settled =
                        new.observations > 0 && (n.indistinguishable(&o) || o.loses_to(&n));
                }
                _ => {
                    out.stopped = "activity";
                    return Ok(out);
                }
            }
        }
        // The third side: what the generation said it would do. Nothing read
        // `predicted` at all, and the two halves Insights printed beside each
        // other are not the same quantity — an MRR delta over replayed pairs
        // against a rate over observations — so a generation adopted on a
        // promised gain that delivered none ended its watch looking like a tie.
        let Some(delivered) = promise_kept(core, &live, &parent, started).await? else {
            out.stopped = "activity";
            return Ok(out);
        };
        if !holds_up(&new, &old) || rehearsal_lost || !delivered {
            let p = revert(core, &live, &new, &old).await?;
            out.reverted = p.reverted;
            return Ok(out);
        }
        if !settled(&new, &old) && !rehearsal_settled {
            tracing::debug!(generation = %live.id, ?new, ?old, "under watch; nothing proposed");
            return Ok(out);
        }
    }

    let p = propose(core, &live, current, &probes).await?;
    out.adopted = p.adopted;
    out.reverted = p.reverted;
    out.refused = p.refused;
    if !p.stopped.is_empty() {
        out.stopped = p.stopped;
    }
    Ok(out)
}

/// Whether the generation under watch delivered what it promised, measured in
/// the units the promise was made in.
///
/// `predicted` is an MRR delta over replayed pairs. The lived watch is a rate
/// over observations, and the probe anchor is an MRR over probes — neither is
/// the promise, and nothing was checking it. The commensurable test is the
/// same replay the adoption was made on, run again over the *new* generation's
/// own observations: if its parent would clear the adoption gate on the
/// evidence the new generation itself gathered, the move did not do what it
/// said it would. Biased in the generation's favour, if anything — those are
/// the results its own ranking put in front of somebody — which is what makes
/// failing it worth acting on.
///
/// Asked only where the promise came from a replay. A lived adoption's
/// `predicted` is a rate off the band or the judged bands, and its knob is one
/// a replay cannot see at all. `None` where somebody came back.
async fn promise_kept(
    core: &Core,
    live: &Generation,
    parent: &Generation,
    started: i64,
) -> Result<Option<bool>> {
    if live.run_id.is_none() {
        return Ok(Some(true));
    }
    let (pairs, _) = sweep::observation_pairs(core, &live.id).await?;
    if pairs.len() < sweep::MIN_PAIRS {
        // It has not earned enough of its own evidence to be asked yet. The
        // lived watch and the probes go on either way.
        return Ok(Some(true));
    }
    let new: crate::core::ranking::RankingParams = live.params.into();
    let old: crate::core::ranking::RankingParams = parent.params.into();
    // The reranker only where the two disagree about it, as everywhere else:
    // held constant it cancels out and costs a call per pair to do so.
    let axis = new.rerank != old.rerank;
    let Some(scored) = sweep::score(core, &pairs, vec![new, old], new, axis, Some(started)).await?
    else {
        return Ok(None);
    };
    if scored.winner() != Some(old) {
        return Ok(Some(true));
    }
    tracing::info!(
        generation = %live.id,
        predicted = live.predicted,
        shortfall = scored.predicted(),
        pairs = pairs.len(),
        "a generation's own observations replay better under its parent; \
         what it promised was not delivered"
    );
    Ok(Some(false))
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
        // Nowhere to go back to, or not live any more: a person's Apply made
        // another generation live while this pass was measuring the one it
        // was about to take back. Either way the base stays where it is.
        return Ok(Pass::default());
    };
    swap_ranking(core, live.params.into(), back.params.into());
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
        ..Default::default()
    })
}

/// Rank the positive observations under the neighbouring settings, and adopt
/// the one that clears the gate, if any does.
async fn propose(
    core: &Core,
    live: &Generation,
    current: crate::core::ranking::RankingParams,
    probes: &[crate::store::rehearsals::Rehearsal],
) -> Result<Pass> {
    let started = crate::store::now();
    let (pairs, skipped) = sweep::observation_pairs(core, &live.id).await?;
    let tried = core
        .store
        .tried_candidates(&live.embed_recipe, &live.chat_model)
        .await?;
    if pairs.is_empty() {
        // No observations under this generation, so the ladder and the flip
        // have nothing to score — both rank the positives and there are none.
        //
        // The two rules below do not read observations at all. `spread_step`
        // reads what the appended band earned while serving (`band_use`, off
        // `search_events`), and `review_step` reads the judged bands
        // (`band_record`, off `artifact_pairs` and `corpus_actions`). Returning
        // here gated both behind evidence neither of them uses: on a base used
        // through the dedupe judge but producing no `Cited` or `Opened` rows
        // under the live generation — a base whose owner reads on one door and
        // answers the queue on another, and every base for the whole of the
        // generation after an adoption — `review_min` could never move at all.
        return spread_step(core, live, current, &tried).await;
    }
    let grid = sweep::candidates(current, &tried, BUDGET);
    let Some(scored) = sweep::score(core, &pairs, grid, current, false, Some(started)).await?
    else {
        tracing::info!(
            "somebody came back; the idle pass stopped and will start over next quiet period"
        );
        return Ok(Pass {
            stopped: "activity",
            ..Default::default()
        });
    };

    // The ladder first. The rerank flip is scored against a different base —
    // the served rank — so its promise is not comparable with a ladder row's,
    // and it is asked only when the ladder proposes nothing.
    let mut run = scored.eval_run(&pairs, judged_count(core).await?, skipped);
    let winner = match scored.winner() {
        Some(w) => Some((w, scored.predicted().unwrap_or(0.0))),
        // `Held` and `Stopped` are opposite instructions and must not be read
        // as one: the second says a search or a question landed while the
        // replay ran, and everything below this — the journal row, the spread
        // rule, the watch — measures against a baseline that person is now
        // moving. Falling through would rewrite `core.ranking` underneath them.
        None => match sweep::rerank_flip(core, &pairs, current, Some(started)).await? {
            sweep::FlipOffer::Stopped => {
                tracing::info!(
                    "somebody came back; the idle pass stopped and will start over next quiet period"
                );
                return Ok(Pass {
                    stopped: "activity",
                    ..Default::default()
                });
            }
            sweep::FlipOffer::Offered(flip)
                if !tried.contains(&GenerationParams::from(flip.params)) =>
            {
                run.best = flip.params.into();
                run.base_mrr = flip.served_mrr;
                run.base_recall = flip.served_recall;
                run.best_mrr = flip.mrr;
                run.best_recall = flip.recall;
                run.recommended = true;
                Some((flip.params, flip.predicted))
            }
            _ => None,
        },
    };

    // Same guard as the sweep, for the same reason: an apply landing while
    // this ran means the baseline it measured against is no longer running.
    if *core.ranking.read().expect("ranking lock") != current {
        tracing::info!("ranking changed while the idle pass ran; its results were discarded");
        return Ok(Pass::default());
    }

    let run_id = core.store.record_eval_run(&run).await?;
    let Some((winner, predicted)) = winner else {
        // Last, and on different evidence: the band is not replayed, it is
        // read off what it earned while serving.
        return spread_step(core, live, current, &tried).await;
    };
    // The yardstick. A candidate the observations chose is replayed on the
    // base's own probes beside the running configuration; one that loses
    // by more than a probe's worth is refused and not offered again. Never
    // the other way: probes refuse and revert, they do not adopt.
    if !probes.is_empty() {
        // As in the watch above: the reranker is run only where the candidate
        // and the running configuration differ about it. That is the flip and
        // nothing else, and it is the one candidate this gate could not read —
        // replayed with the axis off, a flip and its own current are the same
        // configuration, so `loses_to` was false however the flip would serve.
        let axis = winner.rerank != current.rerank;
        let r_live = crate::eval::rehearsed::rehearsed_under(
            core,
            current,
            probes,
            Some(started),
            axis && current.rerank,
        )
        .await?;
        let r_cand = crate::eval::rehearsed::rehearsed_under(
            core,
            winner,
            probes,
            Some(started),
            axis && winner.rerank,
        )
        .await?;
        match (r_live, r_cand) {
            (Some(l), Some(c)) if c.loses_to(&l) => {
                let id = core
                    .store
                    .refuse_generation(
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
                // The run was journalled before this gate, carrying the
                // recommendation that got the candidate here. Nothing will
                // ever stamp it applied — it was refused — so left standing it
                // is the newest open recommendation, and Insights offers the
                // refused parameters under an Apply button that writes them
                // into the file. The numbers stay; the offer goes.
                core.store.withdraw_eval_run(&run_id).await?;
                tracing::info!(
                    generation = %id,
                    live = ?l,
                    candidate = ?c,
                    "the ladder's candidate loses on the base's own probes; refused"
                );
                return Ok(Pass {
                    refused: Some(id),
                    ..Default::default()
                });
            }
            (Some(_), Some(_)) => {}
            _ => {
                // Same reasoning as the refusal above: the candidate was never
                // measured against the probes, so the run must not stand as
                // the base's open recommendation.
                core.store.withdraw_eval_run(&run_id).await?;
                return Ok(Pass {
                    stopped: "activity",
                    ..Default::default()
                });
            }
        }
    }
    adopt(core, live, current, winner, &run_id, predicted, pairs.len()).await
}

/// Serve under `to`, if the base is still serving under `from`.
///
/// The generation writes beside every call are conditional in the store; this
/// is the same condition on the running parameters, which an Apply swaps
/// before it journals. The guard in `propose` reads them long before the write,
/// so an Apply landing after that read and before this one had its parameters
/// replaced by the loop's.
fn swap_ranking(
    core: &Core,
    from: crate::core::ranking::RankingParams,
    to: crate::core::ranking::RankingParams,
) {
    let mut running = core.ranking.write().expect("ranking lock");
    if *running == from {
        *running = to;
    } else {
        tracing::info!("the ranking changed while the idle pass ran; it was left as it is");
    }
}

/// The spread rule. Grow when the band was used more than the ranked tail
/// beside it by more than one event could account for; shrink on the same
/// rule the other way; hold otherwise.
///
/// From zero there is no band, so the same rule is asked of the tail alone —
/// two opens of the last hit the list showed, which is the only sign a base
/// with no band can give that its lists end too soon. Offered unconditionally,
/// as it was, the first rung was adopted on no evidence whatever and then
/// watched by lived rates the band barely moves. Requiring the evidence is the
/// smaller of the two fixes; the other way round is teaching the probe replay
/// to see the band, and a probe goes through `Door::Judge`, which appends no
/// band by construction.
pub fn next_spread(current: usize, use_: crate::store::feedback::BandUse) -> Option<usize> {
    use crate::core::ranking::SPREADS;
    let at = SPREADS.iter().position(|s| *s == current)?;
    if current == 0 {
        return SPREADS.get(1).copied().filter(|_| use_.tail_used >= 2);
    }
    let net = use_.band_used as i64 - use_.tail_used as i64;
    if net >= 2 {
        SPREADS.get(at + 1).copied()
    } else if net <= -2 {
        at.checked_sub(1).map(|i| SPREADS[i])
    } else {
        None
    }
}

/// The lived step, asked only when the ladder and the flip proposed nothing.
async fn spread_step(
    core: &Core,
    live: &Generation,
    current: crate::core::ranking::RankingParams,
    tried: &[GenerationParams],
) -> Result<Pass> {
    // At the off rung there is no band, and no tail of the band's width
    // either: `band_use` reads the last `spread_max` hits shown, which at zero
    // is nobody. Read one hit wide there, so `next_spread` has the one signal
    // that rung can be argued for on.
    let use_ = core
        .store
        .band_use(&live.id, current.spread_max.max(1))
        .await?;
    let Some(next) = next_spread(current.spread_max, use_) else {
        return review_step(core, live, current, tried).await;
    };
    let candidate = crate::core::ranking::RankingParams {
        spread_max: next,
        ..current
    };
    if tried.contains(&GenerationParams::from(candidate)) {
        return review_step(core, live, current, tried).await;
    }
    let predicted = match use_.band_used + use_.tail_used {
        0 => 0.0,
        n => use_.band_used as f64 / n as f64,
    };
    let Some(id) = adopt_lived(core, live, current, candidate, predicted).await? else {
        return Ok(Pass::default());
    };
    tracing::info!(
        generation = %id,
        spread_max = next,
        band_used = use_.band_used,
        tail_used = use_.tail_used,
        "adopted a generation on what the band earned"
    );
    Ok(Pass {
        adopted: Some(id),
        reverted: None,
        ..Default::default()
    })
}

/// The review threshold's rule. Two signals, each a rate over the lowest
/// recorded band against the band above it, compared with one-decision
/// noise: `wrong` — the lowest band's actions taken back more often — steps
/// up; `short` — the lowest band acting as often — steps down. Wrong first.
/// A rung at or above `auto_supersede` is never offered, and a hand-set value
/// off the ladder holds.
///
/// The two signals read the noise term in opposite directions, and only
/// `wrong` gets it for free. It asks for a *difference* to exceed the noise,
/// so a thin band cannot fire it. `short` asks for a difference to stay
/// *within* the noise, which a thin band satisfies by having no evidence at
/// all — at one judged pair a side the term is 2.0 and every possible pair of
/// rates is inside it, so the threshold would walk to the bottom rung on
/// nothing. Hence `MIN_BAND`: the shape is right, it just has to be asked of
/// enough pairs for the noise to mean anything.
/// Judged pairs a band needs before `short` will read it. Ten a side puts
/// the noise term at 0.2 — a fifth of the rate range, so the two bands have
/// to be genuinely close for the difference to fall inside it.
const MIN_BAND: usize = 10;

pub fn next_review_min(
    current: f32,
    auto_supersede: f32,
    low: crate::store::pairs::BandRecord,
    above: crate::store::pairs::BandRecord,
) -> Option<f32> {
    use crate::core::ranking::REVIEW_MINS;
    let at = REVIEW_MINS
        .iter()
        .position(|r| (r - current).abs() < 1e-6)?;
    let rate = |n: usize, d: usize| (d > 0).then(|| n as f64 / d as f64);
    let noise = |a: usize, b: usize| 1.0 / a as f64 + 1.0 / b as f64;
    if let (Some(lw), Some(aw)) = (rate(low.undone, low.acted), rate(above.undone, above.acted))
        && lw - aw > noise(low.acted, above.acted)
    {
        return REVIEW_MINS
            .get(at + 1)
            .copied()
            .filter(|r| *r < auto_supersede);
    }
    if let (Some(ls), Some(as_)) = (rate(low.acted, low.judged), rate(above.acted, above.judged))
        && low.judged >= MIN_BAND
        && above.judged >= MIN_BAND
        && as_ - ls <= noise(low.judged, above.judged)
    {
        return at.checked_sub(1).map(|i| REVIEW_MINS[i]);
    }
    None
}

/// The last step, asked when nothing else moved: the review threshold on
/// what its lowest band earned and what was taken back.
async fn review_step(
    core: &Core,
    live: &Generation,
    current: crate::core::ranking::RankingParams,
    tried: &[GenerationParams],
) -> Result<Pass> {
    use crate::core::ranking::REVIEW_MINS;
    // Under "full" only, and it is the one ladder rung that is not part of the
    // reversible half. Stepping the threshold down widens what the dedupe
    // judge considers, and the merges and supersessions that follow are corpus
    // writes `revert_generation` does not undo — so moving it under "ranking"
    // made the config's claim that "ranking" is the reversible half false.
    // Its `wrong` signal reads `undone` rows besides, which only the corpus
    // rules produce, so under the default the ladder could only ever have
    // walked one way: down.
    if !core.evolve.autonomous.acts_on_corpus() {
        return Ok(Pass::default());
    }
    let Some(at) = REVIEW_MINS
        .iter()
        .position(|r| (r - current.review_min).abs() < 1e-6)
    else {
        return Ok(Pass::default());
    };
    let hi = REVIEW_MINS
        .get(at + 1)
        .copied()
        .unwrap_or(1.0)
        .min(core.consolidate.auto_supersede);
    let low = core.store.band_record(current.review_min, hi).await?;
    let above = core.store.band_record(hi, 1.0).await?;
    let Some(next) = next_review_min(
        current.review_min,
        core.consolidate.auto_supersede,
        low,
        above,
    ) else {
        return Ok(Pass::default());
    };
    let candidate = crate::core::ranking::RankingParams {
        review_min: next,
        ..current
    };
    if tried.contains(&GenerationParams::from(candidate)) {
        return Ok(Pass::default());
    }
    let predicted = match low.judged {
        0 => 0.0,
        n => low.acted as f64 / n as f64,
    };
    let Some(id) = adopt_lived(core, live, current, candidate, predicted).await? else {
        return Ok(Pass::default());
    };
    tracing::info!(
        generation = %id,
        review_min = next,
        ?low,
        ?above,
        "adopted a generation on what the lowest band earned"
    );
    Ok(Pass {
        adopted: Some(id),
        reverted: None,
        ..Default::default()
    })
}

/// Make `candidate` live on lived evidence: no run to name, `predicted` the
/// rate that argued for it. `None` where `live` stopped being live while the
/// pass ran — see `Store::adopt_generation`.
async fn adopt_lived(
    core: &Core,
    live: &Generation,
    current: crate::core::ranking::RankingParams,
    candidate: crate::core::ranking::RankingParams,
    predicted: f64,
) -> Result<Option<String>> {
    let Some(id) = core
        .store
        .adopt_generation_lived(
            &NewGeneration {
                params: candidate.into(),
                embed_recipe: live.embed_recipe.clone(),
                chat_model: live.chat_model.clone(),
                parent_id: Some(live.id.clone()),
            },
            predicted,
        )
        .await?
    else {
        tracing::info!(
            generation = %live.id,
            "the live generation changed while the idle pass ran; nothing adopted"
        );
        return Ok(None);
    };
    swap_ranking(core, current, candidate);
    Ok(Some(id))
}

async fn judged_count(core: &Core) -> Result<i64> {
    Ok(core.store.feedback_stats(core.weak_below()).await?.judged)
}

/// Make `winner` the live generation, naming the run that chose it.
async fn adopt(
    core: &Core,
    live: &Generation,
    current: crate::core::ranking::RankingParams,
    winner: crate::core::ranking::RankingParams,
    run_id: &str,
    predicted: f64,
    pairs: usize,
) -> Result<Pass> {
    let Some(id) = core
        .store
        .adopt_generation(
            &NewGeneration {
                params: winner.into(),
                embed_recipe: live.embed_recipe.clone(),
                chat_model: live.chat_model.clone(),
                parent_id: Some(live.id.clone()),
            },
            run_id,
            predicted,
        )
        .await?
    else {
        // An Apply landed after the guard in `propose` and before this write.
        // The run's baseline is no longer what runs, so it does not stand as
        // the open recommendation either.
        core.store.withdraw_eval_run(run_id).await?;
        tracing::info!(
            generation = %live.id,
            "the live generation changed while the idle pass ran; its candidate was discarded"
        );
        return Ok(Pass::default());
    };
    swap_ranking(core, current, winner);
    // Stamped, or the insights page would offer an Apply button for settings
    // that are already running.
    core.store.mark_eval_run_applied(run_id).await?;
    tracing::info!(
        generation = %id,
        recency_weight = winner.recency_weight,
        per_source_cap = ?winner.per_source_cap,
        prime_lift = winner.prime_lift,
        rerank = winner.rerank,
        predicted,
        pairs,
        "adopted a generation"
    );
    Ok(Pass {
        adopted: Some(id),
        reverted: None,
        ..Default::default()
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
        let generation = core
            .store
            .record_generation(&NewGeneration {
                params: params.into(),
                embed_recipe: "recipe-a".into(),
                chat_model: "qwen".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        rehearsed_once(core, &generation).await;
        generation
    }

    /// One probe, for whatever leads the list, with a result under
    /// `generation`: the base has rehearsed something, so the anchor has a
    /// side to read. One probe cannot refuse or revert anything — its noise
    /// term is 2.0 — so nothing here moves a test's outcome; it only keeps
    /// the pass from returning on no evidence.
    pub(crate) async fn rehearsed_once(core: &Core, generation: &str) {
        let order = crate::eval::sweep::test_support::ranks_order(core).await;
        let Some(lead) = order.first() else {
            return;
        };
        let query_vec = core.embedder.embed_query(QUERY).await.unwrap();
        let Some(pid) = core
            .store
            .record_rehearsal(&crate::store::rehearsals::NewRehearsal {
                class: crate::store::rehearsals::Class::Capture,
                query: QUERY.into(),
                query_vec,
                embed_model: core.embedder.model().to_string(),
                artifact_id: lead.clone(),
                source_id: None,
            })
            .await
            .unwrap()
        else {
            return;
        };
        core.store
            .record_rehearsal_result(&crate::store::rehearsals::NewResult {
                rehearsal_id: pid,
                generation_id: generation.to_string(),
                rank: Some(1),
                outranked_by: vec![],
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_pass_does_nothing_and_says_so_where_there_is_no_evidence_on_either_side() {
        let (mut core, generation) = seeded_with_observations().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        // No verdict has an observation beside it; take the one rehearsed
        // probe away and the anchor is absent on both sides.
        for p in core
            .store
            .rehearsals_after(&crate::store::Cursor::default(), 10)
            .await
            .unwrap()
        {
            core.store.retire_rehearsal(&p.id, 1).await.unwrap();
        }
        let p = pass(&core).await.unwrap();
        assert!(
            p.adopted.is_none(),
            "the None that walked past the anchor no longer does"
        );
        assert_eq!(p.stopped, "no_evidence");
        assert!(core.store.latest_eval_run().await.unwrap().is_none());
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            generation
        );
    }

    /// Thirty probes for the third chunk of the leading source, worded as
    /// the query, each with a result under `generation`. Uncapped it stands
    /// at rank 3; under any cap the ladder proposes it is displaced and
    /// refilled behind the other source, at rank 6 or worse. Thirty, because
    /// the noise term is `2/n` and a fall from a third to a sixth has to
    /// clear it.
    async fn probes_the_cap_buries(core: &Core, generation: &str, buried: &str) -> usize {
        let buried = buried.to_string();
        let query_vec = core.embedder.embed_query(QUERY).await.unwrap();
        let mut n = 0;
        for i in 0..30 {
            let Some(pid) = core
                .store
                .record_rehearsal(&crate::store::rehearsals::NewRehearsal {
                    class: crate::store::rehearsals::Class::Cue,
                    // Distinct queries so the unique index keeps them all;
                    // the vector is the same, so the replay is the same.
                    query: format!("{QUERY} #{i}"),
                    query_vec: query_vec.clone(),
                    embed_model: core.embedder.model().to_string(),
                    artifact_id: buried.clone(),
                    source_id: None,
                })
                .await
                .unwrap()
            else {
                continue;
            };
            core.store
                .record_rehearsal_result(&crate::store::rehearsals::NewResult {
                    rehearsal_id: pid,
                    generation_id: generation.to_string(),
                    rank: Some(3),
                    outranked_by: vec![],
                })
                .await
                .unwrap();
            n += 1;
        }
        n
    }

    #[tokio::test]
    async fn a_candidate_that_loses_on_the_bases_own_probes_is_refused_and_not_offered_again() {
        let (mut core, generation) = seeded_with_observations().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let live = core.store.generation(&generation).await.unwrap().unwrap();
        let order = crate::eval::sweep::test_support::ranks_order(&core).await;
        assert_eq!(
            probes_the_cap_buries(&core, &generation, &order[2]).await,
            30
        );
        let p = pass(&core).await.unwrap();
        let refused = p.refused.expect("the cap buries the probed chunk");
        assert!(p.adopted.is_none());
        let g = core.store.generation(&refused).await.unwrap().unwrap();
        assert_eq!(g.state, "refused");
        // The run that chose it is journalled — the sweep happened — but it is
        // not an offer. Nothing stamps a refused run applied, so a run left
        // recommended would put the refused parameters under Insights' Apply
        // button, where pressing it writes settings the base has measured and
        // rejected and `tried_candidates` guarantees are never re-measured.
        assert!(
            core.store.open_recommendation().await.unwrap().is_none(),
            "a refused candidate is not offered under Apply"
        );
        assert!(
            core.store.latest_eval_run().await.unwrap().is_some(),
            "the run itself stays in the journal"
        );
        assert!(
            core.store
                .tried_candidates(&live.embed_recipe, &live.chat_model)
                .await
                .unwrap()
                .contains(&g.params)
        );
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            generation,
            "nothing moved"
        );
        // The next pass does not offer it again. It may refuse the next rung
        // of the same knob on the same probes; it never re-offers this one,
        // and the base stays put.
        let again = pass(&core).await.unwrap();
        assert!(again.adopted.is_none(), "{again:?}");
        if let Some(r) = &again.refused {
            let h = core.store.generation(r).await.unwrap().unwrap();
            assert_ne!(
                h.params, g.params,
                "a refused candidate is not offered again"
            );
        }
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            generation
        );
    }

    #[tokio::test]
    async fn a_watched_generation_that_loses_on_probes_is_reverted_with_no_observations_at_all() {
        let (mut core, parent) = seeded_with_observations().await;
        // The chunk the cap will displace, read under the uncapped parent
        // before anything is adopted: `ranks_order` reads the live params.
        let buried = crate::eval::sweep::test_support::ranks_order(&core).await[2].clone();
        core.evolve.autonomous = crate::config::Autonomy::Full;
        run(&core).await.unwrap().expect("a candidate cleared");
        let live = core.store.live_generation().await.unwrap().unwrap();
        assert_eq!(live.parent_id.as_deref(), Some(parent.as_str()));
        // Nothing observed under the adopted generation: the lived watch
        // cannot decide. The probes can — the cap it adopted buries the
        // probed chunk.
        assert_eq!(probes_the_cap_buries(&core, &live.id, &buried).await, 30);
        let p = pass(&core).await.unwrap();
        assert_eq!(p.reverted.as_deref(), Some(live.id.as_str()), "{p:?}");
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            parent
        );
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
                event_id: None,
            })
            .await
            .unwrap();
    }

    /// A base whose second source's first two chunks were the ones an answer
    /// drew on — buried behind the leading source uncapped, and promoted the
    /// moment a cap displaces its tail. Autonomy as shipped: "ranking".
    pub(crate) async fn seeded_with_observations() -> (Core, String) {
        let (core, order) = seeded().await;
        let generation = generation_for(&core).await;
        // Ten, because `sweep::MIN_PAIRS` is what a recommendation needs
        // behind it and two opens are two opens. Nine of the first excerpt
        // and one of the second, rather than five each: `jobs::retract`'s
        // pass-level tests supersede whatever stands third *after* this base
        // has adopted — which is this second excerpt — and weigh three fresh
        // observations of it at rank 1 against what it has here. How many of
        // those there are is a fact about this fixture. The cap lifts both, so
        // the gate this fixture exists for reads the same either way.
        for _ in 0..9 {
            observe(&core, &generation, &order[3], 4).await;
        }
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

    /// A base whose reranker buries what an answer drew on: the two top hits
    /// of the vector order, served reversed at the bottom of six. Nothing on
    /// the ladder can lift them — the replay without the reranker already has
    /// them at the top — so only the flip has anything to say.
    async fn seeded_with_a_burying_reranker() -> (Core, String) {
        let (core, order, _) = crate::eval::sweep::test_support::seeded_with_reranker().await;
        assert!(
            core.ranking.read().unwrap().rerank,
            "a configured reranker starts on"
        );
        let generation = generation_for(&core).await;
        for _ in 0..5 {
            observe(&core, &generation, &order[0], 6).await;
            observe(&core, &generation, &order[1], 5).await;
        }
        (core, generation)
    }

    #[tokio::test]
    async fn the_flip_is_asked_when_the_ladder_proposes_nothing_and_is_adopted_like_any_move() {
        let (mut core, parent) = seeded_with_a_burying_reranker().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let adopted = run(&core).await.unwrap().expect("the flip adopts");
        let live = core.store.live_generation().await.unwrap().unwrap();
        assert_eq!(live.id, adopted);
        assert!(!live.params.rerank, "{:?}", live.params);
        assert_eq!(live.parent_id.as_deref(), Some(parent.as_str()));
        assert!(live.predicted.unwrap_or(0.0) > 0.0);
        assert!(!core.ranking.read().unwrap().rerank, "serving follows");
        let run = core.store.latest_eval_run().await.unwrap().unwrap();
        assert_eq!(live.run_id.as_deref(), Some(run.id.as_str()));
        assert!(run.recommended);
        assert!(!run.best_params.rerank);
    }

    /// The rerank axis has to reach the replay, and only when the caller says
    /// so. Hard-coded `false` made the field inert: the flip candidate
    /// `tune::propose` emits differs from `current` in that field alone, so
    /// both sides replayed identically, `loses_to` was never true, and the flip
    /// cleared the refusal gate whatever it would actually have done.
    #[tokio::test]
    async fn the_rerank_flag_reaches_the_replay_and_only_when_it_is_asked_for() {
        use crate::eval::rehearsed::{probe_set, rehearsed_under};
        let (core, _order, reranker) =
            crate::eval::sweep::test_support::seeded_with_reranker().await;
        let generation = generation_for(&core).await;
        let probes = probe_set(&core, &generation).await.unwrap();
        assert!(
            !probes.is_empty(),
            "the fixture must give this something to replay"
        );
        let params = *core.ranking.read().unwrap();

        let before = reranker.calls();
        rehearsed_under(&core, params, &probes, None, false)
            .await
            .unwrap()
            .expect("nobody came back");
        assert_eq!(
            reranker.calls(),
            before,
            "held constant across a comparison, the reranker is not run"
        );

        rehearsed_under(&core, params, &probes, None, true)
            .await
            .unwrap()
            .expect("nobody came back");
        assert!(
            reranker.calls() > before,
            "and where the axis is what is under test, it is"
        );
    }

    #[tokio::test]
    async fn a_ladder_winner_is_taken_before_the_flip_is_asked() {
        // Observations a cap can lift, and a reranker that buries them: the
        // cap is measured first and wins; the flip is not asked in this pass.
        let (core, order, reranker) =
            crate::eval::sweep::test_support::seeded_with_reranker().await;
        let generation = generation_for(&core).await;
        for _ in 0..5 {
            observe(&core, &generation, &order[3], 3).await;
            observe(&core, &generation, &order[4], 2).await;
        }
        let mut core = core;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let before = reranker.calls();
        run(&core).await.unwrap().expect("the cap clears");
        let live = core.store.live_generation().await.unwrap().unwrap();
        assert!(
            live.params.rerank,
            "the flip was not the move: {:?}",
            live.params
        );
        assert_eq!(
            reranker.calls(),
            before,
            "the ladder spends no reranker call"
        );
    }

    #[test]
    fn a_band_used_more_than_the_tail_grows_one_rung_and_less_shrinks_one() {
        use crate::store::feedback::BandUse;
        let u = |band_used, tail_used| BandUse {
            band_used,
            tail_used,
        };
        assert_eq!(next_spread(3, u(4, 2)), Some(5), "two net events: grow");
        assert_eq!(next_spread(3, u(2, 4)), Some(2), "two net events: shrink");
        assert_eq!(
            next_spread(3, u(3, 2)),
            None,
            "one event could account for it"
        );
        assert_eq!(next_spread(3, u(3, 3)), None, "equal use holds");
        assert_eq!(
            next_spread(8, u(9, 0)),
            None,
            "the top rung has nowhere to grow"
        );
        assert_eq!(
            next_spread(1, u(0, 5)),
            Some(0),
            "and the bottom rung is off"
        );
        assert_eq!(
            next_spread(0, u(0, 0)),
            None,
            "from zero, with nothing reaching the end of a list, nothing moves"
        );
        assert_eq!(
            next_spread(0, u(0, 2)),
            Some(1),
            "two opens of the last hit shown is a list that ends too soon"
        );
        assert_eq!(
            next_spread(4, u(9, 0)),
            None,
            "a hand-set value off the ladder holds"
        );
    }

    /// One captured search under `generation` with `ranked` shown hits and
    /// `band` appended ones, opened on the artifact at `open`, which names a
    /// row of either kind.
    async fn captured_and_opened(core: &Core, ranked: &[&str], band: &[&str], open: &str) {
        use crate::store::feedback::{Door, NewCandidate, NewEvent};
        let candidates = ranked
            .iter()
            .map(|a| NewCandidate {
                artifact_id: (*a).to_string(),
                score: 1.0,
                similarity: Some(0.9),
                shown: true,
                ..Default::default()
            })
            .chain(band.iter().map(|a| NewCandidate {
                artifact_id: (*a).to_string(),
                shown: true,
                band: true,
                ..Default::default()
            }))
            .collect();
        let id = core
            .store
            .record_search(
                NewEvent {
                    query: QUERY.into(),
                    door: Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: core.embedder.embed_query(QUERY).await.unwrap(),
                    embed_model: "fake".into(),
                    candidates,
                    answered: false,
                    fold_onto: None,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        assert!(core.store.open_event(&id, open).await.unwrap());
    }

    #[tokio::test]
    async fn the_band_reader_tells_a_band_open_from_a_tail_open_from_a_top_open() {
        let (core, order) = seeded().await;
        let generation = generation_for(&core).await;
        let ranked: Vec<&str> = order[..3].iter().map(String::as_str).collect();
        let band = [order[5].as_str()];
        captured_and_opened(&core, &ranked, &band, &order[5]).await;
        let u = core.store.band_use(&generation, 1).await.unwrap();
        assert_eq!((u.band_used, u.tail_used), (1, 0));

        captured_and_opened(&core, &ranked, &band, &order[2]).await;
        let u = core.store.band_use(&generation, 1).await.unwrap();
        assert_eq!(
            (u.band_used, u.tail_used),
            (1, 1),
            "the last ranked hit is the tail"
        );

        captured_and_opened(&core, &ranked, &band, &order[0]).await;
        let u = core.store.band_use(&generation, 1).await.unwrap();
        assert_eq!((u.band_used, u.tail_used), (1, 1), "the top is neither");
    }

    #[tokio::test]
    async fn a_base_whose_band_is_used_more_than_its_tail_widens_the_band_when_nothing_else_moves()
    {
        let (mut core, parent) = seeded_with_nothing_to_gain().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let spread = core.ranking.read().unwrap().spread_max;
        assert_eq!(spread, 3, "the shipped rung");
        // Four opens on the band, two on the tail: two net events, the band
        // earned a wider rung. The ladder sees observations at the top of the
        // list and proposes nothing.
        let ranked: Vec<&str> = order_of(&core).await;
        let band = [ranked[4], ranked[5]];
        for _ in 0..4 {
            captured_and_opened(&core, &ranked[..3], &band, ranked[5]).await;
        }
        for _ in 0..2 {
            captured_and_opened(&core, &ranked[..3], &band, ranked[2]).await;
        }
        let adopted = run(&core).await.unwrap().expect("spread grows");
        let live = core.store.live_generation().await.unwrap().unwrap();
        assert_eq!(live.id, adopted);
        assert_eq!(live.params.spread_max, 5);
        assert_eq!(live.parent_id.as_deref(), Some(parent.as_str()));
        assert!(live.run_id.is_none(), "a lived adoption names no run");
        assert!(live.predicted.is_some());
        assert_eq!(
            core.ranking.read().unwrap().spread_max,
            5,
            "serving follows"
        );
    }

    /// `judged` pairs in a score band, `acted` of them with a dedupe action
    /// naming the pair — which is what `band_record` counts.
    async fn judged_band(core: &Core, score: f32, judged: usize, acted: usize) {
        use crate::store::actions::{Job, Kind, NewAction};
        use crate::store::pairs::{DecidedBy, PairState};
        let src = core.store.insert_corpus("band", "web", None).await.unwrap();
        for i in 0..judged {
            let rows: Vec<crate::store::artifacts::NewArtifact> = (0..2)
                .map(|k| crate::store::artifacts::NewArtifact {
                    ordinal: (i * 2 + k) as i64,
                    text: format!("band {score} pair {i} side {k}"),
                    ..Default::default()
                })
                .collect();
            let made = core.store.insert_artifacts(&src.id, &rows).await.unwrap();
            core.store
                .record_pair(&made[0].id, &made[1].id, score)
                .await
                .unwrap();
            let pair = core
                .store
                .pair_between(&made[0].id, &made[1].id)
                .await
                .unwrap()
                .unwrap()
                .id;
            core.store
                .set_pair_state(pair, PairState::NoConflict, None, DecidedBy::Model)
                .await
                .unwrap();
            if i < acted {
                core.store
                    .record_action(&NewAction {
                        job: Job::Dedupe,
                        kind: Kind::Supersede,
                        subject_id: made[0].id.clone(),
                        survivor_id: Some(made[1].id.clone()),
                        detail: None,
                        evidence: serde_json::json!({ "pair_id": pair }),
                        pair_score: Some(score),
                    })
                    .await
                    .unwrap();
            }
        }
    }

    /// The review threshold's rule does not read observations, and is no longer
    /// gated on them.
    ///
    /// `propose` returned as soon as `observation_pairs` came back empty, which
    /// is right for the ladder and the flip — both rank the positives, and
    /// there are none. This rule reads the judged bands off `artifact_pairs`
    /// and `corpus_actions`: how often the lowest band it admits leads
    /// anywhere, against the band above it. Nothing in that is an observation.
    ///
    /// So a base worked through the dedupe queue but whose searching produced
    /// no `Cited` or `Opened` rows under the live generation could never move
    /// `review_min` — held behind evidence the rule does not use, which is also
    /// why nothing but `next_review_min`'s arithmetic was ever tested.
    #[tokio::test]
    async fn the_review_threshold_moves_with_no_observations_under_the_live_generation() {
        let (core, order) = seeded().await;
        let _ = order;
        let mut core = core;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let live = generation_for(&core).await;
        assert_eq!(
            core.ranking.read().unwrap().review_min,
            0.88,
            "the shipped rung"
        );
        assert!(
            crate::eval::sweep::observation_pairs(&core, &live)
                .await
                .unwrap()
                .0
                .is_empty(),
            "this generation must carry no observations"
        );

        // The lowest admitted band acts 8 in 10; the band above it 9 in 10 —
        // within one decision of each other, so the line is drawn too high.
        judged_band(&core, 0.90, 10, 8).await;
        judged_band(&core, 0.93, 10, 9).await;

        run(&core).await.unwrap().expect("the threshold steps down");
        assert_eq!(
            core.ranking.read().unwrap().review_min,
            0.84,
            "the rung below 0.88"
        );
    }

    /// The first spread rung is offered once, and a revert is what remembers
    /// it — not the rule, which would hand it back every time it is asked.
    ///
    /// From `spread_max = 0` there is no band to measure, so `next_spread`
    /// reads the tail alone and offers the first rung for as long as the last
    /// hit shown goes on being opened. What stops that becoming a cycle — adopt, watch, revert, adopt again,
    /// with `pass` returning early for the whole of every watch and no other
    /// axis ever getting a turn — is `tried_candidates`, which reads the
    /// `reverted` state off the row. This test is that claim, exercised through
    /// an actual revert rather than a fabricated list.
    #[tokio::test]
    async fn the_first_spread_rung_is_not_offered_again_once_it_has_been_taken_back() {
        let (mut core, _) = seeded_with_nothing_to_gain().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        // A base sitting at the off rung, live and recorded as such.
        core.ranking.write().unwrap().spread_max = 0;
        // Read into a local: a guard living inside the call expression is held
        // across the await, and the future is then not `Send`.
        let at_zero_params = *core.ranking.read().unwrap();
        let at_zero = core
            .store
            .adopt_generation_lived(
                &NewGeneration {
                    params: at_zero_params.into(),
                    embed_recipe: "recipe-a".into(),
                    chat_model: "qwen".into(),
                    ..Default::default()
                },
                0.0,
            )
            .await
            .unwrap()
            .expect("nothing names a parent");
        rehearsed_once(&core, &at_zero).await;
        // Two opens of the last hit the list showed. Without them the off rung
        // has nothing arguing for it and the pass offers nothing at all — the
        // first rung used to be adopted on no evidence whatever.
        let ranked: Vec<&str> = order_of(&core).await;
        for _ in 0..2 {
            captured_and_opened(&core, &ranked[..3], &[], ranked[2]).await;
        }

        let first = run(&core)
            .await
            .unwrap()
            .expect("the first rung is offered");
        assert_eq!(
            core.store
                .live_generation()
                .await
                .unwrap()
                .unwrap()
                .params
                .spread_max,
            1,
            "from zero the first rung is tried"
        );

        // The watch's verdict, applied directly: the band earned nothing.
        core.store.revert_generation(&first).await.unwrap();
        let back = core.store.live_generation().await.unwrap().unwrap();
        *core.ranking.write().unwrap() = back.params.into();
        assert_eq!(
            back.params.spread_max, 0,
            "the revert put the off rung back"
        );

        let again = run(&core).await.unwrap();
        assert_eq!(
            core.store
                .live_generation()
                .await
                .unwrap()
                .unwrap()
                .params
                .spread_max,
            0,
            "the rung that was just taken back was offered again: {again:?}"
        );
    }

    #[tokio::test]
    async fn a_band_nobody_uses_narrows_one_rung() {
        let (mut core, _) = seeded_with_nothing_to_gain().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let ranked: Vec<&str> = order_of(&core).await;
        let band = [ranked[4], ranked[5]];
        for _ in 0..3 {
            captured_and_opened(&core, &ranked[..3], &band, ranked[2]).await;
        }
        run(&core).await.unwrap().expect("spread shrinks");
        assert_eq!(core.ranking.read().unwrap().spread_max, 2);
    }

    async fn order_of(core: &Core) -> Vec<&'static str> {
        crate::eval::sweep::test_support::ranks_order(core)
            .await
            .into_iter()
            .map(|s| Box::leak(s.into_boxed_str()) as &'static str)
            .collect()
    }

    #[test]
    fn the_review_threshold_steps_down_when_its_lowest_band_acts_like_the_band_above_and_up_when_its_actions_are_taken_back_more()
     {
        use crate::store::pairs::BandRecord;
        let b = |judged, acted, undone| BandRecord {
            judged,
            acted,
            undone,
        };
        // short: low acts 8/10, above 9/10 — within one decision → down a rung
        assert_eq!(
            next_review_min(0.88, 0.95, b(10, 8, 0), b(10, 9, 0)),
            Some(0.84)
        );
        // low acts 2/10 against 9/10 → hold
        assert_eq!(next_review_min(0.88, 0.95, b(10, 2, 0), b(10, 9, 0)), None);
        // wrong: low's actions taken back 4/8, above's 0/9 → up a rung
        assert_eq!(
            next_review_min(0.88, 0.95, b(10, 8, 4), b(10, 9, 0)),
            Some(0.92)
        );
        // wrong wins over short when both fire
        assert_eq!(
            next_review_min(0.84, 0.95, b(10, 9, 5), b(10, 9, 0)),
            Some(0.88)
        );
        // a rung at or above auto_supersede is never offered
        assert_eq!(next_review_min(0.92, 0.93, b(10, 8, 4), b(10, 9, 0)), None);
        assert_eq!(next_review_min(0.88, 0.92, b(10, 8, 4), b(10, 9, 0)), None);
        // nothing judged in a band is no evidence
        assert_eq!(next_review_min(0.88, 0.95, b(0, 0, 0), b(10, 9, 0)), None);
        // a hand-set value off the ladder holds
        assert_eq!(next_review_min(0.85, 0.95, b(10, 8, 0), b(10, 9, 0)), None);
        // the bottom rung cannot step down
        assert_eq!(next_review_min(0.80, 0.95, b(10, 9, 0), b(10, 9, 0)), None);
        // a band too thin to say anything does not step down. Both bands act
        // every time here, which is `short` at its strongest — and on one
        // judged pair a side it is still nothing.
        assert_eq!(next_review_min(0.88, 0.95, b(1, 1, 0), b(1, 1, 0)), None);
        assert_eq!(next_review_min(0.88, 0.95, b(9, 9, 0), b(10, 10, 0)), None);
        assert_eq!(
            next_review_min(0.88, 0.95, b(10, 10, 0), b(10, 10, 0)),
            Some(0.84),
            "and does once both bands are wide enough"
        );
        // `wrong` needs no such floor: it asks the difference to exceed the
        // noise, so a thin band cannot fire it either way.
        assert_eq!(next_review_min(0.88, 0.95, b(1, 1, 1), b(1, 1, 0)), None);
    }

    /// `n` pairs in `[lo, hi)` settled by the judge, `acted` of them with a
    /// journal row, `undone` of those taken back.
    async fn band(core: &Core, lo: f32, n: usize, acted: usize, undone: usize) {
        use crate::store::actions::{Job, Kind, NewAction, UndoneBy};
        use crate::store::pairs::{DecidedBy, PairState};
        for i in 0..n {
            let score = lo + 0.001 * i as f32;
            let src = core.store.insert_corpus("x", "web", None).await.unwrap();
            let made = core
                .store
                .insert_artifacts(
                    &src.id,
                    &[
                        crate::store::artifacts::NewArtifact {
                            text: format!("one {score}"),
                            ..Default::default()
                        },
                        crate::store::artifacts::NewArtifact {
                            ordinal: 1,
                            text: format!("two {score}"),
                            ..Default::default()
                        },
                    ],
                )
                .await
                .unwrap();
            core.store
                .record_pair(&made[0].id, &made[1].id, score)
                .await
                .unwrap();
            let id = core
                .store
                .pairs_by_state(PairState::Pending, 100)
                .await
                .unwrap()
                .into_iter()
                .find(|p| (p.score - score).abs() < 1e-6)
                .unwrap()
                .id;
            core.store
                .set_pair_state(id, PairState::Dismissed, None, DecidedBy::Model)
                .await
                .unwrap();
            if i < acted {
                let subject = format!("s{score}");
                core.store
                    .record_action(&NewAction {
                        job: Job::Dedupe,
                        kind: Kind::Supersede,
                        subject_id: subject.clone(),
                        survivor_id: None,
                        detail: None,
                        // The pair, because `band_record` counts pairs.
                        evidence: serde_json::json!({ "pair_id": id }),
                        pair_score: Some(score),
                    })
                    .await
                    .unwrap();
                if i < undone {
                    core.store
                        .undo_action_on(&subject, Kind::Supersede, UndoneBy::Evidence, "lost")
                        .await
                        .unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn a_base_whose_lowest_band_acts_like_the_one_above_lowers_the_review_threshold_when_nothing_else_moves()
     {
        let (mut core, parent) = seeded_with_nothing_to_gain().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        assert_eq!(core.ranking.read().unwrap().review_min, 0.88);
        band(&core, 0.88, 10, 8, 0).await;
        band(&core, 0.92, 10, 9, 0).await;

        let adopted = run(&core).await.unwrap().expect("review_min steps down");
        let live = core.store.live_generation().await.unwrap().unwrap();
        assert_eq!(live.id, adopted);
        assert_eq!(live.params.review_min, 0.84);
        assert_eq!(live.parent_id.as_deref(), Some(parent.as_str()));
        assert!(live.run_id.is_none());
        assert_eq!(
            core.ranking.read().unwrap().review_min,
            0.84,
            "relate reads it from here"
        );
    }

    #[tokio::test]
    async fn a_lowest_band_whose_actions_are_taken_back_raises_the_review_threshold() {
        let (mut core, _) = seeded_with_nothing_to_gain().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        band(&core, 0.88, 10, 8, 4).await;
        band(&core, 0.92, 10, 9, 0).await;

        run(&core).await.unwrap().expect("review_min steps up");
        assert_eq!(core.ranking.read().unwrap().review_min, 0.92);
    }

    #[tokio::test]
    async fn a_pass_with_autonomy_off_changes_nothing() {
        let (mut core, before) = seeded_with_observations().await;
        core.evolve.autonomous = crate::config::Autonomy::Off;
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
        core.evolve.autonomous = crate::config::Autonomy::Full;
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
        core.evolve.autonomous = crate::config::Autonomy::Full;
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
        core.evolve.autonomous = crate::config::Autonomy::Full;
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
                    event_id: None,
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

    /// An Apply pressed while the pass was measuring is not written over by
    /// what the pass decided from that measurement.
    #[tokio::test]
    async fn an_apply_that_lands_while_the_pass_measures_is_not_written_over() {
        let (core, parent) = adopted_and_watching().await;
        observe_badly_under_live(&core, 16).await;
        let watched = core.store.live_generation().await.unwrap().unwrap();
        let new = lived(&core, &watched.id).await.unwrap();
        let old = lived(&core, &parent).await.unwrap();
        assert!(
            !holds_up(&new, &old),
            "a generation the pass would take back"
        );

        // The Apply, the way `insights::tune_apply` makes it: the running
        // parameters first, then the journal.
        let was: crate::core::ranking::RankingParams = watched.params.into();
        let applied_params = crate::core::ranking::RankingParams {
            recency_weight: was.recency_weight + 0.3,
            ..was
        };
        *core.ranking.write().unwrap() = applied_params;
        let applied = crate::store::generations::restate_generation(
            &core.store,
            &watched,
            applied_params.into(),
        )
        .await
        .unwrap();

        // What the pass does next, on what it read before the press.
        let p = revert(&core, &watched, &new, &old).await.unwrap();
        assert!(p.reverted.is_none(), "{p:?}");
        let wider = crate::core::ranking::RankingParams {
            spread_max: crate::core::ranking::SPREADS
                .iter()
                .copied()
                .find(|s| *s != was.spread_max)
                .unwrap(),
            ..was
        };
        assert!(
            adopt_lived(&core, &watched, was, wider, 0.5)
                .await
                .unwrap()
                .is_none()
        );

        let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM generations WHERE state = 'live'")
            .fetch_one(&core.store.pool)
            .await
            .unwrap();
        assert_eq!(live, 1, "two generations live at once");
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            applied.id
        );
        let running = *core.ranking.read().unwrap();
        assert_eq!(
            running, applied_params,
            "the Apply's parameters were replaced"
        );
    }

    /// A move the probes cannot see is not settled by them. The band is not
    /// replayed on `Door::Judge`, so a `spread_max` move replays identically on
    /// both sides, and identical replays over enough probes used to end the
    /// watch on the first pass — before anything had been observed under it.
    #[tokio::test]
    async fn a_band_move_is_not_settled_by_probes_that_replay_without_the_band() {
        let (mut core, parent) = seeded_with_observations().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let order = crate::eval::sweep::test_support::ranks_order(&core).await;
        assert_eq!(probes_the_cap_buries(&core, &parent, &order[2]).await, 30);
        let live = core.store.generation(&parent).await.unwrap().unwrap();
        let current = *core.ranking.read().unwrap();
        let wider = crate::core::ranking::RankingParams {
            spread_max: crate::core::ranking::SPREADS
                .iter()
                .copied()
                .find(|s| *s != current.spread_max)
                .unwrap(),
            ..current
        };
        let child = adopt_lived(&core, &live, current, wider, 0.5)
            .await
            .unwrap()
            .expect("the parent is live");
        // Less observed under the move than under its parent, and no
        // difference between the two: the lived half cannot settle it yet.
        observe(&core, &child, &order[3], 4).await;
        let runs = eval_runs(&core).await;

        let p = pass(&core).await.unwrap();
        assert!(p.adopted.is_none() && p.refused.is_none(), "{p:?}");
        assert_eq!(
            eval_runs(&core).await,
            runs,
            "the watch ended and the pass went looking for the next move"
        );
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            child
        );
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
        for _ in 0..5 {
            observe(&core, &generation, &order[3], 4).await;
            observe(&core, &generation, &order[4], 5).await;
        }
        core.evolve.autonomous = crate::config::Autonomy::Full;
        run(&core).await.unwrap().expect("a candidate cleared");
        let live = core.store.live_generation().await.unwrap().unwrap().id;
        // As many observations as the record it is measured against, on the
        // two excerpts the move did not touch: the leading source's own top
        // two, which sit where they sat under either parameter set. So the
        // parent does not clear the gate on the new generation's own
        // evidence — the promise was kept — and the ladder has nothing left
        // to lift either.
        for _ in 0..5 {
            observe(&core, &live, &order[0], 1).await;
            observe(&core, &live, &order[1], 2).await;
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
                        context: None,
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
                    event_id: None,
                })
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn a_base_whose_evidence_stopped_agreeing_suspends_itself() {
        let (mut core, before) = seeded_with_observations().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
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
                    context: None,
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
        core.evolve.autonomous = crate::config::Autonomy::Full;
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
        core.evolve.autonomous = crate::config::Autonomy::Full;
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
        core.evolve.autonomous = crate::config::Autonomy::Full;
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
