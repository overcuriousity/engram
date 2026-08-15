//! Writing, verifying and undoing a merge.
//!
//! Merging is the one thing in this system that puts model-written text where
//! stored text used to be, so it is also the one thing that can lose knowledge
//! without anyone noticing: a plausible paragraph reads exactly as well without
//! the number it dropped, ranks exactly as well, and nothing downstream can
//! tell. `losses` is what stands between a verdict and a write.
//!
//! Both checks are local and free — two token sets and one substring pass — and
//! that is the point. The argument for letting a model rewrite stored knowledge
//! unattended is not that it rarely goes wrong; it is that when it does, a rule
//! that costs nothing catches it before anything is written.

use crate::core::Core;
use crate::error::Result;
use crate::infer::prompt::MergedDraft;
use crate::store::artifacts::{Chunk, NewMerged, Provenance};
use crate::store::jobs::Stage;

/// Create a merged artifact and queue its embedding. Its roots are **not**
/// superseded here.
///
/// The order is the whole design of this function. Superseding before the merge
/// is indexed opens a window in which the roots are out of search and the merge
/// is not yet in it — the knowledge temporarily unreachable, which is the
/// failure class `heal_dangling_supersessions` and
/// `deleting_the_survivor_puts_the_artifact_it_hid_back` exist to prevent, and
/// the window is as long as the embed queue is deep.
///
/// In this order the worst a crash can leave is the merge and its roots all in
/// search at once. That is redundancy, which is the state the system was already
/// in before the merge — strictly better than a gap. `finish` closes it once the
/// embedding lands, and the sweep finishes any that were interrupted.
pub async fn write(core: &Core, draft: &MergedDraft, roots: &[String]) -> Result<Chunk> {
    let m = core
        .store
        .insert_merged_artifact(
            &NewMerged {
                text: draft.text.clone(),
                title: draft.title.clone(),
                category: draft.category.clone(),
                tags: draft.tags.clone(),
                caveats: draft.caveats.clone(),
            },
            roots,
        )
        .await?;
    core.store.enqueue(Stage::Embed, "artifact", &m.id).await?;
    tracing::info!(merged = %m.id, sources = roots.len(), "wrote a merged artifact");
    Ok(m)
}

/// Hide what an already-indexed merged artifact replaced.
///
/// Called from `mark_indexed` when a merged artifact finishes embedding, and
/// again from the sweep for merges whose process died in between — a state that
/// is complete from the artifact side and absent from the pair side, so only a
/// join across the lineage would ever notice it.
pub async fn finish(core: &Core, merged_id: &str) -> Result<()> {
    let m = core.store.get_artifact(merged_id).await?;
    if m.provenance != Provenance::Merged || !m.in_results() {
        return Ok(());
    }

    // `roots_to_hide`, not `roots_of`: a root an operator explicitly restored
    // out of this merge stays in results, and this repair path — re-run by the
    // sweep for as long as the merge stands — must not undo that decision on
    // every tick.
    for root in core.store.roots_to_hide(&m.id).await? {
        let Ok(r) = core.store.get_artifact(&root).await else {
            continue;
        };
        if !r.in_results() {
            continue;
        }
        // Warn and carry on. `supersede` refuses a side that is no longer
        // active, and an operator deprecating a root between the read and here
        // is an ordinary race rather than a reason to abandon the other roots —
        // the sweep's repair reaches whatever is left.
        if let Err(e) = core.supersede(&root, &m.id).await {
            tracing::warn!(root = %root, merged = %m.id, error = %e,
                "could not hide a merged artifact's root; it stays active");
        }
    }

    // And any earlier merge this one subsumes. Superseding only the roots would
    // leave that merge active and near-identical to this one, so the relate unit
    // would file the pair again on the next sweep and the two would churn
    // against each other for as long as they both existed.
    for older in core.store.subsumed_merges(&m.id).await? {
        // Everything the older merge was hiding has to be re-pointed first.
        // Those roots are already superseded — by `older` — so `finish`'s loop
        // above skipped them, and hiding `older` behind this merge without
        // moving them would leave `root -> older -> m`: a chain whose middle is
        // itself out of results. That is the exact failure `Clusters` exists to
        // prevent on the sweep's side, and the reader who opens a root would be
        // sent to a dead end.
        for hidden in core.store.artifacts_superseded_by(&older).await? {
            if let Err(e) = core.repoint_supersession(&hidden, &m.id).await {
                tracing::warn!(artifact = %hidden, to = %m.id, error = %e,
                    "could not re-point a supersession; it still names a hidden winner");
            }
        }
        if let Err(e) = core.supersede(&older, &m.id).await {
            tracing::warn!(subsumed = %older, by = %m.id, error = %e,
                "could not hide a merge this one subsumes; it stays active");
        }
    }
    Ok(())
}

