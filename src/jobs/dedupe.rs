//! One pair, one call.
//!
//! The sweep used to make up to `max_judgements` of these in a single job, so a
//! consolidation run blocked every capture behind it for as long as twenty model
//! calls took — the second-worst blocker in the system after synthesis. The
//! sweep now decides *which* pairs are worth asking about, which costs nothing,
//! and arms one unit each. The queue paces them and interleaves them with
//! everything else.
//!
//! The unit asks about the two artifacts its pair names and nothing else. It
//! used to expand into the connected component of still-open pairs around it and
//! settle all of them with one verdict, which is what made fan-in something a
//! single call had to survive: past `merge_max_roots` captured roots the
//! component was refused outright and every pair in it settled `Oversized`,
//! terminal and reached before the model saw anything. Sixteen pairs sat that
//! way, twelve roots against a default of eight nobody had typed.
//!
//! A cluster converges by merging two at a time instead. Each merge inherits the
//! flattened roots of both sides — `insert_merged_artifact` resolves the lineage
//! closure — and carries its members' open pairs with it, so the next question is
//! armed as soon as the merge is indexed rather than one sweep later.
//!
//! A member that is itself a merge is shown its own text as the thing under
//! judgement, with its captured roots beneath it as context. The context is what
//! keeps repeated merging from walking away from the wording someone captured:
//! the model can put back a detail an earlier merge dropped. It is unlettered, so
//! no verdict can name it, and it is trimmed oldest-first when the window is
//! tight — reference material degrades an answer when it goes, where refusing the
//! call answers nothing at all.
//!
//! Five verdicts come back and three touch an artifact: one is replaced by the
//! other, the two are merged, or neither states anything and both are retired.
//! Each is carried out where it is found, because deprecation and supersession
//! are reversible and the undo is the review. A value conflict is the one the
//! model is worst at, so it is escalated to a person and never merged.

use crate::core::Core;
use crate::error::{Error, Result};
use crate::infer::prompt::{MergedDraft, Relation};
use crate::store::artifacts::{Chunk, Provenance};
use crate::store::pairs::{ArtifactPair, DecidedBy, PairState};

/// What the model decided, with everything the write path needs already read.
pub struct Settlement {
    pub relation: Relation,
    pub detail: Option<String>,
    /// The member named obsolete, already checked against newest-wins. A member
    /// and not a root, because the members are the only artifacts the prompt
    /// letters and therefore the only things a letter can be naming. Only set
    /// for `Replaced`.
    pub obsolete: Option<String>,
    /// Only set for `Duplicate`, and only once the loss check has passed.
    pub merged: Option<MergedDraft>,
    /// The two artifacts the model was shown, in letter order.
    pub members: Vec<Chunk>,
    pub pair: ArtifactPair,
}

pub async fn run(core: &Core, pair_id: &str) -> Result<()> {
    let id: i64 = pair_id.parse().map_err(|_| Error::NotFound)?;
    let p = core.store.get_pair(id).await?;
    if p.state != PairState::Pending {
        // Settled by an operator, by a later sweep, or by the unit that merged
        // one of its members while this one waited out a backoff.
        return Ok(());
    }

    // Reported, not swallowed. `a_id` and `b_id` are `ON DELETE CASCADE` and
    // every pool sets `foreign_keys`, so a pair naming an artifact that is gone
    // is a state the schema does not allow — a failure here is the store being
    // unwell, not a deletion to absorb.
    let a = core.store.get_artifact(&p.a_id).await?;
    let b = core.store.get_artifact(&p.b_id).await?;
    // Re-checked here and not only when the unit was armed: a member can be
    // superseded by a later sweep or deprecated by an operator while this waits
    // out a backoff, and spending the scarcest thing in the system to rule on an
    // artifact no longer in results buys nothing.
    if !a.in_results() || !b.in_results() {
        return settle(
            core,
            &p,
            PairState::Dismissed,
            Some("a member is no longer in results"),
        )
        .await;
    }

    let members = vec![a, b];
    let member_ids: Vec<String> = members.iter().map(|c| c.id.clone()).collect();
    let root_map = core.store.roots_of(&member_ids).await?;
    // A member with no roots at all is a merge whose sources were deleted out
    // from under it. Its text is a paraphrase with nothing behind it — not
    // something to show the model as an original, and not something a rule can
    // settle. A person decides.
    if members
        .iter()
        .any(|c| root_map.get(&c.id).is_none_or(|r| r.is_empty()))
    {
        return settle(
            core,
            &p,
            PairState::Contradiction,
            Some("a merged member has lost its sources; resolve by hand"),
        )
        .await;
    }
    // A merge and one of its own sources are not two things to compare. Asking
    // would spend a call to be told that an artifact matches itself.
    let one_contains_the_other = root_map[&members[0].id].contains(&members[1].id)
        || root_map[&members[1].id].contains(&members[0].id);
    if one_contains_the_other {
        return settle(
            core,
            &p,
            PairState::Dismissed,
            Some("one of these is a source of the other"),
        )
        .await;
    }

    // Every root a merge would record has to be a captured artifact. That is
    // the invariant `insert_merged_artifact` enforces and the whole anti-drift
    // rule rests on: a merge over verbatim source text rewrites the substrate
    // into wording that belongs to no corpus and carries no span.
    //
    // Asked here, before the model call, and not left to the merge path. That
    // path refuses with `Error::Validation`, and `apply` propagates it — which
    // leaves the pair `Pending`, so `arm_dedupe` re-arms it on the next tick
    // and the same pair buys the same refusal every tick, forever. Refusing at
    // admission costs nothing and settles the row once.
    //
    // A synthesis over passages is the ordinary way to arrive here, and it is
    // not a defect: `roots_of` resolves such an artifact to the passages it
    // drew on, which is exactly what a synthesis is made of. A passage member
    // reaches it too, since a passage is its own root.
    //
    // `Contradiction` and not `Dismissed`: these two may well say the same
    // thing, and that question stays open on somebody's queue. What is
    // unavailable is only the automatic answer.
    let all_roots: Vec<String> = root_map.values().flatten().cloned().collect();
    let roots = core.store.artifacts_by_ids(&all_roots).await?;
    if let Some(r) = roots.iter().find(|r| r.provenance != Provenance::Captured) {
        tracing::info!(
            pair = p.id,
            root = %r.id,
            provenance = r.provenance.as_str(),
            "a member's lineage names something a merge may not rewrite; handing the pair to a person"
        );
        return settle(
            core,
            &p,
            PairState::Contradiction,
            Some(
                "These cannot be merged automatically: what one of them is made of is \
                 stored source text, and a merge must not rewrite that. Resolve by hand.",
            ),
        )
        .await;
    }

    // A merged member's captured roots, oldest first — context, never an input.
    // Read as whole artifacts because the prompt needs their titles: a body that
    // never names its own subject is the failure `dedupe_prompt` documents.
    let mut context: Vec<Vec<Chunk>> = Vec::new();
    for c in &members {
        let mut v = Vec::new();
        if c.provenance.is_model_written() {
            for rid in &root_map[&c.id] {
                match core.store.get_artifact(rid).await {
                    Ok(r) => v.push(r),
                    Err(Error::NotFound) => {}
                    Err(e) => return Err(e),
                }
            }
            v.sort_by_key(|r| r.created_at);
        }
        context.push(v);
    }

    // The two artifacts under judgement are bounded by what capture bounds and
    // always go out. What can grow without limit is the context block behind a
    // long lineage — and context is reference, not input, so it is trimmed
    // rather than defended against. Oldest first: the roots furthest from the
    // present are the ones a later capture is most likely to have restated.
    //
    // No count-based cap. `merge_max_roots` was one, left at a default of eight
    // nobody typed, and it settled whole clusters before any call was made.
    let Some(judge) = core.judge.clone() else {
        // Nothing to ask. The pair stays pending; `run_claimed` closes the unit
        // before it gets here when there is no synthesize role at all.
        return Ok(());
    };
    let counter = crate::infer::budget::TokenCounter::default();
    let window = judge.context_tokens();
    let ceiling = judge.max_output_tokens();
    let system = counter.count(crate::infer::prompt::DEDUPE_SYSTEM);
    let user = loop {
        let user = build_prompt(&members, &context, p.judge_attempts);
        let cost = system + counter.count(&user);
        if crate::infer::budget::checked_ceiling_for_prompt(window, cost, ceiling).is_some() {
            break user;
        }
        // Whichever member still holds the oldest surviving source gives it up.
        let oldest = context
            .iter()
            .enumerate()
            .filter(|(_, v)| !v.is_empty())
            .min_by_key(|(_, v)| v[0].created_at)
            .map(|(i, _)| i);
        match oldest {
            Some(i) => {
                context[i].remove(0);
            }
            // Nothing left to give: the two artifacts alone do not fit one call.
            // That is a fact about an artifact's size rather than about this
            // pair, no rule here can settle it, and recording it as answered is
            // what `Oversized` did wrong. A person decides.
            None => {
                return settle(
                    core,
                    &p,
                    PairState::Contradiction,
                    Some("these two artifacts do not fit one call; resolve by hand"),
                )
                .await;
            }
        }
    };

    // Counted before the call and regardless of how it goes, so a pair the model
    // keeps failing on drops behind the rest of the queue rather than absorbing
    // the budget again on the next sweep.
    core.store.record_judge_attempt(p.id).await?;

    let permit = core.gate.background().await;
    let reply = judge
        .complete(crate::infer::prompt::DEDUPE_SYSTEM, &user)
        .await;
    permit.finished();
    let reply = reply?;

    let verdict = match crate::infer::prompt::parse_dedupe(&reply) {
        Ok(v) => v,
        // A reply that cannot be read is an error, not a verdict: the pair stays
        // pending and the unit retries under the queue's backoff.
        //
        // Retrying is only worth anything because `dedupe_prompt` carries the
        // attempt number. Against an endpoint that caches by exact prompt, an
        // unchanged prompt would replay the same unreadable bytes for every one
        // of `MAX_ATTEMPTS`.
        //
        // Counted here and not beside `record_judge_attempt`, because this is
        // the only failure that says anything about the pair. A call the
        // endpoint never answered says something about the endpoint, and letting
        // an outage count against every pending pair would take the whole review
        // queue out of reach on its way past.
        Err(e) => {
            core.store.record_unreadable_judgement(p.id).await?;
            tracing::warn!(
                pair = id,
                attempt = p.judge_attempts,
                // A parse error names the column it gave up at; without the
                // length there is no way to tell whether that was the end of the
                // reply — a cut-off answer — or a break in the middle of one the
                // endpoint finished writing.
                reply_len = reply.len(),
                error = %e,
                "dedupe reply unreadable; pair stays pending"
            );
            return Err(e);
        }
    };

    apply(core, interpret(verdict, members, p)).await
}

