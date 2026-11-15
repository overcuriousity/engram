//! `Stage::Rescue`: rewrites live a retired note a decision or a person found valuable. Source text
//! rewrites from itself; a model-written note from its roots' text, never from a paraphrase. The only
//! part of the reap that calls a chat model.

use super::reap::{live_neighbours, min_age_secs, reap_era, step_aside};
use crate::core::Core;
use crate::core::judging::band::Band;
use crate::core::judging::check::{Checked, Which, check, outage};
use crate::core::judging::state::Note;
use crate::error::{Error, Result};
use crate::store::artifacts::Chunk;
use crate::store::decision_log::{Logged, NewLogged, Outcome, State};

/// Where the judge's reason was: no text of the decision's reaches the writer.
const LACKS: &str =
    "What the live base lacks: something this note states and the live notes shown do not.";
/// The cue the rewrite is stored with.
const CUE: &str = "reap rescue";

/// The reason on a question put to a person because the model's rescue could not be written.
const UNWRITTEN: &str = "rescue_refused";

/// `Stage::Rescue`: an inference outage is the queue's to retry. Any other failure puts the
/// model's ask to a person, or stands a person's down to the back of the reap queue.
pub async fn run(core: &Core, artifact_id: &str) -> Result<()> {
    let c = core.store.get_artifact(artifact_id).await?;
    // Restored, buried or rescued since it was asked for: there is nothing left to rescue.
    if !core.store.reapable(&c.id, min_age_secs(core)).await? {
        tracing::info!(artifact_id = %c.id, "no longer a reap candidate; the rescue is not written");
        return Ok(());
    }
    if core.generator.is_none() {
        tracing::info!(artifact_id = %c.id, "no generator model to rewrite it with; the rescue is not written");
        return Ok(());
    }
    match rescue_one(core, &c).await {
        Ok(new_id) => {
            tracing::info!(artifact_id = %c.id, new_id, "rescued a retired artifact into a rewrite");
            // The rewrite is stored: a mark that fails must not have the unit write it again.
            if let Err(e) = kept(core, &c).await {
                tracing::warn!(artifact_id = %c.id, error = %e, "could not mark the decision behind a rescue kept");
            }
            Ok(())
        }
        // An outage of a model or of a store is retried; a reply that will not do is not.
        Err(
            e @ (Error::Inference { .. }
            | Error::InferenceBusy { .. }
            | Error::Vector(_)
            | Error::Store(_)),
        ) => Err(e),
        Err(e) => {
            tracing::warn!(artifact_id = %c.id, error = %e, "could not rescue a valuable artifact");
            // Put to a person the note keeps its age: a restamp would close the question as changed.
            if !refused(core, &c).await? {
                step_aside(core, &c.id).await;
            }
            Ok(())
        }
    }
}

/// The model's own decision that armed this rescue, while nothing has become of it. `c` is the
/// note as it was before the rewrite; a person's answer is not the model's and stands as given.
async fn asked_by_model(core: &Core, c: &Chunk) -> Result<Option<Logged>> {
    let d = core
        .store
        .last_logged(
            "reap_worth",
            &c.id,
            c.superseded_by.as_deref(),
            &reap_era(core),
        )
        .await?;
    Ok(d.filter(|d| {
        d.band == Band::Act && d.pick == "yes" && d.answered_pick.is_none() && d.outcome.is_none()
    }))
}

/// A model's ask that was written. No journal row stands for a rescue, so no outcome window can
/// close on one: the decision is marked kept here, which is what the tuner counts.
async fn kept(core: &Core, c: &Chunk) -> Result<()> {
    if let Some(d) = asked_by_model(core, c).await? {
        core.store
            .set_decision_outcome(&d.id, Outcome::Kept, None)
            .await?;
    }
    Ok(())
}

