//! `Stage::Merge`: writes the merge a decision or a person asked for (`synthesis_asked`). The journal
//! rows link to the decision that asked as they are written; a draft that cannot be written
//! faithfully leaves both notes, and a model's ask is then recorded `refused` on its decision.

use super::dedupe::{
    Members, action, asked_for_merge, decided, leave_both, members_of, pair_era, restricted_now,
};
use crate::core::Core;
use crate::error::{Error, Result};
use crate::infer::prompt::{MergedDraft, SYNTHESIZE_SYSTEM, parse_synthesis};
use crate::store::actions::{Job, Kind};
use crate::store::artifacts::Chunk;
use crate::store::decision_log::Outcome;
use crate::store::pairs::{ArtifactPair, DecidedBy, MAX_UNREADABLE_JUDGEMENTS};

/// The draft, not the notes, was at fault, so both stay as they are.
pub(crate) const LOSS_REFUSED: &str =
    "every draft dropped a value one of them states; both stay as they are";

/// The after-write check refused the draft.
pub(crate) const UNFAITHFUL: &str = "a check found the draft unfaithful; both stay as they are";

/// Who asked and the decision the ask came from, when there is one.
struct Ask {
    by: DecidedBy,
    decision: Option<String>,
}

impl Ask {
    /// A model's ask that came to nothing; a person's answer stands as they gave it.
    async fn refused(&self, core: &Core) -> Result<()> {
        if let (DecidedBy::Model, Some(d)) = (self.by, &self.decision) {
            core.store
                .set_decision_outcome(d, Outcome::Refused, None)
                .await?;
        }
        Ok(())
    }
}

/// `Stage::Merge`: write the merge a decision or a person asked for.
pub async fn run(core: &Core, pair_id: &str) -> Result<()> {
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
    // An ask from before `synthesis_by` was a person's press.
    let by = p.synthesis_by.unwrap_or(DecidedBy::Operator);
    // A person's pick is not held by the daily cap; the model's is.
    if by == DecidedBy::Model && !core.may_act(Job::Dedupe).await? {
        tracing::info!(
            pair = id,
            "budget spent; the merge waits for the window to move"
        );
        return Ok(());
    }
    // Only the decision that asked for this merge; a press on the pair itself came from none,
    // whatever was logged about the pair last.
    let decision = core
        .store
        .last_logged("pair_relation", &p.id.to_string(), None, &pair_era(core))
        .await?
        .filter(|d| asked_for_merge(d, by))
        .map(|d| d.id);
    // The model asked while the pair sat above the line, or before it was restricted: a person's
    // now. The ask is dropped and the pair asked again, where it becomes an ask row.
    if by == DecidedBy::Model && restricted_now(core, &p) {
        tracing::warn!(pair = id, "refused a model's merge on a restricted pair");
        core.store.clear_pair_synthesis(p.id).await?;
        if let Some(d) = &decision {
            core.store.set_decision_reason(d, "changed").await?;
        }
        return Ok(());
    }
    let Some(Members { members, .. }) = members_of(core, &p).await? else {
        return Ok(());
    };
    write(core, &p, members, &Ask { by, decision }).await
}

/// The tail is `write_merge`, so a model's and a person's merge leave the base in one shape.
async fn write(core: &Core, p: &ArtifactPair, members: Vec<Chunk>, ask: &Ask) -> Result<()> {
    let sources: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
    // A run that failed after the write left its merge behind: that one is recorded, never a
    // second written.
    if let Some(merged) = core.store.merge_written_from(&p.a_id, &p.b_id).await? {
        return record(core, p, &sources, &merged, ask).await;
    }
    let Some(writer) = core.pair_synthesizer.clone() else {
        // The flag stays set, so the next run with a writer picks it up.
        return Ok(());
    };
    // No context block to trim here, so the two bodies fit or no model can write the pair.
    let user = synthesis_prompt(&members);
    let counter = crate::infer::budget::TokenCounter::default();
    let cost = counter.count(SYNTHESIZE_SYSTEM) + counter.count(&user);
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
        leave_both(
            core,
            p,
            "these two do not fit one call; both stay as they are",
            ask.by,
        )
        .await?;
        return ask.refused(core).await;
    }

    core.store.record_judge_attempt(p.id).await?;
    let permit = core.gate.background().await;
    let reply = writer.complete(SYNTHESIZE_SYSTEM, &user).await;
    permit.finished();
    let reply = reply?;
    let draft = match parse_synthesis(&reply) {
        Ok(d) => d,
        // Without the count this re-arms every tick on a reply that will not parse.
        Err(e) => {
            core.store.record_unreadable_judgement(p.id).await?;
            let spent = p.judge_unreadable + 1 >= MAX_UNREADABLE_JUDGEMENTS;
            tracing::warn!(
                pair = p.id,
                reply_len = reply.len(),
                error = %e,
                spent,
                "synthesis reply unreadable"
            );
            // Past the ceiling nothing re-arms it (`pairs_awaiting_synthesis`), so the ask goes
            // too: the card promises nothing and the pair stays pending for a person.
            if spent {
                core.store.clear_pair_synthesis(p.id).await?;
                ask.refused(core).await?;
            }
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
        leave_both(core, p, LOSS_REFUSED, ask.by).await?;
        return ask.refused(core).await;
    }
    if faithless(core, p, &members, &draft).await? {
        leave_both(core, p, UNFAITHFUL, ask.by).await?;
        return ask.refused(core).await;
    }

    write_merge(core, p, &sources, &draft, ask).await
}

