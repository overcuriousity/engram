//! Turning a ranked list into the excerpts one answer is built from.

/// How many leading hits sit above the relevance cliff; the whole list when there is none. The
/// token budget is applied later, over a list whose reached neighbours must never reach
/// `search::cliff`.
pub(super) fn above_cliff(cliff_at: Option<usize>, hits: usize) -> usize {
    match cliff_at {
        // A cliff at the first hit is the all-retired list: nothing to conclude from, so answer
        // from all of it.
        Some(0) => hits,
        Some(n) => n,
        None => hits,
    }
}

/// How many of the top hits get their neighbours pulled in.
pub(super) const NEIGHBOUR_ANCHORS: usize = 3;

/// Total neighbours admitted, however many links the anchors have between them.
pub(super) const NEIGHBOUR_MAX: usize = 6;

/// Above the cliff, capped at `NEIGHBOUR_ANCHORS`; with no cliff, or `Some(0)`, the top three.
/// Takes the raw `Option` because no cliff and a cliff at the end want opposite treatment.
pub(super) fn anchor_count(cliff_at: Option<usize>, hits: usize) -> usize {
    cliff_at
        .filter(|n| *n > 0)
        .unwrap_or(hits)
        .min(NEIGHBOUR_ANCHORS)
        .min(hits)
}

/// Ranked hits first, then neighbours: a neighbour has no comparable score, and goes first when the
/// budget cuts.
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

/// Every round's ranked hits round-robin, then everything only reached; an uncut round interleaves
/// only [`UNCUT_PREFIX`]. Deduped by `artifact_id`: the earliest ranking wins the position, the
/// earliest touch decides `via`.
pub(super) struct Merged {
    pub hits: Vec<crate::core::search::SearchResult>,
    /// How many leading hits are ranked. Everything from here on was reached.
    pub ranked: usize,
}

/// Without a reranker most rounds are uncut; a few off the top keep planned rounds in without
/// outranking the question.
pub(super) const UNCUT_PREFIX: usize = 3;

/// One round's hits, split by the round's own count: `via` is set on a hit any round reached.
pub(super) struct Part {
    pub ranked: Vec<crate::core::search::SearchResult>,
    pub reached: Vec<crate::core::search::SearchResult>,
    /// False when the ranking was cut nowhere; `merge` then interleaves only [`UNCUT_PREFIX`].
    cut: bool,
}

impl Part {
    /// Takes `cliff_at` so the caller cannot invert the question; `Some(0)` is the all-retired
    /// list, not a cut.
    pub fn of(
        mut hits: Vec<crate::core::search::SearchResult>,
        ranked: usize,
        cliff_at: Option<usize>,
    ) -> Self {
        let reached = hits.split_off(ranked.min(hits.len()));
        Part {
            ranked: hits,
            reached,
            cut: cliff_at.is_some_and(|n| n > 0),
        }
    }
}

fn interleave(
    lists: Vec<Vec<crate::core::search::SearchResult>>,
) -> Vec<crate::core::search::SearchResult> {
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    let mut cursors: Vec<_> = lists.into_iter().map(Vec::into_iter).collect();
    let mut out = Vec::new();
    for _ in 0..longest {
        for c in cursors.iter_mut() {
            if let Some(h) = c.next() {
                out.push(h);
            }
        }
    }
    out
}