/// Take a merge back: what it replaced returns, the merge is retired, and the
/// pairs that produced it are dismissed.
///
/// The third of those is easy to leave out and useless to leave out. Restoring
/// the sources alone accomplishes nothing: the sweep re-finds them, the model
/// reaches the same verdict, and the operator's decision is silently undone on
/// the next tick. `record_pair` is `INSERT OR IGNORE`, so a dismissed row is
/// respected forever — the mechanism exists and only needs to be used. This is
/// the same bug `reactivating_a_superseded_artifact_survives_the_next_sweep`
/// pins for the sweep's own supersessions.
///
/// The merge is deprecated rather than deleted, because `artifact_sources`
/// cascades away with a delete and takes the record of what was attempted.
///
/// For an *explicit* undo only. A merged artifact that is simply deleted is
/// handled by `heal_dangling_supersessions`, which restores what it hid — and a
/// fresh merge is then correct, because the duplication is genuinely back. A
/// decision may overrule the sweep; a deletion may not.
pub async fn undo(core: &Core, merged_id: &str) -> Result<()> {
    let m = core.store.get_artifact(merged_id).await?;
    if m.provenance != Provenance::Merged {
        return Err(crate::error::Error::Validation(format!(
            "{merged_id} is not a merged artifact"
        )));
    }

    // Everything it hid, not just its roots: an earlier merge it subsumed was
    // superseded onto it too, and leaving that hidden behind a deprecated
    // artifact is the dead end `repoint_supersession` exists to avoid.
    let restored = core.store.artifacts_superseded_by(&m.id).await?;
    for id in &restored {
        if let Err(e) = core.reactivate(id).await {
            tracing::warn!(artifact = %id, error = %e, "could not restore a merged artifact's source");
        }
    }

    // Only after they are back. The other order leaves a moment with nothing in
    // search at all, which is the window the whole write path is ordered to
    // avoid.
    core.deprecate(&m.id).await?;

    // Without this the undo lasts exactly one sweep.
    for pair in core.store.pairs_among(&restored).await? {
        core.store
            .set_pair_state(
                pair.id,
                crate::store::pairs::PairState::Dismissed,
                Some("merge undone"),
            )
            .await?;
    }
    // And by lineage, for an undo that outran the embed: before `finish` runs,
    // nothing is superseded by this merge, so `restored` above is empty — but
    // the pairs were settled the moment the merge was written, and leaving
    // them would keep the duplicates invisible to every later sweep.
    core.store
        .dismiss_pairs_merged_into(&m.id, "merge undone")
        .await?;
    tracing::info!(merged = %m.id, restored = restored.len(), "undid a merge");
    Ok(())
}

/// Retire a merge whose embedding can never arrive, and hand its pairs back
/// to a person.
///
/// Safe by the write path's own ordering: the roots are superseded only after
/// the embed lands, so a stranded merge has hidden nothing — the base is
/// exactly what it was before the verdict, plus one unindexed artifact.
/// Deprecated rather than deleted for the same reason `undo` deprecates: the
/// lineage is the record of what was attempted.
///
/// The reopened pairs go to `Contradiction`, not back to `Pending`: re-arming
/// the model would regenerate the same unembeddable draft, at full price,
/// forever.
pub async fn reap_stranded(core: &Core, merged_id: &str) -> Result<()> {
    let m = core.store.get_artifact(merged_id).await?;
    if m.provenance != Provenance::Merged
        || !m.in_results()
        || m.embed_state == crate::store::artifacts::EmbedState::Embedded
    {
        // The embed landed (or someone else acted) between the scan and
        // here. Nothing is stranded any more.
        return Ok(());
    }
    core.deprecate(&m.id).await?;
    let reopened = core
        .store
        .reopen_pairs_merged_into(
            &m.id,
            "the merged text could not be indexed; resolve by hand",
        )
        .await?;
    // The forever-retrying job was this state's only signal; with the merge
    // retired it is pure noise.
    core.store.delete_job(Stage::Embed, &m.id).await?;
    tracing::warn!(merged = %m.id, reopened, "reaped a merge that could not be embedded");
    Ok(())
}

