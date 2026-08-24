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
    /// The `ctx` multivector per artifact, held apart from the point rather
    /// than on it. A field on `VectorPoint` would put a context set in every
    /// signature that carries a point — the embed job's, the reindex's — for
    /// the sake of one caller that writes it and one that reads it.
    ctx: RwLock<HashMap<String, Vec<Vec<f32>>>>,
}

impl MemoryVectors {
    pub fn new() -> Self {
        Self {
            points: RwLock::new(HashMap::new()),
            ctx: RwLock::new(HashMap::new()),
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
            // Both carry `skip_serializing_if`, so Qdrant never receives them
            // from a caller that left them empty and the stored values stand.
            // Only `jobs::embed` knows either: every other caller — the tag and
            // category edits above all — builds a payload with no corpora and
            // no provenance, and copying that over the stored one would strip a
            // synthesized artifact of the mark that earns it its badge and its
            // stopping rule, on a tag edit, in this backend only.
            let corpora = if payload.origin_corpora.is_empty() {
                p.payload.origin_corpora.clone()
            } else {
                payload.origin_corpora.clone()
            };
            let provenance = payload
                .provenance
                .clone()
                .or_else(|| p.payload.provenance.clone());
            p.payload = payload.clone();
            p.payload.last_seen_at = seen;
            p.payload.hit_count = hits;
            p.payload.status = status;
            p.payload.last_verified_at = verified;
            p.payload.superseded_by = superseded_by;
            p.payload.origin_corpora = corpora;
            p.payload.provenance = provenance;
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
                    && filter
                        .corpus_id
                        .as_ref()
                        .is_none_or(|c| &p.payload.corpus_id == c)
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

    async fn set_context_vectors(&self, artifact_id: &str, vectors: Vec<Vec<f32>>) -> Result<()> {
        let mut w = self.ctx.write().unwrap();
        if vectors.is_empty() {
            w.remove(artifact_id);
        } else {
            w.insert(artifact_id.to_string(), vectors);
        }
        Ok(())
    }

    async fn context_query(
        &self,
        vector: &[f32],
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>> {
        let points = self.points.read().unwrap();
        let ctx = self.ctx.read().unwrap();
        let mut hits: Vec<SearchHit> = ctx
            .iter()
            // An artifact with a set but no point cannot be offered: there is
            // no payload to render it from. That is the sweep having run
            // against an artifact whose embedding has not, not an error.
            .filter_map(|(id, set)| points.get(id).map(|p| (p, set)))
            .filter(|(p, _)| {
                let status = status_of(&p.payload);
                (filter.include_superseded || status != ArtifactStatus::Superseded)
                    && (filter.include_deprecated || status != ArtifactStatus::Deprecated)
            })
            .map(|(p, set)| SearchHit {
                payload: p.payload.clone(),
                // `max_sim`: the best of the artifact's situations, which is
                // what makes a set worth more than a mean.
                score: set
                    .iter()
                    .map(|c| cosine(vector, c))
                    .fold(f32::NEG_INFINITY, f32::max),
                // Not a query-to-document similarity, and calling it one would
                // invite it into a ranking it has no business in.
                similarity: None,
            })
            .collect();
        // Ties break on the id, so a HashMap's iteration order never decides
        // what is offered.
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.payload.artifact_id.cmp(&b.payload.artifact_id))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    async fn delete_artifacts(&self, artifact_ids: &[String]) -> Result<()> {
        let mut w = self.points.write().unwrap();
        // The context set goes with the point. A set outliving its payload
        // would keep answering `context_query` with an artifact that is gone.
        let mut c = self.ctx.write().unwrap();
        for id in artifact_ids {
            w.remove(id);
            c.remove(id);
        }
        Ok(())
    }

    async fn delete_by_corpus(&self, corpus_id: &str) -> Result<()> {
        let mut w = self.points.write().unwrap();
        let mut c = self.ctx.write().unwrap();
        w.retain(|id, p| {
            let keep = p.payload.corpus_id != corpus_id;
            if !keep {
                c.remove(id);
            }
            keep
        });
        Ok(())
    }

    async fn sample(&self, limit: usize) -> Result<Vec<(String, Vec<f32>)>> {
        let r = self.points.read().unwrap();
        let mut ids: Vec<&String> = r.keys().collect();
        // Deterministic, so a test never depends on HashMap iteration order —
        // the same rule `all_artifact_ids` obeys.
        ids.sort();
        Ok(ids
            .into_iter()
            .take(limit)
            .map(|id| (id.clone(), r[id].vector.clone()))
            .collect())
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
                provenance: None,
            },
        }
    }

    /// Superseded and deprecated included, so a test asserting the ordinary
    /// path is not silently asserting the filter instead.
    fn wide() -> SearchFilter {
        SearchFilter {
            include_superseded: true,
            include_deprecated: true,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn an_artifact_matches_on_its_nearest_situation_not_its_average() {
        // Friday afternoon *and* Monday morning. Their mean is a situation that
        // never happened, and a store that scored the mean would answer neither.
        let v = MemoryVectors::new();
        v.upsert(vec![point("a", "s1", vec![1.0, 1.0], &[], "procedure")])
            .await
            .unwrap();
        v.set_context_vectors("a", vec![vec![1.0, 0.0], vec![0.0, 1.0]])
            .await
            .unwrap();

        let friday = v.context_query(&[1.0, 0.0], 5, &wide()).await.unwrap();
        assert_eq!(friday.len(), 1);
        assert!((friday[0].score - 1.0).abs() < 1e-5);

        let monday = v.context_query(&[0.0, 1.0], 5, &wide()).await.unwrap();
        assert!((monday[0].score - 1.0).abs() < 1e-5);
    }

    #[tokio::test]
    async fn an_artifact_with_no_situations_is_not_a_candidate() {
        // The candidate set is "anything ever opened", and that is expressed by
        // the absence of a set rather than by a filter.
        let v = MemoryVectors::new();
        v.upsert(vec![
            point("a", "s1", vec![1.0, 0.0], &[], "procedure"),
            point("b", "s1", vec![1.0, 0.0], &[], "procedure"),
        ])
        .await
        .unwrap();
        v.set_context_vectors("a", vec![vec![1.0, 0.0]])
            .await
            .unwrap();

        let out = v.context_query(&[1.0, 0.0], 5, &wide()).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].payload.artifact_id, "a");
    }

    #[tokio::test]
    async fn an_empty_write_removes_the_set_and_leaves_the_point() {
        let v = MemoryVectors::new();
        v.upsert(vec![point("a", "s1", vec![1.0, 0.0], &[], "procedure")])
            .await
            .unwrap();
        v.set_context_vectors("a", vec![vec![1.0, 0.0]])
            .await
            .unwrap();
        v.set_context_vectors("a", vec![]).await.unwrap();

        assert!(
            v.context_query(&[1.0, 0.0], 5, &wide())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(v.count().await.unwrap(), 1, "the point is still there");
    }

    #[tokio::test]
    async fn a_hidden_artifact_is_never_offered() {
        // The same rule search obeys: superseded and deprecated are out.
        let v = MemoryVectors::new();
        v.upsert(vec![point("a", "s1", vec![1.0, 0.0], &[], "procedure")])
            .await
            .unwrap();
        v.set_context_vectors("a", vec![vec![1.0, 0.0]])
            .await
            .unwrap();
        v.set_lifecycle("a", ArtifactStatus::Superseded, Some("b"))
            .await
            .unwrap();

        let out = v
            .context_query(&[1.0, 0.0], 5, &SearchFilter::default())
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn results_come_back_best_first_and_capped() {
        let v = MemoryVectors::new();
        v.upsert(vec![
            point("near", "s1", vec![1.0, 0.0], &[], "procedure"),
            point("far", "s1", vec![1.0, 0.0], &[], "procedure"),
        ])
        .await
        .unwrap();
        v.set_context_vectors("near", vec![vec![1.0, 0.0]])
            .await
            .unwrap();
        v.set_context_vectors("far", vec![vec![0.2, 1.0]])
            .await
            .unwrap();

        let out = v.context_query(&[1.0, 0.0], 1, &wide()).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].payload.artifact_id, "near");
    }

    #[tokio::test]
    async fn a_set_on_an_artifact_with_no_point_is_not_an_error() {
        // An artifact whose embedding never ran has nothing to attach a set to.
        // The sweep must not fail over one.
        let v = MemoryVectors::new();
        v.set_context_vectors("nobody", vec![vec![1.0, 0.0]])
            .await
            .unwrap();
        assert!(
            v.context_query(&[1.0, 0.0], 5, &wide())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn deleting_an_artifact_takes_its_situations_with_it() {
        // A set outliving its point would keep answering `context_query` with
        // a payload that is gone.
        let v = MemoryVectors::new();
        v.upsert(vec![point("a", "s1", vec![1.0, 0.0], &[], "procedure")])
            .await
            .unwrap();
        v.set_context_vectors("a", vec![vec![1.0, 0.0]])
            .await
            .unwrap();

        v.delete_artifacts(&["a".to_string()]).await.unwrap();
        assert!(
            v.context_query(&[1.0, 0.0], 5, &wide())
                .await
                .unwrap()
                .is_empty()
        );

        // And the same when a whole corpus goes.
        v.upsert(vec![point("b", "s2", vec![1.0, 0.0], &[], "procedure")])
            .await
            .unwrap();
        v.set_context_vectors("b", vec![vec![1.0, 0.0]])
            .await
            .unwrap();
        v.delete_by_corpus("s2").await.unwrap();
        assert!(
            v.context_query(&[1.0, 0.0], 5, &wide())
                .await
                .unwrap()
                .is_empty()
        );
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
    async fn a_weighted_search_is_the_same_search_where_nothing_weighs_age() {
        // The default implementation ignores the weight, and this store is
        // what the tuning sweep's tests rank through: a sweep that silently
        // scored every candidate identically would still pass its own gate.
        let v = MemoryVectors::new();
        v.ensure_collection(3).await.unwrap();
        v.upsert(vec![
            point("near", "s1", vec![1.0, 0.0, 0.0], &["a"], "procedure"),
            point("far", "s1", vec![0.0, 0.0, 1.0], &["a"], "procedure"),
        ])
        .await
        .unwrap();

        let plain = v
            .search(&[1.0, 0.0, 0.0], &Default::default(), 10, &wide())
            .await
            .unwrap();
        let weighted = v
            .search_weighted(&[1.0, 0.0, 0.0], &Default::default(), 10, &wide(), 0.9)
            .await
            .unwrap();
        assert_eq!(
            plain
                .iter()
                .map(|h| &h.payload.artifact_id)
                .collect::<Vec<_>>(),
            weighted
                .iter()
                .map(|h| &h.payload.artifact_id)
                .collect::<Vec<_>>()
        );
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

    #[tokio::test]
    async fn a_sample_returns_ids_with_their_vectors_up_to_the_limit() {
        let v = MemoryVectors::new();
        v.upsert(vec![
            point("a", "s", vec![1.0, 0.0], &[], "c"),
            point("b", "s", vec![0.0, 1.0], &[], "c"),
            point("c", "s", vec![1.0, 1.0], &[], "c"),
        ])
        .await
        .unwrap();

        let all = v.sample(10).await.unwrap();
        assert_eq!(all.len(), 3);
        assert!(
            all.iter()
                .any(|(id, vec)| id == "a" && vec == &vec![1.0, 0.0]),
            "the sample carries each artifact's own vector: {all:?}"
        );

        let capped = v.sample(2).await.unwrap();
        assert_eq!(capped.len(), 2, "the limit is respected");
    }
}