pub(super) fn merge(parts: Vec<Part>) -> Merged {
    use crate::core::search::SearchResult;
    type Story = Option<(Option<String>, Option<String>)>;

    // `None`: some round ranked it before any reached it, so it must not claim `via`.
    let mut origin: std::collections::HashMap<String, Story> = std::collections::HashMap::new();
    for p in &parts {
        for h in &p.ranked {
            origin.entry(h.artifact_id.clone()).or_insert(None);
        }
        for h in &p.reached {
            origin
                .entry(h.artifact_id.clone())
                .or_insert_with(|| Some((h.via.clone(), h.reason.clone())));
        }
    }

    let mut ranked_lists = Vec::with_capacity(parts.len());
    let mut tails: Vec<SearchResult> = Vec::new();
    let mut reached_lists = Vec::with_capacity(parts.len());
    for p in parts {
        let mut vouched = p.ranked;
        if !p.cut {
            tails.extend(vouched.split_off(UNCUT_PREFIX.min(vouched.len())));
        }
        ranked_lists.push(vouched);
        reached_lists.push(p.reached);
    }

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let take = |h: &mut SearchResult, seen: &mut std::collections::HashSet<String>| -> bool {
        if !seen.insert(h.artifact_id.clone()) {
            return false;
        }
        match origin.get(&h.artifact_id) {
            Some(Some((via, reason))) => {
                h.via = via.clone();
                h.reason = reason.clone();
            }
            _ => {
                h.via = None;
                h.reason = None;
            }
        }
        true
    };

    let mut out: Vec<SearchResult> = Vec::new();
    for mut h in interleave(ranked_lists) {
        if take(&mut h, &mut seen) {
            out.push(h);
        }
    }
    // Still ranked, so inside `ranked` and counted as shown; they only pack last.
    for mut h in tails {
        if take(&mut h, &mut seen) {
            out.push(h);
        }
    }
    let ranked = out.len();
    for mut h in interleave(reached_lists) {
        if take(&mut h, &mut seen) {
            out.push(h);
        }
    }
    Merged { hits: out, ranked }
}

