//! Ranking knobs a sweep may change while the server runs, kept behind `Core::ranking`
//! so adopting a candidate changes the next request's search.

use crate::config::VectorConfig;

/// The rungs the idle pass may step the pool depth along.
pub const MULTIPLIERS: [usize; 5] = [1, 2, 3, 5, 8];
/// The rungs for the recency half-life, in days.
pub const HALF_LIVES: [u32; 5] = [30, 90, 180, 365, 730];
/// Zero is off; the shipped value sits one above it so the pass can walk either way.
pub const PRIME_LIFTS: [usize; 4] = [0, 1, 2, 4];
/// A switch, kept ladder-shaped so the chooser treats it like every other axis.
pub const SITTING_PRIMES: [bool; 2] = [false, true];
/// The rungs for `spread_max`: how many linked artifacts hang under the list.
pub const SPREADS: [usize; 6] = [0, 1, 2, 3, 5, 8];
/// A rung at or above `consolidate.auto_supersede` is never offered.
pub const REVIEW_MINS: [f32; 4] = [0.80, 0.84, 0.88, 0.92];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankingParams {
    pub recency_weight: f32,
    /// `None` lets one document fill the whole list, which is what `ask` wants.
    pub per_source_cap: Option<usize>,
    /// Fetch this many times the answer size when the cap or reranker will narrow it.
    pub candidate_multiplier: usize,
    /// How many days it takes a result's recency term to halve.
    pub recency_half_life_days: u32,
    /// How many places priming may lift a hit. Zero is off.
    pub prime_lift: usize,
    /// Shares `prime_lift`'s budget, so at a lift of zero it changes nothing.
    pub sitting_prime: bool,
    /// How many associated artifacts are appended under the ranked list.
    pub spread_max: usize,
    /// Serving treats this as `false` where no reranker is configured.
    pub rerank: bool,
    /// Here because `relate.rs` reads the running generation off this lock.
    pub review_min: f32,
}

/// Read from the same functions as `VectorConfig`'s serde defaults so the two cannot drift.
impl Default for RankingParams {
    fn default() -> Self {
        Self {
            recency_weight: crate::config::default_recency_weight(),
            per_source_cap: Some(crate::config::default_per_source_cap()),
            candidate_multiplier: crate::config::default_candidate_multiplier(),
            recency_half_life_days: crate::config::default_recency_half_life_days(),
            prime_lift: crate::config::default_prime_lift(),
            sitting_prime: crate::config::default_sitting_prime(),
            spread_max: crate::config::default_spread_max(),
            rerank: crate::config::default_rerank_knob(),
            review_min: crate::config::default_review_min(),
        }
    }
}

