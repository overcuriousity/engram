# Self-tuning, stage 2b: the retrieval parameters

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the idle pass move the two retrieval knobs the code can measure without spending inference — the candidate pool depth and the recency half-life — and make the pass stop the moment somebody comes back.

**Architecture:** No new engine and no new table. `RankingParams` gains two fields and is threaded through the one search pipeline as a whole rather than as two loose values; the vector trait takes the recency terms per call; the stored shapes (`GenerationParams`, `RunParams`) widen through JSON with serde defaults, which is exactly why they were JSON. The chooser gains two ladders. The pass gains a stop check between pairs.

**Tech Stack:** Rust 2024 edition, sqlx 0.9 over SQLite, tokio, serde. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-04-self-tuning-design.md` — Stage 2, second plan. Read "The three decisions this rests on" and Part 3 before task 1.

**Depends on:** stage 2 (`docs/superpowers/plans/2026-09-04-self-tuning-stage-2.md`), on `feat/observations`. Branch from there. Baseline: 2515 tests passing, 0 failing.

## What the code admits

The spec names four retrieval knobs. Read against the tree, they are not four of a kind, and this plan covers two. The reasons are written here so nobody reopens them one at a time:

| Knob | Decision | Why |
|---|---|---|
| **Candidate pool depth** (`CANDIDATE_MULTIPLIER`, a constant 3 in `src/core/search.rs`) | **In.** | Varies per call, costs only a wider vector read, and the pass already re-searches every pair under every candidate. |
| **Recency half-life** (`recency_half_life_days`, fixed when the Qdrant store connects) | **In.** | The Qdrant formula is built per query already (`scoring_formula`, `src/vector/qdrant.rs`); making the half-life a per-call argument is a signature change and nothing else. |
| **Rerank on/off** | **Out, and not for the cost of measuring it.** | Measuring "on" is a rerank call per pair per candidate — inference the pass may not spend, but that could be argued around on the background lane. What cannot: the gate scores ranking quality only. Adopting "rerank on" makes every future search pay a model call the gate never weighed. That trade is the operator's, the same way `[infer]` is. |
| **`prime_lift`** | **Out, blocked on a fact.** | Priming reads the sitting at the moment of the search. Observations do not record the sitting, and a column on `observations` is a recreated database. Not a rule; a missing fact. |
| **Spread** (`associate.spread_from/spread_max`) | **Out.** | Appends associations under the list rather than reordering it, skipped on the Judge door, and runs off learned links — stage 3's territory. Scoring it needs a definition of what a hit in the appended band counts as, for a knob whose effect is additive. |

One consequence to state plainly: **the in-memory vector store ignores recency.** `MemoryVectors` takes the default `search_weighted`, which drops the recency terms and delegates to `search`. That is already true of `recency_weight` — the suite's sweep tests only ever find the cap — and this plan does not change it. Tests on the recency axes assert structure (the knob is carried, the ladder is walked, the file is written) and not ranking. The Qdrant integration test is where a half-life is seen to do anything.

## What a quiet base may spend

The spec held this stage back for a cost conversation. Here is the answer, and it is two rules rather than a number:

1. **Every rung on every axis, one knob at a time, and no combinations.** `tune::BUDGET` becomes 16: the running configuration plus every other rung of each of four ladders. Sixteen searches per pair, over at most `OBSERVATION_LIMIT` pairs, of vector reads only. *Not* "the nearest step only", which this plan first said: a tie keeps the current value, so an improvement two rungs out behind a rung that ties would never be reached at all — the stage 2 fixture showed exactly that, with caps 5 and 3 tying an uncapped baseline and cap 2 the improvement.
2. **The pass stops when somebody comes back.** Between pairs it asks whether a search or a question has been recorded since it started; if so it abandons the pass with nothing written — a pass is never partially adopted — and the next quiet period starts it over. Recomputing is the resumption: the pass is bounded by rule 1, so a restart costs what a pass costs, and no partial state has to be kept correct across a sitting.

## Global Constraints

- **Rust 1.94**, `cargo fmt --all --check`, `cargo clippy --all-targets --locked -- -D warnings` clean, all tests runnable with no infrastructure.
- **No schema change.** `generations.params` and `eval_runs.*_params` are JSON; new fields carry `#[serde(default = ...)]` so every row written by stages 1 and 2 still deserializes. No `ALTER`, no new table.
- **No gate is a tuned constant.** The ladders are candidate *values* — like `RECENCY` and `CAPS` today — not thresholds. `recommend`, `holds_up`, `settled` and `trustworthy` are untouched.
- **`config.toml` is never written by the loop.** `write_ranking` stays the human apply path and gains the two new keys; nothing in `jobs::tune` calls it. The existing source-scan test in `tune.rs` keeps that true.
- **Serving stays deterministic.** One generation is live; the pass searches through `Door::Judge` with `mark: false` as before.
- **The pass spends no inference.** Stored query vectors, reranker off, background lane. Unchanged, and the `the_pass_embeds_nothing` test keeps it so.
- **Adding a field to `RankingParams` breaks every literal initializer**, including one in `tests/eval.rs` that `cargo test --lib` does not catch. Task 1 gives the struct a `Default` and every literal outside `ranking.rs` and `config.rs` becomes `RankingParams { ..., ..Default::default() }`. Finish every task with `cargo clippy --all-targets`.
- **Test names are sentences stating the rule.** Match the file you are editing.
- **Two traps stage 2 hit:** sqlx 0.9 refuses `sqlx::query(&format!(..))` — write SQL as literals; and `cargo fmt` reflows test code, so an exact-string edit made after it may silently miss — re-read before editing.
- Commit after every task, lowercase sentence subjects in the repo's style.

