//! What every `VectorStore` must do, whichever one it is.
//!
//! The stores differ in how they rank — `MemoryVectors` is dense only, Qdrant
//! and SQLite fuse a lexical half in — so nothing here depends on the order of
//! results where fusion could change it: each ranking case uses a query with
//! no indexable term, which every store answers by cosine alone.

use super::sparse::SparseVector;
use super::{LifecycleRow, SearchFilter, Touch, VectorPayload, VectorPoint, VectorStore};
use crate::store::artifacts::ArtifactStatus;

pub const DIM: usize = 3;

pub fn point(
    id: &str,
    corpus: &str,
    v: [f32; DIM],
    tags: &[&str],
    cat: Option<&str>,
) -> VectorPoint {
    VectorPoint {
        vector: v.to_vec(),
        sparse: SparseVector::default(),
        payload: VectorPayload {
            artifact_id: id.into(),
            corpus_id: corpus.into(),
            text: format!("text of {id}"),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            category: cat.map(str::to_string),
            created_at: 1_000,
            ..Default::default()
        },
    }
}

/// A filter that hides nothing.
pub fn wide() -> SearchFilter {
    SearchFilter {
        include_superseded: true,
        include_deprecated: true,
        ..Default::default()
    }
}

fn ids(hits: &[super::SearchHit]) -> Vec<&str> {
    hits.iter()
        .map(|h| h.payload.artifact_id.as_str())
        .collect()
}

fn none() -> SparseVector {
    SparseVector::default()
}

pub async fn search_ranks_by_cosine(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(vec![
        point("far", "c", [0.0, 1.0, 0.0], &[], None),
        point("near", "c", [1.0, 0.1, 0.0], &[], None),
        point("exact", "c", [1.0, 0.0, 0.0], &[], None),
    ])
    .await
    .unwrap();
    let hits = s
        .search(&[1.0, 0.0, 0.0], &none(), 10, &wide())
        .await
        .unwrap();
    assert_eq!(ids(&hits), ["exact", "near", "far"]);
    assert!((hits[0].similarity.unwrap() - 1.0).abs() < 1e-5);
}

pub async fn limit_is_respected(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(
        (0..5)
            .map(|i| point(&format!("p{i}"), "c", [1.0, i as f32, 0.0], &[], None))
            .collect(),
    )
    .await
    .unwrap();
    assert_eq!(
        s.search(&[1.0, 0.0, 0.0], &none(), 2, &wide())
            .await
            .unwrap()
            .len(),
        2
    );
}

pub async fn zero_vectors_do_not_produce_nan(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(vec![point("zero", "c", [0.0, 0.0, 0.0], &[], None)])
        .await
        .unwrap();
    let hits = s
        .search(&[1.0, 0.0, 0.0], &none(), 10, &wide())
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].score.is_finite());
    assert_eq!(hits[0].similarity, Some(0.0));
}

pub async fn equal_scores_order_by_id(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(vec![
        point("b", "c", [1.0, 0.0, 0.0], &[], None),
        point("a", "c", [1.0, 0.0, 0.0], &[], None),
        point("c", "c", [1.0, 0.0, 0.0], &[], None),
    ])
    .await
    .unwrap();
    let hits = s
        .search(&[1.0, 0.0, 0.0], &none(), 10, &wide())
        .await
        .unwrap();
    assert_eq!(ids(&hits), ["a", "b", "c"]);
}

pub async fn filters_narrow(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(vec![
        point("both", "c1", [1.0, 0.0, 0.0], &["x", "y"], Some("note")),
        point("one", "c1", [1.0, 0.0, 0.0], &["x"], Some("howto")),
        point("other", "c2", [1.0, 0.0, 0.0], &[], None),
    ])
    .await
    .unwrap();
    let q = [1.0, 0.0, 0.0];
    let tags = SearchFilter {
        tags: vec!["x".into(), "y".into()],
        ..wide()
    };
    assert_eq!(
        ids(&s.search(&q, &none(), 10, &tags).await.unwrap()),
        ["both"]
    );
    let cat = SearchFilter {
        category: Some("howto".into()),
        ..wide()
    };
    assert_eq!(
        ids(&s.search(&q, &none(), 10, &cat).await.unwrap()),
        ["one"]
    );
    let corpus = SearchFilter {
        corpus_id: Some("c2".into()),
        ..wide()
    };
    assert_eq!(
        ids(&s.search(&q, &none(), 10, &corpus).await.unwrap()),
        ["other"]
    );
}

