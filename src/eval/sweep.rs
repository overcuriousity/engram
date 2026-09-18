//! What the idle pass replays: the pairs, the ranks, the gate and the chooser.
//!
//! The cargo harness froze a corpus because its numbers had to be comparable
//! across months. The pass asks a smaller question: what would *these* pairs
//! score under neighbouring settings, right now, against the base as it
//! stands. Baseline and candidates run in one pass over one index, so nothing
//! needs freezing and nothing needs re-embedding — every pair carries the
//! vector its query was searched with, and every candidate is one vector read
//! over it, whether it reorders what came back or changes how much comes back.
//!
//! Two kinds of pair, one shape. A verdict on the bar — *this artifact was the
//! answer* — and a positive observation — an excerpt an answer drew on, a
//! result somebody opened — make the same claim about the same query, and
//! `evidence_pairs` hands both to the ladder as one sample. Where a verdict
//! confirms the very open it was given under, the claim is counted once.
//!
//! It reads the live index and only reads it. `Door::Judge` and `mark: false`
//! are the same discipline every replay follows, for the same reason: a
//! replay is not someone reading their notes, and its queries are run in full
//! knowledge of their answers.

use crate::core::Core;
use crate::core::ranking::RankingParams;
use crate::error::Result;
use crate::eval::metrics::{mrr, recall_at};
use crate::store::eval_runs::{DiffRow, NewEvalRun};

/// The `k` in recall@k, and the depth a rank is looked for in. The judge
/// page's own figure is recall@10; a replay reporting recall@20 beside it would
/// be two numbers with one name.
pub(crate) const LIMIT: usize = 10;

/// The recency weight and per-source cap ladders. Both are scoring knobs —
/// they reorder what retrieval already returned — and sit here beside the
/// chooser that walks them; the retrieval ladders live in `core::ranking`.
const RECENCY: [f32; 5] = [0.0, 0.05, 0.1, 0.15, 0.25];
const CAPS: [Option<usize>; 4] = [Some(2), Some(3), Some(5), None];

/// A bounded set of candidates, drawn a step at a time from the running
/// configuration rather than enumerated.
///
/// The grid is twenty candidates over two axes and every axis added multiplies
/// it; this is what the idle pass walks instead, over all five knobs of
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
    // The sitting shares the lift's budget, so below a non-zero lift turning
    // it *on* is a guaranteed tie, and offering it would burn a rank per pair
    // every quiet period, forever, on a question the arithmetic already
    // answers. Not offered where it can do nothing, the way `rerank` is not
    // offered where no reranker is configured. The practical effect is an
    // order: the lift ladder is walked first, and the sitting is asked about
    // only once there is a budget for it to share.
    //
    // Turning it *off* is offered whatever the lift, and that asymmetry is the
    // whole point. Adopt the flip, then let the lift ladder walk back to zero,
    // and the rule above withdrew the axis: the generation row, the file an
    // Apply writes and the Insights parameter string all went on saying the
    // sitting was on while it did nothing, and nothing could ever propose
    // saying otherwise. A knob that cannot be turned off is not on the ladder.
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

/// Replayed pairs a candidate needs behind it, whatever it scores. Ten,
/// the same floor as `lived::MIN_OBSERVATIONS`, `rehearsed::MIN_PROBES` and
/// `tune::MIN_BAND`, and here it is the one the design already claimed to
/// have: "a base with four observations adopts nothing" was not true of the
/// arithmetic below. Two net better pairs out of four is two opens that each
/// moved up one place under a neighbouring rung — a whole generation adopted,
/// and a watch begun, on that. At ten the same two net pairs are a fifth of
/// the sample rather than the whole of it.
///
/// Applied by `score` and `rerank_flip` rather than inside `recommend`,
/// because `recommend` is arithmetic two callers share and only one of them is
/// choosing a candidate: `jobs::retract` asks it whether one corpus action's
/// own record beats its replay, over the observations naming that one
/// artifact. A floor there would not raise the bar on a candidate, it
/// would stop the corpus rules taking a merge or a condensation back at all on
/// any base that has not been used a great deal — a different decision, and
/// nobody's here to make.
pub const MIN_PAIRS: usize = 10;

