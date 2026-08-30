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
    /// When this chunk last appeared in results. Optional and omitted when
    /// unset, because Qdrant merges a payload write: a writer that does not
    /// know the stamp must leave the stored one alone rather than clear it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<i64>,
    /// How many times this chunk has appeared in results, ever. Bumped
    /// alongside `last_seen_at`. Read only by the deprecation-candidate query
    /// (`stale_candidates`) — deliberately never a term in search scoring, or
    /// a popular result would keep boosting itself further while a correct
    /// but rarely-queried artifact never gets the chance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_count: Option<i64>,
    /// Active, deprecated, or superseded. Mirrors the SQLite source of truth
    /// (`store::artifacts::Chunk::status`). Omitted when unset for the same
    /// merge-write reason as `last_seen_at`; absent is treated as active by
    /// every filter, which is what a hand-written point carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ArtifactStatus>,
    /// When this artifact was last confirmed accurate. What search ranking's
    /// recency decay reads, in place of `created_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified_at: Option<i64>,
    /// The artifact this one was superseded by, if any. Mirrors
    /// `Chunk::superseded_by` so a search result can show the replacement
    /// without a second lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Every corpus this artifact draws from — one for a passage or captured
    /// row, several for a merge. A projection of `artifact_sources`, the way
    /// `status` is a projection of the row; SQLite stays the authority.
    /// `cap_per_corpus` groups on it, so a merge counts against each of its
    /// corpora instead of all merges landing under one empty key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub origin_corpora: Vec<String>,
    /// `passage` | `captured` | `merged` | `synthesized` — mirrors the row so a
    /// result can say a model wrote it without a second lookup, and so the
    /// search path can see a synthesized artifact lead the list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
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
    /// Deprecated artifacts (flagged stale, no specific replacement) are
    /// excluded by default, independently of `include_superseded` — the two
    /// statuses mean different things and callers may want to audit one
    /// without the other.
    pub include_deprecated: bool,
    /// Only artifacts of this one document. What the gap coverage check needs:
    /// the question is whether *this capture* answered something, and a hit
    /// from anywhere else answers a different question — the base held it all
    /// along, and the gap is open for a reason.
    pub corpus_id: Option<String>,
}

impl SearchFilter {
    /// Whether this filter narrows nothing. Excluding superseded/deprecated
    /// points is still a narrowing, so a filter that only does that is not
    /// empty — saying otherwise would drop the clause on the way to Qdrant.
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
            && self.category.is_none()
            && self.corpus_id.is_none()
            && self.include_superseded
            && self.include_deprecated
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    #[serde(flatten)]
    pub payload: VectorPayload,
    /// What this hit was ranked by. Deliberately not comparable across queries
    /// or across stores: hybrid retrieval fuses ranks, so this says where the
    /// result placed rather than how good it was. Use `similarity` to judge
    /// whether it is any good.
    pub score: f32,
    /// Cosine similarity between the query vector and this artifact's, when the
    /// store can say. This *is* comparable across queries — that is the whole
    /// difference from `score` — so it is what decides whether a result is
    /// worth presenting as an answer.
    ///
    /// `None` means "no opinion", and is not the same as a low value. It covers
    /// a hit the lexical half found that the dense half did not return, which is
    /// an exact term match and the opposite of weak; a store with no notion of a
    /// query vector, as in the listing methods; and any store that does not
    /// implement the second lookup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f32>,
}

/// One facet value and how many points carry it, counted straight from the
/// payload index rather than from a scan of SQLite.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FacetCount {
    pub value: String,
    pub count: u64,
}

/// What the search page offers to narrow by, already sorted by count,
/// descending, because that is the order the chips are rendered in.
///
/// Categories only. A tag row was here too, and the page stopped rendering it
/// when the vocabulary folded onto a closed list of form words — counting a
/// facet nothing displays cost a round trip per page load. Tags are still
/// stored, still what pinning rides on, and still filter
/// `/ui/search/results?tags=`; nothing offers them as chips.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Facets {
    pub categories: Vec<FacetCount>,
}