/// Assemble the user prompt from the two members and whatever context survives
/// the budget.
fn build_prompt(members: &[Chunk], context: &[Vec<Chunk>], attempt: i64) -> String {
    let member = |i: usize| crate::infer::prompt::DedupeMember {
        title: members[i].title.as_deref().unwrap_or("untitled"),
        text: members[i].text.as_str(),
        sources: context[i]
            .iter()
            .map(|c| (c.title.as_deref().unwrap_or("untitled"), c.text.as_str()))
            .collect(),
    };
    crate::infer::prompt::dedupe_prompt(&member(0), &member(1), attempt)
}

/// Turn a parsed reply into what the write path will do.
///
/// Both guards live here because neither needs the store, which makes them
/// testable without one — and both turn a verdict *down* rather than up. This
/// pass is allowed to decline to act; it is not allowed to act on a shaky
/// answer.
fn interpret(
    v: crate::infer::prompt::Dedupe,
    members: Vec<Chunk>,
    pair: ArtifactPair,
) -> Settlement {
    let mut relation = v.relation;
    // The judge's own line. Nothing here writes one any more: the loss check
    // used to, and what it had to say was a list of tokens rather than a
    // finding. It is dropped rather than carried on the one path that turns the
    // verdict into its opposite — see the loss check below.
    let mut detail = v.detail;
    let mut merged = v.merged;
    let mut obsolete = None;

    if relation == Relation::Replaced {
        // Trust a named direction only when it agrees with the sweep's own
        // newest-wins bias (see `keeper`): a call naming the *newer* artifact
        // obsolete is exactly the failure mode worth guarding against, since it
        // would hide the side more likely to be current.
        //
        // The letter indexes the members, which are the only artifacts the
        // prompt letters. A merged member's sources go in unlettered, so a
        // letter can no longer resolve to something shown as reference — the
        // mismatch that used to supersede an artifact the model had never been
        // shown at all. A letter past the end names nothing and downgrades here,
        // which is why `parse_dedupe` does not pin the range itself.
        let named = v
            .supersedes
            .map(|c| (c as u8 - b'a') as usize)
            .and_then(|i| members.get(i));
        obsolete = match named {
            Some(named)
                if members
                    .iter()
                    .all(|o| o.id == named.id || named.created_at <= o.created_at) =>
            {
                Some(named.id.clone())
            }
            _ => None,
        };
        if obsolete.is_none() {
            relation = Relation::Conflict;
        }
    }

    if relation == Relation::Duplicate
        && let Some(d) = &merged
    {
        // Against the members, which are what is actually being merged — not
        // against every captured root behind them. A merged member's own text is
        // already a generation away from its sources, so checking the draft
        // against those sources would fail on a value an earlier merge dropped
        // and freeze that lineage: no later merge in it could ever be written.
        // Loss stays one generation deep per step, and the sources are in the
        // prompt as context precisely so the model can undo the earlier drift
        // rather than compound it.
        let lost = crate::jobs::merge::losses(&members, d);
        if !lost.is_empty() {
            // Escalated rather than retried: the merge is the thing that was
            // wrong, and refusing it hands the pair to a person, which is the
            // finding. Two sentences are still not written here. Not the lost
            // tokens — those are as often a bare "1, 4" as a version number,
            // which is evidence too thin to put on a card someone has to act
            // on, in a voice unlike every other line beside it. And not the
            // judge's own detail: it was written to say why the two are the
            // *same* ("same claim"), and under Contradiction the card renders
            // it directly beneath "these two disagree", so the pair would
            // state the opposite of its own finding to the person the
            // escalation exists to hand it to.
            //
            // What is written is a third thing, true of this escalation and of
            // no other: the merge was refused because it would have lost
            // something. Saying nothing at all was the state before, and five
            // cards reading "these two disagree" with nothing under them sat
            // on a deployment — a dispute nobody can act on is not better than
            // an imperfect sentence about it, it is the one thing a card in
            // this queue must never be.
            //
            // Logged, though. Keeping the tokens off the card is a judgement
            // about what a person can act on; keeping them out of the process
            // entirely left a refusal nothing anywhere could explain. Three
            // pairs sat as unexplained "these two disagree" for a day, and
            // finding out why meant reconstructing the tokenizer's output by
            // hand against the two texts. This is the one place that knows.
            tracing::warn!(
                pair = pair.id,
                lost = lost.join(", "),
                "refused a merge that would have dropped these"
            );
            relation = Relation::Conflict;
            detail = Some(
                "These two state a value differently, and merging them would have dropped \
                 one of them. Which is current is the judgement this hands over."
                    .to_string(),
            );
            merged = None;
        }
    }

    Settlement {
        relation,
        detail,
        obsolete,
        merged,
        members,
        pair,
    }
}

