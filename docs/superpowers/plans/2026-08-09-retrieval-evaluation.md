# Retrieval Evaluation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Measure ranking quality as recall@10 and MRR over hand-written query/chunk pairs, and make the LLM a hard dependency by deleting the structural segmentation fallback.

**Architecture:** A new library module `engram::eval` owns the on-disk format (frozen chunks, pairs) and the metric functions, so both a preparation binary and an `#[ignore]`d integration test share one definition. The corpus itself never enters the repository; it lives in a directory named by `ENGRAM_EVAL_DIR`. `build_core` moves out of `main.rs` into `Core::from_config` so the binary and the harness construct the same `Core`. Separately, `WindowState::Fallback` becomes `Failed` and the structural split is deleted.

**Tech Stack:** Rust 2024, tokio, sqlx/SQLite, Qdrant REST, serde_json, tempfile (dev), `pdftotext` (already used, outside the build).

## Global Constraints

- Rust edition 2024, `rust-version = "1.94"`. Do not raise the MSRV.
- Corpus text, chunk text, and anything derived from the study material must **never** be committed, printed into the repository, or sent anywhere outside this machine. The eval directory is `/home/user01/engram-eval`, outside the repo.
- No production ranking behaviour changes in this plan. The benchmark and a change to what it measures must not land together.
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` must pass before every commit.
- Comments explain *why*, matching the density and voice of the surrounding code. No comment restates what the line does.
- Tests live in the same file as the code under test (`#[cfg(test)] mod tests`), except integration tests under `tests/`.

---

### Task 1: Move core construction into the library

`build_core` is private in `src/main.rs`, so nothing outside the binary can build a real `Core` from configuration. The eval binary and the eval test both need one. Move it, unchanged in behaviour, to `Core::from_config`.

**Files:**
- Modify: `src/core/mod.rs` (add `impl Core { pub fn from_config(..) }`)
- Modify: `src/main.rs:57-86` (delete `build_core`, call the new method)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `Core::from_config(cfg: &Config, vectors: Arc<dyn VectorStore>, store: Store) -> Core`, used by Tasks 4 and 5.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` at the bottom of `src/core/mod.rs`. If that module does not exist, create it below `pub mod test_support`.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// The one wiring decision `from_config` makes that is not a straight
    /// field copy: rerank is optional, and an absent block must leave search
    /// in vector order rather than panicking or defaulting to an endpoint.
    #[tokio::test]
    async fn rerank_is_wired_only_when_configured() {
        let store = Store::memory().await.unwrap();
        let vectors = Arc::new(crate::vector::memory::MemoryVectors::new());

        // `Config` has no `Default`, and adding one just for a test would put
        // a fake endpoint in the type. The committed example file is a real
        // config and costs nothing to read.
        let mut cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        cfg.infer.rerank = None;
        let core = Core::from_config(&cfg, vectors.clone(), store.clone());
        assert!(core.reranker.is_none());

        cfg.infer.rerank = Some(crate::config::RerankRole {
            base_url: "http://localhost:8001".into(),
            model: "bge-reranker-v2-m3".into(),
            style: crate::config::RerankStyle::Tei,
            api_key: None,
            timeout_secs: 60,
        });
        let core = Core::from_config(&cfg, vectors, store);
        assert!(core.reranker.is_some());
    }
}
```

`RerankRole` is `{ base_url: String, model: String, api_key: Option<String>, style: RerankStyle, timeout_secs: u64 }` (`src/config.rs:143`), and `RerankStyle::Tei` is one of `Tei | Cohere | Vllm`. `config.example.toml` has no `[infer.rerank]` block, so the first assertion holds without touching it.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib core::tests::rerank_is_wired_only_when_configured`
Expected: FAIL, compilation error `no function or associated item named 'from_config' found for struct 'Core'`.

- [ ] **Step 3: Add `Core::from_config`**

In `src/core/mod.rs`, add the imports the moved body needs and the impl block:

```rust
use crate::config::Config;
use crate::infer::openai::{HttpChunker, HttpCompleter, HttpEmbedder, HttpReranker};