/// Retrieved artifacts that never reached the model, by identity across rounds.
/// Only the ranked part counts as shown: a cut hit reached back by adjacency is still lost.
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

    fn hit(id: &str, via: Option<&str>) -> crate::core::search::SearchResult {
        crate::core::search::SearchResult {
            artifact_id: id.into(),
            corpus_id: "c".into(),
            via: via.map(str::to_string),
            ..Default::default()
        }
    }

    fn part(ranked: &[&str], reached: &[(&str, &str)]) -> Part {
        let n = ranked.len();
        Part::of(hits_of(ranked, reached), n, Some(n))
    }

    fn part_uncut(ranked: &[&str], reached: &[(&str, &str)]) -> Part {
        let n = ranked.len();
        Part::of(hits_of(ranked, reached), n, None)
    }

    fn hits_of(
        ranked: &[&str],
        reached: &[(&str, &str)],
    ) -> Vec<crate::core::search::SearchResult> {
        ranked
            .iter()
            .map(|id| hit(id, None))
            .chain(reached.iter().map(|(id, from)| hit(id, Some(from))))
            .collect()
    }

    fn ids(m: &Merged) -> Vec<&str> {
        m.hits.iter().map(|h| h.artifact_id.as_str()).collect()
    }

    #[test]
    fn every_ranked_hit_of_every_round_packs_ahead_of_every_reached_one() {
        let merged = merge(vec![
            part(&["r1"], &[("n1", "r1")]),
            part(&["r2"], &[("n2", "r2")]),
        ]);
        assert_eq!(ids(&merged), vec!["r1", "r2", "n1", "n2"]);
        assert_eq!(merged.ranked, 2, "the seam is where the reaching starts");

        let last_ranked = merged.hits.iter().rposition(|h| h.via.is_none()).unwrap();
        let first_reached = merged.hits.iter().position(|h| h.via.is_some()).unwrap();
        assert!(
            last_ranked < first_reached,
            "a neighbour packs ahead of a hit, so the budget drops the wrong one first"
        );
    }

    #[test]
    fn the_rounds_interleave_so_a_tight_window_still_covers_every_subject() {
        let merged = merge(vec![
            part(&["a1", "a2", "a3"], &[]),
            part(&["b1", "b2"], &[]),
            part(&["c1"], &[]),
        ]);
        assert_eq!(ids(&merged), vec!["a1", "b1", "c1", "a2", "b2", "a3"]);
        assert_eq!(
            &ids(&merged)[..3],
            &["a1", "b1", "c1"],
            "a window holding three excerpts must hold one per subject"
        );
    }

    #[test]
    fn the_question_as_asked_still_takes_the_first_place() {
        let merged = merge(vec![part(&["asked"], &[]), part(&["planned"], &[])]);
        assert_eq!(ids(&merged)[0], "asked");
    }

    #[test]
    fn an_artifact_a_later_round_ranked_takes_a_ranked_place_and_keeps_how_it_was_reached() {
        let merged = merge(vec![
            part(&["a"], &[("b", "a")]),
            // The second round ranks what the first only reached, and re-finds what it already had.
            part(&["b", "a", "c"], &[]),
        ]);
        assert_eq!(ids(&merged), vec!["a", "b", "c"]);
        assert_eq!(
            merged.ranked, 3,
            "the reached hit is inside the ranked half"
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

    #[test]
    fn an_artifact_ranked_before_it_was_ever_reached_carries_no_via() {
        let merged = merge(vec![part(&["a", "x"], &[]), part(&["y"], &[("a", "y")])]);
        assert_eq!(ids(&merged), vec!["a", "y", "x"]);
        assert_eq!(
            merged.hits[0].via, None,
            "a retrieved artifact was reported as reached sideways"
        );
    }

    #[test]
    fn an_artifact_the_cliff_cut_stays_dropped_even_when_the_reach_puts_it_back() {
        let merged = merge(vec![part(&["a"], &[("b", "a")])]);
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

    #[test]
    fn the_fan_out_can_show_fewer_excerpts_than_the_first_round_did() {
        let first = part(&["r1"], &[("n1", "r1"), ("n2", "r1")]);
        let block = |h: &crate::core::search::SearchResult| match h.artifact_id.as_str() {
            "big" => "x".repeat(60),
            _ => "x".repeat(20),
        };
        let budget = 25;

        let one: Vec<String> = first
            .ranked
            .iter()
            .chain(first.reached.iter())
            .map(block)
            .collect();
        let kept_one = pack_by_budget(&one, &TokenCounter::default(), budget);

        let merged = merge(vec![first, part(&["big"], &[])]);
        let two: Vec<String> = merged.hits.iter().map(block).collect();
        let kept_two = pack_by_budget(&two, &TokenCounter::default(), budget);

        assert_eq!(merged.hits[1].artifact_id, "big", "the hit packs second");
        assert!(
            kept_two < kept_one,
            "nothing was displaced: {kept_one} then {kept_two}"
        );
    }

    #[test]
    fn an_uncut_round_interleaves_only_what_its_ranking_can_vouch_for() {
        let merged = merge(vec![
            part_uncut(&["a1", "a2", "a3", "a4", "a5"], &[]),
            part_uncut(&["b1", "b2", "b3", "b4"], &[]),
        ]);
        assert_eq!(
            ids(&merged),
            vec!["a1", "b1", "a2", "b2", "a3", "b3", "a4", "a5", "b4"],
            "an uncut tail outranked another round's vouched-for hits"
        );
        assert_eq!(
            merged.ranked, 9,
            "a tail is still a ranked hit; it only packs last among them"
        );
    }

    #[test]
    fn a_round_that_found_its_cliff_interleaves_whole() {
        let merged = merge(vec![
            part(&["a1", "a2", "a3", "a4"], &[]),
            part(&["b1", "b2", "b3", "b4"], &[]),
        ]);
        assert_eq!(
            ids(&merged),
            vec!["a1", "b1", "a2", "b2", "a3", "b3", "a4", "b4"]
        );
    }

    #[test]
    fn an_uncut_tail_still_packs_ahead_of_everything_reached() {
        let merged = merge(vec![
            part_uncut(&["a1", "a2", "a3", "a4"], &[("n1", "a1")]),
            part_uncut(&["b1"], &[]),
        ]);
        assert_eq!(ids(&merged), vec!["a1", "b1", "a2", "a3", "a4", "n1"]);
        assert_eq!(merged.ranked, 5, "the reaching starts after every tail");
    }

    #[test]
    fn a_list_with_a_cliff_packs_to_it() {
        assert_eq!(cut(&[0.9, 0.88, 0.86, 0.20, 0.19]), 3);
    }

    #[test]
    fn a_cliff_at_the_first_hit_cuts_nothing() {
        assert_eq!(above_cliff(Some(0), 5), 5);
        assert_eq!(above_cliff(Some(0), 0), 0);
        assert_eq!(above_cliff(Some(3), 5), 3);
        assert_eq!(above_cliff(None, 5), 5);
    }

    fn cut(scores: &[f32]) -> usize {
        above_cliff(crate::core::search::cliff(scores), scores.len())
    }

    #[test]
    fn a_list_without_a_cliff_is_kept_whole() {
        assert_eq!(cut(&[0.9, 0.88, 0.86, 0.84, 0.82]), 5);
    }

    #[test]
    fn two_hits_are_too_few_for_a_cliff_and_are_kept_whole() {
        assert_eq!(cut(&[0.9, 0.1]), 2);
    }

    #[test]
    fn the_budget_still_wins_when_the_cliff_would_overrun_the_window() {
        let above = cut(&[0.9, 0.88, 0.86, 0.20, 0.19]);
        let blocks: Vec<String> = (0..above).map(|_| "x".repeat(35)).collect();
        let kept = pack_by_budget(&blocks, &TokenCounter::default(), 25);
        assert!(kept < above, "the window must cut below the cliff: {kept}");
    }

    #[test]
    fn neighbours_are_appended_after_the_ranked_hits() {
        let ranked = vec!["a".to_string(), "b".to_string()];
        let neighbours = vec!["n1".to_string(), "n2".to_string()];
        let merged = append_neighbours(ranked, neighbours, NEIGHBOUR_MAX);
        assert_eq!(merged, vec!["a", "b", "n1", "n2"]);
    }

    #[test]
    fn the_neighbour_cap_holds() {
        let ranked = vec!["a".to_string()];
        let neighbours: Vec<String> = (0..20).map(|i| format!("n{i}")).collect();
        let merged = append_neighbours(ranked, neighbours, NEIGHBOUR_MAX);
        assert_eq!(merged.len(), 1 + NEIGHBOUR_MAX);
    }

    #[test]
    fn a_neighbour_that_is_already_a_hit_is_dropped() {
        let ranked = vec!["a".to_string(), "b".to_string()];
        let neighbours = vec!["b".to_string(), "c".to_string()];
        let merged = append_neighbours(ranked, neighbours, NEIGHBOUR_MAX);
        assert_eq!(merged, vec!["a", "b", "c"]);
    }

    #[test]
    fn a_repeated_neighbour_does_not_consume_the_cap() {
        let ranked = vec!["a".to_string()];
        let mut neighbours: Vec<String> = vec!["n0".to_string(); 20];
        neighbours.push("n1".to_string());
        let merged = append_neighbours(ranked, neighbours, NEIGHBOUR_MAX);
        assert_eq!(merged, vec!["a", "n0", "n1"]);
    }

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

    #[test]
    fn a_cliff_at_the_first_hit_anchors_like_no_cliff_at_all() {
        assert_eq!(anchor_count(Some(0), 10), 3);
        assert_eq!(anchor_count(Some(0), 2), 2);
    }

    #[test]
    fn a_round_cut_at_its_first_hit_interleaves_like_an_uncut_one() {
        let all_retired = Part::of(hits_of(&["a0", "a1", "a2", "a3"], &[]), 4, Some(0));
        let other = Part::of(hits_of(&["b0", "b1", "b2", "b3"], &[]), 4, Some(4));
        let merged = merge(vec![all_retired, other]);
        let ids: Vec<&str> = merged.hits.iter().map(|h| h.artifact_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["a0", "b0", "a1", "b1", "a2", "b2", "b3", "a3"],
            "the uncut round's tail follows every hit a ranking vouched for"
        );
    }
}
