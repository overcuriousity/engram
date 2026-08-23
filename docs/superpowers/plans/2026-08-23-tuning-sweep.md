# Runtime Tuning Sweeps Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Judgements automatically buy background parameter sweeps over the live index; a gated recommendation appears on the judge page and one click applies it to `config.toml` and the running server. The judge page header becomes a cockpit that explains its numbers and ticks on every verdict.

**Architecture:** A `RankingParams` value (recency weight + per-source cap) lives in `Core` behind a lock; the live search path reads it and a new `search_with_ranking` accepts explicit candidate values — one pipeline serves live search, hot-swap, and measurement. The sweep is a background job over the live index (read-only, `Door::Judge`, never captured, no re-embedding — the query cache makes one embedding per distinct query). Results land in a new `eval_runs` table; the judge page renders recommendation/quiet/history states and updates them out-of-band on every verdict. Apply rewrites `config.toml` via `toml_edit` and swaps the live params. The cargo harness (`tests/eval.rs`) is untouched except for reusing a moved helper.

**Tech Stack:** Rust, axum 0.8 + askama 0.16 templates + htmx (`hx-swap-oob`), sqlx/SQLite, `toml_edit` (new dependency), existing fake embedder + `MemoryVectors` for tests.

**Spec:** `docs/superpowers/specs/2026-08-23-tuning-sweep-design.md`

## Global Constraints

- Sweep searches must never be captured or stamp `last_seen_at`: always `Door::Judge` and `mark: false`.
- Nothing may celebrate a particular verdict; animation attaches to progress (counts, MRR movement toward the sweep), never to hit-vs-gap. The `diagnosis` text and its inverted loudness are untouched.
- Recommendation gate: pairs improved minus pairs worsened ≥ 2, and neither recall@10 nor MRR below baseline. Ties keep current values.
- Apply is all-or-nothing: if `config.toml` cannot be read or written, nothing is hot-swapped and nothing is marked applied.
- Miss lists and diffs store/print query prefixes (48 chars) only — never artifact text.
- Run `cargo fmt` before every commit; `cargo test` for the touched module before claiming a step passes.
- Comment style: comments state constraints and reasons, matching the density and voice of neighboring code. No "added X" narration.

---

### Task 1: `[feedback.tune]` and `vector.per_source_cap` configuration

**Files:**
- Modify: `src/config.rs` (FeedbackConfig ~line 143, VectorConfig ~line 595)
- Modify: `config.example.toml`

**Interfaces:**
- Produces: `TuneConfig { min_judgements: i64, resweep_after: i64 }` at `cfg.feedback.tune`; `cfg.vector.per_source_cap: usize` (0 = uncapped, default 3).

- [ ] **Step 1: Write the failing tests** (in `src/config.rs` `mod tests`, following the existing pattern of writing a temp file and `Config::load(Some(&p))` — copy the setup used by the test at ~line 2141 `feedback.candidates`):

```rust
#[test]
fn tune_defaults_and_file_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("config.toml");
    std::fs::write(&p, "").unwrap();
    let cfg = Config::load(Some(&p)).unwrap();
    assert_eq!(cfg.feedback.tune.min_judgements, 50);
    assert_eq!(cfg.feedback.tune.resweep_after, 10);
    assert_eq!(cfg.vector.per_source_cap, 3);

    std::fs::write(
        &p,
        "[feedback.tune]\nmin_judgements = 20\nresweep_after = 5\n[vector]\nper_source_cap = 0\n",
    )
    .unwrap();
    let cfg = Config::load(Some(&p)).unwrap();
    assert_eq!(cfg.feedback.tune.min_judgements, 20);
    assert_eq!(cfg.feedback.tune.resweep_after, 5);
    assert_eq!(cfg.vector.per_source_cap, 0);
}
```

Note: if `Config::load` with an empty file fails validation (it may require `[infer]`), copy whatever minimal TOML the neighboring tests write and extend it — the assertion set stays the same.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib config::tests::tune_defaults_and_file_overrides`
Expected: FAIL — no field `tune`.

- [ ] **Step 3: Implement**

In `src/config.rs`, next to `FeedbackConfig`:

```rust
/// When judgements are spent on a parameter sweep, and how often.
///
/// The floor is statistical, not cautious: with ten pairs recall@10 moves in
/// ten-point steps and every sweep result is noise. Below `min_judgements`
/// nothing runs; after that, a sweep re-runs once `resweep_after` new
/// judgements have accumulated since the last one.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct TuneConfig {
    pub min_judgements: i64,
    pub resweep_after: i64,
}

impl Default for TuneConfig {
    fn default() -> Self {
        Self { min_judgements: 50, resweep_after: 10 }
    }
}
```

Add to `FeedbackConfig`: `pub tune: TuneConfig,` and `tune: TuneConfig::default(),` in its `Default`.

Add to `VectorConfig` (after `pinned_boost`):

```rust
/// Chunks one document may contribute to a result list. `0` lets a single
/// document fill it. Runtime-tunable: the sweep may recommend another value
/// and apply writes it back here.
#[serde(default = "default_per_source_cap")]
pub per_source_cap: usize,
```

with `fn default_per_source_cap() -> usize { 3 }`.

In `config.example.toml`, under `[vector]` add `# per_source_cap = 3` with a one-line comment, and a new commented block:

```toml
# [feedback.tune]
# Judgements before the first automatic parameter sweep, and how many new
# judgements re-run it. See docs/evaluation.md.
# min_judgements = 50
# resweep_after = 10
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib config`
Expected: PASS (all config tests — the removed-keys and example-file tests must still pass).

- [ ] **Step 5: Commit**

```bash
git add src/config.rs config.example.toml
git commit -m "feat(config): the tune thresholds, and a cap that is a setting"
```

---

### Task 2: `VectorStore::search_weighted`

**Files:**
- Modify: `src/vector/mod.rs` (trait, `search` at ~line 265)
- Modify: `src/vector/qdrant.rs` (`search` impl around lines 1540–1650)
- Test: `src/vector/memory.rs` tests module

**Interfaces:**
- Produces: trait method `async fn search_weighted(&self, vector: &[f32], sparse: &sparse::SparseVector, limit: usize, filter: &SearchFilter, recency_weight: f32) -> Result<Vec<SearchHit>>` with a default impl that ignores the weight and delegates to `search`. `QdrantVectors` overrides it so the weight replaces `self.recency_weight` in the scoring stage.

- [ ] **Step 1: Write the failing test** (in `src/vector/memory.rs` tests — MemoryVectors takes the default impl):

```rust
#[tokio::test]
async fn search_weighted_defaults_to_search() {
    let v = MemoryVectors::new();
    // Reuse this module's existing insert helper/fixtures to add one point,
    // then assert both calls return the same hits.
    // (Copy the setup of the nearest existing search test verbatim.)
    let sparse = crate::vector::sparse::encode_query("q");
    let filter = SearchFilter::default();
    let a = v.search(&[0.1; 8], &sparse, 10, &filter).await.unwrap();
    let b = v
        .search_weighted(&[0.1; 8], &sparse, 10, &filter, 0.9)
        .await
        .unwrap();
    assert_eq!(
        a.iter().map(|h| &h.payload.artifact_id).collect::<Vec<_>>(),
        b.iter().map(|h| &h.payload.artifact_id).collect::<Vec<_>>()
    );
}
```

If `SearchFilter` has no `Default`, construct it the way the neighboring memory tests do.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib vector::memory`
Expected: FAIL — no method `search_weighted`.

- [ ] **Step 3: Implement**

In `src/vector/mod.rs`, add to the trait directly under `search`:

```rust
/// `search`, with the recency weight chosen per call rather than fixed at
/// connect time. The default ignores the weight: only stores that apply
/// recency at all have anything to vary, and the tuning sweep is the one
/// caller that needs to vary it.
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
```

In `src/vector/qdrant.rs`: rename the body of the existing `search` impl to a private `async fn search_scored(&self, vector: &[f32], sparse: &sparse::SparseVector, limit: usize, filter: &SearchFilter, recency_weight: f32) -> Result<Vec<SearchHit>>`, replacing the two uses of `self.recency_weight` (the `> 0.0` check at ~line 1585 and the `scoring_formula` argument at ~line 1598) with the parameter. Then:

```rust
async fn search(&self, vector: &[f32], sparse: &sparse::SparseVector, limit: usize, filter: &SearchFilter) -> Result<Vec<SearchHit>> {
    self.search_scored(vector, sparse, limit, filter, self.recency_weight).await
}

