//! The one question an ask is allowed to ask itself.

use super::Core;
use crate::infer::budget::pack_by_budget;
use crate::infer::prompt::{PLAN_SYSTEM, ask_prompt, parse_plan};

/// Which subjects this answer still lacks, as queries; asked once, never in a loop.
/// Empty when no planner is wired or on any failure: the ask carries on with round one.
pub(super) async fn needed_queries(
    core: &Core,
    question: &str,
    excerpts: &[String],
) -> Vec<String> {
    let Some(model) = core.planner.as_ref() else {
        return Vec::new();
    };

    // Packed against the planning model's window, usually smaller than the answer model's.
    let context = model.context_tokens();
    let budget = super::excerpt_budget(&**model, &core.counter, PLAN_SYSTEM, question);
    let kept = pack_by_budget(excerpts, &core.counter, budget);
    if kept == 0 {
        // With no excerpt shown, the model would only re-query the question itself.
        tracing::debug!("ask: no excerpt fits the planning window; answering from one round");
        return Vec::new();
    }

    // Same prompt shape as the answer call, so the model sees the same material.
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

    // Small models echo the question despite the prompt; drop that query, not the plan.
    let mut queries = parse_plan(&reply.text);
    queries.retain(|q| !q.eq_ignore_ascii_case(question.trim()));
    if queries.is_empty() {
        tracing::debug!("ask: the plan named nothing to look for");
    }
    queries
}

/// Planned subjects whose every ranked hit is under `vector.weak_below`: holes in the base.
/// Only ranked hits count; a sideways neighbour is evidence about a different passage.
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
            text: "body".into(),
            score: 0.5,
            weak,
            ..Default::default()
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

    #[test]
    fn a_subject_whose_every_hit_was_weak_is_uncovered() {
        let rounds = [round("job priority", vec![hit(true), hit(true)])];
        assert_eq!(super::uncovered(&rounds), vec!["job priority".to_string()]);
    }

    #[test]
    fn a_subject_with_one_real_match_is_not_a_gap() {
        let rounds = [round("job priority", vec![hit(true), hit(false)])];
        assert!(super::uncovered(&rounds).is_empty());
    }

    #[test]
    fn a_subject_that_returned_nothing_is_uncovered() {
        let rounds = [round("job priority", vec![])];
        assert_eq!(super::uncovered(&rounds), vec!["job priority".to_string()]);
    }

    #[test]
    fn a_neighbour_reached_sideways_does_not_cover_a_subject() {
        let mut r = round("job priority", vec![hit(true)]);
        // Appended past `ranked`, exactly as `reach_sideways` appends.
        r.hits.push(hit(false));
        assert_eq!(super::uncovered(&[r]), vec!["job priority".to_string()]);
    }
}
