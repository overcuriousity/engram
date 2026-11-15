//! The idle pass: a quiet base moves its own ranking and takes the move back if it does not hold.
//! Adoption replays positives counterfactually; the watch reads what each generation earned.

use crate::core::Core;
use crate::error::Result;
use crate::eval::lived::{holds_up, lived, settled};
use crate::eval::sweep;
use crate::store::decision_log::{ActRecord, AskRecord};
use crate::store::generations::{Generation, GenerationParams, NewGeneration};

/// A bound on work, not only the nearest rung: a tie keeps the current value, so a gain two rungs
/// out behind a tying rung would never be reached.
pub(crate) const BUDGET: usize = 20;

/// Flat counts, so `jobs::did_work` reads them.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Pass {
    pub adopted: Option<String>,
    pub reverted: Option<String>,
    /// Rule 1: a merge or supersession whose survivor was not found where the original was.
    pub undone: usize,
    pub restored: usize,
    /// Counted only: `sleep::interference` files nothing.
    pub interference: usize,
    pub condensed: usize,
    pub integrated: crate::jobs::sleep::Integrated,
    pub replayed: crate::jobs::sleep::Replayed,
    pub refused: Option<String>,
    /// `suspended`, `no_evidence` or `activity`; empty for a pass that ran to the end.
    pub stopped: &'static str,
}

/// `None` is the common and correct outcome.
pub async fn run(core: &Core) -> Result<Option<String>> {
    Ok(pass(core).await?.adopted)
}

/// Quiet is read off the base, not a ticker: no search and no question inside `evolve.idle_secs`.
pub async fn run_if_quiet(core: &Core) -> Result<Pass> {
    if !quiet(core).await? {
        return Ok(Pass::default());
    }
    // No inference; under "off" this is the whole of what a quiet base does.
    let started = crate::store::now();
    // Before anything reads the record: `lived` would count an orphaned observation as a hit.
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

/// Written here, where the phases converge; a base under "off" still slept.
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
            // Rows from before the detector was retired still carry a count.
            conflicts: 0,
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
    // The judgement lines rest on undos and answers, not on the ranking's evidence: they move
    // whatever the ranking half decides below.
    if let Err(e) = tune_questions(core).await {
        tracing::warn!(error = %e, "could not tune the judgement lines");
    }
    let Some(live) = core.store.live_generation().await? else {
        return Ok(Pass::default());
    };
    // When self-generated evidence stops agreeing with people, suspend: adopt nothing, revert
    // nothing.
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

    // Before anything measures: parameters that drifted from the generation would poison its
    // record.
    let current = *core
        .ranking
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if GenerationParams::from(current) != live.params {
        tracing::warn!(
            generation = %live.id,
            "the live generation does not describe the running parameters; the pass did nothing"
        );
        return Ok(Pass::default());
    }

    // Rehearse first: the corpus rules and the watch read what this writes.
    let started = crate::store::now();
    let replayed = crate::jobs::sleep::rehearse(core, &live, started).await?;
    if replayed.stopped {
        return Ok(Pass {
            replayed,
            stopped: "activity",
            ..Default::default()
        });
    }

    // A base with nothing to measure a move against does not move.
    let probes = crate::eval::rehearsed::probe_set(core, &live.id).await?;
    if agreement.is_none() && probes.is_empty() {
        tracing::warn!("no evidence on either side; the base is not moving");
        return Ok(Pass {
            replayed,
            stopped: "no_evidence",
            ..Default::default()
        });
    }

    // The corpus half runs before the ranking gates, under "full" only.
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

    // Under `learning` the ranking stays put, so the harness can measure an unmoved ranking.
    if !core.moves_ranking() {
        return Ok(out);
    }

    // The watch comes before any new proposal: one change at a time keeps the revert exact.
    if let (Some(parent_id), Some(_)) = (&live.parent_id, live.predicted)
        && let Some(parent) = core.store.generation(parent_id).await?
    {
        let new = lived(core, &live.id).await?;
        let old = lived(core, &parent.id).await?;
        // Probes replay through `Door::Judge` without priming, so band, lift, sitting and review
        // moves replay identically; the probe side is asked only where a replay can see the
        // difference.
        let mut rehearsal_lost = false;
        let mut rehearsal_settled = false;
        if !probes.is_empty()
            && crate::eval::rehearsed::probes_tell_apart(live.params.into(), parent.params.into())
        {
            // Reranked only where the generations disagree about it; otherwise it cancels out at a
            // call a probe.
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
                    // Probes within noise is the ordinary case and not the live evidence speaking:
                    // probes refuse and revert, they do not license the next move.
                    rehearsal_settled =
                        new.observations > 0 && (n.indistinguishable(&o) || o.loses_to(&n));
                }
                _ => {
                    out.stopped = "activity";
                    return Ok(out);
                }
            }
        }
        // `predicted` is a replay MRR delta, not comparable with the lived rate, so it is checked
        // on its own.
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

/// Re-runs the adoption's replay over the new generation's own observations: if its parent would
/// clear the gate there, the promise was not kept. Only for replay adoptions; `None` where somebody
/// came back.
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
        return Ok(Some(true));
    }
    let new: crate::core::ranking::RankingParams = live.params.into();
    let old: crate::core::ranking::RankingParams = parent.params.into();
    // The reranker only where the two disagree; held constant it cancels out at a call a pair.
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

