# Associative Memory Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give engram two usage-learned associative mechanisms — Hebbian links between co-retrieved artifacts and a decaying per-artifact activation — surfaced only in the search results and the detail pane, without changing what is stored.

**Architecture:** A new SQLite table `artifact_links` holds pair weights that are bumped by a background sweep replaying `search_events`, and read through a lazy decay (never decayed in place). Two new columns on `artifacts` hold activation the same way. The query path gains exactly one indexed SQLite read, which can only add: bounded priming of the ranked order and one hop of association appended outside `limit`. A sparse `LinkJudge` unit names strong cross-corpus relations through the existing job queue.

**Tech Stack:** Rust, sqlx/SQLite, axum + askama (web UI), tokio tickers, the existing `jobs` queue and `infer::Completer` judge role.

**Spec:** `docs/superpowers/specs/2026-08-16-associative-memory-design.md`

## Global Constraints

Copied verbatim from the spec; every task's requirements implicitly include these.

- **The trace is fixed; access is plastic.** Content is verbatim and never changes silently. Everything about how it is found — associations, activation, what surfaces first — learns from use, within visible bounds.
- **Bounded.** Priming moves a hit at most `prime_lift` positions and never displaces rank 1. Association adds hits beside the ranked list; it never removes or reorders one.
- **Visible.** A primed hit says so; an associated hit says which hit recalled it. Nothing about the order is silent.
- **One-way.** Associated hits do not feed the learning that produced them. Priming does not raise activation by more than an ordinary retrieval would.
- **No inference in the query path.** The judge runs in the background, on few links, through the existing queue and pacing.
- Additive schema only: `ADD COLUMN` with defaults and new tables. No `DROP COLUMN`, no column retyped, no route or config key deleted.
- The layer crossing is optional: if the SQLite read fails, search returns exactly what it returns today, with one warning.
- CI gates every task: `cargo fmt --all --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked`.
- Default values, exact: `associate.enabled = true`, `interval_mins = 30`, `half_life_days = 30`, `prune_below = 0.5`, `show_min = 2.0`, `judge_min = 4.0`, `judge_min_queries = 3`, `judge_per_sweep = 10`, `spread_from = 3`, `spread_max = 3`, `prime_margin = 0.5`, `prime_lift = 2`; `activation.half_life_days = 14`, `retrieved = 1.0`, `opened = 0.5`, `confirmed = 3.0`.

---

## Task 1: The link table, its types, and the watermarks

