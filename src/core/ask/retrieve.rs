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

/// How many of the top hits get their neighbours pulled in.
pub(super) const NEIGHBOUR_ANCHORS: usize = 3;

/// Total neighbours admitted, however many links the anchors have between them.
/// Speculation is useful in small quantities and is noise in large ones.
pub(super) const NEIGHBOUR_MAX: usize = 6;

/// Which hits to reach sideways from.
///
/// Above the cliff, capped at `NEIGHBOUR_ANCHORS`. With no cliff the top three
/// outright: "no cliff" means there is no basis for calling any part of the
/// list the reliable part, and reaching from nothing would disable the feature
/// on exactly the lists that need help most.
///
/// Takes the raw `Option` from `search::cliff` rather than `above_cliff`'s
/// count, because the two cases that count conflates — no cliff at all, and a
/// cliff at the very end of the list — want opposite treatment here.
pub(super) fn anchor_count(cliff_at: Option<usize>, hits: usize) -> usize {
    cliff_at.unwrap_or(hits).min(NEIGHBOUR_ANCHORS).min(hits)
}

/// Ranked hits first, then neighbours, deduped, capped.
///
/// The ordering is the safety property, not a presentation choice. A neighbour
/// has no score comparable to a retrieved hit — it was reached, not retrieved —
/// so interleaving would corrupt the cliff that was just computed over those
/// scores. Appending also makes a neighbour the first thing the budget drops,
/// which is right: it is the most speculative excerpt in the prompt.
pub(super) fn append_neighbours(
    ranked: Vec<String>,
    neighbours: Vec<String>,
    cap: usize,
) -> Vec<String> {
    let mut out = ranked;
    let mut added = 0usize;
    for n in neighbours {
        if added == cap {
            break;
        }
        if !out.contains(&n) {
            out.push(n);
            added += 1;
        }
    }
    out
}

/// Two rounds of hits as one candidate list: every ranked hit, in round order,
/// then every artifact that was reached sideways.
///
/// The ordering is the same safety property `append_neighbours` states, held
/// across the seam between the rounds. A neighbour is the most speculative
/// excerpt in the prompt, and appending it last is what makes it the first
/// thing the budget drops. Merging the two rounds as whole lists — round one's
/// neighbours ahead of round two's ranked hits — inverts that on precisely the
/// windows where it matters: a tight one would drop an artifact the model
/// explicitly asked for and keep a speculative neighbour of something else.
///
/// This cannot reach the cliff. Both rounds computed theirs over their own
/// scores before returning, and nothing downstream of here recomputes one, so
/// the seam is a packing-priority decision and nothing more.
///
/// Deduped by `artifact_id` against everything round one holds, ranked and
/// reached alike: an artifact in front of the model twice is a wasted excerpt,
/// not a stronger one. Round one wins the duplicate, keeping the `via` that
/// says how it was found.
pub(super) fn merge_rounds(
    first: Vec<crate::core::search::SearchResult>,
    second: Vec<crate::core::search::SearchResult>,
) -> Vec<crate::core::search::SearchResult> {
    let seen: std::collections::HashSet<&str> =
        first.iter().map(|h| h.artifact_id.as_str()).collect();
    let fresh: Vec<crate::core::search::SearchResult> = second
        .into_iter()
        .filter(|h| !seen.contains(h.artifact_id.as_str()))
        .collect();

    let ranked = |h: &crate::core::search::SearchResult| h.via.is_none();
    let mut out: Vec<crate::core::search::SearchResult> =
        Vec::with_capacity(first.len() + fresh.len());
    out.extend(first.iter().filter(|h| ranked(h)).cloned());
    out.extend(fresh.iter().filter(|h| ranked(h)).cloned());
    out.extend(first.into_iter().filter(|h| !ranked(h)));
    out.extend(fresh.into_iter().filter(|h| !ranked(h)));
    out
}