/// Compared against the predecessor's lived record, never its offline number. `reverted` is a state
/// `tried_candidates` reads, so the base does not oscillate.
async fn revert(
    core: &Core,
    live: &Generation,
    new: &crate::eval::lived::Lived,
    old: &crate::eval::lived::Lived,
) -> Result<Pass> {
    let Some(back) = core.store.revert_generation(&live.id).await? else {
        // A reopen made another generation live while this pass measured; the base stays where it
        // is.
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

async fn propose(
    core: &Core,
    live: &Generation,
    current: crate::core::ranking::RankingParams,
    probes: &[crate::store::rehearsals::Rehearsal],
) -> Result<Pass> {
    let started = crate::store::now();
    let (pairs, skipped) = sweep::evidence_pairs(core, &live.id).await?;
    let tried = core
        .store
        .tried_candidates(&live.embed_recipe, &live.chat_model)
        .await?;
    if pairs.is_empty() {
        // `spread_step` and `review_step` read no observations, so they must not be gated behind
        // them.
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

    // The flip is scored against the served rank, so it is asked only when the ladder proposes
    // nothing.
    let mut run = scored.eval_run(&pairs, judged_count(core).await?, skipped);
    let winner = match scored.winner() {
        Some(w) => Some((w, scored.predicted().unwrap_or(0.0))),
        // `Stopped` means somebody is moving the baseline; falling through would rewrite
        // `core.ranking`.
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
                Some((flip.params, flip.predicted))
            }
            _ => None,
        },
    };

    // A reopen restated the file: the measured baseline is no longer running.
    if *core
        .ranking
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        != current
    {
        tracing::info!("ranking changed while the idle pass ran; its results were discarded");
        return Ok(Pass::default());
    }

    let run_id = core.store.record_eval_run(&run).await?;
    let Some((winner, predicted)) = winner else {
        return spread_step(core, live, current, &tried).await;
    };
    // Probes refuse a candidate that loses by more than a probe's worth; they never adopt.
    if !probes.is_empty() {
        // With the axis off a flip and its current are the same configuration, so `loses_to` cannot
        // see it.
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
                // Never measured against the probes, so nothing is adopted on it.
                return Ok(Pass {
                    stopped: "activity",
                    ..Default::default()
                });
            }
        }
    }
    adopt(core, live, current, winner, &run_id, predicted, pairs.len()).await
}

/// The running parameters' counterpart of the store's conditional writes: a reopen can swap them
/// after `propose` read them.
fn swap_ranking(
    core: &Core,
    from: crate::core::ranking::RankingParams,
    to: crate::core::ranking::RankingParams,
) {
    let mut running = core
        .ranking
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *running == from {
        *running = to;
    } else {
        tracing::info!("the ranking changed while the idle pass ran; it was left as it is");
    }
}

/// Grow when the band was used more than the ranked tail beside it by more than one event; shrink
/// the other way. From zero, two opens of the last hit shown argue for the first rung.
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

