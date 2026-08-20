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