/// A model's ask that came to nothing: refused, and the same question put to a person, so the
/// note is neither buried on the model's word nor out of sight for the era. False: it was a
/// person's ask, or nobody's that can be found.
async fn refused(core: &Core, c: &Chunk) -> Result<bool> {
    let Some(d) = asked_by_model(core, c).await? else {
        return Ok(false);
    };
    core.store
        .set_decision_outcome(&d.id, Outcome::Refused, None)
        .await?;
    let id = core
        .store
        .log_decision(&NewLogged {
            question_id: d.question_id.clone(),
            question_version: u32::try_from(d.question_version).unwrap_or(0),
            model: d.model.clone(),
            subject_kind: "artifact",
            subject_a: d.subject_a.clone(),
            subject_b: d.subject_b.clone(),
            rev_a: d.rev_a,
            rev_b: d.rev_b,
            answer: d.answer.clone(),
            top: d.top,
            pick: d.pick.clone(),
            swapped_agree: d.swapped_agree,
            band: Band::Ask,
            reason: Some(UNWRITTEN.into()),
            state: State::Waiting,
        })
        .await?;
    // Seated if a row is free; else it waits for one as any question does.
    if let Some(ask) = core.store.logged(&id).await? {
        crate::core::judging::settle(core, ask).await?;
    }
    Ok(true)
}

