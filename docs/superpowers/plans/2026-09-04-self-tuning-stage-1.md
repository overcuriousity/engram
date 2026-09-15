# Self-tuning, stage 1: generations, the journal, and observation collection

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Name the running configuration a *generation*, and start recording what use leaves behind as *observations* — with nothing yet reading them on any path a person waits on.

**Architecture:** Two new tables and no changes to existing ones. A generation is an immutable row holding the retrieval and ranking parameters plus the identity of the models that computed under them; exactly one is live. An observation is an immutable row saying that one artifact mattered, or did not, for a query that was really asked, under a named generation. Observations are written at the four moments the evidence already exists — a cited excerpt, an opened result, an unsupported literal, a search someone gave up on — and read by nothing until the last task wires them into the existing sweep behind a key that ships off.

**Tech Stack:** Rust 2024 edition, sqlx 0.9 over SQLite, tokio. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-04-self-tuning-design.md` — read it before task 1. This plan implements its **Stage 1** only.

## Global Constraints

- **Rust 1.94** is the floor. CI has an MSRV job pinned to it; do not use anything newer.
- `cargo fmt --all --check` and `cargo clippy --all-targets --locked -- -D warnings` must pass. Clippy warnings are errors for our own code.
- **Every test must run with no infrastructure.** No Qdrant, no model endpoint. `Store::memory()` and the deterministic fakes are what the suite uses; anything needing a service is `#[ignore]`d and does not count as a test here.
- **Schema changes are declarative.** `src/store/schema.sql` is one statement of what the schema *is*, applied by `Store::migrate()` on every connect, every object `IF NOT EXISTS`. Adding a *column* to an existing table means recreating the database — so **this stage adds only new tables and touches no existing one.**
- **Nothing is deleted.** An observation that stops being usable is stamped `excluded_at`; no row is ever removed by this code.
- **No behaviour change in what is returned.** Nothing in this stage may alter what a search or an ask returns, or the order it returns it in. Two paths gain one INSERT each — opening a result (task 6) and recording an ask (tasks 5 and 7) — and both must be inside the statement's existing transaction, so a stamped open can never lack its observation. The one place observations reach the existing tuning loop is task 10, behind a config key that ships `false`.
- **Every new config key goes into `config.example.toml`** with the reasoning behind its default, in the voice of the keys around it.
- **Test names are sentences.** `an_unused_citation_leaves_no_observation`, not `test_observation_2`. Match the file you are editing.
- Commit after every task. Commit subjects are lowercase sentences in the repo's style: `feat(evolve): ...`, `test(evolve): ...`.

---

### Task 1: The `generations` table and its store

**Files:**
- Create: `src/store/generations.rs`
- Modify: `src/store/schema.sql` (append a new section at the end)
- Modify: `src/store/mod.rs` (add `pub mod generations;` beside the other store modules)

**Interfaces:**
- Consumes: `Store`, `new_id()`, `now()` from `super` — the pattern every other store module uses.
- Produces:
  - `pub struct GenerationParams { pub recency_weight: f32, pub per_source_cap: Option<usize> }` — serde-serializable
  - `pub struct NewGeneration { pub params: GenerationParams, pub embed_recipe: String, pub chat_model: String, pub parent_id: Option<String> }`
  - `pub struct Generation { pub id: String, pub created_at: i64, pub params: GenerationParams, pub embed_recipe: String, pub chat_model: String, pub parent_id: Option<String> }`
  - `Store::record_generation(&self, g: &NewGeneration) -> Result<String>` — inserts as `live` and supersedes whatever was live
  - `Store::live_generation(&self) -> Result<Option<Generation>>`

- [ ] **Step 1: Add the tables to the schema**

Append to the end of `src/store/schema.sql`:

```sql
-- One named, immutable bundle of everything that decides what is retrieved and
-- in what order, together with the identity of the models that computed under
-- it. Exactly one row is `live`.
--
-- `params` is JSON rather than a column per knob because stage 2 widens the set
-- it holds, and a column per knob would make every widening a recreated
-- database — which is the price this schema's doctrine charges for an altered
-- column, and not one a tuning knob should cost.
--
-- The models are named because they are inside the measurement. Change the ask
-- model and every citation-derived number shifts underneath the evidence; a
-- generation that does not say who computed under it is a row of numbers
-- nothing can be compared to.
CREATE TABLE IF NOT EXISTS generations (
  id            TEXT PRIMARY KEY,
  created_at    INTEGER NOT NULL,
  params        TEXT NOT NULL,
  embed_recipe  TEXT NOT NULL,
  chat_model    TEXT NOT NULL,
  -- NULL for the generation a base starts with.
  parent_id     TEXT,
  -- Stage 2 fills these. Stage 1 writes NULL: nothing proposes anything yet.
  run_id        TEXT,
  predicted     REAL,
  -- `live` | `superseded`. Stage 2 adds `proposed` and `reverted`.
  state         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_generations_live ON generations(state, created_at DESC);
```

- [ ] **Step 2: Write the failing tests**

