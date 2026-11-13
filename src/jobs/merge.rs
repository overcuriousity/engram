//! Writing, verifying and undoing a merge. `losses` stands between a verdict and a write: a merged
//! paragraph reads and ranks as well without the literal it dropped.

use crate::core::Core;
use crate::error::Result;
use crate::infer::prompt::MergedDraft;
use crate::store::artifacts::{Chunk, NewMerged, Provenance};
use crate::store::jobs::Stage;

/// Its roots are not superseded here, only once it is indexed (`finish`): a crash can then leave
/// redundancy, never a gap in search as long as the embed queue.
pub async fn write(core: &Core, draft: &MergedDraft, roots: &[String]) -> Result<Chunk> {
    let m = core
        .store
        .insert_merged_artifact(
            &NewMerged {
                text: draft.text.clone(),
                title: draft.title.clone(),
                category: draft.category.clone(),
                tags: carried_tags(core, draft, roots).await,
                caveats: draft.caveats.clone(),
            },
            roots,
        )
        .await?;
    core.store.enqueue(Stage::Embed, "artifact", &m.id).await?;
    tracing::info!(merged = %m.id, sources = roots.len(), "wrote a merged artifact");
    Ok(m)
}

/// The draft's tags plus every source's: tags are hand-pinned filing that `finish` would otherwise
/// make unreachable. An unreadable source is skipped, not fatal.
async fn carried_tags(core: &Core, draft: &MergedDraft, roots: &[String]) -> Vec<String> {
    let mut tags = draft.tags.clone();
    for id in roots {
        let Ok(r) = core.store.get_artifact(id).await else {
            continue;
        };
        for t in r.tags {
            if !tags.contains(&t) {
                tags.push(t);
            }
        }
    }
    tags
}

/// Called from `mark_indexed`, and from the sweep for merges whose process died in between.
pub async fn finish(core: &Core, merged_id: &str) -> Result<()> {
    let m = core.store.get_artifact(merged_id).await?;
    if m.provenance != Provenance::Merged || !m.in_results() {
        return Ok(());
    }

    // `roots_to_hide`: a root an operator restored out of this merge must stay restored on every
    // re-run.
    let hiding = core.store.roots_to_hide(&m.id).await?;
    for root in &hiding {
        let Ok(r) = core.store.get_artifact(root).await else {
            continue;
        };
        if !r.in_results() {
            continue;
        }
        // An operator deprecating a root mid-loop is an ordinary race; the sweep reaches what is
        // left.
        crate::jobs::try_supersede(core, root, &m.id, "a merged artifact's root").await;
    }

    // A subsumed earlier merge left active would be re-paired with this one forever.
    let subsumed = core.store.subsumed_merges(&m.id).await?;
    for older in &subsumed {
        // Re-point first, or `root -> older -> m` is a chain whose middle is out of results.
        for hidden in core.store.artifacts_superseded_by(older).await? {
            if let Err(e) = core.repoint_supersession(&hidden, &m.id).await {
                tracing::warn!(artifact = %hidden, to = %m.id, error = %e,
                    "could not re-point a supersession; it still names a hidden winner");
            }
        }
        crate::jobs::try_supersede(core, older, &m.id, "a merge this one subsumes").await;
    }

    // Its members' open pairs now concern the merge. Here, not in `write`: before its sources are
    // hidden, a further merge could leave them active under a hidden middle. Failures only warn.
    let mut swallowed = hiding;
    swallowed.extend(subsumed);
    match core.store.repoint_open_pairs(&swallowed, &m.id).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(merged = %m.id, pairs = n, "moved open pairs onto a merge"),
        Err(e) => {
            tracing::warn!(merged = %m.id, error = %e, "could not move open pairs onto a merge")
        }
    }
    Ok(())
}

