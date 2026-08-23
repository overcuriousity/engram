//! The background parameter sweep judgements pay for.
//!
//! The cargo harness freezes a corpus because its numbers must be comparable
//! across months. A recommendation asks a smaller question: what would *these*
//! pairs score under other settings, right now, against the base as it stands.
//! Baseline and candidates run in one pass over one index, so nothing needs
//! freezing and nothing needs re-embedding — the query cache means each
//! distinct query is embedded once and every candidate re-ranks the same
//! vectors.
//!
//! It reads the live index and only reads it. `Door::Judge` and `mark: false`
//! are the same discipline the judge page's assign search follows, for the same
//! reason: a sweep is not someone reading their notes, and its queries are
//! replayed in full knowledge of their answers.

use crate::core::Core;
use crate::core::ranking::RankingParams;
use crate::error::Result;
use crate::eval::metrics::{mrr, recall_at};
use crate::store::eval_runs::{DiffRow, NewEvalRun};

/// The `k` in recall@k, and the depth a rank is looked for in. The judge
/// page's own figure is recall@10; a sweep reporting recall@20 beside it would
/// be two numbers with one name.
const LIMIT: usize = 10;

/// The grid's axes. Both are scoring knobs — they reorder what retrieval
/// already returned — which is what makes a sweep cheap enough to run on a
/// verdict. The knobs that change the vector geometry (the embedding model and
/// its templates) cannot be swept at runtime at all, and stay with the cargo
/// harness.
const RECENCY: [f32; 5] = [0.0, 0.05, 0.1, 0.15, 0.25];
const CAPS: [Option<usize>; 4] = [Some(2), Some(3), Some(5), None];

/// Every combination, with the running configuration always among them: it is
/// the baseline everything else is measured against.
pub fn grid(current: RankingParams) -> Vec<RankingParams> {
    let mut out = Vec::with_capacity(RECENCY.len() * CAPS.len() + 1);
    for &recency_weight in &RECENCY {
        for &per_source_cap in &CAPS {
            out.push(RankingParams {
                recency_weight,
                per_source_cap,
            });
        }
    }
    if !out.contains(&current) {
        out.push(current);
    }
    out
}

/// Whether `cand` placed a pair better than `base` did. A miss loses to any
/// rank; two misses are equal.
fn better(cand: Option<usize>, base: Option<usize>) -> bool {
    match (cand, base) {
        (Some(c), Some(b)) => c < b,
        (Some(_), None) => true,
        _ => false,
    }
}

/// The gate, and the reason the whole feature is safe to run automatically.
///
/// An aggregate delta can be a single flipped pair wearing a percentage: on
/// fifty pairs one is two points of recall. Requiring two *net* better pairs
/// is what a change has to look like before it is worth a person's attention,
/// and refusing any candidate that costs either aggregate keeps a trade the
/// operator did not ask for from being presented as an improvement. Ties keep
/// the current values, always: a knob that moves nothing should keep its
/// default.
pub fn recommend(base: &[Option<usize>], cand: &[Option<usize>]) -> bool {
    let improved = base
        .iter()
        .zip(cand)
        .filter(|(b, c)| better(**c, **b))
        .count() as i64;
    let worsened = base
        .iter()
        .zip(cand)
        .filter(|(b, c)| better(**b, **c))
        .count() as i64;
    improved - worsened >= 2
        && recall_at(cand, LIMIT) >= recall_at(base, LIMIT)
        && mrr(cand) >= mrr(base)
}

/// One judged pair, with every id that satisfies it already resolved.
type Pair = (String, Vec<String>);

/// Rank every pair under one configuration.
async fn ranks_for(
    core: &Core,
    pairs: &[Pair],
    params: RankingParams,
) -> Result<Vec<Option<usize>>> {
    let mut ranks = Vec::with_capacity(pairs.len());
    for (query, satisfies) in pairs {
        let q = crate::core::search::SearchQuery {
            q: query.clone(),
            limit: LIMIT,
            tags: vec![],
            category: None,
            // Resurfacing reads `last_seen_at`, and a scored run is not
            // someone reading their notes.
            mark: false,
            include_deprecated: false,
            include_superseded: false,
        };
        let (results, _) = core
            .search_with_ranking(&q, params, crate::store::feedback::Door::Judge)
            .await?;
        ranks.push(
            results
                .iter()
                .position(|r| satisfies.iter().any(|id| id == &r.artifact_id)),
        );
    }
    Ok(ranks)
}

/// The pairs a sweep can actually replay, and how many it had to leave out.
async fn pairs_to_replay(core: &Core) -> Result<(Vec<Pair>, i64)> {
    let mut pairs = Vec::new();
    let mut skipped = 0;
    for p in core.store.judged_pairs().await? {
        // A deleted artifact is housekeeping, not a ranking result. Scored as
        // a miss it would look like a ranking failure for as long as the pair
        // exists; raised as an error it would stop every future sweep over one
        // deletion.
        match core.store.get_artifact(&p.expect).await {
            Ok(_) => {
                let satisfies = crate::eval::satisfied_by(core, &p.expect).await;
                pairs.push((p.query, satisfies));
            }
            Err(crate::error::Error::NotFound) => skipped += 1,
            Err(e) => return Err(e),
        }
    }
    Ok((pairs, skipped))
}