Create `src/store/generations.rs` with only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> NewGeneration {
        NewGeneration {
            params: GenerationParams {
                recency_weight: 0.05,
                per_source_cap: Some(3),
            },
            embed_recipe: "embeddinggemma:768:asym".into(),
            chat_model: "qwen".into(),
            parent_id: None,
        }
    }

    #[tokio::test]
    async fn a_fresh_base_has_no_generation() {
        let store = Store::memory().await.unwrap();
        assert!(store.live_generation().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_generation_recorded_last_is_the_only_live_one() {
        let store = Store::memory().await.unwrap();
        let first = store.record_generation(&sample()).await.unwrap();

        let mut second = sample();
        second.parent_id = Some(first.clone());
        second.params.recency_weight = 0.1;
        let second_id = store.record_generation(&second).await.unwrap();

        let live = store.live_generation().await.unwrap().expect("one is live");
        assert_eq!(live.id, second_id);
        assert_eq!(live.parent_id.as_deref(), Some(first.as_str()));
        assert_eq!(live.params.recency_weight, 0.1);
    }

    #[tokio::test]
    async fn a_superseded_generation_is_kept_rather_than_replaced() {
        // The journal is the whole point: a parameter that moved has to have
        // something to have moved *from*, months later.
        let store = Store::memory().await.unwrap();
        store.record_generation(&sample()).await.unwrap();
        store.record_generation(&sample()).await.unwrap();

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM generations")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(n, 2, "the earlier generation must survive its supersession");
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib store::generations`
Expected: FAIL — `cannot find type NewGeneration in this scope`, and the rest.

- [ ] **Step 4: Write the implementation**

Above the test module in `src/store/generations.rs`:

```rust
//! The named, versioned settings a base is currently retrieving under.
//!
//! A number that moved has to have something to have moved from. This is that
//! something: one immutable row per set of parameters the base has run under,
//! with the models that computed alongside them, and exactly one of them live.

use super::{Store, new_id, now};
use crate::error::Result;
use sqlx::Row;

/// The knobs a generation holds. Stage 1 carries the two the runtime sweep
/// already moves; stage 2 widens this and nothing else has to change, because
/// it is stored as JSON.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GenerationParams {
    pub recency_weight: f32,
    pub per_source_cap: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct NewGeneration {
    pub params: GenerationParams,
    pub embed_recipe: String,
    pub chat_model: String,
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Generation {
    pub id: String,
    pub created_at: i64,
    pub params: GenerationParams,
    pub embed_recipe: String,
    pub chat_model: String,
    pub parent_id: Option<String>,
}

impl Store {
    /// Record a generation and make it the live one.
    ///
    /// One transaction: a base with two live generations is a base whose
    /// searches cannot say which settings produced them.
    pub async fn record_generation(&self, g: &NewGeneration) -> Result<String> {
        let id = new_id();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("UPDATE generations SET state = 'superseded' WHERE state = 'live'")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO generations
               (id, created_at, params, embed_recipe, chat_model, parent_id,
                run_id, predicted, state)
             VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, 'live')",
        )
        .bind(&id)
        .bind(now())
        .bind(json(&g.params)?)
        .bind(&g.embed_recipe)
        .bind(&g.chat_model)
        .bind(&g.parent_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    pub async fn live_generation(&self) -> Result<Option<Generation>> {
        let row = sqlx::query(
            "SELECT id, created_at, params, embed_recipe, chat_model, parent_id
               FROM generations WHERE state = 'live'
              ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            Ok(Generation {
                id: r.get("id"),
                created_at: r.get("created_at"),
                params: from_json(&r.get::<String, _>("params"))?,
                embed_recipe: r.get("embed_recipe"),
                chat_model: r.get("chat_model"),
                parent_id: r.get("parent_id"),
            })
        })
        .transpose()
    }
}

// `Error` has no `From<serde_json::Error>`, so both directions map explicitly.
// This is the shape `src/store/eval_runs.rs` already uses; copy it rather than
// adding a blanket conversion, which would swallow the context in every other
// store module too.
fn json<T: serde::Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| crate::error::Error::Store(format!("generations: {e}")))
}

fn from_json<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(|e| crate::error::Error::Store(format!("generations: {e}")))
}
```

Add `pub mod generations;` to `src/store/mod.rs` beside the other store modules, in alphabetical position.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib store::generations`
Expected: PASS, 3 tests.

- [ ] **Step 6: Check formatting and lints**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add src/store/generations.rs src/store/schema.sql src/store/mod.rs
git commit -m "feat(evolve): a base can say which settings it is retrieving under"
```

---

### Task 2: The live generation is established at boot

**Files:**
- Modify: `src/tenants.rs` — where a tenant's `Store` is opened and `migrate()` runs
- Test: in `src/tenants.rs`'s test module, or `src/store/generations.rs` if the wiring lands there

**Interfaces:**
- Consumes: `Store::live_generation`, `Store::record_generation`, `GenerationParams`, `NewGeneration` from task 1.
- Produces: `pub async fn ensure_generation(store: &Store, params: GenerationParams, embed_recipe: &str, chat_model: &str) -> Result<Generation>` in `src/store/generations.rs`. Called once per tenant open, after `migrate()`.

The rule it implements: a base with no generation gets one from the running configuration. A base whose live generation names different models gets a **new** generation with the same parameters and the new models — a new era, because evidence gathered under other models is not evidence about these.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `src/store/generations.rs`:

```rust
#[tokio::test]
async fn a_base_with_no_generation_gets_one_from_the_running_config() {
    let store = Store::memory().await.unwrap();
    let params = GenerationParams { recency_weight: 0.05, per_source_cap: Some(3) };
    let g = ensure_generation(&store, params, "recipe-a", "qwen").await.unwrap();
    assert_eq!(g.params, params);
    assert!(g.parent_id.is_none(), "the first generation has no parent");
}

#[tokio::test]
async fn a_second_boot_under_the_same_models_reuses_the_generation() {
    let store = Store::memory().await.unwrap();
    let params = GenerationParams { recency_weight: 0.05, per_source_cap: Some(3) };
    let first = ensure_generation(&store, params, "recipe-a", "qwen").await.unwrap();
    let again = ensure_generation(&store, params, "recipe-a", "qwen").await.unwrap();
    assert_eq!(first.id, again.id, "an unchanged boot must not mint a generation");
}

