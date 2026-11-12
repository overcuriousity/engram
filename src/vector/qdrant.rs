//! Qdrant over REST, the port operators expose. `vector.collection` names an alias over `{alias}_v1`, `_v2`, …
//! so a model or schema change is a background rebuild and an atomic swap.

use super::sparse::SparseVector;
use super::{
    FacetCount, Facets, LifecycleRow, Recency, SearchFilter, SearchHit, Touch, VectorExport,
    VectorPayload, VectorPoint, VectorStore,
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
use std::path::Path;
use std::time::Duration;

/// Long enough for a cold HNSW segment load; short enough that a wedged server is a retryable failure.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Named because a collection cannot gain named vectors without a rebuild.
pub const DENSE: &str = "dense";

/// Declared from the first generation so hybrid search is a code change, not a migration.
pub const SPARSE: &str = "text";

const REINDEX_BATCH: usize = 256;

/// Backs off 200 ms, 400 ms, …: about three seconds, startup-shaped.
const READY_ATTEMPTS: u32 = 5;
const READY_BACKOFF: Duration = Duration::from_millis(200);

/// One element per learned situation, scored with `max_sim`: two to five per artifact.
pub const CTX: &str = "ctx";

/// A tag, not a column: tags are editable without re-embedding and already indexed.
pub const PINNED_TAG: &str = "pinned";

const SECONDS_PER_DAY: u64 = 86_400;

pub struct QdrantVectors {
    http: Client,
    base: String,
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

/// A named vector cannot be resized, and an old `ctx` width 400s every read and write; a rebuild repairs it.
fn ctx_dimension_mismatch(collection: &str, configured: usize, existing: Option<u64>) -> Error {
    let found = match existing {
        Some(n) => format!("has a `{CTX}` vector {n} wide"),
        None => format!("has no `{CTX}` vector at all"),
    };
    Error::Vector(format!(
        "collection `{collection}` {found}, but this build encodes situations at {configured}. \
         Refusing to start: Qdrant rejects every context read and write at the other width, \
         which would leave recommendations silently falling back to random and the context \
         sweep failing part way through. Run `--reindex --user <slug>`, once per user, to \
         build the next generation at {configured} — dense vectors are copied across, so \
         nothing is re-embedded, and the situations rebuild from `context_events` on the \
         next sweep."
    ))
}

/// A plain collection where the alias belongs blocks everything; it is not ours to delete.
fn name_collision(alias: &str) -> Error {
    Error::Vector(format!(
        "`{alias}` is a plain collection, but engram addresses vectors through an alias of that \
         name, and Qdrant will not let the two share it. Rename or remove that collection, or \
         point `vector.collection` at a name nothing else is using."
    ))
}

/// Point ids must be integers or UUIDs; non-UUID ids are hashed deterministically.
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

/// A double slash is answered by some proxies with a redirect.
fn normalize_base(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

fn generation_name(alias: &str, n: u32) -> String {
    format!("{alias}_v{n}")
}

/// `None` separates `chunks_v2` from `chunks_verbose`; this decides what `drop_collection` deletes.
fn generation_number(alias: &str, collection: &str) -> Option<u32> {
    collection
        .strip_prefix(&format!("{alias}_v"))
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse().ok())
}

/// A name with no number counts as zero, so the next is `_v1`.
fn generation_of(alias: &str, collection: &str) -> u32 {
    generation_number(alias, collection).unwrap_or(0)
}

/// An empty `must` list matches nothing, so callers skip the filter instead.
fn build_filter(filter: &SearchFilter) -> Option<Value> {
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

    let mut body = json!({});
    if !must.is_empty() {
        body["must"] = json!(must);
    }
    // `must_not`: a point with no `status` key reads as active, and a positive clause would drop it.
    let mut must_not: Vec<Value> =
        vec![json!({ "key": "status", "match": { "value": "trashed" } })];
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

fn lifecycle_payload(status: ArtifactStatus, superseded_by: Option<&str>) -> Value {
    json!({
        "status": status.as_str(),
        "superseded_by": superseded_by,
    })
}

/// An absent `status` means active, as every filter reads it.
fn stored_status(payload: &Value) -> ArtifactStatus {
    match payload.get("status").and_then(Value::as_str) {
        Some(s) => ArtifactStatus::parse(s),
        None => ArtifactStatus::Active,
    }
}

/// `reset_hits` zeroes `hit_count` for an explicit verify (`Core::verify`); a bulk stamp must not.
fn verified_payload(at: i64, reset_hits: bool) -> Value {
    let mut p = json!({ "last_verified_at": at });
    if reset_hits {
        p["hit_count"] = json!(0);
    }
    p
}

fn collection_body(dim: usize) -> Value {
    json!({
        "vectors": {
            DENSE: { "size": dim, "distance": "Cosine" },
            CTX: {
                "size": crate::core::context::CTX_DIM,
                "distance": "Cosine",
                "multivector_config": { "comparator": "max_sim" },
                // Exact scan: only opened artifacts at 45 dimensions, and HNSW would rebuild on every sweep write.
                "hnsw_config": { "m": 0 },
            },
        },
        "sparse_vectors": { SPARSE: { "modifier": "idf" } },
    })
}

/// A sparse vector with no dimensions is not the same as no sparse vector.
fn sparse_body(sparse: &SparseVector) -> Option<Value> {
    if sparse.is_empty() {
        return None;
    }
    Some(json!({ "indices": sparse.indices, "values": sparse.values }))
}

/// Makes `--reindex` the migration for the taxonomy rename; idempotent.
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

/// Must match what the embed job indexed: title then body.
fn sparse_of_payload(payload: &Value) -> SparseVector {
    let text = payload.get("text").and_then(Value::as_str).unwrap_or("");
    match payload.get("title").and_then(Value::as_str) {
        Some(t) if !t.is_empty() => super::sparse::encode_document(&format!("{t}\n{text}")),
        _ => super::sparse::encode_document(text),
    }
}

/// `defaults` is required: one point missing `last_verified_at` fails the whole query. Defaults to `now` (neutral).
/// Decays on `last_verified_at`, not `created_at`; never reads `hit_count`, which would self-reinforce.
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
        // A matching filter condition evaluates to 1.0; no default needed.
        terms.push(json!({
            "mult": [pinned, { "key": "tags", "match": { "value": PINNED_TAG } }]
        }));
    }
    json!({
        "formula": { "sum": terms },
        "defaults": { "last_verified_at": now },
    })
}

/// Named or a pre-alias single default vector.
fn dense_of(vector: &Value) -> Option<&Value> {
    match vector {
        Value::Array(_) => Some(vector),
        Value::Object(m) => m.get(DENSE),
        _ => None,
    }
}

/// `None` for no set, a pre-alias point, or a width from an older `BLOCKS`; the next sweep rebuilds it.
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

/// `result` is absent on failures.
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

/// Only keyword fields are facetted; non-string values cannot filter and are dropped.
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
        // 6334 is gRPC; a REST client there fails like a network problem.
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

    /// Used by the integration suite to start clean.
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

    pub async fn resolve_alias(&self) -> Result<Option<String>> {
        // Listing is `/aliases`; only the update action lives under `/collections/aliases`.
        let list: AliasList = self.call(Method::GET, "/aliases", None).await?;
        Ok(list
            .aliases
            .into_iter()
            .find(|a| a.alias_name == self.alias)
            .map(|a| a.collection_name))
    }

    /// By parsed generation number only: `{alias}_vault` and `{alias}` belong to someone else, and this is what gets deleted.
    async fn generations(&self) -> Result<Vec<String>> {
        let list: CollectionList = self.call(Method::GET, "/collections", None).await?;
        Ok(list
            .collections
            .into_iter()
            .map(|c| c.name)
            .filter(|n| generation_number(&self.alias, n).is_some())
            .collect())
    }

    async fn newest_generation(&self) -> Result<Option<String>> {
        Ok(self
            .generations()
            .await?
            .into_iter()
            .filter_map(|n| generation_number(&self.alias, &n).map(|g| (g, n)))
            .max_by_key(|(g, _)| *g)
            .map(|(_, n)| n))
    }

    /// Asked of the alias' target: a dangling alias or a same-named collection must not read as live.
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
            // Losing a startup race is fine; inheriting a collection of the wrong width is not.
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

    /// On every adopting path: an index added by a later release would otherwise leave filtered searches as full scans.
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

    /// Both actions apply in one transaction, so no request sees an unresolved alias.
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

    /// Losing the creation race is fine; only an alias that cannot serve our vectors fails.
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

    /// New shards answer `0 of 0 read operations failed` until ready; the same message forever means broken, so wait briefly.
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
        // Named vectors nest deeper than a pre-alias single vector; accept both.
        info.pointer(&format!("/config/params/vectors/{DENSE}/size"))
            .or_else(|| info.pointer("/config/params/vectors/size"))
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::Vector("could not read collection vector dimension".into()))
    }

    /// `None` for a collection without one; the caller decides.
    async fn ctx_dim(&self, collection: &str) -> Result<Option<u64>> {
        let info: Value = self
            .call(Method::GET, &format!("/collections/{collection}"), None)
            .await?;
        Ok(info
            .pointer(&format!("/config/params/vectors/{CTX}/size"))
            .and_then(Value::as_u64))
    }

    /// See `ctx_dimension_mismatch`.
    async fn check_ctx_dim(&self, collection: &str) -> Result<()> {
        let want = crate::core::context::CTX_DIM;
        match self.ctx_dim(collection).await? {
            Some(n) if n as usize == want => Ok(()),
            found => Err(ctx_dimension_mismatch(collection, want, found)),
        }
    }

    /// `exact`: the collection-info counter may lag accepted writes.
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

    /// Read only before overwriting payloads built without these keys, which other writers own.
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

    /// Approximate, the right trade for a row of chips.
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

    /// Qdrant rejects an explicit `"offset": null`, so it is spliced in only when present.
    async fn scroll_page(
        &self,
        collection: &str,
        body: &Value,
        offset: &Value,
    ) -> Result<ScrollResult> {
        let mut body = body.clone();
        if !offset.is_null() {
            body["offset"] = offset.clone();
        }
        self.call(
            Method::POST,
            &format!("/collections/{collection}/points/scroll"),
            Some(body),
        )
        .await
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

    /// Dense vectors are copied, so no embedding calls. The previous generation stays as the only rollback.
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
            let page = self
                .scroll_page(
                    &source,
                    &json!({
                        "limit": REINDEX_BATCH,
                        "with_payload": true,
                        "with_vector": true,
                    }),
                    &offset,
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
                // Sparse vectors are recomputed, which adds the lexical half to older generations.
                let mut vector = json!({ DENSE: dense });
                if let Some(sp) = sparse_body(&sparse_of_payload(&p.payload)) {
                    vector[SPARSE] = sp;
                }
                // Skipped on a width mismatch; context vectors rebuild from `context_events`.
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

/// Foreign points (copied or operator-inserted) are skipped with a log line rather than failing search.
fn hits_of(res: QueryResult) -> Vec<SearchHit> {
    let mut out = Vec::with_capacity(res.points.len());
    for p in res.points {
        match serde_json::from_value::<VectorPayload>(p.payload) {
            Ok(payload) => out.push(SearchHit {
                payload,
                score: p.score,
                // Filled in by `search`; other callers are listings with no query.
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

/// Ties are re-sorted so the order does not depend on Qdrant's.
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

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Falls back to the raw body, bounded.
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
    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T> {
        let (status, text) = self.send(method, path, body).await?;
        if !status.is_success() {
            // The path says which of several similar requests failed.
            return Err(Error::Vector(format!(
                "{path}: {}",
                describe_failure(status, &text)
            )));
        }
        decode(path, &text)
    }

    /// Qdrant 404s both a missing point and a missing collection; the latter is a broken store and must stay an error.
    /// A 404 not naming the point is settled by asking whether the collection exists, not by parsing prose.
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
            self.check_ctx_dim(&current).await?;
            // An already-serving collection predates any index this release added.
            self.ensure_payload_indexes(&current).await?;
            return Ok(());
        }

        // A collection where the alias should be holds real vectors; refusing keeps `--reindex` safe.
        if self.collection_exists(&self.alias).await? {
            return Err(name_collision(&self.alias));
        }

        // Generations without an alias mean a rebuild died mid-swap; adopting the newest is repair.
        if let Some(orphan) = self.newest_generation().await? {
            let existing = self.vector_dim(&orphan).await?;
            if existing as usize != dim {
                return Err(dimension_mismatch(&orphan, dim, existing as usize));
            }
            self.check_ctx_dim(&orphan).await?;
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
        // A point write replaces the whole payload, so carry `last_seen_at`, `status` and the rest forward,
        // or a re-embed would put hidden artifacts back into results.
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
        // Absent is not a failure: an artifact may have no point yet, and `embed::upsert_with_current_lifecycle` writes status later.
        // `merge::reap_stranded` depends on this for merges that never got a point.
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
        // One request per batch. `last_verified_at` is merged without touching `hit_count` (see `Core::verify`).
        // Capped so a badly drifted base does not send the one oversized body that fails.
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
        // Batched: these retrieves carry full payloads.
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
        // Batched: a retrieve of thousands of ids may be refused.
        const BATCH: usize = 512;
        for batch in artifact_ids.chunks(BATCH) {
            let ids: Vec<String> = batch.iter().map(|id| point_uuid(id)).collect();
            let found: Vec<ScrolledPoint> = self
                .call(
                    Method::POST,
                    &format!("/collections/{}/points", self.alias),
                    Some(json!({
                        "ids": ids,
                        // `point_uuid` is one-way, so the artifact id comes from the payload.
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
            let page = self
                .scroll_page(
                    &self.alias,
                    &json!({
                        "limit": PAGE,
                        "with_payload": ["artifact_id"],
                        "with_vector": false,
                    }),
                    &offset,
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

    /// Scrolled and sorted, not sampled: a work queue must be stable across renders. Capped at `STALE_SCAN` on page render.
    async fn stale_candidates(
        &self,
        older_than: i64,
        max_hits: i64,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        const STALE_SCAN: usize = 10_000;
        const PAGE: usize = 1_000;

        let filter = json!({
            "must_not": [
                { "key": "status", "match": { "value": "deprecated" } },
                { "key": "status", "match": { "value": "trashed" } },
                { "key": "status", "match": { "value": "superseded" } },
            ],
            "must": [
                // No stamp is unknown, not stale; absent `hit_count` legitimately means never retrieved.
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
            let page = self
                .scroll_page(
                    &self.alias,
                    &json!({
                        "filter": filter,
                        "limit": PAGE.min(STALE_SCAN - found.len()),
                        "with_payload": true,
                        "with_vector": false,
                    }),
                    &offset,
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

        // Stalest first; the id tiebreak keeps the order stable between renders.
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
                // No query; the order above carries staleness.
                score: 0.0,
                similarity: None,
            })
            .collect())
    }

    /// Under the connected weight until the idle pass adopts another.
    async fn search(
        &self,
        vector: &[f32],
        sparse: &SparseVector,
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>> {
        self.search_weighted(
            vector,
            sparse,
            limit,
            filter,
            Recency {
                weight: self.recency_weight,
                half_life_days: self.recency_half_life_days,
            },
        )
        .await
    }

    fn applies_scoring_formula(&self) -> bool {
        true
    }

    async fn search_weighted(
        &self,
        vector: &[f32],
        sparse: &SparseVector,
        limit: usize,
        filter: &SearchFilter,
        recency: Recency,
    ) -> Result<Vec<SearchHit>> {
        let f = build_filter(filter);

        let mut body = match sparse_body(sparse) {
            // Reciprocal rank fusion needs no calibration between cosine and BM25.
            Some(terms) => {
                let mut dense_branch = json!({ "query": vector, "using": DENSE, "limit": limit });
                let mut sparse_branch = json!({ "query": terms, "using": SPARSE, "limit": limit });
                // Per branch: it narrows what each half retrieves, not what fusion returns.
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
            // No indexable term, so the lexical branch is skipped.
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

        // A final stage over retrieval: it reorders without changing what was retrieved.
        if recency.weight > 0.0 || self.pinned_boost > 0.0 {
            let mut prefetch = std::mem::replace(&mut body, Value::Null);
            // The outer stage fetches payload once; the prefetch would carry every candidate's text.
            if let Some(m) = prefetch.as_object_mut() {
                m.remove("with_payload");
            }
            body = json!({
                "prefetch": [prefetch],
                "query": scoring_formula(
                    now_secs(),
                    recency.half_life_days as u64 * SECONDS_PER_DAY,
                    recency.weight,
                    self.pinned_boost,
                ),
                "limit": limit,
                "with_payload": true,
            });
        }

        // Batched raw dense query: cosine is the only score comparable across queries. A lexical-only hit gets none.
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
        let missing = || Error::Vector("a batched search came back empty".into());
        let similarities: HashMap<String, f32> = res
            .pop()
            .ok_or_else(missing)?
            .points
            .into_iter()
            .filter_map(|p| {
                let id = p.payload.get("artifact_id")?.as_str()?.to_string();
                Some((id, p.score))
            })
            .collect();

        let mut hits = hits_of(res.pop().ok_or_else(missing)?);
        for h in &mut hits {
            h.similarity = similarities.get(&h.payload.artifact_id).copied();
        }
        Ok(hits)
    }

    async fn touch(&self, targets: &[Touch], seen_at: i64) -> Result<()> {
        if targets.is_empty() {
            return Ok(());
        }
        // `hit_count` has no atomic increment, so current values come from the caller or a read for its own points.
        // This waits even off the request path: the shutdown drain exists to keep these stamps.
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
                    // `must_not` as in `build_filter`; otherwise the forgotten list offers what the sweep just retired.
                    "filter": {
                      "must_not": [
                        { "key": "status", "match": { "value": "trashed" } },
                        { "key": "status", "match": { "value": "superseded" } },
                        { "key": "status", "match": { "value": "deprecated" } }
                      ],
                      "must": [
                        { "key": "created_at", "range": { "lt": older_than } },
                        // AND-of-OR: a chunk without `last_seen_at` has never been seen.
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
        // `points/vectors`, never `upsert`, which would clear `status`. Update is PUT, delete is POST (POST 404s on 1.19);
        // update takes `vector` by name, delete takes a `vectors` list, or Qdrant 400s.
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
        // Absent is not a failure, but only when Qdrant says the point is missing: a wrong verb also 404s.
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
        // No recency or pinning: there is no question here, only a situation.
        let res: QueryResult = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/query", self.alias),
                Some(body),
            )
            .await?;
        // `max_sim` is not a query-to-document similarity.
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
        // Qdrant looks up the reference point's vector itself; it may return that point, hence one extra.
        let body = json!({
            "query": point_uuid(artifact_id),
            "using": DENSE,
            "limit": limit + 1,
            // Superseded and deprecated artifacts are out of the related pane too.
            "filter": { "must_not": [
                { "key": "status", "match": { "value": "trashed" } },
                { "key": "status", "match": { "value": "superseded" } },
                { "key": "status", "match": { "value": "deprecated" } }
            ] },
            "with_payload": true,
        });
        // Only absent is an empty list. Other failures propagate: `jobs::relate` would complete having filed nothing,
        // and its duplicates would never be merged.
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
        // One dense query without fusion, so the score is the cosine `review_min` compares against.
        for h in &mut hits {
            h.similarity = Some(h.score);
        }
        Ok(hits)
    }

    async fn dense_of(&self, artifact_id: &str) -> Result<Option<Vec<f32>>> {
        // Retrieve endpoint so the `ctx` multivector stays on the server.
        let points: Option<Vec<ScrolledPoint>> = self
            .call_absent_point_as_none(
                Method::POST,
                &format!("/collections/{}/points", self.alias),
                Some(json!({
                    "ids": [point_uuid(artifact_id)],
                    "with_payload": false,
                    "with_vector": [DENSE],
                })),
            )
            .await?;
        Ok(points.and_then(|ps| {
            let p = ps.into_iter().next()?;
            let dense = dense_of(&p.vector)?.as_array()?;
            dense
                .iter()
                .map(|x| x.as_f64().map(|f| f as f32))
                .collect::<Option<Vec<f32>>>()
        }))
    }

    async fn sample(&self, limit: usize) -> Result<Vec<(String, Vec<f32>)>> {
        // Scroll has no random start; a slowly changing first page suits a picture.
        let page = self
            .scroll_page(
                &self.alias,
                &json!({
                    "limit": limit,
                    "with_payload": ["artifact_id"],
                    // Named so the `ctx` multivector stays on the server.
                    "with_vector": [DENSE],
                }),
                &Value::Null,
            )
            .await?;
        Ok(page
            .points
            .iter()
            .filter_map(|p| {
                // Not an engram point: dropped, as `all_artifact_ids` states.
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

    /// Read live: `--reindex` repoints the alias underneath a running server.
    async fn revision(&self) -> Result<Option<String>> {
        self.resolve_alias().await
    }

    /// Created server-side, downloaded while hashed, then deleted so Qdrant keeps no copy of every backup.
    async fn export_snapshot(&self, into: &Path) -> Result<VectorExport> {
        let generation = match self.resolve_alias().await? {
            Some(g) if self.collection_exists(&g).await? => g,
            Some(g) => {
                return Err(Error::Vector(format!(
                    "alias `{}` points at `{g}`, which does not exist",
                    self.alias
                )));
            }
            None => {
                return Err(Error::Vector(format!(
                    "alias `{}` does not exist",
                    self.alias
                )));
            }
        };
        let base = format!("/collections/{generation}/snapshots");
        let res = self
            .long_request(Method::POST, &format!("{base}?wait=true"))
            .send()
            .await
            .map_err(|e| Error::Vector(format!("{base}: {e}")))?;
        let status = res.status();
        let text = res.text().await.map_err(|e| Error::Vector(e.to_string()))?;
        if !status.is_success() {
            return Err(Error::Vector(format!(
                "{base}: {}",
                describe_failure(status, &text)
            )));
        }
        let snap: SnapshotDescription = decode(&base, &text)?;
        let one = format!("{base}/{}", snap.name);

        let downloaded = self.download(&one, into).await;
        // Deleted whatever the download did.
        let deleted = self.call::<Value>(Method::DELETE, &one, None).await;
        let (sha256, bytes) = downloaded?;
        deleted?;
        if let Some(theirs) = snap.checksum.filter(|c| *c != sha256) {
            let _ = std::fs::remove_file(into);
            return Err(Error::Vector(format!(
                "{one}: downloaded SHA-256 {sha256} does not match Qdrant's {theirs}"
            )));
        }
        Ok(VectorExport::File {
            points: self.exact_count(&generation).await?,
            dim: self.vector_dim(&generation).await? as usize,
            generation,
            sha256,
            bytes,
        })
    }

    /// The alias is untouched until the caller adopts. A snapshot of another width is dropped before anything points at it.
    async fn import_snapshot(&self, from: &Path, target: &str, dim: usize) -> Result<()> {
        if self.collection_exists(target).await? {
            return Err(Error::Vector(format!(
                "`{target}` already exists; a previous restore did not finish. \
                 Delete it and run this again."
            )));
        }
        let path = format!("/collections/{target}/snapshots/upload?wait=true&priority=snapshot");
        let io = |e: std::io::Error| Error::Vector(format!("{}: {e}", from.display()));
        let mut file = tokio::fs::File::open(from).await.map_err(io)?;
        let len = file.metadata().await.map_err(io)?.len();
        // One buffer in flight, whatever the size of the collection.
        let body = reqwest::Body::wrap_stream(async_stream::stream! {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match file.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => yield Ok::<Vec<u8>, std::io::Error>(buf[..n].to_vec()),
                    Err(e) => {
                        yield Err(e);
                        break;
                    }
                }
            }
        });
        let form = reqwest::multipart::Form::new().part(
            "snapshot",
            reqwest::multipart::Part::stream_with_length(body, len).file_name("snapshot"),
        );
        let res = self
            .long_request(Method::POST, &path)
            .multipart(form)
            .send()
            .await
            .map_err(|e| Error::Vector(format!("{path}: {e}")))?;
        let status = res.status();
        if !status.is_success() {
            let text = res.text().await.unwrap_or_default();
            return Err(Error::Vector(format!(
                "{path}: {}",
                describe_failure(status, &text)
            )));
        }
        let made = async {
            let existing = self.await_readable(target).await?;
            if existing as usize != dim {
                return Err(dimension_mismatch(target, dim, existing as usize));
            }
            self.ensure_payload_indexes(target).await
        }
        .await;
        if made.is_err() {
            let _ = self.drop_generation(target).await;
        }
        made
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasState {
    Absent,
    Dangling(String),
    Live {
        generation: String,
        dim: usize,
    },
}

#[derive(Deserialize)]
struct SnapshotDescription {
    name: String,
    #[serde(default)]
    checksum: Option<String>,
}

/// The ordinary timeout is sized for one search.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(60 * 60);

impl QdrantVectors {
    /// One past the newest existing or named, so a rerun goes one further instead of refusing.
    pub async fn next_generation_name(&self) -> Result<String> {
        let mut n = match self.newest_generation().await? {
            Some(g) => generation_of(&self.alias, &g),
            None => 0,
        };
        if let Some(current) = self.resolve_alias().await? {
            n = n.max(generation_of(&self.alias, &current));
        }
        Ok(generation_name(&self.alias, n + 1))
    }

    pub async fn alias_state(&self) -> Result<AliasState> {
        Ok(match self.resolve_alias().await? {
            None => AliasState::Absent,
            Some(g) if !self.collection_exists(&g).await? => AliasState::Dangling(g),
            Some(g) => AliasState::Live {
                dim: self.vector_dim(&g).await? as usize,
                generation: g,
            },
        })
    }

    pub async fn create_empty_generation(&self, name: &str, dim: usize) -> Result<()> {
        if self.collection_exists(name).await? {
            return Err(Error::Vector(format!(
                "`{name}` already exists; a previous restore did not finish. \
                 Delete it and run this again."
            )));
        }
        self.create_generation(name, dim).await
    }

    pub async fn generation_count(&self, name: &str) -> Result<u64> {
        self.exact_count(name).await
    }

    /// Refuses the served one and anything not a generation of this alias.
    pub async fn drop_generation(&self, name: &str) -> Result<()> {
        if generation_number(&self.alias, name).is_none() {
            return Err(Error::Vector(format!(
                "`{name}` is not a generation of `{}`",
                self.alias
            )));
        }
        if self.resolve_alias().await?.as_deref() == Some(name) {
            return Err(Error::Vector(format!(
                "`{name}` is what `{}` serves; it is not dropped",
                self.alias
            )));
        }
        let _: Value = self
            .call(Method::DELETE, &format!("/collections/{name}"), None)
            .await?;
        Ok(())
    }

    /// The previous generation is left in place: it is the rollback.
    pub async fn adopt_generation(&self, name: &str) -> Result<()> {
        let replacing = self.resolve_alias().await?.is_some();
        self.point_alias_at(name, replacing).await
    }

    fn long_request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .timeout(SNAPSHOT_TIMEOUT);
        if let Some(k) = &self.api_key {
            req = req.header("api-key", k);
        }
        req
    }

    /// One chunk in memory at a time.
    async fn download(&self, path: &str, into: &Path) -> Result<(String, u64)> {
        use sha2::Digest;
        use tokio::io::AsyncWriteExt;
        let mut res = self
            .long_request(Method::GET, path)
            .send()
            .await
            .map_err(|e| Error::Vector(format!("{path}: {e}")))?;
        let status = res.status();
        if !status.is_success() {
            let text = res.text().await.unwrap_or_default();
            return Err(Error::Vector(format!(
                "{path}: {}",
                describe_failure(status, &text)
            )));
        }
        // `create_private` refuses an existing file, so only a file this call made is removed.
        let mut file = tokio::fs::File::from_std(crate::backup::create_private(into)?);
        let streamed = async {
            let mut hasher = sha2::Sha256::new();
            let mut bytes = 0u64;
            let io = |e: std::io::Error| Error::Store(format!("{}: {e}", into.display()));
            while let Some(chunk) = res
                .chunk()
                .await
                .map_err(|e| Error::Vector(format!("{path}: {e}")))?
            {
                hasher.update(&chunk);
                bytes += chunk.len() as u64;
                file.write_all(&chunk).await.map_err(io)?;
            }
            file.sync_all().await.map_err(io)?;
            Ok((hex::encode(hasher.finalize()), bytes))
        }
        .await;
        if streamed.is_err() {
            let _ = std::fs::remove_file(into);
        }
        streamed
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
        assert_eq!(body["vectors"][CTX]["hnsw_config"]["m"], 0);
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
        let stored = json!({ DENSE: [1.0, 2.0], CTX: [[0.5, 0.5, 0.5]] });
        assert!(ctx_of(&stored).is_none());
    }

    #[test]
    fn a_point_with_no_context_set_reindexes_without_one() {
        assert!(ctx_of(&json!({ DENSE: [1.0, 2.0] })).is_none());
        assert!(ctx_of(&json!([1.0, 2.0])).is_none());
        // An empty set is the same as none.
        assert!(ctx_of(&json!({ DENSE: [1.0], CTX: [] })).is_none());
    }

    #[test]
    fn the_offer_excludes_hidden_artifacts_by_must_not() {
        let f = build_filter(&SearchFilter::default()).unwrap();
        assert!(f.get("must").is_none());
        let not = f["must_not"].as_array().unwrap();
        assert_eq!(not.len(), 3);
    }

    #[tokio::test]
    async fn neighbours_reports_an_unreachable_store_rather_than_no_neighbours() {
        // `jobs::relate` reads "no neighbours" as a permanent verdict, so a down store must error.
        let v = QdrantVectors::connect(&VectorConfig {
            url: "http://127.0.0.1:1".into(),
            collection: "engram".into(),
            api_key: None,
            recency_weight: 0.05,
            recency_half_life_days: 180,
            pinned_boost: 0.15,
            weak_below: 0.35,
            per_source_cap: 3,
            candidate_multiplier: 3,
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
        // A rebuild copies payloads it does not understand; dropping one loses data.
        assert_eq!(new["text"], "t");
        assert_eq!(new["created_at"], 1);
    }

    #[test]
    fn renaming_a_current_payload_changes_nothing() {
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
        // A rebuild copies vectors, so suggesting it for a width change sends the reader in a circle.
        let msg = dimension_mismatch("chunks_v1", 768, 1024).to_string();
        assert!(msg.contains("cannot change their width"), "{msg}");
        assert!(msg.contains("embed again"), "no way forward offered: {msg}");
    }

    #[test]
    fn ctx_dimension_mismatch_names_the_repair_that_exists() {
        // The reader must tell this repairable case from the dense one by the text alone.
        let msg = ctx_dimension_mismatch("engram_v1", 45, Some(53)).to_string();
        assert!(msg.contains("45"), "{msg}");
        assert!(msg.contains("53"), "{msg}");
        assert!(msg.contains("--reindex"), "no way forward offered: {msg}");
    }

    #[test]
    fn a_collection_predating_context_vectors_reads_as_a_mismatch() {
        // `None` fails writes the same way and is fixed by the same rebuild.
        let msg = ctx_dimension_mismatch("engram_v1", 45, None).to_string();
        assert!(msg.contains("no `ctx` vector at all"), "{msg}");
        assert!(msg.contains("--reindex"), "{msg}");
    }

    #[tokio::test]
    async fn a_collection_with_the_old_context_width_stops_startup() {
        // The dense width alone passes a collection built under the previous `BLOCKS`.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/aliases"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "aliases": [
                    { "alias_name": "engram", "collection_name": "engram_v1" }
                ] }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/collections/engram_v1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "config": { "params": { "vectors": {
                    DENSE: { "size": 768 },
                    CTX: { "size": 53 },
                } } } }
            })))
            .mount(&server)
            .await;

        let err = against(&server)
            .await
            .ensure_collection(768)
            .await
            .expect_err("the dense width matching is not enough to serve on");
        let msg = err.to_string();
        assert!(msg.contains("53"), "{msg}");
        assert!(msg.contains("--reindex"), "{msg}");
    }

    #[tokio::test]
    async fn a_collection_at_the_current_context_width_starts() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/aliases"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "aliases": [
                    { "alias_name": "engram", "collection_name": "engram_v1" }
                ] }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/collections/engram_v1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "config": { "params": { "vectors": {
                    DENSE: { "size": 768 },
                    CTX: { "size": crate::core::context::CTX_DIM },
                } } }, "payload_schema": {} }
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/collections/engram_v1/index"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "status": "completed" }, "status": "ok"
            })))
            .mount(&server)
            .await;

        against(&server)
            .await
            .ensure_collection(768)
            .await
            .expect("a collection at the current width is the ordinary case");
    }

    #[test]
    fn chunk_ids_map_to_stable_point_ids() {
        // Otherwise re-embedding leaves an orphaned vector.
        let a = point_uuid("chunk-abc");
        let b = point_uuid("chunk-abc");
        let c = point_uuid("chunk-xyz");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn uuid_chunk_ids_pass_through_unchanged() {
        // Hashing UUIDv7 ids would waste their time-ordering.
        let id = uuid::Uuid::now_v7().to_string();
        assert_eq!(point_uuid(&id), id);
    }

    #[test]
    fn hashed_point_ids_are_valid_uuids() {
        let got = point_uuid("not-a-uuid");
        assert!(uuid::Uuid::parse_str(&got).is_ok(), "got {got}");
    }

    #[test]
    fn base_url_loses_its_trailing_slash() {
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
    fn the_widest_filter_still_keeps_trash_out_and_sends_no_empty_must() {
        // `{"must": []}` matches nothing in Qdrant.
        let f = build_filter(&SearchFilter {
            include_superseded: true,
            include_deprecated: true,
            ..Default::default()
        })
        .unwrap();
        assert!(f.get("must").is_none(), "{f}");
        assert_eq!(
            f["must_not"],
            json!([{ "key": "status", "match": { "value": "trashed" } }])
        );
    }

    #[test]
    fn the_default_filter_excludes_superseded_and_deprecated_points() {
        let f = build_filter(&SearchFilter::default()).unwrap();
        assert!(
            f.get("must").is_none(),
            "an empty condition list must not be sent: {f}"
        );
        assert_eq!(
            f["must_not"],
            json!([
                { "key": "status", "match": { "value": "trashed" } },
                { "key": "status", "match": { "value": "superseded" } },
                { "key": "status", "match": { "value": "deprecated" } },
            ]),
        );
    }

    #[test]
    fn every_tag_becomes_its_own_must_condition() {
        // One condition per tag makes AND; a list in one condition is OR.
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
        // `drop_collection` deletes everything this claims.
        assert_eq!(generation_number("chunks", "chunks_v3"), Some(3));
        assert_eq!(generation_number("chunks", "chunks_verbose"), None);
        assert_eq!(generation_number("chunks", "chunks_vault"), None);
        assert_eq!(generation_number("chunks", "chunks_v"), None);
        assert_eq!(generation_number("chunks", "chunks_v1x"), None);
        assert_eq!(generation_number("chunks", "chunks"), None);
        assert_eq!(generation_number("chunks", "chunks_v+1"), None);
        assert_eq!(generation_number("chunks", "chunks_v1_0"), None);
    }

    #[test]
    fn the_scoring_formula_survives_a_point_without_a_last_verified_at() {
        // One point missing the key fails the entire query.
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
        // An operator turning recency off stops paying for it.
        let f = scoring_formula(0, 86_400, 0.0, 0.0);
        assert_eq!(f["formula"]["sum"].as_array().unwrap().len(), 1);
        assert_eq!(f["formula"]["sum"][0], "$score");
    }

    #[test]
    fn new_collections_declare_both_a_dense_and_a_sparse_vector() {
        let body = collection_body(768);
        assert_eq!(body["vectors"][DENSE]["size"], 768);
        assert_eq!(body["vectors"][DENSE]["distance"], "Cosine");
        assert_eq!(body["sparse_vectors"][SPARSE]["modifier"], "idf");
    }

    #[test]
    fn a_dense_vector_is_found_in_either_storage_layout() {
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

    #[tokio::test]
    async fn dense_of_reads_one_point_by_id_and_none_when_it_is_absent() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let id = point_uuid("a1");
        Mock::given(method("POST"))
            .and(path("/collections/engram/points"))
            .and(body_partial_json(json!({ "ids": [id] })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": [ { "id": id, "vector": { DENSE: [0.1, 0.2, 0.3] } } ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/collections/engram/points"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "result": [] })))
            .mount(&server)
            .await;

        let v = against(&server).await;
        assert_eq!(v.dense_of("a1").await.unwrap(), Some(vec![0.1, 0.2, 0.3]));
        assert_eq!(v.dense_of("nope").await.unwrap(), None);
    }

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
            candidate_multiplier: 3,
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn the_half_life_a_search_is_handed_is_the_one_the_formula_decays_over() {
        // Asserts the request Qdrant actually sees.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let decays_over_thirty_days = |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            body["searches"][0]["query"]["formula"]["sum"][1]["mult"][1]["exp_decay"]["scale"]
                == json!(30 * SECONDS_PER_DAY)
        };
        Mock::given(method("POST"))
            .and(path("/collections/engram/points/query/batch"))
            .and(decays_over_thirty_days)
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": [ { "points": [] }, { "points": [] } ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        against(&server)
            .await
            .search_weighted(
                &[1.0, 0.0, 0.0],
                &Default::default(),
                10,
                &SearchFilter::default(),
                Recency {
                    weight: 0.1,
                    half_life_days: 30,
                },
            )
            .await
            .expect("the request carried the half-life it was handed");
    }

    #[tokio::test]
    async fn clearing_a_context_set_names_the_vector_the_way_the_delete_takes_it() {
        // Delete requires `vectors`; the update's `vector` makes it 400, which killed the sweep.
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
        // `merge::reap_stranded` retires merges that never got a point.
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
        // A dangling alias affects every artifact and must not read as a missing point.
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