async fn apply(core: &Core, s: Settlement) -> Result<()> {
    match s.relation {
        Relation::Distinct => {
            settle(core, &s.pair, PairState::NoConflict, s.detail.as_deref()).await
        }
        Relation::Conflict => {
            tracing::info!("artifacts disagree; escalating rather than merging");
            settle(core, &s.pair, PairState::Contradiction, s.detail.as_deref()).await
        }
        // Acted on where it is found, like the two verdicts below it. Filing it
        // as a recommendation left the one answer that clears a pair of empty
        // artifacts waiting on a person holding no more evidence than the judge
        // had — and unlike a merge, nothing here is rewritten: both sides are
        // one press from active and still readable under `include_deprecated`.
        Relation::Vacuous => {
            discard_both(core, &s.pair, s.detail.as_deref(), DecidedBy::Model).await
        }
        Relation::Replaced => {
            let obsolete = s
                .obsolete
                .clone()
                .expect("interpret sets this or downgrades to Conflict");
            let winner = s
                .members
                .iter()
                .find(|m| m.id != obsolete)
                .map(|m| m.id.clone())
                .expect("a pair has two members and only one of them is obsolete");
            // A fresh status, not the snapshot `interpret` saw: an operator can
            // retire either side while the unit waits out a backoff.
            let live = |id: String| async move {
                match core.store.get_artifact(&id).await {
                    Ok(c) => Ok(c.in_results()),
                    Err(Error::NotFound) => Ok(false),
                    Err(e) => Err(e),
                }
            };
            // The survivor, checked for the same reason and for a worse
            // failure: `Core::supersede` refuses a winner that is not active,
            // so applying this would return a validation error out of the unit
            // and retry the same model call under the queue's backoff until the
            // attempts ran out — never settling the pair and never saying why.
            // Hiding the loser behind it is not the alternative: the answer
            // would disappear from results altogether.
            if !live(winner.clone()).await? {
                // `Stale` and not `Dismissed`, for the reason `PairState::Stale`
                // gives: a lifecycle event took the winner out of results, and
                // nobody answered anything. `Dismissed` is an operator's
                // decision and binding forever — `record_pair` is `INSERT OR
                // IGNORE` and `reopen_stale_pairs` only reopens `'stale'` — so
                // filing it here would bury the duplicate permanently the
                // moment someone deprecated the winner and reactivated it.
                return settle(
                    core,
                    &s.pair,
                    PairState::Stale,
                    Some("the surviving artifact left results before this could be applied"),
                )
                .await;
            }
            if !live(obsolete.clone()).await? {
                // Nothing to apply: the named side is already out of results, so
                // the replacement has in effect already happened.
                return settle(
                    core,
                    &s.pair,
                    PairState::NoConflict,
                    Some("the named replacement is already out of results"),
                )
                .await;
            }

            // The side effect FIRST. A failure here leaves the pair pending, so
            // the unit retries under the queue's backoff — the reverse order
            // left the verdict recorded on the pair but never applied, because
            // `run` skips a pair that is no longer Pending.
            core.supersede(&obsolete, &winner).await?;
            tracing::info!(superseded = %obsolete, by = %winner, "applied a replacement");
            // Done, with the model's reasoning kept as the record of why.
            // Leaving it Superseded listed the applied replacement as awaiting
            // confirmation forever.
            settle(core, &s.pair, PairState::Dismissed, s.detail.as_deref()).await
        }
        Relation::Duplicate => {
            let draft = s
                .merged
                .as_ref()
                .expect("interpret keeps this or downgrades to Conflict");
            // Both members, not their roots. A merged member is not its own
            // root, and `finish` hides what the lineage names — so passing only
            // the roots would leave that earlier merge active and near-identical
            // to the new one. `insert_merged_artifact` flattens both of them to
            // captured roots, and `subsumed_merges` catches the merged member.
            let sources: Vec<String> = s.members.iter().map(|m| m.id.clone()).collect();
            // `Validation` is the merge path's own refusal — a root it may not
            // rewrite — and not a failure to retry. Retrying is precisely the
            // damage: the pair would stay `Pending`, `arm_dedupe` re-arms it,
            // and the same refusal is bought again every tick. `run` already
            // declines such a pair before the call, so reaching this is a case
            // that check does not cover; the row is handed to a person rather
            // than left circling.
            let m = match crate::jobs::merge::write(core, draft, &sources).await {
                Ok(m) => m,
                Err(Error::Validation(why)) => {
                    tracing::warn!(
                        pair = s.pair.id,
                        reason = %why,
                        "the merge path refused this draft; handing the pair to a person"
                    );
                    return settle(
                        core,
                        &s.pair,
                        PairState::Contradiction,
                        Some(
                            "These could not be merged: the merge was refused because of \
                             what one of them is made of. Resolve by hand.",
                        ),
                    )
                    .await;
                }
                Err(e) => return Err(e),
            };
            // `merged_into` rather than a detail string: if the embed never
            // lands, the sweep's reap has to find exactly this pair and reopen
            // it (`reap_stranded`).
            core.store
                .set_pair_merged(s.pair.id, &m.id, s.detail.as_deref(), DecidedBy::Model)
                .await
        }
    }
}

