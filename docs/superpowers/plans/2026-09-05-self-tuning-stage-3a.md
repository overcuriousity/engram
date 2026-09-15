# Self-tuning, stage 3a: the three knobs stage 2b left out

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put rerank on/off, `prime_lift` and `spread_max` under the idle pass, so that after this plan every knob the search pipeline has is either on the counterfactual ladder or on a lived rule.

**Architecture:** No new engine. `RankingParams` gains three fields and the stored shapes widen through JSON with serde defaults, as in 2b. Serving reads the three knobs off `Core::ranking` instead of the file. Two new facts are recorded beside the captured pool — the priming inputs of a search (`search_context`) and which candidates were the appended band (`search_candidates.band`) — and observations learn which search event they came from. The pass gains one ladder (prime lift), one flip candidate with its own base (rerank, compared against the *served* rank), and one lived rule that runs when the ladder proposes nothing (spread).

**Tech Stack:** Rust 2024 edition, sqlx 0.9 over SQLite, tokio, serde. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-05-self-tuning-stage-3-design.md`, Part A. Read "What this stage relies on" and all of Part A before task 1. The stage 2b plan's "What the code admits" table is what this plan overturns; read it too.

**Depends on:** stage 2b, on `feat/observations`. Baseline: 2530 passing, 0 failing, 1 ignored.

## What this plan admits

Three places it reads the spec differently, each flagged here so nobody reopens them one at a time:

- **The rerank flip does not join the ladder's winner choice; it is asked after it.** The spec says the rerank candidate "joins the winner choice with the other candidates". Its rows are scored against a different base (the served rank), so its MRR is not comparable with the ladder rows' MRR, which measure the pre-rerank order. The ladder's winner takes precedence; the flip is proposed only when the ladder proposes nothing. `BUDGET` therefore grows by the prime-lift rungs only, not by one for rerank.
- **The band is not replayed on the Judge door.** The spec says "Replay on the Judge door applies the band with today's links." The spread rule is lived, not counterfactual, so nothing reads a replayed band; and a replayed band sits past `LIMIT`, where `recall_at` cannot see it. An opened appended hit replays like any pair and is a miss on every ladder row — a tie, which is what a knob the row cannot see should be.
- **Priming inputs are captured at every door that primes, and the sitting is non-empty only at the UI door.** That is the tree as it stands (`Origin::session` is `None` off the web, and `sitting.prime` gates it), not a choice made here.

## Global Constraints

- Gate, every task: `cargo fmt --all --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`. Adding a field to `RankingParams` breaks initializers `cargo test --lib` does not compile — `tests/eval.rs` among them — so `--all-targets` is the check, not `--lib`.
- No tuned constant. Every gate reads the evidence in front of it; the two-net rule is `recommend`'s and the rate rule is `holds_up`'s.
- `config.toml` is never written. The file is the starting rung; the database holds what is live.
- Schema changes are on the `ADDITIVE` list in `src/store/mod.rs` with a written reason, or are new tables. Nothing here recreates a base.
- Test names are sentences stating the rule.
- `gen` is a reserved keyword in edition 2024. `crate::error::Error` has no `From<serde_json::Error>`; use the store module's `json()` / `from_json()` helpers. sqlx 0.9 wants SQL as literals. Ids are ULIDs; shorten by the tail.
- Commit after every task, on `feat/observations`.

---

### Task 1: Three more knobs on `RankingParams`, and the stored shapes widen

**Files:**
- Modify: `src/core/ranking.rs`
- Modify: `src/store/generations.rs:15-56` (`GenerationParams` and both `From`s)
- Modify: `src/store/eval_runs.rs:13-40` (`RunParams` and its `From`s)
- Modify: `src/config.rs:955-965` (default fns beside `default_candidate_multiplier`)
- Modify: `src/core/mod.rs:493` (`from_vector` call), `src/core/mod.rs:727` (test core literal)
- Modify: `src/eval/sweep.rs:196-203` (`moved`)
- Modify: `src/web/insights.rs:750-758` (`params_str`)
- Modify: every initializer clippy reports (`src/config.rs`, `src/core/search.rs`, `src/eval/sweep.rs`, `tests/eval.rs`)

**Interfaces:**
- Produces: `RankingParams { prime_lift: usize, spread_max: usize, rerank: bool }` beside the four existing fields; `RankingParams::from_config(vector: &VectorConfig, associate: &AssociateConfig, reranker_configured: bool)` replacing `from_vector`; `pub const PRIME_LIFTS: [usize; 4] = [0, 1, 2, 4]` and `pub const SPREADS: [usize; 6] = [0, 1, 2, 3, 5, 8]` in `src/core/ranking.rs`; `config::default_prime_lift() -> usize` (0), `config::default_spread_max() -> usize` (3), `config::default_rerank_knob() -> bool` (true).

- [ ] **Step 1: Write the failing tests in `src/core/ranking.rs`**

Add to the existing `mod tests`:

```rust
#[test]
fn the_three_late_knobs_are_read_from_the_file_beside_the_others() {
    let associate = crate::config::AssociateConfig {
        prime_lift: 2,
        spread_max: 5,
        ..Default::default()
    };
    let p = RankingParams::from_config(&vector_config(3), &associate, true);
    assert_eq!(p.prime_lift, 2);
    assert_eq!(p.spread_max, 5);
    assert!(p.rerank);
    let p = RankingParams::from_config(&vector_config(3), &associate, false);
    assert!(!p.rerank, "no reranker configured means the knob starts off");
}

