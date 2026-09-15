//! What a generation scores on the base's own probes.
//!
//! A yardstick, not a goal. Adoption stays on observations, where use is the
//! evidence; this may refuse a candidate and may revert a generation, and it
//! never adopts. What it buys is that no adoption and no live generation can
//! escape being measured — on a base nobody judges, this is the anchor.
//!
//! Counterfactual, on the current corpus: two parameter sets are replayed on
//! one probe set now, so corpus drift is not a confound. Nothing is embedded.

use crate::core::Core;
use crate::core::ranking::RankingParams;
use crate::error::Result;
use crate::eval::sweep::OBSERVATION_LIMIT;
use crate::store::rehearsals::Rehearsal;

/// Probes a side before two records can be called the same. Ten puts the
/// noise term at a tenth of the range — the reason `tune::MIN_BAND` is ten.
pub const MIN_PROBES: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rehearsed {
    pub probes: usize,
    pub found: usize,
    pub mrr: f64,
}

impl Rehearsed {
    /// One probe's worth on either side: a single probe moving between rank
    /// one and a miss moves MRR by `1/n`.
    pub fn noise(&self, other: &Rehearsed) -> f64 {
        1.0 / self.probes.max(1) as f64 + 1.0 / other.probes.max(1) as f64
    }
    /// Worse than `other` by more than the noise. False on an empty side:
    /// nothing separates anything from nothing.
    pub fn loses_to(&self, other: &Rehearsed) -> bool {
        self.probes > 0 && other.probes > 0 && other.mrr - self.mrr > self.noise(other)
    }
    /// Within the noise of each other over at least `MIN_PROBES` a side.
    pub fn indistinguishable(&self, other: &Rehearsed) -> bool {
        self.probes >= MIN_PROBES
            && other.probes >= MIN_PROBES
            && (self.mrr - other.mrr).abs() <= self.noise(other)
    }
    fn from_ranks(ranks: &[Option<usize>]) -> Rehearsed {
        Rehearsed {
            probes: ranks.len(),
            found: ranks.iter().filter(|r| r.is_some()).count(),
            mrr: crate::eval::metrics::mrr(ranks),
        }
    }
}

/// Whether a probe replay can tell `a` from `b` at all.
///
/// A replay goes through `Door::Judge` with no priming recorded, which is what
/// keeps a probe a measurement of the ranking rather than of whoever searched
/// last. It also means four knobs never reach it: the appended band
/// (`spread_max`), both halves of priming (`prime_lift`, `sitting_prime`), and
/// the review threshold, which search does not read. Two parameter sets that
/// differ only there replay identically, so `indistinguishable` says nothing
/// about them and `loses_to` can never be true.
///
/// Stated as what is blanked rather than what is kept, so a knob added later
/// counts as visible until somebody says otherwise — which is what every
/// replay assumed of every knob before this.
pub fn probes_tell_apart(a: RankingParams, b: RankingParams) -> bool {
    let seen = |p: RankingParams| RankingParams {
        spread_max: 0,
        prime_lift: 0,
        sitting_prime: false,
        review_min: 0.0,
        ..p
    };
    seen(a) != seen(b)
}

/// The probes with a retained result under `live_id` — the ones the lap has
/// reached — at most `OBSERVATION_LIMIT`.
pub async fn probe_set(core: &Core, live_id: &str) -> Result<Vec<Rehearsal>> {
    Ok(core
        .store
        .latest_results_under(live_id, OBSERVATION_LIMIT)
        .await?
        .into_iter()
        .map(|(p, _)| p)
        .collect())
}

/// Replay `probes` under `params`, now. `None` when somebody came back.
///
/// `rerank` is the caller's, because it is the one axis a replay cannot infer
/// from `params` alone. `search_inner` computes `query.rerank && params.rerank`,
/// so this used to hard-code `false` and the field was inert in every
/// rehearsal: the rerank-flip candidate `tune::propose` emits differs from
/// `current` in that field and nothing else, both sides replayed identically,
/// `loses_to` was never true, and the flip cleared the refusal gate whatever it
/// would actually have done. `sweep::flip_offer` passes `flipped.rerank` for
/// exactly this reason.
///
/// The caller passes `false` where the axis is held constant across the
/// comparison, which is every ladder candidate: a reranker run on both sides
/// cancels out of the verdict and costs a call per probe to do it.
pub async fn rehearsed_under(
    core: &Core,
    params: RankingParams,
    probes: &[Rehearsal],
    started: Option<i64>,
    rerank: bool,
) -> Result<Option<Rehearsed>> {
    let mut ranks = Vec::with_capacity(probes.len());
    for p in probes {
        if let Some(s) = started
            && core.store.activity_since(s).await?
        {
            return Ok(None);
        }
        let pair = crate::jobs::sleep::pair_of(core, p).await;
        ranks.push(crate::eval::sweep::rank_of(core, &pair, params, rerank).await?);
    }
    Ok(Some(Rehearsed::from_ranks(&ranks)))
}

/// The record from stored results, no replay. What Insights shows.
pub async fn rehearsed_live(core: &Core, live_id: &str) -> Result<Rehearsed> {
    let ranks: Vec<Option<usize>> = core
        .store
        .latest_results_under(live_id, OBSERVATION_LIMIT)
        .await?
        .iter()
        .map(|(_, r)| r.rank.map(|n| (n - 1).max(0) as usize))
        .collect();
    Ok(Rehearsed::from_ranks(&ranks))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(probes: usize, mrr: f64) -> Rehearsed {
        Rehearsed {
            probes,
            found: probes,
            mrr,
        }
    }

    #[test]
    fn one_probe_cannot_separate_anything_and_a_clear_loss_can() {
        assert!(
            !r(1, 0.0).loses_to(&r(1, 1.0)),
            "noise is 2.0 at one a side"
        );
        assert!(r(20, 0.50).loses_to(&r(20, 0.70)), "0.2 exceeds 0.1");
        assert!(!r(20, 0.65).loses_to(&r(20, 0.70)));
        assert!(
            !r(0, 0.0).loses_to(&r(20, 0.9)),
            "an empty side says nothing"
        );
    }

    #[test]
    fn a_replay_sees_the_ranking_knobs_and_not_the_band_priming_or_review() {
        let p = RankingParams::default();
        for unseen in [
            RankingParams {
                spread_max: p.spread_max + 1,
                ..p
            },
            RankingParams {
                prime_lift: p.prime_lift + 1,
                ..p
            },
            RankingParams {
                sitting_prime: !p.sitting_prime,
                ..p
            },
            RankingParams {
                review_min: p.review_min + 0.02,
                ..p
            },
        ] {
            assert!(!probes_tell_apart(p, unseen), "{unseen:?}");
        }
        for seen in [
            RankingParams {
                recency_weight: p.recency_weight + 0.1,
                ..p
            },
            RankingParams {
                candidate_multiplier: p.candidate_multiplier + 1,
                ..p
            },
            RankingParams {
                rerank: !p.rerank,
                ..p
            },
        ] {
            assert!(probes_tell_apart(p, seen), "{seen:?}");
        }
    }

    #[test]
    fn indistinguishable_needs_min_probes_a_side() {
        assert!(
            !r(5, 0.5).indistinguishable(&r(5, 0.5)),
            "thin evidence settles nothing"
        );
        assert!(r(10, 0.50).indistinguishable(&r(10, 0.55)));
        assert!(!r(10, 0.30).indistinguishable(&r(10, 0.55)));
    }
}
