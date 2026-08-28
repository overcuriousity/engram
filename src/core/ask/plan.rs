//! The one question an ask is allowed to ask itself.

use super::Core;
use crate::infer::budget::pack_by_budget;
use crate::infer::prompt::{PLAN_SYSTEM, ask_prompt, parse_plan};

/// Which subjects this answer still lacks, asked once, as the queries to run
/// for them. Empty means "it has enough".
///
/// One call, one plan, and the rounds it names all run at once. That bound is
/// structural rather than a limit checked in a loop: there is a single call
/// site, it is not inside anything that repeats, and nothing downstream of the
/// fan-out asks again. "Let the model say once what is missing" is the bounded
/// version of a mechanism whose unbounded version is an agent, and an agent is
/// not what this is.
///
/// Empty the moment no planning model is wired: the switch has to mean no call
/// rather than a call whose result is discarded. There is nothing here to call
/// — `Core::planner` is `None` — so no later edit to the ask path can make the
/// disabled case cost anything.
///
/// Every failure is empty as well. A timeout, a refusal, a reply that was
/// prose: the ask carries on with what the first round retrieved. The operator
/// asked a question, not for a retrieval strategy, and an answer built from one
/// round is the thing that was already working.
pub(super) async fn needed_queries(
    core: &Core,
    question: &str,
    excerpts: &[String],
) -> Vec<String> {
    let Some(model) = core.planner.as_ref() else {
        return Vec::new();
    };

    // Packed against the planning model's window, not the answer model's. The
    // whole point of `plan_tier` is that this call runs on the efficient
    // endpoint while the answer runs on the deep one, and the efficient one is
    // usually the smaller window — handing it everything the deep model can
    // hold is a call refused for its size on exactly the questions that
    // retrieved enough to be worth fanning out from.
    let context = model.context_tokens();
    let budget = super::excerpt_budget(&**model, &core.counter, PLAN_SYSTEM, question);
    let kept = pack_by_budget(excerpts, &core.counter, budget);
    if kept == 0 {
        // Asking which subjects are missing without showing any of the ones
        // that were found invites a query for whatever the question happens to
        // name, which is the search that just ran.
        tracing::debug!("ask: no excerpt fits the planning window; answering from one round");
        return Vec::new();
    }

    // The same prompt shape the answer call uses: the model is shown the same
    // material and asked one thing about it, not a differently framed task.
    let user = ask_prompt(question, &excerpts[..kept]);
    let spent = core.counter.count(PLAN_SYSTEM) + core.counter.count(&user);
    let ceiling =
        crate::infer::budget::ceiling_for_prompt(context, spent, model.max_output_tokens());

    let reply = match model.answer(PLAN_SYSTEM, &user, ceiling).await {
        Ok(reply) => reply,
        Err(e) => {
            tracing::warn!(error = %e, "ask: the planning call failed; answering from one round");
            return Vec::new();
        }
    };

    // The prompt forbids repeating the question back and small models do it
    // anyway. Running it costs a search whose results the first round already
    // holds, and then a re-pack that can evict that round's neighbours for
    // nothing — such a round would take excerpts away rather than add any.
    //
    // Dropped rather than the whole plan, because a plan is now a list: one
    // echoed query beside two useful ones is two useful rounds, and refusing
    // all three over the first would throw them away.
    let mut queries = parse_plan(&reply.text);
    queries.retain(|q| !q.eq_ignore_ascii_case(question.trim()));
    if queries.is_empty() {
        tracing::debug!("ask: the plan named nothing to look for");
    }
    queries
}

/// Which of the planned subjects the base turned out not to hold.
///
/// The plan names what the first round's excerpts miss; each subject then gets
/// a search of its own. A subject whose search came back with every ranked
/// candidate *weak* — under `vector.weak_below` — is a hole in the base, stated
/// in the model's own words, for a question a person actually asked. Today that
/// is computed, used to widen one answer, and thrown away.
///
/// Weakness rather than emptiness, and for the reason `GapKind::Unmatched`
/// gives: retrieval always returns its best candidates however bad they are, so
/// "no hits" almost never happens and "nothing near" is the measurable form of
/// the same claim. It is deliberately the same threshold — one definition of
/// "the base held nothing near this", read at a second door.
///
/// Only the *ranked* hits are read. What a round reached sideways is structure
/// rather than a match: a neighbour is on the page because it sits next to
/// something, and letting it argue that a subject was covered would silence the
/// gap on evidence about a different passage.
///
/// A round that failed is not here at all — `fan_out` drops it — and that is
/// right: a search that never ran is not evidence that the base lacks anything.
pub(super) fn uncovered(rounds: &[super::Round]) -> Vec<String> {
    rounds
        .iter()
        .filter(|r| r.hits[..r.ranked].iter().all(|h| h.weak))
        .map(|r| r.query.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::core::search::SearchResult;

    fn hit(weak: bool) -> SearchResult {
        SearchResult {
            artifact_id: "a".into(),
            corpus_id: "s".into(),
            title: None,
            text: "body".into(),
            category: None,
            tags: vec![],
            score: 0.5,
            status: None,
            superseded_by: None,
            last_verified_at: None,
            weak,
            primed: false,
            in_sitting: false,
            past_cliff: false,
            similarity: None,
            titled_by_corpus: false,
            via: None,
            reason: None,
            explanation: None,
            model_written: false,
            synthesized: false,
            origin_count: 0,
        }
    }

    fn round(query: &str, ranked: Vec<SearchResult>) -> super::super::Round {
        super::super::Round {
            query: query.into(),
            ranked: ranked.len(),
            hits: ranked,
            retrieved: vec![],
            cliff_at: None,
        }
    }

    /// The subject the model asked for and the base could not answer. The
    /// planning call already paid for this; today it is discarded.
    #[test]
    fn a_subject_whose_every_hit_was_weak_is_uncovered() {
        let rounds = [round("job priority", vec![hit(true), hit(true)])];
        assert_eq!(super::uncovered(&rounds), vec!["job priority".to_string()]);
    }

    /// One real match is coverage. The subject was named as missing and turned
    /// out not to be, which is the fan-out working rather than a hole.
    #[test]
    fn a_subject_with_one_real_match_is_not_a_gap() {
        let rounds = [round("job priority", vec![hit(true), hit(false)])];
        assert!(super::uncovered(&rounds).is_empty());
    }

    /// Nothing came back at all. Rare, because retrieval returns its best
    /// candidates however bad they are — but when it happens it is the same
    /// claim in its strongest form.
    #[test]
    fn a_subject_that_returned_nothing_is_uncovered() {
        let rounds = [round("job priority", vec![])];
        assert_eq!(super::uncovered(&rounds), vec!["job priority".to_string()]);
    }

    /// What a round reached sideways is not a match. A neighbour is on the page
    /// because it sits beside something, and letting it vouch for a subject
    /// would close a gap on evidence about a different passage.
    #[test]
    fn a_neighbour_reached_sideways_does_not_cover_a_subject() {
        let mut r = round("job priority", vec![hit(true)]);
        // Appended past `ranked`, exactly as `reach_sideways` appends.
        r.hits.push(hit(false));
        assert_eq!(super::uncovered(&[r]), vec!["job priority".to_string()]);
    }
}
