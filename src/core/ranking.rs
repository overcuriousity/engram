//! The scoring knobs a sweep may move while the server runs.
//!
//! Everything else that shapes ranking is read once at startup and threaded
//! down. These two are different: the tuning sweep ranks the same judged pairs
//! under a grid of them in one pass, and applying its recommendation has to
//! change the search the *next* request runs. So they live behind
//! `Core::ranking` rather than being copied into the places that use them.

use crate::config::VectorConfig;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankingParams {
    pub recency_weight: f32,
    /// Chunks one document may contribute. `None` lets a single document fill
    /// the whole list — which is what `ask` wants, and what the sweep offers as
    /// one of its candidates.
    pub per_source_cap: Option<usize>,
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
        }
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
