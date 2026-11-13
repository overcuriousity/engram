//! What the idle pass replays: pairs, ranks, the gate and the chooser. It only reads the live
//! index; `Door::Judge` and `mark: false` because a replay knows its answers.

use crate::core::Core;
use crate::core::ranking::RankingParams;
use crate::error::Result;
use crate::eval::metrics::{mrr, recall_at};
use crate::store::eval_runs::{DiffRow, NewEvalRun};

/// The `k` in recall@k: the judge page's own figure is recall@10.
pub(crate) const LIMIT: usize = 10;

/// The scoring ladders (recency weight, per-source cap); retrieval ladders live in `core::ranking`.
const RECENCY: [f32; 5] = [0.0, 0.05, 0.1, 0.15, 0.25];
const CAPS: [Option<usize>; 4] = [Some(2), Some(3), Some(5), None];

/// A bounded set of candidates: the running configuration, then neighbours outward on each axis
/// until `budget` is spent, each moving one knob; a set tried and taken back is never offered.
pub fn candidates(
    current: RankingParams,
    tried: &[crate::store::generations::GenerationParams],
    budget: usize,
) -> Vec<RankingParams> {
    use crate::core::ranking::{HALF_LIVES, MULTIPLIERS, PRIME_LIFTS, SITTING_PRIMES};
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
    // The sitting shares the lift's budget, so turning it on under a zero lift is a sure tie.
    // Turning it off is offered whatever the lift, so an adopted sitting can always be undone.
    let sittings: Vec<bool> = match current.sitting_prime || current.prime_lift > 0 {
        true => SITTING_PRIMES
            .iter()
            .copied()
            .filter(|s| *s != current.sitting_prime)
            .collect(),
        false => vec![],
    };

    let mut out = vec![current];
    let longest = [
        recency.len(),
        caps.len(),
        multipliers.len(),
        half_lives.len(),
        lifts.len(),
        sittings.len(),
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
        if let Some(sitting_prime) = sittings.get(i) {
            out.push(RankingParams {
                sitting_prime: *sitting_prime,
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

/// The rungs of one ladder nearest first, alternating sides, the current rung left out.
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

/// Whether `cand` placed a pair better than `base`; a miss loses to any rank.
fn better(cand: Option<usize>, base: Option<usize>) -> bool {
    match (cand, base) {
        (Some(c), Some(b)) => c < b,
        (Some(_), None) => true,
        _ => false,
    }
}

/// Replayed pairs a candidate needs behind it. Applied by `score` and `rerank_flip`, not in
/// `recommend`, which `jobs::retract` also uses over a single artifact's observations.
pub const MIN_PAIRS: usize = 10;

/// The gate: two net better pairs and no loss on either aggregate; ties keep the current values.
/// Sample size is the caller's question (see `MIN_PAIRS`).
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

/// How many knobs a candidate moves off the running configuration: the last tie-break, so
/// identical rank vectors do not let a second knob ride along.
fn moved(cand: RankingParams, current: RankingParams) -> usize {
    usize::from(cand.recency_weight != current.recency_weight)
        + usize::from(cand.per_source_cap != current.per_source_cap)
        + usize::from(cand.candidate_multiplier != current.candidate_multiplier)
        + usize::from(cand.recency_half_life_days != current.recency_half_life_days)
        + usize::from(cand.prime_lift != current.prime_lift)
        + usize::from(cand.spread_max != current.spread_max)
        + usize::from(cand.rerank != current.rerank)
        + usize::from(cand.review_min != current.review_min)
        + usize::from(cand.sitting_prime != current.sitting_prime)
}

/// One pair to replay: a query, the ids that satisfy it, and the query vector where recorded.
#[derive(Debug, Clone)]
pub(crate) struct Pair {
    pub(crate) query: String,
    pub(crate) satisfies: Vec<String>,
    pub(crate) query_vec: Option<Vec<f32>>,
    /// What priming read when the source search ran; a pair without one ties on `prime_lift`.
    pub(crate) priming: Option<crate::core::search::Priming>,
    /// The 1-based, unbounded rank this was served at; narrowed by `served()` only when measured,
    /// so "served beyond `LIMIT`" stays apart from "never served".
    pub(crate) served_rank: Option<i64>,
    /// Whether `served_rank` comes from a reranked list: `rerank.apply` scopes doors separately,
    /// so a cited rank may come from a pipeline the flip is not about.
    pub(crate) served_reranked: bool,
    /// Artifacts that must not count as results: a capture probe's source answers it first.
    pub(crate) exclude: Vec<String>,
}

impl Pair {
    /// Where the artifact was served, 0-based; `None` past `LIMIT`. See `served_at`.
    pub(crate) fn served(&self) -> Option<usize> {
        served_at(self.served_rank)
    }
}

/// The served place 0-based and `None` past `LIMIT`, as `rank_of` measures a replay;
/// `observations.rank` is 1-based and unbounded.
pub(crate) fn served_at(rank: Option<i64>) -> Option<usize> {
    rank.map(|r| (r - 1).max(0) as usize).filter(|r| *r < LIMIT)
}

/// Pairs one pass draws on, both kinds together: work is pairs times candidates, and the flip
/// costs one reranker call per pair on the card serving everything else.
pub(crate) const OBSERVATION_LIMIT: usize = 500;

/// Where one configuration put the answer to one pair, `None` past `LIMIT`. The ladder passes
/// `rerank: false` to spend no inference; the flip passes `true`.
pub(crate) async fn rank_of(
    core: &Core,
    pair: &Pair,
    params: RankingParams,
    rerank: bool,
) -> Result<Option<usize>> {
    // The query's recorded vector, cached so the search embeds nothing.
    if let Some(v) = &pair.query_vec {
        core.remember_query_vector(&pair.query, v.clone());
    }
    let q = crate::core::search::SearchQuery {
        q: pair.query.clone(),
        limit: LIMIT,
        tags: vec![],
        category: None,
        // Resurfacing reads `last_seen_at`; a scored run is not someone reading their notes.
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
    Ok(kept(pair, &results).position(|id| pair.satisfies.iter().any(|s| s == id)))
}

/// The results a pair is scored over, minus `Pair::exclude`.
fn kept<'a>(
    pair: &'a Pair,
    results: &'a [crate::core::search::SearchResult],
) -> impl Iterator<Item = &'a String> {
    results
        .iter()
        .map(|r| &r.artifact_id)
        .filter(|id| !pair.exclude.iter().any(|e| &e == id))
}

/// Where the answer landed, 0-based, and every artifact above it (the top `LIMIT` on a miss).
pub(crate) async fn rank_and_above(
    core: &Core,
    pair: &Pair,
    params: RankingParams,
) -> Result<(Option<usize>, Vec<String>)> {
    if let Some(v) = &pair.query_vec {
        core.remember_query_vector(&pair.query, v.clone());
    }
    let q = crate::core::search::SearchQuery {
        q: pair.query.clone(),
        limit: LIMIT,
        tags: vec![],
        category: None,
        mark: false,
        rerank: false,
        explain: false,
        include_deprecated: false,
        include_superseded: false,
    };
    let origin = crate::store::feedback::Origin::from(crate::store::feedback::Door::Judge);
    let (results, _) = core.search_with_ranking(&q, params, origin).await?;
    let kept: Vec<&String> = kept(pair, &results).collect();
    let rank = kept
        .iter()
        .position(|id| pair.satisfies.iter().any(|s| &s == id));
    let above = kept
        .iter()
        .take(rank.unwrap_or(kept.len()))
        .map(|id| (*id).clone())
        .collect();
    Ok((rank, above))
}

/// Every pair under every configuration. Query-major so each query embeds once: a pass per
/// configuration defeats the insertion-ordered query cache.
async fn ranks_over_grid(
    core: &Core,
    pairs: &[Pair],
    grid: &[RankingParams],
    rerank: bool,
    stop_after: Option<i64>,
) -> Result<Option<Vec<Vec<Option<usize>>>>> {
    let mut ranks = vec![Vec::with_capacity(pairs.len()); grid.len()];
    for pair in pairs {
        // Between pairs: a pair is a handful of vector reads, so whoever came back waits that long.
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

/// The verdicts under one generation as pairs, and how many named a gone artifact. Merged
/// artifacts are satisfied by their successor; deleted ones are skipped, not scored as misses.
async fn pairs_from_verdicts(
    core: &Core,
    verdicts: Vec<crate::store::feedback::JudgedPair>,
) -> Result<(Vec<Pair>, i64)> {
    let mut pairs = Vec::new();
    let mut skipped = 0;
    for p in verdicts {
        match core.store.get_artifact(&p.expect).await {
            Ok(_) => {
                let satisfies = crate::eval::satisfied_by(core, &p.expect).await;
                let priming = core.store.search_context(&p.event_id).await?;
                pairs.push(Pair {
                    query: p.query,
                    satisfies,
                    query_vec: Some(p.query_vec),
                    priming,
                    served_rank: p.served_rank,
                    served_reranked: core.reranks_search(),
                    exclude: Vec::new(),
                });
            }
            Err(crate::error::Error::NotFound) => skipped += 1,
            Err(e) => return Err(e),
        }
    }
    Ok((pairs, skipped))
}

/// Everything the ladder is scored on: verdicts and positive observations, and how many named a
/// gone artifact. An open confirmed by a verdict on the same search counts once, as the verdict.
pub(crate) async fn evidence_pairs(core: &Core, generation_id: &str) -> Result<(Vec<Pair>, i64)> {
    let verdicts = core
        .store
        .judged_pairs(generation_id, OBSERVATION_LIMIT)
        .await?;
    let confirmed: std::collections::HashSet<(String, String)> = verdicts
        .iter()
        .map(|p| (p.event_id.clone(), p.expect.clone()))
        .collect();
    let (mut pairs, mut skipped) = pairs_from_verdicts(core, verdicts).await?;
    // Verdicts take the shared budget first: a person said so, an observation is an inference.
    let room = OBSERVATION_LIMIT.saturating_sub(pairs.len());
    let (observed, left_out) =
        observation_pairs_except(core, generation_id, &confirmed, room).await?;
    pairs.extend(observed);
    skipped += left_out;
    Ok((pairs, skipped))
}

/// The positive observations under one generation as pairs, and how many named a gone artifact.
/// Half the budget to misses past `LIMIT`, half to hits inside it: misses alone all tie at `None`.
pub(crate) async fn observation_pairs(
    core: &Core,
    generation_id: &str,
) -> Result<(Vec<Pair>, i64)> {
    observation_pairs_except(core, generation_id, &Default::default(), OBSERVATION_LIMIT).await
}

/// `observation_pairs`, minus observations repeating a verdict in `confirmed`, at most `budget`.
async fn observation_pairs_except(
    core: &Core,
    generation_id: &str,
    confirmed: &std::collections::HashSet<(String, String)>,
    budget: usize,
) -> Result<(Vec<Pair>, i64)> {
    if budget == 0 {
        return Ok((Vec::new(), 0));
    }
    let mut observations: Vec<_> = core
        .store
        .observations_for_generation(generation_id, OBSERVATION_LIMIT * 2)
        .await?
        .into_iter()
        .filter(|o| o.artifact_id.is_some() && o.strength > 0.0)
        .filter(|o| match (&o.event_id, &o.artifact_id) {
            (Some(e), Some(a)) => !confirmed.contains(&(e.clone(), a.clone())),
            _ => true,
        })
        .collect();
    // Worst-placed first; newest first among equals.
    observations.sort_by_key(|o| std::cmp::Reverse(o.rank.unwrap_or(i64::MAX)));
    let (deep, inside): (Vec<_>, Vec<_>) = observations
        .into_iter()
        .partition(|o| o.rank.is_none_or(|r| r as usize > LIMIT));
    let half = budget / 2;
    let from_deep = half
        .max(budget.saturating_sub(inside.len()))
        .min(deep.len());
    let observations: Vec<_> = deep
        .into_iter()
        .take(from_deep)
        .chain(inside.into_iter().take(budget.saturating_sub(from_deep)))
        .collect();

    let mut pairs = Vec::with_capacity(observations.len());
    let mut skipped = 0;
    for o in observations {
        let Some(artifact) = o.artifact_id.as_deref() else {
            continue;
        };
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
                    served_rank: o.rank,
                    served_reranked: match o.source {
                        crate::store::observations::Source::Cited => core.reranks_ask(),
                        _ => core.reranks_search(),
                    },
                    exclude: Vec::new(),
                });
            }
            Err(crate::error::Error::NotFound) => skipped += 1,
            Err(e) => return Err(e),
        }
    }
    Ok((pairs, skipped))
}

pub(crate) struct Scored {
    grid: Vec<RankingParams>,
    ranks: Vec<Vec<Option<usize>>>,
    base_at: usize,
    best: Option<usize>,
}

/// Rank `pairs` under every configuration in `grid` (which must carry `current`) and pick the
/// winner. `None` when a search or question recorded after `stop_after` ends the pass.
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
    let base_at = grid.iter().position(|p| *p == current).ok_or_else(|| {
        crate::error::Error::Internal("the grid lacks the running configuration".into())
    })?;
    let base = &ranks[base_at];

    // Under `MIN_PAIRS` nothing clears the gate; the run is still journalled.
    if base.len() < MIN_PAIRS {
        return Ok(Some(Scored {
            grid,
            ranks,
            base_at,
            best: None,
        }));
    }

    let mut best: Option<usize> = None;
    for cand in (0..grid.len()).filter(|i| *i != base_at) {
        if !recommend(base, &ranks[cand]) {
            continue;
        }
        // MRR, then recall, then fewest knobs moved; the gate refused anything that costs either.
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
    pub(crate) fn winner(&self) -> Option<RankingParams> {
        self.best.map(|i| self.grid[i])
    }

    /// How much the winner improved MRR: what an adopted generation promised.
    pub(crate) fn predicted(&self) -> Option<f64> {
        self.best
            .map(|i| mrr(&self.ranks[i]) - mrr(&self.ranks[self.base_at]))
    }

    /// The journal row; a quiet pass is recorded too, or a page reads "no pass has ever run".
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
                // No artifact text is written here.
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
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Flip {
    pub params: RankingParams,
    pub predicted: f64,
    /// The served record's aggregates: the base of this axis, not the pre-rerank replay.
    pub served_mrr: f64,
    pub served_recall: f64,
    pub mrr: f64,
    pub recall: f64,
}

/// What `rerank_flip` came back with: "held" continues the pass, "interrupted" stops it.
pub(crate) enum FlipOffer {
    Offered(Flip),
    /// No reranker, no pair with a served rank, or the flip did not clear the gate.
    Held,
    Stopped,
}

impl FlipOffer {
    /// The flip, where one was offered; for the tests, which drive the gate alone.
    #[cfg(test)]
    pub(crate) fn offered(self) -> Option<Flip> {
        match self {
            Self::Offered(f) => Some(f),
            _ => None,
        }
    }
}

/// Offer the rerank flip if a reranker serves search and the flip clears `recommend` against
/// the served ranks, the only row with the live reranker in it.
pub(crate) async fn rerank_flip(
    core: &Core,
    pairs: &[Pair],
    current: RankingParams,
    stop_after: Option<i64>,
) -> Result<FlipOffer> {
    if !core.reranks_search() {
        return Ok(FlipOffer::Held);
    }
    // Deep ranks count (a miss the base is charged with); only ranks from a reranked list,
    // since this axis is scored against what was served. See `Pair::served_reranked`.
    let with_served: Vec<&Pair> = pairs
        .iter()
        .filter(|p| p.served_rank.is_some() && p.served_reranked)
        .collect();
    // Before the replay: saves a reranker call per pair where the flip cannot be offered.
    if with_served.len() < MIN_PAIRS {
        return Ok(FlipOffer::Held);
    }
    let served: Vec<Option<usize>> = with_served.iter().map(|p| p.served()).collect();
    let flipped = RankingParams {
        rerank: !current.rerank,
        ..current
    };
    let mut ranks = Vec::with_capacity(with_served.len());
    for pair in &with_served {
        if let Some(since) = stop_after
            && core.store.activity_since(since).await?
        {
            return Ok(FlipOffer::Stopped);
        }
        ranks.push(rank_of(core, pair, flipped, flipped.rerank).await?);
    }
    if !recommend(&served, &ranks) {
        return Ok(FlipOffer::Held);
    }
    Ok(FlipOffer::Offered(Flip {
        params: flipped,
        predicted: mrr(&ranks) - mrr(&served),
        served_mrr: mrr(&served),
        served_recall: recall_at(&served, LIMIT),
        mrr: mrr(&ranks),
        recall: recall_at(&ranks, LIMIT),
    }))
}

/// A base with an improvement in it, shared with the idle pass's tests.
#[cfg(test)]
pub(crate) mod test_support {
    use super::LIMIT;
    use crate::store::feedback::Door;

    pub(crate) const QUERY: &str = "the image will not mount";

    /// Two sources of three identical chunks each, so only the cap separates them, and the uncapped
    /// order, read back because it depends on the fake embedder's hashes.
    pub(crate) async fn seeded() -> (crate::core::Core, Vec<String>) {
        seeded_on(crate::core::test_support::test_core().await).await
    }

    /// The same base with the reversing, counting fake reranker; `order` is still the vector order.
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
        // One source may fill the whole list, so a cap is the improvement to find.
        core.ranking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .per_source_cap = None;
        let order = ranks_order(&core).await;
        (core, order)
    }

    pub(crate) async fn ranks_order(core: &crate::core::Core) -> Vec<String> {
        let params = *core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let q = crate::core::search::SearchQuery {
            q: QUERY.into(),
            limit: LIMIT,
            tags: vec![],
            category: None,
            mark: false,
            include_deprecated: false,
            include_superseded: false,
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

    /// One web search of `QUERY` judged a hit on `expect`, as the bar records one; the event id.
    pub(crate) async fn judge(core: &crate::core::Core, expect: &str) -> String {
        let query_vec = core.embedder.embed_query(QUERY).await.unwrap();
        let id = core
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: QUERY.into(),
                    door: Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec,
                    embed_model: "fake".into(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                // Folding off: two identical queries are the two pairs the gate needs.
                0,
            )
            .await
            .unwrap();
        core.store
            .judge_hit(&id, expect, crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();
        id
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{QUERY, judge, seeded};
    use super::*;
    use crate::store::feedback::{Door, NewEvent};

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
        observe_on(core, generation, artifact, source, None).await;
    }

    async fn observe_on(
        core: &crate::core::Core,
        generation: &str,
        artifact: &str,
        source: crate::store::observations::Source,
        event: Option<&str>,
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
                event_id: event.map(str::to_string),
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_used_excerpt_is_a_pair_the_pass_can_score() {
        use crate::store::observations::Source;
        let (core, order) = seeded().await;
        let generation = a_generation(&core).await;
        observe(&core, &generation, &order[0], Source::Cited).await;
        observe(&core, &generation, &order[1], Source::Opened).await;

        let (pairs, _) = evidence_pairs(&core, &generation).await.unwrap();
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
        let (core, order) = seeded().await;
        let generation = a_generation(&core).await;
        observe(&core, &generation, &order[0], Source::GaveUp).await;

        assert!(
            evidence_pairs(&core, &generation)
                .await
                .unwrap()
                .0
                .is_empty(),
            "weaker evidence may take a setting back and may never bring one about"
        );
    }

    #[tokio::test]
    async fn evidence_belongs_to_the_generation_it_was_gathered_under() {
        // A model change ends the era its evidence belonged to.
        use crate::store::generations::{GenerationParams, NewGeneration};
        use crate::store::observations::Source;
        let (core, order) = seeded().await;
        let first = a_generation(&core).await;
        observe(&core, &first, &order[0], Source::Cited).await;
        judge(&core, &order[1]).await;
        assert_eq!(evidence_pairs(&core, &first).await.unwrap().0.len(), 2);

        let next = core
            .store
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

        assert!(evidence_pairs(&core, &next).await.unwrap().0.is_empty());
    }

    #[tokio::test]
    async fn a_judged_answer_is_a_pair_the_pass_replays_with_the_searchs_own_vector() {
        let (core, order) = seeded().await;
        let generation = a_generation(&core).await;
        judge(&core, &order[3]).await;

        let (pairs, skipped) = evidence_pairs(&core, &generation).await.unwrap();
        assert_eq!((pairs.len(), skipped), (1, 0));
        assert_eq!(pairs[0].query, QUERY);
        assert!(pairs[0].satisfies.contains(&order[3]));
        assert!(
            pairs[0].query_vec.is_some(),
            "a verdict carries the vector its search was made with, so the replay embeds nothing"
        );
        assert_eq!(
            pairs[0].served_rank, None,
            "the bar's search recorded no pool, so the answer has no served place"
        );
    }

    #[tokio::test]
    async fn a_verdict_and_the_open_it_confirms_are_one_pair() {
        use crate::store::observations::Source;
        let (core, order) = seeded().await;
        let generation = a_generation(&core).await;
        let event = judge(&core, &order[3]).await;
        observe_on(&core, &generation, &order[3], Source::Opened, Some(&event)).await;
        assert_eq!(
            evidence_pairs(&core, &generation).await.unwrap().0.len(),
            1,
            "the observation repeats the verdict"
        );

        observe_on(&core, &generation, &order[4], Source::Opened, Some(&event)).await;
        observe(&core, &generation, &order[3], Source::Cited).await;
        assert_eq!(evidence_pairs(&core, &generation).await.unwrap().0.len(), 3);
    }

    #[tokio::test]
    async fn the_two_kinds_of_evidence_share_one_budget_and_a_verdict_is_served_first() {
        let (core, order) = seeded().await;
        let generation = a_generation(&core).await;
        // A verdict here carries no served place and every observation does; that tells them apart.
        for _ in 0..300 {
            judge(&core, &order[3]).await;
        }
        for i in 0..400 {
            observed_at(&core, &generation, &order[0], 1 + (i % 5) as i64).await;
        }

        let (pairs, _) = evidence_pairs(&core, &generation).await.unwrap();
        assert_eq!(pairs.len(), OBSERVATION_LIMIT, "one budget, not one each");
        assert_eq!(
            pairs.iter().filter(|p| p.served_rank.is_none()).count(),
            300,
            "every verdict is kept; the observations take the room left"
        );

        for _ in 0..300 {
            judge(&core, &order[3]).await;
        }
        let (pairs, _) = evidence_pairs(&core, &generation).await.unwrap();
        assert_eq!(pairs.len(), OBSERVATION_LIMIT);
        assert!(
            pairs.iter().all(|p| p.served_rank.is_none()),
            "no observation is drawn once the verdicts have spent the budget"
        );
    }

    #[tokio::test]
    async fn a_judged_answer_whose_artifact_is_gone_is_counted_rather_than_scored() {
        // Scored as a miss it would look like a ranking failure; raised, it stops every later pass.
        let (core, order) = seeded().await;
        let generation = a_generation(&core).await;
        judge(&core, &order[3]).await;
        judge(&core, "deleted-since").await;

        let (pairs, skipped) = evidence_pairs(&core, &generation).await.unwrap();
        assert_eq!((pairs.len(), skipped), (1, 1));
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
                        ..Default::default()
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

    #[test]
    fn a_served_place_outside_the_window_is_the_miss_it_is_to_everything_else() {
        assert_eq!(served_at(Some(1)), Some(0));
        assert_eq!(served_at(Some(LIMIT as i64)), Some(LIMIT - 1));
        assert_eq!(served_at(Some(LIMIT as i64 + 1)), None);
        assert_eq!(served_at(Some(15)), None);
        assert_eq!(served_at(None), None);
        // The column counts from one; a rank of zero, if ever written, is the first place.
        assert_eq!(served_at(Some(0)), Some(0));
    }

    #[tokio::test]
    async fn the_observation_budget_is_not_spent_wholly_on_places_the_window_cannot_see() {
        let (core, order) = seeded().await;
        let generation = a_generation(&core).await;
        for i in 0..OBSERVATION_LIMIT + 10 {
            observed_at(
                &core,
                &generation,
                &order[0],
                LIMIT as i64 + 1 + (i % 5) as i64,
            )
            .await;
        }
        for _ in 0..5 {
            observed_at(&core, &generation, &order[1], 2).await;
        }

        let (pairs, _) = observation_pairs(&core, &generation).await.unwrap();
        assert_eq!(pairs.len(), OBSERVATION_LIMIT, "the budget is still spent");
        let inside = pairs.iter().filter(|p| p.served().is_some()).count();
        assert_eq!(
            inside, 5,
            "every observation the window can see is drawn on, deep ones or not"
        );
    }

    async fn observed_at(core: &crate::core::Core, generation: &str, artifact: &str, rank: i64) {
        core.store
            .record_observation(&crate::store::observations::NewObservation {
                generation_id: generation.to_string(),
                query: QUERY.into(),
                query_vec: vec![0.1, 0.2],
                embed_model: "fake".into(),
                artifact_id: Some(artifact.to_string()),
                rank: Some(rank),
                source: crate::store::observations::Source::Opened,
                event_id: None,
            })
            .await
            .unwrap();
    }

    fn served_pair(order: &[String], i: usize, served: usize) -> Pair {
        Pair {
            query: QUERY.into(),
            satisfies: vec![order[i].clone()],
            query_vec: None,
            priming: None,
            served_rank: Some(served as i64 + 1),
            served_reranked: true,
            exclude: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_replay_without_the_reranker_that_places_two_net_pairs_better_adopts_rerank_off() {
        // The fake reranker reverses the list; the replay without it is the vector order.
        let (core, order, _) = super::test_support::seeded_with_reranker().await;
        let current = RankingParams {
            rerank: true,
            ..*core
                .ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let pairs: Vec<Pair> = (0..5)
            .flat_map(|_| [served_pair(&order, 0, 5), served_pair(&order, 1, 4)])
            .collect();
        let flip = rerank_flip(&core, &pairs, current, None)
            .await
            .unwrap()
            .offered()
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
    async fn the_rerank_axis_keeps_the_deep_observations_the_budget_went_after() {
        let (core, order, _) = super::test_support::seeded_with_reranker().await;
        let current = RankingParams {
            rerank: true,
            ..*core
                .ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let deep = |i: usize| Pair {
            query: QUERY.into(),
            satisfies: vec![order[i].clone()],
            query_vec: None,
            priming: None,
            served_rank: Some(LIMIT as i64 + 3),
            served_reranked: true,
            exclude: Vec::new(),
        };
        let pairs: Vec<Pair> = (0..5).flat_map(|_| [deep(0), deep(1)]).collect();
        assert!(
            pairs.iter().all(|p| p.served().is_none()),
            "narrowed to the window, both are misses — which is the point"
        );
        let flip = rerank_flip(&core, &pairs, current, None)
            .await
            .unwrap()
            .offered()
            .expect("two pairs the reranker buried are two the flip can recover");
        assert_eq!(flip.served_recall, 0.0, "the base found neither of them");
        assert!(flip.recall > 0.0, "and the flip finds them");
    }

    #[tokio::test]
    async fn a_flip_that_would_not_place_two_net_pairs_better_is_not_offered() {
        let (core, order, _) = super::test_support::seeded_with_reranker().await;
        let current = RankingParams {
            rerank: true,
            ..*core
                .ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        // A tie over enough pairs that the tie, not the floor, holds it.
        let pairs: Vec<Pair> = (0..5)
            .flat_map(|_| [served_pair(&order, 0, 0), served_pair(&order, 1, 1)])
            .collect();
        assert!(matches!(
            rerank_flip(&core, &pairs, current, None).await.unwrap(),
            FlipOffer::Held
        ));
    }

    #[tokio::test]
    async fn a_search_landing_mid_replay_stops_the_flip_rather_than_holding_it() {
        let (core, order, _) = super::test_support::seeded_with_reranker().await;
        let current = RankingParams {
            rerank: true,
            ..*core
                .ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let pairs: Vec<Pair> = (0..5)
            .flat_map(|_| [served_pair(&order, 0, 5), served_pair(&order, 1, 4)])
            .collect();
        let started = crate::store::now();
        judge(&core, &order[0]).await;
        assert!(
            core.store.activity_since(started - 1).await.unwrap(),
            "the search this test leans on was recorded"
        );
        assert!(matches!(
            rerank_flip(&core, &pairs, current, Some(started - 1))
                .await
                .unwrap(),
            FlipOffer::Stopped
        ));
    }

    #[tokio::test]
    async fn no_reranker_means_no_flip_is_offered() {
        let (core, order) = seeded().await;
        let current = *core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pairs = vec![served_pair(&order, 0, 5), served_pair(&order, 1, 4)];
        assert!(matches!(
            rerank_flip(&core, &pairs, current, None).await.unwrap(),
            FlipOffer::Held
        ));
    }

    #[tokio::test]
    async fn a_flip_to_rerank_on_costs_one_call_per_pair_and_a_judged_pair_costs_none() {
        let (core, order, reranker) = super::test_support::seeded_with_reranker().await;
        let current = RankingParams {
            rerank: false,
            ..*core
                .ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let mut pairs: Vec<Pair> = (0..MIN_PAIRS)
            .map(|i| served_pair(&order, i % 3, i % 3))
            .collect();
        pairs.push(Pair {
            served_rank: None,
            ..served_pair(&order, 3, 3)
        });
        let before = reranker.calls();
        let _ = rerank_flip(&core, &pairs, current, None).await.unwrap();
        assert_eq!(reranker.calls() - before, MIN_PAIRS);
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
        let current = RankingParams::default();
        let all = candidates(current, &[], 64);
        for c in &all {
            assert!(moved(*c, current) <= 1, "{c:?}");
        }
        let lifted_base = RankingParams {
            prime_lift: 2,
            ..current
        };
        for c in &candidates(lifted_base, &[], 64) {
            assert!(moved(*c, lifted_base) <= 1, "{c:?}");
        }
        assert_eq!(
            all.len(),
            1 + (RECENCY.len() - 1)
                + (CAPS.len() - 1)
                + (crate::core::ranking::MULTIPLIERS.len() - 1)
                + (crate::core::ranking::HALF_LIVES.len() - 1)
                + (crate::core::ranking::PRIME_LIFTS.len() - 1)
                + 1,
            "every rung on every ladder, once, and nothing off them"
        );
    }

    #[test]
    fn the_pass_budget_covers_every_rung_on_every_axis() {
        // A tie keeps the current value, so the budget must reach the whole of every ladder.
        let zero = RankingParams {
            prime_lift: 0,
            ..RankingParams::default()
        };
        let at_zero = candidates(zero, &[], usize::MAX);
        assert_eq!(at_zero.len(), crate::jobs::tune::BUDGET - 1, "{at_zero:?}");
        // Not `BUDGET - 1`: at a zero lift with the sitting on, the off flip is offered.
        let primed_at_zero = candidates(
            RankingParams {
                sitting_prime: true,
                ..zero
            },
            &[],
            usize::MAX,
        );
        assert_eq!(
            primed_at_zero.len(),
            crate::jobs::tune::BUDGET,
            "{primed_at_zero:?}"
        );
        let lifted_base = RankingParams {
            prime_lift: 2,
            ..RankingParams::default()
        };
        let lifted = candidates(lifted_base, &[], usize::MAX);
        assert_eq!(lifted.len(), crate::jobs::tune::BUDGET, "{lifted:?}");
        // Each grid is checked against its own baseline above; a lifted one differs on two knobs.
        for c in &at_zero {
            assert!(moved(*c, zero) <= 1, "{c:?}");
        }
        for c in &lifted {
            assert!(moved(*c, lifted_base) <= 1, "{c:?}");
        }
    }

    #[test]
    fn the_chooser_walks_the_lift_ladder_both_ways_from_the_shipped_rung() {
        let current = RankingParams::default();
        assert_eq!(current.prime_lift, 1, "the shipped rung");
        let grid = candidates(current, &[], crate::jobs::tune::BUDGET);
        let lifts: Vec<usize> = grid
            .iter()
            .map(|c| c.prime_lift)
            .filter(|l| *l != current.prime_lift)
            .collect();
        assert_eq!(lifts, vec![0, 2, 4]);
    }

    #[test]
    fn the_sitting_flip_is_not_offered_where_it_can_do_nothing() {
        // At a zero lift the flip is a sure tie, never adopted and so never in `tried_candidates`.
        let current = RankingParams {
            prime_lift: 0,
            ..RankingParams::default()
        };
        let grid = candidates(current, &[], crate::jobs::tune::BUDGET);
        assert!(
            grid.iter().all(|c| !c.sitting_prime),
            "no sitting flip at a lift of zero"
        );

        // Turning it off is still offered: the lift ladder can walk back to zero under a sitting.
        let primed = RankingParams {
            sitting_prime: true,
            ..current
        };
        let grid = candidates(primed, &[], crate::jobs::tune::BUDGET);
        assert!(
            grid.iter().any(|c| !c.sitting_prime),
            "a knob that cannot be turned off is not on the ladder: {grid:?}"
        );
    }

    #[test]
    fn the_sitting_flip_is_offered_once_a_lift_has_been_adopted() {
        let current = RankingParams {
            prime_lift: 2,
            ..RankingParams::default()
        };
        let grid = candidates(current, &[], crate::jobs::tune::BUDGET);
        let flips: Vec<bool> = grid
            .iter()
            .map(|c| c.sitting_prime)
            .filter(|s| *s != current.sitting_prime)
            .collect();
        assert_eq!(
            flips,
            vec![true],
            "exactly one flip, and it is the other rung"
        );
    }

    #[tokio::test]
    async fn a_pair_with_a_sitting_ranks_differently_at_lift_two_and_the_same_without_one() {
        use crate::core::search::Priming;
        let (core, order) = seeded().await;
        // At lift 2 with the sitting on, the last-ranked hit climbs two places on the Judge door.
        let with = Pair {
            query: QUERY.into(),
            satisfies: vec![order[5].clone()],
            query_vec: None,
            priming: Some(Priming {
                activation: Default::default(),
                sitting: [order[5].clone()].into_iter().collect(),
                due: Default::default(),
            }),
            served_rank: None,
            served_reranked: false,
            exclude: Vec::new(),
        };
        let without = Pair {
            priming: None,
            ..with.clone()
        };
        let current = *core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lifted = RankingParams {
            prime_lift: 2,
            sitting_prime: true,
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

    #[tokio::test]
    async fn a_recorded_sitting_moves_a_rank_when_only_the_flip_changes() {
        // The evidence was recorded with the knob off, so the sitting shaped nothing this replays.
        use crate::core::search::Priming;
        let (core, order) = seeded().await;
        let buried = order[5].clone();
        let pair = Pair {
            query: QUERY.into(),
            satisfies: vec![buried.clone()],
            query_vec: None,
            priming: Some(Priming {
                activation: Default::default(),
                sitting: [buried].into_iter().collect(),
                due: Default::default(),
            }),
            served_rank: None,
            served_reranked: false,
            exclude: Vec::new(),
        };
        let off = RankingParams {
            prime_lift: 2,
            sitting_prime: false,
            ..*core
                .ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let on = RankingParams {
            sitting_prime: true,
            ..off
        };
        let before = rank_of(&core, &pair, off, false).await.unwrap();
        let after = rank_of(&core, &pair, on, false).await.unwrap();
        assert_eq!(before, Some(5), "the knob off is the served order");
        assert_eq!(
            after,
            Some(3),
            "and on, the sitting lifts it by the bounded step"
        );
    }

    #[tokio::test]
    async fn the_sitting_flip_ties_at_a_zero_lift() {
        use crate::core::search::Priming;
        let (core, order) = seeded().await;
        let buried = order[5].clone();
        let pair = Pair {
            query: QUERY.into(),
            satisfies: vec![buried.clone()],
            query_vec: None,
            priming: Some(Priming {
                activation: Default::default(),
                sitting: [buried].into_iter().collect(),
                due: Default::default(),
            }),
            served_rank: None,
            served_reranked: false,
            exclude: Vec::new(),
        };
        let off = RankingParams {
            prime_lift: 0,
            sitting_prime: false,
            ..*core
                .ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let on = RankingParams {
            sitting_prime: true,
            ..off
        };
        assert_eq!(
            rank_of(&core, &pair, off, false).await.unwrap(),
            rank_of(&core, &pair, on, false).await.unwrap(),
            "the flip cannot move anything while the lift is zero"
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
}
