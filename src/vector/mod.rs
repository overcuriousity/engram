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
    /// Set when this artifact lost a near-identical pair to a newer one. Kept
    /// for filter backward-compatibility now that `status` is the source of
    /// truth for the same thing — see `SearchFilter`. Like `last_seen_at`, it
    /// is omitted when unset so a writer which does not know the value — the
    /// embed job rebuilding a payload — leaves the stored one alone rather
    /// than reviving a hidden artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded: Option<bool>,
    /// Active, deprecated, or superseded. Mirrors the SQLite source of truth
    /// (`store::artifacts::Chunk::status`). Omitted when unset for the same
    /// merge-write reason as `last_seen_at`; absent is treated as active by
    /// every filter, so a point written before this field existed is not
    /// hidden until backfilled.
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
}

impl SearchFilter {
    /// Whether this filter narrows nothing. Excluding superseded/deprecated
    /// points is still a narrowing, so a filter that only does that is not
    /// empty — saying otherwise would drop the clause on the way to Qdrant.
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

/// What the search page offers to narrow by. Both lists arrive already sorted
/// by count, descending, because that is the order the chips are rendered in.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Facets {
    pub categories: Vec<FacetCount>,
    pub tags: Vec<FacetCount>,
}

/// Two artifacts the index says are close, and how close.
///
/// `a` sorts before `b` so the same pair found from either end is one value —
/// the sweep would otherwise queue it twice and supersede the loser twice.
#[derive(Debug, Clone, PartialEq)]
pub struct NearPair {
    pub a: String,
    pub b: String,
    pub score: f32,
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
/// treats a point written before lifecycle tracking existed.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredLifecycle {
    pub status: ArtifactStatus,
    pub superseded_by: Option<String>,
}

/// One artifact's SQLite-side lifecycle state, as the backfill pushes it into
/// the vector store. `last_verified_at` is never optional here: the backfill's
/// whole job is to give every point the stamp that ranking decays against, and
/// an artifact never explicitly verified falls back to its `created_at`.
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
    /// Replace a point's payload, leaving its vector alone. Editing tags or a
    /// category changes nothing the embedding model saw, so re-embedding for it
    /// would spend an inference call to arrive at the same vector.
    async fn set_payload(&self, payload: &VectorPayload) -> Result<()>;
    /// Hide or unhide one artifact. A payload write, not a re-embed: which
    /// artifact won a near-identical pair changes nothing the embedding model
    /// saw.
    async fn set_superseded(&self, artifact_id: &str, superseded: bool) -> Result<()>;
    /// Set an artifact's lifecycle status (and, for `Superseded`, the winner
    /// it was replaced by). A payload write, not a re-embed, for the same
    /// reason as `set_superseded` — also derives and writes the legacy
    /// `superseded: bool` flag so `build_filter`'s pre-backfill safety net
    /// keeps working.
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
    /// artifact passes `true`; the one-shot backfill passes `false`, since it
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
    /// means unknown, not stale: every point predates the backfill until it
    /// runs, and treating those as maximally stale would fill the review list
    /// with an arbitrary sample of the whole base and invite an operator to
    /// deprecate it. A missing `hit_count` is different — never retrieved is a
    /// fact the absent key states correctly — so it counts as zero.
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
    /// Record that these chunks were just shown. Merged into the stored
    /// payload, never written as a whole one. `last_seen_at` is stamped for
    /// every target; `hit_count` only for the ones marked `counts_as_hit`.
    async fn touch(&self, targets: &[Touch], seen_at: i64) -> Result<()>;
    /// Push a batch of artifacts' SQLite-side lifecycle state into the store,
    /// as one request rather than one per artifact per field. What the
    /// migration backfill runs on; also what the sweep's drift repair uses.
    async fn apply_lifecycle(&self, rows: &[LifecycleRow]) -> Result<()>;
    /// How many points carry no `last_verified_at`, i.e. still need the
    /// lifecycle backfill. Startup reads this to decide whether to run it,
    /// rather than making an operator remember a flag.
    ///
    /// Points with no `artifact_id` are excluded, and that exclusion is what
    /// makes "run the backfill once" true. The backfill stamps artifacts, so a
    /// point naming none can never be stamped by it — counting those meant the
    /// number never reached zero and every process start kicked off another
    /// full-base rewrite. Such a point can come from a hand-populated
    /// collection or a `--reindex` over one, it is invisible to search anyway
    /// (its payload will not parse as a chunk), and it is left alone rather than
    /// deleted: nothing here knows what it is.
    async fn unstamped_count(&self) -> Result<u64>;
    /// Artifact ids whose payload says deprecated or superseded, capped at
    /// `limit`. The sweep compares these against SQLite — the source of truth —
    /// to repair drift left by a half-applied lifecycle change in either
    /// direction.
    async fn non_active_ids(&self, limit: usize) -> Result<Vec<String>>;
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
    /// A point whose payload carries no `artifact_id` at all cannot appear here
    /// and is not counted by `unstamped_count` either — see that method. It is
    /// not an engram point and nothing can be said about it.
    async fn all_artifact_ids(&self) -> Result<Vec<String>>;
    /// The full stored payloads for these ids. Ids with no point are absent
    /// from the answer.
    ///
    /// This is what the heal restores an artifact row from, so unlike
    /// `lifecycle_of` it has to hand back everything the payload holds — the
    /// text, the title, the tags — not just the lifecycle fields.
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
    /// Pairs of artifacts closer than `min_score`, best first, over a sample of
    /// the collection. Anything not active is excluded — a resolved pair
    /// re-found every sweep is a review queue that never empties, and a
    /// deprecated artifact must never win a supersession and hide a live one.
    ///
    /// This is one round trip, not one query per point: `sample` points are
    /// drawn and each contributes at most `per_point` neighbours. A sweep over
    /// a base of any size therefore costs a bounded amount rather than growing
    /// with the collection.
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
    // A zero vector has no direction; call it maximally dissimilar rather than
    // dividing by zero and poisoning the ranking with NaN.
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}
