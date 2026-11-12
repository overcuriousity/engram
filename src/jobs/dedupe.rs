//! One judge call per pair. Members that are merges show their captured roots as unlettered
//! context, trimmed oldest-first; a value conflict is kept as a disagreement, never merged.

use crate::core::Core;
use crate::error::{Error, Result};
use crate::infer::prompt::{MergedDraft, Relation};
use crate::store::actions::{Job, Kind, NewAction};
use crate::store::artifacts::{Chunk, Provenance};
use crate::store::pairs::{ArtifactPair, DecidedBy, PairState};

fn action(
    kind: Kind,
    pair: &ArtifactPair,
    subject: &str,
    survivor: Option<&str>,
    detail: Option<&str>,
) -> NewAction {
    NewAction {
        job: Job::Dedupe,
        kind,
        subject_id: subject.to_string(),
        survivor_id: survivor.map(str::to_string),
        detail: detail.map(str::to_string),
        evidence: serde_json::json!({ "pair_id": pair.id, "a": pair.a_id, "b": pair.b_id }),
        pair_score: Some(pair.score),
    }
}

/// Whether this job already did this to any of these artifacts and had it taken back.
async fn taken_back_before(core: &Core, kind: Kind, subjects: &[&str]) -> Result<bool> {
    for id in subjects {
        if core.store.action_was_undone(id, kind).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The draft, not the notes, was at fault, so both stay as they are.
const LOSS_REFUSED: &str = "a draft dropped a value one of them states; both stay as they are";

const TAKEN_BACK: &str =
    "This was done to one of these before and taken back, so both stay as they are.";

/// Older bases' wording for the same refusal; the drain reads it.
#[cfg(test)]
pub(crate) const TAKEN_BACK_OLD: &str =
    "This was done to one of these before and taken back. Resolve by hand.";

pub struct Settlement {
    pub relation: Relation,
    pub detail: Option<String>,
    /// Checked against newest-wins; a member, because only members are lettered.
    pub obsolete: Option<String>,
    pub merged: Option<MergedDraft>,
    /// In letter order.
    pub members: Vec<Chunk>,
    pub pair: ArtifactPair,
}

pub async fn run(core: &Core, pair_id: &str) -> Result<()> {
    let id: i64 = pair_id.parse().map_err(|_| Error::NotFound)?;
    let p = core.store.get_pair(id).await?;
    // `synthesis_asked` arrives on a settled pair (often `Duplicate`); skipping it would freeze the
    // ask.
    if p.state != PairState::Pending && !p.synthesis_asked {
        return Ok(());
    }
    if p.synthesis_asked && p.merged_into.is_some() {
        // Already written; re-running would merge the pair twice.
        core.store.clear_pair_synthesis(p.id).await?;
        return Ok(());
    }
    // The pair outlived its press: the press would be taken as the verdict without re-reading the
    // state.
    if p.synthesis_asked && !crate::store::pairs::AWAITING_REVIEW.contains(&p.state) {
        core.store.clear_pair_synthesis(p.id).await?;
        return Ok(());
    }
    // Read before the judge call: a verdict this unit may not act on is a call spent for nothing.
    if !core.may_act(crate::store::actions::Job::Dedupe).await? {
        tracing::info!(
            pair = id,
            "budget spent; the pair waits for the window to move"
        );
        return Ok(());
    }

    // Pairs cascade on artifact delete, so a missing artifact is a store failure, not a deletion.
    let a = core.store.get_artifact(&p.a_id).await?;
    let b = core.store.get_artifact(&p.b_id).await?;
    // Re-checked: a member can leave results while this waits out a backoff.
    if !a.in_results() || !b.in_results() {
        // A cleared flag stops the card promising a synthesis nothing will write.
        if p.synthesis_asked {
            core.store.clear_pair_synthesis(p.id).await?;
        }
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
    // No roots: a merge whose sources were deleted, a paraphrase with nothing behind it.
    if members
        .iter()
        .any(|c| root_map.get(&c.id).is_none_or(|r| r.is_empty()))
    {
        return leave_both(
            core,
            &p,
            "a merged member has lost its sources; both stay as they are",
        )
        .await;
    }
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

    // Refused here, before the call: the merge path's `Validation` error would leave the pair
    // `Pending` and buy the same refusal every tick. Every merge root must be a captured artifact.
    let all_roots: Vec<String> = root_map.values().flatten().cloned().collect();
    let roots = core.store.artifacts_by_ids(&all_roots).await?;
    if let Some(r) = roots.iter().find(|r| r.provenance != Provenance::Captured) {
        tracing::info!(
            pair = p.id,
            root = %r.id,
            provenance = r.provenance.as_str(),
            "a member's lineage names something a merge may not rewrite; both stay as they are"
        );
        return leave_both(
            core,
            &p,
            "These cannot be written as one: what one of them is made of is stored source \
             text, which a merge must not rewrite, so both stay as they are.",
        )
        .await;
    }

    // The operator already decided these cover the same ground, and the judge does not decide this
    // class reliably (9 of 12 `distinct` on one case). Below the lineage refusal, which still
    // applies.
    if p.synthesis_asked {
        return synthesize_asked_pair(core, &p, members).await;
    }

    // Whole artifacts, because the prompt needs their titles.
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

    // Context is reference, not input: trimmed oldest-first rather than capped by count.
    let Some(judge) = core.judge.clone() else {
        // The role went away between the arming and the run; the pair stays pending.
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
            // The two artifacts alone do not fit one call; no call can settle that.
            None => {
                return leave_both(
                    core,
                    &p,
                    "these two do not fit one call; both stay as they are",
                )
                .await;
            }
        }
    };

    // Counted before the call so a pair the model keeps failing on drops behind the queue.
    core.store.record_judge_attempt(p.id).await?;

    let permit = core.gate.background().await;
    let reply = judge
        .complete(crate::infer::prompt::DEDUPE_SYSTEM, &user)
        .await;
    permit.finished();
    let reply = reply?;

    let verdict = match crate::infer::prompt::parse_dedupe(&reply) {
        Ok(v) => v,
        // Retrying helps only because `dedupe_prompt` carries the attempt number (caching
        // endpoints). Counted here, not beside `record_judge_attempt`: an outage must not count
        // against every pair.
        Err(e) => {
            core.store.record_unreadable_judgement(p.id).await?;
            tracing::warn!(
                pair = id,
                attempt = p.judge_attempts,
                // Tells a cut-off reply from a break mid-reply.
                reply_len = reply.len(),
                error = %e,
                "dedupe reply unreadable; pair stays pending"
            );
            return Err(e);
        }
    };

    apply(core, interpret(verdict, members, p)).await
}

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

/// Both guards only ever turn a verdict down, never up.
fn interpret(
    v: crate::infer::prompt::Dedupe,
    members: Vec<Chunk>,
    pair: ArtifactPair,
) -> Settlement {
    let mut relation = v.relation;
    // Dropped on the loss-check path, where it would argue the opposite of the outcome.
    let mut detail = v.detail;
    let mut merged = v.merged;
    let mut obsolete = None;

    if relation == Relation::Replaced {
        // A named direction is trusted only when it agrees with newest-wins (see `keeper`);
        // otherwise it is a disagreement. A letter past the end names nothing and downgrades here.
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
        // Against the members, not their roots: checking against roots would freeze a lineage on a
        // value an earlier merge dropped.
        let lost = crate::jobs::merge::losses(&members, d);
        if !lost.is_empty() {
            // Closed, not a `Contradiction`: the draft dropped a value, the notes agree. Tokens go
            // to the log, not the row, where they read as too thin to be a reason.
            tracing::warn!(
                pair = pair.id,
                lost = lost.join(", "),
                "refused a merge that would have dropped these"
            );
            relation = Relation::Distinct;
            detail = Some(LOSS_REFUSED.to_string());
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

/// Answers an operator's synthesis ask on an older base; the tail is `write_merge` so both kinds of
/// merge leave the base in one shape.
async fn synthesize_asked_pair(core: &Core, p: &ArtifactPair, members: Vec<Chunk>) -> Result<()> {
    let Some(writer) = core.pair_synthesizer.clone() else {
        // The flag stays set, so the next run with a writer picks it up.
        return Ok(());
    };
    // No context block to trim here, so the two bodies fit or no model can write the pair.
    let user = synthesis_prompt(&members);
    let counter = crate::infer::budget::TokenCounter::default();
    let cost = counter.count(crate::infer::prompt::SYNTHESIZE_SYSTEM) + counter.count(&user);
    if crate::infer::budget::checked_ceiling_for_prompt(
        writer.context_tokens(),
        cost,
        writer.max_output_tokens(),
    )
    .is_none()
    {
        tracing::warn!(
            pair = p.id,
            "these two artifacts do not fit one call; the synthesis is refused"
        );
        return leave_both(
            core,
            p,
            "these two do not fit one call; both stay as they are",
        )
        .await;
    }

    core.store.record_judge_attempt(p.id).await?;
    let permit = core.gate.background().await;
    let reply = writer
        .complete(crate::infer::prompt::SYNTHESIZE_SYSTEM, &user)
        .await;
    permit.finished();
    let reply = reply?;
    let draft = match crate::infer::prompt::parse_synthesis(&reply) {
        Ok(d) => d,
        // Without the count this re-arms every tick on a reply that will not parse.
        Err(e) => {
            core.store.record_unreadable_judgement(p.id).await?;
            tracing::warn!(
                pair = p.id,
                reply_len = reply.len(),
                error = %e,
                "synthesis reply unreadable; the pair keeps its ask and waits"
            );
            return Err(e);
        }
    };

    // `merge::write` validates nothing, so the loss check belongs to each caller; against the
    // members, as in `interpret`.
    let lost = crate::jobs::merge::losses(&members, &draft);
    if !lost.is_empty() {
        tracing::warn!(
            pair = p.id,
            lost = lost.join(", "),
            "refused a synthesis that would have dropped these"
        );
        return leave_both(
            core,
            p,
            "every draft dropped a value one of them states; both stay as they are",
        )
        .await;
    }

    let sources: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
    write_merge(core, p, &sources, &draft, DecidedBy::Operator).await
}

/// The loss check is the caller's.
async fn write_merge(
    core: &Core,
    p: &ArtifactPair,
    sources: &[String],
    draft: &MergedDraft,
    by: DecidedBy,
) -> Result<()> {
    match crate::jobs::merge::write(core, draft, sources).await {
        Ok(m) => {
            let act = format!("merged into {} from {} sources", m.id, sources.len());
            for source in sources {
                let mut row = action(Kind::Merge, p, source, Some(&m.id), Some(act.as_str()));
                // `Store::band_record` reads this job's rows as verdicts; a press is not one.
                if by == DecidedBy::Operator
                    && let Some(evidence) = row.evidence.as_object_mut()
                {
                    evidence.insert("asked_by".into(), serde_json::json!("operator"));
                }
                core.store.record_action(&row).await?;
            }
            let why = match by {
                DecidedBy::Operator => "synthesized at an operator's request",
                _ => "the judge read both as one, and nothing either states was lost",
            };
            core.store.set_pair_merged(p.id, &m.id, Some(why), by).await
        }
        // `leave_both` clears any ask with it.
        Err(Error::Validation(why)) => {
            tracing::warn!(pair = p.id, reason = %why, "the merge path refused a merge");
            leave_both(
                core,
                p,
                "These could not be written as one because of what one of them is made of; \
                 both stay as they are.",
            )
            .await
        }
        Err(e) => Err(e),
    }
}

/// Unlettered and without sources: this call writes, it does not decide.
fn synthesis_prompt(members: &[Chunk]) -> String {
    let mut s = String::new();
    for m in members {
        s.push_str("----- ARTIFACT -----\n");
        // A passage has no name; `row_label` would repeat its opening as a title.
        if let Some(name) = prompt_name(m) {
            s.push_str(&format!("Title: {name}\n"));
        }
        s.push_str(&format!("\n{}\n", m.text));
    }
    s.push_str("----- END -----");
    s
}

/// A passage or note heading names a section, not the artifact.
fn prompt_name(c: &Chunk) -> Option<&str> {
    if !c.provenance.names_its_own_text() {
        return None;
    }
    c.title.as_deref().map(str::trim).filter(|t| !t.is_empty())
}

async fn apply(core: &Core, s: Settlement) -> Result<()> {
    match s.relation {
        Relation::Distinct => {
            settle(core, &s.pair, PairState::NoConflict, s.detail.as_deref()).await
        }
        Relation::Conflict => {
            tracing::info!("artifacts disagree; kept as a disagreement rather than merged");
            settle(core, &s.pair, PairState::Contradiction, s.detail.as_deref()).await
        }
        // Both sides stay one press from active, readable under `include_deprecated`.
        Relation::Vacuous => {
            discard_both(core, &s.pair, s.detail.as_deref(), DecidedBy::Model).await
        }
        Relation::Replaced => {
            let obsolete = s.obsolete.clone().ok_or_else(|| {
                Error::Internal("a replaced pair without its obsolete side".into())
            })?;
            let winner = s
                .members
                .iter()
                .find(|m| m.id != obsolete)
                .map(|m| m.id.clone())
                .ok_or_else(|| Error::Internal("a replaced pair without a winner".into()))?;
            // Fresh status: either side can be retired while the unit waits out a backoff.
            let live = |id: String| async move {
                match core.store.get_artifact(&id).await {
                    Ok(c) => Ok(c.in_results()),
                    Err(Error::NotFound) => Ok(false),
                    Err(e) => Err(e),
                }
            };
            // `Core::supersede` refuses an inactive winner, which would retry the call until
            // attempts ran out.
            if !live(winner.clone()).await? {
                // `Stale`, not `Dismissed`: `record_pair` is `INSERT OR IGNORE`, so `Dismissed`
                // buries it for good.
                return settle(
                    core,
                    &s.pair,
                    PairState::Stale,
                    Some("the surviving artifact left results before this could be applied"),
                )
                .await;
            }
            if !live(obsolete.clone()).await? {
                return settle(
                    core,
                    &s.pair,
                    PairState::NoConflict,
                    Some("the named replacement is already out of results"),
                )
                .await;
            }

            if taken_back_before(core, Kind::Supersede, &[&obsolete]).await? {
                return leave_both(core, &s.pair, TAKEN_BACK).await;
            }
            // Side effect first: `run` skips a non-Pending pair, so the reverse order never applies
            // it.
            core.supersede_with(
                &obsolete,
                &winner,
                Some(action(
                    Kind::Supersede,
                    &s.pair,
                    &obsolete,
                    Some(&winner),
                    s.detail.as_deref(),
                )),
            )
            .await?;
            tracing::info!(superseded = %obsolete, by = %winner, "applied a replacement");
            settle(core, &s.pair, PairState::Dismissed, s.detail.as_deref()).await
        }
        // Safe because of the loss check, the journal and `jobs::retract`, not because the label is
        // steady.
        Relation::Duplicate => {
            let ids: Vec<String> = s.members.iter().map(|m| m.id.clone()).collect();
            let subjects: Vec<&str> = ids.iter().map(String::as_str).collect();
            if taken_back_before(core, Kind::Merge, &subjects).await? {
                return leave_both(core, &s.pair, TAKEN_BACK).await;
            }
            match &s.merged {
                // Members, not roots: a merged member is not its own root and would stay active.
                Some(draft) => write_merge(core, &s.pair, &ids, draft, DecidedBy::Model).await,
                // Unreachable: `parse_dedupe` refuses a duplicate without a draft.
                None => {
                    leave_both(
                        core,
                        &s.pair,
                        "the judge read these as one but wrote no draft; both stay as they are",
                    )
                    .await
                }
            }
        }
    }
}

/// For two artifacts that state nothing. `by` is a parameter so a person's press is not recorded as
/// the model's.
pub(crate) async fn discard_both(
    core: &Core,
    pair: &ArtifactPair,
    detail: Option<&str>,
    by: DecidedBy,
) -> Result<()> {
    // Superseded sides are skipped: `core.deprecate` refuses them, and deprecating in sequence
    // would strand the pair half-retired.
    let mut retire = Vec::new();
    for id in [&pair.a_id, &pair.b_id] {
        if core.store.get_artifact(id).await?.superseded_by.is_none() {
            retire.push(id.clone());
        }
    }
    if by == DecidedBy::Model
        && taken_back_before(
            core,
            Kind::Discard,
            &retire.iter().map(String::as_str).collect::<Vec<_>>(),
        )
        .await?
    {
        return leave_both(core, pair, TAKEN_BACK).await;
    }
    // A supersession winner is not this pass's to retire: `A -> W` would leave both ends out of
    // results.
    for id in &retire {
        let hidden = core.store.artifacts_superseded_by(id).await?;
        if !hidden.is_empty() {
            tracing::warn!(
                pair = pair.id,
                winner = %id,
                hidden = hidden.join(", "),
                "refused to retire an artifact others are hidden behind"
            );
            let why = "One of these is the current version of another artifact, so retiring \
                       both would leave that one pointing at nothing; both stay as they are.";
            return match by {
                DecidedBy::Model => leave_both(core, pair, why).await,
                _ => {
                    if pair.synthesis_asked {
                        core.store.clear_pair_synthesis(pair.id).await?;
                    }
                    settle_as(core, pair, PairState::NoConflict, Some(why), by).await
                }
            };
        }
    }
    // Side effects first, then the pair: a failure part-way leaves it answerable.
    for id in &retire {
        // The journal is what the base did to itself; a person's press is not journaled.
        let journal =
            (by == DecidedBy::Model).then(|| action(Kind::Discard, pair, id, None, detail));
        core.deprecate_with(id, journal).await?;
    }
    // The only record of an unattended discard; `core.deprecate` logs like an operator press.
    tracing::info!(
        pair = pair.id,
        a = %pair.a_id,
        b = %pair.b_id,
        retired = retire.len(),
        detail = detail.unwrap_or("none given"),
        "retired both sides of a pair that states nothing"
    );
    // `set_pair_state` writes `detail` unconditionally, so `None` would null the reason.
    settle_as(core, pair, PairState::Dismissed, detail, by).await
}

/// Close the question with both sides as they are; the reason is kept on the row.
async fn leave_both(core: &Core, p: &ArtifactPair, why: &str) -> Result<()> {
    if p.synthesis_asked {
        core.store.clear_pair_synthesis(p.id).await?;
    }
    settle(core, p, PairState::NoConflict, Some(why)).await
}

async fn settle(
    core: &Core,
    pair: &ArtifactPair,
    state: PairState,
    detail: Option<&str>,
) -> Result<()> {
    settle_as(core, pair, state, detail, DecidedBy::Model).await
}

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
                    text: "Spuren sind materielle Veraenderungen.".into(),
                    segment_idx: Some(0),
                    ..Default::default()
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

    #[tokio::test]
    async fn a_pair_a_merge_could_not_write_is_left_as_it_is_before_the_call() {
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
            PairState::NoConflict,
            "the pair was left for `arm_dedupe` to buy again, or for a person"
        );
        for id in [&synth, &other] {
            assert!(core.store.get_artifact(id).await.unwrap().in_results());
        }
    }

    #[tokio::test]
    async fn a_pair_naming_a_passage_is_left_as_it_is_before_the_call() {
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
            PairState::NoConflict
        );
    }

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
        let rows = core
            .store
            .open_actions(&[crate::store::actions::Kind::Discard], 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.survivor_id.is_none()));
        assert!(rows.iter().all(|r| ids.contains(&r.subject_id)));
    }

    #[tokio::test]
    async fn a_discard_by_a_person_journals_nothing() {
        let core = test_core().await;
        let ids = seed(&core, &[("a", [1.0, 0.0]), ("b", [0.93, 0.37])]).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        let row = core.store.get_pair(pair).await.unwrap();

        discard_both(&core, &row, Some("mine"), DecidedBy::Operator)
            .await
            .unwrap();

        assert!(
            core.store
                .open_actions(&[crate::store::actions::Kind::Discard], 10)
                .await
                .unwrap()
                .is_empty(),
            "a person's press is theirs, not the base's"
        );
    }

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
        // The row alone: `Core::supersede` would move the loser's open pairs onto the winner.
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

    #[tokio::test]
    async fn discarding_a_pair_leaves_both_when_others_are_hidden_behind_a_side() {
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
        let p = core.store.get_pair(pid).await.unwrap();
        assert_eq!(
            p.state,
            PairState::NoConflict,
            "a discard nothing can carry out was recorded as carried out, or handed over"
        );
        assert!(
            p.detail
                .as_deref()
                .unwrap_or_default()
                .contains("both stay as they are"),
            "{:?}",
            p.detail
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

    async fn disagreeing_commands(core: &Core) -> Vec<String> {
        seed(
            core,
            &[
                (
                    "Mount the image with `mount -o ro /dev/loop0 /mnt/case`.",
                    [1.0, 0.0],
                ),
                (
                    "Mount the image with `mount -o rw /dev/loop0 /mnt/case`.",
                    [0.93, 0.37],
                ),
            ],
        )
        .await
    }

    #[tokio::test]
    async fn two_artifacts_about_different_subjects_are_never_merged() {
        // FAT12/16/32 sit at 0.91 and every number differs.
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

        core.reactivate(&winner).await.unwrap();
        assert_eq!(
            core.store.get_pair(pair_id).await.unwrap().state,
            PairState::Pending,
            "reactivating the survivor left the duplicate buried"
        );
    }

    #[tokio::test]
    async fn a_synthesis_asked_for_a_pair_answered_since_writes_nothing() {
        let core = test_core().await;
        let ids = disagreeing(&core).await;
        let pair_id = queue_pair(&core, &ids[0], &ids[1]).await;
        assert!(core.store.ask_pair_synthesis(pair_id).await.unwrap());
        core.store
            .set_pair_state(pair_id, PairState::Dismissed, None, DecidedBy::Operator)
            .await
            .unwrap();

        run(&core, &pair_id.to_string()).await.unwrap();

        let p = core.store.get_pair(pair_id).await.unwrap();
        assert!(p.merged_into.is_none(), "a dismissed pair was merged");
        assert!(!p.synthesis_asked, "the card still promises a synthesis");
        assert_eq!(p.state, PairState::Dismissed);
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().in_results());
        }
    }

    #[tokio::test]
    async fn a_spent_budget_leaves_the_pair_pending_without_spending_the_judge() {
        let mut core = test_core().await;
        let judge = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"replaced","obsolete":"A","detail":"older"}"#.into(),
        ]));
        core.judge = Some(judge.clone());
        core.evolve.autonomous = crate::config::Autonomy::Full;
        core.evolve.max_actions_per_day = 0;
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        run(&core, &pair.to_string()).await.unwrap();
        assert_eq!(judge.calls(), 0, "the budget is read before the call");
        assert_eq!(
            core.store.get_pair(pair).await.unwrap().state,
            PairState::Pending
        );
        core.evolve.autonomous = crate::config::Autonomy::Ranking;
        run(&core, &pair.to_string()).await.unwrap();
        assert_eq!(judge.calls(), 1);
    }

    #[tokio::test]
    async fn a_value_conflict_is_kept_as_a_disagreement_and_never_merged() {
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
    async fn a_replacement_journals_the_loser_naming_the_winner() {
        use crate::store::actions::Kind;
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"replaced","supersedes":"a","detail":"old flag vs new flag"}"#.into(),
        ])));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let rows = core
            .store
            .open_actions(&[Kind::Supersede], 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].subject_id, ids[0]);
        assert_eq!(rows[0].survivor_id.as_deref(), Some(ids[1].as_str()));
        assert_eq!(rows[0].detail.as_deref(), Some("old flag vs new flag"));
        assert!(rows[0].pair_score.is_some());
    }

    #[tokio::test]
    async fn an_action_taken_back_is_not_taken_again_on_the_same_subject() {
        use crate::store::actions::{Job, Kind, NewAction, UndoneBy};
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"replaced","supersedes":"a","detail":"old flag vs new flag"}"#.into(),
        ])));
        let ids = disagreeing(&core).await;
        core.store
            .record_action(&NewAction {
                job: Job::Dedupe,
                kind: Kind::Supersede,
                subject_id: ids[0].clone(),
                survivor_id: Some(ids[1].clone()),
                detail: None,
                evidence: serde_json::json!({}),
                pair_score: None,
            })
            .await
            .unwrap();
        core.store
            .undo_action_on(&ids[0], Kind::Supersede, UndoneBy::Operator, "put back")
            .await
            .unwrap();
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert_eq!(
            p.state,
            PairState::NoConflict,
            "left as it is, neither repeated nor handed to a person"
        );
        assert_eq!(p.detail.as_deref(), Some(TAKEN_BACK));
        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .is_none()
        );
        assert!(
            core.store
                .open_actions(&[Kind::Supersede], 10)
                .await
                .unwrap()
                .is_empty(),
            "no new row for a verdict that was not applied"
        );
    }

    #[tokio::test]
    async fn a_plain_replacement_supersedes_rather_than_merging() {
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
        let mut core = test_core().await;
        let ids = disagreeing(&core).await;
        // `now()` is second-grained; a tie passes the guard, so force b strictly newer.
        sqlx::query!(
            "UPDATE artifacts SET created_at = created_at + 100 WHERE id = ?",
            &ids[1]
        )
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
    async fn a_merge_that_would_lose_a_literal_says_so_without_naming_the_tokens() {
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"duplicate","detail":"same claim",
                "merged":{"text":"Mount the image with `mount -o rw /dev/loop0 /mnt/case`.","tags":[],"caveats":[]}}"#
                .into(),
        ])));
        let ids = disagreeing_commands(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let found = core
            .store
            .pairs_by_state(PairState::NoConflict, 10)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        let detail = found[0].detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("dropped"),
            "the row says nothing about why it was closed: {detail:?}"
        );
        assert!(
            !detail.contains("same claim"),
            "the judge's line said these two were the same, not why the pair \
             was closed: {detail:?}"
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
        let mut core = test_core().await;
        let completer = Arc::new(ScriptedCompleter::new(vec![]));
        core.judge = Some(completer.clone());
        let ids = disagreeing(&core).await;
        let m = core
            .store
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    text: "a paraphrase".into(),
                    ..Default::default()
                },
                &[ids[0].clone()],
            )
            .await
            .unwrap();
        sqlx::query!("DELETE FROM artifact_sources WHERE child_id = ?", &m.id)
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
            PairState::NoConflict,
            "both stay as they are rather than being judged on a paraphrase"
        );
    }

    #[tokio::test]
    async fn an_applied_replacement_does_not_wait_for_an_operator() {
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
        let m = merged_from(
            &core,
            "Merged",
            "the pool holds eight, and unrelated text",
            &[ids[0].clone(), ids[1].clone()],
        )
        .await;
        let pair = queue_pair(&core, &m, &ids[2]).await;
        // `record_pair` stores sides in id order, so the letter is not the caller's to choose.
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
        // This merge already dropped "30s".
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
        assert!(
            core.store
                .get_pair(pair)
                .await
                .unwrap()
                .merged_into
                .is_some(),
            "the merge was not written"
        );
    }

    #[tokio::test]
    async fn a_pair_a_person_asked_about_is_written_without_being_judged_again() {
        use crate::store::actions::Kind;
        let mut core = test_core().await;
        // A verdict-shaped reply would not parse here: falling through to the judge fails the test.
        core.pair_synthesizer = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"merged":{"title":"Tierarztpraxis Dr. Brinkmann","text":"Kleintierpraxis in Bad Aibling. Sprechzeiten Mo-Fr 08:00-12:00.","category":"reference","caveats":[]}}"#
                .into(),
        ])));
        let ids = seed_titled(
            &core,
            &[
                ("Praxis", "Kleintierpraxis in Bad Aibling", [1.0, 0.0]),
                ("Leistungen", "Sprechzeiten Mo-Fr 08:00-12:00", [0.93, 0.37]),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        core.store.ask_pair_synthesis(pair).await.unwrap();

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        let merged = p.merged_into.expect("a merged artifact answered the pair");
        let m = core.store.get_artifact(&merged).await.unwrap();
        assert_eq!(m.provenance, Provenance::Merged);
        assert_eq!(
            p.decided_by,
            Some(DecidedBy::Operator),
            "the press is what settled this, not the model"
        );
        // `merge::finish` hides the sources only when the embed lands.
        let roots = core
            .store
            .roots_of(std::slice::from_ref(&merged))
            .await
            .unwrap();
        assert_eq!(
            roots[&merged].len(),
            2,
            "the synthesis records both originals as its roots"
        );
        let rows = core.store.open_actions(&[Kind::Merge], 10).await.unwrap();
        assert_eq!(rows.len(), 2, "one row per original, as any merge journals");
    }

    #[tokio::test]
    async fn a_settled_duplicate_is_written_when_a_person_presses_synthese() {
        let mut core = test_core().await;
        core.pair_synthesizer = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"merged":{"title":"Tierarztpraxis","text":"Kleintierpraxis in Bad Aibling. Sprechzeiten Mo-Fr 08:00-12:00.","category":"reference","caveats":[]}}"#
                .into(),
        ])));
        let ids = seed_titled(
            &core,
            &[
                ("Praxis", "Kleintierpraxis in Bad Aibling", [1.0, 0.0]),
                ("Leistungen", "Sprechzeiten Mo-Fr 08:00-12:00", [0.93, 0.37]),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        core.store
            .set_pair_state(
                pair,
                PairState::Duplicate,
                Some("these two cover the same ground"),
                DecidedBy::Model,
            )
            .await
            .unwrap();
        core.store.ask_pair_synthesis(pair).await.unwrap();

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert!(
            p.merged_into.is_some(),
            "the press on a settled duplicate wrote nothing: {p:?}"
        );
        assert_eq!(p.decided_by, Some(DecidedBy::Operator));
    }

    #[tokio::test]
    async fn a_settled_pair_nobody_pressed_is_still_left_alone() {
        let mut core = test_core().await;
        // Empty scripts: any call errors and the `unwrap` fails.
        core.pair_synthesizer = Some(Arc::new(ScriptedCompleter::new(vec![])));
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![])));
        let ids = seed_titled(
            &core,
            &[
                ("Praxis", "Kleintierpraxis in Bad Aibling", [1.0, 0.0]),
                ("Leistungen", "Sprechzeiten Mo-Fr 08:00-12:00", [0.93, 0.37]),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        core.store
            .set_pair_state(pair, PairState::Duplicate, None, DecidedBy::Model)
            .await
            .unwrap();

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert!(p.merged_into.is_none(), "nothing was pressed");
        assert_eq!(p.state, PairState::Duplicate, "and nothing was resettled");
    }

    #[tokio::test]
    async fn a_synthesis_that_would_drop_a_literal_is_refused_and_both_stay() {
        let mut core = test_core().await;
        core.pair_synthesizer = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"merged":{"title":"Loop mounts","text":"Mount the image read-only with `mount -o ro image.dd /mnt`.","category":"reference","caveats":[]}}"#
                .into(),
        ])));
        let ids = seed_titled(
            &core,
            &[
                ("Read-only", "use `mount -o ro image.dd /mnt`", [1.0, 0.0]),
                ("Writable", "use `mount -o rw image.dd /mnt`", [0.93, 0.37]),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        core.store.ask_pair_synthesis(pair).await.unwrap();

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert!(
            p.merged_into.is_none(),
            "a draft that drops a command was written anyway"
        );
        assert_eq!(
            p.state,
            PairState::NoConflict,
            "closed with both left as they are, not handed to a person"
        );
        assert!(!p.synthesis_asked, "the ask is cleared");
        for id in &ids {
            assert!(
                core.store.get_artifact(id).await.unwrap().in_results(),
                "both sides stay readable"
            );
        }
    }

    #[tokio::test]
    async fn an_unreadable_synthesis_reply_is_counted_against_the_pair() {
        let mut core = test_core().await;
        core.pair_synthesizer = Some(Arc::new(ScriptedCompleter::new(vec![
            "not json at all".into(),
        ])));
        let ids = seed_titled(
            &core,
            &[
                ("Praxis", "Kleintierpraxis in Bad Aibling", [1.0, 0.0]),
                ("Leistungen", "Sprechzeiten Mo-Fr 08:00-12:00", [0.93, 0.37]),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        core.store.ask_pair_synthesis(pair).await.unwrap();

        run(&core, &pair.to_string())
            .await
            .expect_err("an unreadable reply is an error");

        let p = core.store.get_pair(pair).await.unwrap();
        assert_eq!(p.judge_unreadable, 1, "the pair carries the cost");
        assert!(p.synthesis_asked, "the press is not lost to one bad reply");
    }

    #[tokio::test]
    async fn a_pair_whose_replies_never_parse_leaves_the_synthesis_queue() {
        let core = test_core().await;
        let ids = seed_titled(
            &core,
            &[
                ("Praxis", "Kleintierpraxis in Bad Aibling", [1.0, 0.0]),
                ("Leistungen", "Sprechzeiten Mo-Fr 08:00-12:00", [0.93, 0.37]),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        core.store.ask_pair_synthesis(pair).await.unwrap();
        assert_eq!(
            core.store.pairs_awaiting_synthesis(10).await.unwrap().len(),
            1
        );
        for _ in 0..crate::store::pairs::MAX_UNREADABLE_JUDGEMENTS {
            core.store.record_unreadable_judgement(pair).await.unwrap();
        }
        assert!(
            core.store
                .pairs_awaiting_synthesis(10)
                .await
                .unwrap()
                .is_empty(),
            "a pair nothing can write must stop buying model calls"
        );
    }

    #[tokio::test]
    async fn a_refused_synthesis_clears_the_ask() {
        let core = test_core().await;
        let src = core
            .store
            .insert_corpus("skript", "web", None)
            .await
            .unwrap();
        let raw: Vec<crate::store::artifacts::NewArtifact> =
            ["Spuren sind materiell.", "Spuren sind Veraenderungen."]
                .iter()
                .enumerate()
                .map(|(i, t)| crate::store::artifacts::NewArtifact {
                    ordinal: i as i64,
                    text: (*t).to_string(),
                    segment_idx: Some(0),
                    ..Default::default()
                })
                .collect();
        let passages: Vec<String> = core
            .store
            .insert_artifacts_with_provenance(&src.id, &raw, Provenance::Passage)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        let pair = queue_pair(&core, &passages[0], &passages[1]).await;
        core.store.ask_pair_synthesis(pair).await.unwrap();

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert!(p.merged_into.is_none(), "nothing was written");
        assert!(
            !p.synthesis_asked,
            "the card stops promising what will not come"
        );
        assert_eq!(p.state, PairState::NoConflict, "both stay as they are");
    }

    #[tokio::test]
    async fn a_duplicate_verdict_writes_the_merge_and_journals_it() {
        use crate::store::actions::Kind;
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"duplicate","detail":"same thing",
                "merged":{"title":"Pool","text":"the pool holds sixteen connections","tags":[],"caveats":[]}}"#
                .into(),
        ])));
        let ids = seed_titled(
            &core,
            &[
                ("Pool sizing", "sixteen connections", [1.0, 0.0]),
                ("Connections", "sixteen connections", [0.93, 0.37]),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert!(p.merged_into.is_some(), "the merge was written");
        assert_eq!(p.decided_by, Some(DecidedBy::Model));
        let journal = core.store.open_actions(&[Kind::Merge], 10).await.unwrap();
        assert_eq!(journal.len(), 2, "one row per source");
        assert!(journal.iter().all(|a| {
            let evidence: serde_json::Value = serde_json::from_str(&a.evidence_json).unwrap();
            evidence.get("asked_by").is_none()
        }));
    }

    #[tokio::test]
    async fn a_verdict_repeated_over_an_undo_leaves_both_and_asks_nobody() {
        use crate::store::actions::{Job, Kind, NewAction, UndoneBy};
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"replaced","supersedes":"a","detail":"old flag vs new flag"}"#.into(),
        ])));
        let ids = disagreeing(&core).await;
        core.store
            .record_action(&NewAction {
                job: Job::Dedupe,
                kind: Kind::Supersede,
                subject_id: ids[0].clone(),
                survivor_id: Some(ids[1].clone()),
                detail: None,
                evidence: serde_json::json!({}),
                pair_score: None,
            })
            .await
            .unwrap();
        core.store
            .undo_action_on(&ids[0], Kind::Supersede, UndoneBy::Operator, "put back")
            .await
            .unwrap();
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert_eq!(p.state, PairState::NoConflict);
        assert!(
            !crate::store::pairs::AWAITING_REVIEW.contains(&p.state),
            "the pair still waits on somebody"
        );
        assert!(!p.synthesis_asked);
        for id in &ids {
            assert!(
                core.store.get_artifact(id).await.unwrap().in_results(),
                "a side left results on a verdict that was not applied"
            );
        }
        assert!(
            core.store
                .open_actions(&[Kind::Supersede], 10)
                .await
                .unwrap()
                .is_empty(),
            "no new row for a verdict that was not applied"
        );
    }

    #[tokio::test]
    async fn a_merge_taken_back_is_not_written_again() {
        use crate::store::actions::{Job, Kind, NewAction, UndoneBy};
        let mut core = test_core().await;
        let judge = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"duplicate","detail":"same thing",
                "merged":{"title":"Pool","text":"the pool holds sixteen connections","tags":[],"caveats":[]}}"#
                .into(),
        ]));
        core.judge = Some(judge.clone());
        let ids = seed_titled(
            &core,
            &[
                ("Pool sizing", "sixteen connections", [1.0, 0.0]),
                ("Connections", "sixteen connections", [0.93, 0.37]),
            ],
        )
        .await;
        core.store
            .record_action(&NewAction {
                job: Job::Dedupe,
                kind: Kind::Merge,
                subject_id: ids[0].clone(),
                survivor_id: None,
                detail: None,
                evidence: serde_json::json!({}),
                pair_score: None,
            })
            .await
            .unwrap();
        core.store
            .undo_action_on(&ids[0], Kind::Merge, UndoneBy::Operator, "put back")
            .await
            .unwrap();
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(judge.calls(), 1);
        let p = core.store.get_pair(pair).await.unwrap();
        assert_eq!(p.state, PairState::NoConflict);
        assert_eq!(p.detail.as_deref(), Some(TAKEN_BACK));
        assert!(p.merged_into.is_none(), "the merge was written again");
        assert!(core.store.merged_artifacts(10).await.unwrap().is_empty());
        assert!(
            core.store
                .open_actions(&[Kind::Merge], 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_merge_that_would_lose_a_value_leaves_both_and_closes_the_pair() {
        use crate::store::actions::Kind;
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"duplicate","detail":"same claim",
                "merged":{"text":"Mount the image with `mount -o ro /dev/loop0 /mnt/case`.","tags":[],"caveats":[]}}"#
                .into(),
        ])));
        let ids = disagreeing_commands(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert_eq!(
            p.state,
            PairState::NoConflict,
            "a refused draft is not a disagreement between the notes"
        );
        assert_eq!(p.detail.as_deref(), Some(LOSS_REFUSED));
        assert!(
            core.store
                .open_contradictions(&ids)
                .await
                .unwrap()
                .is_empty(),
            "search would say two agreeing notes disagree"
        );
        assert!(
            p.merged_into.is_none(),
            "a merge that drops a value was written"
        );
        assert!(core.store.merged_artifacts(10).await.unwrap().is_empty());
        assert!(
            core.store
                .open_actions(&[Kind::Merge], 10)
                .await
                .unwrap()
                .is_empty()
        );
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().in_results());
        }
    }

    #[tokio::test]
    async fn a_duplicate_without_a_draft_leaves_both_and_arms_nothing() {
        let core = test_core().await;
        let ids = seed_titled(
            &core,
            &[
                ("Pool sizing", "sixteen connections", [1.0, 0.0]),
                ("Connections", "sixteen connections", [0.93, 0.37]),
            ],
        )
        .await;
        let pair_id = queue_pair(&core, &ids[0], &ids[1]).await;
        let pair = core.store.get_pair(pair_id).await.unwrap();
        let members = vec![
            core.store.get_artifact(&pair.a_id).await.unwrap(),
            core.store.get_artifact(&pair.b_id).await.unwrap(),
        ];

        apply(
            &core,
            Settlement {
                relation: Relation::Duplicate,
                detail: Some("same thing".into()),
                obsolete: None,
                merged: None,
                members,
                pair,
            },
        )
        .await
        .unwrap();

        let p = core.store.get_pair(pair_id).await.unwrap();
        assert_eq!(p.state, PairState::NoConflict);
        assert_eq!(p.decided_by, Some(DecidedBy::Model));
        assert!(!p.synthesis_asked, "a judge verdict was filed as a press");
        assert!(p.merged_into.is_none());
        assert!(
            core.store
                .pairs_awaiting_synthesis(10)
                .await
                .unwrap()
                .is_empty()
        );
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().in_results());
        }
    }

    #[tokio::test]
    async fn a_merge_action_records_the_act_and_not_a_verdict() {
        use crate::store::actions::Kind;
        let mut core = test_core().await;
        core.pair_synthesizer = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"merged":{"title":"Pool","text":"the pool holds sixteen connections","category":"reference","caveats":[]}}"#
                .into(),
        ])));
        let ids = seed_titled(
            &core,
            &[
                ("Pool sizing", "sixteen connections", [1.0, 0.0]),
                ("Connections", "sixteen connections", [0.93, 0.37]),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        core.store.ask_pair_synthesis(pair).await.unwrap();

        run(&core, &pair.to_string()).await.unwrap();

        let merged = core
            .store
            .get_pair(pair)
            .await
            .unwrap()
            .merged_into
            .expect("the pair names its merge");
        let rows = core.store.open_actions(&[Kind::Merge], 10).await.unwrap();
        assert_eq!(rows.len(), 2, "one row per original");
        assert!(
            rows.iter()
                .all(|r| r.survivor_id.as_deref() == Some(merged.as_str()))
        );
        let subjects: std::collections::HashSet<String> =
            rows.iter().map(|r| r.subject_id.clone()).collect();
        assert_eq!(subjects, ids.iter().cloned().collect());
        for r in &rows {
            let d = r.detail.clone().unwrap_or_default();
            assert!(
                d.contains("merged into"),
                "the action says what happened, got {d:?}"
            );
        }
        assert!(rows[0].pair_score.is_some());
    }

    #[tokio::test]
    async fn a_merge_of_a_merge_names_every_original_behind_it() {
        let mut core = test_core().await;
        core.pair_synthesizer = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"merged":{"title":"Pool","text":"max_connections is 16, raise it for batch jobs, sixteen connections","category":"reference","caveats":[]}}"#
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
        core.store.ask_pair_synthesis(pair).await.unwrap();

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
        // Too small for the source block, large enough for the rest; moves with `DEDUPE_SYSTEM`.
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
    async fn two_members_that_alone_do_not_fit_are_left_as_they_are() {
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
        let p = core.store.get_pair(pair).await.unwrap();
        assert_eq!(p.state, PairState::NoConflict);
        assert!(
            p.detail.as_deref().unwrap_or("").contains("do not fit"),
            "the reason both were left as they are was not recorded"
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