---

### Task 1: Two more knobs on the one search pipeline

**Files:**
- Modify: `src/core/ranking.rs` — the struct, `from_vector`, `Default`, the two ladders
- Modify: `src/config.rs` — `VectorConfig` gains `candidate_multiplier`; `normalize` validates it and sizes the `feedback.candidates` ceiling from the top rung
- Modify: `src/vector/mod.rs` — `search_weighted` takes `Recency` instead of `f32`
- Modify: `src/vector/qdrant.rs` — reads the half-life off the argument
- Modify: `src/core/search.rs` — `search_inner` takes `RankingParams`; `CANDIDATE_MULTIPLIER` read from it; explain reads the half-life from it
- Modify: `src/core/mod.rs` — drop `Core::recency_half_life_days` (it now travels in `ranking`); the test core's `ranking` literal
- Modify: `tests/eval.rs` — the harness's `ranking` literal and the dropped field
- Modify: `config.example.toml` — `[vector] candidate_multiplier`

**Interfaces produced:**
- `RankingParams { recency_weight: f32, per_source_cap: Option<usize>, candidate_multiplier: usize, recency_half_life_days: u32 }`, `impl Default for RankingParams`
- `pub const MULTIPLIERS: [usize; 5] = [1, 2, 3, 5, 8];` and `pub const HALF_LIVES: [u32; 5] = [30, 90, 180, 365, 730];` in `src/core/ranking.rs`
- `pub struct Recency { pub weight: f32, pub half_life_days: u32 }` in `src/vector/mod.rs`; `VectorStore::search_weighted(&self, vector, sparse, limit, filter, recency: Recency)`
- `VectorConfig::candidate_multiplier: usize` (default 3)

The ladders live beside the struct rather than in `sweep.rs`, because `normalize` needs the top rung of `MULTIPLIERS` to size a ceiling, and a config module reaching into the eval module for it would be the dependency pointing the wrong way. `RECENCY` and `CAPS` stay where they are; nothing else reads them.

- [ ] **Step 1: Write the failing tests**

In `src/core/ranking.rs`'s test module (replace the existing `vector_config` helper with this one — it gains the new field):

```rust
fn vector_config(per_source_cap: usize) -> VectorConfig {
    VectorConfig {
        url: String::new(),
        collection: String::new(),
        api_key: None,
        recency_weight: 0.05,
        recency_half_life_days: 180,
        pinned_boost: 0.15,
        weak_below: 0.35,
        per_source_cap,
        candidate_multiplier: 3,
    }
}

#[test]
fn the_retrieval_knobs_are_read_from_the_file_beside_the_ranking_ones() {
    let p = RankingParams::from_vector(&VectorConfig {
        candidate_multiplier: 5,
        recency_half_life_days: 90,
        ..vector_config(3)
    });
    assert_eq!(p.candidate_multiplier, 5);
    assert_eq!(p.recency_half_life_days, 90);
}

#[test]
fn the_shipped_values_sit_in_the_middle_of_their_ladders() {
    // A ladder walked from its end can only go one way; the pass would then
    // be told the shipped value is an extreme, which nobody decided.
    let d = RankingParams::default();
    assert_eq!(MULTIPLIERS[MULTIPLIERS.len() / 2], d.candidate_multiplier);
    assert_eq!(HALF_LIVES[HALF_LIVES.len() / 2], d.recency_half_life_days);
    assert!(MULTIPLIERS.windows(2).all(|w| w[0] < w[1]), "ascending");
    assert!(HALF_LIVES.windows(2).all(|w| w[0] < w[1]), "ascending");
}
```

In `src/core/search.rs`'s test module, beside `the_cap_over_fetches_...` (whatever the existing test around line 2252 that reads `candidates_fetched` is called — put it next to that):

```rust
#[tokio::test]
async fn the_pool_is_as_deep_as_the_multiplier_says() {
    let core = crate::core::test_support::test_core().await;
    seed(&core, &["mount the image", "loop device", "losetup"]).await;
    let mut query = q("mount");
    query.limit = 4;
    let base = *core.ranking.read().unwrap();
    for multiplier in [1usize, 2, 5] {
        let params = crate::core::ranking::RankingParams {
            candidate_multiplier: multiplier,
            per_source_cap: Some(2),
            ..base
        };
        let (_, outcome) = core
            .search_with_ranking(&query, params, Door::Judge)
            .await
            .unwrap();
        assert_eq!(
            outcome.explanation.candidates_fetched,
            4 * multiplier,
            "multiplier {multiplier}"
        );
    }
}

#[tokio::test]
async fn the_half_life_reaches_the_store_with_the_weight() {
    // `MemoryVectors` drops both on the floor, so this asks a store that
    // records what it was handed.
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut core = crate::core::test_support::test_core().await;
    core.vectors = std::sync::Arc::new(RecordsRecency {
        inner: core.vectors.clone(),
        seen: recorded.clone(),
    });
    let params = crate::core::ranking::RankingParams {
        recency_weight: 0.1,
        recency_half_life_days: 30,
        ..*core.ranking.read().unwrap()
    };
    core.search_with_ranking(&q("mount"), params, Door::Judge)
        .await
        .unwrap();
    assert_eq!(
        recorded.lock().unwrap().as_slice(),
        &[crate::vector::Recency {
            weight: 0.1,
            half_life_days: 30
        }]
    );
}

/// A store that writes down the recency terms each search was handed and
/// otherwise behaves as the one it wraps.
struct RecordsRecency {
    inner: std::sync::Arc<dyn crate::vector::VectorStore>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<crate::vector::Recency>>>,
}
```

