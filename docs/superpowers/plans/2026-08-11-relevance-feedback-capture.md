# Relevance Feedback — Capture, Judging and Export — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Capture real searches with their candidate lists, let the operator label them into query/artifact pairs, and export those pairs to the evaluation harness — which this plan also repairs.

**Architecture:** A new `store::feedback` module persists search events and their candidate pools into the existing SQLite database. `core::search` writes an event through the existing `Background` queue, so search never gets slower or fails because of it. A judging page turns pending events into verdicts. An `--export-eval` command writes `pairs.json` and `artifacts.json` for `tests/eval.rs`.

**Tech Stack:** Rust 2024 (MSRV 1.94), axum 0.8, sqlx 0.9 (SQLite), askama 0.16 templates, htmx, tokio.

## Global Constraints

- **The invariant:** captured feedback never influences the ranking of an individual search. The search path writes to the feedback store and never reads from it.
- **No model call anywhere in this feature** — not on capture, not on judging, not on export.
- Rust edition 2024, MSRV 1.94. `cargo clippy --all-targets -- -D warnings` must pass.
- `cargo fmt --all --check` must pass. All tests run without infrastructure (Qdrant-touching tests are `#[ignore]`d).
- Comments explain *why*, not *what* — match the density and voice of surrounding code.
- Test names are full sentences describing the behaviour, e.g. `an_empty_result_list_is_still_captured`.
- Config keys are settable by environment variable via the `ENGRAM__` prefix; nothing new is required for that beyond adding the struct field.
- This plan covers stages 1–4 of the spec. Stage 5 (the `Tunables` rebuild, the sweep and the proposal card) is a separate plan, written after this one lands.

**Spec:** `docs/superpowers/specs/2026-08-11-relevance-feedback-loop-design.md`

---

## File Structure

**Created:**
- `migrations/0013_feedback.sql` — the two capture tables
- `src/store/feedback.rs` — persistence and the `Door` enum; no knowledge of Qdrant or search
- `src/web/judge.rs` — judging routes and template structs
- `src/web/templates/judge.html`, `src/web/templates/_judge_card.html`
- `src/eval/export.rs` — writes `pairs.json` and `artifacts.json` from the live database

**Modified:**
- `src/store/mod.rs` — register the module
- `src/config.rs` — `FeedbackConfig`
- `src/core/mod.rs` — carry `feedback` on `Core`
- `src/core/search.rs` — `Door` parameter, similarity map, the capture hook
- `src/web/ui.rs`, `src/web/api.rs`, `src/mcp/mod.rs`, `src/core/ask.rs` — pass a `Door`
- `src/web/mod.rs` — mount the judge router
- `src/web/templates/ops.html` — capture status line and purge action
- `src/eval/mod.rs` — `save_pairs`, register `export`
- `src/store/artifacts.rs` — `all_active_artifacts`
- `src/main.rs` — `--export-eval`
- `assets/app.js` — judging keyboard shortcuts
- `tests/eval.rs` — the id-mapping repair
- `config.example.toml`, `README.md`, `ROADMAP.md`

---

### Task 1: The feedback tables and event recording with prefix coalescing

**Files:**
- Create: `migrations/0013_feedback.sql`
- Create: `src/store/feedback.rs`
- Modify: `src/store/mod.rs:1-7`

**Interfaces:**
- Consumes: `Store` (`src/store/mod.rs:15`), `now()` (`:66`), `new_id()` (`:73`)
- Produces:
  - `pub enum Door { Ui, Api, Mcp, Judge }` with `as_str(&self) -> &'static str` and `captured(&self) -> bool`
  - `pub struct NewCandidate { pub artifact_id: String, pub score: f32, pub similarity: Option<f32>, pub shown: bool }`
  - `pub struct NewEvent { pub query: String, pub door: Door, pub filters: String, pub query_vec: Vec<f32>, pub embed_model: String, pub candidates: Vec<NewCandidate> }`
  - `Store::record_search(&self, ev: NewEvent, coalesce_secs: i64) -> Result<String>`

- [ ] **Step 1: Write the migration**

Create `migrations/0013_feedback.sql`:

```sql
-- Real searches, captured before their results were seen, so the wording is
-- the searcher's own rather than the artifact's. `judged_at` NULL means the
-- event is still waiting for a verdict.
CREATE TABLE search_events (
  id          TEXT PRIMARY KEY,
  query       TEXT NOT NULL,
  door        TEXT NOT NULL,
  filters     TEXT NOT NULL DEFAULT '{}',
  query_vec   BLOB NOT NULL,
  vec_dim     INTEGER NOT NULL,
  embed_model TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  judged_at   INTEGER,
  verdict     TEXT,
  expect_id   TEXT,
  skips       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_events_pending ON search_events(judged_at, skips, created_at DESC);
CREATE INDEX idx_events_verdict ON search_events(verdict);

-- What the search offered, wider than what it showed. `shown` separates the
-- two: the judging card offers the whole pool so a hit that ranked far down
-- can still be confirmed, which is the only way a ranking failure becomes
-- measurable. No foreign key on `artifact_id` on purpose — deleting an
-- artifact must not erase the record of what was once returned.
CREATE TABLE search_candidates (
  event_id    TEXT NOT NULL REFERENCES search_events(id) ON DELETE CASCADE,
  rank        INTEGER NOT NULL,
  artifact_id TEXT NOT NULL,
  score       REAL NOT NULL,
  similarity  REAL,
  shown       INTEGER NOT NULL,
  PRIMARY KEY (event_id, rank)
);
```

- [ ] **Step 2: Write the failing tests**

Create `src/store/feedback.rs` with only the test module for now, so the tests fail to compile against absent items — that is the failure we want:

```rust
//! What a real search looked like, so it can be judged later.
//!
//! The query is the one thing no amount of care can reconstruct afterwards: it
//! has to be recorded in the moment, before any result was seen. The verdict is
//! the opposite — it needs a person, and it can wait. Everything here exists to
//! keep those two apart in time.

use super::{Store, new_id, now};
use crate::error::Result;
use sqlx::Row;

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(query: &str, door: Door) -> NewEvent {
        NewEvent {
            query: query.into(),
            door,
            filters: "{}".into(),
            query_vec: vec![0.5, -0.25],
            embed_model: "fake".into(),
            candidates: vec![NewCandidate {
                artifact_id: "a1".into(),
                score: 0.9,
                similarity: Some(0.8),
                shown: true,
            }],
        }
    }

    async fn queries(store: &Store) -> Vec<String> {
        sqlx::query("SELECT query FROM search_events ORDER BY created_at")
            .fetch_all(&store.pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("query"))
            .collect()
    }

    #[tokio::test]
    async fn a_typing_burst_collapses_to_its_final_wording() {
        let store = Store::memory().await.unwrap();
        for q in ["daten", "datentr", "datenträger nicht erkannt"] {
            store.record_search(ev(q, Door::Ui), 15).await.unwrap();
        }
        assert_eq!(queries(&store).await, vec!["datenträger nicht erkannt"]);
    }

    #[tokio::test]
    async fn a_query_that_is_not_a_prefix_starts_its_own_event() {
        let store = Store::memory().await.unwrap();
        store.record_search(ev("fat32", Door::Ui), 15).await.unwrap();
        store.record_search(ev("ntfs", Door::Ui), 15).await.unwrap();
        assert_eq!(queries(&store).await, vec!["fat32", "ntfs"]);
    }

    #[tokio::test]
    async fn a_prefix_from_another_door_does_not_fold_into_this_one() {
        // Two front doors are two people as far as this is concerned. Folding
        // an MCP call into a half-typed UI query would invent a search nobody
        // made.
        let store = Store::memory().await.unwrap();
        store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        store.record_search(ev("fat32", Door::Mcp), 15).await.unwrap();
        assert_eq!(queries(&store).await, vec!["fat", "fat32"]);
    }

    #[tokio::test]
    async fn a_prefix_outside_the_window_starts_its_own_event() {
        let store = Store::memory().await.unwrap();
        store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        // Zero window: the previous event is already too old to extend.
        store.record_search(ev("fat32", Door::Ui), 0).await.unwrap();
        assert_eq!(queries(&store).await, vec!["fat", "fat32"]);
    }

    #[tokio::test]
    async fn folding_replaces_the_candidate_list_rather_than_appending_to_it() {
        // The candidates belong to the query that produced them. Keeping the
        // earlier ones would describe a result list that was never shown.
        let store = Store::memory().await.unwrap();
        store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        let mut second = ev("fat32", Door::Ui);
        second.candidates[0].artifact_id = "a2".into();
        store.record_search(second, 15).await.unwrap();

        let rows = sqlx::query("SELECT artifact_id FROM search_candidates")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<String, _>("artifact_id"), "a2");
    }

    #[tokio::test]
    async fn a_judged_event_is_never_folded_into() {
        // Folding rewrites the query. Doing that under a verdict would leave a
        // label attached to words the operator never judged.
        let store = Store::memory().await.unwrap();
        let id = store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        sqlx::query("UPDATE search_events SET judged_at = ?, verdict = 'gap' WHERE id = ?")
            .bind(now())
            .bind(&id)
            .execute(&store.pool)
            .await
            .unwrap();
        store.record_search(ev("fat32", Door::Ui), 15).await.unwrap();
        assert_eq!(queries(&store).await, vec!["fat", "fat32"]);
    }

    #[tokio::test]
    async fn an_empty_result_list_is_still_captured() {
        // A search that found nothing is the most direct evidence of a gap the
        // system will ever get. It has no candidates and must still be stored.
        let store = Store::memory().await.unwrap();
        let mut e = ev("etwas das es nicht gibt", Door::Ui);
        e.candidates.clear();
        store.record_search(e, 15).await.unwrap();
        assert_eq!(queries(&store).await.len(), 1);
    }

    #[tokio::test]
    async fn the_query_vector_survives_a_round_trip_through_the_blob() {
        let store = Store::memory().await.unwrap();
        let id = store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        let row = sqlx::query("SELECT query_vec, vec_dim FROM search_events WHERE id = ?")
            .bind(&id)
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(row.get::<i64, _>("vec_dim"), 2);
        assert_eq!(
            blob_to_vec(&row.get::<Vec<u8>, _>("query_vec")),
            vec![0.5, -0.25]
        );
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib store::feedback`
Expected: compile error — `Door`, `NewEvent`, `NewCandidate`, `record_search`, `blob_to_vec` do not exist.