pub async fn hidden_statuses_stay_out_unless_asked_for(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(vec![
        point("live", "c", [1.0, 0.0, 0.0], &[], None),
        point("old", "c", [1.0, 0.0, 0.0], &[], None),
        point("stale", "c", [1.0, 0.0, 0.0], &[], None),
    ])
    .await
    .unwrap();
    s.set_lifecycle("old", ArtifactStatus::Superseded, Some("live"))
        .await
        .unwrap();
    s.set_lifecycle("stale", ArtifactStatus::Deprecated, None)
        .await
        .unwrap();
    let q = [1.0, 0.0, 0.0];
    assert_eq!(
        ids(&s
            .search(&q, &none(), 10, &SearchFilter::default())
            .await
            .unwrap()),
        ["live"]
    );
    let with_old = SearchFilter {
        include_superseded: true,
        ..Default::default()
    };
    assert_eq!(
        ids(&s.search(&q, &none(), 10, &with_old).await.unwrap()),
        ["live", "old"]
    );
    let got = s
        .lifecycle_of(&["old".into(), "ghost".into()])
        .await
        .unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got["old"].status, ArtifactStatus::Superseded);
    assert_eq!(got["old"].superseded_by.as_deref(), Some("live"));
}

pub async fn a_re_embed_keeps_the_stamps(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(vec![point("a", "c", [1.0, 0.0, 0.0], &[], None)])
        .await
        .unwrap();
    s.touch(&[Touch::retrieved("a", None)], 500).await.unwrap();
    s.set_lifecycle("a", ArtifactStatus::Deprecated, None)
        .await
        .unwrap();
    s.set_last_verified_at("a", 700, false).await.unwrap();
    // The embed job rebuilds the payload knowing none of this.
    s.upsert(vec![point("a", "c", [0.0, 1.0, 0.0], &[], None)])
        .await
        .unwrap();
    let p = &s.payloads_of(&["a".into()]).await.unwrap()["a"];
    assert_eq!(p.last_seen_at, Some(500));
    assert_eq!(p.hit_count, Some(1));
    assert_eq!(p.status, Some(ArtifactStatus::Deprecated));
    assert_eq!(p.last_verified_at, Some(700));
    assert_eq!(s.dense_of("a").await.unwrap(), Some(vec![0.0, 1.0, 0.0]));
}

pub async fn set_payload_merges_and_leaves_the_vector(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    let mut first = point("a", "c", [1.0, 0.0, 0.0], &["x"], Some("note"));
    first.payload.provenance = Some("synthesized".into());
    first.payload.origin_corpora = vec!["c".into(), "d".into()];
    s.upsert(vec![first]).await.unwrap();
    s.touch(&[Touch::shown("a")], 900).await.unwrap();
    let edit = point("a", "c", [9.0, 9.0, 9.0], &["z"], Some("howto")).payload;
    s.set_payload(&edit).await.unwrap();
    let p = &s.payloads_of(&["a".into()]).await.unwrap()["a"];
    assert_eq!(p.tags, ["z"]);
    assert_eq!(p.category.as_deref(), Some("howto"));
    assert_eq!(p.last_seen_at, Some(900));
    assert_eq!(p.provenance.as_deref(), Some("synthesized"));
    assert_eq!(p.origin_corpora, ["c", "d"]);
    assert_eq!(s.dense_of("a").await.unwrap(), Some(vec![1.0, 0.0, 0.0]));
}

pub async fn touch_counts_only_retrievals(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(vec![point("a", "c", [1.0, 0.0, 0.0], &[], None)])
        .await
        .unwrap();
    s.touch(&[Touch::shown("a")], 10).await.unwrap();
    s.touch(&[Touch::retrieved("a", None)], 20).await.unwrap();
    s.touch(&[Touch::retrieved("a", Some(1))], 30)
        .await
        .unwrap();
    let p = &s.payloads_of(&["a".into()]).await.unwrap()["a"];
    assert_eq!(p.last_seen_at, Some(30));
    assert_eq!(p.hit_count, Some(2));
    s.set_last_verified_at("a", 40, true).await.unwrap();
    let p = &s.payloads_of(&["a".into()]).await.unwrap()["a"];
    assert_eq!(p.hit_count, Some(0));
}