/// The gate, and the reason the whole feature is safe to run automatically.
///
/// An aggregate delta can be a single flipped pair wearing a percentage: on
/// fifty pairs one is two points of recall. Requiring two *net* better pairs
/// is what a change has to look like before it is worth a person's attention,
/// and refusing any candidate that costs either aggregate keeps a trade the
/// operator did not ask for from being presented as an improvement. Ties keep
/// the current values, always: a knob that moves nothing should keep its
/// default.
///
/// The arithmetic only. How big a sample it takes before the answer is worth
/// acting on is the caller's question, because the callers are asking
/// different ones — see `MIN_PAIRS`.
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
/// The last tie-break, and the reason it has to exist: among candidates whose
/// rank vectors are identical — the ordinary case, since most pairs carry no
/// recency signal at all — the first one walked would otherwise win, and a
/// cap change could arrive with a recency change attached to it, measured by
/// nothing. `recommend` already promises a knob that moves nothing keeps its
/// value; this is that promise held among the candidates rather than only
/// against the base.
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
    /// The rank this observation was served at — 1-based and unbounded, as
    /// `observations.rank` records it. The rerank axis's base: the one row
    /// that has the reranker in it where the reranker is live. `None` for a
    /// judged pair whose answer was never in the pool, and for an observation
    /// that recorded no rank at all.
    ///
    /// Raw, and narrowed to the window by `served()` at the point of
    /// measurement. Stored already narrowed, "served beyond `LIMIT`" and
    /// "never served" were the same `None`, and `rerank_flip` — which selects
    /// on exactly that — threw away every deep observation in the sample.
    /// Those are the rows `observation_pairs` spends half its budget going out
    /// of its way to gather, so the axis could not accumulate the evidence it
    /// needs to offer a flip.
    pub(crate) served_rank: Option<i64>,
    /// Whether the list `served_rank` is a place in was ordered by the
    /// reranker — which is what makes it a baseline the rerank axis can be
    /// scored against.
    ///
    /// Not every positive observation is. `Opened` is a place in a search
    /// result list and `Cited` is a position in an answer's excerpt list, and
    /// `rerank.apply` scopes the two doors separately: under
    /// `apply = ["search"]` an ask never reranks, so a cited rank comes from a
    /// pipeline the flip is not about. Scored as though it did, the replay —
    /// which reranks — reads its own ordering as a change the flip caused, and
    /// counts it for or against a setting that had nothing to do with it.
    ///
    /// Nothing writes `Cited` observations today, so no live base can have
    /// such a row; this is the rule stated where the assumption lives, rather
    /// than a bug being fixed. A citation path is an obvious thing to add.
    pub(crate) served_reranked: bool,
    /// Artifacts that must not count as results for this pair.
    ///
    /// A capture probe's query *is* an artifact's text, so that artifact
    /// answers it at cosine 1.0 and a perfect lexical match and stands at rank
    /// one of every replay, whatever the parameters are. Left in, it caps a
    /// probe's reciprocal rank at one half — which is half the range
    /// `Rehearsed::noise` is calibrated over, so the refuse gate was twice as
    /// lenient as it reads — and makes the source an interferer of its own
    /// owner in every rehearsal. Removed before the rank is read, so a probe
    /// measures where the *owner* landed among everything else.
    ///
    /// Empty for an observation pair: a person's query is not an artifact.
    pub(crate) exclude: Vec<String>,
}

impl Pair {
    /// Where the artifact was served, 0-based, as the replay measures it —
    /// `None` past `LIMIT`, which is a miss on both sides alike. See
    /// `served_at`.
    pub(crate) fn served(&self) -> Option<usize> {
        served_at(self.served_rank)
    }
}

