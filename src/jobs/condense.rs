//! Condense: one artifact, one shorter version of itself, losing no literal.
//!
//! Armed by `sleep::condense_candidates` under "full" and the budget; one
//! generation. The reply is checked twice — `merge::losses` for the machine
//! literals, `verify::missing_numbers` for the numbers `merge::losses`
//! deliberately does not look at — and a draft that would lose either is
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
    // Already condensed, and the arming path's own check is not enough to say
    // so. `sleep::condense_candidates` asks this before arming, which stops a
    // second *arming*; it does nothing about the same arming running twice.
    // The write below commits and the `enqueue` after it lives in another
    // database, so a retryable failure there leaves this unit queued with the
    // condensation already applied — and the retry rewrote the artifact a
    // second time off one arming.
    //
    // Two versions off one nomination is not merely redundant. `undo_action_on`
    // stamps *every* open row for a subject and kind, so taking the newer
    // condensation back marks the older one undone as well, and the original
    // text — the one the first rewrite retired — is hidden from `jobs::retract`
    // for good: it reads open rows, and there are none left.
    if let Some(open) = core
        .store
        .open_action_on(&c.id, crate::store::actions::Kind::Condense)
        .await?
    {
        tracing::info!(
            artifact_id,
            action = %open.id,
            "this artifact is already condensed and the condensation stands; nothing written"
        );
        return Ok(());
    }
    // And the other half of the arming path's test, which this had only one of.
    //
    // An open row says the condensation stands. A row that was *taken back*
    // says a person or `jobs::retract` decided against it — and that leaves no
    // open row at all, so the check above waves the unit through. In exactly
    // the window this function's guards exist for — the rewrite committed, the
    // `enqueue` in the other database failed, the unit still queued — an undo
    // landing before the retry was reversed by it, unasked and unreported.
    //
    // `sleep::condense_candidates` asks both before it arms anything, for the
    // same reason: a condensation somebody took back is not a nomination.
    if core
        .store
        .action_was_undone(&c.id, crate::store::actions::Kind::Condense)
        .await?
    {
        tracing::info!(
            artifact_id,
            "this artifact's condensation was taken back; the queued rewrite is dropped"
        );
        return Ok(());
    }
    // Both gates are read again at the unit, not only where it was armed:
    // permission and budget can each have gone while this waited in the queue.
    //
    // `acts_on_corpus()` as well as `may_act()`, for the reason
    // `background.rs` gives about Reap: `may_act` answers `true`
    // unconditionally below "full", so on its own it was no gate at all here.
    // A `Condense` row is *persisted* — armed under "full", it outlives a drop
    // to "ranking" and a restart, and the generator rewrote the artifact
    // anyway. `jobs::retract`, the path that takes a condensation back, is
    // itself behind `acts_on_corpus()` and so was switched off at that same
    // level: the rewrite happened and nothing could undo it.
    if !core.evolve.autonomous.acts_on_corpus() {
        tracing::info!(
            artifact_id,
            "the corpus rules are switched off; the condensation is dropped"
        );
        return Ok(());
    }
    if !core.may_act(crate::store::actions::Job::Sleep).await? {
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
    // And the numbers, which `merge::losses` does not check and must not: a
    // merge brings two artifacts' numbers together across a rewrite, routinely
    // between languages, and refusing one for renumbering a list is what that
    // half was dropped for. A condensation is the same artifact in the same
    // language with less prose around the same facts, so "requires 1.22.0 or
    // later" coming back as "requires a recent version" is a silent change of
    // meaning — and a bare version in prose is neither fenced, backticked nor
    // path-shaped, so nothing in `losses` looks at it at all.
    //
    // Against the caveats as well as the text, for the reason `losses` reads
    // both: a number demoted to a caveat is stored, rendered and recoverable,
    // and this checks for loss, not for prominence.
    let mut kept = g.text.clone();
    for cav in &g.caveats {
        kept.push(' ');
        kept.push_str(cav);
    }
    let numbers = crate::infer::verify::missing_numbers(&c.text, &c.caveats, &kept);
    if !lost.is_empty() || !numbers.is_empty() {
        tracing::info!(
            artifact_id,
            ?lost,
            ?numbers,
            "the rewrite would lose a literal or a number; refused"
        );
        return Ok(());
    }
    let evidence = serde_json::json!({
        "rehearsals": probes.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
        "before_chars": c.text.len(),
        "after_chars": g.text.len(),
    });
    // `c.embed_rev` is the revision the text above was read at, and the model
    // call between the two is unbounded in time. An edit that landed in that
    // window has to win: it is a person's, it is newer, and the `losses()`
    // check just above was computed against the copy it replaced.
    let Some((action_id, n)) = core
        .store
        .condense_artifact(
            &c.id,
            Some(c.embed_rev),
            &g.text,
            Some(&g.title),
            &g.caveats,
            evidence,
        )
        .await?
    else {
        // Dropped rather than retried. The rewrite that came back describes
        // text the base no longer holds, so there is nothing here worth
        // keeping — and if the artifact still earns a condensation,
        // `sleep::condense_candidates` nominates it again on a later pass with
        // the current text in front of it.
        tracing::info!(
            artifact_id,
            rev = c.embed_rev,
            "the artifact was edited while this was being written; the rewrite is dropped"
        );
        return Ok(());
    };
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
        synthesized_with(core, "A long explanation of why the loop mount needs `mount -o loop /dev/loop0` and how it was found after three dead ends.").await
    }

    async fn synthesized_with(core: &Core, text: &str) -> String {
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
                    text: text.into(),
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

    /// A version in prose is not fenced, backticked or path-shaped, so
    /// `merge::losses` — written for a merge, where the numbers of two
    /// artifacts are being brought together across languages — sees nothing
    /// wrong with a rewrite that drops it. This is the same artifact in the
    /// same language, and "requires 1.22.0 or later" coming back as "requires
    /// a recent version" is a silent change of meaning.
    #[tokio::test]
    async fn a_rewrite_that_drops_a_number_out_of_prose_is_refused_without_writing() {
        let (core, writer) = core_with(vec![reply(
            "The loop mount needs a recent kernel and `mount -o loop /dev/loop0`.",
        )])
        .await;
        let id = synthesized_with(
            &core,
            "The loop mount requires 1.22.0 or later and is done with `mount -o loop /dev/loop0`, after three dead ends.",
        )
        .await;
        run(&core, &id).await.unwrap();
        assert_eq!(writer.calls(), 1);
        assert!(
            core.store.versions_of(&id).await.unwrap().is_empty(),
            "the version number went missing and the rewrite was written anyway"
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

    /// One arming, one condensation. The enqueue after the commit lives in
    /// another database, so a retryable failure there leaves this unit queued
    /// with the rewrite already applied — and the retry used to write a second
    /// version off the same nomination. The arming path's own check
    /// (`sleep::condense_candidates`) stops a second *arming* and does nothing
    /// about this.
    ///
    /// Two versions off one nomination is not merely redundant:
    /// `undo_action_on` stamps every open row for a subject and kind, so
    /// taking the newer one back marks the older undone too, and the original
    /// text is hidden from `jobs::retract` for good.
    #[tokio::test]
    async fn an_artifact_that_is_already_condensed_is_not_condensed_again() {
        let (core, writer) = core_with(vec![
            reply("The loop mount needs `mount -o loop /dev/loop0`."),
            reply("Needs `mount -o loop /dev/loop0`."),
        ])
        .await;
        let id = synthesized(&core).await;
        run(&core, &id).await.unwrap();
        assert_eq!(core.store.versions_of(&id).await.unwrap().len(), 1);

        // The retry the failed enqueue would have caused.
        run(&core, &id).await.unwrap();

        assert_eq!(writer.calls(), 1, "the second run spends no model call");
        assert_eq!(
            core.store.versions_of(&id).await.unwrap().len(),
            1,
            "one nomination, one version"
        );
    }

    /// The same retry, against a condensation somebody has since taken back.
    ///
    /// The guard above reads open rows, and an undo leaves none — so in the
    /// exact window these guards exist for (the rewrite committed, the
    /// `enqueue` in the other database failed, the unit still queued) a retry
    /// landing after the undo re-condensed the artifact and reversed it. The
    /// operator pressed the button, watched the original come back, and it
    /// went away again on the next tick with nothing to say why.
    ///
    /// `sleep::condense_candidates` has asked both questions since it was
    /// written; this asked one of them.
    #[tokio::test]
    async fn a_condensation_that_was_taken_back_is_not_written_again_by_a_retry() {
        let (core, writer) = core_with(vec![
            reply("The loop mount needs `mount -o loop /dev/loop0`."),
            reply("Needs `mount -o loop /dev/loop0`."),
        ])
        .await;
        let id = synthesized(&core).await;
        let before = core.store.get_artifact(&id).await.unwrap().text;
        run(&core, &id).await.unwrap();
        let action = core
            .store
            .open_action_on(&id, crate::store::actions::Kind::Condense)
            .await
            .unwrap()
            .expect("journaled");
        core.uncondense(&action.id, crate::store::actions::UndoneBy::Operator)
            .await
            .unwrap();

        // The retry the failed enqueue would have caused, arriving after.
        run(&core, &id).await.unwrap();

        assert_eq!(
            writer.calls(),
            1,
            "the retry spent a model call re-doing an undone condensation"
        );
        assert_eq!(
            core.store.get_artifact(&id).await.unwrap().text,
            before,
            "the operator's undo was reversed by a retry"
        );
    }

    /// A condensation is a read-modify-write with a model call in the middle,
    /// and that call takes as long as the endpoint feels like taking. An edit
    /// through the API in that window has to win: it is a person's, it is
    /// newer, and the `losses()` guard that is the whole safety argument of
    /// this path was computed against the copy it replaced — so a value the
    /// user had just added was covered by nothing at all.
    #[tokio::test]
    async fn an_edit_made_while_the_rewrite_was_being_written_is_not_reverted() {
        let (core, _) = core_with(vec![reply(
            "The loop mount needs `mount -o loop /dev/loop0`.",
        )])
        .await;
        let id = synthesized(&core).await;
        // The revision the unit would have read the text at, before the model
        // was asked. The scripted writer answers instantly, so the window is
        // opened by hand — but the revision is the whole of what the check
        // reads, and bumping it is exactly what an edit does.
        let read_at = core.store.get_artifact(&id).await.unwrap().embed_rev;
        let edited = "Loop mounts need `mount -o loop /dev/loop0` and `losetup -f`.";
        core.store.update_artifact_text(&id, edited).await.unwrap();

        let written = core
            .store
            .condense_artifact(
                &id,
                Some(read_at),
                "The loop mount needs `mount -o loop /dev/loop0`.",
                Some("Loop mounts"),
                &[],
                serde_json::json!({}),
            )
            .await
            .unwrap();

        assert!(written.is_none(), "the stale rewrite was written anyway");
        assert_eq!(
            core.store.get_artifact(&id).await.unwrap().text,
            edited,
            "the edit stands"
        );
        assert!(
            core.store.versions_of(&id).await.unwrap().is_empty(),
            "and nothing was journaled for a write that did not happen"
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

    /// A `Condense` row outlives the permission it was armed under: it is
    /// persisted, so dropping to "ranking" and restarting used to find it
    /// still on the queue and rewrite the artifact anyway — with
    /// `jobs::retract`, the only way back, switched off at that same level.
    #[tokio::test]
    async fn a_row_armed_under_full_writes_nothing_once_the_permission_is_gone() {
        for level in [
            crate::config::Autonomy::Off,
            crate::config::Autonomy::Ranking,
        ] {
            let (mut core, writer) = core_with(vec![reply("`mount -o loop /dev/loop0`")]).await;
            let id = synthesized(&core).await;
            arm(&core, &id).await.unwrap();
            core.evolve.autonomous = level;
            run(&core, &id).await.unwrap();
            assert_eq!(
                writer.calls(),
                0,
                "the permission is read before the call: {level:?}"
            );
            assert!(
                core.store.versions_of(&id).await.unwrap().is_empty(),
                "an artifact the operator did not permit rewriting was rewritten: {level:?}"
            );
        }
    }
}