#[tokio::test]
async fn a_changed_chat_model_starts_a_new_era() {
    // Every citation-derived number shifts when the generator changes. Carrying
    // on under the same generation would compare two things that were never
    // measured the same way.
    let store = Store::memory().await.unwrap();
    let params = GenerationParams { recency_weight: 0.05, per_source_cap: Some(3) };
    let first = ensure_generation(&store, params, "recipe-a", "qwen").await.unwrap();
    let second = ensure_generation(&store, params, "recipe-a", "llama").await.unwrap();

    assert_ne!(first.id, second.id);
    assert_eq!(second.parent_id.as_deref(), Some(first.id.as_str()));
    assert_eq!(second.params, params, "a model change moves no knob");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib store::generations`
Expected: FAIL — `cannot find function ensure_generation`.

- [ ] **Step 3: Write the implementation**

Add to `src/store/generations.rs`:

```rust
/// The live generation for the running configuration, minting one where the
/// base has none or where the models have changed under it.
///
/// A model change mints a generation rather than editing the live one: a
/// generation is immutable, and the point of the row is that observations
/// collected under it name something that still says what it said.
pub async fn ensure_generation(
    store: &Store,
    params: GenerationParams,
    embed_recipe: &str,
    chat_model: &str,
) -> Result<Generation> {
    if let Some(live) = store.live_generation().await?
        && live.embed_recipe == embed_recipe
        && live.chat_model == chat_model
    {
        return Ok(live);
    }
    let parent_id = store.live_generation().await?.map(|g| g.id);
    if parent_id.is_some() {
        tracing::info!(
            embed_recipe,
            chat_model,
            "models changed; observations before this belong to another era"
        );
    }
    let id = store
        .record_generation(&NewGeneration {
            params,
            embed_recipe: embed_recipe.to_string(),
            chat_model: chat_model.to_string(),
            parent_id,
        })
        .await?;
    store
        .live_generation()
        .await?
        .filter(|g| g.id == id)
        .ok_or_else(|| crate::error::Error::Internal("generation was not made live".into()))
}
```

Call it from `src/tenants.rs` where the tenant's `Store` is opened, immediately after `migrate()`, passing the ranking parameters the tenant already resolves and the two model identities from `Config::infer`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib store::generations && cargo test --lib tenants`
Expected: PASS.

- [ ] **Step 5: Run the whole suite — nothing else may move**

Run: `cargo test --locked`
Expected: PASS, no new failures. This task adds a write on tenant open; if any test asserts on the number of statements or rows at boot, it has to be updated deliberately, not silenced.

- [ ] **Step 6: Commit**

```bash
git add src/store/generations.rs src/tenants.rs
git commit -m "feat(evolve): a base names its generation at boot, and a changed model starts an era"
```

---

### Task 3: `--print-config` names the live generation

**Files:**
- Modify: `src/store/generations.rs` — the function and its tests live beside the type they render
- Modify: `src/main.rs` — call it from `--print-config`

**Interfaces:**
- Consumes: `Generation` from task 1.
- Produces: `pub fn render_generation_line(g: Option<&Generation>) -> String` in `src/store/generations.rs`.

The spec's disclosure promise: `config.toml` holds the envelope and the starting point, the database holds what is live. An operator reading only the file would otherwise be reading something that no longer describes what is running. `--print-config` already prints `learn.mode` first and then the keys the mode decided; this is the same disclosure one layer out.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn print_config_names_the_live_generation_and_its_parameters() {
    let store = Store::memory().await.unwrap();
    let params = GenerationParams { recency_weight: 0.1, per_source_cap: None };
    let g = ensure_generation(&store, params, "recipe-a", "qwen").await.unwrap();

    let rendered = render_generation_line(Some(&g));
    assert!(rendered.contains(&g.id), "{rendered}");
    assert!(rendered.contains("recency_weight = 0.1"), "{rendered}");
    assert!(rendered.contains("per_source_cap = none"), "{rendered}");
}