/// The place an observation was served at, 0-based, as the replay measures it.
///
/// `observations.rank` is 1-based and unbounded: `record_search` writes a rank
/// for every candidate in the pool, which is `feedback.candidates` wide. Every
/// number it is ever compared against comes from `rank_of`, which searches at
/// `limit: LIMIT` and answers `None` for anything past it. Carried through
/// raw, an opened result that sat at pool position fifteen became
/// `Some(14)` against a replay's `None`, and `recommend` read that as the
/// candidate having made the pair *worse* — though both are misses at ten —
/// while `mrr` credited the served side with a fifteenth the replay could not
/// earn. `rerank_flip` was the loser: its base was inflated on both aggregates
/// at once, so the flip was systematically under-offered and `Flip::predicted`
/// biased negative.
///
/// So a hit outside the window is a miss here, which is what it is to
/// everything else in this module.
pub(crate) fn served_at(rank: Option<i64>) -> Option<usize> {
    rank.map(|r| (r - 1).max(0) as usize).filter(|r| *r < LIMIT)
}

/// How many pairs one pass will draw on, of both kinds together. A bound
/// rather than a setting: the pass re-ranks every pair under every candidate,
/// so the work is pairs times candidates, and a base that has been used for a
/// year would otherwise make one pass unbounded.
///
/// One bound over the whole sample and not one per kind, because the two costs
/// it holds down are both counted in pairs. The grid is `tune::BUDGET` vector
/// reads per pair, which a night absorbs either way; the flip is one reranker
/// call per pair, on the card that is also serving embeddings and the chat
/// model, and that is the one a doubled sample would be felt on.
pub(crate) const OBSERVATION_LIMIT: usize = 500;

/// Where one configuration put the answer to one pair. `None` past `LIMIT`.
///
/// `rerank` is whether the reranker may run. The ladder may spend no
/// inference at all, so it passes `false` and measures the ordering that
/// feeds the reranker where one serves search; the flip passes `true`, once,
/// to ask what the reranker would have changed.
pub(crate) async fn rank_of(
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
    Ok(kept(pair, &results).position(|id| pair.satisfies.iter().any(|s| s == id)))
}

/// The results a pair is scored over: everything the search returned, minus
/// what the pair excludes. See `Pair::exclude`.
fn kept<'a>(
    pair: &'a Pair,
    results: &'a [crate::core::search::SearchResult],
) -> impl Iterator<Item = &'a String> {
    results
        .iter()
        .map(|r| &r.artifact_id)
        .filter(|id| !pair.exclude.iter().any(|e| &e == id))
}

/// Where the answer landed, 0-based, and every artifact above it in order —
/// the whole top `LIMIT` when it was not found. `rank_of` with the list kept,
/// for the replay that wants to know who stood in the way.
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

/// The answers people gave under one generation, as pairs the ranking can be
/// scored on, and how many named an artifact that no longer exists.
///
/// The same claim an observation makes — this query was answered by that
/// artifact — made out loud, on the bar under a result or the rail beside a
/// list. Far fewer than the observations, and never the volume of the sample;
/// but each one is a person saying so, and an answer the pool never held is
/// the one kind of pair no observation can ever supply, since an open needs
/// something on screen to open.
///
/// Both rules `observation_pairs` enforces apply unchanged, because these go
/// through the same `get_artifact` and the same `satisfied_by`: a merged
/// artifact is satisfied by what superseded it, and a deleted one is skipped
/// rather than scored as a miss. Each pair carries the vector its search was
/// made with and the priming that search recorded, so a replay embeds nothing
/// and the lift axis has something to read.
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
                    // A verdict is given on a search result list, and that
                    // list is the one the search scope reranks or does not.
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

