# Contained mode, part 1: SqliteVectors — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `VectorStore` that lives in a SQLite file, does hybrid retrieval and the scoring formula itself, and carries engram's whole ingest-to-search path with no Qdrant.

**Architecture:** `SqliteVectors` keeps points, dense vectors, BM25 postings and context sets in `vec_*` tables of one SQLite file, and ranks with sqlite-vec's `vec_distance_cosine`. What Qdrant does server-side — IDF, reciprocal rank fusion, recency decay, the pinned boost — is done in Rust over the rows. A conformance suite that every store must pass is written first, against `MemoryVectors`, and then held against the new store.

**Tech Stack:** Rust, sqlx 0.9 (SQLite, bundled through `libsqlite3-sys` 0.37), `sqlite-vec` 0.1.9 linked statically, tokio, async-trait.

**Spec:** `docs/superpowers/specs/2026-09-18-android-contained-mode-design.md` (section 2, "Vectors"; section 7, step 1). This is the first of seven plans, one per step of the spec's order. The next is written when this has landed.

## Global Constraints

- `sqlite-vec` is pinned exactly: `=0.1.9`. It is pre-1.0.
- Nothing is loaded from storage at runtime: sqlite-vec is registered with `sqlite3_auto_extension`, never `load_extension`.
- Everything new is behind the Cargo feature `contained`. A build without it is byte-for-byte today's server build. Every `cargo` command below that touches new code passes `--features contained`.
- Commit messages follow the repo's form: `feat(vector): …`, `test(vector): …`, and end with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`.
- Match the surrounding code's comment density and voice: comments say why, in full sentences.
- Work stays on the checked-out branch. Run targeted tests, not the full suite, before each commit.

## Where the spec was wrong, and what this plan does instead

Three things the spec assumed do not hold in the tree. The spec is corrected in Task 7.

1. **"Sparse vectors reuse the logic of `memory.rs`."** `MemoryVectors::search` is dense only; IDF, fusion and the recency formula all happen inside Qdrant. `SqliteVectors` implements them (Task 4), mirroring `qdrant.rs`: both halves retrieve `limit`, ranks fuse as `1 / (rank + 2)`, then `score + weight · 0.5^(age / half_life) + pinned_boost`.
2. **"The existing `VectorStore` test suite runs against all three."** There is no shared suite; the tests are inline in `memory.rs`. Task 1 creates one.
3. **"The `contained` feature leaves out Qdrant, OIDC, MCP, templates, CLI."** The crate has no such seams, and Qdrant is plain `reqwest`, not a dependency. `contained` starts as an *additive* feature. Carving the server-only parts out is deferred to plan 3, where the library's size can be measured and the cut justified.

## File Structure

- Create `src/vector/conformance.rs` — `#[cfg(test)]`. Behaviour every store must show, as async functions over `&dyn VectorStore`, and a macro that turns them into tests for one store.
- Create `src/vector/sqlite.rs` — `SqliteVectors`: connection, schema, every trait method. Its own tests cover only what `MemoryVectors` cannot show: hybrid fusion, the formula, persistence.
- Modify `src/vector/mod.rs` — declare the two modules.
- Modify `src/vector/memory.rs` — run the conformance suite.
- Modify `src/tenants.rs` — `SqliteFactory`.
- Modify `Cargo.toml` — the feature and two optional dependencies.
- Modify `src/core/mod.rs` — one end-to-end test in the existing test module.

---

### Task 1: The conformance suite, passing for MemoryVectors

**Files:**
- Create: `src/vector/conformance.rs`
- Modify: `src/vector/mod.rs:1-3`
- Modify: `src/vector/memory.rs` (test module, near line 513)

**Interfaces:**
- Produces: `crate::vector::conformance::{point, wide}` helpers, and the macro `crate::vector::conformance::suite!(make)` where `make` is an async expression yielding something that derefs to `dyn VectorStore`. Later tasks invoke `suite!` for `SqliteVectors`.

- [ ] **Step 1: Declare the module**

In `src/vector/mod.rs`, after `pub mod sparse;`:

```rust
#[cfg(test)]
pub(crate) mod conformance;
```

- [ ] **Step 2: Write the suite**

Create `src/vector/conformance.rs`:

```rust
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

pub fn point(id: &str, corpus: &str, v: [f32; DIM], tags: &[&str], cat: Option<&str>) -> VectorPoint {
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
    hits.iter().map(|h| h.payload.artifact_id.as_str()).collect()
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
    let hits = s.search(&[1.0, 0.0, 0.0], &none(), 10, &wide()).await.unwrap();
    assert_eq!(ids(&hits), ["exact", "near", "far"]);
    assert!((hits[0].similarity.unwrap() - 1.0).abs() < 1e-5);
}

pub async fn limit_is_respected(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert((0..5).map(|i| point(&format!("p{i}"), "c", [1.0, i as f32, 0.0], &[], None)).collect())
        .await
        .unwrap();
    assert_eq!(s.search(&[1.0, 0.0, 0.0], &none(), 2, &wide()).await.unwrap().len(), 2);
}

pub async fn zero_vectors_do_not_produce_nan(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(vec![point("zero", "c", [0.0, 0.0, 0.0], &[], None)]).await.unwrap();
    let hits = s.search(&[1.0, 0.0, 0.0], &none(), 10, &wide()).await.unwrap();
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
    let hits = s.search(&[1.0, 0.0, 0.0], &none(), 10, &wide()).await.unwrap();
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
    let tags = SearchFilter { tags: vec!["x".into(), "y".into()], ..wide() };
    assert_eq!(ids(&s.search(&q, &none(), 10, &tags).await.unwrap()), ["both"]);
    let cat = SearchFilter { category: Some("howto".into()), ..wide() };
    assert_eq!(ids(&s.search(&q, &none(), 10, &cat).await.unwrap()), ["one"]);
    let corpus = SearchFilter { corpus_id: Some("c2".into()), ..wide() };
    assert_eq!(ids(&s.search(&q, &none(), 10, &corpus).await.unwrap()), ["other"]);
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
    s.set_lifecycle("old", ArtifactStatus::Superseded, Some("live")).await.unwrap();
    s.set_lifecycle("stale", ArtifactStatus::Deprecated, None).await.unwrap();
    let q = [1.0, 0.0, 0.0];
    assert_eq!(ids(&s.search(&q, &none(), 10, &SearchFilter::default()).await.unwrap()), ["live"]);
    let with_old = SearchFilter { include_superseded: true, ..Default::default() };
    assert_eq!(ids(&s.search(&q, &none(), 10, &with_old).await.unwrap()), ["live", "old"]);
    let got = s.lifecycle_of(&["old".into(), "ghost".into()]).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got["old"].status, ArtifactStatus::Superseded);
    assert_eq!(got["old"].superseded_by.as_deref(), Some("live"));
}

pub async fn a_re_embed_keeps_the_stamps(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert(vec![point("a", "c", [1.0, 0.0, 0.0], &[], None)]).await.unwrap();
    s.touch(&[Touch::retrieved("a", None)], 500).await.unwrap();
    s.set_lifecycle("a", ArtifactStatus::Deprecated, None).await.unwrap();
    s.set_last_verified_at("a", 700, false).await.unwrap();
    // The embed job rebuilds the payload knowing none of this.
    s.upsert(vec![point("a", "c", [0.0, 1.0, 0.0], &[], None)]).await.unwrap();
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
    s.upsert(vec![point("a", "c", [1.0, 0.0, 0.0], &[], None)]).await.unwrap();
    s.touch(&[Touch::shown("a")], 10).await.unwrap();
    s.touch(&[Touch::retrieved("a", None)], 20).await.unwrap();
    s.touch(&[Touch::retrieved("a", Some(1))], 30).await.unwrap();
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
    s.set_last_verified_at("old-quiet", 100, false).await.unwrap();
    s.set_last_verified_at("old-busy", 100, false).await.unwrap();
    s.set_last_verified_at("fresh", 9_000, false).await.unwrap();
    for _ in 0..3 {
        s.touch(&[Touch::retrieved("old-busy", None)], 200).await.unwrap();
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
    s.upsert(vec![point("a", "c", [1.0, 0.0, 0.0], &[], None), point("b", "c", [1.0, 0.0, 0.0], &[], None)])
        .await
        .unwrap();
    s.touch(&[Touch::retrieved("a", None)], 5).await.unwrap();
    s.apply_lifecycle(&[
        LifecycleRow { artifact_id: "a".into(), status: ArtifactStatus::Superseded, superseded_by: Some("b".into()), last_verified_at: 77 },
        LifecycleRow { artifact_id: "ghost".into(), status: ArtifactStatus::Active, superseded_by: None, last_verified_at: 1 },
    ])
    .await
    .unwrap();
    let p = &s.payloads_of(&["a".into()]).await.unwrap()["a"];
    assert_eq!(p.status, Some(ArtifactStatus::Superseded));
    assert_eq!(p.superseded_by.as_deref(), Some("b"));
    assert_eq!(p.last_verified_at, Some(77));
    assert_eq!(p.hit_count, Some(1), "a bulk write must not wipe the counter");
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
    assert_eq!((f.categories[0].value.as_str(), f.categories[0].count), ("note", 2));
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
    s.set_context_vectors("two", vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]]).await.unwrap();
    s.set_context_vectors("hidden", vec![vec![0.0, 1.0, 0.0]]).await.unwrap();
    s.set_context_vectors("ghost", vec![vec![0.0, 1.0, 0.0]]).await.unwrap();
    s.set_lifecycle("hidden", ArtifactStatus::Deprecated, None).await.unwrap();
    let got = s.context_query(&[0.0, 1.0, 0.0], 10, &SearchFilter::default()).await.unwrap();
    assert_eq!(ids(&got), ["two"]);
    assert!((got[0].score - 1.0).abs() < 1e-5, "the mean of the set would be 0.707");
    assert_eq!(got[0].similarity, None);
    s.set_context_vectors("two", vec![]).await.unwrap();
    assert!(s.context_query(&[0.0, 1.0, 0.0], 10, &wide()).await.unwrap().iter().all(|h| h.payload.artifact_id != "two"));
    assert_eq!(s.count().await.unwrap(), 3, "an empty write leaves the point");
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
    s.set_context_vectors("a", vec![vec![1.0, 0.0, 0.0]]).await.unwrap();
    s.delete_artifacts(&["a".into()]).await.unwrap();
    assert!(s.context_query(&[1.0, 0.0, 0.0], 10, &wide()).await.unwrap().is_empty());
    assert_eq!(s.dense_of("a").await.unwrap(), None);
    s.delete_by_corpus("c1").await.unwrap();
    let mut left = s.all_artifact_ids().await.unwrap();
    left.sort();
    assert_eq!(left, ["c"]);
    assert_eq!(s.count().await.unwrap(), 1);
}

pub async fn a_sample_is_capped_and_repeatable(s: &dyn VectorStore) {
    s.ensure_collection(DIM).await.unwrap();
    s.upsert((0..4).map(|i| point(&format!("p{i}"), "c", [i as f32, 1.0, 0.0], &[], None)).collect())
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
```

- [ ] **Step 3: Run it for MemoryVectors**

At the end of the test module in `src/vector/memory.rs` (inside `mod tests`), add:

```rust
    mod conforms {
        crate::vector::conformance::suite!(async { Box::new(super::super::MemoryVectors::new()) });
    }
```

- [ ] **Step 4: Run the suite**

Run: `cargo test --lib vector::memory::tests::conforms`
Expected: 17 passed. A failure here is a disagreement between this suite and the store that defines the behaviour: read `memory.rs` for that method and correct the *suite*, never `memory.rs`. The likeliest candidates are `a_sample_is_capped_and_repeatable` (check how `sample` orders) and `stale_candidates_need_a_stamp` (check the comparison operators at `memory.rs:221`).

- [ ] **Step 5: Commit**

```bash
git add src/vector/conformance.rs src/vector/mod.rs src/vector/memory.rs
git commit -m "test(vector): one suite every store has to pass"
```

---

### Task 2: The `contained` feature, sqlite-vec, and the point tables

**Files:**
- Modify: `Cargo.toml` (`[dependencies]`, `[features]` at line 150)
- Modify: `src/vector/mod.rs`
- Create: `src/vector/sqlite.rs`

**Interfaces:**
- Produces: `SqliteVectors::connect(path: &std::path::Path, scoring: Scoring) -> Result<SqliteVectors>`, `SqliteVectors::memory(scoring) -> Result<SqliteVectors>` (tests), and `pub struct Scoring { pub recency: Recency, pub pinned_boost: f32 }` with `Scoring::off()`.

- [ ] **Step 1: Add the feature and dependencies**

In `Cargo.toml` under `[dependencies]`:

```toml
# The contained build's vector index: an extension compiled into the SQLite
# sqlx already bundles, registered at startup and never loaded from storage.
# Pre-1.0 and pinned exactly. `libsqlite3-sys` is named only for the symbol
# that registers it, and must stay on the version sqlx resolves.
sqlite-vec = { version = "=0.1.9", optional = true }
libsqlite3-sys = { version = "0.37", optional = true }
```

Under `[features]`:

```toml
contained = ["dep:sqlite-vec", "dep:libsqlite3-sys"]
```

In `src/vector/mod.rs`, after `pub mod sparse;`:

```rust
#[cfg(feature = "contained")]
pub mod sqlite;
```

Run: `cargo tree --features contained -i libsqlite3-sys | head -5`
Expected: one `libsqlite3-sys v0.37.x`, depended on by both `sqlx-sqlite` and `engram`. Two versions means the pin drifted from sqlx's; fix the version before going on.

- [ ] **Step 2: Write the failing test**

Create `src/vector/sqlite.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::VectorStore;
    use crate::vector::conformance::{point, wide, DIM};

    #[tokio::test]
    async fn what_was_written_is_there_after_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("base.db");
        {
            let s = SqliteVectors::connect(&path, Scoring::off()).await.unwrap();
            s.ensure_collection(DIM).await.unwrap();
            s.upsert(vec![point("a", "c", [1.0, 0.0, 0.0], &["t"], Some("note"))]).await.unwrap();
        }
        let s = SqliteVectors::connect(&path, Scoring::off()).await.unwrap();
        s.ensure_collection(DIM).await.unwrap();
        assert_eq!(s.count().await.unwrap(), 1);
        assert_eq!(s.dense_of("a").await.unwrap(), Some(vec![1.0, 0.0, 0.0]));
        let hits = s.search(&[1.0, 0.0, 0.0], &Default::default(), 5, &wide()).await.unwrap();
        assert_eq!(hits[0].payload.tags, ["t"]);
    }

    #[tokio::test]
    async fn a_base_embedded_at_another_width_is_refused() {
        let s = SqliteVectors::memory(Scoring::off()).await.unwrap();
        s.ensure_collection(3).await.unwrap();
        let err = s.ensure_collection(4).await.unwrap_err();
        assert!(err.to_string().contains("3"), "{err}");
    }
}
```