- [ ] **Step 4: Write the implementation**

Insert above the test module in `src/store/feedback.rs`:

```rust
/// Which front door a search came through.
///
/// An explicit parameter rather than a field on `SearchQuery`: that struct is
/// deserialised from the query string, so a `Default` there would silently
/// record an API search as a UI search the first time a caller forgot to set
/// it. With no default, the compiler asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Door {
    Ui,
    Api,
    Mcp,
    /// The search inside the judging view's "none of these" path. Never
    /// captured: those queries are composed in full knowledge of the answer,
    /// which is exactly the contamination this whole feature exists to avoid.
    Judge,
}

impl Door {
    pub fn as_str(&self) -> &'static str {
        match self {
            Door::Ui => "ui",
            Door::Api => "api",
            Door::Mcp => "mcp",
            Door::Judge => "judge",
        }
    }

    pub fn captured(&self) -> bool {
        !matches!(self, Door::Judge)
    }
}

#[derive(Debug, Clone)]
pub struct NewCandidate {
    pub artifact_id: String,
    pub score: f32,
    /// Cosine, where the store could report one. `None` for a hit the lexical
    /// half matched verbatim — see `SearchResult::weak`.
    pub similarity: Option<f32>,
    /// Whether it was inside the answer the searcher actually saw.
    pub shown: bool,
}

#[derive(Debug, Clone)]
pub struct NewEvent {
    pub query: String,
    pub door: Door,
    /// JSON, so a replay can reproduce the same narrowing.
    pub filters: String,
    pub query_vec: Vec<f32>,
    /// Vectors are only comparable under the model that produced them.
    pub embed_model: String,
    pub candidates: Vec<NewCandidate>,
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

impl Store {
    /// Record one search, folding a typing burst into a single event.
    ///
    /// Capturing only deliberate searches would lose the most valuable case:
    /// `mark` is set on open, expand and submit, so a search where the operator
    /// found nothing useful and gave up would never be recorded. So everything
    /// is captured, and an event whose query strictly extends the previous one
    /// from the same door, within `coalesce_secs`, replaces it. What survives is
    /// the final wording — the query that was actually meant.
    pub async fn record_search(&self, ev: NewEvent, coalesce_secs: i64) -> Result<String> {
        let mut tx = self.pool.begin().await?;
        let at = now();

        let prev = sqlx::query(
            "SELECT id, query, created_at FROM search_events
             WHERE door = ? AND judged_at IS NULL
             ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(ev.door.as_str())
        .fetch_optional(&mut *tx)
        .await?;

        let extends = prev.as_ref().and_then(|r| {
            let prior: String = r.get("query");
            let created: i64 = r.get("created_at");
            let fresh = at - created <= coalesce_secs;
            let grew = ev.query.len() > prior.len() && ev.query.starts_with(&prior);
            (fresh && grew).then(|| r.get::<String, _>("id"))
        });

        let id = match extends {
            Some(id) => {
                sqlx::query(
                    "UPDATE search_events
                     SET query = ?, filters = ?, query_vec = ?, vec_dim = ?,
                         embed_model = ?, created_at = ?
                     WHERE id = ?",
                )
                .bind(&ev.query)
                .bind(&ev.filters)
                .bind(vec_to_blob(&ev.query_vec))
                .bind(ev.query_vec.len() as i64)
                .bind(&ev.embed_model)
                .bind(at)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
                sqlx::query("DELETE FROM search_candidates WHERE event_id = ?")
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                id
            }
            None => {
                let id = new_id();
                sqlx::query(
                    "INSERT INTO search_events
                       (id, query, door, filters, query_vec, vec_dim, embed_model, created_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(&ev.query)
                .bind(ev.door.as_str())
                .bind(&ev.filters)
                .bind(vec_to_blob(&ev.query_vec))
                .bind(ev.query_vec.len() as i64)
                .bind(&ev.embed_model)
                .bind(at)
                .execute(&mut *tx)
                .await?;
                id
            }
        };

        for (rank, c) in ev.candidates.iter().enumerate() {
            sqlx::query(
                "INSERT INTO search_candidates
                   (event_id, rank, artifact_id, score, similarity, shown)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(rank as i64)
            .bind(&c.artifact_id)
            .bind(c.score)
            .bind(c.similarity)
            .bind(c.shown as i64)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(id)
    }
}
```

Register the module — add to `src/store/mod.rs` in alphabetical position (after `corpora`):

```rust
pub mod feedback;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib store::feedback`
Expected: 8 passed.

- [ ] **Step 6: Check formatting and lints**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add migrations/0013_feedback.sql src/store/feedback.rs src/store/mod.rs
git commit -m "feat: remember what a search looked like before its results were seen"
```

---

### Task 2: Configuration and the capture hook in the search path

**Files:**
- Modify: `src/config.rs` (add `FeedbackConfig`, add the field to `Config`)
- Modify: `src/core/mod.rs:56-108` (carry `feedback`), `:149-168` (test builder)
- Modify: `src/core/search.rs:283-439` (the `Door` parameter, the similarity map, the hook)
- Modify: `src/web/ui.rs`, `src/web/api.rs`, `src/mcp/mod.rs`, `src/core/ask.rs` (call sites)
- Modify: `config.example.toml`

**Interfaces:**
- Consumes: `Door`, `NewEvent`, `NewCandidate`, `Store::record_search` from Task 1; `Background::spawn` (`src/core/background.rs:44`)
- Produces:
  - `pub struct FeedbackConfig { pub enabled: bool, pub candidates: usize, pub coalesce_secs: i64, pub retain_days: i64 }` with `Default`
  - `Core::search(&self, query: &SearchQuery, door: Door)`, `Core::search_timed(&self, query: &SearchQuery, door: Door)`, `Core::search_capped(&self, query: &SearchQuery, cap: Option<usize>, door: Door)`

- [ ] **Step 1: Write the failing tests**

Add to the test module in `src/core/search.rs`:

```rust
#[tokio::test]
async fn a_captured_search_stores_the_pool_it_could_have_shown() {
    // The stored pool is wider than the answer: the judging card offers all of
    // it, so an artifact the ranking buried can still be confirmed.
    let mut core = test_core().await;
    core.feedback.enabled = true;
    let corpus = seed_artifacts(&core, 12).await;
    let _ = corpus;

    let mut q = SearchQuery::for_test("a query about something");
    q.limit = 3;
    core.search(&q, Door::Ui).await.unwrap();
    core.background.wait_idle().await;

    let rows = sqlx::query("SELECT rank, shown FROM search_candidates ORDER BY rank")
        .fetch_all(&core.store.pool)
        .await
        .unwrap();
    assert!(
        rows.len() > 3,
        "the pool must be wider than the three results shown, got {}",
        rows.len()
    );
    let shown: i64 = rows.iter().map(|r| r.get::<i64, _>("shown")).sum();
    assert_eq!(shown, 3, "exactly the answer the searcher saw is flagged shown");
}