/// One artifact to stamp as shown, and the `hit_count` the caller already read
/// for it.
///
/// `hit_count` is `Some` when the caller has just seen the stored payload — a
/// marked search holds every hit's count already, so passing it spares `touch`
/// a read round trip it would otherwise pay on every query. `None` means the
/// caller does not know (opening one artifact by id), and the store reads the
/// current value itself. It is only ever read when `counts_as_hit`.
///
/// `counts_as_hit` separates the two stamps this carries. `last_seen_at` answers
/// "when was this last put in front of anyone", which every path updates —
/// that is what keeps the forgotten-chunks list from offering the same artifact
/// every day. `hit_count` answers "how often did search return this as an
/// answer since it was last verified", which is what `stale_candidates` reads,
/// so only a query result may increment it. Opening one artifact from the
/// review list, or being drawn at random by `resurface`, is the operator
/// *looking at* a candidate — counting either as a retrieval is how reading a
/// row used to remove it from the very list that offered it.
#[derive(Debug, Clone)]
pub struct Touch {
    pub artifact_id: String,
    pub hit_count: Option<i64>,
    pub counts_as_hit: bool,
}

impl Touch {
    /// Returned by a search: bumps both stamps. `hit_count` is the value the
    /// caller just read, or `None` to make the store look it up.
    pub fn retrieved(artifact_id: &str, hit_count: Option<i64>) -> Touch {
        Touch {
            artifact_id: artifact_id.to_string(),
            hit_count,
            counts_as_hit: true,
        }
    }

    /// Shown without being asked for — an artifact opened by id, or drawn by
    /// `resurface`. Stamps `last_seen_at` only.
    pub fn shown(artifact_id: &str) -> Touch {
        Touch {
            artifact_id: artifact_id.to_string(),
            hit_count: None,
            counts_as_hit: false,
        }
    }
}

/// What a point's payload says about its lifecycle, as the drift repair reads
/// it back. A point missing from a `lifecycle_of` answer is simply absent from
/// the map; an absent `status` key reads as `Active`, which is how every filter
/// treats it.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredLifecycle {
    pub status: ArtifactStatus,
    pub superseded_by: Option<String>,
}