#[test]
fn the_shipped_prime_lift_and_spread_sit_on_their_ladders() {
    let p = RankingParams::default();
    assert!(PRIME_LIFTS.contains(&p.prime_lift));
    assert!(SPREADS.contains(&p.spread_max));
    // Lift cannot be negative, so its ladder starts at the shipped value and
    // can only be walked up; that is a fact about the knob, not a bias.
    assert_eq!(PRIME_LIFTS[0], p.prime_lift);
}
```

And in `src/store/generations.rs` tests:

```rust
#[test]
fn a_generation_row_written_before_the_late_knobs_still_reads() {
    let old = r#"{"recency_weight":0.05,"per_source_cap":3,"candidate_multiplier":3,"recency_half_life_days":180}"#;
    let p: GenerationParams = serde_json::from_str(old).unwrap();
    assert_eq!(p.prime_lift, crate::config::default_prime_lift());
    assert_eq!(p.spread_max, crate::config::default_spread_max());
    assert!(p.rerank);
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib ranking:: generations::tests::a_generation_row_written_before 2>&1 | tail -20`
Expected: compile errors — no `from_config`, no `prime_lift` field.

- [ ] **Step 3: Add the fields, defaults, ladders and constructor**

In `src/config.rs` beside `default_candidate_multiplier`:

```rust
/// The shipped `associate.prime_lift`: off. Read by the generation shapes so
/// a row written before the knob existed decodes as what it ran under.
pub(crate) fn default_prime_lift() -> usize {
    0
}
pub(crate) fn default_spread_max() -> usize {
    3
}
/// A generation written before the rerank knob existed ran with whatever
/// reranker the file named, which is "on" wherever one was configured.
pub(crate) fn default_rerank_knob() -> bool {
    true
}
```

Make `AssociateConfig::default()` read `prime_lift: default_prime_lift()` and `spread_max: default_spread_max()` so the two cannot drift.

In `src/core/ranking.rs`:

```rust
/// The rungs for `prime_lift`: how many places an accessible hit may climb.
/// Starts at the shipped zero, because a lift cannot be negative.
pub const PRIME_LIFTS: [usize; 4] = [0, 1, 2, 4];
/// The rungs for `spread_max`: how many linked artifacts hang under the list.
pub const SPREADS: [usize; 6] = [0, 1, 2, 3, 5, 8];

pub struct RankingParams {
    // ...existing four...
    /// How many places priming may lift a hit. Zero is off.
    pub prime_lift: usize,
    /// How many associated artifacts are appended under the ranked list.
    pub spread_max: usize,
    /// Whether the configured reranker runs. Meaningless where none is
    /// configured: serving treats it as `false` there.
    pub rerank: bool,
}

impl Default for RankingParams {
    fn default() -> Self {
        Self {
            // ...existing...
            prime_lift: crate::config::default_prime_lift(),
            spread_max: crate::config::default_spread_max(),
            rerank: crate::config::default_rerank_knob(),
        }
    }
}

impl RankingParams {
    /// The file's starting rungs. `reranker_configured` is whether `[infer]`
    /// names one: the knob starts on where it can, and there is nothing to
    /// start where it cannot.
    pub fn from_config(
        cfg: &VectorConfig,
        associate: &crate::config::AssociateConfig,
        reranker_configured: bool,
    ) -> Self {
        Self {
            // ...the existing four, as `from_vector` built them...
            prime_lift: associate.prime_lift,
            spread_max: associate.spread_max,
            rerank: reranker_configured,
        }
    }
}
```

Delete `from_vector`; update its one caller in `src/core/mod.rs:493` to
`RankingParams::from_config(&cfg.vector, &cfg.associate, cfg.infer.rerank.is_some())` (read how `Core` holds the reranker — `self.reranker.is_some()` after build is the same fact; use whichever the constructor has in hand). Update `the_retrieval_knobs_are_read_from_the_file_beside_the_ranking_ones` to call `from_config(.., &Default::default(), false)`.

`GenerationParams` and `RunParams` each gain the three fields with `#[serde(default = "crate::config::default_prime_lift")]` etc., and both `From` impls copy them. `moved` in `src/eval/sweep.rs` adds three `usize::from(...)` terms. `params_str` in `src/web/insights.rs` becomes:

```rust
format!(
    "recency {:.2}, cap {}, pool ×{}, half-life {}d, lift {}, spread {}, rerank {}",
    p.recency_weight,
    cap_str(p.per_source_cap),
    p.candidate_multiplier,
    p.recency_half_life_days,
    p.prime_lift,
    p.spread_max,
    if p.rerank { "on" } else { "off" }
)
```

Fix the insights test that asserts on `params_str` output, if one does.

- [ ] **Step 4: Run the gate**

Run: `cargo clippy --all-targets --locked -- -D warnings 2>&1 | grep -E '^error' | head`
Expected: missing-field errors at every literal initializer. Fix each with the new fields or `..Default::default()` / `..current`. Then:

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked 2>&1 | tail -5`
Expected: all green; 2530 + 3 tests.

- [ ] **Step 5: Commit**

```bash
git add -A src tests
git commit -m "feat(evolve): three more knobs on RankingParams, and the stored shapes still read"
```

---

### Task 2: Serving reads the three knobs off the live generation

**Files:**
- Modify: `src/core/search.rs:1329-1336` (the `reranking` decision), `:1663-1690` (the prime block), `:989-1000` and `:1804-1806` (`associated` and its call), tests near `:2572` and `:4162`

**Interfaces:**
- Consumes: `RankingParams.{prime_lift, spread_max, rerank}` from task 1.
- Produces: `associated(&self, results, filter, spread_max: usize)`; serving that ignores `self.associate.prime_lift` and `self.associate.spread_max` (they seed the generation and nothing else).

- [ ] **Step 1: Write the failing tests in `src/core/search.rs` tests**

Find the fixture `rerank_reorders_when_configured` (`:2572`) and the spread test at `:4162`; model on them:

```rust
#[tokio::test]
async fn a_generation_with_rerank_off_never_calls_the_reranker() {
    // Same base as `rerank_reorders_when_configured`, with the knob off.
    let (core, reranker) = core_with_counting_reranker().await; // reuse that test's helper, or lift one
    core.ranking.write().unwrap().rerank = false;
    let q = SearchQuery { q: "anything".into(), ..Default::default() };
    core.search(&q, Door::Ui).await.unwrap();
    assert_eq!(reranker.calls(), 0, "rerank off in the generation must mean no call");
}

#[tokio::test]
async fn a_generation_with_spread_zero_appends_nothing() {
    // Same base as the test at :4162 that sees an association appended.
    let (core, results) = base_with_an_association().await;
    core.ranking.write().unwrap().spread_max = 0;
    let out = core.associated(&results, &SearchFilter::default(), 0).await;
    assert!(out.is_empty());
}
```

Adapt the helper names to what those two tests actually build; the assertions are the point.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib search::tests::a_generation_with 2>&1 | tail -20`
Expected: the rerank test fails on the call count; the spread test does not compile (`associated` takes two arguments).

- [ ] **Step 3: Read the knobs**

At `:1329`:

```rust
let reranking = query.rerank
    && params.rerank
    && match door { /* unchanged */ };
```

The prime block at `:1663`: replace `self.associate.prime_lift > 0` in the guard with nothing (the guard becomes `self.associating() && !matches!(door, Door::Ask | Door::Judge)`), and pass `params.prime_lift` to `prime(...)` instead of `self.associate.prime_lift`. `prime` already returns the list untouched at zero. (Task 3 restructures this block further; do the minimum here.)

`associated` gains `spread_max: usize` and uses it wherever it read `self.associate.spread_max`; the call at `:1805` passes `params.spread_max`. Check the store-limit arithmetic at `:1033` uses the parameter.

- [ ] **Step 4: Run the gate**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked 2>&1 | tail -5`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add -A src
git commit -m "feat(evolve): serving reads prime lift, spread and rerank off the live generation"
```

---

### Task 3: A search records the inputs priming read, and an observation names its search

**Files:**
- Modify: `src/store/schema.sql` (new table `search_context`; `observations.event_id`)
- Modify: `src/store/mod.rs:150-224` (`ADDITIVE` gains `observations.event_id`)
- Modify: `src/store/feedback.rs:144-176` (`NewEvent.context`), `:224-430` (`record_search` writes it), `:690-725` (open path stamps `event_id`), new reader `search_context`
- Modify: `src/store/observations.rs:74-96` (`NewObservation.event_id`, `Observation.event_id`), `:104-130` (`insert`), `:141-170` (reader selects it)
- Modify: `src/jobs/observe.rs:78-87` (give-ups carry their event)
- Modify: `src/store/asks.rs` (cited observations pass `event_id: None`)
- Modify: `src/core/search.rs:1663-1690` (the prime block builds a `Priming` and keeps it)

**Interfaces:**
- Produces:
  ```rust
  // src/core/search.rs
  #[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
  pub struct Priming {
      pub activation: HashMap<String, f64>,
      pub sitting: HashSet<String>,
      pub due: HashSet<String>,
  }
  // src/store/feedback.rs
  pub struct NewEvent { /* existing */ pub context: Option<Priming> }
  impl Store { pub async fn search_context(&self, event_id: &str) -> Result<Option<Priming>> }
  // src/store/observations.rs
  pub struct NewObservation { /* existing */ pub event_id: Option<String> }
  pub struct Observation    { /* existing */ pub event_id: Option<String> }
  ```

- [ ] **Step 1: Write the failing tests**

In `src/store/feedback.rs` tests:

```rust
#[tokio::test]
async fn a_recorded_search_keeps_what_priming_read_and_an_open_names_the_search() {
    let store = test_store().await; // whatever fixture the module's tests use
    let mut ev = sample_event(); // the module's NewEvent fixture, door Ui, two candidates
    ev.context = Some(crate::core::search::Priming {
        activation: [("art-1".to_string(), 0.7)].into_iter().collect(),
        sitting: ["art-2".to_string()].into_iter().collect(),
        due: Default::default(),
    });
    let id = store.record_search(ev, 0).await.unwrap();
    let ctx = store.search_context(&id).await.unwrap().expect("context stored");
    assert_eq!(ctx.activation.get("art-1"), Some(&0.7));
    assert!(ctx.sitting.contains("art-2"));

    // An open on it writes an observation that names the event.
    let generation = record_a_generation(&store).await; // the fixture the open test at :1187 uses
    store.mark_opened(&id, "art-1").await.unwrap(); // the module's open entry point
    let obs = store.observations_for_generation(&generation, 10).await.unwrap();
    assert_eq!(obs[0].event_id.as_deref(), Some(id.as_str()));
}

#[tokio::test]
async fn a_search_without_priming_stores_no_context() {
    let store = test_store().await;
    let id = store.record_search(sample_event(), 0).await.unwrap();
    assert!(store.search_context(&id).await.unwrap().is_none());
}
```

In `src/jobs/observe.rs` tests, extend the existing give-up test: after `run`, the gave-up observation's `event_id` is `Some(the unopened event's id)`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib feedback::tests::a_recorded_search_keeps feedback::tests::a_search_without observe:: 2>&1 | tail -20`
Expected: compile errors on `context`, `search_context`, `event_id`.

