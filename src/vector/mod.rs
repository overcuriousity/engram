pub mod memory;
pub mod qdrant;
pub mod sparse;

use crate::error::Result;
use crate::store::artifacts::ArtifactStatus;
use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VectorPayload {
    pub artifact_id: String,
    pub corpus_id: String,
    pub text: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_count: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ArtifactStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
}

#[derive(Debug, Clone)]
pub struct VectorPoint {
    pub vector: Vec<f32>,
    pub sparse: sparse::SparseVector,
    pub payload: VectorPayload,
}

#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    pub tags: Vec<String>,
    pub category: Option<String>,
    pub include_superseded: bool,
    pub include_deprecated: bool,
}

impl SearchFilter {
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
            && self.category.is_none()
            && self.include_superseded
            && self.include_deprecated
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    #[serde(flatten)]
    pub payload: VectorPayload,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FacetCount {
    pub value: String,
    pub count: u64,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Facets {
    pub categories: Vec<FacetCount>,
    pub tags: Vec<FacetCount>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NearPair {
    pub a: String,
    pub b: String,
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct Touch {
    pub artifact_id: String,
    pub hit_count: Option<i64>,
    pub counts_as_hit: bool,
}

impl Touch {
    pub fn retrieved(artifact_id: &str, hit_count: Option<i64>) -> Touch {
        Touch {
            artifact_id: artifact_id.to_string(),
            hit_count,
            counts_as_hit: true,
        }
    }

    pub fn shown(artifact_id: &str) -> Touch {
        Touch {
            artifact_id: artifact_id.to_string(),
            hit_count: None,
            counts_as_hit: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredLifecycle {
    pub status: ArtifactStatus,
    pub superseded_by: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LifecycleRow {
    pub artifact_id: String,
    pub status: ArtifactStatus,
    pub superseded_by: Option<String>,
    pub last_verified_at: i64,
}

impl NearPair {
    pub fn new(x: &str, y: &str, score: f32) -> NearPair {
        let (a, b) = if x <= y { (x, y) } else { (y, x) };
        NearPair {
            a: a.to_string(),
            b: b.to_string(),
            score,
        }
    }
}

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn ensure_collection(&self, dim: usize) -> Result<()>;
    async fn upsert(&self, points: Vec<VectorPoint>) -> Result<()>;
    async fn set_payload(&self, payload: &VectorPayload) -> Result<()>;
    async fn set_superseded(&self, artifact_id: &str, superseded: bool) -> Result<()>;
    async fn set_lifecycle(
        &self,
        artifact_id: &str,
        status: ArtifactStatus,
        superseded_by: Option<&str>,
    ) -> Result<()>;
    async fn set_last_verified_at(
        &self,
        artifact_id: &str,
        at: i64,
        reset_hits: bool,
    ) -> Result<()>;
    async fn stale_candidates(
        &self,
        older_than: i64,
        max_hits: i64,
        limit: usize,
    ) -> Result<Vec<SearchHit>>;
    async fn search(
        &self,
        vector: &[f32],
        sparse: &sparse::SparseVector,
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>>;
    async fn touch(&self, targets: &[Touch], seen_at: i64) -> Result<()>;
    async fn apply_lifecycle(&self, rows: &[LifecycleRow]) -> Result<()>;
    async fn unstamped_count(&self) -> Result<u64>;
    async fn non_active_ids(&self, limit: usize) -> Result<Vec<String>>;
    async fn lifecycle_of(
        &self,
        artifact_ids: &[String],
    ) -> Result<std::collections::HashMap<String, StoredLifecycle>>;
    async fn all_artifact_ids(&self) -> Result<Vec<String>>;
    async fn payloads_of(
        &self,
        artifact_ids: &[String],
    ) -> Result<std::collections::HashMap<String, VectorPayload>>;
    async fn resurface(
        &self,
        limit: usize,
        older_than: i64,
        unseen_since: i64,
    ) -> Result<Vec<SearchHit>>;
    async fn facets(&self, limit: usize) -> Result<Facets>;
    async fn neighbours(&self, artifact_id: &str, limit: usize) -> Result<Vec<SearchHit>>;
    async fn near_pairs(
        &self,
        sample: usize,
        per_point: usize,
        min_score: f32,
    ) -> Result<Vec<NearPair>>;
    async fn delete_artifacts(&self, artifact_ids: &[String]) -> Result<()>;
    async fn delete_by_corpus(&self, corpus_id: &str) -> Result<()>;
    async fn count(&self) -> Result<u64>;
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}
