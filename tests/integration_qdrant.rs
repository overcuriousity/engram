//! Integration tests against a real Qdrant.
//!
//! Requires a running server: `docker compose up -d` (or `podman run -d --name
//! engram-qdrant -p 127.0.0.1:6333:6333 qdrant/qdrant`).
//!
//! Run with: `cargo test --test integration_qdrant -- --ignored`
//!
//! Override the endpoint with `ENGRAM_TEST_QDRANT`, e.g.
//! `ENGRAM_TEST_QDRANT=http://localhost:16333`.

use engram::config::VectorConfig;
use engram::vector::{
    SearchFilter, VectorPayload, VectorPoint, VectorStore, qdrant::QdrantVectors,
};

fn cfg(collection: &str) -> VectorConfig {
    VectorConfig {
        url: url(),
        collection: collection.to_string(),
        api_key: None,
        recency_weight: 0.05,
        recency_half_life_days: 180,
        pinned_boost: 0.15,
    }
}

fn url() -> String {
    std::env::var("ENGRAM_TEST_QDRANT").unwrap_or_else(|_| "http://localhost:6333".into())
}

/// Raw REST, for setting up states engram itself will no longer create — such
/// as a pre-alias collection left behind by an older deployment.
async fn raw(method: reqwest::Method, path: &str, body: Option<serde_json::Value>) -> String {
    let mut req = reqwest::Client::new().request(method, format!("{}{path}", url()));
    if let Some(b) = body {
        req = req.json(&b);
    }
    req.send().await.unwrap().text().await.unwrap()
}

