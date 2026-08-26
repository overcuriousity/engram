# Ranking Explanation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Carry one explanation object on every search hit, saying what each of the eight ranking stages did to it, and read that same object at all three doors — the rail, MCP's meta line, and the API.

**Architecture:** A `HitExplanation` per result and a `SearchExplanation` per search, both filled in as `search_inner` walks its stages. The three stages that run inside Qdrant are reconstructed locally from payload fields that are already fetched, so nothing costs a second query. Computation always runs; a new `explain` flag decides only whether anything is rendered. No storage, no migration, no change to the order of results.

**Tech Stack:** Rust, axum, askama templates, sqlx, Qdrant REST, tokio test harness.

**Spec:** `docs/superpowers/specs/2026-08-26-ranking-explanation-design.md`

## Global Constraints

- The order of results must be byte-identical with `explain` on and off, at every door. Task 8 pins this and it must not regress in any later task.
- No new store table, no migration, no model call, no second vector search.
- The reconstruction of Qdrant's scoring must be tested against real Qdrant (`tests/integration_qdrant.rs`), never only against our own formula.
- `recency_weight`, `recency_half_life_days`, `pinned_boost` and `per_source_cap` are read from the same `RankingParams` under `self.ranking` that built the Qdrant formula. Never from a second source.
- An associated hit (appended by stage 8) carries `recalled_via` and nothing else: it was never ranked, so it has no ranking story.
- Run `cargo fmt` and `cargo clippy --all-targets -- -D warnings` before every commit. CI is the only clippy gate, so it has to be run by hand here.
- Commit messages: no scope, imperative mood, `feat:` / `test:` / `refactor:` prefixes as used in the existing history.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/core/explain.rs` (new) | The explanation types and the local reconstruction of Qdrant's recency and pinned terms. Pure, no I/O, directly testable. |
| `src/core/search.rs` | Fills the objects in as the pipeline runs; `cap_per_corpus` grows a report; `SearchQuery` grows `explain`. |
| `src/core/mod.rs` | Declares the new module. |
| `src/mcp/mod.rs` | `explain` parameter; renders the block into the meta line. |
| `src/web/api.rs` | `?explain=1`; the envelope response. |
| `src/web/ui.rs` | Carries the explanation into `RenderedResult`. |
| `src/web/templates/_results.html` | Renders the compact form on the existing `rail-why` line. |
| `tests/integration_qdrant.rs` | The reconstruction contract against real Qdrant. |

`src/core/search.rs` is already 3,300 lines. The new types and the reconstruction go in their own module rather than growing it further; only the wiring lands in `search.rs`.

---

### Task 1: The explanation types

**Files:**
- Create: `src/core/explain.rs`
- Modify: `src/core/mod.rs`
- Test: `src/core/explain.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `StageEffect`, `CapEffect`, `HitExplanation`, `SearchExplanation`, all `pub` from `crate::core::explain`.

- [ ] **Step 1: Write the failing test**

Append to `src/core/explain.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stage_that_did_not_apply_serialises_to_nothing() {
        let e = HitExplanation {
            retrieved_rank: 0,
            ..Default::default()
        };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(
            json, r#"{"retrieved_rank":0,"cap":"not_applied"}"#,
            "an absent stage must be absent, not null: a door that renders \
             every key would claim a stage ran"
        );
    }

    #[test]
    fn a_recalled_hit_carries_only_that() {
        let e = HitExplanation::recalled("a1");
        assert_eq!(e.recalled_via.as_deref(), Some("a1"));
        assert!(e.rerank.is_none() && e.prime.is_none());
        assert!(
            matches!(e.cap, CapEffect::NotApplied),
            "an associated hit never competed, so no stage may claim it acted"
        );
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib core::explain`
Expected: FAIL — `src/core/explain.rs` does not exist, or `cannot find type HitExplanation`.

- [ ] **Step 3: Write the implementation**

Create `src/core/explain.rs`:

```rust
//! Why a hit is where it is.
//!
//! A rank is the product of eight stages (see the design record,
//! `docs/superpowers/specs/2026-08-26-ranking-explanation-design.md`, §3).
//! Each used to say what it did in its own way or not at all. This is the one
//! object all three doors read, so that the rail, MCP's meta line and the API
//! cannot disagree about what happened to a result.
//!
//! Nothing here is stored and nothing here reorders anything.

/// What one stage did to one hit. `None` everywhere the stage did not apply.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct StageEffect {
    /// Rank before the stage, where the stage reorders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<usize>,
    /// Rank after it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<usize>,
    /// Score contribution, where the stage is additive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<f32>,
}

/// What the per-source diversity rule did to this hit.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapEffect {
    /// No cap configured, or this hit never went through one.
    #[default]
    NotApplied,
    /// Took a place within its corpus's allowance.
    Kept,
    /// Over its cap in one of its corpora, and present only because the
    /// refill had nothing else to offer. The case the cap silently fails in:
    /// a pool filled by one corpus leaves nothing to redistribute, so the
    /// displaced hits come straight back and the list is dominated despite a
    /// configured `per_source_cap`.
    Refilled,
}

/// Why one hit is where it is.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct HitExplanation {
    /// Rank as retrieval returned it — fusion *and* the scoring stage, since
    /// Qdrant applies both before anything comes back. Not the RRF rank on its
    /// own: that would need a second query, which §10 of the spec forbids.
    pub retrieved_rank: usize,
    /// The recency term's contribution to the score, reconstructed locally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recency: Option<f32>,
    /// The pinned term's, likewise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank: Option<StageEffect>,
    pub cap: CapEffect,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prime: Option<StageEffect>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub past_cliff: bool,
    /// Set for a hit stage 8 appended. Every other field is then absent: it
    /// never competed for a place, so there is no ranking story to tell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recalled_via: Option<String>,
}

impl HitExplanation {
    /// The explanation of a hit association appended, which is that it was
    /// recalled and nothing more.
    pub fn recalled(via: &str) -> Self {
        Self {
            recalled_via: Some(via.to_string()),
            ..Default::default()
        }
    }
}

/// What cannot belong to a hit: the shape of the pool it was drawn from.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct SearchExplanation {
    /// How wide the fetch was — `limit * CANDIDATE_MULTIPLIER`, or wider when
    /// capture asked for a bigger pool.
    pub candidates_fetched: usize,
    /// Distinct corpora in the pool before the cap ran. One here, with a
    /// `per_source_cap` configured, is the saturation the spec is about.
    pub corpora_in_pool: usize,
    /// The cap in force, `None` when uncapped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capped: Option<usize>,
    pub displaced: usize,
    /// How many of the displaced came straight back. Equal to `displaced`
    /// means the cap redistributed nothing at all.
    pub refilled: usize,
    pub reranked: bool,
}
```

