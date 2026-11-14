//! The pair decision: `pair_relation` through the judgement layer; `PairApplier` carries out the
//! pick, and a duplicate asks `Stage::Merge` to write. A merged member shows its roots as context,
//! dropped oldest-first; a value conflict is kept as a disagreement, never merged.

use crate::core::Core;
use crate::core::judging::band::{Band, era};
use crate::core::judging::{self, Applier, Subjects, catalogue, state};
use crate::error::{Error, Result};
use crate::infer::prompt::MergedDraft;
use crate::store::actions::{Job, Kind, NewAction};
use crate::store::artifacts::{Chunk, Provenance};
use crate::store::decision_log::Logged;
use crate::store::jobs::Stage;
use crate::store::pairs::{ArtifactPair, DecidedBy, PairState};
use async_trait::async_trait;

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

/// A row a decision wrote names it; a person's is marked theirs, as `Store::band_record` reads.
fn decided(mut row: NewAction, by: DecidedBy, decision_id: &str) -> NewAction {
    if let Some(evidence) = row.evidence.as_object_mut() {
        evidence.insert("decision".into(), serde_json::json!(decision_id));
        if by == DecidedBy::Operator {
            evidence.insert("asked_by".into(), serde_json::json!("operator"));
        }
    }
    row
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
const LOSS_REFUSED: &str = "every draft dropped a value one of them states; both stay as they are";

const TAKEN_BACK: &str =
    "This was done to one of these before and taken back, so both stay as they are.";

/// Older bases' wording for the same refusal; the drain reads it.
#[cfg(test)]
pub(crate) const TAKEN_BACK_OLD: &str =
    "This was done to one of these before and taken back. Resolve by hand.";

/// A pair's members in letter order, the roots of each, and those roots.
struct Members {
    members: Vec<Chunk>,
    root_map: std::collections::BTreeMap<String, Vec<String>>,
    roots: Vec<Chunk>,
}

/// `None` when the pair was settled here because no answer about it could be carried out.
async fn members_of(core: &Core, p: &ArtifactPair) -> Result<Option<Members>> {
    // Pairs cascade on artifact delete, so a missing artifact is a store failure, not a deletion.
    let a = core.store.get_artifact(&p.a_id).await?;
    let b = core.store.get_artifact(&p.b_id).await?;
    // Re-checked: a member can leave results while this waits out a backoff.
    if !a.in_results() || !b.in_results() {
        // A cleared flag stops the card promising a synthesis nothing will write.
        if p.synthesis_asked {
            core.store.clear_pair_synthesis(p.id).await?;
        }
        settle(
            core,
            p,
            PairState::Dismissed,
            Some("a member is no longer in results"),
        )
        .await?;
        return Ok(None);
    }

    let members = vec![a, b];
    let member_ids: Vec<String> = members.iter().map(|c| c.id.clone()).collect();
    let root_map = core.store.roots_of(&member_ids).await?;
    // No roots: a merge whose sources were deleted, a paraphrase with nothing behind it.
    if members
        .iter()
        .any(|c| root_map.get(&c.id).is_none_or(|r| r.is_empty()))
    {
        leave_both(
            core,
            p,
            "a merged member has lost its sources; both stay as they are",
        )
        .await?;
        return Ok(None);
    }
    let one_contains_the_other = root_map[&members[0].id].contains(&members[1].id)
        || root_map[&members[1].id].contains(&members[0].id);
    if one_contains_the_other {
        settle(
            core,
            p,
            PairState::Dismissed,
            Some("one of these is a source of the other"),
        )
        .await?;
        return Ok(None);
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
        leave_both(
            core,
            p,
            "These cannot be written as one: what one of them is made of is stored source \
             text, which a merge must not rewrite, so both stay as they are.",
        )
        .await?;
        return Ok(None);
    }
    Ok(Some(Members {
        members,
        root_map,
        roots,
    }))
}

fn day(at: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(at, 0).map(|d| d.format("%Y-%m-%d").to_string())
}

fn note(c: &Chunk) -> state::Note<'_> {
    state::Note {
        title: c.title.as_deref(),
        day: day(c.created_at),
        text: &c.text,
    }
}

fn pair_era(core: &Core) -> String {
    era(core.decider.model(), catalogue::pair_relation().version)
}

/// Asked in this era about the members as they are now, and not put back since.
async fn asked_already(core: &Core, p: &ArtifactPair, a: &Chunk, b: &Chunk) -> Result<bool> {
    let last = core
        .store
        .last_logged("pair_relation", &p.id.to_string(), None, &pair_era(core))
        .await?;
    Ok(last.is_some_and(|d| {
        d.rev_a == Some(a.embed_rev)
            && d.rev_b == Some(b.embed_rev)
            && !matches!(d.reason.as_deref(), Some("asked_again" | "changed"))
    }))
}

/// `Stage::Dedupe`: ask `pair_relation` about one pair and settle the answer.
pub async fn run(core: &Core, pair_id: &str) -> Result<()> {
    let id: i64 = pair_id.parse().map_err(|_| Error::NotFound)?;
    let p = core.store.get_pair(id).await?;
    // A pair asked for writing is the merge unit's, not a question any more.
    if p.state != PairState::Pending || p.synthesis_asked {
        return Ok(());
    }
    let Some(Members {
        members,
        root_map,
        roots,
    }) = members_of(core, &p).await?
    else {
        return Ok(());
    };
    let (a, b) = (&members[0], &members[1]);
    // A retried or second unit must not ask what is already answered.
    if asked_already(core, &p, a, b).await? {
        return Ok(());
    }

    // Context, oldest first: only a merge shows what it was made of.
    let sources = |c: &Chunk| -> Vec<&Chunk> {
        if !c.provenance.is_model_written() {
            return vec![];
        }
        let mut v: Vec<&Chunk> = roots
            .iter()
            .filter(|r| root_map.get(&c.id).is_some_and(|ids| ids.contains(&r.id)))
            .collect();
        v.sort_by_key(|r| r.created_at);
        v
    };
    let sa: Vec<state::Note> = sources(a).into_iter().map(note).collect();
    let sb: Vec<state::Note> = sources(b).into_iter().map(note).collect();
    let asked = catalogue::pair_relation();
    let room = state::room(core, &[&asked]);
    let s = state::pair(&note(a), &note(b), &sa, &sb, &core.counter, room);
    if s.is_some() {
        // Before the call, so a pair the decider keeps failing on drops behind the queue.
        core.store.record_judge_attempt(p.id).await?;
    }
    let subjects = Subjects {
        kind: "pair",
        a: p.id.to_string(),
        b: None,
        rev_a: Some(a.embed_rev),
        rev_b: Some(b.embed_rev),
    };
    let d = judging::ask(core, &asked, subjects, s.as_deref()).await?;
    // Nothing can write a merge: a sure duplicate is a person's to resolve, keeping one.
    let d = if d.band == Band::Act && d.pick == "duplicate" && core.pair_synthesizer.is_none() {
        judging::demote_to_ask(core, d, "no_writer").await?
    } else {
        d
    };
    judging::settle(core, d).await?;
    Ok(())
}