async fn search_weighted(&self, vector: &[f32], sparse: &sparse::SparseVector, limit: usize, filter: &SearchFilter, recency_weight: f32) -> Result<Vec<SearchHit>> {
    self.search_scored(vector, sparse, limit, filter, recency_weight).await
}
```

(Adjust `&self` receiver/signature details to match how the impl block is written — it may be inside `#[async_trait]`.)

- [ ] **Step 4: Run tests**

Run: `cargo test --lib vector`
Expected: PASS, including qdrant's existing request-body tests.

- [ ] **Step 5: Commit**

```bash
git add src/vector/mod.rs src/vector/qdrant.rs src/vector/memory.rs
git commit -m "feat(vector): the recency weight becomes a per-call choice"
```

---

### Task 3: `RankingParams` in `Core`, read by the hot path

**Files:**
- Create: `src/core/ranking.rs`
- Modify: `src/core/mod.rs` (Core struct ~line 72, `from_config`, `mod ranking;`)
- Modify: `src/core/search.rs` (`search` ~line 817, `search_with` ~line 843)
- Modify: `src/web/ui.rs:1111` (the explicit `Some(MAX_PER_CORPUS)`)
- Modify: `src/core/test_support.rs` and `tests/eval.rs` (Core literals gain the new fields)
- Test: `src/core/ranking.rs` tests + one search test in `src/core/search.rs`

**Interfaces:**
- Produces: `pub struct RankingParams { pub recency_weight: f32, pub per_source_cap: Option<usize> }` with `RankingParams::from_vector(&VectorConfig)`; `Core.ranking: Arc<std::sync::RwLock<RankingParams>>`; `Core.tuning: Arc<std::sync::atomic::AtomicBool>`; `pub async fn search_with_ranking(&self, query: &SearchQuery, params: RankingParams, origin: impl Into<Origin>) -> Result<(Vec<SearchResult>, SearchTiming)>`.
- Consumes: `search_weighted` from Task 2.

- [ ] **Step 1: Write the failing tests**

`src/core/ranking.rs`:

```rust
//! The scoring knobs a sweep may move at runtime.

use crate::config::VectorConfig;

/// Everything else that shapes ranking still comes from `Config` at startup;
/// these two are the ones the tuning sweep measures and apply hot-swaps, so
/// they live behind `Core::ranking` instead of being copied into closures.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankingParams {
    pub recency_weight: f32,
    /// `None` lets one source fill the whole list.
    pub per_source_cap: Option<usize>,
}

impl RankingParams {
    pub fn from_vector(cfg: &VectorConfig) -> Self {
        Self {
            recency_weight: cfg.recency_weight,
            per_source_cap: match cfg.per_source_cap {
                0 => None,
                n => Some(n),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_cap_means_uncapped() {
        let mut cfg = VectorConfig::default_for_tests();
        cfg.per_source_cap = 0;
        assert_eq!(RankingParams::from_vector(&cfg).per_source_cap, None);
        cfg.per_source_cap = 3;
        assert_eq!(RankingParams::from_vector(&cfg).per_source_cap, Some(3));
    }
}
```

If `VectorConfig` has no test constructor, build it literally with the field values the defaults functions return (url/collection as `String::new()` etc.).