impl RankingParams {
    /// `reranker_configured`: whether `[infer]` names a reranker, so the knob can start on.
    pub fn from_config(
        cfg: &VectorConfig,
        associate: &crate::config::AssociateConfig,
        consolidate: &crate::config::ConsolidateConfig,
        sitting: &crate::config::SittingConfig,
        reranker_configured: bool,
    ) -> Self {
        Self {
            recency_weight: cfg.recency_weight,
            // A file writes "no cap" as `0`; a cap of zero would return nothing.
            per_source_cap: match cfg.per_source_cap {
                0 => None,
                n => Some(n),
            },
            candidate_multiplier: cfg.candidate_multiplier.max(1),
            recency_half_life_days: cfg.recency_half_life_days.max(1),
            prime_lift: associate.prime_lift,
            sitting_prime: sitting.prime,
            spread_max: associate.spread_max,
            rerank: reranker_configured,
            review_min: consolidate.review_min,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector_config(per_source_cap: usize) -> VectorConfig {
        VectorConfig {
            url: String::new(),
            collection: String::new(),
            api_key: None,
            recency_weight: 0.05,
            recency_half_life_days: 180,
            pinned_boost: 0.15,
            weak_below: 0.35,
            per_source_cap,
            candidate_multiplier: 3,
        }
    }

    #[test]
    fn the_retrieval_knobs_are_read_from_the_file_beside_the_ranking_ones() {
        let p = RankingParams::from_config(
            &VectorConfig {
                candidate_multiplier: 5,
                recency_half_life_days: 90,
                ..vector_config(3)
            },
            &Default::default(),
            &Default::default(),
            &Default::default(),
            false,
        );
        assert_eq!(p.candidate_multiplier, 5);
        assert_eq!(p.recency_half_life_days, 90);
    }

    #[test]
    fn the_three_late_knobs_are_read_from_the_file_beside_the_others() {
        let associate = crate::config::AssociateConfig {
            prime_lift: 2,
            spread_max: 5,
            ..Default::default()
        };
        let p = RankingParams::from_config(
            &vector_config(3),
            &associate,
            &Default::default(),
            &Default::default(),
            true,
        );
        assert_eq!(p.prime_lift, 2);
        assert_eq!(p.spread_max, 5);
        assert!(p.rerank);
        let p = RankingParams::from_config(
            &vector_config(3),
            &associate,
            &Default::default(),
            &Default::default(),
            false,
        );
        assert!(
            !p.rerank,
            "no reranker configured means the knob starts off"
        );
    }

    #[test]
    fn the_sitting_flag_is_read_from_the_file_and_ships_off() {
        let sitting = crate::config::SittingConfig { prime: true };
        let p = RankingParams::from_config(
            &vector_config(3),
            &Default::default(),
            &Default::default(),
            &sitting,
            false,
        );
        assert!(p.sitting_prime, "the file's value is the starting rung");
        assert!(
            !RankingParams::default().sitting_prime,
            "and the shipped rung is off"
        );
    }

    #[test]
    fn the_shipped_prime_lift_and_spread_sit_on_their_ladders() {
        let p = RankingParams::default();
        assert!(PRIME_LIFTS.contains(&p.prime_lift));
        assert!(SPREADS.contains(&p.spread_max));
        assert!(SITTING_PRIMES.contains(&p.sitting_prime));
        assert_eq!(PRIME_LIFTS[0], 0);
        assert_eq!(PRIME_LIFTS[1], p.prime_lift);
        assert!(PRIME_LIFTS.windows(2).all(|w| w[0] < w[1]), "ascending");
        assert!(SPREADS.windows(2).all(|w| w[0] < w[1]), "ascending");
    }

    #[test]
    fn the_review_threshold_is_read_from_the_file_and_its_rung_sits_mid_ladder() {
        let consolidate = crate::config::ConsolidateConfig {
            review_min: 0.84,
            ..Default::default()
        };
        let p = RankingParams::from_config(
            &vector_config(3),
            &Default::default(),
            &consolidate,
            &Default::default(),
            false,
        );
        assert_eq!(p.review_min, 0.84);
        let d = RankingParams::default();
        assert!(REVIEW_MINS.contains(&d.review_min));
        assert!(REVIEW_MINS.windows(2).all(|w| w[0] < w[1]), "ascending");
    }

    #[test]
    fn the_shipped_values_sit_in_the_middle_of_their_ladders() {
        let d = RankingParams::default();
        assert_eq!(MULTIPLIERS[MULTIPLIERS.len() / 2], d.candidate_multiplier);
        assert_eq!(HALF_LIVES[HALF_LIVES.len() / 2], d.recency_half_life_days);
        assert!(MULTIPLIERS.windows(2).all(|w| w[0] < w[1]), "ascending");
        assert!(HALF_LIVES.windows(2).all(|w| w[0] < w[1]), "ascending");
    }

    #[test]
    fn a_cap_of_zero_is_no_cap_rather_than_a_search_that_returns_nothing() {
        assert_eq!(
            RankingParams::from_config(
                &vector_config(0),
                &Default::default(),
                &Default::default(),
                &Default::default(),
                false
            )
            .per_source_cap,
            None
        );
        assert_eq!(
            RankingParams::from_config(
                &vector_config(3),
                &Default::default(),
                &Default::default(),
                &Default::default(),
                false
            )
            .per_source_cap,
            Some(3)
        );
    }
}