/// Restore what it replaced, retire it, and dismiss its pairs, or the next sweep redoes it
/// (`record_pair` is `INSERT OR IGNORE`). Deprecated, not deleted, to keep the lineage; explicit
/// undo only.
pub async fn undo(
    core: &Core,
    merged_id: &str,
    by: crate::store::pairs::DecidedBy,
) -> Result<Undone> {
    let m = core.store.get_artifact(merged_id).await?;
    if m.provenance != Provenance::Merged {
        return Err(crate::error::Error::Validation(format!(
            "{merged_id} is not a merged artifact"
        )));
    }

    // A merge hidden behind a later one: `deprecate_with` refuses it and its roots now point at the
    // later merge. Report that nothing was done, not `Err`, which would wedge rule 1's idle pass.
    if let Some(later) = m.superseded_by.clone() {
        tracing::info!(
            merged = %m.id,
            later = %later,
            "the merge is hidden behind a later one; leaving it, and its journal rows, alone"
        );
        return Ok(Undone::HiddenBehind { later });
    }

    // Everything it hid, including a subsumed earlier merge.
    let restored = core.store.artifacts_superseded_by(&m.id).await?;
    for id in &restored {
        if let Err(e) = core.reactivate(id).await {
            tracing::warn!(artifact = %id, error = %e, "could not restore a merged artifact's source");
        }
    }

    // Only after they are back, so search is never empty of it.
    core.deprecate(&m.id).await?;

    // Without this the undo lasts exactly one sweep.
    for pair in core.store.pairs_among(&restored).await? {
        core.store
            .set_pair_state(
                pair.id,
                crate::store::pairs::PairState::Dismissed,
                Some("merge undone"),
                by,
            )
            .await?;
    }
    // By lineage too: before `finish`, nothing is superseded by this merge but the pairs are
    // settled.
    core.store
        .dismiss_pairs_merged_into(&m.id, "merge undone", by)
        .await?;
    tracing::info!(merged = %m.id, restored = restored.len(), "undid a merge");
    Ok(Undone::TakenApart)
}

/// Callers stamp the merge's journal rows undone, and rule 1 never re-reads a closed row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Undone {
    TakenApart,
    /// A later merge subsumed this one; carries the merge standing in front of it.
    HiddenBehind {
        later: String,
    },
}

impl Undone {
    /// Whether the journal rows that recorded this merge may be stamped undone.
    pub fn happened(&self) -> bool {
        matches!(self, Undone::TakenApart)
    }
}

/// Safe because the roots are superseded only after the first embed. Pairs go to `NoConflict`, not
/// `Pending`, which would regenerate the same unembeddable draft forever.
pub async fn reap_stranded(core: &Core, merged_id: &str) -> Result<()> {
    let m = core.store.get_artifact(merged_id).await?;
    if m.provenance != Provenance::Merged
        || !m.in_results()
        || m.embed_state == crate::store::artifacts::EmbedState::Embedded
    {
        return Ok(());
    }
    // The ordering argument covers only a first embed; one that already hid its roots is not
    // stranded.
    if !core.store.artifacts_superseded_by(&m.id).await?.is_empty() {
        return Ok(());
    }
    core.deprecate(&m.id).await?;
    let reopened = core
        .store
        .reopen_pairs_merged_into(
            &m.id,
            "the merged text could not be indexed; both stay as they are",
        )
        .await?;
    core.store.delete_job(Stage::Embed, &m.id).await?;
    tracing::warn!(merged = %m.id, reopened, "reaped a merge that could not be embedded");
    Ok(())
}

/// Accept merges that lost a source to a delete: the text still carries it. Returns how many, since
/// a pass that fires on a clean base is a bug.
pub async fn flag_orphans(core: &Core) -> Result<usize> {
    let mut n = 0;
    for id in core.store.artifacts_flagged("orphaned_source", 500).await? {
        core.store.accept_source_loss(&id).await?;
        core.store.clear_artifact_flags(&id).await?;
        n += 1;
    }
    // An accepted row leaves the scan, so every id here is new work.
    for id in core.store.merged_missing_a_source(500).await? {
        core.store.accept_source_loss(&id).await?;
        n += 1;
    }
    if n > 0 {
        tracing::info!(accepted = n, "merged artifacts have lost a source");
    }
    Ok(n)
}

