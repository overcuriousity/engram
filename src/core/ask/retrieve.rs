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
/// then every artifact that was only ever reached sideways. `ranked` is where
/// the second half starts.
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
/// Deduped by `artifact_id`: an artifact in front of the model twice is a
/// wasted excerpt, not a stronger one. Where the two rounds disagree about what
/// an artifact *is*, round two's ranking wins the position and round one keeps
/// the story — an artifact round one only reached and round two then ranked
/// takes its ranked place, carrying the `via` that says how it was first found.
/// Leaving it in the tail because that is where it entered would demote a hit
/// the model explicitly asked for below neighbours of something else, and would
/// have `dropped` report it missing while it sat in the prompt.
pub(super) struct Merged {
    pub hits: Vec<crate::core::search::SearchResult>,
    /// How many leading hits are ranked. Everything from here on was reached.
    pub ranked: usize,
}

pub(super) fn merge_rounds(
    first: Vec<crate::core::search::SearchResult>,
    second: Vec<crate::core::search::SearchResult>,
) -> Merged {
    use crate::core::search::SearchResult;
    let is_ranked = |h: &SearchResult| h.via.is_none();

    // How round one found each artifact it reached, so a promoted hit can keep
    // saying so on the rail.
    let reached: std::collections::HashMap<&str, (&Option<String>, &Option<String>)> = first
        .iter()
        .filter(|h| !is_ranked(h))
        .map(|h| (h.artifact_id.as_str(), (&h.via, &h.reason)))
        .collect();
    let ranked_already: std::collections::HashSet<&str> = first
        .iter()
        .filter(|h| is_ranked(h))
        .map(|h| h.artifact_id.as_str())
        .collect();

    let mut promoted: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut second_ranked: Vec<SearchResult> = Vec::new();
    let mut second_reached: Vec<SearchResult> = Vec::new();
    for h in &second {
        let id = h.artifact_id.as_str();
        if ranked_already.contains(id) {
            continue;
        }
        match (is_ranked(h), reached.get(id)) {
            (true, Some((via, reason))) => {
                promoted.insert(id);
                let mut h = h.clone();
                h.via = (*via).clone();
                h.reason = (*reason).clone();
                second_ranked.push(h);
            }
            (true, None) => second_ranked.push(h.clone()),
            // Reached in both rounds, or reached now and never ranked: round
            // one's copy already stands for it.
            (false, Some(_)) => {}
            (false, None) => second_reached.push(h.clone()),
        }
    }

    let mut out: Vec<SearchResult> = first.iter().filter(|h| is_ranked(h)).cloned().collect();
    out.extend(second_ranked);
    let ranked = out.len();
    out.extend(
        first
            .into_iter()
            .filter(|h| !is_ranked(h) && !promoted.contains(h.artifact_id.as_str())),
    );
    out.extend(second_reached);
    Merged { hits: out, ranked }
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
/// Only the ranked part of the list counts as showing a retrieved artifact.
/// `dropped` answers "what did I ask for and not get shown", where the asking
/// is the ranking: an artifact the cliff cut and adjacency then reached back is
/// still a hit the ranking lost, and reading it as retained would hide the
/// cliff on exactly the lists where it did the most work. Position rather than
/// `via` is what tells the two apart, because a promoted hit carries a `via`
/// and is a ranked hit all the same — round two returned it above its own
/// cliff, which is the opposite of lost.
pub(super) fn dropped_count(
    retrieved: &[String],
    shown: &[crate::core::search::SearchResult],
    ranked: usize,
) -> usize {
    let shown: std::collections::HashSet<&str> = shown
        .iter()
        .take(ranked)
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
        let ids: Vec<&str> = merged.hits.iter().map(|h| h.artifact_id.as_str()).collect();
        assert_eq!(ids, vec!["r1", "r2", "n1", "n2"]);
        assert_eq!(merged.ranked, 2, "the seam is where the reaching starts");

        let last_ranked = merged.hits.iter().rposition(|h| h.via.is_none()).unwrap();
        let first_reached = merged.hits.iter().position(|h| h.via.is_some()).unwrap();
        assert!(
            last_ranked < first_reached,
            "a neighbour packs ahead of a hit, so the budget drops the wrong one first"
        );
    }

    /// An artifact round one only reached and round two then ranked is a hit,
    /// and packs like one. Leaving it in the tail because that is where it
    /// entered demotes it below neighbours of something else — the R32
    /// inversion, surviving inside the dedup — and has `dropped` report it
    /// missing while it sits in the prompt.
    ///
    /// It keeps its `via` all the same: that string is how the rail explains
    /// where it came from, and round two ranking it does not unsay it.
    #[test]
    fn an_artifact_round_two_ranked_takes_a_ranked_place_and_keeps_how_it_was_reached() {
        let first = vec![hit("a", None), hit("b", Some("a"))];
        // The second round ranks what the first only reached, and re-finds what
        // it already had.
        let second = vec![hit("b", None), hit("a", None), hit("c", None)];

        let merged = merge_rounds(first, second);
        let ids: Vec<&str> = merged.hits.iter().map(|h| h.artifact_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        assert_eq!(
            merged.ranked, 3,
            "the promoted hit is inside the ranked half"
        );
        assert_eq!(
            merged.hits[1].via.as_deref(),
            Some("a"),
            "the rail lost the anchor this artifact was first reached from"
        );
        assert_eq!(
            dropped_count(
                &["a".to_string(), "b".to_string(), "c".to_string()],
                &merged.hits,
                merged.ranked
            ),
            0,
            "an artifact in the prompt was counted as dropped"
        );
    }

    /// `dropped` is measured by position, not by `via`, and the two disagree
    /// exactly here: an artifact the cliff cut and adjacency reached back is a
    /// hit the ranking lost, however it reads on the rail.
    #[test]
    fn an_artifact_the_cliff_cut_stays_dropped_even_when_the_reach_puts_it_back() {
        let first = vec![hit("a", None), hit("b", Some("a"))];
        let merged = merge_rounds(first, vec![]);
        assert_eq!(
            dropped_count(
                &["a".to_string(), "b".to_string()],
                &merged.hits,
                merged.ranked
            ),
            1,
            "the cliff stopped being visible in `dropped`"
        );
    }

    /// The second round can leave the model with *fewer* excerpts than the
    /// first, and that is the R32 priority working rather than a regression: a
    /// hit round two asked for packs ahead of round one's neighbours, so a
    /// window that held three small speculative excerpts may hold one hit
    /// instead. `Retrieved { round: 2, shown }` can therefore be lower than
    /// round one's, which is honest — those neighbours are no longer in the
    /// prompt.
    #[test]
    fn a_second_round_can_show_fewer_excerpts_than_the_first_did() {
        let first = vec![
            hit("r1", None),
            hit("n1", Some("r1")),
            hit("n2", Some("r1")),
        ];
        let second = vec![hit("big", None)];

        let block = |h: &crate::core::search::SearchResult| match h.artifact_id.as_str() {
            "big" => "x".repeat(60),
            _ => "x".repeat(20),
        };
        let budget = 25;

        let one: Vec<String> = first.iter().map(block).collect();
        let kept_one = pack_by_budget(&one, &TokenCounter, budget);

        let merged = merge_rounds(first, second);
        let two: Vec<String> = merged.hits.iter().map(block).collect();
        let kept_two = pack_by_budget(&two, &TokenCounter, budget);

        assert_eq!(merged.hits[1].artifact_id, "big", "the hit packs second");
        assert!(
            kept_two < kept_one,
            "nothing was displaced: {kept_one} then {kept_two}"
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