/// Answer a pair by retiring both sides.
///
/// The queue's other answers each assume something. Keeping one side assumes
/// one of them is worth keeping; Dismiss assumes the pair is a question not
/// worth asking and leaves both in results. Neither fits two artifacts that
/// state nothing — a body that is its own file path, an outline with nothing
/// under it — which are alike for a reason that has no keeper.
///
/// Deprecation, not deletion, and through `core.deprecate` like every other
/// hide in the app: this is the one answer here that retires *both* artifacts,
/// so it is also the one that most needs the same undo as the rest.
///
/// Both callers of this — the judge's own `Relation::Vacuous` and the "Discard
/// both" button — reach it through here rather than each writing the sequence,
/// because the orderings and the one refusal below are the whole of what makes
/// it safe and none of them is obvious from the outside.
///
/// Which is also why `by` is a parameter and not `DecidedBy::Model`. The two
/// callers are a judge and a person, and a shared helper that picked one of
/// them would record every press of "Discard both" as the model's decision —
/// the precise untruth `DecidedBy` exists to end.
pub(crate) async fn discard_both(
    core: &Core,
    pair: &ArtifactPair,
    detail: Option<&str>,
    by: DecidedBy,
) -> Result<()> {
    // Both sides read before either is retired. A side already hidden in
    // favour of another artifact — an applied supersede from a neighbouring
    // pair does this, and on the job path it can land while this pair waits on
    // its own model call — is one `core.deprecate` refuses. Deprecating in
    // sequence therefore hid the first side and then failed on the second,
    // leaving the pair answerable, half of it already gone, and unanswerable
    // for good: every later attempt repeated the same refusal.
    //
    // A superseded artifact is out of results already, which is all a discard
    // is asking for, so it is skipped rather than treated as an error.
    let mut retire = Vec::new();
    for id in [&pair.a_id, &pair.b_id] {
        if core.store.get_artifact(id).await?.superseded_by.is_none() {
            retire.push(id.clone());
        }
    }
    // A side that other artifacts are hidden *behind* is not this pass's to
    // retire. `core.deprecate` refuses a supersession loser and has no rule for
    // a winner, so retiring one would leave `A -> W` with both ends out of
    // results: the reader who opens A is sent to an artifact that answers
    // nothing either, and no page can follow the hop.
    //
    // Nothing should reach here. A winner is an artifact a judgement already
    // preferred to another, and a later call finding that same artifact states
    // nothing contradicts the earlier one. So it is neither skipped nor
    // absorbed: the pair goes to a person with the reason on it, and the log
    // carries the ids, because a rule that fires when it cannot is worth
    // hearing about rather than working around.
    for id in &retire {
        let hidden = core.store.artifacts_superseded_by(id).await?;
        if !hidden.is_empty() {
            tracing::warn!(
                pair = pair.id,
                winner = %id,
                hidden = hidden.join(", "),
                "refused to retire an artifact others are hidden behind"
            );
            return settle_as(
                core,
                pair,
                PairState::Contradiction,
                Some(
                    "One of these is the current version of another artifact, so retiring \
                     both would leave that one pointing at nothing. Resolve by hand.",
                ),
                by,
            )
            .await;
        }
    }
    // The side effects first and the pair settled after, the ordering the rest
    // of `apply` documents: a failure part-way leaves the pair answerable
    // rather than recorded as answered and never applied.
    for id in &retire {
        core.deprecate(id).await?;
    }
    // The one line that says this happened. `core.deprecate` logs an id apiece,
    // byte-identical to an operator pressing the button, and the pair settles
    // `Dismissed` — a state no queue lists. Without this, two artifacts leaving
    // results unattended is a thing an operator can find no record of, and the
    // judge's reason for it sits on a row nothing in the UI reads back.
    tracing::info!(
        pair = pair.id,
        a = %pair.a_id,
        b = %pair.b_id,
        retired = retire.len(),
        detail = detail.unwrap_or("none given"),
        "retired both sides of a pair that states nothing"
    );
    // Settled the way an applied replacement settles, and carrying the judge's
    // line rather than dropping it: it is the only record of why both sides
    // were retired, and `set_pair_state` writes `detail` unconditionally, so
    // `None` would null it.
    settle_as(core, pair, PairState::Dismissed, detail, by).await
}

/// One pair, one verdict, decided by the judge.
///
/// Everything on this path is the model's: the unit runs unattended, and the
/// rules it applies without asking are the judge's too (`DecidedBy::Model`).
async fn settle(
    core: &Core,
    pair: &ArtifactPair,
    state: PairState,
    detail: Option<&str>,
) -> Result<()> {
    settle_as(core, pair, state, detail, DecidedBy::Model).await
}

