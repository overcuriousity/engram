//! Qdrant backend, spoken over its REST API.
//!
//! REST rather than gRPC because REST is the port operators actually expose —
//! typically behind a TLS reverse proxy on 443 — and because the feature set is
//! identical. `reqwest` was already a dependency, so this costs no new crates
//! and saves the whole `tonic`/`prost` tree.
//!
//! `vector.collection` names an *alias*, never a collection. The data lives in
//! `{alias}_v1`, `_v2`, … and the alias points at whichever generation is
//! current. Changing the embedding model or the index schema is then a
//! background rebuild followed by an atomic swap, rather than an outage.

use super::sparse::SparseVector;
use super::{
    FacetCount, Facets, LifecycleRow, SearchFilter, SearchHit, Touch, VectorPayload, VectorPoint,
    VectorStore,
};
use crate::config::VectorConfig;
use crate::error::{Error, Result};
use crate::store::artifacts::ArtifactStatus;
use async_trait::async_trait;
use reqwest::{Client, Method, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

/// Generous enough for a cold HNSW segment load, short enough that a wedged
/// server surfaces as a retryable job failure rather than a hung worker.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The named dense vector. Named rather than default because a collection
/// cannot gain named vectors later without being rebuilt, and the sparse half
/// of hybrid search needs the namespace.
pub const DENSE: &str = "dense";

/// The sparse (BM25) vector slot. Declared from the first generation onwards so
/// that turning hybrid search on is a code change, not a migration.
pub const SPARSE: &str = "text";

/// Points copied per scroll page during a rebuild.
const REINDEX_BATCH: usize = 256;

/// How long to wait for a collection another process just created to answer
/// for itself: five attempts backing off 200ms, 400ms, … — three seconds in
/// total, which is startup-shaped rather than request-shaped.
const READY_ATTEMPTS: u32 = 5;
const READY_BACKOFF: Duration = Duration::from_millis(200);

/// The context multivector: one element per learned situation, scored with
/// `max_sim`. Not the multivector the roadmap cut — that was ColBERT-style
/// late-interaction reranking, one reduced-width vector per *token* and
/// thousands per artifact. This is two to five per artifact.
pub const CTX: &str = "ctx";

/// A chunk carrying this tag is boosted past the decay curve. A tag rather than
/// a column: `PATCH /api/v1/artifacts/{id}` already edits tags without
/// re-embedding, and the payload index that makes it filterable already exists.
pub const PINNED_TAG: &str = "pinned";

const SECONDS_PER_DAY: u64 = 86_400;

pub struct QdrantVectors {
    http: Client,
    base: String,
    /// The alias clients read and write through, from `vector.collection`.
    alias: String,
    api_key: Option<String>,
    recency_weight: f32,
    recency_half_life_days: u32,
    pinned_boost: f32,
}

pub fn dimension_mismatch(collection: &str, configured: usize, existing: usize) -> Error {
    Error::Vector(format!(
        "collection `{collection}` has vector dimension {existing}, but config says {configured}. \
         Refusing to start: writing mismatched vectors would corrupt search results. \
         A rebuild cannot fix this — `--reindex` copies vectors and cannot change their width. \
         Either restore the embedding model that produced {existing}-wide vectors, or point \
         `vector.collection` at a new alias name and let every chunk embed again under the \
         new model."
    ))
}

/// Qdrant refuses an alias whose name collides with an existing collection, so
/// a plain collection sitting where the alias belongs blocks every read and
/// write. It is not ours and nothing here may delete it.
fn name_collision(alias: &str) -> Error {
    Error::Vector(format!(
        "`{alias}` is a plain collection, but engram addresses vectors through an alias of that \
         name, and Qdrant will not let the two share it. Rename or remove that collection, or \
         point `vector.collection` at a name nothing else is using."
    ))
}

/// Qdrant point ids must be an unsigned integer or a UUID. Chunk ids are
/// already UUIDv7 strings, so they pass through; anything else is hashed into
/// a deterministic UUID so the mapping stays stable across restarts.
pub fn point_uuid(artifact_id: &str) -> String {
    match uuid::Uuid::parse_str(artifact_id) {
        Ok(u) => u.to_string(),
        Err(_) => {
            let digest = <sha2::Sha256 as sha2::Digest>::digest(artifact_id.as_bytes());
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&digest[..16]);
            uuid::Uuid::from_bytes(bytes).to_string()
        }
    }
}

/// Strip trailing slashes so path joining never produces a double slash, which
/// some proxies answer with a redirect instead of the resource.
fn normalize_base(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

/// The physical collection backing generation `n` of an alias.
fn generation_name(alias: &str, n: u32) -> String {
    format!("{alias}_v{n}")
}

/// Read the generation number back out of a physical collection name, if it
/// carries one at all.
///
/// `None` is what separates `chunks_v2` from `chunks_verbose`: a prefix match
/// alone would claim any collection whose name happens to start the same way,
/// and this answer decides what `drop_collection` deletes and what
/// `ensure_collection` adopts.
fn generation_number(alias: &str, collection: &str) -> Option<u32> {
    collection
        .strip_prefix(&format!("{alias}_v"))
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse().ok())
}

/// The generation a rebuild would count from. A name carrying no number counts
/// as zero, so the next one lands on `_v1`.
fn generation_of(alias: &str, collection: &str) -> u32 {
    generation_number(alias, collection).unwrap_or(0)
}

/// Qdrant answers `{"must": [...]}` with an implicit AND. An empty condition
/// list would match nothing, so callers must skip the filter entirely instead.
fn build_filter(filter: &SearchFilter) -> Option<Value> {
    if filter.is_empty() {
        return None;
    }
    let mut must: Vec<Value> = Vec::new();
    for t in &filter.tags {
        must.push(json!({ "key": "tags", "match": { "value": t } }));
    }
    if let Some(c) = &filter.category {
        must.push(json!({ "key": "category", "match": { "value": c } }));
    }
    if let Some(c) = &filter.corpus_id {
        must.push(json!({ "key": "corpus_id", "match": { "value": c } }));
    }

    // `must` is omitted rather than sent empty. A search that only excludes
    // superseded points has no positive condition, and an empty condition list
    // is not something to hand Qdrant and hope it reads as "no constraint".
    let mut body = json!({});
    if !must.is_empty() {
        body["must"] = json!(must);
    }
    // Excluded with `must_not` rather than by matching a value: a point whose
    // payload carries no `status` key at all — a hand-written one — reads as
    // active, and a positive clause would drop every one of them from search.
    let mut must_not: Vec<Value> = Vec::new();
    if !filter.include_superseded {
        must_not.push(json!({ "key": "status", "match": { "value": "superseded" } }));
    }
    if !filter.include_deprecated {
        must_not.push(json!({ "key": "status", "match": { "value": "deprecated" } }));
    }
    if !must_not.is_empty() {
        body["must_not"] = json!(must_not);
    }
    Some(body)
}

/// The payload `set_lifecycle` merges into a point.
fn lifecycle_payload(status: ArtifactStatus, superseded_by: Option<&str>) -> Value {
    json!({
        "status": status.as_str(),
        "superseded_by": superseded_by,
    })
}

/// What a stored payload says its lifecycle status is, read the way every
/// filter reads it: an absent `status` means active, which is what a
/// hand-written point carries.
fn stored_status(payload: &Value) -> ArtifactStatus {
    match payload.get("status").and_then(Value::as_str) {
        Some(s) => ArtifactStatus::parse(s),
        None => ArtifactStatus::Active,
    }
}

/// The payload `set_last_verified_at` merges into a point.
///
/// `reset_hits` zeroes `hit_count` in the same write, because `stale_max_hits`
/// counts retrievals *since* the last verification — see `Core::verify`. A bulk
/// stamp leaves it alone: it touches every artifact and must not wipe every
/// counter.
fn verified_payload(at: i64, reset_hits: bool) -> Value {
    let mut p = json!({ "last_verified_at": at });
    if reset_hits {
        p["hit_count"] = json!(0);
    }
    p
}

/// The schema every generation is created with.
fn collection_body(dim: usize) -> Value {
    json!({
        "vectors": {
            DENSE: { "size": dim, "distance": "Cosine" },
            CTX: {
                "size": crate::core::context::CTX_DIM,
                "distance": "Cosine",
                "multivector_config": { "comparator": "max_sim" },
                // No index, an exact scan. The candidates are only artifacts
                // ever opened, at 53 dimensions, and an HNSW graph would be
                // rebuilt on every sweep write to beat a scan it cannot beat at
                // that size.
                "hnsw_config": { "m": 0 },
            },
        },
        "sparse_vectors": { SPARSE: { "modifier": "idf" } },
    })
}

/// The sparse half of a stored point, as Qdrant wants it. Skipped entirely when
/// the text held no indexable term, because a sparse vector with no dimensions
/// is not the same as no sparse vector.
fn sparse_body(sparse: &SparseVector) -> Option<Value> {
    if sparse.is_empty() {
        return None;
    }
    Some(json!({ "indices": sparse.indices, "values": sparse.values }))
}

/// Bring a payload written before the taxonomy rename up to the current key
/// names.
///
/// This is what makes `--reindex` the migration for it: the vectors are already
/// correct, and paying an embedding endpoint to change two key names would be
/// absurd. A payload that already uses the new names passes through untouched,
/// so running a rebuild twice is safe.
fn renamed_payload(payload: &Value) -> Value {
    let mut out = payload.clone();
    let Some(obj) = out.as_object_mut() else {
        return out;
    };
    for (old, new) in [("chunk_id", "artifact_id"), ("source_id", "corpus_id")] {
        if let Some(v) = obj.remove(old) {
            obj.entry(new).or_insert(v);
        }
    }
    out
}

/// Rebuild a point's terms from its payload. A rebuild has the text but not the
/// artifact row, and this must match what the embed job indexed: title then
/// body.
fn sparse_of_payload(payload: &Value) -> SparseVector {
    let text = payload.get("text").and_then(Value::as_str).unwrap_or("");
    match payload.get("title").and_then(Value::as_str) {
        Some(t) if !t.is_empty() => super::sparse::encode_document(&format!("{t}\n{text}")),
        _ => super::sparse::encode_document(text),
    }
}

