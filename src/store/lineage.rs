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
            // No lineage rows means a captured artifact — or a merged one every
            // root of which has since been deleted. The two are told apart by
            // `source_count`, and the second is `merged_missing_a_source`'s
            // business; treating it as its own root here is the safe reading,
            // since it keeps the artifact in the prompt rather than dropping it.
            out.insert(
                id.clone(),
                if roots.is_empty() {
                    vec![id.clone()]
                } else {
                    roots
                },
            );
        }
        Ok(out)
    }

    /// Merged artifacts at least one of whose roots is still active.
    ///
    /// The write path indexes a merged artifact before superseding its roots,
    /// so this is precisely the state a crash between those two steps leaves
    /// behind. Nothing else would see it: the merge looks finished from the
    /// artifact side and absent from the pair side, and only a join across the
    /// lineage says otherwise.
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
              LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get("child_id")).collect())
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
    /// they were written from.
    ///
    /// A comparison rather than a guess, which is what `source_count` is for:
    /// without it, "lost a source" cannot be told from "only ever had two".
    pub async fn merged_missing_a_source(&self, limit: i64) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT a.id FROM artifacts a
              WHERE a.provenance = 'merged'
                AND a.source_count >
                    (SELECT COUNT(*) FROM artifact_sources WHERE child_id = a.id)
              LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get("id")).collect())
    }

    /// Every pair, in any state, both of whose artifacts are in this set.
    ///
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
    use super::*;
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
        let orphan = s
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
