use super::{
    FacetCount, Facets, SearchFilter, SearchHit, VectorPayload, VectorPoint, VectorStore, cosine,
};
use crate::error::{Error, Result};
use crate::store::artifacts::ArtifactStatus;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::RwLock;

/// Brute-force cosine, so ingest-to-search is testable without Qdrant.
pub struct MemoryVectors {
    points: RwLock<HashMap<String, VectorPoint>>,
    /// Apart from the point so `VectorPoint` signatures need not carry a context set.
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

/// Unset reads as active, matching Qdrant's `build_filter`.
fn status_of(payload: &super::VectorPayload) -> ArtifactStatus {
    payload.status.unwrap_or(ArtifactStatus::Active)
}

/// Ties break on the value so HashMap order never reaches the page.
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
        let mut w = self
            .points
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for mut p in points {
            // As in Qdrant: a re-embed must not clear the stamp, or yesterday's chunk looks forgotten.
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
            // Unset means "keep stored", so a re-embed cannot revive an artifact the sweep hid.
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
        let mut w = self
            .points
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(p) = w.get_mut(&payload.artifact_id) {
            // A merge, matching Qdrant: an absent stamp means unchanged.
            let seen = payload.last_seen_at.or(p.payload.last_seen_at);
            let hits = payload.hit_count.or(p.payload.hit_count);
            let status = payload.status.or(p.payload.status);
            let verified = payload.last_verified_at.or(p.payload.last_verified_at);
            let superseded_by = payload
                .superseded_by
                .clone()
                .or_else(|| p.payload.superseded_by.clone());
            // Only `jobs::embed` knows these; a tag edit's empty values must not strip a synthesized artifact's provenance.
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
        let mut w = self
            .points
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut w = self
            .points
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(p) = w.get_mut(artifact_id) {
            p.payload.last_verified_at = Some(at);
            if reset_hits {
                p.payload.hit_count = Some(0);
            }
        }
        Ok(())
    }

    async fn apply_lifecycle(&self, rows: &[super::LifecycleRow]) -> Result<()> {
        let mut w = self
            .points
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let r = self
            .points
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let r = self
            .points
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(artifact_ids
            .iter()
            .filter_map(|id| Some((id.clone(), r.get(id)?.payload.clone())))
            .collect())
    }

    async fn all_artifact_ids(&self) -> Result<Vec<String>> {
        let r = self
            .points
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let r = self
            .points
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut found: Vec<&VectorPayload> = r
            .values()
            .map(|p| &p.payload)
            .filter(|p| {
                status_of(p) == ArtifactStatus::Active
                    // An unstamped point is unknown, not stale; a missing `hit_count` means never retrieved.
                    && p.last_verified_at.is_some_and(|v| v < older_than)
                    && p.hit_count.unwrap_or(0) <= max_hits
            })
            .collect();
        // A work queue needs a stable order; stalest first, id tiebreak, matching Qdrant so the two test alike.
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
                similarity: None,
            })
            .collect())
    }

    async fn touch(&self, targets: &[super::Touch], seen_at: i64) -> Result<()> {
        let mut w = self
            .points
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for t in targets {
            // The caller's count only spares Qdrant a round trip; this store reads its own for free.
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
        let r = self
            .points
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(r.values()
            .filter(|p| {
                // Anything not active is out, matching Qdrant: a deprecated artifact was retired on purpose.
                status_of(&p.payload) == ArtifactStatus::Active
                    && p.payload.created_at < older_than
                    && p.payload.last_seen_at.is_none_or(|s| s < unseen_since)
            })
            .take(limit)
            .map(|p| SearchHit {
                payload: p.payload.clone(),
                score: 0.0,
                similarity: None,
            })
            .collect())
    }

    /// Dense only: hybrid fusion is Qdrant's, covered by the integration suite.
    async fn search(
        &self,
        vector: &[f32],
        _sparse: &super::sparse::SparseVector,
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>> {
        let r = self
            .points
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut hits: Vec<SearchHit> = r
            .values()
            .filter(|p| {
                let status = status_of(&p.payload);
                status != ArtifactStatus::Trashed
                    && (filter.include_superseded || status != ArtifactStatus::Superseded)
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
                // Dense only, so the ranking score is the similarity.
                let similarity = cosine(vector, &p.vector);
                SearchHit {
                    payload: p.payload.clone(),
                    score: similarity,
                    similarity: Some(similarity),
                }
            })
            .collect();
        // Tie-break on artifact_id for a stable order.
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.payload.artifact_id.cmp(&b.payload.artifact_id))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    async fn facets(&self, limit: usize) -> Result<Facets> {
        let r = self
            .points
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let r = self
            .points
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(seed) = r.get(artifact_id) else {
            return Ok(vec![]);
        };
        let mut hits: Vec<SearchHit> = r
            .values()
            .filter(|p| {
                p.payload.artifact_id != artifact_id
                    && status_of(&p.payload) == ArtifactStatus::Active
            })
            .map(|p| {
                let cos = cosine(&seed.vector, &p.vector);
                SearchHit {
                    payload: p.payload.clone(),
                    score: cos,
                    // One dense lookup, so the score is the cosine `review_min` compares against.
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
        let mut w = self
            .ctx
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let points = self
            .points
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ctx = self
            .ctx
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut hits: Vec<SearchHit> = ctx
            .iter()
            // A set without a point has no payload to render; the sweep ran before the embedding did.
            .filter_map(|(id, set)| points.get(id).map(|p| (p, set)))
            .filter(|(p, _)| {
                let status = status_of(&p.payload);
                status != ArtifactStatus::Trashed
                    && (filter.include_superseded || status != ArtifactStatus::Superseded)
                    && (filter.include_deprecated || status != ArtifactStatus::Deprecated)
            })
            .map(|(p, set)| SearchHit {
                payload: p.payload.clone(),
                // `max_sim`: the best situation, which is why a set beats a mean.
                score: set
                    .iter()
                    .map(|c| cosine(vector, c))
                    .fold(f32::NEG_INFINITY, f32::max),
                // `max_sim` is not a query-to-document similarity.
                similarity: None,
            })
            .collect();
        // Ties break on the id, never on HashMap order.
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
        let mut w = self
            .points
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A set outliving its payload would keep answering `context_query` with a gone artifact.
        let mut c = self
            .ctx
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for id in artifact_ids {
            w.remove(id);
            c.remove(id);
        }
        Ok(())
    }

    async fn delete_by_corpus(&self, corpus_id: &str) -> Result<()> {
        let mut w = self
            .points
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut c = self
            .ctx
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        w.retain(|id, p| {
            let keep = p.payload.corpus_id != corpus_id;
            if !keep {
                c.remove(id);
            }
            keep
        });
        Ok(())
    }

    async fn dense_of(&self, artifact_id: &str) -> Result<Option<Vec<f32>>> {
        Ok(self
            .points
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(artifact_id)
            .map(|p| p.vector.clone()))
    }

    async fn sample(&self, limit: usize) -> Result<Vec<(String, Vec<f32>)>> {
        let r = self
            .points
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ids: Vec<&String> = r.keys().collect();
        // Deterministic, as `all_artifact_ids` is.
        ids.sort();
        Ok(ids
            .into_iter()
            .take(limit)
            .map(|id| (id.clone(), r[id].vector.clone()))
            .collect())
    }

    async fn count(&self) -> Result<u64> {
        Ok(self
            .points
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len() as u64)
    }

    /// JSON of every point, a fixture format so the whole backup runs without Qdrant.
    async fn export_snapshot(&self, into: &std::path::Path) -> Result<super::VectorExport> {
        let mut points: Vec<SnapshotPoint> = {
            let r = self
                .points
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ctx = self
                .ctx
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            r.values()
                .map(|p| SnapshotPoint {
                    vector: p.vector.clone(),
                    sparse: (p.sparse.indices.clone(), p.sparse.values.clone()),
                    payload: p.payload.clone(),
                    ctx: ctx.get(&p.payload.artifact_id).cloned().unwrap_or_default(),
                })
                .collect()
        };
        points.sort_by(|a, b| a.payload.artifact_id.cmp(&b.payload.artifact_id));
        let dim = points.first().map_or(0, |p| p.vector.len());
        let file = crate::backup::create_private(into)?;
        serde_json::to_writer(std::io::BufWriter::new(file), &points)
            .map_err(|e| Error::Vector(format!("{}: {e}", into.display())))?;
        let (sha256, bytes) = crate::backup::manifest::sha256_file(into)?;
        Ok(super::VectorExport::File {
            generation: "memory".into(),
            sha256,
            bytes,
            points: points.len() as u64,
            dim,
        })
    }

    async fn import_snapshot(
        &self,
        from: &std::path::Path,
        _target: &str,
        dim: usize,
    ) -> Result<()> {
        let file = std::fs::File::open(from)
            .map_err(|e| Error::Vector(format!("{}: {e}", from.display())))?;
        let points: Vec<SnapshotPoint> = serde_json::from_reader(std::io::BufReader::new(file))
            .map_err(|e| Error::Vector(format!("{}: {e}", from.display())))?;
        if let Some(p) = points.iter().find(|p| p.vector.len() != dim) {
            return Err(super::qdrant::dimension_mismatch(
                "memory",
                dim,
                p.vector.len(),
            ));
        }
        let mut w = self
            .points
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ctx = self
            .ctx
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for p in points {
            let id = p.payload.artifact_id.clone();
            if !p.ctx.is_empty() {
                ctx.insert(id.clone(), p.ctx);
            }
            w.insert(
                id,
                VectorPoint {
                    vector: p.vector,
                    sparse: super::sparse::SparseVector {
                        indices: p.sparse.0,
                        values: p.sparse.1,
                    },
                    payload: p.payload,
                },
            );
        }
        Ok(())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SnapshotPoint {
    vector: Vec<f32>,
    sparse: (Vec<u32>, Vec<f32>),
    payload: VectorPayload,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ctx: Vec<Vec<f32>>,
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
                ..Default::default()
            },
        }
    }

    fn wide() -> SearchFilter {
        SearchFilter {
            include_superseded: true,
            include_deprecated: true,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_snapshot_imports_into_a_fresh_store_with_the_same_ids() {
        let m = MemoryVectors::new();
        m.upsert(vec![
            point("b", "s", vec![0.0, 1.0], &["t"], "c"),
            point("a", "s", vec![1.0, 0.0], &[], "c"),
        ])
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("m.snapshot");
        let crate::vector::VectorExport::File {
            sha256,
            bytes,
            points,
            dim,
            ..
        } = m.export_snapshot(&file).await.unwrap()
        else {
            panic!("a memory store exports a file");
        };
        assert_eq!((points, dim), (2, 2));
        assert_eq!(
            crate::backup::manifest::sha256_file(&file).unwrap(),
            (sha256, bytes)
        );

        let fresh = MemoryVectors::new();
        fresh.import_snapshot(&file, "next", 2).await.unwrap();
        assert_eq!(
            fresh.all_artifact_ids().await.unwrap(),
            m.all_artifact_ids().await.unwrap()
        );
        assert!(
            MemoryVectors::new()
                .import_snapshot(&file, "next", 3)
                .await
                .is_err(),
            "a snapshot of another width is refused"
        );
    }

    #[tokio::test]
    async fn dense_of_returns_the_stored_vector_and_none_for_a_stranger() {
        let m = MemoryVectors::new();
        m.upsert(vec![point("a1", "s1", vec![0.1, 0.2, 0.3], &[], "concept")])
            .await
            .unwrap();
        assert_eq!(m.dense_of("a1").await.unwrap(), Some(vec![0.1, 0.2, 0.3]));
        assert_eq!(m.dense_of("nope").await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_artifact_matches_on_its_nearest_situation_not_its_average() {
        // Their mean is a situation that never happened.
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
        // "Anything ever opened" is the absence of a set, not a filter.
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
        // The idle pass's tests rank through this store; ignoring the weight would pass its gate silently.
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
            .search_weighted(
                &[1.0, 0.0, 0.0],
                &Default::default(),
                10,
                &wide(),
                crate::vector::Recency {
                    weight: 0.9,
                    half_life_days: 180,
                },
            )
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

    mod conforms {
        crate::vector::conformance::suite!(async { Box::new(super::super::MemoryVectors::new()) });
    }
}