Add to `src/core/mod.rs`, in the module list beside the existing declarations:

```rust
pub mod explain;
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib core::explain`
Expected: PASS, 2 tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/core/explain.rs src/core/mod.rs
git commit -m "feat: the types a ranking explanation is made of"
```

---

### Task 2: `SearchOutcome`, so a search can say something about itself

**Files:**
- Modify: `src/core/search.rs:71-79` (`SearchTiming`), `:872-881` (`search_with`), `:894-908` (`search_with_ranking`), `:912-919` (`search_inner`), `:1240-1247` (the return)
- Modify: `src/core/ask/mod.rs:525`, `src/eval/sweep.rs:125`, `src/eval/sweep.rs:384`, `src/web/ui.rs:1189`
- Test: `src/core/search.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `SearchExplanation` from Task 1.
- Produces: `pub struct SearchOutcome { pub timing: SearchTiming, pub explanation: SearchExplanation }`; `search_with` and `search_with_ranking` now return `Result<(Vec<SearchResult>, SearchOutcome)>`.

`SearchTiming` keeps every field it has, so nothing that reads timing changes shape. `reranked` is *mirrored* into `SearchExplanation` rather than moved, because `ResultsTemplate::reranked` already reads it off the timing and a move would churn the UI for nothing.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/core/search.rs`:

```rust
#[tokio::test]
async fn a_search_reports_the_shape_of_its_own_pool() {
    let core = test_core().await;
    seed(&core, &[("mounting an image", "procedure", &[])]).await;

    let (_, outcome) = core.search_with(&q("mount"), Some(3), Door::Ui).await.unwrap();

    assert_eq!(
        outcome.explanation.capped,
        Some(3),
        "the cap in force belongs in the explanation: a reader cannot tell \
         whether one corpus dominating is the rule failing or no rule at all"
    );
    assert!(outcome.timing.total_ms < u128::MAX, "timing survives the move");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib a_search_reports_the_shape_of_its_own_pool`
Expected: FAIL — `no field 'explanation' on type 'SearchTiming'`.

- [ ] **Step 3: Write the implementation**

In `src/core/search.rs`, after the `SearchTiming` definition:

```rust
/// Everything a search says about itself, beside the results.
///
/// Two things that are not one thing: how long it took, and how it decided.
/// They are returned together because a caller that wants either has already
/// paid for both.
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub timing: SearchTiming,
    pub explanation: crate::core::explain::SearchExplanation,
}
```

Change the three signatures from `Result<(Vec<SearchResult>, SearchTiming)>` to `Result<(Vec<SearchResult>, SearchOutcome)>`.

In `search_inner`, declare the accumulator immediately after `let door = origin.door;`:

```rust
// Filled in as the stages run. Always computed: a conditional path through
// the ranking would be a second pipeline, and the unexercised one is the
// one that ships. `query.explain` decides only what is rendered.
let mut explanation = crate::core::explain::SearchExplanation {
    capped: cap,
    ..Default::default()
};
```

Right after the `candidates` value is settled (`src/core/search.rs:1024`), record the width:

```rust
explanation.candidates_fetched = candidates;
```

At the return, replace the `SearchTiming { .. }` literal with:

```rust
SearchOutcome {
    timing: SearchTiming {
        embed_ms,
        total_ms: started.elapsed().as_millis(),
        reranked,
    },
    explanation: crate::core::explain::SearchExplanation {
        reranked,
        ..explanation
    },
}
```

Update the four call sites to destructure the new shape. Each currently binds the second element; rename the binding and read `.timing` where it read the timing:

- `src/core/ask/mod.rs:525` — bind `(hits, outcome)` and use `outcome.timing` wherever the old binding was read.
- `src/eval/sweep.rs:125` and `:384` — both discard the second element already; change the pattern only if it names a type.
- `src/web/ui.rs:1189` — `let (hits, t) = …` becomes `let (hits, outcome) = …`; every later `t.` becomes `outcome.timing.`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS, including the new test and every existing search test.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/core/search.rs src/core/ask/mod.rs src/eval/sweep.rs src/web/ui.rs
git commit -m "refactor: a search returns an outcome, not only a duration"
```

---

### Task 3: `cap_per_corpus` reports what it did

