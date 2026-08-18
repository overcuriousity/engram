//! Which captured artifacts a merged artifact is made of.
//!
//! Stored as the resolved closure rather than as parent edges. The re-merge
//! rule needs the *captured* roots of every candidate on every decision — a
//! merge is always written from originals, never from text an earlier merge
//! produced, which is what keeps information loss one generation deep however
//! many times a group is merged. Walking edges with a recursive CTE would put a
//! graph traversal on the sweep's hot path for an answer that never changes
//! once written.
//!
//! The cost of the denormalisation is that a deleted root removes closure rows
//! an edge table could have recomputed. `source_count` on the artifact is what
//! notices: it records how many sources the merge was written from, so a merge
//! that has lost one can be flagged rather than quietly claiming less
//! provenance than its text carries.

use super::Store;
use crate::error::Result;
use crate::store::pairs::ArtifactPair;
use sqlx::Row;
use std::collections::BTreeMap;

impl Store {
    /// The captured roots of each of `artifact_ids`, keyed by the input id.
    ///
    /// A captured artifact is its own root — the dedupe pass asks for the roots
    /// of every component member without caring which kind it is, and answering
    /// "none" for a captured one would silently drop it from the very prompt it
    /// is supposed to be in.
    ///
    /// A merged artifact resolves through `artifact_sources`, which already
    /// holds the closure, so this is one query per id and not a traversal.
    ///
    /// A merged artifact with no lineage rows — every root deleted out from
    /// under it — resolves to the *empty* list, never to itself. Its text is a
    /// synthesis, and handing it back as a root is how a paraphrase of a
    /// paraphrase ends up in a prompt as an original, or recorded as another
    /// merge's `root_id`. The empty answer makes that state visible to the
    /// caller instead; the dedupe unit escalates such a component to a person.
    pub async fn roots_of(&self, artifact_ids: &[String]) -> Result<BTreeMap<String, Vec<String>>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for id in artifact_ids {
            let rows = sqlx::query(
                "SELECT root_id FROM artifact_sources WHERE child_id = ? ORDER BY root_id",
            )
            .bind(id)
            .fetch_all(&self.pool)
            .await?;
            let roots: Vec<String> = rows.iter().map(|r| r.get("root_id")).collect();
            let entry = if roots.is_empty() {
                let provenance: Option<String> =
                    sqlx::query_scalar("SELECT provenance FROM artifacts WHERE id = ?")
                        .bind(id)
                        .fetch_optional(&self.pool)
                        .await?;
                if provenance.as_deref() == Some("merged") {
                    vec![]
                } else {
                    vec![id.clone()]
                }
            } else {
                roots
            };
            out.insert(id.clone(), entry);
        }
        Ok(out)
    }

    /// Merged artifacts still standing, newest first. What Ops lists.
    ///
    /// Standing is the whole of the filter, and it is not cosmetic: every row
    /// here is rendered with an "Undo merge" button, and undoing is defined as
    /// restoring what the merge hid and then deprecating it. A merge that was
    /// already undone is deprecated and hid nothing that is still hidden; one
    /// subsumed by a later merge is superseded, and its sources belong to that
    /// later merge now. Pressing undo on either finds nothing to restore and
    /// deprecates an artifact that is already gone from results.
    pub async fn merged_artifacts(
        &self,
        limit: i64,
    ) -> Result<Vec<crate::store::artifacts::Chunk>> {
        let rows = sqlx::query(
            "SELECT * FROM artifacts
              WHERE provenance = 'merged'
                AND status = 'active'
                AND superseded_by IS NULL
              ORDER BY created_at DESC, id LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(crate::store::artifacts::row_to_artifact)
            .collect())
    }

    /// Merged artifacts at least one of whose roots is still active.
    ///
    /// The write path indexes a merged artifact before superseding its roots,
    /// so this is precisely the state a crash between those two steps leaves
    /// behind. Nothing else would see it: the merge looks finished from the
    /// artifact side and absent from the pair side, and only a join across the
    /// lineage says otherwise.
    ///
    /// A root an operator explicitly restored (`restored = 1`) is not an
    /// unfinished merge, and the repair must not see it — re-hiding it undid
    /// the operator's decision on every sweep.
    pub async fn merged_with_active_roots(&self, limit: i64) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT DISTINCT s.child_id FROM artifact_sources s
               JOIN artifacts child ON child.id = s.child_id
               JOIN artifacts root  ON root.id  = s.root_id
              WHERE child.provenance = 'merged'
                AND child.status = 'active'
                AND child.superseded_by IS NULL
                AND child.embed_state = 'embedded'
                AND root.status = 'active'
                AND root.superseded_by IS NULL
                AND s.restored = 0
              LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get("child_id")).collect())
    }

    /// Record that an operator explicitly restored this captured root: no
    /// repair may hide it behind a merge again. Every merge naming it, not
    /// one — the operator's decision is about the root, not about a lineage
    /// edge. A *new* merge decision may still hide it: `insert_merged_artifact`
    /// writes fresh rows with `restored = 0`, and that is new evidence rather
    /// than a repair of old state.
    pub async fn mark_source_restored(&self, root_id: &str) -> Result<()> {
        sqlx::query("UPDATE artifact_sources SET restored = 1 WHERE root_id = ?")
            .bind(root_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The roots `merge::finish` is allowed to hide: the lineage minus every
    /// root an operator explicitly restored. Distinct from `roots_of`, which
    /// answers "what was this written from" and must keep seeing the true
    /// closure.
    pub async fn roots_to_hide(&self, child_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT root_id FROM artifact_sources
              WHERE child_id = ? AND restored = 0 ORDER BY root_id",
        )
        .bind(child_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get("root_id")).collect())
    }

    /// Active merged artifacts whose embedding can no longer arrive: no live
    /// embed job below the retry ceiling. The write path settles the pairs the
    /// moment the merge is written, so a merge stuck here is invisible to
    /// search, its roots were never superseded, and nothing else would notice
    /// — the only signal was a forever-retrying job.
    pub async fn stranded_merges(&self, limit: i64) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT a.id FROM artifacts a
              WHERE a.provenance = 'merged'
                AND a.status = 'active'
                AND a.superseded_by IS NULL
                AND a.embed_state != 'embedded'
                AND NOT EXISTS (
                      SELECT 1 FROM jobs j
                       WHERE j.stage = 'embed'
                         AND j.target_id = a.id
                         AND j.state IN ('pending', 'running')
                         AND j.attempts < ?)
              LIMIT ?",
        )
        .bind(crate::store::jobs::MAX_ATTEMPTS)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get("id")).collect())
    }

    /// Active merged artifacts, other than `child_id`, every root of which is
    /// also a root of `child_id`.
    ///
    /// A merge written from everything an earlier merge was made of subsumes
    /// it. Superseding only the roots would leave that earlier merge active and
    /// near-identical to the new one, so the relate unit would file the pair
    /// again on the next sweep and the two would churn against each other for
    /// as long as they both existed.
    pub async fn subsumed_merges(&self, child_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query(
            // The EXISTS guard is load-bearing. A merged artifact whose roots
            // have all been deleted has no lineage rows left, and without it the
            // NOT EXISTS below would call the empty set a subset of everything
            // and hide it behind an unrelated merge. That artifact is
            // `merged_missing_a_source`'s business, not this function's.
            "SELECT other.id FROM artifacts other
              WHERE other.provenance = 'merged'
                AND other.status = 'active'
                AND other.superseded_by IS NULL
                AND other.id != ?1
                AND EXISTS (SELECT 1 FROM artifact_sources WHERE child_id = other.id)
                AND NOT EXISTS (
                      SELECT 1 FROM artifact_sources mine
                       WHERE mine.child_id = other.id
                         AND mine.root_id NOT IN (
                               SELECT root_id FROM artifact_sources WHERE child_id = ?1))",
        )
        .bind(child_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get("id")).collect())
    }

    /// Merged artifacts holding fewer lineage rows than the number of sources
    /// they were written from — and not yet flagged for it. The exclusion is
    /// in the SQL, not the caller: membership in this set is permanent
    /// (deletes are hard), so without it the oldest flagged rows fill the
    /// LIMIT forever and a newly orphaned merge past the five-hundredth is
    /// never seen. Newest first for the same reason.
    ///
    /// A comparison rather than a guess, which is what `source_count` is for:
    /// without it, "lost a source" cannot be told from "only ever had two".
    pub async fn merged_missing_a_source(&self, limit: i64) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT a.id FROM artifacts a
              WHERE a.provenance = 'merged'
                AND a.source_count >
                    (SELECT COUNT(*) FROM artifact_sources WHERE child_id = a.id)
                AND (a.flags IS NULL OR a.flags NOT LIKE '%orphaned_source%')
              ORDER BY a.created_at DESC, a.id
              LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get("id")).collect())
    }

    /// Every pair, in any state, both of whose artifacts are in this set.
    ///
    /// Each captured root of `child_id`, with the direct parent it entered
    /// through.
    ///
    /// `roots_of` answers what a merge is *made of*, which is what every
    /// decision needs. This answers how it got there, which is what a picture
    /// of the lineage needs and nothing else does: `via_id` is the column the
    /// schema marks "rendering only". Equal to the root for a first-generation
    /// merge, `None` for a row whose intermediate has since been deleted —
    /// `ON DELETE SET NULL`, because losing the middle of the path does not
    /// invalidate its end.
    pub async fn sources_with_via(&self, child_id: &str) -> Result<Vec<(String, Option<String>)>> {
        let rows = sqlx::query(
            "SELECT root_id, via_id FROM artifact_sources
              WHERE child_id = ? ORDER BY root_id",
        )
        .bind(child_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get("root_id"), r.get("via_id")))
            .collect())
    }

    /// What undoing a merge has to dismiss. Reactivating the roots alone
    /// accomplishes nothing: the sweep re-finds them and reaches the same
    /// verdict, so the operator's decision lasts until the next tick.
    pub async fn pairs_among(&self, ids: &[String]) -> Result<Vec<ArtifactPair>> {
        if ids.len() < 2 {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        // A handful of ids at most — the fan-in cap bounds it — so the pairwise
        // loop is cheaper than building a variadic IN clause for sqlx.
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                if let Some(row) =
                    sqlx::query("SELECT * FROM artifact_pairs WHERE a_id = ? AND b_id = ?")
                        .bind(lo)
                        .bind(hi)
                        .fetch_optional(&self.pool)
                        .await?
                {
                    out.push(crate::store::pairs::row_to_pair(&row));
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use crate::store::Store;
    use crate::store::artifacts::{NewArtifact, NewMerged, Provenance};

    async fn three(s: &Store) -> (String, String, String) {
        let src = s.insert_corpus("x", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..3)
            .map(|i| NewArtifact {
                ordinal: i,
                text: format!("artifact {i}"),
                corpus_span: None,
                title: None,
                category: None,
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        let made = s.insert_artifacts(&src.id, &new).await.unwrap();
        (made[0].id.clone(), made[1].id.clone(), made[2].id.clone())
    }

    fn merged(text: &str) -> NewMerged {
        NewMerged {
            text: text.into(),
            title: None,
            category: None,
            tags: vec![],
            caveats: vec![],
        }
    }

    /// The column exists for one reason — drawing the lineage — so the query
    /// that reads it is the only place the generation between a merge and its
    /// roots survives.
    #[tokio::test]
    async fn a_source_row_names_the_parent_its_root_entered_through() {
        let s = Store::memory().await.unwrap();
        let (a, b, c) = three(&s).await;
        let m1 = s
            .insert_merged_artifact(&merged("a and b"), &[a.clone(), b.clone()])
            .await
            .unwrap();
        let m2 = s
            .insert_merged_artifact(&merged("all three"), &[m1.id.clone(), c.clone()])
            .await
            .unwrap();

        let first = s.sources_with_via(&m1.id).await.unwrap();
        assert_eq!(
            first,
            vec![
                (a.clone().min(b.clone()), Some(a.clone().min(b.clone()))),
                (a.clone().max(b.clone()), Some(a.clone().max(b.clone()))),
            ],
            "a first-generation merge is its own via"
        );

        let second: std::collections::BTreeMap<String, Option<String>> = s
            .sources_with_via(&m2.id)
            .await
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(
            second[&a],
            Some(m1.id.clone()),
            "a entered through the merge"
        );
        assert_eq!(second[&b], Some(m1.id.clone()));
        assert_eq!(second[&c], Some(c.clone()), "c was merged directly");
    }

    #[tokio::test]
    async fn a_captured_artifact_is_its_own_root() {
        // The dedupe pass asks for the roots of every component member without
        // caring which kind it is. Answering "none" for a captured artifact
        // would silently drop it from the prompt it is supposed to be in.
        let s = Store::memory().await.unwrap();
        let (a, b, _) = three(&s).await;
        let roots = s.roots_of(&[a.clone(), b.clone()]).await.unwrap();
        assert_eq!(roots[&a], vec![a.clone()]);
        assert_eq!(roots[&b], vec![b.clone()]);
    }

    #[tokio::test]
    async fn a_merged_artifact_resolves_to_its_captured_roots() {
        let s = Store::memory().await.unwrap();
        let (a, b, _) = three(&s).await;
        let m = s
            .insert_merged_artifact(&merged("both"), &[a.clone(), b.clone()])
            .await
            .unwrap();

        let roots = s.roots_of(std::slice::from_ref(&m.id)).await.unwrap();
        let mut got = roots[&m.id].clone();
        got.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(got, want);
        assert_eq!(m.provenance, Provenance::Merged);
        assert_eq!(m.corpus_id, None);
        assert_eq!(m.source_count, 2);
    }

    #[tokio::test]
    async fn a_merge_of_a_merge_records_only_captured_roots() {
        // The anti-drift invariant, at the storage layer: `root_id` never names
        // a merged artifact, so a re-merge is always written from originals and
        // never from text an earlier merge produced.
        let s = Store::memory().await.unwrap();
        let (a, b, c) = three(&s).await;
        let m1 = s
            .insert_merged_artifact(&merged("a and b"), &[a.clone(), b.clone()])
            .await
            .unwrap();
        let m2 = s
            .insert_merged_artifact(&merged("a and b and c"), &[m1.id.clone(), c.clone()])
            .await
            .unwrap();

        let roots = s.roots_of(std::slice::from_ref(&m2.id)).await.unwrap();
        let mut got = roots[&m2.id].clone();
        got.sort();
        let mut want = vec![a, b, c];
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
    async fn deleting_a_root_takes_its_lineage_rows_with_it() {
        let s = Store::memory().await.unwrap();
        let (a, b, _) = three(&s).await;
        let m = s
            .insert_merged_artifact(&merged("both"), &[a.clone(), b.clone()])
            .await
            .unwrap();

        s.delete_artifact(&a).await.unwrap();

        let roots = s.roots_of(std::slice::from_ref(&m.id)).await.unwrap();
        assert_eq!(roots[&m.id], vec![b], "the cascade left a dangling root");
        // And the loss is visible rather than silent: the merge still claims
        // two sources while only one row survives.
        assert_eq!(s.merged_missing_a_source(10).await.unwrap(), vec![m.id]);
    }

    #[tokio::test]
    async fn a_merge_that_is_no_longer_standing_leaves_the_ops_list() {
        // Every row Ops renders carries an "Undo merge" button, and undo means
        // restore what this hid, then deprecate it. A merge already deprecated
        // hid nothing that is still hidden, and one superseded by a later merge
        // has handed its sources over — so the button would restore nothing and
        // deprecate an artifact that is already gone from results.
        let s = Store::memory().await.unwrap();
        let (a, b, c) = three(&s).await;
        let live = s
            .insert_merged_artifact(&merged("a and b"), &[a.clone(), b])
            .await
            .unwrap();
        let undone = s
            .insert_merged_artifact(&merged("undone"), &[a.clone(), c.clone()])
            .await
            .unwrap();
        let subsumed = s
            .insert_merged_artifact(&merged("subsumed"), &[a, c])
            .await
            .unwrap();
        s.set_artifact_status(
            &undone.id,
            crate::store::artifacts::ArtifactStatus::Deprecated,
        )
        .await
        .unwrap();
        s.set_superseded_by(&subsumed.id, Some(&live.id))
            .await
            .unwrap();

        let listed: Vec<String> = s
            .merged_artifacts(50)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();

        assert_eq!(listed, vec![live.id], "{listed:?}");
    }

    #[tokio::test]
    async fn a_merge_of_a_merge_notices_a_lost_root_too() {
        // `source_count` counted the arguments the call was given, which for a
        // merge of a merge is fewer than the roots it wrote: M2 = merge(M1(a,b),
        // c) recorded two against three lineage rows. `merged_missing_a_source`
        // asks whether the count exceeds the surviving rows, so losing a root
        // left 2 > 2 — false — and the orphan flag never fired for a merge of a
        // merge at all. That is the case the counter was added for.
        let s = Store::memory().await.unwrap();
        let (a, b, c) = three(&s).await;
        let m1 = s
            .insert_merged_artifact(&merged("a and b"), &[a.clone(), b.clone()])
            .await
            .unwrap();
        let m2 = s
            .insert_merged_artifact(&merged("a and b and c"), &[m1.id.clone(), c.clone()])
            .await
            .unwrap();
        assert_eq!(m2.source_count, 3, "the count is the roots, not the inputs");

        s.delete_artifact(&a).await.unwrap();

        assert!(
            s.merged_missing_a_source(10)
                .await
                .unwrap()
                .contains(&m2.id),
            "a merge of a merge lost a root without saying so"
        );
    }

    #[tokio::test]
    async fn two_sources_sharing_a_root_are_counted_once() {
        // Merging M(a,b) with `a` itself is a component the sweep can build, and
        // it names `a` twice. `artifact_sources` is keyed on (child_id, root_id)
        // so it writes one row — and a count of three against two rows would
        // report a lost source that was never there.
        let s = Store::memory().await.unwrap();
        let (a, b, _) = three(&s).await;
        let m1 = s
            .insert_merged_artifact(&merged("a and b"), &[a.clone(), b.clone()])
            .await
            .unwrap();
        let m2 = s
            .insert_merged_artifact(&merged("still a and b"), &[m1.id.clone(), a.clone()])
            .await
            .unwrap();

        assert_eq!(m2.source_count, 2);
        assert!(
            s.merged_missing_a_source(10).await.unwrap().is_empty(),
            "a shared root was reported as a loss"
        );
    }

    #[tokio::test]
    async fn a_merge_whose_roots_are_still_active_is_findable() {
        // The write path indexes a merged artifact before superseding its
        // roots, so a crash in between leaves exactly this state. Nothing else
        // in the system would ever notice it.
        let s = Store::memory().await.unwrap();
        let (a, b, _) = three(&s).await;
        let m = s
            .insert_merged_artifact(&merged("both"), &[a.clone(), b.clone()])
            .await
            .unwrap();

        // Not yet: an unindexed merge has not reached the step that hides its
        // roots, so it is not unfinished — it is unstarted.
        assert!(
            s.merged_with_active_roots(10).await.unwrap().is_empty(),
            "a merge that is not indexed yet was reported as unfinished"
        );

        s.mark_embedded(&m.id, "fake-embed", 0).await.unwrap();
        assert_eq!(
            s.merged_with_active_roots(10).await.unwrap(),
            vec![m.id.clone()]
        );

        s.set_superseded_by(&a, Some(&m.id)).await.unwrap();
        s.set_superseded_by(&b, Some(&m.id)).await.unwrap();
        assert!(
            s.merged_with_active_roots(10).await.unwrap().is_empty(),
            "a finished merge is still reported as unfinished"
        );
    }

    #[tokio::test]
    async fn a_merge_subsumes_an_earlier_one_built_from_the_same_roots() {
        let s = Store::memory().await.unwrap();
        let (a, b, c) = three(&s).await;
        let m1 = s
            .insert_merged_artifact(&merged("a and b"), &[a.clone(), b.clone()])
            .await
            .unwrap();
        let m2 = s
            .insert_merged_artifact(&merged("a and b and c"), &[m1.id.clone(), c])
            .await
            .unwrap();

        assert_eq!(
            s.subsumed_merges(&m2.id).await.unwrap(),
            vec![m1.id.clone()]
        );
        // Not the other way round: m2 draws on a root m1 never had.
        assert!(s.subsumed_merges(&m1.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_merge_that_lost_every_root_is_not_subsumed_by_an_unrelated_one() {
        // Without the EXISTS guard the empty root set reads as a subset of
        // everything, and an artifact whose sources were deleted would be
        // hidden behind a merge it has nothing to do with.
        let s = Store::memory().await.unwrap();
        let (a, b, c) = three(&s).await;
        let _orphan = s
            .insert_merged_artifact(&merged("a and b"), &[a.clone(), b.clone()])
            .await
            .unwrap();
        let unrelated = s
            .insert_merged_artifact(&merged("c alone"), &[c])
            .await
            .unwrap();

        s.delete_artifact(&a).await.unwrap();
        s.delete_artifact(&b).await.unwrap();

        assert!(
            s.subsumed_merges(&unrelated.id).await.unwrap().is_empty(),
            "a merge with no surviving roots was swallowed by an unrelated one"
        );
    }

    #[tokio::test]
    async fn pairs_among_finds_only_pairs_inside_the_set() {
        let s = Store::memory().await.unwrap();
        let (a, b, c) = three(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        s.record_pair(&b, &c, 0.90).await.unwrap();

        let found = s.pairs_among(&[a.clone(), b.clone()]).await.unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].a_id == a || found[0].b_id == a);
    }
}