#[test]
fn print_config_says_so_plainly_when_no_generation_is_live() {
    let rendered = render_generation_line(None);
    assert!(rendered.contains("no generation"), "{rendered}");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib render_generation_line`
Expected: FAIL — `cannot find function render_generation_line`.

- [ ] **Step 3: Implement**

```rust
/// What `--print-config` says about the live generation.
///
/// The file below this line is the envelope and the starting point; this line
/// is what is actually in force. Printed together so neither is read as the
/// other.
pub fn render_generation_line(g: Option<&Generation>) -> String {
    match g {
        None => "# live generation: none — the file below is in force as written".to_string(),
        Some(g) => format!(
            "# live generation: {} (recency_weight = {}, per_source_cap = {})",
            g.id,
            g.params.recency_weight,
            match g.params.per_source_cap {
                Some(n) => n.to_string(),
                None => "none".to_string(),
            }
        ),
    }
}
```

Print it at the top of `--print-config`'s output, before the resolved TOML.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib render_generation_line`
Expected: PASS, 2 tests.

- [ ] **Step 5: Commit**

```bash
git add src/store/generations.rs src/main.rs
git commit -m "feat(evolve): --print-config says which generation is in force"
```

---

### Task 4: The `observations` table and its store

**Files:**
- Create: `src/store/observations.rs`
- Modify: `src/store/schema.sql`
- Modify: `src/store/mod.rs`

**Interfaces:**
- Consumes: `Store`, `new_id()`, `now()`.
- Produces:
  - `pub enum Source { Cited, Opened, Unsupported, GaveUp }` with `as_str()`/`from_str`
  - `pub struct NewObservation { pub generation_id: String, pub query: String, pub query_vec: Vec<f32>, pub embed_model: String, pub artifact_id: Option<String>, pub rank: Option<i64>, pub source: Source }`
  - `pub struct Observation { pub id: String, pub created_at: i64, pub generation_id: String, pub query: String, pub query_vec: Vec<f32>, pub artifact_id: Option<String>, pub rank: Option<i64>, pub source: Source, pub strength: f32 }`
  - `Store::record_observation(&self, o: &NewObservation) -> Result<String>`
  - `Store::observations_for_generation(&self, generation_id: &str, limit: usize) -> Result<Vec<Observation>>` — excludes rows with `excluded_at` set

Strength is derived from the source rather than passed in, so no caller can invent a weight: `Cited` and `Opened` are `1.0`, `Unsupported` is `-1.0`, `GaveUp` is `-0.25`.

- [ ] **Step 1: Add the table to the schema**

Append to `src/store/schema.sql`:

```sql
-- What use left behind: one statement that a particular artifact mattered, or
-- did not, for a query somebody really asked, under a named generation.
--
-- Never updated except to exclude. An observation is a fact about a moment, and
-- a moment does not change its mind.
CREATE TABLE IF NOT EXISTS observations (
  id            TEXT PRIMARY KEY,
  created_at    INTEGER NOT NULL,
  generation_id TEXT NOT NULL REFERENCES generations(id),
  -- The query as asked, and its vector, so replaying it costs no embedding.
  query         TEXT NOT NULL,
  query_vec     BLOB NOT NULL,
  vec_dim       INTEGER NOT NULL,
  embed_model   TEXT NOT NULL,
  -- What it is about. NULL where the observation is about the retrieval as a
  -- whole rather than one artifact: an unsupported literal says the set failed
  -- to carry the answer and names nothing inside it.
  artifact_id   TEXT,
  -- Where the artifact stood, 1-based, in the list this is about. NULL with a
  -- NULL artifact.
  rank          INTEGER,
  -- `cited` | `opened` | `unsupported` | `gave_up`
  source        TEXT NOT NULL,
  -- Positive above zero, negative below. A weight class, not a tuned number:
  -- strong is 1.0, the give-up is 0.25, and the asymmetry that a weak negative
  -- may revert but never adopt is enforced where they are read, not here.
  strength      REAL NOT NULL,
  -- Set when the artifact this names has gone. An excluded observation is not
  -- scored as a miss: a miss is a claim about ordering, and this is not one.
  excluded_at   INTEGER
);
CREATE INDEX IF NOT EXISTS idx_observations_generation
  ON observations(generation_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_observations_artifact
  ON observations(artifact_id) WHERE artifact_id IS NOT NULL;
```

- [ ] **Step 2: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::generations::{GenerationParams, NewGeneration};

    async fn base() -> (Store, String) {
        let store = Store::memory().await.unwrap();
        let gen = store
            .record_generation(&NewGeneration {
                params: GenerationParams { recency_weight: 0.05, per_source_cap: Some(3) },
                embed_recipe: "recipe-a".into(),
                chat_model: "qwen".into(),
                parent_id: None,
            })
            .await
            .unwrap();
        (store, gen)
    }

    fn obs(gen: &str, artifact: Option<&str>, rank: Option<i64>, source: Source) -> NewObservation {
        NewObservation {
            generation_id: gen.to_string(),
            query: "how did I mount it".into(),
            query_vec: vec![0.1, 0.2, 0.3],
            embed_model: "embeddinggemma".into(),
            artifact_id: artifact.map(str::to_string),
            rank,
            source,
        }
    }

    #[tokio::test]
    async fn an_observation_keeps_its_query_vector_so_a_replay_costs_no_embedding() {
        let (store, gen) = base().await;
        store
            .record_observation(&obs(&gen, Some("art-1"), Some(2), Source::Cited))
            .await
            .unwrap();

        let back = store.observations_for_generation(&gen, 10).await.unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].query_vec, vec![0.1, 0.2, 0.3]);
        assert_eq!(back[0].rank, Some(2));
    }

    #[tokio::test]
    async fn the_source_decides_the_strength_and_the_caller_cannot() {
        let (store, gen) = base().await;
        for (source, want) in [
            (Source::Cited, 1.0),
            (Source::Opened, 1.0),
            (Source::Unsupported, -1.0),
            (Source::GaveUp, -0.25),
        ] {
            store.record_observation(&obs(&gen, Some("art-1"), Some(1), source)).await.unwrap();
            let back = store.observations_for_generation(&gen, 1).await.unwrap();
            assert_eq!(back[0].strength, want, "{source:?}");
        }
    }

    #[tokio::test]
    async fn an_observation_about_the_whole_retrieval_names_no_artifact() {
        let (store, gen) = base().await;
        store
            .record_observation(&obs(&gen, None, None, Source::Unsupported))
            .await
            .unwrap();
        let back = store.observations_for_generation(&gen, 10).await.unwrap();
        assert_eq!(back[0].artifact_id, None);
        assert_eq!(back[0].rank, None);
    }
}
```

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test --lib store::observations`
Expected: FAIL — the types do not exist.

- [ ] **Step 4: Implement**