Run: `cargo test --features contained --lib vector::sqlite`
Expected: FAIL to compile, `SqliteVectors` not found.

- [ ] **Step 3: Write the store's skeleton and the point methods**

Above the test module in `src/vector/sqlite.rs`:

```rust
//! A `VectorStore` in a SQLite file, for an engram with no Qdrant beside it.
//!
//! Exact search: every query scans every vector, through sqlite-vec's
//! `vec_distance_cosine`. That is the right trade for a personal base — tens
//! of thousands of passages scan in milliseconds, and there is no index to
//! fall out of step with the rows. What Qdrant does on its side of the wire —
//! IDF over the sparse half, reciprocal rank fusion, the recency formula — is
//! done here in Rust, to the same arithmetic, so a base ranks the same in
//! either store.

use super::sparse::SparseVector;
use super::{
    FacetCount, Facets, LifecycleRow, Recency, SearchFilter, SearchHit, StoredLifecycle, Touch,
    VectorPayload, VectorPoint, VectorStore,
};
use crate::error::{Error, Result};
use crate::store::artifacts::ArtifactStatus;
use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::collections::HashMap;

/// The terms of the scoring stage that are fixed when the store is opened.
/// `recency` is the default a plain `search` uses; `search_weighted` names its
/// own.
#[derive(Debug, Clone, Copy)]
pub struct Scoring {
    pub recency: Recency,
    pub pinned_boost: f32,
}

impl Scoring {
    /// No recency, no boost: the fused rank is the score.
    pub fn off() -> Scoring {
        Scoring { recency: Recency { weight: 0.0, half_life_days: 1 }, pinned_boost: 0.0 }
    }
}

pub struct SqliteVectors {
    pool: sqlx::SqlitePool,
    scoring: Scoring,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS vec_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS vec_points (
    artifact_id      TEXT PRIMARY KEY,
    corpus_id        TEXT NOT NULL,
    category         TEXT,
    status           TEXT NOT NULL DEFAULT 'active',
    created_at       INTEGER NOT NULL,
    last_seen_at     INTEGER,
    hit_count        INTEGER,
    last_verified_at INTEGER,
    payload          TEXT NOT NULL,
    embedding        BLOB NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS vec_points_corpus ON vec_points(corpus_id);
CREATE TABLE IF NOT EXISTS vec_tags (
    artifact_id TEXT NOT NULL REFERENCES vec_points(artifact_id) ON DELETE CASCADE,
    tag         TEXT NOT NULL,
    PRIMARY KEY (artifact_id, tag)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS vec_sparse (
    term        INTEGER NOT NULL,
    artifact_id TEXT NOT NULL REFERENCES vec_points(artifact_id) ON DELETE CASCADE,
    value       REAL NOT NULL,
    PRIMARY KEY (term, artifact_id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS vec_ctx (
    artifact_id TEXT NOT NULL REFERENCES vec_points(artifact_id) ON DELETE CASCADE,
    n           INTEGER NOT NULL,
    embedding   BLOB NOT NULL,
    PRIMARY KEY (artifact_id, n)
) WITHOUT ROWID;
";

/// sqlite-vec, made part of every connection this process opens from here on.
/// An auto-extension rather than `load_extension`: nothing is read from
/// storage, which is what lets a hardened Android forbid that outright.
fn register() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        libsqlite3_sys::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

fn blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn unblob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn status_word(s: ArtifactStatus) -> &'static str {
    match s {
        ArtifactStatus::Active => "active",
        ArtifactStatus::Deprecated => "deprecated",
        ArtifactStatus::Superseded => "superseded",
    }
}

fn decode(payload: &str) -> Result<VectorPayload> {
    serde_json::from_str(payload).map_err(|e| Error::Vector(format!("stored payload: {e}")))
}

fn encode(p: &VectorPayload) -> String {
    // The stamps are `skip_serializing_if` for Qdrant's merge semantics; here
    // the JSON is the whole truth, so an absent key simply reads back as None.
    serde_json::to_string(p).expect("a payload is plain data")
}

impl SqliteVectors {
    pub async fn connect(path: &std::path::Path, scoring: Scoring) -> Result<SqliteVectors> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        Self::open(opts, 2, scoring).await
    }

    /// One connection, because every connection to `:memory:` is its own base.
    pub async fn memory(scoring: Scoring) -> Result<SqliteVectors> {
        let opts = SqliteConnectOptions::new().in_memory(true).foreign_keys(true);
        Self::open(opts, 1, scoring).await
    }

    async fn open(opts: SqliteConnectOptions, conns: u32, scoring: Scoring) -> Result<SqliteVectors> {
        register();
        let pool = SqlitePoolOptions::new().max_connections(conns).connect_with(opts).await?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        Ok(SqliteVectors { pool, scoring })
    }

    /// Read a stored payload with the columns that are the authority for the
    /// stamps laid over it.
    fn hydrate(row: &sqlx::sqlite::SqliteRow) -> Result<VectorPayload> {
        let mut p = decode(row.get::<&str, _>("payload"))?;
        p.last_seen_at = row.get("last_seen_at");
        p.hit_count = row.get("hit_count");
        p.last_verified_at = row.get("last_verified_at");
        Ok(p)
    }

    async fn write_point(
        tx: &mut sqlx::SqliteConnection,
        p: &VectorPayload,
        embedding: Option<&[f32]>,
    ) -> Result<()> {
        let status = status_word(p.status.unwrap_or(ArtifactStatus::Active));
        match embedding {
            Some(v) => {
                sqlx::query(
                    "INSERT INTO vec_points (artifact_id, corpus_id, category, status, created_at,
                        last_seen_at, hit_count, last_verified_at, payload, embedding)
                     VALUES (?,?,?,?,?,?,?,?,?,?)
                     ON CONFLICT(artifact_id) DO UPDATE SET corpus_id=excluded.corpus_id,
                        category=excluded.category, status=excluded.status, created_at=excluded.created_at,
                        last_seen_at=excluded.last_seen_at, hit_count=excluded.hit_count,
                        last_verified_at=excluded.last_verified_at, payload=excluded.payload,
                        embedding=excluded.embedding",
                )
                .bind(&p.artifact_id).bind(&p.corpus_id).bind(&p.category).bind(status)
                .bind(p.created_at).bind(p.last_seen_at).bind(p.hit_count).bind(p.last_verified_at)
                .bind(encode(p)).bind(blob(v))
                .execute(&mut *tx).await?;
            }
            None => {
                sqlx::query(
                    "UPDATE vec_points SET corpus_id=?, category=?, status=?, created_at=?,
                        last_seen_at=?, hit_count=?, last_verified_at=?, payload=? WHERE artifact_id=?",
                )
                .bind(&p.corpus_id).bind(&p.category).bind(status).bind(p.created_at)
                .bind(p.last_seen_at).bind(p.hit_count).bind(p.last_verified_at)
                .bind(encode(p)).bind(&p.artifact_id)
                .execute(&mut *tx).await?;
            }
        }
        sqlx::query("DELETE FROM vec_tags WHERE artifact_id=?").bind(&p.artifact_id).execute(&mut *tx).await?;
        for tag in &p.tags {
            sqlx::query("INSERT OR IGNORE INTO vec_tags (artifact_id, tag) VALUES (?,?)")
                .bind(&p.artifact_id).bind(tag).execute(&mut *tx).await?;
        }
        Ok(())
    }

    async fn stored(tx: &mut sqlx::SqliteConnection, id: &str) -> Result<Option<VectorPayload>> {
        let row = sqlx::query("SELECT payload, last_seen_at, hit_count, last_verified_at FROM vec_points WHERE artifact_id=?")
            .bind(id).fetch_optional(&mut *tx).await?;
        row.as_ref().map(Self::hydrate).transpose()
    }
}
```