/// Flag merged artifacts that have lost a source to a delete.
///
/// The text still carries what the deleted source said, so this is not data
/// loss — it is a claim of provenance the artifact can no longer support. The
/// detail pane says so rather than quietly showing one fewer source, which
/// would make a merge of three look like a merge of two.
///
/// Returns how many it flagged, which is worth asserting on for the same reason
/// the repairs are: a pass that fires on a base with nothing wrong is a bug
/// hiding behind a correct end state.
pub async fn flag_orphans(core: &Core) -> Result<usize> {
    let mut n = 0;
    // The scan already excludes flagged rows, so every id here is new work.
    for id in core.store.merged_missing_a_source(500).await? {
        core.store
            .set_artifact_flags(
                &id,
                &["orphaned_source".to_string()],
                Some("one of the artifacts this was written from has been deleted"),
            )
            .await?;
        n += 1;
    }
    if n > 0 {
        tracing::info!(flagged = n, "merged artifacts have lost a source");
    }
    Ok(n)
}

/// Every value and literal in `roots` that `draft` does not carry.
///
/// Empty means the merge may be written. Anything else is a merge that would
/// have lost something, and the caller escalates rather than retrying: the text
/// is what was wrong, and a person can read what it would have cost.
///
/// Both halves search the draft's text *and* its caveats. A caveat is stored,
/// rendered and recoverable, so a value demoted there has not been lost — this
/// checks for loss, not for prominence. Deciding that a value belongs in the
/// caveats rather than the body is exactly the judgement a merge is for.
pub fn losses(roots: &[Chunk], draft: &MergedDraft) -> Vec<String> {
    let mut haystack = draft.text.clone();
    for c in &draft.caveats {
        haystack.push(' ');
        haystack.push_str(c);
    }

    let have = crate::infer::facts::fact_tokens(&haystack);
    let mut out: Vec<String> = Vec::new();

    for r in roots {
        // Values: a version, a timeout, a port. The failure this catches is a
        // model answering "duplicate" and then quietly picking a side while
        // writing, which is a conflict resolved by deletion.
        for tok in crate::infer::facts::fact_tokens(&r.text) {
            if !have.contains(&tok) {
                out.push(tok);
            }
        }
        // Literals: commands, paths, flags, error strings. The existing check,
        // with the merged text as the haystack instead of the segment —
        // `missing_literals(artifact_text, caveats, haystack)` asks which
        // literals of the first argument are absent from the third, which is
        // exactly this question with the arguments in this order.
        //
        // `verify`'s module header states the stake: a paraphrased command is a
        // command that later gets pasted into a root shell.
        out.extend(crate::infer::verify::missing_literals(
            &r.text, &r.caveats, &haystack,
        ));
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::artifacts::{ArtifactStatus, EmbedState, Provenance};

    fn draft(text: &str) -> MergedDraft {
        MergedDraft {
            title: None,
            text: text.into(),
            category: None,
            tags: vec![],
            caveats: vec![],
        }
    }

    /// A captured artifact carrying only the text the checks read.
    fn root(text: &str) -> Chunk {
        Chunk {
            id: "root".into(),
            corpus_id: Some("corpus".into()),
            provenance: Provenance::Captured,
            source_count: 0,
            ordinal: 0,
            text: text.into(),
            corpus_span: None,
            title: None,
            category: None,
            tags: vec![],
            embed_state: EmbedState::Embedded,
            embed_model: None,
            created_at: 0,
            embed_rev: 0,
            segment_idx: None,
            flags: vec![],
            flag_detail: None,
            superseded_by: None,
            caveats: vec![],
            status: ArtifactStatus::Active,
            last_verified_at: None,
        }
    }

    #[test]
    fn a_merge_that_keeps_both_values_is_allowed() {
        let roots = [
            root("The request timeout is 30 seconds."),
            root("The request timeout is 90 seconds."),
        ];
        let d = draft(
            "Sources differ on the request timeout: an earlier capture gives 30 seconds, \
             a later one 90 seconds.",
        );
        assert!(losses(&roots, &d).is_empty(), "{:?}", losses(&roots, &d));
    }

    #[test]
    fn a_merge_that_drops_a_value_is_refused() {
        // The one way this feature can destroy knowledge without anyone
        // noticing: the model answers "duplicate" and quietly picks a side while
        // writing. The result reads well, ranks well, and the missing number is
        // gone from the base — a conflict resolved by deletion.
        let roots = [
            root("The request timeout is 30 seconds."),
            root("The request timeout is 90 seconds."),
        ];
        let d = draft("The request timeout is 90 seconds.");
        assert_eq!(losses(&roots, &d), vec!["30".to_string()]);
    }

    #[test]
    fn a_value_moved_into_the_caveats_is_not_lost() {
        // Caveats are stored and rendered beside the artifact, so a value
        // demoted there is still recoverable. This checks for loss, not for
        // prominence — deciding what belongs in the body is what a merge is for.
        let roots = [
            root("The request timeout is 30 seconds."),
            root("The request timeout is 90 seconds."),
        ];
        let mut d = draft("The request timeout is 90 seconds.");
        d.caveats = vec!["An earlier capture gave 30 seconds.".into()];
        assert!(losses(&roots, &d).is_empty(), "{:?}", losses(&roots, &d));
    }

    #[test]
    fn a_merge_that_paraphrases_a_command_is_refused() {
        // A paraphrased command is a command that later gets pasted into a root
        // shell. The literal check is the same one synthesis already runs, with
        // the merged text as the haystack instead of the source window.
        let roots = [
            root("Attach it with `mount --bind /src /dst`."),
            root("Bind mounts attach a directory elsewhere."),
        ];
        let d = draft("Bind mounts attach a directory elsewhere; use the bind mount option.");
        let lost = losses(&roots, &d);
        assert!(
            lost.iter().any(|l| l.contains("mount --bind")),
            "the literal check let a paraphrased command through: {lost:?}"
        );
    }

    #[test]
    fn a_merge_that_reproduces_the_command_verbatim_is_allowed() {
        let roots = [
            root("Attach it with `mount --bind /src /dst`."),
            root("Bind mounts attach a directory elsewhere."),
        ];
        let d = draft(
            "Bind mounts attach a directory elsewhere. Attach one with \
             `mount --bind /src /dst`.",
        );
        assert!(losses(&roots, &d).is_empty(), "{:?}", losses(&roots, &d));
    }

    #[test]
    fn a_literal_a_root_carried_only_in_its_caveats_still_has_to_survive() {
        // Caveats are the newest place model prose appears, and one that says to
        // run something first is a command like any other. `missing_literals`
        // already reads them on the source side; this pins that a merge cannot
        // drop them.
        let mut r = root("Mount the filesystem before writing.");
        r.caveats = vec!["Only after running `systemctl stop app`.".into()];
        let d = draft("Mount the filesystem before writing.");
        let lost = losses(&[r], &d);
        assert!(
            lost.iter().any(|l| l.contains("systemctl stop app")),
            "a caveat's command was dropped without complaint: {lost:?}"
        );
    }

    #[tokio::test]
    async fn knowledge_is_never_unreachable_during_a_merge() {
        // The write path is several steps over two stores that cannot be
        // written atomically. Indexing before superseding means the worst a
        // crash can leave is the merge and its roots all in search at once --
        // redundancy, which is the state the system was already in. The other
        // order leaves a window in which none of them is findable, and that
        // window is as long as the embed queue is deep.
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;
        let d = draft("Mount the filesystem, or attach the volume, before writing.");

        // Step one: the merge exists and nothing is hidden.
        let m = write(&core, &d, &ids).await.unwrap();
        for id in &ids {
            assert!(
                core.store
                    .get_artifact(id)
                    .await
                    .unwrap()
                    .superseded_by
                    .is_none(),
                "a root left search before the merge was indexed"
            );
        }
        let hits = core
            .vectors
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert!(
            hits.iter().any(|h| h.payload.artifact_id == ids[0]),
            "the roots are gone and the merge is not indexed yet"
        );

        // Step two: indexing the merge, which is what triggers the supersede.
        crate::jobs::embed::run(&core, &m.id).await.unwrap();

        for id in &ids {
            assert_eq!(
                core.store
                    .get_artifact(id)
                    .await
                    .unwrap()
                    .superseded_by
                    .as_deref(),
                Some(m.id.as_str()),
                "the roots were never superseded"
            );
        }
        let hits = core
            .vectors
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert!(
            hits.iter().any(|h| h.payload.artifact_id == m.id),
            "the merge never reached search"
        );
    }

    #[tokio::test]
    async fn a_merge_whose_roots_were_never_superseded_is_finished_by_the_next_sweep() {
        // A crash between indexing the merge and hiding its roots. The merge
        // looks complete from the artifact side and absent from the pair side,
        // so only a join across the lineage would ever notice.
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;
        let d = draft("Mount the filesystem, or attach the volume, before writing.");
        let m = write(&core, &d, &ids).await.unwrap();
        // Marked indexed without the arming hook, as an interrupted run leaves it.
        core.store
            .mark_embedded(&m.id, "fake-embed", 0)
            .await
            .unwrap();
        assert_eq!(
            core.store.merged_with_active_roots(10).await.unwrap(),
            vec![m.id.clone()]
        );

        crate::jobs::consolidate::run(&core).await.unwrap();

        for id in &ids {
            assert_eq!(
                core.store
                    .get_artifact(id)
                    .await
                    .unwrap()
                    .superseded_by
                    .as_deref(),
                Some(m.id.as_str())
            );
        }
    }

    #[tokio::test]
    async fn restoring_one_merge_source_survives_the_next_sweep() {
        // "Put it back" on a merge-hidden artifact used to last exactly one
        // sweep: merged_with_active_roots cannot tell a crash-interrupted
        // merge from an operator's explicit restore, and finish re-hid it.
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;
        let d = draft("Mount the filesystem, or attach the volume, before writing.");
        let m = write(&core, &d, &ids).await.unwrap();
        crate::jobs::embed::run(&core, &m.id).await.unwrap();
        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .is_some(),
            "the merge never finished; the test setup is wrong"
        );

        core.unsupersede(&ids[0]).await.unwrap();

        crate::jobs::consolidate::run(&core).await.unwrap();

        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .is_none(),
            "the sweep re-hid an artifact an operator had explicitly restored"
        );
        // The rest of the merge is untouched: the other source stays hidden,
        // the merge stays active.
        assert!(
            core.store
                .get_artifact(&ids[1])
                .await
                .unwrap()
                .superseded_by
                .is_some()
        );
        assert_eq!(
            core.store.get_artifact(&m.id).await.unwrap().status,
            ArtifactStatus::Active
        );
    }

    #[tokio::test]
    async fn a_merge_of_a_merge_is_written_from_the_captured_roots() {
        // The anti-drift rule end to end. M1(a,b) merged with c is written from
        // a, b and c -- never from M1's text, which is itself a rewrite.
        // Otherwise each generation paraphrases a paraphrase and the originals
        // drift further away with every pass.
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])],
        )
        .await;
        let m1 = write(&core, &draft("a text and b text"), &ids)
            .await
            .unwrap();
        crate::jobs::embed::run(&core, &m1.id).await.unwrap();

        let c =
            crate::jobs::consolidate::tests::seed_into_new_corpus(&core, "c text", [0.94, 0.34])
                .await;
        let m2 = write(
            &core,
            &draft("a text and b text and c text"),
            &[m1.id.clone(), c.clone()],
        )
        .await
        .unwrap();
        crate::jobs::embed::run(&core, &m2.id).await.unwrap();

        let roots = core
            .store
            .roots_of(std::slice::from_ref(&m2.id))
            .await
            .unwrap();
        let mut got = roots[&m2.id].clone();
        got.sort();
        let mut want = vec![ids[0].clone(), ids[1].clone(), c];
        want.sort();
        assert_eq!(
            got, want,
            "the second merge did not flatten to captured roots"
        );
        assert!(
            !got.contains(&m1.id),
            "a merged artifact was recorded as a root of another merge"
        );
    }

    #[tokio::test]
    async fn a_merge_that_subsumes_an_earlier_one_hides_it() {
        // Superseding only the roots leaves the earlier merge active and
        // near-identical to the new one, so the relate unit re-pairs them on
        // every sweep and the two churn against each other forever. This is the
        // gap the design did not cover; `subsumed_merges` closes it.
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])],
        )
        .await;
        let m1 = write(&core, &draft("a text and b text"), &ids)
            .await
            .unwrap();
        crate::jobs::embed::run(&core, &m1.id).await.unwrap();

        let c =
            crate::jobs::consolidate::tests::seed_into_new_corpus(&core, "c text", [0.94, 0.34])
                .await;
        let m2 = write(
            &core,
            &draft("a text and b text and c text"),
            &[m1.id.clone(), c.clone()],
        )
        .await
        .unwrap();
        crate::jobs::embed::run(&core, &m2.id).await.unwrap();

        assert_eq!(
            core.store
                .get_artifact(&m1.id)
                .await
                .unwrap()
                .superseded_by
                .as_deref(),
            Some(m2.id.as_str()),
            "the subsumed merge is still active and will be re-paired forever"
        );
        // And no chain: every hidden artifact points at something that is
        // itself in results. `root -> m1 -> m2` would leave the reader who
        // opens a root at an artifact that is not in results either, and
        // nothing in the UI can follow the second hop.
        for id in ids.iter().chain(std::iter::once(&c)) {
            let winner = core
                .store
                .get_artifact(id)
                .await
                .unwrap()
                .superseded_by
                .expect("every member should be hidden by now");
            assert_eq!(winner, m2.id, "{id} was left pointing at the older merge");
            assert!(
                core.store
                    .get_artifact(&winner)
                    .await
                    .unwrap()
                    .superseded_by
                    .is_none(),
                "{id} points at a winner that is itself superseded"
            );
        }
    }

    #[tokio::test]
    async fn undoing_a_merge_survives_the_next_sweep() {
        // Restoring the sources alone accomplishes nothing: the sweep re-finds
        // them, the model reaches the same verdict, and the operator's decision
        // is silently undone on the next tick. Dismissing the pairs is what
        // makes it stick, and it is the half that is easy to leave out --
        // literally the same bug as
        // reactivating_a_superseded_artifact_survives_the_next_sweep.
        let mut core = crate::core::test_support::test_core().await;
        core.consolidate.autonomous = true;
        core.judge = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"relation":"duplicate","detail":"same claim",
                "merged":{"text":"Mount the filesystem, or attach the volume, before writing.",
                          "tags":[],"caveats":[]}}"#
                .into(),
        ]));
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;
        core.store
            .record_pair(&ids[0], &ids[1], 0.91)
            .await
            .unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()[0]
            .id;
        crate::jobs::dedupe::run(&core, &pair.to_string())
            .await
            .unwrap();
        let merged_id = core
            .store
            .merged_artifacts(10)
            .await
            .unwrap()
            .first()
            .map(|c| c.id.clone())
            .unwrap_or_else(|| panic!("no merge was written"));
        crate::jobs::embed::run(&core, &merged_id).await.unwrap();

        undo(&core, &merged_id).await.unwrap();

        for id in &ids {
            let c = core.store.get_artifact(id).await.unwrap();
            assert_eq!(c.status, ArtifactStatus::Active);
            assert!(c.superseded_by.is_none());
        }
        assert_eq!(
            core.store.get_artifact(&merged_id).await.unwrap().status,
            ArtifactStatus::Deprecated,
            "the merge was deleted, taking its lineage with it"
        );
        assert_eq!(
            core.store
                .pairs_by_state(crate::store::pairs::PairState::Dismissed, 10)
                .await
                .unwrap()
                .len(),
            1,
            "the pair was left answerable, so the sweep will merge it again"
        );

        // The point of the whole test: it survives.
        crate::jobs::consolidate::run(&core).await.unwrap();
        for id in &ids {
            assert!(
                core.store
                    .get_artifact(id)
                    .await
                    .unwrap()
                    .superseded_by
                    .is_none(),
                "the sweep merged the pair again after an explicit undo"
            );
        }
    }

    #[tokio::test]
    async fn an_already_flagged_merge_does_not_occupy_the_orphan_scan() {
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])],
        )
        .await;
        write(&core, &draft("a text and b text"), &ids)
            .await
            .unwrap();
        core.store.delete_artifact(&ids[0]).await.unwrap();
        assert_eq!(flag_orphans(&core).await.unwrap(), 1);
        // Flagged rows leave the scan entirely — not fetched and skipped in
        // Rust, which is what let 500 of them starve every newer orphan out
        // of the LIMIT.
        assert!(
            core.store
                .merged_missing_a_source(500)
                .await
                .unwrap()
                .is_empty(),
            "a flagged merge still occupies a scan slot"
        );
    }

    #[tokio::test]
    async fn reviewing_an_orphaned_merge_is_not_undone_by_the_next_sweep() {
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])],
        )
        .await;
        let m = write(&core, &draft("a text and b text"), &ids)
            .await
            .unwrap();
        core.store.delete_artifact(&ids[0]).await.unwrap();
        assert_eq!(flag_orphans(&core).await.unwrap(), 1);

        // What mark_artifact_reviewed does for an orphaned merge.
        core.store.accept_source_loss(&m.id).await.unwrap();
        core.store.clear_artifact_flags(&m.id).await.unwrap();

        assert_eq!(
            flag_orphans(&core).await.unwrap(),
            0,
            "the sweep re-flagged a merge the operator had reviewed"
        );
        assert!(
            core.store
                .get_artifact(&m.id)
                .await
                .unwrap()
                .flags
                .is_empty()
        );
    }

    #[tokio::test]
    async fn undoing_a_merge_before_its_embed_lands_still_releases_its_pairs() {
        // Pairs are settled at write time; roots are superseded at embed time.
        // An undo in between used to find nothing superseded, dismiss nothing,
        // and leave the pairs no_conflict behind a deprecated merge — both
        // duplicates active, and record_pair unable to ever re-file them.
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])],
        )
        .await;
        core.store
            .record_pair(&ids[0], &ids[1], 0.91)
            .await
            .unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()[0]
            .id;

        let m = write(&core, &draft("a text and b text"), &ids)
            .await
            .unwrap();
        core.store
            .set_pair_merged(pair, &m.id, Some("duplicate"))
            .await
            .unwrap();
        // No embed ran: nothing is superseded by m yet.

        undo(&core, &m.id).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert_eq!(
            p.state,
            crate::store::pairs::PairState::Dismissed,
            "the pair stayed settled behind a deprecated merge: {p:?}"
        );
        assert_eq!(p.merged_into, None);
    }

    #[tokio::test]
    async fn deleting_a_source_flags_the_merge_rather_than_hiding_the_loss() {
        // The cascade removes the lineage row while the merged text still
        // carries that source's content, so the artifact claims less provenance
        // than it has. Not data loss, but a silent untruth: a merge of three
        // would quietly render as a merge of two.
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])],
        )
        .await;
        let m = write(&core, &draft("a text and b text"), &ids)
            .await
            .unwrap();

        assert_eq!(
            flag_orphans(&core).await.unwrap(),
            0,
            "the flag fired on a merge with all its sources"
        );

        core.store.delete_artifact(&ids[0]).await.unwrap();
        assert_eq!(flag_orphans(&core).await.unwrap(), 1);

        let flagged = core.store.get_artifact(&m.id).await.unwrap();
        assert!(
            flagged.flags.iter().any(|f| f == "orphaned_source"),
            "{flagged:?}"
        );
        // And it does not keep re-flagging the same artifact every sweep.
        assert_eq!(flag_orphans(&core).await.unwrap(), 0);
    }

    #[test]
    fn a_merge_of_three_roots_is_checked_against_all_of_them() {
        // The fan-in cap allows up to eight. A check that only read the first
        // two would pass a merge that dropped everything the third said.
        let roots = [
            root("Port 8080 is the default."),
            root("The timeout is 30s."),
            root("Retries are capped at 5."),
        ];
        let d = draft("Port 8080 is the default and the timeout is 30s.");
        assert_eq!(losses(&roots, &d), vec!["5".to_string()]);
    }
}