Write `src/store/observations.rs` with the module doc, the `Source` enum with `strength()` returning the four constants above, `NewObservation`, `Observation`, and the two `Store` methods. Store `query_vec` as a `BLOB` of little-endian `f32`s with `vec_dim` beside it — copy the encoding `search_events.query_vec` already uses in `src/store/feedback.rs` rather than inventing a second one. `observations_for_generation` filters `excluded_at IS NULL` and orders `created_at DESC, id DESC`.

Add `pub mod observations;` to `src/store/mod.rs`.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test --lib store::observations`
Expected: PASS, 3 tests.

- [ ] **Step 6: Commit**

```bash
git add src/store/observations.rs src/store/schema.sql src/store/mod.rs
git commit -m "feat(evolve): what use leaves behind has somewhere to go"
```

---

### Task 5: A cited excerpt becomes an observation

**Files:**
- Modify: `src/store/asks.rs` — inside `record_ask`, after the citations are written
- Test: `src/store/asks.rs` test module

**Interfaces:**
- Consumes: `Store::record_observation`, `Source::Cited`, `Store::live_generation`.
- Produces: no new public signature. `record_ask` gains the side effect, inside its existing transaction.

`NewAskCitation.used` is already computed by `check::referenced` and already persisted. This is the densest positive signal in the system and today nothing reads it for tuning.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_used_citation_becomes_an_observation_at_the_rank_it_was_shown() {
    let (store, gen) = ask_base().await;
    store.record_ask(&ask_with(vec![
        NewAskCitation { artifact_id: "art-1".into(), score: 0.9, used: false },
        NewAskCitation { artifact_id: "art-2".into(), score: 0.8, used: true },
    ])).await.unwrap();

    let obs = store.observations_for_generation(&gen, 10).await.unwrap();
    assert_eq!(obs.len(), 1, "only the used citation is an observation");
    assert_eq!(obs[0].artifact_id.as_deref(), Some("art-2"));
    assert_eq!(obs[0].rank, Some(2), "the [n] it was shown as");
    assert_eq!(obs[0].source, Source::Cited);
}

#[tokio::test]
async fn an_abstention_leaves_no_observation_however_much_it_was_shown() {
    // Being packed into the prompt is not engagement. An abstention references
    // nothing however many excerpts it was given.
    let (store, gen) = ask_base().await;
    let mut a = ask_with(vec![
        NewAskCitation { artifact_id: "art-1".into(), score: 0.9, used: false },
    ]);
    a.abstained = true;
    store.record_ask(&a).await.unwrap();

    assert!(store.observations_for_generation(&gen, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn an_ask_recorded_before_any_generation_exists_writes_no_observation() {
    // Ordering safety: nothing may fail because the boot path has not run yet.
    let store = Store::memory().await.unwrap();
    store.record_ask(&ask_with(vec![
        NewAskCitation { artifact_id: "art-1".into(), score: 0.9, used: true },
    ])).await.unwrap();
    // No panic, no error, and no orphan row.
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM observations")
        .fetch_one(&store.pool).await.unwrap();
    assert_eq!(n, 0);
}
```

Write the `ask_base()` and `ask_with()` helpers in the same module, following the shape of the existing `sample()` helpers in `src/store/eval_runs.rs`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib store::asks`
Expected: FAIL — no observations are written.

- [ ] **Step 3: Implement**

Inside `record_ask`, after the citation rows are inserted and inside the same transaction: read the live generation once; if there is none, write nothing and return as before. For each citation with `used = true`, insert an observation with `source = 'cited'`, `rank = n`, and the ask's `question`, `query_vec` and `embed_model`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib store::asks`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/store/asks.rs
git commit -m "feat(evolve): an excerpt the answer actually used says so"
```

---

### Task 6: An opened result becomes an observation

**Files:**
- Modify: `src/store/feedback.rs` — inside `open_event`
- Test: `src/store/feedback.rs` test module

**Interfaces:**
- Consumes: `Store::record_observation`, `Source::Opened`.
- Produces: no signature change. `open_event` keeps returning `bool`.

`open_event` already proves the artifact was in the pool before it stamps anything, and `search_candidates.rank` already holds the position. Both facts the observation needs are there.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn an_opened_result_is_an_observation_at_the_rank_it_was_listed() {
    let (store, gen) = feedback_base().await;
    let event = store.record_search(event_with(&["art-1", "art-2", "art-3"]), 5).await.unwrap();

    assert!(store.open_event(&event, "art-2").await.unwrap());

    let obs = store.observations_for_generation(&gen, 10).await.unwrap();
    assert_eq!(obs.len(), 1);
    assert_eq!(obs[0].artifact_id.as_deref(), Some("art-2"));
    assert_eq!(obs[0].rank, Some(2));
    assert_eq!(obs[0].source, Source::Opened);
}

#[tokio::test]
async fn opening_an_artifact_the_search_never_listed_writes_nothing() {
    // open_event already refuses this; the observation must not outlive the
    // refusal, or a Yes would be recorded against a pool that never held it.
    let (store, gen) = feedback_base().await;
    let event = store.record_search(event_with(&["art-1"]), 5).await.unwrap();

    assert!(!store.open_event(&event, "art-9").await.unwrap());
    assert!(store.observations_for_generation(&gen, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn opening_the_same_result_twice_leaves_one_observation() {
    let (store, gen) = feedback_base().await;
    let event = store.record_search(event_with(&["art-1"]), 5).await.unwrap();
    store.open_event(&event, "art-1").await.unwrap();
    store.open_event(&event, "art-1").await.unwrap();
    assert_eq!(store.observations_for_generation(&gen, 10).await.unwrap().len(), 1);
}
```

