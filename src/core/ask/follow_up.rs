//! The one question an ask is allowed to ask itself.

use super::Core;
use crate::infer::budget::pack_by_budget;
use crate::infer::prompt::{FOLLOW_UP_SYSTEM, follow_up_prompt, parse_follow_up};

/// What this answer still needs, asked once, or `None` for "it has enough".
///
/// `None` the moment no follow-up model is wired, which is the shipped default:
/// the feature costs one model call per question, so off has to mean no call
/// rather than a call whose result is discarded. There is nothing here to call
/// — `Core::follow_up` is `None` — so no later edit to the ask path can make
/// the disabled case cost anything.
///
/// Every failure is `None` as well. A timeout, a refusal, a reply that was
/// prose: the ask carries on with what round one retrieved. The operator asked
/// a question, not for a retrieval strategy, and an answer built from one round
/// is the thing that was already working.
pub(super) async fn needed_query(
    core: &Core,
    question: &str,
    excerpts: &[String],
) -> Option<String> {
    let model = core.follow_up.as_ref()?;

    // Packed against the follow-up model's window, not the answer model's. The
    // whole point of `follow_up_tier` is that this call runs on the efficient
    // endpoint while the answer runs on the deep one, and the efficient one is
    // usually the smaller window — handing it everything the deep model can
    // hold is a call refused for its size on exactly the questions that
    // retrieved enough to be worth a second round.
    let context = model.context_tokens();
    let reserve = model
        .max_output_tokens()
        .saturating_add(crate::infer::budget::MAX_HEADROOM_TOKENS)
        .min(context / 2);
    let budget = context
        .saturating_sub(core.counter.count(FOLLOW_UP_SYSTEM))
        .saturating_sub(core.counter.count(question))
        .saturating_sub(reserve);
    let kept = pack_by_budget(excerpts, &core.counter, budget);
    if kept == 0 {
        // Asking whether excerpts are sufficient without showing any of them
        // invites a query for whatever the question happens to name, which is
        // the search that just ran.
        tracing::debug!("ask: no excerpt fits the follow-up window; answering from one round");
        return None;
    }

    let user = follow_up_prompt(question, &excerpts[..kept]);
    let spent = core.counter.count(FOLLOW_UP_SYSTEM) + core.counter.count(&user);
    let ceiling =
        crate::infer::budget::ceiling_for_prompt(context, spent, model.max_output_tokens());

    match model.answer(FOLLOW_UP_SYSTEM, &user, ceiling).await {
        Ok(reply) => parse_follow_up(&reply.text),
        Err(e) => {
            tracing::warn!(error = %e, "ask: the follow-up call failed; answering from one round");
            None
        }
    }
}