/// The same write, for the one path here a person can also reach.
async fn settle_as(
    core: &Core,
    pair: &ArtifactPair,
    state: PairState,
    detail: Option<&str>,
    by: DecidedBy,
) -> Result<()> {
    core.store.set_pair_state(pair.id, state, detail, by).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::infer::fake::ScriptedCompleter;
    use crate::jobs::consolidate::tests::{seed, seed_titled};
    use crate::store::artifacts::ArtifactStatus;
    use std::sync::Arc;

    /// Record a pair for two artifacts and hand back its row id.
    async fn queue_pair(core: &Core, a: &str, b: &str) -> i64 {
        core.store.record_pair(a, b, 0.91).await.unwrap();
        core.store
            .pairs_by_state(PairState::Pending, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|p| (p.a_id == a || p.b_id == a) && (p.a_id == b || p.b_id == b))
            .expect("the pair was just recorded")
            .id
    }

    /// A passage, and a synthesis drawn from it, under a corpus of their own.
    async fn a_passage_and_a_synthesis_over_it(core: &Core) -> (String, String) {
        let src = core
            .store
            .insert_corpus("skript", "web", None)
            .await
            .unwrap();
        let passage = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "Spuren sind materielle Veraenderungen.".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
                Provenance::Passage,
            )
            .await
            .unwrap()
            .remove(0)
            .id;
        let synth = core
            .store
            .insert_synthesized_artifact(
                &crate::store::artifacts::NewSynthesized {
                    text: "Spuren sind Veraenderungen am Material.".into(),
                    title: Some("Spurenkunde".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                    cues: vec![],
                },
                std::slice::from_ref(&passage),
            )
            .await
            .unwrap()
            .id;
        (passage, synth)
    }

    /// A synthesis is made of passages, by design — so `roots_of` resolves it
    /// to source text, and `insert_merged_artifact` refuses to merge it.
    ///
    /// The refusal has to happen before the model call. Leaving it to the merge
    /// path means `apply` propagates `Validation`, the pair stays `Pending`, and
    /// `arm_dedupe` arms it again on the next tick — the same pair buying the
    /// same refusal at full price, forever.
    #[tokio::test]
    async fn a_pair_a_merge_could_not_write_is_handed_over_before_the_call() {
        let mut core = test_core().await;
        let judge = Arc::new(ScriptedCompleter::new(vec![]));
        core.judge = Some(judge.clone());
        let (_passage, synth) = a_passage_and_a_synthesis_over_it(&core).await;
        let other = seed(&core, &[("Spuren am Material", [1.0, 0.0])])
            .await
            .remove(0);
        let pair = queue_pair(&core, &synth, &other).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(
            judge.calls(),
            0,
            "a pair no merge can write was still judged"
        );
        let read = core.store.get_pair(pair).await.unwrap();
        assert_eq!(
            read.state,
            PairState::Contradiction,
            "the pair was left for `arm_dedupe` to buy again"
        );
    }

    /// The same, for a passage on one side. A passage is its own root, so it
    /// reaches the merge path's refusal by a different route and must be
    /// declined by the same rule.
    #[tokio::test]
    async fn a_pair_naming_a_passage_is_handed_over_before_the_call() {
        let mut core = test_core().await;
        let judge = Arc::new(ScriptedCompleter::new(vec![]));
        core.judge = Some(judge.clone());
        let (passage, _synth) = a_passage_and_a_synthesis_over_it(&core).await;
        let other = seed(&core, &[("Spuren am Material", [1.0, 0.0])])
            .await
            .remove(0);
        let pair = queue_pair(&core, &passage, &other).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(judge.calls(), 0, "a passage pair was sent to the judge");
        assert_eq!(
            core.store.get_pair(pair).await.unwrap().state,
            PairState::Contradiction
        );
    }

    /// The button and the judge reach one helper, and the row has to say which
    /// of them pressed it — the whole point of the column.
    #[tokio::test]
    async fn discarding_both_records_whoever_asked_for_it() {
        let core = test_core().await;
        let ids = seed(&core, &[("a", [1.0, 0.0]), ("b", [0.93, 0.37])]).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        let row = core.store.get_pair(pair).await.unwrap();

        discard_both(
            &core,
            &row,
            Some("each body is its own file path"),
            DecidedBy::Operator,
        )
        .await
        .unwrap();

        assert_eq!(
            core.store.get_pair(pair).await.unwrap().decided_by,
            Some(DecidedBy::Operator),
            "a person's press was recorded as the model's decision"
        );
    }

    #[tokio::test]
    async fn the_judge_runs_on_the_synthesize_endpoint_not_the_ask_one() {
        // The duplicate judge used to share `completer` with the RAG answer
        // path, which put background sweep traffic in front of an interactive
        // question and tuned one model for two unrelated jobs. The pair here
        // must be ruled on without the ask model being touched at all.
        let mut core = test_core().await;
        let judge = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
        ]));
        let asker = Arc::new(ScriptedCompleter::new(vec![]));
        core.judge = Some(judge.clone());
        core.completer = Some(asker.clone());
        let ids = seed(&core, &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])]).await;
        let seed_pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &seed_pair.to_string()).await.unwrap();

        assert_eq!(judge.calls(), 1, "the judge endpoint was not the one asked");
        assert_eq!(asker.calls(), 0, "the sweep called the ask model");
    }

    /// A verdict that neither side states anything is acted on where it is
    /// found, as `replaced` and `duplicate` already are. The queue's other
    /// answers each assume something — keeping one side assumes one is worth
    /// keeping, Dismiss assumes the question was not worth asking — and neither
    /// fits two artifacts that say nothing. Filing it as a recommendation left
    /// the one answer that clears them waiting on a person holding no more
    /// evidence than the judge had.
    ///
    /// Deprecation, not deletion: both sides are one press from active and
    /// still readable under `include_deprecated`. That undo is the review.
    #[tokio::test]
    async fn a_vacuous_verdict_retires_both_sides_without_waiting_for_an_operator() {
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"vacuous","detail":"each body is its own file path"}"#.into(),
        ])));
        let ids = seed(
            &core,
            &[("notes/a.md", [1.0, 0.0]), ("notes/b.md", [0.99, 0.01])],
        )
        .await;
        let pid = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pid.to_string()).await.unwrap();

        for id in &ids {
            assert_eq!(
                core.store.get_artifact(id).await.unwrap().status,
                ArtifactStatus::Deprecated,
                "a side of a vacuous pair was left in results"
            );
        }
        let pair = core.store.get_pair(pid).await.unwrap();
        assert_eq!(
            pair.state,
            PairState::Dismissed,
            "an applied discard is still listed as awaiting confirmation"
        );
        assert_eq!(
            pair.detail.as_deref(),
            Some("each body is its own file path"),
            "the only record of why both sides went was dropped"
        );
    }

    /// `Core::deprecate` refuses an artifact already hidden in favour of
    /// another, so retiring the two sides in sequence would hide the first and
    /// then fail on the second — leaving the pair recorded as unanswered with
    /// half of it already gone, and every later attempt repeating the same
    /// error. A side already out of results is what a discard is asking for, so
    /// it is skipped rather than treated as a failure.
    ///
    /// Against `discard_both` directly, because `run` settles a pair whose
    /// member is already hidden before it ever reaches the judge. The window
    /// this closes is the model call itself: a neighbouring pair's unit can
    /// supersede a side while this one waits for its answer.
    #[tokio::test]
    async fn discarding_a_pair_skips_a_side_that_is_already_hidden() {
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("notes/a.md", [1.0, 0.0]),
                ("notes/b.md", [0.99, 0.01]),
                ("ext4 needs a journal to be enabled", [0.2, 0.98]),
            ],
        )
        .await;
        let pid = queue_pair(&core, &ids[0], &ids[1]).await;
        // The row alone, not `Core::supersede`: that path moves the loser's
        // open pairs onto the winner, so the pair under test would no longer
        // name a hidden side. What this pins is the settlement holding a pair
        // that still does — the drift the sweep repairs
        // (`jobs::consolidate::follow_supersessions`), and the window in which
        // a neighbouring unit hides a side while this one waits for its answer.
        core.store
            .set_superseded_by(&ids[0], Some(&ids[2]))
            .await
            .unwrap();
        let pair = core.store.get_pair(pid).await.unwrap();

        discard_both(
            &core,
            &pair,
            Some("each body is its own file path"),
            DecidedBy::Model,
        )
        .await
        .unwrap();

        assert_eq!(
            core.store.get_artifact(&ids[1]).await.unwrap().status,
            ArtifactStatus::Deprecated,
            "the side that could be retired was not"
        );
        assert_eq!(
            core.store.get_pair(pid).await.unwrap().state,
            PairState::Dismissed,
            "the pair was left answerable with half of it already gone"
        );
    }

    /// The mirror of the case above, and the one `core.deprecate` has no rule
    /// for: not a side that is hidden, but a side that others are hidden
    /// *behind*. Retiring it would leave the artifacts pointing at it out of
    /// results with a winner that is out of results too — the dead end
    /// `Core::supersede` refuses to create from the other direction.
    ///
    /// It should not be reachable: a winner is an artifact some earlier
    /// judgement preferred, so a later one calling it empty disagrees with
    /// that. Which is why nothing is retired and nothing is skipped — the pair
    /// goes to a person, who is the only one who can say which call was wrong.
    #[tokio::test]
    async fn discarding_a_pair_refuses_a_side_others_are_hidden_behind() {
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("notes/a.md", [1.0, 0.0]),
                ("notes/b.md", [0.99, 0.01]),
                ("ext4 needs a journal to be enabled", [0.2, 0.98]),
            ],
        )
        .await;
        let pid = queue_pair(&core, &ids[0], &ids[1]).await;
        core.supersede(&ids[2], &ids[0]).await.unwrap();
        let pair = core.store.get_pair(pid).await.unwrap();

        discard_both(
            &core,
            &pair,
            Some("each body is its own file path"),
            DecidedBy::Model,
        )
        .await
        .unwrap();

        for id in &ids[..2] {
            assert_eq!(
                core.store.get_artifact(id).await.unwrap().status,
                ArtifactStatus::Active,
                "a side was retired out from under an artifact hidden behind it"
            );
        }
        assert_eq!(
            core.store.get_pair(pid).await.unwrap().state,
            PairState::Contradiction,
            "a discard nothing can carry out was recorded as carried out"
        );
    }

    async fn disagreeing(core: &Core) -> Vec<String> {
        seed(
            core,
            &[
                ("engram needs Rust 1.21.4 to build.", [1.0, 0.0]),
                ("engram needs Rust 1.30.0 to build.", [0.93, 0.37]),
            ],
        )
        .await
    }

    #[tokio::test]
    async fn two_artifacts_about_different_subjects_are_never_merged() {
        // FAT12, FAT16 and FAT32 are near-identical in form and deliberately
        // different in content: they sit at 0.91 and every number in them
        // differs. This is where the feature fires hardest and is most wrong,
        // and it no longer merely flags them — a merge would grind a reference
        // document into one paragraph that describes none of its subjects.
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"distinct","detail":"two different filesystems"}"#.into(),
        ])));
        let ids = seed_titled(
            &core,
            &[
                (
                    "FAT16 Specifications",
                    "Maximale Partitionsgröße 2 GB, 65524 Cluster.",
                    [1.0, 0.0],
                ),
                (
                    "FAT32 Specifications",
                    "32 Bit Clusternummern, 268435445 Cluster.",
                    [0.93, 0.37],
                ),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(
            core.store
                .pairs_by_state(PairState::NoConflict, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        for id in &ids {
            assert!(
                core.store
                    .get_artifact(id)
                    .await
                    .unwrap()
                    .superseded_by
                    .is_none(),
                "a reference document's sections were consolidated into each other"
            );
        }
    }

    /// The obsolete side is re-read before the verdict is applied; the winner
    /// was not. A supersession landing on the winner while the unit waited for
    /// the model — the sweep hiding it, an operator deprecating it — then made
    /// `Core::supersede` refuse, and the refusal came back as a failing job that
    /// retried the same call until its attempts ran out, rather than as a pair
    /// settled with a reason.
    #[tokio::test]
    async fn a_replacement_whose_survivor_left_results_settles_rather_than_failing() {
        let core = test_core().await;
        let ids = disagreeing(&core).await;
        let pair_id = queue_pair(&core, &ids[0], &ids[1]).await;
        let pair = core.store.get_pair(pair_id).await.unwrap();
        let members = vec![
            core.store.get_artifact(&pair.a_id).await.unwrap(),
            core.store.get_artifact(&pair.b_id).await.unwrap(),
        ];
        let obsolete = pair.a_id.clone();
        let winner = pair.b_id.clone();
        // Hidden after the model was asked and before the verdict is applied.
        core.deprecate(&winner).await.unwrap();

        apply(
            &core,
            Settlement {
                relation: Relation::Replaced,
                detail: Some("old flag vs new flag".into()),
                obsolete: Some(obsolete.clone()),
                merged: None,
                members,
                pair,
            },
        )
        .await
        .expect("a survivor that left results is a settlement, not a failure");

        assert!(
            core.store
                .get_artifact(&obsolete)
                .await
                .unwrap()
                .in_results(),
            "the loser was hidden behind an artifact that is itself out of results"
        );
        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty(),
            "the pair is still pending, so the same call will be spent again"
        );
        assert_eq!(
            core.store.get_pair(pair_id).await.unwrap().state,
            PairState::Stale,
            "a lifecycle event settled the pair as though a person had decided it"
        );

        // The property `Stale` buys, and the reason it is not `Dismissed`:
        // putting the winner back puts the question back. `record_pair` is
        // `INSERT OR IGNORE`, so a `Dismissed` row here would have buried this
        // duplicate for the life of the base with nothing saying why.
        core.reactivate(&winner).await.unwrap();
        assert_eq!(
            core.store.get_pair(pair_id).await.unwrap().state,
            PairState::Pending,
            "reactivating the survivor left the duplicate buried"
        );
    }

    #[tokio::test]
    async fn a_value_conflict_is_escalated_and_never_merged() {
        // Deciding which of two contradictory facts is current stays a person's
        // job. This is the one queue that expects a human, and autonomy does not
        // empty it.
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"conflict","detail":"1.21.4 versus 1.30.0"}"#.into(),
        ])));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let found = core
            .store
            .pairs_by_state(PairState::Contradiction, 10)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].detail.as_deref(), Some("1.21.4 versus 1.30.0"));
        for id in &ids {
            let c = core.store.get_artifact(id).await.unwrap();
            assert_eq!(
                c.status,
                ArtifactStatus::Active,
                "a conflict hid an artifact"
            );
            assert!(c.superseded_by.is_none());
        }
    }

    #[tokio::test]
    async fn a_plain_replacement_supersedes_rather_than_merging() {
        // The survivor is a stored original with a valid span and corpus lines
        // to render beside it. That is strictly better than a rewrite, and it is
        // the path by which the fidelity thesis keeps holding under autonomy —
        // so the prompt prefers it and this pins that the code does too.
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"replaced","supersedes":"a","detail":"old flag vs new flag"}"#.into(),
        ])));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .as_deref(),
            Some(ids[1].as_str())
        );
        let winner = core.store.get_artifact(&ids[1]).await.unwrap();
        assert_eq!(
            winner.provenance,
            crate::store::artifacts::Provenance::Captured,
            "a replacement wrote synthetic text where an original would do"
        );
        assert_eq!(winner.text, "engram needs Rust 1.30.0 to build.");
    }

    #[tokio::test]
    async fn a_replacement_naming_the_newer_artifact_is_not_trusted() {
        // A miscalibrated call proposing to hide the *newer* side disagrees with
        // the sweep's own newest-wins bias, so it falls back to a conflict
        // rather than being applied. Guessing here means hiding an artifact for
        // no stated reason.
        let mut core = test_core().await;
        let ids = disagreeing(&core).await;
        // `now()` is second-grained, so two rows inserted in one test would tie,
        // and a tie is meant to pass the guard. Force b strictly newer.
        sqlx::query("UPDATE artifacts SET created_at = created_at + 100 WHERE id = ?")
            .bind(&ids[1])
            .execute(&core.store.pool)
            .await
            .unwrap();
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"replaced","supersedes":"b","detail":"x"}"#.into(),
        ])));
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        for id in &ids {
            assert!(
                core.store
                    .get_artifact(id)
                    .await
                    .unwrap()
                    .superseded_by
                    .is_none()
            );
        }
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Contradiction, 10)
                .await
                .unwrap()
                .len(),
            1,
            "an untrusted direction must land as a conflict, not vanish"
        );
    }

    #[tokio::test]
    async fn a_merge_that_would_lose_a_value_says_so_without_naming_the_tokens() {
        // The loss check, from the unit's side. Two sentences must not be
        // written here: the lost tokens, which are as often a bare "1, 4" as a
        // version number and are evidence too thin to act on; and the judge's
        // own line, which was written to say the two are the *same* and would
        // contradict the "these two disagree" it renders under. Neither of
        // those is an argument for saying nothing at all — a card naming no
        // dispute is one nobody can decide, and five of them sat on the
        // deployment.
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"duplicate","detail":"same claim",
                "merged":{"text":"engram needs Rust 1.30.0 to build.","tags":[],"caveats":[]}}"#
                .into(),
        ])));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let found = core
            .store
            .pairs_by_state(PairState::Contradiction, 10)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        let detail = found[0].detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("dropped"),
            "the card says nothing about why it was escalated: {detail:?}"
        );
        assert!(
            !detail.contains("same claim"),
            "the judge's line said these two were the same; under \"these two \
             disagree\" it contradicts the card it sits on: {detail:?}"
        );
        assert!(
            !detail.contains("1.30.0"),
            "the lost tokens are evidence too thin to act on: {detail:?}"
        );
        for id in &ids {
            assert!(
                core.store
                    .get_artifact(id)
                    .await
                    .unwrap()
                    .superseded_by
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn a_second_unit_for_a_pair_already_settled_is_a_no_op() {
        // Two units are armed for one pair — a sweep re-arming what a queued
        // job already holds — and both run. The second must find its work done
        // rather than paying for the same verdict twice.
        let mut core = test_core().await;
        let completer = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"distinct","detail":"unrelated"}"#.into(),
        ]));
        core.judge = Some(completer.clone());
        let ids = seed(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.93, 0.37]),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();
        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(completer.calls(), 1, "the second unit asked again");
    }

    #[tokio::test]
    async fn a_failed_dedupe_leaves_the_pair_pending() {
        // A dead endpoint must not silently clear a queue of real duplicates.
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec!["not json".into()])));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        assert!(run(&core, &pair.to_string()).await.is_err());

        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(core.store.get_pair(pair).await.unwrap().judge_unreadable, 1);
    }

    #[tokio::test]
    async fn a_pair_whose_member_was_retired_is_dismissed_without_a_call() {
        let mut core = test_core().await;
        let completer = Arc::new(ScriptedCompleter::new(vec![]));
        core.judge = Some(completer.clone());
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        core.deprecate(&ids[0]).await.unwrap();

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(completer.calls(), 0, "a retired artifact cost a call");
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Dismissed, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_merge_that_lost_its_sources_is_never_shown_as_an_original() {
        // The self-root fallback put a model-synthesized paraphrase in the
        // prompt as a captured original, and a Duplicate verdict would then
        // record a merged artifact as root_id — paraphrase drift, one
        // generation per merge, which the lineage design exists to prevent.
        let mut core = test_core().await;
        let completer = Arc::new(ScriptedCompleter::new(vec![]));
        core.judge = Some(completer.clone());
        let ids = disagreeing(&core).await;
        let m = core
            .store
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    text: "a paraphrase".into(),
                    title: None,
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                },
                &[ids[0].clone()],
            )
            .await
            .unwrap();
        // Its every source cascades away, as a corpus deletion does.
        sqlx::query("DELETE FROM artifact_sources WHERE child_id = ?")
            .bind(&m.id)
            .execute(&core.store.pool)
            .await
            .unwrap();

        let pair = queue_pair(&core, &m.id, &ids[1]).await;
        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(
            completer.calls(),
            0,
            "a rootless merge reached the model as an original"
        );
        assert_eq!(
            core.store.get_pair(pair).await.unwrap().state,
            PairState::Contradiction,
            "the component goes to a person rather than being judged on a paraphrase"
        );
    }

    #[tokio::test]
    async fn an_applied_replacement_does_not_wait_for_an_operator() {
        // The pair used to stay in Superseded — the state every consumer reads
        // as "awaiting confirmation" — with Keep buttons that could only
        // return a validation error against the already-superseded side.
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"replaced","supersedes":"a","detail":"old flag vs new flag"}"#.into(),
        ])));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .is_some(),
            "the replacement was not applied"
        );
        assert!(
            core.store
                .pairs_by_state(PairState::Superseded, 10)
                .await
                .unwrap()
                .is_empty(),
            "an applied replacement is still listed as awaiting confirmation"
        );
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Dismissed, 10)
                .await
                .unwrap()
                .len(),
            1,
            "the applied pair should be settled the way the manual apply settles it"
        );
    }

    /// A merged artifact, written straight to the store, for the cases where
    /// "a member is itself a merge" is the thing under test.
    async fn merged_from(core: &Core, title: &str, text: &str, sources: &[String]) -> String {
        let m = crate::jobs::merge::write(
            core,
            &MergedDraft {
                title: Some(title.into()),
                text: text.into(),
                category: None,
                tags: vec![],
                caveats: vec![],
            },
            sources,
        )
        .await
        .unwrap();
        m.id
    }

    #[tokio::test]
    async fn a_unit_settles_only_its_own_pair() {
        // The unit used to claim the whole connected component and answer every
        // pair in it with one verdict. A sibling pair is a separate question
        // about a different pair of artifacts, and it keeps its own turn.
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
        ])));
        let ids = seed(
            &core,
            &[
                ("a text", [1.0, 0.0]),
                ("b text", [0.93, 0.37]),
                ("c text", [0.90, 0.44]),
            ],
        )
        .await;
        let seed_pair = queue_pair(&core, &ids[0], &ids[1]).await;
        queue_pair(&core, &ids[1], &ids[2]).await;

        run(&core, &seed_pair.to_string()).await.unwrap();

        assert_eq!(
            core.store
                .pairs_by_state(PairState::NoConflict, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1,
            "the sibling pair was answered by a call that was not about it"
        );
    }

    #[tokio::test]
    async fn a_cluster_past_the_old_cap_is_asked_about_rather_than_refused() {
        // Twelve artifacts in one chain is exactly the shape the fan-in cap
        // refused: it flattened past the default of eight and every pair in it
        // was settled Oversized, terminal, with no call ever made. Nothing about
        // it is oversized now — it is a sequence of two-artifact questions.
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
        ])));
        let rows: Vec<(&str, [f32; 2])> = vec![
            ("t0", [1.00, 0.00]),
            ("t1", [0.99, 0.01]),
            ("t2", [0.98, 0.02]),
            ("t3", [0.97, 0.03]),
            ("t4", [0.96, 0.04]),
            ("t5", [0.95, 0.05]),
            ("t6", [0.94, 0.06]),
            ("t7", [0.93, 0.07]),
            ("t8", [0.92, 0.08]),
            ("t9", [0.91, 0.09]),
            ("t10", [0.90, 0.10]),
            ("t11", [0.89, 0.11]),
        ];
        let ids = seed(&core, &rows).await;
        let seed_pair = queue_pair(&core, &ids[0], &ids[1]).await;
        for w in ids.windows(2).skip(1) {
            queue_pair(&core, &w[0], &w[1]).await;
        }

        run(&core, &seed_pair.to_string()).await.unwrap();

        assert!(
            core.store
                .pairs_by_state(PairState::Oversized, 20)
                .await
                .unwrap()
                .is_empty(),
            "a twelve-artifact cluster was refused instead of asked about"
        );
        assert_eq!(
            core.store
                .pairs_by_state(PairState::NoConflict, 20)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_letter_names_a_member_and_never_one_of_its_sources() {
        // The letter used to be resolved against the flattened roots while the
        // members were a different list, and whenever a component held an
        // earlier merge the two diverged — superseding an artifact the model had
        // never been shown. Here "a" is the merge, whose own sources would be
        // the ones a stale resolution reached.
        let mut core = test_core().await;
        let ids = seed_titled(
            &core,
            &[
                ("Old", "the pool holds eight", [1.0, 0.0]),
                ("Aside", "unrelated text", [0.99, 0.05]),
                ("Stale", "the pool holds four", [0.60, 0.80]),
            ],
        )
        .await;
        // A is the merge, so its two sources are what a letter resolved against
        // a flattened root list would reach. B is the artifact beside it.
        let m = merged_from(
            &core,
            "Merged",
            "the pool holds eight, and unrelated text",
            &[ids[0].clone(), ids[1].clone()],
        )
        .await;
        let pair = queue_pair(&core, &m, &ids[2]).await;
        // `record_pair` stores the two sides in id order, and `run` letters them
        // as it finds them — so which letter the merge got is not the caller's
        // to choose. The stale artifact is the one being named here; naming the
        // merge would be refused by newest-wins, since a merge is always the
        // newest artifact in its own lineage.
        let stored = core.store.get_pair(pair).await.unwrap();
        let letter = if stored.a_id == ids[2] { "a" } else { "b" };
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![format!(
            r#"{{"relation":"replaced","detail":"stale","supersedes":"{letter}"}}"#
        )])));

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(
            core.store
                .get_artifact(&ids[2])
                .await
                .unwrap()
                .superseded_by
                .as_deref(),
            Some(m.as_str()),
            "the letter did not name the member it was shown beside"
        );
        for src in &ids[..2] {
            assert!(
                core.store.get_artifact(src).await.unwrap().in_results(),
                "a source the model was shown as context was superseded by the verdict"
            );
        }
    }

    #[tokio::test]
    async fn a_merged_member_reaches_the_model_with_its_sources() {
        // Its own words are the thing under judgement; the captured originals go
        // beneath them so a detail an earlier merge dropped can come back.
        let mut core = test_core().await;
        let judge = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
        ]));
        core.judge = Some(judge.clone());
        let ids = seed_titled(
            &core,
            &[
                ("Pool sizing", "max_connections is 16", [1.0, 0.0]),
                ("Pool notes", "raise it for batch jobs", [0.99, 0.05]),
                ("Connections", "sixteen connections", [0.93, 0.37]),
            ],
        )
        .await;
        let m = merged_from(
            &core,
            "Pool",
            "the pool holds sixteen",
            &[ids[0].clone(), ids[1].clone()],
        )
        .await;
        let pair = queue_pair(&core, &m, &ids[2]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let sent = judge.prompts();
        let sent = sent.first().expect("the judge was asked");
        assert!(
            sent.contains("the pool holds sixteen"),
            "the merge's own words were withheld: {sent}"
        );
        assert!(
            sent.contains("max_connections is 16"),
            "a source was not shown as context: {sent}"
        );
        assert!(sent.contains("SOURCES OF"), "{sent}");
    }

    #[tokio::test]
    async fn a_merge_and_one_of_its_own_sources_is_dismissed_without_a_call() {
        // The pairwise form of the old "flattens to one root" guard: comparing a
        // merge with an artifact it was written from asks whether something
        // matches itself, at the price of a call.
        let mut core = test_core().await;
        let judge = Arc::new(ScriptedCompleter::new(vec![]));
        core.judge = Some(judge.clone());
        let ids = seed(&core, &[("a text", [1.0, 0.0]), ("b text", [0.99, 0.05])]).await;
        let m = merged_from(
            &core,
            "Merged",
            "a text and b text",
            &[ids[0].clone(), ids[1].clone()],
        )
        .await;
        let pair = queue_pair(&core, &m, &ids[0]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(
            judge.calls(),
            0,
            "a call was spent asking whether an artifact matches itself"
        );
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Dismissed, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_value_lost_by_an_earlier_merge_does_not_block_the_next_one() {
        // The loss check runs against what is actually being merged. Against the
        // whole flattened lineage instead, a value dropped a generation ago
        // would fail every later merge in that lineage forever and freeze it.
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"duplicate","detail":"same thing",
                "merged":{"title":"Pool","text":"the pool holds sixteen connections","tags":[],"caveats":[]}}"#
                .into(),
        ])));
        let ids = seed_titled(
            &core,
            &[
                (
                    "Pool sizing",
                    "max_connections is 16 and the timeout is 30s",
                    [1.0, 0.0],
                ),
                ("Pool notes", "raise it for batch jobs", [0.99, 0.05]),
                ("Connections", "sixteen connections", [0.93, 0.37]),
            ],
        )
        .await;
        // This merge already dropped "30s". The next one is not to blame for it.
        let m = merged_from(
            &core,
            "Pool",
            "the pool holds sixteen",
            &[ids[0].clone(), ids[1].clone()],
        )
        .await;
        let pair = queue_pair(&core, &m, &ids[2]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert!(
            core.store
                .pairs_by_state(PairState::Contradiction, 10)
                .await
                .unwrap()
                .is_empty(),
            "the merge was blamed for a value an earlier one had dropped"
        );
    }

    #[tokio::test]
    async fn a_merge_of_a_merge_names_every_original_behind_it() {
        // The whole point of merging two at a time: the result is mergeable
        // again, and it carries the flattened lineage of both sides rather than
        // naming the intermediate.
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"duplicate","detail":"same thing",
                "merged":{"title":"Pool","text":"max_connections is 16, raise it for batch jobs, sixteen connections","tags":[],"caveats":[]}}"#
                .into(),
        ])));
        let ids = seed_titled(
            &core,
            &[
                ("Pool sizing", "max_connections is 16", [1.0, 0.0]),
                ("Pool notes", "raise it for batch jobs", [0.99, 0.05]),
                ("Connections", "sixteen connections", [0.93, 0.37]),
            ],
        )
        .await;
        let m1 = merged_from(
            &core,
            "Pool",
            "max_connections is 16, raise it for batch jobs",
            &[ids[0].clone(), ids[1].clone()],
        )
        .await;
        let pair = queue_pair(&core, &m1, &ids[2]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let settled = core.store.get_pair(pair).await.unwrap();
        let m2 = settled
            .merged_into
            .expect("the second merge was never written");
        let roots = core.store.roots_of(&[m2]).await.unwrap();
        let roots: Vec<&String> = roots.values().flatten().collect();
        assert_eq!(
            roots.len(),
            3,
            "the merge of a merge did not inherit both lineages"
        );
        for id in &ids {
            assert!(
                roots.contains(&id),
                "an original is missing from the lineage"
            );
        }
    }

    #[tokio::test]
    async fn a_context_block_too_big_for_the_window_is_trimmed_not_refused() {
        // Context is reference material, so a window too small to hold it costs
        // the answer some quality — never the answer itself.
        //
        // The window is the system prompt plus room for the two members and
        // the reply, and nothing more: it has to be too small for the source
        // block and large enough for everything that is not trimmable, so it
        // moves when `DEDUPE_SYSTEM` does. Left at a number chosen against an
        // older, shorter prompt, this stopped testing the trim and started
        // testing the refusal one paragraph later.
        let mut core = test_core().await;
        let judge = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
        ]));
        judge.set_context_tokens(1600);
        core.judge = Some(judge.clone());
        let long = "verylongsourcetoken ".repeat(400);
        let ids = seed_titled(
            &core,
            &[
                ("Root one", long.as_str(), [1.0, 0.0]),
                ("Root two", "raise it for batch jobs", [0.99, 0.05]),
                ("Other", "sixteen connections", [0.93, 0.37]),
            ],
        )
        .await;
        let m = merged_from(
            &core,
            "Pool",
            "the pool holds sixteen",
            &[ids[0].clone(), ids[1].clone()],
        )
        .await;
        let pair = queue_pair(&core, &m, &ids[2]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(judge.calls(), 1, "the call was refused instead of trimmed");
        let sent = judge.prompts().first().cloned().unwrap();
        assert!(
            sent.contains("the pool holds sixteen"),
            "a member was trimmed away"
        );
        assert!(
            sent.contains("sixteen connections"),
            "a member was trimmed away"
        );
        assert!(
            !sent.contains(long.as_str()),
            "the oversized source was not trimmed"
        );
    }

    #[tokio::test]
    async fn two_members_that_alone_do_not_fit_go_to_a_person() {
        // A different failure with a different cause: an artifact so large that
        // no pair containing it can be judged. No rule here settles that, and
        // recording it as answered is what `Oversized` did wrong.
        let mut core = test_core().await;
        let judge = Arc::new(ScriptedCompleter::new(vec![]));
        judge.set_context_tokens(100);
        core.judge = Some(judge.clone());
        let long = "verylongartifacttoken ".repeat(500);
        let ids = seed_titled(
            &core,
            &[
                ("One", long.as_str(), [1.0, 0.0]),
                ("Two", long.as_str(), [0.93, 0.37]),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(
            judge.calls(),
            0,
            "a call that cannot fit the window was sent anyway"
        );
        let stuck = core
            .store
            .pairs_by_state(PairState::Contradiction, 10)
            .await
            .unwrap();
        assert_eq!(stuck.len(), 1);
        assert!(
            stuck[0]
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("do not fit"),
            "the reason a person is being asked was not recorded"
        );
        assert!(
            core.store
                .pairs_by_state(PairState::Oversized, 10)
                .await
                .unwrap()
                .is_empty(),
            "the refused state came back under the old name"
        );
    }
}
