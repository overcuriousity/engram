//! Two numbers, because they answer different questions.
//!
//! Recall asks whether the answer was on the page at all. MRR asks how far
//! down it was. A ranking change can improve one and hurt the other, and which
//! matters is a judgement about what a search page is for — so the harness
//! reports both rather than choosing.

/// Fraction of queries whose expected chunk landed within the first `k`
/// results. `None` is a miss.
pub fn recall_at(ranks: &[Option<usize>], k: usize) -> f64 {
    if ranks.is_empty() {
        return 0.0;
    }
    let hits = ranks
        .iter()
        .filter(|r| matches!(r, Some(i) if *i < k))
        .count();
    hits as f64 / ranks.len() as f64
}

/// Mean reciprocal rank, counting a miss as zero rather than excluding it.
/// Excluding misses would let a system that answers one query perfectly and
/// fails nineteen report a perfect score.
pub fn mrr(ranks: &[Option<usize>]) -> f64 {
    if ranks.is_empty() {
        return 0.0;
    }
    let total: f64 = ranks
        .iter()
        .map(|r| r.map_or(0.0, |i| 1.0 / (i as f64 + 1.0)))
        .sum();
    total / ranks.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_counts_a_hit_anywhere_within_k() {
        // Ranks are zero-based: 0 is the top result, 9 is the tenth.
        let ranks = [Some(0), Some(9), Some(10), None];
        assert!((recall_at(&ranks, 10) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn recall_of_nothing_found_is_zero_not_a_division_by_zero() {
        assert_eq!(recall_at(&[None, None], 10), 0.0);
        assert_eq!(recall_at(&[], 10), 0.0);
    }

    #[test]
    fn mrr_is_the_mean_of_the_reciprocal_ranks() {
        // 1/1 and 1/2, averaged over three queries including the miss.
        let ranks = [Some(0), Some(1), None];
        assert!((mrr(&ranks) - (1.0 + 0.5) / 3.0).abs() < 1e-9);
    }

    #[test]
    fn a_miss_contributes_nothing_to_mrr_rather_than_being_dropped() {
        // Dropping misses would make a system that answers one query
        // perfectly and fails nineteen score 1.0.
        assert!((mrr(&[Some(0), None]) - 0.5).abs() < 1e-9);
        assert_eq!(mrr(&[]), 0.0);
    }
}
