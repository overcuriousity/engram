//! Integration tests against a real Qdrant.
//!
//! Requires a running server: `docker compose up -d` (or `podman run -d --name
//! pkdb-qdrant -p 127.0.0.1:6333:6333 -p 127.0.0.1:6334:6334 qdrant/qdrant`).
//!
//! Run with: `cargo test --test integration_qdrant -- --ignored`
//!
//! Override the endpoint with `PKDB_TEST_QDRANT`, e.g.
//! `PKDB_TEST_QDRANT=http://localhost:16334`.

use pkdb::config::VectorConfig;
use pkdb::vector::{SearchFilter, VectorPayload, VectorPoint, VectorStore, qdrant::QdrantVectors};

fn cfg(collection: &str) -> VectorConfig {
    VectorConfig {
        url: std::env::var("PKDB_TEST_QDRANT").unwrap_or_else(|_| "http://localhost:6334".into()),
        collection: collection.to_string(),
        api_key: None,
    }
}

fn point(id: &str, src: &str, v: Vec<f32>, tags: &[&str], cat: &str) -> VectorPoint {
    VectorPoint {
        vector: v,
        payload: VectorPayload {
            chunk_id: id.into(),
            source_id: src.into(),
            text: format!("text {id}"),
            title: Some(id.into()),
            category: Some(cat.into()),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            created_at: 42,
        },
    }
}

/// Each test owns its own collection and drops it first, so a rerun is never
/// polluted by the previous one.
async fn fresh(name: &str, dim: usize) -> QdrantVectors {
    let v = QdrantVectors::connect(&cfg(name)).await.unwrap();
    v.drop_collection().await.unwrap();
    v.ensure_collection(dim).await.unwrap();
    v
}

#[tokio::test]
#[ignore]
async fn upsert_search_and_payload_roundtrip() {
    let v = fresh("pkdb_it_roundtrip", 4).await;
    v.upsert(vec![
        point("a", "s1", vec![1.0, 0.0, 0.0, 0.0], &["linux"], "procedure"),
        point("b", "s1", vec![0.0, 1.0, 0.0, 0.0], &["windows"], "concept"),
    ])
    .await
    .unwrap();

    let hits = v
        .search(&[1.0, 0.0, 0.0, 0.0], 5, &SearchFilter::default())
        .await
        .unwrap();
    assert_eq!(hits[0].payload.chunk_id, "a");
    assert_eq!(hits[0].payload.tags, vec!["linux".to_string()]);
    assert_eq!(
        hits[0].payload.created_at, 42,
        "payload must survive the round trip intact"
    );
    assert_eq!(hits[0].payload.title.as_deref(), Some("a"));
    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn filtered_search_uses_payload_indexes() {
    let v = fresh("pkdb_it_filter", 4).await;
    v.upsert(vec![
        point(
            "a",
            "s1",
            vec![1.0, 0.0, 0.0, 0.0],
            &["linux", "forensics"],
            "procedure",
        ),
        point("b", "s1", vec![1.0, 0.0, 0.0, 0.0], &["linux"], "concept"),
    ])
    .await
    .unwrap();

    let f = SearchFilter {
        tags: vec!["forensics".into()],
        category: None,
    };
    let hits = v.search(&[1.0, 0.0, 0.0, 0.0], 5, &f).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.chunk_id, "a");

    let f = SearchFilter {
        tags: vec![],
        category: Some("concept".into()),
    };
    assert_eq!(
        v.search(&[1.0, 0.0, 0.0, 0.0], 5, &f).await.unwrap()[0]
            .payload
            .chunk_id,
        "b"
    );
    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn multiple_tags_are_an_and_not_an_or() {
    // The in-memory implementation requires every listed tag. Qdrant must
    // agree, or filtered search means different things in tests and production.
    let v = fresh("pkdb_it_tags_and", 4).await;
    v.upsert(vec![
        point(
            "both",
            "s1",
            vec![1.0, 0.0, 0.0, 0.0],
            &["linux", "forensics"],
            "procedure",
        ),
        point(
            "one",
            "s1",
            vec![1.0, 0.0, 0.0, 0.0],
            &["linux"],
            "procedure",
        ),
    ])
    .await
    .unwrap();

    let f = SearchFilter {
        tags: vec!["linux".into(), "forensics".into()],
        category: None,
    };
    let hits = v.search(&[1.0, 0.0, 0.0, 0.0], 5, &f).await.unwrap();
    assert_eq!(hits.len(), 1, "tag filter behaved as OR, not AND");
    assert_eq!(hits[0].payload.chunk_id, "both");
    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn upsert_is_idempotent_per_chunk_id() {
    let v = fresh("pkdb_it_idempotent", 4).await;
    v.upsert(vec![point("a", "s1", vec![1.0, 0.0, 0.0, 0.0], &[], "c")])
        .await
        .unwrap();
    v.upsert(vec![point("a", "s1", vec![0.0, 0.0, 0.0, 1.0], &[], "c")])
        .await
        .unwrap();

    let hits = v
        .search(&[0.0, 0.0, 0.0, 1.0], 5, &SearchFilter::default())
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "re-embedding must overwrite, not duplicate");
    assert!(hits[0].score > 0.99);
    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn delete_by_source_removes_only_that_source() {
    let v = fresh("pkdb_it_delete", 4).await;
    v.upsert(vec![
        point("a", "s1", vec![1.0, 0.0, 0.0, 0.0], &[], "c"),
        point("b", "s2", vec![1.0, 0.0, 0.0, 0.0], &[], "c"),
    ])
    .await
    .unwrap();
    v.delete_by_source("s1").await.unwrap();

    let hits = v
        .search(&[1.0, 0.0, 0.0, 0.0], 5, &SearchFilter::default())
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.source_id, "s2");
    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn delete_chunks_removes_exactly_the_listed_ids() {
    let v = fresh("pkdb_it_delete_chunks", 4).await;
    v.upsert(vec![
        point("a", "s1", vec![1.0, 0.0, 0.0, 0.0], &[], "c"),
        point("b", "s1", vec![1.0, 0.0, 0.0, 0.0], &[], "c"),
    ])
    .await
    .unwrap();
    v.delete_chunks(&["a".to_string()]).await.unwrap();

    let hits = v
        .search(&[1.0, 0.0, 0.0, 0.0], 5, &SearchFilter::default())
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.chunk_id, "b");
    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn a_dimension_change_is_refused_rather_than_silently_accepted() {
    let v = QdrantVectors::connect(&cfg("pkdb_it_dim")).await.unwrap();
    v.drop_collection().await.unwrap();
    v.ensure_collection(4).await.unwrap();

    let err = v.ensure_collection(8).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains('4') && msg.contains('8'), "unhelpful: {msg}");
    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn ensure_collection_is_idempotent_at_the_same_dimension() {
    // Every restart calls this; it must not fail or recreate the collection.
    let v = fresh("pkdb_it_idem_ensure", 4).await;
    v.upsert(vec![point("a", "s1", vec![1.0, 0.0, 0.0, 0.0], &[], "c")])
        .await
        .unwrap();
    v.ensure_collection(4).await.unwrap();
    assert_eq!(v.count().await.unwrap(), 1, "restart dropped the vectors");
    v.drop_collection().await.unwrap();
}
