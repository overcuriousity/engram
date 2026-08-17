//! Turning a ranked list into the excerpts one answer is built from.

/// How many leading hits sit above the point where the ranked list's relevance
/// falls off.
///
/// `pack_by_budget` alone bounds cost, not relevance — it will hand the model
/// eight excerpts when the fourth was already noise, and noise makes the answer
/// worse as well as dearer. This is where the list stops meaning anything, and
/// it is the same computation the results rail draws.
///
/// The budget is deliberately NOT applied here. It is applied later, over the
/// candidate list that also carries reached neighbours, because those are
/// appended after the ranked hits and must never enter the scores handed to
/// `search::cliff`.
///
/// No cliff — fewer than three hits, or no single step standing out — returns
/// the whole list. A list with no cliff is a list with nothing to conclude
/// from, and inventing a cut there would be worse than the greedy pack.
pub(super) fn above_cliff(scores: &[f32]) -> usize {
    crate::core::search::cliff(scores).unwrap_or(scores.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::budget::{TokenCounter, pack_by_budget};

    /// Ten-token blocks and a budget that fits all of them, so only the cliff
    /// can do any cutting here.
    fn blocks(n: usize) -> Vec<String> {
        (0..n).map(|_| "x".repeat(35)).collect()
    }

    /// The whole point: a list whose relevance falls off is cut where it falls
    /// off, not where the context window runs out.
    #[test]
    fn a_list_with_a_cliff_packs_to_it() {
        assert_eq!(above_cliff(&[0.9, 0.88, 0.86, 0.20, 0.19]), 3);
    }

    /// No cliff means no basis for concluding anything about the tail, so the
    /// behaviour is exactly what it was before this function existed.
    #[test]
    fn a_list_without_a_cliff_packs_everything_the_budget_allows() {
        assert_eq!(above_cliff(&[0.9, 0.88, 0.86, 0.84, 0.82]), 5);
    }

    /// Fewer than three hits: `cliff` returns None by construction and the
    /// budget is the only bound.
    #[test]
    fn two_hits_are_packed_without_a_cliff() {
        assert_eq!(above_cliff(&[0.9, 0.1]), 2);
    }

    /// The cliff decides what is worth showing; the window decides what fits,
    /// and the window still wins. An excerpt that does not fit cannot be sent
    /// whatever its relevance. Run in the order `ask` runs it: cut to the
    /// cliff, then pack what is left.
    #[test]
    fn the_budget_still_wins_when_the_cliff_would_overrun_the_window() {
        let above = above_cliff(&[0.9, 0.88, 0.86, 0.20, 0.19]);
        let blocks = blocks(above);
        let kept = pack_by_budget(&blocks, &TokenCounter, 25);
        assert!(kept < above, "the window must cut below the cliff: {kept}");
    }
}