`feedback_base()` returns a `Store` with one generation already live, the way
task 4's `base()` does. `event_with(&["art-1", ...])` builds a `NewEvent` whose
candidate list holds those artifacts at ranks 1, 2, 3 — copy the existing
`NewEvent` construction in this file's test module rather than writing a second
shape of one.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib store::feedback`
Expected: FAIL.

- [ ] **Step 3: Implement**

In `open_event`, write the observation only on the branch where `rows_affected() == 1` — which is exactly the branch that already means "this artifact was in this search's pool and nobody had spoken for it". Read the rank with the same `event_id`/`artifact_id` pair. Read the query, vector and model off the event row.

The third test passes for free: the second `open_event` affects zero rows because `opened_at` is already set — but assert it, because it is the property that stops a double-click becoming double evidence.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib store::feedback`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/store/feedback.rs
git commit -m "feat(evolve): a result that was opened says where it stood"
```

---

### Task 7: An unsupported literal becomes a negative observation

**Files:**
- Modify: `src/store/asks.rs` — the same `record_ask` path as task 5
- Modify: `src/core/ask/mod.rs` — pass the count of unsupported literals into `NewAsk`
- Test: `src/store/asks.rs`

**Interfaces:**
- Consumes: `Source::Unsupported`.
- Produces: `NewAsk` gains `pub unsupported: usize`. Every construction site of `NewAsk` must set it; `src/core/ask/mod.rs` already computes the list.

An answer that asserts a command or path no excerpt supports is retrieval having failed to supply what the answer needed. It names no artifact, because the claim is about the set.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn an_answer_asserting_what_no_excerpt_supports_is_a_negative_observation() {
    let (store, gen) = ask_base().await;
    let mut a = ask_with(vec![
        NewAskCitation { artifact_id: "art-1".into(), score: 0.9, used: true },
    ]);
    a.unsupported = 2;
    store.record_ask(&a).await.unwrap();

    let obs = store.observations_for_generation(&gen, 10).await.unwrap();
    let negative: Vec<_> = obs.iter().filter(|o| o.source == Source::Unsupported).collect();
    assert_eq!(negative.len(), 1, "one observation per answer, not one per literal");
    assert_eq!(negative[0].artifact_id, None, "the claim is about the set");
    assert!(negative[0].strength < 0.0);
}

#[tokio::test]
async fn an_answer_whose_literals_were_all_supported_writes_no_negative() {
    let (store, gen) = ask_base().await;
    let mut a = ask_with(vec![
        NewAskCitation { artifact_id: "art-1".into(), score: 0.9, used: true },
    ]);
    a.unsupported = 0;
    store.record_ask(&a).await.unwrap();

    let obs = store.observations_for_generation(&gen, 10).await.unwrap();
    assert!(obs.iter().all(|o| o.source != Source::Unsupported));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib store::asks`
Expected: FAIL — `NewAsk` has no field `unsupported`.

- [ ] **Step 3: Implement**

Add `pub unsupported: usize` to `NewAsk`. In `src/core/ask/mod.rs`, set it from the length of the list `check::unsupported_literals` already returns — the value is computed and used for the badge today, so this is reading a local, not calling anything new. In `record_ask`, write one observation with `source = 'unsupported'` when the count is above zero.

One per answer, not one per literal: three unsupported literals in one answer is one retrieval that fell short, not three.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib store::asks && cargo test --lib core::ask`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/store/asks.rs src/core/ask/mod.rs
git commit -m "feat(evolve): an answer the excerpts could not carry is evidence about the search"
```

---

### Task 8: The give-up chain

**Files:**
- Create: `src/jobs/observe.rs`
- Modify: `src/jobs/mod.rs` (register the job), `src/config.rs` (the window key), `config.example.toml`
- Test: `src/jobs/observe.rs`

**Interfaces:**
- Consumes: `Store::record_observation`, `Source::GaveUp`, `search_events`.
- Produces: `pub async fn run(core: &Core) -> Result<usize>` — returns how many observations it wrote. Idempotent: a chain already observed is not observed twice.

The rule, and every clause of it earns its place:

> A search event with `opened_at IS NULL` **and** `judged_at IS NULL`, from a scope that made **another search** within `give_up_window_secs`, is a weak negative.

`fold_onto` is **not** this signal and must not be used as one. It coalesces a typing burst into one event and overwrites the intermediate wordings on purpose. What it buys here is that consecutive stored events are already distinct search acts rather than keystrokes.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_search_nobody_opened_and_then_searched_past_is_a_weak_negative() {
    let (core, gen) = job_base().await;
    let first = record_at(&core, "loop device", 1_000).await;
    record_at(&core, "mount loop image", 1_060).await;

    assert_eq!(run(&core).await.unwrap(), 1);
    let obs = core.store.observations_for_generation(&gen, 10).await.unwrap();
    assert_eq!(obs[0].source, Source::GaveUp);
    assert_eq!(obs[0].query, "loop device");
    assert!(obs[0].strength < 0.0 && obs[0].strength > -1.0, "weak, not strong");
    let _ = first;
}

#[tokio::test]
async fn a_search_whose_result_was_opened_is_never_a_give_up() {
    let (core, gen) = job_base().await;
    let first = record_at(&core, "loop device", 1_000).await;
    core.store.open_event(&first, "art-1").await.unwrap();
    record_at(&core, "mount loop image", 1_060).await;

    run(&core).await.unwrap();
    let obs = core.store.observations_for_generation(&gen, 10).await.unwrap();
    assert!(obs.iter().all(|o| o.source != Source::GaveUp));
}