/// The score adjustment applied on top of retrieval: newer wins ties, and a
/// pinned chunk wins outright.
///
/// `$score` here is the fused rank from the prefetch below it, which lands
/// between roughly 0.1 and 1.0. `exp_decay` returns 1.0 for something captured
/// now and 0.5 at one half-life old, so the recency term is a small nudge
/// rather than a second ranking.
///
/// `defaults` is not optional politeness: a single point whose payload lacks
/// `last_verified_at` fails the *whole* query with
/// `Expected number value for last_verified_at in the payload and/or in the
/// formula defaults`, not just its own scoring. Every point engram writes
/// carries the key, but `--reindex` copies payloads verbatim from whatever was
/// in the source collection, so one hand-written point would otherwise take
/// search down.
///
/// The default is `now`, not the epoch. A missing stamp means unknown, not
/// stale, and defaulting to the epoch would collapse the recency term to zero
/// for every point that lacks the key. Defaulting to `now` leaves the term
/// neutral (`exp_decay` returns 1.0).
///
/// Decays against `last_verified_at` rather than `created_at`, deliberately:
/// an artifact confirmed correct last week should outrank one merely written
/// last week and never looked at since. Nothing here reads `hit_count` — a
/// popularity term would let a frequently-shown result keep boosting itself
/// further, at the expense of a correct but rarely-queried one that never
/// gets the chance to accumulate hits.
fn scoring_formula(now: i64, half_life_secs: u64, recency: f32, pinned: f32) -> Value {
    let mut terms = vec![json!("$score")];
    if recency > 0.0 {
        terms.push(json!({
            "mult": [recency, {
                "exp_decay": { "x": "last_verified_at", "target": now, "scale": half_life_secs, "midpoint": 0.5 }
            }]
        }));
    }
    if pinned > 0.0 {
        // A filter condition evaluates to 1.0 for a point that matches it, and
        // needs no default: a point without tags simply does not match.
        terms.push(json!({
            "mult": [pinned, { "key": "tags", "match": { "value": PINNED_TAG } }]
        }));
    }
    json!({
        "formula": { "sum": terms },
        "defaults": { "last_verified_at": now },
    })
}

/// A stored point's dense vector, whether the collection uses named vectors or
/// is a pre-alias collection with a single default one.
fn dense_of(vector: &Value) -> Option<&Value> {
    match vector {
        Value::Array(_) => Some(vector),
        Value::Object(m) => m.get(DENSE),
        _ => None,
    }
}

/// A stored point's context set, when it is still readable under the running
/// encoder.
///
/// `None` covers three cases that a rebuild treats alike: no set at all, a
/// pre-alias point with a single unnamed vector, and a set written under an
/// older layout. The last is why the width is checked rather than trusted — a
/// changed `BLOCKS` changes `CTX_DIM`, and copying the old numbers into the new
/// space would give every artifact a profile assembled from the wrong blocks.
/// Discarded, and the next sweep rebuilds from the raw bundles.
fn ctx_of(vector: &Value) -> Option<Vec<Vec<f32>>> {
    let set = vector.as_object()?.get(CTX)?.as_array()?;
    if set.is_empty() {
        return None;
    }
    set.iter()
        .map(|e| {
            let row: Vec<f32> = e
                .as_array()?
                .iter()
                .map(|x| x.as_f64().map(|f| f as f32))
                .collect::<Option<_>>()?;
            (row.len() == crate::core::context::CTX_DIM).then_some(row)
        })
        .collect()
}

/// Every Qdrant response is wrapped in this. `result` is absent on failures,
/// which is why it is optional rather than required.
#[derive(Deserialize)]
struct Envelope<T> {
    result: Option<T>,
}

#[derive(Deserialize)]
struct Exists {
    exists: bool,
}

#[derive(Deserialize)]
struct CountResult {
    count: u64,
}

#[derive(Deserialize)]
struct QueryResult {
    #[serde(default)]
    points: Vec<ScoredPoint>,
}

#[derive(Deserialize)]
struct ScoredPoint {
    score: f32,
    #[serde(default)]
    payload: Value,
}

#[derive(Deserialize)]
struct FacetResult {
    #[serde(default)]
    hits: Vec<FacetHit>,
}

/// Qdrant reports a facet value as whatever type the field holds. Only keyword
/// fields are facetted here, so anything that is not a string is not a value
/// this collection can be filtered by and is dropped.
#[derive(Deserialize)]
struct FacetHit {
    value: Value,
    count: u64,
}

#[derive(Deserialize)]
struct AliasList {
    #[serde(default)]
    aliases: Vec<AliasEntry>,
}

#[derive(Deserialize)]
struct AliasEntry {
    alias_name: String,
    collection_name: String,
}

#[derive(Deserialize)]
struct CollectionList {
    #[serde(default)]
    collections: Vec<CollectionEntry>,
}

#[derive(Deserialize)]
struct CollectionEntry {
    name: String,
}

#[derive(Deserialize)]
struct ScrollResult {
    #[serde(default)]
    points: Vec<ScrolledPoint>,
    #[serde(default)]
    next_page_offset: Value,
}

/// The payload keys a point write must not clobber, as currently stored.
/// `None` means the key is absent.
#[derive(Debug, Clone, Default)]
struct StoredBookkeeping {
    last_seen_at: Option<i64>,
    hit_count: Option<i64>,
    status: Option<ArtifactStatus>,
    last_verified_at: Option<i64>,
    superseded_by: Option<String>,
}

#[derive(Deserialize)]
struct ScrolledPoint {
    id: Value,
    #[serde(default)]
    payload: Value,
    #[serde(default)]
    vector: Value,
}

impl QdrantVectors {
    pub async fn connect(cfg: &VectorConfig) -> Result<QdrantVectors> {
        // 6334 is the gRPC port. Pointing the REST client at it fails in a way
        // that reads like a network problem, so name the real cause up front.
        if cfg.url.trim_end_matches('/').ends_with(":6334") {
            tracing::warn!(
                url = %cfg.url,
                "vector.url points at 6334, Qdrant's gRPC port; the REST API is usually on 6333"
            );
        }

        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| Error::Vector(e.to_string()))?;