async fn rescue_one(core: &Core, c: &Chunk) -> Result<String> {
    // `core.generator`: `RESCUE_SYSTEM` needs `for_generating`'s response format.
    let writer = core
        .generator
        .as_ref()
        .ok_or_else(|| Error::Validation("no generator model configured".into()))?;
    let mut excerpts: Vec<(String, String)> = Vec::new();
    if c.provenance.is_source_text() {
        excerpts.push((
            c.title.clone().unwrap_or_else(|| "untitled".into()),
            c.text.clone(),
        ));
    } else {
        let roots = core
            .store
            .roots_of(std::slice::from_ref(&c.id))
            .await?
            .remove(&c.id)
            .unwrap_or_default();
        for root in &roots {
            if let Ok(r) = core.store.get_artifact(root).await
                && !r.text.trim().is_empty()
            {
                excerpts.push((
                    r.title.clone().unwrap_or_else(|| "untitled".into()),
                    r.text.clone(),
                ));
            }
        }
        if excerpts.is_empty() {
            return Err(Error::Validation(
                "a model-written candidate with no surviving source text cannot be rescued".into(),
            ));
        }
    }
    let mut user = String::new();
    user.push_str(&format!("{LACKS}\n\n"));
    user.push_str("----- SOURCE EXCERPTS -----\n");
    for (title, text) in &excerpts {
        user.push_str(&format!("Title: {title}\n\n{text}\n\n"));
    }
    let neighbours = live_neighbours(core, &c.id).await?;
    if !neighbours.is_empty() {
        user.push_str("----- CLOSEST LIVE ARTIFACTS -----\n");
        for (title, text) in &neighbours {
            user.push_str(&format!("Title: {title}\n\n{text}\n\n"));
        }
    }
    let permit = core.gate.background().await;
    let reply = writer
        .complete(crate::infer::prompt::RESCUE_SYSTEM, &user)
        .await;
    permit.finished();
    let g = crate::infer::prompt::parse_generation(&reply?)?;
    // A failed check is an outage, retried; a refusal stands the note down as any failed rescue.
    let originals: Vec<Note> = excerpts
        .iter()
        .map(|(title, text)| Note {
            title: Some(title),
            day: None,
            text,
        })
        .collect();
    let rewrite = Note {
        title: Some(&g.title),
        day: None,
        text: &g.text,
    };
    match check(core, &c.id, &originals, &rewrite, Which::Both)
        .await
        .map_err(outage)?
    {
        Checked::Refused { question, yes } => {
            return Err(Error::Validation(format!(
                "the check refused the rewrite ({question} at {yes})"
            )));
        }
        // A rescue stands in for the note, so a rewrite nothing checked is not written.
        Checked::Unjudged { detail } => {
            return Err(Error::Validation(format!(
                "the rewrite could not be checked ({detail})"
            )));
        }
        Checked::Passed | Checked::TooLong => {}
    }
    let made = core
        .store
        .insert_synthesized_artifact(
            &crate::store::artifacts::NewSynthesized {
                text: g.text,
                title: Some(g.title),
                category: g.category,
                tags: g.tags,
                caveats: g.caveats,
                cues: vec![CUE.into()],
            },
            std::slice::from_ref(&c.id),
        )
        .await?;
    core.store
        .enqueue(crate::store::jobs::Stage::Embed, "artifact", &made.id)
        .await?;
    // Through the store, not `core.supersede()`, whose both-active guard refuses a retired loser.
    // Re-checked like `bury`: the row may have been reactivated during the model call.
    if c.superseded_by.is_none() {
        let _guard = core.lifecycle_lock.lock().await;
        match core
            .store
            .supersede_if_reapable(&c.id, &made.id, min_age_secs(core))
            .await
        {
            Ok(()) => {
                core.vectors
                    .set_lifecycle(
                        &c.id,
                        crate::store::artifacts::ArtifactStatus::Superseded,
                        Some(&made.id),
                    )
                    .await?;
                core.store
                    .clear_lifecycle_dirty(std::slice::from_ref(&c.id))
                    .await?;
            }
            Err(Error::NotFound) => {
                tracing::info!(
                    artifact_id = %c.id,
                    new_id = %made.id,
                    "the candidate stopped being one while it was being rewritten; \
                     the rewrite stands and the row is left alone"
                );
                return Ok(made.id);
            }
            Err(e) => return Err(e),
        }
    }
    // Restamped on both branches, so the second visit is an age later, against the rewrite.
    core.store.restamp_retired(&c.id).await?;
    Ok(made.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::judging::Applier;
    use crate::core::test_support::test_core;
    use crate::jobs::reap::test_support::{
        SURE_NO, SURE_YES, backdate_retired_at, deciding, decision, deprecate_long_ago, scripted,
        seed,
    };
    use crate::store::artifacts::{ArtifactStatus, NewMerged};
    use std::sync::Arc;

    /// The one open question, which must be about `id`.
    async fn open_question(core: &Core, id: &str) -> Logged {
        let mut open = core.store.open_asks(10).await.unwrap();
        assert_eq!(open.len(), 1, "one question is put to a person");
        let row = open.remove(0);
        assert_eq!(row.subject_a, id);
        row
    }

    const RESCUE_REPLY: &str = r#"{"artifact":{"title":"Kept fact","text":"the port is 8443","category":null,"tags":[],"caveats":[]}}"#;

    fn writing(core: &mut Core, replies: &[&str]) -> Arc<crate::infer::fake::ScriptedCompleter> {
        let w = Arc::new(crate::infer::fake::ScriptedCompleter::new(
            replies.iter().map(|r| r.to_string()).collect(),
        ));
        core.generator = Some(w.clone());
        w
    }

    #[tokio::test]
    async fn a_rescue_is_written_from_the_fixed_line_and_supersedes_the_retired_note() {
        let mut core = test_core().await;
        deciding(&mut core, SURE_YES);
        let w = writing(&mut core, &[RESCUE_REPLY]);
        let ids = seed(&core, &["the admin port is 8443"]).await;
        deprecate_long_ago(&core, &ids[0]).await;

        let report = crate::jobs::reap::run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped, report.rescued), (1, 0, 1));
        assert_eq!(w.calls(), 0, "the sweep writes nothing itself");
        run(&core, &ids[0]).await.unwrap();

        let prompt = &w.prompts()[0];
        assert!(
            prompt.starts_with(
                "What the live base lacks: something this note states and the live notes shown do not."
            ),
            "{prompt}"
        );
        let old = core.store.get_artifact(&ids[0]).await.unwrap();
        let new_id = old
            .superseded_by
            .clone()
            .expect("superseded by the rewrite");
        let new = core.store.get_artifact(&new_id).await.unwrap();
        assert_eq!(
            new.provenance,
            crate::store::artifacts::Provenance::Synthesized
        );
        assert_eq!(new.text, "the port is 8443");
        assert_eq!(new.cues, vec!["reap rescue".to_string()]);
        assert!(new.in_results(), "the rewrite is live");
        assert!(!old.text.is_empty(), "a rescue wipes nothing");
    }

    #[tokio::test]
    async fn a_model_written_candidate_is_rewritten_from_its_roots() {
        let mut core = test_core().await;
        deciding(&mut core, SURE_YES);
        let scripted = writing(&mut core, &[RESCUE_REPLY]);
        let ids = seed(&core, &["mount with ro,loop", "journal at /var/log"]).await;
        let merge = core
            .store
            .insert_merged_artifact(
                &NewMerged {
                    text: "a paraphrase of both".into(),
                    title: Some("M".into()),
                    ..Default::default()
                },
                &ids,
            )
            .await
            .unwrap();
        core.store
            .set_artifact_status(&merge.id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        backdate_retired_at(&core, &merge.id, 100 * 86_400).await;

        let report = crate::jobs::reap::run(&core).await.unwrap();
        assert_eq!(report.rescued, 1);
        run(&core, &merge.id).await.unwrap();
        let rescue_prompt = &scripted.prompts()[0];
        assert!(
            rescue_prompt.contains("mount with ro,loop")
                && rescue_prompt.contains("journal at /var/log"),
            "the rewrite must be fed source text: {rescue_prompt}"
        );
        assert!(
            !rescue_prompt.contains("a paraphrase of both"),
            "never the model-written candidate's own text"
        );
        let new_id = core
            .store
            .get_artifact(&merge.id)
            .await
            .unwrap()
            .superseded_by
            .unwrap();
        let mut roots = core
            .store
            .roots_of(std::slice::from_ref(&new_id))
            .await
            .unwrap()
            .remove(&new_id)
            .unwrap();
        roots.sort();
        let mut want = ids.clone();
        want.sort();
        assert_eq!(
            roots, want,
            "the rewrite's lineage names the captured roots"
        );
    }

    #[tokio::test]
    async fn a_rescue_is_reaped_against_its_own_rewrite_on_the_second_pass() {
        // A rescue must restamp, or the candidate is back before the judge next interval, and must
        // wipe nothing.
        let mut core = test_core().await;
        // The rescue's after-write check takes the second answer.
        scripted(&mut core, &[SURE_YES, SURE_YES, SURE_NO]);
        writing(&mut core, &[RESCUE_REPLY]);
        let ids = seed(&core, &["the admin port is 8443"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;

        let first = crate::jobs::reap::run(&core).await.unwrap();
        assert_eq!((first.reaped, first.rescued), (0, 1));
        run(&core, &ids[0]).await.unwrap();
        let new_id = core
            .store
            .get_artifact(&ids[0])
            .await
            .unwrap()
            .superseded_by
            .expect("superseded by the rewrite");
        crate::jobs::embed::run(&core, &new_id).await.unwrap();

        let (none_yet, _) = crate::jobs::reap::nominees(&core).await.unwrap();
        assert!(
            none_yet.is_empty(),
            "a rescued candidate waits out a fresh age"
        );

        backdate_retired_at(&core, &ids[0], 100 * 86_400).await;

        let second = crate::jobs::reap::run(&core).await.unwrap();
        assert_eq!((second.judged, second.reaped, second.rescued), (1, 1, 0));
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_some());
        let rewrite = core.store.get_artifact(&new_id).await.unwrap();
        assert!(rewrite.in_results());
        assert_eq!(rewrite.text, "the port is 8443");
    }

    #[tokio::test]
    async fn a_person_pressing_valuable_arms_a_rescue_past_the_pacing() {
        let mut core = test_core().await;
        core.reap.max_rescues_per_run = 0;
        deciding(&mut core, 0.3);
        writing(&mut core, &[RESCUE_REPLY]);
        let ids = seed(&core, &["the admin port is 8443"]).await;
        deprecate_long_ago(&core, &ids[0]).await;
        crate::jobs::reap::run(&core).await.unwrap();
        let d = crate::jobs::reap::test_support::decision(&core, &ids[0], None).await;

        crate::core::judging::answer(&core, &d.id, Some("yes"))
            .await
            .unwrap();
        assert!(
            core.store
                .live_job(crate::store::jobs::Stage::Rescue, &ids[0])
                .await
                .unwrap()
        );
        run(&core, &ids[0]).await.unwrap();
        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .is_some()
        );
        // Theirs, as they gave it: the rescue being written does not make it the model's act.
        assert_eq!(
            core.store.logged(&d.id).await.unwrap().unwrap().outcome,
            Some(Outcome::Corrected)
        );
    }

    #[tokio::test]
    async fn a_model_s_rescue_that_was_written_is_kept_though_it_journals_nothing() {
        let mut core = test_core().await;
        deciding(&mut core, SURE_YES);
        writing(&mut core, &[RESCUE_REPLY]);
        let ids = seed(&core, &["the admin port is 8443"]).await;
        deprecate_long_ago(&core, &ids[0]).await;
        crate::jobs::reap::run(&core).await.unwrap();
        let asked = decision(&core, &ids[0], None).await;
        assert_eq!(asked.outcome, None);

        run(&core, &ids[0]).await.unwrap();

        let d = core.store.logged(&asked.id).await.unwrap().unwrap();
        assert_eq!(d.outcome, Some(Outcome::Kept));
        assert!(
            core.store
                .decision_action_ids(&d.id)
                .await
                .unwrap()
                .is_empty()
        );
        let record = core
            .store
            .act_record("reap_worth", &reap_era(&core))
            .await
            .unwrap();
        assert_eq!((record.acted, record.undone), (1, 0));
    }

    #[tokio::test]
    async fn an_outage_is_retried_and_any_other_failure_puts_the_model_s_ask_to_a_person() {
        let mut core = test_core().await;
        deciding(&mut core, SURE_YES);
        let ids = seed(&core, &["the admin port is 8443"]).await;
        deprecate_long_ago(&core, &ids[0]).await;
        crate::jobs::reap::run(&core).await.unwrap();
        let asked = decision(&core, &ids[0], None).await;
        let before = core.store.get_artifact(&ids[0]).await.unwrap().retired_at;

        // No replies left: the writer is unreachable.
        writing(&mut core, &[]);
        assert!(run(&core, &ids[0]).await.unwrap_err().retryable());
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().retired_at,
            before
        );
        assert!(core.store.open_asks(10).await.unwrap().is_empty());

        writing(&mut core, &["not a draft"]);
        run(&core, &ids[0]).await.unwrap();
        assert_eq!(
            core.store.logged(&asked.id).await.unwrap().unwrap().outcome,
            Some(Outcome::Refused)
        );
        let row = open_question(&core, &ids[0]).await;
        assert_ne!(row.id, asked.id);
        assert_eq!(
            (row.band, row.pick.as_str(), row.reason.as_deref()),
            (Band::Ask, "yes", Some("rescue_refused"))
        );
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().retired_at,
            before,
            "a restamp closes the person's question as changed"
        );
    }

    #[tokio::test]
    async fn a_note_whose_rescue_was_refused_stays_out_of_the_reap_and_is_a_person_s_to_bury() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, SURE_YES);
        writing(&mut core, &["not a draft"]);
        let ids = seed(&core, &["the admin port is 8443"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;
        crate::jobs::reap::run(&core).await.unwrap();
        run(&core, &ids[0]).await.unwrap();
        let row = open_question(&core, &ids[0]).await;
        let calls = dec.calls();

        // Not the model's again in this era, and still the note the question is about.
        let (cands, _) = crate::jobs::reap::nominees(&core).await.unwrap();
        assert!(cands.is_empty());
        crate::jobs::reap::run(&core).await.unwrap();
        assert_eq!(dec.calls(), calls);
        assert!(
            crate::jobs::reap::ReapApplier
                .unchanged(&core, &row)
                .await
                .unwrap()
        );
        crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(open_question(&core, &ids[0]).await.id, row.id);
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());

        crate::core::judging::answer(&core, &row.id, Some("no"))
            .await
            .unwrap();
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_person_s_rescue_that_fails_stands_the_note_down_and_asks_nobody() {
        let mut core = test_core().await;
        deciding(&mut core, 0.3);
        writing(&mut core, &["not a draft"]);
        let ids = seed(&core, &["the admin port is 8443"]).await;
        deprecate_long_ago(&core, &ids[0]).await;
        crate::jobs::reap::run(&core).await.unwrap();
        let d = decision(&core, &ids[0], None).await;
        crate::core::judging::answer(&core, &d.id, Some("yes"))
            .await
            .unwrap();
        let before = core.store.get_artifact(&ids[0]).await.unwrap().retired_at;

        run(&core, &ids[0]).await.unwrap();

        assert!(core.store.get_artifact(&ids[0]).await.unwrap().retired_at > before);
        assert!(core.store.open_asks(10).await.unwrap().is_empty());
        assert_eq!(
            core.store.logged(&d.id).await.unwrap().unwrap().outcome,
            Some(Outcome::Corrected)
        );
    }

    #[tokio::test]
    async fn a_rescue_that_fails_the_check_is_not_stored() {
        let mut core = test_core().await;
        deciding(&mut core, SURE_YES);
        let w = writing(&mut core, &[RESCUE_REPLY]);
        let ids = seed(&core, &["the admin port is 8443"]).await;
        deprecate_long_ago(&core, &ids[0]).await;
        crate::jobs::reap::run(&core).await.unwrap();
        let asked = decision(&core, &ids[0], None).await;
        let before = core.store.get_artifact(&ids[0]).await.unwrap().retired_at;
        let check = Arc::new(crate::core::judging::check::test_support::judge(0.9, 0.04));
        core.decider = check.clone();

        run(&core, &ids[0]).await.unwrap();

        assert_eq!((w.calls(), check.calls()), (1, 1));
        let old = core.store.get_artifact(&ids[0]).await.unwrap();
        assert!(
            old.superseded_by.is_none(),
            "an unfaithful rescue was stored"
        );
        assert!(
            core.store
                .synthesized_artifacts(10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            core.store.logged(&asked.id).await.unwrap().unwrap().outcome,
            Some(Outcome::Refused)
        );
        // Judged valuable and not rewritten: a person's to answer, in its own place in the queue.
        let row = open_question(&core, &ids[0]).await;
        assert_eq!(
            (row.band, row.reason.as_deref()),
            (Band::Ask, Some("rescue_refused"))
        );
        assert_eq!(old.retired_at, before);
    }

    #[tokio::test]
    async fn a_rescue_whose_check_errs_is_retried_and_not_stood_down() {
        let mut core = test_core().await;
        deciding(&mut core, SURE_YES);
        writing(&mut core, &[RESCUE_REPLY]);
        let ids = seed(&core, &["the admin port is 8443"]).await;
        deprecate_long_ago(&core, &ids[0]).await;
        crate::jobs::reap::run(&core).await.unwrap();
        let before = core.store.get_artifact(&ids[0]).await.unwrap().retired_at;
        core.decider = Arc::new(crate::infer::fake::RuleDecider::new(|_, _| {
            Err(Error::Inference {
                role: "decide",
                detail: "down".into(),
            })
        }));

        assert!(run(&core, &ids[0]).await.unwrap_err().retryable());
        let old = core.store.get_artifact(&ids[0]).await.unwrap();
        assert_eq!(old.retired_at, before);
        assert!(old.superseded_by.is_none());
    }
}
