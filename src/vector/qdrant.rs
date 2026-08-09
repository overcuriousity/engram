use super::{SearchFilter, SearchHit, VectorPayload, VectorPoint, VectorStore};
use crate::config::VectorConfig;
use crate::error::{Error, Result};
use async_trait::async_trait;
use qdrant_client::Qdrant;
use qdrant_client::qdrant::{
    Condition, CreateCollectionBuilder, CreateFieldIndexCollectionBuilder, DeletePointsBuilder,
    Distance, FieldType, Filter, PointStruct, QueryPointsBuilder, UpsertPointsBuilder,
    VectorParamsBuilder,
};

pub struct QdrantVectors {
    client: Qdrant,
    collection: String,
}

pub fn dimension_mismatch(collection: &str, configured: usize, existing: usize) -> Error {
    Error::Vector(format!(
        "collection `{collection}` has vector dimension {existing}, but config says {configured}. \
         Refusing to start: writing mismatched vectors would corrupt search results. \
         Either restore the original embedding model, or delete the collection and re-embed \
         every chunk."
    ))
}

/// Qdrant point ids must be an unsigned integer or a UUID. Chunk ids are
/// already UUIDv7 strings, so they pass through; anything else is hashed into
/// a deterministic UUID so the mapping stays stable across restarts.
pub fn point_uuid(chunk_id: &str) -> String {
    match uuid::Uuid::parse_str(chunk_id) {
        Ok(u) => u.to_string(),
        Err(_) => {
            let digest = <sha2::Sha256 as sha2::Digest>::digest(chunk_id.as_bytes());
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&digest[..16]);
            uuid::Uuid::from_bytes(bytes).to_string()
        }
    }
}

impl QdrantVectors {
    pub async fn connect(cfg: &VectorConfig) -> Result<QdrantVectors> {
        let mut builder = Qdrant::from_url(&cfg.url);
        if let Some(k) = &cfg.api_key {
            builder = builder.api_key(k.clone());
        }
        let client = builder.build().map_err(|e| Error::Vector(e.to_string()))?;
        Ok(QdrantVectors {
            client,
            collection: cfg.collection.clone(),
        })
    }

    /// Drop the collection. Used by the integration suite to start clean.
    pub async fn drop_collection(&self) -> Result<()> {
        let _ = self.client.delete_collection(&self.collection).await;
        Ok(())
    }
}

#[async_trait]
impl VectorStore for QdrantVectors {
    async fn ensure_collection(&self, dim: usize) -> Result<()> {
        let exists = self
            .client
            .collection_exists(&self.collection)
            .await
            .map_err(|e| Error::Vector(e.to_string()))?;

        if !exists {
            self.client
                .create_collection(
                    CreateCollectionBuilder::new(&self.collection)
                        .vectors_config(VectorParamsBuilder::new(dim as u64, Distance::Cosine)),
                )
                .await
                .map_err(|e| Error::Vector(e.to_string()))?;

            // Payload indexes: without these, filtered search degrades to a
            // full scan of the collection.
            for (field, kind) in [
                ("tags", FieldType::Keyword),
                ("category", FieldType::Keyword),
                ("source_id", FieldType::Keyword),
                ("created_at", FieldType::Integer),
            ] {
                self.client
                    .create_field_index(CreateFieldIndexCollectionBuilder::new(
                        &self.collection,
                        field,
                        kind,
                    ))
                    .await
                    .map_err(|e| Error::Vector(format!("payload index on {field}: {e}")))?;
            }
            tracing::info!(collection = %self.collection, dim, "created qdrant collection");
            return Ok(());
        }

        let info = self
            .client
            .collection_info(&self.collection)
            .await
            .map_err(|e| Error::Vector(e.to_string()))?;
        let existing_dim = info
            .result
            .and_then(|r| r.config)
            .and_then(|c| c.params)
            .and_then(|p| p.vectors_config)
            .and_then(|vc| vc.config)
            .and_then(|c| match c {
                qdrant_client::qdrant::vectors_config::Config::Params(p) => Some(p.size),
                _ => None,
            })
            .ok_or_else(|| Error::Vector("could not read collection vector dimension".into()))?;

        if existing_dim as usize != dim {
            return Err(dimension_mismatch(
                &self.collection,
                dim,
                existing_dim as usize,
            ));
        }
        Ok(())
    }

    async fn upsert(&self, points: Vec<VectorPoint>) -> Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        let mut structs = Vec::with_capacity(points.len());
        for p in points {
            let value =
                serde_json::to_value(&p.payload).map_err(|e| Error::Vector(e.to_string()))?;
            let payload: qdrant_client::Payload = value
                .try_into()
                .map_err(|e| Error::Vector(format!("payload conversion: {e}")))?;
            structs.push(PointStruct::new(
                point_uuid(&p.payload.chunk_id),
                p.vector,
                payload,
            ));
        }

        self.client
            .upsert_points(UpsertPointsBuilder::new(&self.collection, structs).wait(true))
            .await
            .map_err(|e| Error::Vector(e.to_string()))?;
        Ok(())
    }

    async fn search(
        &self,
        vector: &[f32],
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>> {
        let mut q = QueryPointsBuilder::new(&self.collection)
            .query(vector.to_vec())
            .limit(limit as u64)
            .with_payload(true);

        if !filter.is_empty() {
            let mut conds: Vec<Condition> = Vec::new();
            for t in &filter.tags {
                conds.push(Condition::matches("tags", t.clone()));
            }
            if let Some(c) = &filter.category {
                conds.push(Condition::matches("category", c.clone()));
            }
            q = q.filter(Filter::must(conds));
        }

        let res = self
            .client
            .query(q.build())
            .await
            .map_err(|e| Error::Vector(e.to_string()))?;

        let mut out = Vec::new();
        for p in res.result {
            let map: serde_json::Map<String, serde_json::Value> = p
                .payload
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::from(v)))
                .collect();
            let payload: VectorPayload = serde_json::from_value(serde_json::Value::Object(map))
                .map_err(|e| Error::Vector(format!("payload did not match schema: {e}")))?;
            out.push(SearchHit {
                payload,
                score: p.score,
            });
        }
        Ok(out)
    }

    async fn delete_chunks(&self, chunk_ids: &[String]) -> Result<()> {
        if chunk_ids.is_empty() {
            return Ok(());
        }
        let ids: Vec<qdrant_client::qdrant::PointId> =
            chunk_ids.iter().map(|c| point_uuid(c).into()).collect();
        self.client
            .delete_points(
                DeletePointsBuilder::new(&self.collection)
                    .points(ids)
                    .wait(true),
            )
            .await
            .map_err(|e| Error::Vector(e.to_string()))?;
        Ok(())
    }

    async fn delete_by_source(&self, source_id: &str) -> Result<()> {
        self.client
            .delete_points(
                DeletePointsBuilder::new(&self.collection)
                    .points(Filter::must([Condition::matches(
                        "source_id",
                        source_id.to_string(),
                    )]))
                    .wait(true),
            )
            .await
            .map_err(|e| Error::Vector(e.to_string()))?;
        Ok(())
    }

    async fn count(&self) -> Result<u64> {
        let info = self
            .client
            .collection_info(&self.collection)
            .await
            .map_err(|e| Error::Vector(e.to_string()))?;
        Ok(info.result.and_then(|r| r.points_count).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimension_mismatch_message_names_both_numbers() {
        let e = dimension_mismatch("chunks", 768, 1024);
        let msg = e.to_string();
        assert!(msg.contains("768"), "{msg}");
        assert!(msg.contains("1024"), "{msg}");
        assert!(msg.contains("chunks"), "{msg}");
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
}
