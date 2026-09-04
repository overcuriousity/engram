//! The knobs a sweep may move while the server runs.
//!
//! Everything else that shapes ranking is read once at startup and threaded
//! down. These are different: the tuning sweep and the idle pass rank the same
//! pairs under several of them in one pass, and adopting a candidate has to
//! change the search the *next* request runs. So they live behind
//! `Core::ranking` rather than being copied into the places that use them.
//!
//! Two reorder what retrieval returned (`recency_weight`, `per_source_cap`);
//! two change what is retrieved at all (`candidate_multiplier`,
//! `recency_half_life_days`). Both kinds cost the idle pass the same thing —
//! one vector read per pair per candidate — which is what lets them share a
//! struct and a chooser.

use crate::config::VectorConfig;

/// The rungs the idle pass may step the pool depth along. Values, not a
/// threshold: the pass never prefers one over another except by measuring.
pub const MULTIPLIERS: [usize; 5] = [1, 2, 3, 5, 8];
/// The rungs for the recency half-life, in days.
pub const HALF_LIVES: [u32; 5] = [30, 90, 180, 365, 730];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankingParams {
    pub recency_weight: f32,
    /// Chunks one document may contribute. `None` lets a single document fill
    /// the whole list — which is what `ask` wants, and what the sweep offers as
    /// one of its candidates.
    pub per_source_cap: Option<usize>,
    /// How many times the answer size retrieval fetches when something
    /// downstream — the cap or the reranker — will narrow the list. Wider
    /// costs a bigger vector read and gives the cap more to choose from.
    pub candidate_multiplier: usize,
    /// How many days it takes a result's recency term to halve.
    pub recency_half_life_days: u32,
}

/// The shipped values — the same ones `VectorConfig`'s serde defaults hold,
/// read from the same functions so the two cannot drift.
impl Default for RankingParams {
    fn default() -> Self {
        Self {
            recency_weight: crate::config::default_recency_weight(),
            per_source_cap: Some(crate::config::default_per_source_cap()),
            candidate_multiplier: crate::config::default_candidate_multiplier(),
            recency_half_life_days: crate::config::default_recency_half_life_days(),
        }
    }
}

impl RankingParams {
    pub fn from_vector(cfg: &VectorConfig) -> Self {
        Self {
            recency_weight: cfg.recency_weight,
            // `0` is how a file says "no cap": a setting cannot hold `None`,
            // and a cap of zero would otherwise mean a search that returns
            // nothing at all.
            per_source_cap: match cfg.per_source_cap {
                0 => None,
                n => Some(n),
            },
            candidate_multiplier: cfg.candidate_multiplier.max(1),
            recency_half_life_days: cfg.recency_half_life_days.max(1),
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
        let p = RankingParams::from_vector(&VectorConfig {
            candidate_multiplier: 5,
            recency_half_life_days: 90,
            ..vector_config(3)
        });
        assert_eq!(p.candidate_multiplier, 5);
        assert_eq!(p.recency_half_life_days, 90);
    }

    #[test]
    fn the_shipped_values_sit_in_the_middle_of_their_ladders() {
        // A ladder walked from its end can only go one way; the pass would
        // then be told the shipped value is an extreme, which nobody decided.
        let d = RankingParams::default();
        assert_eq!(MULTIPLIERS[MULTIPLIERS.len() / 2], d.candidate_multiplier);
        assert_eq!(HALF_LIVES[HALF_LIVES.len() / 2], d.recency_half_life_days);
        assert!(MULTIPLIERS.windows(2).all(|w| w[0] < w[1]), "ascending");
        assert!(HALF_LIVES.windows(2).all(|w| w[0] < w[1]), "ascending");
    }

    #[test]
    fn a_cap_of_zero_is_no_cap_rather_than_a_search_that_returns_nothing() {
        assert_eq!(
            RankingParams::from_vector(&vector_config(0)).per_source_cap,
            None
        );
        assert_eq!(
            RankingParams::from_vector(&vector_config(3)).per_source_cap,
            Some(3)
        );
    }
}