- [ ] **Step 3: Schema**

In `src/store/schema.sql`, after `search_candidates`:

```sql
-- What priming read at the moment of one search: the activation of the
-- candidates, the artifacts this sitting had been in, and the due reminders.
-- Recorded so the idle pass can replay the search at another `prime_lift` and
-- see what the searcher would have seen; observations alone do not carry it.
-- One row per event that primed, none for a door that never primes.
CREATE TABLE IF NOT EXISTS search_context (
  event_id  TEXT PRIMARY KEY REFERENCES search_events(id) ON DELETE CASCADE,
  context   TEXT NOT NULL
);
```

On `observations`, add after `excluded_at`:

```sql
  -- The search event this came from, where it came from one: opened and
  -- gave-up observations. NULL for a citation, which comes from an ask.
  event_id      TEXT
```

In `ADDITIVE` (`src/store/mod.rs`), bump the array length and add:

```rust
// Nullable, no default, and NULL is the truth about every observation
// written before it: nothing recorded which search it came from.
(
    "observations",
    "event_id",
    "ALTER TABLE observations ADD COLUMN event_id TEXT",
),
```

- [ ] **Step 4: The store**

`Priming` in `src/core/search.rs` as in Interfaces (derive `Serialize`/`Deserialize`; `HashMap`/`HashSet` serialize as object/array). `NewEvent` gains `pub context: Option<crate::core::search::Priming>`; every literal `NewEvent { .. }` in tests gains `context: None`.