/// Rank the judged pairs under the whole grid and record what was found.
///
/// Runs whatever the thresholds say — `maybe_spawn` is what decides whether it
/// is due. Callable directly, which is what the tests do.
pub async fn run_sweep(core: &Core) -> Result<()> {
    let (pairs, skipped) = pairs_to_replay(core).await?;
    if pairs.is_empty() {
        return Ok(());
    }
    let judged = core.store.feedback_stats().await?.judged;
    let current = *core.ranking.read().expect("ranking lock");

    let base = ranks_for(core, &pairs, current).await?;
    let mut best: Option<(RankingParams, Vec<Option<usize>>)> = None;
    for cand in grid(current) {
        if cand == current {
            continue;
        }
        let ranks = ranks_for(core, &pairs, cand).await?;
        if !recommend(&base, &ranks) {
            continue;
        }
        // MRR first, recall as the tie-break: the gate has already refused
        // anything that costs either, so this only chooses among improvements.
        let beats = best.as_ref().is_none_or(|(_, b)| {
            mrr(&ranks) > mrr(b)
                || (mrr(&ranks) == mrr(b) && recall_at(&ranks, LIMIT) > recall_at(b, LIMIT))
        });
        if beats {
            best = Some((cand, ranks));
        }
    }

    let (winner, winning_ranks, recommended) = match best {
        Some((p, r)) => (p, r, true),
        // A quiet sweep still records itself: without the row a page can only
        // say nothing, which reads as "no sweep has ever run".
        None => (current, base.clone(), false),
    };
    let diff: Vec<DiffRow> = pairs
        .iter()
        .zip(base.iter().zip(&winning_ranks))
        .filter(|(_, (b, n))| b != n)
        .map(|((query, _), (b, n))| DiffRow {
            // The query names its own row, as it does in the harness's miss
            // list. No artifact text is written here.
            query: query.chars().take(48).collect(),
            base: *b,
            new: *n,
        })
        .collect();

    core.store
        .record_eval_run(&NewEvalRun {
            judged_count: judged,
            pairs_used: pairs.len() as i64,
            pairs_skipped: skipped,
            base: current.into(),
            base_recall: recall_at(&base, LIMIT),
            base_mrr: mrr(&base),
            best: winner.into(),
            best_recall: recall_at(&winning_ranks, LIMIT),
            best_mrr: mrr(&winning_ranks),
            diff,
            recommended,
        })
        .await?;
    Ok(())
}

/// Run a sweep if the judgements have paid for one.
///
/// Called after every verdict, off the request path: the verdict must not wait
/// on a grid of searches, and a sweep that fails must not fail the verdict.
pub fn maybe_spawn(core: &Core) {
    if !core.learn.enabled {
        return;
    }
    let core = core.clone();
    core.background.clone().spawn(async move {
        use std::sync::atomic::Ordering;
        // A run of verdicts is one sweep, not one each. Released on the way
        // out whatever happened, including the early returns below.
        if core.tuning.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(e) = sweep_if_due(&core).await {
            tracing::warn!(error = %e, "tuning sweep failed");
        }
        core.tuning.store(false, Ordering::SeqCst);
    });
}

async fn sweep_if_due(core: &Core) -> Result<()> {
    let tune = &core.feedback.tune;
    let judged = core.store.feedback_stats().await?.judged;
    if judged < tune.min_judgements {
        return Ok(());
    }
    // Paced by judgements rather than by the clock: what makes a sweep worth
    // repeating is new evidence, and nothing else about the base changes what
    // these two knobs do.
    if let Some(last) = core.store.latest_eval_run().await?
        && judged - last.judged_count < tune.resweep_after
    {
        return Ok(());
    }
    run_sweep(core).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_needs_two_net_better_pairs_and_no_aggregate_loss() {
        let base = vec![Some(5), Some(7), None, Some(0)];
        assert!(
            recommend(&base, &[Some(1), Some(2), None, Some(0)]),
            "two pairs climbed and none fell"
        );
        assert!(
            !recommend(&base, &[Some(1), Some(7), None, Some(0)]),
            "one pair is noise wearing a percentage"
        );
        assert!(
            !recommend(&base, &[Some(1), Some(2), None, None]),
            "two climbed but one was lost: net one"
        );
        assert!(!recommend(&base, &base), "a tie keeps the current values");
        assert!(
            !recommend(&base, &[Some(0), Some(0), None, Some(3)]),
            "two climbed and one fell out of the head: MRR must not pay for it"
        );
    }

    #[test]
    fn the_grid_always_contains_the_configuration_it_is_measured_against() {
        let shipped = RankingParams {
            recency_weight: 0.05,
            per_source_cap: Some(3),
        };
        assert!(grid(shipped).contains(&shipped));
        assert_eq!(grid(shipped).len(), RECENCY.len() * CAPS.len());

        // A hand-set value nobody would have guessed is still the baseline.
        let odd = RankingParams {
            recency_weight: 0.07,
            per_source_cap: Some(4),
        };
        assert!(grid(odd).contains(&odd));
        assert_eq!(grid(odd).len(), RECENCY.len() * CAPS.len() + 1);
    }
}