**Files:**
- Create: `src/store/links.rs`
- Modify: `src/store/schema.sql` (append two tables before the Auth section)
- Modify: `src/store/mod.rs:1-10` (module declaration)
- Test: `src/store/links.rs` (inline `mod tests`, the repo's convention)

**Interfaces:**
- Consumes: `crate::store::{Store, now}`, `crate::error::Result`.
- Produces: `LinkState` (`Learning|Related|Unrelated|Dismissed`, `as_str`/`parse`), `Cue { q: String, n: i64 }`, `Link { a_id, b_id, weight: f64, bumped_at: i64, queries: i64, cues: Vec<Cue>, state: LinkState, reason: Option<String>, judged_rev_a: Option<i64>, judged_rev_b: Option<i64>, judge_attempts: i64, created_at: i64 }`, `canonical(&str,&str) -> (&str,&str)`, `normalize_query(&str) -> String`, `decayed(f64, i64, i64, f64) -> f64`, `MAX_CUES: usize`, and on `Store`: `bump_link`, `get_link`, `meta_get`, `meta_set`.

- [ ] **Step 1: Add the two tables to the schema**

Append to `src/store/schema.sql`, immediately before the `-- ── Auth ──` section. One column per line — `migrate`'s checker parses this file.

```sql
-- ── Association ──────────────────────────────────────────────────────────────
-- Two artifacts that keep being retrieved by the same searches. The other half
-- of relatedness: `artifact_pairs` is about two texts saying the same thing,
-- this is about two texts being needed together. A pair can be both — filed by
-- `Relate` at 0.89 and judged distinct, and co-retrieved and related — and one
-- row cannot hold two verdicts, so they are separate tables.
CREATE TABLE IF NOT EXISTS artifact_links (
  a_id        TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  b_id        TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  -- Strength as of `bumped_at`. Read through decay; never decayed in place, so
  -- learning is one UPDATE and forgetting costs no writes at all.
  weight      REAL NOT NULL,
  bumped_at   INTEGER NOT NULL,
  -- Distinct normalised query texts that bound this pair. What separates a
  -- link from one search typed twice.
  queries     INTEGER NOT NULL DEFAULT 1,
  -- Up to three binding queries with counts, JSON: [{"q":..,"n":..}].
  cues        TEXT NOT NULL DEFAULT '[]',
  -- 'learning' | 'related' | 'unrelated' | 'dismissed'
  state       TEXT NOT NULL DEFAULT 'learning',
  -- The judge's one line, for `related`.
  reason      TEXT,
  -- Revisions the judge read. A re-embed of either side reopens the verdict:
  -- the text changed under it.
  judged_rev_a INTEGER,
  judged_rev_b INTEGER,
  judge_attempts INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY (a_id, b_id),
  CHECK (a_id < b_id)
);
CREATE INDEX IF NOT EXISTS idx_links_b ON artifact_links(b_id);
CREATE INDEX IF NOT EXISTS idx_links_state ON artifact_links(state, weight DESC);

-- Cursors that have no row to live on. Two keys so far:
-- `associate.events_after` and `associate.judged_after`.
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
```

- [ ] **Step 2: Declare the module**

In `src/store/mod.rs`, add to the module list (alphabetical, after `jobs`):

```rust
pub mod lineage;
pub mod links;
pub mod pairs;
```

- [ ] **Step 3: Write the failing tests**

Create `src/store/links.rs` with only this test module plus `use super::*;` at the top of it — the implementation comes in step 5.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::artifacts::NewArtifact;

    /// Two artifacts in one corpus, returned as their ids.
    async fn two(store: &Store) -> (String, String) {
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = ["alpha", "beta"]
            .iter()
            .enumerate()
            .map(|(i, t)| NewArtifact {
                ordinal: i as i64,
                text: (*t).into(),
                corpus_span: None,
                title: Some((*t).into()),
                category: None,
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        let made = store.insert_artifacts(&src.id, &new).await.unwrap();
        (made[0].id.clone(), made[1].id.clone())
    }

    #[test]
    fn a_pair_is_filed_the_same_way_round_however_it_is_named() {
        // The primary key is (a_id, b_id) with a CHECK that a < b, so a lookup
        // by either side is two indexed reads and there is no "which way round"
        // bug to have.
        assert_eq!(canonical("b", "a"), ("a", "b"));
        assert_eq!(canonical("a", "b"), ("a", "b"));
    }

    #[test]
    fn a_query_is_the_same_cue_however_it_was_typed() {
        assert_eq!(normalize_query("  Loop   Device \n"), "loop device");
    }

    #[test]
    fn weight_halves_over_one_half_life() {
        // Lazy decay is what makes forgetting free: no sweep walks every row to
        // subtract from it, so this function is the only place decay happens.
        let day = 86_400;
        assert!((decayed(4.0, 0, 30 * day, 30.0) - 2.0).abs() < 1e-9);
        assert!((decayed(4.0, 0, 60 * day, 30.0) - 1.0).abs() < 1e-9);
        // Not yet moved, and never grown by a clock running backwards.
        assert!((decayed(4.0, 100, 100, 30.0) - 4.0).abs() < 1e-9);
        assert!((decayed(4.0, 100, 0, 30.0) - 4.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn bumping_folds_the_decay_in_rather_than_adding_to_a_stale_number() {
        // `weight` means "strength as of bumped_at". Adding to it without
        // folding the decay in would make a link that was strong a year ago and
        // used once today as strong as one used constantly.
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        let day = 86_400;
        store
            .bump_link(&a, &b, 4.0, Some("fat32"), 30.0, 0)
            .await
            .unwrap();
        store
            .bump_link(&b, &a, 1.0, Some("fat32"), 30.0, 30 * day)
            .await
            .unwrap();

        let l = store.get_link(&a, &b).await.unwrap().expect("the link");
        assert!((l.weight - 3.0).abs() < 1e-6, "weight was {}", l.weight);
        assert_eq!(l.bumped_at, 30 * day);
        // Named the other way round the second time, and still one row.
        assert_eq!(l.a_id, a.min(b.clone()));
    }

    #[tokio::test]
    async fn the_same_query_twice_is_one_binding_query() {
        // What separates a link from one search typed twice. The cue count
        // still climbs, because that is how the top three are chosen.
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        for _ in 0..3 {
            store
                .bump_link(&a, &b, 1.0, Some("fat32"), 30.0, 0)
                .await
                .unwrap();
        }
        store
            .bump_link(&a, &b, 1.0, Some("ntfs"), 30.0, 0)
            .await
            .unwrap();

        let l = store.get_link(&a, &b).await.unwrap().unwrap();
        assert_eq!(l.queries, 2);
        assert_eq!(l.cues[0].q, "fat32");
        assert_eq!(l.cues[0].n, 3);
        assert_eq!(l.cues.len(), 2);
    }

    #[tokio::test]
    async fn only_three_binding_queries_are_kept_and_the_busiest_lead() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        for q in ["one", "two", "three", "four"] {
            store.bump_link(&a, &b, 1.0, Some(q), 30.0, 0).await.unwrap();
        }
        store.bump_link(&a, &b, 1.0, Some("two"), 30.0, 0).await.unwrap();

        let l = store.get_link(&a, &b).await.unwrap().unwrap();
        assert_eq!(l.cues.len(), MAX_CUES);
        assert_eq!(l.cues[0].q, "two", "the busiest cue must lead");
    }

    #[tokio::test]
    async fn an_artifact_linking_to_itself_is_not_a_link() {
        let store = Store::memory().await.unwrap();
        let (a, _) = two(&store).await;
        store.bump_link(&a, &a, 1.0, Some("q"), 30.0, 0).await.unwrap();
        assert!(store.get_link(&a, &a).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_watermark_reads_back_what_was_written_and_nothing_otherwise() {
        let store = Store::memory().await.unwrap();
        assert_eq!(store.meta_get("associate.events_after").await.unwrap(), None);
        store.meta_set("associate.events_after", "42").await.unwrap();
        store.meta_set("associate.events_after", "99").await.unwrap();
        assert_eq!(
            store.meta_get("associate.events_after").await.unwrap(),
            Some("99".to_string())
        );
    }
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test --lib store::links`
Expected: compile failure — `cannot find function canonical`, `no method named bump_link`.

- [ ] **Step 5: Write the implementation**

Put this above the `mod tests` block in `src/store/links.rs`.

```rust
//! What was reached together.
//!
//! `artifact_pairs` is about two texts saying the same thing; this is about two
//! texts being *needed* together — the config passage and the troubleshooting
//! passage for one subsystem are strangers to the embedding and inseparable to
//! the person who needed both to answer one question.
//!
//! Every strength here is stored as a value and the stamp it was true at, and
//! read through `decayed`. Nothing is ever decayed in place: learning is one
//! UPDATE and forgetting costs no writes, which is what lets a sweep run every
//! half hour on a base of any size.

use super::{Store, now};
use crate::error::Result;
use sqlx::Row;

/// Binding queries kept per link. Three is what a person reads in the pane;
/// the count of *distinct* ones is `queries`, which is not bounded by this.
pub const MAX_CUES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Bound by use, nothing has ruled on it. Shown, prunable, judgeable.
    Learning,
    /// The judge named the relation. Shown with its reason, and never pruned by
    /// decay: a verified relation is about content, not use.
    Related,
    /// A coincidence of retrieval. Kept so it is not asked again, hidden from
    /// the pane, reopened only if either text changes.
    Unrelated,
    /// The operator said no. Never shown, never judged, never pruned.
    Dismissed,
}

impl LinkState {
    pub fn as_str(&self) -> &'static str {
        match self {
            LinkState::Learning => "learning",
            LinkState::Related => "related",
            LinkState::Unrelated => "unrelated",
            LinkState::Dismissed => "dismissed",
        }
    }
    pub fn parse(s: &str) -> LinkState {
        match s {
            "related" => LinkState::Related,
            "unrelated" => LinkState::Unrelated,
            "dismissed" => LinkState::Dismissed,
            _ => LinkState::Learning,
        }
    }
}

/// One binding query and how often it bound this pair.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Cue {
    pub q: String,
    pub n: i64,
}

#[derive(Debug, Clone)]
pub struct Link {
    pub a_id: String,
    pub b_id: String,
    /// Strength as of `bumped_at`. Read it through `decayed`, never directly.
    pub weight: f64,
    pub bumped_at: i64,
    pub queries: i64,
    pub cues: Vec<Cue>,
    pub state: LinkState,
    pub reason: Option<String>,
    pub judged_rev_a: Option<i64>,
    pub judged_rev_b: Option<i64>,
    pub judge_attempts: i64,
    pub created_at: i64,
}

/// The pair in the order the table stores it. `a_id < b_id` is a CHECK, so this
/// is not a convention that can be forgotten at one call site.
pub fn canonical<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Two spellings of one question are one cue: lowercased, whitespace collapsed.
pub fn normalize_query(q: &str) -> String {
    q.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Strength now, from strength then. `half_life_days <= 0` turns decay off.
///
/// A clock that moved backwards — a restored database, an NTP correction —
/// must not *grow* a weight, so the elapsed time is floored at zero.
pub fn decayed(weight: f64, bumped_at: i64, at: i64, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return weight;
    }
    let elapsed = (at - bumped_at).max(0) as f64;
    weight * 2f64.powf(-elapsed / (half_life_days * 86_400.0))
}

pub(crate) fn row_to_link(r: &sqlx::sqlite::SqliteRow) -> Link {
    Link {
        a_id: r.get("a_id"),
        b_id: r.get("b_id"),
        weight: r.get("weight"),
        bumped_at: r.get("bumped_at"),
        queries: r.get("queries"),
        cues: serde_json::from_str(&r.get::<String, _>("cues")).unwrap_or_default(),
        state: LinkState::parse(r.get::<String, _>("state").as_str()),
        reason: r.get("reason"),
        judged_rev_a: r.get("judged_rev_a"),
        judged_rev_b: r.get("judged_rev_b"),
        judge_attempts: r.get("judge_attempts"),
        created_at: r.get("created_at"),
    }
}

/// Fold one query into the cue list. Returns whether it was new to this link.
///
/// Only the busiest `MAX_CUES` survive, and a cue that falls off the end can be
/// counted as new again later — so `queries` is a floor on the number of
/// distinct questions that bound this pair rather than an exact count. Exactness
/// would cost a second table for a number that only gates the judge.
fn bump_cue(cues: &mut Vec<Cue>, q: &str) -> bool {
    if let Some(c) = cues.iter_mut().find(|c| c.q == q) {
        c.n += 1;
        return false;
    }
    cues.push(Cue {
        q: q.to_string(),
        n: 1,
    });
    cues.sort_by(|x, y| y.n.cmp(&x.n));
    cues.truncate(MAX_CUES);
    true
}

impl Store {
    /// Strengthen the link between two artifacts, folding the decay in.
    ///
    /// `cue` is the normalised query that bound them, where there is one. A
    /// bump with no cue — a confirmation replayed for an event whose
    /// co-appearance was already folded in — strengthens without claiming a new
    /// binding question.
    ///
    /// One transaction per pair. An event with ten shown candidates is 45 of
    /// these, which is a few milliseconds of local SQLite and no network at all.
    pub async fn bump_link(
        &self,
        a: &str,
        b: &str,
        delta: f64,
        cue: Option<&str>,
        half_life_days: f64,
        at: i64,
    ) -> Result<()> {
        // An artifact is not linked to itself, and the CHECK would refuse it
        // anyway — caught here so a caller enumerating pairs need not.
        if a == b {
            return Ok(());
        }
        let (a, b) = canonical(a, b);
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT weight, bumped_at, queries, cues FROM artifact_links
              WHERE a_id = ? AND b_id = ?",
        )
        .bind(a)
        .bind(b)
        .fetch_optional(&mut *tx)
        .await?;

        match row {
            Some(r) => {
                let mut cues: Vec<Cue> =
                    serde_json::from_str(&r.get::<String, _>("cues")).unwrap_or_default();
                let fresh = cue.is_some_and(|q| bump_cue(&mut cues, q));
                let weight =
                    decayed(r.get("weight"), r.get("bumped_at"), at, half_life_days) + delta;
                sqlx::query(
                    "UPDATE artifact_links
                        SET weight = ?, bumped_at = ?, queries = ?, cues = ?
                      WHERE a_id = ? AND b_id = ?",
                )
                .bind(weight)
                .bind(at)
                .bind(r.get::<i64, _>("queries") + i64::from(fresh))
                .bind(serde_json::to_string(&cues).unwrap_or_else(|_| "[]".into()))
                .bind(a)
                .bind(b)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                let mut cues = Vec::new();
                if let Some(q) = cue {
                    bump_cue(&mut cues, q);
                }
                sqlx::query(
                    "INSERT INTO artifact_links
                       (a_id, b_id, weight, bumped_at, queries, cues, state, created_at)
                     VALUES (?, ?, ?, ?, ?, ?, 'learning', ?)",
                )
                .bind(a)
                .bind(b)
                .bind(delta)
                .bind(at)
                .bind(cues.len() as i64)
                .bind(serde_json::to_string(&cues).unwrap_or_else(|_| "[]".into()))
                .bind(now())
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// One link, whichever way round it is named.
    pub async fn get_link(&self, a: &str, b: &str) -> Result<Option<Link>> {
        let (a, b) = canonical(a, b);
        Ok(sqlx::query("SELECT * FROM artifact_links WHERE a_id = ? AND b_id = ?")
            .bind(a)
            .bind(b)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(row_to_link))
    }

    pub async fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT value FROM meta WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO meta (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib store::links`
Expected: PASS, 7 tests.

- [ ] **Step 7: Check the whole suite still passes**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: PASS. In particular `store::tests::a_fresh_file_database_gets_the_whole_schema` and `the_schema_parses_into_the_columns_it_declares` must still pass — they read `schema.sql` back.

- [ ] **Step 8: Commit**

```bash
git add src/store/links.rs src/store/mod.rs src/store/schema.sql
git commit -m "feat(links): store co-retrieval links and their watermarks"
```

---

## Task 2: Reading links back — lists, pruning, reopening, verdicts

**Files:**
- Modify: `src/store/links.rs` (append to the `impl Store` block and the test module)
- Test: `src/store/links.rs`

**Interfaces:**
- Consumes: everything Task 1 produced.
- Produces: on `Store` — `links_from(anchors: &[String], states: &[LinkState], half_life_days: f64, at: i64, min_weight: f64) -> Result<Vec<LinkedTo>>`, `links_to_judge(min_weight: f64, min_queries: i64, half_life_days: f64, at: i64, limit: i64) -> Result<Vec<Link>>`, `prune_learning_links(below: f64, half_life_days: f64, at: i64, scan_limit: i64) -> Result<u64>`, `reopen_stale_judged_links(limit: i64) -> Result<u64>`, `set_link_state(a, b, state, reason: Option<&str>, judged_revs: Option<(i64, i64)>) -> Result<()>`, `record_link_judge_attempt(a, b) -> Result<i64>`, `link_counts() -> Result<LinkCounts>`; and the struct `LinkedTo { via: String, other: String, weight: f64, state: LinkState, reason: Option<String>, cues: Vec<Cue>, cross_corpus: bool }` plus `LinkCounts { total: i64, related: i64, judge_queue: i64 }`.

- [ ] **Step 1: Write the failing tests**

Append to the `mod tests` block in `src/store/links.rs`.

```rust
    /// Two artifacts in two different corpora, so a link between them is a
    /// cross-corpus one — the only kind the judge is ever asked about.
    async fn two_corpora(store: &Store) -> (String, String) {
        let mut ids = Vec::new();
        for (raw, text) in [("raw one", "alpha"), ("raw two", "beta")] {
            let src = store.insert_corpus(raw, "web", None).await.unwrap();
            let made = store
                .insert_artifacts(
                    &src.id,
                    &[NewArtifact {
                        ordinal: 0,
                        text: text.into(),
                        corpus_span: None,
                        title: Some(text.into()),
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    }],
                )
                .await
                .unwrap();
            ids.push(made[0].id.clone());
        }
        (ids[0].clone(), ids[1].clone())
    }

    #[tokio::test]
    async fn a_link_is_found_from_either_of_its_ends() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        store.bump_link(&a, &b, 3.0, Some("q"), 30.0, 0).await.unwrap();

        for anchor in [&a, &b] {
            let out = store
                .links_from(&[anchor.clone()], &[LinkState::Learning], 30.0, 0, 2.0)
                .await
                .unwrap();
            assert_eq!(out.len(), 1, "anchored at {anchor}");
            assert_eq!(&out[0].via, anchor);
            assert_ne!(&out[0].other, anchor);
        }
    }

    #[tokio::test]
    async fn a_link_below_the_threshold_once_decayed_is_not_shown() {
        // The stored weight is strength as of `bumped_at`. Filtering on it
        // directly would keep showing a link that has not been used in a year.
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        store.bump_link(&a, &b, 3.0, Some("q"), 30.0, 0).await.unwrap();

        let now = store
            .links_from(&[a.clone()], &[LinkState::Learning], 30.0, 0, 2.0)
            .await
            .unwrap();
        assert_eq!(now.len(), 1);
        let later = store
            .links_from(&[a.clone()], &[LinkState::Learning], 30.0, 60 * 86_400, 2.0)
            .await
            .unwrap();
        assert!(later.is_empty(), "a link decayed to 0.75 was still shown");
    }

    #[tokio::test]
    async fn a_link_whose_other_side_is_hidden_is_not_shown() {
        // Superseded and deprecated endpoints are filtered at read time, so
        // undoing a merge brings its links back without a write.
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        store.bump_link(&a, &b, 3.0, Some("q"), 30.0, 0).await.unwrap();
        store
            .set_artifact_status(&b, crate::store::artifacts::ArtifactStatus::Deprecated)
            .await
            .unwrap();

        assert!(
            store
                .links_from(&[a], &[LinkState::Learning], 30.0, 0, 2.0)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_dismissed_link_is_never_returned() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        store.bump_link(&a, &b, 9.0, Some("q"), 30.0, 0).await.unwrap();
        store
            .set_link_state(&a, &b, LinkState::Dismissed, None, None)
            .await
            .unwrap();

        assert!(
            store
                .links_from(&[a], &[LinkState::Learning, LinkState::Related], 30.0, 0, 0.0)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn only_a_strong_cross_corpus_link_is_offered_to_the_judge() {
        // Two passages of one document being related is not information, so a
        // same-corpus link is shown and never judged.
        let store = Store::memory().await.unwrap();
        let (same_a, same_b) = two(&store).await;
        let (cross_a, cross_b) = two_corpora(&store).await;
        for (a, b) in [(&same_a, &same_b), (&cross_a, &cross_b)] {
            for q in ["one", "two", "three"] {
                store.bump_link(a, b, 2.0, Some(q), 30.0, 0).await.unwrap();
            }
        }

        let armed = store.links_to_judge(4.0, 3, 30.0, 0, 10).await.unwrap();
        assert_eq!(armed.len(), 1, "got {armed:?}");
        assert_eq!(canonical(&cross_a, &cross_b), (armed[0].a_id.as_str(), armed[0].b_id.as_str()));
    }

    #[tokio::test]
    async fn a_link_bound_by_too_few_questions_is_not_judged() {
        // One question asked six times is one question. The judge is the only
        // thing here that costs a model call, and this is what bounds it.
        let store = Store::memory().await.unwrap();
        let (a, b) = two_corpora(&store).await;
        for _ in 0..6 {
            store.bump_link(&a, &b, 1.0, Some("same"), 30.0, 0).await.unwrap();
        }
        assert!(store.links_to_judge(4.0, 3, 30.0, 0, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pruning_drops_faded_learning_links_and_spares_judged_ones() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        let (c, d) = two_corpora(&store).await;
        store.bump_link(&a, &b, 1.0, Some("q"), 30.0, 0).await.unwrap();
        store.bump_link(&c, &d, 1.0, Some("q"), 30.0, 0).await.unwrap();
        store
            .set_link_state(&c, &d, LinkState::Related, Some("both about disks"), Some((0, 0)))
            .await
            .unwrap();

        // A year on, both have decayed to almost nothing.
        let dropped = store
            .prune_learning_links(0.5, 30.0, 365 * 86_400, 5_000)
            .await
            .unwrap();
        assert_eq!(dropped, 1);
        assert!(store.get_link(&a, &b).await.unwrap().is_none());
        assert!(
            store.get_link(&c, &d).await.unwrap().is_some(),
            "a verified relation is about content, not use"
        );
    }

    #[tokio::test]
    async fn a_judged_link_reopens_when_either_text_changes_under_it() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two_corpora(&store).await;
        store.bump_link(&a, &b, 5.0, Some("q"), 30.0, 0).await.unwrap();
        store
            .set_link_state(&a, &b, LinkState::Related, Some("a reason"), Some((0, 0)))
            .await
            .unwrap();

        assert_eq!(store.reopen_stale_judged_links(100).await.unwrap(), 0);

        store.update_artifact_text(&a, "alpha, rewritten").await.unwrap();
        assert_eq!(store.reopen_stale_judged_links(100).await.unwrap(), 1);
        let l = store.get_link(&a, &b).await.unwrap().unwrap();
        assert_eq!(l.state, LinkState::Learning);
        assert_eq!(l.reason, None, "the judge read text that no longer exists");
    }

    #[tokio::test]
    async fn the_counts_say_how_many_links_there_are_and_how_many_are_named() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        let (c, d) = two_corpora(&store).await;
        store.bump_link(&a, &b, 1.0, Some("q"), 30.0, 0).await.unwrap();
        store.bump_link(&c, &d, 9.0, Some("q"), 30.0, 0).await.unwrap();
        store
            .set_link_state(&c, &d, LinkState::Related, Some("why"), Some((0, 0)))
            .await
            .unwrap();

        let n = store.link_counts().await.unwrap();
        assert_eq!((n.total, n.related), (2, 1));
    }

    #[tokio::test]
    async fn a_judge_attempt_is_counted_and_reported_back() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        store.bump_link(&a, &b, 1.0, Some("q"), 30.0, 0).await.unwrap();
        assert_eq!(store.record_link_judge_attempt(&a, &b).await.unwrap(), 1);
        assert_eq!(store.record_link_judge_attempt(&b, &a).await.unwrap(), 2);
    }
```

Two store methods used above already exist; confirm their exact names before running — `set_artifact_status` and `update_artifact_text` in `src/store/artifacts.rs`. If either differs, use the real one (the point of the test is the status filter and the `embed_rev` bump, not the setter's name).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib store::links`
Expected: FAIL — `no method named links_from`.

- [ ] **Step 3: Write the implementation**

Append to the `impl Store` block in `src/store/links.rs`, and put the two structs above it.

```rust
/// One end of a link, seen from an anchor that is already in the result list.
#[derive(Debug, Clone)]
pub struct LinkedTo {
    /// The ranked hit that recalled it.
    pub via: String,
    pub other: String,
    /// Already decayed to the caller's clock. There is no stale number here.
    pub weight: f64,
    pub state: LinkState,
    pub reason: Option<String>,
    pub cues: Vec<Cue>,
    /// The two sides come from different documents — or one of them is a merge,
    /// which belongs to no document and always counts as differing.
    pub cross_corpus: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LinkCounts {
    pub total: i64,
    pub related: i64,
    /// Links whose judgement is queued or running.
    pub judge_queue: i64,
}
```

```rust
    /// Every live link out of these anchors, strongest first.
    ///
    /// One statement per anchor rather than one `IN` over all of them: the
    /// anchor list is `spread_from` long (three), the repo builds no SQL
    /// strings from data, and a link has to be found from either end — which is
    /// an `OR` over two indexed columns, not a set membership test.
    ///
    /// The endpoint's status is joined rather than stored, so undoing a merge
    /// brings its links back without a write.
    pub async fn links_from(
        &self,
        anchors: &[String],
        states: &[LinkState],
        half_life_days: f64,
        at: i64,
        min_weight: f64,
    ) -> Result<Vec<LinkedTo>> {
        let allowed: Vec<&str> = states.iter().map(|s| s.as_str()).collect();
        let mut out: Vec<LinkedTo> = Vec::new();
        for anchor in anchors {
            let rows = sqlx::query(
                // `weight >= ?` is a necessary condition, not the answer: decay
                // only ever lowers it, so this narrows the scan with the index
                // and the exact test happens below in Rust. Doing it in SQL
                // would need a `pow` that is not in every SQLite build.
                "SELECT l.a_id AS a_id, l.b_id AS b_id, l.weight AS weight,
                        l.bumped_at AS bumped_at, l.state AS state, l.reason AS reason,
                        l.cues AS cues,
                        a.corpus_id AS a_corpus, b.corpus_id AS b_corpus
                   FROM artifact_links l
                   JOIN artifacts a ON a.id = l.a_id
                   JOIN artifacts b ON b.id = l.b_id
                  WHERE (l.a_id = ? OR l.b_id = ?)
                    AND l.weight >= ?
                    AND a.status = 'active' AND a.superseded_by IS NULL
                    AND b.status = 'active' AND b.superseded_by IS NULL
                  ORDER BY l.weight DESC",
            )
            .bind(anchor)
            .bind(anchor)
            .bind(min_weight)
            .fetch_all(&self.pool)
            .await?;

            for r in &rows {
                let state = LinkState::parse(r.get::<String, _>("state").as_str());
                if !allowed.contains(&state.as_str()) {
                    continue;
                }
                let weight = decayed(r.get("weight"), r.get("bumped_at"), at, half_life_days);
                if weight < min_weight {
                    continue;
                }
                let a_id: String = r.get("a_id");
                let b_id: String = r.get("b_id");
                let other = if &a_id == anchor { b_id } else { a_id };
                let a_corpus: Option<String> = r.get("a_corpus");
                let b_corpus: Option<String> = r.get("b_corpus");
                out.push(LinkedTo {
                    via: anchor.clone(),
                    other,
                    weight,
                    state,
                    reason: r.get("reason"),
                    cues: serde_json::from_str(&r.get::<String, _>("cues")).unwrap_or_default(),
                    // A merged artifact belongs to no corpus, so it can never be
                    // "the same document" as anything.
                    cross_corpus: match (a_corpus, b_corpus) {
                        (Some(x), Some(y)) => x != y,
                        _ => true,
                    },
                });
            }
        }
        out.sort_by(|x, y| y.weight.total_cmp(&x.weight));
        Ok(out)
    }

    /// Links strong enough, various enough, live enough and cross-corpus enough
    /// to be worth one model call. Strongest first.
    ///
    /// Same two-step as `links_from`: the raw weight narrows with the index and
    /// the decayed weight decides. Four times the caller's limit is fetched so
    /// that rows failing the exact test do not eat the budget.
    pub async fn links_to_judge(
        &self,
        min_weight: f64,
        min_queries: i64,
        half_life_days: f64,
        at: i64,
        limit: i64,
    ) -> Result<Vec<Link>> {
        let rows = sqlx::query(
            "SELECT l.* FROM artifact_links l
               JOIN artifacts a ON a.id = l.a_id
               JOIN artifacts b ON b.id = l.b_id
              WHERE l.state = 'learning'
                AND l.weight >= ? AND l.queries >= ?
                AND a.status = 'active' AND a.superseded_by IS NULL
                AND b.status = 'active' AND b.superseded_by IS NULL
                AND (a.corpus_id IS NULL OR b.corpus_id IS NULL OR a.corpus_id <> b.corpus_id)
              ORDER BY l.weight DESC LIMIT ?",
        )
        .bind(min_weight)
        .bind(min_queries)
        .bind(limit.saturating_mul(4))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(row_to_link)
            .filter(|l| decayed(l.weight, l.bumped_at, at, half_life_days) >= min_weight)
            .take(limit.max(0) as usize)
            .collect())
    }

    /// Delete `learning` links that have faded below the floor.
    ///
    /// The only write a quiet link ever costs. Rows whose *stored* weight is
    /// already under the floor go in one statement; the rest are read back and
    /// tested exactly, capped at `scan_limit` so one sweep cannot walk an
    /// unbounded table. Whatever the cap leaves is pruned by the next sweep.
    pub async fn prune_learning_links(
        &self,
        below: f64,
        half_life_days: f64,
        at: i64,
        scan_limit: i64,
    ) -> Result<u64> {
        let mut dropped = sqlx::query("DELETE FROM artifact_links WHERE state = 'learning' AND weight < ?")
            .bind(below)
            .execute(&self.pool)
            .await?
            .rows_affected();

        let rows = sqlx::query(
            "SELECT a_id, b_id, weight, bumped_at FROM artifact_links
              WHERE state = 'learning' ORDER BY bumped_at ASC LIMIT ?",
        )
        .bind(scan_limit)
        .fetch_all(&self.pool)
        .await?;
        for r in &rows {
            if decayed(r.get("weight"), r.get("bumped_at"), at, half_life_days) >= below {
                continue;
            }
            dropped += sqlx::query("DELETE FROM artifact_links WHERE a_id = ? AND b_id = ?")
                .bind(r.get::<String, _>("a_id"))
                .bind(r.get::<String, _>("b_id"))
                .execute(&self.pool)
                .await?
                .rows_affected();
        }
        if rows.len() as i64 == scan_limit {
            tracing::info!(scan_limit, "prune scan hit its cap; the rest waits for the next sweep");
        }
        Ok(dropped)
    }

    /// Put judged links back to `learning` where either side has been re-embedded
    /// since. The judge read text that no longer exists.
    pub async fn reopen_stale_judged_links(&self, limit: i64) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE artifact_links
                SET state = 'learning', reason = NULL,
                    judged_rev_a = NULL, judged_rev_b = NULL
              WHERE (a_id, b_id) IN (
                SELECT l.a_id, l.b_id FROM artifact_links l
                  JOIN artifacts a ON a.id = l.a_id
                  JOIN artifacts b ON b.id = l.b_id
                 WHERE l.state IN ('related', 'unrelated')
                   AND (a.embed_rev IS NOT l.judged_rev_a OR b.embed_rev IS NOT l.judged_rev_b)
                 LIMIT ?
              )",
        )
        .bind(limit)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Record a verdict. `judged_revs` is `(embed_rev of a, embed_rev of b)` as
    /// read by whoever judged, which is what `reopen_stale_judged_links`
    /// compares against later.
    pub async fn set_link_state(
        &self,
        a: &str,
        b: &str,
        state: LinkState,
        reason: Option<&str>,
        judged_revs: Option<(i64, i64)>,
    ) -> Result<()> {
        let (a, b) = canonical(a, b);
        let (rev_a, rev_b) = match judged_revs {
            Some((x, y)) => (Some(x), Some(y)),
            None => (None, None),
        };
        sqlx::query(
            "UPDATE artifact_links
                SET state = ?, reason = ?, judged_rev_a = ?, judged_rev_b = ?
              WHERE a_id = ? AND b_id = ?",
        )
        .bind(state.as_str())
        .bind(reason)
        .bind(rev_a)
        .bind(rev_b)
        .bind(a)
        .bind(b)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Count one call against a link and report the new total.
    ///
    /// Counted only where the answer said something about the *link* — an
    /// unreadable reply. A call an outage ate says something about the endpoint,
    /// and shelving a link for that would empty the pane every time the model is
    /// down.
    pub async fn record_link_judge_attempt(&self, a: &str, b: &str) -> Result<i64> {
        let (a, b) = canonical(a, b);
        Ok(sqlx::query_scalar(
            "UPDATE artifact_links SET judge_attempts = judge_attempts + 1
              WHERE a_id = ? AND b_id = ? RETURNING judge_attempts",
        )
        .bind(a)
        .bind(b)
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(0))
    }

    /// The three numbers Ops shows.
    pub async fn link_counts(&self) -> Result<LinkCounts> {
        Ok(LinkCounts {
            total: sqlx::query_scalar("SELECT COUNT(*) FROM artifact_links")
                .fetch_one(&self.pool)
                .await?,
            related: sqlx::query_scalar(
                "SELECT COUNT(*) FROM artifact_links WHERE state = 'related'",
            )
            .fetch_one(&self.pool)
            .await?,
            judge_queue: sqlx::query_scalar(
                "SELECT COUNT(*) FROM jobs
                  WHERE stage = 'link_judge' AND state IN ('pending', 'running')",
            )
            .fetch_one(&self.pool)
            .await?,
        })
    }
```

Note on `reopen_stale_judged_links`: `IS NOT` rather than `<>` because `judged_rev_*` is nullable and `NULL <> 0` is NULL, which would never reopen a link judged before the column carried a revision.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib store::links`
Expected: PASS, 17 tests.

- [ ] **Step 5: Commit**

```bash
git add src/store/links.rs
git commit -m "feat(links): read links through decay, prune, reopen and judge them"
```

---

## Task 3: Activation on artifacts

**Files:**
- Modify: `src/store/schema.sql` (two columns on `artifacts`)
- Modify: `src/store/mod.rs` (`ADDED_COLUMNS`, one backfill statement)
- Modify: `src/store/artifacts.rs:277-290` and `:348-362` (both INSERT statements)
- Modify: `src/store/links.rs` (activation accessors — same feature, same module)
- Test: `src/store/links.rs`, `src/store/mod.rs`

**Interfaces:**
- Consumes: `decayed` from Task 1.
- Produces: on `Store` — `activation_of(ids: &[String]) -> Result<HashMap<String, (f64, i64)>>` and `bump_activation(ids: &[String], delta: f64, half_life_days: f64, at: i64) -> Result<()>`.

- [ ] **Step 1: Add the columns**

In `src/store/schema.sql`, inside `CREATE TABLE IF NOT EXISTS artifacts`, after `last_verified_at`:

```sql
  -- Current accessibility. Raised by being captured, retrieved, opened and
  -- confirmed; read through the same lazy decay as a link's weight. In SQLite
  -- rather than the vector payload because the query path already needs one
  -- SQLite read for links, and the same read returns this — one crossing.
  activation       REAL    NOT NULL DEFAULT 1.0,
  activated_at     INTEGER NOT NULL DEFAULT 0
```

- [ ] **Step 2: Teach `migrate` about them**

In `src/store/mod.rs`, append to `ADDED_COLUMNS`:

```rust
            // Arrived with associative memory. Both have defaults that are the
            // truth about an artifact captured before it existed — full
            // accessibility, no stamp — and the stamp is backfilled below from
            // `created_at`, which is when it was in fact last activated.
            ("artifacts", "activation", "REAL NOT NULL DEFAULT 1.0"),
            ("artifacts", "activated_at", "INTEGER NOT NULL DEFAULT 0"),
```

And after `sqlx::raw_sql(SCHEMA)` has been applied, beside the two other post-schema statements:

```rust
        // The one default an append cannot state: `activated_at` has to be the
        // artifact's own creation time, not zero. Left at zero every artifact
        // predating this column reads as decayed to nothing since 1970 — the
        // whole base equally inaccessible, which is the opposite of the truth.
        sqlx::query("UPDATE artifacts SET activated_at = created_at WHERE activated_at = 0")
            .execute(&self.pool)
            .await?;
```

- [ ] **Step 3: Stamp new artifacts**

In `src/store/artifacts.rs`, the merged insert at line 277 and the captured insert at line 348 both need the two columns. Add `, activation, activated_at` to each column list and `, 1.0, ?` to each `VALUES` list, binding `c.created_at` at that position (place the bind in the same order as the placeholder). For the captured insert the value is the artifact's `created_at`; for the merged one likewise.

- [ ] **Step 4: Write the failing tests**

In `src/store/links.rs` tests:

```rust
    #[tokio::test]
    async fn a_fresh_artifact_starts_fully_activated_and_stamped() {
        // Left unstamped, every artifact in the base reads as having decayed
        // since 1970 — equally inaccessible, which is the opposite of true.
        let store = Store::memory().await.unwrap();
        let (a, _) = two(&store).await;
        let act = store.activation_of(&[a.clone()]).await.unwrap();
        let (value, stamp) = act.get(&a).copied().expect("an artifact carries activation");
        assert!((value - 1.0).abs() < 1e-9);
        assert!(stamp > 0, "activated_at was never set at insert");
    }

    #[tokio::test]
    async fn a_bump_folds_the_decay_in_like_a_link_does() {
        let store = Store::memory().await.unwrap();
        let (a, _) = two(&store).await;
        sqlx::query("UPDATE artifacts SET activation = 4.0, activated_at = 0 WHERE id = ?")
            .bind(&a)
            .execute(&store.pool)
            .await
            .unwrap();

        store
            .bump_activation(&[a.clone()], 1.0, 14.0, 14 * 86_400)
            .await
            .unwrap();

        let (value, stamp) = store.activation_of(&[a.clone()]).await.unwrap()[&a];
        assert!((value - 3.0).abs() < 1e-6, "value was {value}");
        assert_eq!(stamp, 14 * 86_400);
    }

    #[tokio::test]
    async fn bumping_nothing_is_not_a_write() {
        let store = Store::memory().await.unwrap();
        store.bump_activation(&[], 1.0, 14.0, 0).await.unwrap();
        assert!(store.activation_of(&[]).await.unwrap().is_empty());
    }
```

In `src/store/mod.rs` tests, the upgrade path:

```rust
    #[tokio::test]
    async fn a_base_captured_before_activation_gains_it_with_a_real_stamp() {
        // The append can only state a constant, and zero is the wrong stamp:
        // it reads as an artifact last reached in 1970. The backfill is what
        // makes the column true of a base that predates it.
        let store = Store::memory().await.unwrap();
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        store
            .insert_artifacts(
                &src.id,
                &[artifacts::NewArtifact {
                    ordinal: 0,
                    text: "alpha".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        for stmt in [
            "ALTER TABLE artifacts DROP COLUMN activation",
            "ALTER TABLE artifacts DROP COLUMN activated_at",
        ] {
            sqlx::query(stmt)
                .execute(&store.pool)
                .await
                .expect("the fixture needs an artifacts table from before activation");
        }

        store
            .migrate()
            .await
            .expect("migrate refused a base captured before activation");

        let (value, stamp): (f64, i64) =
            sqlx::query_as("SELECT activation, activated_at FROM artifacts")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert!((value - 1.0).abs() < 1e-9);
        assert!(stamp > 0, "activated_at was left at the epoch");
    }
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo test --lib store::`
Expected: FAIL — `no method named activation_of`.

- [ ] **Step 6: Write the implementation**

Append to the `impl Store` block in `src/store/links.rs`:

```rust
    /// Each artifact's stored activation and the stamp it was true at.
    ///
    /// One statement for the whole candidate list: this is on the query path,
    /// and fifty round trips to answer one search is exactly the layer crossing
    /// the design promises not to be. The SQL is built only from `?`
    /// placeholders — one per id, never a value — so nothing from a request
    /// reaches the statement text.
    pub async fn activation_of(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, (f64, i64)>> {
        if ids.is_empty() {
            return Ok(Default::default());
        }
        let holes = std::iter::repeat_n("?", ids.len()).collect::<Vec<_>>().join(",");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT id, activation, activated_at FROM artifacts WHERE id IN ({holes})"
        )));
        for id in ids {
            q = q.bind(id);
        }
        Ok(q.fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| {
                (
                    r.get::<String, _>("id"),
                    (r.get::<f64, _>("activation"), r.get::<i64, _>("activated_at")),
                )
            })
            .collect())
    }

    /// Raise the accessibility of these artifacts, folding the decay in.
    ///
    /// Read-then-write per artifact rather than one arithmetic UPDATE, for the
    /// same reason `bump_link` does it: the decay is an exponential, and not
    /// every SQLite build ships the math functions to express one in SQL.
    pub async fn bump_activation(
        &self,
        ids: &[String],
        delta: f64,
        half_life_days: f64,
        at: i64,
    ) -> Result<()> {
        if ids.is_empty() || delta == 0.0 {
            return Ok(());
        }
        let current = self.activation_of(ids).await?;
        for id in ids {
            let Some((value, stamp)) = current.get(id).copied() else {
                continue;
            };
            sqlx::query("UPDATE artifacts SET activation = ?, activated_at = ? WHERE id = ?")
                .bind(decayed(value, stamp, at, half_life_days) + delta)
                .bind(at)
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }
```

`sqlx::AssertSqlSafe` is already used in `migrate`; the audit it asks for is in the doc comment — the only interpolation is a run of `?`.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --lib store::`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/store/schema.sql src/store/mod.rs src/store/artifacts.rs src/store/links.rs
git commit -m "feat(activation): give every artifact a decaying accessibility"
```

---

## Task 4: Configuration and wiring

**Files:**
- Modify: `src/config.rs` (two config structs, `Config` fields, one warning)
- Modify: `src/core/mod.rs` (two `Core` fields, `from_config`, `test_support::build`)
- Test: `src/config.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `AssociateConfig { enabled: bool, interval_mins: u64, half_life_days: f64, prune_below: f64, show_min: f64, judge_min: f64, judge_min_queries: i64, judge_per_sweep: i64, spread_from: usize, spread_max: usize, prime_margin: f64, prime_lift: usize }`, `ActivationConfig { half_life_days: f64, retrieved: f64, opened: f64, confirmed: f64 }`, `Config::associate`, `Config::activation`, `Core::associate`, `Core::activation`.

- [ ] **Step 1: Write the failing test**

In `src/config.rs` tests:

```rust
    #[test]
    fn the_association_defaults_are_the_documented_ones() {
        let a = AssociateConfig::default();
        assert!(a.enabled);
        assert_eq!(a.interval_mins, 30);
        assert_eq!(a.half_life_days, 30.0);
        assert_eq!((a.show_min, a.judge_min, a.prune_below), (2.0, 4.0, 0.5));
        assert_eq!((a.spread_from, a.spread_max), (3, 3));
        assert_eq!((a.prime_margin, a.prime_lift), (0.5, 2));
        let v = ActivationConfig::default();
        assert_eq!(v.half_life_days, 14.0);
        assert_eq!((v.retrieved, v.opened, v.confirmed), (1.0, 0.5, 3.0));
    }

    #[test]
    fn a_config_with_no_association_block_still_gets_one() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        let cfg = Config::load(Some(&p)).unwrap();
        assert!(cfg.associate.enabled);
        // ...and the feature is inert regardless, because there is nothing to
        // learn from until searches are recorded.
        assert!(!cfg.feedback.enabled);
    }

    #[test]
    fn the_example_config_carries_the_association_block() {
        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert_eq!(cfg.associate.spread_max, 3);
    }
```

The third test fails until Task 14 adds the block to `config.example.toml`; `#[serde(default)]` on the section means it passes on the defaults alone, so it is safe to write now.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib config::`
Expected: FAIL — `cannot find type AssociateConfig`.

- [ ] **Step 3: Write the config**

In `src/config.rs`, after `FeedbackConfig`:

```rust
/// Links learned from co-retrieval, and what they are allowed to do.
///
/// Every threshold here is a weight in the same units: one co-appearance is
/// `+1`, one confirmed answer is `+2`, and a half-life of thirty days is what
/// makes those numbers mean "lately" rather than "ever".
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct AssociateConfig {
    /// Requires `feedback.enabled`. Without recorded searches there is nothing
    /// to learn from, and that combination is a warning at startup.
    pub enabled: bool,
    pub interval_mins: u64,
    pub half_life_days: f64,
    /// Decayed weight under which a `learning` link is deleted.
    pub prune_below: f64,
    /// Decayed weight at which a link is worth showing.
    pub show_min: f64,
    /// ...and at which it is worth one model call.
    pub judge_min: f64,
    /// Distinct binding questions a link needs before it is judged. One question
    /// asked six times is one question.
    pub judge_min_queries: i64,
    pub judge_per_sweep: i64,
    /// How many of the top ranked hits are asked what they are linked to.
    pub spread_from: usize,
    /// How many associated hits may be appended, outside `limit`.
    pub spread_max: usize,
    /// How much more activated a hit must be than the one above it to pass it.
    /// Normalised within one result list, so this is a fraction, not a weight.
    pub prime_margin: f64,
    /// Positions a hit may climb. `0` turns priming off.
    pub prime_lift: usize,
}

impl Default for AssociateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_mins: 30,
            half_life_days: 30.0,
            prune_below: 0.5,
            show_min: 2.0,
            judge_min: 4.0,
            judge_min_queries: 3,
            judge_per_sweep: 10,
            spread_from: 3,
            spread_max: 3,
            prime_margin: 0.5,
            prime_lift: 2,
        }
    }
}

/// How accessible an artifact is, and what raises it.
///
/// Being surfaced *because* of activation raises nothing: `resurface` and
/// association both leave it alone. Loops that reinforce themselves are the
/// failure mode of this whole idea, and they are closed by construction.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ActivationConfig {
    pub half_life_days: f64,
    /// Returned by a search the caller marked as seen.
    pub retrieved: f64,
    /// Opened in the detail pane.
    pub opened: f64,
    /// Judged the answer to a real question. The strong signal.
    pub confirmed: f64,
}

impl Default for ActivationConfig {
    fn default() -> Self {
        Self {
            half_life_days: 14.0,
            retrieved: 1.0,
            opened: 0.5,
            confirmed: 3.0,
        }
    }
}
```

Add to `Config`:

```rust
    #[serde(default)]
    pub associate: AssociateConfig,
    #[serde(default)]
    pub activation: ActivationConfig,
```

And in `warn_on_moved_keys` — which is where startup says what is quietly not happening — add:

```rust
        if self.associate.enabled && !self.feedback.enabled {
            tracing::warn!(
                "associate.enabled has no effect while feedback.enabled is false: links are \
                 learned from recorded searches, and none are being recorded. Recording queries \
                 is a privacy decision, so it keeps its own switch."
            );
        }
```

- [ ] **Step 4: Wire it into `Core`**

In `src/core/mod.rs`, add the fields to `Core` beside `feedback`:

```rust
    /// Link learning, priming and association. Read on the search path and by
    /// the sweep, so it lives here rather than being threaded down.
    pub associate: crate::config::AssociateConfig,
    pub activation: crate::config::ActivationConfig,
```

Set them in `from_config` (`associate: cfg.associate.clone(), activation: cfg.activation.clone(),`) and in `test_support::build`:

```rust
            // On, like the shipped default — and inert in most tests, because
            // nothing has learned a link yet. The association tests seed one.
            associate: crate::config::AssociateConfig::default(),
            activation: crate::config::ActivationConfig::default(),
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --locked`
Expected: PASS (the whole suite — `Core` gained fields, so every construction site had to be updated).

- [ ] **Step 6: Commit**

```bash
git add src/config.rs src/core/mod.rs
git commit -m "feat(associate): add the [associate] and [activation] settings"
```

---

## Task 5: The stages, the ticker, and the dispatch

**Files:**
- Modify: `src/store/jobs.rs:12-71` (`Stage`)
- Modify: `src/jobs/mod.rs` (module list, `run_claimed` arms)
- Create: `src/jobs/associate.rs` (skeleton: `run` and `judge` that do nothing yet)
- Modify: `src/core/background.rs` (`spawn_associate_ticker`, `ASSOCIATE_TARGET`)
- Modify: `src/main.rs` (spawn the ticker)
- Test: `src/core/background.rs`

**Interfaces:**
- Consumes: `AssociateConfig` from Task 4.
- Produces: `Stage::Associate` (`"associate"`), `Stage::LinkJudge` (`"link_judge"`), `background::ASSOCIATE_TARGET`, `background::spawn_associate_ticker(core, shutdown) -> JoinHandle<()>`, `jobs::associate::{run, judge}`.

- [ ] **Step 1: Add the stages**

In `src/store/jobs.rs`, add to the `Stage` enum, `as_str` and `parse`:

```rust
    /// The periodic association sweep. Its target is the collection rather than
    /// any one artifact, so the `UNIQUE(stage, target_id)` on `jobs` guarantees
    /// at most one queued sweep however often the ticker fires. Local work: it
    /// replays the search log and arms `LinkJudge` units, and calls no model.
    Associate,
    /// One strong cross-corpus link, one call. Target is `"<a_id>|<b_id>"`.
    LinkJudge,
```

```rust
            Stage::Associate => "associate",
            Stage::LinkJudge => "link_judge",
```
```rust
            "associate" => Some(Stage::Associate),
            "link_judge" => Some(Stage::LinkJudge),
```

- [ ] **Step 2: Add the skeleton unit**

Create `src/jobs/associate.rs`:

```rust
//! Learning what belongs together, and saying so.
//!
//! Two things happen here and they are deliberately not the same job. The sweep
//! is pure SQLite: it replays the search log, strengthens the pairs that were
//! reached together, fades and prunes the ones that were not, and decides which
//! links are worth asking about. The judge is one model call on one link, armed
//! by the sweep and paced by the queue like every other call in the system.

use crate::core::Core;
use crate::error::Result;

/// One sweep over everything learned since the last one.
pub async fn run(core: &Core) -> Result<()> {
    if !core.associate.enabled || !core.feedback.enabled {
        return Ok(());
    }
    Ok(())
}

/// One link, one call. `target` is `"<a_id>|<b_id>"`.
pub async fn judge(core: &Core, target: &str) -> Result<()> {
    let _ = (core, target);
    Ok(())
}
```

And in `src/jobs/mod.rs`, `pub mod associate;` at the top of the module list, plus two arms in `run_claimed`'s match, beside `Stage::Consolidate`:

```rust
        // The sweep looks at the whole collection, so it ignores the target.
        (Stage::Associate, _) => associate::run(core).await,
        (Stage::LinkJudge, _) => associate::judge(core, &job.target_id).await,
```

`LinkJudge` deliberately gets no arm in the exhausted-attempts match below: a dead endpoint leaves the unit queued at the backoff ceiling like every other unit, and the link stays visible as `learning` in the meantime.

- [ ] **Step 3: Write the failing test**

In `src/core/background.rs` tests:

```rust
    #[tokio::test]
    async fn the_association_ticker_queues_exactly_one_sweep() {
        // Same reasoning as the consolidation sweep: `jobs` is unique on
        // (stage, target), so a ticker firing while a sweep is still queued
        // collapses onto the same row rather than stacking sweeps.
        let mut core = crate::core::test_support::test_core().await;
        core.feedback.enabled = true;
        let (tx, rx) = tokio::sync::watch::channel(false);
        let h = spawn_associate_ticker(core.clone(), rx);
        for _ in 0..50 {
            if core.store.job_counts().await.unwrap().iter().any(|(_, n)| *n > 0) {
                break;
            }
            tokio::task::yield_now().await;
        }
        let _ = tx.send(true);
        let _ = h.await;

        let j = core
            .store
            .claim_job()
            .await
            .unwrap()
            .expect("the ticker queued nothing");
        assert_eq!(j.stage, crate::store::jobs::Stage::Associate);
        assert_eq!(j.target_id, ASSOCIATE_TARGET);
        assert!(core.store.claim_job().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn no_recorded_searches_means_no_association_ticker_at_all() {
        // `associate.enabled` without `feedback.enabled` is a warning at startup
        // and nothing else: there is nothing to learn from.
        let core = crate::core::test_support::test_core().await; // feedback off
        let (_tx, rx) = tokio::sync::watch::channel(false);
        // Returns rather than looping, so awaiting it cannot hang.
        let _ = spawn_associate_ticker(core.clone(), rx).await;
        assert!(core.store.claim_job().await.unwrap().is_none());
    }
```

- [ ] **Step 4: Run to verify it fails**

Run: `cargo test --lib core::background`
Expected: FAIL — `cannot find function spawn_associate_ticker`.

- [ ] **Step 5: Write the ticker**

In `src/core/background.rs`, after `spawn_dedupe_ticker`:

```rust
/// The association sweep's job target. A constant rather than an artifact id:
/// the sweep replays the whole log, and `UNIQUE(stage, target_id)` then bounds
/// the queue to one of them however often the ticker fires.
pub const ASSOCIATE_TARGET: &str = "collection";

/// Queue an association sweep now and every `associate.interval_mins` after.
///
/// Its own ticker, like retention and dedupe: the rhythm of replaying a search
/// log has nothing to do with the rhythm of duplicate discovery, and coupling
/// the two is how switching one feature off silently switches another one off.
///
/// Returns before its loop when there is nothing to learn from — either the
/// feature is off, or searches are not being recorded, which is the same thing.
pub fn spawn_associate_ticker(
    core: crate::core::Core,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !core.associate.enabled || !core.feedback.enabled {
            tracing::info!("association sweep disabled");
            return;
        }
        let period = std::time::Duration::from_secs(core.associate.interval_mins.max(1) * 60);
        let mut tick = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    if let Err(e) = core
                        .store
                        .enqueue(crate::store::jobs::Stage::Associate, "collection", ASSOCIATE_TARGET)
                        .await
                    {
                        tracing::warn!(error = %e, "could not queue the association sweep");
                    }
                }
            }
        }
        tracing::info!("association ticker stopped");
    })
}
```

- [ ] **Step 6: Spawn it at startup**

In `src/main.rs`, after the `repair` binding at line 299:

```rust
    let associate =
        engram::core::background::spawn_associate_ticker(core.clone(), shutdown_rx.clone());
```

and beside `handles.push(repair);` at line 306:

```rust
    handles.push(associate);
```

Both go before `Worker::spawn` consumes `core`, which is why every other ticker is bound above that line too. Joined with the workers so shutdown waits for it rather than leaving a task the runtime drops mid-enqueue.

- [ ] **Step 7: Run the tests**

Run: `cargo test --locked`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/store/jobs.rs src/jobs/mod.rs src/jobs/associate.rs src/core/background.rs src/main.rs
git commit -m "feat(associate): add the sweep and judge stages and their ticker"
```

---

## Task 6: The sweep replays what fired together

**Files:**
- Modify: `src/jobs/associate.rs`
- Test: `src/jobs/associate.rs`

**Interfaces:**
- Consumes: `Store::{bump_link, bump_activation, meta_get, meta_set}`, `links::normalize_query`, `Core::{associate, activation, feedback}`.
- Produces: `associate::run` now performs steps 1–3 of the spec's sweep; the private helpers `replay_events`, `replay_verdicts`, and the constants `EVENTS_AFTER: &str = "associate.events_after"`, `JUDGED_AFTER: &str = "associate.judged_after"`, `REPLAY_LIMIT: i64 = 2_000`.

- [ ] **Step 1: Write the failing tests**

In `src/jobs/associate.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::artifacts::NewArtifact;
    use crate::store::feedback::{Door, NewCandidate, NewEvent, Verdict};
    use crate::store::links::LinkState;

    /// `n` artifacts, each in its own corpus, so every link between them is a
    /// cross-corpus one.
    async fn seed(core: &Core, n: usize) -> Vec<String> {
        let mut ids = Vec::new();
        for i in 0..n {
            let src = core
                .store
                .insert_corpus(&format!("raw {i}"), "web", None)
                .await
                .unwrap();
            let made = core
                .store
                .insert_artifacts(
                    &src.id,
                    &[NewArtifact {
                        ordinal: 0,
                        text: format!("artifact {i}"),
                        corpus_span: None,
                        title: Some(format!("t{i}")),
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    }],
                )
                .await
                .unwrap();
            ids.push(made[0].id.clone());
        }
        ids
    }

    /// One recorded search, with `shown` deciding what the searcher saw.
    async fn record(core: &Core, query: &str, shown: &[&String], unshown: &[&String]) -> String {
        let candidates = shown
            .iter()
            .map(|id| NewCandidate {
                artifact_id: (*id).clone(),
                score: 1.0,
                similarity: Some(0.9),
                shown: true,
            })
            .chain(unshown.iter().map(|id| NewCandidate {
                artifact_id: (*id).clone(),
                score: 0.1,
                similarity: Some(0.2),
                shown: false,
            }))
            .collect();
        core.store
            .record_search(
                NewEvent {
                    query: query.into(),
                    door: Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.0],
                    embed_model: "fake".into(),
                    candidates,
                },
                0,
            )
            .await
            .unwrap()
    }

    /// Age every recorded event past the coalescing window, so the sweep will
    /// look at it: a folding event is still moving.
    async fn settle(core: &Core) {
        sqlx::query("UPDATE search_events SET created_at = created_at - 3600")
            .execute(&core.store.pool)
            .await
            .unwrap();
    }

    async fn on(core: &mut Core) {
        core.feedback.enabled = true;
    }

    #[tokio::test]
    async fn two_searches_showing_the_same_pair_bind_it_twice() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        record(&core, "fat32 cluster", &[&ids[0], &ids[1]], &[]).await;
        record(&core, "ntfs journal", &[&ids[0], &ids[1]], &[]).await;
        settle(&core).await;

        run(&core).await.unwrap();

        let l = core.store.get_link(&ids[0], &ids[1]).await.unwrap().unwrap();
        assert!((l.weight - 2.0).abs() < 1e-6, "weight was {}", l.weight);
        assert_eq!(l.queries, 2, "two different questions bound this pair");
        assert_eq!(l.state, LinkState::Learning);
    }

    #[tokio::test]
    async fn the_same_question_asked_twice_is_one_binding_query() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        record(&core, "fat32", &[&ids[0], &ids[1]], &[]).await;
        // Coalescing is off in `record`, so this is a second event with the
        // same words — which is a second use, and one question.
        record(&core, "  FAT32  ", &[&ids[0], &ids[1]], &[]).await;
        settle(&core).await;

        run(&core).await.unwrap();

        let l = core.store.get_link(&ids[0], &ids[1]).await.unwrap().unwrap();
        assert!((l.weight - 2.0).abs() < 1e-6);
        assert_eq!(l.queries, 1);
    }

    #[tokio::test]
    async fn only_what_the_searcher_saw_fires_together() {
        // The stored pool is wider than the answer for evaluation's sake. An
        // artifact nobody was shown was not reached, and did not fire.
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;
        record(&core, "q", &[&ids[0], &ids[1]], &[&ids[2]]).await;
        settle(&core).await;

        run(&core).await.unwrap();

        assert!(core.store.get_link(&ids[0], &ids[1]).await.unwrap().is_some());
        assert!(core.store.get_link(&ids[0], &ids[2]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_event_still_folding_is_left_for_the_next_sweep() {
        // A typing burst is one event, and it is not finished until the
        // coalescing window has passed. Replaying it early would bind the pairs
        // of a half-typed query and then bind the finished one again.
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        record(&core, "fat", &[&ids[0], &ids[1]], &[]).await;

        run(&core).await.unwrap();
        assert!(core.store.get_link(&ids[0], &ids[1]).await.unwrap().is_none());

        settle(&core).await;
        run(&core).await.unwrap();
        assert!(core.store.get_link(&ids[0], &ids[1]).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_replayed_event_is_never_replayed_again() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        record(&core, "q", &[&ids[0], &ids[1]], &[]).await;
        settle(&core).await;

        run(&core).await.unwrap();
        run(&core).await.unwrap();

        let l = core.store.get_link(&ids[0], &ids[1]).await.unwrap().unwrap();
        assert!((l.weight - 1.0).abs() < 1e-6, "the event was replayed twice");
    }

    #[tokio::test]
    async fn a_confirmed_answer_binds_harder_and_raises_its_activation() {
        // Confirmation is the strong signal; co-appearance is the weak one.
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;
        let ev = record(&core, "q", &[&ids[0], &ids[1], &ids[2]], &[]).await;
        core.store.judge_hit(&ev, &ids[0]).await.unwrap();
        settle(&core).await;

        run(&core).await.unwrap();

        // +1 for co-appearance, +2 more for the pairs containing the answer.
        let with = core.store.get_link(&ids[0], &ids[1]).await.unwrap().unwrap();
        let without = core.store.get_link(&ids[1], &ids[2]).await.unwrap().unwrap();
        assert!((with.weight - 3.0).abs() < 1e-6, "weight was {}", with.weight);
        assert!((without.weight - 1.0).abs() < 1e-6);

        let act = core.store.activation_of(&ids).await.unwrap();
        assert!(
            act[&ids[0]].0 > act[&ids[1]].0,
            "the confirmed answer gained no activation"
        );
    }

    #[tokio::test]
    async fn a_gap_and_a_discard_teach_nothing_about_pairs() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        let g = record(&core, "nothing about this", &[&ids[0], &ids[1]], &[]).await;
        core.store.judge(&g, Verdict::Gap).await.unwrap();
        settle(&core).await;

        run(&core).await.unwrap();

        // Co-appearance still counts — the searcher did see both — but the
        // verdict adds nothing on top of it.
        let l = core.store.get_link(&ids[0], &ids[1]).await.unwrap().unwrap();
        assert!((l.weight - 1.0).abs() < 1e-6, "weight was {}", l.weight);
    }

    #[tokio::test]
    async fn the_sweep_does_nothing_at_all_while_nothing_is_recorded() {
        let core = test_core().await; // feedback off, associate on
        let ids = seed(&core, 2).await;
        run(&core).await.unwrap();
        assert!(core.store.get_link(&ids[0], &ids[1]).await.unwrap().is_none());
        assert_eq!(core.store.meta_get(EVENTS_AFTER).await.unwrap(), None);
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib jobs::associate`
Expected: FAIL — assertions about links that were never written.

- [ ] **Step 3: Write the implementation**

Replace `run` in `src/jobs/associate.rs` and add the helpers:

```rust
use crate::store::links::normalize_query;
use sqlx::Row;

/// Last `search_events.created_at` folded into links.
pub const EVENTS_AFTER: &str = "associate.events_after";
/// Last `search_events.judged_at` folded into links.
pub const JUDGED_AFTER: &str = "associate.judged_after";
/// Events read per sweep. A ceiling rather than a budget: at 30-minute ticks
/// nothing real reaches it, and a base that has been offline for a month
/// catches up over a few sweeps instead of holding one worker for minutes.
const REPLAY_LIMIT: i64 = 2_000;

/// One sweep over everything learned since the last one.
pub async fn run(core: &Core) -> Result<()> {
    if !core.associate.enabled || !core.feedback.enabled {
        return Ok(());
    }
    let at = crate::store::now();
    let bound = replay_events(core, at).await?;
    let confirmed = replay_verdicts(core, at).await?;
    tracing::info!(events = bound, verdicts = confirmed, "association sweep");
    Ok(())
}

/// Every pair of shown candidates in every settled event past the watermark.
///
/// "Settled" is the whole of the read condition beyond the watermark: an event
/// inside `feedback.coalesce_secs` of now is still moving — a typing burst folds
/// into one row — and binding the pairs of a half-typed query would then be
/// followed by binding the finished one.
async fn replay_events(core: &Core, at: i64) -> Result<usize> {
    let after: i64 = core
        .store
        .meta_get(EVENTS_AFTER)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let settled = at - core.feedback.coalesce_secs.max(0);

    let events = sqlx::query(
        "SELECT id, query, created_at FROM search_events
          WHERE created_at > ? AND created_at <= ?
          ORDER BY created_at ASC, id ASC LIMIT ?",
    )
    .bind(after)
    .bind(settled)
    .bind(REPLAY_LIMIT)
    .fetch_all(&core.store.pool)
    .await?;

    let mut high = after;
    for e in &events {
        let id: String = e.get("id");
        let cue = normalize_query(&e.get::<String, _>("query"));
        let shown = shown_candidates(core, &id).await?;
        for i in 0..shown.len() {
            for j in (i + 1)..shown.len() {
                core.store
                    .bump_link(
                        &shown[i],
                        &shown[j],
                        1.0,
                        Some(&cue),
                        core.associate.half_life_days,
                        at,
                    )
                    .await?;
            }
        }
        high = high.max(e.get::<i64, _>("created_at"));
    }

    if high > after {
        core.store.meta_set(EVENTS_AFTER, &high.to_string()).await?;
    }
    Ok(events.len())
}

/// Every hit verdict past the second watermark: the pairs containing the
/// confirmed answer bind harder, and the answer itself becomes more accessible.
///
/// Its own cursor, because a verdict arrives days after the event it is about —
/// one cursor would either replay the event's pairs again or skip the verdict.
/// `gap` and `discard` are not read here: neither says anything about a pair.
async fn replay_verdicts(core: &Core, at: i64) -> Result<usize> {
    let after: i64 = core
        .store
        .meta_get(JUDGED_AFTER)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let events = sqlx::query(
        "SELECT id, expect_id, judged_at FROM search_events
          WHERE judged_at > ? AND verdict = 'hit' AND expect_id IS NOT NULL
          ORDER BY judged_at ASC, id ASC LIMIT ?",
    )
    .bind(after)
    .bind(REPLAY_LIMIT)
    .fetch_all(&core.store.pool)
    .await?;

    let mut high = after;
    for e in &events {
        let id: String = e.get("id");
        let expect: String = e.get("expect_id");
        let shown = shown_candidates(core, &id).await?;
        for other in shown.iter().filter(|c| **c != expect) {
            // No cue: this event's words were already folded in as a binding
            // query when its co-appearance was replayed, and counting them
            // twice would say two questions bound this pair.
            core.store
                .bump_link(&expect, other, 2.0, None, core.associate.half_life_days, at)
                .await?;
        }
        // Raised whether or not the answer was in the pool at all — an artifact
        // the ranking never returned and a person confirmed anyway is the most
        // valuable confirmation there is.
        core.store
            .bump_activation(
                std::slice::from_ref(&expect),
                core.activation.confirmed,
                core.activation.half_life_days,
                at,
            )
            .await?;
        high = high.max(e.get::<i64, _>("judged_at"));
    }

    if high > after {
        core.store.meta_set(JUDGED_AFTER, &high.to_string()).await?;
    }
    Ok(events.len())
}

/// What one event actually put in front of the searcher, in rank order.
async fn shown_candidates(core: &Core, event_id: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT artifact_id FROM search_candidates
          WHERE event_id = ? AND shown = 1 ORDER BY rank",
    )
    .bind(event_id)
    .fetch_all(&core.store.pool)
    .await?)
}
```

Note: `bump_link` has a foreign key to `artifacts`, and a candidate row can name an artifact that has since been deleted. Wrap the bump so that is not a sweep failure — change the two call sites to:

```rust
                if let Err(e) = core.store.bump_link(...).await {
                    tracing::debug!(error = %e, "could not bind a pair; one side is gone");
                }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib jobs::associate`
Expected: PASS, 8 tests.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/associate.rs
git commit -m "feat(associate): bind what was reached together, once each"
```

---

## Task 7: The sweep forgets, reopens, and arms the judge

**Files:**
- Modify: `src/jobs/associate.rs`
- Test: `src/jobs/associate.rs`

**Interfaces:**
- Consumes: `Store::{prune_learning_links, reopen_stale_judged_links, links_to_judge, rearm_idle_seq, live_job}`.
- Produces: `associate::link_target(a: &str, b: &str) -> String` (public — the judge parses it back), `PRUNE_SCAN_LIMIT: i64 = 5_000`, and `run` completing steps 4–6.

- [ ] **Step 1: Write the failing tests**

Append to the tests in `src/jobs/associate.rs`:

```rust
    #[tokio::test]
    async fn a_faded_link_is_forgotten_and_a_judged_one_is_not() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 4).await;
        core.store
            .bump_link(&ids[0], &ids[1], 1.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        core.store
            .bump_link(&ids[2], &ids[3], 1.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        core.store
            .set_link_state(&ids[2], &ids[3], LinkState::Related, Some("why"), Some((0, 0)))
            .await
            .unwrap();

        run(&core).await.unwrap();

        assert!(
            core.store.get_link(&ids[0], &ids[1]).await.unwrap().is_none(),
            "a link last used at the epoch has decayed to nothing"
        );
        assert!(core.store.get_link(&ids[2], &ids[3]).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_strong_cross_corpus_link_is_armed_for_the_judge_exactly_once() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        for q in ["one", "two", "three", "four"] {
            core.store
                .bump_link(&ids[0], &ids[1], 1.0, Some(q), 30.0, crate::store::now())
                .await
                .unwrap();
        }

        run(&core).await.unwrap();
        let target = link_target(&ids[0], &ids[1]);
        assert!(
            core.store
                .live_job(crate::store::jobs::Stage::LinkJudge, &target)
                .await
                .unwrap()
        );

        // A second sweep must not wind the queued unit's attempts back.
        run(&core).await.unwrap();
        let mut seen = 0;
        while let Some(j) = core.store.claim_job().await.unwrap() {
            if j.stage == crate::store::jobs::Stage::LinkJudge {
                seen += 1;
                assert_eq!(j.attempts, 1, "the unit was re-armed underneath itself");
            }
        }
        assert_eq!(seen, 1);
    }

    #[tokio::test]
    async fn a_judged_link_is_reopened_when_its_text_changes_under_it() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 9.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        core.store
            .set_link_state(&ids[0], &ids[1], LinkState::Unrelated, Some("coincidence"), Some((0, 0)))
            .await
            .unwrap();
        core.store
            .update_artifact_text(&ids[0], "rewritten")
            .await
            .unwrap();

        run(&core).await.unwrap();

        let l = core.store.get_link(&ids[0], &ids[1]).await.unwrap().unwrap();
        assert_eq!(l.state, LinkState::Learning, "the judge read text that is gone");
    }

    #[test]
    fn a_link_names_itself_the_same_way_round_however_it_is_armed() {
        assert_eq!(link_target("b", "a"), link_target("a", "b"));
        assert_eq!(link_target("a", "b"), "a|b");
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib jobs::associate`
Expected: FAIL — `cannot find function link_target`.

- [ ] **Step 3: Write the implementation**

Add to `src/jobs/associate.rs`:

```rust
/// Learning links read per prune pass. Whatever the cap leaves is pruned by the
/// next sweep — a bound on one tick's work, not on what is eventually forgotten.
const PRUNE_SCAN_LIMIT: i64 = 5_000;

/// The queue name of one link. Canonical, so the same pair never gets two units.
pub fn link_target(a: &str, b: &str) -> String {
    let (a, b) = crate::store::links::canonical(a, b);
    format!("{a}|{b}")
}
```

and extend `run`:

```rust
pub async fn run(core: &Core) -> Result<()> {
    if !core.associate.enabled || !core.feedback.enabled {
        return Ok(());
    }
    let at = crate::store::now();
    let bound = replay_events(core, at).await?;
    let confirmed = replay_verdicts(core, at).await?;

    let forgotten = core
        .store
        .prune_learning_links(
            core.associate.prune_below,
            core.associate.half_life_days,
            at,
            PRUNE_SCAN_LIMIT,
        )
        .await?;
    // A re-embed of either side reopens the verdict before anything is armed,
    // so a link whose text changed is re-asked in this same sweep rather than
    // waiting out another interval.
    let reopened = core.store.reopen_stale_judged_links(PRUNE_SCAN_LIMIT).await?;

    let mut armed = 0;
    for l in core
        .store
        .links_to_judge(
            core.associate.judge_min,
            core.associate.judge_min_queries,
            core.associate.half_life_days,
            at,
            core.associate.judge_per_sweep,
        )
        .await?
    {
        let target = link_target(&l.a_id, &l.b_id);
        // A link whose judgement is already queued is already going to be
        // judged; arming it again is a no-op that costs another link its turn.
        if core.store.live_job(crate::store::jobs::Stage::LinkJudge, &target).await? {
            continue;
        }
        core.store
            .rearm_idle_seq(crate::store::jobs::Stage::LinkJudge, "link", &target, armed)
            .await?;
        armed += 1;
    }

    tracing::info!(
        events = bound,
        verdicts = confirmed,
        forgotten,
        reopened,
        armed,
        "association sweep"
    );
    Ok(())
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib jobs::associate`
Expected: PASS, 12 tests.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/associate.rs
git commit -m "feat(associate): forget faded links, reopen stale verdicts, arm the judge"
```

---

## Task 8: The judge

**Files:**
- Modify: `src/infer/prompt.rs` (system prompt and prompt builder, beside the dedupe ones)
- Modify: `src/jobs/associate.rs` (`judge`, the parser)
- Test: `src/infer/prompt.rs`, `src/jobs/associate.rs`

**Interfaces:**
- Consumes: `Core::judge` (`Arc<dyn Completer>`), `Core::gate`, `Store::{get_link, get_artifact, set_link_state, record_link_judge_attempt, record_pair_with_detail}`.
- Produces: `prompt::LINK_SYSTEM: &str`, `prompt::link_prompt(a: (&str, &str), b: (&str, &str), cues: &[String], attempt: i64) -> String`; in `associate`: `LinkVerdict { Related, Unrelated, Duplicate }`, `parse_link(&str) -> Result<(LinkVerdict, String)>`, `MAX_UNREADABLE_LINK_JUDGEMENTS: i64 = 3`.
- Also produces on `Store`: `record_pair_with_detail(a, b, score, detail) -> Result<bool>` in `src/store/pairs.rs`.

- [ ] **Step 1: Write the failing tests**

In `src/infer/prompt.rs` tests:

```rust
    #[test]
    fn the_link_prompt_carries_both_titles_and_the_questions_that_bound_them() {
        // The binding queries are the evidence. Without them the model is being
        // asked whether two arbitrary texts are related, which is a different
        // and much worse question than why these two keep being needed at once.
        let p = link_prompt(
            ("Mounting E01 images", "ewfmount /dev/..."),
            ("Loop device limits", "max_loop=64"),
            &["mount forensic image".into()],
            0,
        );
        assert!(p.contains("Mounting E01 images"));
        assert!(p.contains("max_loop=64"));
        assert!(p.contains("mount forensic image"));
        assert!(!p.contains("attempt"), "a first ask must stay cache-identical");
        assert!(link_prompt(("a", "b"), ("c", "d"), &[], 2).contains("attempt 3"));
    }
```

In `src/jobs/associate.rs` tests:

```rust
    #[test]
    fn a_verdict_is_read_out_of_the_reply_and_an_unreadable_one_is_an_error() {
        let (v, why) = parse_link(r#"{"relation":"related","reason":"both about mounting"}"#).unwrap();
        assert_eq!(v, LinkVerdict::Related);
        assert_eq!(why, "both about mounting");
        assert_eq!(
            parse_link(r#"{"relation":"duplicate","reason":"same thing"}"#).unwrap().0,
            LinkVerdict::Duplicate
        );
        assert!(parse_link("I think they are related!").is_err());
        assert!(parse_link(r#"{"relation":"maybe","reason":"x"}"#).is_err());
    }

    #[tokio::test]
    async fn a_related_verdict_names_the_relation_and_stops_the_decay() {
        let mut core = test_core().await;
        on(&mut core).await;
        core.judge = std::sync::Arc::new(crate::infer::fake::FakeCompleter::replying(
            r#"{"relation":"related","reason":"the config and its errors"}"#,
        ));
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = core.store.get_link(&ids[0], &ids[1]).await.unwrap().unwrap();
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.reason.as_deref(), Some("the config and its errors"));
        assert_eq!(l.judged_rev_a, Some(0));
    }

    #[tokio::test]
    async fn an_unrelated_verdict_is_stored_so_it_is_not_asked_again() {
        let mut core = test_core().await;
        on(&mut core).await;
        core.judge = std::sync::Arc::new(crate::infer::fake::FakeCompleter::replying(
            r#"{"relation":"unrelated","reason":"a coincidence of retrieval"}"#,
        ));
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(
            core.store.get_link(&ids[0], &ids[1]).await.unwrap().unwrap().state,
            LinkState::Unrelated
        );
        // ...and it is never armed again, however strong it becomes.
        run(&core).await.unwrap();
        assert!(
            !core
                .store
                .live_job(crate::store::jobs::Stage::LinkJudge, &link_target(&ids[0], &ids[1]))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_disguised_duplicate_is_handed_to_consolidation_and_still_shown() {
        // The embedding failed to notice; the reader should still see the
        // connection while dedupe decides what to do about it.
        let mut core = test_core().await;
        on(&mut core).await;
        core.judge = std::sync::Arc::new(crate::infer::fake::FakeCompleter::replying(
            r#"{"relation":"duplicate","reason":"the same procedure twice"}"#,
        ));
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let pair = core
            .store
            .pair_state_between(&ids[0], &ids[1])
            .await
            .unwrap()
            .expect("consolidation was never told");
        assert_eq!(pair, crate::store::pairs::PairState::Pending);
        let l = core.store.get_link(&ids[0], &ids[1]).await.unwrap().unwrap();
        assert_eq!(l.state, LinkState::Related);
        assert!(l.reason.as_deref().unwrap().contains("consolidation"));
    }

    #[tokio::test]
    async fn three_unreadable_replies_shelve_the_link_rather_than_asking_forever() {
        let mut core = test_core().await;
        on(&mut core).await;
        core.judge = std::sync::Arc::new(crate::infer::fake::FakeCompleter::replying(
            "no idea, sorry",
        ));
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        let target = link_target(&ids[0], &ids[1]);

        for _ in 0..2 {
            assert!(judge(&core, &target).await.is_err(), "an unreadable reply is an error");
            assert_eq!(
                core.store.get_link(&ids[0], &ids[1]).await.unwrap().unwrap().state,
                LinkState::Learning,
                "the link stays visible while it is still being asked about"
            );
        }
        judge(&core, &target).await.unwrap();

        let l = core.store.get_link(&ids[0], &ids[1]).await.unwrap().unwrap();
        assert_eq!(l.state, LinkState::Unrelated);
        assert_eq!(l.reason.as_deref(), Some("unreadable"));
    }

    #[tokio::test]
    async fn a_link_that_has_already_been_answered_costs_no_call() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        core.store
            .set_link_state(&ids[0], &ids[1], LinkState::Dismissed, None, None)
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(
            core.store.get_link(&ids[0], &ids[1]).await.unwrap().unwrap().state,
            LinkState::Dismissed
        );
    }
```

`FakeCompleter::replying` may not exist — check `src/infer/fake.rs` and add it if not, in the shape the other fakes use (a stored `String` returned from `complete`). That is part of this step.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib jobs::associate infer::prompt`
Expected: FAIL — `cannot find function link_prompt`, `parse_link`.

- [ ] **Step 3: Write the prompt**

In `src/infer/prompt.rs`, after `dedupe_prompt`:

```rust
pub const LINK_SYSTEM: &str = r#"Two knowledge artifacts keep being retrieved by the same searches. You say what that means, in one line a reader would find useful.

Choose exactly one:

- "related" — being needed together makes sense: one is the configuration and the other its failure mode, one is the procedure and the other the tool it needs, one explains why the other is done. Say what the relation is, in the reader's own terms, in one sentence.
- "unrelated" — the searches that returned both were about something else, and there is no connection worth showing. A shared word is not a connection.
- "duplicate" — they say the same thing in different words. Only this, and not "related", when neither adds anything the other lacks.

Judge the relation between the artifacts, not their similarity. Two texts that share no vocabulary at all can be strongly related; two that read alike can be about different subjects.

Reply with JSON only, no commentary, in exactly this shape:

{"relation": "related", "reason": "..."}

- relation: one of "related", "unrelated", "duplicate".
- reason: one sentence. For "related" it is shown to the reader beside the link, so write it for them and not about the task."#;

/// Two artifacts, and the questions that kept returning both.
///
/// The cues are the evidence. Without them this asks whether two arbitrary texts
/// are related, which is a worse question with a worse answer: what is being
/// judged is why these two keep being *needed at once*.
///
/// `attempt` is in the prompt for the same reason it is in `dedupe_prompt`: the
/// endpoint caches by exact prompt text, and a retry of a reply the parser could
/// not read would otherwise re-read the same unreadable bytes. Zero adds
/// nothing, so a first ask stays byte-identical between runs.
pub fn link_prompt(
    a: (&str, &str),
    b: (&str, &str),
    cues: &[String],
    attempt: i64,
) -> String {
    let mut s = String::new();
    if attempt > 0 {
        s.push_str(&format!("(attempt {})\n", attempt + 1));
    }
    s.push_str(&format!(
        "----- ARTIFACT A -----\nTitle: {}\n\n{}\n----- ARTIFACT B -----\nTitle: {}\n\n{}\n----- END -----",
        a.0, a.1, b.0, b.1
    ));
    if !cues.is_empty() {
        s.push_str(&format!(
            "\n\nBoth were returned by these searches: {}.",
            cues.join("; ")
        ));
    }
    s
}
```

- [ ] **Step 4: Add the pair-with-detail writer**

In `src/store/pairs.rs`, beside `record_pair`:

```rust
    /// File a pair for review, saying where it came from.
    ///
    /// `record_pair` with a `detail`, for the one producer that is not the
    /// similarity sweep: a link the judge found to be a disguised duplicate has
    /// no cosine behind it, so its `score` is genuinely zero and the detail is
    /// what explains the row on a page that otherwise renders a percentage.
    pub async fn record_pair_with_detail(
        &self,
        a: &str,
        b: &str,
        score: f32,
        detail: &str,
    ) -> Result<bool> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let res = sqlx::query(
            "INSERT OR IGNORE INTO artifact_pairs (a_id, b_id, score, state, detail, created_at)
             VALUES (?, ?, ?, 'pending', ?, ?)",
        )
        .bind(a)
        .bind(b)
        .bind(score as f64)
        .bind(detail)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }
```

`INSERT OR IGNORE`, like `record_pair`: a pair an operator already dismissed must not be re-filed by a link.

- [ ] **Step 5: Write the judge**

In `src/jobs/associate.rs`:

```rust
use crate::error::Error;
use crate::store::links::LinkState;

/// Unreadable replies after which a link is shelved rather than asked forever.
pub const MAX_UNREADABLE_LINK_JUDGEMENTS: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkVerdict {
    Related,
    Unrelated,
    /// The two say the same thing and the embedding failed to notice.
    Duplicate,
}

/// A reply that cannot be read is an error, not a verdict.
///
/// Defaulting to `unrelated` would quietly close real relations; defaulting to
/// `related` would show the reader a line the model never wrote. Failing leaves
/// the link `learning` — still visible, still binding — and the unit retries
/// under the queue's backoff with a prompt that differs by its attempt number.
pub fn parse_link(body: &str) -> Result<(LinkVerdict, String)> {
    #[derive(serde::Deserialize)]
    struct Raw {
        relation: String,
        #[serde(default)]
        reason: Option<String>,
    }
    let r: Raw = serde_json::from_str(crate::infer::prompt::extract_json(body)).map_err(|e| {
        Error::MalformedLlmOutput(format!("link reply was not the expected JSON: {e}"))
    })?;
    let verdict = match r.relation.as_str() {
        "related" => LinkVerdict::Related,
        "unrelated" => LinkVerdict::Unrelated,
        "duplicate" => LinkVerdict::Duplicate,
        other => {
            return Err(Error::MalformedLlmOutput(format!(
                "link reply named no relation this understands: {other}"
            )));
        }
    };
    Ok((verdict, r.reason.unwrap_or_default()))
}

/// One link, one call.
pub async fn judge(core: &Core, target: &str) -> Result<()> {
    let (a_id, b_id) = target.split_once('|').ok_or(Error::NotFound)?;
    let Some(link) = core.store.get_link(a_id, b_id).await? else {
        // Pruned, or one side deleted, while the unit waited out a backoff.
        return Ok(());
    };
    if link.state != LinkState::Learning {
        // Answered by an operator's dismissal, or by a sweep that reopened and
        // a sibling unit that then settled it.
        return Ok(());
    }
    let a = core.store.get_artifact(&link.a_id).await?;
    let b = core.store.get_artifact(&link.b_id).await?;
    // Re-checked here and not only when the unit was armed: a side can be
    // superseded or deprecated while this waits, and spending the scarcest
    // thing in the system on an artifact nobody will be shown buys nothing.
    if !a.in_results() || !b.in_results() {
        return Ok(());
    }

    let cues: Vec<String> = link.cues.iter().map(|c| c.q.clone()).collect();
    let permit = core.gate.background().await;
    let reply = core
        .judge
        .complete(
            crate::infer::prompt::LINK_SYSTEM,
            &crate::infer::prompt::link_prompt(
                (a.title.as_deref().unwrap_or("untitled"), &a.text),
                (b.title.as_deref().unwrap_or("untitled"), &b.text),
                &cues,
                link.judge_attempts,
            ),
        )
        .await;
    permit.finished();
    // A call the endpoint never answered says nothing about the link: it stays
    // `learning`, stays visible, and the queue backs the unit off.
    let reply = reply?;

    let revs = Some((a.embed_rev, b.embed_rev));
    let (verdict, reason) = match parse_link(&reply) {
        Ok(v) => v,
        Err(e) => {
            // Counted only here, because this is the only failure that says
            // anything about the link itself.
            let attempts = core.store.record_link_judge_attempt(&link.a_id, &link.b_id).await?;
            if attempts >= MAX_UNREADABLE_LINK_JUDGEMENTS {
                tracing::warn!(target, attempts, "shelving a link the model will not answer for");
                core.store
                    .set_link_state(&link.a_id, &link.b_id, LinkState::Unrelated, Some("unreadable"), revs)
                    .await?;
                return Ok(());
            }
            tracing::warn!(target, attempts, reply_len = reply.len(), error = %e, "link reply unreadable");
            return Err(e);
        }
    };

    match verdict {
        LinkVerdict::Related => {
            core.store
                .set_link_state(&link.a_id, &link.b_id, LinkState::Related, Some(&reason), revs)
                .await?;
        }
        LinkVerdict::Unrelated => {
            core.store
                .set_link_state(&link.a_id, &link.b_id, LinkState::Unrelated, Some(&reason), revs)
                .await?;
        }
        LinkVerdict::Duplicate => {
            // Handed over rather than acted on: consolidation owns every
            // decision that hides an artifact, with its own guards and its own
            // undo. The score is zero because no cosine was ever measured —
            // that is what `detail` is there to explain on the review page.
            core.store
                .record_pair_with_detail(&link.a_id, &link.b_id, 0.0, "link")
                .await?;
            core.store
                .set_link_state(
                    &link.a_id,
                    &link.b_id,
                    LinkState::Related,
                    Some("same content; handed to consolidation"),
                    revs,
                )
                .await?;
        }
    }
    Ok(())
}
```

`extract_json` in `src/infer/prompt.rs` is private today — make it `pub(crate)` so the parser here can reuse the same fence-stripping the dedupe parser gets.

- [ ] **Step 6: Run the tests**

Run: `cargo test --lib jobs::associate infer::prompt store::pairs`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/infer/prompt.rs src/infer/fake.rs src/jobs/associate.rs src/store/pairs.rs
git commit -m "feat(associate): name what a strong cross-corpus link is"
```

---

## Task 9: Priming the ranked order

**Files:**
- Modify: `src/core/search.rs` (`SearchResult::primed`, `prime`, one read in `search_with`)
- Test: `src/core/search.rs`

**Interfaces:**
- Consumes: `Store::activation_of`, `links::decayed`, `Core::{associate, activation}`.
- Produces: `SearchResult.primed: bool` (skipped when false), private `fn prime(Vec<SearchResult>, &HashMap<String, f64>, f64, usize) -> Vec<SearchResult>`, private `async fn activation_now(&self, ids: &[String]) -> HashMap<String, f64>`.

- [ ] **Step 1: Write the failing tests**

In `src/core/search.rs` tests:

```rust
    fn ranked(ids: &[&str]) -> Vec<SearchResult> {
        ids.iter()
            .map(|id| SearchResult {
                artifact_id: (*id).into(),
                corpus_id: "c".into(),
                title: None,
                text: String::new(),
                category: None,
                tags: vec![],
                score: 0.5,
                status: None,
                superseded_by: None,
                last_verified_at: None,
                weak: false,
                primed: false,
            })
            .collect()
    }

    fn order(rs: &[SearchResult]) -> Vec<&str> {
        rs.iter().map(|r| r.artifact_id.as_str()).collect()
    }

    #[test]
    fn a_hit_climbs_at_most_two_places_and_never_past_the_first() {
        // Rank-based rather than score-based on purpose: hybrid scores are
        // fused ranks and mean nothing across queries, while "moved up two
        // places" means the same thing every time — and can be tested here.
        let act = HashMap::from([("d".to_string(), 4.0)]);
        let out = prime(ranked(&["a", "b", "c", "d"]), &act, 0.5, 2);
        assert_eq!(order(&out), vec!["a", "d", "b", "c"]);
        assert!(out[1].primed, "the hit that moved must say so");
        assert!(!out[2].primed, "the hit it passed did not move up");
    }

    #[test]
    fn the_most_active_hit_cannot_displace_an_exact_match() {
        let act = HashMap::from([("b".to_string(), 9.0)]);
        let out = prime(ranked(&["a", "b", "c"]), &act, 0.5, 2);
        assert_eq!(order(&out), vec!["a", "b", "c"]);
        assert!(out.iter().all(|r| !r.primed));
    }

    #[test]
    fn a_lift_of_zero_turns_priming_off_entirely() {
        let act = HashMap::from([("d".to_string(), 4.0)]);
        let out = prime(ranked(&["a", "b", "c", "d"]), &act, 0.5, 0);
        assert_eq!(order(&out), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn activation_that_is_merely_higher_does_not_move_anything() {
        // The margin is what keeps this from reshuffling every list: two hits
        // that are both somewhat active stay in the order the ranking gave.
        let act = HashMap::from([("c".to_string(), 4.0), ("d".to_string(), 3.6)]);
        let out = prime(ranked(&["a", "b", "c", "d"]), &act, 0.5, 2);
        assert_eq!(order(&out), vec!["a", "c", "b", "d"], "only c clears the margin");
    }

    #[tokio::test]
    async fn priming_changes_the_order_a_search_returns_and_says_which_hit_moved() {
        let mut core = test_core().await;
        core.associate.prime_lift = 2;
        let texts: Vec<(&str, &str, &[&str])> = (0..6)
            .map(|_| ("alpha text about it", "note", &[][..]))
            .collect();
        seed(&core, &texts).await;
        reembed_all(&core).await;

        let plain = {
            let mut off = core.clone();
            off.associate.prime_lift = 0;
            off.search(&q("alpha text about it"), Door::Ui).await.unwrap()
        };
        assert!(plain.len() >= 4, "this test needs a list to reorder");
        assert!(plain.iter().all(|r| !r.primed));

        // The one at the bottom is the one people actually keep confirming.
        let bottom = plain.last().unwrap().artifact_id.clone();
        sqlx::query("UPDATE artifacts SET activation = 100.0, activated_at = ? WHERE id = ?")
            .bind(now_secs())
            .bind(&bottom)
            .execute(&core.store.pool)
            .await
            .unwrap();

        let primed = core.search(&q("alpha text about it"), Door::Ui).await.unwrap();
        let moved = primed.iter().position(|r| r.artifact_id == bottom).unwrap();
        assert!(
            moved < plain.len() - 1,
            "activation did not reach the ranked list at all"
        );
        assert!(primed[moved].primed, "a hit that moved must say so");
        assert_ne!(primed[0].artifact_id, bottom, "rank 1 was displaced");
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib core::search`
Expected: FAIL — `struct SearchResult has no field named primed`.

- [ ] **Step 3: Write the implementation**

Add the field to `SearchResult` (after `weak`):

```rust
    /// This hit moved up because it is more accessible than the ones it passed
    /// — recently and often reached. Bounded by `associate.prime_lift`, never
    /// past rank 1, and said out loud wherever it happened: nothing about the
    /// order is silent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub primed: bool,
```

Set `primed: false` in `impl From<SearchHit> for SearchResult` and at the one other construction site in `search_with`.

Add the two functions:

```rust
/// Move hits up on activation, within hard bounds.
///
/// Rank-based rather than score-based: hybrid scores are fused ranks and mean
/// nothing across queries, while "moved up two places" means the same thing
/// every time. The activation is normalised within this one list, so `margin` is
/// a fraction of the most accessible hit here rather than an absolute weight —
/// which is what makes one default work for a list of ones and a list of
/// hundreds.
///
/// Index 0 is untouchable and index 1 cannot move, because moving it would
/// displace rank 1. An exact match is never buried.
fn prime(
    results: Vec<SearchResult>,
    activation: &HashMap<String, f64>,
    margin: f64,
    lift: usize,
) -> Vec<SearchResult> {
    if lift == 0 || results.len() < 3 {
        return results;
    }
    let max = activation.values().copied().fold(0.0f64, f64::max);
    if max <= 0.0 {
        return results;
    }
    let mut rows: Vec<(SearchResult, f64, usize)> = results
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let a = activation.get(&r.artifact_id).copied().unwrap_or(0.0) / max;
            (r, a, i)
        })
        .collect();

    for i in 2..rows.len() {
        let mut j = i;
        let mut moved = 0;
        while j > 1 && moved < lift && rows[j].1 - rows[j - 1].1 > margin {
            rows.swap(j - 1, j);
            j -= 1;
            moved += 1;
        }
    }

    rows.into_iter()
        .enumerate()
        .map(|(pos, (mut r, _, was))| {
            // Only a climb is priming. A hit that was passed did not move up,
            // and labelling it would say something untrue about it.
            r.primed = pos < was;
            r
        })
        .collect()
}
```

and on `Core`:

```rust
    /// Each artifact's activation, already decayed to now.
    ///
    /// The one SQLite read the query path takes. It can only add: a failure is
    /// one warning and an empty map, and everything downstream then behaves
    /// exactly as it did before any of this existed.
    async fn activation_now(&self, ids: &[String]) -> HashMap<String, f64> {
        let at = now_secs();
        match self.store.activation_of(ids).await {
            Ok(rows) => rows
                .into_iter()
                .map(|(id, (value, stamp))| {
                    (
                        id,
                        crate::store::links::decayed(
                            value,
                            stamp,
                            at,
                            self.activation.half_life_days,
                        ),
                    )
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "could not read activation; results are unprimed");
                HashMap::new()
            }
        }
    }
```

In `search_with`, immediately after the rerank block and before the feedback capture block:

```rust
        // Before capture and before the truncate, so the pool that is recorded
        // is the order the searcher was actually shown — a judged rank has to
        // be a rank that happened. Bounded by `prime_lift` and never past rank
        // 1, so this can reorder near-ties and nothing else.
        if self.associate.enabled && self.associate.prime_lift > 0 {
            let ids: Vec<String> = results.iter().map(|r| r.artifact_id.clone()).collect();
            let activation = self.activation_now(&ids).await;
            results = prime(
                results,
                &activation,
                self.associate.prime_margin,
                self.associate.prime_lift,
            );
        }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib core::search`
Expected: PASS.

- [ ] **Step 5: Check nothing else moved**

Run: `cargo test --locked`
Expected: PASS. `tests/eval.rs` in particular: priming is off in `test_core` only if `prime_lift` is 0 — it is not, so if an eval assertion moves, that is the harness telling you priming changed ranking. Record the before/after numbers in the commit message rather than adjusting the assertion.

- [ ] **Step 6: Commit**

```bash
git add src/core/search.rs
git commit -m "feat(search): let activation prime the order, visibly and within bounds"
```

---

## Task 10: One hop of association

**Files:**
- Modify: `src/core/search.rs`
- Test: `src/core/search.rs`

**Interfaces:**
- Consumes: `Store::links_from`, `Store::get_artifact`, `LinkedTo`.
- Produces: `SearchResult.via: Option<String>`, `SearchResult.reason: Option<String>`, private `async fn associated(&self, results: &[SearchResult]) -> Vec<SearchResult>`.

- [ ] **Step 1: Write the failing tests**

In `src/core/search.rs` tests. Every one of them needs to name a specific
artifact, and `list_all_artifact_ids` promises no order — so they look ids up by
the text they were seeded with:

```rust
    /// The id of the artifact whose text is exactly this. Ordering from
    /// `list_all_artifact_ids` is not promised, and a test that assumed one
    /// would pass or fail on which row SQLite happened to return first.
    async fn id_of(core: &crate::core::Core, text: &str) -> String {
        sqlx::query_scalar("SELECT id FROM artifacts WHERE text = ?")
            .bind(text)
            .fetch_one(&core.store.pool)
            .await
            .unwrap()
    }
```

```rust
    #[tokio::test]
    async fn a_linked_artifact_is_recalled_beside_the_answer_and_says_what_recalled_it() {
        let core = test_core().await;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        seed_from(&core, "two", &[("something else entirely", "note", &[])]).await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text").await;
        let b = id_of(&core, "something else entirely").await;
        core.store
            .bump_link(&a, &b, 5.0, Some("both of these"), 30.0, now_secs())
            .await
            .unwrap();

        let mut query = q("t0\nalpha text");
        query.limit = 1;
        let out = core.search(&query, Door::Ui).await.unwrap();

        assert_eq!(out.len(), 2, "the association was not appended: {out:?}");
        assert_eq!(out[0].artifact_id, a);
        assert_eq!(out[0].via, None, "a ranked hit was not recalled by anything");
        assert_eq!(out[1].artifact_id, b);
        assert_eq!(out[1].via.as_deref(), Some(a.as_str()));
    }

    #[tokio::test]
    async fn an_artifact_already_in_the_answer_is_not_recalled_again() {
        let core = test_core().await;
        seed_from(&core, "one", &[("alpha text", "note", &[]), ("alpha other", "note", &[])]).await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text").await;
        let b = id_of(&core, "alpha other").await;
        core.store
            .bump_link(&a, &b, 5.0, Some("q"), 30.0, now_secs())
            .await
            .unwrap();

        let out = core.search(&q("alpha"), Door::Ui).await.unwrap();
        let seen: std::collections::HashSet<&str> =
            out.iter().map(|r| r.artifact_id.as_str()).collect();
        assert_eq!(seen.len(), out.len(), "an artifact was returned twice");
    }

    #[tokio::test]
    async fn a_recalled_artifact_does_not_feed_the_learning_that_produced_it() {
        // The failure mode of any Hebbian system: a link recalls an artifact, is
        // strengthened by having done so, and recalls it harder next time. The
        // recalled hit is not written as a candidate and does not count as a
        // retrieval — both loops are closed by construction.
        let mut core = test_core().await;
        core.feedback.enabled = true;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        seed_from(&core, "two", &[("something else entirely", "note", &[])]).await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text").await;
        let b = id_of(&core, "something else entirely").await;
        core.store
            .bump_link(&a, &b, 5.0, Some("q"), 30.0, now_secs())
            .await
            .unwrap();
        let before = core.store.activation_of(&[b.clone()]).await.unwrap()[&b].0;

        let mut query = q("t0\nalpha text");
        query.limit = 1;
        core.search(&query, Door::Ui).await.unwrap();
        core.background.wait_idle().await;

        let recorded: Vec<String> = sqlx::query_scalar("SELECT artifact_id FROM search_candidates")
            .fetch_all(&core.store.pool)
            .await
            .unwrap();
        assert!(!recorded.contains(&b), "the recalled artifact was recorded as a candidate");
        let after = core.store.activation_of(&[b.clone()]).await.unwrap()[&b].0;
        assert!((after - before).abs() < 1e-9, "being recalled raised activation");
    }

    #[tokio::test]
    async fn a_hidden_artifact_is_never_recalled_by_association() {
        let core = test_core().await;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        seed_from(&core, "two", &[("something else entirely", "note", &[])]).await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text").await;
        let b = id_of(&core, "something else entirely").await;
        core.store
            .bump_link(&a, &b, 5.0, Some("q"), 30.0, now_secs())
            .await
            .unwrap();
        core.deprecate(&b).await.unwrap();

        let mut query = q("t0\nalpha text");
        query.limit = 1;
        assert_eq!(core.search(&query, Door::Ui).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_weak_link_is_not_strong_enough_to_recall_anything() {
        let core = test_core().await;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        seed_from(&core, "two", &[("something else entirely", "note", &[])]).await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text").await;
        let b = id_of(&core, "something else entirely").await;
        // One co-appearance, against a `show_min` of 2.0.
        core.store
            .bump_link(&a, &b, 1.0, Some("q"), 30.0, now_secs())
            .await
            .unwrap();

        let mut query = q("t0\nalpha text");
        query.limit = 1;
        assert_eq!(core.search(&query, Door::Ui).await.unwrap().len(), 1);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib core::search`
Expected: FAIL — `struct SearchResult has no field named via`.

- [ ] **Step 3: Write the implementation**

Two more fields on `SearchResult`:

```rust
    /// The ranked hit that recalled this one. `None` for a ranked hit — which
    /// is every hit inside `limit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// What the judge said the relation is, where a link has been judged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
```

Set both to `None` at every existing construction site — including the `ranked` test helper Task 9 added, which is a struct literal and will not compile without them.

```rust
    /// Artifacts linked to the top of the answer, appended beside it.
    ///
    /// One hop only. Spreading further is what a graph view would be for, and
    /// there is none. Everything here is additive: it never removes or reorders
    /// a ranked hit, and a store that will not answer produces an empty list and
    /// one warning.
    async fn associated(&self, results: &[SearchResult]) -> Vec<SearchResult> {
        let anchors: Vec<String> = results
            .iter()
            .take(self.associate.spread_from)
            .map(|r| r.artifact_id.clone())
            .collect();
        if anchors.is_empty() || self.associate.spread_max == 0 {
            return Vec::new();
        }
        let links = match self
            .store
            .links_from(
                &anchors,
                &[
                    crate::store::links::LinkState::Learning,
                    crate::store::links::LinkState::Related,
                ],
                self.associate.half_life_days,
                now_secs(),
                self.associate.show_min,
            )
            .await
        {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "could not read links; results are unassociated");
                return Vec::new();
            }
        };

        let mut have: std::collections::HashSet<String> =
            results.iter().map(|r| r.artifact_id.clone()).collect();
        let mut out = Vec::new();
        for l in links {
            if out.len() >= self.associate.spread_max {
                break;
            }
            if !have.insert(l.other.clone()) {
                continue;
            }
            // Read from SQLite rather than the vector store: the row is already
            // one connection away, and the payload adds nothing this needs.
            let Ok(c) = self.store.get_artifact(&l.other).await else {
                continue;
            };
            out.push(SearchResult {
                artifact_id: c.id,
                corpus_id: c.corpus_id.unwrap_or_default(),
                title: c.title,
                text: c.text,
                category: c.category,
                tags: c.tags,
                // Not a rank and not a similarity: this hit did not compete for
                // a place in the list, it was recalled beside it.
                score: 0.0,
                status: Some(c.status),
                superseded_by: c.superseded_by,
                last_verified_at: c.last_verified_at,
                weak: false,
                primed: false,
                via: Some(l.via),
                reason: l.reason,
            });
        }
        out
    }
```

In `search_with`, at the very end — after `results.truncate(limit)` and after the `mark_seen` for the ranked list:

```rust
        results.truncate(limit);
        if query.mark {
            // A query answered these, so they count as retrievals.
            self.mark_seen(&results, &hit_counts, true);
        }
        // After the truncate and after capture, so an association can only ever
        // add: it is outside `limit`, outside the recorded pool, and outside the
        // retrieval count. See `Touch::shown`.
        if self.associate.enabled {
            let recalled = self.associated(&results).await;
            if !recalled.is_empty() {
                self.mark_seen(&recalled, &HashMap::new(), false);
                results.extend(recalled);
            }
        }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib core::search`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core/search.rs
git commit -m "feat(search): recall what was reached together, beside the ranked list"
```

---

## Task 11: Retrieval and opening raise activation

**Files:**
- Modify: `src/core/search.rs` (`mark_seen`, `mark_artifact_seen`)
- Test: `src/core/search.rs`

**Interfaces:**
- Consumes: `Store::bump_activation`, `Core::activation`.
- Produces: no new API — the two existing touch points gain a second background write.

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn a_deliberate_search_makes_what_it_returned_more_accessible() {
        let core = test_core().await;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;
        let id = core.store.list_all_artifact_ids().await.unwrap()[0].clone();
        let before = core.store.activation_of(&[id.clone()]).await.unwrap()[&id].0;

        core.search(&q("alpha"), Door::Ui).await.unwrap();
        core.background.wait_idle().await;

        let after = core.store.activation_of(&[id.clone()]).await.unwrap()[&id].0;
        assert!(after > before, "a retrieval raised nothing");
    }

    #[tokio::test]
    async fn typing_does_not_make_what_it_happened_to_match_more_accessible() {
        // The same rule as `last_seen_at`: an incremental request is not a
        // retrieval, and letting every keystroke raise activation would make
        // accessibility a function of how slowly someone types.
        let core = test_core().await;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;
        let id = core.store.list_all_artifact_ids().await.unwrap()[0].clone();
        let before = core.store.activation_of(&[id.clone()]).await.unwrap()[&id].0;

        let mut query = q("alpha");
        query.mark = false;
        core.search(&query, Door::Ui).await.unwrap();
        core.background.wait_idle().await;

        assert_eq!(core.store.activation_of(&[id.clone()]).await.unwrap()[&id].0, before);
    }

    #[tokio::test]
    async fn being_drawn_at_random_raises_nothing() {
        // `resurface` shows what has been forgotten. Counting that as a reason
        // to be more accessible is the loop this whole design is built to close.
        let core = test_core().await;
        seed_from(&core, "old", &[("long forgotten", "c", &[])]).await;
        sqlx::query("UPDATE artifacts SET created_at = ?")
            .bind(now_secs() - FORGOTTEN_AFTER_DAYS * SECONDS_PER_DAY - 1)
            .execute(&core.store.pool)
            .await
            .unwrap();
        reembed_all(&core).await;
        let id = core.store.list_all_artifact_ids().await.unwrap()[0].clone();
        let before = core.store.activation_of(&[id.clone()]).await.unwrap()[&id].0;

        core.resurface(10).await.unwrap();
        core.background.wait_idle().await;

        assert_eq!(core.store.activation_of(&[id.clone()]).await.unwrap()[&id].0, before);
    }

    #[tokio::test]
    async fn opening_an_artifact_makes_it_more_accessible_by_less_than_a_retrieval() {
        let core = test_core().await;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        let id = core.store.list_all_artifact_ids().await.unwrap()[0].clone();
        let before = core.store.activation_of(&[id.clone()]).await.unwrap()[&id].0;

        core.mark_artifact_seen(&id);
        core.background.wait_idle().await;

        let after = core.store.activation_of(&[id.clone()]).await.unwrap()[&id].0;
        assert!((after - before - core.activation.opened).abs() < 1e-6);
        assert!(core.activation.opened < core.activation.retrieved);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib core::search`
Expected: FAIL — activation unchanged.

- [ ] **Step 3: Write the implementation**

In `mark_seen`, after the existing `self.background.spawn(...)` block:

```rust
        // Only a retrieval raises accessibility. A list nobody asked for —
        // `resurface`, an association — is shown, not reached, and the row in
        // §5.4 that reads zero is the one-way guard this implements.
        if counts_as_hit {
            let ids: Vec<String> = results.iter().map(|r| r.artifact_id.clone()).collect();
            let store = self.store.clone();
            let (delta, half_life) = (self.activation.retrieved, self.activation.half_life_days);
            let at = now_secs();
            self.background.spawn(async move {
                if let Err(e) = store.bump_activation(&ids, delta, half_life, at).await {
                    tracing::warn!(error = %e, "could not raise activation for a search");
                }
            });
        }
```

In `mark_artifact_seen`, the same shape with `self.activation.opened` and `std::slice::from_ref`-style single-id vec, and the comment:

```rust
        // An open is a deliberate act and counts for less than a retrieval:
        // clicking a candidate says you looked, not that it answered.
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core/search.rs
git commit -m "feat(activation): raise it on retrieval and on opening, and nowhere else"
```

---

## Task 12: Associated hits in the results, and in MCP

**Files:**
- Modify: `src/web/ui.rs` (`RenderedResult`, `ResultsTemplate`, `render_hit`, `search_results`)
- Modify: `src/web/templates/_results.html`
- Modify: `src/assets/app.css` (one rule for the divider)
- Modify: `src/mcp/mod.rs` (`format_search_results`)
- Test: `src/web/ui.rs`, `src/mcp/mod.rs`

**Interfaces:**
- Consumes: `SearchResult.{primed, via, reason}`.
- Produces: `RenderedResult.{primed: bool, via_title: Option<String>, reason: Option<String>}`, `ResultsTemplate.associated: Vec<RenderedResult>`.

- [ ] **Step 1: Write the failing tests**

In `src/web/ui.rs` tests. The harness is the module's own `app_session_and_core`
and `get_body`, as every other page test uses; `artifacts` seeds one corpus and
returns the ids in the order it was given the titles.

```rust
    #[tokio::test]
    fn rendered(via: Option<&str>, reason: Option<&str>) -> RenderedResult {
        RenderedResult {
            artifact_id: "a1".into(),
            title: "The one that was recalled".into(),
            html: String::new(),
            snippet: "a snippet".into(),
            category: None,
            tags: vec![],
            corpus_id: "c1".into(),
            rank: String::new(),
            weak: false,
            primed: false,
            via_title: via.map(str::to_string),
            reason: reason.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn the_results_name_what_recalled_an_associated_hit() {
        // An associated hit says which hit recalled it, or it is an unexplained
        // result in a list the reader believes is ranked. Rendered directly
        // rather than driven through a search: the UI handler asks for the
        // default limit, so on any base small enough to reason about, every
        // artifact is already ranked and there is nothing left to recall. What
        // this task changed is the split and the copy, and that is what this
        // pins.
        let template = ResultsTemplate {
            results: vec![],
            associated: vec![rendered(Some("Mounting E01 images"), None)],
            all_weak: false,
            terms: String::new(),
            timing: String::new(),
        };
        let body = template.render().unwrap();
        assert!(body.contains("Recalled by association"), "{body}");
        assert!(body.contains("seen together with"), "{body}");
        assert!(body.contains("Mounting E01 images"), "{body}");

        // A judged link says what the relation is instead of what was asked.
        let judged = ResultsTemplate {
            results: vec![],
            associated: vec![rendered(Some("Mounting E01 images"), Some("the tool and its errors"))],
            all_weak: false,
            terms: String::new(),
            timing: String::new(),
        };
        let body = judged.render().unwrap();
        assert!(body.contains("the tool and its errors"), "{body}");
        assert!(!body.contains("seen together with"), "{body}");
    }

    #[tokio::test]
    async fn an_association_cannot_make_the_answer_look_worse_than_it_was() {
        // `all_weak` is a statement about how well the *query* was answered. An
        // associated hit did not answer the query at all, so counting it either
        // way would put a warning over a good list, or take one off a bad one.
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["alpha text"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();

        let body = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;
        assert!(!body.contains("Recalled by association"), "{body}");
    }
```

`ResultsTemplate` and `RenderedResult` are private to the module, which is where
this test lives; `render()` comes from `askama::Template`, already in scope.

In `src/mcp/mod.rs` tests:

```rust
    #[test]
    fn an_associated_result_says_it_was_recalled_rather_than_ranked() {
        // Straight into an agent's context: without this the extra result reads
        // as the fourth-best match for the query, which it is not.
        let out = format_search_results(&[hit("ranked", None), hit("recalled", Some("ranked"))]);
        assert!(out.contains("recalled beside"), "{out}");
    }
```

with a small `fn hit(id: &str, via: Option<&str>) -> SearchResult` helper in that test module.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib web::ui mcp::`
Expected: FAIL.

- [ ] **Step 3: Split the list in the handler**

In `src/web/ui.rs`:

```rust
pub struct RenderedResult {
    ...existing fields...
    /// This hit moved up on activation. A small marker, because the claim is
    /// small: it passed a near-tie, it did not become a better match.
    pub primed: bool,
    /// The title of the ranked hit that recalled this one. Set only on an
    /// associated hit, and it is what the row names.
    pub via_title: Option<String>,
    /// The judge's line, where the link was judged.
    pub reason: Option<String>,
}
```

`ResultsTemplate` gains `associated: Vec<RenderedResult>`.

In `search_results`, after the search returns:

```rust
    // The ranked answer and what it recalled are two lists on the page, and one
    // list here: an associated hit carries the id of the hit that recalled it,
    // and the title is looked up among the ranked ones rather than fetched.
    let titles: std::collections::HashMap<String, String> = hits
        .iter()
        .filter(|h| h.via.is_none())
        .map(|h| {
            (
                h.artifact_id.clone(),
                h.title.clone().unwrap_or_else(|| "Untitled".into()),
            )
        })
        .collect();
    let (ranked, recalled): (Vec<_>, Vec<_>) = hits.into_iter().partition(|h| h.via.is_none());
    let results: Vec<RenderedResult> = ranked
        .into_iter()
        .enumerate()
        .map(|(i, h)| render_hit(i, h, &titles))
        .collect();
    let associated: Vec<RenderedResult> = recalled
        .into_iter()
        .map(|h| render_hit(0, h, &titles))
        .collect();
```

`render_hit` gains the map as a third parameter and fills the three new fields; an associated hit gets `rank: String::new()` (it has no rank — the same reasoning that drops the rank on a weak hit), `primed: h.primed`, `via_title: h.via.as_ref().and_then(|v| titles.get(v).cloned())`, `reason: h.reason.clone()`.

Four other call sites take the new parameter: the answer citations at `src/web/ui.rs:1467`, and the three tests at `:1712`, `:2435` and `:2438`. All four pass `&Default::default()` — a citation and a rendered test fixture are ranked hits, so there is no `via` to resolve.

`all_weak` must be computed from `results` only: an association is not an answer to the query and cannot make the answer look better or worse than it was.

- [ ] **Step 4: Render it**

In `src/web/templates/_results.html`, before the closing `</div>` of the `data-terms` block:

```html
  {% if !associated.is_empty() %}
  {# Below a rule and under its own heading: these were not ranked against the
     query, they were recalled by something that was. Presenting them in the
     same list would be a claim about relevance that nothing here supports. #}
  <div class="assoc-rule" role="separator">Recalled by association</div>
  {% for r in associated %}
  <div class="rail-row">
    <a class="rail-item rail-assoc" role="option" aria-selected="false"
       href="/ui/artifacts/{{ r.artifact_id }}"
       hx-get="/ui/artifacts/{{ r.artifact_id }}?terms={{ terms|urlencode }}"
       hx-target="#pane" hx-swap="innerHTML" hx-push-url="true">
      <div class="rail-head">
        <span class="rail-title">{{ r.title }}</span>
      </div>
      {% if let Some(why) = r.reason %}
      <div class="rail-why">{{ why }}</div>
      {% else if let Some(via) = r.via_title %}
      <div class="rail-why">seen together with “{{ via }}”</div>
      {% endif %}
      <div class="rail-snippet">{{ r.snippet }}</div>
    </a>
  </div>
  {% endfor %}
  {% endif %}
```

and in the ranked loop, beside the rank span:

```html
        {% if r.primed %}
        <span class="badge" title="Moved up: you reach this one often">primed</span>
        {% endif %}
```

In `assets/app.css`, add `.assoc-rule` (a muted small-caps label with a top border) and `.rail-why` (muted, one line) in the style of the neighbouring rules.

- [ ] **Step 5: Say it in MCP too**

In `src/mcp/mod.rs`, inside the `map`:

```rust
            // An agent reads this as a ranked list unless it is told otherwise,
            // and an associated hit did not compete for its place.
            let how = match (&r.via, &r.reason) {
                (Some(_), Some(why)) => format!("recalled beside the answer — {why}"),
                (Some(_), None) => "recalled beside the answer".to_string(),
                (None, _) => format!("score {:.3}", r.score),
            };
```
and use `how` where `format!("_score {:.3}...")` used the score.

- [ ] **Step 6: Run the tests**

Run: `cargo test --locked`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/web/ui.rs src/web/templates/_results.html assets/app.css src/mcp/mod.rs
git commit -m "feat(ui): show what association recalled, and what priming moved"
```

---

## Task 13: "Seen together" in the detail pane

**Files:**
- Modify: `src/web/ui.rs` (`ArtifactDetail`, `build_artifact_detail`, one new route)
- Modify: `src/web/templates/_artifact_detail.html`
- Modify: `assets/app.css`
- Test: `src/web/ui.rs`

**Interfaces:**
- Consumes: `Store::{links_from, set_link_state, get_corpus}`.
- Produces: `pub struct SeenTogether { id, title, snippet, why: Option<String>, corpus_title: String, cross_corpus: bool }`, `ArtifactDetail.seen_together: Vec<SeenTogether>`, route `POST /ui/artifacts/{id}/links/{other}/dismiss`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn the_pane_lists_what_this_artifact_is_seen_together_with() {
        let core = crate::core::test_support::test_core().await;
        let ids = artifacts(&core, &["alpha text", "something else entirely"]).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("mount forensic image"), 30.0, crate::store::now())
            .await
            .unwrap();

        let d = build_artifact_detail(&core, &ids[0], "").await.unwrap();
        assert_eq!(d.seen_together.len(), 1);
        assert_eq!(d.seen_together[0].id, ids[1]);
        assert_eq!(
            d.seen_together[0].why.as_deref(),
            Some("when asking: mount forensic image"),
            "an unjudged link explains itself with the question that bound it"
        );
    }

    #[tokio::test]
    async fn a_judged_link_shows_the_judges_line_instead_of_the_query() {
        let core = crate::core::test_support::test_core().await;
        let ids = artifacts(&core, &["alpha text", "something else entirely"]).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        core.store
            .set_link_state(
                &ids[0],
                &ids[1],
                crate::store::links::LinkState::Related,
                Some("the tool and the error it prints"),
                Some((0, 0)),
            )
            .await
            .unwrap();

        let d = build_artifact_detail(&core, &ids[0], "").await.unwrap();
        assert_eq!(
            d.seen_together[0].why.as_deref(),
            Some("the tool and the error it prints")
        );
    }

    #[tokio::test]
    async fn dismissing_a_link_takes_it_out_for_good_without_losing_the_evidence() {
        // The weight stays, so the decision is auditable; the state is final,
        // so it is never shown, judged or pruned again.
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["alpha text", "something else entirely"]).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();

        app.clone()
            .oneshot(form(
                &format!("/ui/artifacts/{}/links/{}/dismiss", ids[0], ids[1]),
                &cookie,
                "",
            ))
            .await
            .unwrap();

        let l = core.store.get_link(&ids[0], &ids[1]).await.unwrap().unwrap();
        assert_eq!(l.state, crate::store::links::LinkState::Dismissed);
        assert!(l.weight > 0.0, "the evidence was thrown away with the decision");
        assert!(
            build_artifact_detail(&core, &ids[0], "").await.unwrap().seen_together.is_empty()
        );
    }

    #[tokio::test]
    async fn a_pane_still_renders_when_the_links_cannot_be_read() {
        // The associative layer can only add. It is not a reason to refuse to
        // show an artifact beside its source.
        let core = crate::core::test_support::test_core().await;
        let ids = artifacts(&core, &["alpha text"]).await;
        sqlx::query("DROP TABLE artifact_links")
            .execute(&core.store.pool)
            .await
            .unwrap();
        let d = build_artifact_detail(&core, &ids[0], "").await.unwrap();
        assert!(d.seen_together.is_empty());
    }
```

`form` is the module's existing POST helper (see `ops_lists_a_superseded_artifact_and_can_undo_it` around line 2988). The route must therefore accept a form POST with an empty body, like `unsupersede` does.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib web::ui`
Expected: FAIL — `no field seen_together`.

- [ ] **Step 3: Write the implementation**

```rust
/// A link, as one line in the pane. Beside the nearest neighbours, not instead
/// of them: one list is what this artifact resembles, the other is what it has
/// been needed alongside, and they answer different questions.
pub struct SeenTogether {
    pub id: String,
    pub title: String,
    pub snippet: String,
    /// The judge's line, or the question that bound the pair. `None` only for a
    /// link with neither, which is a link nothing can explain yet.
    pub why: Option<String>,
    pub corpus_title: String,
    /// Rendered emphasised: two documents needing each other is the finding.
    /// Two passages of one document needing each other is not.
    pub cross_corpus: bool,
}
```

`ArtifactDetail` gains `pub seen_together: Vec<SeenTogether>`, filled in `build_artifact_detail` beside `related`:

```rust
    // Unreadable links are not a missing pane, for the same reason a missing
    // neighbour list is not: this layer can only ever add.
    let anchor = vec![c.id.clone()];
    let seen_together = match core
        .store
        .links_from(
            &anchor,
            &[
                crate::store::links::LinkState::Learning,
                crate::store::links::LinkState::Related,
            ],
            core.associate.half_life_days,
            crate::store::now(),
            core.associate.show_min,
        )
        .await
    {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(artifact_id, error = %e, "no links for this pane");
            vec![]
        }
    };
    let mut seen = Vec::new();
    for l in seen_together.into_iter().take(RELATED_LIMIT) {
        let Ok(other) = core.store.get_artifact(&l.other).await else {
            continue;
        };
        let corpus_title = match &other.corpus_id {
            Some(id) => core
                .store
                .get_corpus(id)
                .await
                .ok()
                .and_then(|s| s.title_hint)
                .unwrap_or_else(|| "untitled".into()),
            // A merged artifact belongs to no document, which is worth saying
            // rather than leaving blank.
            None => "merged".to_string(),
        };
        seen.push(SeenTogether {
            title: title_of(&other),
            snippet: markdown::snippet(&other.text, 90),
            // The judge's line where there is one; otherwise the question that
            // bound them, which is the link's own explanation and free.
            why: l.reason.clone().or_else(|| {
                l.cues.first().map(|c| format!("when asking: {}", c.q))
            }),
            corpus_title,
            cross_corpus: l.cross_corpus,
            id: other.id,
        });
    }
```

Set `seen_together: seen` in the returned struct.

The route, registered beside the other artifact POSTs in this module's `Router`:

```rust
/// The operator saying this pair does not belong together.
///
/// Final for that pair: never shown, never judged, never pruned. The weight is
/// left exactly as it is, so the decision stays auditable against the evidence
/// that produced it — undoing one is out of scope, and Ops is where it would go.
async fn dismiss_link(
    State(st): State<AppState>,
    _id: Identity,
    Path((artifact_id, other_id)): Path<(String, String)>,
) -> Result<Response> {
    st.core
        .store
        .set_link_state(
            &artifact_id,
            &other_id,
            crate::store::links::LinkState::Dismissed,
            None,
            None,
        )
        .await?;
    // The row swaps itself out and leaves the pane alone, so the artifact you
    // were reading is still on screen afterwards.
    Ok(axum::response::Html(String::new()).into_response())
}
```

- [ ] **Step 4: Render it**

In `src/web/templates/_artifact_detail.html`, directly after the existing `Related` block's `{% endif %}`:

```html
      {% if !d.seen_together.is_empty() %}
      {# Beside the nearest neighbours, not instead of them: one list is what
         this resembles, the other is what it has been needed alongside. #}
      <div class="pane-label">Seen together</div>
      <div class="related">
        {% for r in d.seen_together %}
        <div class="seen-row{% if !r.cross_corpus %} muted{% endif %}">
          <a class="rail-item" href="/ui/artifacts/{{ r.id }}"
             hx-get="/ui/artifacts/{{ r.id }}"
             hx-target="closest [data-terms]" hx-swap="outerHTML"
             hx-push-url="true">
            <div class="rail-head"><span class="rail-title">{{ r.title }}</span></div>
            {% if let Some(why) = r.why %}<div class="rail-why">{{ why }}</div>{% endif %}
            <div class="rail-snippet">{{ r.snippet }} · {{ r.corpus_title }}</div>
          </a>
          <div class="rail-del">
            <button class="btn-icon" title="Not related" aria-label="Not related"
                    hx-post="/ui/artifacts/{{ d.id }}/links/{{ r.id }}/dismiss"
                    hx-target="closest .seen-row" hx-swap="outerHTML">
              {% include "_icon_hide.html" %}
            </button>
          </div>
        </div>
        {% endfor %}
      </div>
      {% endif %}
```

Add `.seen-row` to `assets/app.css` in the shape of `.rail-row`.

- [ ] **Step 5: Run the tests**

Run: `cargo test --locked`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/web/ui.rs src/web/templates/_artifact_detail.html assets/app.css
git commit -m "feat(ui): list what an artifact has been seen together with"
```

---

## Task 14: Ops, the example config, and the roadmap

**Files:**
- Modify: `src/web/ui.rs` (`OpsTemplate`, `ops`)
- Modify: `src/web/templates/ops.html`
- Modify: `config.example.toml`
- Modify: `ROADMAP.md`
- Test: `src/web/ui.rs`, `src/config.rs`

**Interfaces:**
- Consumes: `Store::link_counts`, `LinkCounts`.
- Produces: `OpsTemplate.links: Option<crate::store::links::LinkCounts>`.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn ops_says_how_many_links_there_are_and_how_many_are_named() {
        let (app, cookie, core) = app_session_and_core_with_feedback().await;
        let ids = artifacts(&core, &["alpha text", "something else entirely"]).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();

        let page = get_body(&app, &cookie, "/ui/ops").await;
        assert!(page.contains("2 links"), "{page}");
    }
```

`app_session_and_core` builds its core from `test_core()`, where `feedback.enabled` is false — and the Ops block is `None` in that case, so this test would assert against a section that renders nothing. Add a sibling helper beside it rather than mutating a core the router already holds:

```rust
    /// A session whose core records searches, which is what the association
    /// features are gated on. `app_session_and_core` cannot be reused: the
    /// router owns its own clone of the core, so flipping a flag afterwards
    /// changes the handle and not the app.
    async fn app_session_and_core_with_feedback() -> (axum::Router, String, crate::core::Core) {
        let mut core = crate::core::test_support::test_core().await;
        core.feedback.enabled = true;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        (app, cookie, handle)
    }
```

and call that in the test instead. The same applies to any later UI test that needs recorded searches switched on.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib web::ui`
Expected: FAIL.

- [ ] **Step 3: Add the numbers**

`OpsTemplate` gains:

```rust
    /// `None` when nothing is being learned, which renders nothing at all: a
    /// count of links on a base that records no searches is a line about a
    /// feature that is switched off.
    links: Option<crate::store::links::LinkCounts>,
```

In `ops`:

```rust
        links: match st.core.associate.enabled && st.core.feedback.enabled {
            true => Some(st.core.store.link_counts().await?),
            false => None,
        },
```

In `ops.html`, inside the existing counts paragraph:

```html
  {% if let Some(l) = links %}
  {{ l.total }} links, {{ l.related }} named, {{ l.judge_queue }} waiting on the judge.
  {% endif %}
```

- [ ] **Step 4: Document the settings**

Append to `config.example.toml`, after the `[feedback]` block:

```toml
# Links learned from co-retrieval, and a decaying accessibility per artifact.
# Both are learned from use and neither touches what is stored: the trace is
# fixed, access is plastic. Needs `feedback.enabled` — without recorded searches
# there is nothing to learn from, and saying otherwise is a warning at startup.
[associate]
enabled = true
# How often the sweep replays the search log. Pure SQLite, no model call.
interval_mins = 30
# A link unused for this long has half the strength it had. One co-appearance is
# +1 and one confirmed answer is +2, so these numbers are in "uses".
half_life_days = 30
# Below this, a link nobody has ruled on is forgotten. A judged one never is: a
# verified relation is about content, not use.
prune_below = 0.5
# Strength at which a link is worth showing, and at which it is worth one call.
show_min = 2.0
judge_min = 4.0
# Distinct questions a link needs before it is judged. One question asked six
# times is one question, and this is what keeps the judge cheap.
judge_min_queries = 3
judge_per_sweep = 10
# How many top hits are asked what they are linked to, and how many associated
# hits may be appended after the ranked list — outside `limit`, never instead
# of a ranked hit.
spread_from = 3
spread_max = 3
# How much more accessible a hit must be than the one above it to pass it, as a
# fraction of the most accessible hit in that list; and how many places it may
# climb. 0 turns priming off. Neither default moves until the eval harness has
# been run with it off and on against the frozen corpus.
prime_margin = 0.5
prime_lift = 2

[activation]
half_life_days = 14
# Returned by a deliberate search, opened in the pane, judged the answer. Being
# surfaced *because* of activation — resurfaced, or recalled by a link — raises
# nothing: that is the loop this design exists to close.
retrieved = 1.0
opened = 0.5
confirmed = 3.0
```

- [ ] **Step 5: Update the roadmap**

In `ROADMAP.md`, the "What is built" paragraph ends with the eval harness. Add to that list, before the closing sentence about design records:

```
Hebbian links learned from co-retrieval with bounded priming and one-hop
association in the results;
```

and under `## [Associative Memory]`, change the spec line's framing from what will be built to what was: replace "Spec: ... — Hebbian links learned from co-retrieval, ... The items below are the mechanisms that come after it, in order." with a sentence saying it is built, naming `[associate]` and `[activation]` as its switches, and keeping the list of what comes after unchanged.

- [ ] **Step 6: Run everything**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: PASS. `config::tests::the_example_config_carries_the_association_block` now reads the real block.

- [ ] **Step 7: Measure the layer crossing**

The spec requires this before merge: `total_ms` with and without the SQLite read. Run one search against a populated base with `associate.enabled = true` and once with `enabled = false`, and record both `total_ms` values (the UI prints them under the rail; the log line does too). Put the two numbers in the commit message. If the difference is more than a few milliseconds, stop and say so rather than merging.

- [ ] **Step 8: Commit**

```bash
git add src/web/ui.rs src/web/templates/ops.html config.example.toml ROADMAP.md
git commit -m "feat(associate): report links on Ops and document the settings"
```

---

## After the plan

Two things the spec asks for that are deliberately not tasks here:

- **Tuning `prime_margin` / `prime_lift`.** The defaults ship as written. Moving either is a separate change, made only after `cargo test --test eval` has been run against the frozen corpus with priming off and on, with both numbers recorded.
- **No backfill of historical links.** The watermarks start at the first sweep. An operator who wants the log replayed sets `associate.events_after = 0` in `meta` by hand — documented here, not automated.