#[tokio::test]
async fn capture_writes_nothing_while_it_is_switched_off() {
    let core = test_core().await; // feedback.enabled defaults to false
    seed_artifacts(&core, 3).await;
    core.search(&SearchQuery::for_test("anything"), Door::Ui)
        .await
        .unwrap();
    core.background.wait_idle().await;

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM search_events")
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn a_search_that_found_nothing_is_still_captured() {
    // Deliberately unlike `mark_seen`, which skips an empty list because there
    // is nothing to stamp. Here the empty list is the finding.
    let mut core = test_core().await;
    core.feedback.enabled = true;
    core.search(&SearchQuery::for_test("nothing is indexed yet"), Door::Ui)
        .await
        .unwrap();
    core.background.wait_idle().await;

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM search_events")
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn the_judging_door_is_never_captured() {
    let mut core = test_core().await;
    core.feedback.enabled = true;
    seed_artifacts(&core, 3).await;
    core.search(&SearchQuery::for_test("assigning an artifact"), Door::Judge)
        .await
        .unwrap();
    core.background.wait_idle().await;

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM search_events")
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}
```

If `SearchQuery::for_test` and `seed_artifacts` do not already exist in that test module, write them beside the tests:

```rust
#[cfg(test)]
impl SearchQuery {
    fn for_test(q: &str) -> SearchQuery {
        SearchQuery {
            q: q.into(),
            limit: 10,
            tags: vec![],
            category: None,
            mark: false,
            include_deprecated: false,
            include_superseded: false,
        }
    }
}

#[cfg(test)]
async fn seed_artifacts(core: &Core, n: usize) -> String {
    let src = core
        .store
        .insert_corpus("seed corpus", "test", Some("seed"))
        .await
        .unwrap();
    let new: Vec<crate::store::artifacts::NewArtifact> = (0..n)
        .map(|i| crate::store::artifacts::NewArtifact {
            ordinal: i as i64,
            text: format!("artifact number {i} about something"),
            corpus_span: None,
            title: Some(format!("artifact {i}")),
            category: None,
            tags: vec![],
            segment_idx: None,
            caveats: vec![],
        })
        .collect();
    core.store.insert_artifacts(&src.id, &new).await.unwrap();
    crate::jobs::embed::run_corpus(core, &src.id).await.unwrap();
    src.id
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib core::search`
Expected: compile error — `search` takes one argument, `core.feedback` does not exist.

- [ ] **Step 3: Add the config section**

In `src/config.rs`, add the struct and wire it into `Config`:

```rust
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct FeedbackConfig {
    /// Whether real searches are recorded at all. Off by default: the wording
    /// of a query is personal, and nothing here is useful to anyone but the
    /// operator.
    pub enabled: bool,
    /// Candidates stored per event. Wider than the answer on purpose — search
    /// over-fetches anyway, so the extra rows are free and they are what lets a
    /// buried hit be confirmed later.
    pub candidates: usize,
    /// Window in which a query that extends the previous one replaces it
    /// instead of starting a new event.
    pub coalesce_secs: i64,
    /// Days captured searches are kept. 0 keeps them forever.
    pub retain_days: i64,
}

impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            candidates: 20,
            coalesce_secs: 15,
            retain_days: 0,
        }
    }
}
```

Add to `Config` (`src/config.rs:5-13`):

```rust
    #[serde(default)]
    pub feedback: FeedbackConfig,
```

- [ ] **Step 4: Carry it on `Core`**

In `src/core/mod.rs`, add the field to the struct (after `consolidate`):

```rust
    /// Whether and how real searches are recorded for later judging. Read on
    /// the search path, so it lives here rather than being threaded down.
    pub feedback: crate::config::FeedbackConfig,