/// Every machine literal in `roots` that `draft` does not carry. Caveats count: a literal demoted
/// there is stored and recoverable. Values are not checked; that is the judge's.
pub fn losses(roots: &[Chunk], draft: &MergedDraft) -> Vec<String> {
    let mut haystack = draft.text.clone();
    for c in &draft.caveats {
        haystack.push(' ');
        haystack.push_str(c);
    }

    let mut out: Vec<String> = Vec::new();

    for r in roots {
        // Machine-shaped literals only: merged text is a rewrite, often across languages, and the
        // source-window rules read a four-space indent as code.
        out.extend(crate::infer::verify::missing_machine_literals(
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

    #[tokio::test]
    async fn a_merge_keeps_the_tags_its_sources_were_filed_under() {
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])],
        )
        .await;
        core.store
            .update_artifact_tags(&ids[0], &["pinned".into(), "shared".into()])
            .await
            .unwrap();
        core.store
            .update_artifact_tags(&ids[1], &["shared".into(), "runbook".into()])
            .await
            .unwrap();

        let m = write(&core, &draft("a text and b text"), &ids)
            .await
            .unwrap();

        assert_eq!(m.tags, vec!["pinned", "shared", "runbook"]);
    }

    #[tokio::test]
    async fn a_merge_hidden_behind_a_later_one_does_not_wedge_the_idle_pass() {
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])],
        )
        .await;
        let older = write(&core, &draft("a text and b text"), &ids)
            .await
            .unwrap();
        let newer = write(&core, &draft("a text and b text, restated"), &ids)
            .await
            .unwrap();
        // What `finish` does when a newer merge subsumes an older one.
        core.supersede(&older.id, &newer.id).await.unwrap();
        assert!(
            core.store
                .get_artifact(&older.id)
                .await
                .unwrap()
                .superseded_by
                .is_some()
        );

        undo(&core, &older.id, crate::store::pairs::DecidedBy::Evidence)
            .await
            .expect("an already-hidden merge wedged the whole idle pass");

        assert!(
            core.store
                .get_artifact(&older.id)
                .await
                .unwrap()
                .superseded_by
                .is_some(),
            "the later merge's claim on it was thrown away"
        );
        assert_eq!(
            core.store.get_artifact(&older.id).await.unwrap().status,
            ArtifactStatus::Superseded,
            "it was left in a state the rest of the app cannot render"
        );
    }

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
            cues: vec![],
            retired_at: None,
            reaped_at: None,
        }
    }

    #[test]
    fn a_merge_that_keeps_both_values_is_allowed() {
        let roots = [
            root("The request timeout is 30s."),
            root("The request timeout is 90s."),
        ];
        let d = draft(
            "Sources differ on the request timeout: an earlier capture gives 30s, \
             a later one 90s.",
        );
        assert!(losses(&roots, &d).is_empty(), "{:?}", losses(&roots, &d));
    }

    #[test]
    fn a_merge_that_drops_a_literal_is_refused() {
        let roots = [
            root("Mount it read-only with `mount -o ro /dev/sdb1 /mnt/case`."),
            root("Unmount it again with `umount /mnt/case`."),
        ];
        let d = draft("Unmount it again with `umount /mnt/case`.");
        assert!(
            losses(&roots, &d).contains(&"mount -o ro /dev/sdb1 /mnt/case".to_string()),
            "{:?}",
            losses(&roots, &d)
        );
    }

    #[test]
    fn a_dropped_value_is_no_longer_caught_and_that_is_the_trade() {
        // Values are no longer checked; the judge answers "conflict" when both cannot be kept.
        let roots = [
            root("The request timeout is 30s."),
            root("The request timeout is 90s."),
        ];
        assert!(losses(&roots, &draft("The request timeout is 90s.")).is_empty());
    }

    #[test]
    fn a_value_moved_into_the_caveats_is_not_lost() {
        let roots = [
            root("The request timeout is 30 seconds."),
            root("The request timeout is 90 seconds."),
        ];
        let mut d = draft("The request timeout is 90 seconds.");
        d.caveats = vec!["An earlier capture gave 30 seconds.".into()];
        assert!(losses(&roots, &d).is_empty(), "{:?}", losses(&roots, &d));
    }

    #[test]
    fn a_merge_may_reword_an_indented_bullet_list() {
        // Pair 481 from a live base: German and English, the English list indented four spaces.
        let roots = [
            root(
                "Log Dateien befinden sich im Ordner \
                 \"\\Users\\<USERNAME>\\AppData\\Local\\Microsoft\\OneDrive\\logs\". \
                 Der Ordner Personal enthält die Datei SyncEngine.odl, welche logs der \
                 Synchronisation enthält, und die Datei SyncDiagnostics.txt.",
            ),
            root(
                "Standard OneDrive locations and log files:\n\
                 * Personal directory contents:\n\
                 \x20   * SyncEngine.odl: Logs of synchronized files and file hashes.\n\
                 \x20   * Trace.ETL: TraceCurrent.ETL and TraceArchive.ETL for activity tracking.\n\
                 \x20   * SyncDiagnostics.txt: Log of current synchronization content.",
            ),
        ];
        let d = draft(
            "OneDrive log files live in \\Users\\<USERNAME>\\AppData\\Local\\Microsoft\\OneDrive\\logs, \
             which has the subdirectories Common and Personal. Personal holds SyncEngine.odl \
             (synchronized files and their hashes), Trace.ETL (TraceCurrent.ETL and \
             TraceArchive.ETL, for activity tracking) and SyncDiagnostics.txt (current \
             synchronization content).",
        );
        assert!(
            losses(&roots, &d).is_empty(),
            "a correct merge was refused for rewording prose: {:?}",
            losses(&roots, &d)
        );
    }

    #[test]
    fn a_merge_that_drops_a_path_is_still_refused() {
        // Pair 486: a real model error that must stay caught.
        let roots = [
            root("xmount --in ewf --out dd *.E?? /mnt stellt das Image unter /mnt bereit."),
            root(
                "Im Mountpoint /tmp befindet sich jetzt die Datei image.vdi welche direkt \
                 in VirtualBox über den Mountpoint /tmp/image.vdi eingebunden werden kann.",
            ),
        ];
        let d = draft("xmount --in ewf --out dd *.E?? /mnt stellt das Image unter /mnt bereit.");
        let lost = losses(&roots, &d);
        assert!(
            lost.iter().any(|l| l.contains("/tmp/image.vdi")),
            "the merge dropped the path the second artifact exists for: {lost:?}"
        );
    }

    #[test]
    fn a_merge_that_paraphrases_a_command_is_refused() {
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
        // Written from a, b and c, never from M1's text, which is itself a rewrite.
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
    async fn a_merge_takes_its_roots_verdicts_with_it() {
        use crate::store::pairs::PairState;
        let mut core = crate::core::test_support::test_core().await;
        core.pair_synthesizer = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"merged":{"title":"Mounting","text":"Mount the filesystem, or attach the volume, before writing.",
                          "category":"procedure","caveats":[]}}"#
                    .into(),
            ]),
        ));
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
                ("Never mount the filesystem before writing.", [0.1, 0.99]),
            ],
        )
        .await;
        core.store.record_pair(&ids[0], &ids[2], 0.6).await.unwrap();
        let verdict = core
            .store
            .pair_between(&ids[0], &ids[2])
            .await
            .unwrap()
            .unwrap()
            .id;
        core.store
            .set_pair_state(
                verdict,
                PairState::Contradiction,
                Some("mount vs never mount"),
                crate::store::pairs::DecidedBy::Model,
            )
            .await
            .unwrap();
        core.store
            .record_pair(&ids[0], &ids[1], 0.91)
            .await
            .unwrap();
        let dupe = core
            .store
            .pair_between(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap()
            .id;

        core.store.ask_pair_synthesis(dupe).await.unwrap();
        crate::jobs::dedupe::run(&core, &dupe.to_string())
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
        // `finish` runs off the back of the embed, not the verdict.
        crate::jobs::embed::run(&core, &merged_id).await.unwrap();

        let p = core.store.get_pair(verdict).await.unwrap();
        assert_eq!(
            p.state,
            PairState::Contradiction,
            "the verdict was thrown away"
        );
        assert!(
            p.a_id == merged_id || p.b_id == merged_id,
            "the verdict still names a root that is out of results"
        );
        assert!(
            p.detail.unwrap_or_default().contains("carried over"),
            "nothing says the verdict was pronounced over a different artifact"
        );
    }

    #[tokio::test]
    async fn undoing_a_merge_survives_the_next_sweep() {
        let mut core = crate::core::test_support::test_core().await;
        core.pair_synthesizer = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"merged":{"title":"Mounting","text":"Mount the filesystem, or attach the volume, before writing.",
                          "category":"procedure","caveats":[]}}"#
                    .into(),
            ]),
        ));
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
        core.store.ask_pair_synthesis(pair).await.unwrap();
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

        undo(&core, &merged_id, crate::store::pairs::DecidedBy::Operator)
            .await
            .unwrap();

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
    async fn an_accepted_merge_does_not_occupy_the_orphan_scan() {
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
        // Filtered in SQL, so 500 accepted rows cannot starve newer orphans out of the LIMIT.
        assert!(
            core.store
                .merged_missing_a_source(500)
                .await
                .unwrap()
                .is_empty(),
            "an accepted merge still occupies a scan slot"
        );
    }

    #[tokio::test]
    async fn a_merge_an_older_base_flagged_is_accepted_and_unflagged() {
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
        core.store
            .set_artifact_flags(
                &m.id,
                &["orphaned_source".to_string()],
                Some("one of the artifacts this was written from has been deleted"),
            )
            .await
            .unwrap();

        assert_eq!(flag_orphans(&core).await.unwrap(), 1);

        assert!(
            core.store
                .get_artifact(&m.id)
                .await
                .unwrap()
                .flags
                .is_empty(),
            "the old flag still waits on a person"
        );
        assert!(
            core.store
                .merged_missing_a_source(500)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(flag_orphans(&core).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn undoing_a_merge_before_its_embed_lands_still_releases_its_pairs() {
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
            .set_pair_merged(
                pair,
                &m.id,
                Some("duplicate"),
                crate::store::pairs::DecidedBy::Model,
            )
            .await
            .unwrap();
        // No embed ran: nothing is superseded by m yet.

        undo(&core, &m.id, crate::store::pairs::DecidedBy::Operator)
            .await
            .unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert_eq!(
            p.state,
            crate::store::pairs::PairState::Dismissed,
            "the pair stayed settled behind a deprecated merge: {p:?}"
        );
        assert_eq!(p.merged_into, None);
        assert_eq!(
            p.decided_by,
            Some(crate::store::pairs::DecidedBy::Operator),
            "an undo a person pressed was recorded as nobody's: {p:?}"
        );
    }

    #[tokio::test]
    async fn deleting_a_source_accepts_the_merge_as_what_remains() {
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

        let accepted = core.store.get_artifact(&m.id).await.unwrap();
        assert!(accepted.flags.is_empty(), "{accepted:?}");
        assert_eq!(flag_orphans(&core).await.unwrap(), 0);
    }

    #[test]
    fn a_merge_of_three_roots_is_checked_against_all_of_them() {
        let roots = [
            root("The config lives at /etc/engram/config.toml."),
            root("Pass --dry-run first."),
            root("The socket is at /run/engram/engram.sock."),
        ];
        let d = draft("The config lives at /etc/engram/config.toml; pass --dry-run first.");
        assert_eq!(
            losses(&roots, &d),
            vec!["/run/engram/engram.sock".to_string()]
        );
    }

    #[tokio::test]
    async fn finishing_a_merge_moves_its_members_open_pairs_onto_it() {
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[
                ("a text", [1.0, 0.0]),
                ("b text", [0.99, 0.05]),
                ("c text", [0.93, 0.37]),
            ],
        )
        .await;
        core.store
            .record_pair(&ids[1], &ids[2], 0.91)
            .await
            .unwrap();
        let m = write(
            &core,
            &draft("a text and b text"),
            &[ids[0].clone(), ids[1].clone()],
        )
        .await
        .unwrap();

        finish(&core, &m.id).await.unwrap();

        assert_eq!(
            core.store.pair_state_between(&ids[2], &m.id).await.unwrap(),
            Some(crate::store::pairs::PairState::Pending),
            "the surviving duplicate has no question against the merge"
        );
    }

    #[tokio::test]
    async fn re_pointing_is_safe_to_run_twice() {
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[
                ("a text", [1.0, 0.0]),
                ("b text", [0.99, 0.05]),
                ("c text", [0.93, 0.37]),
            ],
        )
        .await;
        core.store
            .record_pair(&ids[1], &ids[2], 0.91)
            .await
            .unwrap();
        let m = write(
            &core,
            &draft("a text and b text"),
            &[ids[0].clone(), ids[1].clone()],
        )
        .await
        .unwrap();

        finish(&core, &m.id).await.unwrap();
        finish(&core, &m.id).await.unwrap();

        assert_eq!(
            core.store
                .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1,
            "a second finish duplicated the question"
        );
    }

    #[tokio::test]
    async fn undoing_a_merge_that_a_later_one_hides_does_nothing_and_says_so() {
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])],
        )
        .await;
        let older = write(&core, &draft("a text and b text"), &ids)
            .await
            .unwrap();
        finish(&core, &older.id).await.unwrap();
        let newer = write(
            &core,
            &draft("all three texts"),
            std::slice::from_ref(&older.id),
        )
        .await
        .unwrap();
        finish(&core, &newer.id).await.unwrap();

        let hidden = core.store.get_artifact(&older.id).await.unwrap();
        assert_eq!(
            hidden.superseded_by.as_deref(),
            Some(newer.id.as_str()),
            "the fixture stopped hiding the older merge behind the newer one"
        );

        let outcome = undo(&core, &older.id, crate::store::pairs::DecidedBy::Operator)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            Undone::HiddenBehind {
                later: newer.id.clone()
            },
            "an undo that changed nothing reported that it had"
        );
        assert!(!outcome.happened());

        let after = core.store.get_artifact(&older.id).await.unwrap();
        assert_eq!(after.superseded_by, hidden.superseded_by);
        assert_eq!(
            after.status, hidden.status,
            "the older merge was deprecated by an undo that did nothing else"
        );
    }
}