fn point(id: &str, src: &str, v: Vec<f32>, tags: &[&str], cat: &str) -> VectorPoint {
    VectorPoint {
        vector: v,
        sparse: Default::default(),
        payload: VectorPayload {
            artifact_id: id.into(),
            corpus_id: src.into(),
            text: format!("text {id}"),
            title: Some(id.into()),
            category: Some(cat.into()),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            created_at: 42,
            last_seen_at: None,
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
    let v = fresh("engram_it_roundtrip", 4).await;
    v.upsert(vec![
        point("a", "s1", vec![1.0, 0.0, 0.0, 0.0], &["linux"], "procedure"),
        point("b", "s1", vec![0.0, 1.0, 0.0, 0.0], &["windows"], "concept"),
    ])
    .await
    .unwrap();

    let hits = v
        .search(
            &[1.0, 0.0, 0.0, 0.0],
            &Default::default(),
            5,
            &SearchFilter::default(),
        )
        .await
        .unwrap();
    assert_eq!(hits[0].payload.artifact_id, "a");
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
    let v = fresh("engram_it_filter", 4).await;
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
    let hits = v
        .search(&[1.0, 0.0, 0.0, 0.0], &Default::default(), 5, &f)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.artifact_id, "a");

    let f = SearchFilter {
        tags: vec![],
        category: Some("concept".into()),
    };
    assert_eq!(
        v.search(&[1.0, 0.0, 0.0, 0.0], &Default::default(), 5, &f)
            .await
            .unwrap()[0]
            .payload
            .artifact_id,
        "b"
    );
    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn multiple_tags_are_an_and_not_an_or() {
    // The in-memory implementation requires every listed tag. Qdrant must
    // agree, or filtered search means different things in tests and production.
    let v = fresh("engram_it_tags_and", 4).await;
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
    let hits = v
        .search(&[1.0, 0.0, 0.0, 0.0], &Default::default(), 5, &f)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "tag filter behaved as OR, not AND");
    assert_eq!(hits[0].payload.artifact_id, "both");
    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn upsert_is_idempotent_per_chunk_id() {
    let v = fresh("engram_it_idempotent", 4).await;
    v.upsert(vec![point("a", "s1", vec![1.0, 0.0, 0.0, 0.0], &[], "c")])
        .await
        .unwrap();
    v.upsert(vec![point("a", "s1", vec![0.0, 0.0, 0.0, 1.0], &[], "c")])
        .await
        .unwrap();

    let hits = v
        .search(
            &[0.0, 0.0, 0.0, 1.0],
            &Default::default(),
            5,
            &SearchFilter::default(),
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "re-embedding must overwrite, not duplicate");
    assert!(hits[0].score > 0.99);
    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn delete_by_source_removes_only_that_source() {
    let v = fresh("engram_it_delete", 4).await;
    v.upsert(vec![
        point("a", "s1", vec![1.0, 0.0, 0.0, 0.0], &[], "c"),
        point("b", "s2", vec![1.0, 0.0, 0.0, 0.0], &[], "c"),
    ])
    .await
    .unwrap();
    v.delete_by_corpus("s1").await.unwrap();

    let hits = v
        .search(
            &[1.0, 0.0, 0.0, 0.0],
            &Default::default(),
            5,
            &SearchFilter::default(),
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.corpus_id, "s2");
    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn delete_chunks_removes_exactly_the_listed_ids() {
    let v = fresh("engram_it_delete_chunks", 4).await;
    v.upsert(vec![
        point("a", "s1", vec![1.0, 0.0, 0.0, 0.0], &[], "c"),
        point("b", "s1", vec![1.0, 0.0, 0.0, 0.0], &[], "c"),
    ])
    .await
    .unwrap();
    v.delete_artifacts(&["a".to_string()]).await.unwrap();

    let hits = v
        .search(
            &[1.0, 0.0, 0.0, 0.0],
            &Default::default(),
            5,
            &SearchFilter::default(),
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.artifact_id, "b");
    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn a_dimension_change_is_refused_rather_than_silently_accepted() {
    let v = QdrantVectors::connect(&cfg("engram_it_dim")).await.unwrap();
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
    let v = fresh("engram_it_idem_ensure", 4).await;
    v.upsert(vec![point("a", "s1", vec![1.0, 0.0, 0.0, 0.0], &[], "c")])
        .await
        .unwrap();
    v.ensure_collection(4).await.unwrap();
    assert_eq!(v.count().await.unwrap(), 1, "restart dropped the vectors");
    v.drop_collection().await.unwrap();
}

// ── Generations and aliases ─────────────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn a_new_collection_is_created_as_a_generation_behind_an_alias() {
    let v = fresh("engram_it_alias_new", 4).await;
    assert_eq!(
        v.resolve_alias().await.unwrap().as_deref(),
        Some("engram_it_alias_new_v1"),
        "the configured name must be an alias, not the collection itself"
    );
    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn reindex_copies_every_point_and_swaps_the_alias() {
    let v = fresh("engram_it_reindex", 4).await;
    v.upsert(vec![
        point("a", "s1", vec![1.0, 0.0, 0.0, 0.0], &["linux"], "procedure"),
        point("b", "s1", vec![0.0, 1.0, 0.0, 0.0], &[], "concept"),
        point("c", "s2", vec![0.0, 0.0, 1.0, 0.0], &[], "concept"),
    ])
    .await
    .unwrap();

    let target = v.reindex(4, false).await.unwrap();
    assert_eq!(target, "engram_it_reindex_v2");
    assert_eq!(
        v.resolve_alias().await.unwrap().as_deref(),
        Some("engram_it_reindex_v2")
    );
    assert_eq!(v.count().await.unwrap(), 3, "a rebuild lost points");

    // Reading through the alias must land on the new generation, with the
    // payload intact — the vectors were copied, not re-embedded.
    let hits = v
        .search(
            &[1.0, 0.0, 0.0, 0.0],
            &Default::default(),
            5,
            &SearchFilter::default(),
        )
        .await
        .unwrap();
    assert_eq!(hits[0].payload.artifact_id, "a");
    assert_eq!(hits[0].payload.tags, vec!["linux".to_string()]);
    assert_eq!(hits[0].payload.created_at, 42);
    assert!(hits[0].score > 0.99);

    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn reindex_leaves_the_previous_generation_in_place() {
    // The old generation is the only rollback that exists. Deleting it is a
    // decision for whoever ran the rebuild, not a side effect of running it.
    let v = fresh("engram_it_reindex_keep", 4).await;
    v.upsert(vec![point("a", "s1", vec![1.0, 0.0, 0.0, 0.0], &[], "c")])
        .await
        .unwrap();
    v.reindex(4, false).await.unwrap();

    let body = raw(
        reqwest::Method::GET,
        "/collections/engram_it_reindex_keep_v1/exists",
        None,
    )
    .await;
    assert!(
        body.contains("true"),
        "previous generation was deleted: {body}"
    );

    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn a_pre_alias_collection_is_refused_at_startup_then_migrated() {
    let name = "engram_it_legacy";
    let v = QdrantVectors::connect(&cfg(name)).await.unwrap();
    v.drop_collection().await.unwrap();

    // What an older engram left behind: a plain collection with a single
    // unnamed vector, holding real data.
    raw(
        reqwest::Method::PUT,
        &format!("/collections/{name}"),
        Some(serde_json::json!({ "vectors": { "size": 4, "distance": "Cosine" } })),
    )
    .await;
    raw(
        reqwest::Method::PUT,
        &format!("/collections/{name}/points?wait=true"),
        Some(serde_json::json!({
            "points": [ {
                "id": "11111111-1111-1111-1111-111111111111",
                "vector": [1.0, 0.0, 0.0, 0.0],
                "payload": {
                    "artifact_id": "legacy", "corpus_id": "s1", "text": "text legacy",
                    "title": "legacy", "category": "c", "tags": [], "created_at": 42
                }
            } ]
        })),
    )
    .await;

    // Starting up against it must fail loudly rather than create a second,
    // empty home for the vectors.
    let err = v.ensure_collection(4).await.unwrap_err().to_string();
    assert!(err.contains("--reindex"), "unhelpful: {err}");

    let target = v.reindex(4, true).await.unwrap();
    assert_eq!(target, "engram_it_legacy_v1");
    assert_eq!(
        v.resolve_alias().await.unwrap().as_deref(),
        Some("engram_it_legacy_v1")
    );

    let hits = v
        .search(
            &[1.0, 0.0, 0.0, 0.0],
            &Default::default(),
            5,
            &SearchFilter::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        hits[0].payload.artifact_id, "legacy",
        "an unnamed vector must survive the move to named vectors"
    );

    // And the next startup is clean.
    v.ensure_collection(4).await.unwrap();
    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn reindex_refuses_when_there_is_nothing_to_rebuild() {
    let v = QdrantVectors::connect(&cfg("engram_it_reindex_empty"))
        .await
        .unwrap();
    v.drop_collection().await.unwrap();
    let err = v.reindex(4, false).await.unwrap_err().to_string();
    assert!(err.contains("nothing to reindex"), "{err}");
}

#[tokio::test]
#[ignore]
async fn reindex_refuses_at_a_dimension_the_source_does_not_have() {
    // Rebuilding copies vectors rather than re-embedding them, so it cannot
    // change their width. Saying so beats writing 4-wide vectors into an
    // 8-wide collection.
    let v = fresh("engram_it_reindex_dim", 4).await;
    let err = v.reindex(8, false).await.unwrap_err().to_string();
    assert!(err.contains('4') && err.contains('8'), "unhelpful: {err}");
    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn a_pre_alias_collection_is_never_deleted_without_being_asked() {
    // Freeing the alias name costs the source collection, which is the only
    // copy. Without the opt-in the rebuild must stop before creating anything.
    let name = "engram_it_legacy_consent";
    let v = QdrantVectors::connect(&cfg(name)).await.unwrap();
    v.drop_collection().await.unwrap();
    raw(
        reqwest::Method::PUT,
        &format!("/collections/{name}"),
        Some(serde_json::json!({ "vectors": { "size": 4, "distance": "Cosine" } })),
    )
    .await;

    let err = v.reindex(4, false).await.unwrap_err().to_string();
    assert!(err.contains("--replace-legacy"), "unhelpful: {err}");

    let body = raw(
        reqwest::Method::GET,
        &format!("/collections/{name}/exists"),
        None,
    )
    .await;
    assert!(
        body.contains("true"),
        "the source was deleted anyway: {body}"
    );
    let leftover = raw(
        reqwest::Method::GET,
        &format!("/collections/{name}_v1/exists"),
        None,
    )
    .await;
    assert!(
        leftover.contains("false"),
        "a refused rebuild left a half-built generation behind: {leftover}"
    );

    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn generations_without_an_alias_are_adopted_rather_than_duplicated() {
    // A rebuild that dies between deleting the old collection and creating the
    // alias leaves exactly this state. Starting up must find the vectors, not
    // build a second empty home beside them.
    let name = "engram_it_orphan";
    let v = fresh(name, 4).await;
    v.upsert(vec![point("a", "s1", vec![1.0, 0.0, 0.0, 0.0], &[], "c")])
        .await
        .unwrap();

    raw(
        reqwest::Method::POST,
        "/collections/aliases",
        Some(serde_json::json!({
            "actions": [ { "delete_alias": { "alias_name": name } } ]
        })),
    )
    .await;
    assert!(v.resolve_alias().await.unwrap().is_none());

    v.ensure_collection(4).await.unwrap();
    assert_eq!(
        v.resolve_alias().await.unwrap().as_deref(),
        Some("engram_it_orphan_v1")
    );
    assert_eq!(v.count().await.unwrap(), 1, "adoption lost the vectors");

    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn set_payload_rewrites_metadata_without_touching_the_vector() {
    // Editing a tag must not cost an embedding call, and must not disturb the
    // vector that makes the chunk findable in the first place.
    let v = fresh("engram_it_set_payload", 4).await;
    v.upsert(vec![point(
        "a",
        "s1",
        vec![1.0, 0.0, 0.0, 0.0],
        &["old"],
        "concept",
    )])
    .await
    .unwrap();

    let mut updated = point("a", "s1", vec![], &["fresh", "second"], "procedure").payload;
    updated.title = Some("a new title".into());
    v.set_payload(&updated).await.unwrap();

    let hits = v
        .search(
            &[1.0, 0.0, 0.0, 0.0],
            &Default::default(),
            5,
            &SearchFilter::default(),
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].score > 0.99, "the vector was disturbed");
    assert_eq!(
        hits[0].payload.tags,
        vec!["fresh".to_string(), "second".to_string()]
    );
    assert_eq!(hits[0].payload.category.as_deref(), Some("procedure"));

    // The keyword index must see the new value, or filtered search would still
    // answer for the old one.
    let f = SearchFilter {
        tags: vec!["fresh".into()],
        category: None,
    };
    assert_eq!(
        v.search(&[1.0, 0.0, 0.0, 0.0], &Default::default(), 5, &f)
            .await
            .unwrap()
            .len(),
        1
    );
    let stale = SearchFilter {
        tags: vec!["old".into()],
        category: None,
    };
    assert!(
        v.search(&[1.0, 0.0, 0.0, 0.0], &Default::default(), 5, &stale)
            .await
            .unwrap()
            .is_empty(),
        "the replaced tag is still indexed"
    );

    v.drop_collection().await.unwrap();
}

// ── Hybrid retrieval ────────────────────────────────────────────────────────

/// A point whose sparse half is computed from real text, the way the embed job
/// builds one. The dense vector is supplied separately so a test can make the
/// semantic half deliberately unhelpful.
fn hybrid_point(id: &str, text: &str, dense: Vec<f32>) -> VectorPoint {
    VectorPoint {
        vector: dense,
        sparse: engram::vector::sparse::encode_document(text),
        payload: VectorPayload {
            artifact_id: id.into(),
            corpus_id: "s1".into(),
            text: text.into(),
            title: None,
            category: Some("c".into()),
            tags: vec![],
            created_at: 42,
            last_seen_at: None,
        },
    }
}

#[tokio::test]
#[ignore]
async fn an_exact_token_is_found_even_when_the_dense_vector_points_elsewhere() {
    // The case hybrid search exists for. The dense query vector is orthogonal
    // to the document that actually contains `E01`, so a dense-only search
    // ranks it last. The lexical branch has to rescue it.
    let v = fresh("engram_it_hybrid", 4).await;
    v.upsert(vec![
        hybrid_point(
            "target",
            "mounting an E01 image with ewfmount",
            vec![0.0, 1.0, 0.0, 0.0],
        ),
        hybrid_point(
            "decoy_a",
            "configuring a printer on windows",
            vec![1.0, 0.0, 0.0, 0.0],
        ),
        hybrid_point(
            "decoy_b",
            "resetting a password in the console",
            vec![1.0, 0.0, 0.0, 0.0],
        ),
    ])
    .await
    .unwrap();

    let dense_query = [1.0, 0.0, 0.0, 0.0];

    // Dense only: the decoys win, because the query vector is theirs.
    let dense_only = v
        .search(
            &dense_query,
            &Default::default(),
            1,
            &SearchFilter::default(),
        )
        .await
        .unwrap();
    assert_ne!(
        dense_only[0].payload.artifact_id, "target",
        "the fixture is wrong: dense search already finds it, so this proves nothing"
    );

    // Hybrid: the term rescues it.
    let sparse = engram::vector::sparse::encode_query("E01");
    let hybrid = v
        .search(&dense_query, &sparse, 3, &SearchFilter::default())
        .await
        .unwrap();
    assert_eq!(
        hybrid[0].payload.artifact_id,
        "target",
        "hybrid search did not promote the exact-token match: {:?}",
        hybrid
            .iter()
            .map(|h| &h.payload.artifact_id)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore]
async fn a_filter_still_applies_to_both_halves_of_a_hybrid_query() {
    // The filter has to be repeated per prefetch branch. If it were only on the
    // outer query, the lexical branch would happily return excluded points.
    let v = fresh("engram_it_hybrid_filter", 4).await;
    let mut wanted = hybrid_point(
        "wanted",
        "the ext4 journal replays on mount",
        vec![1.0, 0.0, 0.0, 0.0],
    );
    wanted.payload.tags = vec!["keep".into()];
    let mut excluded = hybrid_point(
        "excluded",
        "the ext4 journal also replays here",
        vec![1.0, 0.0, 0.0, 0.0],
    );
    excluded.payload.tags = vec!["drop".into()];
    v.upsert(vec![wanted, excluded]).await.unwrap();

    let sparse = engram::vector::sparse::encode_query("ext4 journal");
    let f = SearchFilter {
        tags: vec!["keep".into()],
        category: None,
    };
    let hits = v
        .search(&[1.0, 0.0, 0.0, 0.0], &sparse, 10, &f)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "the filter leaked on one of the branches");
    assert_eq!(hits[0].payload.artifact_id, "wanted");

    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn a_query_with_no_indexable_term_falls_back_to_dense_alone() {
    // Punctuation produces no terms. Asking the lexical index to match nothing
    // is not the same as not asking it, so the query shape has to change.
    let v = fresh("engram_it_hybrid_empty", 4).await;
    v.upsert(vec![hybrid_point(
        "a",
        "some text",
        vec![1.0, 0.0, 0.0, 0.0],
    )])
    .await
    .unwrap();

    let sparse = engram::vector::sparse::encode_query("?? ...");
    assert!(sparse.is_empty(), "the fixture must produce no terms");
    let hits = v
        .search(&[1.0, 0.0, 0.0, 0.0], &sparse, 5, &SearchFilter::default())
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].score > 0.99,
        "a dense-only search still scores by cosine"
    );

    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn a_rebuild_adds_the_lexical_half_to_a_generation_without_it() {
    // Sparse vectors are recomputed from the payload rather than copied, so a
    // collection written before hybrid search gains it for free.
    let v = fresh("engram_it_hybrid_reindex", 4).await;
    v.upsert(vec![
        // Written the old way: dense only, no terms.
        point("target", "s1", vec![0.0, 1.0, 0.0, 0.0], &[], "c"),
        point("decoy", "s1", vec![1.0, 0.0, 0.0, 0.0], &[], "c"),
    ])
    .await
    .unwrap();

    // `point` builds its text as "text {id}", so this is the term to look for.
    let sparse = engram::vector::sparse::encode_query("target");
    let before = v
        .search(&[1.0, 0.0, 0.0, 0.0], &sparse, 1, &SearchFilter::default())
        .await
        .unwrap();
    assert_eq!(
        before[0].payload.artifact_id, "decoy",
        "the fixture is wrong: the term already matched before the rebuild"
    );

    v.reindex(4, false).await.unwrap();

    let after = v
        .search(&[1.0, 0.0, 0.0, 0.0], &sparse, 2, &SearchFilter::default())
        .await
        .unwrap();
    assert_eq!(
        after[0].payload.artifact_id, "target",
        "the rebuild did not compute sparse vectors"
    );

    v.drop_collection().await.unwrap();
}

// ── Recency and pinning ─────────────────────────────────────────────────────

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn aged(id: &str, dense: Vec<f32>, days_old: i64, tags: &[&str]) -> VectorPoint {
    VectorPoint {
        vector: dense,
        sparse: Default::default(),
        payload: VectorPayload {
            artifact_id: id.into(),
            corpus_id: "s1".into(),
            text: format!("text {id}"),
            title: None,
            category: Some("c".into()),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            created_at: now_secs() - days_old * 86_400,
            last_seen_at: None,
        },
    }
}

#[tokio::test]
#[ignore]
async fn recency_breaks_a_tie_between_equally_relevant_chunks() {
    let v = fresh("engram_it_recency", 4).await;
    v.upsert(vec![
        aged("old", vec![1.0, 0.0, 0.0, 0.0], 3650, &[]),
        aged("new", vec![1.0, 0.0, 0.0, 0.0], 0, &[]),
    ])
    .await
    .unwrap();

    let hits = v
        .search(
            &[1.0, 0.0, 0.0, 0.0],
            &Default::default(),
            5,
            &SearchFilter::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        hits[0].payload.artifact_id, "new",
        "identical vectors, so only age can order them"
    );
}

#[tokio::test]
#[ignore]
async fn recency_does_not_overturn_a_clearly_better_match() {
    // The nudge has to stay a nudge. A note captured today must not outrank a
    // genuinely better answer written years ago.
    let v = fresh("engram_it_recency_bounded", 4).await;
    v.upsert(vec![
        aged("relevant_but_old", vec![1.0, 0.0, 0.0, 0.0], 3650, &[]),
        aged("fresh_but_wrong", vec![0.0, 1.0, 0.0, 0.0], 0, &[]),
    ])
    .await
    .unwrap();

    let hits = v
        .search(
            &[1.0, 0.0, 0.0, 0.0],
            &Default::default(),
            5,
            &SearchFilter::default(),
        )
        .await
        .unwrap();
    assert_eq!(hits[0].payload.artifact_id, "relevant_but_old");
}

#[tokio::test]
#[ignore]
async fn a_pinned_chunk_outranks_the_decay_curve() {
    // Pinning is a decision the user made, and it has to beat the heuristic.
    let v = fresh("engram_it_pinned", 4).await;
    v.upsert(vec![
        aged("pinned_old", vec![0.94, 0.34, 0.0, 0.0], 3650, &["pinned"]),
        aged("plain_new", vec![1.0, 0.0, 0.0, 0.0], 0, &[]),
    ])
    .await
    .unwrap();

    let hits = v
        .search(
            &[1.0, 0.0, 0.0, 0.0],
            &Default::default(),
            5,
            &SearchFilter::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        hits[0].payload.artifact_id, "pinned_old",
        "the pinned tag did not beat a newer, slightly closer chunk"
    );
}

#[tokio::test]
#[ignore]
async fn scoring_leaves_the_filter_alone() {
    // The formula runs over what retrieval returned. If it were applied
    // instead of the filter, excluded points would reappear at the top.
    let v = fresh("engram_it_recency_filter", 4).await;
    v.upsert(vec![
        aged("keep", vec![1.0, 0.0, 0.0, 0.0], 3650, &["keep"]),
        aged("drop", vec![1.0, 0.0, 0.0, 0.0], 0, &["pinned"]),
    ])
    .await
    .unwrap();

    let f = SearchFilter {
        tags: vec!["keep".into()],
        category: None,
    };
    let hits = v
        .search(&[1.0, 0.0, 0.0, 0.0], &Default::default(), 10, &f)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.artifact_id, "keep");

    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn resurface_finds_the_old_and_unseen_and_skips_everything_else() {
    let v = fresh("engram_it_resurface", 4).await;
    let month = 31 * 86_400;
    v.upsert(vec![
        aged("forgotten", vec![1.0, 0.0, 0.0, 0.0], 60, &[]),
        aged("recent", vec![1.0, 0.0, 0.0, 0.0], 1, &[]),
        aged("old_but_just_seen", vec![1.0, 0.0, 0.0, 0.0], 60, &[]),
    ])
    .await
    .unwrap();
    v.touch(&["old_but_just_seen".to_string()], now_secs())
        .await
        .unwrap();

    let cutoff = now_secs() - month;
    let out = v.resurface(10, cutoff, cutoff).await.unwrap();
    let ids: Vec<&str> = out.iter().map(|h| h.payload.artifact_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["forgotten"],
        "expected only the old, unseen chunk; got {ids:?}"
    );

    // Showing it counts as seeing it.
    v.touch(&["forgotten".to_string()], now_secs())
        .await
        .unwrap();
    assert!(
        v.resurface(10, cutoff, cutoff).await.unwrap().is_empty(),
        "a chunk shown a moment ago is not forgotten"
    );

    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn touching_a_chunk_leaves_the_rest_of_its_payload_alone() {
    // The stamp is merged, not written as a whole payload. Getting this wrong
    // would silently erase tags and text on every search.
    let v = fresh("engram_it_touch_merge", 4).await;
    v.upsert(vec![aged(
        "a",
        vec![1.0, 0.0, 0.0, 0.0],
        1,
        &["keep", "these"],
    )])
    .await
    .unwrap();

    v.touch(&["a".to_string()], 12_345).await.unwrap();

    let hits = v
        .search(
            &[1.0, 0.0, 0.0, 0.0],
            &Default::default(),
            5,
            &SearchFilter::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        hits[0].payload.tags,
        vec!["keep".to_string(), "these".to_string()]
    );
    assert_eq!(hits[0].payload.text, "text a");
    assert_eq!(hits[0].payload.last_seen_at, Some(12_345));

    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn editing_metadata_does_not_erase_when_a_chunk_was_last_seen() {
    // `set_payload` sends no stamp, and Qdrant merges, so the stored one has
    // to survive a tag edit.
    let v = fresh("engram_it_touch_survives", 4).await;
    v.upsert(vec![aged("a", vec![1.0, 0.0, 0.0, 0.0], 1, &["old"])])
        .await
        .unwrap();
    v.touch(&["a".to_string()], 12_345).await.unwrap();

    let mut edited = aged("a", vec![], 1, &["fresh"]).payload;
    edited.last_seen_at = None;
    v.set_payload(&edited).await.unwrap();

    let hits = v
        .search(
            &[1.0, 0.0, 0.0, 0.0],
            &Default::default(),
            5,
            &SearchFilter::default(),
        )
        .await
        .unwrap();
    assert_eq!(hits[0].payload.tags, vec!["fresh".to_string()]);
    assert_eq!(
        hits[0].payload.last_seen_at,
        Some(12_345),
        "a tag edit reset the last-seen stamp"
    );

    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn re_embedding_does_not_erase_when_a_chunk_was_last_seen() {
    // Unlike `set_payload` and `touch`, a point write replaces the payload
    // whole. A re-embed builds it from the chunk row, which knows nothing about
    // the stamp, so the store has to carry the stored one forward — otherwise
    // editing a chunk makes `resurface` call it forgotten.
    let v = fresh("engram_it_upsert_survives", 4).await;
    v.upsert(vec![aged("a", vec![1.0, 0.0, 0.0, 0.0], 60, &["t"])])
        .await
        .unwrap();
    v.touch(&["a".to_string()], now_secs()).await.unwrap();

    // The same chunk, embedded again after an edit.
    let mut again = aged("a", vec![0.0, 1.0, 0.0, 0.0], 60, &["t"]);
    again.payload.text = "edited text".into();
    v.upsert(vec![again]).await.unwrap();

    let cutoff = now_secs() - 31 * 86_400;
    assert!(
        v.resurface(10, cutoff, cutoff).await.unwrap().is_empty(),
        "the re-embed dropped the stamp and the chunk now reads as forgotten"
    );

    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn a_point_written_by_something_else_does_not_take_search_down() {
    // A rebuild copies payloads verbatim, and nothing stops an operator from
    // inserting their own point. Two things would otherwise turn one foreign
    // point into a total outage: the scoring formula reads `created_at` and
    // fails the whole query when it is missing, and the payload would not
    // deserialize. Neither may cost the results that *are* ours.
    let name = "engram_it_foreign_point";
    let v = fresh(name, 4).await;
    v.upsert(vec![point(
        "mine",
        "s1",
        vec![1.0, 0.0, 0.0, 0.0],
        &[],
        "c",
    )])
    .await
    .unwrap();

    let collection = v.resolve_alias().await.unwrap().expect("no alias");
    raw(
        reqwest::Method::PUT,
        &format!("/collections/{collection}/points?wait=true"),
        Some(serde_json::json!({
            "points": [{
                "id": "11111111-1111-4111-8111-111111111111",
                "vector": { "dense": [0.9, 0.1, 0.0, 0.0] },
                "payload": { "something": "else" },
            }],
        })),
    )
    .await;

    let hits = v
        .search(
            &[1.0, 0.0, 0.0, 0.0],
            &Default::default(),
            5,
            &SearchFilter::default(),
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "the foreign point was not skipped: {hits:?}");
    assert_eq!(hits[0].payload.artifact_id, "mine");

    v.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn a_neighbouring_collection_is_not_mistaken_for_a_generation() {
    // `drop_collection` deletes everything the alias claims. Claiming by name
    // prefix would make `engram_it_neighbour_vault` ours to delete.
    let name = "engram_it_neighbour";
    let neighbour = format!("{name}_vault");
    let v = fresh(name, 4).await;
    raw(
        reqwest::Method::PUT,
        &format!("/collections/{neighbour}"),
        Some(serde_json::json!({ "vectors": { "size": 4, "distance": "Cosine" } })),
    )
    .await;

    v.drop_collection().await.unwrap();

    let body = raw(
        reqwest::Method::GET,
        &format!("/collections/{neighbour}/exists"),
        None,
    )
    .await;
    assert!(
        body.contains("true"),
        "a collection that merely starts the same way was deleted: {body}"
    );

    // And it is not adopted as a generation either: with no alias and no
    // numbered generation, startup must build `_v1` rather than claim it.
    v.ensure_collection(4).await.unwrap();
    assert_eq!(
        v.resolve_alias().await.unwrap().as_deref(),
        Some(format!("{name}_v1").as_str()),
        "the neighbour was adopted as this alias's newest generation"
    );

    v.drop_collection().await.unwrap();
    raw(
        reqwest::Method::DELETE,
        &format!("/collections/{neighbour}"),
        None,
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn two_processes_starting_together_end_up_on_one_generation() {
    // Both read no alias, both create the first generation, both write the
    // alias. Losing that race is the outcome we wanted; failing startup over
    // it is not.
    let name = "engram_it_startup_race";
    let v = QdrantVectors::connect(&cfg(name)).await.unwrap();
    v.drop_collection().await.unwrap();

    let a = QdrantVectors::connect(&cfg(name)).await.unwrap();
    let b = QdrantVectors::connect(&cfg(name)).await.unwrap();
    let (ra, rb) = tokio::join!(a.ensure_collection(4), b.ensure_collection(4));
    ra.expect("first process failed to start");
    rb.expect("second process failed to start");

    assert_eq!(
        v.resolve_alias().await.unwrap().as_deref(),
        Some(format!("{name}_v1").as_str())
    );
    v.drop_collection().await.unwrap();
}
