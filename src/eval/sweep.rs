//! The background parameter sweep judgements pay for.
//!
//! The cargo harness freezes a corpus because its numbers must be comparable
//! across months. A recommendation asks a smaller question: what would *these*
//! pairs score under other settings, right now, against the base as it stands.
//! Baseline and candidates run in one pass over one index, so nothing needs
//! freezing and nothing needs re-embedding — the query cache means each
//! distinct query is embedded once, and every candidate is one vector read
//! over it, whether it reorders what came back or changes how much comes back.
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
                ..current
            });
        }
    }
    if !out.contains(&current) {
        out.push(current);
    }
    out
}

/// A bounded set of candidates, drawn a step at a time from the running
/// configuration rather than enumerated.
///
/// The grid is twenty candidates over two axes and every axis added multiplies
/// it; this is what the idle pass walks instead, over all four knobs of
/// `RankingParams`. The running configuration comes first — it is the baseline
/// — then its nearest neighbour on each axis, then the next step out on each,
/// until `budget` is spent. A parameter set already tried and taken back is
/// never offered.
///
/// Deliberately not a learned sampler. Neighbours-first is the whole
/// heuristic: a knob that helps usually helps a little, and the pass runs every
/// quiet period, so a long walk is reached in small steps that each get their
/// own watch. Every candidate moves exactly one knob off the baseline, which
/// is what keeps a result about caps from arriving wearing a recency change.
/// A reorder knob and a retrieval knob cost the pass the same — one vector
/// read per pair — so the axes are interleaved rather than ordered.
pub fn candidates(
    current: RankingParams,
    tried: &[crate::store::generations::GenerationParams],
    budget: usize,
) -> Vec<RankingParams> {
    use crate::core::ranking::{HALF_LIVES, MULTIPLIERS, PRIME_LIFTS};
    let recency = outward(
        &RECENCY,
        |v| *v < current.recency_weight,
        |v| *v == current.recency_weight,
    );
    let cap_key = |c: Option<usize>| c.unwrap_or(usize::MAX);
    let caps = outward(
        &CAPS,
        |v| cap_key(*v) < cap_key(current.per_source_cap),
        |v| *v == current.per_source_cap,
    );
    let multipliers = outward(
        &MULTIPLIERS,
        |v| *v < current.candidate_multiplier,
        |v| *v == current.candidate_multiplier,
    );
    let half_lives = outward(
        &HALF_LIVES,
        |v| *v < current.recency_half_life_days,
        |v| *v == current.recency_half_life_days,
    );
    let lifts = outward(
        &PRIME_LIFTS,
        |v| *v < current.prime_lift,
        |v| *v == current.prime_lift,
    );

    let mut out = vec![current];
    let longest = [
        recency.len(),
        caps.len(),
        multipliers.len(),
        half_lives.len(),
        lifts.len(),
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    for i in 0..longest {
        if let Some(per_source_cap) = caps.get(i) {
            out.push(RankingParams {
                per_source_cap: *per_source_cap,
                ..current
            });
        }
        if let Some(recency_weight) = recency.get(i) {
            out.push(RankingParams {
                recency_weight: *recency_weight,
                ..current
            });
        }
        if let Some(candidate_multiplier) = multipliers.get(i) {
            out.push(RankingParams {
                candidate_multiplier: *candidate_multiplier,
                ..current
            });
        }
        if let Some(recency_half_life_days) = half_lives.get(i) {
            out.push(RankingParams {
                recency_half_life_days: *recency_half_life_days,
                ..current
            });
        }
        if let Some(prime_lift) = lifts.get(i) {
            out.push(RankingParams {
                prime_lift: *prime_lift,
                ..current
            });
        }
    }
    out.retain(|c| {
        *c == current
            || !tried
                .iter()
                .any(|t| *t == crate::store::generations::GenerationParams::from(*c))
    });
    out.truncate(budget.max(1));
    out
}

/// The rungs of one ladder in order of distance from the current one, nearest
/// first and alternating sides, with the current rung left out.
fn outward<T: Copy>(
    ladder: &[T],
    below: impl Fn(&T) -> bool,
    current: impl Fn(&T) -> bool,
) -> Vec<T> {
    let lower: Vec<T> = ladder.iter().filter(|v| below(v)).rev().copied().collect();
    let upper: Vec<T> = ladder
        .iter()
        .filter(|v| !below(v) && !current(v))
        .copied()
        .collect();
    let mut out = Vec::with_capacity(lower.len() + upper.len());
    for i in 0..lower.len().max(upper.len()) {
        out.extend(lower.get(i));
        out.extend(upper.get(i));
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

/// How many knobs a candidate moves off the running configuration.
///
/// The last tie-break, and the reason it has to exist: `grid` is walked
/// recency-major from `RECENCY[0] = 0.0`, so among candidates whose rank
/// vectors are identical — the ordinary case, since most judged pairs carry no
/// recency signal at all — the lowest index wins, and that index is recency
/// zero. A cap change would then arrive with "recency 0.05 → 0.00" attached to
/// it, measured by nothing, and applying it would switch recency weighting off
/// on the strength of a result about caps. `recommend` already promises a knob
/// that moves nothing keeps its value; this is that promise held among the
/// candidates rather than only against the base.
fn moved(cand: RankingParams, current: RankingParams) -> usize {
    usize::from(cand.recency_weight != current.recency_weight)
        + usize::from(cand.per_source_cap != current.per_source_cap)
        + usize::from(cand.candidate_multiplier != current.candidate_multiplier)
        + usize::from(cand.recency_half_life_days != current.recency_half_life_days)
        + usize::from(cand.prime_lift != current.prime_lift)
        + usize::from(cand.spread_max != current.spread_max)
        + usize::from(cand.rerank != current.rerank)
}

/// One pair to replay: a query, every id that satisfies it already resolved,
/// and — where the pair came from an observation — the vector the query was
/// searched with, so replaying it costs no embedding.
#[derive(Debug, Clone)]
pub(crate) struct Pair {
    pub(crate) query: String,
    pub(crate) satisfies: Vec<String>,
    pub(crate) query_vec: Option<Vec<f32>>,
    /// What priming read when the search this came from ran, where it was
    /// recorded. Handed in on the Judge door so a rung of `prime_lift` can be
    /// replayed; a pair without one ties on that axis.
    pub(crate) priming: Option<crate::core::search::Priming>,
    /// Where the artifact was actually served, 0-based, for a pair that came
    /// from an observation. The rerank axis's base: the one row that has the
    /// reranker in it where the reranker is live. `None` for a judged pair.
    pub(crate) served: Option<usize>,
}

/// How many observations one sweep will draw on. A bound rather than a
/// setting: a sweep re-ranks every pair under every grid candidate, so the
/// work is pairs times grid, and a base that has been used for a year would
/// otherwise make one pass unbounded.
const OBSERVATION_LIMIT: usize = 500;

/// Where one configuration put the answer to one pair. `None` past `LIMIT`.
///
/// `rerank` is whether the reranker may run. The verdict-paid sweep measures
/// the pipeline as configured, reranker included, and lets the scope decide.
/// The idle pass may spend no inference at all, so it passes `false` and
/// measures the ordering that feeds the reranker where one serves search.
async fn rank_of(
    core: &Core,
    pair: &Pair,
    params: RankingParams,
    rerank: bool,
) -> Result<Option<usize>> {
    // The vector the query was actually searched with, handed to the cache so
    // the search below finds it there and embeds nothing.
    if let Some(v) = &pair.query_vec {
        core.remember_query_vector(&pair.query, v.clone());
    }
    let q = crate::core::search::SearchQuery {
        q: pair.query.clone(),
        limit: LIMIT,
        tags: vec![],
        category: None,
        // Resurfacing reads `last_seen_at`, and a scored run is not someone
        // reading their notes.
        mark: false,
        rerank,
        explain: false,
        include_deprecated: false,
        include_superseded: false,
    };
    let mut origin = crate::store::feedback::Origin::from(crate::store::feedback::Door::Judge);
    if let Some(p) = &pair.priming {
        origin = origin.primed_as(p.clone());
    }
    let (results, _) = core.search_with_ranking(&q, params, origin).await?;
    Ok(results
        .iter()
        .position(|r| pair.satisfies.iter().any(|id| id == &r.artifact_id)))
}

/// Every pair under every configuration, one row per configuration.
///
/// Query-major, and that is the whole point of the function. A pass per
/// configuration walks the same queries in the same order twenty-one times,
/// which is the one access pattern an insertion-ordered cache of
/// `QUERY_CACHE_CAPACITY` entries can never serve: past that many distinct
/// judged queries the hit rate was zero and every search in the grid embedded
/// its query again. Asking all twenty-one questions about one query before
/// moving to the next embeds it once, however many pairs there are.
async fn ranks_over_grid(
    core: &Core,
    pairs: &[Pair],
    grid: &[RankingParams],
    rerank: bool,
    stop_after: Option<i64>,
) -> Result<Option<Vec<Vec<Option<usize>>>>> {
    let mut ranks = vec![Vec::with_capacity(pairs.len()); grid.len()];
    for pair in pairs {
        // Between pairs, not between candidates: a pair is a handful of vector
        // reads, and whoever came back is behind at most that.
        if let Some(since) = stop_after
            && core.store.activity_since(since).await?
        {
            return Ok(None);
        }
        for (row, params) in ranks.iter_mut().zip(grid) {
            row.push(rank_of(core, pair, *params, rerank).await?);
        }
    }
    Ok(Some(ranks))
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
                pairs.push(Pair {
                    query: p.query,
                    satisfies,
                    query_vec: None,
                    priming: None,
                    served: None,
                });
            }
            Err(crate::error::Error::NotFound) => skipped += 1,
            Err(e) => return Err(e),
        }
    }

    // Beside the judged pairs, not instead of them. An excerpt an answer drew
    // on and a result somebody opened are the same claim a verdict makes —
    // this query was answered by that artifact — arrived at without anybody
    // being asked, and there are two orders of magnitude more of them.
    //
    // Positive observations only. A weak negative may take a setting back and
    // may never bring one about, and this is the bringing-about path.
    //
    // Both rules the loop above enforces apply unchanged, because these go
    // through the same `get_artifact` and the same `satisfied_by`: a merged
    // artifact is satisfied by what superseded it, and a deleted one is
    // skipped rather than scored as a miss.
    if core.evolve.feed_sweep
        && let Some(generation) = core.store.live_generation().await?
    {
        let (observed, left_out) = observation_pairs(core, &generation.id).await?;
        pairs.extend(observed);
        skipped += left_out;
    }

    Ok((pairs, skipped))
}

