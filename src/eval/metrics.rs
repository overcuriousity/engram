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

/// One question's citation recall: the fraction of its carriers that were
/// cited. Each carrier is a list of ids that satisfy it — itself and whatever
/// superseded it. No carriers is nothing to miss, and scores 1.
pub fn fraction_cited(carriers: &[Vec<String>], cited: &[String]) -> f64 {
    if carriers.is_empty() {
        return 1.0;
    }
    let hit = carriers
        .iter()
        .filter(|alts| alts.iter().any(|a| cited.contains(a)))
        .count();
    hit as f64 / carriers.len() as f64
}

/// The four corners of "did it say nothing here when it should have".
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Abstention {
    pub should_and_did: usize,
    pub should_and_did_not: usize,
    pub should_not_did: usize,
    pub should_not_did_not: usize,
}

impl Abstention {
    /// `(expected, observed)` per question.
    pub fn tally(pairs: &[(bool, bool)]) -> Self {
        let mut t = Self::default();
        for &(expected, observed) in pairs {
            match (expected, observed) {
                (true, true) => t.should_and_did += 1,
                (true, false) => t.should_and_did_not += 1,
                (false, true) => t.should_not_did += 1,
                (false, false) => t.should_not_did_not += 1,
            }
        }
        t
    }
}

/// `(answers with no unsupported item, answers)`.
pub fn fully_supported(unsupported_counts: &[usize]) -> (usize, usize) {
    (
        unsupported_counts.iter().filter(|n| **n == 0).count(),
        unsupported_counts.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fraction_cited_counts_each_carrier_once_and_accepts_a_successor() {
        let carriers = vec![
            vec!["a".to_string()],
            vec!["b".to_string(), "b2".to_string()],
        ];
        assert_eq!(fraction_cited(&carriers, &["a".into(), "x".into()]), 0.5);
        assert_eq!(fraction_cited(&carriers, &["a".into(), "b2".into()]), 1.0);
        assert_eq!(
            fraction_cited(&[], &["a".into()]),
            1.0,
            "no carriers is nothing to miss"
        );
    }

    #[test]
    fn abstention_tallies_the_four_corners() {
        let t = Abstention::tally(&[
            (true, true),
            (true, false),
            (false, true),
            (false, false),
            (false, false),
        ]);
        assert_eq!(
            (
                t.should_and_did,
                t.should_and_did_not,
                t.should_not_did,
                t.should_not_did_not
            ),
            (1, 1, 1, 2)
        );
    }

    #[test]
    fn fully_supported_counts_answers_with_nothing_unsupported() {
        assert_eq!(fully_supported(&[0, 2, 0]), (2, 3));
        assert_eq!(fully_supported(&[]), (0, 0));
    }

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