**Files:**
- Modify: `src/core/search.rs:203-241` (`cap_per_corpus`), `:1042` (its call site)
- Test: `src/core/search.rs` (the existing `mod tests`, beside the cap tests at `:1896`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `fn cap_per_corpus(hits, max, target) -> (Vec<SearchHit>, CapReport)` and `struct CapReport { pub corpora_in_pool: usize, pub displaced: usize, pub refilled: std::collections::HashSet<String> }`, both private to `search.rs`.

`refilled` is a set of artifact ids rather than a count, because Task 5 needs to mark the individual hits. `SearchExplanation::refilled` is its length.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/core/search.rs`, beside the existing cap tests:

```rust
#[test]
fn a_pool_one_corpus_filled_reports_a_cap_that_redistributed_nothing() {
    // The failure the whole explanation exists to make visible: with only
    // one corpus in the pool there is nothing to promote, so everything the
    // cap displaces comes straight back and the list is dominated despite
    // `per_source_cap` being set.
    let only_a = vec![
        hit("a1", "a", 0.9),
        hit("a2", "a", 0.8),
        hit("a3", "a", 0.7),
        hit("a4", "a", 0.6),
    ];
    let (kept, report) = cap_per_corpus(only_a, 2, 4);

    assert_eq!(kept.len(), 4, "the refill still fills the list");
    assert_eq!(report.corpora_in_pool, 1);
    assert_eq!(report.displaced, 2);
    assert_eq!(
        report.refilled.len(),
        2,
        "every displaced hit came back: the cap redistributed nothing"
    );
    assert!(report.refilled.contains("a3") && report.refilled.contains("a4"));
}

#[test]
fn a_cap_that_actually_redistributed_reports_no_refill() {
    let mixed = vec![
        hit("a1", "a", 0.9),
        hit("a2", "a", 0.8),
        hit("b1", "b", 0.7),
        hit("a3", "a", 0.6),
    ];
    let (kept, report) = cap_per_corpus(mixed, 2, 3);

    assert_eq!(kept.len(), 3);
    assert_eq!(report.corpora_in_pool, 2);
    assert_eq!(report.displaced, 1);
    assert!(
        report.refilled.is_empty(),
        "the target was met without the displaced hit, so the rule held"
    );
}
```

The existing cap tests build their hits inside one test function. Lift that `hit` closure to a `fn hit(id: &str, corpus: &str, score: f32) -> SearchHit` in `mod tests` so all four tests share it, and update the existing assertions to destructure the tuple:

```rust
assert_eq!(ids(cap_per_corpus(ranked(), 2, 3).0), vec!["a1", "a2", "b1"]);
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cap_per_corpus`
Expected: FAIL — `expected 2 elements, found 1` / `mismatched types`.

- [ ] **Step 3: Write the implementation**

Replace `cap_per_corpus` in `src/core/search.rs`:

```rust
/// What the cap did, for the explanation. Kept beside the returned list
/// rather than derived from it, because "was displaced and came back" is not
/// recoverable from the order alone.
#[derive(Debug, Default)]
struct CapReport {
    corpora_in_pool: usize,
    displaced: usize,
    /// Artifact ids that were over their cap and returned anyway.
    refilled: std::collections::HashSet<String>,
}

fn cap_per_corpus(
    hits: Vec<crate::vector::SearchHit>,
    max: usize,
    target: usize,
) -> (Vec<crate::vector::SearchHit>, CapReport) {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut all_corpora: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut kept = Vec::with_capacity(hits.len());
    let mut displaced = Vec::new();
    for h in hits {
        // A merge counts against every corpus it drew from; a passage or a
        // captured artifact against its one. The payload carries the
        // projection; a point written before it existed falls back to
        // `corpus_id`.
        let keys: Vec<String> = if h.payload.origin_corpora.is_empty() {
            vec![h.payload.corpus_id.clone()]
        } else {
            h.payload.origin_corpora.clone()
        };
        all_corpora.extend(keys.iter().cloned());
        let over = keys
            .iter()
            .any(|k| seen.get(k).copied().unwrap_or(0) >= max);
        if !over {
            // Only a hit that took a place counts against one. A displaced hit
            // is over its cap in *one* of its corpora, and charging it to the
            // others as well let a five-corpus merge that never made the list
            // evict unrelated hits from the four that had room for it.
            for k in &keys {
                *seen.entry(k.clone()).or_insert(0) += 1;
            }
            kept.push(h);
        } else {
            displaced.push(h);
        }
    }
    let mut report = CapReport {
        corpora_in_pool: all_corpora.len(),
        displaced: displaced.len(),
        refilled: Default::default(),
    };
    if kept.len() < target {
        let room = target - kept.len();
        for h in displaced.into_iter().take(room) {
            report.refilled.insert(h.payload.artifact_id.clone());
            kept.push(h);
        }
    }
    (kept, report)
}
```

At the call site (`src/core/search.rs:1042`), keep the pool statistics even when no cap is configured — a reader needs to know how many corpora the pool held either way:

```rust
let (hits, cap_report) = match cap {
    Some(max) => cap_per_corpus(hits, max, candidates),
    None => {
        let corpora: std::collections::HashSet<&str> = hits
            .iter()
            .flat_map(|h| match h.payload.origin_corpora.is_empty() {
                true => vec![h.payload.corpus_id.as_str()],
                false => h.payload.origin_corpora.iter().map(String::as_str).collect(),
            })
            .collect();
        let report = CapReport {
            corpora_in_pool: corpora.len(),
            ..Default::default()
        };
        (hits, report)
    }
};
explanation.corpora_in_pool = cap_report.corpora_in_pool;
explanation.displaced = cap_report.displaced;
explanation.refilled = cap_report.refilled.len();
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib cap_per_corpus && cargo test --lib`
Expected: PASS, all four cap tests and the rest of the suite.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/core/search.rs
git commit -m "feat: the per-source cap says what it displaced and what came back"
```

---

### Task 4: The per-hit explanation, attached

**Files:**
- Modify: `src/core/search.rs:82-148` (`SearchResult`), `:154-183` (`From<SearchHit>`), `:1060-1070` (the conversion), `:1203` (`mark_past_cliff`), `:1222-1228` (association)
- Test: `src/core/search.rs`

**Interfaces:**
- Consumes: `HitExplanation`, `CapEffect` (Task 1); `CapReport` (Task 3).
- Produces: `SearchResult::explanation: Option<HitExplanation>`.

Optional because `stale_candidates`, `resurface` and the neighbour lists all build a `SearchResult` without any ranking behind it. `None` there is the honest value; a `Default` would claim rank 0.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn every_ranked_hit_explains_its_retrieved_rank_and_the_cap() {
    let core = test_core().await;
    seed(&core, &[("mounting an image", "procedure", &[])]).await;

    let (hits, _) = core.search_with(&q("mount"), Some(3), Door::Ui).await.unwrap();
    let e = hits[0].explanation.as_ref().expect("a ranked hit explains itself");

    assert_eq!(e.retrieved_rank, 0);
    assert!(
        matches!(e.cap, crate::core::explain::CapEffect::Kept),
        "a hit inside its corpus's allowance was kept, not refilled"
    );
    assert!(
        e.recalled_via.is_none(),
        "a ranked hit was not recalled by anything"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib every_ranked_hit_explains`
Expected: FAIL — `no field 'explanation' on type 'SearchResult'`.

- [ ] **Step 3: Write the implementation**

Add to `SearchResult`, after `reason`:

```rust
/// Why this hit is where it is. `None` for a `SearchResult` that never came
/// out of a ranking — a stale candidate, a neighbour, a resurfaced row.
#[serde(skip_serializing_if = "Option::is_none")]
pub explanation: Option<crate::core::explain::HitExplanation>,
```

Add `explanation: None` to the `From<SearchHit>` body, so every other construction site keeps compiling unchanged.

In the conversion at `src/core/search.rs:1060`, attach the retrieved rank and the cap verdict:

```rust
let mut results: Vec<SearchResult> = hits
    .into_iter()
    .enumerate()
    .map(|(rank, h)| {
        // Demonstrated, never assumed: a hit with no similarity to
        // read is one the lexical half matched verbatim.
        let weak = h.similarity.is_some_and(|s| s < self.weak_below);
        let cap = match (cap.is_some(), cap_report.refilled.contains(&h.payload.artifact_id)) {
            (false, _) => crate::core::explain::CapEffect::NotApplied,
            (true, true) => crate::core::explain::CapEffect::Refilled,
            (true, false) => crate::core::explain::CapEffect::Kept,
        };
        SearchResult {
            weak,
            explanation: Some(crate::core::explain::HitExplanation {
                retrieved_rank: rank,
                cap,
                ..Default::default()
            }),
            ..SearchResult::from(h)
        }
    })
    .collect();
```

In `mark_past_cliff` (`src/core/search.rs:313`), mirror the flag it already sets:

```rust
if let Some(e) = r.explanation.as_mut() {
    e.past_cliff = true;
}
```

Where association appends (`src/core/search.rs:1222`), stamp each recalled hit before extending:

```rust
let recalled = self.associated(&results, &filter).await;
if !recalled.is_empty() {
    self.mark_seen(&recalled, &HashMap::new(), false);
    // Never ranked, so there is no ranking story: `recalled` clears every
    // other stage rather than leaving defaults that would read as facts.
    let recalled: Vec<SearchResult> = recalled
        .into_iter()
        .map(|r| SearchResult {
            explanation: r
                .via
                .as_deref()
                .map(crate::core::explain::HitExplanation::recalled),
            ..r
        })
        .collect();
    results.extend(recalled);
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/core/search.rs
git commit -m "feat: every ranked hit carries its retrieved rank and cap verdict"
```

---

### Task 5: The two stages engram reorders — rerank and prime

**Files:**
- Modify: `src/core/search.rs:1072-1104` (rerank), `:1120-1153` (prime)
- Test: `src/core/search.rs`

**Interfaces:**
- Consumes: `StageEffect`, `HitExplanation` (Task 1); `SearchResult::explanation` (Task 4).
- Produces: populated `HitExplanation::rerank` and `HitExplanation::prime`.

Both stages reorder a `Vec<SearchResult>` in place. The pattern is the same for both: snapshot the id-to-index map before, compare after, and write the pair only where it changed. A stage that moved nothing writes nothing, because "the reranker considered this and left it alone" and "the reranker did not run" must not render the same.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn a_rerank_that_moved_a_hit_says_where_it_moved_it_from() {
    let mut core = test_core().await;
    // The fake reranker reverses the order it is given, so every hit in a
    // list of two moves and the assertion cannot pass by accident.
    core.reranker = Some(std::sync::Arc::new(crate::infer::fake::ReversingReranker));
    seed(
        &core,
        &[("mounting an image", "procedure", &[]), ("mounting a share", "procedure", &[])],
    )
    .await;

    let (hits, _) = core.search_with(&q("mount"), None, Door::Ui).await.unwrap();
    let moved = hits
        .iter()
        .filter_map(|h| h.explanation.as_ref()?.rerank.as_ref())
        .count();

    assert!(moved > 0, "a reranker that reordered must say so on the hits it moved");
    let e = hits[0].explanation.as_ref().unwrap().rerank.as_ref().unwrap();
    assert_ne!(e.from, e.to, "only a hit that actually moved carries the stage");
}
```

If `crate::infer::fake` has no reversing reranker, add one beside the existing fakes in `src/infer/fake.rs`, following the shape of the reranker already there at `src/infer/fake.rs:471`:

```rust
/// Returns the input order reversed, so a test can prove a reorder happened
/// without depending on any scoring behaviour.
pub struct ReversingReranker;

#[async_trait::async_trait]
impl crate::infer::Reranker for ReversingReranker {
    async fn rerank(
        &self,
        _query: &str,
        docs: &[String],
        top_n: usize,
    ) -> crate::error::Result<Vec<(usize, f32)>> {
        let mut order: Vec<(usize, f32)> = (0..docs.len())
            .map(|i| (i, (docs.len() - i) as f32))
            .collect();
        order.reverse();
        order.truncate(top_n);
        Ok(order)
    }
}
```

Match the exact trait name and signature at `src/infer/mod.rs:116`.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib a_rerank_that_moved_a_hit`
Expected: FAIL — `assertion failed: moved > 0`.

- [ ] **Step 3: Write the implementation**

Add a helper near `cap_per_corpus` in `src/core/search.rs`:

```rust
/// Record what a reordering stage did, on the hits it actually moved.
///
/// A stage that left a hit alone writes nothing: "considered and kept" and
/// "did not run" must not render the same, and only the second is silence.
fn note_reorder(
    results: &mut [SearchResult],
    before: &HashMap<String, usize>,
    field: impl Fn(&mut crate::core::explain::HitExplanation) -> &mut Option<crate::core::explain::StageEffect>,
) {
    for (to, r) in results.iter_mut().enumerate() {
        let Some(&from) = before.get(&r.artifact_id) else {
            continue;
        };
        if from == to {
            continue;
        }
        if let Some(e) = r.explanation.as_mut() {
            *field(e) = Some(crate::core::explain::StageEffect {
                from: Some(from),
                to: Some(to),
                delta: None,
            });
        }
    }
}

/// Each hit's current position, for `note_reorder` to compare against.
fn positions(results: &[SearchResult]) -> HashMap<String, usize> {
    results
        .iter()
        .enumerate()
        .map(|(i, r)| (r.artifact_id.clone(), i))
        .collect()
}
```

In the rerank block, immediately before `let docs: Vec<String> = …`:

```rust
let before = positions(&results);
```

and immediately after the `results = order…collect();` assignment inside the `Ok(order)` arm:

```rust
note_reorder(&mut results, &before, |e| &mut e.rerank);
```

In the prime block, before `let ids: Vec<String> = …`:

```rust
let before = positions(&results);
```

and after the `results = prime(…);` call, still inside the same `if`:

```rust
note_reorder(&mut results, &before, |e| &mut e.prime);
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/core/search.rs src/infer/fake.rs
git commit -m "feat: the reranker and priming say which hits they moved"
```

---

### Task 6: Reconstructing what Qdrant did

**Files:**
- Modify: `src/core/explain.rs`, `src/core/search.rs:1060` (the conversion from Task 4)
- Test: `src/core/explain.rs`

**Interfaces:**
- Consumes: `VectorPayload` fields `tags` (`src/vector/mod.rs:16`) and `last_verified_at` (`:39`).
- Produces: `pub fn scoring_terms(payload: &VectorPayload, now: i64, recency_weight: f32, half_life_secs: u64, pinned_boost: f32, pinned_tag: &str) -> (Option<f32>, Option<f32>)` returning `(recency, pinned)`.

Qdrant's `exp_decay` with `midpoint: 0.5` and `scale: s` is `0.5^(|x - target| / s)` — a half-life curve whose half-life is `scale`. `scoring_formula` (`src/vector/qdrant.rs:328`) multiplies that by `recency_weight`, and adds `pinned_boost` where the point matches the pinned tag. A point with no `last_verified_at` defaults to `now`, which is what `"defaults"` in the formula says, giving a decay of exactly `1.0`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/core/explain.rs`:

```rust
fn payload(last_verified_at: Option<i64>, tags: &[&str]) -> crate::vector::VectorPayload {
    crate::vector::VectorPayload {
        artifact_id: "a".into(),
        corpus_id: "c".into(),
        text: String::new(),
        title: None,
        category: None,
        tags: tags.iter().map(|t| t.to_string()).collect(),
        created_at: 0,
        last_seen_at: None,
        hit_count: None,
        status: None,
        last_verified_at,
        superseded_by: None,
        origin_corpora: vec![],
        provenance: None,
    }
}

#[test]
fn one_half_life_of_age_halves_the_recency_term() {
    let half_life = 1_000u64;
    let (recency, pinned) = scoring_terms(
        &payload(Some(9_000), &[]),
        10_000,
        0.05,
        half_life,
        0.15,
        "pinned",
    );
    let recency = recency.expect("a weighted recency term is present");
    assert!(
        (recency - 0.025).abs() < 1e-6,
        "one half-life old halves the decay: 0.05 * 0.5 = 0.025, got {recency}"
    );
    assert!(pinned.is_none(), "an untagged point earns no pinned term");
}

#[test]
fn a_point_with_no_verification_stamp_decays_not_at_all() {
    let (recency, _) = scoring_terms(&payload(None, &[]), 10_000, 0.05, 1_000, 0.15, "pinned");
    assert_eq!(
        recency,
        Some(0.05),
        "the formula's own default is `now`, which is a decay of 1.0 — \
         reading the absence as maximum age would rank the opposite way"
    );
}

#[test]
fn a_pinned_point_earns_the_whole_boost() {
    let (_, pinned) = scoring_terms(&payload(None, &["pinned"]), 10_000, 0.0, 1_000, 0.15, "pinned");
    assert_eq!(pinned, Some(0.15));
}

#[test]
fn a_disabled_term_is_absent_rather_than_zero() {
    let (recency, pinned) = scoring_terms(&payload(None, &["pinned"]), 10_000, 0.0, 1_000, 0.0, "pinned");
    assert!(
        recency.is_none() && pinned.is_none(),
        "`scoring_formula` omits a term at weight zero, so the explanation \
         must not claim a stage that never entered the sum"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib core::explain`
Expected: FAIL — `cannot find function scoring_terms`.

- [ ] **Step 3: Write the implementation**

Add to `src/core/explain.rs`:

```rust
/// The two score terms Qdrant applied, reconstructed from the payload.
///
/// Stages 2 and 3 run inside Qdrant as one sum and only the final score comes
/// back (`src/vector/qdrant.rs:328`). Both terms are nonetheless computable
/// here from fields the payload already carries, so a full explanation costs
/// no second query.
///
/// `exp_decay` with `midpoint: 0.5` and `scale: s` is `0.5^(|x - target| / s)`
/// — a half-life curve whose half-life is `scale`. A weight of zero omits its
/// term from the formula entirely, so this returns `None` rather than `0.0`:
/// a rendered zero would claim a stage ran and contributed nothing, which is
/// a different statement from the stage not being configured.
///
/// This re-implements another system's semantics, which is the one real risk
/// in this design. It is pinned against real Qdrant in
/// `tests/integration_qdrant.rs`, not against our own belief about the
/// formula.
pub fn scoring_terms(
    payload: &crate::vector::VectorPayload,
    now: i64,
    recency_weight: f32,
    half_life_secs: u64,
    pinned_boost: f32,
    pinned_tag: &str,
) -> (Option<f32>, Option<f32>) {
    let recency = (recency_weight > 0.0).then(|| {
        // Absent means `now`, exactly as the formula's `"defaults"` says.
        let stamp = payload.last_verified_at.unwrap_or(now);
        let age = (now - stamp).max(0) as f64;
        let decay = 0.5f64.powf(age / half_life_secs.max(1) as f64);
        recency_weight * decay as f32
    });
    let pinned = (pinned_boost > 0.0 && payload.tags.iter().any(|t| t == pinned_tag))
        .then_some(pinned_boost);
    (recency, pinned)
}
```

Make `PINNED_TAG` reachable from `core`: in `src/vector/qdrant.rs`, change its declaration to `pub(crate) const PINNED_TAG`.

In `src/core/search.rs`, before the conversion loop of Task 4, read the params once:

```rust
// Read once, from the same lock that built the Qdrant formula: a runtime
// change to `recency_weight` between the query and the explanation would
// make the two disagree about the same search.
let (half_life_secs, pinned_boost) = {
    let r = self.ranking.read().expect("ranking lock");
    (r.recency_half_life_days as u64 * 86_400, r.pinned_boost)
};
let scored_at = now_secs();
```

and inside the closure, beside the existing fields:

```rust
let (recency, pinned) = crate::core::explain::scoring_terms(
    &h.payload,
    scored_at,
    recency_weight,
    half_life_secs,
    pinned_boost,
    crate::vector::qdrant::PINNED_TAG,
);
```

writing `recency` and `pinned` into the `HitExplanation` literal.

Note `recency_weight` is already a parameter of `search_inner`, and it is the value that was passed to `search_weighted`. Use it, never `self.ranking`'s copy — `search_with_ranking` overrides it, and the sweep would otherwise be explained with the wrong weight.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/core/explain.rs src/core/search.rs src/vector/qdrant.rs
git commit -m "feat: reconstruct the recency and pinned terms without a second query"
```

---

### Task 7: The reconstruction contract, against real Qdrant

**Files:**
- Modify: `tests/integration_qdrant.rs`

**Interfaces:**
- Consumes: `scoring_terms` (Task 6).
- Produces: nothing further.

This is the test the design record calls the contract of §5. A unit test pins our own arithmetic; only this one pins that our arithmetic is Qdrant's.

- [ ] **Step 1: Write the failing test**

Append to `tests/integration_qdrant.rs`:

```rust
/// The explanation reconstructs two terms Qdrant computed and never returned.
/// If Qdrant's `exp_decay` ever stops meaning what `scoring_terms` believes,
/// this fails — and it has to, because an explanation that contradicts the
/// ranking it explains is worse than silence.
#[tokio::test]
#[ignore]
async fn the_reconstructed_recency_term_matches_what_qdrant_scored() {
    let v = fresh("engram_it_explain", 4).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let half_life_secs = 180u64 * 86_400;

    // Two identical vectors, one a full half-life old. Identical directions
    // mean the fused half of the score is equal, so the whole difference
    // between the two scores is the recency term.
    let mut fresh_point = point("fresh", "s1", vec![1.0, 0.0, 0.0, 0.0], &[], "concept");
    fresh_point.payload.last_verified_at = Some(now);
    let mut old_point = point("old", "s1", vec![1.0, 0.0, 0.0, 0.0], &[], "concept");
    old_point.payload.last_verified_at = Some(now - half_life_secs as i64);
    v.upsert(vec![fresh_point, old_point]).await.unwrap();

    let hits = v
        .search_weighted(
            &[1.0, 0.0, 0.0, 0.0],
            &Default::default(),
            10,
            &SearchFilter::default(),
            0.05,
        )
        .await
        .unwrap();

    let score_of = |id: &str| {
        hits.iter()
            .find(|h| h.payload.artifact_id == id)
            .expect("both points come back")
            .score
    };
    let measured = score_of("fresh") - score_of("old");

    let terms = |id: &str| {
        let h = hits.iter().find(|h| h.payload.artifact_id == id).unwrap();
        engram::core::explain::scoring_terms(&h.payload, now, 0.05, half_life_secs, 0.15, "pinned")
            .0
            .unwrap()
    };
    let reconstructed = terms("fresh") - terms("old");

    assert!(
        (measured - reconstructed).abs() < 1e-4,
        "Qdrant scored a difference of {measured}; the reconstruction says \
         {reconstructed}. The explanation may not contradict the ranking."
    );
    v.drop_collection().await.unwrap();
}
```

`point(...)` is the existing helper in that file; if it does not expose `payload` as a mutable field, set `last_verified_at` by constructing the `VectorPayload` directly, following `point`'s own body.

- [ ] **Step 2: Run the test to verify it fails**

Run: `docker compose up -d && cargo test --test integration_qdrant the_reconstructed_recency_term -- --ignored`
Expected: FAIL — `scoring_terms` is not reachable, or the assertion trips.

- [ ] **Step 3: Make it pass**

If `engram::core::explain` is not reachable from an integration test, make the module public in `src/core/mod.rs` (`pub mod explain;` — Task 1 already does this) and confirm `core` itself is `pub` in `src/lib.rs`. No production logic changes in this task: if the assertion trips on arithmetic, the bug is in Task 6's `scoring_terms` and is fixed there.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --test integration_qdrant the_reconstructed_recency_term -- --ignored`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add tests/integration_qdrant.rs src/lib.rs
git commit -m "test: pin the reconstruction against real Qdrant, not against ourselves"
```

---

### Task 8: The `explain` flag, and the promise it must keep

**Files:**
- Modify: `src/core/search.rs` (`SearchQuery`), and every `SearchQuery` literal in the tree
- Test: `src/core/search.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: `SearchQuery::explain: bool`, default `false`.

The flag gates rendering only. Computation always runs, because a conditional path through the ranking stages would be a second pipeline and the unexercised one is the one that ships.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn asking_for_an_explanation_does_not_change_the_order() {
    // The whole of what this branch promises not to break.
    let core = test_core().await;
    seed(
        &core,
        &[
            ("mounting an image", "procedure", &[]),
            ("mounting a share", "procedure", &[]),
            ("unmounting cleanly", "procedure", &[]),
        ],
    )
    .await;

    let plain = core.search(&q("mount"), Door::Ui).await.unwrap();
    let explained = core
        .search(&SearchQuery { explain: true, ..q("mount") }, Door::Ui)
        .await
        .unwrap();

    let ids = |v: &[SearchResult]| -> Vec<String> {
        v.iter().map(|r| r.artifact_id.clone()).collect()
    };
    assert_eq!(
        ids(&plain),
        ids(&explained),
        "an explanation is an observation, never a stage"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib asking_for_an_explanation`
Expected: FAIL — `struct SearchQuery has no field named explain`.

- [ ] **Step 3: Write the implementation**

Add to `SearchQuery`:

```rust
/// Render the ranking explanation. Off by default: the object is computed
/// on every search either way, and this decides only whether a door says
/// any of it out loud.
#[serde(default)]
pub explain: bool,
```

Then `cargo build` and add `explain: false` to every `SearchQuery` literal the compiler names — `src/core/search.rs` tests, `src/eval/sweep.rs:110`, `src/mcp/mod.rs:447`, `src/web/api.rs:725`, `src/web/ui.rs:1192`, `src/core/ask/mod.rs`. Take the compiler's list; do not work from this one.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add -A
git commit -m "feat: an explain flag that gates rendering and nothing else"
```

---

### Task 9: MCP says it

**Files:**
- Modify: `src/mcp/mod.rs:16-102` (`format_search_results`), `:287-298` (`SearchParams`), `:442-465` (the `search` tool)
- Test: `src/mcp/mod.rs`

**Interfaces:**
- Consumes: `HitExplanation`, `SearchExplanation`, `SearchQuery::explain`.
- Produces: `format_search_results(results: &[SearchResult], explanation: Option<&SearchExplanation>) -> String`.

The signature grows a parameter rather than gaining a sibling function: two formatters would be two things to keep in step, and the meta line is the one place this door speaks.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/mcp/mod.rs`:

```rust
fn explained_result() -> SearchResult {
    let mut r = crate::core::search::SearchResult {
        artifact_id: "a1".into(),
        corpus_id: "c1".into(),
        title: Some("Backup".into()),
        text: "body".into(),
        category: None,
        tags: vec![],
        score: 0.812,
        status: None,
        superseded_by: None,
        last_verified_at: None,
        weak: false,
        model_written: false,
        synthesized: false,
        origin_count: 0,
        primed: false,
        in_sitting: false,
        past_cliff: false,
        via: None,
        reason: None,
        explanation: None,
    };
    r.explanation = Some(crate::core::explain::HitExplanation {
        retrieved_rank: 3,
        recency: Some(0.021),
        cap: crate::core::explain::CapEffect::Refilled,
        ..Default::default()
    });
    r
}

#[test]
fn the_meta_line_stays_silent_unless_an_explanation_was_asked_for() {
    let out = format_search_results(&[explained_result()], None);
    assert!(
        !out.contains("why it is here"),
        "an agent that did not ask for the stages must not be handed them"
    );
    assert!(out.contains("corpus: c1"), "the existing meta line is untouched");
}

#[test]
fn an_explained_result_names_the_stage_that_failed_to_redistribute() {
    let summary = crate::core::explain::SearchExplanation {
        candidates_fetched: 30,
        corpora_in_pool: 1,
        capped: Some(3),
        displaced: 4,
        refilled: 4,
        reranked: false,
    };
    let out = format_search_results(&[explained_result()], Some(&summary));

    assert!(out.contains("retrieved at #4"), "ranks are 1-based at every door");
    assert!(
        out.contains("displaced, refilled"),
        "the case the whole object exists for has to be readable, not inferred"
    );
    assert!(
        out.contains("1 corpus") && out.contains("30 candidates"),
        "the pool's shape belongs above the list, not on a hit"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib mcp::tests`
Expected: FAIL — `this function takes 1 argument but 2 arguments were supplied`.

- [ ] **Step 3: Write the implementation**

Change the signature to `pub fn format_search_results(results: &[SearchResult], explanation: Option<&crate::core::explain::SearchExplanation>) -> String`.

Prepend the summary when one is given:

```rust
let mut out = String::new();
if let Some(s) = explanation {
    // The pool's shape cannot belong to a hit: no result can say how many
    // corpora it was drawn from.
    out.push_str(&format!(
        "_Pool: {} candidates, {} corpus{}{}{}._\n\n",
        s.candidates_fetched,
        s.corpora_in_pool,
        if s.corpora_in_pool == 1 { "" } else { "es" },
        match s.capped {
            Some(n) => format!(" · cap {n} per source"),
            None => " · uncapped".to_string(),
        },
        match (s.displaced, s.refilled) {
            (0, _) => String::new(),
            (d, r) if d == r => format!(
                " · {d} displaced and all {r} refilled — the cap redistributed nothing"
            ),
            (d, r) => format!(" · {d} displaced, {r} refilled"),
        },
    ));
}
```

Per hit, after the existing meta line, when `r.explanation` is present and `explanation.is_some()`:

```rust
fn why_line(e: &crate::core::explain::HitExplanation) -> String {
    if let Some(via) = &e.recalled_via {
        return format!("\n_why it is here: recalled beside `{via}`; never ranked._");
    }
    let mut parts = vec![format!("retrieved at #{}", e.retrieved_rank + 1)];
    if let Some(v) = e.recency {
        parts.push(format!("recency +{v:.3}"));
    }
    if let Some(v) = e.pinned {
        parts.push(format!("pinned +{v:.3}"));
    }
    if let Some(s) = &e.rerank {
        parts.push(format!(
            "reranked {} → {}",
            s.from.unwrap_or(0) + 1,
            s.to.unwrap_or(0) + 1
        ));
    }
    match e.cap {
        crate::core::explain::CapEffect::Refilled => {
            parts.push("displaced, refilled".to_string())
        }
        crate::core::explain::CapEffect::Kept
        | crate::core::explain::CapEffect::NotApplied => {}
    }
    if let Some(s) = &e.prime {
        parts.push(format!(
            "primed {} → {}",
            s.from.unwrap_or(0) + 1,
            s.to.unwrap_or(0) + 1
        ));
    }
    if e.past_cliff {
        parts.push("below the cliff".to_string());
    }
    format!("\n_why it is here: {}._", parts.join(" · "))
}
```

Add `explain: Option<bool>` to `SearchParams` with a doc comment saying what it is for, pass `explain: p.explain.unwrap_or(false)` into the `SearchQuery`, and call `core.search_with(&query, cap, Door::Mcp)` so the tool has the `SearchOutcome` to hand to the formatter. Read the cap from `core.ranking` exactly as `Core::search` does (`src/core/search.rs:853`), so the MCP door keeps the cap it has today.

Update the other `format_search_results` call inside `format_answer` to pass `None`: an answer's sources were not ranked for the reader.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/mcp/mod.rs
git commit -m "feat: MCP says why a hit is where it is, when asked"
```

---

### Task 10: The API says it

**Files:**
- Modify: `src/web/api.rs:690-760` (`SearchParams` and the `search` handler)
- Test: `src/web/api.rs`

**Interfaces:**
- Consumes: `SearchOutcome`, `SearchQuery::explain`.
- Produces: `?explain=1` returns `{"results": [...], "explanation": {...}}`; without it the response is the bare array it is today.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn the_bare_search_response_is_still_an_array() {
    let (app, token) = app_and_token().await;
    let body = get_json(&app, "/api/v1/search?q=anything", &token).await;
    assert!(
        body.is_array(),
        "no existing client passes `explain`, so no existing client may see \
         a different envelope"
    );
}

#[tokio::test]
async fn explain_wraps_the_results_and_adds_the_pool() {
    let (app, token) = app_and_token().await;
    let body = get_json(&app, "/api/v1/search?q=anything&explain=1", &token).await;
    assert!(body["results"].is_array());
    assert!(body["explanation"]["candidates_fetched"].is_number());
}
```

Use whatever request helper the surrounding tests use — `app_and_token` exists at `src/web/api.rs`; follow the nearest existing GET test for the exact helper name and signature.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib web::api`
Expected: FAIL — `explanation` is null / the response is an array under `explain=1`.

- [ ] **Step 3: Write the implementation**

Add `pub explain: Option<bool>` to the handler's `SearchParams`, set `explain` on the `SearchQuery`, and branch the response:

```rust
let explain = q.explain.unwrap_or(false);
// … build `query` with `explain`
let cap = tenant.core.ranking.read().expect("ranking lock").per_source_cap;
let (results, outcome) = tenant.core.search_with(&query, cap, origin).await?;
Ok(Json(match explain {
    // Only a caller that asked sees the envelope, so no existing client is
    // broken by a response that grew a shape.
    true => serde_json::json!({ "results": results, "explanation": outcome.explanation }),
    false => serde_json::to_value(results)?,
}))
```

The handler's return type becomes `Result<Json<serde_json::Value>>`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/web/api.rs
git commit -m "feat: the API returns the explanation to a caller that asks"
```

---

### Task 11: The rail says it

**Files:**
- Modify: `src/web/ui.rs:17-60` (`RenderedResult`), the `render_hit` function, `:1189` (the search call)
- Modify: `src/web/templates/_results.html:96-104` (the `rail-why` block)
- Test: `src/web/ui.rs`

**Interfaces:**
- Consumes: `HitExplanation` on `SearchResult`.
- Produces: `RenderedResult::why_ranked: Option<String>` — one prerendered sentence, because askama should not carry the branching this needs.

The rail already has a line for exactly this (`_results.html:96`, "This says why it is *here*"). The explanation extends that line rather than adding a second one; two lines saying why would be the disagreement this branch exists to end.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn the_rail_sentence_names_a_cap_that_redistributed_nothing() {
    let e = crate::core::explain::HitExplanation {
        retrieved_rank: 3,
        cap: crate::core::explain::CapEffect::Refilled,
        ..Default::default()
    };
    let s = why_ranked(&e).expect("a refilled hit has something to say");
    assert!(
        s.contains("one source filled the list"),
        "the operator gets the consequence, not the mechanism: got {s:?}"
    );
}

#[test]
fn a_hit_no_stage_touched_says_nothing() {
    let e = crate::core::explain::HitExplanation {
        retrieved_rank: 0,
        cap: crate::core::explain::CapEffect::Kept,
        ..Default::default()
    };
    assert!(
        why_ranked(&e).is_none(),
        "a quiet stage renders nothing; a row of no-ops is noise"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib web::ui`
Expected: FAIL — `cannot find function why_ranked`.

- [ ] **Step 3: Write the implementation**

Add to `src/web/ui.rs`:

```rust
/// The rail's half of the explanation: the consequence, in a sentence.
///
/// Deliberately not the MCP form. An agent reads a list of stages; a person
/// reads why this row is above the one below it, and a stage that changed
/// nothing is not part of that answer.
fn why_ranked(e: &crate::core::explain::HitExplanation) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = &e.rerank
        && s.from > s.to
    {
        parts.push("moved up by the reranker".to_string());
    }
    if matches!(e.cap, crate::core::explain::CapEffect::Refilled) {
        parts.push("kept only because one source filled the list".to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}
```

Add `pub why_ranked: Option<String>` to `RenderedResult`, filled in `render_hit` from `r.explanation.as_ref().and_then(why_ranked)` — but only when the request asked, so the field is `None` for an ordinary search.

Extend the existing `rail-why` block in `_results.html`:

```html
{% if r.primed || r.weak || r.model_written || r.why_ranked.is_some() %}
<p class="rail-why">
  {# … the three existing clauses, unchanged … #}
  {%- if let Some(why) = r.why_ranked %}{% if r.primed || r.weak || r.model_written %} · {% endif %}{{ why }}{% endif -%}
</p>
{% endif %}
```

Set `explain` on the `SearchQuery` at `src/web/ui.rs:1192` from the incremental-search parameters, defaulting off.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib && cargo test`
Expected: PASS, including the browser tests if they run in this environment.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/web/ui.rs src/web/templates/_results.html
git commit -m "feat: the rail says when one source filled the list"
```

---

### Task 12: Move the roadmap item to built

**Files:**
- Modify: `ROADMAP.md:498-510`

- [ ] **Step 1: Rewrite the item as built**

Delete the bullet at `ROADMAP.md:498` and add a `Built:` paragraph to `## [Retrieval]`, in the voice the rest of the file uses — what it is, what it deliberately is not, and what it cost that the estimate missed. Cover:

- One object on the hit, read by the rail, MCP's meta line and the API.
- Three of the eight stages run inside Qdrant and are reconstructed from the payload rather than measured, so a full explanation still costs one vector search. The reconstruction is pinned against real Qdrant, because a unit test would only pin our own belief.
- What it is not: nothing is stored, so the corpus-concentration figure has to be gathered by deliberate searches rather than read off history.
- The `retrieved_rank` naming: the pre-recency RRF rank is not obtainable without a second query, so the baseline is what retrieval returned, fusion and scoring together.

Then update the **Server-side grouping** item at `ROADMAP.md:460` to say that the instrument for its measurement now exists.

- [ ] **Step 2: Commit**

```bash
git add ROADMAP.md
git commit -m "docs: the ranking explanation is built"
```

---

## Self-Review

**Spec coverage.** §2.1 → Tasks 1, 4, 5, 6. §2.2 → Tasks 1, 2, 3. §2.3 → Tasks 6, 7. §2.4 → Tasks 9, 10, 11. §3 (stage order) → Task 4's ordering and Task 3's call site. §4 (the two objects) → Tasks 1 and 2. §5 (reconstruction and its contract) → Tasks 6 and 7. §6 (the cap) → Task 3. §7 (three doors) → Tasks 9, 10, 11. §8 (opt-in rendering, order invariance) → Task 8. §9 (the test table) → every row is claimed by a task: rows 1–5 by Tasks 7, 3, 3, 8, 4; rows 6–7 by Tasks 9 and 10. §10 and §11 are statements, not code; §10's storage decision is recorded in Task 12's roadmap text.

**Placeholder scan.** No TBDs. Two places defer to the codebase rather than naming a value, and both say exactly what to look at: Task 5's reranker trait signature (`src/infer/mod.rs:116`) and Task 10's request helper. Task 8 says to take the compiler's list of `SearchQuery` literals rather than this document's, because a stale list here would be worse than none.

**Type consistency.** `HitExplanation`, `SearchExplanation`, `StageEffect`, `CapEffect` are defined in Task 1 and used under those names throughout. `SearchOutcome { timing, explanation }` from Task 2 is destructured as `outcome.timing` in Tasks 2 and 9 and `outcome.explanation` in Tasks 9 and 10. `CapReport { corpora_in_pool, displaced, refilled }` from Task 3 is read in Tasks 3 and 4; `refilled` is a `HashSet<String>` there and a `usize` on `SearchExplanation`, which Task 3's call site converts with `.len()` — the two names are deliberate and the conversion is written out. `scoring_terms` returns `(Option<f32>, Option<f32>)` in Task 6 and is called that way in Tasks 6 and 7. `why_ranked` is Task 11's only new function and is used only there.