        Ok(QdrantVectors {
            http,
            base: normalize_base(&cfg.url),
            alias: cfg.collection.clone(),
            api_key: cfg.api_key.clone(),
            recency_weight: cfg.recency_weight,
            recency_half_life_days: cfg.recency_half_life_days.max(1),
            pinned_boost: cfg.pinned_boost,
        })
    }

    /// Drop the alias and every generation behind it. Used by the integration
    /// suite to start clean.
    pub async fn drop_collection(&self) -> Result<()> {
        if self.resolve_alias().await?.is_some() {
            let _: Result<Value> = self
                .call(
                    Method::POST,
                    "/collections/aliases",
                    Some(json!({
                        "actions": [ { "delete_alias": { "alias_name": self.alias } } ]
                    })),
                )
                .await;
        }
        for name in self.generations().await? {
            let _: Result<Value> = self
                .call(Method::DELETE, &format!("/collections/{name}"), None)
                .await;
        }
        Ok(())
    }

    /// The physical collection the alias currently points at, if the alias
    /// exists at all.
    pub async fn resolve_alias(&self) -> Result<Option<String>> {
        // Listing is `/aliases`; only the update action lives under
        // `/collections/aliases`, which otherwise reads as a collection name.
        let list: AliasList = self.call(Method::GET, "/aliases", None).await?;
        Ok(list
            .aliases
            .into_iter()
            .find(|a| a.alias_name == self.alias)
            .map(|a| a.collection_name))
    }

    /// Every collection belonging to this alias: the numbered generations, and
    /// nothing else.
    ///
    /// Membership is by parsed generation number, never by prefix and never by
    /// the alias name itself. A collection called `{alias}_vault` belongs to
    /// whoever made it, and so does one called `{alias}` — that one is the
    /// collision `name_collision` reports and refuses to touch. This list is
    /// what `drop_collection` deletes, so anything it claims wrongly is
    /// somebody else's data.
    async fn generations(&self) -> Result<Vec<String>> {
        let list: CollectionList = self.call(Method::GET, "/collections", None).await?;
        Ok(list
            .collections
            .into_iter()
            .map(|c| c.name)
            .filter(|n| generation_number(&self.alias, n).is_some())
            .collect())
    }

    /// The highest-numbered generation that exists.
    async fn newest_generation(&self) -> Result<Option<String>> {
        Ok(self
            .generations()
            .await?
            .into_iter()
            .filter_map(|n| generation_number(&self.alias, &n).map(|g| (g, n)))
            .max_by_key(|(g, _)| *g)
            .map(|(_, n)| n))
    }

    /// Whether the collection this instance reads and writes is actually there.
    ///
    /// Asked of the alias' target rather than of the alias, because a dangling
    /// alias — one left pointing at a generation that was dropped — is exactly
    /// the fault this has to report, and a check that resolved it away would
    /// call it healthy. No alias row at all is the same kind of fault: a
    /// collection named exactly like the alias is not ours and is not serving
    /// anything, it is the collision `ensure_collection` refuses to start on,
    /// so answering "live" there would turn every point lookup into a silent
    /// `None`.
    async fn collection_is_live(&self) -> Result<bool> {
        match self.resolve_alias().await? {
            Some(target) => self.collection_exists(&target).await,
            None => Ok(false),
        }
    }

    async fn collection_exists(&self, name: &str) -> Result<bool> {
        let e: Exists = self
            .call(Method::GET, &format!("/collections/{name}/exists"), None)
            .await?;
        Ok(e.exists)
    }

    async fn create_generation(&self, name: &str, dim: usize) -> Result<()> {
        if let Err(e) = self
            .call::<Value>(
                Method::PUT,
                &format!("/collections/{name}"),
                Some(collection_body(dim)),
            )
            .await
        {
            // Two processes starting together both find no alias and both try
            // to build the first generation. Losing that race is not a failure,
            // but inheriting a collection of the wrong width would be.
            if !self.collection_exists(name).await? {
                return Err(e);
            }
            let existing = self.await_readable(name).await?;
            if existing as usize != dim {
                return Err(dimension_mismatch(name, dim, existing as usize));
            }
            tracing::info!(collection = %name, "collection already existed; adopting it");
        }

        self.ensure_payload_indexes(name).await?;
        tracing::info!(collection = %name, dim, "created qdrant collection");
        Ok(())
    }

    /// Create every payload index this codebase filters on, tolerating ones
    /// that already exist (Qdrant treats an identical index as a no-op).
    ///
    /// Called on every path that adopts a collection, not only on the one that
    /// creates it. A field added to this list by a later release exists in no
    /// collection created before it — and `build_filter` starts emitting a
    /// clause on it immediately — so a deployment that only ever ran the
    /// creation path once would run every filtered search as a full scan until
    /// someone thought to `--reindex`.
    async fn ensure_payload_indexes(&self, name: &str) -> Result<()> {
        for (field, schema) in [
            ("tags", "keyword"),
            ("category", "keyword"),
            ("corpus_id", "keyword"),
            ("created_at", "integer"),
            ("last_seen_at", "integer"),
            ("status", "keyword"),
            ("last_verified_at", "integer"),
            ("hit_count", "integer"),
        ] {
            let _: Value = self
                .call(
                    Method::PUT,
                    &format!("/collections/{name}/index?wait=true"),
                    Some(json!({ "field_name": field, "field_schema": schema })),
                )
                .await
                .map_err(|e| Error::Vector(format!("payload index on {field}: {e}")))?;
        }
        Ok(())
    }

    /// Point a new alias name at whatever this one currently serves, and drop
    /// the old name.
    ///
    /// One `actions` batch, so there is no window in which the collection is
    /// reachable under neither name. The alias moves rather than the
    /// collections behind it: nothing re-embeds, and the generation history the
    /// reindex path walks is preserved exactly as it was.
    ///
    /// An alias that resolves to nothing is not an error. There is no data to
    /// carry across, which is the ordinary shape of adopting a base that was
    /// configured but never captured into.
    pub async fn rename_alias(&self, to: &str) -> Result<()> {
        let Some(target) = self.resolve_alias().await? else {
            return Ok(());
        };
        let _: Value = self
            .call(
                Method::POST,
                "/collections/aliases",
                Some(json!({ "actions": [
                    { "create_alias": { "collection_name": target, "alias_name": to } },
                    { "delete_alias": { "alias_name": self.alias } }
                ]})),
            )
            .await?;
        tracing::info!(from = %self.alias, to, collection = %target, "alias renamed");
        Ok(())
    }

    /// Point the alias at `collection`, replacing any previous target. Qdrant
    /// applies both actions in one transaction, so no request ever observes an
    /// alias that resolves to nothing.
    async fn point_alias_at(&self, collection: &str, replacing: bool) -> Result<()> {
        let mut actions = Vec::new();
        if replacing {
            actions.push(json!({ "delete_alias": { "alias_name": self.alias } }));
        }
        actions.push(json!({
            "create_alias": { "collection_name": collection, "alias_name": self.alias }
        }));

        let _: Value = self
            .call(
                Method::POST,
                "/collections/aliases",
                Some(json!({ "actions": actions })),
            )
            .await?;
        tracing::info!(alias = %self.alias, collection, "alias now points at this generation");
        Ok(())
    }

    /// Create the alias, tolerating a process that got there first.
    ///
    /// Startup reads the alias, finds none, and writes one. Two processes
    /// starting together both do that, and Qdrant refuses the second. Losing
    /// that race is the outcome we wanted; the only thing worth failing on is
    /// an alias pointing somewhere that cannot serve our vectors.
    async fn claim_alias(&self, collection: &str, dim: usize) -> Result<()> {
        let Err(e) = self.point_alias_at(collection, false).await else {
            return Ok(());
        };
        let Some(current) = self.resolve_alias().await? else {
            return Err(e);
        };
        let existing = self.await_readable(&current).await?;
        if existing as usize != dim {
            return Err(dimension_mismatch(&current, dim, existing as usize));
        }
        tracing::info!(
            alias = %self.alias,
            collection = %current,
            "another process created the alias first"
        );
        Ok(())
    }

    /// The dimension of a collection that may still be coming up.
    ///
    /// A collection another process created a moment ago answers
    /// `Service internal error: 0 of 0 read operations failed` until its shards
    /// are ready. That is a state to wait out, not a startup to abort — but
    /// only briefly, because the same message is what a genuinely broken
    /// collection returns forever.
    async fn await_readable(&self, collection: &str) -> Result<u64> {
        let mut last = None;
        for attempt in 0..READY_ATTEMPTS {
            match self.vector_dim(collection).await {
                Ok(dim) => return Ok(dim),
                Err(e) => {
                    tracing::debug!(collection, error = %e, "collection not readable yet");
                    last = Some(e);
                }
            }
            // No backoff after the last attempt: there is nothing left to wait
            // for, and the caller is holding up a startup.
            if attempt + 1 < READY_ATTEMPTS {
                tokio::time::sleep(READY_BACKOFF * (attempt + 1)).await;
            }
        }
        Err(last
            .unwrap_or_else(|| Error::Vector(format!("`{collection}` did not become readable"))))
    }

    async fn vector_dim(&self, collection: &str) -> Result<u64> {
        let info: Value = self
            .call(Method::GET, &format!("/collections/{collection}"), None)
            .await?;
        // Named vectors nest one level deeper than a pre-alias collection's
        // single default vector; accept either so a rebuild can read its source.
        info.pointer(&format!("/config/params/vectors/{DENSE}/size"))
            .or_else(|| info.pointer("/config/params/vectors/size"))
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::Vector("could not read collection vector dimension".into()))
    }

    /// `exact` because the collection-info counter is allowed to lag behind an
    /// accepted write, and both callers act on the number.
    async fn exact_count(&self, collection: &str) -> Result<u64> {
        let res: CountResult = self
            .call(
                Method::POST,
                &format!("/collections/{collection}/points/count"),
                Some(json!({ "exact": true })),
            )
            .await?;
        Ok(res.count)
    }

    /// The bookkeeping keys already stored for these chunks: when the chunk was
    /// last shown, how often, and its lifecycle status.
    ///
    /// Each is written by code that knows nothing about the others — `touch`
    /// sets the first two, the sweep or an operator action sets the rest, and
    /// the embed job rebuilding a payload knows none of them. Only asked for
    /// when a caller is about to overwrite payloads it built without them, and
    /// only for those keys, so this is a small read next to the write it
    /// protects.
    async fn stored_bookkeeping(
        &self,
        points: &[VectorPoint],
    ) -> Result<std::collections::HashMap<String, StoredBookkeeping>> {
        let wanted: Vec<&VectorPoint> = points
            .iter()
            .filter(|p| {
                p.payload.last_seen_at.is_none()
                    || p.payload.hit_count.is_none()
                    || p.payload.status.is_none()
                    || p.payload.last_verified_at.is_none()
                    || p.payload.superseded_by.is_none()
            })
            .collect();
        if wanted.is_empty() {
            return Ok(Default::default());
        }
        let by_uuid: std::collections::HashMap<String, &str> = wanted
            .iter()
            .map(|p| {
                (
                    point_uuid(&p.payload.artifact_id),
                    p.payload.artifact_id.as_str(),
                )
            })
            .collect();
        let ids: Vec<&String> = by_uuid.keys().collect();
        let found: Vec<ScrolledPoint> = self
            .call(
                Method::POST,
                &format!("/collections/{}/points", self.alias),
                Some(json!({
                    "ids": ids,
                    "with_payload": [
                        "last_seen_at", "hit_count",
                        "status", "last_verified_at", "superseded_by",
                    ],
                    "with_vector": false,
                })),
            )
            .await?;

        let mut out = std::collections::HashMap::new();
        for p in found {
            let Some(uuid) = p.id.as_str() else { continue };
            let Some(artifact_id) = by_uuid.get(uuid) else {
                continue;
            };
            out.insert(
                (*artifact_id).to_string(),
                StoredBookkeeping {
                    last_seen_at: p.payload.get("last_seen_at").and_then(Value::as_i64),
                    hit_count: p.payload.get("hit_count").and_then(Value::as_i64),
                    status: p
                        .payload
                        .get("status")
                        .and_then(Value::as_str)
                        .map(ArtifactStatus::parse),
                    last_verified_at: p.payload.get("last_verified_at").and_then(Value::as_i64),
                    superseded_by: p
                        .payload
                        .get("superseded_by")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                },
            );
        }
        Ok(out)
    }

    /// Counts for one keyword field, straight from its payload index. Approximate
    /// by default in Qdrant, which is the right trade for a row of chips: an
    /// exact count would scan the collection to change a number nobody reads as
    /// a total.
    async fn facet(&self, key: &str, limit: usize) -> Result<Vec<FacetCount>> {
        let res: FacetResult = self
            .call(
                Method::POST,
                &format!("/collections/{}/facet", self.alias),
                Some(json!({ "key": key, "limit": limit })),
            )
            .await
            .map_err(|e| Error::Vector(format!("facet on {key}: {e}")))?;
        Ok(facet_counts(res))
    }

    async fn put_points(&self, collection: &str, points: Vec<Value>) -> Result<()> {
        let _: Value = self
            .call(
                Method::PUT,
                &format!("/collections/{collection}/points?wait=true"),
                Some(json!({ "points": points })),
            )
            .await?;
        Ok(())
    }

    /// Copy every point into a fresh generation and swap the alias onto it.
    ///
    /// Dense vectors are copied as they are, so a rebuild costs no embedding
    /// calls. The previous generation is left in place: it is the only rollback
    /// that exists, and deleting it is a decision for whoever ran this.
    pub async fn reindex(&self, dim: usize) -> Result<String> {
        let Some(source) = self.resolve_alias().await? else {
            return Err(Error::Vector(format!(
                "nothing to reindex: no alias named `{}` exists",
                self.alias
            )));
        };
        let target = generation_name(&self.alias, generation_of(&self.alias, &source) + 1);

        if self.collection_exists(&target).await? {
            return Err(Error::Vector(format!(
                "`{target}` already exists; a previous rebuild did not finish. \
                 Delete it and run this again."
            )));
        }

        let source_dim = self.vector_dim(&source).await?;
        if source_dim as usize != dim {
            return Err(dimension_mismatch(&source, dim, source_dim as usize));
        }

        self.create_generation(&target, dim).await?;

        let mut offset = Value::Null;
        let mut copied = 0usize;
        loop {
            let mut body = json!({
                "limit": REINDEX_BATCH,
                "with_payload": true,
                "with_vector": true,
            });
            if !offset.is_null() {
                body["offset"] = offset.clone();
            }

            let page: ScrollResult = self
                .call(
                    Method::POST,
                    &format!("/collections/{source}/points/scroll"),
                    Some(body),
                )
                .await?;

            if page.points.is_empty() {
                break;
            }

            let mut batch = Vec::with_capacity(page.points.len());
            for p in &page.points {
                let dense = dense_of(&p.vector).ok_or_else(|| {
                    Error::Vector(format!("point {} in `{source}` has no dense vector", p.id))
                })?;
                // Dense vectors are copied; sparse ones are recomputed, which
                // is free and lets a rebuild add the lexical half to a
                // generation written before it existed.
                let mut vector = json!({ DENSE: dense });
                if let Some(sp) = sparse_body(&sparse_of_payload(&p.payload)) {
                    vector[SPARSE] = sp;
                }
                // Copied when the dimension matches, skipped when it does not.
                // No embedding call in either case — a context vector is
                // assembled from a bundle, and the bundles are all still in
                // `context_events`.
                if let Some(set) = ctx_of(&p.vector) {
                    vector[CTX] = json!(set);
                }
                batch.push(json!({
                    "id": p.id,
                    "vector": vector,
                    "payload": renamed_payload(&p.payload),
                }));
            }
            copied += batch.len();
            self.put_points(&target, batch).await?;
            tracing::info!(copied, source = %source, target = %target, "reindex progress");

            offset = page.next_page_offset;
            if offset.is_null() {
                break;
            }
        }

        self.point_alias_at(&target, true).await?;
        tracing::info!(
            copied,
            previous = %source,
            current = %target,
            "reindex complete; the previous generation was left in place"
        );

        Ok(target)
    }
}