/// How many of the artifacts a round actually retrieved never reached the
/// model.
///
/// Counted by identity rather than by arithmetic over list lengths, because
/// there are now two rounds and one merged list: `retrieved` spans both rounds
/// while the prefix that survived packing spans neither cleanly, so "the ranked
/// ones that survived are exactly `kept.min(ranked)`" stopped being true the
/// moment a second round existed. Identity is true either way.
///
/// Only ranked citations count as showing a retrieved artifact. `dropped`
/// answers "what did I ask for and not get shown", where the asking is the
/// ranking: an artifact the cliff cut and adjacency then reached back is still
/// a hit the ranking lost, and reading it as retained would hide the cliff on
/// exactly the lists where it did the most work. `via` is what tells the two
/// apart, and nobody asked for a neighbour in the first place — counting one
/// would make `dropped` grow every time the reach worked.
pub(super) fn dropped_count(
    retrieved: &[String],
    shown: &[crate::core::search::SearchResult],
) -> usize {
    let shown: std::collections::HashSet<&str> = shown
        .iter()
        .filter(|h| h.via.is_none())
        .map(|h| h.artifact_id.as_str())
        .collect();
    retrieved
        .iter()
        .filter(|id| !shown.contains(id.as_str()))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::budget::{TokenCounter, pack_by_budget};

    /// A hit, ranked or reached, with nothing in it but what the merge reads.
    fn hit(id: &str, via: Option<&str>) -> crate::core::search::SearchResult {
        crate::core::search::SearchResult {
            artifact_id: id.into(),
            corpus_id: "c".into(),
            title: None,
            text: String::new(),
            category: None,
            tags: vec![],
            score: 0.0,
            status: None,
            superseded_by: None,
            last_verified_at: None,
            weak: false,
            primed: false,
            past_cliff: false,
            via: via.map(str::to_string),
            reason: None,
        }
    }

    /// The seam between the rounds obeys the same rule as the seam inside one:
    /// everything ranked before everything reached. Merging the rounds as whole
    /// lists would leave round one's neighbours ahead of round two's hits, and
    /// a tight window would then drop the artifact the model explicitly asked
    /// for while keeping a neighbour of something else — the priority
    /// `append_neighbours` exists to set, inverted at the one point nothing was
    /// checking.
    #[test]
    fn every_ranked_hit_of_either_round_packs_ahead_of_every_reached_one() {
        let first = vec![hit("r1", None), hit("n1", Some("r1"))];
        let second = vec![hit("r2", None), hit("n2", Some("r2"))];

        let merged = merge_rounds(first, second);
        let ids: Vec<&str> = merged.iter().map(|h| h.artifact_id.as_str()).collect();
        assert_eq!(ids, vec!["r1", "r2", "n1", "n2"]);

        let last_ranked = merged.iter().rposition(|h| h.via.is_none()).unwrap();
        let first_reached = merged.iter().position(|h| h.via.is_some()).unwrap();
        assert!(
            last_ranked < first_reached,
            "a neighbour packs ahead of a hit, so the budget drops the wrong one first"
        );
    }

    /// Round one wins a duplicate, keeping how it was found. An artifact in
    /// front of the model twice is a wasted excerpt, not a stronger one — and
    /// the copy that survives must be the one whose `via` the rail already
    /// explained.
    #[test]
    fn an_artifact_both_rounds_found_appears_once_as_round_one_found_it() {
        let first = vec![hit("a", None), hit("b", Some("a"))];
        // The second round ranks what the first only reached, and re-finds
        // what it already had.
        let second = vec![hit("b", None), hit("a", None), hit("c", None)];

        let merged = merge_rounds(first, second);
        let ids: Vec<&str> = merged.iter().map(|h| h.artifact_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "c", "b"]);
        assert_eq!(
            merged[2].via.as_deref(),
            Some("a"),
            "the surviving copy lost the anchor the rail names"
        );
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
    fn a_list_without_a_cliff_is_kept_whole() {
        assert_eq!(above_cliff(&[0.9, 0.88, 0.86, 0.84, 0.82]), 5);
    }

    /// Fewer than three hits: `cliff` returns None by construction, so there
    /// is nothing here to cut and the budget is left as the only bound.
    #[test]
    fn two_hits_are_too_few_for_a_cliff_and_are_kept_whole() {
        assert_eq!(above_cliff(&[0.9, 0.1]), 2);
    }

    /// The cliff decides what is worth showing; the window decides what fits,
    /// and the window still wins. An excerpt that does not fit cannot be sent
    /// whatever its relevance. Run in the order `ask` runs it: cut to the
    /// cliff, then pack what is left.
    #[test]
    fn the_budget_still_wins_when_the_cliff_would_overrun_the_window() {
        let above = above_cliff(&[0.9, 0.88, 0.86, 0.20, 0.19]);
        // Ten-token blocks against a budget that holds two.
        let blocks: Vec<String> = (0..above).map(|_| "x".repeat(35)).collect();
        let kept = pack_by_budget(&blocks, &TokenCounter, 25);
        assert!(kept < above, "the window must cut below the cliff: {kept}");
    }

    /// Neighbours are reached, not retrieved, so they carry no comparable
    /// score. Letting one into the score list would corrupt the cliff
    /// computation that just ran — this asserts the ordering that prevents it.
    #[test]
    fn neighbours_are_appended_after_the_ranked_hits() {
        let ranked = vec!["a".to_string(), "b".to_string()];
        let neighbours = vec!["n1".to_string(), "n2".to_string()];
        let merged = append_neighbours(ranked, neighbours, NEIGHBOUR_MAX);
        assert_eq!(merged, vec!["a", "b", "n1", "n2"]);
    }

    /// A hit with many links must not flood the prompt with speculation.
    #[test]
    fn the_neighbour_cap_holds() {
        let ranked = vec!["a".to_string()];
        let neighbours: Vec<String> = (0..20).map(|i| format!("n{i}")).collect();
        let merged = append_neighbours(ranked, neighbours, NEIGHBOUR_MAX);
        assert_eq!(merged.len(), 1 + NEIGHBOUR_MAX);
    }

    /// An artifact already retrieved must not appear twice.
    #[test]
    fn a_neighbour_that_is_already_a_hit_is_dropped() {
        let ranked = vec!["a".to_string(), "b".to_string()];
        let neighbours = vec!["b".to_string(), "c".to_string()];
        let merged = append_neighbours(ranked, neighbours, NEIGHBOUR_MAX);
        assert_eq!(merged, vec!["a", "b", "c"]);
    }

    /// A duplicate must not spend a place from the cap either: two anchors
    /// that share a neighbour is the ordinary case, not the exotic one.
    #[test]
    fn a_repeated_neighbour_does_not_consume_the_cap() {
        let ranked = vec!["a".to_string()];
        let mut neighbours: Vec<String> = vec!["n0".to_string(); 20];
        neighbours.push("n1".to_string());
        let merged = append_neighbours(ranked, neighbours, NEIGHBOUR_MAX);
        assert_eq!(merged, vec!["a", "n0", "n1"]);
    }

    /// With no cliff there is no reliable part of the list to anchor on, so the
    /// top three are used outright rather than none.
    #[test]
    fn anchors_fall_back_to_the_top_three_when_there_is_no_cliff() {
        assert_eq!(anchor_count(None, 10), 3);
        assert_eq!(
            anchor_count(Some(2), 10),
            2,
            "never more anchors than the cliff allows"
        );
        assert_eq!(
            anchor_count(Some(9), 10),
            3,
            "never more than NEIGHBOUR_ANCHORS"
        );
        assert_eq!(
            anchor_count(None, 2),
            2,
            "never more anchors than there are hits"
        );
    }
}