/// `Stage::Merge`: write the merge a decision or a person asked for. The writing moves to its own
/// file with decision linking next; the early-outs are those the dedupe unit had for an ask.
pub async fn merge(core: &Core, pair_id: &str) -> Result<()> {
    let id: i64 = pair_id.parse().map_err(|_| Error::NotFound)?;
    let p = core.store.get_pair(id).await?;
    if !p.synthesis_asked {
        return Ok(());
    }
    if p.merged_into.is_some() {
        // Already written; re-running would merge the pair twice.
        core.store.clear_pair_synthesis(p.id).await?;
        return Ok(());
    }
    // The pair outlived its ask: writing would take the ask as the verdict without re-reading.
    if !crate::store::pairs::AWAITING_REVIEW.contains(&p.state) {
        core.store.clear_pair_synthesis(p.id).await?;
        return Ok(());
    }
    if !core.may_act(Job::Dedupe).await? {
        tracing::info!(
            pair = id,
            "budget spent; the merge waits for the window to move"
        );
        return Ok(());
    }
    let Some(m) = members_of(core, &p).await? else {
        return Ok(());
    };
    synthesize_asked_pair(core, &p, m.members).await
}

/// What a pick does to a pair, guards included. Returns the journal rows written.
pub(crate) async fn carry_out(
    core: &Core,
    pair: &ArtifactPair,
    pick: &str,
    by: DecidedBy,
    decision_id: &str,
) -> Result<Vec<String>> {
    // The guards stand between the model and the base; a person's verdict wins.
    let model = by == DecidedBy::Model;
    match pick {
        "distinct" => {
            settle_as(core, pair, PairState::NoConflict, None, by).await?;
            Ok(vec![])
        }
        "conflict" => {
            tracing::info!(
                pair = pair.id,
                "artifacts disagree; kept as a disagreement rather than merged"
            );
            settle_as(core, pair, PairState::Contradiction, None, by).await?;
            Ok(vec![])
        }
        // Both sides stay one press from active, readable under `include_deprecated`.
        "vacuous" => discard_both(core, pair, None, by, Some(decision_id)).await,
        "a_replaces_b" | "b_replaces_a" | "keep_a" | "keep_b" => {
            let (obsolete, winner) = if matches!(pick, "a_replaces_b" | "keep_a") {
                (&pair.b_id, &pair.a_id)
            } else {
                (&pair.a_id, &pair.b_id)
            };
            // Newest wins: a direction is trusted only when the note it retires is the older;
            // otherwise it is a disagreement.
            if model {
                let older = core.store.get_artifact(obsolete).await?.created_at
                    <= core.store.get_artifact(winner).await?.created_at;
                if !older {
                    settle_as(core, pair, PairState::Contradiction, None, by).await?;
                    return Ok(vec![]);
                }
            }
            replace(core, pair, obsolete, winner, by, decision_id).await
        }
        // The writer writes, checks for losses and journals; this only asks it to.
        "duplicate" => {
            if model && taken_back_before(core, Kind::Merge, &[&pair.a_id, &pair.b_id]).await? {
                leave_both(core, pair, TAKEN_BACK).await?;
                return Ok(vec![]);
            }
            // Again on a retry: the ask is idempotent while the pair is open.
            if core.store.ask_pair_synthesis_by(pair.id, by).await? {
                core.store
                    .rearm_idle_seq(Stage::Merge, "pair", &pair.id.to_string(), 0)
                    .await?;
            }
            Ok(vec![])
        }
        other => Err(Error::Validation(format!(
            "\"{other}\" is not an answer about a pair"
        ))),
    }
}