Then the trait implementation. Task 2 fills in these methods; every other method is written in the task named, and until then is `todo!("task N")` so the file compiles:

```rust
#[async_trait]
impl VectorStore for SqliteVectors {
    async fn ensure_collection(&self, dim: usize) -> Result<()> {
        let have: Option<String> = sqlx::query_scalar("SELECT value FROM vec_meta WHERE key='dim'")
            .fetch_optional(&self.pool).await?;
        match have.and_then(|d| d.parse::<usize>().ok()) {
            Some(d) if d == dim => Ok(()),
            Some(d) => Err(Error::Vector(format!(
                "this base was embedded at {d} dimensions and the embedder now gives {dim}"
            ))),
            None => {
                sqlx::query("INSERT OR REPLACE INTO vec_meta (key, value) VALUES ('dim', ?)")
                    .bind(dim.to_string()).execute(&self.pool).await?;
                Ok(())
            }
        }
    }

    async fn upsert(&self, points: Vec<VectorPoint>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for mut p in points {
            // The same rule as the other stores: a re-embed rebuilds the
            // payload knowing none of the stamps, and an unset one means
            // "whatever is stored", never "clear it".
            if let Some(old) = Self::stored(&mut tx, &p.payload.artifact_id).await? {
                p.payload.last_seen_at = p.payload.last_seen_at.or(old.last_seen_at);
                p.payload.hit_count = p.payload.hit_count.or(old.hit_count);
                p.payload.status = p.payload.status.or(old.status);
                p.payload.last_verified_at = p.payload.last_verified_at.or(old.last_verified_at);
                p.payload.superseded_by = p.payload.superseded_by.or(old.superseded_by);
            }
            Self::write_point(&mut tx, &p.payload, Some(&p.vector)).await?;
            sqlx::query("DELETE FROM vec_sparse WHERE artifact_id=?")
                .bind(&p.payload.artifact_id).execute(&mut *tx).await?;
            for (term, value) in p.sparse.indices.iter().zip(&p.sparse.values) {
                sqlx::query("INSERT OR REPLACE INTO vec_sparse (term, artifact_id, value) VALUES (?,?,?)")
                    .bind(*term as i64).bind(&p.payload.artifact_id).bind(*value)
                    .execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    async fn dense_of(&self, artifact_id: &str) -> Result<Option<Vec<f32>>> {
        let b: Option<Vec<u8>> = sqlx::query_scalar("SELECT embedding FROM vec_points WHERE artifact_id=?")
            .bind(artifact_id).fetch_optional(&self.pool).await?;
        Ok(b.map(|b| unblob(&b)))
    }

    async fn count(&self) -> Result<u64> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vec_points").fetch_one(&self.pool).await?;
        Ok(n as u64)
    }

    // search: a dense-only version for now; Task 4 replaces it with the hybrid one.
    async fn search(&self, vector: &[f32], _sparse: &SparseVector, limit: usize, filter: &SearchFilter) -> Result<Vec<SearchHit>> {
        self.dense(vector, limit, filter, None).await
    }
    // … every remaining method: `todo!("task 3")`, except `set_context_vectors`
    // and `context_query`: `todo!("task 5")`.
}
```

And the dense query with its filter, as inherent methods (Task 4 and 5 reuse both):

```rust
impl SqliteVectors {
    /// The WHERE clause a filter amounts to over `vec_points p`, and its binds
    /// in order. Built as text because the tag count varies; every value is
    /// bound, never spliced.
    fn where_clause(filter: &SearchFilter) -> (String, Vec<String>) {
        let mut sql = String::from("1=1");
        let mut binds = Vec::new();
        if !filter.include_superseded { sql.push_str(" AND p.status <> 'superseded'"); }
        if !filter.include_deprecated { sql.push_str(" AND p.status <> 'deprecated'"); }
        if let Some(c) = &filter.category { sql.push_str(" AND p.category = ?"); binds.push(c.clone()); }
        if let Some(c) = &filter.corpus_id { sql.push_str(" AND p.corpus_id = ?"); binds.push(c.clone()); }
        for t in &filter.tags {
            sql.push_str(" AND EXISTS (SELECT 1 FROM vec_tags t WHERE t.artifact_id = p.artifact_id AND t.tag = ?)");
            binds.push(t.clone());
        }
        (sql, binds)
    }

    /// The nearest `limit` points by cosine. `except` leaves one artifact out,
    /// which is how `neighbours` keeps a point from being its own neighbour.
    async fn dense(&self, vector: &[f32], limit: usize, filter: &SearchFilter, except: Option<&str>) -> Result<Vec<SearchHit>> {
        let (clause, binds) = Self::where_clause(filter);
        let sql = format!(
            "SELECT p.payload, p.last_seen_at, p.hit_count, p.last_verified_at,
                    vec_distance_cosine(p.embedding, ?) AS dist
             FROM vec_points p
             WHERE length(p.embedding) = ? AND p.artifact_id IS NOT ? AND {clause}
             ORDER BY dist IS NULL, dist, p.artifact_id LIMIT ?"
        );
        let mut q = sqlx::query(&sql).bind(blob(vector)).bind((vector.len() * 4) as i64).bind(except);
        for b in &binds { q = q.bind(b); }
        let rows = q.bind(limit as i64).fetch_all(&self.pool).await?;
        rows.iter().map(|r| {
            // A zero vector has no direction: sqlite-vec answers NULL or NaN,
            // and `cosine()` in `mod.rs` answers 0.0. Agree with `cosine()`.
            let similarity = r.get::<Option<f64>, _>("dist").map(|d| 1.0 - d as f32).filter(|s| s.is_finite()).unwrap_or(0.0);
            Ok(SearchHit { payload: Self::hydrate(r)?, score: similarity, similarity: Some(similarity) })
        }).collect()
    }
}
```

`length(p.embedding) = ?` is the width guard `cosine()` documents: a vector of another width is not in the same space, and sqlite-vec would raise an error for the whole query rather than skip the row.

- [ ] **Step 4: Run the two tests**

