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

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub const DENSE: &str = "dense";

pub const SPARSE: &str = "text";

const REINDEX_BATCH: usize = 256;

const READY_ATTEMPTS: u32 = 5;
const READY_BACKOFF: Duration = Duration::from_millis(200);

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

fn legacy_layout(alias: &str) -> Error {
    Error::Vector(format!(
        "`{alias}` is a plain collection, but engram now addresses vectors through an alias of \
         that name, and Qdrant will not let the two share it. Run \
         `engram --reindex --replace-legacy` to copy every point into `{alias}_v1`, check that \
         nothing was lost, and only then delete `{alias}` and create the alias. No re-embedding \
         is needed. Take a Qdrant snapshot first if you want a way back."
    ))
}

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

fn normalize_base(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

fn generation_name(alias: &str, n: u32) -> String {
    format!("{alias}_v{n}")
}

fn generation_number(alias: &str, collection: &str) -> Option<u32> {
    collection
        .strip_prefix(&format!("{alias}_v"))
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse().ok())
}

fn generation_of(alias: &str, collection: &str) -> u32 {
    generation_number(alias, collection).unwrap_or(0)
}

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

    let mut body = json!({});
    if !must.is_empty() {
        body["must"] = json!(must);
    }
    let mut must_not: Vec<Value> = Vec::new();
    if !filter.include_superseded {
        must_not.push(json!({ "key": "status", "match": { "value": "superseded" } }));
        must_not.push(json!({ "key": "superseded", "match": { "value": true } }));
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
        "superseded": status == ArtifactStatus::Superseded,
        "superseded_by": superseded_by,
    })
}

fn stored_status(payload: &Value) -> ArtifactStatus {
    match payload.get("status").and_then(Value::as_str) {
        Some(s) => ArtifactStatus::parse(s),
        None if payload.get("superseded").and_then(Value::as_bool) == Some(true) => {
            ArtifactStatus::Superseded
        }
        None => ArtifactStatus::Active,
    }
}

fn verified_payload(at: i64, reset_hits: bool) -> Value {
    let mut p = json!({ "last_verified_at": at });
    if reset_hits {
        p["hit_count"] = json!(0);
    }
    p
}

#[derive(Deserialize)]
struct MatrixPairs {
    pairs: Vec<MatrixPair>,
}

#[derive(Deserialize)]
struct MatrixPair {
    a: Value,
    b: Value,
    score: f32,
}

fn collection_body(dim: usize) -> Value {
    json!({
        "vectors": { DENSE: { "size": dim, "distance": "Cosine" } },
        "sparse_vectors": { SPARSE: { "modifier": "idf" } },
    })
}

fn sparse_body(sparse: &SparseVector) -> Option<Value> {
    if sparse.is_empty() {
        return None;
    }
    Some(json!({ "indices": sparse.indices, "values": sparse.values }))
}

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

fn sparse_of_payload(payload: &Value) -> SparseVector {
    let text = payload.get("text").and_then(Value::as_str).unwrap_or("");
    match payload.get("title").and_then(Value::as_str) {
        Some(t) if !t.is_empty() => super::sparse::encode_document(&format!("{t}\n{text}")),
        _ => super::sparse::encode_document(text),
    }
}

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
        terms.push(json!({
            "mult": [pinned, { "key": "tags", "match": { "value": PINNED_TAG } }]
        }));
    }
    json!({
        "formula": { "sum": terms },
        "defaults": { "last_verified_at": now },
    })
}

fn dense_of(vector: &Value) -> Option<&Value> {
    match vector {
        Value::Array(_) => Some(vector),
        Value::Object(m) => m.get(DENSE),
        _ => None,
    }
}

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

