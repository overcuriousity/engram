pub mod memory;
pub mod qdrant;
pub mod sparse;

use crate::error::Result;
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
    /// When this chunk last appeared in results. Optional and omitted when
    /// unset, because Qdrant merges a payload write: a writer that does not
    /// know the stamp must leave the stored one alone rather than clear it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<i64>,
    /// Set when this artifact lost a near-identical pair to a newer one. Like
    /// `last_seen_at`, it is omitted when unset so that a writer which does not
    /// know the value — the embed job rebuilding a payload — leaves the stored
    /// one alone rather than reviving a hidden artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct VectorPoint {
    pub vector: Vec<f32>,
    /// BM25 terms for the same text. Empty when the store does not do hybrid
    /// retrieval, which is why it is a plain value rather than an option.
    pub sparse: sparse::SparseVector,
    pub payload: VectorPayload,
}

#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    /// All listed tags must be present (AND, not OR).
    pub tags: Vec<String>,
    pub category: Option<String>,
    /// Superseded artifacts are excluded by default. They are still stored and
    /// still readable by id — keeping them out of ranking is the whole of what
    /// superseding does.
    pub include_superseded: bool,
}

impl SearchFilter {
    /// Whether this filter narrows nothing. Excluding superseded points is
    /// still a narrowing, so a filter that only does that is not empty — saying
    /// otherwise would drop the clause on the way to Qdrant.
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty() && self.category.is_none() && self.include_superseded
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    #[serde(flatten)]
    pub payload: VectorPayload,
    pub score: f32,
}

/// One facet value and how many points carry it, counted straight from the
/// payload index rather than from a scan of SQLite.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FacetCount {
    pub value: String,
    pub count: u64,
}

/// What the search page offers to narrow by. Both lists arrive already sorted
/// by count, descending, because that is the order the chips are rendered in.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Facets {
    pub categories: Vec<FacetCount>,
    pub tags: Vec<FacetCount>,
}

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn ensure_collection(&self, dim: usize) -> Result<()>;
    async fn upsert(&self, points: Vec<VectorPoint>) -> Result<()>;
    /// Replace a point's payload, leaving its vector alone. Editing tags or a
    /// category changes nothing the embedding model saw, so re-embedding for it
    /// would spend an inference call to arrive at the same vector.
    async fn set_payload(&self, payload: &VectorPayload) -> Result<()>;
    /// Hide or unhide one artifact. A payload write, not a re-embed: which
    /// artifact won a near-identical pair changes nothing the embedding model
    /// saw.
    async fn set_superseded(&self, artifact_id: &str, superseded: bool) -> Result<()>;
    /// `sparse` carries the query's BM25 terms. An empty one means the query
    /// held no indexable token, and the lexical half is skipped rather than
    /// asked to match nothing.
    async fn search(
        &self,
        vector: &[f32],
        sparse: &sparse::SparseVector,
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>>;
    /// Record that these chunks were just shown. Merged into the stored
    /// payload, never written as a whole one.
    async fn touch(&self, artifact_ids: &[String], seen_at: i64) -> Result<()>;
    /// A random sample of chunks captured before `older_than` and not shown
    /// since `unseen_since`. Random rather than ranked: there is no query here,
    /// only the question of what has been forgotten.
    async fn resurface(
        &self,
        limit: usize,
        older_than: i64,
        unseen_since: i64,
    ) -> Result<Vec<SearchHit>>;
    /// Distinct categories and tags with their counts, each list capped at
    /// `limit` values. Feeds the filter chips, which exist so narrowing does
    /// not mean guessing which categories the corpus even contains.
    async fn facets(&self, limit: usize) -> Result<Facets>;
    /// The artifacts nearest this one, by the vector already stored for it —
    /// no embedding call, because the query is a point that is already in the
    /// index. The artifact itself is never among its own neighbours.
    async fn neighbours(&self, artifact_id: &str, limit: usize) -> Result<Vec<SearchHit>>;
    async fn delete_artifacts(&self, artifact_ids: &[String]) -> Result<()>;
    async fn delete_by_corpus(&self, corpus_id: &str) -> Result<()>;
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