Run: `cargo test --features contained --lib vector::sqlite`
Expected: 2 passed. If `sqlite3_vec_init` is not found at link time, the `sqlite-vec` crate did not build its C source for this target: run `cargo build --features contained -vv 2>&1 | grep -i sqlite-vec` and read the `cc` invocation.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/vector/mod.rs src/vector/sqlite.rs
git commit -m "feat(vector): a store in a SQLite file, behind the contained feature"
```

---

### Task 3: The payload, lifecycle and listing methods

**Files:**
- Modify: `src/vector/sqlite.rs`

**Interfaces:**
- Consumes: `write_point`, `stored`, `hydrate`, `where_clause`, `dense` from Task 2.
- Produces: every `VectorStore` method except the two context ones.

- [ ] **Step 1: Turn the conformance suite on, minus context**

The macro runs all seventeen, and two need Task 5. Add to the test module of `src/vector/sqlite.rs`:

```rust
    mod conforms {
        crate::vector::conformance::suite!(async {
            Box::new(crate::vector::sqlite::SqliteVectors::memory(crate::vector::sqlite::Scoring::off()).await.unwrap())
        });
    }
```

Run: `cargo test --features contained --lib vector::sqlite::tests::conforms`
Expected: most FAIL with `not yet implemented: task 3`; `search_ranks_by_cosine`, `limit_is_respected`, `zero_vectors_do_not_produce_nan`, `equal_scores_order_by_id` and `filters_narrow` already PASS.

- [ ] **Step 2: Implement the methods**

Replace the `todo!("task 3")` bodies:

```rust
    async fn set_payload(&self, payload: &VectorPayload) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let Some(old) = Self::stored(&mut tx, &payload.artifact_id).await? else { return Ok(()) };
        let mut p = payload.clone();
        p.last_seen_at = p.last_seen_at.or(old.last_seen_at);
        p.hit_count = p.hit_count.or(old.hit_count);
        p.status = p.status.or(old.status);
        p.last_verified_at = p.last_verified_at.or(old.last_verified_at);
        p.superseded_by = p.superseded_by.or(old.superseded_by);
        // Only the embed job knows these two; an edit that leaves them empty
        // means "unchanged", as it does in the other stores.
        if p.origin_corpora.is_empty() { p.origin_corpora = old.origin_corpora; }
        p.provenance = p.provenance.or(old.provenance);
        Self::write_point(&mut tx, &p, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn set_lifecycle(&self, artifact_id: &str, status: ArtifactStatus, superseded_by: Option<&str>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let Some(mut p) = Self::stored(&mut tx, artifact_id).await? else { return Ok(()) };
        p.status = Some(status);
        p.superseded_by = superseded_by.map(str::to_string);
        Self::write_point(&mut tx, &p, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn set_last_verified_at(&self, artifact_id: &str, at: i64, reset_hits: bool) -> Result<()> {
        sqlx::query("UPDATE vec_points SET last_verified_at=?, hit_count=CASE WHEN ? THEN 0 ELSE hit_count END WHERE artifact_id=?")
            .bind(at).bind(reset_hits).bind(artifact_id).execute(&self.pool).await?;
        Ok(())
    }

    async fn apply_lifecycle(&self, rows: &[LifecycleRow]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for r in rows {
            let Some(mut p) = Self::stored(&mut tx, &r.artifact_id).await? else { continue };
            p.status = Some(r.status);
            p.superseded_by = r.superseded_by.clone();
            p.last_verified_at = Some(r.last_verified_at);
            Self::write_point(&mut tx, &p, None).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn touch(&self, targets: &[Touch], seen_at: i64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for t in targets {
            if t.counts_as_hit {
                // The caller's count when it has one, as the other stores
                // honour it; the stored one otherwise.
                sqlx::query("UPDATE vec_points SET last_seen_at=?, hit_count=COALESCE(?, hit_count, 0) + 1 WHERE artifact_id=?")
                    .bind(seen_at).bind(t.hit_count).bind(&t.artifact_id).execute(&mut *tx).await?;
            } else {
                sqlx::query("UPDATE vec_points SET last_seen_at=? WHERE artifact_id=?")
                    .bind(seen_at).bind(&t.artifact_id).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    async fn lifecycle_of(&self, artifact_ids: &[String]) -> Result<HashMap<String, StoredLifecycle>> {
        Ok(self.payloads_of(artifact_ids).await?.into_iter().map(|(id, p)| {
            (id, StoredLifecycle { status: p.status.unwrap_or(ArtifactStatus::Active), superseded_by: p.superseded_by })
        }).collect())
    }

    async fn payloads_of(&self, artifact_ids: &[String]) -> Result<HashMap<String, VectorPayload>> {
        let mut out = HashMap::new();
        let mut conn = self.pool.acquire().await?;
        for id in artifact_ids {
            if let Some(p) = Self::stored(&mut conn, id).await? { out.insert(id.clone(), p); }
        }
        Ok(out)
    }

    async fn all_artifact_ids(&self) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar("SELECT artifact_id FROM vec_points").fetch_all(&self.pool).await?)
    }

    async fn stale_candidates(&self, older_than: i64, max_hits: i64, limit: usize) -> Result<Vec<SearchHit>> {
        let rows = sqlx::query(
            "SELECT payload, last_seen_at, hit_count, last_verified_at FROM vec_points
             WHERE status='active' AND last_verified_at IS NOT NULL AND last_verified_at < ?
               AND COALESCE(hit_count, 0) <= ?
             ORDER BY last_verified_at, artifact_id LIMIT ?")
            .bind(older_than).bind(max_hits).bind(limit as i64).fetch_all(&self.pool).await?;
        rows.iter().map(|r| Ok(SearchHit { payload: Self::hydrate(r)?, score: 0.0, similarity: None })).collect()
    }

    async fn resurface(&self, limit: usize, older_than: i64, unseen_since: i64) -> Result<Vec<SearchHit>> {
        let rows = sqlx::query(
            "SELECT payload, last_seen_at, hit_count, last_verified_at FROM vec_points
             WHERE status='active' AND created_at < ? AND (last_seen_at IS NULL OR last_seen_at < ?)
             ORDER BY random() LIMIT ?")
            .bind(older_than).bind(unseen_since).bind(limit as i64).fetch_all(&self.pool).await?;
        rows.iter().map(|r| Ok(SearchHit { payload: Self::hydrate(r)?, score: 0.0, similarity: None })).collect()
    }

    async fn facets(&self, limit: usize) -> Result<Facets> {
        let rows = sqlx::query("SELECT category, COUNT(*) AS n FROM vec_points WHERE category IS NOT NULL GROUP BY category ORDER BY n DESC, category LIMIT ?")
            .bind(limit as i64).fetch_all(&self.pool).await?;
        Ok(Facets { categories: rows.iter().map(|r| FacetCount { value: r.get("category"), count: r.get::<i64, _>("n") as u64 }).collect() })
    }

    async fn neighbours(&self, artifact_id: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let Some(v) = self.dense_of(artifact_id).await? else { return Ok(vec![]) };
        self.dense(&v, limit, &SearchFilter::default(), Some(artifact_id)).await
    }

    async fn delete_artifacts(&self, artifact_ids: &[String]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for id in artifact_ids {
            // The tags, postings and context set go with it: ON DELETE CASCADE.
            sqlx::query("DELETE FROM vec_points WHERE artifact_id=?").bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn delete_by_corpus(&self, corpus_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM vec_points WHERE corpus_id=?").bind(corpus_id).execute(&self.pool).await?;
        Ok(())
    }

    async fn sample(&self, limit: usize) -> Result<Vec<(String, Vec<f32>)>> {
        let rows = sqlx::query("SELECT artifact_id, embedding FROM vec_points ORDER BY artifact_id LIMIT ?")
            .bind(limit as i64).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| (r.get("artifact_id"), unblob(r.get::<&[u8], _>("embedding")))).collect())
    }
```

Before running, open `memory.rs` at each of `stale_candidates` (221), `resurface` (280) and `neighbours` (370) and check three things against the SQL above, changing the SQL to agree where they differ: which statuses each admits, whether the age comparisons are strict, and which filter `neighbours` searches under.

- [ ] **Step 3: Run the suite**

Run: `cargo test --features contained --lib vector::sqlite::tests::conforms`
Expected: 15 passed; `context_sets_match_on_the_nearest_situation` and `deletes_take_everything_with_them` FAIL with `not yet implemented: task 5`.

- [ ] **Step 4: Commit**

```bash
git add src/vector/sqlite.rs
git commit -m "feat(vector): payloads, lifecycle and the listings, in SQLite"
```

---

### Task 4: Hybrid retrieval and the scoring formula

**Files:**
- Modify: `src/vector/sqlite.rs`

**Interfaces:**
- Consumes: `dense`, `where_clause`, `hydrate`, `Scoring`.
- Produces: the final `search`, `search_weighted`, `applies_scoring_formula() == true`.

- [ ] **Step 1: Write the failing tests**

In the test module of `src/vector/sqlite.rs`:

```rust
    use crate::vector::sparse::{encode_document, encode_query};
    use crate::vector::{Recency, SearchFilter};

    fn worded(id: &str, v: [f32; 3], text: &str) -> crate::vector::VectorPoint {
        let mut p = point(id, "c", v, &[], None);
        p.payload.text = text.into();
        p.sparse = encode_document(text);
        p
    }

    #[tokio::test]
    async fn an_exact_term_lifts_a_passage_the_dense_half_ranked_last() {
        let s = SqliteVectors::memory(Scoring::off()).await.unwrap();
        s.ensure_collection(3).await.unwrap();
        s.upsert(vec![
            worded("near", [1.0, 0.0, 0.0], "a note about nothing in particular"),
            worded("mid", [0.9, 0.4, 0.0], "another note about nothing"),
            worded("term", [0.0, 1.0, 0.0], "run it with --dry-run first"),
        ]).await.unwrap();
        let q = [1.0, 0.0, 0.0];
        let dense_only = s.search(&q, &Default::default(), 3, &wide()).await.unwrap();
        assert_eq!(dense_only[2].payload.artifact_id, "term");
        let hybrid = s.search(&q, &encode_query("dry-run"), 3, &wide()).await.unwrap();
        // Last of three in the dense half (1/4), first in the lexical (1/2):
        // 0.75, against 0.5 for the dense winner the lexical half never saw.
        assert_eq!(hybrid[0].payload.artifact_id, "term");
        assert!((hybrid[0].score - 0.75).abs() < 1e-5, "{}", hybrid[0].score);
        assert!(hybrid[0].similarity.is_some());
    }

    #[tokio::test]
    async fn a_lexical_only_hit_has_no_similarity() {
        let s = SqliteVectors::memory(Scoring::off()).await.unwrap();
        s.ensure_collection(3).await.unwrap();
        s.upsert(vec![
            worded("a", [1.0, 0.0, 0.0], "plain"),
            worded("b", [0.9, 0.1, 0.0], "plain too"),
            worded("far", [0.0, 0.0, 1.0], "the zugzwang entry"),
        ]).await.unwrap();
        // Each half retrieves `limit`; at 2 the dense half never reaches `far`.
        let hits = s.search(&[1.0, 0.0, 0.0], &encode_query("zugzwang"), 2, &wide()).await.unwrap();
        let far = hits.iter().find(|h| h.payload.artifact_id == "far").expect("the lexical half found it");
        assert_eq!(far.similarity, None);
    }

    #[tokio::test]
    async fn a_rare_term_outweighs_a_common_one() {
        let s = SqliteVectors::memory(Scoring::off()).await.unwrap();
        s.ensure_collection(3).await.unwrap();
        let mut pts: Vec<_> = (0..6).map(|i| worded(&format!("c{i}"), [0.0, 0.0, 1.0], "common words")).collect();
        pts.push(worded("rare", [0.0, 0.0, 1.0], "common quetzal"));
        s.upsert(pts).await.unwrap();
        let hits = s.search(&[0.0, 1.0, 0.0], &encode_query("common quetzal"), 7, &wide()).await.unwrap();
        assert_eq!(hits[0].payload.artifact_id, "rare");
    }

    #[tokio::test]
    async fn recency_breaks_a_tie_and_pinning_beats_recency() {
        let day = 86_400;
        let now = crate::store::now();
        let scoring = Scoring { recency: Recency { weight: 0.05, half_life_days: 30 }, pinned_boost: 0.15 };
        let s = SqliteVectors::memory(scoring).await.unwrap();
        s.ensure_collection(3).await.unwrap();
        s.upsert(vec![
            point("old", "c", [1.0, 0.0, 0.0], &[], None),
            point("new", "c", [1.0, 0.0, 0.0], &[], None),
            point("pinned", "c", [1.0, 0.0, 0.0], &["pinned"], None),
        ]).await.unwrap();
        s.set_last_verified_at("old", now - 300 * day, false).await.unwrap();
        s.set_last_verified_at("new", now, false).await.unwrap();
        s.set_last_verified_at("pinned", now - 300 * day, false).await.unwrap();
        let hits = s.search(&[1.0, 0.0, 0.0], &Default::default(), 3, &wide()).await.unwrap();
        let order: Vec<_> = hits.iter().map(|h| h.payload.artifact_id.as_str()).collect();
        assert_eq!(order, ["pinned", "new", "old"]);
        assert!(s.applies_scoring_formula());
        // The similarity is the cosine, untouched by either term.
        assert!(hits.iter().all(|h| (h.similarity.unwrap() - 1.0).abs() < 1e-5));
        // And a caller may turn the weight off for one call.
        let flat = s.search_weighted(&[1.0, 0.0, 0.0], &Default::default(), 3, &wide(), Recency { weight: 0.0, half_life_days: 30 }).await.unwrap();
        assert_eq!(flat[1].payload.artifact_id, "new");
        assert_eq!(flat[2].payload.artifact_id, "old", "equal scores fall back to the id");
    }
```

The clock is `crate::store::now()` (`src/store/mod.rs:493`), seconds since the epoch.

Run: `cargo test --features contained --lib vector::sqlite::tests`
Expected: the four new tests FAIL (the first on the assertion that `term` leads).

- [ ] **Step 2: Implement**

Replace `search` in the trait impl, and add `search_weighted` and `applies_scoring_formula`:

```rust
    async fn search(&self, vector: &[f32], sparse: &SparseVector, limit: usize, filter: &SearchFilter) -> Result<Vec<SearchHit>> {
        self.search_weighted(vector, sparse, limit, filter, self.scoring.recency).await
    }

    async fn search_weighted(&self, vector: &[f32], sparse: &SparseVector, limit: usize, filter: &SearchFilter, recency: Recency) -> Result<Vec<SearchHit>> {
        let dense = self.dense(vector, limit, filter, None).await?;
        let mut hits = if sparse.is_empty() {
            // No indexable term: the dense order stands and its cosine is the
            // score, as in `qdrant.rs`'s unfused branch.
            dense
        } else {
            let lexical = self.lexical(sparse, limit, filter).await?;
            fuse(dense, lexical)
        };
        self.apply_formula(&mut hits, recency);
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.payload.artifact_id.cmp(&b.payload.artifact_id)));
        hits.truncate(limit);
        Ok(hits)
    }

    fn applies_scoring_formula(&self) -> bool {
        true
    }
```

And as inherent methods and a free function:

```rust
pub const PINNED_TAG: &str = super::qdrant::PINNED_TAG;

/// Reciprocal rank fusion as Qdrant computes it: `1 / (rank + 2)` per half,
/// ranks from zero, summed. A hit only the lexical half returned keeps no
/// similarity — an exact term match is not a weak result, and `None` is how
/// `SearchHit` says "no opinion".
fn fuse(dense: Vec<SearchHit>, lexical: Vec<SearchHit>) -> Vec<SearchHit> {
    let mut by_id: HashMap<String, SearchHit> = HashMap::new();
    for (rank, mut h) in dense.into_iter().enumerate() {
        h.score = 1.0 / (rank as f32 + 2.0);
        by_id.insert(h.payload.artifact_id.clone(), h);
    }
    for (rank, mut h) in lexical.into_iter().enumerate() {
        let part = 1.0 / (rank as f32 + 2.0);
        match by_id.get_mut(&h.payload.artifact_id) {
            Some(seen) => seen.score += part,
            None => {
                h.score = part;
                h.similarity = None;
                by_id.insert(h.payload.artifact_id.clone(), h);
            }
        }
    }
    by_id.into_values().collect()
}

impl SqliteVectors {
    /// The lexical half: BM25 with the IDF Qdrant's `idf` modifier applies,
    /// `ln((N - n + 0.5) / (n + 0.5) + 1)`. The document side already carries
    /// saturated term frequencies (`sparse::encode_document`), so a score is a
    /// dot product with the IDF folded into the query weight.
    async fn lexical(&self, sparse: &SparseVector, limit: usize, filter: &SearchFilter) -> Result<Vec<SearchHit>> {
        let total = self.count().await? as f32;
        let (clause, binds) = Self::where_clause(filter);
        let mut scores: HashMap<String, f32> = HashMap::new();
        for (term, qv) in sparse.indices.iter().zip(&sparse.values) {
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vec_sparse WHERE term=?")
                .bind(*term as i64).fetch_one(&self.pool).await?;
            if n == 0 { continue; }
            let idf = ((total - n as f32 + 0.5) / (n as f32 + 0.5) + 1.0).ln();
            let sql = format!("SELECT s.artifact_id, s.value FROM vec_sparse s JOIN vec_points p ON p.artifact_id = s.artifact_id WHERE s.term = ? AND {clause}");
            let mut q = sqlx::query(&sql).bind(*term as i64);
            for b in &binds { q = q.bind(b); }
            for r in q.fetch_all(&self.pool).await? {
                *scores.entry(r.get("artifact_id")).or_insert(0.0) += qv * idf * r.get::<f64, _>("value") as f32;
            }
        }
        let mut ranked: Vec<(String, f32)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(limit);
        let ids: Vec<String> = ranked.iter().map(|(id, _)| id.clone()).collect();
        let mut payloads = self.payloads_of(&ids).await?;
        Ok(ranked.into_iter().filter_map(|(id, score)| {
            payloads.remove(&id).map(|payload| SearchHit { payload, score, similarity: None })
        }).collect())
    }

    /// `score + weight · 0.5^(age / half_life) + pinned_boost`, the formula of
    /// `qdrant.rs::scoring_formula`. A point never verified decays from now,
    /// which is that formula's default and makes the term neutral.
    fn apply_formula(&self, hits: &mut [SearchHit], recency: Recency) {
        let now = crate::store::now();
        let half_life = (recency.half_life_days.max(1) as f32) * 86_400.0;
        for h in hits {
            if recency.weight > 0.0 {
                let age = (now - h.payload.last_verified_at.unwrap_or(now)).max(0) as f32;
                h.score += recency.weight * 0.5f32.powf(age / half_life);
            }
            if self.scoring.pinned_boost > 0.0 && h.payload.tags.iter().any(|t| t == PINNED_TAG) {
                h.score += self.scoring.pinned_boost;
            }
        }
    }
}
```

`fuse` keeps the dense hit's `similarity` for anything the dense half returned, which is why `an_exact_term_lifts…` asserts `is_some()`.

- [ ] **Step 3: Run**

Run: `cargo test --features contained --lib vector::sqlite`
Expected: every test passes except the two conformance cases waiting on Task 5. If the conformance ranking cases now fail, the unfused branch is not being taken: they pass an empty `SparseVector`.

- [ ] **Step 4: Commit**

```bash
git add src/vector/sqlite.rs
git commit -m "feat(vector): fusion, IDF and the scoring formula, done where Qdrant is not"
```

---

### Task 5: Context sets

**Files:**
- Modify: `src/vector/sqlite.rs`

**Interfaces:**
- Consumes: `where_clause`, `hydrate`, `blob`.
- Produces: `set_context_vectors`, `context_query`; the conformance suite passes whole.

- [ ] **Step 1: Confirm the failing tests**

Run: `cargo test --features contained --lib vector::sqlite::tests::conforms`
Expected: exactly 2 FAIL, both `not yet implemented: task 5`.

- [ ] **Step 2: Implement**

```rust
    async fn set_context_vectors(&self, artifact_id: &str, vectors: Vec<Vec<f32>>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let there: Option<i64> = sqlx::query_scalar("SELECT 1 FROM vec_points WHERE artifact_id=?")
            .bind(artifact_id).fetch_optional(&mut *tx).await?;
        // No point, nothing to attach a set to: its embedding may never have
        // run. Not an error, by the trait's word.
        if there.is_none() { return Ok(()); }
        sqlx::query("DELETE FROM vec_ctx WHERE artifact_id=?").bind(artifact_id).execute(&mut *tx).await?;
        for (n, v) in vectors.iter().enumerate() {
            sqlx::query("INSERT INTO vec_ctx (artifact_id, n, embedding) VALUES (?,?,?)")
                .bind(artifact_id).bind(n as i64).bind(blob(v)).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn context_query(&self, vector: &[f32], limit: usize, filter: &SearchFilter) -> Result<Vec<SearchHit>> {
        let (clause, binds) = Self::where_clause(filter);
        // `max_sim`: an artifact matches on its nearest situation, so the
        // smallest distance in its set is the one that speaks for it.
        let sql = format!(
            "SELECT p.payload, p.last_seen_at, p.hit_count, p.last_verified_at,
                    MIN(vec_distance_cosine(c.embedding, ?)) AS dist
             FROM vec_ctx c JOIN vec_points p ON p.artifact_id = c.artifact_id
             WHERE length(c.embedding) = ? AND {clause}
             GROUP BY p.artifact_id ORDER BY dist, p.artifact_id LIMIT ?"
        );
        let mut q = sqlx::query(&sql).bind(blob(vector)).bind((vector.len() * 4) as i64);
        for b in &binds { q = q.bind(b); }
        let rows = q.bind(limit as i64).fetch_all(&self.pool).await?;
        rows.iter().map(|r| {
            let score = r.get::<Option<f64>, _>("dist").map(|d| 1.0 - d as f32).filter(|s| s.is_finite()).unwrap_or(0.0);
            Ok(SearchHit { payload: Self::hydrate(r)?, score, similarity: None })
        }).collect()
    }
```

- [ ] **Step 3: Run the whole file, then confirm nothing is left unwritten**

Run: `cargo test --features contained --lib vector::`
Expected: all pass, both stores.

Run: `grep -n "todo!" src/vector/sqlite.rs`
Expected: no output.

- [ ] **Step 4: Commit**

```bash
git add src/vector/sqlite.rs
git commit -m "feat(vector): context sets in SQLite; the store passes the whole suite"
```

---

### Task 6: The factory, and the whole path over it

**Files:**
- Modify: `src/tenants.rs` (after `QdrantFactory`, near line 50)
- Modify: `src/core/mod.rs` (test module, after `test_core`, near line 796)

**Interfaces:**
- Consumes: `SqliteVectors::connect`, `Scoring`.
- Produces: `crate::tenants::SqliteFactory { path: PathBuf, scoring: Scoring }`. Plan 3 constructs it in `Core.start` with the phone's one database file.

- [ ] **Step 1: Write the failing end-to-end test**

In `src/core/mod.rs`, in the test module that holds `test_core` users — add a new module at the end of the file:

```rust
#[cfg(all(test, feature = "contained"))]
mod contained_tests {
    use crate::core::test_support::test_core;
    use crate::store::feedback::Door;
    use std::sync::Arc;

    /// The whole of part 1 in one test: a capture goes in, the jobs run, and a
    /// search finds it — with no Qdrant anywhere, and again after a restart.
    #[tokio::test]
    async fn a_capture_is_found_again_with_only_a_file_for_a_vector_store() {
        use crate::tenants::VectorFactory;
        let dir = tempfile::tempdir().unwrap();
        let factory = crate::tenants::SqliteFactory {
            path: dir.path().join("engram.db"),
            scoring: crate::vector::sqlite::Scoring::off(),
        };
        let mut core = test_core().await;
        core.vectors = factory.open("ignored", crate::core::test_support::TEST_DIM).await.unwrap();

        let out = core.ingest("Die Rechnung für den Steuerberater liegt im blauen Ordner.", "web", None).await.unwrap();
        crate::jobs::test_support::drain(&core).await;

        let q = crate::core::search::SearchQuery {
            q: "Steuerberater".into(), limit: 5, tags: vec![], category: None, mark: true,
            rerank: false, explain: false, include_deprecated: false, include_superseded: false,
        };
        let hits = core.search(&q, Door::Cli).await.unwrap();
        assert!(hits.iter().any(|h| h.corpus_id == out.id), "the capture was not found");

        // A second store over the same file sees the same base.
        let again: Arc<dyn crate::vector::VectorStore> = factory.open("ignored", crate::core::test_support::TEST_DIM).await.unwrap();
        assert_eq!(again.count().await.unwrap(), core.vectors.count().await.unwrap());
        assert!(again.count().await.unwrap() > 0);
    }
}
```

`SearchQuery`'s field list is copied from the `q()` helper at `src/core/search.rs:2553`; if a field has been added since, the compiler names it, and it takes the value that helper gives it.

Run: `cargo test --features contained --lib core::contained_tests`
Expected: FAIL to compile, `SqliteFactory` not found.

- [ ] **Step 2: Write the factory**

In `src/tenants.rs`, after `impl VectorFactory for QdrantFactory`:

```rust
/// The contained build's: one file, one tenant.
///
/// The alias is not consulted. A phone holds one person's base, and the file
/// is the same one the `Store` lives in — the vectors are `vec_*` tables
/// beside the artifacts, so a base is one thing to keep rather than two.
#[cfg(feature = "contained")]
pub struct SqliteFactory {
    pub path: std::path::PathBuf,
    pub scoring: crate::vector::sqlite::Scoring,
}

#[cfg(feature = "contained")]
#[async_trait::async_trait]
impl VectorFactory for SqliteFactory {
    async fn open(&self, _alias: &str, dim: usize) -> Result<Arc<dyn crate::vector::VectorStore>> {
        let vectors: Arc<dyn crate::vector::VectorStore> =
            Arc::new(crate::vector::sqlite::SqliteVectors::connect(&self.path, self.scoring).await?);
        vectors.ensure_collection(dim).await?;
        Ok(vectors)
    }
}
```

- [ ] **Step 3: Run**

Run: `cargo test --features contained --lib core::contained_tests`
Expected: PASS. If `test_support::TEST_DIM` or `test_core` is not visible from the new module, they are `pub` inside `pub(crate) mod test_support` at `src/core/mod.rs:713`; adjust the path, not the visibility.

- [ ] **Step 4: Prove the server build is untouched**

Run: `cargo build && cargo test --lib vector::`
Expected: builds with no `sqlite-vec` in the output; the memory and Qdrant unit tests pass.

Run: `cargo clippy --features contained --all-targets -- -D warnings 2>&1 | tail -20`
Expected: no warnings from `src/vector/sqlite.rs`, `src/vector/conformance.rs` or `src/tenants.rs`.

- [ ] **Step 5: Commit**

```bash
git add src/tenants.rs src/core/mod.rs
git commit -m "feat(contained): ingest to search over one SQLite file, end to end"
```

---

### Task 7: Bring the spec into line

**Files:**
- Modify: `docs/superpowers/specs/2026-09-18-android-contained-mode-design.md`

- [ ] **Step 1: Correct section 2, "Vectors"**

Replace the sentence beginning "Dense vectors live in a sqlite-vec `vec0` table" through "The existing `VectorStore` test suite runs against all three implementations." with:

```markdown
`SqliteVectors` implements `VectorStore` beside `QdrantVectors` and
`MemoryVectors`. Points, dense vectors, BM25 postings and context sets are
`vec_*` tables inside `engram.db`; ranking is sqlite-vec's
`vec_distance_cosine` over every row, exact, with payload filters as plain
SQL. sqlite-vec is linked statically into the SQLite that sqlx bundles and
registered as an auto-extension, so nothing is loaded from storage at
runtime. It is pre-1.0; the version is pinned exactly.

What Qdrant does on its side of the wire, this store does in Rust to the same
arithmetic: IDF over the sparse half, reciprocal rank fusion of the two
halves, recency decay and the pinned boost. A base ranks the same in either.

One conformance suite, `src/vector/conformance.rs`, states what every store
must do, and runs against `MemoryVectors` and `SqliteVectors`.
```

- [ ] **Step 2: Correct section 1's last paragraph**

Replace "That feature set leaves out Qdrant, OIDC, MCP, the web templates and the CLI, and keeps the store, the core pipeline, the jobs and the JSON routes the app uses." with:

```markdown
The feature is additive: it brings in what the phone needs and takes nothing
out. Whether OIDC, MCP, the templates and the CLI are worth carving out of
the library is decided in step 3, against the measured size of the `.so`.
```

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-09-18-android-contained-mode-design.md
git commit -m "docs(android): the contained spec, corrected by what part 1 found"
```