/// The positive observations under one generation, as pairs the ranking can be
/// scored on, and how many named an artifact that no longer exists.
///
/// Bounded at `OBSERVATION_LIMIT`, and within the bound prioritised by how
/// wrong the system was: the observations whose artifact sat furthest down the
/// list are replayed first. Replaying what surprised the system is what keeps a
/// pass over a well-used base about the cases with room to improve.
///
/// Each pair carries the vector the query was searched with, so a replay
/// embeds nothing.
pub(crate) async fn observation_pairs(
    core: &Core,
    generation_id: &str,
) -> Result<(Vec<Pair>, i64)> {
    let mut observations: Vec<_> = core
        .store
        .observations_for_generation(generation_id, OBSERVATION_LIMIT * 2)
        .await?
        .into_iter()
        .filter(|o| o.artifact_id.is_some() && o.strength > 0.0)
        .collect();
    // Worst-placed first; newest first among equals, which is the order they
    // arrived in.
    observations.sort_by_key(|o| std::cmp::Reverse(o.rank.unwrap_or(i64::MAX)));
    observations.truncate(OBSERVATION_LIMIT);

    let mut pairs = Vec::with_capacity(observations.len());
    let mut skipped = 0;
    for o in observations {
        let artifact = o.artifact_id.as_deref().expect("filtered above");
        match core.store.get_artifact(artifact).await {
            Ok(_) => {
                let satisfies = crate::eval::satisfied_by(core, artifact).await;
                let priming = match o.event_id.as_deref() {
                    Some(e) => core.store.search_context(e).await?,
                    None => None,
                };
                pairs.push(Pair {
                    query: o.query,
                    satisfies,
                    query_vec: Some(o.query_vec),
                    priming,
                    served: o.rank.map(|r| (r - 1).max(0) as usize),
                });
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
    let judged = core.store.feedback_stats(core.weak_below()).await?.judged;
    let current = *core.ranking.read().expect("ranking lock");

    // Never stopped: the verdict-paid sweep runs on the background lane and
    // answers to nobody's quiet period.
    let Some(scored) = score(core, &pairs, grid(current), current, true, None).await? else {
        return Ok(());
    };

    // An apply can land in the minutes this takes: `Sweeping` keeps two sweeps
    // apart and nothing else, and `tune_apply` takes the write lock without
    // asking anyone. The row would name a base that is no longer in force and a
    // winner measured against it — and being the newest, it would be the
    // recommendation the page then offers, walking the ranking back off what
    // the operator just applied. That is the failure "only the newest sweep's
    // recommendation stands" was written to prevent, arriving by the other
    // door. A sweep whose baseline moved under it has measured nothing worth
    // recording; the next verdict pays for one against the settings now running.
    if *core.ranking.read().expect("ranking lock") != current {
        tracing::info!("ranking changed while the sweep ran; its results were discarded");
        return Ok(());
    }

    core.store
        .record_eval_run(&scored.eval_run(&pairs, judged, skipped))
        .await?;
    Ok(())
}

/// Every pair ranked under every candidate, and the one that cleared the gate.
pub(crate) struct Scored {
    grid: Vec<RankingParams>,
    ranks: Vec<Vec<Option<usize>>>,
    /// The running configuration's row: the baseline.
    base_at: usize,
    /// The winning row, if any candidate cleared `recommend`.
    best: Option<usize>,
}

/// Rank `pairs` under every configuration in `grid` and pick the winner, if
/// there is one. `grid` must carry `current`: it is the baseline everything
/// else is measured against.
///
/// `stop_after` is the moment the pass began; a search or a question recorded
/// after it ends the pass with nothing scored, and `None` comes back. `None`
/// as the argument never stops.
pub(crate) async fn score(
    core: &Core,
    pairs: &[Pair],
    grid: Vec<RankingParams>,
    current: RankingParams,
    rerank: bool,
    stop_after: Option<i64>,
) -> Result<Option<Scored>> {
    let Some(ranks) = ranks_over_grid(core, pairs, &grid, rerank, stop_after).await? else {
        return Ok(None);
    };
    let base_at = grid
        .iter()
        .position(|p| *p == current)
        .expect("the grid carries the running configuration");
    let base = &ranks[base_at];

    let mut best: Option<usize> = None;
    for cand in (0..grid.len()).filter(|i| *i != base_at) {
        if !recommend(base, &ranks[cand]) {
            continue;
        }
        // MRR first, then recall: the gate has already refused anything that
        // costs either, so this only chooses among improvements. Then the
        // fewest knobs moved, which is what keeps a candidate the measurements
        // cannot tell apart from claiming credit for the axis it changed.
        let beats = best.is_none_or(|b| {
            let score = |i: usize| (mrr(&ranks[i]), recall_at(&ranks[i], LIMIT));
            score(cand) > score(b)
                || (score(cand) == score(b) && moved(grid[cand], current) < moved(grid[b], current))
        });
        if beats {
            best = Some(cand);
        }
    }
    Ok(Some(Scored {
        grid,
        ranks,
        base_at,
        best,
    }))
}

impl Scored {
    /// The candidate that cleared the gate, or `None` when the running
    /// configuration held.
    pub(crate) fn winner(&self) -> Option<RankingParams> {
        self.best.map(|i| self.grid[i])
    }

    /// How much the winner improved MRR over the baseline. What an adopted
    /// generation is recorded as having promised.
    pub(crate) fn predicted(&self) -> Option<f64> {
        self.best
            .map(|i| mrr(&self.ranks[i]) - mrr(&self.ranks[self.base_at]))
    }

    /// The row the journal keeps. A quiet run is recorded too: without the row
    /// a page can only say nothing, which reads as "no sweep has ever run".
    pub(crate) fn eval_run(&self, pairs: &[Pair], judged: i64, skipped: i64) -> NewEvalRun {
        let base = &self.ranks[self.base_at];
        let (winner, winning_ranks) = match self.best {
            Some(i) => (self.grid[i], &self.ranks[i]),
            None => (self.grid[self.base_at], base),
        };
        let diff: Vec<DiffRow> = pairs
            .iter()
            .zip(base.iter().zip(winning_ranks))
            .filter(|(_, (b, n))| b != n)
            .map(|(pair, (b, n))| DiffRow {
                // The query names its own row, as it does in the harness's miss
                // list. No artifact text is written here.
                query: pair.query.chars().take(48).collect(),
                base: *b,
                new: *n,
            })
            .collect();
        NewEvalRun {
            judged_count: judged,
            pairs_used: pairs.len() as i64,
            pairs_skipped: skipped,
            base: self.grid[self.base_at].into(),
            base_recall: recall_at(base, LIMIT),
            base_mrr: mrr(base),
            best: winner.into(),
            best_recall: recall_at(winning_ranks, LIMIT),
            best_mrr: mrr(winning_ranks),
            diff,
            recommended: self.best.is_some(),
        }
    }
}

/// The other value of the rerank knob, and what it promised.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Flip {
    pub params: RankingParams,
    /// MRR of the flipped replay, less MRR over the served ranks.
    pub predicted: f64,
    /// The two rows' aggregates, for the journal: the served record is the
    /// base of this axis, not the pre-rerank replay the ladder measures from.
    pub served_mrr: f64,
    pub served_recall: f64,
    pub mrr: f64,
    pub recall: f64,
}

/// Offer the rerank flip, if a reranker serves search and the flip clears
/// `recommend` against the ranks that were actually served.
///
/// Its own base, because the served rank is the only row that has the
/// reranker in it where the reranker is live. Where the live value is "on",
/// the candidate is the replay without the reranker, which costs nothing;
/// where it is "off", the candidate is one reranker call per pair — spent
/// only because the operator configured the reranker, and only here.
pub(crate) async fn rerank_flip(
    core: &Core,
    pairs: &[Pair],
    current: RankingParams,
    stop_after: Option<i64>,
) -> Result<Option<Flip>> {
    if !core.reranks_search() {
        return Ok(None);
    }
    let with_served: Vec<&Pair> = pairs.iter().filter(|p| p.served.is_some()).collect();
    if with_served.is_empty() {
        return Ok(None);
    }
    let served: Vec<Option<usize>> = with_served.iter().map(|p| p.served).collect();
    let flipped = RankingParams {
        rerank: !current.rerank,
        ..current
    };
    let mut ranks = Vec::with_capacity(with_served.len());
    for pair in &with_served {
        if let Some(since) = stop_after
            && core.store.activity_since(since).await?
        {
            return Ok(None);
        }
        ranks.push(rank_of(core, pair, flipped, flipped.rerank).await?);
    }
    Ok(recommend(&served, &ranks).then(|| Flip {
        params: flipped,
        predicted: mrr(&ranks) - mrr(&served),
        served_mrr: mrr(&served),
        served_recall: recall_at(&served, LIMIT),
        mrr: mrr(&ranks),
        recall: recall_at(&ranks, LIMIT),
    }))
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
        // A run of verdicts is one sweep, not one each.
        let Some(_claim) = Sweeping::claim(&core) else {
            return;
        };
        if let Err(e) = sweep_if_due(&core).await {
            tracing::warn!(error = %e, "tuning sweep failed");
        }
    });
}