/// Everything the ladder is scored on under one generation: the judged answers
/// and the positive observations, and how many of either named an artifact
/// that is gone.
///
/// One claim, counted once. A result somebody opened and then confirmed on
/// the bar is an `Opened` observation and a `hit` verdict on the same search
/// naming the same artifact; handed in as two pairs it would weigh twice what
/// either says alone. The verdict is kept — it is the one a person made — and
/// the observation that repeats it is left out. An open on the same search of
/// a *different* artifact is a different claim and stays.
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
    // One budget over both kinds — see `OBSERVATION_LIMIT` — and the verdicts
    // are served out of it first. Not an ordering of convenience: a verdict is
    // a person having said so, and an observation is the system's reading of
    // an open, so where the two compete for the last of the budget the one a
    // person made is the one that is kept. A deleted artifact costs a verdict
    // its place and hands the room back to the observations, which is right:
    // what was skipped is not evidence either.
    let room = OBSERVATION_LIMIT.saturating_sub(pairs.len());
    let (observed, left_out) =
        observation_pairs_except(core, generation_id, &confirmed, room).await?;
    pairs.extend(observed);
    skipped += left_out;
    Ok((pairs, skipped))
}

/// The positive observations under one generation, as pairs the ranking can be
/// scored on, and how many named an artifact that no longer exists.
///
/// Bounded at what the verdicts left of `OBSERVATION_LIMIT`, and within that
/// bound split in half.
///
/// Prioritising by how wrong the system was — worst-placed first — is the
/// half that reads well and cannot stand alone. Every candidate is scored by
/// `rank_of` at `LIMIT`, so an observation whose artifact sat below ten is
/// `None` under the baseline, and on a well-used base the worst five hundred
/// are all of them: `recommend` compares `None` with `None` five hundred
/// times, never reaches its net-two, and `score()` answers `best: None` for
/// the life of the base. The selection was choosing precisely the rows the
/// scorer cannot tell apart.
///
/// So half the budget goes to those — a miss the knobs can lift into the
/// window is a real recall gain, and nothing else would ever look for one —
/// and half to observations that placed *inside* the window, where a
/// candidate moving a hit from six to three is a difference `recommend` can
/// actually see. Either half takes the other's unused room. It is the same
/// split `sleep::rehearse` makes between what wobbles and the lap, for the
/// same reason: one signal spent whole is one signal.
///
/// Each pair carries the vector the query was searched with, so a replay
/// embeds nothing.
pub(crate) async fn observation_pairs(
    core: &Core,
    generation_id: &str,
) -> Result<(Vec<Pair>, i64)> {
    observation_pairs_except(core, generation_id, &Default::default(), OBSERVATION_LIMIT).await
}

/// `observation_pairs`, leaving out every observation that repeats a verdict:
/// one whose `(event_id, artifact_id)` is in `confirmed`, and drawing at most
/// `budget` of what is left. See `evidence_pairs`.
///
/// `budget` is the room the verdicts did not take, so it is `OBSERVATION_LIMIT`
/// where there are none and zero where they filled it. The halves below split
/// what they are given rather than the constant: a shrunken budget still buys
/// the worst-placed observations their share, which is the half that cannot
/// stand alone and the half nothing else would look for.
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
    // Worst-placed first; newest first among equals, which is the order they
    // arrived in.
    observations.sort_by_key(|o| std::cmp::Reverse(o.rank.unwrap_or(i64::MAX)));
    // `rank` is 1-based, so `> LIMIT` is exactly what `served_at` calls a miss.
    let (deep, inside): (Vec<_>, Vec<_>) = observations
        .into_iter()
        .partition(|o| o.rank.is_none_or(|r| r as usize > LIMIT));
    let half = budget / 2;
    // Each half takes the other's unused room, so a base with nothing on one
    // side still fills the budget from the other.
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
                    served_rank: o.rank,
                    // Which door's list this rank is a place in, against the
                    // scope the reranker is configured for.
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

    // The sample floor. The run is still journalled — a quiet pass is a fact
    // about the base — but nothing under `MIN_PAIRS` pairs clears the gate,
    // and `tune::propose` adopts only what this picks.
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

    /// The row the journal keeps. A quiet pass is recorded too: without the
    /// row a page can only say nothing, which reads as "no pass has ever run".
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

