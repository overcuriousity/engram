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
        let ranks = [Some(0), Some(1), None];
        assert!((mrr(&ranks) - (1.0 + 0.5) / 3.0).abs() < 1e-9);
    }

    #[test]
    fn a_miss_contributes_nothing_to_mrr_rather_than_being_dropped() {
        assert!((mrr(&[Some(0), None]) - 0.5).abs() < 1e-9);
        assert_eq!(mrr(&[]), 0.0);
    }
}