pub async fn stale_candidates_need_a_stamp(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(vec![
        point("old-quiet", "c", [1.0, 0.0, 0.0], &[], None),
        point("old-busy", "c", [1.0, 0.0, 0.0], &[], None),
        point("unknown", "c", [1.0, 0.0, 0.0], &[], None),
        point("fresh", "c", [1.0, 0.0, 0.0], &[], None),
    ])
    .await
    .unwrap();
    s.set_last_verified_at("old-quiet", 100, false)
        .await
        .unwrap();
    s.set_last_verified_at("old-busy", 100, false)
        .await
        .unwrap();
    s.set_last_verified_at("fresh", 9_000, false).await.unwrap();
    for _ in 0..3 {
        s.touch(&[Touch::retrieved("old-busy", None)], 200)
            .await
            .unwrap();
    }
    let got = s.stale_candidates(1_000, 1, 10).await.unwrap();
    assert_eq!(ids(&got), ["old-quiet"]);
}

pub async fn resurface_offers_the_old_and_unseen(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    let mut young = point("young", "c", [1.0, 0.0, 0.0], &[], None);
    young.payload.created_at = 9_000;
    s.upsert(vec![
        point("forgotten", "c", [1.0, 0.0, 0.0], &[], None),
        point("seen", "c", [1.0, 0.0, 0.0], &[], None),
        young,
    ])
    .await
    .unwrap();
    s.touch(&[Touch::shown("seen")], 8_000).await.unwrap();
    let got = s.resurface(10, 5_000, 7_000).await.unwrap();
    assert_eq!(ids(&got), ["forgotten"]);
    assert_eq!(got[0].similarity, None);
}

pub async fn apply_lifecycle_writes_a_batch(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(vec![
        point("a", "c", [1.0, 0.0, 0.0], &[], None),
        point("b", "c", [1.0, 0.0, 0.0], &[], None),
    ])
    .await
    .unwrap();
    s.touch(&[Touch::retrieved("a", None)], 5).await.unwrap();
    s.apply_lifecycle(&[
        LifecycleRow {
            artifact_id: "a".into(),
            status: ArtifactStatus::Superseded,
            superseded_by: Some("b".into()),
            last_verified_at: 77,
        },
        LifecycleRow {
            artifact_id: "ghost".into(),
            status: ArtifactStatus::Active,
            superseded_by: None,
            last_verified_at: 1,
        },
    ])
    .await
    .unwrap();
    let p = &s.payloads_of(&["a".into()]).await.unwrap()["a"];
    assert_eq!(p.status, Some(ArtifactStatus::Superseded));
    assert_eq!(p.superseded_by.as_deref(), Some("b"));
    assert_eq!(p.last_verified_at, Some(77));
    assert_eq!(
        p.hit_count,
        Some(1),
        "a bulk write must not wipe the counter"
    );
}

pub async fn facets_count_categories(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(vec![
        point("a", "c", [1.0, 0.0, 0.0], &[], Some("note")),
        point("b", "c", [1.0, 0.0, 0.0], &[], Some("note")),
        point("c", "c", [1.0, 0.0, 0.0], &[], Some("howto")),
        point("d", "c", [1.0, 0.0, 0.0], &[], None),
    ])
    .await
    .unwrap();
    let f = s.facets(1).await.unwrap();
    assert_eq!(f.categories.len(), 1);
    assert_eq!(
        (f.categories[0].value.as_str(), f.categories[0].count),
        ("note", 2)
    );
}

pub async fn neighbours_exclude_the_artifact_itself(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(vec![
        point("me", "c", [1.0, 0.0, 0.0], &[], None),
        point("close", "c", [1.0, 0.2, 0.0], &[], None),
        point("far", "c", [0.0, 0.0, 1.0], &[], None),
    ])
    .await
    .unwrap();
    assert_eq!(ids(&s.neighbours("me", 1).await.unwrap()), ["close"]);
    assert!(s.neighbours("ghost", 5).await.unwrap().is_empty());
}