#[derive(Debug, Clone, Default)]
struct StoredBookkeeping {
    last_seen_at: Option<i64>,
    hit_count: Option<i64>,
    superseded: Option<bool>,
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
        let list: AliasList = self.call(Method::GET, "/aliases", None).await?;
        Ok(list
            .aliases
            .into_iter()
            .find(|a| a.alias_name == self.alias)
            .map(|a| a.collection_name))
    }

    async fn generations(&self) -> Result<Vec<String>> {
        let list: CollectionList = self.call(Method::GET, "/collections", None).await?;
        Ok(list
            .collections
            .into_iter()
            .map(|c| c.name)
            .filter(|n| *n == self.alias || generation_number(&self.alias, n).is_some())
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
        info.pointer(&format!("/config/params/vectors/{DENSE}/size"))
            .or_else(|| info.pointer("/config/params/vectors/size"))
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::Vector("could not read collection vector dimension".into()))
    }

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

    async fn stored_bookkeeping(
        &self,
        points: &[VectorPoint],
    ) -> Result<std::collections::HashMap<String, StoredBookkeeping>> {
        let wanted: Vec<&VectorPoint> = points
            .iter()
            .filter(|p| {
                p.payload.last_seen_at.is_none()
                    || p.payload.hit_count.is_none()
                    || p.payload.superseded.is_none()
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
                        "last_seen_at", "hit_count", "superseded",
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
                    superseded: p.payload.get("superseded").and_then(Value::as_bool),
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

    pub async fn reindex(&self, dim: usize, replace_legacy: bool) -> Result<String> {
        let source = match self.resolve_alias().await? {
            Some(c) => c,
            None if self.collection_exists(&self.alias).await? => self.alias.clone(),
            None => {
                return Err(Error::Vector(format!(
                    "nothing to reindex: neither an alias nor a collection named `{}` exists",
                    self.alias
                )));
            }
        };
        let replacing = source != self.alias;

        if !replacing && !replace_legacy {
            return Err(legacy_layout(&self.alias));
        }
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
                let mut vector = json!({ DENSE: dense });
                if let Some(sp) = sparse_body(&sparse_of_payload(&p.payload)) {
                    vector[SPARSE] = sp;
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

        if replacing {
            self.point_alias_at(&target, true).await?;
            tracing::info!(
                copied,
                previous = %source,
                current = %target,
                "reindex complete; the previous generation was left in place"
            );
        } else {
            let (before, after) = (
                self.exact_count(&source).await?,
                self.exact_count(&target).await?,
            );
            if before != after {
                return Err(Error::Vector(format!(
                    "refusing to delete `{source}`: it holds {before} points but `{target}` \
                     received {after}. The copy is in `{target}`; nothing was deleted."
                )));
            }
            let _: Value = self
                .call(Method::DELETE, &format!("/collections/{source}"), None)
                .await?;
            self.point_alias_at(&target, false).await?;
            tracing::warn!(
                copied,
                deleted = %source,
                current = %target,
                "reindex complete; the pre-alias collection was deleted to free its name"
            );
        }
        Ok(target)
    }
}

fn hits_of(res: QueryResult) -> Vec<SearchHit> {
    let mut out = Vec::with_capacity(res.points.len());
    for p in res.points {
        match serde_json::from_value::<VectorPayload>(p.payload) {
            Ok(payload) => out.push(SearchHit {
                payload,
                score: p.score,
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

        if !status.is_success() {
            return Err(Error::Vector(format!(
                "{path}: {}",
                describe_failure(status, &text)
            )));
        }

        let env: Envelope<T> = serde_json::from_str(&text)
            .map_err(|e| Error::Vector(format!("unreadable response from {path}: {e}")))?;
        env.result
            .ok_or_else(|| Error::Vector(format!("{path} returned no result")))
    }
}

#[async_trait]
impl VectorStore for QdrantVectors {
    async fn ensure_collection(&self, dim: usize) -> Result<()> {
        if let Some(current) = self.resolve_alias().await? {
            let existing = self.vector_dim(&current).await?;
            if existing as usize != dim {
                return Err(dimension_mismatch(&current, dim, existing as usize));
            }
            self.ensure_payload_indexes(&current).await?;
            return Ok(());
        }

        if self.collection_exists(&self.alias).await? {
            return Err(legacy_layout(&self.alias));
        }

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
            if payload.superseded.is_none() {
                payload.superseded = old.and_then(|s| s.superseded);
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

    async fn set_superseded(&self, artifact_id: &str, superseded: bool) -> Result<()> {
        let _: Value = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/payload?wait=true", self.alias),
                Some(json!({
                    "payload": { "superseded": superseded },
                    "points": [ point_uuid(artifact_id) ],
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
        let _: Value = self
            .call(
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

    async fn unstamped_count(&self) -> Result<u64> {
        let res: CountResult = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/count", self.alias),
                Some(json!({
                    "exact": true,
                    "filter": {
                        "must": [ { "is_empty": { "key": "last_verified_at" } } ],
                        "must_not": [ { "is_empty": { "key": "artifact_id" } } ],
                    },
                })),
            )
            .await?;
        Ok(res.count)
    }

    async fn non_active_ids(&self, limit: usize) -> Result<Vec<String>> {
        let page: ScrollResult = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/scroll", self.alias),
                Some(json!({
                    "filter": { "should": [
                        { "key": "status", "match": { "value": "deprecated" } },
                        { "key": "status", "match": { "value": "superseded" } },
                        { "key": "superseded", "match": { "value": true } },
                    ] },
                    "limit": limit,
                    "with_payload": ["artifact_id"],
                    "with_vector": false,
                })),
            )
            .await?;
        Ok(page
            .points
            .iter()
            .filter_map(|p| {
                p.payload
                    .get("artifact_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect())
    }

    async fn payloads_of(
        &self,
        artifact_ids: &[String],
    ) -> Result<std::collections::HashMap<String, VectorPayload>> {
        let mut out = std::collections::HashMap::new();
        if artifact_ids.is_empty() {
            return Ok(out);
        }
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
        const BATCH: usize = 512;
        for batch in artifact_ids.chunks(BATCH) {
            let ids: Vec<String> = batch.iter().map(|id| point_uuid(id)).collect();
            let found: Vec<ScrolledPoint> = self
                .call(
                    Method::POST,
                    &format!("/collections/{}/points", self.alias),
                    Some(json!({
                        "ids": ids,
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
                { "key": "status", "match": { "value": "superseded" } },
                { "key": "superseded", "match": { "value": true } },
            ],
            "must": [
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
                score: 0.0,
                similarity: None,
            })
            .collect())
    }

    async fn search(
        &self,
        vector: &[f32],
        sparse: &SparseVector,
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>> {
        let f = build_filter(filter);

        let mut body = match sparse_body(sparse) {
            Some(terms) => {
                let mut dense_branch = json!({ "query": vector, "using": DENSE, "limit": limit });
                let mut sparse_branch = json!({ "query": terms, "using": SPARSE, "limit": limit });
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

        if self.recency_weight > 0.0 || self.pinned_boost > 0.0 {
            let mut prefetch = std::mem::replace(&mut body, Value::Null);
            if let Some(m) = prefetch.as_object_mut() {
                m.remove("with_payload");
            }
            body = json!({
                "prefetch": [prefetch],
                "query": scoring_formula(
                    now_secs(),
                    self.recency_half_life_days as u64 * SECONDS_PER_DAY,
                    self.recency_weight,
                    self.pinned_boost,
                ),
                "limit": limit,
                "with_payload": true,
            });
        }

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
                    "filter": {
                      "must_not": [
                        { "key": "status", "match": { "value": "superseded" } },
                        { "key": "status", "match": { "value": "deprecated" } },
                        { "key": "superseded", "match": { "value": true } }
                      ],
                      "must": [
                        { "key": "created_at", "range": { "lt": older_than } },
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
            tags: self.facet("tags", limit).await?,
        })
    }

    async fn neighbours(&self, artifact_id: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let body = json!({
            "query": point_uuid(artifact_id),
            "using": DENSE,
            "limit": limit + 1,
            "filter": { "must_not": [
                { "key": "status", "match": { "value": "superseded" } },
                { "key": "status", "match": { "value": "deprecated" } },
                { "key": "superseded", "match": { "value": true } }
            ] },
            "with_payload": true,
        });
        let res: QueryResult = match self
            .call(
                Method::POST,
                &format!("/collections/{}/points/query", self.alias),
                Some(body),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(artifact_id, error = %e, "no neighbours for this artifact");
                return Ok(vec![]);
            }
        };
        let mut hits = hits_of(res);
        hits.retain(|h| h.payload.artifact_id != artifact_id);
        hits.truncate(limit);
        Ok(hits)
    }

    async fn near_pairs(
        &self,
        sample: usize,
        per_point: usize,
        min_score: f32,
    ) -> Result<Vec<super::NearPair>> {
        let res: MatrixPairs = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/search/matrix/pairs", self.alias),
                Some(json!({
                    "sample": sample,
                    "limit": per_point,
                    "using": DENSE,
                    "filter": { "must_not": [
                        { "key": "status", "match": { "value": "superseded" } },
                        { "key": "status", "match": { "value": "deprecated" } },
                        { "key": "superseded", "match": { "value": true } }
                    ] },
                })),
            )
            .await?;

        let mut ids: Vec<&str> = Vec::new();
        for p in &res.pairs {
            if p.score < min_score {
                continue;
            }
            let (Some(a), Some(b)) = (p.a.as_str(), p.b.as_str()) else {
                continue;
            };
            ids.push(a);
            ids.push(b);
        }
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        ids.sort_unstable();
        ids.dedup();

        let found: Vec<ScrolledPoint> = self
            .call(
                Method::POST,
                &format!("/collections/{}/points", self.alias),
                Some(json!({ "ids": ids, "with_payload": ["artifact_id"], "with_vector": false })),
            )
            .await?;
        let mut by_uuid: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for p in &found {
            let (Some(uuid), Some(aid)) = (
                p.id.as_str(),
                p.payload.get("artifact_id").and_then(Value::as_str),
            ) else {
                continue;
            };
            by_uuid.insert(uuid.to_string(), aid.to_string());
        }

        let mut out: Vec<super::NearPair> = res
            .pairs
            .iter()
            .filter(|p| p.score >= min_score)
            .filter_map(|p| {
                let a = by_uuid.get(p.a.as_str()?)?;
                let b = by_uuid.get(p.b.as_str()?)?;
                (a != b).then(|| super::NearPair::new(a, b, p.score))
            })
            .collect();
        out.sort_by(|x, y| {
            y.score
                .total_cmp(&x.score)
                .then_with(|| x.a.cmp(&y.a))
                .then_with(|| x.b.cmp(&y.b))
        });
        out.dedup_by(|x, y| x.a == y.a && x.b == y.b);
        Ok(out)
    }

    async fn count(&self) -> Result<u64> {
        self.exact_count(&self.alias).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let msg = dimension_mismatch("chunks_v1", 768, 1024).to_string();
        assert!(msg.contains("cannot change their width"), "{msg}");
        assert!(msg.contains("embed again"), "no way forward offered: {msg}");
    }

    #[test]
    fn chunk_ids_map_to_stable_point_ids() {
        let a = point_uuid("chunk-abc");
        let b = point_uuid("chunk-abc");
        let c = point_uuid("chunk-xyz");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn uuid_chunk_ids_pass_through_unchanged() {
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
    fn matrix_pairs_deserialise_from_qdrant_shape() {
        let res: MatrixPairs = serde_json::from_value(json!({
            "pairs": [ { "a": 1, "b": 2, "score": 0.97 } ]
        }))
        .unwrap();
        assert_eq!(res.pairs.len(), 1);
        assert!((res.pairs[0].score - 0.97).abs() < 1e-6);
    }

    #[test]
    fn a_pair_is_canonically_ordered() {
        assert_eq!(
            super::super::NearPair::new("z", "a", 0.9),
            super::super::NearPair::new("a", "z", 0.9)
        );
    }

    #[test]
    fn a_filter_that_narrows_nothing_produces_no_filter_at_all() {
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
        let f = build_filter(&SearchFilter::default()).unwrap();
        assert!(
            f.get("must").is_none(),
            "an empty condition list must not be sent: {f}"
        );
        assert_eq!(
            f["must_not"],
            json!([
                { "key": "status", "match": { "value": "superseded" } },
                { "key": "superseded", "match": { "value": true } },
                { "key": "status", "match": { "value": "deprecated" } },
            ]),
        );
    }

    #[test]
    fn every_tag_becomes_its_own_must_condition() {
        let f = build_filter(&SearchFilter {
            tags: vec!["linux".into(), "forensics".into()],
            category: Some("procedure".into()),
            include_superseded: false,
            include_deprecated: false,
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
    fn a_pre_alias_collection_rebuilds_into_generation_one() {
        assert_eq!(generation_of("chunks", "chunks"), 0);
        assert_eq!(
            generation_name("chunks", generation_of("chunks", "chunks") + 1),
            "chunks_v1"
        );
    }

    #[test]
    fn a_collection_whose_suffix_is_not_a_number_does_not_panic() {
        assert_eq!(generation_of("chunks", "chunks_vNEXT"), 0);
        assert_eq!(generation_of("chunks", "something_else"), 0);
    }

    #[test]
    fn only_a_numeric_suffix_makes_a_collection_ours() {
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
        let f = scoring_formula(1_700_000_000, 86_400, 0.05, 0.15);
        assert_eq!(
            f["defaults"]["last_verified_at"], 1_700_000_000,
            "an unstamped payload must score as unknown, not as maximally stale: {f}"
        );
    }

    #[test]
    fn deprecating_does_not_set_the_legacy_superseded_flag() {
        let p = lifecycle_payload(ArtifactStatus::Deprecated, None);
        assert_eq!(p["status"], "deprecated");
        assert_eq!(p["superseded"], false, "deprecated is not superseded: {p}");

        let s = lifecycle_payload(ArtifactStatus::Superseded, Some("winner"));
        assert_eq!(s["superseded"], true);
        assert_eq!(s["superseded_by"], "winner");

        let a = lifecycle_payload(ArtifactStatus::Active, None);
        assert_eq!(a["superseded"], false);
    }

    #[test]
    fn only_a_verify_resets_the_hit_counter() {
        let verified = verified_payload(42, true);
        assert_eq!(verified["last_verified_at"], 42);
        assert_eq!(verified["hit_count"], 0);

        let backfilled = verified_payload(42, false);
        assert_eq!(backfilled["last_verified_at"], 42);
        assert!(
            backfilled.get("hit_count").is_none(),
            "a backfill must leave the counter alone: {backfilled}"
        );
    }

    #[test]
    fn a_disabled_weight_contributes_no_term() {
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

    #[test]
    fn the_legacy_layout_error_offers_a_non_destructive_fix() {
        let msg = legacy_layout("chunks").to_string();
        assert!(msg.contains("--reindex"), "{msg}");
        assert!(msg.contains("chunks_v1"), "{msg}");
    }
}