/// After the loss check: whether the decision model refuses the draft against the members. A failed
/// check is an outage of this unit; a draft too long to judge rests on the loss check alone.
async fn faithless(
    core: &Core,
    p: &ArtifactPair,
    members: &[Chunk],
    draft: &MergedDraft,
) -> Result<bool> {
    use crate::core::judging::check::{Checked, Which, check, outage};
    use crate::core::judging::state::Note;
    let originals: Vec<Note> = members
        .iter()
        .map(|m| Note {
            title: prompt_name(m),
            day: None,
            text: &m.text,
        })
        .collect();
    let rewrite = Note {
        title: draft.title.as_deref(),
        day: None,
        text: &draft.text,
    };
    let got = check(core, &p.id.to_string(), &originals, &rewrite, Which::Both)
        .await
        .map_err(outage)?;
    if let Checked::Refused { question, yes } = got {
        tracing::warn!(
            pair = p.id,
            question,
            yes,
            "the check refused the merge draft"
        );
        return Ok(true);
    }
    Ok(false)
}

/// The loss check is the caller's.
async fn write_merge(
    core: &Core,
    p: &ArtifactPair,
    sources: &[String],
    draft: &MergedDraft,
    ask: &Ask,
) -> Result<()> {
    match crate::jobs::merge::write(core, draft, sources).await {
        Ok(m) => record(core, p, sources, &m.id, ask).await,
        // `leave_both` clears any ask with it.
        Err(Error::Validation(why)) => {
            tracing::warn!(pair = p.id, reason = %why, "the merge path refused a merge");
            leave_both(
                core,
                p,
                "These could not be written as one because of what one of them is made of; \
                 both stay as they are.",
                ask.by,
            )
            .await?;
            ask.refused(core).await
        }
        Err(e) => Err(e),
    }
}