/// What `rerank_flip` came back with.
///
/// Three outcomes, not two. "No flip" and "somebody came back mid-replay" are
/// opposite instructions to the caller — the first says carry on to the next
/// rule, the second says stop the pass — and an `Option` that spelled both
/// `None` had the idle pass adopt a generation and rewrite the running ranking
/// while a user was searching. `score` returns `None` for exactly the second
/// case; this says which one it is out loud.
pub(crate) enum FlipOffer {
    /// The flip cleared `recommend` against the served ranks.
    Offered(Flip),
    /// No reranker, no pair with a served rank, or the flip did not clear the
    /// gate. The pass carries on.
    Held,
    /// A search or a question landed while the replay ran. The pass stops.
    Stopped,
}

impl FlipOffer {
    /// The flip, where one was offered. The pass matches on the variants
    /// directly, because it has to answer `Stopped` differently; this is for
    /// the tests, which drive the gate on its own.
    #[cfg(test)]
    pub(crate) fn offered(self) -> Option<Flip> {
        match self {
            Self::Offered(f) => Some(f),
            _ => None,
        }
    }
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
) -> Result<FlipOffer> {
    if !core.reranks_search() {
        return Ok(FlipOffer::Held);
    }
    // Every pair that came from an observation with a rank on it, deep ones
    // included: a place past `LIMIT` is a miss the base has to be charged
    // with, not a row to leave out of the sample.
    //
    // And only ranks from a list the reranker actually ordered — see
    // `Pair::served_reranked`. This axis is scored against what was served,
    // so a rank from a door the reranker does not serve is not a baseline for
    // it: the replay reranks, and the difference it would read is its own.
    let with_served: Vec<&Pair> = pairs
        .iter()
        .filter(|p| p.served_rank.is_some() && p.served_reranked)
        .collect();
    // Before the replay, not after it: the same sample floor `score` applies,
    // and here it also saves one reranker call per pair on a base that could
    // not have been offered the flip whatever the calls came back with.
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

/// A base with an improvement in it, for this module's tests and the idle
/// pass's: the same corpus, so what the replay can find the pass can adopt.
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
        // The baseline the replay is measured against: one source may fill the
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

    /// One search of `QUERY` on the web door, judged a hit on `expect`, the
    /// way the bar records one: the search carries the vector the query was
    /// embedded with, and the verdict names the answer. The event's id, for a
    /// test that wants to observe on the same search.
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

    /// The same, from the search `event` recorded — an open on that list.
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
        // Seed under the generation that is live, then mint another — which
        // supersedes it. A model change ends the era its evidence belonged to,
        // and the pass reads the live generation's evidence only.
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
        // A result opened and then confirmed on the bar is one claim made
        // twice — an observation and a verdict on the same search naming the
        // same artifact. Counted twice it would weigh double.
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

        // A different artifact opened on the same search is a different
        // claim, and an open with no search behind it is not this verdict's.
        observe_on(&core, &generation, &order[4], Source::Opened, Some(&event)).await;
        observe(&core, &generation, &order[3], Source::Cited).await;
        assert_eq!(evidence_pairs(&core, &generation).await.unwrap().0.len(), 3);
    }

    /// One budget over both kinds, and the verdicts served out of it first.
    ///
    /// The bound exists because the pass's work is pairs times candidates and
    /// the pair count grows with the age of the base — see `OBSERVATION_LIMIT`.
    /// Drawn per kind it was two bounds wearing one name: a pass could rank
    /// twice what the constant promised, and where a reranker serves search
    /// that is twice the inference, which is the only inference the pass ever
    /// spends.
    #[tokio::test]
    async fn the_two_kinds_of_evidence_share_one_budget_and_a_verdict_is_served_first() {
        let (core, order) = seeded().await;
        let generation = a_generation(&core).await;
        // Between them far more than the budget. A verdict here carries no
        // served place — the bar's search recorded no pool — and every
        // observation carries one, so the two are told apart below by that.
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

        // And where the verdicts alone fill it, the observations take none —
        // the budget is spent on what people said, not topped up past it.
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
        // Housekeeping, not a ranking result. Scored as a miss it would look
        // like a ranking failure forever; raised it would stop every later
        // pass over one deletion.
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

    /// `observations.rank` is 1-based and unbounded — `record_search` writes
    /// one for every candidate in the pool — while everything it is compared
    /// against is measured at `LIMIT`. Carried through raw, a hit at pool
    /// position fifteen was `Some(14)` against a replay's `None`, which
    /// `recommend` scored as the candidate having made it worse and `mrr`
    /// credited with a fifteenth no replay could earn.
    #[test]
    fn a_served_place_outside_the_window_is_the_miss_it_is_to_everything_else() {
        assert_eq!(served_at(Some(1)), Some(0));
        assert_eq!(served_at(Some(LIMIT as i64)), Some(LIMIT - 1));
        assert_eq!(served_at(Some(LIMIT as i64 + 1)), None);
        assert_eq!(served_at(Some(15)), None);
        assert_eq!(served_at(None), None);
        // A rank of zero should not exist — the column counts from one — and
        // if one ever does it is the first place, not a negative.
        assert_eq!(served_at(Some(0)), Some(0));
    }

    /// Half the budget to what the window can measure. Every candidate is
    /// scored by `rank_of` at `LIMIT`, so an observation that sat below ten is
    /// `None` under the baseline and under every candidate alike: selecting
    /// the worst five hundred selected precisely the rows the scorer cannot
    /// tell apart, `recommend` never reached its net-two, and `score()`
    /// answered `best: None` for the life of the base.
    #[tokio::test]
    async fn the_observation_budget_is_not_spent_wholly_on_places_the_window_cannot_see() {
        let (core, order) = seeded().await;
        let generation = a_generation(&core).await;
        // More deep observations than the whole budget, and a handful inside
        // the window behind them.
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

    /// One observation naming `artifact` at `rank`, 1-based.
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

    /// `served` is 0-based, the way the replay measures; the column it stands
    /// for counts from one.
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
        // The fake reranker reverses the list. Served ranks are what it
        // produced; the replay without it is the vector order.
        let (core, order, _) = super::test_support::seeded_with_reranker().await;
        let current = RankingParams {
            rerank: true,
            ..*core.ranking.read().unwrap()
        };
        // Five of each: the gate wants `MIN_PAIRS` behind a candidate.
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

    /// A served place past `LIMIT` is a miss the base is charged with, not a
    /// row to leave out of the sample.
    ///
    /// `served_at` maps every rank at or past `LIMIT` to `None`, so selecting
    /// on the narrowed value could not tell "served, deep" from "never
    /// served" — and threw away exactly the deep observations
    /// `observation_pairs` spends half its budget going out of its way to
    /// gather. With nothing left in the sample the axis could never
    /// accumulate the evidence a flip needs.
    #[tokio::test]
    async fn the_rerank_axis_keeps_the_deep_observations_the_budget_went_after() {
        let (core, order, _) = super::test_support::seeded_with_reranker().await;
        let current = RankingParams {
            rerank: true,
            ..*core.ranking.read().unwrap()
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
            ..*core.ranking.read().unwrap()
        };
        // Served where the vector order already puts them: a tie, read over
        // enough pairs that the tie is what holds it rather than the floor.
        let pairs: Vec<Pair> = (0..5)
            .flat_map(|_| [served_pair(&order, 0, 0), served_pair(&order, 1, 1)])
            .collect();
        assert!(matches!(
            rerank_flip(&core, &pairs, current, None).await.unwrap(),
            FlipOffer::Held
        ));
    }

    /// "No flip" and "somebody came back" are opposite instructions, and the
    /// pass that read both as `None` went on to adopt a generation and rewrite
    /// the running ranking underneath the person who came back.
    #[tokio::test]
    async fn a_search_landing_mid_replay_stops_the_flip_rather_than_holding_it() {
        let (core, order, _) = super::test_support::seeded_with_reranker().await;
        let current = RankingParams {
            rerank: true,
            ..*core.ranking.read().unwrap()
        };
        // The same pairs that offer a flip when nothing interrupts.
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
        let current = *core.ranking.read().unwrap();
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
            ..*core.ranking.read().unwrap()
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
        // And from a baseline that has the sitting axis available, so the flip
        // is counted rather than merely absent.
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
                // The shipped lift is above zero, so the sitting flip is on
                // offer from the shipped parameters.
                + 1,
            "every rung on every ladder, once, and nothing off them"
        );
    }

    #[test]
    fn the_pass_budget_covers_every_rung_on_every_axis() {
        // A tie keeps the current value, so an improvement two rungs out
        // behind a rung that ties would never be reached by a pass that only
        // tried the nearest step. The budget has to reach the whole ladder —
        // including the sitting flip, which only exists above a zero lift, so
        // the widest grid is the one the budget has to cover.
        // The lift ships above zero; the narrowest grid is the one at zero,
        // where the sitting flip is withheld.
        let zero = RankingParams {
            prime_lift: 0,
            ..RankingParams::default()
        };
        let at_zero = candidates(zero, &[], usize::MAX);
        assert_eq!(at_zero.len(), crate::jobs::tune::BUDGET - 1, "{at_zero:?}");
        // The other widest grid, and the reason the budget is not `BUDGET - 1`:
        // a base at a zero lift whose sitting is on is offered the flip that
        // turns it off.
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
        // Not re-asserted against one shared baseline: a lifted candidate
        // differs from the shipped parameters on two knobs by construction, so
        // that comparison would be either wrong or vacuous. Each grid is
        // checked against its own baseline in the test above.
        for c in &at_zero {
            assert!(moved(*c, zero) <= 1, "{c:?}");
        }
        for c in &lifted {
            assert!(moved(*c, lifted_base) <= 1, "{c:?}");
        }
    }

    #[test]
    fn the_chooser_walks_the_lift_ladder_both_ways_from_the_shipped_rung() {
        // Shipped at one: off is one step down and on offer, the way two is
        // one step up. Nearest first, so the pass can turn priming off on
        // evidence as readily as it can raise it.
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
        // At a lift of zero the flip is a guaranteed tie — `prime` returns
        // early — and a tie is never adopted, never becomes a generation, and
        // so never reaches `tried_candidates`, which holds only the reverted
        // and the refused. Offered here it would be re-measured every quiet
        // period forever, at one rank per pair, to settle something the
        // arithmetic settles for free.
        let current = RankingParams {
            prime_lift: 0,
            ..RankingParams::default()
        };
        let grid = candidates(current, &[], crate::jobs::tune::BUDGET);
        assert!(
            grid.iter().all(|c| !c.sitting_prime),
            "no sitting flip at a lift of zero"
        );

        // Turning it off is another matter. The lift ladder can walk back to
        // zero under an adopted `sitting_prime = true`, and there the base was
        // stuck saying the sitting was on while it did nothing.
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
        // The last-ranked hit was read in this sitting: at lift 2, and with the
        // sitting flag on, it climbs two places on the Judge door, where
        // priming is otherwise off.
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
        let current = *core.ranking.read().unwrap();
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
        // The counterfactual the axis rests on, with the lift held constant so
        // the flip is the only thing that moved. Honest as a counterfactual
        // precisely because the searcher saw the unprimed order: the evidence
        // was recorded while the knob was off, so the sitting influenced
        // nothing about the list this replays.
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
            ..*core.ranking.read().unwrap()
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
        // Why the chooser does not offer the axis there: there is nothing to
        // measure, and a tie keeps the current value forever.
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
            ..*core.ranking.read().unwrap()
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