Search test (in `src/core/search.rs` tests, using the module's existing `test_core` fixture pattern): create one corpus with 4 artifacts, search with a query matching all; assert that with `core.ranking` cap swapped to `Some(1)` the results contain 1 hit, and with `None` all 4 — through the public `core.search(&q, Door::Judge)` path:

```rust
#[tokio::test]
async fn the_live_cap_is_read_from_ranking_params() {
    let core = crate::core::test_support::test_core().await;
    // ...insert one corpus with four artifacts sharing vocabulary,
    // embed via jobs::embed::run_corpus, as sibling tests do...
    core.ranking.write().unwrap().per_source_cap = Some(1);
    let one = core.search(&q, crate::store::feedback::Door::Judge).await.unwrap();
    assert_eq!(one.len(), 1);
    core.ranking.write().unwrap().per_source_cap = None;
    let all = core.search(&q, crate::store::feedback::Door::Judge).await.unwrap();
    assert_eq!(all.len(), 4);
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test --lib core::ranking core::search`
Expected: FAIL — module/field missing.

- [ ] **Step 3: Implement**

1. Add `pub mod ranking;` to `src/core/mod.rs`; add fields to `Core`:

```rust
/// The scoring knobs the tuning sweep may move. Shared by every clone, like
/// the background queue — apply must change the search the next request runs.
pub ranking: Arc<std::sync::RwLock<crate::core::ranking::RankingParams>>,
/// Whether a sweep is in flight, so two verdicts cannot start two.
pub tuning: Arc<std::sync::atomic::AtomicBool>,
```

2. In `from_config`: `ranking: Arc::new(std::sync::RwLock::new(crate::core::ranking::RankingParams::from_vector(&cfg.vector)))`, `tuning: Arc::new(std::sync::atomic::AtomicBool::new(false))`.
3. In `src/core/search.rs`:
   - `search()` (~line 817): replace `Some(MAX_PER_CORPUS)` with `self.ranking.read().expect("ranking lock").per_source_cap`.
   - Rename the current `search_with` body to `async fn search_inner(&self, query: &SearchQuery, cap: Option<usize>, recency_weight: f32, origin: Origin) -> Result<(Vec<SearchResult>, SearchTiming)>` and change the one `self.vectors.search(&vector, &sparse, candidates, &filter)` call (~line 923) to `self.vectors.search_weighted(&vector, &sparse, candidates, &filter, recency_weight)`.
   - New public pair:

```rust
pub async fn search_with(
    &self,
    query: &SearchQuery,
    cap: Option<usize>,
    origin: impl Into<Origin>,
) -> Result<(Vec<SearchResult>, SearchTiming)> {
    let w = self.ranking.read().expect("ranking lock").recency_weight;
    self.search_inner(query, cap, w, origin.into()).await
}

/// `search_with`, with every runtime knob chosen by the caller. What the
/// tuning sweep runs its candidates through, so measurement and the live
/// path are one pipeline rather than two that can drift.
pub async fn search_with_ranking(
    &self,
    query: &SearchQuery,
    params: crate::core::ranking::RankingParams,
    origin: impl Into<Origin>,
) -> Result<(Vec<SearchResult>, SearchTiming)> {
    self.search_inner(query, params.per_source_cap, params.recency_weight, origin.into())
        .await
}
```

4. `src/web/ui.rs:1111`: replace `Some(crate::core::search::MAX_PER_CORPUS)` with `st.core.ranking.read().expect("ranking lock").per_source_cap`.
5. Add the two fields to the `Core` literals in `src/core/test_support.rs` and `tests/eval.rs` (`ranking: Arc::new(std::sync::RwLock::new(engram::core::ranking::RankingParams { recency_weight: 0.0, per_source_cap: Some(engram::core::search::MAX_PER_CORPUS) }))`, `tuning: Arc::new(std::sync::atomic::AtomicBool::new(false))` — adjust paths for crate-internal vs integration context).

- [ ] **Step 4: Run tests** — `cargo test` (full lib + integration wiring test)
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core/ranking.rs src/core/mod.rs src/core/search.rs src/web/ui.rs src/core/test_support.rs tests/eval.rs
git commit -m "feat(core): the two ranking knobs become one swappable value"
```

---

### Task 4: the `eval_runs` store

**Files:**
- Modify: `src/store/schema.sql` (append), `src/store/mod.rs` (`pub mod eval_runs;`)
- Create: `src/store/eval_runs.rs`

**Interfaces:**
- Produces: `RunParams { recency_weight: f32, per_source_cap: Option<usize> }` (Serialize/Deserialize/PartialEq/Clone/Copy/Debug), `DiffRow { query: String, base: Option<usize>, new: Option<usize> }`, `NewEvalRun`, `EvalRun`, and on `Store`: `record_eval_run(&NewEvalRun) -> Result<String>`, `latest_eval_run() -> Result<Option<EvalRun>>`, `open_recommendation() -> Result<Option<EvalRun>>`, `eval_run(&str) -> Result<Option<EvalRun>>`, `mark_eval_run_applied(&str) -> Result<bool>`, `applied_eval_runs(i64) -> Result<Vec<EvalRun>>`.

- [ ] **Step 1: Schema.** Append to `src/store/schema.sql`:

```sql
-- ── Tuning sweeps ────────────────────────────────────────────────────────────
-- One row per background sweep over the judged pairs: the baseline the live
-- configuration scored, the best candidate the grid found, and whether the
-- gate let it become a recommendation. This table is the provenance that used
-- to live in commit messages: a number recorded without the configuration
-- that produced it cannot be compared against anything.
CREATE TABLE IF NOT EXISTS eval_runs (
  id            TEXT PRIMARY KEY,
  created_at    INTEGER NOT NULL,
  judged_count  INTEGER NOT NULL,
  pairs_used    INTEGER NOT NULL,
  pairs_skipped INTEGER NOT NULL,
  base_params   TEXT NOT NULL,   -- JSON RunParams
  base_recall   REAL NOT NULL,
  base_mrr      REAL NOT NULL,
  best_params   TEXT NOT NULL,   -- JSON RunParams; equals base when nothing won
  best_recall   REAL NOT NULL,
  best_mrr      REAL NOT NULL,
  diff          TEXT NOT NULL,   -- JSON [DiffRow]: query prefixes only, never artifact text
  recommended   INTEGER NOT NULL,
  applied_at    INTEGER
);
```

- [ ] **Step 2: Write the failing tests** (in `src/store/eval_runs.rs`, following the fixture style of sibling store modules — `Store::memory().await`):

```rust
#[tokio::test]
async fn a_recommendation_is_open_until_applied_and_applies_once() {
    let store = Store::memory().await.unwrap();
    let id = store.record_eval_run(&sample(true)).await.unwrap();
    assert_eq!(store.open_recommendation().await.unwrap().map(|r| r.id), Some(id.clone()));
    assert!(store.mark_eval_run_applied(&id).await.unwrap());
    assert!(store.open_recommendation().await.unwrap().is_none());
    assert!(!store.mark_eval_run_applied(&id).await.unwrap(), "applied twice");
    assert_eq!(store.applied_eval_runs(10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_quiet_sweep_is_recorded_but_never_recommended() {
    let store = Store::memory().await.unwrap();
    store.record_eval_run(&sample(false)).await.unwrap();
    assert!(store.open_recommendation().await.unwrap().is_none());
    let latest = store.latest_eval_run().await.unwrap().unwrap();
    assert!(!latest.recommended);
    assert_eq!(latest.base_params, latest.best_params);
}
```

with a `fn sample(recommended: bool) -> NewEvalRun` helper (judged_count 50, pairs_used 12, skipped 1, base `{0.05, Some(3)}` recall 0.7 mrr 0.5, best `{0.0, Some(5)}` (or = base when `!recommended`) recall 0.8 mrr 0.6, one `DiffRow { query: "the image will not mount".into(), base: None, new: Some(2) }`).

- [ ] **Step 3: Run to verify failure** — `cargo test --lib store::eval_runs` → FAIL.

- [ ] **Step 4: Implement** `src/store/eval_runs.rs`: structs as in Interfaces, with `EvalRun` in full:

```rust
#[derive(Debug, Clone)]
pub struct EvalRun {
    pub id: String,
    pub created_at: i64,
    pub judged_count: i64,
    pub pairs_used: i64,
    pub pairs_skipped: i64,
    pub base_params: RunParams,
    pub base_recall: f64,
    pub base_mrr: f64,
    pub best_params: RunParams,
    pub best_recall: f64,
    pub best_mrr: f64,
    pub diff: Vec<DiffRow>,
    pub recommended: bool,
    pub applied_at: Option<i64>,
}
```

(`NewEvalRun` carries the same fields minus `id`/`created_at`/`applied_at`, with `base`/`best` as the param names.) `record_eval_run` inserts with `new_id()`/`now()` and `serde_json::to_string` for params/diff; queries:

- `latest_eval_run`: `ORDER BY created_at DESC, id DESC LIMIT 1`.
- `open_recommendation`: `WHERE recommended = 1 AND applied_at IS NULL ORDER BY created_at DESC, id DESC LIMIT 1`.
- `mark_eval_run_applied`: `UPDATE eval_runs SET applied_at = ? WHERE id = ? AND applied_at IS NULL`, return `rows_affected() == 1`.
- `applied_eval_runs(limit)`: `WHERE applied_at IS NOT NULL ORDER BY applied_at DESC LIMIT ?`.

One private `fn hydrate(row) -> Result<EvalRun>` doing the JSON parses. Add `pub mod eval_runs;` beside the sibling `pub mod` lines in `src/store/mod.rs`.

- [ ] **Step 5: Run tests** — `cargo test --lib store` → PASS.

- [ ] **Step 6: Commit**

```bash
git add src/store/schema.sql src/store/mod.rs src/store/eval_runs.rs
git commit -m "feat(store): sweeps get the provenance commit messages had"
```

---

### Task 5: judged pairs and the day's count

**Files:**
- Modify: `src/store/feedback.rs`

**Interfaces:**
- Produces: `pub struct JudgedPair { pub query: String, pub expect: String }`; `Store::judged_pairs() -> Result<Vec<JudgedPair>>`; `Store::judged_since(since: i64) -> Result<i64>`.

- [ ] **Step 1: Write the failing test** (reuse this module's existing event fixtures — the tests around `feedback_stats` already record and judge events; copy that setup):

```rust
#[tokio::test]
async fn judged_pairs_are_the_hits_and_only_the_hits() {
    // one event judged hit (expect a1), one gap, one discard, one pending
    // ...fixture as in the neighboring stats test...
    let pairs = store.judged_pairs().await.unwrap();
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].expect, a1);
    // hit + gap + discard: judged_since counts verdicts given, mirroring
    // Stats::judged, because it feeds the "14 today" counter.
    assert_eq!(store.judged_since(0).await.unwrap(), 3);
}
```

- [ ] **Step 2: Run to verify failure** — FAIL, methods missing.

- [ ] **Step 3: Implement**

```rust
#[derive(Debug, Clone)]
pub struct JudgedPair {
    pub query: String,
    pub expect: String,
}

/// Every judgement that names an answer: the dataset a sweep replays.
/// Gaps and discards are verdicts but not pairs — there is nothing to rank.
pub async fn judged_pairs(&self) -> Result<Vec<JudgedPair>> {
    Ok(sqlx::query(
        "SELECT query, expect_id FROM search_events
         WHERE verdict = 'hit' AND expect_id IS NOT NULL
         ORDER BY created_at, id",
    )
    .fetch_all(&self.pool)
    .await?
    .iter()
    .map(|r| JudgedPair { query: r.get("query"), expect: r.get("expect_id") })
    .collect())
}

/// Verdicts given since `since` — what the "today" counter reads.
pub async fn judged_since(&self, since: i64) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM search_events WHERE judged_at >= ?",
    )
    .bind(since)
    .fetch_one(&self.pool)
    .await?)
}
```

- [ ] **Step 4: Run tests** — `cargo test --lib store::feedback` → PASS.

- [ ] **Step 5: Commit**

```bash
git add src/store/feedback.rs
git commit -m "feat(store): the judged pairs, enumerable at runtime"
```

---

### Task 6: `satisfied_by` moves into the library

**Files:**
- Modify: `src/eval/mod.rs`, `tests/eval.rs`

**Interfaces:**
- Produces: `pub async fn satisfied_by(core: &Core, expected: &str) -> Vec<String>` in `src/eval/mod.rs` — the exact body of `resolve_expected` currently at `tests/eval.rs:369-385`, doc comment included.

- [ ] **Step 1: Move the function.** Copy `resolve_expected` (with its full doc comment about bounded supersession chains) into `src/eval/mod.rs` as `satisfied_by`, adjusting imports (`use crate::core::Core;`). Delete it from `tests/eval.rs` and replace both call sites with `engram::eval::satisfied_by(&core, expect).await`.

- [ ] **Step 2: Run the wiring test** — the non-ignored test in `tests/eval.rs` covers exactly this function's contract (merged-away artifacts still satisfy their grade):

Run: `cargo test --test eval a_pair_naming_a_frozen_artifact_can_actually_be_found`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src/eval/mod.rs tests/eval.rs
git commit -m "refactor(eval): supersession resolution moves where the sweep can reach it"
```