In `record_search`, after the candidate loop, inside the same transaction:

```rust
if let Some(ctx) = &ev.context {
    sqlx::query("INSERT OR REPLACE INTO search_context (event_id, context) VALUES (?, ?)")
        .bind(&id)
        .bind(json(ctx)?)
        .execute(&mut *tx)
        .await?;
}
```

Use the module's JSON helper (add a local `json()` if `feedback.rs` has none; look at how `filters` is built — it is already a string, so a `serde_json::to_string(...).map_err(|e| Error::Store(e.to_string()))` inline is the shape).

Reader:

```rust
/// What priming read when this search ran, or `None` where it did not prime.
pub async fn search_context(&self, event_id: &str) -> Result<Option<crate::core::search::Priming>> {
    let raw: Option<String> = sqlx::query_scalar("SELECT context FROM search_context WHERE event_id = ?")
        .bind(event_id)
        .fetch_optional(&self.pool)
        .await?;
    raw.map(|s| serde_json::from_str(&s).map_err(|e| crate::error::Error::Store(e.to_string())))
        .transpose()
}
```

`NewObservation` and `Observation` gain `event_id: Option<String>`; `insert` binds it; `observations_for_generation` selects and reads it. The open path (`feedback.rs:706-724`) passes `event_id: Some(event_id.to_string())`; `observe.rs:78` passes `event_id: Some(r.get("id"))` (the query at `:53` already selects `e.id AS id`); `asks.rs` passes `None`. Every test literal gains `event_id: None`.

- [ ] **Step 5: The prime block keeps what it read**

Restructure `src/core/search.rs:1663-1690`:

```rust
let mut primed_with: Option<Priming> = None;
if self.associating() && !matches!(door, Door::Ask | Door::Judge) {
    let before = positions(&results);
    let ids: Vec<String> = results.iter().map(|r| r.artifact_id.clone()).collect();
    let sitting: HashSet<String> = match self.sitting.prime { /* unchanged */ };
    let priming = Priming {
        activation: self.engagement_now(&ids).await,
        sitting,
        due: due.clone(),
    };
    results = prime(
        results,
        &priming.activation,
        self.associate.prime_margin,
        params.prime_lift,
        &priming.sitting,
        &priming.due,
    );
    note_reorder(&mut results, &before, |e| &mut e.prime);
    primed_with = Some(priming);
}
```

and in the capture block set `context: primed_with.clone()` on `NewEvent`. The one behaviour change: `engagement_now` is now read at lift zero too, on the doors that prime. That is the capture the spec asks for.

- [ ] **Step 6: Run the gate**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked 2>&1 | tail -5`
Expected: green.

- [ ] **Step 7: Commit**

```bash
git add -A src
git commit -m "feat(evolve): a search records what priming read, and an observation names its search"
```

---

### Task 4: The pass replays priming, and walks the lift ladder

**Files:**
- Modify: `src/store/feedback.rs:89-131` (`Origin.replay`)
- Modify: `src/core/search.rs` (the prime block admits a replay on the Judge door)
- Modify: `src/eval/sweep.rs:223-227` (`Pair.priming`), `:240-272` (`rank_of`), `:361-400` (`observation_pairs` loads it), `:71-140` (`candidates` walks `PRIME_LIFTS`)
- Modify: `src/jobs/tune.rs:43` (`BUDGET`)

**Interfaces:**
- Consumes: `Priming`, `Store::search_context`, `Observation.event_id` from task 3.
- Produces: `Origin { /* existing */ pub replay: Option<Priming> }` with builder `Origin::primed_as(self, p: Priming) -> Origin`; `Pair { /* existing */ pub(crate) priming: Option<Priming> }`; `tune::BUDGET = 19`.

- [ ] **Step 1: Write the failing tests**

In `src/eval/sweep.rs` tests, using `seeded()` and `ranks_order`:

```rust
#[tokio::test]
async fn a_pair_with_a_sitting_ranks_differently_at_lift_two_and_the_same_without_one() {
    let (core, order) = seeded().await;
    // The last-ranked hit was read in this sitting: at lift 2 it climbs two
    // places on the Judge door, where priming is otherwise off.
    let priming = Priming {
        activation: Default::default(),
        sitting: [order[5].clone()].into_iter().collect(),
        due: Default::default(),
    };
    let current = *core.ranking.read().unwrap();
    let with = Pair { query: QUERY.into(), satisfies: vec![order[5].clone()], query_vec: None, priming: Some(priming) };
    let without = Pair { priming: None, ..with.clone() };
    let lifted = RankingParams { prime_lift: 2, ..current };
    let at_zero = rank_of(&core, &with, current, false).await.unwrap();
    let at_two = rank_of(&core, &with, lifted, false).await.unwrap();
    assert_eq!(at_zero, Some(5));
    assert_eq!(at_two, Some(3), "two places, no further, never past rank 1");
    assert_eq!(
        rank_of(&core, &without, lifted, false).await.unwrap(),
        Some(5),
        "no context, no lift: every rung is the same list"
    );
}

#[test]
fn the_chooser_walks_the_lift_ladder_upward_from_zero() {
    let current = RankingParams::default();
    let grid = candidates(current, &[], crate::jobs::tune::BUDGET);
    let lifts: Vec<usize> = grid.iter().map(|c| c.prime_lift).filter(|l| *l != current.prime_lift).collect();
    assert_eq!(lifts, vec![1, 2, 4]);
    assert_eq!(grid.len(), crate::jobs::tune::BUDGET, "every rung of every axis, once");
}
```

`prime` skips lists shorter than three and never moves rank 0 or 1; the seeded base has six hits, so index 5 climbing two lands at 3.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib sweep::tests::a_pair_with_a_sitting sweep::tests::the_chooser_walks_the_lift 2>&1 | tail -20`
Expected: compile errors on `priming` / `replay`; the ladder test fails on length.

- [ ] **Step 3: `Origin` carries a replay**