/// Turn a query response into hits, skipping any point whose payload is not
/// one of ours.
///
/// A collection can hold points engram did not write — a rebuild copies
/// payloads verbatim, and nothing stops an operator from inserting their own.
/// Failing the whole result set over one of them would mean a single foreign
/// point takes search down, which is a worse answer than a shorter list and a
/// log line naming what was skipped.
fn hits_of(res: QueryResult) -> Vec<SearchHit> {
    let mut out = Vec::with_capacity(res.points.len());
    for p in res.points {
        match serde_json::from_value::<VectorPayload>(p.payload) {
            Ok(payload) => out.push(SearchHit {
                payload,
                score: p.score,
                // Filled in by `search` from the batched dense lookup; every
                // other caller of this helper is a listing with no query.
                similarity: None,
            }),
            Err(e) => tracing::warn!(
                error = %e,
                "skipping a point whose payload is not an engram chunk"
            ),
        }
    }
    out
}

/// Facet hits as chips, most frequent first. Qdrant already sorts by count, but
/// the order is restated here so ties do not depend on it, and non-string values
/// are dropped: only a keyword field can be filtered by what the chip carries.
fn facet_counts(res: FacetResult) -> Vec<FacetCount> {
    let mut out: Vec<FacetCount> = res
        .hits
        .into_iter()
        .filter_map(|h| {
            h.value.as_str().map(|v| FacetCount {
                value: v.to_string(),
                count: h.count,
            })
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value)));
    out
}

/// Seconds since the epoch, the unit `created_at` is stored in.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Qdrant reports failures as `{"status": {"error": "..."}}`. Fall back to the
/// raw body when it does not, but never dump an unbounded response into a log
/// line or an error message.
fn describe_failure(status: StatusCode, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("status")
                .and_then(|s| s.get("error"))
                .and_then(|e| e.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(200).collect());
    format!("qdrant returned {status}: {detail}")
}

impl QdrantVectors {
    /// The single place a Qdrant response turns into either a value or an
    /// `Error::Vector`, so no call site has to reason about the envelope.
    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T> {
        let (status, text) = self.send(method, path, body).await?;
        if !status.is_success() {
            // The path is part of the message because several requests can fail
            // the same way, and "which one" is the first thing anyone reading
            // the log needs.
            return Err(Error::Vector(format!(
                "{path}: {}",
                describe_failure(status, &text)
            )));
        }
        decode(path, &text)
    }

    /// `call`, for a request naming one point that may legitimately not be in
    /// the collection yet — an artifact captured a moment ago, whose embedding
    /// job has not run. Absent is `None`; everything else is still an error.
    ///
    /// The distinction cannot be made on the status alone, which is the whole
    /// reason this is not two lines at the call site: Qdrant answers 404 both
    /// for a point it does not hold *and* for a collection that does not exist.
    /// The second is a store that is broken or misconfigured — an alias
    /// pointing at a dropped generation, say — and reading it as "this artifact
    /// has no neighbours" would turn a fault affecting every artifact into a
    /// silent, permanent answer for each of them.
    ///
    /// So a 404 whose message does not name the missing point is settled by
    /// asking whether the collection is there, rather than by reading further
    /// prose. The message check stays as the fast path because it is right
    /// today and costs nothing; what it may not be is the *only* thing standing
    /// between an unembedded artifact and a permanent error, because it is a
    /// substring of one Qdrant version's wording. Read that way round, a
    /// reworded message costs one extra request; read the old way round, it
    /// turned every artifact awaiting its embedding into a `Relate` unit that
    /// failed and retried at the backoff ceiling forever.
    async fn call_absent_point_as_none<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Option<T>> {
        let (status, text) = self.send(method, path, body).await?;
        if status == reqwest::StatusCode::NOT_FOUND
            && (text.contains("No point with id") || self.collection_is_live().await?)
        {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(Error::Vector(format!(
                "{path}: {}",
                describe_failure(status, &text)
            )));
        }
        decode(path, &text).map(Some)
    }

    /// The round trip both of the above are built on: everything up to, but not
    /// including, the decision about what a non-success status means.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(reqwest::StatusCode, String)> {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(k) = &self.api_key {
            req = req.header("api-key", k);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }

        let res = req.send().await.map_err(|e| Error::Vector(e.to_string()))?;
        let status = res.status();
        let text = res.text().await.map_err(|e| Error::Vector(e.to_string()))?;
        Ok((status, text))
    }
}

/// Unwrap Qdrant's envelope, which every successful response carries.
fn decode<T: DeserializeOwned>(path: &str, text: &str) -> Result<T> {
    let env: Envelope<T> = serde_json::from_str(text)
        .map_err(|e| Error::Vector(format!("unreadable response from {path}: {e}")))?;
    env.result
        .ok_or_else(|| Error::Vector(format!("{path} returned no result")))
}

#[async_trait]
impl VectorStore for QdrantVectors {
    async fn ensure_collection(&self, dim: usize) -> Result<()> {
        if let Some(current) = self.resolve_alias().await? {
            let existing = self.vector_dim(&current).await?;
            if existing as usize != dim {
                return Err(dimension_mismatch(&current, dim, existing as usize));
            }
            // An already-serving collection predates any index this release
            // added, so this is the path that matters most.
            self.ensure_payload_indexes(&current).await?;
            return Ok(());
        }

        // A collection sitting where the alias should be holds real vectors.
        // Refusing here is what keeps `--reindex` from being a data-loss step.
        if self.collection_exists(&self.alias).await? {
            return Err(name_collision(&self.alias));
        }

        // Generations with no alias mean a rebuild died between deleting the
        // old collection and creating the alias. The vectors are all there;
        // adopting the newest generation is repair, not guesswork.
        if let Some(orphan) = self.newest_generation().await? {
            let existing = self.vector_dim(&orphan).await?;
            if existing as usize != dim {
                return Err(dimension_mismatch(&orphan, dim, existing as usize));
            }
            tracing::warn!(
                collection = %orphan,
                alias = %self.alias,
                "found generations with no alias; adopting the newest, \
                 which means an earlier rebuild did not finish"
            );
            self.claim_alias(&orphan, dim).await?;
            self.ensure_payload_indexes(&orphan).await?;
            return Ok(());
        }

        let first = generation_name(&self.alias, 1);
        self.create_generation(&first, dim).await?;
        self.claim_alias(&first, dim).await?;
        Ok(())
    }