#[tokio::test]
async fn a_search_an_hour_later_is_a_new_question_and_not_a_give_up() {
    // Search, read something, leave the page open, come back to something else
    // entirely. The window is what stops that being scored as a failure.
    let (core, gen) = job_base().await;
    record_at(&core, "loop device", 1_000).await;
    record_at(&core, "invoice due date", 1_000 + 3_600).await;

    assert_eq!(run(&core).await.unwrap(), 0);
    assert!(core.store.observations_for_generation(&gen, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_last_search_of_a_chain_is_not_a_give_up_because_nothing_followed_it() {
    let (core, gen) = job_base().await;
    record_at(&core, "loop device", 1_000).await;
    record_at(&core, "mount loop image", 1_060).await;
    run(&core).await.unwrap();

    let obs = core.store.observations_for_generation(&gen, 10).await.unwrap();
    assert_eq!(obs.len(), 1, "only the abandoned one, never the one that ended it");
}

#[tokio::test]
async fn a_second_pass_writes_nothing_new() {
    let (core, gen) = job_base().await;
    record_at(&core, "loop device", 1_000).await;
    record_at(&core, "mount loop image", 1_060).await;
    run(&core).await.unwrap();
    assert_eq!(run(&core).await.unwrap(), 0);
    assert_eq!(core.store.observations_for_generation(&gen, 10).await.unwrap().len(), 1);
}
```

`record_at` inserts a search event with a controlled `created_at`; `job_base` builds a `Core` on `Store::memory()` with the deterministic fakes, following whatever the nearest existing job test module does.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib jobs::observe`
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Add the config key**

In `src/config.rs`, add to the new `EvolveConfig` section (created here, extended in task 10):

```rust
/// Recording what use leaves behind, and what is done with it.
#[derive(Debug, Deserialize, Clone)]
pub struct EvolveConfig {
    /// A search nobody opened, followed by another search from the same
    /// person inside this many seconds, is a weak negative.
    ///
    /// Minutes, and deliberately far above `feedback.coalesce_secs`: that one
    /// is a typing burst, this one is a person reading a list, thinking, and
    /// asking again. Too long and an unrelated question an hour later is
    /// scored as a failure of a search that worked.
    pub give_up_window_secs: i64,
}

impl Default for EvolveConfig {
    fn default() -> Self {
        Self { give_up_window_secs: 300 }
    }
}
```

Hang it on `Config` with `#[serde(default)]`, and document the key in `config.example.toml` under a new `[evolve]` section in the voice of the sections around it.

- [ ] **Step 4: Implement the job**

Write `src/jobs/observe.rs`. One query finds candidate events: `opened_at IS NULL AND judged_at IS NULL`, with an `EXISTS` for a later event of the same `scope` within the window, and a `NOT EXISTS` against `observations` for the same query and source so a second pass writes nothing. Register it on the retention ticker beside `jobs::gaps`.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test --lib jobs::observe && cargo test --lib config`
Expected: PASS, 5 tests plus the config defaults.

- [ ] **Step 6: Commit**

```bash
git add src/jobs/observe.rs src/jobs/mod.rs src/config.rs config.example.toml
git commit -m "feat(evolve): a search that was given up on says so, quietly"
```

---

### Task 9 — dropped, because it already exists

The original task 9 was "follow `superseded_by`, exclude what is gone". Both
rules are already implemented on the path observations will flow down:
`pairs_to_replay` in `src/eval/sweep.rs` calls `get_artifact` and treats
`NotFound` as skipped rather than as a miss — "a deleted artifact is
housekeeping, not a ranking result" — and calls `crate::eval::satisfied_by`,
which is the supersede rule.

Writing a second implementation of both would be machinery for a rule the tree
already enforces. Stage 2 scores observations directly rather than through
pairs and will need the rule at that point; it reuses `satisfied_by` then.

The `excluded_at` column stays in the schema. It costs nothing, stage 2 uses
it, and a column added later is a recreated database.

### Task 10: The sweep may draw on observations, and ships not doing so

(Kept as task 10 so the commit trail matches the plan; task 9 above is a note, not work.)

**Files:**
- Modify: `src/eval/sweep.rs`
- Modify: `src/config.rs`, `config.example.toml`, `src/core/mod.rs` (the `Core` field and `test_support`)
- Test: `src/eval/sweep.rs`

**Interfaces:**
- Consumes: `Store::observations_for_generation`, `EvolveConfig`.
- Produces: `EvolveConfig` gains `pub feed_sweep: bool`, default `false`.

This is the whole payoff of stage 1, and it ships **off**. Widening the sweep's evidence base changes which parameters it recommends, and a recommendation changes ranking. The house rule is that a default which changes ranking moves only after the harness has been run, so the operator turns this on deliberately and can run the harness on either side of it.

Only **positive** observations may enter. A weak negative is not enough to move anything, by the asymmetry the design rests on: weaker evidence may stop a change and may never cause one. Stage 1 has no revert path, so weak negatives are recorded here and read by nothing.

- [ ] **Step 1: Put the config on `Core` and give the tests a way to vary it**

Add `pub evolve: crate::config::EvolveConfig` to `Core` (`src/core/mod.rs`),
populated where the other config sections are. Beside the existing `test_core()` in the
`pub mod test_support` block of `src/core/mod.rs` (line ~612), add:

```rust
/// `test_core()` with one section overridden. Stage 1 needs it because
/// `evolve.feed_sweep` is the difference between two of the tests below and
/// nothing mutates it at runtime.
pub async fn test_core_with_evolve(evolve: crate::config::EvolveConfig) -> Core {
    let mut core = test_core().await;
    core.evolve = evolve;
    core
}
```

Derive `Default` on `EvolveConfig` is already done in task 8; `feed_sweep`
joins it with `false`.

- [ ] **Step 2: Write the failing tests**

Write these helpers at the top of the test module — `src/eval/sweep.rs` cannot
see task 4's test helpers, so it needs its own:

```rust
/// A test `Core` with one live generation, and `feed_sweep` set as asked.
async fn sweep_base(feed_sweep: bool) -> (crate::core::Core, String) {
    let core = crate::core::test_support::test_core_with_evolve(
        crate::config::EvolveConfig { feed_sweep, ..Default::default() },
    )
    .await;
    let gen = core
        .store
        .record_generation(&NewGeneration {
            params: GenerationParams { recency_weight: 0.05, per_source_cap: Some(3) },
            embed_recipe: "recipe-a".into(),
            chat_model: "qwen".into(),
            parent_id: None,
        })
        .await
        .unwrap();
    (core, gen)
}

fn observation(gen: &str, artifact: &str, rank: i64, source: Source) -> NewObservation {
    NewObservation {
        generation_id: gen.to_string(),
        query: format!("query about {artifact}"),
        query_vec: vec![0.1, 0.2, 0.3],
        embed_model: "embeddinggemma".into(),
        artifact_id: Some(artifact.to_string()),
        rank: Some(rank),
        source,
    }
}

async fn seed_used(core: &Core, gen: &str, n: usize) {
    for i in 0..n {
        core.store
            .record_observation(&observation(gen, &format!("art-{i}"), 1, Source::Cited))
            .await
            .unwrap();
    }
}
```

```rust
#[tokio::test]
async fn the_sweep_ignores_observations_by_default() {
    let (core, gen) = sweep_base(false).await;
    seed_used(&core, &gen, 40).await;
    assert!(
        pairs_to_replay(&core).await.unwrap().0.is_empty(),
        "a shipped default must not change what is recommended"
    );
}

#[tokio::test]
async fn with_the_key_on_a_used_excerpt_is_a_pair_the_sweep_can_score() {
    let (core, gen) = sweep_base(true).await;
    seed_used(&core, &gen, 40).await;
    assert_eq!(pairs_to_replay(&core).await.unwrap().0.len(), 40);
}

#[tokio::test]
async fn a_weak_negative_is_never_a_pair() {
    let (core, gen) = sweep_base(true).await;
    core.store
        .record_observation(&observation(&gen, "art-1", 1, Source::GaveUp))
        .await
        .unwrap();
    assert!(
        pairs_to_replay(&core).await.unwrap().0.is_empty(),
        "weaker evidence may stop a change and may never cause one"
    );
}

#[tokio::test]
async fn observations_from_a_superseded_generation_are_not_evidence_about_this_one() {
    // Seed under the generation that is live now, then mint a new one — which
    // supersedes it. The sweep asks about the live generation, so what was
    // gathered under the old models must stop counting.
    let (core, first) = sweep_base(true).await;
    seed_used(&core, &first, 40).await;
    assert_eq!(pairs_to_replay(&core).await.unwrap().0.len(), 40, "live, so far");

    core.store
        .record_generation(&NewGeneration {
            params: GenerationParams { recency_weight: 0.05, per_source_cap: Some(3) },
            embed_recipe: "recipe-a".into(),
            chat_model: "a-different-model".into(),
            parent_id: Some(first),
        })
        .await
        .unwrap();

    assert!(
        pairs_to_replay(&core).await.unwrap().0.is_empty(),
        "a model change ends the era its evidence belonged to"
    );
}
```

`pairs_to_replay(core)` already exists in this file and is exactly the
pair-gathering step — no extraction is needed. It returns `(Vec<Pair>, i64)`,
where a `Pair` is `(query, satisfies)`. The tests call it directly and read
`.0.len()`; `sweep_pairs` in the snippets above is that call.

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test --lib eval::sweep`
Expected: FAIL.

- [ ] **Step 4: Implement**

Add `feed_sweep: bool` to `EvolveConfig`, defaulting to `false`, documented in `config.example.toml` with the reasoning above written out. In `sweep.rs`, where the judged pairs are gathered, append pairs derived from the **live generation's** positive observations when the key is on — a pair being `{query, expect}` exactly as the existing ones are, with `query_vec` reused so the pass still embeds nothing.

Leave the sweep's own discipline untouched: `Door::Judge`, `mark: false`, live index read-only.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test --lib eval::sweep`
Expected: PASS, 4 tests.

- [ ] **Step 6: Run the whole suite and the lints**

Run: `cargo fmt --all --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: all clean. This is the stage's gate: 2,460 tests passed before it started, and none of them may have changed meaning.

- [ ] **Step 7: Update the docs**

Add a short section to `docs/evaluation.md` §2 saying where pairs now come from when `evolve.feed_sweep` is on, and that the cargo harness is unaffected. One paragraph; the file already explains why pairs made by hand are scarce, and this says what changed.

- [ ] **Step 8: Commit**

```bash
git add src/eval/sweep.rs src/config.rs config.example.toml docs/evaluation.md
git commit -m "feat(evolve): the sweep may read what use left behind, and ships not doing so"
```

---

## What this stage does not do

Named so a reviewer does not look for them:

- **Nothing adopts anything.** No idle pass, no candidate generations, no revert. That is stage 2.
- **No retrieval parameter is in a generation yet** — only the two the sweep already moves. Widening `GenerationParams` is stage 2's first task and costs no migration, because `params` is JSON.
- **The anchor check does not exist.** Comparing self-generated observations against human verdicts, and suspending on decay, is stage 2.
- **The corpus jobs are untouched.** Stage 3.