```rust
pub struct Origin {
    // ...existing...
    /// Priming inputs handed in by a replay, on the Judge door where priming
    /// is otherwise off. Serving never sets this; the idle pass does, from
    /// `search_context`, so the pass sees what the searcher saw.
    pub replay: Option<crate::core::search::Priming>,
}
impl Origin {
    pub fn primed_as(mut self, p: crate::core::search::Priming) -> Origin {
        self.replay = Some(p);
        self
    }
}
```

`From<Door>` sets `replay: None`; fix the other literals.

In the prime block from task 3, the guard becomes:

```rust
let primes = self.associating() && !matches!(door, Door::Ask | Door::Judge);
if primes || origin.replay.is_some() {
    let priming = match origin.replay.clone() {
        Some(p) => p,
        None => Priming { activation: self.engagement_now(&ids).await, sitting, due: due.clone() },
    };
    // ...prime(...) as before...
    // Only a real search records what it read; a replay is not an event.
    if primes { primed_with = Some(priming); }
}
```

- [ ] **Step 4: The pair carries it, the ladder walks it**

`Pair` gains `pub(crate) priming: Option<Priming>`; `pairs_to_replay` sets `None`; `observation_pairs` loads it:

```rust
let priming = match o.event_id.as_deref() {
    Some(e) => core.store.search_context(e).await?,
    None => None,
};
```

`rank_of` builds its origin as `Origin::from(Door::Judge)` and, when `pair.priming` is `Some`, `.primed_as(p.clone())`, and hands that to `search_with_ranking` instead of the bare door.

`candidates` gains a fifth `outward` over `PRIME_LIFTS` (`below: |v| *v < current.prime_lift`), interleaved with the others in the same loop. `BUDGET` becomes 19 (the current configuration plus 4 + 3 + 4 + 4 + 3 rungs); update its doc comment and the assertion in any test that counts sixteen.

- [ ] **Step 5: Run the gate**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked 2>&1 | tail -5`
Expected: green.

- [ ] **Step 6: Commit**

```bash
git add -A src
git commit -m "feat(evolve): the pass replays priming from what the search recorded, and walks the lift ladder"
```

---

### Task 5: The rerank flip, measured against the served rank

**Files:**
- Modify: `src/eval/sweep.rs:223-227` (`Pair.served`), `:361-400` (`observation_pairs` fills it), `:440-520` (`score` and `Scored`)
- Modify: `src/jobs/tune.rs:170-240` (`propose` asks the flip after the ladder)

**Interfaces:**
- Consumes: `RankingParams.rerank`, `Core::reranks_search()`.
- Produces: `Pair { /* existing */ pub(crate) served: Option<usize> }` (0-based; `observation.rank - 1`); `sweep::rerank_flip(core, pairs, current, stop_after) -> Result<Option<Flip>>` where `pub(crate) struct Flip { pub params: RankingParams, pub predicted: f64 }`.

- [ ] **Step 1: Write the failing tests in `src/eval/sweep.rs`**

The suite has a counting fake reranker (`:901-915`, a `rerank` that runs inside every grid search). Use it:

```rust
#[tokio::test]
async fn a_replay_without_the_reranker_that_places_two_net_pairs_better_adopts_rerank_off() {
    // A reranker that buries everything: it reverses the list. Served ranks
    // are what it produced; the replay without it is the vector order.
    let (core, order) = seeded_with_reversing_reranker().await;
    let current = RankingParams { rerank: true, ..*core.ranking.read().unwrap() };
    let pairs: Vec<Pair> = [0, 1].iter().map(|i| Pair {
        query: QUERY.into(),
        satisfies: vec![order[*i].clone()],
        query_vec: None,
        priming: None,
        // The reranker reversed six hits: rank 0 was served at 5, rank 1 at 4.
        served: Some(5 - i),
    }).collect();
    let flip = rerank_flip(&core, &pairs, current, None).await.unwrap().expect("a flip is offered");
    assert!(!flip.params.rerank);
    assert!(flip.predicted > 0.0);
}

#[tokio::test]
async fn no_reranker_means_no_flip_is_offered() {
    let (core, order) = seeded().await;
    let current = *core.ranking.read().unwrap();
    let pairs = vec![Pair { query: QUERY.into(), satisfies: vec![order[0].clone()], query_vec: None, priming: None, served: Some(0) }];
    assert!(rerank_flip(&core, &pairs, current, None).await.unwrap().is_none());
}

#[tokio::test]
async fn a_flip_to_rerank_on_costs_one_call_per_pair() {
    let (core, order) = seeded_with_counting_reranker().await;
    let current = RankingParams { rerank: false, ..*core.ranking.read().unwrap() };
    let pairs: Vec<Pair> = (0..3).map(|i| Pair { query: QUERY.into(), satisfies: vec![order[i].clone()], query_vec: None, priming: None, served: Some(i) }).collect();
    let _ = rerank_flip(&core, &pairs, current, None).await.unwrap();
    assert_eq!(core_reranker_calls(&core), 3);
}
```

Build `seeded_with_reversing_reranker` and `seeded_with_counting_reranker` on `seeded()` plus the fake reranker the file already has at `:901`; if that fake counts, one fixture with a `reverse: bool` serves both.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib sweep::tests::a_replay_without sweep::tests::no_reranker sweep::tests::a_flip_to 2>&1 | tail -20`
Expected: compile errors on `served` / `rerank_flip`.

- [ ] **Step 3: The flip**

`Pair` gains `pub(crate) served: Option<usize>`; `observation_pairs` sets `served: o.rank.map(|r| (r - 1).max(0) as usize)`; `pairs_to_replay` sets `None`.