    async fn upsert(&self, points: Vec<VectorPoint>) -> Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        // A point write replaces the whole payload, unlike `set_payload` and
        // `touch`, which merge. A writer that does not know when the chunk was
        // last shown would therefore clear the stamp, and `resurface` would
        // offer a chunk read yesterday as forgotten. The same hazard applies to
        // `status`, and costs more: clearing it puts an artifact
        // the sweep hid straight back into results, on every re-embed. So
        // carry every bookkeeping key forward for every point that arrives
        // without it.
        let stored = self.stored_bookkeeping(&points).await?;
        let mut body = Vec::with_capacity(points.len());
        for p in points {
            let mut payload = p.payload.clone();
            let old = stored.get(&payload.artifact_id);
            if payload.last_seen_at.is_none() {
                payload.last_seen_at = old.and_then(|s| s.last_seen_at);
            }
            if payload.hit_count.is_none() {
                payload.hit_count = old.and_then(|s| s.hit_count);
            }
            if payload.status.is_none() {
                payload.status = old.and_then(|s| s.status);
            }
            if payload.last_verified_at.is_none() {
                payload.last_verified_at = old.and_then(|s| s.last_verified_at);
            }
            if payload.superseded_by.is_none() {
                payload.superseded_by = old.and_then(|s| s.superseded_by.clone());
            }
            let payload =
                serde_json::to_value(&payload).map_err(|e| Error::Vector(e.to_string()))?;
            let mut vector = json!({ DENSE: p.vector });
            if let Some(sp) = sparse_body(&p.sparse) {
                vector[SPARSE] = sp;
            }
            body.push(json!({
                "id": point_uuid(&p.payload.artifact_id),
                "vector": vector,
                "payload": payload,
            }));
        }
        self.put_points(&self.alias, body).await
    }

    async fn set_payload(&self, payload: &VectorPayload) -> Result<()> {
        let body = serde_json::to_value(payload).map_err(|e| Error::Vector(e.to_string()))?;
        let _: Value = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/payload?wait=true", self.alias),
                Some(json!({
                    "payload": body,
                    "points": [ point_uuid(&payload.artifact_id) ],
                })),
            )
            .await?;
        Ok(())
    }

    async fn set_lifecycle(
        &self,
        artifact_id: &str,
        status: ArtifactStatus,
        superseded_by: Option<&str>,
    ) -> Result<()> {
        // Absent is not a failure here. An artifact can legitimately have no
        // point — its embedding never ran, or it failed — and a lifecycle change
        // on one has nothing to write to rather than something to complain
        // about. `embed::upsert_with_current_lifecycle` reads the row when it
        // finally writes a point, so the status lands with the vector if one is
        // ever created, and until then there is nothing to drift from.
        //
        // Reading the 404 as an error broke `merge::reap_stranded` completely:
        // it retires a merge *because that merge could not be indexed*, so the
        // point never existed, so the deprecation errored on every call. The row
        // went to `deprecated` and the rest of the reap — reopening the pairs
        // merged into it, dropping the embed job that could never succeed —
        // never ran, while the log reported the reap as failed. It also handed
        // the same 404 to `consolidate::repair_lifecycle_drift`, which writes
        // its batch in one call and would fail every other artifact's repair
        // alongside it.
        let _: Option<Value> = self
            .call_absent_point_as_none(
                Method::POST,
                &format!("/collections/{}/points/payload?wait=true", self.alias),
                Some(json!({
                    "payload": lifecycle_payload(status, superseded_by),
                    "points": [ point_uuid(artifact_id) ],
                })),
            )
            .await?;
        Ok(())
    }

    async fn set_last_verified_at(
        &self,
        artifact_id: &str,
        at: i64,
        reset_hits: bool,
    ) -> Result<()> {
        let _: Value = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/payload?wait=true", self.alias),
                Some(json!({
                    "payload": verified_payload(at, reset_hits),
                    "points": [ point_uuid(artifact_id) ],
                })),
            )
            .await?;
        Ok(())
    }

    async fn apply_lifecycle(&self, rows: &[LifecycleRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        // One request per batch, not two writes per artifact. This pass can run
        // over every artifact in the base, and at two round trips each — both
        // with `wait=true` — that is slow enough to matter.
        //
        // `last_verified_at` is merged without touching `hit_count`: this pass
        // stamps artifacts it knows nothing about, and zeroing every retrieval
        // counter in the base is not part of its job. See `Core::verify` for
        // the case that does reset it.
        //
        // Capped per request for the same reason `lifecycle_of` caps its
        // retrieves: one operation per artifact means an unbounded caller
        // produces an unbounded request body, and Qdrant may well refuse it.
        // The caller that most needs this pass is the one with the most to
        // write — a drift repair over a base that drifted badly — so the
        // largest request is exactly the one that must not be the one that
        // fails.
        const BATCH: usize = 512;
        for group in rows.chunks(BATCH) {
            let ops: Vec<Value> = group
                .iter()
                .map(|r| {
                    let mut payload = lifecycle_payload(r.status, r.superseded_by.as_deref());
                    payload["last_verified_at"] = json!(r.last_verified_at);
                    json!({
                        "set_payload": {
                            "payload": payload,
                            "points": [ point_uuid(&r.artifact_id) ],
                        }
                    })
                })
                .collect();
            let _: Value = self
                .call(
                    Method::POST,
                    &format!("/collections/{}/points/batch?wait=true", self.alias),
                    Some(json!({ "operations": ops })),
                )
                .await?;
        }
        Ok(())
    }

    async fn payloads_of(
        &self,
        artifact_ids: &[String],
    ) -> Result<std::collections::HashMap<String, VectorPayload>> {
        let mut out = std::collections::HashMap::new();
        if artifact_ids.is_empty() {
            return Ok(out);
        }
        // Batched for the same reason as `lifecycle_of` below, and with more
        // reason: these retrieves carry the full payload, text included.
        const BATCH: usize = 256;
        for batch in artifact_ids.chunks(BATCH) {
            let ids: Vec<String> = batch.iter().map(|id| point_uuid(id)).collect();
            let found: Vec<ScrolledPoint> = self
                .call(
                    Method::POST,
                    &format!("/collections/{}/points", self.alias),
                    Some(json!({
                        "ids": ids,
                        "with_payload": true,
                        "with_vector": false,
                    })),
                )
                .await?;
            for p in found {
                match serde_json::from_value::<VectorPayload>(p.payload) {
                    Ok(payload) => {
                        out.insert(payload.artifact_id.clone(), payload);
                    }
                    Err(e) => tracing::warn!(
                        error = %e,
                        "skipping a point whose payload is not an engram chunk"
                    ),
                }
            }
        }
        Ok(out)
    }

    async fn lifecycle_of(
        &self,
        artifact_ids: &[String],
    ) -> Result<std::collections::HashMap<String, super::StoredLifecycle>> {
        let mut out = std::collections::HashMap::new();
        if artifact_ids.is_empty() {
            return Ok(out);
        }
        // Batched, because the drift repair asks about every hidden artifact it
        // found and a single retrieve of thousands of ids is a request body
        // Qdrant may well refuse.
        const BATCH: usize = 512;
        for batch in artifact_ids.chunks(BATCH) {
            let ids: Vec<String> = batch.iter().map(|id| point_uuid(id)).collect();
            let found: Vec<ScrolledPoint> = self
                .call(
                    Method::POST,
                    &format!("/collections/{}/points", self.alias),
                    Some(json!({
                        "ids": ids,
                        // `point_uuid` is one-way, so the artifact id has to
                        // come back from the payload.
                        "with_payload": ["artifact_id", "status", "superseded", "superseded_by"],
                        "with_vector": false,
                    })),
                )
                .await?;
            for p in found {
                let Some(id) = p.payload.get("artifact_id").and_then(Value::as_str) else {
                    continue;
                };
                out.insert(
                    id.to_string(),
                    super::StoredLifecycle {
                        status: stored_status(&p.payload),
                        superseded_by: p
                            .payload
                            .get("superseded_by")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    },
                );
            }
        }
        Ok(out)
    }

    async fn all_artifact_ids(&self) -> Result<Vec<String>> {
        const PAGE: usize = 1_000;
        let mut out = Vec::new();
        let mut offset = Value::Null;
        loop {
            let mut body = json!({
                "limit": PAGE,
                "with_payload": ["artifact_id"],
                "with_vector": false,
            });
            if !offset.is_null() {
                body["offset"] = offset.clone();
            }
            let page: ScrollResult = self
                .call(
                    Method::POST,
                    &format!("/collections/{}/points/scroll", self.alias),
                    Some(body),
                )
                .await?;
            if page.points.is_empty() {
                break;
            }
            out.extend(page.points.iter().filter_map(|p| {
                p.payload
                    .get("artifact_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }));
            offset = page.next_page_offset;
            if offset.is_null() {
                break;
            }
        }
        Ok(out)
    }

    /// Scrolled and sorted rather than sampled, because this list is a work
    /// queue.
    ///
    /// A random draw meant every render of Ops produced a different set: acting
    /// on a candidate redirects back to the page, which re-drew, so an operator
    /// could not work the queue down and a given artifact might take many page
    /// loads to reappear. A scroll is deterministic (Qdrant returns points in id
    /// order), so the same base with the same threshold yields the same queue,
    /// and answering a candidate is what removes it — verifying restamps it out
    /// of the range, deprecating filters it out.
    ///
    /// The scroll is capped: the filtered set is "everything stale enough", with
    /// no upper bound on a neglected base, and this runs on a page render. The
    /// cap costs nothing an operator can perceive, since the whole point is to
    /// hand back `limit` rows and `limit` is small — it only means that on a
    /// base with more than `STALE_SCAN` stale artifacts, the queue is drawn from
    /// the first window in point-id order rather than the true global worst.
    async fn stale_candidates(
        &self,
        older_than: i64,
        max_hits: i64,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        /// How many matching points the queue is drawn from.
        const STALE_SCAN: usize = 10_000;
        const PAGE: usize = 1_000;

        let filter = json!({
            "must_not": [
                { "key": "status", "match": { "value": "deprecated" } },
                { "key": "status", "match": { "value": "superseded" } },
            ],
            "must": [
                // Present *and* old. A point with no stamp is not a stale
                // candidate, it is one nothing knows about — see the trait doc.
                // `hit_count` below is the opposite case: absent legitimately
                // means never retrieved.
                { "key": "last_verified_at", "range": { "lt": older_than } },
                { "should": [
                    { "key": "hit_count", "range": { "lte": max_hits } },
                    { "is_empty": { "key": "hit_count" } },
                ] },
            ],
        });

        let mut found: Vec<VectorPayload> = Vec::new();
        let mut offset = Value::Null;
        while found.len() < STALE_SCAN {
            let mut body = json!({
                "filter": filter,
                "limit": PAGE.min(STALE_SCAN - found.len()),
                "with_payload": true,
                "with_vector": false,
            });
            if !offset.is_null() {
                body["offset"] = offset.clone();
            }
            let page: ScrollResult = self
                .call(
                    Method::POST,
                    &format!("/collections/{}/points/scroll", self.alias),
                    Some(body),
                )
                .await?;
            if page.points.is_empty() {
                break;
            }
            for p in page.points {
                match serde_json::from_value::<VectorPayload>(p.payload) {
                    Ok(payload) => found.push(payload),
                    Err(e) => tracing::warn!(
                        error = %e,
                        "skipping a point whose payload is not an engram chunk"
                    ),
                }
            }
            offset = page.next_page_offset;
            if offset.is_null() {
                break;
            }
        }

        // Stalest first, so the queue leads with the artifacts most worth an
        // operator's attention rather than with whichever ids sort lowest. The
        // id tiebreak keeps the order total: two artifacts stamped in the same
        // second must not swap places between one render and the next.
        found.sort_by(|a, b| {
            a.last_verified_at
                .cmp(&b.last_verified_at)
                .then_with(|| a.artifact_id.cmp(&b.artifact_id))
        });
        found.truncate(limit);
        Ok(found
            .into_iter()
            .map(|payload| SearchHit {
                payload,
                // No query to be similar to. The caller ranks this list by
                // staleness, which the order above already carries.
                score: 0.0,
                similarity: None,
            })
            .collect())
    }

    /// Under the weight this store was connected with, which is the configured
    /// one until a tuning sweep applies another.
    async fn search(
        &self,
        vector: &[f32],
        sparse: &SparseVector,
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>> {
        self.search_weighted(vector, sparse, limit, filter, self.recency_weight)
            .await
    }

    async fn search_weighted(
        &self,
        vector: &[f32],
        sparse: &SparseVector,
        limit: usize,
        filter: &SearchFilter,
        recency_weight: f32,
    ) -> Result<Vec<SearchHit>> {
        let f = build_filter(filter);

        let mut body = match sparse_body(sparse) {
            // Hybrid: both halves run as prefetch branches and Qdrant fuses
            // their ranks. Reciprocal rank fusion needs no score calibration
            // between a cosine similarity and a BM25 weight, which is exactly
            // why it is the right combiner here.
            Some(terms) => {
                let mut dense_branch = json!({ "query": vector, "using": DENSE, "limit": limit });
                let mut sparse_branch = json!({ "query": terms, "using": SPARSE, "limit": limit });
                // The filter has to be repeated per branch: it narrows what
                // each half retrieves, not what the fusion returns.
                if let Some(f) = &f {
                    dense_branch["filter"] = f.clone();
                    sparse_branch["filter"] = f.clone();
                }
                json!({
                    "prefetch": [dense_branch, sparse_branch],
                    "query": { "fusion": "rrf" },
                    "limit": limit,
                    "with_payload": true,
                })
            }
            // No indexable term in the query, so there is nothing for the
            // lexical branch to match and asking it to would only cost a round
            // trip through an empty index.
            None => json!({
                "query": vector,
                "using": DENSE,
                "limit": limit,
                "with_payload": true,
            }),
        };
        if let Some(f) = &f
            && body.get("prefetch").is_none()
        {
            body["filter"] = f.clone();
        }

        // Recency and pinning are applied as a final scoring stage over
        // whatever retrieval returned, so they reorder results without
        // changing which ones were retrieved.
        if recency_weight > 0.0 || self.pinned_boost > 0.0 {
            let mut prefetch = std::mem::replace(&mut body, Value::Null);
            // The payload is fetched once, by the outer stage. Asking the
            // prefetch for it too would carry every candidate's full text
            // through a stage that only reorders ids.
            if let Some(m) = prefetch.as_object_mut() {
                m.remove("with_payload");
            }
            body = json!({
                "prefetch": [prefetch],
                "query": scoring_formula(
                    now_secs(),
                    self.recency_half_life_days as u64 * SECONDS_PER_DAY,
                    recency_weight,
                    self.pinned_boost,
                ),
                "limit": limit,
                "with_payload": true,
            });
        }

        // The second half of the request: the same dense vector, on its own,
        // with no fusion and no recency stage over it. That returns raw cosine
        // similarity, which is the only number in this whole path that means
        // the same thing from one query to the next — the ranking score above
        // is a fused rank, so the top hit for a typo scores like the top hit for
        // a perfect match.
        //
        // Batched rather than sent after, so the confidence costs a round trip
        // of nothing. `limit` matches the ranking query's, so every hit the
        // dense branch contributed is covered; a hit only the lexical branch
        // found is absent from this set and gets no similarity, which is
        // correct — it contains the query's terms verbatim.
        let mut confidence = json!({
            "query": vector,
            "using": DENSE,
            "limit": limit,
            "with_payload": ["artifact_id"],
        });
        if let Some(f) = &f {
            confidence["filter"] = f.clone();
        }

        let mut res: Vec<QueryResult> = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/query/batch", self.alias),
                Some(json!({ "searches": [body, confidence] })),
            )
            .await?;
        if res.len() != 2 {
            return Err(Error::Vector(format!(
                "asked qdrant for 2 batched searches and got {}",
                res.len()
            )));
        }
        let similarities: HashMap<String, f32> = res
            .pop()
            .expect("length checked")
            .points
            .into_iter()
            .filter_map(|p| {
                let id = p.payload.get("artifact_id")?.as_str()?.to_string();
                Some((id, p.score))
            })
            .collect();

        let mut hits = hits_of(res.pop().expect("length checked"));
        for h in &mut hits {
            h.similarity = similarities.get(&h.payload.artifact_id).copied();
        }
        Ok(hits)
    }

    async fn touch(&self, targets: &[Touch], seen_at: i64) -> Result<()> {
        if targets.is_empty() {
            return Ok(());
        }
        // `last_seen_at` is the same value for the whole batch. `hit_count` is
        // not — each point's next value depends on its own current one, and
        // Qdrant has no atomic increment — so the current value has to come
        // from somewhere. A marked search has just read every hit's payload and
        // passes the counts in, which is the common case and costs no round
        // trip; a caller that counts a hit while knowing nothing but an id pays
        // for a read, and only for its own points.
        //
        // Both stay off the request path (the caller backgrounds this call),
        // and this still waits: the shutdown drain exists to keep these stamps,
        // and an acknowledged-but-unapplied write is exactly the loss it is
        // meant to prevent. A count missed under a rare concurrent double-touch
        // is an acceptable soft-counter race — it only ever feeds the one-way
        // stale-candidate query, never live scoring.
        //
        // A target that does not count as a hit needs no count at all: it only
        // stamps `last_seen_at`, so it neither joins the read below nor carries
        // a `hit_count` key into the write.
        let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        let mut ids: Vec<(String, bool)> = Vec::with_capacity(targets.len());
        let mut unknown: Vec<String> = Vec::new();
        for t in targets {
            let uuid = point_uuid(&t.artifact_id);
            if t.counts_as_hit {
                match t.hit_count {
                    Some(n) => {
                        counts.insert(uuid.clone(), n);
                    }
                    None => unknown.push(uuid.clone()),
                }
            }
            ids.push((uuid, t.counts_as_hit));
        }
        if !unknown.is_empty() {
            let found: Vec<ScrolledPoint> = self
                .call(
                    Method::POST,
                    &format!("/collections/{}/points", self.alias),
                    Some(
                        json!({ "ids": unknown, "with_payload": ["hit_count"], "with_vector": false }),
                    ),
                )
                .await?;
            for p in found {
                if let Some(uuid) = p.id.as_str() {
                    counts.insert(
                        uuid.to_string(),
                        p.payload
                            .get("hit_count")
                            .and_then(Value::as_i64)
                            .unwrap_or(0),
                    );
                }
            }
        }
        let ops: Vec<Value> = ids
            .iter()
            .map(|(uuid, counts_as_hit)| {
                let mut payload = json!({ "last_seen_at": seen_at });
                if *counts_as_hit {
                    payload["hit_count"] = json!(counts.get(uuid).copied().unwrap_or(0) + 1);
                }
                json!({
                    "set_payload": {
                        "payload": payload,
                        "points": [uuid],
                    }
                })
            })
            .collect();
        let _: Value = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/batch?wait=true", self.alias),
                Some(json!({ "operations": ops })),
            )
            .await?;
        Ok(())
    }

    async fn resurface(
        &self,
        limit: usize,
        older_than: i64,
        unseen_since: i64,
    ) -> Result<Vec<SearchHit>> {
        let res: QueryResult = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/query", self.alias),
                Some(json!({
                    "query": { "sample": "random" },
                    // `must_not` for the same reason as in `build_filter`: a
                    // point carrying no `status` key reads as active, and
                    // matching a value would drop it. Without this the
                    // forgotten list offers exactly the duplicates the sweep
                    // just took out of search — and, since a deprecated
                    // artifact is old and by definition unseen, exactly the
                    // artifacts an operator has just retired.
                    "filter": {
                      "must_not": [
                        { "key": "status", "match": { "value": "superseded" } },
                        { "key": "status", "match": { "value": "deprecated" } }
                      ],
                      "must": [
                        { "key": "created_at", "range": { "lt": older_than } },
                        // Nested so this reads as AND-of-OR. A chunk written
                        // before the stamp existed has no `last_seen_at` at
                        // all, and has certainly not been seen.
                        { "should": [
                            { "key": "last_seen_at", "range": { "lt": unseen_since } },
                            { "is_empty": { "key": "last_seen_at" } },
                        ] },
                      ],
                    },
                    "limit": limit,
                    "with_payload": true,
                })),
            )
            .await?;
        Ok(hits_of(res))
    }

    async fn set_context_vectors(&self, artifact_id: &str, vectors: Vec<Vec<f32>>) -> Result<()> {
        let id = point_uuid(artifact_id);
        // `points/vectors`, never `upsert`. A point write replaces the entire
        // payload — see the comment on `upsert` — and clearing `status` puts
        // every artifact the sweep hid straight back into search, on every
        // sweep run. This endpoint does not touch payload at all.
        // Update is PUT, delete is POST. Not symmetrical, and not a guess: the
        // update was written as POST first, and Qdrant 1.19 answered 404 —
        // which `call_absent_point_as_none` then read as "no such point" and
        // swallowed, so the sweep wrote its clusters to SQLite, reported `ok`,
        // and left the index with no `ctx` set at all. Nothing but running it
        // against a real Qdrant would have caught that.
        //
        // The bodies do not match either. The update names its vectors under
        // `vector`, singular, mapped by name; the delete takes `vectors`,
        // plural, a list of the names to drop. Writing the update's spelling
        // into the delete leaves the required field missing, and Qdrant answers
        // 400 rather than ignoring it — which took the whole sweep down on the
        // ordinary path, since every artifact decayed below `min_weight` is
        // cleared through here.
        let (method, path, body) = match vectors.is_empty() {
            true => (
                Method::POST,
                format!(
                    "/collections/{}/points/vectors/delete?wait=true",
                    self.alias
                ),
                json!({ "points": [id], "vectors": [CTX] }),
            ),
            false => (
                Method::PUT,
                format!("/collections/{}/points/vectors?wait=true", self.alias),
                json!({ "points": [{ "id": id, "vector": { CTX: vectors } }] }),
            ),
        };
        // Absent is not a failure, for the same reason `set_lifecycle` gives:
        // an artifact whose embedding never ran has no point, and a sweep that
        // errored on one would take the whole run down over an artifact that
        // has nothing to attach a set to.
        //
        // `missing_point_only` rather than the broader helper: that one accepts
        // any 404 on a live collection, which is exactly how a wrong verb read
        // as a missing point. Here only Qdrant actually saying so counts.
        let (status, text) = self.send(method, &path, Some(body)).await?;
        if status == reqwest::StatusCode::NOT_FOUND && text.contains("No point with id") {
            return Ok(());
        }
        if !status.is_success() {
            return Err(Error::Vector(format!("{path}: {}", text.trim())));
        }
        Ok(())
    }

    async fn context_query(
        &self,
        vector: &[f32],
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>> {
        let mut body = json!({
            "query": vector,
            "using": CTX,
            "limit": limit,
            "with_payload": true,
        });
        if let Some(f) = build_filter(filter) {
            body["filter"] = f;
        }
        // No recency stage and no pinning over this. Those exist to reorder a
        // ranked list of answers to a question; there is no question here, and
        // an artifact captured today is not a better answer to "it is Friday
        // afternoon" than one captured last year.
        let res: QueryResult = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/query", self.alias),
                Some(body),
            )
            .await?;
        // `hits_of` leaves `similarity` unset, which is correct here: `max_sim`
        // is not a query-to-document similarity.
        Ok(hits_of(res))
    }

    async fn delete_artifacts(&self, artifact_ids: &[String]) -> Result<()> {
        if artifact_ids.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = artifact_ids.iter().map(|c| point_uuid(c)).collect();
        let _: Value = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/delete?wait=true", self.alias),
                Some(json!({ "points": ids })),
            )
            .await?;
        Ok(())
    }

    async fn delete_by_corpus(&self, corpus_id: &str) -> Result<()> {
        let _: Value = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/delete?wait=true", self.alias),
                Some(json!({
                    "filter": { "must": [ { "key": "corpus_id", "match": { "value": corpus_id } } ] }
                })),
            )
            .await?;
        Ok(())
    }

    async fn facets(&self, limit: usize) -> Result<Facets> {
        Ok(Facets {
            categories: self.facet("category", limit).await?,
        })
    }

    async fn neighbours(&self, artifact_id: &str, limit: usize) -> Result<Vec<SearchHit>> {
        // The query is a point that is already in the index, so Qdrant looks
        // its vector up itself and this costs no embedding call. It may return
        // the reference point, hence one extra result and the filter below.
        let body = json!({
            "query": point_uuid(artifact_id),
            "using": DENSE,
            "limit": limit + 1,
            // Anything out of search is out of the related pane too. A
            // superseded artifact is by construction near-identical to its
            // keeper, so it would lead that list on every artifact it was
            // hidden in favour of; a deprecated one is content an operator
            // retired, and linking to it from a live artifact presents it as
            // current.
            "filter": { "must_not": [
                { "key": "status", "match": { "value": "superseded" } },
                { "key": "status", "match": { "value": "deprecated" } }
            ] },
            "with_payload": true,
        });
        // Absent is an empty list; unreachable, misconfigured or refused is an
        // error. Only the first is an ordinary state — a freshly captured
        // artifact whose embedding job has not run yet — and the detail pane
        // loses its related list rather than failing to open.
        //
        // Everything else has to propagate, because this is what `jobs::relate`
        // reads and relate is the only duplicate detector in the system. A
        // failure reported as an empty list is a unit that completes having
        // filed nothing, leaves a job row behind, and is therefore never asked
        // again: that artifact's duplicates are never merged and never
        // superseded, permanently and silently. Failing instead hands it to the
        // queue's backoff, which retries it once the store is back.
        let res: QueryResult = match self
            .call_absent_point_as_none(
                Method::POST,
                &format!("/collections/{}/points/query", self.alias),
                Some(body),
            )
            .await?
        {
            Some(r) => r,
            None => {
                tracing::debug!(
                    artifact_id,
                    "not in the collection yet; no neighbours to list"
                );
                return Ok(vec![]);
            }
        };
        let mut hits = hits_of(res);
        hits.retain(|h| h.payload.artifact_id != artifact_id);
        hits.truncate(limit);
        // One dense query, no prefetch and no fusion, so Qdrant's score here
        // *is* the cosine. Stated rather than left at `hits_of`'s `None`: the
        // relate unit compares this against `review_min`, a cosine threshold,
        // and `score` everywhere else in this trait is a fused rank that means
        // nothing across queries.
        for h in &mut hits {
            h.similarity = Some(h.score);
        }
        Ok(hits)
    }

    async fn sample(&self, limit: usize) -> Result<Vec<(String, Vec<f32>)>> {
        // One scroll page: Qdrant's scroll has no random start, and a slowly
        // changing first page is fine for a picture that refreshes every few
        // hours.
        let page: ScrollResult = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/scroll", self.alias),
                Some(json!({
                    "limit": limit,
                    // Only `artifact_id` is read below; a full payload would
                    // haul every point's chunk text along for nothing.
                    "with_payload": ["artifact_id"],
                    // Named rather than `true`, for the same reason: `true`
                    // returns every vector on the point, and the `ctx`
                    // multivector — up to eleven rows of `CTX_DIM` — is
                    // dropped on the next line. `dense_of` already reads the
                    // object form.
                    "with_vector": [DENSE],
                })),
            )
            .await?;
        Ok(page
            .points
            .iter()
            .filter_map(|p| {
                // A point without a dense vector or without an `artifact_id`
                // is not an engram point — dropped, not an error, the same
                // rule `all_artifact_ids` states in the trait doc.
                let dense = dense_of(&p.vector)?.as_array()?;
                let vector: Vec<f32> = dense
                    .iter()
                    .map(|x| x.as_f64().map(|f| f as f32))
                    .collect::<Option<_>>()?;
                let id = p.payload.get("artifact_id")?.as_str()?.to_string();
                Some((id, vector))
            })
            .collect())
    }

    async fn count(&self) -> Result<u64> {
        self.exact_count(&self.alias).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_collection_carries_a_context_multivector() {
        let body = collection_body(768);
        assert_eq!(body["vectors"][CTX]["size"], crate::core::context::CTX_DIM);
        assert_eq!(body["vectors"][CTX]["distance"], "Cosine");
        assert_eq!(
            body["vectors"][CTX]["multivector_config"]["comparator"],
            "max_sim"
        );
        // No HNSW, an exact scan. Right here rather than thrifty: the
        // candidates are only artifacts ever opened — hundreds to a few
        // thousand at 53 dimensions — and an index would be rebuilt on every
        // sweep write to beat a scan it cannot beat at that size.
        assert_eq!(body["vectors"][CTX]["hnsw_config"]["m"], 0);
        // And nothing else moved.
        assert_eq!(body["vectors"][DENSE]["size"], 768);
        assert_eq!(body["sparse_vectors"][SPARSE]["modifier"], "idf");
    }

    #[test]
    fn a_context_set_is_copied_by_a_rebuild_when_it_still_fits() {
        let dim = crate::core::context::CTX_DIM;
        let stored = json!({ DENSE: [1.0, 2.0], CTX: [vec![0.5; dim], vec![0.25; dim]] });
        let copied = ctx_of(&stored).unwrap();
        assert_eq!(copied.len(), 2);
        assert_eq!(copied[0].len(), dim);
    }

    #[test]
    fn a_context_set_from_an_older_layout_is_dropped_rather_than_copied() {
        // A changed encoder layout changes CTX_DIM. The old sets are discarded
        // and the next sweep rebuilds them from the raw bundles in
        // `context_events` — which costs no embedding call either way.
        let stored = json!({ DENSE: [1.0, 2.0], CTX: [[0.5, 0.5, 0.5]] });
        assert!(ctx_of(&stored).is_none());
    }

    #[test]
    fn a_point_with_no_context_set_reindexes_without_one() {
        assert!(ctx_of(&json!({ DENSE: [1.0, 2.0] })).is_none());
        // A pre-alias point with one unnamed vector.
        assert!(ctx_of(&json!([1.0, 2.0])).is_none());
        // And an empty set is the same as none, not a set of nothing.
        assert!(ctx_of(&json!({ DENSE: [1.0], CTX: [] })).is_none());
    }

    #[test]
    fn the_offer_excludes_hidden_artifacts_by_must_not() {
        // `must_not` rather than a positive match, for the reason `build_filter`
        // gives: a point carrying no `status` key at all reads as active, and a
        // positive clause would drop every hand-written one.
        let f = build_filter(&SearchFilter::default()).unwrap();
        assert!(f.get("must").is_none());
        let not = f["must_not"].as_array().unwrap();
        assert_eq!(not.len(), 2);
    }

    #[tokio::test]
    async fn neighbours_reports_an_unreachable_store_rather_than_no_neighbours() {
        // `jobs::relate` is the only duplicate detector there is, and a job row
        // survives its completion — so a unit that runs while Qdrant is down
        // and answers "no neighbours" is not a retry, it is a permanent
        // verdict. That artifact's duplicates are never merged and never
        // superseded, and nothing asks about it again. An empty list has to
        // mean "asked, and there are none".
        let v = QdrantVectors::connect(&VectorConfig {
            // Nothing listens here, so the round trip fails before any status.
            url: "http://127.0.0.1:1".into(),
            collection: "engram".into(),
            api_key: None,
            recency_weight: 0.05,
            recency_half_life_days: 180,
            pinned_boost: 0.15,
            weak_below: 0.35,
            per_source_cap: 3,
        })
        .await
        .unwrap();

        assert!(
            v.neighbours("any-artifact", 5).await.is_err(),
            "a store that could not be reached was reported as an artifact with no duplicates"
        );
    }

    #[test]
    fn facet_hits_become_chips_ordered_by_count() {
        let res: FacetResult = serde_json::from_value(json!({
            "hits": [
                { "value": "concept", "count": 1 },
                { "value": "procedure", "count": 4 },
            ]
        }))
        .unwrap();
        assert_eq!(
            facet_counts(res),
            vec![
                FacetCount {
                    value: "procedure".into(),
                    count: 4
                },
                FacetCount {
                    value: "concept".into(),
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn a_facet_value_that_is_not_a_keyword_is_not_a_chip() {
        // Nothing can be filtered by a number the chip cannot put in a URL, so
        // a non-string value is dropped rather than stringified into a filter
        // that would match nothing.
        let res: FacetResult = serde_json::from_value(json!({
            "hits": [{ "value": 7, "count": 3 }, { "value": "procedure", "count": 1 }]
        }))
        .unwrap();
        assert_eq!(
            facet_counts(res),
            vec![FacetCount {
                value: "procedure".into(),
                count: 1
            }]
        );
    }

    #[test]
    fn a_pre_taxonomy_payload_is_brought_up_to_the_current_key_names() {
        let old = json!({
            "chunk_id": "c1", "source_id": "s1", "text": "t", "created_at": 1
        });
        let new = renamed_payload(&old);
        assert_eq!(new["artifact_id"], "c1");
        assert_eq!(new["corpus_id"], "s1");
        assert!(new.get("chunk_id").is_none());
        assert!(new.get("source_id").is_none());
        // Everything else survives: a rebuild copies payloads it does not
        // understand, and dropping one would lose data no re-embed restores.
        assert_eq!(new["text"], "t");
        assert_eq!(new["created_at"], 1);
    }

    #[test]
    fn renaming_a_current_payload_changes_nothing() {
        // A rebuild run twice must not disturb what the first one produced.
        let current = json!({ "artifact_id": "a1", "corpus_id": "c1", "text": "t" });
        assert_eq!(renamed_payload(&current), current);
    }

    #[test]
    fn dimension_mismatch_message_names_both_numbers() {
        let e = dimension_mismatch("chunks", 768, 1024);
        let msg = e.to_string();
        assert!(msg.contains("768"), "{msg}");
        assert!(msg.contains("1024"), "{msg}");
        assert!(msg.contains("chunks"), "{msg}");
    }

    #[test]
    fn dimension_mismatch_does_not_promise_a_rebuild() {
        // A rebuild copies vectors, so it can never resolve a width change.
        // Suggesting it would send the reader in a circle.
        let msg = dimension_mismatch("chunks_v1", 768, 1024).to_string();
        assert!(msg.contains("cannot change their width"), "{msg}");
        assert!(msg.contains("embed again"), "no way forward offered: {msg}");
    }

    #[test]
    fn chunk_ids_map_to_stable_point_ids() {
        // The same chunk must always hit the same Qdrant point, otherwise
        // re-embedding leaves an orphaned vector behind.
        let a = point_uuid("chunk-abc");
        let b = point_uuid("chunk-abc");
        let c = point_uuid("chunk-xyz");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn uuid_chunk_ids_pass_through_unchanged() {
        // Real chunk ids are UUIDv7 already; hashing them would waste the
        // time-ordering Qdrant can otherwise exploit.
        let id = uuid::Uuid::now_v7().to_string();
        assert_eq!(point_uuid(&id), id);
    }

    #[test]
    fn hashed_point_ids_are_valid_uuids() {
        // Qdrant rejects a point id that is neither an integer nor a UUID.
        let got = point_uuid("not-a-uuid");
        assert!(uuid::Uuid::parse_str(&got).is_ok(), "got {got}");
    }

    #[test]
    fn base_url_loses_its_trailing_slash() {
        // `{base}/collections` with a trailing slash becomes `//collections`,
        // which a reverse proxy may answer with a redirect rather than JSON.
        assert_eq!(
            normalize_base("http://localhost:6333/"),
            "http://localhost:6333"
        );
        assert_eq!(normalize_base("https://q.example//"), "https://q.example");
        assert_eq!(
            normalize_base("http://localhost:6333"),
            "http://localhost:6333"
        );
    }

    #[test]
    fn a_filter_that_narrows_nothing_produces_no_filter_at_all() {
        // `{"must": []}` matches nothing in Qdrant, so a search that narrows
        // nothing must omit the key rather than send an empty condition list.
        // Asking for superseded and deprecated points back is what "narrows
        // nothing" means now.
        assert!(
            build_filter(&SearchFilter {
                include_superseded: true,
                include_deprecated: true,
                ..Default::default()
            })
            .is_none()
        );
    }

    #[test]
    fn the_default_filter_excludes_superseded_and_deprecated_points() {
        // The default is what every ordinary search sends, and superseding /
        // deprecating is only meaningful if it keeps an artifact out of that.
        let f = build_filter(&SearchFilter::default()).unwrap();
        assert!(
            f.get("must").is_none(),
            "an empty condition list must not be sent: {f}"
        );
        assert_eq!(
            f["must_not"],
            json!([
                { "key": "status", "match": { "value": "superseded" } },
                { "key": "status", "match": { "value": "deprecated" } },
            ]),
        );
    }

    #[test]
    fn every_tag_becomes_its_own_must_condition() {
        // Tags are an AND. One condition per tag is what makes that true;
        // a single condition with a list would be an OR.
        let f = build_filter(&SearchFilter {
            tags: vec!["linux".into(), "forensics".into()],
            category: Some("procedure".into()),
            include_superseded: false,
            include_deprecated: false,
            corpus_id: None,
        })
        .unwrap();
        let must = f["must"].as_array().unwrap();
        assert_eq!(must.len(), 3, "expected two tags plus the category: {f}");
        assert_eq!(must[0]["key"], "tags");
        assert_eq!(must[0]["match"]["value"], "linux");
        assert_eq!(must[2]["key"], "category");
    }

    #[test]
    fn a_qdrant_error_body_is_reduced_to_its_message() {
        let body = r#"{"status":{"error":"Not found: Collection `x` doesn't exist!"},"time":0.0}"#;
        let msg = describe_failure(StatusCode::NOT_FOUND, body);
        assert!(msg.contains("doesn't exist"), "{msg}");
        assert!(!msg.contains("time"), "raw envelope leaked: {msg}");
    }

    #[test]
    fn an_unparseable_error_body_is_truncated_not_dumped() {
        let body = "x".repeat(10_000);
        let msg = describe_failure(StatusCode::BAD_GATEWAY, &body);
        assert!(
            msg.len() < 300,
            "unbounded body reached the error: {}",
            msg.len()
        );
    }

    #[test]
    fn generations_are_numbered_and_read_back() {
        assert_eq!(generation_name("chunks", 1), "chunks_v1");
        assert_eq!(generation_of("chunks", "chunks_v1"), 1);
        assert_eq!(generation_of("chunks", "chunks_v12"), 12);
    }

    #[test]
    fn a_collection_whose_suffix_is_not_a_number_does_not_panic() {
        assert_eq!(generation_of("chunks", "chunks_vNEXT"), 0);
        assert_eq!(generation_of("chunks", "something_else"), 0);
    }

    #[test]
    fn only_a_numeric_suffix_makes_a_collection_ours() {
        // `drop_collection` deletes everything this claims, so a neighbouring
        // collection that merely starts the same way must not be claimed.
        assert_eq!(generation_number("chunks", "chunks_v3"), Some(3));
        assert_eq!(generation_number("chunks", "chunks_verbose"), None);
        assert_eq!(generation_number("chunks", "chunks_vault"), None);
        assert_eq!(generation_number("chunks", "chunks_v"), None);
        assert_eq!(generation_number("chunks", "chunks_v1x"), None);
        assert_eq!(generation_number("chunks", "chunks"), None);
        // `+1` and `1_0` parse as numbers in Rust but are not names we write.
        assert_eq!(generation_number("chunks", "chunks_v+1"), None);
        assert_eq!(generation_number("chunks", "chunks_v1_0"), None);
    }

    #[test]
    fn the_scoring_formula_survives_a_point_without_a_last_verified_at() {
        // Qdrant fails the entire query, not just the offending point, when a
        // formula reads a key the payload does not have. The default is `now`,
        // not the epoch: a missing stamp means unknown, not maximally stale.
        let f = scoring_formula(1_700_000_000, 86_400, 0.05, 0.15);
        assert_eq!(
            f["defaults"]["last_verified_at"], 1_700_000_000,
            "an unstamped payload must score as unknown, not as maximally stale: {f}"
        );
    }

    #[test]
    fn a_lifecycle_write_names_the_status_and_the_winner() {
        let p = lifecycle_payload(ArtifactStatus::Deprecated, None);
        assert_eq!(p["status"], "deprecated");

        let s = lifecycle_payload(ArtifactStatus::Superseded, Some("winner"));
        assert_eq!(s["status"], "superseded");
        assert_eq!(s["superseded_by"], "winner");
    }

    #[test]
    fn only_a_verify_resets_the_hit_counter() {
        // `stale_max_hits` counts retrievals since the last verification, so
        // only an explicit verify may zero the counter.
        let verified = verified_payload(42, true);
        assert_eq!(verified["last_verified_at"], 42);
        assert_eq!(verified["hit_count"], 0);

        let bulk = verified_payload(42, false);
        assert_eq!(bulk["last_verified_at"], 42);
        assert!(
            bulk.get("hit_count").is_none(),
            "a bulk stamp must leave the counter alone: {bulk}"
        );
    }

    #[test]
    fn a_disabled_weight_contributes_no_term() {
        // Zero weights must drop out of the formula rather than multiply by
        // zero, so an operator turning recency off stops paying for it.
        let f = scoring_formula(0, 86_400, 0.0, 0.0);
        assert_eq!(f["formula"]["sum"].as_array().unwrap().len(), 1);
        assert_eq!(f["formula"]["sum"][0], "$score");
    }

    #[test]
    fn new_collections_declare_both_a_dense_and_a_sparse_vector() {
        // The sparse slot is unused until hybrid search lands, but adding it
        // later would mean another rebuild for every deployment.
        let body = collection_body(768);
        assert_eq!(body["vectors"][DENSE]["size"], 768);
        assert_eq!(body["vectors"][DENSE]["distance"], "Cosine");
        assert_eq!(body["sparse_vectors"][SPARSE]["modifier"], "idf");
    }

    #[test]
    fn a_dense_vector_is_found_in_either_storage_layout() {
        // A rebuild reads from the previous generation, which may predate
        // named vectors.
        let named = json!({ DENSE: [1.0, 0.0], "other": [9.0] });
        assert_eq!(dense_of(&named).unwrap(), &json!([1.0, 0.0]));

        let unnamed = json!([1.0, 0.0]);
        assert_eq!(dense_of(&unnamed).unwrap(), &json!([1.0, 0.0]));

        assert!(dense_of(&Value::Null).is_none());
        assert!(
            dense_of(&json!({ "sparse_only": {} })).is_none(),
            "a point without a dense vector must be reported, not silently skipped"
        );
    }

    /// A store pointed at a mock, for the arms that turn an HTTP status into a
    /// decision rather than a value.
    async fn against(server: &wiremock::MockServer) -> QdrantVectors {
        QdrantVectors::connect(&VectorConfig {
            url: server.uri(),
            collection: "engram".into(),
            api_key: None,
            recency_weight: 0.05,
            recency_half_life_days: 180,
            pinned_boost: 0.15,
            weak_below: 0.35,
            per_source_cap: 3,
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn clearing_a_context_set_names_the_vector_the_way_the_delete_takes_it() {
        // The update and the delete do not spell it the same way, and the
        // difference is not cosmetic: `vectors` is required on the delete, so
        // sending the update's `vector` leaves it missing and Qdrant answers
        // 400 rather than ignoring the field it does not know. That is the
        // ordinary path — every artifact whose profile decayed below
        // `min_weight` is cleared through here — so the whole sweep died on it
        // and the stale centroids stayed in the index, still being offered.
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let id = "0192abcd-0000-7000-8000-000000000000";
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/collections/engram/points/vectors/delete"))
            .and(body_json(
                json!({ "points": [point_uuid(id)], "vectors": [CTX] }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "status": "completed" }, "status": "ok"
            })))
            .expect(1)
            .mount(&server)
            .await;

        against(&server)
            .await
            .set_context_vectors(id, Vec::new())
            .await
            .expect("an empty set is a delete, and the delete takes `vectors`");
    }

    #[tokio::test]
    async fn a_lifecycle_change_on_an_artifact_with_no_point_is_not_a_failure() {
        // `merge::reap_stranded` retires a merge *because it could not be
        // indexed*, so the point it would update never existed. Reading that
        // 404 as an error meant the reap deprecated the row and then died
        // before reopening the merge's pairs or dropping the embed job that
        // could never succeed — on every call, for every stranded merge, while
        // the log said the reap had failed.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/collections/engram/points/payload"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "status": { "error": "Not found: No point with id 0192… found" }
            })))
            .mount(&server)
            .await;

        against(&server)
            .await
            .set_lifecycle(
                "0192abcd-0000-7000-8000-000000000000",
                ArtifactStatus::Deprecated,
                None,
            )
            .await
            .expect("a missing point is nothing to write to, not an error");
    }

    #[tokio::test]
    async fn a_lifecycle_change_against_a_missing_collection_is_still_a_failure() {
        // The other thing Qdrant answers 404 to. An alias pointing at a dropped
        // generation affects every artifact, and reading it as "this one has no
        // point" would turn a broken store into a silent success each time.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/collections/engram/points/payload"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "status": { "error": "Not found: Collection `engram` doesn't exist!" }
            })))
            .mount(&server)
            .await;
        // What `call_absent_point_as_none` asks when the message does not name
        // a point: is the collection there? It resolves the alias first, then
        // asks. It is not.
        Mock::given(method("GET"))
            .and(path("/aliases"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "aliases": [] }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/collections/engram/exists"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "exists": false }
            })))
            .mount(&server)
            .await;

        assert!(
            against(&server)
                .await
                .set_lifecycle(
                    "0192abcd-0000-7000-8000-000000000000",
                    ArtifactStatus::Deprecated,
                    None
                )
                .await
                .is_err(),
            "a store with no collection must be reported, not read as an empty one"
        );
    }

    #[test]
    fn a_name_collision_names_the_collection_and_the_way_out() {
        let msg = name_collision("chunks").to_string();
        assert!(msg.contains("chunks"), "{msg}");
        assert!(msg.contains("vector.collection"), "{msg}");
    }
}