impl Core {
    /// Build the running core from configuration. Lives here rather than in
    /// `main`, so the evaluation harness drives exactly the `Core` the binary
    /// does — a benchmark against a differently wired core measures the wrong
    /// program.
    pub fn from_config(
        cfg: &Config,
        vectors: Arc<dyn VectorStore>,
        store: crate::store::Store,
    ) -> Core {
        // Chunk size is capped by what the embedder accepts, with headroom for
        // token-count estimation error.
        let max_chunk_tokens = (cfg.infer.embed.max_input_tokens as f32 * 0.8) as usize;

        Core {
            store,
            vectors,
            chunker: Arc::new(
                HttpChunker::new(&cfg.infer.chunk).with_max_chunk_tokens(max_chunk_tokens),
            ),
            embedder: Arc::new(HttpEmbedder::new(&cfg.infer.embed)),
            reranker: cfg
                .infer
                .rerank
                .as_ref()
                .map(|r| Arc::new(HttpReranker::new(r)) as Arc<dyn crate::infer::Reranker>),
            completer: Arc::new(HttpCompleter::new(&cfg.infer.ask)),
            counter: Arc::new(TokenCounter::load(cfg.infer.chunk.tokenizer_path.as_deref())),
            background: Arc::new(background::Background::default()),
            query_cache: Arc::new(std::sync::Mutex::new(QueryCache::new(QUERY_CACHE_CAPACITY))),
        }
    }
}
```

- [ ] **Step 4: Delete `build_core` from `main.rs` and call the new method**

Remove `fn build_core(...)` at `src/main.rs:57-86` entirely. Replace each call site (search for `build_core(`) with `Core::from_config(&cfg, vectors, store)`, keeping the same argument values. Delete any `use` in `main.rs` that is now unused — the compiler will name them.

- [ ] **Step 5: Run the test and the whole suite**

Run: `cargo test --lib core::tests::rerank_is_wired_only_when_configured`
Expected: PASS

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: all pass, no warnings.

- [ ] **Step 6: Commit**

```bash
git add src/core/mod.rs src/main.rs
git commit -m "refactor: build the core from config inside the library

The evaluation harness has to drive the same Core the binary does, and
build_core was private to main. Behaviour is unchanged."
```

---

### Task 2: Delete the structural segmentation fallback

A window the model refuses currently gets split on blank lines and stored verbatim, producing chunks with no title, no category, no tags, and no rewriting to stand alone. The model is a hard dependency now: such a window stays unsegmented and says why.

**Files:**
- Create: `migrations/0006_window_failed.sql`
- Modify: `src/store/windows.rs:5-30` (enum and doc comment), `src/store/windows.rs:146-148` (doc comment on `window_progress`), tests at `:206-240`
- Modify: `src/jobs/segment.rs:286-367` (replace `fallback_pending_windows`), `:150-160` region (comment referencing the fallback)
- Modify: `src/jobs/mod.rs:50-70` (the exhausted-segment branch)
- Modify: `src/infer/split.rs:133-156` (delete `structural_chunks`), `:262` (delete its test)
- Modify: `src/jobs/embed.rs:390` (comment), `:736` (test name and body)
- Modify: `src/web/ui.rs:863` (test uses `WindowState::Fallback`)
- Leave alone: `migrations/0005_segment_windows.sql`. An applied migration is immutable; its `-- pending | done | fallback` comment is now history, and `0006` carries the change.

**Interfaces:**
- Consumes: nothing.
- Produces: `WindowState::Failed`; `segment::fail_pending_windows(core: &Core, source_id: &str, reason: &str) -> Result<bool>` replacing `fallback_pending_windows` with the same return meaning (`true` when windows are still waiting for the model and the caller should requeue).

- [ ] **Step 1: Write the failing tests**

In `src/jobs/segment.rs`, inside `#[cfg(test)] mod tests`, replace any test exercising the fallback and add these. Read the existing tests first — `test_core_with_failing_chunker` and `multi_window_body` already exist and are what these build on.

```rust
#[tokio::test]
async fn a_window_the_model_refuses_is_marked_failed_not_split() {
    let core = test_core_with_failing_chunker().await;
    let out = core.ingest("first para\n\nsecond para", "web", None).await.unwrap();

    // The job itself fails; the runner is what decides the window's fate.
    assert!(run(&core, &out.id).await.is_err());
    let requeue = fail_pending_windows(&core, &out.id, "endpoint down").await.unwrap();

    assert!(!requeue, "nothing is left waiting when every window failed");
    let w = &core.store.windows_for_source(&out.id).await.unwrap()[0];
    assert_eq!(w.state, WindowState::Failed);
    assert_eq!(w.last_error.as_deref(), Some("endpoint down"));

    // The point of the change: no paragraph-shaped debris.
    assert!(
        core.store.chunks_for_source(&out.id).await.unwrap().is_empty(),
        "a refused window must produce no chunks at all"
    );
    assert_eq!(
        core.store.get_source(&out.id).await.unwrap().status,
        SourceStatus::Failed
    );
}

#[tokio::test]
async fn windows_that_succeeded_keep_their_chunks_when_a_later_one_fails() {
    let core = test_core().await;
    let out = core.ingest("first para\n\nsecond para", "web", None).await.unwrap();
    run(&core, &out.id).await.unwrap();
    let before = core.store.chunks_for_source(&out.id).await.unwrap().len();
    assert!(before > 0);

    // A window that was never windowed away still exists; force one back to
    // pending and fail it, which is the partial case.
    core.store.upsert_windows(&out.id, &[(1, 1), (2, 3)]).await.unwrap();
    core.store.bump_window_attempts(&out.id, 1).await.unwrap();
    fail_pending_windows(&core, &out.id, "unparsable output").await.unwrap();

    assert_eq!(
        core.store.chunks_for_source(&out.id).await.unwrap().len(),
        before,
        "a failed window must not disturb the chunks another window earned"
    );
    assert_eq!(
        core.store.get_source(&out.id).await.unwrap().status,
        SourceStatus::Partial
    );
}

#[tokio::test]
async fn a_window_that_was_never_tried_stays_queued() {
    let core = test_core().await;
    let out = core.ingest("first para\n\nsecond para", "web", None).await.unwrap();
    core.store.upsert_windows(&out.id, &[(1, 1), (2, 3)]).await.unwrap();
    // Only window 0 has spent an attempt.
    core.store.bump_window_attempts(&out.id, 0).await.unwrap();

    let requeue = fail_pending_windows(&core, &out.id, "endpoint down").await.unwrap();

    assert!(requeue, "an untried window earns the source another job");
    let ws = core.store.windows_for_source(&out.id).await.unwrap();
    assert_eq!(ws[0].state, WindowState::Failed);
    assert_eq!(ws[1].state, WindowState::Pending);
}
```

Also update, in place, the three existing tests that name the old state:
- `src/store/windows.rs`: `progress_counts_done_and_fallback_as_resolved` → `progress_counts_done_and_failed_as_resolved`, with `WindowState::Fallback` → `WindowState::Failed` at both call sites (`:217`, `:234`).
- `src/jobs/embed.rs`: `a_fallback_segmented_source_is_not_promoted_to_ready` → `a_partially_segmented_source_is_not_promoted_to_ready`, same substitution inside.
- `src/web/ui.rs:863`: `WindowState::Fallback` → `WindowState::Failed`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib jobs::segment`
Expected: FAIL, compilation errors — `no variant named 'Failed' found for enum 'WindowState'` and `cannot find function 'fail_pending_windows'`.

- [ ] **Step 3: Rename the window state**

In `src/store/windows.rs`, replace the enum and its doc comment:

```rust
/// Where one window of a source stands. `Failed` means the chunker never
/// succeeded here and the lines are not represented by any chunk — the model
/// is a hard dependency, so an unsegmentable window leaves a hole the source's
/// coverage measures rather than filling it with a worse split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowState {
    Pending,
    Done,
    Failed,
}