```rust
/// The other value of the rerank knob, scored against the rank that was
/// actually served. Its own base, because the served rank is the only row
/// that has the reranker in it where the reranker is live.
pub(crate) struct Flip {
    pub params: RankingParams,
    pub predicted: f64,
}

/// Offer the rerank flip, if a reranker serves search and the flip clears
/// `recommend` against the served ranks. Where the live value is "on", the
/// candidate is the replay without the reranker, which costs nothing; where it
/// is "off", the candidate is one reranker call per pair — spent only because
/// the operator configured the reranker.
pub(crate) async fn rerank_flip(
    core: &Core,
    pairs: &[Pair],
    current: RankingParams,
    stop_after: Option<i64>,
) -> Result<Option<Flip>> {
    if !core.reranks_search() {
        return Ok(None);
    }
    let with_served: Vec<&Pair> = pairs.iter().filter(|p| p.served.is_some()).collect();
    if with_served.is_empty() {
        return Ok(None);
    }
    let served: Vec<Option<usize>> = with_served.iter().map(|p| p.served).collect();
    let flipped = RankingParams { rerank: !current.rerank, ..current };
    let mut ranks = Vec::with_capacity(with_served.len());
    for pair in &with_served {
        if let Some(since) = stop_after && core.store.activity_since(since).await? {
            return Ok(None);
        }
        ranks.push(rank_of(core, pair, current, flipped.rerank).await?);
    }
    Ok(recommend(&served, &ranks).then(|| Flip {
        params: flipped,
        predicted: mrr(&ranks) - mrr(&served),
    }))
}
```

Note `rank_of(core, pair, current, rerank)`: the `rerank` argument is what runs the reranker in the replay; `current` supplies the rest. The row's params are `flipped` only in the journal.

In `tune::propose`, after `let Some(winner) = scored.winner() else { ... }` becomes a `match`: when the ladder has no winner, ask `rerank_flip(core, &pairs, current, Some(started)).await?`; if `Some(flip)`, and `flip.params` is not in `tried`, adopt it exactly as the ladder winner is adopted — the same `record_eval_run` (build a `NewEvalRun` from `scored.eval_run(..)` with `best: flip.params.into()`, `best_mrr: base_mrr + flip.predicted`, `recommended: true`), the same `adopt_generation`, the same `mark_eval_run_applied`, the same `tracing::info!`. Factor the adoption tail into `async fn adopt(core, live, winner, run_id, predicted) -> Result<Pass>` so the ladder path and the flip path share it.

- [ ] **Step 4: Write the adoption test in `src/jobs/tune.rs`**

```rust
#[tokio::test]
async fn the_flip_is_asked_only_when_the_ladder_proposes_nothing_and_is_adopted_like_any_move() {
    let (core, generation) = seeded_with_reversing_reranker_and_observations().await;
    // Observations served at the reranked (reversed) ranks, at the top of the
    // uncapped list: nothing on the ladder can improve them, and the replay
    // without the reranker places both better.
    core.evolve.autonomous = true; // however the module's tests switch it on
    let adopted = run(&core).await.unwrap().expect("the flip adopts");
    let live = core.store.live_generation().await.unwrap().unwrap();
    assert_eq!(live.id, adopted);
    assert!(!live.params.rerank);
    assert_eq!(live.parent_id.as_deref(), Some(generation.as_str()));
    assert!(live.run_id.is_some());
}
```

Build the fixture from `seeded_with_nothing_to_gain` with the reversing reranker installed and `observe(.., rank)` at the reversed ranks.

- [ ] **Step 5: Run the gate**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked 2>&1 | tail -5`
Expected: green.

- [ ] **Step 6: Commit**

```bash
git add -A src
git commit -m "feat(evolve): the rerank flip, measured against the rank that was served"
```

---

### Task 6: The band is captured, and spread moves on what the band earned

**Files:**
- Modify: `src/store/schema.sql` (`search_candidates.band`), `src/store/mod.rs` (`ADDITIVE`)
- Modify: `src/store/feedback.rs:144-152` (`NewCandidate.band`), `:409-423` (insert), new reader `band_use`
- Modify: `src/core/search.rs:1700-1760` (the capture builds band rows), `:1804-1815` (the append uses the same `recalled`)
- Modify: `src/store/generations.rs:109-116` (`adopt_generation_lived`)
- Modify: `src/jobs/tune.rs` (`spread_step`, called after the flip)
- Modify: `src/web/insights.rs:816-822` (history names a lived adoption)

**Interfaces:**
- Produces: `NewCandidate { /* existing */ pub band: bool }`; `Store::band_use(generation_id: &str, spread_max: usize) -> Result<BandUse>` with `pub struct BandUse { pub band_used: usize, pub tail_used: usize }`; `Store::adopt_generation_lived(g: &NewGeneration, predicted: f64) -> Result<String>` (`run_id` NULL); `tune::spread_step(core, live, current) -> Result<Pass>`; `pub fn next_spread(current: usize, use_: BandUse) -> Option<usize>` in `src/jobs/tune.rs`.

- [ ] **Step 1: Write the failing tests**

The rule, pure, in `src/jobs/tune.rs`:

```rust
#[test]
fn a_band_used_more_than_the_tail_grows_one_rung_and_less_shrinks_one() {
    use crate::store::feedback::BandUse;
    let u = |band_used, tail_used| BandUse { band_used, tail_used };
    assert_eq!(next_spread(3, u(4, 2)), Some(5), "two net events: grow");
    assert_eq!(next_spread(3, u(2, 4)), Some(2), "two net events: shrink");
    assert_eq!(next_spread(3, u(3, 2)), None, "one event could account for it");
    assert_eq!(next_spread(3, u(3, 3)), None, "equal use holds");
    assert_eq!(next_spread(8, u(9, 0)), None, "the top rung has nowhere to grow");
    assert_eq!(next_spread(0, u(0, 0)), Some(1), "from zero, the first rung is tried once");
}
```

The capture, in `src/core/search.rs` tests near `:4162`:

```rust
#[tokio::test]
async fn an_appended_hit_is_captured_in_the_band_and_an_open_on_it_is_an_observation() {
    let (core, ..) = base_with_an_association().await; // the fixture the test at :4162 uses, learn on
    let generation = generation_for(&core).await; // as in tune.rs tests
    let (results, outcome) = core.search(&query_that_recalls(), Door::Ui).await.unwrap();
    let appended = results.last().unwrap();
    let event = outcome.event_id.expect("the UI door waits for its capture");
    let rows = core.store.candidates_of(&event).await.unwrap(); // whatever the module's test reader is
    assert!(rows.iter().any(|c| c.artifact_id == appended.artifact_id && c.band));
    core.store.mark_opened(&event, &appended.artifact_id).await.unwrap();
    let obs = core.store.observations_for_generation(&generation, 5).await.unwrap();
    assert_eq!(obs[0].artifact_id.as_deref(), Some(appended.artifact_id.as_str()));
    assert_eq!(obs[0].rank, Some(rows.len() as i64), "the rank it was shown at, after the ranked pool");
}
```

The reader, in `src/store/feedback.rs` tests: record an event with three ranked shown candidates and one band candidate, under a generation; open the band one → `band_use(gen, 1)` is `{1, 0}`; open the last ranked one instead → `{0, 1}`; open the first ranked one → `{0, 0}`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib tune::tests::a_band_used search::tests::an_appended_hit feedback::tests::band_use 2>&1 | tail -20`
Expected: compile errors on `next_spread`, `band`, `band_use`.