```

Set it in `from_config`: `feedback: cfg.feedback.clone(),`
Set it in `test_support::build`: `feedback: crate::config::FeedbackConfig::default(),`

- [ ] **Step 5: Thread the door through the search entry points**

In `src/core/search.rs`, change the four signatures and add the similarity map. `search`:

```rust
    pub async fn search(&self, query: &SearchQuery, door: Door) -> Result<Vec<SearchResult>> {
        Ok(self.search_inner(query, Some(MAX_PER_CORPUS), door).await?.0)
    }

    pub async fn search_timed(
        &self,
        query: &SearchQuery,
        door: Door,
    ) -> Result<(Vec<SearchResult>, SearchTiming)> {
        self.search_inner(query, Some(MAX_PER_CORPUS), door).await
    }

    pub async fn search_capped(
        &self,
        query: &SearchQuery,
        cap: Option<usize>,
        door: Door,
    ) -> Result<Vec<SearchResult>> {
        Ok(self.search_inner(query, cap, door).await?.0)
    }

    async fn search_inner(
        &self,
        query: &SearchQuery,
        cap: Option<usize>,
        door: Door,
    ) -> Result<(Vec<SearchResult>, SearchTiming)> {
```

Beside the existing `let hit_counts = counts_of(&hits);` (`src/core/search.rs:378`), add — the similarity is consumed when the payloads are turned into results, so it has to be taken first:

```rust
        // Taken for the same reason as `hit_counts`: the similarity is dropped
        // when the payload becomes a `SearchResult`, and capture needs the
        // value rather than the `weak` verdict computed from it.
        let sims: HashMap<String, Option<f32>> = hits
            .iter()
            .map(|h| (h.payload.artifact_id.clone(), h.similarity))
            .collect();
```

- [ ] **Step 6: Add the capture hook**

In `src/core/search.rs`, immediately **before** `results.truncate(limit);` (`:420`) — that is where the pool is still wide and the answer is already ordered:

```rust
        // Recorded here, where the list is still wider than the answer and the
        // ordering is final. Off the request path via `Background`, like
        // `mark_seen` beside it: a search must not get slower, or fail, because
        // bookkeeping did.
        if self.feedback.enabled && door.captured() {
            let candidates: Vec<crate::store::feedback::NewCandidate> = results
                .iter()
                .take(self.feedback.candidates)
                .enumerate()
                .map(|(i, r)| crate::store::feedback::NewCandidate {
                    artifact_id: r.artifact_id.clone(),
                    score: r.score,
                    similarity: sims.get(&r.artifact_id).copied().flatten(),
                    shown: i < limit,
                })
                .collect();
            let event = crate::store::feedback::NewEvent {
                query: query.q.trim().to_string(),
                door,
                filters: serde_json::json!({
                    "tags": query.tags,
                    "category": query.category,
                    "limit": limit,
                })
                .to_string(),
                query_vec: vector.clone(),
                embed_model: self.embedder.model().to_string(),
                candidates,
            };
            let store = self.store.clone();
            let window = self.feedback.coalesce_secs;
            self.background.spawn(async move {
                if let Err(e) = store.record_search(event, window).await {
                    tracing::warn!(error = %e, "could not record the search");
                }
            });
        }
```

Add `use crate::store::feedback::Door;` to the imports at the top of `src/core/search.rs`.

A blank query never reaches here: `search_inner` rejects it at `:313`.

- [ ] **Step 7: Update the call sites**

Each caller names its door explicitly:

- `src/web/ui.rs` — every `core.search…` in the UI handlers: `Door::Ui`. The handler behind the judging view's assignment search (added in Task 6) uses `Door::Judge`.
- `src/web/api.rs` — `Door::Api`
- `src/mcp/mod.rs` — `Door::Mcp`
- `src/core/ask.rs` — `Door::Judge`, because `ask` is deliberately not captured and `Judge` is the door that is never recorded. Add a comment saying so:

```rust
    // `ask` is not captured: its right answer is a synthesis across several
    // artifacts, so "which one was it" has no well-defined meaning. Reusing the
    // never-recorded door keeps that in one place.
```

Every test call site in `src/core/search.rs` and elsewhere gains `Door::Ui`.

- [ ] **Step 8: Run the tests**

Run: `cargo test --locked`
Expected: all pass, including the four new ones.

- [ ] **Step 9: Document the config**

Add to `config.example.toml`, in the house style — the reasoning, not just the key:

```toml
# Recording real searches so they can be judged later, and turned into the
# query/artifact pairs the evaluation harness scores against. Off by default:
# a query log is personal, and it is useful to nobody but you.
#
# Everything is recorded, not only deliberate searches — a search where you
# found nothing and gave up is the most valuable one there is, and it leaves no
# other trace. Typing bursts are folded into their final wording, so `daten`,
# `datentr`, `datenträger nicht erkannt` is one event, not three.
[feedback]
enabled       = false
candidates    = 20   # stored per search; wider than the answer on purpose
coalesce_secs = 15   # window in which a longer query replaces the previous one
retain_days   = 0    # 0 keeps them forever
```

Add a `feedback.*` row to the README config table:

```markdown
| `feedback.*` | Recording real searches for later judging: `enabled`, `candidates`, `coalesce_secs`, `retain_days`. Off by default. |
```

- [ ] **Step 10: Check formatting and lints, then commit**

```bash
cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked
git add -A
git commit -m "feat: searches leave a trace worth judging later"
```

---

### Task 3: Reading pending events, verdicts and the field metrics

**Files:**
- Modify: `src/store/feedback.rs`

**Interfaces:**
- Consumes: everything from Task 1
- Produces:
  - `pub enum Verdict { Hit, Gap, Discard }` with `as_str`/`parse`
  - `pub struct Candidate { pub artifact_id: String, pub rank: i64, pub shown: bool }`
  - `pub struct PendingEvent { pub id: String, pub query: String, pub door: String, pub created_at: i64, pub candidates: Vec<Candidate> }`
  - `pub struct Stats { pub judged: i64, pub hits: i64, pub finds: i64, pub gaps: i64, pub discards: i64, pub pending: i64, pub captured: i64, pub recall_at_10: f64, pub mrr: f64 }`
  - `pub struct Miss { pub query: String, pub rank: Option<i64> }`
  - `Store::next_pending(&self) -> Result<Option<PendingEvent>>`
  - `Store::judge_hit(&self, event_id: &str, artifact_id: &str) -> Result<()>`
  - `Store::judge(&self, event_id: &str, verdict: Verdict) -> Result<()>`
  - `Store::skip_event(&self, event_id: &str) -> Result<()>`
  - `Store::feedback_stats(&self) -> Result<Stats>`
  - `Store::misses(&self, limit: i64) -> Result<Vec<Miss>>`
  - `Store::purge_feedback(&self) -> Result<u64>`

- [ ] **Step 1: Write the failing tests**

Add to the test module in `src/store/feedback.rs`:

```rust
    async fn seed(store: &Store, query: &str, ranked: &[&str]) -> String {
        let mut e = ev(query, Door::Ui);
        e.candidates = ranked
            .iter()
            .enumerate()
            .map(|(i, id)| NewCandidate {
                artifact_id: (*id).into(),
                score: 1.0 - i as f32 / 100.0,
                similarity: Some(0.5),
                shown: i < 10,
            })
            .collect();
        store.record_search(e, 0).await.unwrap()
    }

    #[tokio::test]
    async fn the_newest_unjudged_event_comes_up_first() {
        // Judging is worth something because the situation is still in mind,
        // and that memory is the most perishable part of the dataset.
        let store = Store::memory().await.unwrap();
        seed(&store, "older", &["a"]).await;
        seed(&store, "newer", &["b"]).await;
        assert_eq!(store.next_pending().await.unwrap().unwrap().query, "newer");
    }

    #[tokio::test]
    async fn a_skipped_event_sinks_below_the_ones_never_looked_at() {
        let store = Store::memory().await.unwrap();
        seed(&store, "older", &["a"]).await;
        let newer = seed(&store, "newer", &["b"]).await;
        store.skip_event(&newer).await.unwrap();
        assert_eq!(store.next_pending().await.unwrap().unwrap().query, "older");
    }

    #[tokio::test]
    async fn a_judged_event_does_not_come_back() {
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "only one", &["a"]).await;
        store.judge_hit(&id, "a").await.unwrap();
        assert!(store.next_pending().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_field_metrics_read_the_rank_the_search_actually_gave() {
        // No Qdrant and no embedding: the rank of every candidate was stored
        // when the search happened, so confirming one settles its rank too.
        let store = Store::memory().await.unwrap();
        let first = seed(&store, "top hit", &["a", "b", "c"]).await;
        store.judge_hit(&first, "a").await.unwrap();
        let third = seed(&store, "third hit", &["x", "y", "z"]).await;
        store.judge_hit(&third, "z").await.unwrap();

        let s = store.feedback_stats().await.unwrap();
        assert_eq!(s.judged, 2);
        assert_eq!(s.hits, 2);
        assert!((s.recall_at_10 - 1.0).abs() < 1e-9);
        // 1/1 and 1/3, averaged.
        assert!((s.mrr - (1.0 + 1.0 / 3.0) / 2.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn an_answer_outside_the_pool_counts_as_a_find_and_a_miss() {
        // The whole point of the "none of these" path: an artifact the ranker
        // never returned. It has no rank, so it contributes nothing to MRR and
        // it drags recall down — which is the truth about that search.
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "found nothing useful", &["a", "b"]).await;
        store.judge_hit(&id, "something-else").await.unwrap();

        let s = store.feedback_stats().await.unwrap();
        assert_eq!(s.finds, 1);
        assert_eq!(s.recall_at_10, 0.0);
        assert_eq!(s.mrr, 0.0);
        assert_eq!(store.misses(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn gaps_and_discards_are_counted_but_are_not_pairs() {
        let store = Store::memory().await.unwrap();
        let g = seed(&store, "nothing written about this", &[]).await;
        store.judge(&g, Verdict::Gap).await.unwrap();
        let d = seed(&store, "asdf", &["a"]).await;
        store.judge(&d, Verdict::Discard).await.unwrap();

        let s = store.feedback_stats().await.unwrap();
        assert_eq!((s.gaps, s.discards, s.hits), (1, 1, 0));
        // Neither can score: one has no answer, the other was not a question.
        assert_eq!(s.mrr, 0.0);
    }

    #[tokio::test]
    async fn purging_removes_events_and_their_candidates() {
        let store = Store::memory().await.unwrap();
        seed(&store, "a search", &["a", "b"]).await;
        store.purge_feedback().await.unwrap();

        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM search_candidates")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert!(store.next_pending().await.unwrap().is_none());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib store::feedback`
Expected: compile error — `Verdict`, `next_pending`, `judge_hit`, `judge`, `skip_event`, `feedback_stats`, `misses`, `purge_feedback` do not exist.

- [ ] **Step 3: Write the implementation**

Add to `src/store/feedback.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// One artifact is the answer. `expect_id` names it — it may be an artifact
    /// the search never returned, which is the most valuable case there is.
    Hit,
    /// Nothing in the base could have answered this. Not a pair; a finding.
    Gap,
    /// Not a real search — a typo, or poking at the box.
    Discard,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Hit => "hit",
            Verdict::Gap => "gap",
            Verdict::Discard => "discard",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub artifact_id: String,
    pub rank: i64,
    pub shown: bool,
}

#[derive(Debug, Clone)]
pub struct PendingEvent {
    pub id: String,
    pub query: String,
    pub door: String,
    pub created_at: i64,
    pub candidates: Vec<Candidate>,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub captured: i64,
    pub pending: i64,
    pub judged: i64,
    pub hits: i64,
    /// Hits whose artifact the search never returned. Rare and expensive, and
    /// the only evidence that ranking — rather than the corpus — was at fault.
    pub finds: i64,
    pub gaps: i64,
    pub discards: i64,
    pub recall_at_10: f64,
    pub mrr: f64,
}

#[derive(Debug, Clone)]
pub struct Miss {
    pub query: String,
    /// `None` means the confirmed artifact was not in the stored pool at all.
    pub rank: Option<i64>,
}

impl Store {
    /// The next event to judge: never-skipped first, newest first within that.
    pub async fn next_pending(&self) -> Result<Option<PendingEvent>> {
        let row = sqlx::query(
            "SELECT id, query, door, created_at FROM search_events
             WHERE judged_at IS NULL
             ORDER BY skips ASC, created_at DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let id: String = row.get("id");

        let candidates = sqlx::query(
            "SELECT artifact_id, rank, shown FROM search_candidates
             WHERE event_id = ? ORDER BY rank",
        )
        .bind(&id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| Candidate {
            artifact_id: r.get("artifact_id"),
            rank: r.get("rank"),
            shown: r.get::<i64, _>("shown") == 1,
        })
        .collect();

        Ok(Some(PendingEvent {
            id,
            query: row.get("query"),
            door: row.get("door"),
            created_at: row.get("created_at"),
            candidates,
        }))
    }

    pub async fn judge_hit(&self, event_id: &str, artifact_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE search_events SET judged_at = ?, verdict = 'hit', expect_id = ?
             WHERE id = ?",
        )
        .bind(now())
        .bind(artifact_id)
        .bind(event_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn judge(&self, event_id: &str, verdict: Verdict) -> Result<()> {
        sqlx::query("UPDATE search_events SET judged_at = ?, verdict = ? WHERE id = ?")
            .bind(now())
            .bind(verdict.as_str())
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Not a verdict: the event stays pending and only sinks in the order. An
    /// honest "I don't remember" must never cost anything, or it stops being
    /// honest.
    pub async fn skip_event(&self, event_id: &str) -> Result<()> {
        sqlx::query("UPDATE search_events SET skips = skips + 1 WHERE id = ?")
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The field value: recall@10 and MRR read from the ranks the searches
    /// actually gave. No vector store and no embedding are involved, so the
    /// number can move on every judgement.
    pub async fn feedback_stats(&self) -> Result<Stats> {
        let mut s = Stats {
            captured: sqlx::query_scalar("SELECT count(*) FROM search_events")
                .fetch_one(&self.pool)
                .await?,
            pending: sqlx::query_scalar(
                "SELECT count(*) FROM search_events WHERE judged_at IS NULL",
            )
            .fetch_one(&self.pool)
            .await?,
            ..Default::default()
        };

        for (field, verdict) in [
            (&mut s.hits, "hit"),
            (&mut s.gaps, "gap"),
            (&mut s.discards, "discard"),
        ] {
            *field = sqlx::query_scalar("SELECT count(*) FROM search_events WHERE verdict = ?")
                .bind(verdict)
                .fetch_one(&self.pool)
                .await?;
        }
        s.judged = s.hits + s.gaps + s.discards;

        // A left join, because an expected artifact that was never returned has
        // no row to join to — and that absence is precisely what a miss is.
        let ranks: Vec<Option<i64>> = sqlx::query(
            "SELECT c.rank AS rank FROM search_events e
             LEFT JOIN search_candidates c
               ON c.event_id = e.id AND c.artifact_id = e.expect_id
             WHERE e.verdict = 'hit'",
        )
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| r.get::<Option<i64>, _>("rank"))
        .collect();

        s.finds = ranks.iter().filter(|r| r.is_none()).count() as i64;
        if !ranks.is_empty() {
            let n = ranks.len() as f64;
            s.recall_at_10 =
                ranks.iter().filter(|r| matches!(r, Some(i) if *i < 10)).count() as f64 / n;
            s.mrr = ranks
                .iter()
                .map(|r| r.map_or(0.0, |i| 1.0 / (i as f64 + 1.0)))
                .sum::<f64>()
                / n;
        }
        Ok(s)
    }

    /// The queries whose confirmed answer fell outside the first ten. The list
    /// that is actually read: an aggregate says something is wrong, this says
    /// what.
    pub async fn misses(&self, limit: i64) -> Result<Vec<Miss>> {
        Ok(sqlx::query(
            "SELECT e.query AS query, c.rank AS rank FROM search_events e
             LEFT JOIN search_candidates c
               ON c.event_id = e.id AND c.artifact_id = e.expect_id
             WHERE e.verdict = 'hit' AND (c.rank IS NULL OR c.rank >= 10)
             ORDER BY e.judged_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| Miss {
            query: r.get("query"),
            rank: r.get("rank"),
        })
        .collect())
    }

    /// Everything captured, gone. Judgements included: they are statements
    /// about queries, and a judgement whose query no longer exists is not a
    /// record of anything.
    pub async fn purge_feedback(&self) -> Result<u64> {
        // `search_candidates` goes with it through ON DELETE CASCADE.
        Ok(sqlx::query("DELETE FROM search_events")
            .execute(&self.pool)
            .await?
            .rows_affected())
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib store::feedback`
Expected: 15 passed.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings
git add src/store/feedback.rs
git commit -m "feat: a verdict, and the two numbers it moves"
```

---

### Task 4: The judging page

**Files:**
- Create: `src/web/judge.rs`, `src/web/templates/judge.html`, `src/web/templates/_judge_card.html`
- Modify: `src/web/mod.rs:52-60` (mount), `assets/app.js` (shortcuts)

**Interfaces:**
- Consumes: `Store::next_pending`, `judge_hit`, `judge`, `skip_event`, `feedback_stats`, `misses` (Task 3); `Store::artifacts_by_ids` — if it does not exist, add it in this task beside the other readers in `src/store/artifacts.rs`, signature `pub async fn artifacts_by_ids(&self, ids: &[String]) -> Result<Vec<Chunk>>`
- Produces: routes `GET /ui/judge`, `GET /ui/judge/next`, `POST /ui/judge/{id}/hit`, `POST /ui/judge/{id}/gap`, `POST /ui/judge/{id}/discard`, `POST /ui/judge/{id}/skip`

- [ ] **Step 1: Write the failing tests**

Create the test module at the bottom of `src/web/judge.rs`, following the `oneshot` pattern already used in `src/web/api.rs:587`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_card_offers_the_whole_pool_not_only_what_was_shown() {
        // Offering only the ten that were displayed would make a buried hit
        // unconfirmable, and the ranking failure invisible.
        let (app, core) = judge_app_with_event(&["a", "b", "c"], 1).await;
        let body = text(app.clone().oneshot(get("/ui/judge/next", None)).await).await;
        for id in ["a", "b", "c"] {
            assert!(body.contains(id), "candidate {id} missing from the card");
        }
        let _ = core;
    }

    #[tokio::test]
    async fn the_card_shows_no_ranks_and_no_scores() {
        // Both are the ranker's opinion, which is exactly what must not be
        // heard while judging.
        let (app, _) = judge_app_with_event(&["a", "b", "c"], 3).await;
        let body = text(app.oneshot(get("/ui/judge/next", None)).await).await;
        assert!(!body.contains("rank"), "a rank leaked into the card");
        assert!(!body.contains("score"), "a score leaked into the card");
    }

    #[tokio::test]
    async fn confirming_a_candidate_records_the_hit_and_moves_on() {
        let (app, core) = judge_app_with_event(&["a", "b"], 2).await;
        let id = core.store.next_pending().await.unwrap().unwrap().id;
        let res = app
            .oneshot(post(&format!("/ui/judge/{id}/hit"), "artifact_id=b"))
            .await;
        assert_eq!(res.status(), 200);

        let s = core.store.feedback_stats().await.unwrap();
        assert_eq!(s.hits, 1);
        assert!(core.store.next_pending().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn skipping_leaves_it_pending() {
        let (app, core) = judge_app_with_event(&["a"], 1).await;
        let id = core.store.next_pending().await.unwrap().unwrap().id;
        app.oneshot(post(&format!("/ui/judge/{id}/skip"), ""))
            .await;
        assert!(core.store.next_pending().await.unwrap().is_some());
        assert_eq!(core.store.feedback_stats().await.unwrap().judged, 0);
    }

    #[tokio::test]
    async fn a_vanished_artifact_is_left_out_of_the_card() {
        // The pool is history and keeps its rows; the card is a list of things
        // you can still choose.
        let (app, _) = judge_app_with_event(&["gone-for-good"], 0).await;
        let body = text(app.oneshot(get("/ui/judge/next", None)).await).await;
        assert!(!body.contains("gone-for-good"));
    }

    #[tokio::test]
    async fn nothing_pending_says_so_rather_than_rendering_an_empty_card() {
        let (app, _) = judge_app_with_event(&[], 0).await;
        let body = text(app.oneshot(get("/ui/judge", None)).await).await;
        assert!(body.to_lowercase().contains("nothing"));
    }
}
```

`judge_app_with_event(pool_ids, real_artifacts)` builds a test app with an authenticated identity (copy the harness `src/web/api.rs` tests use), seeds `real_artifacts` genuine artifacts whose ids replace the leading entries of `pool_ids`, and records one search event with that pool. `get`, `post` and `text` are the same helpers those tests use; reuse them rather than writing new ones.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib web::judge`
Expected: compile error — the module has no routes yet.

- [ ] **Step 3: Write the templates**

`src/web/templates/judge.html`:

```html
{% extends "layout.html" %}
{% block title %}Judge — engram{% endblock %}
{% block content %}
{# The header is the real measurement, not a score standing in for one: both
   numbers are read from the ranks the searches actually gave. #}
<div class="judge-header">
  <div class="row">
    <span class="mono">{{ stats.judged }} judged</span>
    <span class="spacer"></span>
    <span class="mono">recall@10 {{ recall }}</span>
    <span class="mono">MRR {{ mrr }}</span>
  </div>
  <div class="progress" role="progressbar"
       aria-valuenow="{{ stats.judged }}" aria-valuemin="0" aria-valuemax="{{ target }}">
    <span style="width:{{ progress_pct }}%"></span>
  </div>
  <p class="muted hint">{{ stats.judged }} / {{ target }} until the first sweep</p>
  <p class="muted mono">
    {{ stats.hits }} hits · {{ stats.finds }} finds · {{ stats.gaps }} gaps ·
    {{ stats.discards }} discarded
  </p>
</div>

<div id="card">{% include "_judge_card.html" %}</div>
{% endblock %}
```

`src/web/templates/_judge_card.html`:

```html
{% match card %}
{% when Some with (c) %}
<article class="judge-card" data-judge-id="{{ c.id }}">
  <p class="muted mono">{{ c.when }} · {{ c.door }}</p>
  <h2 class="judge-query">{{ c.query }}</h2>
  <p class="muted hint">Which of these was the one you needed?</p>
  <ol class="judge-options">
    {% for o in c.options %}
    <li>
      <button class="judge-option" hx-post="/ui/judge/{{ c.id }}/hit"
              hx-vals='{"artifact_id": "{{ o.artifact_id }}"}'
              hx-target="#card" hx-swap="innerHTML">
        <span class="judge-key">{{ loop.index }}</span>
        <span class="judge-title">{{ o.title }}</span>
        <span class="judge-snippet">{{ o.snippet }}</span>
      </button>
    </li>
    {% endfor %}
  </ol>
  <div class="row">
    <button hx-get="/ui/judge/{{ c.id }}/assign" hx-target="#card" hx-swap="innerHTML">
      None of these <kbd>N</kbd>
    </button>
    <button hx-post="/ui/judge/{{ c.id }}/skip" hx-target="#card" hx-swap="innerHTML">
      Can't remember <kbd>S</kbd>
    </button>
    <span class="spacer"></span>
    <button hx-post="/ui/judge/{{ c.id }}/discard" hx-target="#card" hx-swap="innerHTML">
      Not a real search <kbd>X</kbd>
    </button>
  </div>
</article>
{% when None %}
<p class="muted">Nothing to judge. <a href="/ui/search">Back to search.</a></p>
{% endmatch %}
```

- [ ] **Step 4: Write the handlers**

`src/web/judge.rs` — the card assembly is the part with a decision in it:

```rust
//! Turning captured searches into labelled pairs.
//!
//! The card shows the query as it was typed and the stored pool shuffled, with
//! no ranks and no scores. Both omissions are deliberate: the ranker's opinion
//! is the one thing that must not be visible while its work is being judged, or
//! the judgement measures agreement rather than relevance.

use crate::store::feedback::{PendingEvent, Verdict};
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use askama::Template;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::Router;

pub struct Option_ {
    pub artifact_id: String,
    pub title: String,
    pub snippet: String,
}

pub struct Card {
    pub id: String,
    pub query: String,
    pub door: String,
    pub when: String,
    pub options: Vec<Option_>,
}

/// Shuffle without pulling in a random-number crate: the event id is already a
/// uuid v7, so hashing it with each artifact id gives an order that is stable
/// for one card and unrelated to rank.
fn shuffled(event: &PendingEvent, mut options: Vec<Option_>) -> Vec<Option_> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    options.sort_by_key(|o| {
        let mut h = DefaultHasher::new();
        event.id.hash(&mut h);
        o.artifact_id.hash(&mut h);
        h.finish()
    });
    options
}

pub fn judge_router(state: AppState) -> Router<AppState> {
    let _ = state;
    Router::new()
        .route("/ui/judge", get(page))
        .route("/ui/judge/next", get(next_card))
        .route("/ui/judge/{id}/hit", post(hit))
        .route("/ui/judge/{id}/gap", post(gap))
        .route("/ui/judge/{id}/discard", post(discard))
        .route("/ui/judge/{id}/skip", post(skip))
}
```

Each handler loads the next event, hydrates its candidates through `artifacts_by_ids`, drops the ones that no longer exist, wraps them with `shuffled`, and renders `_judge_card.html`. `hit`/`gap`/`discard`/`skip` call the matching store method and then render the next card into `#card`, so one keystroke both records and advances.

`page` additionally computes `recall`/`mrr` formatted to two decimals, `target` from `feedback.tune.min_judgements` (50 until Stage 5 lands — hardcode the constant `FIRST_SWEEP_AT: i64 = 50` in this module with a comment saying the tuning plan replaces it with the config value), and `progress_pct`.

- [ ] **Step 5: Mount the router**

In `src/web/mod.rs`, beside the existing merges:

```rust
        .merge(crate::web::judge::judge_router(state.clone()))
```

and `pub mod judge;` at the top.

- [ ] **Step 6: Add the keyboard shortcuts**

In `assets/app.js`:

```javascript
// Judging has to cost five seconds or it will not happen. Digits pick an
// option, N/S/X take the three ways out. Ignored while typing in the
// assignment search, which is a text field like any other.
document.addEventListener('keydown', (e) => {
  const card = document.querySelector('.judge-card');
  if (!card || e.metaKey || e.ctrlKey || e.altKey) return;
  if (/^(INPUT|TEXTAREA)$/.test(document.activeElement.tagName)) return;

  const options = card.querySelectorAll('.judge-option');
  if (/^[1-9]$/.test(e.key)) {
    const pick = options[Number(e.key) - 1];
    if (pick) { e.preventDefault(); pick.click(); }
    return;
  }
  const buttons = { n: 0, s: 1, x: 2 };
  const idx = buttons[e.key.toLowerCase()];
  if (idx !== undefined) {
    const row = card.querySelectorAll('.row button');
    if (row[idx]) { e.preventDefault(); row[idx].click(); }
  }
});
```

- [ ] **Step 7: Add the styles**

In `assets/app.css`, beside the other component blocks:

```css
/* One card at a time, wide enough to read a snippet without wrapping every
   line. The options are buttons rather than links: they act, they do not
   navigate. */
.judge-card { max-width: 52rem; }
.judge-query { font-size: 1.25rem; font-weight: 500; margin: 0.25rem 0 0.75rem; }
.judge-options { list-style: none; margin: 0 0 1rem; padding: 0;
                 display: flex; flex-direction: column; gap: 0.375rem; }
.judge-option { display: grid; grid-template-columns: 1.5rem 1fr; gap: 0.5rem;
                width: 100%; text-align: left; padding: 0.5rem 0.625rem;
                border: 1px solid var(--color-border-subtle);
                border-radius: var(--radius-sm); background: var(--color-bg-elevated); }
.judge-option:hover { background: var(--color-bg-hover); }
.judge-key { font-family: var(--font-mono); color: var(--color-fg-muted); }
.judge-title { font-weight: 500; }
.judge-snippet { grid-column: 2; color: var(--color-fg-secondary); font-size: 0.875rem; }
.progress { height: 4px; background: var(--color-bg-active);
            border-radius: 2px; overflow: hidden; }
.progress span { display: block; height: 100%; background: var(--color-accent); }
```

- [ ] **Step 8: Run the tests**

Run: `cargo test --locked`
Expected: all pass.

- [ ] **Step 9: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings
git add -A
git commit -m "feat: one card, one question, five seconds"
```

---

### Task 5: The diagnosis line, the miss list and the assignment path

**Files:**
- Modify: `src/web/judge.rs`, `src/web/templates/_judge_card.html`, `src/web/templates/judge.html`
- Create: `src/web/templates/_judge_assign.html`, `src/web/templates/_judge_result.html`

**Interfaces:**
- Consumes: Task 4's routes and `Card`; `Core::search` with `Door::Judge` (Task 2)
- Produces: routes `GET /ui/judge/{id}/assign`, `GET /ui/judge/{id}/assign/results`; `pub fn diagnosis(rank: Option<i64>, verdict: Verdict) -> &'static str`

- [ ] **Step 1: Write the failing test**

In `src/web/judge.rs` tests:

```rust
    #[test]
    fn the_diagnosis_is_loudest_where_the_ranking_did_worst() {
        // Inverted on purpose. A rank-1 hit is the least informative card of
        // the day; making it the most celebrated would breed agreement with
        // whatever the ranker already thought.
        assert_eq!(diagnosis(Some(0), Verdict::Hit), "found as expected.");
        assert!(diagnosis(Some(13), Verdict::Hit).contains("wrong"));
        assert!(diagnosis(None, Verdict::Hit).contains("find"));
        assert!(diagnosis(None, Verdict::Gap).contains("hole"));
    }

    #[tokio::test]
    async fn the_assignment_search_is_never_captured() {
        // It is composed in full knowledge of the answer. Recording it would
        // feed the dataset exactly the contamination this feature avoids.
        let (app, core) = judge_app_with_event(&["a"], 1).await;
        let before = core.store.feedback_stats().await.unwrap().captured;
        let id = core.store.next_pending().await.unwrap().unwrap().id;
        app.oneshot(get(&format!("/ui/judge/{id}/assign/results?q=anything"), None))
            .await;
        core.background.wait_idle().await;
        assert_eq!(core.store.feedback_stats().await.unwrap().captured, before);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib web::judge`
Expected: compile error — `diagnosis` and the assign routes do not exist.

- [ ] **Step 3: Implement the diagnosis**

In `src/web/judge.rs`:

```rust
/// What the judgement just revealed, said plainly.
///
/// The emphasis runs opposite to intuition: the better the ranking did, the
/// quieter the line. A rank-1 confirmation teaches almost nothing, and an
/// interface that cheers for it is training the operator to agree.
pub fn diagnosis(rank: Option<i64>, verdict: Verdict) -> &'static str {
    match (verdict, rank) {
        (Verdict::Gap, _) => "a hole: your base doesn't know this yet.",
        (Verdict::Discard, _) => "discarded.",
        (Verdict::Hit, None) => "a find: search would never have shown you this.",
        (Verdict::Hit, Some(r)) if r >= 10 => {
            "the ranking got this wrong — this is what we're here for."
        }
        (Verdict::Hit, Some(r)) if r > 0 => "there, but far down. These move the MRR.",
        (Verdict::Hit, _) => "found as expected.",
    }
}
```

Render it above the next card, together with the moved numbers, e.g. `MRR 0.54 → 0.57` — the previous values are read before the verdict is written, the new ones after.

- [ ] **Step 4: Implement the assignment path**

`GET /ui/judge/{id}/assign` renders `_judge_assign.html`: a search box posting to `/ui/judge/{id}/assign/results`, plus a "nothing here fits" button that posts to `/ui/judge/{id}/gap`.

`GET /ui/judge/{id}/assign/results` runs `core.search(&query, Door::Judge)` and renders each result as a button that posts to `/ui/judge/{id}/hit`. This is the only search in the application that must pass `Door::Judge`; add a comment at the call site saying why.

The gap card additionally offers **"write it down now"**, a link to `/ui/capture?title=<the query>` — the loop from "I looked for this and it wasn't there" to a new corpus, in one click. Extend the capture page handler to accept an optional `title` query parameter and pre-fill the title field with it.

- [ ] **Step 5: Add the miss list**

On `/ui/judge`, below the header, render `store.misses(20)` — but only once `stats.judged >= 10`, so the section appears when it has something to say:

```html
{% if !misses.is_empty() %}
<details class="misses">
  <summary class="muted">{{ misses.len() }} queries the ranking got wrong</summary>
  <ul class="mono">
    {% for m in misses %}
    <li>{{ m.query }} — {% match m.rank %}{% when Some with (r) %}rank {{ r + 1 }}{% when None %}not returned{% endmatch %}</li>
    {% endfor %}
  </ul>
</details>
{% endif %}
```

- [ ] **Step 6: Run the tests and commit**

```bash
cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked
git add -A
git commit -m "feat: say what each judgement just revealed"
```

---

### Task 6: Ops integration and retention

**Files:**
- Modify: `src/web/ui.rs` (the `ops` handler and its template struct), `src/web/templates/ops.html`
- Modify: `src/jobs/consolidate.rs` or wherever the periodic sweep runs, to drop expired events

**Interfaces:**
- Consumes: `Store::feedback_stats`, `Store::purge_feedback` (Task 3)
- Produces: route `POST /ui/ops/feedback/purge`; `Store::expire_feedback(&self, retain_days: i64) -> Result<u64>`

- [ ] **Step 1: Write the failing tests**

In `src/store/feedback.rs` tests:

```rust
    #[tokio::test]
    async fn retention_of_zero_keeps_everything() {
        let store = Store::memory().await.unwrap();
        seed(&store, "old", &["a"]).await;
        assert_eq!(store.expire_feedback(0).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn an_event_past_the_retention_window_is_dropped() {
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "old", &["a"]).await;
        sqlx::query("UPDATE search_events SET created_at = ? WHERE id = ?")
            .bind(now() - 40 * 86_400)
            .bind(&id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(store.expire_feedback(30).await.unwrap(), 1);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib store::feedback`
Expected: compile error — `expire_feedback` does not exist.

- [ ] **Step 3: Implement**

```rust
impl Store {
    /// Drop captured searches older than the window. `0` keeps them forever.
    pub async fn expire_feedback(&self, retain_days: i64) -> Result<u64> {
        if retain_days <= 0 {
            return Ok(0);
        }
        Ok(
            sqlx::query("DELETE FROM search_events WHERE created_at < ?")
                .bind(now() - retain_days * 86_400)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }
}
```

Call it from the consolidation sweep, which already runs on a timer — a second ticker for a `DELETE` would be a moving part for nothing. Log the count at `info` when it is non-zero.

- [ ] **Step 4: Add the Ops section**

In `ops.html`, only when `feedback.enabled`:

```html
<section>
  <h2>Feedback</h2>
  <p class="muted">
    Searches are being recorded — {{ feedback.captured }} captured,
    {{ feedback.pending }} unjudged.
    <a href="/ui/judge">Judge them</a>
  </p>
  <form hx-post="/ui/ops/feedback/purge" hx-confirm="Delete every captured search?">
    <button class="danger">Delete all captured searches</button>
  </form>
</section>
```

The purge handler calls `purge_feedback` and re-renders the section.

- [ ] **Step 5: Run the tests and commit**

```bash
cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked
git add -A
git commit -m "feat: say what is being recorded, and offer to forget it"
```

---

### Task 7: Repair the evaluation harness

**Files:**
- Modify: `tests/eval.rs:134-166` (`index`), `:100-120` (the scoring loop)

**Interfaces:**
- Consumes: `Store::insert_artifacts` (returns `Vec<Chunk>` in input order, `src/store/artifacts.rs:171`)
- Produces: nothing outside the test file

- [ ] **Step 1: Write the failing test**

Add to `tests/eval.rs`. It runs without Qdrant by using `MemoryVectors` and the deterministic fake embedder, so it can be part of the ordinary suite:

```rust
/// The harness scored every pair as a miss for as long as it existed: `index`
/// re-inserts the frozen artifacts, `insert_artifacts` assigns fresh ids, and
/// the scoring loop compared against the ids in `artifacts.json`. Nothing could
/// ever match, so every run reported 0.00 — invisible because the benchmark is
/// `#[ignore]`d and returns early without pairs.
///
/// This is a wiring test, not a quality one: the fake embedder is deterministic,
/// so a query equal to an artifact's text embeds identically and must rank first.
#[tokio::test]
async fn a_pair_naming_a_frozen_artifact_can_actually_be_found() {
    use engram::core::test_support::test_core;

    let artifacts = vec![
        FrozenArtifact {
            id: "frozen-1".into(),
            source: "one.txt".into(),
            text: "the smallest addressable unit is a cluster".into(),
            title: Some("cluster".into()),
            category: None,
            tags: vec![],
        },
        FrozenArtifact {
            id: "frozen-2".into(),
            source: "two.txt".into(),
            text: "a journal records intent before the write".into(),
            title: Some("journal".into()),
            category: None,
            tags: vec![],
        },
    ];

    let core = test_core().await;
    let ids = index(&core, &artifacts).await;

    let q = SearchQuery {
        q: "a journal records intent before the write".into(),
        limit: LIMIT,
        tags: vec![],
        category: None,
        mark: false,
        include_deprecated: false,
        include_superseded: false,
    };
    let results = core
        .search_capped(&q, None, engram::store::feedback::Door::Judge)
        .await
        .unwrap();

    let expect = ids
        .get("frozen-2")
        .expect("index must report the id it gave each frozen artifact");
    assert_eq!(
        results.iter().position(|r| &r.artifact_id == expect),
        Some(0),
        "the frozen id was never translated to the inserted one"
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --test eval a_pair_naming_a_frozen_artifact`
Expected: compile error — `index` returns `()`.

- [ ] **Step 3: Make `index` report the translation**

In `tests/eval.rs`, change `index` to return `HashMap<String, String>` (frozen id → inserted id). `insert_artifacts` returns the inserted rows in input order, so zip them with the group:

```rust
/// Returns the map from frozen id to the id the store actually assigned.
///
/// `insert_artifacts` mints a fresh id per artifact, so the ids in
/// `artifacts.json` do not exist in the store being searched. Without this map
/// every pair scores as a miss.
async fn index(core: &Core, artifacts: &[FrozenArtifact]) -> HashMap<String, String> {
    let mut translated = HashMap::new();
    // ... existing grouping and insertion, then, per group:
    let inserted = core.store.insert_artifacts(&src.id, &new).await.unwrap();
    for (frozen, stored) in group.iter().zip(inserted.iter()) {
        translated.insert(frozen.id.clone(), stored.id.clone());
    }
    // ... existing embed call
    translated
}
```

- [ ] **Step 4: Translate in the scoring loop**

In `evaluate_retrieval`, after `index`:

```rust
    let translated = index(&core, &artifacts).await;
```

and where the rank is taken:

```rust
        // `pair.expect` names a frozen id; the store searched knows it under
        // another one.
        let expect = translated
            .get(&pair.expect)
            .expect("every pair was checked against artifacts.json above");
        let rank = results.iter().position(|r| &r.artifact_id == expect);
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --test eval`
Expected: the new test passes; `evaluate_retrieval` still returns early with no corpus.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings
git add tests/eval.rs
git commit -m "fix: the benchmark scored every pair as a miss"
```

---

### Task 8: Export to the harness format

**Files:**
- Create: `src/eval/export.rs`
- Modify: `src/eval/mod.rs` (register, add `save_pairs`), `src/store/artifacts.rs` (add `all_active_artifacts`), `src/main.rs:20-40` (the CLI flag)

**Interfaces:**
- Consumes: `FrozenArtifact`, `EvalPair`, `save_artifacts` (`src/eval/mod.rs:28,46,69`); `Store`
- Produces:
  - `pub fn save_pairs(dir: &Path, pairs: &[EvalPair]) -> Result<()>` in `src/eval/mod.rs`
  - `Store::all_active_artifacts(&self) -> Result<Vec<(crate::store::artifacts::Chunk, String)>>` — artifact plus its corpus title
  - `pub async fn export(store: &Store, dir: &Path) -> anyhow::Result<(usize, usize)>` in `src/eval/export.rs`, returning (artifacts, pairs)
  - CLI: `engram --export-eval <DIR>`

- [ ] **Step 1: Write the failing test**

In `src/eval/export.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_judged_hit_becomes_a_pair_and_its_artifact_is_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::memory().await.unwrap();
        let (artifact_id, _) = seed_one_artifact(&store).await;
        let event = record_one_search(&store, "how do I read a deleted entry", &artifact_id).await;
        store.judge_hit(&event, &artifact_id).await.unwrap();

        let (artifacts, pairs) = export(&store, dir.path()).await.unwrap();
        assert_eq!((artifacts, pairs), (1, 1));

        let frozen = crate::eval::load_artifacts(dir.path()).unwrap();
        assert_eq!(frozen[0].id, artifact_id, "ids must stay the production ones");
        let loaded = crate::eval::load_pairs(dir.path()).unwrap();
        assert_eq!(loaded[0].expect, artifact_id);
        assert_eq!(loaded[0].query, "how do I read a deleted entry");
    }

    #[tokio::test]
    async fn a_pair_pointing_at_a_deleted_artifact_is_left_out() {
        // Scored as a miss it would look like a ranking problem forever.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::memory().await.unwrap();
        let event = record_one_search(&store, "gone", "no-such-artifact").await;
        store.judge_hit(&event, "no-such-artifact").await.unwrap();

        let (_, pairs) = export(&store, dir.path()).await.unwrap();
        assert_eq!(pairs, 0);
    }

    #[tokio::test]
    async fn gaps_and_discards_never_become_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::memory().await.unwrap();
        let g = record_one_search(&store, "nothing about this", "x").await;
        store.judge(&g, Verdict::Gap).await.unwrap();

        let (_, pairs) = export(&store, dir.path()).await.unwrap();
        assert_eq!(pairs, 0);
    }
}
```

`seed_one_artifact` and `record_one_search` are two short helpers in the same module; write them with `insert_corpus` + `insert_artifacts` + `record_search` as Task 2's test helpers do.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib eval::export`
Expected: compile error — the module does not exist.

- [ ] **Step 3: Implement `save_pairs` and `all_active_artifacts`**

In `src/eval/mod.rs`, mirroring `save_artifacts`:

```rust
pub fn save_pairs(dir: &Path, pairs: &[EvalPair]) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = pairs_path(dir);
    let json = serde_json::to_string_pretty(pairs)?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))
}
```

In `src/store/artifacts.rs`, beside the other readers:

```rust
    /// Every artifact an ordinary search could return, with the title of the
    /// corpus it came from. Superseded and deprecated stay out, so a benchmark
    /// built from this sees the same base the search page does.
    pub async fn all_active_artifacts(&self) -> Result<Vec<(Chunk, String)>> {
```

Implement it with a join on `corpora`, reusing the existing row-to-`Chunk` mapping (`src/store/artifacts.rs:154`), falling back to the corpus id when it has no title.

- [ ] **Step 4: Implement the export**

`src/eval/export.rs`:

```rust
//! Writing the evaluation corpus straight out of the live database.
//!
//! Cheaper and steadier than `eval-prepare`: the artifacts have already been
//! synthesised, so this costs no completions, and it keeps their production
//! ids — which means re-exporting does not invalidate the pairs, the way
//! re-freezing does.

use crate::eval::{EvalPair, FrozenArtifact, save_artifacts, save_pairs};
use crate::store::Store;
use anyhow::Result;
use sqlx::Row;
use std::path::Path;

pub async fn export(store: &Store, dir: &Path) -> Result<(usize, usize)> {
    let artifacts = store.all_active_artifacts().await?;
    let known: std::collections::HashSet<String> =
        artifacts.iter().map(|(c, _)| c.id.clone()).collect();

    let frozen: Vec<FrozenArtifact> = artifacts
        .iter()
        .map(|(c, corpus)| FrozenArtifact {
            id: c.id.clone(),
            source: corpus.clone(),
            text: c.text.clone(),
            title: c.title.clone(),
            category: c.category.clone(),
            tags: c.tags.clone(),
        })
        .collect();

    let rows = sqlx::query(
        "SELECT query, expect_id, door, judged_at FROM search_events
         WHERE verdict = 'hit' AND expect_id IS NOT NULL
         ORDER BY judged_at",
    )
    .fetch_all(&store.pool)
    .await?;

    let mut pairs = Vec::new();
    let mut dropped = 0usize;
    for r in &rows {
        let expect: String = r.get("expect_id");
        if !known.contains(&expect) {
            dropped += 1;
            continue;
        }
        pairs.push(EvalPair {
            query: r.get("query"),
            expect,
            note: Some(format!(
                "{} · judged {}",
                r.get::<String, _>("door"),
                r.get::<i64, _>("judged_at")
            )),
        });
    }
    if dropped > 0 {
        tracing::warn!(dropped, "pairs skipped: their artifact no longer exists");
    }

    save_artifacts(dir, &frozen)?;
    save_pairs(dir, &pairs)?;
    Ok((frozen.len(), pairs.len()))
}
```

- [ ] **Step 5: Wire the CLI flag**

In `src/main.rs`, beside `--reindex`:

```rust
    /// Write artifacts.json and pairs.json for the evaluation harness and exit.
    #[arg(long, value_name = "DIR")]
    export_eval: Option<std::path::PathBuf>,
```

Handle it after the store is opened and before the server starts, printing what was written and returning:

```rust
    if let Some(dir) = &args.export_eval {
        let (artifacts, pairs) = engram::eval::export::export(&store, dir).await?;
        println!(
            "wrote {artifacts} artifacts and {pairs} pairs to {}",
            dir.display()
        );
        return Ok(());
    }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test --locked`
Expected: all pass.

- [ ] **Step 7: Update the documentation**

- README: a short section after "Asking for something" describing capture, judging and `--export-eval` in three sentences, plus the `feedback.*` config row from Task 2.
- `ROADMAP.md`: replace the paragraph calling the eval harness "unpopulated by design" — the pairs now come from real use, so the justification no longer holds. Say what does hold: the corpus stays on the operator's machine, and the harness is the only figure comparable across months.

- [ ] **Step 8: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked
git add -A
git commit -m "feat: hand the judged pairs to the benchmark"
```

---

## Self-review

**Spec coverage.** Capture with prefix coalescing (Tasks 1–2), the door parameter and the `ask` exclusion (Task 2), the wide pool and `shown` flag (Tasks 1–2), verdicts and skip ordering (Task 3), field metrics and the three-name discipline (Task 3, header labelled "recall@10 / MRR" on the judging page only), the judging card without ranks or scores (Task 4), keyboard-first operation (Task 4), the diagnosis line with inverted emphasis (Task 5), the `N` path with `Door::Judge` and the gap-to-capture jump (Task 5), counters and levels (Tasks 4–5: counters in the header, the miss list gated at 10 judgements; the 50 and 100 thresholds belong to the tuning plan), Ops visibility and purge (Task 6), retention (Task 6), the harness repair (Task 7), export (Task 8), documentation (Tasks 2 and 8).

Not covered here, by design: `Tunables`, the sweep, proposals, `auto_apply`, and the `tuning_*` tables. Those are stage 5 and get their own plan and their own migration.

**Placeholders.** None: every code step carries the code, and the two places that describe rather than show — the judging handlers in Task 4 step 4 and `all_active_artifacts` in Task 8 step 3 — name the exact function, its signature, and the existing code to mirror.

**Type consistency.** `Door`, `NewEvent`, `NewCandidate`, `Verdict`, `PendingEvent`, `Candidate`, `Stats`, `Miss` are defined once in `src/store/feedback.rs` and referred to by those names throughout. `search`/`search_timed`/`search_capped` gain the `door` parameter in Task 2 and are called with it in Tasks 5 and 7. `feedback_stats` returns `Stats` in Task 3 and is consumed under that name in Tasks 4, 5, 6 and the export tests.