pub async fn context_sets_match_on_the_nearest_situation(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(vec![
        point("two", "c", [1.0, 0.0, 0.0], &[], None),
        point("none", "c", [1.0, 0.0, 0.0], &[], None),
        point("hidden", "c", [1.0, 0.0, 0.0], &[], None),
    ])
    .await
    .unwrap();
    s.set_context_vectors("two", vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]])
        .await
        .unwrap();
    s.set_context_vectors("hidden", vec![vec![0.0, 1.0, 0.0]])
        .await
        .unwrap();
    s.set_context_vectors("ghost", vec![vec![0.0, 1.0, 0.0]])
        .await
        .unwrap();
    s.set_lifecycle("hidden", ArtifactStatus::Deprecated, None)
        .await
        .unwrap();
    let got = s
        .context_query(&[0.0, 1.0, 0.0], 10, &SearchFilter::default())
        .await
        .unwrap();
    assert_eq!(ids(&got), ["two"]);
    assert!(
        (got[0].score - 1.0).abs() < 1e-5,
        "the mean of the set would be 0.707"
    );
    assert_eq!(got[0].similarity, None);
    s.set_context_vectors("two", vec![]).await.unwrap();
    assert!(
        s.context_query(&[0.0, 1.0, 0.0], 10, &wide())
            .await
            .unwrap()
            .iter()
            .all(|h| h.payload.artifact_id != "two")
    );
    assert_eq!(
        s.count().await.unwrap(),
        3,
        "an empty write leaves the point"
    );
}

pub async fn deletes_take_everything_with_them(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(vec![
        point("a", "c1", [1.0, 0.0, 0.0], &["t"], None),
        point("b", "c1", [1.0, 0.0, 0.0], &[], None),
        point("c", "c2", [1.0, 0.0, 0.0], &[], None),
    ])
    .await
    .unwrap();
    s.set_context_vectors("a", vec![vec![1.0, 0.0, 0.0]])
        .await
        .unwrap();
    s.delete_artifacts(&["a".into()]).await.unwrap();
    assert!(
        s.context_query(&[1.0, 0.0, 0.0], 10, &wide())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(s.dense_of("a").await.unwrap(), None);
    s.delete_by_corpus("c1").await.unwrap();
    let mut left = s.all_artifact_ids().await.unwrap();
    left.sort();
    assert_eq!(left, ["c"]);
    assert_eq!(s.count().await.unwrap(), 1);
}

pub async fn a_sample_is_capped_and_repeatable(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(
        (0..4)
            .map(|i| point(&format!("p{i}"), "c", [i as f32, 1.0, 0.0], &[], None))
            .collect(),
    )
    .await
    .unwrap();
    let one = s.sample(3).await.unwrap();
    assert_eq!(one.len(), 3);
    assert_eq!(one, s.sample(3).await.unwrap());
    assert!(one.iter().all(|(_, v)| v.len() == DIM));
}

/// One `#[tokio::test]` per behaviour above, for the store `$make` builds.
/// `$make` is an async expression; whatever it yields must deref to a store
/// and is kept alive for the length of the test, so it may own a `TempDir`.
macro_rules! suite {
    ($make:expr) => {
        $crate::vector::conformance::suite!(@each $make;
            search_ranks_by_cosine, limit_is_respected, zero_vectors_do_not_produce_nan,
            equal_scores_order_by_id, filters_narrow, hidden_statuses_stay_out_unless_asked_for,
            a_re_embed_keeps_the_stamps, set_payload_merges_and_leaves_the_vector,
            touch_counts_only_retrievals, stale_candidates_need_a_stamp,
            resurface_offers_the_old_and_unseen, apply_lifecycle_writes_a_batch,
            facets_count_categories, neighbours_exclude_the_artifact_itself,
            context_sets_match_on_the_nearest_situation, deletes_take_everything_with_them,
            a_sample_is_capped_and_repeatable
        );
    };
    (@each $make:expr; $($name:ident),+) => {
        $(
            #[tokio::test]
            async fn $name() {
                let held = $make.await;
                $crate::vector::conformance::$name(&*held).await;
            }
        )+
    };
}
pub(crate) use suite;