---

### Task 7: the sweep engine

**Files:**
- Create: `src/eval/sweep.rs`; add `pub mod sweep;` to `src/eval/mod.rs`

**Interfaces:**
- Consumes: `Core.ranking`/`Core.tuning` (Task 3), `search_with_ranking` (Task 3), `judged_pairs` (Task 5), `satisfied_by` (Task 6), `record_eval_run`/`latest_eval_run` (Task 4), `eval::metrics::{recall_at, mrr}`.
- Produces: `pub fn grid(current: RankingParams) -> Vec<RankingParams>`, `pub fn recommend(base: &[Option<usize>], cand: &[Option<usize>]) -> bool` (pure gate), `pub async fn run_sweep(core: &Core) -> crate::error::Result<()>`, `pub fn maybe_spawn(core: &Core)`.

- [ ] **Step 1: Write the failing gate tests** (pure, no async):

```rust
#[test]
fn the_gate_needs_two_net_better_pairs_and_no_aggregate_loss() {
    use super::recommend;
    let base = vec![Some(5), Some(7), None, Some(0)];
    // two improved, none worse
    assert!(recommend(&base, &[Some(1), Some(2), None, Some(0)]));
    // one improved only: noise
    assert!(!recommend(&base, &[Some(1), Some(7), None, Some(0)]));
    // two improved but one previously-found pair lost: net 1
    assert!(!recommend(&base, &[Some(1), Some(2), None, None]));
    // identical: ties keep current values
    assert!(!recommend(&base, &base));
}

#[test]
fn the_grid_always_contains_the_current_configuration() {
    let cur = RankingParams { recency_weight: 0.07, per_source_cap: Some(4) };
    let g = super::grid(cur);
    assert!(g.contains(&cur));
    assert!(g.len() >= 20);
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test --lib eval::sweep` → FAIL.

- [ ] **Step 3: Implement the pure parts**

```rust
//! The background parameter sweep judgements pay for.
//!
//! It reads the live index and only reads it: `Door::Judge` and `mark: false`
//! are the same discipline as the judge page's assign search, because a sweep
//! is not someone reading their notes and its queries are composed in full
//! knowledge of the answers.

use crate::core::Core;
use crate::core::ranking::RankingParams;
use crate::eval::metrics::{mrr, recall_at};
use crate::store::eval_runs::{DiffRow, NewEvalRun, RunParams};

const LIMIT: usize = 10;
const RECENCY: [f32; 5] = [0.0, 0.05, 0.1, 0.15, 0.25];
const CAPS: [Option<usize>; 4] = [Some(2), Some(3), Some(5), None];

/// The product of the two axes, with the running configuration always among
/// the candidates — it is the baseline everything is gated against.
pub fn grid(current: RankingParams) -> Vec<RankingParams> {
    let mut out = Vec::new();
    for &w in &RECENCY {
        for &c in &CAPS {
            out.push(RankingParams { recency_weight: w, per_source_cap: c });
        }
    }
    if !out.contains(&current) {
        out.push(current);
    }
    out
}

/// Whether a rank is better than another. `None` is a miss and loses to any
/// rank; two `None`s are equal.
fn better(cand: Option<usize>, base: Option<usize>) -> bool {
    match (cand, base) {
        (Some(c), Some(b)) => c < b,
        (Some(_), None) => true,
        _ => false,
    }
}

/// The overfitting brake. An aggregate delta can be one flipped pair wearing
/// a percentage; two net-better pairs cannot. Ties keep the current values.
pub fn recommend(base: &[Option<usize>], cand: &[Option<usize>]) -> bool {
    let improved = base.iter().zip(cand).filter(|(b, c)| better(**c, **b)).count() as i64;
    let worsened = base.iter().zip(cand).filter(|(b, c)| better(**b, **c)).count() as i64;
    improved - worsened >= 2
        && recall_at(cand, LIMIT) >= recall_at(base, LIMIT)
        && mrr(cand) >= mrr(base)
}
```

- [ ] **Step 4: Run the pure tests** — PASS. Commit checkpoint:

```bash
git add src/eval/sweep.rs src/eval/mod.rs
git commit -m "feat(eval): the sweep grid and the gate that keeps noise out"
```

- [ ] **Step 5: Write the failing end-to-end test** (in the same module; `crate::core::test_support::test_core()` gives fake embedder + `MemoryVectors`, deterministic). Arrange a corpus where the cap decides: one corpus of four artifacts sharing vocabulary with two queries whose expected artifact sits fourth in vector order (the fake embedder embeds identical text identically, so make the three decoys near-copies of the query text and the expected artifact the query text itself placed last by recency— if ordering proves fiddly, arrange instead two *separate* corpora where decoy chunks from corpus A bury the expected corpus-B chunk under cap `Some(3)` by giving corpus A four near-identical chunks). The test asserts mechanics, not retrieval quality:

```rust
#[tokio::test]
async fn a_sweep_records_a_run_and_a_winning_candidate_is_recommended() {
    let core = crate::core::test_support::test_core().await;
    // fixture: artifacts + embed + two search_events judged hit whose
    // expected artifact misses under the current cap but lands with a wider
    // one (copy the record_search fixture from web::judge tests)
    core.ranking.write().unwrap().per_source_cap = Some(1);
    super::run_sweep(&core).await.unwrap();
    let run = core.store.latest_eval_run().await.unwrap().unwrap();
    assert_eq!(run.pairs_used, 2);
    assert!(run.recommended, "a strictly better candidate must pass the gate");
    assert!(run.best_recall >= run.base_recall);
    assert!(!run.diff.is_empty(), "the diff is the part a person reads");
}

#[tokio::test]
async fn a_sweep_with_nothing_better_records_the_silence() {
    // fixture where every pair already ranks first
    super::run_sweep(&core).await.unwrap();
    let run = core.store.latest_eval_run().await.unwrap().unwrap();
    assert!(!run.recommended);
    assert_eq!(run.base_params, run.best_params);
}

#[tokio::test]
async fn a_pair_whose_artifact_vanished_is_skipped_and_counted() {
    // one good pair, one whose expect_id names nothing
    super::run_sweep(&core).await.unwrap();
    let run = core.store.latest_eval_run().await.unwrap().unwrap();
    assert_eq!((run.pairs_used, run.pairs_skipped), (1, 1));
}
```

- [ ] **Step 6: Implement `run_sweep` and `maybe_spawn`**