/// One artifact's SQLite-side lifecycle state, as the drift repair pushes it
/// into the vector store. `last_verified_at` is never optional here: ranking
/// decays against that stamp, and an artifact never explicitly verified falls
/// back to its `created_at`.
#[derive(Debug, Clone)]
pub struct LifecycleRow {
    pub artifact_id: String,
    pub status: ArtifactStatus,
    pub superseded_by: Option<String>,
    pub last_verified_at: i64,
}

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn ensure_collection(&self, dim: usize) -> Result<()>;
    async fn upsert(&self, points: Vec<VectorPoint>) -> Result<()>;
    /// Replace a point's payload, leaving its vector alone. Editing tags or a
    /// category changes nothing the embedding model saw, so re-embedding for it
    /// would spend an inference call to arrive at the same vector.
    async fn set_payload(&self, payload: &VectorPayload) -> Result<()>;
    /// Set an artifact's lifecycle status (and, for `Superseded`, the winner
    /// it was replaced by). A payload write, not a re-embed: which artifact won
    /// a near-identical pair changes nothing the embedding model saw.
    async fn set_lifecycle(
        &self,
        artifact_id: &str,
        status: ArtifactStatus,
        superseded_by: Option<&str>,
    ) -> Result<()>;
    /// Stamp an artifact as confirmed accurate now — what the recency-decay
    /// scoring formula reads.
    ///
    /// `reset_hits` also zeroes `hit_count`, because `stale_max_hits` counts
    /// retrievals *since* the last verification. An operator verifying an
    /// artifact passes `true`; a bulk lifecycle write passes `false`, since it
    /// stamps every artifact and would otherwise wipe every counter.
    async fn set_last_verified_at(
        &self,
        artifact_id: &str,
        at: i64,
        reset_hits: bool,
    ) -> Result<()>;
    /// Active artifacts confirmed stale a while ago (`last_verified_at` older
    /// than `older_than`) and rarely or never retrieved since (`hit_count` at
    /// most `max_hits`) — candidates for an operator to review and deprecate.
    /// A one-way signal: it can only surface a candidate, never rank anything
    /// higher, so it cannot create a popularity feedback loop.
    ///
    /// A point with no `last_verified_at` at all is *not* a candidate. Missing
    /// means unknown, not stale, and treating those as maximally stale would
    /// invite an operator to deprecate points nothing knows anything about. A
    /// missing `hit_count` is different — never retrieved is a fact the absent
    /// key states correctly — so it counts as zero.
    async fn stale_candidates(
        &self,
        older_than: i64,
        max_hits: i64,
        limit: usize,
    ) -> Result<Vec<SearchHit>>;
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
    /// `search`, with the recency weight chosen per call rather than fixed
    /// when the store was connected.
    ///
    /// The default ignores it and delegates: only a store that applies recency
    /// at all has anything to vary, and the tuning sweep is the one caller that
    /// needs to — it ranks the same pairs under several weights in one pass, so
    /// the knob cannot live in the connection.
    async fn search_weighted(
        &self,
        vector: &[f32],
        sparse: &sparse::SparseVector,
        limit: usize,
        filter: &SearchFilter,
        recency_weight: f32,
    ) -> Result<Vec<SearchHit>> {
        let _ = recency_weight;
        self.search(vector, sparse, limit, filter).await
    }
    /// Whether this store actually runs the recency-decay and pinned-boost
    /// formula over its scores.
    ///
    /// Default `false`, because the default `search_weighted` above drops the
    /// weight on the floor and delegates to a plain `search`. Only a store
    /// that overrides it scores those terms, and an explanation that reports a
    /// `recency +0.05` for a stage that never ran contradicts the very
    /// ranking it claims to explain — which is the one thing the design record
    /// forbids. Asked, rather than assumed from the configuration, because the
    /// configuration says what was *asked for* and this says what *happened*.
    fn applies_scoring_formula(&self) -> bool {
        false
    }
    /// Record that these chunks were just shown. Merged into the stored
    /// payload, never written as a whole one. `last_seen_at` is stamped for
    /// every target; `hit_count` only for the ones marked `counts_as_hit`.
    async fn touch(&self, targets: &[Touch], seen_at: i64) -> Result<()>;
    /// Push a batch of artifacts' SQLite-side lifecycle state into the store,
    /// as one request rather than one per artifact per field. What the sweep's
    /// drift repair uses.
    async fn apply_lifecycle(&self, rows: &[LifecycleRow]) -> Result<()>;
    /// What these artifacts' payloads currently say about their lifecycle. Ids
    /// with no point are absent from the answer.
    ///
    /// The drift repair needs this because set membership in two independently
    /// truncated lists proves nothing: an id missing from a capped scan may be
    /// in agreement and simply past the cap. Comparing the stored value per id
    /// is what tells an actual disagreement from the edge of a page.
    async fn lifecycle_of(
        &self,
        artifact_ids: &[String],
    ) -> Result<std::collections::HashMap<String, StoredLifecycle>>;
    /// Every artifact id the store holds a point for. Unbounded on purpose:
    /// the one caller is the heal, which is already a pass over the whole base
    /// and compares this against SQLite in both directions.
    ///
    /// A point whose payload carries no `artifact_id` at all cannot appear
    /// here. It is not an engram point and nothing can be said about it.
    async fn all_artifact_ids(&self) -> Result<Vec<String>>;
    /// The full stored payloads for these ids, as the store holds them. Ids
    /// with no point are absent from the answer.
    async fn payloads_of(
        &self,
        artifact_ids: &[String],
    ) -> Result<std::collections::HashMap<String, VectorPayload>>;
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
    /// Replace this artifact's set of context centroids — the `ctx`
    /// multivector, scored with `max_sim`.
    ///
    /// A **vector** write, never a point write. `upsert` replaces the whole
    /// payload, and a writer that does not know when the artifact was last
    /// shown would clear `last_seen_at` and `status` with it — which puts every
    /// artifact the sweep hid straight back into search. See `qdrant.rs`'s
    /// `upsert` for the same hazard stated where it bites.
    ///
    /// An empty `vectors` removes the set. That is the ordinary case for an
    /// artifact whose every cluster has decayed below `min_weight`, not an
    /// error, and it must leave the point and its dense vector alone.
    ///
    /// An artifact with no point is not a failure: its embedding may never have
    /// run. There is nothing to attach a set to, and nothing to complain about.
    async fn set_context_vectors(&self, artifact_id: &str, vectors: Vec<Vec<f32>>) -> Result<()>;
    /// The artifacts whose learned situations most resemble this one.
    ///
    /// `max_sim` over each artifact's set: an artifact matches if *any* of its
    /// situations does, which is the whole reason the profile is a set rather
    /// than a mean. A thing looked up on Friday afternoons and occasionally on
    /// Monday mornings must match both, and their average is a situation that
    /// never happened.
    ///
    /// Points carrying no set are absent from the answer, so the candidates are
    /// "anything ever opened" without a filter saying so.
    ///
    /// `score` is the `max_sim`. `similarity` is `None`: this is not a query
    /// vector against a document vector, and calling it a similarity would
    /// invite it into a ranking it has no business in.
    async fn context_query(
        &self,
        vector: &[f32],
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>>;
    async fn delete_artifacts(&self, artifact_ids: &[String]) -> Result<()>;
    async fn delete_by_corpus(&self, corpus_id: &str) -> Result<()>;
    /// Up to `limit` points' dense vectors, for the background visualization.
    ///
    /// A decorative read, which is why nothing is filtered: deprecated and
    /// superseded points are still part of the store's shape, and a lifecycle
    /// filter would cost a payload parse for a picture. No ordering is
    /// promised beyond determinism between identical calls.
    async fn sample(&self, limit: usize) -> Result<Vec<(String, Vec<f32>)>>;
    /// One point's dense vector, by artifact id. The moments stage compares
    /// an artifact's own vector to intent prototypes; reading it back is one
    /// point fetch, and re-embedding it would be a second call for a vector
    /// already paid for. `None` for a point that is not there.
    async fn dense_of(&self, artifact_id: &str) -> Result<Option<Vec<f32>>>;
    async fn count(&self) -> Result<u64>;
    /// Which generation of the store a `count` was taken from, where the
    /// backing store has such a thing.
    ///
    /// The background's cache tag carries this beside the count, because a
    /// count on its own cannot see a `--reindex` that re-embeds the same
    /// number of points into a fresh collection: the tag matched, the answer
    /// said `unchanged`, and the page went on drawing a cloud of vectors that
    /// no longer existed anywhere. `None` from a store with no notion of a
    /// generation, which leaves the tag as it was.
    async fn revision(&self) -> Result<Option<String>> {
        Ok(None)
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    // Vectors of different widths are not directions in the same space. Zipping
    // them would truncate the dot product to the shorter one while both norms
    // stayed full-length, which answers a plausible-looking number for what is
    // always a bug — a stored vector written under an older layout, or a slice
    // taken at the wrong offset. Fail closed: no similarity, so nothing ranks
    // on it and the caller's own guard, if it has one, still sees zero.
    if a.len() != b.len() {
        return 0.0;
    }
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

#[cfg(test)]
mod tests {
    use super::cosine;

    #[test]
    fn a_width_mismatch_scores_zero_rather_than_a_truncated_cosine() {
        // Zipping alone would take the dot product over the first two slots and
        // divide it by the full norm of both sides — a number in range, ordered
        // against real scores, and meaningless. The case that produces it is a
        // centroid stored under an older layout.
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