- [ ] **Step 3: Schema and capture**

`search_candidates` gains `band INTEGER NOT NULL DEFAULT 0` with a comment ("1 for an artifact appended under the ranked list by association; it was shown, at the rank after the pool, and an open on it is an observation like any other"), and the `ADDITIVE` entry (defaulted, and 0 is the truth about every old row: nothing appended was ever captured). `NewCandidate` gains `pub band: bool`, bound in the insert.

In `search_inner`, move the association read *before* the capture block: compute

```rust
let recalled = if self.associating() && !matches!(door, Door::Ask | Door::Judge) {
    self.associated(&results[..limit.min(results.len())], &filter, params.spread_max).await
} else {
    Vec::new()
};
```

(`associated` takes the top `spread_from` of what it is given, so handing it the shown window is the same anchor set the append used). In the capture, after the ranked `candidates` are built, extend them with `recalled.iter().map(|r| NewCandidate { artifact_id, score, similarity: None, shown: true, band: true })`. After the truncate, the existing append block uses this `recalled` instead of calling `associated` again. Ranked candidates set `band: false`.

- [ ] **Step 4: The reader and the lived adoption**

```rust
pub struct BandUse { pub band_used: usize, pub tail_used: usize }

/// Over the opened observations under one generation: how many opened the
/// band, and how many opened the last `spread_max` ranked hits that were
/// shown — the band's own width, at the weak end of the list beside it.
pub async fn band_use(&self, generation_id: &str, spread_max: usize) -> Result<BandUse> {
    let r = sqlx::query(
        "SELECT
           COALESCE(SUM(CASE WHEN c.band = 1 THEN 1 ELSE 0 END), 0) AS band_used,
           COALESCE(SUM(CASE WHEN c.band = 0 AND c.rank >=
               (SELECT COUNT(*) FROM search_candidates s
                 WHERE s.event_id = c.event_id AND s.band = 0 AND s.shown = 1) - ?
             THEN 1 ELSE 0 END), 0) AS tail_used
         FROM observations o
         JOIN search_candidates c ON c.event_id = o.event_id AND c.artifact_id = o.artifact_id
        WHERE o.generation_id = ? AND o.source = 'opened' AND o.excluded_at IS NULL",
    )
    .bind(spread_max as i64)
    .bind(generation_id)
    .fetch_one(&self.pool)
    .await?;
    Ok(BandUse {
        band_used: r.get::<i64, _>("band_used") as usize,
        tail_used: r.get::<i64, _>("tail_used") as usize,
    })
}
```

(`SUM` over zero rows is `NULL` and `COALESCE` makes it `0`, an INTEGER, which is what `i64` wants.)

`adopt_generation_lived` in `generations.rs`: `self.insert_live(g, None, Some(predicted)).await`, doc: "Adopted on lived evidence rather than a replay: no run to name, and `predicted` is the rate that argued for it."

- [ ] **Step 5: The rule and the step**

```rust
/// The spread rule. Grow when the band was used more than the ranked tail
/// beside it by more than one event could account for; shrink on the same
/// rule the other way; hold otherwise. From zero there is no band to
/// measure, so the first rung is offered once and the watch decides.
pub fn next_spread(current: usize, use_: crate::store::feedback::BandUse) -> Option<usize> {
    use crate::core::ranking::SPREADS;
    let at = SPREADS.iter().position(|s| *s == current)?;
    if current == 0 {
        return SPREADS.get(1).copied();
    }
    let net = use_.band_used as i64 - use_.tail_used as i64;
    if net >= 2 {
        SPREADS.get(at + 1).copied()
    } else if net <= -2 {
        at.checked_sub(1).map(|i| SPREADS[i])
    } else {
        None
    }
}

/// The lived step, asked only when the ladder and the flip proposed nothing.
async fn spread_step(core: &Core, live: &Generation, current: RankingParams) -> Result<Pass> {
    let use_ = core.store.band_use(&live.id, current.spread_max).await?;
    let Some(next) = next_spread(current.spread_max, use_) else {
        return Ok(Pass::default());
    };
    let candidate = RankingParams { spread_max: next, ..current };
    let tried = core.store.tried_candidates(&live.embed_recipe, &live.chat_model).await?;
    if tried.contains(&GenerationParams::from(candidate)) {
        return Ok(Pass::default());
    }
    let predicted = match use_.band_used + use_.tail_used {
        0 => 0.0,
        n => use_.band_used as f64 / n as f64,
    };
    let id = core.store.adopt_generation_lived(
        &NewGeneration {
            params: candidate.into(),
            embed_recipe: live.embed_recipe.clone(),
            chat_model: live.chat_model.clone(),
            parent_id: Some(live.id.clone()),
        },
        predicted,
    ).await?;
    *core.ranking.write().expect("ranking lock") = candidate;
    tracing::info!(generation = %id, spread_max = next, band_used = use_.band_used, tail_used = use_.tail_used, "adopted a generation on what the band earned");
    Ok(Pass { adopted: Some(id), reverted: None })
}
```

In `propose`, the order is: ladder winner → rerank flip → `spread_step`. The `ranking != current` guard runs before any of the three adopt.