```rust
async fn ranks_for(
    core: &Core,
    pairs: &[(String, Vec<String>)],
    p: RankingParams,
) -> crate::error::Result<Vec<Option<usize>>> {
    let mut ranks = Vec::with_capacity(pairs.len());
    for (query, satisfies) in pairs {
        let q = crate::core::search::SearchQuery {
            q: query.clone(),
            limit: LIMIT,
            tags: vec![],
            category: None,
            // A sweep is not someone reading their notes.
            mark: false,
            include_deprecated: false,
            include_superseded: false,
        };
        let (results, _) = core
            .search_with_ranking(&q, p, crate::store::feedback::Door::Judge)
            .await?;
        ranks.push(
            results
                .iter()
                .position(|r| satisfies.iter().any(|id| id == &r.artifact_id)),
        );
    }
    Ok(ranks)
}

pub async fn run_sweep(core: &Core) -> crate::error::Result<()> {
    let stats = core.store.feedback_stats().await?;
    let current = *core.ranking.read().expect("ranking lock");

    let mut pairs = Vec::new();
    let mut skipped = 0i64;
    for p in core.store.judged_pairs().await? {
        // A deleted artifact is housekeeping, not a ranking result. Scored as
        // a miss it would look like one forever; a background job that dies
        // on it sweeps nothing ever again.
        match core.store.get_artifact(&p.expect).await {
            Ok(_) => {
                let satisfies = crate::eval::satisfied_by(core, &p.expect).await;
                pairs.push((p.query, satisfies));
            }
            Err(crate::error::Error::NotFound) => skipped += 1,
            Err(e) => return Err(e),
        }
    }
    if pairs.is_empty() {
        return Ok(());
    }

    let base = ranks_for(core, &pairs, current).await?;
    let mut best: Option<(RankingParams, Vec<Option<usize>>)> = None;
    for cand in grid(current) {
        if cand == current {
            continue;
        }
        let ranks = ranks_for(core, &pairs, cand).await?;
        if recommend(&base, &ranks) {
            let beats_best = best.as_ref().is_none_or(|(_, b)| {
                mrr(&ranks) > mrr(b) || (mrr(&ranks) == mrr(b) && recall_at(&ranks, LIMIT) > recall_at(b, LIMIT))
            });
            if beats_best {
                best = Some((cand, ranks));
            }
        }
    }

    let (best_params, best_ranks, recommended) = match best {
        Some((p, r)) => (p, r, true),
        None => (current, base.clone(), false),
    };
    let diff: Vec<DiffRow> = pairs
        .iter()
        .zip(base.iter().zip(&best_ranks))
        .filter(|(_, (b, n))| b != n)
        .map(|((query, _), (b, n))| DiffRow {
            query: query.chars().take(48).collect(),
            base: *b,
            new: *n,
        })
        .collect();

    core.store
        .record_eval_run(&NewEvalRun {
            judged_count: stats.judged,
            pairs_used: pairs.len() as i64,
            pairs_skipped: skipped,
            base: RunParams { recency_weight: current.recency_weight, per_source_cap: current.per_source_cap },
            base_recall: recall_at(&base, LIMIT),
            base_mrr: mrr(&base),
            best: RunParams { recency_weight: best_params.recency_weight, per_source_cap: best_params.per_source_cap },
            best_recall: recall_at(&best_ranks, LIMIT),
            best_mrr: mrr(&best_ranks),
            diff,
            recommended,
        })
        .await?;
    Ok(())
}

/// Enqueue a sweep when the judgements have paid for one. Called after every
/// verdict; cheap when nothing is due, silent when a sweep already runs.
pub fn maybe_spawn(core: &Core) {
    use std::sync::atomic::Ordering;
    if !core.learn.enabled {
        return;
    }
    let core = core.clone();
    core.background.clone().spawn(async move {
        if core.tuning.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(e) = spawn_inner(&core).await {
            tracing::warn!(error = %e, "tuning sweep failed");
        }
        core.tuning.store(false, Ordering::SeqCst);
    });
}

async fn spawn_inner(core: &Core) -> crate::error::Result<()> {
    let tune = &core.feedback.tune;
    let stats = core.store.feedback_stats().await?;
    if stats.judged < tune.min_judgements {
        return Ok(());
    }
    if let Some(last) = core.store.latest_eval_run().await?
        && stats.judged - last.judged_count < tune.resweep_after
    {
        return Ok(());
    }
    run_sweep(core).await
}
```

(Adapt `RunParams` construction if Task 4 made `RunParams` and `RankingParams` convertible — a `From` impl in `eval_runs.rs` is welcome if it removes the duplication.)

- [ ] **Step 7: Run tests** — `cargo test --lib eval::sweep` → PASS.

- [ ] **Step 8: Commit**

```bash
git add src/eval/sweep.rs
git commit -m "feat(eval): judgements buy sweeps over the live index"
```

---

### Task 8: verdicts trigger the sweep

**Files:**
- Modify: `src/web/judge.rs` (`card_after` ~line 249)

**Interfaces:**
- Consumes: `eval::sweep::maybe_spawn` (Task 7).

- [ ] **Step 1: Write the failing test** (in `src/web/judge.rs` tests, reusing `judge_app`):

```rust
#[tokio::test]
async fn a_verdict_past_the_floor_runs_a_sweep_in_the_background() {
    let (app, cookie, core, ids) = judge_app(2, &[]).await;
    // The floor is config; drop it so one verdict is enough.
    // (judge_app hands back the core before the app consumed a clone —
    // mutate `feedback.tune` on the handle used to build the app, which
    // requires judge_app to set it before app_with_cookie. Adjust judge_app
    // to take the core through a `tune floor` of 1 for all tests, or add a
    // variant fixture — smallest diff wins.)
    let event = core.store.next_pending().await.unwrap().unwrap();
    post(&app, &format!("/ui/judge/{}/hit", event.id), &cookie,
         &format!("artifact_id={}", ids[0])).await;
    core.background.wait_idle().await;
    assert!(
        core.store.latest_eval_run().await.unwrap().is_some(),
        "the verdict crossed the floor and no sweep ran"
    );
}

#[tokio::test]
async fn under_the_floor_no_sweep_runs() {
    let (app, cookie, core, ids) = judge_app(2, &[]).await; // default floor 50
    let event = core.store.next_pending().await.unwrap().unwrap();
    post(&app, &format!("/ui/judge/{}/hit", event.id), &cookie,
         &format!("artifact_id={}", ids[0])).await;
    core.background.wait_idle().await;
    assert!(core.store.latest_eval_run().await.unwrap().is_none());
}
```

For the first test, the smallest change to `judge_app` is an extra fixture `judge_app_tuned(real, phantom, min_judgements)` that sets `core.feedback.tune.min_judgements` before `app_with_cookie(core)`; the existing `judge_app` delegates with 50.

- [ ] **Step 2: Run to verify failure** — first test FAILS (no sweep runs).

- [ ] **Step 3: Implement.** In `card_after`, after the flash is built but before returning (one line, so every verdict path — hit, gap, discard — triggers; skip and undo do not go through `card_after`):

```rust
// Every verdict is a chance the judgements now pay for a sweep. Off the
// request path: the verdict must not wait on twenty grid searches.
crate::eval::sweep::maybe_spawn(&st.core);
```

Place it in `card_after` directly after `let after = ...` — it reads state written by the verdict, and the spawn is fire-and-forget.