async fn spread_step(
    core: &Core,
    live: &Generation,
    current: crate::core::ranking::RankingParams,
    tried: &[GenerationParams],
) -> Result<Pass> {
    // At zero `band_use` reads nobody, so read one hit wide.
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

/// `short` asks a difference to stay within the noise, which a thin band satisfies on no evidence;
/// ten judged pairs a side puts the noise term at 0.2.
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

async fn review_step(
    core: &Core,
    live: &Generation,
    current: crate::core::ranking::RankingParams,
    tried: &[GenerationParams],
) -> Result<Pass> {
    use crate::core::ranking::REVIEW_MINS;
    // Under "full" only: the merges a lower threshold leads to are corpus writes no revert undoes.
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

/// `None` where `live` stopped being live while the pass ran (see `Store::adopt_generation`).
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
        // A reopen restated the file after the guard in `propose`; the baseline no longer runs.
        tracing::info!(
            generation = %live.id,
            "the live generation changed while the idle pass ran; its candidate was discarded"
        );
        return Ok(Pass::default());
    };
    swap_ranking(core, current, winner);
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

/// One step of a judgement line.
pub const STEP: f64 = 0.02;
/// Where an act line may move to.
const ACT_BOUNDS: (f64, f64) = (0.80, 0.99);
/// The floor of an ask line; its ceiling is a step under the act line.
const ASK_FLOOR: f64 = 0.50;
/// Where Decide's `settled_at` may move to.
const SETTLED_BOUNDS: (f64, f64) = (0.70, 0.95);

/// Lines are kept to two places, so a step lands on 0.92 and not on 0.9200000000000002.
fn two_places(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// With `tune_min` acted, an undone share above `undo_target` raises the line a step; under half of
/// it, with rows in the ask band a lower line would act on, lowers it. Never past `lo` or `hi`.
pub fn next_act_at(
    current: f64,
    r: ActRecord,
    tune_min: usize,
    undo_target: f64,
    lo: f64,
    hi: f64,
) -> Option<f64> {
    if r.acted < tune_min || r.acted == 0 {
        return None;
    }
    let share = r.undone as f64 / r.acted as f64;
    // A step that would cross a bound stops at it; at the bound, nothing moves.
    if share > undo_target && current < hi - 1e-9 {
        Some(two_places(current + STEP).min(hi))
    } else if share < undo_target / 2.0 && r.asks_open > 0 && current > lo + 1e-9 {
        Some(two_places(current - STEP).max(lo))
    } else {
        None
    }
}

/// A step up when a row lapsed since the ask line last moved; a step down when, since then, none
/// lapsed, some were answered and rows wait. Never under 0.50 by its own move, never within a step
/// of `act_at`: a rise stops a step under it, and an act line that came down this pass
/// (`act_moved`) takes the ask line with it. Nothing else moves it: a line put closer than a step
/// stays where it is until its own evidence or the act line moves it, and a lapse never lowers it.
pub fn next_ask_at(current: f64, act_at: f64, act_moved: bool, r: AskRecord) -> Option<f64> {
    let ceiling = two_places(act_at - STEP);
    let own = if r.lapsed_since_move > 0 {
        two_places(current + STEP).min(ceiling).max(current)
    } else if keeping_up(r) && current > ASK_FLOOR + 1e-9 {
        two_places(current - STEP).max(ASK_FLOOR)
    } else {
        current
    };
    let next = if act_moved { own.min(ceiling) } else { own };
    ((next - current).abs() > 1e-9).then_some(next)
}

fn acted_reason(r: ActRecord) -> String {
    format!(
        "{} of {} acted or answered were undone or corrected",
        r.undone, r.acted
    )
}

/// Since the ask line last moved: none lapsed, some answered, and rows wait for a seat.
fn keeping_up(r: AskRecord) -> bool {
    r.lapsed_since_move == 0 && r.answered_since_move > 0 && r.waiting > 0
}

fn lapsed_reason(lapsed: usize) -> String {
    match lapsed {
        1 => "a row lapsed unanswered".into(),
        n => format!("{n} rows lapsed unanswered"),
    }
}

/// What the ask line's own evidence says, worded to stand alone or after the act line's reason;
/// `None` when it has none.
fn ask_reason(r: AskRecord) -> Option<String> {
    match r.lapsed_since_move {
        0 if keeping_up(r) => Some(format!(
            "{} answered with none lapsed and {} waiting",
            r.answered_since_move, r.waiting
        )),
        0 => None,
        n => Some(lapsed_reason(n)),
    }
}

/// One line for one pass: the act line's reason, then the ask line's where its own evidence
/// moved it. `None` when neither line moved; an ask line the act line took down says nothing of
/// its own.
fn move_reason(
    t: crate::core::judging::band::Thresholds,
    act: Option<f64>,
    ask: Option<f64>,
    acted: ActRecord,
    asked: AskRecord,
) -> Option<String> {
    let act_at = act.unwrap_or(t.act_at);
    let own = ask.is_some() && ask == next_ask_at(t.ask_at, act_at, false, asked);
    let why = if own { ask_reason(asked) } else { None };
    match (act, ask, why) {
        (None, None, _) => None,
        (Some(_), _, Some(why)) => Some(format!("{}, and {why}", acted_reason(acted))),
        (Some(_), _, None) => Some(acted_reason(acted)),
        (None, Some(_), why) => why,
    }
}

/// Moves each line one step at most, for every question an applier carries out and for Decide's
/// `settled_at`; nothing while the base may not act on its own. Returns the moves made.
pub async fn tune_questions(core: &Core) -> Result<usize> {
    use crate::core::judging::band::{Thresholds, era};
    if !core.evolve.autonomous.acts_on_corpus() {
        return Ok(0);
    }
    let (min, target) = (core.decide_cfg.tune_min, core.decide_cfg.undo_target);
    let model = core.decider.model();
    let mut moved = 0;
    for a in crate::core::judging::catalogue::all() {
        if core.appliers.get(a.id).is_none() {
            continue;
        }
        let era = era(model, a.version);
        let t = core.thresholds(&a).await?;
        let acted = core.store.act_record(a.id, &era).await?;
        let act = next_act_at(t.act_at, acted, min, target, ACT_BOUNDS.0, ACT_BOUNDS.1);
        let act_at = act.unwrap_or(t.act_at);
        let asked = core.store.ask_record(a.id, &era).await?;
        let ask = next_ask_at(t.ask_at, act_at, act.is_some(), asked);
        let to = Thresholds {
            act_at,
            ask_at: ask.unwrap_or(t.ask_at),
        };
        if let Some(reason) = move_reason(t, act, ask, acted, asked) {
            core.store.move_lines(a.id, &era, t, to, &reason).await?;
            tracing::info!(question = a.id, %era, act_at = to.act_at, ask_at = to.ask_at, %reason, "a judgement line moved");
            moved += 1;
        }
        // Lapses the ask line could not rise on stand for the line as it is: left counted, they
        // would keep it from ever coming down. Its evidence starts over; nothing moved.
        if ask.is_none() && asked.lapsed_since_move > 0 {
            let reason = format!(
                "{}, the ask line already a step under the act line",
                lapsed_reason(asked.lapsed_since_move)
            );
            core.store.hold_ask_line(a.id, &era, to, &reason).await?;
            tracing::info!(question = a.id, %era, ask_at = to.ask_at, %reason, "the ask line was held");
        }
    }

    // Decide's line: a settled verdict judged wrong is its undo, one judged right is kept; held
    // decisions that leaned are what a lower line would settle. `split_at` and `bears_min` stay.
    let era = crate::core::decide::settled_era(model);
    let (judged, wrong) = core.store.settled_record(model).await?;
    let leaning = core.store.leaning_record(model).await?;
    let r = ActRecord {
        acted: judged,
        undone: wrong,
        asks_open: leaning,
    };
    let current = core.settled_at().await;
    if let Some(n) = next_act_at(current, r, min, target, SETTLED_BOUNDS.0, SETTLED_BOUNDS.1) {
        let reason = format!("{wrong} of {judged} settled were judged wrong");
        // Decide has no ask line; the column holds the settled line again.
        let to = Thresholds {
            act_at: n,
            ask_at: n,
        };
        core.store
            .move_thresholds(crate::core::decide::SETTLED_ID, &era, to, &reason)
            .await?;
        tracing::info!(%era, settled_at = n, %reason, "Decide's settled line moved");
        moved += 1;
    }
    Ok(moved)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::tests::{adopted_and_watching as watching, disagree_loudly};
    use crate::core::Core;

    pub(crate) async fn adopted_and_watching() -> (Core, String) {
        watching().await
    }

    pub(crate) async fn observe_badly_under_live(core: &Core, n: usize) {
        super::tests::observe_badly_under_live(core, n).await
    }

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

    async fn generation_for(core: &Core) -> String {
        let params = *core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

    async fn probes_the_cap_buries(core: &Core, generation: &str, buried: &str) -> usize {
        let buried = buried.to_string();
        let query_vec = core.embedder.embed_query(QUERY).await.unwrap();
        let mut n = 0;
        for i in 0..30 {
            let Some(pid) = core
                .store
                .record_rehearsal(&crate::store::rehearsals::NewRehearsal {
                    class: crate::store::rehearsals::Class::Cue,
                    // Distinct queries keep them past the unique index; the same vector makes the
                    // same replay.
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
        // Read before anything is adopted: `ranks_order` reads the live params.
        let buried = crate::eval::sweep::test_support::ranks_order(&core).await[2].clone();
        core.evolve.autonomous = crate::config::Autonomy::Full;
        run(&core).await.unwrap().expect("a candidate cleared");
        let live = core.store.live_generation().await.unwrap().unwrap();
        assert_eq!(live.parent_id.as_deref(), Some(parent.as_str()));
        assert_eq!(probes_the_cap_buries(&core, &live.id, &buried).await, 30);
        let p = pass(&core).await.unwrap();
        assert_eq!(p.reverted.as_deref(), Some(live.id.as_str()), "{p:?}");
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            parent
        );
    }

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

    pub(crate) async fn seeded_with_observations() -> (Core, String) {
        let (core, order) = seeded().await;
        let generation = generation_for(&core).await;
        // Ten for `sweep::MIN_PAIRS`; nine and one because `jobs::retract`'s pass tests weigh the
        // second excerpt's count.
        for _ in 0..9 {
            observe(&core, &generation, &order[3], 4).await;
        }
        observe(&core, &generation, &order[4], 5).await;
        (core, generation)
    }

    async fn seeded_with_nothing_to_gain() -> (Core, String) {
        let (core, order) = seeded().await;
        let generation = generation_for(&core).await;
        observe(&core, &generation, &order[0], 1).await;
        observe(&core, &generation, &order[1], 2).await;
        (core, generation)
    }

    async fn seeded_with_a_burying_reranker() -> (Core, String) {
        let (core, order, _) = crate::eval::sweep::test_support::seeded_with_reranker().await;
        assert!(
            core.ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .rerank,
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
        assert!(
            !core
                .ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .rerank,
            "serving follows"
        );
        let run = core.store.latest_eval_run().await.unwrap().unwrap();
        assert_eq!(live.run_id.as_deref(), Some(run.id.as_str()));
        assert!(!run.best_params.rerank);
    }

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
        let params = *core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

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
        let spread = core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .spread_max;
        assert_eq!(spread, 3, "the shipped rung");
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
            core.ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .spread_max,
            5,
            "serving follows"
        );
    }

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

    #[tokio::test]
    async fn the_review_threshold_moves_with_no_observations_under_the_live_generation() {
        let (core, order) = seeded().await;
        let _ = order;
        let mut core = core;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let live = generation_for(&core).await;
        assert_eq!(
            core.ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .review_min,
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

        judged_band(&core, 0.90, 10, 8).await;
        judged_band(&core, 0.93, 10, 9).await;

        run(&core).await.unwrap().expect("the threshold steps down");
        assert_eq!(
            core.ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .review_min,
            0.84,
            "the rung below 0.88"
        );
    }

    #[tokio::test]
    async fn the_first_spread_rung_is_not_offered_again_once_it_has_been_taken_back() {
        let (mut core, _) = seeded_with_nothing_to_gain().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        core.ranking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .spread_max = 0;
        // A guard inside the call expression would be held across the await, making the future not
        // `Send`.
        let at_zero_params = *core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

        core.store.revert_generation(&first).await.unwrap();
        let back = core.store.live_generation().await.unwrap().unwrap();
        *core
            .ranking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = back.params.into();
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
        assert_eq!(
            core.ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .spread_max,
            2
        );
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
        // Both bands act every time, `short` at its strongest, and one pair a side is still
        // nothing.
        assert_eq!(next_review_min(0.88, 0.95, b(1, 1, 0), b(1, 1, 0)), None);
        assert_eq!(next_review_min(0.88, 0.95, b(9, 9, 0), b(10, 10, 0)), None);
        assert_eq!(
            next_review_min(0.88, 0.95, b(10, 10, 0), b(10, 10, 0)),
            Some(0.84),
            "and does once both bands are wide enough"
        );
        // `wrong` needs no floor: it asks the difference to exceed the noise.
        assert_eq!(next_review_min(0.88, 0.95, b(1, 1, 1), b(1, 1, 0)), None);
    }

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
                        // `band_record` counts pairs.
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
        assert_eq!(
            core.ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .review_min,
            0.88
        );
        band(&core, 0.88, 10, 8, 0).await;
        band(&core, 0.92, 10, 9, 0).await;

        let adopted = run(&core).await.unwrap().expect("review_min steps down");
        let live = core.store.live_generation().await.unwrap().unwrap();
        assert_eq!(live.id, adopted);
        assert_eq!(live.params.review_min, 0.84);
        assert_eq!(live.parent_id.as_deref(), Some(parent.as_str()));
        assert!(live.run_id.is_none());
        assert_eq!(
            core.ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .review_min,
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
        assert_eq!(
            core.ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .review_min,
            0.92
        );
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
            GenerationParams::from(
                *core
                    .ranking
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            ),
            live.params,
            "and serve under it"
        );
        let run = core.store.latest_eval_run().await.unwrap().unwrap();
        assert_eq!(live.run_id.as_deref(), Some(run.id.as_str()));
        assert_eq!(
            run.best_params, live.params,
            "the journal names the settings the base took"
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
        assert_eq!(
            run.base_params, run.best_params,
            "the quiet pass is still journaled"
        );
    }

    pub(crate) async fn adopted_and_watching() -> (Core, String) {
        let (mut core, parent) = seeded_with_observations().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        run(&core).await.unwrap().expect("a candidate cleared");
        (core, parent)
    }

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
        sqlx::query_scalar!("SELECT COUNT(*) FROM eval_runs")
            .fetch_one(&core.store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_restatement_that_lands_while_the_pass_measures_is_not_written_over() {
        let (core, parent) = adopted_and_watching().await;
        observe_badly_under_live(&core, 16).await;
        let watched = core.store.live_generation().await.unwrap().unwrap();
        let new = lived(&core, &watched.id).await.unwrap();
        let old = lived(&core, &parent).await.unwrap();
        assert!(
            !holds_up(&new, &old),
            "a generation the pass would take back"
        );

        // Either order lands under the pass.
        let was: crate::core::ranking::RankingParams = watched.params.into();
        let applied_params = crate::core::ranking::RankingParams {
            recency_weight: was.recency_weight + 0.3,
            ..was
        };
        *core
            .ranking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = applied_params;
        let applied = crate::store::generations::restate_generation(
            &core.store,
            &watched,
            applied_params.into(),
        )
        .await
        .unwrap();

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

        let live: i64 =
            sqlx::query_scalar!("SELECT COUNT(*) FROM generations WHERE state = 'live'")
                .fetch_one(&core.store.pool)
                .await
                .unwrap();
        assert_eq!(live, 1, "two generations live at once");
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            applied.id
        );
        let running = *core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            running, applied_params,
            "the restated parameters were replaced"
        );
    }

    #[tokio::test]
    async fn a_band_move_is_not_settled_by_probes_that_replay_without_the_band() {
        let (mut core, parent) = seeded_with_observations().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let order = crate::eval::sweep::test_support::ranks_order(&core).await;
        assert_eq!(probes_the_cap_buries(&core, &parent, &order[2]).await, 30);
        let live = core.store.generation(&parent).await.unwrap().unwrap();
        let current = *core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        observe_badly_under_live(&core, 16).await;
        let adopted = core.store.live_generation().await.unwrap().unwrap();

        assert!(run(&core).await.unwrap().is_none());
        let live = core.store.live_generation().await.unwrap().unwrap();
        assert_eq!(live.id, parent, "the base put itself back");
        assert_eq!(
            GenerationParams::from(
                *core
                    .ranking
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            ),
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
    async fn a_learning_base_neither_adopts_nor_reverts() {
        let (mut core, before) = seeded_with_observations().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        core.learn.mode = crate::config::LearnMode::Learning;
        assert!(run(&core).await.unwrap().is_none(), "learning adopted");
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            before
        );

        let (mut core, _) = adopted_and_watching().await;
        observe_badly_under_live(&core, 16).await;
        let adopted = core.store.live_generation().await.unwrap().unwrap().id;
        core.learn.mode = crate::config::LearnMode::Learning;
        let p = pass(&core).await.unwrap();
        assert!(p.reverted.is_none(), "learning reverted: {p:?}");
        assert_eq!(
            core.store.live_generation().await.unwrap().unwrap().id,
            adopted
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
        let (mut core, order) = seeded().await;
        let generation = generation_for(&core).await;
        for _ in 0..5 {
            observe(&core, &generation, &order[3], 4).await;
            observe(&core, &generation, &order[4], 5).await;
        }
        core.evolve.autonomous = crate::config::Autonomy::Full;
        run(&core).await.unwrap().expect("a candidate cleared");
        let live = core.store.live_generation().await.unwrap().unwrap().id;
        // On the two excerpts the move did not touch, so the parent does not clear the gate.
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
        sqlx::query!(
            "UPDATE search_events SET created_at = ? WHERE id = ?",
            crate::store::now() + 5,
            &id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn a_pass_stops_when_somebody_comes_back_and_adopts_nothing() {
        // A search stamped just after the pass starts stands in for one landing mid-pass: same
        // predicate.
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
        let (mut core, _) = seeded_with_observations().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let id = somebody_returns(&core).await;
        assert!(run(&core).await.unwrap().is_none(), "interrupted");

        sqlx::query!(
            "UPDATE search_events SET created_at = ? WHERE id = ?",
            crate::store::now() - 5_000,
            &id
        )
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
        let (mut core, embedder) =
            crate::core::test_support::test_core_counting_embed_calls().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let generation = generation_for(&core).await;
        observe(&core, &generation, "art-1", 4).await;
        observe(&core, &generation, "art-2", 5).await;
        crate::eval::sweep::test_support::judge(&core, "art-1").await;
        let before = embedder.calls();

        run(&core).await.unwrap();
        assert_eq!(embedder.calls(), before, "the pass embedded a query");
    }

    #[tokio::test]
    async fn judged_answers_alone_can_move_the_base() {
        let (mut core, order) = seeded().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let generation = generation_for(&core).await;
        for _ in 0..5 {
            crate::eval::sweep::test_support::judge(&core, &order[3]).await;
            crate::eval::sweep::test_support::judge(&core, &order[4]).await;
        }

        let adopted = run(&core)
            .await
            .unwrap()
            .expect("the verdicts cleared the gate");
        let live = core.store.live_generation().await.unwrap().unwrap();
        assert_eq!(live.id, adopted);
        assert_eq!(live.parent_id.as_deref(), Some(generation.as_str()));
        assert!(
            live.params.per_source_cap.is_some(),
            "the improvement here is a cap, and the generation must carry it"
        );
        let run = core.store.latest_eval_run().await.unwrap().unwrap();
        assert_eq!(run.pairs_used, 10);
        assert_eq!(
            run.judged_count, 10,
            "the journal says what the base was judged on"
        );
    }

    #[test]
    fn the_pass_never_writes_the_operators_config_file() {
        let source = include_str!("tune.rs");
        let body = source.split("#[cfg(test)]").next().unwrap();
        assert!(!body.contains("std::fs"), "the pass writes a file");
        assert!(
            !body.contains("config_path"),
            "the pass knows where config.toml is"
        );
    }
}

#[cfg(test)]
mod question_tests {
    use super::*;
    use crate::config::Autonomy;
    use crate::core::judging::band::{Band, Thresholds};
    use crate::core::judging::{Asked, Subjects, catalogue};
    use crate::store::decision_log::{NewLogged, Outcome, State};

    const LO: f64 = 0.80;
    const HI: f64 = 0.99;

    fn act(acted: usize, undone: usize, asks_open: usize) -> ActRecord {
        ActRecord {
            acted,
            undone,
            asks_open,
        }
    }

    fn ask(lapsed: usize, answered: usize, open: usize, waiting: usize) -> AskRecord {
        AskRecord {
            lapsed_since_move: lapsed,
            answered_since_move: answered,
            open,
            waiting,
        }
    }

    async fn full() -> Core {
        let mut core = crate::core::test_support::test_core().await;
        core.evolve.autonomous = Autonomy::Full;
        core
    }

    /// `n` decisions of `q` in the core's era, each given `outcome`.
    async fn seed(
        core: &Core,
        q: &Asked,
        band: Band,
        state: State,
        outcome: Option<Outcome>,
        n: usize,
    ) {
        for i in 0..n {
            let id = core
                .store
                .log_decision(&NewLogged {
                    question_id: q.id.into(),
                    question_version: q.version,
                    model: core.decider.model().into(),
                    subject_kind: "pair",
                    subject_a: format!("a{i}"),
                    subject_b: None,
                    rev_a: None,
                    rev_b: None,
                    answer: "{}".into(),
                    top: 0.95,
                    pick: "yes".into(),
                    swapped_agree: None,
                    band,
                    reason: None,
                    state,
                })
                .await
                .unwrap();
            if let Some(o) = outcome {
                core.store.set_decision_outcome(&id, o, None).await.unwrap();
            }
        }
    }

    /// 41 acted pair decisions, 3 undone: a share above the target.
    async fn three_of_41_undone(core: &Core) {
        let q = catalogue::pair_relation();
        seed(core, &q, Band::Act, State::Done, Some(Outcome::Kept), 38).await;
        seed(core, &q, Band::Act, State::Done, Some(Outcome::Undone), 3).await;
    }

    #[test]
    fn too_few_acted_decisions_move_nothing() {
        assert_eq!(next_act_at(0.90, act(29, 10, 5), 30, 0.05, LO, HI), None);
        assert_eq!(next_act_at(0.90, act(29, 0, 5), 30, 0.05, LO, HI), None);
    }

    #[test]
    fn more_than_five_in_a_hundred_undone_raises_the_line_one_step() {
        assert_eq!(
            next_act_at(0.90, act(100, 6, 0), 30, 0.05, LO, HI),
            Some(0.92)
        );
        assert_eq!(next_act_at(0.90, act(100, 5, 3), 30, 0.05, LO, HI), None);
        assert_eq!(next_act_at(0.99, act(100, 6, 0), 30, 0.05, LO, HI), None);
        // A step past the bound stops at it.
        assert_eq!(
            next_act_at(0.98, act(100, 6, 0), 30, 0.05, LO, HI),
            Some(0.99)
        );
    }

    #[test]
    fn under_half_the_target_with_rows_waiting_lowers_it_one_step() {
        assert_eq!(
            next_act_at(0.90, act(60, 1, 3), 30, 0.05, LO, HI),
            Some(0.88)
        );
        assert_eq!(next_act_at(0.80, act(60, 1, 3), 30, 0.05, LO, HI), None);
        assert_eq!(
            next_act_at(0.81, act(60, 1, 3), 30, 0.05, LO, HI),
            Some(0.80)
        );
    }

    #[test]
    fn under_half_the_target_with_no_ask_band_stays() {
        assert_eq!(next_act_at(0.90, act(60, 1, 0), 30, 0.05, LO, HI), None);
        assert_eq!(next_act_at(0.90, act(60, 0, 0), 30, 0.05, LO, HI), None);
    }

    #[test]
    fn a_lapsed_row_raises_ask_at_and_a_cleared_table_with_rows_waiting_lowers_it() {
        assert_eq!(next_ask_at(0.60, 0.90, false, ask(1, 0, 5, 0)), Some(0.62));
        assert_eq!(next_ask_at(0.60, 0.90, false, ask(1, 4, 0, 3)), Some(0.62));
        assert_eq!(next_ask_at(0.60, 0.90, false, ask(0, 4, 0, 3)), Some(0.58));
        // Rows still open do not keep it up: answered, none lapsed and rows waiting brings it down.
        assert_eq!(next_ask_at(0.60, 0.90, false, ask(0, 4, 20, 3)), Some(0.58));
        // Nothing waiting, or nothing answered: it stays.
        assert_eq!(next_ask_at(0.60, 0.90, false, ask(0, 4, 0, 0)), None);
        assert_eq!(next_ask_at(0.60, 0.90, false, ask(0, 0, 0, 3)), None);
    }

    #[test]
    fn ask_at_stays_under_act_at_and_above_a_half() {
        assert_eq!(next_ask_at(0.88, 0.90, false, ask(1, 0, 0, 0)), None);
        // A lowered act line takes the ask line with it, whatever the ask band did.
        assert_eq!(
            next_ask_at(0.88, 0.88, true, AskRecord::default()),
            Some(0.86)
        );
        assert_eq!(next_ask_at(0.88, 0.88, true, ask(1, 0, 0, 0)), Some(0.86));
        assert_eq!(next_ask_at(0.50, 0.90, false, ask(0, 4, 0, 3)), None);
        assert_eq!(next_ask_at(0.51, 0.90, false, ask(0, 4, 20, 3)), Some(0.50));
        assert_eq!(next_ask_at(0.89, 0.92, true, ask(1, 0, 20, 0)), Some(0.90));
    }

    #[test]
    fn an_ask_line_nearer_than_a_step_stays_until_evidence_or_the_act_line_moves_it() {
        // Put a hundredth under the act line and nothing happened: nothing moves.
        assert_eq!(next_ask_at(0.89, 0.90, false, AskRecord::default()), None);
        // A lapse cannot raise it and never lowers it.
        assert_eq!(next_ask_at(0.89, 0.90, false, ask(1, 0, 0, 0)), None);
        // Its own evidence brings it down a whole step.
        assert_eq!(next_ask_at(0.89, 0.90, false, ask(0, 4, 0, 3)), Some(0.87));
        // An act line that came down takes it a step under.
        assert_eq!(
            next_ask_at(0.89, 0.88, true, AskRecord::default()),
            Some(0.86)
        );
    }

    #[test]
    fn one_pass_that_moves_both_lines_gives_one_reason() {
        let t = Thresholds {
            act_at: 0.90,
            ask_at: 0.60,
        };
        let acted = act(41, 3, 0);
        assert_eq!(
            move_reason(t, Some(0.92), Some(0.62), acted, ask(2, 0, 0, 0)).as_deref(),
            Some(
                "3 of 41 acted or answered were undone or corrected, and 2 rows lapsed unanswered"
            )
        );
        assert_eq!(
            move_reason(t, None, Some(0.62), acted, ask(1, 0, 0, 0)).as_deref(),
            Some("a row lapsed unanswered")
        );
        assert_eq!(
            move_reason(t, None, Some(0.58), acted, ask(0, 4, 0, 3)).as_deref(),
            Some("4 answered with none lapsed and 3 waiting")
        );
        // An ask line the act line took down says nothing of its own, lapses or not.
        let near = Thresholds {
            act_at: 0.90,
            ask_at: 0.88,
        };
        assert_eq!(
            move_reason(near, Some(0.88), Some(0.86), acted, ask(1, 0, 0, 0)).as_deref(),
            Some("3 of 41 acted or answered were undone or corrected")
        );
        assert_eq!(move_reason(t, None, None, acted, ask(0, 4, 0, 0)), None);
    }

    #[tokio::test]
    async fn a_move_is_recorded_and_is_in_force_at_the_next_decision() {
        let mut core = full().await;
        core.decider = std::sync::Arc::new(crate::infer::fake::RuleDecider::new(|_, qs| {
            Ok(crate::infer::decide::Answers {
                answers: qs
                    .iter()
                    .map(|q| {
                        (
                            q.key.clone(),
                            crate::infer::decide::Answer::Noul { yes: 0.02 },
                        )
                    })
                    .collect(),
                model: "rule".into(),
                input_tokens: 0,
            })
        }));
        let reap = catalogue::reap_worth();
        let subjects = |a: &str| Subjects {
            kind: "artifact",
            a: a.into(),
            b: None,
            rev_a: Some(1),
            rev_b: None,
        };
        // "no" at 0.98: under the starting line of 0.99, an ask.
        let before = crate::core::judging::ask(&core, &reap, subjects("x"), Some("s"))
            .await
            .unwrap();
        assert_eq!(before.band, Band::Ask);
        seed(
            &core,
            &reap,
            Band::Ask,
            State::Done,
            Some(Outcome::Confirmed),
            30,
        )
        .await;

        assert_eq!(tune_questions(&core).await.unwrap(), 1);
        let moves = core.store.threshold_moves(10).await.unwrap();
        assert_eq!(moves.len(), 1);
        assert_eq!(
            (moves[0].question_id.as_str(), moves[0].era.as_str()),
            ("reap_worth", "rule-decider/v1")
        );
        // The row asked above waits, none lapsed: the ask line comes down too.
        assert_eq!((moves[0].act_at, moves[0].ask_at), (0.97, 0.58));
        assert_eq!(
            moves[0].reason,
            "0 of 30 acted or answered were undone or corrected, and 30 answered with none lapsed and 1 waiting"
        );
        let after = crate::core::judging::ask(&core, &reap, subjects("y"), Some("s"))
            .await
            .unwrap();
        assert_eq!(after.band, Band::Act);
        // What the move rested on does not move it again.
        assert_eq!(tune_questions(&core).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_persons_answers_move_a_line_the_model_never_acted_on() {
        let core = full().await;
        let reap = catalogue::reap_worth();
        seed(
            &core,
            &reap,
            Band::Ask,
            State::Done,
            Some(Outcome::Confirmed),
            30,
        )
        .await;
        seed(&core, &reap, Band::Ask, State::Waiting, None, 2).await;
        let pair = catalogue::pair_relation();
        seed(
            &core,
            &pair,
            Band::Ask,
            State::Done,
            Some(Outcome::Confirmed),
            26,
        )
        .await;
        seed(
            &core,
            &pair,
            Band::Ask,
            State::Done,
            Some(Outcome::Corrected),
            4,
        )
        .await;
        assert_eq!(tune_questions(&core).await.unwrap(), 2);
        // Answered, none lapsed and two waiting: the ask line comes down a step too.
        assert_eq!(
            core.thresholds(&reap).await.unwrap(),
            Thresholds {
                act_at: 0.97,
                ask_at: 0.58
            }
        );
        assert_eq!(
            core.thresholds(&pair).await.unwrap(),
            Thresholds {
                act_at: 0.92,
                ask_at: 0.60
            }
        );
        let moves = core.store.threshold_moves(10).await.unwrap();
        let reason = |id: &str| {
            moves
                .iter()
                .find(|m| m.question_id == id)
                .unwrap()
                .reason
                .clone()
        };
        assert_eq!(
            reason("pair_relation"),
            "4 of 30 acted or answered were undone or corrected"
        );
        assert_eq!(
            reason("reap_worth"),
            "0 of 30 acted or answered were undone or corrected, and 30 answered with none lapsed and 2 waiting"
        );
    }

    #[tokio::test]
    async fn a_new_model_starts_from_config_again() {
        let mut core = full().await;
        three_of_41_undone(&core).await;
        assert_eq!(tune_questions(&core).await.unwrap(), 1);
        let pair = catalogue::pair_relation();
        let config = core.decide_cfg.bands.of(pair.id);
        assert_eq!(core.thresholds(&pair).await.unwrap().act_at, 0.92);
        assert_eq!(
            core.store.threshold_moves(1).await.unwrap()[0].reason,
            "3 of 41 acted or answered were undone or corrected"
        );
        // A new wording is a new era as well.
        let mut reworded = pair.clone();
        reworded.version += 1;
        assert_eq!(core.thresholds(&reworded).await.unwrap(), config);
        core.decider = std::sync::Arc::new(crate::infer::fake::RuleDecider::new(|_, _| {
            Err(crate::error::Error::Internal("not asked".into()))
        }));
        assert_eq!(core.thresholds(&pair).await.unwrap(), config);
        // Its record is empty: nothing moves for it.
        assert_eq!(tune_questions(&core).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn nothing_is_tuned_while_the_base_may_not_act() {
        let mut core = full().await;
        three_of_41_undone(&core).await;
        core.evolve.autonomous = Autonomy::Ranking;
        assert_eq!(tune_questions(&core).await.unwrap(), 0);
        pass(&core).await.unwrap();
        assert!(core.store.threshold_moves(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_idle_pass_moves_the_lines_under_full() {
        let core = full().await;
        three_of_41_undone(&core).await;
        pass(&core).await.unwrap();
        assert_eq!(core.store.threshold_moves(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_ask_only_move_leaves_the_act_lines_evidence() {
        let core = full().await;
        let reap = catalogue::reap_worth();
        let back = |secs: i64| {
            let store = core.store.clone();
            async move {
                // runtime: test-only backdating.
                sqlx::query(
                    "UPDATE decisions SET decided_at = ?, outcome_at = COALESCE(outcome_at, ?)",
                )
                .bind(crate::store::now() - secs)
                .bind(crate::store::now() - secs)
                .execute(&store.pool)
                .await
                .unwrap();
            }
        };
        seed(
            &core,
            &reap,
            Band::Ask,
            State::Done,
            Some(Outcome::Confirmed),
            25,
        )
        .await;
        seed(&core, &reap, Band::Ask, State::Lapsed, None, 1).await;
        // runtime: test-only stamp: the row lapsed now.
        sqlx::query("UPDATE decisions SET outcome_at = ? WHERE state = 'lapsed'")
            .bind(crate::store::now())
            .execute(&core.store.pool)
            .await
            .unwrap();
        back(100).await;
        assert_eq!(tune_questions(&core).await.unwrap(), 1);
        assert_eq!(
            core.thresholds(&reap).await.unwrap(),
            Thresholds {
                act_at: 0.99,
                ask_at: 0.62
            }
        );
        // runtime: test-only backdating of that move.
        sqlx::query("UPDATE decision_thresholds SET moved_at = ?")
            .bind(crate::store::now() - 50)
            .execute(&core.store.pool)
            .await
            .unwrap();
        seed(
            &core,
            &reap,
            Band::Ask,
            State::Done,
            Some(Outcome::Confirmed),
            5,
        )
        .await;
        seed(&core, &reap, Band::Ask, State::Waiting, None, 1).await;
        assert_eq!(tune_questions(&core).await.unwrap(), 1);
        assert_eq!(core.thresholds(&reap).await.unwrap().act_at, 0.97);
    }

    /// `reap_worth` configured to act from `act_at` and ask from `ask_at`.
    async fn reap_lines(act_at: f64, ask_at: f64) -> Core {
        let mut core = full().await;
        core.decide_cfg.bands.per.insert(
            "reap_worth".into(),
            crate::config::BandOverride {
                act_at: Some(act_at),
                ask_at: Some(ask_at),
            },
        );
        core
    }

    /// Every row the tuner wrote, moves and holds alike.
    async fn threshold_rows(core: &Core) -> i64 {
        sqlx::query_scalar!("SELECT COUNT(*) FROM decision_thresholds")
            .fetch_one(&core.store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn lapses_at_the_ceiling_start_the_ask_lines_evidence_over_so_it_can_come_down_later() {
        let core = reap_lines(0.90, 0.88).await;
        let reap = catalogue::reap_worth();
        let at = |act_at, ask_at| Thresholds { act_at, ask_at };
        seed(&core, &reap, Band::Ask, State::Lapsed, None, 2).await;
        // runtime: test-only stamp: the rows lapsed a while ago.
        sqlx::query("UPDATE decisions SET outcome_at = ? WHERE state = 'lapsed'")
            .bind(crate::store::now() - 100)
            .execute(&core.store.pool)
            .await
            .unwrap();
        // The line cannot rise: nothing moves, and the lapses are held against it once.
        assert_eq!(tune_questions(&core).await.unwrap(), 0);
        assert_eq!(core.thresholds(&reap).await.unwrap(), at(0.90, 0.88));
        assert!(core.store.threshold_moves(10).await.unwrap().is_empty());
        assert_eq!(threshold_rows(&core).await, 1);
        assert_eq!(tune_questions(&core).await.unwrap(), 0);
        assert_eq!(threshold_rows(&core).await, 1, "not on every pass");
        // runtime: test-only backdating of the hold.
        sqlx::query("UPDATE decision_thresholds SET moved_at = ?")
            .bind(crate::store::now() - 50)
            .execute(&core.store.pool)
            .await
            .unwrap();
        seed(
            &core,
            &reap,
            Band::Ask,
            State::Done,
            Some(Outcome::Confirmed),
            5,
        )
        .await;
        seed(&core, &reap, Band::Ask, State::Waiting, None, 1).await;
        // Answers since, none lapsed, a row waiting: the line comes down.
        assert_eq!(tune_questions(&core).await.unwrap(), 1);
        assert_eq!(core.thresholds(&reap).await.unwrap(), at(0.90, 0.86));
        assert_eq!(
            core.store.threshold_moves(10).await.unwrap()[0].reason,
            "5 answered with none lapsed and 1 waiting"
        );
    }

    #[tokio::test]
    async fn nothing_is_recorded_at_the_ceiling_when_nothing_lapsed() {
        let core = reap_lines(0.90, 0.88).await;
        let reap = catalogue::reap_worth();
        seed(
            &core,
            &reap,
            Band::Ask,
            State::Done,
            Some(Outcome::Confirmed),
            5,
        )
        .await;
        assert_eq!(tune_questions(&core).await.unwrap(), 0);
        assert_eq!(threshold_rows(&core).await, 0);
    }

    #[tokio::test]
    async fn an_ask_line_configured_nearer_than_a_step_is_not_moved_without_evidence() {
        let core = reap_lines(0.90, 0.89).await;
        let reap = catalogue::reap_worth();
        assert_eq!(tune_questions(&core).await.unwrap(), 0);
        assert_eq!(threshold_rows(&core).await, 0);
        assert_eq!(
            core.thresholds(&reap).await.unwrap(),
            Thresholds {
                act_at: 0.90,
                ask_at: 0.89
            }
        );
    }
}