/// The claim on `core.tuning`, released on the way out whatever happened.
///
/// It used to be a `store(false)` at the end of the task, which a panic
/// skipped: a poisoned `ranking` lock or an unwrap anywhere under the search
/// path left the flag standing at true, and every later sweep then returned at
/// the first line for the lifetime of the process — silently, since a sweep
/// that declines to start says nothing. `Background::spawn` is a bare
/// `tokio::spawn` and catches nothing on its own.
pub(crate) struct Sweeping(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Sweeping {
    pub(crate) fn claim(core: &Core) -> Option<Self> {
        use std::sync::atomic::Ordering;
        match core.tuning.swap(true, Ordering::SeqCst) {
            true => None,
            false => Some(Self(core.tuning.clone())),
        }
    }
}

impl Drop for Sweeping {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

async fn sweep_if_due(core: &Core) -> Result<()> {
    let tune = &core.feedback.tune;
    let judged = core.store.feedback_stats(core.weak_below()).await?.judged;
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

/// A base with an improvement in it, for this module's tests and the idle
/// pass's: the same corpus, so what the sweep can find the pass can adopt.
#[cfg(test)]
pub(crate) mod test_support {
    use super::LIMIT;
    use crate::store::feedback::Door;

    pub(crate) const QUERY: &str = "the image will not mount";

    /// Two sources of three identical, untitled chunks each, and the order the
    /// uncapped ranking gives them.
    ///
    /// Identical within a source so the three tie and the cap is the only
    /// thing that can separate them; the order is read back rather than
    /// assumed, because which source leads is a property of the fake
    /// embedder's hashes and nothing this is testing.
    pub(crate) async fn seeded() -> (crate::core::Core, Vec<String>) {
        seeded_on(crate::core::test_support::test_core().await).await
    }

    /// The same base on a core that has the reversing, counting fake
    /// reranker. `order` is still the vector order: the pass replays with
    /// the reranker off, and the tests that want the reranked order reverse
    /// it themselves.
    pub(crate) async fn seeded_with_reranker() -> (
        crate::core::Core,
        Vec<String>,
        std::sync::Arc<crate::infer::fake::FakeReranker>,
    ) {
        let (core, reranker) = crate::core::test_support::test_core_counting_reranked_docs().await;
        let (core, order) = seeded_on(core).await;
        (core, order, reranker)
    }

    pub(crate) async fn seeded_on(core: crate::core::Core) -> (crate::core::Core, Vec<String>) {
        for (raw, text) in [("raw one", QUERY), ("raw two", "unrelated words")] {
            let src = core.store.insert_corpus(raw, "web", None).await.unwrap();
            let new: Vec<crate::store::artifacts::NewArtifact> = (0..3)
                .map(|i| crate::store::artifacts::NewArtifact {
                    ordinal: i,
                    text: text.to_string(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                })
                .collect();
            for c in core.store.insert_artifacts(&src.id, &new).await.unwrap() {
                crate::jobs::embed::run(&core, &c.id).await.unwrap();
            }
        }
        // The baseline this sweep is measured against: one source may fill the
        // whole list, so a cap is the improvement available to be found.
        core.ranking.write().unwrap().per_source_cap = None;
        let order = ranks_order(&core).await;
        (core, order)
    }

    pub(crate) async fn ranks_order(core: &crate::core::Core) -> Vec<String> {
        let params = *core.ranking.read().unwrap();
        let q = crate::core::search::SearchQuery {
            q: QUERY.into(),
            limit: LIMIT,
            tags: vec![],
            category: None,
            mark: false,
            include_deprecated: false,
            include_superseded: false,
            // The vector order, whatever reranker the core has: the ladder
            // replays with the reranker off, and this is its baseline.
            rerank: false,
            explain: false,
        };
        core.search_with_ranking(&q, params, Door::Judge)
            .await
            .unwrap()
            .0
            .into_iter()
            .map(|r| r.artifact_id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{QUERY, seeded};
    use super::*;
    use crate::store::feedback::{Door, NewEvent, Verdict};

    async fn a_generation(core: &crate::core::Core) -> String {
        use crate::store::generations::{GenerationParams, NewGeneration};
        core.store
            .record_generation(&NewGeneration {
                params: GenerationParams {
                    recency_weight: 0.05,
                    per_source_cap: Some(3),
                    ..Default::default()
                },
                embed_recipe: "recipe-a".into(),
                chat_model: "qwen".into(),
                parent_id: None,
            })
            .await
            .unwrap()
    }

    async fn observe(
        core: &crate::core::Core,
        generation: &str,
        artifact: &str,
        source: crate::store::observations::Source,
    ) {
        core.store
            .record_observation(&crate::store::observations::NewObservation {
                generation_id: generation.to_string(),
                query: QUERY.into(),
                query_vec: vec![0.1, 0.2, 0.3],
                embed_model: "fake".into(),
                artifact_id: Some(artifact.to_string()),
                rank: Some(1),
                source,
                event_id: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_sweep_ignores_observations_by_default() {
        use crate::store::observations::Source;
        let (core, order) = seeded().await;
        let generation = a_generation(&core).await;
        observe(&core, &generation, &order[0], Source::Cited).await;

        assert!(
            pairs_to_replay(&core).await.unwrap().0.is_empty(),
            "a shipped default must not change what is recommended"
        );
    }

    #[tokio::test]
    async fn with_the_key_on_a_used_excerpt_is_a_pair_the_sweep_can_score() {
        use crate::store::observations::Source;
        let (mut core, order) = seeded().await;
        core.evolve.feed_sweep = true;
        let generation = a_generation(&core).await;
        observe(&core, &generation, &order[0], Source::Cited).await;
        observe(&core, &generation, &order[1], Source::Opened).await;

        let (pairs, _) = pairs_to_replay(&core).await.unwrap();
        assert_eq!(pairs.len(), 2);
        assert!(pairs.iter().all(|p| p.query == QUERY));
        assert!(
            pairs.iter().all(|p| p.query_vec.is_some()),
            "an observation carries the vector it was searched with"
        );
    }

    #[tokio::test]
    async fn a_weak_negative_is_never_a_pair() {
        use crate::store::observations::Source;
        let (mut core, order) = seeded().await;
        core.evolve.feed_sweep = true;
        let generation = a_generation(&core).await;
        observe(&core, &generation, &order[0], Source::GaveUp).await;

        assert!(
            pairs_to_replay(&core).await.unwrap().0.is_empty(),
            "weaker evidence may take a setting back and may never bring one about"
        );
    }

    #[tokio::test]
    async fn observations_from_a_superseded_generation_stop_counting() {
        // Seed under the generation that is live, then mint another — which
        // supersedes it. A model change ends the era its evidence belonged to.
        use crate::store::generations::{GenerationParams, NewGeneration};
        use crate::store::observations::Source;
        let (mut core, order) = seeded().await;
        core.evolve.feed_sweep = true;
        let first = a_generation(&core).await;
        observe(&core, &first, &order[0], Source::Cited).await;
        assert_eq!(
            pairs_to_replay(&core).await.unwrap().0.len(),
            1,
            "live so far"
        );

        core.store
            .record_generation(&NewGeneration {
                params: GenerationParams {
                    recency_weight: 0.05,
                    per_source_cap: Some(3),
                    ..Default::default()
                },
                embed_recipe: "recipe-a".into(),
                chat_model: "a-different-model".into(),
                parent_id: Some(first),
            })
            .await
            .unwrap();

        assert!(pairs_to_replay(&core).await.unwrap().0.is_empty());
    }

    /// One judged search naming `expect` as its answer.
    async fn judge(core: &crate::core::Core, expect: &str) {
        let id = core
            .store
            .record_search(
                NewEvent {
                    fold_onto: None,
                    query: QUERY.into(),
                    door: Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.1, 0.2],
                    embed_model: "fake".into(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                // Folding off: these are the same query on purpose, and two
                // pairs are what the gate needs.
                0,
            )
            .await
            .unwrap();
        core.store
            .judge_hit(&id, expect, crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_opened_observation_replays_with_what_its_search_recorded() {
        use crate::core::search::Priming;
        use crate::store::feedback::NewCandidate;
        let (core, order) = seeded().await;
        let generation = a_generation(&core).await;
        let id = core
            .store
            .record_search(
                NewEvent {
                    fold_onto: None,
                    query: QUERY.into(),
                    door: Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.1, 0.2],
                    embed_model: "fake".into(),
                    candidates: vec![NewCandidate {
                        artifact_id: order[5].clone(),
                        score: 0.5,
                        similarity: Some(0.5),
                        shown: true,
                        band: false,
                    }],
                    answered: false,
                    context: Some(Priming {
                        activation: Default::default(),
                        sitting: [order[5].clone()].into_iter().collect(),
                        due: Default::default(),
                    }),
                },
                0,
            )
            .await
            .unwrap();
        core.store.open_event(&id, &order[5]).await.unwrap();

        let (pairs, _) = observation_pairs(&core, &generation).await.unwrap();
        assert_eq!(pairs.len(), 1);
        let priming = pairs[0]
            .priming
            .as_ref()
            .expect("the pair carries its context");
        assert!(priming.sitting.contains(&order[5]));
    }

    fn served_pair(order: &[String], i: usize, served: usize) -> Pair {
        Pair {
            query: QUERY.into(),
            satisfies: vec![order[i].clone()],
            query_vec: None,
            priming: None,
            served: Some(served),
        }
    }

    #[tokio::test]
    async fn a_replay_without_the_reranker_that_places_two_net_pairs_better_adopts_rerank_off() {
        // The fake reranker reverses the list. Served ranks are what it
        // produced; the replay without it is the vector order.
        let (core, order, _) = super::test_support::seeded_with_reranker().await;
        let current = RankingParams {
            rerank: true,
            ..*core.ranking.read().unwrap()
        };
        let pairs = vec![served_pair(&order, 0, 5), served_pair(&order, 1, 4)];
        let flip = rerank_flip(&core, &pairs, current, None)
            .await
            .unwrap()
            .expect("a flip is offered");
        assert!(!flip.params.rerank);
        assert!(flip.predicted > 0.0, "{flip:?}");
        assert_eq!(
            RankingParams {
                rerank: true,
                ..flip.params
            },
            current,
            "the flip moves the one knob"
        );
    }

    #[tokio::test]
    async fn a_flip_that_would_not_place_two_net_pairs_better_is_not_offered() {
        let (core, order, _) = super::test_support::seeded_with_reranker().await;
        let current = RankingParams {
            rerank: true,
            ..*core.ranking.read().unwrap()
        };
        // Served where the vector order already puts them: a tie.
        let pairs = vec![served_pair(&order, 0, 0), served_pair(&order, 1, 1)];
        assert!(
            rerank_flip(&core, &pairs, current, None)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn no_reranker_means_no_flip_is_offered() {
        let (core, order) = seeded().await;
        let current = *core.ranking.read().unwrap();
        let pairs = vec![served_pair(&order, 0, 5), served_pair(&order, 1, 4)];
        assert!(
            rerank_flip(&core, &pairs, current, None)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_flip_to_rerank_on_costs_one_call_per_pair_and_a_judged_pair_costs_none() {
        let (core, order, reranker) = super::test_support::seeded_with_reranker().await;
        let current = RankingParams {
            rerank: false,
            ..*core.ranking.read().unwrap()
        };
        let mut pairs: Vec<Pair> = (0..3).map(|i| served_pair(&order, i, i)).collect();
        pairs.push(Pair {
            served: None,
            ..served_pair(&order, 3, 3)
        });
        let before = reranker.calls();
        let _ = rerank_flip(&core, &pairs, current, None).await.unwrap();
        assert_eq!(reranker.calls() - before, 3);
    }

    #[tokio::test]
    async fn a_candidate_that_lifts_two_pairs_is_recorded_as_a_recommendation() {
        let (core, order) = seeded().await;
        // The second source's first two chunks: buried behind the leading
        // source uncapped, promoted the moment a cap displaces its tail.
        judge(&core, &order[3]).await;
        judge(&core, &order[4]).await;

        run_sweep(&core).await.unwrap();

        let run = core.store.latest_eval_run().await.unwrap().unwrap();
        assert_eq!(run.pairs_used, 2);
        assert_eq!(run.pairs_skipped, 0);
        assert!(run.recommended, "a strictly better candidate was refused");
        assert!(
            run.best_params.per_source_cap.is_some(),
            "the improvement here is a cap, and the run must name it"
        );
        assert!(run.best_mrr > run.base_mrr);
        assert_eq!(
            run.diff.len(),
            2,
            "both pairs moved, and the diff is what a person reads"
        );
        assert!(
            run.diff.iter().all(|d| d.new < d.base),
            "a recommended run's diff must show the pairs climbing"
        );
    }

    #[tokio::test]
    async fn a_cap_result_does_not_arrive_wearing_a_recency_change() {
        // The grid is walked recency-major from 0.0, so among candidates whose
        // rank vectors are identical the lowest index wins — and these pairs
        // are all the same age, so recency separates nothing. Untie-broken, the
        // recommendation reads "recency 0.05 → 0.00" beside the cap that
        // actually earned it, and applying it switches recency weighting off on
        // the strength of a result about caps.
        let (core, order) = seeded().await;
        core.ranking.write().unwrap().recency_weight = 0.05;
        judge(&core, &order[3]).await;
        judge(&core, &order[4]).await;

        run_sweep(&core).await.unwrap();

        let run = core.store.latest_eval_run().await.unwrap().unwrap();
        assert!(run.recommended, "the cap improvement was refused");
        assert!(
            run.best_params.per_source_cap.is_some(),
            "the cap is what this sweep measured"
        );
        assert_eq!(
            run.best_params.recency_weight, 0.05,
            "the sweep moved a knob nothing it measured had an opinion about"
        );
    }

    #[tokio::test]
    async fn a_sweep_with_nothing_better_records_the_silence() {
        // Pairs already answered at the top of the list. Nothing can lift them
        // and a cap can only push the second one down, so the gate refuses
        // everything — and the run is still written, so the page can say so.
        let (core, order) = seeded().await;
        judge(&core, &order[0]).await;
        judge(&core, &order[1]).await;

        run_sweep(&core).await.unwrap();

        let run = core.store.latest_eval_run().await.unwrap().unwrap();
        assert!(!run.recommended);
        assert_eq!(run.base_params, run.best_params);
        assert!(run.diff.is_empty(), "nothing changed, so nothing moved");
        assert!(core.store.open_recommendation().await.unwrap().is_none());
    }

    /// An apply landing mid-sweep, at a point the sweep cannot miss.
    ///
    /// Wall-clock racing would be the honest shape of this and a coin-toss as a
    /// test. The reranker runs inside every search in the grid — always after
    /// the baseline has been snapshotted, never before — so hanging the write
    /// off the first call puts it exactly where `tune_apply`'s would land, on
    /// every run.
    struct ApplyOnFirstSearch(std::sync::Arc<std::sync::RwLock<RankingParams>>);

    #[async_trait::async_trait]
    impl crate::infer::Reranker for ApplyOnFirstSearch {
        async fn rerank(
            &self,
            _query: &str,
            docs: &[String],
            top_n: usize,
        ) -> Result<Vec<(usize, f32)>> {
            self.0.write().expect("ranking lock").per_source_cap = Some(2);
            // Scores descending, so the order search already had comes back
            // unchanged: this is here to write, not to rank.
            Ok((0..docs.len().min(top_n))
                .map(|i| (i, 1.0 - i as f32 / 100.0))
                .collect())
        }
    }

    #[tokio::test]
    async fn a_sweep_whose_baseline_was_applied_out_from_under_it_records_nothing() {
        // The operator judging is the operator clicking Apply, and a sweep over
        // real pairs runs for minutes. Recorded anyway, the row would be the
        // newest — so the page would offer a winner measured against settings
        // that are no longer running, and taking it would undo the apply that
        // raced it.
        let (mut core, order) = seeded().await;
        judge(&core, &order[3]).await;
        judge(&core, &order[4]).await;
        let base = *core.ranking.read().unwrap();
        core.reranker = Some(std::sync::Arc::new(ApplyOnFirstSearch(
            core.ranking.clone(),
        )));

        run_sweep(&core).await.unwrap();

        assert_ne!(
            base,
            *core.ranking.read().unwrap(),
            "the apply never landed"
        );
        assert!(
            core.store.latest_eval_run().await.unwrap().is_none(),
            "a sweep measured against a baseline that is gone was written down anyway"
        );
    }

    #[tokio::test]
    async fn a_sweep_that_panics_does_not_disable_every_later_one() {
        // The flag used to be cleared at the end of the task body, which a
        // panic skipped — a poisoned `ranking` lock, an unwrap anywhere under
        // the search path — and `Background::spawn` is a bare `tokio::spawn`,
        // so nothing caught it. Every later sweep then returned at its first
        // line for the lifetime of the process, and said nothing about it.
        let core = crate::core::test_support::test_core().await;
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _claim = Sweeping::claim(&core).expect("the first claim is free");
            panic!("a poisoned lock under the search path");
        }));
        std::panic::set_hook(hook);

        assert!(died.is_err());
        assert!(
            Sweeping::claim(&core).is_some(),
            "the claim outlived the panic that dropped it"
        );
    }

    #[tokio::test]
    async fn a_pair_whose_artifact_is_gone_is_counted_rather_than_scored() {
        // Housekeeping, not a ranking result. Scored as a miss it would look
        // like a ranking failure forever; raised it would stop every later
        // sweep over one deletion.
        let (core, order) = seeded().await;
        judge(&core, &order[3]).await;
        judge(&core, "deleted-since").await;

        run_sweep(&core).await.unwrap();

        let run = core.store.latest_eval_run().await.unwrap().unwrap();
        assert_eq!((run.pairs_used, run.pairs_skipped), (1, 1));
    }

    #[tokio::test]
    async fn verdicts_that_name_no_answer_leave_nothing_to_sweep() {
        // Gaps and discards count towards the floor but are not pairs. With
        // only those, there is nothing to rank and no run to record.
        let (core, _) = seeded().await;
        let id = core
            .store
            .record_search(
                NewEvent {
                    fold_onto: None,
                    query: QUERY.into(),
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
            .judge(&id, Verdict::Gap, crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();

        run_sweep(&core).await.unwrap();
        assert!(core.store.latest_eval_run().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_sweep_leaves_no_trace_in_what_it_measures() {
        // It replays queries in full knowledge of their answers. Recorded,
        // those would be captured searches nobody made, waiting to be judged.
        let (mut core, order) = seeded().await;
        core.learn.enabled = true;
        judge(&core, &order[3]).await;
        judge(&core, &order[4]).await;
        let before = core.store.feedback_stats(0.0).await.unwrap().captured;

        run_sweep(&core).await.unwrap();
        core.background.wait_idle().await;

        assert_eq!(
            core.store.feedback_stats(0.0).await.unwrap().captured,
            before,
            "the sweep's own searches became data"
        );
    }

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
    fn the_running_configuration_is_always_among_the_candidates() {
        let current = RankingParams {
            recency_weight: 0.05,
            per_source_cap: Some(3),
            ..Default::default()
        };
        assert_eq!(
            candidates(current, &[], 8)[0],
            current,
            "and it comes first"
        );
        // A hand-set value off every ladder is still the baseline.
        let odd = RankingParams {
            recency_weight: 0.07,
            per_source_cap: Some(4),
            ..Default::default()
        };
        assert!(candidates(odd, &[], 8).contains(&odd));
    }

    #[test]
    fn a_reverted_candidate_is_never_offered() {
        use crate::store::generations::GenerationParams;
        let current = RankingParams {
            recency_weight: 0.05,
            per_source_cap: Some(3),
            ..Default::default()
        };
        let tried = vec![GenerationParams {
            recency_weight: 0.1,
            per_source_cap: Some(3),
            ..Default::default()
        }];
        let out = candidates(current, &tried, 64);
        assert!(
            !out.iter()
                .any(|c| c.recency_weight == 0.1 && c.per_source_cap == Some(3)),
            "{out:?}"
        );
        assert!(
            out.iter()
                .any(|c| c.recency_weight == 0.15 && c.per_source_cap == Some(3)),
            "the step past the one that failed is still reachable: {out:?}"
        );
    }

    #[test]
    fn the_budget_is_respected_and_neighbours_come_first() {
        let current = RankingParams {
            recency_weight: 0.05,
            per_source_cap: Some(3),
            ..Default::default()
        };
        let out = candidates(current, &[], 4);
        assert!(out.len() <= 4);
        assert!(
            out.iter()
                .any(|c| c.per_source_cap == Some(2) || c.per_source_cap == Some(5)),
            "a neighbour on the cap axis must be reachable inside a small budget: {out:?}"
        );
        assert!(
            out.iter()
                .any(|c| c.recency_weight == 0.0 || c.recency_weight == 0.1),
            "and so must one on the recency axis: {out:?}"
        );
        assert!(
            !out.iter().any(|c| c.per_source_cap.is_none()),
            "the far end of the ladder waits its turn: {out:?}"
        );
    }

    #[test]
    fn every_candidate_moves_at_most_one_knob() {
        // `moved` is what keeps a result about caps from arriving wearing a
        // recency change; the chooser must not hand it a candidate that already
        // moved both.
        let current = RankingParams::default();
        let all = candidates(current, &[], 64);
        for c in &all {
            assert!(moved(*c, current) <= 1, "{c:?}");
        }
        assert_eq!(
            all.len(),
            1 + (RECENCY.len() - 1)
                + (CAPS.len() - 1)
                + (crate::core::ranking::MULTIPLIERS.len() - 1)
                + (crate::core::ranking::HALF_LIVES.len() - 1)
                + (crate::core::ranking::PRIME_LIFTS.len() - 1),
            "every rung on every ladder, once, and nothing off them"
        );
    }

    #[test]
    fn the_pass_budget_covers_every_rung_on_every_axis() {
        // A tie keeps the current value, so an improvement two rungs out
        // behind a rung that ties would never be reached by a pass that only
        // tried the nearest step. The budget has to reach the whole ladder.
        let current = RankingParams::default();
        let all = candidates(current, &[], usize::MAX);
        assert_eq!(all.len(), crate::jobs::tune::BUDGET, "{all:?}");
        for c in &all {
            assert!(moved(*c, current) <= 1, "{c:?}");
        }
    }

    #[test]
    fn the_chooser_walks_the_lift_ladder_upward_from_zero() {
        let current = RankingParams::default();
        let grid = candidates(current, &[], crate::jobs::tune::BUDGET);
        let lifts: Vec<usize> = grid
            .iter()
            .map(|c| c.prime_lift)
            .filter(|l| *l != current.prime_lift)
            .collect();
        assert_eq!(lifts, vec![1, 2, 4]);
    }

    #[tokio::test]
    async fn a_pair_with_a_sitting_ranks_differently_at_lift_two_and_the_same_without_one() {
        use crate::core::search::Priming;
        let (core, order) = seeded().await;
        // The last-ranked hit was read in this sitting: at lift 2 it climbs
        // two places on the Judge door, where priming is otherwise off.
        let with = Pair {
            query: QUERY.into(),
            satisfies: vec![order[5].clone()],
            query_vec: None,
            priming: Some(Priming {
                activation: Default::default(),
                sitting: [order[5].clone()].into_iter().collect(),
                due: Default::default(),
            }),
            served: None,
        };
        let without = Pair {
            priming: None,
            ..with.clone()
        };
        let current = *core.ranking.read().unwrap();
        let lifted = RankingParams {
            prime_lift: 2,
            ..current
        };
        assert_eq!(
            rank_of(&core, &with, current, false).await.unwrap(),
            Some(5)
        );
        assert_eq!(
            rank_of(&core, &with, lifted, false).await.unwrap(),
            Some(3),
            "two places, no further, never past rank 1"
        );
        assert_eq!(
            rank_of(&core, &without, lifted, false).await.unwrap(),
            Some(5),
            "no context, no lift: every rung is the same list"
        );
    }

    #[test]
    fn a_reverted_pool_depth_is_not_offered_again() {
        use crate::store::generations::GenerationParams;
        let current = RankingParams::default();
        let tried = vec![GenerationParams::from(RankingParams {
            candidate_multiplier: 5,
            ..current
        })];
        let out = candidates(current, &tried, 64);
        assert!(!out.iter().any(|c| c.candidate_multiplier == 5), "{out:?}");
        assert!(
            out.iter().any(|c| c.candidate_multiplier == 8),
            "the rung past it is still there"
        );
    }

    #[test]
    fn the_verdict_paid_grid_moves_only_the_two_knobs_it_measures() {
        let current = RankingParams {
            candidate_multiplier: 5,
            recency_half_life_days: 90,
            ..RankingParams::default()
        };
        for c in grid(current) {
            assert_eq!(c.candidate_multiplier, 5, "{c:?}");
            assert_eq!(c.recency_half_life_days, 90, "{c:?}");
        }
    }

    #[test]
    fn the_grid_always_contains_the_configuration_it_is_measured_against() {
        let shipped = RankingParams {
            recency_weight: 0.05,
            per_source_cap: Some(3),
            ..Default::default()
        };
        assert!(grid(shipped).contains(&shipped));
        assert_eq!(grid(shipped).len(), RECENCY.len() * CAPS.len());

        // A hand-set value nobody would have guessed is still the baseline.
        let odd = RankingParams {
            recency_weight: 0.07,
            per_source_cap: Some(4),
            ..Default::default()
        };
        assert!(grid(odd).contains(&odd));
        assert_eq!(grid(odd).len(), RECENCY.len() * CAPS.len() + 1);
    }
}
