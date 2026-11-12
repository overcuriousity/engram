//! Which captured artifacts a merged artifact is made of, stored as the resolved closure:
//! a merge is always written from originals, and the sweep must not traverse a graph.

use super::Store;
use crate::error::Result;
use crate::store::artifacts::{Chunk, CorpusSpan};
use crate::store::pairs::ArtifactPair;
use std::collections::BTreeMap;

/// Derived from lineage, never stored, so it cannot drift.
#[derive(Debug, Clone, PartialEq)]
pub struct Origin {
    pub corpus_id: String,
    pub span: Option<CorpusSpan>,
}

impl Store {
    /// A captured artifact is its own root. A merged one with no lineage rows resolves to the
    /// empty list, never to itself, so a synthesis is never passed off as an original.
    pub async fn roots_of(&self, artifact_ids: &[String]) -> Result<BTreeMap<String, Vec<String>>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for id in artifact_ids {
            let roots: Vec<String> = sqlx::query_scalar!(
                "SELECT root_id FROM artifact_sources WHERE child_id = ? ORDER BY root_id",
                id
            )
            .fetch_all(&self.pool)
            .await?;
            let entry = if roots.is_empty() {
                let provenance: Option<String> =
                    sqlx::query_scalar!("SELECT provenance FROM artifacts WHERE id = ?", id)
                        .fetch_optional(&self.pool)
                        .await?;
                if provenance.as_deref().is_some_and(|p| {
                    crate::store::artifacts::Provenance::parse(p).is_model_written()
                }) {
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

    /// Every root's corpus and span. Empty only in the broken state `roots_of` reports.
    pub async fn origins_of(&self, ids: &[String]) -> Result<BTreeMap<String, Vec<Origin>>> {
        let roots = self.roots_of(ids).await?;
        let mut all: Vec<String> = roots.values().flatten().cloned().collect();
        all.sort();
        all.dedup();
        let rows = self.artifacts_by_ids(&all).await?;
        let by_id: std::collections::HashMap<&str, &Chunk> =
            rows.iter().map(|c| (c.id.as_str(), c)).collect();
        let mut out = BTreeMap::new();
        for (id, rs) in roots {
            let mut o: Vec<Origin> = rs
                .iter()
                .filter_map(|r| by_id.get(r.as_str()))
                .filter_map(|c| {
                    Some(Origin {
                        corpus_id: c.corpus_id.clone()?,
                        span: c.corpus_span.clone(),
                    })
                })
                .collect();
            o.sort_by(|a, b| {
                (a.corpus_id.as_str(), a.span.as_ref().map(|s| s.start_line))
                    .cmp(&(b.corpus_id.as_str(), b.span.as_ref().map(|s| s.start_line)))
            });
            out.insert(id, o);
        }
        Ok(out)
    }

    pub async fn artifacts_originating_in(&self, corpus_id: &str) -> Result<Vec<Chunk>> {
        let rows = sqlx::query_as!(
            crate::store::artifacts::ArtifactRow,
            "SELECT DISTINCT m.* FROM artifacts m
               JOIN artifact_sources s ON s.child_id = m.id
               JOIN artifacts r ON r.id = s.root_id
              WHERE r.corpus_id = ? AND m.provenance IN ('merged', 'synthesized')
              ORDER BY m.created_at",
            corpus_id
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Chunk::from).collect())
    }

    /// Only standing merges: each row gets an "Undo merge" button, which would do nothing on
    /// an undone or subsumed merge.
    pub async fn merged_artifacts(
        &self,
        limit: i64,
    ) -> Result<Vec<crate::store::artifacts::Chunk>> {
        let rows = sqlx::query_as!(
            crate::store::artifacts::ArtifactRow,
            "SELECT * FROM artifacts
              WHERE provenance = 'merged'
                AND status = 'active'
                AND superseded_by IS NULL
              ORDER BY created_at DESC, id LIMIT ?",
            limit
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Chunk::from).collect())
    }

    /// Exactly the state a crash between indexing and superseding leaves. Roots an operator
    /// restored (`restored = 1`) are excluded, or the repair re-hides them every sweep.
    pub async fn merged_with_active_roots(&self, limit: i64) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar!(
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
            limit
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Every merge naming the root. A new merge writes `restored = 0` and may still hide it.
    pub async fn mark_source_restored(&self, root_id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE artifact_sources SET restored = 1 WHERE root_id = ?",
            root_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Unlike `roots_of`, excludes roots an operator restored.
    pub async fn roots_to_hide(&self, child_id: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar!(
            "SELECT root_id FROM artifact_sources
              WHERE child_id = ? AND restored = 0 ORDER BY root_id",
            child_id
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// The embed-job half lives in the control database, so the `LIMIT` cannot apply with the
    /// `WHERE`: paged instead. Only merges that hid nothing: a later re-embed is not stranding.
    pub async fn stranded_merges(&self, limit: i64) -> Result<Vec<String>> {
        const PAGE: i64 = 500;
        let want = limit.max(0) as usize;
        let mut out: Vec<String> = Vec::new();
        let mut after = String::new();
        while out.len() < want {
            let candidates: Vec<String> = sqlx::query_scalar!(
                r#"SELECT a.id AS "id!" FROM artifacts a
                  WHERE a.provenance = 'merged'
                    AND a.status = 'active'
                    AND a.superseded_by IS NULL
                    AND a.embed_state != 'embedded'
                    AND NOT EXISTS (SELECT 1 FROM artifacts r WHERE r.superseded_by = a.id)
                    AND a.id > ?
                  ORDER BY a.id
                  LIMIT ?"#,
                after,
                PAGE
            )
            .fetch_all(&self.pool)
            .await?;
            let Some(last) = candidates.last().cloned() else {
                break;
            };
            after = last;
            let armed = self
                .control
                .targets_with_jobs(
                    &self.subject,
                    crate::store::jobs::Stage::Embed,
                    &candidates,
                    &["pending", "running"],
                    Some(crate::store::jobs::MAX_ATTEMPTS),
                )
                .await?;
            out.extend(
                candidates
                    .into_iter()
                    .filter(|id| !armed.contains(id))
                    .take(want - out.len()),
            );
        }
        Ok(out)
    }

    /// Otherwise the earlier merge stays active beside the new one and relate churns the pair.
    pub async fn subsumed_merges(&self, child_id: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar!(
            // The EXISTS guard stops NOT EXISTS calling an empty root set a subset of everything.
            r#"SELECT other.id AS "id!" FROM artifacts other
              WHERE other.provenance = 'merged'
                AND other.status = 'active'
                AND other.superseded_by IS NULL
                AND other.id != ?1
                AND EXISTS (SELECT 1 FROM artifact_sources WHERE child_id = other.id)
                AND NOT EXISTS (
                      SELECT 1 FROM artifact_sources mine
                       WHERE mine.child_id = other.id
                         AND mine.root_id NOT IN (
                               SELECT root_id FROM artifact_sources WHERE child_id = ?1))"#,
            child_id
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Flagged rows are excluded in SQL: membership is permanent, so they would fill the LIMIT.
    pub async fn merged_missing_a_source(&self, limit: i64) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar!(
            r#"SELECT a.id AS "id!" FROM artifacts a
              WHERE a.provenance = 'merged'
                AND a.source_count >
                    (SELECT COUNT(*) FROM artifact_sources WHERE child_id = a.id)
                AND (a.flags IS NULL OR a.flags NOT LIKE '%orphaned_source%')
              ORDER BY a.created_at DESC, a.id
              LIMIT ?"#,
            limit
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn artifacts_flagged(&self, flag: &str, limit: i64) -> Result<Vec<String>> {
        let pattern = format!("%\"{flag}\"%");
        Ok(sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM artifacts WHERE flags LIKE ? ORDER BY created_at DESC, id LIMIT ?"#,
            pattern,
            limit
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// `None` where the intermediate was deleted (`ON DELETE SET NULL`); for rendering only.
    pub async fn sources_with_via(&self, child_id: &str) -> Result<Vec<(String, Option<String>)>> {
        let rows = sqlx::query!(
            "SELECT root_id, via_id FROM artifact_sources
              WHERE child_id = ? ORDER BY root_id",
            child_id
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| (r.root_id, r.via_id)).collect())
    }

    /// Every pair, in any state, both of whose artifacts are in this set: what undoing a merge
    /// has to dismiss, or the sweep re-finds the same verdict.
    pub async fn pairs_among(&self, ids: &[String]) -> Result<Vec<ArtifactPair>> {
        if ids.len() < 2 {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        // The fan-in cap bounds the ids, so the pairwise loop beats a variadic IN clause.
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                if let Some(row) = sqlx::query_as!(
                    crate::store::pairs::PairRow,
                    "SELECT * FROM artifact_pairs WHERE a_id = ? AND b_id = ?",
                    lo,
                    hi
                )
                .fetch_optional(&self.pool)
                .await?
                {
                    out.push(crate::store::pairs::row_to_pair(row));
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::Origin;
    use crate::store::Store;
    use crate::store::artifacts::{NewArtifact, NewMerged, Provenance};

    async fn three(s: &Store) -> (String, String, String) {
        let src = s.insert_corpus("x", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..3)
            .map(|i| NewArtifact {
                ordinal: i,
                text: format!("artifact {i}"),
                ..Default::default()
            })
            .collect();
        let made = s.insert_artifacts(&src.id, &new).await.unwrap();
        (made[0].id.clone(), made[1].id.clone(), made[2].id.clone())
    }

    fn merged(text: &str) -> NewMerged {
        NewMerged {
            text: text.into(),
            ..Default::default()
        }
    }

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
        // The merge still claims two sources while only one row survives.
        assert_eq!(s.merged_missing_a_source(10).await.unwrap(), vec![m.id]);
    }

    #[tokio::test]
    async fn a_merge_that_is_no_longer_standing_leaves_the_ops_list() {
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
        // M2 = merge(M1(a,b), c) must count three sources, its lineage rows, not two arguments.
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
        // `artifact_sources` is keyed on (child_id, root_id), so `a` named twice writes one row.
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
        let s = Store::memory().await.unwrap();
        let (a, b, _) = three(&s).await;
        let m = s
            .insert_merged_artifact(&merged("both"), &[a.clone(), b.clone()])
            .await
            .unwrap();

        // An unindexed merge is unstarted, not unfinished.
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
        assert!(s.subsumed_merges(&m1.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_merge_that_lost_every_root_is_not_subsumed_by_an_unrelated_one() {
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

    #[tokio::test]
    async fn roots_of_a_model_written_artifact_with_no_lineage_is_empty_not_itself() {
        // A synthesized artifact read as captured would hand model text to a merge as an original.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let na = |ordinal: i64, text: &str| crate::store::artifacts::NewArtifact {
            ordinal,
            text: text.into(),
            ..Default::default()
        };
        let made = s
            .insert_artifacts_with_provenance(
                &src.id,
                &[na(0, "written from a pursuit")],
                crate::store::artifacts::Provenance::Synthesized,
            )
            .await
            .unwrap();
        let roots = s.roots_of(std::slice::from_ref(&made[0].id)).await.unwrap();
        assert!(roots[&made[0].id].is_empty(), "{roots:?}");
        let p = s
            .insert_artifacts_with_provenance(
                &src.id,
                &[na(1, "verbatim")],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();
        let roots = s.roots_of(std::slice::from_ref(&p[0].id)).await.unwrap();
        assert_eq!(roots[&p[0].id], vec![p[0].id.clone()]);
    }

    #[tokio::test]
    async fn origins_of_a_merge_are_its_roots_corpora_and_spans() {
        let s = Store::memory().await.unwrap();
        let c1 = s.insert_corpus("one", "web", None).await.unwrap();
        let c2 = s.insert_corpus("two", "web", None).await.unwrap();
        let na = |t: &str, a: i64, b: i64| crate::store::artifacts::NewArtifact {
            text: t.into(),
            corpus_span: Some(crate::store::artifacts::CorpusSpan {
                start_line: a,
                end_line: b,
                source: crate::store::artifacts::SpanSource::Located,
            }),
            segment_idx: Some(0),
            ..Default::default()
        };
        let r1 = s.insert_artifacts(&c1.id, &[na("r1", 1, 3)]).await.unwrap()[0]
            .id
            .clone();
        let r2 = s.insert_artifacts(&c2.id, &[na("r2", 7, 9)]).await.unwrap()[0]
            .id
            .clone();
        let m = s
            .insert_merged_artifact(&merged("r1 and r2"), &[r1.clone(), r2.clone()])
            .await
            .unwrap();
        let o = s.origins_of(&[m.id.clone(), r1.clone()]).await.unwrap();
        let mut got: Vec<(String, Option<i64>)> = o[&m.id]
            .iter()
            .map(|x| (x.corpus_id.clone(), x.span.as_ref().map(|sp| sp.start_line)))
            .collect();
        got.sort();
        let mut want = vec![(c1.id.clone(), Some(1)), (c2.id.clone(), Some(7))];
        want.sort();
        assert_eq!(got, want);
        assert_eq!(
            o[&r1],
            vec![Origin {
                corpus_id: c1.id.clone(),
                span: Some(crate::store::artifacts::CorpusSpan {
                    start_line: 1,
                    end_line: 3,
                    source: crate::store::artifacts::SpanSource::Located,
                })
            }]
        );
        assert!(o.values().all(|v| !v.is_empty()));
        let from_c2 = s.artifacts_originating_in(&c2.id).await.unwrap();
        assert_eq!(
            from_c2.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            vec![m.id.clone()]
        );
    }
}