Implement `VectorStore` for `RecordsRecency` by delegating every method to `inner`, and in `search_weighted` push `recency` onto `seen` before delegating. It is boilerplate, and it is the only way to see what the pipeline handed the store without a Qdrant. `crate::vector::Recency` needs `Debug, Clone, Copy, PartialEq` for the assertion.

If `q(...)` and `seed(...)` are not the names of the helpers in that test module, use the ones that are — read the module's first hundred lines.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib core::ranking core::search::tests::the_pool`
Expected: FAIL — `no field candidate_multiplier`.

- [ ] **Step 3: Widen `RankingParams` and `VectorConfig`**

`src/core/ranking.rs`:

```rust
//! The knobs a sweep may move while the server runs.
//!
//! Everything else that shapes ranking is read once at startup and threaded
//! down. These are different: the tuning sweep and the idle pass rank the same
//! pairs under several of them in one pass, and adopting a candidate has to
//! change the search the *next* request runs. So they live behind
//! `Core::ranking` rather than being copied into the places that use them.
//!
//! Two reorder what retrieval returned (`recency_weight`, `per_source_cap`);
//! two change what is retrieved at all (`candidate_multiplier`,
//! `recency_half_life_days`). Both kinds cost the idle pass the same thing —
//! one vector read per pair per candidate — which is what lets them share a
//! struct and a chooser.

use crate::config::VectorConfig;

/// The rungs the idle pass may step the pool depth along. Values, not a
/// threshold: the pass never prefers one over another except by measuring.
pub const MULTIPLIERS: [usize; 5] = [1, 2, 3, 5, 8];
/// The rungs for the recency half-life, in days.
pub const HALF_LIVES: [u32; 5] = [30, 90, 180, 365, 730];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankingParams {
    pub recency_weight: f32,
    /// Chunks one document may contribute. `None` lets a single document fill
    /// the whole list — which is what `ask` wants, and what the sweep offers as
    /// one of its candidates.
    pub per_source_cap: Option<usize>,
    /// How many times the answer size retrieval fetches when something
    /// downstream — the cap or the reranker — will narrow the list. Wider
    /// costs a bigger vector read and gives the cap more to choose from.
    pub candidate_multiplier: usize,
    /// How many days it takes a result's recency term to halve.
    pub recency_half_life_days: u32,
}

impl RankingParams {
    pub fn from_vector(cfg: &VectorConfig) -> Self {
        Self {
            recency_weight: cfg.recency_weight,
            // `0` is how a file says "no cap": a setting cannot hold `None`,
            // and a cap of zero would otherwise mean a search that returns
            // nothing at all.
            per_source_cap: match cfg.per_source_cap {
                0 => None,
                n => Some(n),
            },
            candidate_multiplier: cfg.candidate_multiplier.max(1),
            recency_half_life_days: cfg.recency_half_life_days.max(1),
        }
    }
}

/// The shipped values — the same ones `VectorConfig`'s serde defaults hold,
/// read from the same functions so the two cannot drift.
impl Default for RankingParams {
    fn default() -> Self {
        Self {
            recency_weight: crate::config::default_recency_weight(),
            per_source_cap: Some(crate::config::default_per_source_cap()),
            candidate_multiplier: crate::config::default_candidate_multiplier(),
            recency_half_life_days: crate::config::default_recency_half_life_days(),
        }
    }
}
```

`src/config.rs`: make `default_recency_weight`, `default_per_source_cap`, `default_recency_half_life_days` `pub(crate)`, add beside them:

```rust
pub(crate) fn default_candidate_multiplier() -> usize {
    crate::core::search::CANDIDATE_MULTIPLIER
}
```

and on `VectorConfig`, after `per_source_cap`:

```rust
    /// How many times the answer size a search fetches when the cap or the
    /// reranker will narrow it. See `RankingParams::candidate_multiplier`.
    #[serde(default = "default_candidate_multiplier")]
    pub candidate_multiplier: usize,
```

In `normalize`, before the `feedback.candidates` ceiling:

```rust
        if self.vector.candidate_multiplier == 0 {
            let d = default_candidate_multiplier();
            self.vector.candidate_multiplier = d;
            tracing::warn!(
                using = d,
                "vector.candidate_multiplier = 0 would fetch nothing to cap; using the default"
            );
        }
        // The widest ordinary search is the top rung of the ladder the pass may
        // climb to, not the shipped multiplier: a pass that adopted the top
        // rung would otherwise fetch wider than the ceiling promised.
        let ceiling = crate::core::search::MAX_LIMIT
            * crate::core::ranking::MULTIPLIERS[crate::core::ranking::MULTIPLIERS.len() - 1];
```

and replace the later `MAX_LIMIT * CANDIDATE_MULTIPLIER` (around line 2985, in a test or a doc) the same way. Every literal `VectorConfig { ... }` in `src/config.rs` (four) and `tests/eval.rs` gains `candidate_multiplier: 3,`.

- [ ] **Step 4: The vector trait takes the recency terms as one value**

`src/vector/mod.rs`, above the trait:

```rust
/// The two terms of the recency stage, chosen per call. Together because they
/// are meaningless apart: a weight with no half-life is a number with no
/// curve under it, and a store that applies one applies both.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Recency {
    pub weight: f32,
    pub half_life_days: u32,
}
```

