use super::{SearchFilter, SearchHit, VectorPoint, VectorStore, cosine};
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

#[async_trait]
impl VectorStore for MemoryVectors {
    async fn ensure_collection(&self, _dim: usize) -> Result<()> {
        Ok(())
    }

    async fn upsert(&self, points: Vec<VectorPoint>) -> Result<()> {
        let mut w = self.points.write().unwrap();
        for p in points {
            w.insert(p.payload.chunk_id.clone(), p);
        }
        Ok(())
    }

    async fn search(
        &self,
        vector: &[f32],
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>> {
        let r = self.points.read().unwrap();
        let mut hits: Vec<SearchHit> = r
            .values()
            .filter(|p| {
                filter.tags.iter().all(|t| p.payload.tags.contains(t))
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
        // Tie-break on chunk_id so equal scores produce a stable order rather
        // than whatever the HashMap iterated this time.
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.payload.chunk_id.cmp(&b.payload.chunk_id))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    async fn delete_chunks(&self, chunk_ids: &[String]) -> Result<()> {
        let mut w = self.points.write().unwrap();
        for id in chunk_ids {
            w.remove(id);
        }
        Ok(())
    }

    async fn delete_by_source(&self, source_id: &str) -> Result<()> {
        let mut w = self.points.write().unwrap();
        w.retain(|_, p| p.payload.source_id != source_id);
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
            payload: VectorPayload {
                chunk_id: id.into(),
                source_id: src.into(),
                text: format!("text of {id}"),
                title: Some(id.into()),
                category: Some(cat.into()),
                tags: tags.iter().map(|s| s.to_string()).collect(),
                created_at: 0,
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
            .search(&[1.0, 0.0, 0.0], 10, &SearchFilter::default())
            .await
            .unwrap();
        assert_eq!(hits[0].payload.chunk_id, "near");
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
            v.search(&[1.0, 0.0, 0.0], 2, &SearchFilter::default())
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
        };
        let hits = v.search(&[1.0, 0.0, 0.0], 10, &f).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].payload.chunk_id, "both");
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
        };
        let hits = v.search(&[1.0, 0.0, 0.0], 10, &f).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].payload.chunk_id, "c");
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
            .search(&[0.0, 1.0, 0.0], 1, &SearchFilter::default())
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
        v.delete_by_source("s1").await.unwrap();
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
            .search(&[1.0, 0.0, 0.0], 10, &SearchFilter::default())
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
            .search(&[1.0, 0.0, 0.0], 5, &SearchFilter::default())
            .await
            .unwrap();
        for _ in 0..5 {
            let again = v
                .search(&[1.0, 0.0, 0.0], 5, &SearchFilter::default())
                .await
                .unwrap();
            assert_eq!(
                first
                    .iter()
                    .map(|h| &h.payload.chunk_id)
                    .collect::<Vec<_>>(),
                again
                    .iter()
                    .map(|h| &h.payload.chunk_id)
                    .collect::<Vec<_>>(),
                "identical scores produced an unstable ordering"
            );
        }
    }
}
