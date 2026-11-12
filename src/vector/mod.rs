pub mod memory;
pub mod qdrant;
pub mod sparse;
#[cfg(feature = "contained")]
pub mod sqlite;

#[cfg(test)]
pub(crate) mod conformance;

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
    /// Omitted when unset: Qdrant merges payload writes, so a writer that does not know the stamp must not clear it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<i64>,
    /// Read only by `stale_candidates`, never by scoring, or popular results would boost themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_count: Option<i64>,
    /// Mirrors `Chunk::status`; omitted when unset (see `last_seen_at`). Absent reads as active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ArtifactStatus>,
    /// What recency decay reads, in place of `created_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified_at: Option<i64>,
    /// Mirrors `Chunk::superseded_by` so a result can show the replacement without a lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// A projection of `artifact_sources`. `cap_per_corpus` groups on it, so a merge counts against each corpus.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub origin_corpora: Vec<String>,
    /// `passage` | `captured` | `merged` | `synthesized`, so a result can say a model wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
}

/// Test-only: in production a field added later must break every call site until someone answers for it.
#[cfg(test)]
impl Default for VectorPayload {
    fn default() -> Self {
        Self {
            artifact_id: String::new(),
            corpus_id: String::new(),
            text: String::new(),
            title: None,
            category: None,
            tags: vec![],
            created_at: 0,
            last_seen_at: None,
            hit_count: None,
            status: None,
            last_verified_at: None,
            superseded_by: None,
            origin_corpora: vec![],
            provenance: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VectorPoint {
    pub vector: Vec<f32>,
    /// Empty when the store does no hybrid retrieval.
    pub sparse: sparse::SparseVector,
    pub payload: VectorPayload,
}

/// Test-only: in production a field added later must break every call site until someone answers for it.
#[cfg(test)]
impl Default for VectorPoint {
    fn default() -> Self {
        Self {
            vector: vec![],
            sparse: Default::default(),
            payload: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    /// AND, not OR.
    pub tags: Vec<String>,
    pub category: Option<String>,
    /// Superseded artifacts stay readable by id; keeping them out of ranking is all superseding does.
    pub include_superseded: bool,
    /// Independent of `include_superseded`: callers may audit one status without the other.
    pub include_deprecated: bool,
    /// For the gap coverage check: did this capture answer something.
    pub corpus_id: Option<String>,
}

impl SearchFilter {}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    #[serde(flatten)]
    pub payload: VectorPayload,
    /// Not comparable across queries or stores (hybrid fuses ranks); use `similarity` to judge quality.
    pub score: f32,
    /// Cosine to the query vector, comparable across queries. `None` is "no opinion", not low: e.g. a lexical-only hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f32>,
}

/// Counted from the payload index, not a scan of SQLite.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FacetCount {
    pub value: String,
    pub count: u64,
}

/// Sorted by count descending, the chip order. Categories only: nothing renders tag chips.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Facets {
    pub categories: Vec<FacetCount>,
}

/// `hit_count` is `Some` when the caller already read it, sparing a round trip. Only `counts_as_hit` bumps it:
/// opening or resurfacing a review candidate must not count as retrieval and drop it from the list.
#[derive(Debug, Clone)]
pub struct Touch {
    pub artifact_id: String,
    pub hit_count: Option<i64>,
    pub counts_as_hit: bool,
}

impl Touch {
    /// Bumps both stamps.
    pub fn retrieved(artifact_id: &str, hit_count: Option<i64>) -> Touch {
        Touch {
            artifact_id: artifact_id.to_string(),
            hit_count,
            counts_as_hit: true,
        }
    }

    /// Opened by id or drawn by `resurface`; stamps `last_seen_at` only.
    pub fn shown(artifact_id: &str) -> Touch {
        Touch {
            artifact_id: artifact_id.to_string(),
            hit_count: None,
            counts_as_hit: false,
        }
    }
}

/// An absent `status` key reads as `Active`, as every filter treats it.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredLifecycle {
    pub status: ArtifactStatus,
    pub superseded_by: Option<String>,
}

/// `last_verified_at` is required: ranking decays against it, falling back to `created_at`.
#[derive(Debug, Clone)]
pub struct LifecycleRow {
    pub artifact_id: String,
    pub status: ArtifactStatus,
    pub superseded_by: Option<String>,
    pub last_verified_at: i64,
}

/// Together because a weight without a half-life has no curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Recency {
    pub weight: f32,
    pub half_life_days: u32,
}

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn ensure_collection(&self, dim: usize) -> Result<()>;
    async fn upsert(&self, points: Vec<VectorPoint>) -> Result<()>;
    /// Tags and category change nothing the embedding model saw, so no re-embed.
    async fn set_payload(&self, payload: &VectorPayload) -> Result<()>;
    /// A payload write, not a re-embed.
    async fn set_lifecycle(
        &self,
        artifact_id: &str,
        status: ArtifactStatus,
        superseded_by: Option<&str>,
    ) -> Result<()>;
    /// `reset_hits` zeroes `hit_count` (retrievals since verification); bulk writes pass `false` or every counter is wiped.
    async fn set_last_verified_at(
        &self,
        artifact_id: &str,
        at: i64,
        reset_hits: bool,
    ) -> Result<()>;
    /// Review candidates only, never a ranking signal. No `last_verified_at` is unknown, not stale; no `hit_count` is zero.
    async fn stale_candidates(
        &self,
        older_than: i64,
        max_hits: i64,
        limit: usize,
    ) -> Result<Vec<SearchHit>>;
    /// An empty `sparse` means no indexable token, and the lexical half is skipped.
    async fn search(
        &self,
        vector: &[f32],
        sparse: &sparse::SparseVector,
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>>;
    /// Per-call recency terms, since the idle pass ranks the same pairs under several settings.
    async fn search_weighted(
        &self,
        vector: &[f32],
        sparse: &sparse::SparseVector,
        limit: usize,
        filter: &SearchFilter,
        recency: Recency,
    ) -> Result<Vec<SearchHit>> {
        let _ = recency;
        self.search(vector, sparse, limit, filter).await
    }
    /// Default `false`, matching the default `search_weighted`; an explanation must not report a stage that never ran.
    fn applies_scoring_formula(&self) -> bool {
        false
    }
    /// Merged into the stored payload; `hit_count` only for targets marked `counts_as_hit`.
    async fn touch(&self, targets: &[Touch], seen_at: i64) -> Result<()>;
    async fn apply_lifecycle(&self, rows: &[LifecycleRow]) -> Result<()>;
    /// Per-id comparison, since membership in two truncated lists proves nothing.
    async fn lifecycle_of(
        &self,
        artifact_ids: &[String],
    ) -> Result<std::collections::HashMap<String, StoredLifecycle>>;
    /// Unbounded on purpose: the heal compares the whole base both ways. Points without `artifact_id` are not engram's.
    async fn all_artifact_ids(&self) -> Result<Vec<String>>;
    async fn payloads_of(
        &self,
        artifact_ids: &[String],
    ) -> Result<std::collections::HashMap<String, VectorPayload>>;
    /// Random rather than ranked: there is no query, only what has been forgotten.
    async fn resurface(
        &self,
        limit: usize,
        older_than: i64,
        unseen_since: i64,
    ) -> Result<Vec<SearchHit>>;
    async fn facets(&self, limit: usize) -> Result<Facets>;
    /// By the stored vector, no embedding call; never includes the artifact itself.
    async fn neighbours(&self, artifact_id: &str, limit: usize) -> Result<Vec<SearchHit>>;
    /// A vector write, never a point write: `upsert` would clear `last_seen_at` and `status` (see `qdrant.rs`).
    /// Empty `vectors` removes the set; a missing point is not an error.
    async fn set_context_vectors(&self, artifact_id: &str, vectors: Vec<Vec<f32>>) -> Result<()>;
    /// `max_sim` over each set, so any situation matches; a mean would be one that never happened.
    /// `similarity` is `None`: this must not enter a query-vs-document ranking.
    async fn context_query(
        &self,
        vector: &[f32],
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>>;
    async fn delete_artifacts(&self, artifact_ids: &[String]) -> Result<()>;
    async fn delete_by_corpus(&self, corpus_id: &str) -> Result<()>;
    /// Decorative, so unfiltered; deterministic between identical calls, no other order.
    async fn sample(&self, limit: usize) -> Result<Vec<(String, Vec<f32>)>>;
    /// Read back rather than re-embedded. `None` for a missing point.
    async fn dense_of(&self, artifact_id: &str) -> Result<Option<Vec<f32>>>;
    async fn count(&self) -> Result<u64>;
    /// The background's cache tag needs it: a `--reindex` can keep the count but change every vector.
    async fn revision(&self) -> Result<Option<String>> {
        Ok(None)
    }
    /// `Embedded` when the vectors live in the tenant database, which the backup copies anyway.
    async fn export_snapshot(&self, _into: &std::path::Path) -> Result<VectorExport> {
        Err(crate::error::Error::Vector(
            "this vector store cannot export a snapshot".into(),
        ))
    }
    /// Never touches what the store serves until the caller adopts `target`.
    async fn import_snapshot(
        &self,
        _from: &std::path::Path,
        _target: &str,
        _dim: usize,
    ) -> Result<()> {
        Err(crate::error::Error::Vector(
            "this vector store cannot import a snapshot".into(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum VectorExport {
    Embedded,
    File {
        generation: String,
        sha256: String,
        bytes: u64,
        points: u64,
        dim: usize,
    },
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    // Different widths are not one space: zipping would give a plausible number for a bug. Fail closed.
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    // A zero vector has no direction; maximally dissimilar instead of NaN.
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

#[cfg(test)]
mod tests {
    use super::cosine;

    #[test]
    fn a_width_mismatch_scores_zero_rather_than_a_truncated_cosine() {
        // Zipping alone would give an in-range, meaningless number.
        let short = [1.0, 1.0];
        let long = [1.0, 1.0, 1.0];
        assert_eq!(cosine(&short, &long), 0.0);
        assert_eq!(cosine(&long, &short), 0.0);
    }

    #[test]
    fn equal_widths_are_untouched_by_the_guard() {
        let a = [1.0, 0.0];
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-6);
        assert!((cosine(&a, &[0.0, 1.0]) - 0.0).abs() < 1e-6);
    }
}
