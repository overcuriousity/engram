use super::{FacetCount, Facets, SearchFilter, SearchHit, VectorPoint, VectorStore, cosine};
use crate::error::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::RwLock;

/// Brute-force cosine over a HashMap. Lets the entire ingest-to-search path be
/// tested without running Qdrant.
pub struct MemoryVectors {
    points: RwLock<HashMap<String, VectorPoint>>,
}

impl MemoryVectors {
    pub fn new() -> Self {
        Self {
            points: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for MemoryVectors {
    fn default() -> Self {
        Self::new()
    }
}

/// Counts into chips, most frequent first. Ties break on the value so a
/// HashMap's iteration order never leaks into what the page renders.
fn ranked(counts: HashMap<&str, u64>, limit: usize) -> Vec<FacetCount> {
    let mut out: Vec<FacetCount> = counts
        .into_iter()
        .map(|(value, count)| FacetCount {
            value: value.to_string(),
            count,
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value)));
    out.truncate(limit);
    out
}

#[async_trait]
impl VectorStore for MemoryVectors {
    async fn ensure_collection(&self, _dim: usize) -> Result<()> {
        Ok(())
    }

    async fn upsert(&self, points: Vec<VectorPoint>) -> Result<()> {
        let mut w = self.points.write().unwrap();
        for mut p in points {
            // Matching the Qdrant path: a re-embed rebuilds the payload without
            // knowing when the chunk was last shown, and clearing the stamp
            // would make a chunk read yesterday look forgotten.
            if p.payload.last_seen_at.is_none() {
                p.payload.last_seen_at = w
                    .get(&p.payload.artifact_id)
                    .and_then(|old| old.payload.last_seen_at);
            }
            // Same rule, same reason: an unset flag means "whatever is already
            // stored", so a re-embed cannot revive an artifact the sweep hid.
            if p.payload.superseded.is_none() {
                p.payload.superseded = w
                    .get(&p.payload.artifact_id)
                    .and_then(|old| old.payload.superseded);
            }
            w.insert(p.payload.artifact_id.clone(), p);
        }
        Ok(())
    }

    async fn set_payload(&self, payload: &super::VectorPayload) -> Result<()> {
        let mut w = self.points.write().unwrap();
        if let Some(p) = w.get_mut(&payload.artifact_id) {
            // A merge, matching Qdrant: an absent stamp means "unchanged", so
            // a tag edit must not erase when the chunk was last shown.
            let seen = payload.last_seen_at.or(p.payload.last_seen_at);
            let sup = payload.superseded.or(p.payload.superseded);
            p.payload = payload.clone();
            p.payload.last_seen_at = seen;
            p.payload.superseded = sup;
        }
        Ok(())
    }

    async fn set_superseded(&self, artifact_id: &str, superseded: bool) -> Result<()> {
        let mut w = self.points.write().unwrap();
        if let Some(p) = w.get_mut(artifact_id) {
            p.payload.superseded = Some(superseded);
        }
        Ok(())
    }

    async fn touch(&self, artifact_ids: &[String], seen_at: i64) -> Result<()> {
        let mut w = self.points.write().unwrap();
        for id in artifact_ids {
            if let Some(p) = w.get_mut(id) {
                p.payload.last_seen_at = Some(seen_at);
            }
        }
        Ok(())
    }

    async fn resurface(
        &self,
        limit: usize,
        older_than: i64,
        unseen_since: i64,
    ) -> Result<Vec<SearchHit>> {
        let r = self.points.read().unwrap();
        Ok(r.values()
            .filter(|p| {
                p.payload.created_at < older_than
                    && p.payload.last_seen_at.is_none_or(|s| s < unseen_since)
            })
            .take(limit)
            .map(|p| SearchHit {
                payload: p.payload.clone(),
                score: 0.0,
            })
            .collect())
    }

    /// Dense only. Hybrid fusion is a Qdrant feature; reimplementing BM25
    /// scoring here would test this file rather than the real retrieval path,
    /// which the integration suite covers instead.
    async fn search(
        &self,
        vector: &[f32],
        _sparse: &super::sparse::SparseVector,
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>> {
        let r = self.points.read().unwrap();
        let mut hits: Vec<SearchHit> = r
            .values()
            .filter(|p| {
                (filter.include_superseded || p.payload.superseded != Some(true))
                    && filter.tags.iter().all(|t| p.payload.tags.contains(t))
                    && filter
                        .category
                        .as_ref()
                        .is_none_or(|c| p.payload.category.as_ref() == Some(c))
            })
            .map(|p| SearchHit {
                payload: p.payload.clone(),
                score: cosine(vector, &p.vector),
            })
            .collect();
        // Tie-break on artifact_id so equal scores produce a stable order rather
        // than whatever the HashMap iterated this time.
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.payload.artifact_id.cmp(&b.payload.artifact_id))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    async fn facets(&self, limit: usize) -> Result<Facets> {
        let r = self.points.read().unwrap();
        let mut categories: HashMap<&str, u64> = HashMap::new();
        let mut tags: HashMap<&str, u64> = HashMap::new();
        for p in r.values() {
            if let Some(c) = &p.payload.category {
                *categories.entry(c.as_str()).or_default() += 1;
            }
            for t in &p.payload.tags {
                *tags.entry(t.as_str()).or_default() += 1;
            }
        }
        Ok(Facets {
            categories: ranked(categories, limit),
            tags: ranked(tags, limit),
        })
    }

    async fn neighbours(&self, artifact_id: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let r = self.points.read().unwrap();
        let Some(seed) = r.get(artifact_id) else {
            return Ok(vec![]);
        };
        let mut hits: Vec<SearchHit> = r
            .values()
            .filter(|p| p.payload.artifact_id != artifact_id)
            .map(|p| SearchHit {
                payload: p.payload.clone(),
                score: cosine(&seed.vector, &p.vector),
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.payload.artifact_id.cmp(&b.payload.artifact_id))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    async fn near_pairs(
        &self,
        sample: usize,
        per_point: usize,
        min_score: f32,
    ) -> Result<Vec<super::NearPair>> {
        let r = self.points.read().unwrap();
        let live: Vec<&VectorPoint> = r
            .values()
            .filter(|p| p.payload.superseded != Some(true))
            .take(sample)
            .collect();

        let mut out: Vec<super::NearPair> = Vec::new();
        for (i, a) in live.iter().enumerate() {
            let mut mine: Vec<super::NearPair> = live
                .iter()
                .skip(i + 1)
                .map(|b| {
                    super::NearPair::new(
                        &a.payload.artifact_id,
                        &b.payload.artifact_id,
                        cosine(&a.vector, &b.vector),
                    )
                })
                .filter(|p| p.score >= min_score)
                .collect();
            mine.sort_by(|x, y| y.score.total_cmp(&x.score));
            mine.truncate(per_point);
            out.extend(mine);
        }
        // Deterministic order, so a test never depends on HashMap iteration.
        out.sort_by(|x, y| {
            y.score
                .total_cmp(&x.score)
                .then_with(|| x.a.cmp(&y.a))
                .then_with(|| x.b.cmp(&y.b))
        });
        out.dedup_by(|x, y| x.a == y.a && x.b == y.b);
        Ok(out)
    }

    async fn delete_artifacts(&self, artifact_ids: &[String]) -> Result<()> {
        let mut w = self.points.write().unwrap();
        for id in artifact_ids {
            w.remove(id);
        }
        Ok(())
    }

    async fn delete_by_corpus(&self, corpus_id: &str) -> Result<()> {
        let mut w = self.points.write().unwrap();
        w.retain(|_, p| p.payload.corpus_id != corpus_id);
        Ok(())
    }

    async fn count(&self) -> Result<u64> {
        Ok(self.points.read().unwrap().len() as u64)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::{SearchFilter, VectorPayload, VectorPoint, VectorStore};

    fn point(id: &str, src: &str, v: Vec<f32>, tags: &[&str], cat: &str) -> VectorPoint {
        VectorPoint {
            vector: v,
            sparse: Default::default(),
            payload: VectorPayload {
                artifact_id: id.into(),
                corpus_id: src.into(),
                text: format!("text of {id}"),
                title: Some(id.into()),
                category: Some(cat.into()),
                tags: tags.iter().map(|s| s.to_string()).collect(),
                created_at: 0,
                last_seen_at: None,
                superseded: None,
            },
        }
    }

    #[tokio::test]
    async fn search_ranks_by_cosine_similarity() {
        let v = MemoryVectors::new();
        v.ensure_collection(3).await.unwrap();
        v.upsert(vec![
            point("near", "s1", vec![1.0, 0.0, 0.0], &["a"], "procedure"),
            point("far", "s1", vec![0.0, 0.0, 1.0], &["a"], "procedure"),
        ])
        .await
        .unwrap();

        let hits = v
            .search(
                &[1.0, 0.0, 0.0],
                &Default::default(),
                10,
                &SearchFilter::default(),
            )
            .await
            .unwrap();
        assert_eq!(hits[0].payload.artifact_id, "near");
        assert!(hits[0].score > hits[1].score);
    }

    #[tokio::test]
    async fn limit_is_respected() {
        let v = MemoryVectors::new();
        v.ensure_collection(3).await.unwrap();
        v.upsert(vec![
            point("a", "s", vec![1.0, 0.0, 0.0], &[], "c"),
            point("b", "s", vec![0.9, 0.1, 0.0], &[], "c"),
            point("c", "s", vec![0.8, 0.2, 0.0], &[], "c"),
        ])
        .await
        .unwrap();
        assert_eq!(
            v.search(
                &[1.0, 0.0, 0.0],
                &Default::default(),
                2,
                &SearchFilter::default()
            )
            .await
            .unwrap()
            .len(),
            2
        );
    }

    #[tokio::test]
    async fn tag_filter_requires_all_listed_tags() {
        let v = MemoryVectors::new();
        v.ensure_collection(3).await.unwrap();
        v.upsert(vec![
            point(
                "both",
                "s",
                vec![1.0, 0.0, 0.0],
                &["linux", "forensics"],
                "procedure",
            ),
            point("one", "s", vec![1.0, 0.0, 0.0], &["linux"], "procedure"),
        ])
        .await
        .unwrap();

        let f = SearchFilter {
            tags: vec!["linux".into(), "forensics".into()],
            category: None,
            include_superseded: false,
        };
        let hits = v
            .search(&[1.0, 0.0, 0.0], &Default::default(), 10, &f)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].payload.artifact_id, "both");
    }

    #[tokio::test]
    async fn category_filter_is_exact() {
        let v = MemoryVectors::new();
        v.ensure_collection(3).await.unwrap();
        v.upsert(vec![
            point("p", "s", vec![1.0, 0.0, 0.0], &[], "procedure"),
            point("c", "s", vec![1.0, 0.0, 0.0], &[], "concept"),
        ])
        .await
        .unwrap();
        let f = SearchFilter {
            tags: vec![],
            category: Some("concept".into()),
            include_superseded: false,
        };
        let hits = v
            .search(&[1.0, 0.0, 0.0], &Default::default(), 10, &f)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].payload.artifact_id, "c");
    }

    #[tokio::test]
    async fn upsert_replaces_a_point_with_the_same_chunk_id() {
        let v = MemoryVectors::new();
        v.ensure_collection(3).await.unwrap();
        v.upsert(vec![point("x", "s", vec![1.0, 0.0, 0.0], &[], "a")])
            .await
            .unwrap();
        v.upsert(vec![point("x", "s", vec![0.0, 1.0, 0.0], &[], "a")])
            .await
            .unwrap();
        assert_eq!(
            v.count().await.unwrap(),
            1,
            "re-embedding must not duplicate the point"
        );
        let hits = v
            .search(
                &[0.0, 1.0, 0.0],
                &Default::default(),
                1,
                &SearchFilter::default(),
            )
            .await
            .unwrap();
        assert!(hits[0].score > 0.99);
    }

    #[tokio::test]
    async fn delete_by_source_removes_every_chunk_of_that_source() {
        let v = MemoryVectors::new();
        v.ensure_collection(3).await.unwrap();
        v.upsert(vec![
            point("a", "s1", vec![1.0, 0.0, 0.0], &[], "c"),
            point("b", "s1", vec![1.0, 0.0, 0.0], &[], "c"),
            point("c", "s2", vec![1.0, 0.0, 0.0], &[], "c"),
        ])
        .await
        .unwrap();
        v.delete_by_corpus("s1").await.unwrap();
        assert_eq!(v.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn zero_vectors_do_not_produce_nan_scores() {
        let v = MemoryVectors::new();
        v.ensure_collection(3).await.unwrap();
        v.upsert(vec![point("z", "s", vec![0.0, 0.0, 0.0], &[], "c")])
            .await
            .unwrap();
        let hits = v
            .search(
                &[1.0, 0.0, 0.0],
                &Default::default(),
                10,
                &SearchFilter::default(),
            )
            .await
            .unwrap();
        assert!(hits[0].score.is_finite());
    }

    #[tokio::test]
    async fn equal_scores_return_a_stable_order() {
        // Ranking must not depend on HashMap iteration order, or the same
        // query would shuffle its results between calls.
        let v = MemoryVectors::new();
        v.ensure_collection(3).await.unwrap();
        v.upsert(
            (0..20)
                .map(|i| point(&format!("c{i:02}"), "s", vec![1.0, 0.0, 0.0], &[], "c"))
                .collect(),
        )
        .await
        .unwrap();

        let first = v
            .search(
                &[1.0, 0.0, 0.0],
                &Default::default(),
                5,
                &SearchFilter::default(),
            )
            .await
            .unwrap();
        for _ in 0..5 {
            let again = v
                .search(
                    &[1.0, 0.0, 0.0],
                    &Default::default(),
                    5,
                    &SearchFilter::default(),
                )
                .await
                .unwrap();
            assert_eq!(
                first
                    .iter()
                    .map(|h| &h.payload.artifact_id)
                    .collect::<Vec<_>>(),
                again
                    .iter()
                    .map(|h| &h.payload.artifact_id)
                    .collect::<Vec<_>>(),
                "identical scores produced an unstable ordering"
            );
        }
    }

    #[tokio::test]
    async fn facets_count_every_value_and_sort_by_frequency() {
        let v = MemoryVectors::new();
        v.ensure_collection(3).await.unwrap();
        v.upsert(vec![
            point(
                "a",
                "s",
                vec![1.0, 0.0, 0.0],
                &["linux", "shared"],
                "procedure",
            ),
            point("b", "s", vec![1.0, 0.0, 0.0], &["shared"], "procedure"),
            point("c", "s", vec![1.0, 0.0, 0.0], &["shared"], "concept"),
        ])
        .await
        .unwrap();

        let f = v.facets(10).await.unwrap();
        assert_eq!(
            f.categories,
            vec![
                FacetCount {
                    value: "procedure".into(),
                    count: 2
                },
                FacetCount {
                    value: "concept".into(),
                    count: 1
                },
            ]
        );
        assert_eq!(
            f.tags,
            vec![
                FacetCount {
                    value: "shared".into(),
                    count: 3
                },
                FacetCount {
                    value: "linux".into(),
                    count: 1
                },
            ]
        );
    }

    #[tokio::test]
    async fn facets_cap_each_list_at_the_limit() {
        let v = MemoryVectors::new();
        v.ensure_collection(3).await.unwrap();
        v.upsert(
            (0..10)
                .map(|i| {
                    point(
                        &format!("a{i}"),
                        "s",
                        vec![1.0, 0.0, 0.0],
                        &[],
                        &format!("cat{i}"),
                    )
                })
                .collect(),
        )
        .await
        .unwrap();
        assert_eq!(v.facets(3).await.unwrap().categories.len(), 3);
    }

    #[tokio::test]
    async fn neighbours_rank_by_similarity_and_exclude_the_artifact_itself() {
        let v = MemoryVectors::new();
        v.ensure_collection(3).await.unwrap();
        v.upsert(vec![
            point("seed", "s", vec![1.0, 0.0, 0.0], &[], "c"),
            point("close", "s", vec![0.9, 0.1, 0.0], &[], "c"),
            point("distant", "s", vec![0.0, 0.0, 1.0], &[], "c"),
        ])
        .await
        .unwrap();

        let n = v.neighbours("seed", 10).await.unwrap();
        assert_eq!(
            n.iter()
                .map(|h| h.payload.artifact_id.as_str())
                .collect::<Vec<_>>(),
            vec!["close", "distant"]
        );
    }

    #[tokio::test]
    async fn an_unknown_artifact_has_no_neighbours() {
        let v = MemoryVectors::new();
        v.ensure_collection(3).await.unwrap();
        v.upsert(vec![point("a", "s", vec![1.0, 0.0, 0.0], &[], "c")])
            .await
            .unwrap();
        assert!(v.neighbours("missing", 5).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn near_pairs_finds_the_close_pair_and_not_the_far_one() {
        let v = MemoryVectors::new();
        v.upsert(vec![
            point("a", "s1", vec![1.0, 0.0], &[], "note"),
            point("b", "s1", vec![0.999, 0.01], &[], "note"),
            point("c", "s1", vec![0.0, 1.0], &[], "note"),
        ])
        .await
        .unwrap();

        let pairs = v.near_pairs(100, 5, 0.9).await.unwrap();
        assert_eq!(pairs.len(), 1, "got {pairs:?}");
        assert_eq!((pairs[0].a.as_str(), pairs[0].b.as_str()), ("a", "b"));
        assert!(pairs[0].score >= 0.9);
    }

    #[tokio::test]
    async fn a_pair_is_reported_once_not_twice() {
        // (a,b) and (b,a) are the same pair. Reporting both doubles the review
        // queue and makes the sweep supersede an artifact twice.
        let v = MemoryVectors::new();
        v.upsert(vec![
            point("a", "s1", vec![1.0, 0.0], &[], "note"),
            point("b", "s1", vec![0.999, 0.01], &[], "note"),
        ])
        .await
        .unwrap();
        assert_eq!(v.near_pairs(100, 5, 0.9).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_superseded_artifact_is_not_paired_again() {
        // Otherwise every sweep re-finds the pair it resolved last time and
        // the review queue never empties.
        let v = MemoryVectors::new();
        v.upsert(vec![
            point("a", "s1", vec![1.0, 0.0], &[], "note"),
            point("b", "s1", vec![0.999, 0.01], &[], "note"),
        ])
        .await
        .unwrap();
        v.set_superseded("b", true).await.unwrap();
        assert!(v.near_pairs(100, 5, 0.9).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pairs_come_back_best_first() {
        let v = MemoryVectors::new();
        v.upsert(vec![
            point("a", "s1", vec![1.0, 0.0], &[], "note"),
            point("b", "s1", vec![0.999, 0.01], &[], "note"),
            point("c", "s1", vec![0.99, 0.1], &[], "note"),
        ])
        .await
        .unwrap();
        let pairs = v.near_pairs(100, 5, 0.5).await.unwrap();
        for w in pairs.windows(2) {
            assert!(w[0].score >= w[1].score, "not sorted: {pairs:?}");
        }
    }

    #[tokio::test]
    async fn a_superseded_artifact_drops_out_of_search() {
        let v = MemoryVectors::new();
        v.upsert(vec![
            point("a", "s1", vec![1.0, 0.0], &[], "note"),
            point("b", "s1", vec![0.99, 0.1], &[], "note"),
        ])
        .await
        .unwrap();
        assert_eq!(
            v.search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
                .await
                .unwrap()
                .len(),
            2
        );

        v.set_superseded("b", true).await.unwrap();
        let hits = v
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].payload.artifact_id, "a");
    }

    #[tokio::test]
    async fn a_superseded_artifact_is_still_reachable_when_asked_for() {
        // Superseding hides an artifact from ranking. It must not make it
        // unreadable: the review queue and the undo both need to see it.
        let v = MemoryVectors::new();
        v.upsert(vec![
            point("a", "s1", vec![1.0, 0.0], &[], "note"),
            point("b", "s1", vec![0.99, 0.1], &[], "note"),
        ])
        .await
        .unwrap();
        v.set_superseded("b", true).await.unwrap();

        let filter = SearchFilter {
            include_superseded: true,
            ..Default::default()
        };
        assert_eq!(
            v.search(&[1.0, 0.0], &Default::default(), 10, &filter)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn un_superseding_puts_an_artifact_back_in_results() {
        let v = MemoryVectors::new();
        v.upsert(vec![point("a", "s1", vec![1.0, 0.0], &[], "note")])
            .await
            .unwrap();
        v.set_superseded("a", true).await.unwrap();
        v.set_superseded("a", false).await.unwrap();
        assert_eq!(
            v.search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn re_embedding_does_not_un_supersede_an_artifact() {
        // `payload_of` in the embed job knows nothing about consolidation, so
        // it leaves the field unset — and unset must mean "keep what is
        // stored", or every re-embed would silently revive a hidden artifact.
        let v = MemoryVectors::new();
        v.upsert(vec![point("a", "s1", vec![1.0, 0.0], &[], "note")])
            .await
            .unwrap();
        v.set_superseded("a", true).await.unwrap();
        v.upsert(vec![point("a", "s1", vec![1.0, 0.0], &[], "note")])
            .await
            .unwrap();
        assert!(
            v.search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
                .await
                .unwrap()
                .is_empty()
        );
    }
}