/// Retire `obsolete` behind `winner`, after the checks of today.
async fn replace(
    core: &Core,
    pair: &ArtifactPair,
    obsolete: &str,
    winner: &str,
    by: DecidedBy,
    decision_id: &str,
) -> Result<Vec<String>> {
    // Fresh status: either side can be retired while the decision waits.
    let live = |id: String| async move {
        match core.store.get_artifact(&id).await {
            Ok(c) => Ok(c.in_results()),
            Err(Error::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    };
    // `Core::supersede` refuses an inactive winner, which would fail the decision for ever.
    if !live(winner.to_string()).await? {
        // `Stale`, not `Dismissed`: `record_pair` is `INSERT OR IGNORE`, so `Dismissed` buries it
        // for good.
        settle_as(
            core,
            pair,
            PairState::Stale,
            Some("the surviving artifact left results before this could be applied"),
            by,
        )
        .await?;
        return Ok(vec![]);
    }
    if !live(obsolete.to_string()).await? {
        settle_as(
            core,
            pair,
            PairState::NoConflict,
            Some("the named replacement is already out of results"),
            by,
        )
        .await?;
        return Ok(vec![]);
    }
    if by == DecidedBy::Model && taken_back_before(core, Kind::Supersede, &[obsolete]).await? {
        leave_both(core, pair, TAKEN_BACK).await?;
        return Ok(vec![]);
    }
    // Side effect first: a pair settled first would never be applied.
    let row = core
        .supersede_with(
            obsolete,
            winner,
            Some(decided(
                action(Kind::Supersede, pair, obsolete, Some(winner), None),
                by,
                decision_id,
            )),
        )
        .await?;
    tracing::info!(superseded = %obsolete, by = %winner, "applied a replacement");
    settle_as(core, pair, PairState::Dismissed, None, by).await?;
    Ok(row.into_iter().collect())
}

/// What `pair_relation`'s answers do to a pair.
pub struct PairApplier;

async fn pair_of(core: &Core, d: &Logged) -> Result<Option<ArtifactPair>> {
    let Ok(id) = d.subject_a.parse::<i64>() else {
        return Ok(None);
    };
    match core.store.get_pair(id).await {
        Ok(p) => Ok(Some(p)),
        Err(Error::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

#[async_trait]
impl Applier for PairApplier {
    fn question_id(&self) -> &'static str {
        "pair_relation"
    }

    fn label(&self) -> &'static str {
        "Same thing?"
    }

    fn job(&self) -> Option<Job> {
        Some(Job::Dedupe)
    }

    async fn buttons(&self, core: &Core, _: &Logged) -> Result<Vec<(String, String)>> {
        // With nothing to write a merge, "the same" is answered by keeping one.
        let mut b = match core.pair_synthesizer {
            Some(_) => vec![("duplicate", "Duplicate")],
            None => vec![("keep_a", "Keep A"), ("keep_b", "Keep B")],
        };
        b.extend([
            ("b_replaces_a", "Replaced ←"),
            ("a_replaces_b", "Replaced →"),
            ("conflict", "Conflict"),
            ("distinct", "Distinct"),
        ]);
        Ok(b.into_iter()
            .map(|(p, l)| (p.to_string(), l.to_string()))
            .collect())
    }

    async fn subjects(&self, core: &Core, d: &Logged) -> Result<Vec<(String, String)>> {
        let Some(p) = pair_of(core, d).await? else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        for id in [&p.a_id, &p.b_id] {
            match core.store.get_artifact(id).await {
                Ok(c) => out.push((c.id.clone(), crate::web::ui::row_label(&c).text)),
                Err(Error::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    async fn unchanged(&self, core: &Core, d: &Logged) -> Result<bool> {
        let Some(p) = pair_of(core, d).await? else {
            return Ok(false);
        };
        for (id, rev) in [(&p.a_id, d.rev_a), (&p.b_id, d.rev_b)] {
            let c = match core.store.get_artifact(id).await {
                Ok(c) => c,
                Err(Error::NotFound) => return Ok(false),
                Err(e) => return Err(e),
            };
            if !c.in_results() || Some(c.embed_rev) != rev {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn apply(
        &self,
        core: &Core,
        d: &Logged,
        pick: &str,
        by: DecidedBy,
    ) -> Result<Vec<String>> {
        let Some(p) = pair_of(core, d).await? else {
            return Ok(vec![]);
        };
        // Settled since it was asked, by a person or the sweep: nothing is left to do.
        if p.state != PairState::Pending {
            return Ok(vec![]);
        }
        carry_out(core, &p, pick, by, &d.id).await
    }

    async fn reopen(&self, core: &Core, d: &Logged) -> Result<()> {
        let Some(p) = pair_of(core, d).await? else {
            return Ok(());
        };
        // Asked again means asked, not written: a standing ask would keep it from the question.
        if p.synthesis_asked {
            core.store.clear_pair_synthesis(p.id).await?;
        }
        core.store.reopen_pair(p.id).await
    }
}

/// Answers an ask for writing, a decision's or a person's; the tail is `write_merge` so both kinds
/// of merge leave the base in one shape.
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

    // `merge::write` validates nothing, so the loss check is the caller's. Against the members, not
    // their roots: roots would freeze a lineage on a value an earlier merge dropped.
    let lost = crate::jobs::merge::losses(&members, &draft);
    if !lost.is_empty() {
        tracing::warn!(
            pair = p.id,
            lost = lost.join(", "),
            "refused a synthesis that would have dropped these"
        );
        return leave_both(core, p, LOSS_REFUSED).await;
    }

    let sources: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
    // An ask from before `synthesis_by` was a person's press.
    let by = p.synthesis_by.unwrap_or(DecidedBy::Operator);
    write_merge(core, p, &sources, &draft, by).await
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

/// For two artifacts that state nothing. `by` is a parameter so a person's press is not recorded as
/// the model's. The journal rows name `decision_id`; returns them.
pub(crate) async fn discard_both(
    core: &Core,
    pair: &ArtifactPair,
    detail: Option<&str>,
    by: DecidedBy,
    decision_id: Option<&str>,
) -> Result<Vec<String>> {
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
        leave_both(core, pair, TAKEN_BACK).await?;
        return Ok(vec![]);
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
            match by {
                DecidedBy::Model => leave_both(core, pair, why).await?,
                _ => {
                    if pair.synthesis_asked {
                        core.store.clear_pair_synthesis(pair.id).await?;
                    }
                    settle_as(core, pair, PairState::NoConflict, Some(why), by).await?
                }
            }
            return Ok(vec![]);
        }
    }
    // Side effects first, then the pair: a failure part-way leaves it answerable.
    let mut rows = Vec::new();
    for id in &retire {
        // The journal is what the base did to itself; a person's press is not journaled.
        let journal = (by == DecidedBy::Model).then(|| {
            let row = action(Kind::Discard, pair, id, None, detail);
            match decision_id {
                Some(d) => decided(row, by, d),
                None => row,
            }
        });
        rows.extend(core.deprecate_with(id, journal).await?);
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
    settle_as(core, pair, PairState::Dismissed, detail, by).await?;
    Ok(rows)
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
pub(crate) mod tests {
    use super::*;
    use crate::core::judging::band::{Band, era};
    use crate::core::judging::catalogue::pair_relation;
    use crate::core::test_support::test_core;
    use crate::infer::decide::{Answer, Answers, Kind as QKind, Question};
    use crate::infer::fake::{RuleDecider, ScriptedCompleter};
    use crate::jobs::consolidate::tests::{seed, seed_titled};
    use crate::store::artifacts::ArtifactStatus;
    use crate::store::decision_log::{Logged, Outcome, State};
    use crate::store::jobs::Stage;
    use std::sync::Arc;

    /// Every choice answered `pick` at `top`; the reversed read picks `reversed` when given.
    fn rule(
        pick: &str,
        top: f64,
        reversed: Option<&str>,
    ) -> impl Fn(&str, &[Question]) -> Result<Answers> + Send + Sync + 'static {
        let (pick, reversed) = (pick.to_string(), reversed.map(str::to_string));
        move |_, qs| {
            let mut answers = std::collections::HashMap::new();
            for q in qs {
                let QKind::Choice { options } = &q.kind else {
                    return Err(Error::Validation("a choice only".into()));
                };
                let rest = (1.0 - top) / (options.len() - 1) as f64;
                let said = match &reversed {
                    Some(r) if q.key.ends_with('r') => r.clone(),
                    _ => pick.clone(),
                };
                answers.insert(
                    q.key.clone(),
                    Answer::Choice {
                        pick: said,
                        probabilities: options
                            .iter()
                            .map(|(n, _)| if *n == pick { top } else { rest })
                            .collect(),
                        confidence: top,
                    },
                );
            }
            Ok(Answers {
                answers,
                model: "rule-decider".into(),
                input_tokens: 0,
            })
        }
    }

    /// A decider answering `pair_relation` with `pick` at `top`, both reads agreeing.
    pub(crate) fn deciding(core: &mut Core, pick: &str, top: f64) -> Arc<RuleDecider> {
        let d = Arc::new(RuleDecider::new(rule(pick, top, None)));
        core.decider = d.clone();
        d
    }

    /// A chat judge that fails the test if anything calls it.
    fn no_chat_judge(core: &mut Core) -> Arc<ScriptedCompleter> {
        let judge = Arc::new(ScriptedCompleter::new(vec![]));
        core.judge = Some(judge.clone());
        judge
    }

    fn pair_era(core: &Core) -> String {
        era(core.decider.model(), pair_relation().version)
    }

    async fn decision(core: &Core, pair: i64) -> Logged {
        core.store
            .last_logged("pair_relation", &pair.to_string(), None, &pair_era(core))
            .await
            .unwrap()
            .expect("the pair was asked")
    }

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

    async fn superseded_by(core: &Core, id: &str) -> Option<String> {
        core.store.get_artifact(id).await.unwrap().superseded_by
    }

    // ---- structural refusals, before any call ----

    #[tokio::test]
    async fn a_pair_a_merge_could_not_write_is_left_as_it_is_before_the_call() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, "distinct", 0.95);
        let (_passage, synth) = a_passage_and_a_synthesis_over_it(&core).await;
        let other = seed(&core, &[("Spuren am Material", [1.0, 0.0])])
            .await
            .remove(0);
        let pair = queue_pair(&core, &synth, &other).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(dec.calls(), 0, "a pair no merge can write was still judged");
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
        let dec = deciding(&mut core, "distinct", 0.95);
        let (passage, _synth) = a_passage_and_a_synthesis_over_it(&core).await;
        let other = seed(&core, &[("Spuren am Material", [1.0, 0.0])])
            .await
            .remove(0);
        let pair = queue_pair(&core, &passage, &other).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(dec.calls(), 0, "a passage pair was sent to the judge");
        assert_eq!(
            core.store.get_pair(pair).await.unwrap().state,
            PairState::NoConflict
        );
    }

    #[tokio::test]
    async fn a_pair_whose_member_was_retired_is_dismissed_without_a_call() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, "distinct", 0.95);
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        core.deprecate(&ids[0]).await.unwrap();

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(dec.calls(), 0, "a retired artifact cost a call");
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
        let dec = deciding(&mut core, "distinct", 0.95);
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
            dec.calls(),
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
    async fn a_merge_and_one_of_its_own_sources_is_dismissed_without_a_call() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, "distinct", 0.95);
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
            dec.calls(),
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

    // ---- discard_both ----

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
            None,
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
    async fn a_discard_by_a_person_journals_nothing() {
        let core = test_core().await;
        let ids = seed(&core, &[("a", [1.0, 0.0]), ("b", [0.93, 0.37])]).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        let row = core.store.get_pair(pair).await.unwrap();

        discard_both(&core, &row, Some("mine"), DecidedBy::Operator, None)
            .await
            .unwrap();

        assert!(
            core.store
                .open_actions(&[Kind::Discard], 10)
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
            None,
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
            None,
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

    // ---- the picks, on the decider ----

    #[tokio::test]
    async fn the_pair_is_judged_by_the_decider_and_never_by_a_chat_model() {
        let mut core = test_core().await;
        let judge = no_chat_judge(&mut core);
        let asker = Arc::new(ScriptedCompleter::new(vec![]));
        core.completer = Some(asker.clone());
        let dec = deciding(&mut core, "distinct", 0.95);
        let ids = seed(&core, &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])]).await;
        let seed_pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &seed_pair.to_string()).await.unwrap();

        assert_eq!(dec.calls(), 1, "the decider was not the one asked");
        assert_eq!(judge.calls(), 0, "a chat judge was called");
        assert_eq!(asker.calls(), 0, "the sweep called the ask model");
    }

    #[tokio::test]
    async fn two_artifacts_about_different_subjects_are_never_merged() {
        // FAT12/16/32 sit at 0.91 and every number differs.
        let mut core = test_core().await;
        deciding(&mut core, "distinct", 0.95);
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
                superseded_by(&core, id).await.is_none(),
                "a reference document's sections were consolidated into each other"
            );
        }
    }

    #[tokio::test]
    async fn a_value_conflict_is_kept_as_a_disagreement_and_never_merged() {
        let mut core = test_core().await;
        deciding(&mut core, "conflict", 0.95);
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let found = core
            .store
            .pairs_by_state(PairState::Contradiction, 10)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        // No text of the model's is stored.
        assert_eq!(found[0].detail, None);
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
    async fn a_vacuous_verdict_retires_both_sides_without_waiting_for_an_operator() {
        let mut core = test_core().await;
        deciding(&mut core, "vacuous", 0.95);
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
        // No text of the model's is stored; the decision row is the record.
        assert_eq!(pair.detail, None);
        let rows = core.store.open_actions(&[Kind::Discard], 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.survivor_id.is_none()));
        assert!(rows.iter().all(|r| ids.contains(&r.subject_id)));
        let d = decision(&core, pid).await;
        assert_eq!(
            core.store.decision_action_ids(&d.id).await.unwrap().len(),
            2
        );
    }

    #[tokio::test]
    async fn a_replacement_journals_the_loser_naming_the_winner() {
        let mut core = test_core().await;
        deciding(&mut core, "b_replaces_a", 0.95);
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
        assert_eq!(rows[0].detail, None, "no text of the model's is stored");
        assert!(rows[0].pair_score.is_some());
    }

    #[tokio::test]
    async fn a_plain_replacement_supersedes_rather_than_merging() {
        let mut core = test_core().await;
        deciding(&mut core, "b_replaces_a", 0.95);
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(
            superseded_by(&core, &ids[0]).await.as_deref(),
            Some(ids[1].as_str())
        );
        let winner = core.store.get_artifact(&ids[1]).await.unwrap();
        assert_eq!(
            winner.provenance,
            Provenance::Captured,
            "a replacement wrote synthetic text where an original would do"
        );
        assert_eq!(winner.text, "engram needs Rust 1.30.0 to build.");
    }

    #[tokio::test]
    async fn an_applied_replacement_does_not_wait_for_an_operator() {
        let mut core = test_core().await;
        deciding(&mut core, "b_replaces_a", 0.95);
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert!(
            superseded_by(&core, &ids[0]).await.is_some(),
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
        deciding(&mut core, "a_replaces_b", 0.95);
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        for id in &ids {
            assert!(superseded_by(&core, id).await.is_none());
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
    async fn a_replacement_whose_survivor_left_results_settles_rather_than_failing() {
        let core = test_core().await;
        let ids = disagreeing(&core).await;
        let pair_id = queue_pair(&core, &ids[0], &ids[1]).await;
        let pair = core.store.get_pair(pair_id).await.unwrap();
        let obsolete = pair.a_id.clone();
        let winner = pair.b_id.clone();
        // Hidden after the model was asked and before the verdict is applied.
        core.deprecate(&winner).await.unwrap();

        carry_out(&core, &pair, "b_replaces_a", DecidedBy::Model, "d")
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
    async fn an_action_taken_back_is_not_taken_again_on_the_same_subject() {
        use crate::store::actions::UndoneBy;
        let mut core = test_core().await;
        deciding(&mut core, "b_replaces_a", 0.95);
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
        assert!(!crate::store::pairs::AWAITING_REVIEW.contains(&p.state));
        assert!(!p.synthesis_asked);
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().in_results());
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
        use crate::store::actions::UndoneBy;
        let mut core = test_core().await;
        let writer = Arc::new(ScriptedCompleter::new(vec![]));
        core.pair_synthesizer = Some(writer.clone());
        let dec = deciding(&mut core, "duplicate", 0.95);
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

        assert_eq!(dec.calls(), 1);
        assert_eq!(writer.calls(), 0);
        let p = core.store.get_pair(pair).await.unwrap();
        assert_eq!(p.state, PairState::NoConflict);
        assert_eq!(p.detail.as_deref(), Some(TAKEN_BACK));
        assert!(!p.synthesis_asked, "the merge was asked for again");
        assert!(p.merged_into.is_none(), "the merge was written again");
        assert!(core.store.merged_artifacts(10).await.unwrap().is_empty());
        assert!(
            !core
                .store
                .live_job(Stage::Merge, &pair.to_string())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_second_unit_for_a_pair_already_settled_is_a_no_op() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, "distinct", 0.95);
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

        assert_eq!(dec.calls(), 1, "the second unit asked again");
    }

    #[tokio::test]
    async fn a_failed_dedupe_leaves_the_pair_pending() {
        let mut core = test_core().await;
        core.decider = Arc::new(RuleDecider::new(|_, _| {
            Err(Error::InferenceRejected {
                role: "decide",
                detail: "down".into(),
            })
        }));
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
        // Nothing logged, so it is asked again.
        let era = pair_era(&core);
        assert_eq!(core.store.pairs_to_judge(&era, 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_unit_settles_only_its_own_pair() {
        let mut core = test_core().await;
        deciding(&mut core, "distinct", 0.95);
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
        deciding(&mut core, "distinct", 0.95);
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
        // `record_pair` stores sides in id order, so the direction is not the caller's to choose.
        let stored = core.store.get_pair(pair).await.unwrap();
        let pick = if stored.a_id == ids[2] {
            "b_replaces_a"
        } else {
            "a_replaces_b"
        };
        deciding(&mut core, pick, 0.95);

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(
            superseded_by(&core, &ids[2]).await.as_deref(),
            Some(m.as_str()),
            "the direction did not name the member it was shown as"
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
        let dec = deciding(&mut core, "distinct", 0.95);
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

        let sent = dec.requests();
        let (sent, _) = sent.first().expect("the decider was asked");
        assert!(
            sent.contains("the pool holds sixteen"),
            "the merge's own words were withheld: {sent}"
        );
        assert!(
            sent.contains("max_connections is 16"),
            "a source was not shown as context: {sent}"
        );
        assert!(sent.contains("Earlier versions merged into"), "{sent}");
        assert!(sent.contains("(captured "), "members are dated: {sent}");
    }

    #[tokio::test]
    async fn a_context_block_too_big_for_the_window_is_trimmed_not_refused() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, "distinct", 0.95);
        let long = "verylongsourcetoken ".repeat(4000);
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

        assert_eq!(dec.calls(), 1, "the call was refused instead of trimmed");
        let (sent, _) = dec.requests().first().cloned().unwrap();
        assert!(
            sent.contains("the pool holds sixteen"),
            "a member was trimmed away"
        );
        assert!(
            sent.contains("sixteen connections"),
            "a member was trimmed away"
        );
        assert!(
            !sent.contains("verylongsourcetoken"),
            "the oversized source was not trimmed"
        );
    }

    #[tokio::test]
    async fn merge_sources_are_dropped_before_the_pair_is_refused() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, "distinct", 0.95);
        let long = "verylongsourcetoken ".repeat(4000);
        let ids = seed_titled(
            &core,
            &[
                ("Root one", long.as_str(), [1.0, 0.0]),
                ("Root two", "raise it for batch jobs", [0.99, 0.05]),
                ("Other", "sixteen connections", [0.93, 0.37]),
            ],
        )
        .await;
        // The newer source is kept while the older one alone does not fit.
        sqlx::query!(
            "UPDATE artifacts SET created_at = created_at + 100 WHERE id = ?",
            &ids[1]
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        let m = merged_from(
            &core,
            "Pool",
            "the pool holds sixteen",
            &[ids[0].clone(), ids[1].clone()],
        )
        .await;
        let pair = queue_pair(&core, &m, &ids[2]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let d = decision(&core, pair).await;
        assert_ne!(
            d.reason.as_deref(),
            Some("too_long"),
            "the pair was refused"
        );
        assert_eq!(dec.calls(), 1);
        let (sent, _) = dec.requests().first().cloned().unwrap();
        assert!(!sent.contains("verylongsourcetoken"));
        assert!(sent.contains("raise it for batch jobs"), "{sent}");
    }

    #[tokio::test]
    async fn members_that_do_not_fit_are_left_and_counted_without_a_call() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, "distinct", 0.95);
        let long = "verylongartifacttoken ".repeat(3000);
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

        assert_eq!(dec.calls(), 0, "a call that cannot fit was sent anyway");
        let d = decision(&core, pair).await;
        assert_eq!(d.reason.as_deref(), Some("too_long"));
        assert_eq!(d.band, Band::Leave);
        assert_eq!(
            core.store.get_pair(pair).await.unwrap().state,
            PairState::Pending,
            "left as it is, not closed"
        );
        assert!(
            core.store
                .pairs_to_judge(&pair_era(&core), 10)
                .await
                .unwrap()
                .is_empty(),
            "and not asked again while nothing changed"
        );
    }

    // ---- the decision record ----

    #[tokio::test]
    async fn a_sure_replacement_links_its_journal_row_to_the_decision() {
        let mut core = test_core().await;
        deciding(&mut core, "b_replaces_a", 0.95);
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let d = decision(&core, pair).await;
        assert_eq!((d.band, d.state), (Band::Act, State::Done));
        let row = core
            .store
            .open_action_on(&ids[0], Kind::Supersede)
            .await
            .unwrap()
            .expect("the supersede was journaled");
        assert_eq!(
            core.store.decision_action_ids(&d.id).await.unwrap(),
            vec![row.id]
        );
    }

    #[tokio::test]
    async fn undoing_that_replacement_marks_the_decision_undone() {
        let mut core = test_core().await;
        deciding(&mut core, "b_replaces_a", 0.95);
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        run(&core, &pair.to_string()).await.unwrap();

        let tenant = crate::tenants::Tenant {
            core: core.clone(),
            user: crate::store::control::User {
                subject: core.store.subject.clone(),
                email: None,
                slug: "u".into(),
                created_at: 0,
            },
        };
        crate::web::ops::unsupersede_artifact(&tenant, &ids[0])
            .await
            .unwrap();

        assert!(superseded_by(&core, &ids[0]).await.is_none());
        assert_eq!(decision(&core, pair).await.outcome, Some(Outcome::Undone));
    }

    #[tokio::test]
    async fn a_torn_pair_becomes_an_unsure_row_and_stays_pending() {
        let mut core = test_core().await;
        deciding(&mut core, "b_replaces_a", 0.7);
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let open = core.store.open_asks(10).await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].subject_a, pair.to_string());
        assert!(core.store.recent_actions(10).await.unwrap().is_empty());
        assert_eq!(
            core.store.get_pair(pair).await.unwrap().state,
            PairState::Pending
        );
        let subjects = PairApplier.subjects(&core, &open[0]).await.unwrap();
        let named: Vec<&str> = subjects.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(named.len(), 2, "both notes are named on the row");
        assert!(named.contains(&ids[0].as_str()) && named.contains(&ids[1].as_str()));
    }

    #[tokio::test]
    async fn a_sure_pick_whose_reversed_read_disagrees_asks_instead_of_acting() {
        let mut core = test_core().await;
        core.decider = Arc::new(RuleDecider::new(rule(
            "b_replaces_a",
            0.99,
            Some("distinct"),
        )));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let d = decision(&core, pair).await;
        assert_eq!((d.band, d.state), (Band::Ask, State::Open));
        assert_eq!(d.swapped_agree, Some(false));
        assert!(superseded_by(&core, &ids[0]).await.is_none());
        assert_eq!(
            core.store.get_pair(pair).await.unwrap().state,
            PairState::Pending
        );
    }

    #[tokio::test]
    async fn a_weak_answer_is_left_and_not_asked_again_until_a_member_changes() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, "distinct", 0.4);
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        let era = pair_era(&core);
        assert_eq!(core.store.pairs_to_judge(&era, 10).await.unwrap().len(), 1);

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(decision(&core, pair).await.band, Band::Leave);
        assert_eq!(
            core.store.get_pair(pair).await.unwrap().state,
            PairState::Pending
        );
        for _ in 0..3 {
            assert!(
                core.store
                    .pairs_to_judge(&era, 10)
                    .await
                    .unwrap()
                    .is_empty()
            );
            run(&core, &pair.to_string()).await.unwrap();
        }
        assert_eq!(dec.calls(), 1, "an untouched pair was asked again");

        sqlx::query!(
            "UPDATE artifacts SET embed_rev = embed_rev + 1 WHERE id = ?",
            &ids[1]
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        let back = core.store.pairs_to_judge(&era, 10).await.unwrap();
        assert_eq!(back.len(), 1, "an edited member's pair was not asked again");
        assert_eq!(back[0].id, pair);
        run(&core, &pair.to_string()).await.unwrap();
        assert_eq!(dec.calls(), 2);
    }

    #[tokio::test]
    async fn a_new_era_asks_everything_again() {
        let mut core = test_core().await;
        deciding(&mut core, "distinct", 0.4);
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert!(
            core.store
                .pairs_to_judge(&pair_era(&core), 10)
                .await
                .unwrap()
                .is_empty()
        );
        let next = era("another-model", pair_relation().version);
        let back = core.store.pairs_to_judge(&next, 10).await.unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, pair);
    }

    #[tokio::test]
    async fn a_spent_budget_leaves_the_pair_pending_without_spending_the_judge() {
        // The daily cap: the decision is made once and waits; a later sweep applies it without a
        // second call.
        let mut core = test_core().await;
        let dec = deciding(&mut core, "b_replaces_a", 0.95);
        core.evolve.autonomous = crate::config::Autonomy::Full;
        core.evolve.max_actions_per_day = 0;
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(dec.calls(), 1, "the decision is made once");
        let d = decision(&core, pair).await;
        assert_eq!((d.band, d.state), (Band::Act, State::Waiting));
        assert_eq!(
            core.store.get_pair(pair).await.unwrap().state,
            PairState::Pending
        );
        assert!(
            superseded_by(&core, &ids[0]).await.is_none(),
            "it acted past the cap"
        );
        assert_eq!(core.store.open_ask_count().await.unwrap(), 0);
        run(&core, &pair.to_string()).await.unwrap();
        assert_eq!(dec.calls(), 1, "a waiting decision was asked again");

        core.evolve.max_actions_per_day = 20;
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(r.applied, 1);
        assert_eq!(dec.calls(), 1);
        assert_eq!(
            superseded_by(&core, &ids[0]).await.as_deref(),
            Some(ids[1].as_str())
        );
        assert_eq!(
            core.store.get_pair(pair).await.unwrap().state,
            PairState::Dismissed
        );
    }

    // ---- duplicates and the writer ----

    #[tokio::test]
    async fn a_sure_duplicate_asks_for_the_merge_and_writes_nothing_itself() {
        let mut core = test_core().await;
        let judge = no_chat_judge(&mut core);
        let writer = Arc::new(ScriptedCompleter::new(vec![]));
        core.pair_synthesizer = Some(writer.clone());
        deciding(&mut core, "duplicate", 0.95);
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
        assert!(p.synthesis_asked);
        assert_eq!(p.synthesis_by, Some(DecidedBy::Model));
        assert!(
            core.store
                .live_job(Stage::Merge, &pair.to_string())
                .await
                .unwrap(),
            "no writer unit was armed"
        );
        assert!(core.store.merged_artifacts(10).await.unwrap().is_empty());
        assert!(core.store.recent_actions(10).await.unwrap().is_empty());
        assert_eq!(writer.calls(), 0);
        assert_eq!(judge.calls(), 0);
        let d = decision(&core, pair).await;
        assert_eq!((d.band, d.state), (Band::Act, State::Done));
        // The pair waits for the writer, not for another decision.
        assert!(
            core.store
                .pairs_to_judge(&pair_era(&core), 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_duplicate_with_no_writer_becomes_an_unsure_row_with_keep_one_buttons() {
        let mut core = test_core().await;
        let judge = no_chat_judge(&mut core);
        core.pair_synthesizer = None;
        deciding(&mut core, "duplicate", 0.95);
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let d = decision(&core, pair).await;
        assert_eq!((d.band, d.state), (Band::Ask, State::Open));
        assert_eq!(d.reason.as_deref(), Some("no_writer"));
        let buttons: Vec<String> = PairApplier
            .buttons(&core, &d)
            .await
            .unwrap()
            .into_iter()
            .map(|(_, l)| l)
            .collect();
        assert!(buttons.contains(&"Keep A".to_string()), "{buttons:?}");
        assert!(buttons.contains(&"Keep B".to_string()), "{buttons:?}");
        assert!(!buttons.contains(&"Duplicate".to_string()), "{buttons:?}");
        assert_eq!(judge.calls(), 0);
        assert!(!core.store.get_pair(pair).await.unwrap().synthesis_asked);
    }

    #[tokio::test]
    async fn with_a_writer_the_row_offers_duplicate_and_both_directions() {
        let mut core = test_core().await;
        core.pair_synthesizer = Some(Arc::new(ScriptedCompleter::new(vec![])));
        deciding(&mut core, "duplicate", 0.7);
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let d = decision(&core, pair).await;
        let buttons = PairApplier.buttons(&core, &d).await.unwrap();
        let want: Vec<(String, String)> = [
            ("duplicate", "Duplicate"),
            ("b_replaces_a", "Replaced ←"),
            ("a_replaces_b", "Replaced →"),
            ("conflict", "Conflict"),
            ("distinct", "Distinct"),
        ]
        .iter()
        .map(|(p, l)| (p.to_string(), l.to_string()))
        .collect();
        assert_eq!(buttons, want);
    }

    #[tokio::test]
    async fn keep_a_from_a_person_supersedes_b_past_the_cap() {
        let mut core = test_core().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        core.evolve.max_actions_per_day = 0;
        deciding(&mut core, "duplicate", 0.7);
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        run(&core, &pair.to_string()).await.unwrap();
        let p = core.store.get_pair(pair).await.unwrap();
        let d = decision(&core, pair).await;
        assert_eq!(d.state, State::Open);

        let d = crate::core::judging::answer(&core, &d.id, Some("keep_a"))
            .await
            .unwrap();

        assert_eq!(
            superseded_by(&core, &p.b_id).await.as_deref(),
            Some(p.a_id.as_str())
        );
        assert!(core.store.get_artifact(&p.a_id).await.unwrap().in_results());
        let settled = core.store.get_pair(pair).await.unwrap();
        assert_eq!(settled.decided_by, Some(DecidedBy::Operator));
        assert_eq!(d.outcome, Some(Outcome::Corrected));
    }

    #[tokio::test]
    async fn a_person_answering_distinct_closes_the_pair_and_can_ask_again() {
        let mut core = test_core().await;
        deciding(&mut core, "duplicate", 0.7);
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        run(&core, &pair.to_string()).await.unwrap();
        let d = decision(&core, pair).await;

        crate::core::judging::answer(&core, &d.id, Some("distinct"))
            .await
            .unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert_eq!(p.state, PairState::NoConflict);
        assert_eq!(p.decided_by, Some(DecidedBy::Operator));

        crate::core::judging::ask_again(&core, &d.id).await.unwrap();
        assert_eq!(
            core.store.get_pair(pair).await.unwrap().state,
            PairState::Pending
        );
        let back = core
            .store
            .pairs_to_judge(&pair_era(&core), 10)
            .await
            .unwrap();
        assert_eq!(back.len(), 1, "asked again, but not queued to be asked");
    }

    #[tokio::test]
    async fn a_waiting_act_on_a_pair_that_changed_is_asked_again() {
        let mut core = test_core().await;
        deciding(&mut core, "b_replaces_a", 0.95);
        core.evolve.autonomous = crate::config::Autonomy::Full;
        core.evolve.max_actions_per_day = 0;
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        run(&core, &pair.to_string()).await.unwrap();
        let d = decision(&core, pair).await;
        assert!(PairApplier.unchanged(&core, &d).await.unwrap());

        sqlx::query!(
            "UPDATE artifacts SET embed_rev = embed_rev + 1 WHERE id = ?",
            &ids[0]
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        assert!(!PairApplier.unchanged(&core, &d).await.unwrap());
        core.evolve.max_actions_per_day = 20;
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!((r.applied, r.reasked), (0, 1));
        assert!(superseded_by(&core, &ids[0]).await.is_none());
        assert_eq!(
            core.store
                .pairs_to_judge(&pair_era(&core), 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn the_dedupe_unit_no_longer_holds_for_generation_or_waits_out_the_cooldown() {
        use std::time::Duration;
        assert!(!Stage::GENERATORS.contains(&Stage::Dedupe));
        assert!(!Stage::Dedupe.calls_a_generator());
        let mut core = test_core().await;
        let dec = deciding(&mut core, "distinct", 0.95);
        core.gate = Arc::new(crate::infer::gate::InferenceGate::new(Duration::from_secs(
            600,
        )));
        core.gate.call_finished();
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        tokio::time::timeout(Duration::from_secs(10), run(&core, &pair.to_string()))
            .await
            .expect("the decision waited out the cooldown")
            .unwrap();

        assert_eq!(dec.calls(), 1);
    }

    #[tokio::test]
    async fn arming_stops_at_what_the_tick_has_left() {
        let mut core = test_core().await;
        core.decide_cfg.max_per_tick = 2;
        let ids = seed(
            &core,
            &[
                ("one", [1.0, 0.0]),
                ("two", [0.9, 0.1]),
                ("three", [0.8, 0.2]),
                ("four", [0.7, 0.3]),
                ("five", [0.6, 0.4]),
                ("six", [0.5, 0.5]),
            ],
        )
        .await;
        for w in ids.windows(2) {
            core.store.record_pair(&w[0], &w[1], 0.91).await.unwrap();
        }
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            5
        );

        assert_eq!(
            crate::jobs::consolidate::arm_dedupe(&core).await.unwrap(),
            2
        );
    }

    // ---- the operator's synthesis path (Stage::Merge) ----

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

        merge(&core, &pair_id.to_string()).await.unwrap();

        let p = core.store.get_pair(pair_id).await.unwrap();
        assert!(p.merged_into.is_none(), "a dismissed pair was merged");
        assert!(!p.synthesis_asked, "the card still promises a synthesis");
        assert_eq!(p.state, PairState::Dismissed);
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().in_results());
        }
    }

    #[tokio::test]
    async fn a_pair_awaiting_its_writer_is_not_the_decision_units_business() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, "distinct", 0.95);
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        core.store.ask_pair_synthesis(pair).await.unwrap();

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(dec.calls(), 0);
        let p = core.store.get_pair(pair).await.unwrap();
        assert!(p.synthesis_asked);
        assert_eq!(p.state, PairState::Pending);
    }

    #[tokio::test]
    async fn a_merge_that_would_lose_a_literal_says_so_without_naming_the_tokens() {
        let mut core = test_core().await;
        core.pair_synthesizer = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"merged":{"title":"Mount","text":"Mount the image with `mount -o rw /dev/loop0 /mnt/case`.","category":"procedure","caveats":[]}}"#
                .into(),
        ])));
        deciding(&mut core, "duplicate", 0.95);
        let ids = disagreeing_commands(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();
        merge(&core, &pair.to_string()).await.unwrap();

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
            !detail.contains("ro /dev"),
            "the lost tokens are evidence too thin to act on: {detail:?}"
        );
        for id in &ids {
            assert!(superseded_by(&core, id).await.is_none());
        }
    }

    #[tokio::test]
    async fn a_merge_that_would_lose_a_value_leaves_both_and_closes_the_pair() {
        let mut core = test_core().await;
        core.pair_synthesizer = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"merged":{"title":"Mount","text":"Mount the image with `mount -o ro /dev/loop0 /mnt/case`.","category":"procedure","caveats":[]}}"#
                .into(),
        ])));
        deciding(&mut core, "duplicate", 0.95);
        let ids = disagreeing_commands(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();
        merge(&core, &pair.to_string()).await.unwrap();

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
    async fn a_value_lost_by_an_earlier_merge_does_not_block_the_next_one() {
        let mut core = test_core().await;
        core.pair_synthesizer = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"merged":{"title":"Pool","text":"the pool holds sixteen connections","category":"reference","caveats":[]}}"#
                .into(),
        ])));
        deciding(&mut core, "duplicate", 0.95);
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
        merge(&core, &pair.to_string()).await.unwrap();

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
    async fn a_duplicate_verdict_writes_the_merge_and_journals_it() {
        let mut core = test_core().await;
        core.pair_synthesizer = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"merged":{"title":"Pool","text":"the pool holds sixteen connections","category":"reference","caveats":[]}}"#
                .into(),
        ])));
        deciding(&mut core, "duplicate", 0.95);
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
        merge(&core, &pair.to_string()).await.unwrap();

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
    async fn a_pair_a_person_asked_about_is_written_without_being_judged_again() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, "distinct", 0.95);
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

        merge(&core, &pair.to_string()).await.unwrap();

        assert_eq!(dec.calls(), 0);
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

        merge(&core, &pair.to_string()).await.unwrap();

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
        let dec = deciding(&mut core, "distinct", 0.95);
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
        merge(&core, &pair.to_string()).await.unwrap();

        assert_eq!(dec.calls(), 0);
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

        merge(&core, &pair.to_string()).await.unwrap();

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

        merge(&core, &pair.to_string())
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

        merge(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert!(p.merged_into.is_none(), "nothing was written");
        assert!(
            !p.synthesis_asked,
            "the card stops promising what will not come"
        );
        assert_eq!(p.state, PairState::NoConflict, "both stay as they are");
    }

    #[tokio::test]
    async fn a_merge_action_records_the_act_and_not_a_verdict() {
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

        merge(&core, &pair.to_string()).await.unwrap();

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

        merge(&core, &pair.to_string()).await.unwrap();

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
}