In `src/web/insights.rs:816`, the history match gains an arm before `(None, Some(_))`:

```rust
(None, Some(_)) if g.predicted.is_some() => format!(
    "adopted by the base on what the band earned, at a use rate of {:.2}",
    g.predicted.unwrap_or(0.0)
),
```

and the standing sentence at `evolve_view` that reads `(parent_id, predicted)` needs no change: a lived adoption has both.

- [ ] **Step 6: Write the pass-level test in `src/jobs/tune.rs`**

```rust
#[tokio::test]
async fn a_base_whose_band_is_used_more_than_its_tail_widens_the_band_when_nothing_else_moves() {
    let (core, generation) = seeded_with_nothing_to_gain_and_a_band().await;
    // Four opens on the band, two on the tail, recorded as observations with
    // their events; the ladder sees the top of the list and proposes nothing.
    let adopted = run(&core).await.unwrap().expect("spread grows");
    let live = core.store.live_generation().await.unwrap().unwrap();
    assert_eq!(live.id, adopted);
    assert_eq!(live.params.spread_max, 5);
    assert!(live.run_id.is_none(), "a lived adoption names no run");
    assert!(live.predicted.is_some());
}
```

Build the fixture by recording events with `NewEvent` literals (three ranked shown, two band) and opening through the store, so the observations carry `event_id`; the module's `observe` helper writes none, so this fixture writes through `mark_opened` (or whatever `feedback.rs` names the open path).

- [ ] **Step 7: Run the gate**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked 2>&1 | tail -5`
Expected: green.

- [ ] **Step 8: Commit**

```bash
git add -A src
git commit -m "feat(evolve): the band is captured, and spread moves on what the band earned"
```

---

### Task 7: The words catch up

**Files:**
- Modify: `config.example.toml:487-491` (`auto_supersede` comment), `:597-608` (`spread_max` / `prime_lift` comments), `:894-959` (`[evolve]` names seven knobs)
- Modify: `src/core/ranking.rs:1-13` (module doc: seven knobs, two kinds and a lived one)
- Modify: `src/jobs/tune.rs:1-28` (module doc: the pass spends inference in exactly one case)
- Modify: `docs/superpowers/plans/2026-09-05-self-tuning-handoff.md` ("What exists" table, the "Rerank on/off, `prime_lift` and spread are not tuned" bullet)
- Modify: `src/core/mod.rs:916` if it asserts the example's `[consolidate]` text

- [ ] **Step 1: Rewrite the comments**

`auto_supersede`: "the lane judged first; it hides nothing on its own — a pair above it goes to the judge before one below it, and the judge decides."

`spread_max` / `prime_lift`: "The file's starting rung. A base with `evolve.autonomous` on moves it from here on what use leaves behind — see `[evolve]` — and the database holds what is live." Drop the sentence about the cargo harness.

`[evolve]`: the sentence that names the four knobs names seven, and adds: "The pass spends inference in one case: a reranker is configured, the live generation runs without it, and the pass asks what one call per observation would have changed."

`tune.rs` module doc: replace "The pass spends no inference ... the reranker is left out, so it calls nothing" with the same sentence.

- [ ] **Step 2: Update the handoff**

In "What exists", the knobs row reads "Seven knobs on `RankingParams`: four from 2b, prime lift and rerank on the ladder, spread on a lived rule (3a)". Replace the "not tuned" bullet with: "**Rerank, `prime_lift` and spread are tuned as of 3a**; the flip is asked after the ladder, the band is not replayed, and priming is captured at every door that primes — see the 3a plan's 'What this plan admits'." Add `search_context` and `search_candidates.band` and `observations.event_id` to the table. Update the "Plans, in order" list: 3a built, 3b next.

- [ ] **Step 3: Run the gate**

Run: `cargo fmt --all --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked 2>&1 | tail -5`
Expected: green.

- [ ] **Step 4: Commit**

```bash
git add -A config.example.toml src docs
git commit -m "docs(evolve): seven knobs, what the pass spends on the flip, and the 3a admissions"
```

---

## Self-review against the spec

- **A1 rerank.** Knob on `RankingParams` (T1); serving reads it beside the request flag and the scope (T2); served rank as base, replay as candidate, one call per pair only when live is off, only where `reranks_search` (T5); watched by `lived` — no change needed, the adoption path is the ladder's (T5). "Joins the winner choice" read as "asked after the ladder" — admitted above.
- **A2 prime lift.** Knob and ladder (T1, T4); `search_context` written beside the pool at every priming door, whatever the lift (T3); `observations.event_id` on opened and gave-up (T3); replay hands the snapshot in on the Judge door, a pair without one ties (T4). Cost: one `engagement_now` per priming search at lift zero (T3).
- **A3 spread.** Knob and ladder (T1); serving reads it (T2); band captured with `band = 1` after the ranked pool, an open on it is an observation (T6); lived rule with the two-net shape (T6); asked only when the ladder and the flip propose nothing (T6); journaled as a generation with parent and `predicted`, run NULL, and Insights names it (T6); first rung from zero offered once, tried memory holds it (T6). Band not replayed — admitted above.
- **Ladder after Part A.** Six axes counterfactual (five on the ladder plus the flip), spread lived; `BUDGET` 19; `moved` counts seven (T1, T4).
- **Configuration.** No new keys; serde defaults equal the shipped values (T1); the two stale comments corrected (T7).
- **Error handling.** A failed rerank call inside a replay: `rank_of` goes through `search_inner`, which already degrades a failed rerank to vector order; the "on" row for that pair is then the vector rank — a miss relative to the served rank only if the served rank was better, which is the honest outcome. Missing `search_context` → `None` → tie (T4). Nothing here touches the corpus.

Types across tasks: `Priming` (T3) used by `Origin.replay` and `Pair.priming` (T4); `Pair.served` (T5) set in `observation_pairs` (T5) beside `priming` (T4); `BandUse` (T6) consumed by `next_spread` (T6); `adopt_generation_lived` (T6) distinct from `adopt_generation` (existing). `rank_of`'s fourth argument stays `rerank: bool` throughout.