/// One journal row per source, each linked as it is written, then the pair. Run again after a
/// failure, it writes only the rows that are missing.
async fn record(
    core: &Core,
    p: &ArtifactPair,
    sources: &[String],
    merged: &str,
    ask: &Ask,
) -> Result<()> {
    let act = format!("merged into {merged} from {} sources", sources.len());
    for source in sources {
        let written = match core.store.open_action_on(source, Kind::Merge).await? {
            Some(row) if row.survivor_id.as_deref() == Some(merged) => row.id,
            _ => {
                let row = action(Kind::Merge, p, source, Some(merged), Some(act.as_str()));
                let row = decided(row, ask.by, ask.decision.as_deref());
                core.store.record_action(&row).await?
            }
        };
        // Linked as written: a failure on the next row must not leave this one unlinked.
        if let Some(d) = &ask.decision {
            core.store.link_decision_actions(d, &[written]).await?;
        }
    }
    let why = match ask.by {
        DecidedBy::Operator => "synthesized at an operator's request",
        _ => "the judge read both as one, and nothing either states was lost",
    };
    core.store
        .set_pair_merged(p.id, merged, Some(why), ask.by)
        .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Autonomy;
    use crate::core::judging::check::test_support::{judge, not_checks};
    use crate::core::test_support::test_core;
    use crate::infer::fake::ScriptedCompleter;
    use crate::jobs::consolidate::tests::seed_titled;
    use crate::jobs::dedupe;
    use crate::jobs::dedupe::tests::{
        deciding, decision, disagreeing, disagreeing_commands, merged_from, queue_pair,
        superseded_by,
    };
    use crate::store::actions::Kind;
    use crate::store::artifacts::Provenance;
    use crate::store::decision_log::{Outcome, State};
    use crate::store::jobs::Stage;
    use crate::store::pairs::{DecidedBy, PairState};
    use std::sync::Arc;

    const POOL: &str = r#"{"merged":{"title":"Pool","text":"the pool holds sixteen connections","category":"reference","caveats":[]}}"#;
    /// Drops the `rw` command of `disagreeing_commands`.
    const DROPS_RW: &str = r#"{"merged":{"title":"Mount","text":"Mount the image with `mount -o ro /dev/loop0 /mnt/case`.","category":"procedure","caveats":[]}}"#;

    fn writing(core: &mut Core, replies: &[&str]) -> Arc<ScriptedCompleter> {
        let w = Arc::new(ScriptedCompleter::new(
            replies.iter().map(|r| r.to_string()).collect(),
        ));
        core.pair_synthesizer = Some(w.clone());
        w
    }

    async fn pool_pair(core: &Core) -> (Vec<String>, i64) {
        let ids = seed_titled(
            core,
            &[
                ("Pool sizing", "sixteen connections", [1.0, 0.0]),
                ("Connections", "sixteen connections", [0.93, 0.37]),
            ],
        )
        .await;
        let pair = queue_pair(core, &ids[0], &ids[1]).await;
        (ids, pair)
    }

    fn asked_by(a: &crate::store::actions::Action) -> Option<String> {
        let e: serde_json::Value = serde_json::from_str(&a.evidence_json).unwrap();
        e.get("asked_by")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// A pick of `duplicate` short of the act band, answered `duplicate` by a person.
    async fn a_person_picks_duplicate(core: &mut Core, pair: i64) -> String {
        deciding(core, "duplicate", 0.7);
        dedupe::run(core, &pair.to_string()).await.unwrap();
        let d = decision(core, pair).await;
        assert_eq!(d.state, State::Open);
        crate::core::judging::answer(core, &d.id, Some("duplicate"))
            .await
            .unwrap();
        d.id
    }

    #[tokio::test]
    async fn a_merge_the_model_asked_for_is_written_and_linked_to_its_decision() {
        let mut core = test_core().await;
        writing(&mut core, &[POOL]);
        deciding(&mut core, "duplicate", 0.95);
        let (_ids, pair) = pool_pair(&core).await;

        dedupe::run(&core, &pair.to_string()).await.unwrap();
        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert!(p.merged_into.is_some(), "the merge was not written");
        assert_eq!(p.decided_by, Some(DecidedBy::Model));
        let rows = core.store.open_actions(&[Kind::Merge], 10).await.unwrap();
        assert_eq!(rows.len(), 2, "one row per source");
        assert!(rows.iter().all(|r| asked_by(r).is_none()));
        let mut ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
        ids.sort();
        let d = decision(&core, pair).await;
        assert_eq!(core.store.decision_action_ids(&d.id).await.unwrap(), ids);
        assert_eq!(d.outcome, None, "kept only once the window closes");
    }

    #[tokio::test]
    async fn undoing_the_merge_marks_the_decision_undone() {
        let mut core = test_core().await;
        writing(&mut core, &[POOL]);
        deciding(&mut core, "duplicate", 0.95);
        let (_ids, pair) = pool_pair(&core).await;
        dedupe::run(&core, &pair.to_string()).await.unwrap();
        run(&core, &pair.to_string()).await.unwrap();
        let merged = core
            .store
            .get_pair(pair)
            .await
            .unwrap()
            .merged_into
            .unwrap();
        crate::jobs::embed::run(&core, &merged).await.unwrap();

        let tenant = crate::tenants::Tenant {
            core: core.clone(),
            user: crate::store::control::User {
                subject: core.store.subject.clone(),
                email: None,
                slug: "u".into(),
                created_at: 0,
            },
        };
        crate::web::ops::undo_merge(&tenant, &merged).await.unwrap();

        assert_eq!(decision(&core, pair).await.outcome, Some(Outcome::Undone));
    }

    #[tokio::test]
    async fn a_draft_that_drops_a_literal_is_refused_and_the_decision_says_so() {
        let mut core = test_core().await;
        writing(&mut core, &[DROPS_RW]);
        deciding(&mut core, "duplicate", 0.95);
        let ids = disagreeing_commands(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        dedupe::run(&core, &pair.to_string()).await.unwrap();
        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert_eq!(p.state, PairState::NoConflict, "the pair is closed");
        assert!(!p.synthesis_asked);
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().in_results());
        }
        assert!(core.store.merged_artifacts(10).await.unwrap().is_empty());
        assert_eq!(decision(&core, pair).await.outcome, Some(Outcome::Refused));
        // Never counted as kept, however long it stands.
        core.store
            .close_outcome_windows(crate::store::now() + 10)
            .await
            .unwrap();
        assert_eq!(decision(&core, pair).await.outcome, Some(Outcome::Refused));
    }

    #[tokio::test]
    async fn an_unreadable_draft_past_the_ceiling_is_refused_on_the_decision() {
        let mut core = test_core().await;
        writing(&mut core, &["not json", "not json either"]);
        deciding(&mut core, "duplicate", 0.95);
        let (_ids, pair) = pool_pair(&core).await;
        dedupe::run(&core, &pair.to_string()).await.unwrap();
        for _ in 0..crate::store::pairs::MAX_UNREADABLE_JUDGEMENTS - 2 {
            core.store.record_unreadable_judgement(pair).await.unwrap();
        }

        run(&core, &pair.to_string()).await.unwrap_err();
        assert_eq!(decision(&core, pair).await.outcome, None, "one more try");
        run(&core, &pair.to_string()).await.unwrap_err();

        assert_eq!(decision(&core, pair).await.outcome, Some(Outcome::Refused));
        assert!(core.store.merged_artifacts(10).await.unwrap().is_empty());
        let p = core.store.get_pair(pair).await.unwrap();
        assert!(
            !p.synthesis_asked,
            "an ask nothing serves any more still promises a synthesis"
        );
        assert_eq!(p.state, PairState::Pending, "it stays for a person");
    }

    #[tokio::test]
    async fn a_draft_unreadable_to_the_end_drops_the_ask_whoever_asked() {
        for by in [DecidedBy::Model, DecidedBy::Operator] {
            let mut core = test_core().await;
            writing(&mut core, &["not json"]);
            let (_ids, pair) = pool_pair(&core).await;
            core.store.ask_pair_synthesis_by(pair, by).await.unwrap();
            for _ in 0..MAX_UNREADABLE_JUDGEMENTS - 1 {
                core.store.record_unreadable_judgement(pair).await.unwrap();
            }

            run(&core, &pair.to_string()).await.unwrap_err();

            let p = core.store.get_pair(pair).await.unwrap();
            assert!(!p.synthesis_asked, "{by:?}: the ask is in neither queue");
            assert_eq!(p.state, PairState::Pending, "{by:?}");
            assert!(p.merged_into.is_none(), "{by:?}");
            // The retry the queue makes of the failed unit finds nothing left to do.
            run(&core, &pair.to_string()).await.unwrap();
        }
    }

    #[tokio::test]
    async fn an_unreadable_count_a_judge_left_does_not_shelve_a_first_merge() {
        let mut core = test_core().await;
        writing(&mut core, &["not json", POOL]);
        let (_ids, pair) = pool_pair(&core).await;
        // What the chat judge left on the row, one short of the ceiling.
        for _ in 0..MAX_UNREADABLE_JUDGEMENTS - 1 {
            core.store.record_unreadable_judgement(pair).await.unwrap();
        }
        core.store.ask_pair_synthesis(pair).await.unwrap();

        run(&core, &pair.to_string()).await.unwrap_err();

        let p = core.store.get_pair(pair).await.unwrap();
        assert!(
            p.synthesis_asked,
            "the writer's first unreadable draft was its last"
        );
        assert_eq!(p.judge_unreadable, 1, "the count is the writer's own");
        run(&core, &pair.to_string()).await.unwrap();
        assert!(
            core.store
                .get_pair(pair)
                .await
                .unwrap()
                .merged_into
                .is_some(),
            "the merge was not written on the next try"
        );
    }

    #[tokio::test]
    async fn a_merge_a_person_pressed_for_links_to_no_decision_that_asked_something_else() {
        let mut core = test_core().await;
        writing(&mut core, &[POOL]);
        deciding(&mut core, "conflict", 0.95);
        let (_ids, pair) = pool_pair(&core).await;
        dedupe::run(&core, &pair.to_string()).await.unwrap();
        assert_eq!(
            core.store.get_pair(pair).await.unwrap().state,
            PairState::Contradiction
        );
        // Pressed on the pair itself, not an answer to the row.
        assert!(core.store.ask_pair_synthesis(pair).await.unwrap());

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert!(p.merged_into.is_some(), "the merge was not written");
        let rows = core.store.open_actions(&[Kind::Merge], 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        for r in &rows {
            let e: serde_json::Value = serde_json::from_str(&r.evidence_json).unwrap();
            assert!(e.get("decision").is_none(), "{e}");
            assert_eq!(asked_by(r).as_deref(), Some("operator"));
        }
        let d = decision(&core, pair).await;
        assert!(
            core.store
                .decision_action_ids(&d.id)
                .await
                .unwrap()
                .is_empty(),
            "a person's merge was linked to the model's conflict"
        );
        // So neither the merge standing nor its undo says anything about that act.
        core.store
            .close_outcome_windows(crate::store::now() + 10)
            .await
            .unwrap();
        assert_eq!(decision(&core, pair).await.outcome, None);
    }

    /// Makes every journal write fail, as a crash after the merge is written would.
    async fn fail_the_journal(core: &Core, on: bool) {
        let sql = if on {
            "CREATE TRIGGER journal_fails BEFORE INSERT ON corpus_actions
             BEGIN SELECT RAISE(ABORT, 'injected'); END"
        } else {
            "DROP TRIGGER journal_fails"
        };
        // runtime: a test-only trigger, the seam for a failure after the merge is written.
        sqlx::query(sql).execute(&core.store.pool).await.unwrap();
    }

    #[tokio::test]
    async fn a_merge_whose_journal_failed_is_finished_by_the_retry_and_not_written_twice() {
        let mut core = test_core().await;
        let w = writing(&mut core, &[POOL, POOL]);
        deciding(&mut core, "duplicate", 0.95);
        let (_ids, pair) = pool_pair(&core).await;
        dedupe::run(&core, &pair.to_string()).await.unwrap();
        fail_the_journal(&core, true).await;

        run(&core, &pair.to_string()).await.unwrap_err();
        let p = core.store.get_pair(pair).await.unwrap();
        assert!(p.synthesis_asked && p.merged_into.is_none());
        fail_the_journal(&core, false).await;
        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(w.calls(), 1, "the retry had the merge written again");
        let merged = core.store.merged_artifacts(10).await.unwrap();
        assert_eq!(merged.len(), 1, "a second merge was written");
        let p = core.store.get_pair(pair).await.unwrap();
        assert_eq!(p.merged_into.as_deref(), Some(merged[0].id.as_str()));
        let rows = core.store.open_actions(&[Kind::Merge], 10).await.unwrap();
        assert_eq!(rows.len(), 2, "one row per source");
        let d = decision(&core, pair).await;
        assert_eq!(
            core.store.decision_action_ids(&d.id).await.unwrap().len(),
            2
        );
        // A third run, as a queue retry would make, journals nothing more.
        run(&core, &pair.to_string()).await.unwrap();
        assert_eq!(
            core.store
                .open_actions(&[Kind::Merge], 10)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn a_merge_a_person_asked_for_is_not_counted_against_the_band() {
        let mut core = test_core().await;
        writing(&mut core, &[POOL]);
        let (_ids, pair) = pool_pair(&core).await;
        let d = a_person_picks_duplicate(&mut core, pair).await;

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert!(p.merged_into.is_some());
        assert_eq!(p.decided_by, Some(DecidedBy::Operator));
        let rows = core.store.open_actions(&[Kind::Merge], 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|r| asked_by(r).as_deref() == Some("operator")),
            "a person's merge was read as the model's verdict"
        );
        assert_eq!(core.store.decision_action_ids(&d).await.unwrap().len(), 2);
        assert_eq!(
            decision(&core, pair).await.outcome,
            Some(Outcome::Confirmed)
        );
    }

    #[tokio::test]
    async fn a_refused_merge_a_person_asked_for_leaves_their_answer_standing() {
        let mut core = test_core().await;
        writing(&mut core, &[DROPS_RW]);
        let ids = disagreeing_commands(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        a_person_picks_duplicate(&mut core, pair).await;

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert_eq!(p.state, PairState::NoConflict);
        assert!(p.merged_into.is_none());
        assert_eq!(
            p.decided_by,
            Some(DecidedBy::Operator),
            "what a person's answer came to was recorded as the model's decision"
        );
        assert_eq!(
            decision(&core, pair).await.outcome,
            Some(Outcome::Confirmed)
        );
    }

    #[tokio::test]
    async fn a_merge_a_person_asked_for_is_written_past_the_daily_cap() {
        let mut core = test_core().await;
        core.evolve.autonomous = Autonomy::Full;
        core.evolve.max_actions_per_day = 0;
        writing(&mut core, &[POOL]);
        let (_ids, pair) = pool_pair(&core).await;
        core.store.ask_pair_synthesis(pair).await.unwrap();

        run(&core, &pair.to_string()).await.unwrap();

        assert!(
            core.store
                .get_pair(pair)
                .await
                .unwrap()
                .merged_into
                .is_some(),
            "a person's press waited on the model's cap"
        );
    }

    #[tokio::test]
    async fn a_merge_the_model_asked_for_waits_past_the_daily_cap() {
        let mut core = test_core().await;
        core.evolve.autonomous = Autonomy::Full;
        core.evolve.max_actions_per_day = 0;
        let w = writing(&mut core, &[POOL]);
        let (_ids, pair) = pool_pair(&core).await;
        core.store
            .ask_pair_synthesis_by(pair, DecidedBy::Model)
            .await
            .unwrap();

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert!(p.merged_into.is_none(), "it wrote past the cap");
        assert!(p.synthesis_asked, "the ask is kept for the next window");
        assert_eq!(w.calls(), 0);
    }

    fn raise_the_line(core: &Core, to: f32) {
        core.ranking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .review_min = to;
    }

    #[tokio::test]
    async fn a_merge_the_model_asked_for_is_not_written_once_its_pair_is_restricted() {
        let mut core = test_core().await;
        let w = writing(&mut core, &[POOL]);
        deciding(&mut core, "duplicate", 0.95);
        let (ids, pair) = pool_pair(&core).await;
        dedupe::run(&core, &pair.to_string()).await.unwrap();
        assert!(core.store.get_pair(pair).await.unwrap().synthesis_asked);
        raise_the_line(&core, 0.92);

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert!(
            p.merged_into.is_none(),
            "a restricted pair must not be merged on the model's word"
        );
        assert!(!p.synthesis_asked, "the model's ask must be dropped");
        assert_eq!(p.state, PairState::Pending);
        assert_eq!(w.calls(), 0);
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().in_results());
        }
        // Asked again, where it lands as an ask row.
        assert_eq!(
            core.store
                .pairs_to_judge(&dedupe::pair_era(&core), 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_merge_a_person_asked_for_is_written_on_a_restricted_pair() {
        let mut core = test_core().await;
        writing(&mut core, &[POOL]);
        let (_ids, pair) = pool_pair(&core).await;
        a_person_picks_duplicate(&mut core, pair).await;
        raise_the_line(&core, 0.92);

        run(&core, &pair.to_string()).await.unwrap();

        assert!(
            core.store
                .get_pair(pair)
                .await
                .unwrap()
                .merged_into
                .is_some()
        );
    }

    #[tokio::test]
    async fn the_merge_unit_holds_while_generation_is_shut() {
        assert!(Stage::GENERATORS.contains(&Stage::Merge));
        assert!(Stage::Merge.calls_a_generator());
        assert!(!Stage::GENERATORS.contains(&Stage::Dedupe));
    }

    #[tokio::test]
    async fn with_no_writer_the_unit_leaves_the_ask_standing() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, "distinct", 0.95);
        let (_ids, pair) = pool_pair(&core).await;
        core.store.ask_pair_synthesis(pair).await.unwrap();

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert!(p.synthesis_asked, "the ask was dropped");
        assert_eq!(p.state, PairState::Pending);
        assert!(p.merged_into.is_none());
        assert_eq!(dec.calls(), 0, "the writer unit asked a question");
    }

    #[tokio::test]
    async fn a_merge_that_fails_the_check_is_not_written_and_both_stay() {
        let mut core = test_core().await;
        writing(&mut core, &[POOL]);
        deciding(&mut core, "duplicate", 0.95);
        let (ids, pair) = pool_pair(&core).await;
        dedupe::run(&core, &pair.to_string()).await.unwrap();
        let check = Arc::new(judge(0.03, 0.9));
        core.decider = check.clone();

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(check.calls(), 1, "the draft was checked");
        let p = core.store.get_pair(pair).await.unwrap();
        assert!(p.merged_into.is_none(), "an unfaithful draft was written");
        assert_eq!(p.state, PairState::NoConflict);
        assert_eq!(p.detail.as_deref(), Some(UNFAITHFUL));
        assert!(!p.synthesis_asked);
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().in_results());
        }
        assert!(core.store.merged_artifacts(10).await.unwrap().is_empty());
        assert_eq!(decision(&core, pair).await.outcome, Some(Outcome::Refused));
    }

    #[tokio::test]
    async fn a_merge_a_person_asked_for_that_fails_the_check_leaves_their_answer_standing() {
        let mut core = test_core().await;
        writing(&mut core, &[POOL]);
        let (_ids, pair) = pool_pair(&core).await;
        a_person_picks_duplicate(&mut core, pair).await;
        core.decider = Arc::new(judge(0.9, 0.04));

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert!(p.merged_into.is_none());
        assert_eq!(p.state, PairState::NoConflict);
        assert_eq!(
            decision(&core, pair).await.outcome,
            Some(Outcome::Confirmed)
        );
    }

    #[tokio::test]
    async fn a_lost_literal_is_still_caught_before_the_model_is_asked() {
        let mut core = test_core().await;
        writing(&mut core, &[DROPS_RW]);
        deciding(&mut core, "duplicate", 0.95);
        let ids = disagreeing_commands(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        dedupe::run(&core, &pair.to_string()).await.unwrap();
        let check = Arc::new(judge(0.9, 0.9));
        core.decider = check.clone();

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(check.calls(), 0, "the check was asked about a lost literal");
        let p = core.store.get_pair(pair).await.unwrap();
        assert_eq!(p.detail.as_deref(), Some(LOSS_REFUSED));
    }

    #[tokio::test]
    async fn a_check_that_errs_fails_the_unit_for_a_retry_and_writes_nothing() {
        let mut core = test_core().await;
        writing(&mut core, &[POOL]);
        deciding(&mut core, "duplicate", 0.95);
        let (_ids, pair) = pool_pair(&core).await;
        dedupe::run(&core, &pair.to_string()).await.unwrap();
        core.decider = Arc::new(crate::infer::fake::RuleDecider::new(|_, _| {
            Err(Error::InferenceRejected {
                role: "decide",
                detail: "400".into(),
            })
        }));

        let err = run(&core, &pair.to_string()).await.unwrap_err();

        assert!(err.retryable(), "{err}");
        let p = core.store.get_pair(pair).await.unwrap();
        assert!(p.merged_into.is_none());
        assert!(p.synthesis_asked, "the ask waits for the retry");
        assert_eq!(decision(&core, pair).await.outcome, None);
    }

    #[tokio::test]
    async fn too_long_falls_back_to_the_literal_checks_alone() {
        let mut core = test_core().await;
        writing(&mut core, &[POOL]);
        deciding(&mut core, "duplicate", 0.95);
        let (_ids, pair) = pool_pair(&core).await;
        dedupe::run(&core, &pair.to_string()).await.unwrap();
        let check = Arc::new(judge(0.01, 0.01));
        check.set_context_tokens(200);
        core.decider = check.clone();

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(check.calls(), 0);
        assert!(
            core.store
                .get_pair(pair)
                .await
                .unwrap()
                .merged_into
                .is_some(),
            "a draft that passed the literal checks was not written"
        );
    }

    // ---- moved from `dedupe` with their assertions ----

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
    async fn a_merge_that_would_lose_a_literal_says_so_without_naming_the_tokens() {
        let mut core = test_core().await;
        core.pair_synthesizer = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"merged":{"title":"Mount","text":"Mount the image with `mount -o rw /dev/loop0 /mnt/case`.","category":"procedure","caveats":[]}}"#
                .into(),
        ])));
        deciding(&mut core, "duplicate", 0.95);
        let ids = disagreeing_commands(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        dedupe::run(&core, &pair.to_string()).await.unwrap();
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

        dedupe::run(&core, &pair.to_string()).await.unwrap();
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

        dedupe::run(&core, &pair.to_string()).await.unwrap();
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

        dedupe::run(&core, &pair.to_string()).await.unwrap();
        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert!(p.merged_into.is_some(), "the merge was not written");
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

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(not_checks(&dec), 0);
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

        dedupe::run(&core, &pair.to_string()).await.unwrap();
        run(&core, &pair.to_string()).await.unwrap();

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
}
