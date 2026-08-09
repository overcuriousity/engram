pub mod memory;
pub mod qdrant;

use crate::error::Result;
use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VectorPayload {
    pub chunk_id: String,
    pub source_id: String,
    pub text: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct VectorPoint {
    pub vector: Vec<f32>,
    pub payload: VectorPayload,
}

#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    /// All listed tags must be present (AND, not OR).
    pub tags: Vec<String>,
    pub category: Option<String>,
}

impl SearchFilter {
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty() && self.category.is_none()
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    #[serde(flatten)]
    pub payload: VectorPayload,
    pub score: f32,
}

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn ensure_collection(&self, dim: usize) -> Result<()>;
    async fn upsert(&self, points: Vec<VectorPoint>) -> Result<()>;
    async fn search(
        &self,
        vector: &[f32],
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>>;
    async fn delete_chunks(&self, chunk_ids: &[String]) -> Result<()>;
    async fn delete_by_source(&self, source_id: &str) -> Result<()>;
    async fn count(&self) -> Result<u64>;
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    // A zero vector has no direction; call it maximally dissimilar rather than
    // dividing by zero and poisoning the ranking with NaN.
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}
