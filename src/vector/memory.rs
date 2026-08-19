use super::{
    FacetCount, Facets, SearchFilter, SearchHit, VectorPayload, VectorPoint, VectorStore, cosine,
};
use crate::error::Result;
use crate::store::artifacts::ArtifactStatus;
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

/// A payload's effective lifecycle status. An unset `status` reads as active,
/// matching the Qdrant backend's `build_filter`.
fn status_of(payload: &super::VectorPayload) -> ArtifactStatus {
    payload.status.unwrap_or(ArtifactStatus::Active)
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
            if p.payload.hit_count.is_none() {
                p.payload.hit_count = w
                    .get(&p.payload.artifact_id)
                    .and_then(|old| old.payload.hit_count);
            }
            // Same rule, same reason: an unset status means "whatever is
            // already stored", so a re-embed cannot revive an artifact the
            // sweep hid.
            if p.payload.status.is_none() {
                p.payload.status = w
                    .get(&p.payload.artifact_id)
                    .and_then(|old| old.payload.status);
            }
            if p.payload.last_verified_at.is_none() {
                p.payload.last_verified_at = w
                    .get(&p.payload.artifact_id)
                    .and_then(|old| old.payload.last_verified_at);
            }
            if p.payload.superseded_by.is_none() {
                p.payload.superseded_by = w
                    .get(&p.payload.artifact_id)
                    .and_then(|old| old.payload.superseded_by.clone());
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
            let hits = payload.hit_count.or(p.payload.hit_count);
            let status = payload.status.or(p.payload.status);
            let verified = payload.last_verified_at.or(p.payload.last_verified_at);
            let superseded_by = payload
                .superseded_by
                .clone()
                .or_else(|| p.payload.superseded_by.clone());
            p.payload = payload.clone();
            p.payload.last_seen_at = seen;
            p.payload.hit_count = hits;
            p.payload.status = status;
            p.payload.last_verified_at = verified;
            p.payload.superseded_by = superseded_by;
        }
        Ok(())
    }

    async fn set_lifecycle(
        &self,
        artifact_id: &str,
        status: ArtifactStatus,
        superseded_by: Option<&str>,
    ) -> Result<()> {
        let mut w = self.points.write().unwrap();
        if let Some(p) = w.get_mut(artifact_id) {
            p.payload.status = Some(status);
            p.payload.superseded_by = superseded_by.map(str::to_string);
        }
        Ok(())
    }

    async fn set_last_verified_at(
        &self,
        artifact_id: &str,
        at: i64,
        reset_hits: bool,
    ) -> Result<()> {
        let mut w = self.points.write().unwrap();
        if let Some(p) = w.get_mut(artifact_id) {
            p.payload.last_verified_at = Some(at);
            if reset_hits {
                p.payload.hit_count = Some(0);
            }
        }
        Ok(())
    }

    async fn apply_lifecycle(&self, rows: &[super::LifecycleRow]) -> Result<()> {
        let mut w = self.points.write().unwrap();
        for r in rows {
            if let Some(p) = w.get_mut(&r.artifact_id) {
                p.payload.status = Some(r.status);
                p.payload.superseded_by = r.superseded_by.clone();
                p.payload.last_verified_at = Some(r.last_verified_at);
            }
        }
        Ok(())
    }

    async fn lifecycle_of(
        &self,
        artifact_ids: &[String],
    ) -> Result<HashMap<String, super::StoredLifecycle>> {
        let r = self.points.read().unwrap();
        Ok(artifact_ids
            .iter()
            .filter_map(|id| {
                let p = r.get(id)?;
                Some((
                    id.clone(),
                    super::StoredLifecycle {
                        status: status_of(&p.payload),
                        superseded_by: p.payload.superseded_by.clone(),
                    },
                ))
            })
            .collect())
    }

    async fn payloads_of(&self, artifact_ids: &[String]) -> Result<HashMap<String, VectorPayload>> {
        let r = self.points.read().unwrap();
        Ok(artifact_ids
            .iter()
            .filter_map(|id| Some((id.clone(), r.get(id)?.payload.clone())))
            .collect())
    }

    async fn all_artifact_ids(&self) -> Result<Vec<String>> {
        let r = self.points.read().unwrap();
        let mut out: Vec<String> = r.keys().cloned().collect();
        // Deterministic, so a test never depends on HashMap iteration order.
        out.sort();
        Ok(out)
    }

    async fn stale_candidates(
        &self,
        older_than: i64,
        max_hits: i64,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let r = self.points.read().unwrap();
        let mut found: Vec<&VectorPayload> = r
            .values()
            .map(|p| &p.payload)
            .filter(|p| {
                status_of(p) == ArtifactStatus::Active
                    // Present and old. An unstamped point is unknown, not
                    // stale — see the trait doc. A missing `hit_count` is the
                    // other way round: never retrieved is what it means.
                    && p.last_verified_at.is_some_and(|v| v < older_than)
                    && p.hit_count.unwrap_or(0) <= max_hits
            })
            .collect();
        // Sorted before truncating, and sorted at all: this backs a work queue
        // an operator returns to after every action, and `values()` iterates a
        // HashMap in an order that is neither stable nor meaningful. Stalest
        // first, id as the tiebreak — see the Qdrant implementation, whose order
        // this has to match for the two to be testable against each other.
        found.sort_by(|a, b| {
            a.last_verified_at
                .cmp(&b.last_verified_at)
                .then_with(|| a.artifact_id.cmp(&b.artifact_id))
        });
        Ok(found
            .into_iter()
            .take(limit)
            .map(|payload| SearchHit {
                payload: payload.clone(),
                score: 0.0,
                // No query vector to be similar to; this is a listing.
                similarity: None,
            })
            .collect())
    }

    async fn touch(&self, targets: &[super::Touch], seen_at: i64) -> Result<()> {
        let mut w = self.points.write().unwrap();
        for t in targets {
            // The count the caller passed is ignored here on purpose: this
            // store holds the authoritative value already, and reading it is
            // free. The Qdrant path uses the caller's copy to skip a round
            // trip, which is a network optimisation, not a semantic one.
            if let Some(p) = w.get_mut(&t.artifact_id) {
                p.payload.last_seen_at = Some(seen_at);
                // Only a search result counts as a retrieval; see `Touch`.
                if t.counts_as_hit {
                    p.payload.hit_count = Some(p.payload.hit_count.unwrap_or(0) + 1);
                }
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
                // Anything not active is out, matching the Qdrant backend: a
                // deprecated artifact is old and by definition unseen, which
                // makes it a prime candidate for a list of things nobody has
                // looked at — and it has just been retired on purpose.
                status_of(&p.payload) == ArtifactStatus::Active
                    && p.payload.created_at < older_than
                    && p.payload.last_seen_at.is_none_or(|s| s < unseen_since)
            })
            .take(limit)
            .map(|p| SearchHit {
                payload: p.payload.clone(),
                score: 0.0,
                // Drawn at random rather than matched; nothing to be close to.
                similarity: None,
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
                let status = status_of(&p.payload);
                (filter.include_superseded || status != ArtifactStatus::Superseded)
                    && (filter.include_deprecated || status != ArtifactStatus::Deprecated)
                    && filter.tags.iter().all(|t| p.payload.tags.contains(t))
                    && filter
                        .category
                        .as_ref()
                        .is_none_or(|c| p.payload.category.as_ref() == Some(c))
            })
            .map(|p| {
                // Dense only here, so the ranking score *is* the similarity.
                // The Qdrant store has to fetch it separately because fusion
                // throws the magnitude away.
                let similarity = cosine(vector, &p.vector);
                SearchHit {
                    payload: p.payload.clone(),
                    score: similarity,
                    similarity: Some(similarity),
                }
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
        for p in r.values() {
            if let Some(c) = &p.payload.category {
                *categories.entry(c.as_str()).or_default() += 1;
            }
        }
        Ok(Facets {
            categories: ranked(categories, limit),
        })
    }

    async fn neighbours(&self, artifact_id: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let r = self.points.read().unwrap();
        let Some(seed) = r.get(artifact_id) else {
            return Ok(vec![]);
        };
        let mut hits: Vec<SearchHit> = r
            .values()
            // Anything out of search is out of the related pane too, matching
            // the Qdrant backend.
            .filter(|p| {
                p.payload.artifact_id != artifact_id
                    && status_of(&p.payload) == ArtifactStatus::Active
            })
            .map(|p| {
                let cos = cosine(&seed.vector, &p.vector);
                SearchHit {
                    payload: p.payload.clone(),
                    score: cos,
                    // Stated rather than left `None`. This is one dense lookup
                    // with no fusion, so the score *is* a cosine — and the
                    // relate unit compares it against `review_min`, which is a
                    // cosine threshold. Leaving it unset would make a caller
                    // read `score`, which everywhere else in this trait means a
                    // fused rank that is not comparable to anything.
                    similarity: Some(cos),
                }
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
                hit_count: None,
                status: None,
                last_verified_at: None,
                superseded_by: None,
                origin_corpora: vec![],
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
}