- [ ] **Step 4: Run tests** — `cargo test --lib web::judge` → PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/judge.rs
git commit -m "feat(judge): every verdict asks whether a sweep is due"
```

---

### Task 9: apply — `toml_edit`, the config path, the route

**Files:**
- Modify: `Cargo.toml` (add `toml_edit = "0.23"` under `[dependencies]` with a one-line comment: rewriting config.toml in place, comments preserved)
- Modify: `src/config.rs` (add `write_ranking`)
- Modify: `src/web/state.rs` (`AppState` gains `pub config_path: Arc<std::path::PathBuf>`), `src/main.rs:251` (pass `args.config.clone().unwrap_or_else(|| "config.toml".into())`), `src/web/test_support.rs` (temp config file per app)
- Modify: `src/web/judge.rs` (route + handler)

**Interfaces:**
- Produces: `pub fn write_ranking(path: &Path, p: &crate::core::ranking::RankingParams) -> std::io::Result<()>`; route `POST /ui/judge/tune/{run_id}/apply` returning the tune fragment (Task 10 supplies the template; until then return a minimal `HtmlTemplate` — see Step 4 ordering note).

**Ordering note:** Tasks 9 and 10 touch the same handler. Task 9 lands the mechanics with a minimal inline response body; Task 10 replaces it with the real fragment. Keep both green at each commit.

- [ ] **Step 1: Write the failing `write_ranking` test** (in `src/config.rs` tests):

```rust
#[test]
fn write_ranking_edits_in_place_and_keeps_comments() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("config.toml");
    std::fs::write(&p, "# the operator's comment\n[vector]\nurl = \"http://q\"\nrecency_weight = 0.05\n").unwrap();
    let params = crate::core::ranking::RankingParams { recency_weight: 0.1, per_source_cap: None };
    write_ranking(&p, &params).unwrap();
    let out = std::fs::read_to_string(&p).unwrap();
    assert!(out.contains("# the operator's comment"), "comments must survive: {out}");
    assert!(out.contains("recency_weight = 0.1"), "{out}");
    assert!(out.contains("per_source_cap = 0"), "{out}");
    assert!(out.contains("url = \"http://q\""), "{out}");
    // a missing file is a refusal, not a file created from nothing
    assert!(write_ranking(&dir.path().join("absent.toml"), &params).is_err());
}
```

- [ ] **Step 2: Implement `write_ranking`**

```rust
/// Rewrite the two runtime-tunable keys in place. `toml_edit` keeps every
/// comment and every other key byte-for-byte; a missing file is refused
/// rather than invented, because a server that writes a config nobody wrote
/// is a server with two authors.
pub fn write_ranking(
    path: &std::path::Path,
    p: &crate::core::ranking::RankingParams,
) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let mut doc: toml_edit::DocumentMut =
        text.parse().map_err(std::io::Error::other)?;
    // f32→f64 verbatim writes 0.05000000074…; three decimals is the grid's
    // whole resolution.
    let w = (f64::from(p.recency_weight) * 1000.0).round() / 1000.0;
    doc["vector"]["recency_weight"] = toml_edit::value(w);
    doc["vector"]["per_source_cap"] =
        toml_edit::value(p.per_source_cap.map_or(0, |n| n as i64));
    std::fs::write(path, doc.to_string())
}
```

Run: `cargo test --lib config::tests::write_ranking_edits_in_place_and_keeps_comments` → PASS. Commit checkpoint:

```bash
git add Cargo.toml Cargo.lock src/config.rs
git commit -m "feat(config): apply rewrites the file the operator wrote"
```

- [ ] **Step 3: Plumb the path.** Add `pub config_path: Arc<std::path::PathBuf>` to `AppState`; in `src/main.rs:251` set it from `args.config`; in `src/web/test_support.rs`, before building `AppState`, write a scratch config the apply tests can hit:

```rust
let config_path = std::env::temp_dir().join(format!("engram-test-{}.toml", crate::store::new_id()));
std::fs::write(&config_path, "[vector]\nrecency_weight = 0.05\n").unwrap();
```

and store `Arc::new(config_path)`. Run `cargo test --lib web` — everything must still compile and pass.

- [ ] **Step 4: Write the failing route tests** (in `src/web/judge.rs` tests):

```rust
#[tokio::test]
async fn apply_writes_the_file_swaps_the_params_and_stamps_the_run() {
    let (app, cookie, core, _) = judge_app(1, &[]).await;
    let run = sample_recommended(&core).await; // records a NewEvalRun with best {0.1, None}, recommended
    let status = post(&app, &format!("/ui/judge/tune/{run}/apply"), &cookie, "").await;
    assert_eq!(status, StatusCode::OK);
    let p = *core.ranking.read().unwrap();
    assert_eq!(p.recency_weight, 0.1);
    assert_eq!(p.per_source_cap, None);
    assert!(core.store.eval_run(&run).await.unwrap().unwrap().applied_at.is_some());
    // and the file agrees with memory — read it via the app's config_path
}

#[tokio::test]
async fn apply_refuses_a_run_that_is_not_an_open_recommendation() {
    // (a) not recommended  (b) already applied — both: 200 with a refusal
    // flash, params unchanged, file unchanged
}