`search_weighted`'s last parameter becomes `recency: Recency`; the default body becomes `let _ = recency;`. Update the doc comment's "recency weight" to "recency terms".

`src/vector/qdrant.rs`: `search` delegates with `Recency { weight: self.recency_weight, half_life_days: self.recency_half_life_days }`; `search_weighted` uses `recency.weight` where it used `recency_weight` and `recency.half_life_days as u64 * SECONDS_PER_DAY` where it used `self.recency_half_life_days`. The `recency_half_life_days` field on the Qdrant struct stays: it is what the plain `search` — every caller that is not the ranking pipeline — runs with.

- [ ] **Step 5: The pipeline carries the whole struct**

`src/core/search.rs`: change `search_inner`'s signature from `(query, cap: Option<usize>, recency_weight: f32, origin, waited_on, stages)` to `(query, params: RankingParams, origin, waited_on, stages)`. Inside: `let cap = params.per_source_cap;` at the top, so the body below changes as little as possible; `limit * CANDIDATE_MULTIPLIER` becomes `limit * params.candidate_multiplier`; the `search_weighted` call passes `crate::vector::Recency { weight: params.recency_weight, half_life_days: params.recency_half_life_days }`; the explain block's `self.recency_half_life_days as u64 * 86_400` becomes `params.recency_half_life_days as u64 * 86_400`, and its comment gains one sentence: *the half-life comes off the same parameter for the same reason.*

Callers:

```rust
pub async fn search_with(&self, query, cap, origin) -> ... {
    let params = RankingParams {
        per_source_cap: cap,
        ..*self.ranking.read().expect("ranking lock")
    };
    self.search_inner(query, params, origin.into(), true, None).await
}

pub async fn search_with_ranking(&self, query, params, origin) -> ... {
    self.search_inner(query, params, origin.into(), false, None).await
}
```

and `search_events`, which reads `weight` off the lock today, reads the whole struct the same way `search_with` does and passes it. `CANDIDATE_MULTIPLIER` stays a `pub const`: it is the shipped default `default_candidate_multiplier` returns.

