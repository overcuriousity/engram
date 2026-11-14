//! `Stage::Rescue`: rewrites live a retired note a decision or a person found valuable. Source text
//! rewrites from itself; a model-written note from its roots' text, never from a paraphrase. The only
//! part of the reap that calls a chat model.

use super::reap::{live_neighbours, min_age_secs, reap_era, step_aside};
use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::artifacts::Chunk;
use crate::store::decision_log::Outcome;

/// Where the judge's reason was: no text of the decision's reaches the writer.
const LACKS: &str =
    "What the live base lacks: something this note states and the live notes shown do not.";
/// The cue the rewrite is stored with.
const CUE: &str = "reap rescue";

/// `Stage::Rescue`: an inference outage is the queue's to retry; any other failure stands the note
/// down to the back of the reap queue.
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
            Ok(())
        }
        Err(e @ (Error::Inference { .. } | Error::InferenceBusy { .. })) => Err(e),
        Err(e) => {
            tracing::warn!(artifact_id = %c.id, error = %e, "could not rescue a valuable artifact; standing it down");
            step_aside(core, &c.id).await;
            refused(core, &c).await
        }
    }
}

/// A model's ask that came to nothing; a person's answer stands as they gave it.
async fn refused(core: &Core, c: &Chunk) -> Result<()> {
    let d = core
        .store
        .last_logged(
            "reap_worth",
            &c.id,
            c.superseded_by.as_deref(),
            &reap_era(core),
        )
        .await?;
    if let Some(d) = d.filter(|d| d.answered_pick.is_none()) {
        core.store
            .set_decision_outcome(&d.id, Outcome::Refused, None)
            .await?;
    }
    Ok(())
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
    use crate::core::test_support::test_core;
    use crate::jobs::reap::test_support::{
        SURE_NO, SURE_YES, backdate_retired_at, deciding, deprecate_long_ago, scripted, seed,
    };
    use crate::store::artifacts::{ArtifactStatus, NewMerged};
    use std::sync::Arc;

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
        scripted(&mut core, &[SURE_YES, SURE_NO]);
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
    }

    #[tokio::test]
    async fn an_outage_is_retried_and_any_other_failure_stands_the_note_down() {
        let mut core = test_core().await;
        deciding(&mut core, SURE_YES);
        let ids = seed(&core, &["the admin port is 8443"]).await;
        deprecate_long_ago(&core, &ids[0]).await;
        crate::jobs::reap::run(&core).await.unwrap();
        let before = core.store.get_artifact(&ids[0]).await.unwrap().retired_at;

        // No replies left: the writer is unreachable.
        writing(&mut core, &[]);
        assert!(run(&core, &ids[0]).await.unwrap_err().retryable());
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().retired_at,
            before
        );

        writing(&mut core, &["not a draft"]);
        run(&core, &ids[0]).await.unwrap();
        assert!(core.store.get_artifact(&ids[0]).await.unwrap().retired_at > before);
        let d = crate::jobs::reap::test_support::decision(&core, &ids[0], None).await;
        assert_eq!(
            d.outcome,
            Some(crate::store::decision_log::Outcome::Refused)
        );
    }
}