#[tokio::test]
async fn an_unwritable_config_fails_the_whole_apply() {
    // point config_path at a missing file (build AppState by hand as
    // web::test_support does, or delete the scratch file first):
    // POST → error status; core.ranking unchanged; run not stamped.
}
```

`sample_recommended(&core)` records via `core.store.record_eval_run` the Task 4 sample with `best = {0.1, None}` and returns the id. For the file assertion the test needs the path — have the fixture return it (extend `app_with_cookie` to also return the `AppState` or its `config_path`; smallest diff: `app_with_cookie` keeps its signature, add `app_with_state` beside it and use it here).

- [ ] **Step 5: Implement the handler** in `src/web/judge.rs`:

```rust
/// Take the open recommendation live: file first, then memory, then the
/// stamp. The order is the guarantee — a hot-swap the file does not carry
/// would vanish on restart and leave the history claiming otherwise.
async fn tune_apply(
    State(st): State<AppState>,
    _id: Identity,
    Path(run_id): Path<String>,
) -> Result<Response> {
    let Some(run) = st.core.store.eval_run(&run_id).await? else {
        return Err(crate::error::Error::NotFound);
    };
    if !run.recommended || run.applied_at.is_some() {
        return tune_fragment(&st, "nothing to apply: that sweep is not an open recommendation.").await;
    }
    let params = crate::core::ranking::RankingParams {
        recency_weight: run.best_params.recency_weight,
        per_source_cap: run.best_params.per_source_cap,
    };
    crate::config::write_ranking(&st.config_path, &params)
        .map_err(|e| crate::error::Error::Validation(format!("config.toml not updated ({e}); nothing was applied")))?;
    *st.core.ranking.write().expect("ranking lock") = params;
    st.core.store.mark_eval_run_applied(&run_id).await?;
    tune_fragment(&st, "applied — the next search runs with these settings.").await
}
```

Until Task 10 exists, `tune_fragment(&st, line)` can return the line wrapped in `<div id="judge-tune">{line}</div>` via a two-field askama template or `axum::response::Html`; Task 10 replaces it. Register the route in `judge_router()`: `.route("/ui/judge/tune/{run_id}/apply", post(tune_apply))`. Check the error-to-status mapping in `src/error.rs`: `Validation` should already map to a 4xx — if apply-failure needs 500 semantics, use the error variant sibling handlers use for IO.

- [ ] **Step 6: Run tests** — `cargo test --lib web::judge` → PASS.

- [ ] **Step 7: Commit**

```bash
git add src/web/judge.rs src/web/state.rs src/web/test_support.rs src/main.rs
git commit -m "feat(judge): one click takes a recommendation live"
```

---

### Task 10: the cockpit header and the pulse

**Files:**
- Modify: `src/web/judge.rs` (Pulse struct, template fields, handlers), `src/web/templates/judge.html`, `src/web/templates/_judge_card.html`
- Create: `src/web/templates/_judge_pulse.html`
- Modify: `assets/css/42-judge.css`

**Interfaces:**
- Produces: `struct Pulse { judged: i64, target: i64, pct: i64, label: &'static str, recall: String, mrr: String, delta: String, today: i64, pending: i64, hits: i64, finds: i64, gaps: i64, discards: i64 }`; `JudgeTemplate` gains `pulse: Pulse`, `CardTemplate` gains `pulse: Option<Pulse>`; `async fn pulse_of(st: &AppState, delta: String) -> Result<Pulse>`.

- [ ] **Step 1: Write the failing tests** (in `src/web/judge.rs` tests):

```rust
#[tokio::test]
async fn the_header_explains_its_numbers_and_counts_the_day() {
    let (app, cookie, _core, _) = judge_app(2, &[]).await;
    let body = get(&app, "/ui/judge", &cookie).await;
    assert!(body.contains("Mean reciprocal rank"), "MRR unexplained: shows a number the page never explains");
    assert!(body.contains("top ten"), "recall@10 unexplained");
    assert!(body.contains("until the first sweep"));
    assert!(body.contains("a sweep tries other ranking settings"), "the progress bar must say what it buys");
    assert!(body.contains("today"));
}

#[tokio::test]
async fn a_verdict_ships_the_header_out_of_band_with_the_delta() {
    let (app, cookie, core, ids) = judge_app(2, &[]).await;
    let event = core.store.next_pending().await.unwrap().unwrap();
    let body = /* POST hit, read body as in a_verdict_can_be_taken_back */;
    assert!(body.contains(r#"id="judge-live""#));
    assert!(body.contains("hx-swap-oob"));
    assert!(body.contains("judge-tick"), "the MRR must visibly tick");
}

#[tokio::test]
async fn a_plain_card_fetch_carries_no_pulse() {
    let (app, cookie, _core, _) = judge_app(2, &[]).await;
    let body = get(&app, "/ui/judge/next", &cookie).await;
    assert!(!body.contains("hx-swap-oob"), "a fetch that judged nothing must not animate the header");
}
```

- [ ] **Step 2: Run to verify failure** — FAIL.

- [ ] **Step 3: Implement.**

`pulse_of` in `src/web/judge.rs`:

```rust
/// The header's live half. `delta` is empty on a plain page load and set by
/// the verdict that just moved the number — computed across the write, never
/// recomputed later.
async fn pulse_of(st: &AppState, delta: String) -> Result<Pulse> {
    let stats = st.core.store.feedback_stats().await?;
    let tune = &st.core.feedback.tune;
    let (target, label) = match st.core.store.latest_eval_run().await? {
        None => (tune.min_judgements, "until the first sweep"),
        Some(l) => (l.judged_count + tune.resweep_after, "until the next sweep"),
    };
    let target = target.max(stats.judged).max(1);
    // Local midnight: "today" in the operator's day, not UTC's.
    let midnight = chrono::Local::now().date_naive().and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_local_timezone(chrono::Local).earliest()
        .map_or(0, |t| t.timestamp());
    Ok(Pulse {
        judged: stats.judged,
        pct: (stats.judged * 100 / target).min(100),
        target,
        label,
        recall: format!("{:.2}", stats.recall_at_10),
        mrr: format!("{:.2}", stats.mrr),
        delta,
        today: st.core.store.judged_since(midnight).await?,
        pending: stats.pending,
        hits: stats.hits,
        finds: stats.finds,
        gaps: stats.gaps,
        discards: stats.discards,
    })
}
```

`card_after` computes the delta string from `before`/`after`:

```rust
let d = after - before;
let delta = if d.abs() < 0.005 {
    String::new()
} else if d > 0.0 {
    format!("▲ +{:.2}", d)
} else {
    format!("▼ {:.2}", d.abs())
};
```

Pass `pulse: Some(pulse_of(&st, delta).await?)`; `next_card`, `card_again`, `undo`, and the assign handlers pass `pulse: None`; `page` passes `pulse_of(&st, String::new()).await?` (non-optional on `JudgeTemplate`).

`_judge_pulse.html` (askama include, expects `p` and `oob` in scope):

```html
<div id="judge-live" {% if oob %}hx-swap-oob="true"{% endif %} class="judge-live">
  <div class="row">
    <span class="mono">{{ p.judged }} judged</span>
    <span class="spacer"></span>
    <span class="mono" title="Of the answers you confirmed, the share the ranking placed in its top ten.">recall@10 {{ p.recall }}</span>
    <span class="mono{% if !p.delta.is_empty() %} judge-tick{% endif %}"
          title="Mean reciprocal rank: 1.0 means every confirmed answer came first. A miss counts as zero.">MRR {{ p.mrr }}</span>
    {% if !p.delta.is_empty() %}<span class="mono judge-delta">{{ p.delta }}</span>{% endif %}
    <span class="spacer"></span>
    <span class="mono muted">{{ p.today }} today · {{ p.pending }} waiting</span>
  </div>
  <div class="progress" role="progressbar" aria-valuenow="{{ p.judged }}" aria-valuemin="0" aria-valuemax="{{ p.target }}">
    <span style="width:{{ p.pct }}%"{% if oob %} class="judge-grow"{% endif %}></span>
  </div>
  <p class="muted hint">{{ p.judged }} / {{ p.target }} {{ p.label }} — a sweep tries other ranking settings over everything you have judged and recommends what scores better.</p>
  <p class="muted mono">
    {{ p.hits }} hits · {{ p.finds }} finds · {{ p.gaps }} gaps · {{ p.discards }} discarded
  </p>
</div>
```

`judge.html`: replace the current `.judge-header` inner block (rows, progress, hint, counters — lines 7–22 of the current file) with:

```html
<div class="judge-header">
  {% let p = pulse %}{% let oob = false %}{% include "_judge_pulse.html" %}
  {% if asks.asked > 0 %}
  <p class="muted mono">
    {{ asks.judged }} of {{ asks.asked }} questions judged ·
    {{ asks.right }} right · {{ asks.wrong }} wrong · {{ asks.nothing_here }} nothing here
  </p>
  {% endif %}
</div>
```

(`recall`/`mrr`/`target`/`progress_pct`/`stats` leave `JudgeTemplate` — the pulse carries them; keep `stats` only if `judge_pending` still reads it, and it does: keep the field, drop the template usages.)

`_judge_card.html`: at the very top add:

```html
{% if let Some(p) = pulse %}{% let oob = true %}{% include "_judge_pulse.html" %}{% endif %}
```

CSS, appended to `assets/css/42-judge.css`:

```css
/* The verdict moment. Only the numbers that already exist respond — a tick
   on the MRR, a delta that rises and fades, a brightness pulse on the bar.
   Nothing here reacts to *which* verdict was given: what animates is
   progress toward the sweep, never agreement with the ranker. */
@keyframes judge-tick { 0% { background: transparent; } 20% { background: color-mix(in srgb, var(--color-accent) 25%, transparent); } 100% { background: transparent; } }
.judge-tick { animation: judge-tick 0.9s ease-out 1; border-radius: var(--radius-sm); }
@keyframes judge-delta { 0% { opacity: 0; transform: translateY(5px); } 20% { opacity: 1; transform: none; } 70% { opacity: 1; } 100% { opacity: 0; } }
.judge-delta { color: var(--color-accent); animation: judge-delta 2.4s ease-out 1 forwards; }
@keyframes judge-grow { 0% { filter: brightness(1.6); } 100% { filter: none; } }
.judge-grow { animation: judge-grow 0.7s ease-out 1; }
```

- [ ] **Step 4: Run tests** — `cargo test --lib web::judge` → PASS (existing tests too: the no-ranks/no-scores assertions must survive — the pulse contains neither word; `title` texts avoid "rank"… note `"Mean reciprocal rank"` contains "rank" and `the_card_shows_no_ranks_and_no_scores` greps the whole body of `/ui/judge/next`! The pulse is absent from plain `next` fetches, so that test stays green; the verdict-response test above asserts oob content separately and must not assert the absence of "rank").

- [ ] **Step 5: Commit**

```bash
git add src/web/judge.rs src/web/templates/judge.html src/web/templates/_judge_card.html src/web/templates/_judge_pulse.html assets/css/42-judge.css
git commit -m "feat(judge): a cockpit that explains itself and ticks as you work"
```

---

### Task 11: the tune banner, three states, and the history

**Files:**
- Modify: `src/web/judge.rs`, `src/web/templates/judge.html`, `src/web/templates/_judge_card.html`
- Create: `src/web/templates/_judge_tune.html`
- Modify: `assets/css/42-judge.css`

**Interfaces:**
- Produces: `struct TuneView { rec: Option<Rec>, quiet: String, applied: Vec<String>, flash: String }` with `struct Rec { id: String, line: String, diff: Vec<String> }`; `async fn tune_view(st: &AppState, flash: &str) -> Result<TuneView>`; `tune_fragment` (from Task 9) now renders `_judge_tune.html`.
- Consumes: `open_recommendation`, `latest_eval_run`, `applied_eval_runs` (Task 4); `ago` (this file).

- [ ] **Step 1: Write the failing tests:**

```rust
#[tokio::test]
async fn an_open_recommendation_appears_with_its_diff_and_an_apply_button() {
    let (app, cookie, core, _) = judge_app(1, &[]).await;
    let run = sample_recommended(&core).await;
    let body = get(&app, "/ui/judge", &cookie).await;
    assert!(body.contains(&format!("/ui/judge/tune/{run}/apply")));
    assert!(body.contains("recency"), "the line must name what changes");
    assert!(body.contains("what changes"), "the diff is mandatory, not an extra");
}

#[tokio::test]
async fn a_quiet_sweep_says_so_instead_of_hiding() {
    let (app, cookie, core, _) = judge_app(1, &[]).await;
    sample_quiet(&core).await; // recommended: false
    let body = get(&app, "/ui/judge", &cookie).await;
    assert!(body.contains("no improvement found"), "{body}");
}

#[tokio::test]
async fn below_the_floor_the_banner_is_silent() {
    let (app, cookie, _core, _) = judge_app(1, &[]).await; // no runs at all
    let body = get(&app, "/ui/judge", &cookie).await;
    assert!(!body.contains("no improvement found"));
    assert!(!body.contains("/apply"));
}

#[tokio::test]
async fn applied_changes_stand_in_the_history_with_their_numbers() {
    let (app, cookie, core, _) = judge_app(1, &[]).await;
    let run = sample_recommended(&core).await;
    post(&app, &format!("/ui/judge/tune/{run}/apply"), &cookie, "").await;
    let body = get(&app, "/ui/judge", &cookie).await;
    assert!(body.contains("tuning history"));
    assert!(body.contains("MRR 0.50 → 0.60"), "before/after numbers are the provenance: {body}");
}
```

(`sample_recommended`/`sample_quiet` are the Task 9 fixtures; align the sample numbers so `0.50 → 0.60` matches the Task 4 `sample()` values.)

- [ ] **Step 2: Run to verify failure** — FAIL.

- [ ] **Step 3: Implement.**

```rust
fn cap_str(c: Option<usize>) -> String {
    c.map_or("none".to_string(), |n| n.to_string())
}

/// One sentence naming what would change and what it buys. The numbers come
/// from the run, never recomputed — the settings travel with the result.
fn describe(run: &crate::store::eval_runs::EvalRun) -> String {
    format!(
        "recency {:.2} → {:.2}, cap {} → {}: MRR {:.2} → {:.2}, recall@10 {:.2} → {:.2}",
        run.base_params.recency_weight, run.best_params.recency_weight,
        cap_str(run.base_params.per_source_cap), cap_str(run.best_params.per_source_cap),
        run.base_mrr, run.best_mrr, run.base_recall, run.best_recall,
    )
}

fn rank_str(r: Option<usize>) -> String {
    r.map_or("not returned".to_string(), |i| format!("rank {}", i + 1))
}

async fn tune_view(st: &AppState, flash: &str) -> Result<TuneView> {
    let rec = st.core.store.open_recommendation().await?.map(|run| Rec {
        line: describe(&run),
        diff: run.diff.iter()
            .map(|d| format!("{:<50} {} → {}", d.query, rank_str(d.base), rank_str(d.new)))
            .collect(),
        id: run.id,
    });
    let quiet = match (&rec, st.core.store.latest_eval_run().await?) {
        (None, Some(l)) if !l.recommended =>
            format!("last sweep: {}, no improvement found over {} pairs.", ago(l.created_at), l.pairs_used),
        _ => String::new(),
    };
    let applied = st.core.store.applied_eval_runs(10).await?.iter()
        .map(|r| format!("{} — {}", ago(r.applied_at.unwrap_or(r.created_at)), describe(r)))
        .collect();
    Ok(TuneView { rec, quiet, applied, flash: flash.to_string() })
}
```

`_judge_tune.html` (expects `t` and `t_oob` in scope):

```html
<div id="judge-tune" {% if t_oob %}hx-swap-oob="true"{% endif %} class="judge-tune">
  {% if !t.flash.is_empty() %}<p class="judge-flash">{{ t.flash }}</p>{% endif %}
  {% match t.rec %}
  {% when Some with (r) %}
  <div class="judge-tune-rec">
    <p>{{ r.line }}
      <button class="btn btn-sm" hx-post="/ui/judge/tune/{{ r.id }}/apply"
              hx-target="#judge-tune" hx-swap="outerHTML">Apply</button>
    </p>
    <details><summary class="muted">what changes</summary>
      <ul class="mono">{% for d in r.diff %}<li>{{ d }}</li>{% endfor %}</ul>
    </details>
  </div>
  {% when None %}
  {% if !t.quiet.is_empty() %}<p class="muted hint">{{ t.quiet }}</p>{% endif %}
  {% endmatch %}
  {% if !t.applied.is_empty() %}
  <details class="judge-tune-history"><summary class="muted">tuning history ({{ t.applied.len() }})</summary>
    <ul class="mono">{% for a in t.applied %}<li>{{ a }}</li>{% endfor %}</ul>
  </details>
  {% endif %}
</div>
```

Wire it in: `JudgeTemplate` gains `tune: TuneView` (rendered in `judge.html` between the header and `#card` with `{% let t = tune %}{% let t_oob = false %}{% include "_judge_tune.html" %}`); `CardTemplate` gains `tune: Option<TuneView>` — `card_after` fills it (`Some(tune_view(&st, "").await?)`) so a sweep that finished in the background surfaces on the next verdict, oob (`{% if let Some(t) = tune %}{% let t_oob = true %}{% include "_judge_tune.html" %}{% endif %}` in `_judge_card.html`); the other card paths pass `None`. Task 9's `tune_fragment(&st, line)` becomes:

```rust
async fn tune_fragment(st: &AppState, line: &str) -> Result<Response> {
    use axum::response::IntoResponse;
    Ok(HtmlTemplate(TuneTemplate { t: tune_view(st, line).await?, t_oob: false }).into_response())
}
```

with `#[derive(Template)] #[template(path = "_judge_tune.html")] struct TuneTemplate { t: TuneView, t_oob: bool }` — askama field names must match the include's expectations (`t`, `t_oob`); for the `{% let %}` includes in judge.html/_judge_card.html the bound names are likewise `t`/`t_oob`.

CSS append:

```css
/* The recommendation. Accent-bordered because it is the one actionable thing
   on the page, and gone entirely below the judgement floor. */
.judge-tune { max-width: 52rem; margin-bottom: 1rem; }
.judge-tune-rec { border: 1px solid var(--color-accent); border-radius: var(--radius-sm); padding: 0.5rem 0.75rem; }
.judge-tune-rec ul, .judge-tune-history ul { margin: 0.5rem 0 0; padding-left: 1.25rem; font-size: var(--text-sm); }
.judge-tune li { overflow-wrap: anywhere; }
```

- [ ] **Step 4: Run tests** — `cargo test --lib web::judge` → PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/judge.rs src/web/templates/_judge_tune.html src/web/templates/judge.html src/web/templates/_judge_card.html assets/css/42-judge.css
git commit -m "feat(judge): the sweep speaks on the page it was paid on"
```

---

### Task 12: `FIRST_SWEEP_AT` retires; docs; final verification

**Files:**
- Modify: `src/web/judge.rs` (delete `FIRST_SWEEP_AT`; anything still reading it reads `st.core.feedback.tune.min_judgements`)
- Modify: `docs/evaluation.md`

- [ ] **Step 1: Remove `FIRST_SWEEP_AT`.** Its comment promised "the tuning plan replaces this constant with `feedback.tune.min_judgements`" — keep that promise. `MISS_LIST_AT` stays. Run `cargo test --lib web::judge`.

- [ ] **Step 2: Document.** In `docs/evaluation.md`:
  - In section 6's table, add `src/eval/sweep.rs` — "The runtime sweep: recency × cap over the judged pairs, gated, recommended on `/ui/judge`."
  - New short section between 4 and 5, titled "Tuning at runtime":

```markdown
## 4½. Tuning at runtime

Since the sweep feature, the two cheap knobs tune themselves: once fifty
judgements exist (`feedback.tune.min_judgements`), every tenth further verdict
(`resweep_after`) re-runs a background sweep of recency weight × per-source
cap over the judged pairs, against the live index. A candidate is recommended
only when at least two pairs are net better and neither aggregate is worse;
the recommendation appears on `/ui/judge` with its miss diff, and applying it
rewrites `config.toml` and the running server in one step. Every sweep —
recommended or not — is recorded in `eval_runs` with its settings, which is
the provenance rule from section 4 made structural.

The cargo harness remains the instrument for everything the runtime sweep
cannot reach: embedding models and templates, priming, pinning, the ask side,
and any number that must be comparable across months.
```

  - In section 4's knob table, mark the recency-weight and cap rows: "swept automatically at runtime — see 4½".

- [ ] **Step 3: Full verification**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: clean, all green (the `#[ignore]`d benchmarks stay ignored).

- [ ] **Step 4: Commit**

```bash
git add src/web/judge.rs docs/evaluation.md
git commit -m "docs(eval): the sweep the numbers now buy, and the constant it retires"
```