Remove `recency_half_life_days` from `Core` (`src/core/mod.rs`: the field, its builder line, the test core's line) and from `tests/eval.rs`. Add `candidate_multiplier: 3, recency_half_life_days: 180,` — or rather `..Default::default()` — to the two `ranking` literals in `src/core/mod.rs` and `tests/eval.rs`:

```rust
ranking: Arc::new(std::sync::RwLock::new(
    crate::core::ranking::RankingParams {
        recency_weight: 0.0,
        per_source_cap: Some(crate::core::search::MAX_PER_CORPUS),
        ..Default::default()
    },
)),
```

Then `cargo clippy --all-targets` and fix every remaining literal the same way. There are ten in `src/eval/sweep.rs`'s tests, one in `src/store/eval_runs.rs`, one in `src/store/generations.rs` — the last two are `From` impls and are task 2's; make them compile here by filling the new fields from `Default::default()` and let task 2 make them right.

- [ ] **Step 6: Document the key**

`config.example.toml`, after `per_source_cap = 3`:

```toml
# How many times the answer size a search fetches when the cap or the reranker
# will narrow the list. Wider gives the cap more to choose from and costs a
# bigger vector read; at 1 a capped search has nothing to refill from.
# This, per_source_cap, recency_weight and recency_half_life_days are the four
# knobs a self-tuning base may move — see [evolve].
candidate_multiplier = 3
```

and reword the `per_source_cap` comment's "This and recency_weight above are the two knobs the tuning sweep measures" to "the four knobs the tuning sweep and the idle pass may move".

- [ ] **Step 7: Run to verify they pass, then the gate**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: clean; the two new ranking tests, the two new search tests, and everything that passed before.

- [ ] **Step 8: Commit**

```bash
git add src/core/ranking.rs src/config.rs src/vector/mod.rs src/vector/qdrant.rs src/core/search.rs src/core/mod.rs tests/eval.rs config.example.toml src/eval/sweep.rs src/store/eval_runs.rs src/store/generations.rs
git commit -m "feat(evolve): the pool depth and the half-life ride the same parameters as the ranking knobs"
```

---

### Task 2: The stored shapes widen without a migration

**Files:**
- Modify: `src/store/generations.rs` — `GenerationParams`, both `From` impls
- Modify: `src/store/eval_runs.rs` — `RunParams`, both `From` impls
- Modify: `src/config.rs` — `write_ranking`, `ranking_keys_in_env`
- Modify: `src/web/insights.rs` — `describe`, `params_str`
- Test: each file's own test module

**Interfaces produced:**
- `GenerationParams` and `RunParams` each gain `#[serde(default = "...")] pub candidate_multiplier: usize` and `#[serde(default = "...")] pub recency_half_life_days: u32`
- `write_ranking` writes `vector.candidate_multiplier` and `vector.recency_half_life_days`
- `ranking_keys_in_env` also matches `ENGRAM__VECTOR__CANDIDATE_MULTIPLIER` and `ENGRAM__VECTOR__RECENCY_HALF_LIFE_DAYS`

This is the payoff of `params` being JSON: every generation and every eval run written so far deserializes with the shipped values filled in, which is what they were running under.

- [ ] **Step 1: Write the failing tests**

`src/store/generations.rs`:

```rust
#[test]
fn a_generation_written_before_the_retrieval_knobs_still_reads() {
    // Stage 1 and 2 rows. A migration here would be a recreated database,
    // which is the price this schema charges and not one a knob may cost.
    let p: GenerationParams =
        from_json(r#"{"recency_weight":0.05,"per_source_cap":3}"#).unwrap();
    assert_eq!(p.candidate_multiplier, 3);
    assert_eq!(p.recency_half_life_days, 180);
}

#[test]
fn the_two_shapes_of_the_parameters_round_trip() {
    let r = crate::core::ranking::RankingParams {
        recency_weight: 0.1,
        per_source_cap: None,
        candidate_multiplier: 5,
        recency_half_life_days: 90,
    };
    let back: crate::core::ranking::RankingParams = GenerationParams::from(r).into();
    assert_eq!(back, r);
}
```

`src/store/eval_runs.rs`:

```rust
#[test]
fn a_run_written_before_the_retrieval_knobs_still_reads() {
    let p: RunParams = parse(r#"{"recency_weight":0.05,"per_source_cap":3}"#.to_string()).unwrap();
    assert_eq!(p.candidate_multiplier, 3);
    assert_eq!(p.recency_half_life_days, 180);
}
```

(`parse` is the existing private helper `hydrate` uses; if its argument type differs, match it.)

`src/config.rs`, beside the existing `write_ranking` test around line 3040:

```rust
#[test]
fn applying_writes_all_four_knobs_and_eats_no_comment() {
    let dir = tempfile::tempdir().unwrap();
    let p = write(
        &dir,
        "[vector]\n# a comment the apply path must not eat\nrecency_weight = 0.05\nper_source_cap = 3\n",
    );
    write_ranking(
        &p,
        &crate::core::ranking::RankingParams {
            recency_weight: 0.1,
            per_source_cap: None,
            candidate_multiplier: 5,
            recency_half_life_days: 90,
        },
    )
    .unwrap();
    let out = std::fs::read_to_string(&p).unwrap();
    assert!(out.contains("candidate_multiplier = 5"), "{out}");
    assert!(out.contains("recency_half_life_days = 90"), "{out}");
    assert!(out.contains("# a comment the apply path must not eat"), "{out}");
    let back = Config::load(Some(&p)).unwrap();
    assert_eq!(back.vector.candidate_multiplier, 5);
    assert_eq!(back.vector.recency_half_life_days, 90);
}

#[test]
fn the_environment_check_knows_all_four_keys() {
    temp_env::with_var("ENGRAM__VECTOR__CANDIDATE_MULTIPLIER", Some("4"), || {
        assert!(ranking_keys_in_env()
            .iter()
            .any(|k| k.eq_ignore_ascii_case("ENGRAM__VECTOR__CANDIDATE_MULTIPLIER")));
    });
}
```

Use whatever the existing `write(&dir, text)` helper in that test module is called; it exists — the tests at lines ~3103 and ~3128 use it.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib store::generations store::eval_runs` and `cargo test --lib config::tests::applying_writes_all_four`
Expected: FAIL — `no field candidate_multiplier`, and the file lacks the keys.

- [ ] **Step 3: Widen the two stored structs**

`src/store/generations.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GenerationParams {
    pub recency_weight: f32,
    pub per_source_cap: Option<usize>,
    /// Absent in rows written before stage 2b, which ran under the shipped
    /// value. The default says so rather than a migration.
    #[serde(default = "crate::config::default_candidate_multiplier")]
    pub candidate_multiplier: usize,
    #[serde(default = "crate::config::default_recency_half_life_days")]
    pub recency_half_life_days: u32,
}
```

Both `From` impls copy all four fields. Same shape for `RunParams` in `src/store/eval_runs.rs`, with the same serde defaults and both `From` impls copying all four. Update `GenerationParams`'s doc comment: "These are the two the runtime sweep already moves" becomes "The four knobs the idle pass may move".

- [ ] **Step 4: The human apply path writes and checks all four**

`src/config.rs` `write_ranking`, after the `per_source_cap` line:

```rust
    doc["vector"]["candidate_multiplier"] = toml_edit::value(p.candidate_multiplier as i64);
    doc["vector"]["recency_half_life_days"] = toml_edit::value(i64::from(p.recency_half_life_days));
```

`ranking_keys_in_env`'s `matches!` gains `| "ENGRAM__VECTOR__CANDIDATE_MULTIPLIER" | "ENGRAM__VECTOR__RECENCY_HALF_LIFE_DAYS"`; its doc comment's "the two swept keys" becomes "the four swept keys".

- [ ] **Step 5: The page names all four**

`src/web/insights.rs`:

```rust
fn params_str(p: &crate::store::generations::GenerationParams) -> String {
    format!(
        "recency {:.2}, cap {}, pool ×{}, half-life {}d",
        p.recency_weight,
        cap_str(p.per_source_cap),
        p.candidate_multiplier,
        p.recency_half_life_days
    )
}
```

and `describe(run)` gains `pool ×{} → ×{}, half-life {}d → {}d` after the cap pair, read off `run.base_params` / `run.best_params` like the others. Update the existing insights test that asserts on the line's wording if one does (`grep -n "recency 0" src/web/insights.rs`).

- [ ] **Step 6: Run to verify they pass, then the gate**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add src/store/generations.rs src/store/eval_runs.rs src/config.rs src/web/insights.rs
git commit -m "feat(evolve): a generation carries the retrieval knobs, and rows written before it still read"
```

---

### Task 3: The chooser walks four ladders

**Files:**
- Modify: `src/eval/sweep.rs` — `candidates`, `moved`, `grid`
- Modify: `src/jobs/tune.rs` — `BUDGET`

**Interfaces produced:** none new. `candidates(current, tried, budget)` keeps its signature and offers steps on four axes. `BUDGET` becomes `pub(crate)` and 16 — see "What a quiet base may spend" for why not 9.

`grid` — the verdict-paid sweep's 5 × 4 — stays two-axis. The human sweep is paced by verdicts and runs the reranker; widening it to a hundred candidates is not this plan's business. It fills the other two knobs from `current` so its candidates move nothing it does not measure.

- [ ] **Step 1: Write the failing tests**

In `src/eval/sweep.rs`'s test module, beside `every_candidate_moves_at_most_one_knob`:

```rust
#[test]
fn a_neighbour_on_every_axis_is_reachable_inside_the_pass_budget() {
    // Nine is the running configuration plus one step each way on four axes.
    // A budget that could not reach one of them would make that knob one the
    // pass never proposes, silently.
    let current = RankingParams::default();
    let out = candidates(current, &[], crate::jobs::tune::BUDGET);
    assert!(out.iter().any(|c| c.candidate_multiplier != current.candidate_multiplier), "{out:?}");
    assert!(out.iter().any(|c| c.recency_half_life_days != current.recency_half_life_days), "{out:?}");
    assert!(out.iter().any(|c| c.per_source_cap != current.per_source_cap), "{out:?}");
    assert!(out.iter().any(|c| c.recency_weight != current.recency_weight), "{out:?}");
    assert_eq!(out.len(), crate::jobs::tune::BUDGET);
}

#[test]
fn every_candidate_on_four_axes_still_moves_one_knob() {
    let current = RankingParams::default();
    let all = candidates(current, &[], 64);
    for c in &all {
        assert!(moved(*c, current) <= 1, "{c:?}");
    }
    assert_eq!(
        all.len(),
        1 + (RECENCY.len() - 1)
            + (CAPS.len() - 1)
            + (crate::core::ranking::MULTIPLIERS.len() - 1)
            + (crate::core::ranking::HALF_LIVES.len() - 1),
        "every rung on every ladder, once"
    );
}

#[test]
fn a_reverted_pool_depth_is_not_offered_again() {
    use crate::store::generations::GenerationParams;
    let current = RankingParams::default();
    let tried = vec![GenerationParams::from(RankingParams {
        candidate_multiplier: 5,
        ..current
    })];
    let out = candidates(current, &tried, 64);
    assert!(!out.iter().any(|c| c.candidate_multiplier == 5), "{out:?}");
    assert!(out.iter().any(|c| c.candidate_multiplier == 8), "the rung past it is still there");
}

#[test]
fn the_verdict_paid_grid_moves_only_the_two_knobs_it_measures() {
    let current = RankingParams {
        candidate_multiplier: 5,
        recency_half_life_days: 90,
        ..RankingParams::default()
    };
    for c in grid(current) {
        assert_eq!(c.candidate_multiplier, 5, "{c:?}");
        assert_eq!(c.recency_half_life_days, 90, "{c:?}");
    }
}
```

Update the existing `every_candidate_moves_at_most_one_knob` test's expected count (`1 + (RECENCY.len() - 1) + (CAPS.len() - 1)`) — it is superseded by the four-axis count above; delete it or fold it in. Update `the_grid_always_contains_the_configuration_it_is_measured_against`'s literals to `..Default::default()` if task 1 did not already.

`BUDGET` must be `pub(crate)` for the first test.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib eval::sweep::tests`
Expected: FAIL — the candidates never move the new knobs; the grid moves them.

- [ ] **Step 3: Implement**

`grid` builds each candidate as `RankingParams { recency_weight, per_source_cap, ..current }`.

`moved`:

```rust
fn moved(cand: RankingParams, current: RankingParams) -> usize {
    usize::from(cand.recency_weight != current.recency_weight)
        + usize::from(cand.per_source_cap != current.per_source_cap)
        + usize::from(cand.candidate_multiplier != current.candidate_multiplier)
        + usize::from(cand.recency_half_life_days != current.recency_half_life_days)
}
```

`candidates`: two more `outward` calls —

```rust
    let multipliers = outward(
        &crate::core::ranking::MULTIPLIERS,
        |v| *v < current.candidate_multiplier,
        |v| *v == current.candidate_multiplier,
    );
    let half_lives = outward(
        &crate::core::ranking::HALF_LIVES,
        |v| *v < current.recency_half_life_days,
        |v| *v == current.recency_half_life_days,
    );
```

— and the loop runs to the longest of the four, pushing `caps.get(i)`, `recency.get(i)`, `multipliers.get(i)`, `half_lives.get(i)` in that order, each as `RankingParams { <that field>: *v, ..current }`. Update the doc comment: "its nearest neighbour on each axis" already says it; add that the axes are the four knobs of `RankingParams` and that a reorder knob and a retrieval knob cost the pass the same.

`src/jobs/tune.rs`:

```rust
/// How many candidates one pass ranks the pairs under: the running
/// configuration and its nearest neighbour each way on each of four axes. A
/// bound on work rather than a setting. Far rungs are reached in later passes
/// as the base walks, each step getting its own watch — which is the whole
/// heuristic, since a knob that helps usually helps a little.
pub(crate) const BUDGET: usize = 9;
```

- [ ] **Step 4: Run to verify they pass, then the gate**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: clean. The stage 2 `jobs::tune` tests still adopt a cap: with the memory store ignoring recency and no reranker, the pool depth and half-life candidates tie the baseline on every pair and the cap remains the only improvement — which is `recommend` doing what it says.

- [ ] **Step 5: Commit**

```bash
git add src/eval/sweep.rs src/jobs/tune.rs
git commit -m "feat(evolve): the chooser walks the retrieval knobs a step at a time, like the others"
```

---

### Task 4: The pass stops when somebody comes back

**Files:**
- Modify: `src/store/feedback.rs` — `Store::activity_since`
- Modify: `src/eval/sweep.rs` — `score` and `ranks_over_grid` take a stop condition
- Modify: `src/jobs/tune.rs` — `quiet` uses the new store method; `propose` passes its start time and reports an abandoned pass
- Test: `src/store/feedback.rs`, `src/jobs/tune.rs`

**Interfaces produced:**
- `Store::activity_since(&self, since: i64) -> Result<bool>` — whether any search or question has been recorded after `since`
- `sweep::score(core, pairs, grid, current, rerank, stop_after: Option<i64>) -> Result<Option<Scored>>` — `None` when activity arrived after `stop_after`

The check runs between pairs, not between candidates: a pair is nine vector reads, and a person who came back is behind at most that. It is a store query per pair — one indexed count — which is nothing beside the reads it sits between.

- [ ] **Step 1: Write the failing tests**

`src/store/feedback.rs` test module:

```rust
#[tokio::test]
async fn activity_is_a_search_or_a_question_after_the_moment_asked_about() {
    let store = Store::memory().await.unwrap();
    let before = crate::store::now() - 10;
    assert!(!store.activity_since(before).await.unwrap(), "an empty base is quiet");
    store
        .record_search(
            NewEvent {
                fold_onto: None,
                query: "loop device".into(),
                door: Door::Ui,
                scope: None,
                filters: "{}".into(),
                query_vec: vec![0.1, 0.2],
                embed_model: "fake".into(),
                candidates: vec![],
                answered: false,
            },
            0,
        )
        .await
        .unwrap();
    assert!(store.activity_since(before).await.unwrap());
    assert!(
        !store.activity_since(crate::store::now() + 10).await.unwrap(),
        "nothing has happened in the future"
    );
}
```

`src/jobs/tune.rs` test module:

```rust
#[tokio::test]
async fn a_pass_stops_when_somebody_comes_back_and_adopts_nothing() {
    // The check reads the same predicate whether the search landed before
    // the first pair or between two of them, so a search stamped a moment
    // *after* the pass starts stands in for one that lands mid-pass — the
    // pass cannot see the difference, and neither can this test without a
    // vector store that writes to the log on its own first read.
    let (mut core, before) = seeded_with_observations().await;
    core.evolve.autonomous = true;
    let id = core
        .store
        .record_search(
            crate::store::feedback::NewEvent {
                fold_onto: None,
                query: "back at the keyboard".into(),
                door: crate::store::feedback::Door::Ui,
                scope: None,
                filters: "{}".into(),
                query_vec: vec![0.1, 0.2],
                embed_model: "fake".into(),
                candidates: vec![],
                answered: false,
            },
            0,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE search_events SET created_at = ? WHERE id = ?")
        .bind(crate::store::now() + 5)
        .bind(&id)
        .execute(&core.store.pool)
        .await
        .unwrap();

    assert!(run(&core).await.unwrap().is_none());
    assert_eq!(core.store.live_generation().await.unwrap().unwrap().id, before);
    assert!(
        core.store.latest_eval_run().await.unwrap().is_none(),
        "an abandoned pass writes nothing: it is never partially adopted"
    );
}

#[tokio::test]
async fn the_next_quiet_period_starts_the_pass_over() {
    // Resumption is recomputation. The pass is bounded, so a restart costs a
    // pass, and no partial state has to be kept correct across a sitting.
    let (mut core, _) = seeded_with_observations().await;
    core.evolve.autonomous = true;
    let id = core
        .store
        .record_search(
            crate::store::feedback::NewEvent {
                fold_onto: None,
                query: "back at the keyboard".into(),
                door: crate::store::feedback::Door::Ui,
                scope: None,
                filters: "{}".into(),
                query_vec: vec![0.1, 0.2],
                embed_model: "fake".into(),
                candidates: vec![],
                answered: false,
            },
            0,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE search_events SET created_at = ? WHERE id = ?")
        .bind(crate::store::now() + 5)
        .bind(&id)
        .execute(&core.store.pool)
        .await
        .unwrap();
    assert!(run(&core).await.unwrap().is_none(), "interrupted");

    // The sitting ends: the search is now in the past.
    sqlx::query("UPDATE search_events SET created_at = ? WHERE id = ?")
        .bind(crate::store::now() - 5_000)
        .bind(&id)
        .execute(&core.store.pool)
        .await
        .unwrap();
    assert!(run(&core).await.unwrap().is_some(), "and the pass finds what it would have");
}
```

`run` bypasses the quiet check on purpose (that is `run_if_quiet`'s job), so the interruption here is the in-pass check and nothing else.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib store::feedback::tests::activity jobs::tune`
Expected: FAIL — `no method activity_since`; the pass adopts despite the search.

- [ ] **Step 3: Implement**

`src/store/feedback.rs`, beside `judged_since`:

```rust
    /// Whether anybody has searched or asked since `since`. What tells an idle
    /// pass that the quiet it started in has ended.
    pub async fn activity_since(&self, since: i64) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT (SELECT COUNT(*) FROM search_events WHERE created_at > ?)
                  + (SELECT COUNT(*) FROM ask_events WHERE created_at > ?)",
        )
        .bind(since)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }
```

`src/jobs/tune.rs` `quiet` becomes one line over it:

```rust
async fn quiet(core: &Core) -> Result<bool> {
    Ok(!core
        .store
        .activity_since(crate::store::now() - core.evolve.idle_secs.max(0))
        .await?)
}
```

`src/eval/sweep.rs`: `ranks_over_grid` gains `stop_after: Option<i64>` and returns `Result<Option<Vec<Vec<Option<usize>>>>>`:

```rust
    for pair in pairs {
        // Between pairs, not between candidates: a pair is a handful of vector
        // reads, and whoever came back is behind at most that.
        if let Some(since) = stop_after
            && core.store.activity_since(since).await?
        {
            return Ok(None);
        }
        for (row, params) in ranks.iter_mut().zip(grid) {
            row.push(rank_of(core, pair, *params, rerank).await?);
        }
    }
    Ok(Some(ranks))
```

`score` gains the same parameter, returns `Result<Option<Scored>>`, and returns `Ok(None)` when the ranks are `None`. Its doc comment gains: *`stop_after` is the moment the pass began; a search or a question recorded after it ends the pass with nothing scored. `None` never stops.* `run_sweep` passes `None` and handles the `Option` with `let Some(scored) = ... else { return Ok(()) };` — the arm is unreachable there and says so in a one-line comment.

`src/jobs/tune.rs` `propose`:

```rust
    let started = crate::store::now();
    ...
    let Some(scored) = sweep::score(core, &pairs, grid, current, false, Some(started)).await? else {
        tracing::info!("somebody came back; the idle pass stopped and will start over next quiet period");
        return Ok(Pass::default());
    };
```

Update the module doc's last paragraph: *It stops the moment somebody comes back — between pairs, with nothing written — and the next quiet period starts it over. Recomputing is the resumption: the pass is bounded, so a restart costs what a pass costs.*

- [ ] **Step 4: Run to verify they pass, then the gate**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/store/feedback.rs src/eval/sweep.rs src/jobs/tune.rs
git commit -m "feat(evolve): the idle pass stops the moment somebody comes back"
```

---

### Task 5: The words catch up

**Files:**
- Modify: `config.example.toml` — the `[evolve] autonomous` comment
- Modify: `docs/evaluation.md` — one paragraph
- Modify: `src/eval/sweep.rs` — the module doc's second paragraph

**Interfaces produced:** none.

Not a separate deliverable in the reviewer's sense — but the previous four tasks each left a sentence stale somewhere else, and a plan that folds them into whichever task noticed last leaves the executor of task 2 rewriting prose about task 4. Done once, at the end, against the finished code.

- [ ] **Step 1: `config.example.toml`**

In the `[evolve] autonomous` comment, "move its own ranking parameters — the recency weight and the per-source cap —" becomes "move its own ranking parameters — the recency weight, the per-source cap, the candidate pool depth and the recency half-life —". Add after the "One knob per adoption" paragraph:

```toml
# What a quiet base spends on this: nine vector searches per replayed
# observation — the running settings and one step each way on each knob — and
# it stops between observations the moment a search or a question arrives,
# writing nothing. The next quiet period starts it over. No model is called.
```

- [ ] **Step 2: `docs/evaluation.md`**

Find the paragraph stage 1 added to §2 about where pairs come from when `evolve.feed_sweep` is on. After it, one paragraph:

> With `evolve.autonomous` on, the same positive observations feed an idle pass that adopts settings rather than recommending them. It walks four knobs — recency weight, per-source cap, candidate pool depth, recency half-life — one step at a time, replays only stored query vectors through the live index with the reranker off, and stops when anybody comes back. Nothing here touches the cargo harness: `tests/eval.rs` still freezes its corpus and ranks under the parameters it constructs itself, so its numbers stay comparable across every generation a base moves through. The harness is how a move the base made is checked against something that did not move.

- [ ] **Step 3: `src/eval/sweep.rs` module doc**

The second paragraph says the sweep "re-ranks the same vectors". After task 1 the candidates also change what is fetched. Reword: *Baseline and candidates run in one pass over one index, so nothing needs freezing and nothing needs re-embedding — the query cache means each distinct query is embedded once, and every candidate is one vector read over it, whether it reorders what came back or changes how much comes back.*

- [ ] **Step 4: Gate and commit**

Run: `cargo fmt --all --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: clean.

```bash
git add config.example.toml docs/evaluation.md src/eval/sweep.rs
git commit -m "docs(evolve): four knobs, what a quiet base spends, and what the harness still measures"
```

---

## What this stage does not do

- **Rerank on/off, `prime_lift`, spread.** See "What the code admits". Rerank is a judgement recorded there; the other two wait on a fact (the sitting per observation) and a definition (a hit in the appended band).
- **Corpus growth as re-eligibility for a reverted candidate.** `tried_candidates` keys on the models. The spec also names "the corpus has grown substantially"; that needs a size stamp on the reverted generation, which is a JSON field on `params` — cheap, but not this plan's, because nothing here defines "substantially" without a tuned number.
- **Recency on the in-memory store.** It ignores both terms, so the suite cannot see a half-life do anything. `tests/integration_qdrant.rs` is where that lives, and it is `#[ignore]`d without a Qdrant, as it always was.
- **The corpus jobs.** Stage 3.