impl WindowState {
    pub fn as_str(&self) -> &'static str {
        match self {
            WindowState::Pending => "pending",
            WindowState::Done => "done",
            WindowState::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> WindowState {
        match s {
            "done" => WindowState::Done,
            "failed" => WindowState::Failed,
            _ => WindowState::Pending,
        }
    }
}
```

And the doc comment on `window_progress` (`src/store/windows.rs:146`):

```rust
    /// `(resolved, total)`, where resolved counts both a clean window and one
    /// the model gave up on. Both are settled; neither is still owed work.
```

- [ ] **Step 4: Write the migration**

Create `migrations/0006_window_failed.sql`:

```sql
-- The structural fallback is gone: the LLM is a hard dependency, so a window
-- it refuses is now a hole rather than a worse split. Rows written under the
-- old name are the same situation minus the debris chunks, which stay where
-- they are — deleting them would silently shrink existing sources.
UPDATE segment_windows SET state = 'failed' WHERE state = 'fallback';
```

- [ ] **Step 5: Replace the fallback with a failure**

In `src/jobs/segment.rs`, delete `fallback_pending_windows` in full (`:286-367`) and put this in its place:

```rust
/// Settle the windows a spent job leaves behind.
///
/// Scoped to windows that have actually been tried: a local endpoint fails in
/// bursts, and the attempt count belongs to the job rather than the window, so
/// an outage during window 1 must not condemn windows the model never saw.
/// Those stay queued.
///
/// Returns whether windows are still waiting for the model, which the caller
/// answers with a fresh job. It cannot be enqueued here: the caller's own job
/// row is keyed `(stage, target_id)`, so enqueuing the same source would reuse
/// that row and the `complete_job` that follows would close it again.
pub async fn fail_pending_windows(core: &Core, source_id: &str, reason: &str) -> Result<bool> {
    let pending = core.store.pending_windows(source_id).await?;
    if pending.is_empty() {
        finish(core, source_id).await?;
        return Ok(false);
    }

    let (tried, untried): (Vec<_>, Vec<_>) = pending.into_iter().partition(|w| w.attempts > 0);

    if !untried.is_empty() {
        tracing::info!(
            source_id,
            windows = untried.len(),
            "leaving untried windows queued rather than failing them"
        );
    }

    if tried.is_empty() {
        // Nothing has earned a verdict yet; the caller queues another attempt.
        return Ok(true);
    }

    for w in tried {
        core.store
            .set_window_state(source_id, w.idx, WindowState::Failed, Some(reason))
            .await?;
        tracing::warn!(
            source_id,
            window = w.idx,
            lines = format!("{}-{}", w.start_line, w.end_line),
            reason,
            "window could not be segmented; its lines have no chunk"
        );
    }

    if core.store.pending_windows(source_id).await?.is_empty() {
        finish(core, source_id).await?;
        return Ok(false);
    }
    Ok(true)
}
```

`finish` already handles both outcomes: it sets `Failed` when the source has no chunks at all, and `Partial` when any window is not `Done`. Check `finish` reads `w.state != WindowState::Done` rather than `== WindowState::Fallback`; if it still tests the old variant (`src/jobs/segment.rs:246`), change that line to:

```rust
    let degraded = windows.iter().any(|w| w.state != WindowState::Done);
```

- [ ] **Step 6: Update the job runner**

In `src/jobs/mod.rs:50-70`, replace the comment and the call:

```rust
                // Out of attempts against the chunker. The windows that were
                // actually tried are recorded as failed; the ones that never
                // ran go back in the queue.
                (Stage::Segment, _) if exhausted => {
                    tracing::warn!(error = %e, "segmentation exhausted retries; failing the windows it tried");
                    match segment::fail_pending_windows(core, &job.target_id, &e.to_string())
                        .await
```

Leave the rest of that branch — `complete_job`, the requeue, the ordering comment — exactly as it is.

- [ ] **Step 7: Delete `structural_chunks`**

In `src/infer/split.rs`, delete `pub fn structural_chunks` (`:133-156`) and its test `structural_fallback_splits_on_paragraphs` (`:262`). Remove `structural_chunks` from the `use crate::infer::split::{...}` list at `src/jobs/segment.rs:4`. In `src/infer/split.rs`, the test `empty_input_produces_nothing` (`:284`) asserts on it too — drop that one assertion, keep the `split_into_windows` one.

- [ ] **Step 8: Update the stale comment in embed**

`src/jobs/embed.rs:390`:

```rust
        // A source with a window the model refused is already partial. Its
        // chunks embedding cleanly does not fill the hole, and reporting
        // `ready` would hide it.
```

- [ ] **Step 9: Run the tests**

Run: `cargo test`
Expected: PASS. `grep -rn "fallback\|Fallback" src/ migrations/0006_window_failed.sql` should return only the historical note in `migrations/0005_segment_windows.sql` and the unrelated hybrid-search comment at `src/web/ui.rs:30`.

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: pass.

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "feat!: make the LLM a hard dependency for segmentation

A window the model refuses used to be split on blank lines and stored
verbatim, which is not what the rest of the system means by a chunk: no
title, no category, no tags, not rewritten to stand alone. It now stays
unsegmented and records why, the source ends partial, and coverage
measures the hole.

Windows the model never saw are still left queued rather than condemned
by an outage during an earlier window."
```

---

### Task 3: The eval module — format and metrics

Pure code with no I/O beyond reading two JSON files. It is what makes the harness in Task 5 short enough to read.

**Files:**
- Create: `src/eval/mod.rs`
- Create: `src/eval/metrics.rs`
- Modify: `src/lib.rs` (add `pub mod eval;`)
- Modify: `.gitignore`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `engram::eval::eval_dir() -> std::path::PathBuf`
  - `engram::eval::FrozenChunk { id: String, source: String, text: String, title: Option<String>, category: Option<String>, tags: Vec<String> }`
  - `engram::eval::EvalPair { query: String, expect: String, note: Option<String> }`
  - `engram::eval::load_chunks(dir: &Path) -> anyhow::Result<Vec<FrozenChunk>>`
  - `engram::eval::save_chunks(dir: &Path, chunks: &[FrozenChunk]) -> anyhow::Result<()>`
  - `engram::eval::load_pairs(dir: &Path) -> anyhow::Result<Vec<EvalPair>>`
  - `engram::eval::metrics::recall_at(ranks: &[Option<usize>], k: usize) -> f64`
  - `engram::eval::metrics::mrr(ranks: &[Option<usize>]) -> f64`

Ranks are zero-based positions in the result list; `None` means the expected chunk was not returned at all.

- [ ] **Step 1: Write the failing metric tests**

Create `src/eval/metrics.rs` containing only the tests for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_counts_a_hit_anywhere_within_k() {
        // ranks are zero-based: 0 is the top result, 9 is the tenth.
        let ranks = [Some(0), Some(9), Some(10), None];
        assert!((recall_at(&ranks, 10) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn recall_of_nothing_found_is_zero_not_a_division_by_zero() {
        assert_eq!(recall_at(&[None, None], 10), 0.0);
        assert_eq!(recall_at(&[], 10), 0.0);
    }

    #[test]
    fn mrr_is_the_mean_of_the_reciprocal_ranks() {
        // 1/1 and 1/2, averaged over three queries including the miss.
        let ranks = [Some(0), Some(1), None];
        assert!((mrr(&ranks) - (1.0 + 0.5) / 3.0).abs() < 1e-9);
    }

    #[test]
    fn a_miss_contributes_nothing_to_mrr_rather_than_being_dropped() {
        // Dropping misses would make a system that answers one query
        // perfectly and fails nineteen score 1.0.
        assert!((mrr(&[Some(0), None]) - 0.5).abs() < 1e-9);
        assert_eq!(mrr(&[]), 0.0);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Add `pub mod eval;` to `src/lib.rs` and `pub mod metrics;` to a stub `src/eval/mod.rs`, then:

Run: `cargo test --lib eval::metrics`
Expected: FAIL, `cannot find function 'recall_at' in this scope`.

- [ ] **Step 3: Implement the metrics**

At the top of `src/eval/metrics.rs`, above the tests:

```rust
//! Two numbers, because they answer different questions.
//!
//! Recall asks whether the answer was on the page at all. MRR asks how far
//! down it was. A ranking change can improve one and hurt the other, and which
//! matters is a judgement about what a search page is for — so the harness
//! reports both rather than choosing.

/// Fraction of queries whose expected chunk landed within the first `k`
/// results. `None` is a miss.
pub fn recall_at(ranks: &[Option<usize>], k: usize) -> f64 {
    if ranks.is_empty() {
        return 0.0;
    }
    let hits = ranks.iter().filter(|r| matches!(r, Some(i) if *i < k)).count();
    hits as f64 / ranks.len() as f64
}

/// Mean reciprocal rank, counting a miss as zero rather than excluding it.
/// Excluding misses would let a system that answers one query perfectly and
/// fails nineteen report a perfect score.
pub fn mrr(ranks: &[Option<usize>]) -> f64 {
    if ranks.is_empty() {
        return 0.0;
    }
    let total: f64 = ranks
        .iter()
        .map(|r| r.map_or(0.0, |i| 1.0 / (i as f64 + 1.0)))
        .sum();
    total / ranks.len() as f64
}
```

- [ ] **Step 4: Run to verify the metric tests pass**

Run: `cargo test --lib eval::metrics`
Expected: PASS, 4 tests.

- [ ] **Step 5: Write the format test**

Append to `src/eval/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_survive_a_round_trip_through_the_frozen_file() {
        let dir = tempfile::tempdir().unwrap();
        let chunks = vec![FrozenChunk {
            id: "01J8".into(),
            source: "dateisysteme-fat.txt".into(),
            text: "Ein Cluster ist die kleinste adressierbare Einheit.".into(),
            title: Some("Cluster".into()),
            category: Some("concept".into()),
            tags: vec!["fat".into()],
        }];

        save_chunks(dir.path(), &chunks).unwrap();
        assert_eq!(load_chunks(dir.path()).unwrap(), chunks);
    }

    #[test]
    fn a_missing_pairs_file_says_which_path_it_wanted() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_pairs(dir.path()).unwrap_err().to_string();
        assert!(err.contains("pairs.json"), "unhelpful error: {err}");
    }

    #[test]
    fn the_eval_directory_comes_from_the_environment() {
        temp_env::with_var("ENGRAM_EVAL_DIR", Some("/somewhere/else"), || {
            assert_eq!(eval_dir(), std::path::PathBuf::from("/somewhere/else"));
        });
        temp_env::with_var_unset("ENGRAM_EVAL_DIR", || {
            assert_eq!(eval_dir(), std::path::PathBuf::from("eval-data"));
        });
    }
}
```

`tempfile` and `temp_env` are already dev-dependencies.

- [ ] **Step 6: Run to verify it fails**

Run: `cargo test --lib eval::tests`
Expected: FAIL, `cannot find struct 'FrozenChunk'`.

- [ ] **Step 7: Implement the module**

Replace the stub `src/eval/mod.rs` with:

```rust
//! Retrieval evaluation: the on-disk format and the metrics.
//!
//! The corpus this measures is not in the repository and must not be. It is
//! real study material, and what lives here is only the shape of the files and
//! the arithmetic over ranks — see
//! `docs/superpowers/specs/2026-08-09-retrieval-evaluation-design.md`.

pub mod metrics;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Where the corpus, the frozen chunks and the pairs live. Outside the
/// repository by default; the in-repo fallback exists only so an error message
/// can name a concrete path, and it is gitignored.
pub fn eval_dir() -> PathBuf {
    std::env::var("ENGRAM_EVAL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("eval-data"))
}

/// One chunk as the segmenter produced it, frozen so a benchmark run costs no
/// completions and two runs rank exactly the same text.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FrozenChunk {
    pub id: String,
    /// Corpus file this came from. Also what the per-source cap groups by, so
    /// it has to survive the freeze.
    pub source: String,
    pub text: String,
    pub title: Option<String>,
    pub category: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A query and the chunk that should answer it.
///
/// The query is meant to be phrased as a situation, in the words a reader
/// happens to have — not in the vocabulary of the chunk. A pair that shares
/// the chunk's terminology measures nothing: every retrieval system passes it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EvalPair {
    pub query: String,
    /// `FrozenChunk::id` of the expected answer.
    pub expect: String,
    #[serde(default)]
    pub note: Option<String>,
}

pub fn chunks_path(dir: &Path) -> PathBuf {
    dir.join("chunks.json")
}

pub fn pairs_path(dir: &Path) -> PathBuf {
    dir.join("pairs.json")
}

pub fn load_chunks(dir: &Path) -> Result<Vec<FrozenChunk>> {
    let path = chunks_path(dir);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

pub fn save_chunks(dir: &Path, chunks: &[FrozenChunk]) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = chunks_path(dir);
    let json = serde_json::to_string_pretty(chunks)?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))
}

pub fn load_pairs(dir: &Path) -> Result<Vec<EvalPair>> {
    let path = pairs_path(dir);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}
```

- [ ] **Step 8: Guard the default path**

Append to `.gitignore`:

```
# Evaluation corpus. Real study material, not publishable — the harness reads
# it from ENGRAM_EVAL_DIR, and this is only the default path's guard.
/eval-data/
```

- [ ] **Step 9: Run the tests**

Run: `cargo test --lib eval && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: PASS, 7 tests in `eval`.

- [ ] **Step 10: Commit**

```bash
git add src/eval src/lib.rs .gitignore
git commit -m "feat: add the evaluation file format and metrics

recall@10 and MRR over zero-based ranks, plus the frozen-chunk and pair
formats both the preparation binary and the harness read. A miss counts
as zero in MRR rather than being dropped, or a system that answers one
query and fails nineteen would score perfectly.

The corpus itself stays out of the repository."
```

---

### Task 4: `eval-prepare` — freeze the chunks

Turns `$ENGRAM_EVAL_DIR/corpus/*.txt` into `chunks.json` by running the real segmenter once. Needs the chunk endpoint from `config.toml`; needs no Qdrant, because nothing is embedded here.

**Files:**
- Create: `src/bin/eval_prepare.rs`
- Modify: `Cargo.toml` (declare the binary explicitly — an explicit `[[bin]]` is already present, so be explicit for both)

**Interfaces:**
- Consumes: `Core::from_config` (Task 1), `engram::eval::{FrozenChunk, save_chunks, eval_dir}` (Task 3).
- Produces: `$ENGRAM_EVAL_DIR/chunks.json`. Nothing in code depends on this binary.

- [ ] **Step 1: Declare the binary**

Add to `Cargo.toml`, after the existing `[[bin]]` block:

```toml
# Freezes the evaluation corpus into chunks.json by running the real
# segmenter once. Not part of the server; it exists so a benchmark run costs
# no completions.
[[bin]]
name = "eval-prepare"
path = "src/bin/eval_prepare.rs"
```

- [ ] **Step 2: Write the binary**

Create `src/bin/eval_prepare.rs`:

```rust
//! Freeze the evaluation corpus: segment every `corpus/*.txt` once with the
//! real chunker and write the result to `chunks.json`.
//!
//! Run deliberately, not per benchmark. Segmenting on every run would cost a
//! completion per window and return slightly different chunks each time, so a
//! two percent ranking change would be indistinguishable from segmenter noise.
//!
//! Chunk ids change on every run, so re-running invalidates `pairs.json` and
//! the pairs have to be re-checked. That is the point of it being a separate
//! command.
//!
//!   ENGRAM_EVAL_DIR=/home/user01/engram-eval cargo run --bin eval-prepare

use anyhow::{Context, Result, bail};
use engram::config::Config;
use engram::core::Core;
use engram::eval::{FrozenChunk, eval_dir, save_chunks};
use engram::store::Store;
use engram::vector::memory::MemoryVectors;
use std::sync::Arc;

/// A window is retried this many times before it is called hopeless. The job
/// runner has its own budget; this loop is standing in for the runner.
const MAX_PASSES: usize = 4;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let dir = eval_dir();
    let corpus = dir.join("corpus");
    if !corpus.is_dir() {
        bail!(
            "no corpus at {}. Put the extracted .txt files there, or set ENGRAM_EVAL_DIR.",
            corpus.display()
        );
    }

    let cfg = Config::load(None).context("loading config.toml")?;
    // A throwaway in-memory store and vector index: this run produces a JSON
    // file, not a searchable instance.
    let store = Store::memory().await?;
    let core = Core::from_config(&cfg, Arc::new(MemoryVectors::new()), store);

    let mut files: Vec<_> = std::fs::read_dir(&corpus)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    files.sort();
    if files.is_empty() {
        bail!("no .txt files in {}", corpus.display());
    }

    let mut frozen: Vec<FrozenChunk> = Vec::new();
    for path in &files {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed")
            .to_string();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;

        tracing::info!(file = %name, bytes = text.len(), "segmenting");
        let out = core.ingest(&text, "eval", Some(&name)).await?;

        // Stand in for the job runner: `segment::run` resumes from the first
        // pending window, so repeating it is what drives a multi-window source
        // to completion.
        let mut passes = 0;
        loop {
            match engram::jobs::segment::run(&core, &out.id).await {
                Ok(()) => {}
                Err(e) => tracing::warn!(error = %e, file = %name, "segmentation pass failed"),
            }
            let pending = core.store.pending_windows(&out.id).await?;
            passes += 1;
            if pending.is_empty() {
                break;
            }
            if passes >= MAX_PASSES {
                bail!(
                    "{name}: {} window(s) still unsegmented after {MAX_PASSES} passes. \
                     The corpus must be fully segmented, or the benchmark ranks a \
                     document with holes in it.",
                    pending.len()
                );
            }
        }

        let chunks = core.store.chunks_for_source(&out.id).await?;
        tracing::info!(file = %name, chunks = chunks.len(), "segmented");
        for c in chunks {
            frozen.push(FrozenChunk {
                id: c.id,
                source: name.clone(),
                text: c.text,
                title: c.title,
                category: c.category,
                tags: c.tags,
            });
        }
    }

    save_chunks(&dir, &frozen)?;
    println!(
        "froze {} chunks from {} documents into {}",
        frozen.len(),
        files.len(),
        dir.join("chunks.json").display()
    );
    Ok(())
}
```

- [ ] **Step 3: Build it**

Run: `cargo build --bin eval-prepare`
Expected: compiles clean. If `engram::jobs::segment::run` or `Store::memory` is not public, make it so in the smallest way — do not duplicate the logic here.

- [ ] **Step 4: Check clippy and formatting**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml src/bin/eval_prepare.rs
git commit -m "feat: add eval-prepare, which freezes the eval corpus

Segments every corpus/*.txt once with the real chunker and writes
chunks.json, so a benchmark run costs no completions and two runs rank
exactly the same text. It refuses to write a partially segmented corpus:
a document with holes would silently change what the numbers mean."
```

- [ ] **Step 6: Run it against the real corpus**

This is the one step that touches the private material. It writes only to `/home/user01/engram-eval/`.

Run:
```bash
ENGRAM_EVAL_DIR=/home/user01/engram-eval cargo run --bin eval-prepare
```
Expected: `froze N chunks from 3 documents into /home/user01/engram-eval/chunks.json`, with N somewhere in the low hundreds.

Do **not** commit anything from that directory, and do not paste chunk text into the repository, a commit message, or any external service.

---

### Task 5: The harness

**Files:**
- Create: `tests/eval.rs`
- Modify: `README.md` (a short section on running it)

**Interfaces:**
- Consumes: `Core::from_config` (Task 1), `engram::eval::*` and `engram::eval::metrics::*` (Task 3), `chunks.json` (Task 4), `pairs.json` (written by hand after this task).
- Produces: nothing other code uses.

- [ ] **Step 1: Write the harness**

Create `tests/eval.rs`:

```rust
//! Retrieval evaluation over hand-written query/chunk pairs.
//!
//! Requires a running Qdrant and a real embedding endpoint, which is why it is
//! `#[ignore]`d — the fake embedder produces meaningless vectors, so a
//! benchmark built on it would measure nothing.
//!
//! Run with:
//!   ENGRAM_EVAL_DIR=/home/user01/engram-eval \
//!     cargo test --test eval -- --ignored --nocapture
//!
//! Ranking settings come from configuration, so a sweep is a loop over
//! environment variables rather than a rebuild:
//!   ENGRAM__VECTOR__RECENCY_WEIGHT=0.0 …
//!   ENGRAM_EVAL_CAP=none              (disable the per-source cap)
//!
//! The corpus it reads is private study material. Nothing here prints chunk
//! text beyond the leading characters of a query, and none of it belongs in
//! the repository.

use engram::config::Config;
use engram::core::Core;
use engram::core::search::SearchQuery;
use engram::eval::metrics::{mrr, recall_at};
use engram::eval::{EvalPair, FrozenChunk, eval_dir, load_chunks, load_pairs};
use engram::store::Store;
use engram::store::chunks::NewChunk;
use engram::vector::qdrant::QdrantVectors;
use std::sync::Arc;

/// Results asked of the search path per query, and the `k` in recall@k.
const LIMIT: usize = 10;

/// Its own collection, dropped before and after, so a run is never polluted by
/// the previous one or by the operator's real index.
const COLLECTION: &str = "engram_eval";

fn cap_from_env() -> Option<usize> {
    match std::env::var("ENGRAM_EVAL_CAP").ok().as_deref() {
        None => Some(engram::core::search::MAX_PER_SOURCE),
        Some("none") => None,
        Some(n) => Some(n.parse().expect("ENGRAM_EVAL_CAP must be a number or 'none'")),
    }
}

#[tokio::test]
#[ignore]
async fn evaluate_retrieval() {
    let dir = eval_dir();
    let (chunks, pairs) = match (load_chunks(&dir), load_pairs(&dir)) {
        (Ok(c), Ok(p)) => (c, p),
        (c, p) => {
            let why = c.err().map(|e| e.to_string()).unwrap_or_default()
                + &p.err().map(|e| format!(" {e}")).unwrap_or_default();
            eprintln!(
                "no evaluation corpus at {} ({}). Set ENGRAM_EVAL_DIR and run \
                 `cargo run --bin eval-prepare` first; see \
                 docs/superpowers/specs/2026-08-09-retrieval-evaluation-design.md.",
                dir.display(),
                why.trim()
            );
            return;
        }
    };
    assert!(!chunks.is_empty(), "chunks.json is empty");
    assert!(!pairs.is_empty(), "pairs.json is empty");

    let mut cfg = Config::load(None).expect("config.toml");
    cfg.vector.collection = COLLECTION.to_string();

    let vectors = Arc::new(QdrantVectors::connect(&cfg.vector).await.unwrap());
    vectors.drop_collection().await.unwrap();
    vectors.ensure_collection(cfg.infer.embed.dim).await.unwrap();

    let store = Store::memory().await.unwrap();
    let core = Core::from_config(&cfg, vectors.clone(), store);

    index(&core, &chunks).await;

    let cap = cap_from_env();
    let mut ranks: Vec<Option<usize>> = Vec::with_capacity(pairs.len());
    let mut misses: Vec<(&EvalPair, Option<usize>)> = Vec::new();

    for pair in &pairs {
        let q = SearchQuery {
            q: pair.query.clone(),
            limit: LIMIT,
            tags: vec![],
            category: None,
            // A benchmark must not stamp last_seen_at: resurfacing reads the
            // same field, and a scored run is not someone reading their notes.
            mark: false,
        };
        let results = core.search_capped(&q, cap).await.expect("search failed");
        let rank = results.iter().position(|r| r.chunk_id == pair.expect);
        if rank.is_none_or(|i| i >= LIMIT) {
            misses.push((pair, rank));
        }
        ranks.push(rank);
    }

    report(&cfg, &chunks, &pairs, &ranks, &misses, cap);
    vectors.drop_collection().await.unwrap();
}

/// Load the frozen chunks and embed them.
///
/// One SQLite source per corpus file, because `source_id` is what the
/// per-source cap groups by — collapsing the corpus into one source would
/// silently disable the cap and measure a different program.
async fn index(core: &Core, chunks: &[FrozenChunk]) {
    let mut by_source: std::collections::BTreeMap<&str, Vec<&FrozenChunk>> = Default::default();
    for c in chunks {
        by_source.entry(c.source.as_str()).or_default().push(c);
    }

    for (name, group) in by_source {
        // The raw text has to differ per source: sources are deduplicated by
        // a hash of it.
        let src = core
            .store
            .insert_source(&format!("eval corpus: {name}"), "eval", Some(name))
            .await
            .unwrap();
        let new: Vec<NewChunk> = group
            .iter()
            .enumerate()
            .map(|(i, c)| NewChunk {
                ordinal: i as i64,
                text: c.text.clone(),
                source_span: None,
                title: c.title.clone(),
                category: c.category.clone(),
                tags: c.tags.clone(),
                window_idx: None,
            })
            .collect();
        core.store.insert_chunks(&src.id, &new).await.unwrap();
        engram::jobs::embed::run_source(core, &src.id)
            .await
            .expect("embedding the corpus failed");
    }
}

fn report(
    cfg: &Config,
    chunks: &[FrozenChunk],
    pairs: &[EvalPair],
    ranks: &[Option<usize>],
    misses: &[(&EvalPair, Option<usize>)],
    cap: Option<usize>,
) {
    let found = ranks.iter().filter(|r| matches!(r, Some(i) if *i < LIMIT)).count();
    println!(
        "\n{} queries over {} chunks   (embed {}, rerank {}, recency {}, cap {})",
        pairs.len(),
        chunks.len(),
        cfg.infer.embed.model,
        if cfg.infer.rerank.is_some() { "on" } else { "off" },
        cfg.vector.recency_weight,
        cap.map_or("none".to_string(), |c| c.to_string()),
    );
    println!(
        "recall@{LIMIT}   {:.2}   ({}/{})",
        recall_at(ranks, LIMIT),
        found,
        pairs.len()
    );
    println!("MRR         {:.2}\n", mrr(ranks));

    if misses.is_empty() {
        println!("no misses.");
        return;
    }
    // The list that is actually read. An aggregate says something moved; this
    // says what.
    println!("missed:");
    for (pair, rank) in misses {
        let q: String = pair.query.chars().take(48).collect();
        match rank {
            Some(i) => println!("  {q:<50} rank {}", i + 1),
            None => println!("  {q:<50} not returned"),
        }
    }
    println!();
}
```

- [ ] **Step 2: Verify it compiles and skips cleanly without a corpus**

Run: `cargo test --test eval -- --ignored --nocapture`
Expected: with `ENGRAM_EVAL_DIR` unset, it prints `no evaluation corpus at eval-data (...)` and passes. It must not panic — a developer without the private corpus gets an explanation.

- [ ] **Step 3: Check clippy and formatting**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: pass.

- [ ] **Step 4: Document how to run it**

Add to `README.md`, in whatever section covers testing (match the existing heading style):

````markdown
### Evaluating retrieval

Ranking has several knobs and no way to tell whether turning one helped. The
evaluation harness answers that with two numbers over hand-written
query/chunk pairs: recall@10, and MRR.

The corpus is not in this repository — it is whatever documents you actually
want to search, and they stay on your machine:

```
$ENGRAM_EVAL_DIR/corpus/*.txt   your documents
$ENGRAM_EVAL_DIR/chunks.json    written by eval-prepare
$ENGRAM_EVAL_DIR/pairs.json     written by hand
```

Freeze the chunks once, so a benchmark run costs no completions:

```
ENGRAM_EVAL_DIR=~/engram-eval cargo run --bin eval-prepare
```

Write pairs — a query phrased the way you would actually type it, and the id
of the chunk that should answer it. Pairs that reuse the chunk's own
vocabulary measure nothing; the useful ones share almost no words with their
answer.

```json
[
  { "query": "handy war aus als die polizei kam",
    "expect": "01J8ZK…",
    "note": "BFU vs AFU" }
]
```

Then run it. It needs a live Qdrant and embedding endpoint:

```
ENGRAM_EVAL_DIR=~/engram-eval cargo test --test eval -- --ignored --nocapture
```

Settings come from configuration, so comparing two of anything is a loop
rather than a rebuild:

```
ENGRAM__VECTOR__RECENCY_WEIGHT=0.0 ENGRAM_EVAL_CAP=none \
  ENGRAM_EVAL_DIR=~/engram-eval cargo test --test eval -- --ignored --nocapture
```
````

- [ ] **Step 5: Commit**

```bash
git add tests/eval.rs README.md
git commit -m "feat: add the retrieval evaluation harness

Runs hand-written query/chunk pairs through Core::search and reports
recall@10, MRR and the queries that missed. Ignored by default: it needs
a real Qdrant and a real embedding endpoint, and the fake embedder's
vectors would measure nothing.

Without a corpus it explains itself and returns rather than failing, so
the suite still passes for anyone who does not have one."
```

- [ ] **Step 6: Write the pairs, then run it for real**

This step produces no repository changes. Read `$ENGRAM_EVAL_DIR/chunks.json`, draft roughly twenty pairs into `$ENGRAM_EVAL_DIR/pairs.json`, and hand them to the user for correction — they are the only one who can say which passage was the wanted answer.

Aim, per document:

- `dateisysteme-fat.txt` — questions about recovering or interpreting FAT data, phrased without the words *Cluster*, *Verzeichniseintrag* or *Allocation*.
- `mobilfunkforensik.txt` — questions about getting data off a phone, phrased without *Extraktion*, *JTAG* or *Chip-off*.
- `datenhehlerei-202d.txt` — questions about whether something is a crime, phrased without *Vortat*, *Datenhehlerei* or a paragraph number.

Then, with Qdrant and the embedding endpoint up:

```bash
ENGRAM_EVAL_DIR=/home/user01/engram-eval cargo test --test eval -- --ignored --nocapture
```

Record the resulting numbers in the handoff message, not in a committed file — the baseline belongs with the corpus, which is not in the repository.

---

## What happens after this plan

Out of scope here, deliberately: tuning the defaults. Once the harness reports a
baseline, the sweep over `recency_weight`, the per-source cap, reranking, and
the embedding model is a separate piece of work whose deliverable is a change
to `config.example.toml` with the measured numbers in the commit message.

A benchmark and a change to the thing it measures must not arrive together, or
the first numbers have nothing to be compared against.
