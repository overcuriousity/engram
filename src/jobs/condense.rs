//! Condense: one artifact, one shorter version of itself, losing no literal.
//!
//! Armed by `sleep::condense_candidates` under "full" and the budget; one
//! generation. The reply is checked by `merge::losses` with the artifact as
//! its own root, and a draft that would lose a value or a machine literal is
//! refused without writing. Condensation may cost prose. Never a number —
//! and that is the one line on which this differs from biological gist, on
//! purpose.

use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::jobs::Stage;

pub async fn arm(core: &Core, artifact_id: &str) -> Result<()> {
    core.store
        .rearm_idle_seq(Stage::Condense, "artifact", artifact_id, 0)
        .await
}

pub async fn run(core: &Core, artifact_id: &str) -> Result<()> {
    let c = core.store.get_artifact(artifact_id).await?;
    // A passage is corpus text and is not condensed; one that earns it is
    // promoted, which exists.
    if c.provenance == crate::store::artifacts::Provenance::Passage || !c.in_results() {
        return Ok(());
    }
    // Read again at the unit, not only where it was armed: the window may
    // have closed while this waited in the queue.
    if !core.may_act().await? {
        tracing::info!(
            artifact_id,
            "budget spent; the condensation waits for the window to move"
        );
        return Ok(());
    }
    let writer = core
        .generator
        .as_ref()
        .ok_or_else(|| Error::Validation("no generator model configured".into()))?;
    let probes = core.store.rehearsals_of(&c.id).await?;
    let mut user = String::new();
    user.push_str(&format!(
        "Title: {}\n\n{}\n\n",
        c.title.clone().unwrap_or_else(|| "untitled".into()),
        c.text
    ));
    if !c.caveats.is_empty() {
        user.push_str(&format!("Caveats: {}\n\n", c.caveats.join(" | ")));
    }
    user.push_str("----- QUESTIONS IT HAS BEEN ANSWERING -----\n");
    for p in probes.iter().take(10) {
        user.push_str(&format!(
            "- {}\n",
            p.query.chars().take(200).collect::<String>()
        ));
    }
    let permit = core.gate.background().await;
    let reply = writer
        .complete(crate::infer::prompt::CONDENSE_SYSTEM, &user)
        .await;
    permit.finished();
    let g = crate::infer::prompt::parse_generation(&reply?)?;
    if g.text.len() >= c.text.len() {
        tracing::info!(artifact_id, "the rewrite is no shorter; nothing written");
        return Ok(());
    }
    let draft = crate::infer::prompt::MergedDraft {
        title: Some(g.title.clone()),
        text: g.text.clone(),
        category: g.category.clone(),
        tags: g.tags.clone(),
        caveats: g.caveats.clone(),
    };
    let lost = crate::jobs::merge::losses(std::slice::from_ref(&c), &draft);
    if !lost.is_empty() {
        tracing::info!(
            artifact_id,
            ?lost,
            "the rewrite would lose a value or a literal; refused"
        );
        return Ok(());
    }
    let evidence = serde_json::json!({
        "rehearsals": probes.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
        "before_chars": c.text.len(),
        "after_chars": g.text.len(),
    });
    let (action_id, n) = core
        .store
        .condense_artifact(&c.id, &g.text, Some(&g.title), &g.caveats, evidence)
        .await?;
    core.store.enqueue(Stage::Embed, "artifact", &c.id).await?;
    tracing::info!(artifact_id, action = %action_id, version = n, "condensed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::fake::ScriptedCompleter;
    use crate::store::artifacts::NewSynthesized;
    use std::sync::Arc;

    async fn synthesized(core: &Core) -> String {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let passage = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "verbatim".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        core.store
            .insert_synthesized_artifact(
                &NewSynthesized {
                    text: "A long explanation of why the loop mount needs `mount -o loop /dev/loop0` and how it was found after three dead ends.".into(),
                    title: Some("Loop mounts".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                    cues: vec!["how do I loop mount".into()],
                },
                std::slice::from_ref(&passage),
            )
            .await
            .unwrap()
            .id
    }

    fn reply(text: &str) -> String {
        format!(
            r#"{{"artifact":{{"title":"Loop mounts","text":"{text}","category":null,"tags":[],"caveats":[]}}}}"#
        )
    }

    async fn core_with(replies: Vec<String>) -> (Core, Arc<ScriptedCompleter>) {
        let mut core = crate::core::test_support::test_core().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let writer = Arc::new(ScriptedCompleter::new(replies));
        core.generator = Some(writer.clone());
        (core, writer)
    }

    #[tokio::test]
    async fn a_rewrite_that_drops_a_literal_is_refused_without_writing() {
        let (core, writer) = core_with(vec![reply("The loop mount needs a recent version.")]).await;
        let id = synthesized(&core).await;
        let before = core.store.get_artifact(&id).await.unwrap();
        run(&core, &id).await.unwrap();
        assert_eq!(writer.calls(), 1);
        let after = core.store.get_artifact(&id).await.unwrap();
        assert_eq!(after.text, before.text);
        assert!(core.store.versions_of(&id).await.unwrap().is_empty());
        assert!(
            core.store
                .open_action_on(&id, crate::store::actions::Kind::Condense)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_shorter_rewrite_that_keeps_every_token_becomes_version_one_and_is_taken_back_by_uncondense()
     {
        let (core, _) = core_with(vec![reply(
            "The loop mount needs `mount -o loop /dev/loop0`.",
        )])
        .await;
        let id = synthesized(&core).await;
        let before = core.store.get_artifact(&id).await.unwrap();
        run(&core, &id).await.unwrap();
        let after = core.store.get_artifact(&id).await.unwrap();
        assert_eq!(
            after.text,
            "The loop mount needs `mount -o loop /dev/loop0`."
        );
        assert_eq!(after.embed_rev, before.embed_rev + 1);
        let versions = core.store.versions_of(&id).await.unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].text, before.text);
        let action = core
            .store
            .open_action_on(&id, crate::store::actions::Kind::Condense)
            .await
            .unwrap()
            .expect("journaled");
        assert!(
            core.store.live_job(Stage::Embed, &id).await.unwrap(),
            "the new text is re-embedded"
        );

        core.uncondense(&action.id, crate::store::actions::UndoneBy::Operator)
            .await
            .unwrap();
        let back = core.store.get_artifact(&id).await.unwrap();
        assert_eq!(back.text, before.text);
        assert!(
            core.store
                .action_was_undone(&id, crate::store::actions::Kind::Condense)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_rewrite_no_shorter_and_a_spent_budget_both_write_nothing() {
        let (core, writer) = core_with(vec![reply(
            "A long explanation of why the loop mount needs `mount -o loop /dev/loop0` and how it was found after three dead ends, again.",
        )])
        .await;
        let id = synthesized(&core).await;
        run(&core, &id).await.unwrap();
        assert_eq!(writer.calls(), 1);
        assert!(core.store.versions_of(&id).await.unwrap().is_empty());

        let (mut core, writer) = core_with(vec![reply("`mount -o loop /dev/loop0`")]).await;
        core.evolve.max_actions_per_week = 0;
        let id = synthesized(&core).await;
        run(&core, &id).await.unwrap();
        assert_eq!(writer.calls(), 0, "the budget is read before the call");
        assert!(core.store.versions_of(&id).await.unwrap().is_empty());
    }
}
