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
///
/// Takes the `Option` `search::cliff` already produced for the same list — the
/// caller reports that position on the wire and reaches neighbours from it —
/// rather than computing the cliff a second time.
pub(super) fn above_cliff(cliff_at: Option<usize>, hits: usize) -> usize {
    match cliff_at {
        // A cliff at the very first hit says nothing is above it, which is not
        // a cut — it is the same "nothing to conclude from" the `None` arm
        // already hands the whole list back for. `search` produces it in one
        // case: every hit is retired, and the retired tail it marks is the
        // whole list. Retiring is ordinary lifecycle — completing a reminder
        // retires the note it was read from — so those rows can be the only
        // record the base has of an answer, and truncating here left `ask`
        // writing from no excerpts at all while the search page beside it
        // listed them. They are answered from, and `AskResponse::retired_only`
        // is what says so.
        Some(0) => hits,
        Some(n) => n,
        None => hits,
    }
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
///
/// `Some(0)` is read the way [`above_cliff`] reads it: not a cut. It is the
/// all-retired list, which is "nothing to conclude from" and not "nothing is
/// reliable" — and taking it literally gave zero anchors, so `reach_sideways`
/// returned at once and the answer got no neighbours in precisely the case
/// `retired_only` exists for: a note whose completed reminder retired it.
pub(super) fn anchor_count(cliff_at: Option<usize>, hits: usize) -> usize {
    cliff_at
        .filter(|n| *n > 0)
        .unwrap_or(hits)
        .min(NEIGHBOUR_ANCHORS)
        .min(hits)
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

/// Every round's hits as one candidate list: the ranked ones interleaved across
/// rounds, then everything that was only ever reached sideways. `ranked` is
/// where the second half starts.
///
/// The ordering is the packing priority, and two properties are held in it.
///
/// Reached last, always — the same safety property `append_neighbours` states,
/// held across every seam between rounds. A neighbour is the most speculative
/// excerpt in the prompt, and putting it behind every ranked hit is what makes
/// it the first thing the budget drops. Merging the rounds as whole lists —
/// round one's neighbours ahead of round two's ranked hits — inverts that on
/// precisely the windows where it matters: a tight one would drop an artifact
/// a round explicitly asked for and keep a speculative neighbour of something
/// else.
///
/// Ranked round-robin, not concatenated. A question that named three subjects
/// is answered from all three or from none, and concatenating would let the
/// first subject's hits fill the window while the other two go unrepresented —
/// which is the failure the fan-out exists to fix. The first round is the
/// question asked verbatim and still takes the first slot, because round-robin
/// starts with it.
///
/// Only what a round's own ranking vouched for is interleaved. A round that
/// found a cliff was cut at it and every hit it still carries is one that
/// ranking stood behind, so it interleaves whole. A round that found none was
/// cut nowhere — `above_cliff` hands back the entire ranked list — and with no
/// reranker wired that is the ordinary case rather than the exotic one.
/// Interleaving such a list whole would put a planned query's eighth-best hit
/// ahead of the asked question's second-best, and a tight window would then be
/// spent on the model's guesses while the question's own hits were evicted.
/// So an uncut round interleaves its leading [`UNCUT_PREFIX`] and its tail
/// follows the interleaved section in round order — still ranked, still ahead
/// of everything reached, but behind every hit that got there on a ranking's
/// word.
///
/// This cannot reach the cliff. Every round computed its own over its own
/// scores before returning, and nothing downstream of here recomputes one, so
/// the seam is a packing-priority decision and nothing more.
///
/// Deduped by `artifact_id`: an artifact in front of the model twice is a
/// wasted excerpt, not a stronger one. Where the rounds disagree about what an
/// artifact *is*, the earliest round to rank it wins the position and the
/// earliest round to touch it at all decides what it carries — an artifact one
/// round only reached and another then ranked takes its ranked place while
/// keeping the `via` that says how it was first found. Leaving it in the tail
/// because that is where it entered would demote a hit a round explicitly asked
/// for below neighbours of something else, and would have `dropped` report it
/// missing while it sat in the prompt. Giving it a `via` when the first round to
/// touch it ranked it outright would be the same error inverted: claiming an
/// artifact was reached sideways when it was retrieved.
pub(super) struct Merged {
    pub hits: Vec<crate::core::search::SearchResult>,
    /// How many leading hits are ranked. Everything from here on was reached.
    pub ranked: usize,
}

/// How many leading hits of a round that found no cliff are trusted enough to
/// interleave.
///
/// The same judgement `anchor_count` makes, for the same reason: "no cliff"
/// means no basis for calling any part of the list the reliable part, and the
/// answer is neither "all of it" nor "none of it". None of it would leave a
/// planned round unable to reach the prompt at all on any deployment without a
/// reranker — the fan-out reduced to a no-op on the configuration that ships.
/// All of it would let a round's tail outrank another round's second-best.
/// A few off the top is what a ranking is worth when nothing confirms where it
/// stops being worth anything.
pub(super) const UNCUT_PREFIX: usize = 3;

/// One round's hits, already split where its own `ranked` said to split them.
///
/// The split is the round's own count and never a property read off a hit: a
/// hit carries a `via` when *some* round reached it, and that stays true of the
/// round that then ranked it. Reading `via.is_none()` to decide would file such
/// a hit under "reached" in the very round that retrieved it.
pub(super) struct Part {
    pub ranked: Vec<crate::core::search::SearchResult>,
    pub reached: Vec<crate::core::search::SearchResult>,
    /// Whether this round's ranking found its own cliff. False says the list
    /// was cut nowhere, and `merge` interleaves only its leading
    /// [`UNCUT_PREFIX`] on that account.
    cut: bool,
}

impl Part {
    /// Split a round's hits at the count it reported.
    ///
    /// Takes the round's `cliff_at` rather than a bare flag so the caller
    /// cannot get the question backwards: it is the same `Option` the round
    /// already reported on the wire, and `Some(n > 0)` is exactly "this
    /// ranking said where it stopped meaning anything".
    ///
    /// `Some(0)` is not that, and [`above_cliff`] already says so: it is the
    /// all-retired list, handed back whole because there is nothing to
    /// conclude from. Reading it as a cut here made `merge` treat such a round
    /// as fully vouched-for and interleave its entire retired tail ahead of
    /// another round's fourth-and-later live hits — the one thing
    /// [`UNCUT_PREFIX`] exists to prevent.
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

/// Round-robin across the lists: every list's first before any list's second.
///
/// Empty lists simply contribute nothing, so a round that retrieved less than
/// the others does not leave gaps in the order.
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

    // How each artifact first entered the answer, in round order. `None` says
    // some round ranked it before any round reached it, and a hit that was
    // retrieved must not go on to claim it was reached sideways.
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

    // Each round's ranked hits split into the part its own ranking vouched for
    // and the part nothing did. A round that found a cliff vouched for all of
    // what it still carries; one that found none vouched for its leading few.
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
            // Retrieved before it was ever reached: it carries no `via`, even
            // if the copy that won this position came from a round that
            // reached it.
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
    // The uncut rounds' tails, in round order. Ranked hits all, so they sit
    // inside `ranked` and count as shown — they simply pack last among the
    // hits, which is the whole of what "nothing vouched for this" buys them.
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

/// How many of the artifacts a round actually retrieved never reached the
/// model.
///
/// Counted by identity rather than by arithmetic over list lengths, because
/// there are several rounds and one merged list: `retrieved` spans all of them
/// while the prefix that survived packing spans none of them cleanly, so "the
/// ranked ones that survived are exactly `kept.min(ranked)`" stopped being true
/// the moment a second round existed. Identity is true either way.
///
/// Only the ranked part of the list counts as showing a retrieved artifact.
/// `dropped` answers "what did I ask for and not get shown", where the asking
/// is the ranking: an artifact the cliff cut and adjacency then reached back is
/// still a hit the ranking lost, and reading it as retained would hide the
/// cliff on exactly the lists where it did the most work. Position rather than
/// `via` is what tells the two apart, because a hit one round reached and
/// another ranked carries a `via` and is a ranked hit all the same — that
/// round returned it above its own cliff, which is the opposite of lost.
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
            due_at: None,
            due_in: None,
            in_sitting: false,
            past_cliff: false,
            retired: false,
            similarity: None,
            titled_by_corpus: false,
            via: via.map(str::to_string),
            reason: None,
            explanation: None,
            model_written: false,
            synthesized: false,
            origin_count: 0,
        }
    }

    /// One round, as `Part::of` receives it: ranked hits first, then whatever
    /// was reached from them, split at the count the round reported.
    ///
    /// `cliff_at` is `Some` — this round's ranking found where it stopped
    /// meaning anything and was cut there, so every hit it carries is vouched
    /// for. `part_uncut` is the other case.
    fn part(ranked: &[&str], reached: &[(&str, &str)]) -> Part {
        let n = ranked.len();
        Part::of(hits_of(ranked, reached), n, Some(n))
    }

    /// A round whose ranking found no cliff, so nothing vouched for where its
    /// list stops being worth reading.
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

    /// The seam between the rounds obeys the same rule as the seam inside one:
    /// everything ranked before everything reached. Merging the rounds as whole
    /// lists would leave round one's neighbours ahead of round two's hits, and
    /// a tight window would then drop the artifact a round explicitly asked
    /// for while keeping a neighbour of something else — the priority
    /// `append_neighbours` exists to set, inverted at the one point nothing was
    /// checking.
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

    /// The reason the fan-out exists. A question that named three subjects gets
    /// a round for each, and a window that holds three excerpts has to spend
    /// them on three subjects rather than on the first subject's three best
    /// hits. Concatenating the rounds would do the latter and leave two thirds
    /// of the question unretrieved in the prompt.
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

    /// Round-robin does not cost the question its own best hit. The first part
    /// is the question as it was actually asked, and round-robin starts there.
    #[test]
    fn the_question_as_asked_still_takes_the_first_place() {
        let merged = merge(vec![part(&["asked"], &[]), part(&["planned"], &[])]);
        assert_eq!(ids(&merged)[0], "asked");
    }

    /// An artifact one round only reached and another then ranked is a hit, and
    /// packs like one. Leaving it in the tail because that is where it entered
    /// demotes it below neighbours of something else — the R32 inversion,
    /// surviving inside the dedup — and has `dropped` report it missing while it
    /// sits in the prompt.
    ///
    /// It keeps its `via` all the same: that string is how the rail explains
    /// where it came from, and a later round ranking it does not unsay it.
    #[test]
    fn an_artifact_a_later_round_ranked_takes_a_ranked_place_and_keeps_how_it_was_reached() {
        let merged = merge(vec![
            part(&["a"], &[("b", "a")]),
            // The second round ranks what the first only reached, and re-finds
            // what it already had.
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

    /// The inverse, and the reason `origin` is built in round order rather than
    /// from whichever copy wins a position: an artifact the first round
    /// retrieved outright was not reached sideways, and a later round happening
    /// to link to it must not have the rail claim it was. `via` is a statement
    /// about how an artifact first entered the answer.
    #[test]
    fn an_artifact_ranked_before_it_was_ever_reached_carries_no_via() {
        let merged = merge(vec![part(&["a", "x"], &[]), part(&["y"], &[("a", "y")])]);
        assert_eq!(ids(&merged), vec!["a", "y", "x"]);
        assert_eq!(
            merged.hits[0].via, None,
            "a retrieved artifact was reported as reached sideways"
        );
    }

    /// `dropped` is measured by position, not by `via`, and the two disagree
    /// exactly here: an artifact the cliff cut and adjacency reached back is a
    /// hit the ranking lost, however it reads on the rail.
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

    /// The fan-out can leave the model with *fewer* excerpts than the first
    /// round did, and that is the R32 priority working rather than a
    /// regression: a hit a planned round asked for packs ahead of round one's
    /// neighbours, so a window that held three small speculative excerpts may
    /// hold one hit instead. `Retrieved { round: 2, shown }` can therefore be
    /// lower than round one's, which is honest — those neighbours are no longer
    /// in the prompt.
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

    /// The no-reranker case, which is the shipped one. Nothing cut these lists,
    /// so `above_cliff` handed each round all eight of its hits. Interleaving
    /// them whole would put a planned query's fourth-best hit ahead of the
    /// asked question's fourth-best and let the model's guesses fill a window
    /// the question's own hits were evicted from. Only the leading
    /// `UNCUT_PREFIX` of each round interleaves; the tails follow, in round
    /// order, behind every hit some ranking vouched for.
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

    /// A round that found its cliff was cut at it, so everything it still
    /// carries is vouched for and interleaves whole — even past
    /// `UNCUT_PREFIX`. Holding a cut round to the same prefix would throw away
    /// the one thing a cliff is for.
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

    /// The seam the tails must not cross. A tail is untrusted among the ranked
    /// hits and packs behind all of them, but it is still a hit a round
    /// retrieved, and a neighbour of something else must not outrank it.
    #[test]
    fn an_uncut_tail_still_packs_ahead_of_everything_reached() {
        let merged = merge(vec![
            part_uncut(&["a1", "a2", "a3", "a4"], &[("n1", "a1")]),
            part_uncut(&["b1"], &[]),
        ]);
        assert_eq!(ids(&merged), vec!["a1", "b1", "a2", "a3", "a4", "n1"]);
        assert_eq!(merged.ranked, 5, "the reaching starts after every tail");
    }

    /// The whole point: a list whose relevance falls off is cut where it falls
    /// off, not where the context window runs out.
    #[test]
    fn a_list_with_a_cliff_packs_to_it() {
        assert_eq!(cut(&[0.9, 0.88, 0.86, 0.20, 0.19]), 3);
    }

    /// A cliff at the very first hit is not a cut.
    ///
    /// `search` produces it in one case: every hit is retired, so the retired
    /// tail it marks is the whole list. Taken literally, `ask` truncated to
    /// zero excerpts and wrote its answer from nothing while the search page
    /// beside it listed the rows. Retiring is ordinary lifecycle — completing a
    /// reminder retires the note it was read from — so those rows can be all
    /// the base has, and answering from them with `retired_only` set is the
    /// better of the two failures.
    #[test]
    fn a_cliff_at_the_first_hit_cuts_nothing() {
        assert_eq!(above_cliff(Some(0), 5), 5);
        assert_eq!(above_cliff(Some(0), 0), 0);
        // And every other position still means what it said.
        assert_eq!(above_cliff(Some(3), 5), 3);
        assert_eq!(above_cliff(None, 5), 5);
    }

    /// `above_cliff` as `ask` runs it: the cliff of this list, then the cut.
    fn cut(scores: &[f32]) -> usize {
        above_cliff(crate::core::search::cliff(scores), scores.len())
    }

    /// No cliff means no basis for concluding anything about the tail, so the
    /// behaviour is exactly what it was before this function existed.
    #[test]
    fn a_list_without_a_cliff_is_kept_whole() {
        assert_eq!(cut(&[0.9, 0.88, 0.86, 0.84, 0.82]), 5);
    }

    /// Fewer than three hits: `cliff` returns None by construction, so there
    /// is nothing here to cut and the budget is left as the only bound.
    #[test]
    fn two_hits_are_too_few_for_a_cliff_and_are_kept_whole() {
        assert_eq!(cut(&[0.9, 0.1]), 2);
    }

    /// The cliff decides what is worth showing; the window decides what fits,
    /// and the window still wins. An excerpt that does not fit cannot be sent
    /// whatever its relevance. Run in the order `ask` runs it: cut to the
    /// cliff, then pack what is left.
    #[test]
    fn the_budget_still_wins_when_the_cliff_would_overrun_the_window() {
        let above = cut(&[0.9, 0.88, 0.86, 0.20, 0.19]);
        // Ten-token blocks against a budget that holds two.
        let blocks: Vec<String> = (0..above).map(|_| "x".repeat(35)).collect();
        let kept = pack_by_budget(&blocks, &TokenCounter::default(), 25);
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

    /// `Some(0)` is the all-retired list, which `above_cliff` hands back whole
    /// because there is nothing to conclude from it. Reading it as a cut here
    /// gave no anchors at all, so the one case `retired_only` exists for — a
    /// note whose completed reminder retired it — was answered with no
    /// neighbour context, while the `None` arm, the same "nothing to conclude
    /// from", got three.
    #[test]
    fn a_cliff_at_the_first_hit_anchors_like_no_cliff_at_all() {
        assert_eq!(anchor_count(Some(0), 10), 3);
        assert_eq!(anchor_count(Some(0), 2), 2);
    }

    /// And the same `Some(0)`, one seam over: a round whose whole list is
    /// retired was not cut, so it interleaves its leading `UNCUT_PREFIX` and
    /// no more. Treating it as cut put its fourth-and-later hits ahead of
    /// another round's second-best.
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
