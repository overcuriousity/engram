# Autonomous Consolidation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Duplicate hygiene detects, judges and settles near-duplicate artifacts without an operator, on a base that grows, while every decision stays reversible and no stored value is ever lost.

**Architecture:** Detection moves from a sampled sweep to a per-artifact `Relate` unit armed when an artifact finishes embedding, which makes coverage complete instead of probabilistic. A `Dedupe` unit then settles a whole connected component of pending pairs in one model call, choosing between four verdicts; `duplicate` writes a new artifact with `provenance = 'merged'` and explicit lineage rows, superseding its roots. Two local verification passes refuse any merge that would drop a fact token or a literal. Value conflicts are escalated to a person and never merged.

**Tech Stack:** Rust, `sqlx` (SQLite), Qdrant REST, `tokio`, `serde_json`, `tracing`. Tests are `#[tokio::test]` in-module, using `crate::core::test_support::test_core()`, `Store::memory()`, and `crate::infer::fake::ScriptedCompleter`.

**Spec:** `docs/superpowers/specs/2026-08-14-autonomous-consolidation-design.md`

## Global Constraints

- **SQLite is the source of truth.** Qdrant is derived. Every lifecycle change writes SQLite first, then the payload. Never the reverse.
- **`src/store/schema.sql` is applied on every connect and cannot alter a table.** `migrate` parses **one column per line** and checks the columns back against the database. Keep one column per line. Changing `corpus_id` to nullable requires recreating the database (`schema.sql:9–12`).
- **A model call is the scarcest resource in the system.** No code path may spend one where a local rule already answers the question. Pairs at or above `auto_supersede` must never reach a `Dedupe` unit.
- **No artifact text is ever rewritten in place, and nothing is ever deleted on a similarity score.** Merging writes a *new* artifact and supersedes the originals.
- **A value conflict is never settled autonomously.** It becomes `PairState::Contradiction` and goes to Ops.
- **Test names are sentences** and carry the bug they pin in a comment above the assertions. Follow the existing style in `src/jobs/consolidate.rs`.
- **Every commit message ends with** `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.
- Run the full suite with `cargo test` before each commit; run a single test with `cargo test <test_name>`.

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src/store/schema.sql` | whole schema | Modify: `provenance`, `lifecycle_dirty`, nullable `corpus_id`, `artifact_sources` |
| `src/store/artifacts.rs` | artifact rows | Modify: `Provenance`, `Chunk.provenance`, `insert_merged_artifact`, dirty marking |
| `src/store/lineage.rs` | `artifact_sources` reads/writes | **Create** |
| `src/store/pairs.rs` | review queue | Modify: `NearIdentical`, `Oversized`, component expansion |
| `src/jobs/classify.rs` | the one place a scored pair is turned into a pair row | **Create** |
| `src/jobs/relate.rs` | per-artifact neighbour discovery | **Create** |
| `src/jobs/dedupe.rs` | one component, one call, one verdict | **Create** (replaces `judge.rs`) |
| `src/jobs/merge.rs` | writing and undoing a merge | **Create** |
| `src/jobs/consolidate.rs` | the sweep: backlog, backstop, clustering, repairs | Modify |
| `src/infer/prompt.rs` | prompts and parsers | Modify: `DEDUPE_SYSTEM`, `dedupe_prompt`, `parse_dedupe` |
| `src/infer/verify.rs` | literal checks | Reused unmodified; new caller |
| `src/config.rs` | `ConsolidateConfig` | Modify |
| `src/core/background.rs` | tickers | Modify: `spawn_dedupe_ticker` |
| `src/web/ui.rs` | Ops and detail pane | Modify |
| `tests/eval.rs` | retrieval scoring | Modify |

`judge.rs`, `relate.rs`, `dedupe.rs`, `merge.rs` and `classify.rs` are split by responsibility rather than folded into `consolidate.rs`, which is already 1745 lines. Each is one unit of the pipeline with one entry point.

---

## Phase 1 — Schema and data model

### Task 1: Artifacts gain a provenance kind

**Files:**
- Modify: `src/store/schema.sql:54-85`
- Modify: `src/store/artifacts.rs:34-103` (`Provenance`, `Chunk`), `:171` (`insert_artifacts`), `row_to_artifact`
- Modify: `src/vector/mod.rs` (`VectorPayload`), `src/vector/qdrant.rs`, `src/vector/memory.rs`, `src/jobs/embed.rs` (`payload_of`)
- Test: `src/store/artifacts.rs` (in-module `mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `Provenance::{Captured, Merged}` with `as_str(&self) -> &'static str` and `parse(s: &str) -> Provenance`; `Chunk.provenance: Provenance`; `Chunk.corpus_id: Option<String>`; `VectorPayload.provenance: Option<String>`.

**Note on `corpus_id`.** It becomes `Option<String>` on `Chunk`. Every existing reader of `chunk.corpus_id` must be updated; there are roughly two dozen. `VectorPayload.corpus_id` stays `String` and carries `""` for a merged artifact, because making it optional ripples through search filters for no gain — a corpus filter genuinely should not match an artifact that belongs to no corpus. `restore_artifact` reads `provenance` from the payload to decide whether `""` means "merged" or is a bug.

- [ ] **Step 1: Write the failing test**

In `src/store/artifacts.rs`, inside `mod tests`:

```rust
#[tokio::test]
async fn a_captured_artifact_is_captured_and_names_its_corpus() {
    // `provenance` is the discriminator every consumer branches on, never
    // `corpus_id IS NULL`. A null is an absence; a kind is an assertion, and
    // the failure modes merging can produce want to hang off an assertion.
    let s = Store::memory().await.unwrap();
    let src = s.insert_corpus("x", "web", None).await.unwrap();
    let made = s
        .insert_artifacts(
            &src.id,
            &[NewArtifact {
                ordinal: 0,
                text: "one".into(),
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

    assert_eq!(made[0].provenance, Provenance::Captured);
    assert_eq!(made[0].corpus_id.as_deref(), Some(src.id.as_str()));

    let read = s.get_artifact(&made[0].id).await.unwrap();
    assert_eq!(read.provenance, Provenance::Captured);
    assert_eq!(read.corpus_id.as_deref(), Some(src.id.as_str()));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test a_captured_artifact_is_captured_and_names_its_corpus`
Expected: FAIL — `Provenance` not found, `corpus_id` type mismatch.

- [ ] **Step 3: Add the schema columns**

In `src/store/schema.sql`, change the `artifacts` table. One column per line — `migrate` parses it that way:

```sql
CREATE TABLE IF NOT EXISTS artifacts (
  id               TEXT PRIMARY KEY,
  -- NULL for a merged artifact, which belongs to no single corpus. Claiming a
  -- corpus it did not come from would put wrong lines beside it in the detail
  -- pane, which is the specific dishonesty merging must not commit.
  corpus_id        TEXT REFERENCES corpora(id) ON DELETE CASCADE,
  -- 'captured' | 'merged'. The discriminator every consumer branches on.
  provenance       TEXT NOT NULL DEFAULT 'captured',
  -- Set in the same SQLite write that changes status/superseded_by, cleared
  -- once the payload write is acknowledged. The lifecycle repair reads this
  -- instead of scanning, so its cost is the open writes and not the (forever
  -- growing) set of hidden artifacts.
  lifecycle_dirty  INTEGER NOT NULL DEFAULT 0,
  ordinal          INTEGER NOT NULL,
  text             TEXT NOT NULL,
  ...
);
CREATE INDEX IF NOT EXISTS idx_artifacts_dirty ON artifacts(lifecycle_dirty) WHERE lifecycle_dirty = 1;
```

Leave every other column exactly as it is.

- [ ] **Step 4: Add the Rust type**

In `src/store/artifacts.rs`, beside `ArtifactStatus`:

```rust
/// Where an artifact's text came from.
///
/// `Captured` text was written by synthesis over one window of one corpus, so
/// it has a span, a segment, and corpus lines to render beside it. `Merged`
/// text was written by the dedupe pass out of two or more captured artifacts;
/// it has no corpus and no span, and names its roots through
/// `artifact_sources` instead. Nothing may treat the two alike — see
/// `verify`, which cannot check a merged artifact against a segment that does
/// not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Captured,
    Merged,
}

impl Provenance {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provenance::Captured => "captured",
            Provenance::Merged => "merged",
        }
    }
    pub fn parse(s: &str) -> Provenance {
        match s {
            "merged" => Provenance::Merged,
            _ => Provenance::Captured,
        }
    }
}
```

Change `Chunk`:

```rust
pub struct Chunk {
    pub id: String,
    /// `None` for a merged artifact. See `Provenance`.
    pub corpus_id: Option<String>,
    pub provenance: Provenance,
    ...
}
```

In `row_to_artifact`, read both: `corpus_id: r.get("corpus_id")` (sqlx maps a nullable TEXT to `Option<String>`) and `provenance: Provenance::parse(r.get::<String, _>("provenance").as_str())`.

In `insert_artifacts`, set `corpus_id: Some(corpus_id.to_string())`, `provenance: Provenance::Captured`, and add `provenance` to the INSERT column list bound to `"captured"`.

- [ ] **Step 5: Update every reader of `corpus_id`**

Run `cargo build` and fix each error. The mechanical fix is `.as_deref()` or `.clone().unwrap_or_default()` at the call site. Two need judgement:

- `src/jobs/embed.rs`'s `payload_of`: `corpus_id: c.corpus_id.clone().unwrap_or_default()` and add `provenance: Some(c.provenance.as_str().to_string())`.
- `src/store/artifacts.rs`'s `restore_artifact`: when the payload says `provenance == "merged"`, write `corpus_id` as NULL rather than `""`.

Add `provenance: Option<String>` to `VectorPayload` in `src/vector/mod.rs`, defaulting to `None`, and carry it through `src/vector/qdrant.rs` and `src/vector/memory.rs` beside the existing `status` field.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS, including `a_captured_artifact_is_captured_and_names_its_corpus`. The database must be recreated first: `rm -f engram.db`.

- [ ] **Step 7: Commit**

```bash
git add src/store/schema.sql src/store/artifacts.rs src/vector/ src/jobs/embed.rs
git commit -m "feat(store): artifacts carry a provenance kind

A merged artifact belongs to no corpus, so corpus_id becomes nullable and
provenance says which kind a row is. Consumers branch on the kind rather
than on the null: an absence is not an assertion, and the failure modes
merging can produce need one.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: Lineage, stored as the transitive closure

**Files:**
- Modify: `src/store/schema.sql` (append after `artifacts`)
- Create: `src/store/lineage.rs`
- Modify: `src/store/mod.rs` (add `pub mod lineage;`)
- Test: `src/store/lineage.rs` (in-module)

**Interfaces:**
- Consumes: `Provenance` from Task 1.
- Produces on `Store`:
  - `record_lineage(&self, child_id: &str, roots: &[(String, String)]) -> Result<()>` — pairs are `(root_id, via_id)`.
  - `roots_of(&self, artifact_ids: &[String]) -> Result<BTreeMap<String, Vec<String>>>` — for a captured artifact the answer is itself; for a merged one, its stored roots. Keyed by the input id.
  - `merged_with_active_roots(&self, limit: i64) -> Result<Vec<String>>` — merged artifact ids at least one of whose roots is still `active`.
  - `children_of_root(&self, root_id: &str) -> Result<Vec<String>>`.

- [ ] **Step 1: Write the failing tests**

Create `src/store/lineage.rs`:

```rust
//! Which captured artifacts a merged artifact is made of.
//!
//! Stored as the resolved closure rather than as parent edges. The re-merge
//! rule in the dedupe pass needs the *captured* roots of every candidate on
//! every decision, and walking edges with a recursive CTE would put a graph
//! traversal on the sweep's hot path. The fan-in cap bounds how much this
//! duplicates.
//!
//! The cost of the denormalisation is that a deleted root removes closure rows
//! an edge table could have recomputed. `merge.rs` flags the child rather than
//! pretending the source was never there.

use super::{Store, now};
use crate::error::Result;
use sqlx::Row;
use std::collections::BTreeMap;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::artifacts::{NewArtifact, Provenance};

    async fn three(s: &Store) -> (String, String, String) {
        let src = s.insert_corpus("x", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..3)
            .map(|i| NewArtifact {
                ordinal: i,
                text: format!("artifact {i}"),
                corpus_span: None,
                title: None,
                category: None,
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        let made = s.insert_artifacts(&src.id, &new).await.unwrap();
        (made[0].id.clone(), made[1].id.clone(), made[2].id.clone())
    }

    #[tokio::test]
    async fn a_captured_artifact_is_its_own_root() {
        // The dedupe pass asks for the roots of every component member without
        // caring which kind it is. Answering "none" for a captured artifact
        // would silently drop it from the prompt it is supposed to be in.
        let s = Store::memory().await.unwrap();
        let (a, b, _) = three(&s).await;
        let roots = s.roots_of(&[a.clone(), b.clone()]).await.unwrap();
        assert_eq!(roots[&a], vec![a.clone()]);
        assert_eq!(roots[&b], vec![b.clone()]);
    }

    #[tokio::test]
    async fn a_merged_artifact_resolves_to_its_captured_roots() {
        let s = Store::memory().await.unwrap();
        let (a, b, _) = three(&s).await;
        let m = s
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    text: "both".into(),
                    title: Some("both".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                },
                &[a.clone(), b.clone()],
            )
            .await
            .unwrap();

        let roots = s.roots_of(std::slice::from_ref(&m.id)).await.unwrap();
        let mut got = roots[&m.id].clone();
        got.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(got, want);
        assert_eq!(m.provenance, Provenance::Merged);
        assert_eq!(m.corpus_id, None);
    }

    #[tokio::test]
    async fn deleting_a_root_takes_its_lineage_rows_with_it() {
        let s = Store::memory().await.unwrap();
        let (a, b, _) = three(&s).await;
        let m = s
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    text: "both".into(),
                    title: None,
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                },
                &[a.clone(), b.clone()],
            )
            .await
            .unwrap();

        s.delete_artifact(&a).await.unwrap();
        let roots = s.roots_of(std::slice::from_ref(&m.id)).await.unwrap();
        assert_eq!(roots[&m.id], vec![b], "the cascade left a dangling root");
    }

    #[tokio::test]
    async fn a_merge_whose_roots_are_still_active_is_findable() {
        // The write path embeds a merged artifact before superseding its
        // roots, so a crash in between leaves exactly this state. Nothing else
        // in the system would ever notice it.
        let s = Store::memory().await.unwrap();
        let (a, b, _) = three(&s).await;
        let m = s
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    text: "both".into(),
                    title: None,
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                },
                &[a.clone(), b.clone()],
            )
            .await
            .unwrap();

        assert_eq!(s.merged_with_active_roots(10).await.unwrap(), vec![m.id.clone()]);

        s.set_superseded_by(&a, Some(&m.id)).await.unwrap();
        s.set_superseded_by(&b, Some(&m.id)).await.unwrap();
        assert!(
            s.merged_with_active_roots(10).await.unwrap().is_empty(),
            "a finished merge is still reported as unfinished"
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib lineage`
Expected: FAIL — `roots_of` and `insert_merged_artifact` do not exist.

- [ ] **Step 3: Add the table**

In `src/store/schema.sql`, after the `artifacts` indexes:

```sql
-- ── Lineage ──────────────────────────────────────────────────────────────────
-- What a merged artifact is made of, as resolved captured roots rather than as
-- parent edges. `root_id` always names a `provenance = 'captured'` artifact, so
-- a re-merge reads the leaves in one query and never rewrites from text that
-- was itself written by a model.
CREATE TABLE IF NOT EXISTS artifact_sources (
  child_id   TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  root_id    TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  -- The direct parent through which root_id entered this child; equal to
  -- root_id for a first-generation merge. Rendering only. SET NULL because a
  -- deleted intermediate does not invalidate the root relationship.
  via_id     TEXT REFERENCES artifacts(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (child_id, root_id)
);
CREATE INDEX IF NOT EXISTS idx_sources_root ON artifact_sources(root_id);
```

- [ ] **Step 4: Implement `NewMerged` and `insert_merged_artifact`**

In `src/store/artifacts.rs`:

```rust
/// A merged artifact being created. Deliberately not `NewArtifact`: there is no
/// corpus, no span, no segment and no ordinal within a document, and a struct
/// that carries those fields as `None` invites a caller to fill them in.
#[derive(Debug, Clone)]
pub struct NewMerged {
    pub text: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub caveats: Vec<String>,
}

impl Store {
    /// Write a merged artifact and its lineage in one transaction.
    ///
    /// One transaction because an artifact with no lineage rows is one whose
    /// detail pane can render nothing and whose roots nobody can recover — and
    /// the re-merge rule reads exactly those rows. Splitting the two writes
    /// makes that state reachable by a crash.
    ///
    /// `roots` may name merged artifacts; they are flattened to their own
    /// captured roots here, so `artifact_sources.root_id` is only ever
    /// captured.
    pub async fn insert_merged_artifact(
        &self,
        new: &NewMerged,
        roots: &[String],
    ) -> Result<Chunk> {
        let resolved = self.roots_of(roots).await?;
        let mut tx = self.pool.begin().await?;
        let created_at = now();
        let c = Chunk {
            id: new_id(),
            corpus_id: None,
            provenance: Provenance::Merged,
            ordinal: 0,
            text: new.text.clone(),
            corpus_span: None,
            title: new.title.clone(),
            category: new.category.clone(),
            tags: new.tags.clone(),
            embed_state: EmbedState::Pending,
            embed_model: None,
            created_at,
            embed_rev: 0,
            segment_idx: None,
            flags: vec![],
            flag_detail: None,
            superseded_by: None,
            caveats: new.caveats.clone(),
            status: ArtifactStatus::Active,
            last_verified_at: Some(created_at),
        };
        sqlx::query(
            "INSERT INTO artifacts (id, corpus_id, provenance, ordinal, text, corpus_span, title, category, tags, embed_state, embed_model, created_at, segment_idx, caveats, status, last_verified_at)
             VALUES (?, NULL, 'merged', 0, ?, NULL, ?, ?, ?, ?, NULL, ?, NULL, ?, ?, ?)",
        )
        .bind(&c.id)
        .bind(&c.text)
        .bind(&c.title)
        .bind(&c.category)
        .bind(serde_json::to_string(&c.tags).unwrap())
        .bind(c.embed_state.as_str())
        .bind(c.created_at)
        .bind(serde_json::to_string(&c.caveats).unwrap_or_else(|_| "[]".into()))
        .bind(c.status.as_str())
        .bind(c.last_verified_at)
        .execute(&mut *tx)
        .await?;

        for (via, roots) in &resolved {
            for root in roots {
                sqlx::query(
                    "INSERT OR IGNORE INTO artifact_sources (child_id, root_id, via_id, created_at)
                     VALUES (?, ?, ?, ?)",
                )
                .bind(&c.id)
                .bind(root)
                .bind(via)
                .bind(created_at)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(c)
    }
}
```

- [ ] **Step 5: Implement the lineage reads**

In `src/store/lineage.rs`, above the tests:

```rust
impl Store {
    /// The captured roots of each of `artifact_ids`, keyed by the input id.
    ///
    /// A captured artifact is its own root. A merged one resolves through
    /// `artifact_sources`, which already holds the closure, so this is one
    /// query and not a traversal.
    pub async fn roots_of(
        &self,
        artifact_ids: &[String],
    ) -> Result<BTreeMap<String, Vec<String>>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for id in artifact_ids {
            let rows = sqlx::query(
                "SELECT root_id FROM artifact_sources WHERE child_id = ? ORDER BY root_id",
            )
            .bind(id)
            .fetch_all(&self.pool)
            .await?;
            let roots: Vec<String> = rows.iter().map(|r| r.get("root_id")).collect();
            // No lineage rows means this is a captured artifact — or a merged
            // one every root of which has since been deleted, which
            // `merge::flag_orphans` is what notices.
            out.insert(id.clone(), if roots.is_empty() { vec![id.clone()] } else { roots });
        }
        Ok(out)
    }

    /// Merged artifacts at least one of whose roots is still active.
    ///
    /// The write path embeds a merged artifact before superseding its roots, so
    /// this is the state a crash between those two steps leaves. It is
    /// invisible to everything else: the merge looks complete from the artifact
    /// side and absent from the pair side.
    pub async fn merged_with_active_roots(&self, limit: i64) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT DISTINCT s.child_id FROM artifact_sources s
               JOIN artifacts child ON child.id = s.child_id
               JOIN artifacts root  ON root.id  = s.root_id
              WHERE child.provenance = 'merged'
                AND child.status = 'active'
                AND root.status = 'active'
                AND root.superseded_by IS NULL
              LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get("child_id")).collect())
    }

    pub async fn children_of_root(&self, root_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT child_id FROM artifact_sources WHERE root_id = ?")
            .bind(root_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get("child_id")).collect())
    }
}
```

Add `pub mod lineage;` to `src/store/mod.rs`.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --lib lineage && cargo test`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/store/schema.sql src/store/lineage.rs src/store/mod.rs src/store/artifacts.rs
git commit -m "feat(store): lineage for merged artifacts

artifact_sources holds resolved captured roots rather than parent edges.
The re-merge rule needs the leaves on every decision, and a recursive CTE
would put a graph walk on the sweep's hot path; the fan-in cap bounds the
duplication that buys.

insert_merged_artifact writes the artifact and its lineage in one
transaction: a merged artifact with no roots is one whose detail pane can
render nothing and whose sources nobody can recover.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## Phase 2 — Detection and lifecycle repair

### Task 3: Two new pair states

**Files:**
- Modify: `src/store/pairs.rs:27-64` (`PairState`), `:259` (`pairs_to_judge`)
- Test: `src/store/pairs.rs` (in-module)

**Interfaces:**
- Produces: `PairState::NearIdentical`, `PairState::Oversized`; `Store::pairs_by_state` and `count_pairs_by_state` accept both.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn a_near_identical_pair_is_never_offered_to_the_model() {
    // The >= auto_supersede band is settled for free by clustering. A pair
    // filed there that reached the dedupe queue would spend a model call on
    // exactly the case where the cheap rule is already correct — the free path
    // quietly becoming a paid one, which is the expensive regression.
    let s = Store::memory().await.unwrap();
    let (a, b) = two_artifacts(&s).await;
    s.record_settled_pair(&a, &b, 0.99, PairState::NearIdentical)
        .await
        .unwrap();

    assert!(s.pairs_to_judge(10).await.unwrap().is_empty());
    assert_eq!(
        s.pairs_by_state(PairState::NearIdentical, 10).await.unwrap().len(),
        1
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test a_near_identical_pair_is_never_offered_to_the_model`
Expected: FAIL — no variant `NearIdentical`.

- [ ] **Step 3: Add the variants**

```rust
pub enum PairState {
    Pending,
    NoConflict,
    Contradiction,
    Superseded,
    Dismissed,
    /// Scored at or above `auto_supersede`. Settled by the sweep's free
    /// clustering pass, and never armed for a model call: that band is
    /// answered correctly by a rule that costs nothing.
    NearIdentical,
    /// The component this pair belongs to has more captured roots than
    /// `merge_max_roots`. Not merged — a merge of forty roots is no longer one
    /// atomic piece of knowledge, which is what an artifact is defined to be.
    Oversized,
}
```

Extend `as_str` with `"near_identical"` and `"oversized"`, and `parse` with the two matching arms. `pairs_to_judge` already filters `state = 'pending'`, so no change is needed there — but add the assertion above to prove it.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib pairs`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/store/pairs.rs
git commit -m "feat(store): NearIdentical and Oversized pair states

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: One place that classifies a scored pair

**Files:**
- Create: `src/jobs/classify.rs`
- Modify: `src/jobs/consolidate.rs:317-419` (delete the inline body, call the new function), `src/jobs/mod.rs`
- Test: `src/jobs/classify.rs` (in-module)

**Interfaces:**
- Consumes: `PairState` (Task 3), `Chunk`, `Provenance` (Task 1).
- Produces: `pub async fn classify_pair(core: &Core, a: &Chunk, b: &Chunk, score: f32) -> Result<Verdict>` where

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing was written: one side is not active, or the two are the same
    /// artifact.
    Skipped,
    /// Filed at or above `auto_supersede` for the sweep's clustering pass.
    NearIdentical,
    /// One text is wholly inside the other and both came from one corpus.
    /// Superseded here, with no call and no queue slot.
    Contained,
    /// Filed as `Pending` — a dedupe unit will decide.
    Queued,
    /// Already on the queue, or already answered. Nothing changed.
    Unchanged,
}
```

Also `pub fn contains_normalized(long: &str, short: &str) -> bool`, moved here from `consolidate.rs:77`.

**Why this task exists.** Detection gains a second producer in Task 5. Two discovery paths with two copies of these rules is the kind of divergence you only notice when the outcome starts depending on which path saw a pair first.

**Behaviour change from today.** The `may_disagree` gate is *removed* from this function. Under the old contract a pair with no differing values was filed `NoConflict` and both artifacts stayed active; under the new one it is the cleanest merge candidate and must reach the model. See spec §6.5.

- [ ] **Step 1: Write the failing tests**

Create `src/jobs/classify.rs` with the module doc and this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::pairs::PairState;

    // Reuses the `seed` helper from consolidate.rs's tests, made
    // `pub(crate)` in this task.
    use crate::jobs::consolidate::tests::seed;

    #[tokio::test]
    async fn a_pair_with_no_differing_values_is_queued_not_closed() {
        // The polarity change. `may_disagree` admits a pair only when both
        // sides state values AND those values differ, which is backwards for
        // deduplication: the pairs it discarded are the cleanest merge
        // candidates. Two artifacts at 0.93 saying the same thing used to be
        // filed "nothing to decide" and both stayed in every result set.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;
        let a = core.store.get_artifact(&ids[0]).await.unwrap();
        let b = core.store.get_artifact(&ids[1]).await.unwrap();

        assert_eq!(classify_pair(&core, &a, &b, 0.93).await.unwrap(), Verdict::Queued);
        assert_eq!(
            core.store.pairs_by_state(PairState::Pending, 10).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn a_pair_at_auto_supersede_is_filed_for_the_cluster_pass() {
        // It must not reach the dedupe queue: that band is answered by a rule
        // that costs nothing, and a model call there is pure waste. It must
        // also not be superseded here — pairwise resolution is what `Clusters`
        // exists to avoid, because A loses to B and B then loses to C.
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        let a = core.store.get_artifact(&ids[0]).await.unwrap();
        let b = core.store.get_artifact(&ids[1]).await.unwrap();

        assert_eq!(
            classify_pair(&core, &a, &b, 0.999).await.unwrap(),
            Verdict::NearIdentical
        );
        assert!(a.superseded_by.is_none() && b.superseded_by.is_none());
        assert!(core.store.pairs_to_judge(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn one_synthesis_call_emitting_a_passage_twice_resolves_itself() {
        // Same corpus, one text wholly inside the other: a defect in one
        // artifact rather than two sources saying different things. Nothing is
        // lost by hiding it, and it costs no call.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Bind mounts attach a directory elsewhere. Use mount --bind.", [1.0, 0.0]),
                ("Bind mounts attach a directory elsewhere.", [0.93, 0.37]),
            ],
        )
        .await;
        let a = core.store.get_artifact(&ids[0]).await.unwrap();
        let b = core.store.get_artifact(&ids[1]).await.unwrap();

        assert_eq!(classify_pair(&core, &a, &b, 0.93).await.unwrap(), Verdict::Contained);
        assert_eq!(
            core.store.get_artifact(&ids[1]).await.unwrap().superseded_by.as_deref(),
            Some(ids[0].as_str())
        );
    }

    #[tokio::test]
    async fn a_pair_naming_a_hidden_artifact_is_skipped() {
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        core.deprecate(&ids[0]).await.unwrap();
        let a = core.store.get_artifact(&ids[0]).await.unwrap();
        let b = core.store.get_artifact(&ids[1]).await.unwrap();

        assert_eq!(classify_pair(&core, &a, &b, 0.93).await.unwrap(), Verdict::Skipped);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib classify`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Implement**

```rust
//! Turning one scored pair into one decision.
//!
//! Two things discover near pairs now — the per-artifact `Relate` unit and the
//! sweep's backlog scan — and both must reach the same conclusion about the
//! same pair. Keeping these rules in the sweep's body meant the second producer
//! would have arrived with a copy, and a copy diverges silently: you notice
//! when the outcome starts depending on which path saw a pair first.
//!
//! Nothing here calls a model. Every rule is local, and the two that settle a
//! pair outright — containment, and the `auto_supersede` band — are the reason
//! most near pairs never cost anything at all.

use crate::core::Core;
use crate::error::Result;
use crate::store::artifacts::{ArtifactStatus, Chunk};
use crate::store::pairs::PairState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict { Skipped, NearIdentical, Contained, Queued, Unchanged }

/// Is the whole of one artifact inside the other, whitespace aside?
///
/// Not a similarity — containment. A score says two texts are alike; this says
/// one of them adds nothing.
pub fn contains_normalized(long: &str, short: &str) -> bool {
    let n = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    !short.trim().is_empty() && n(long).contains(&n(short))
}

pub async fn classify_pair(core: &Core, a: &Chunk, b: &Chunk, score: f32) -> Result<Verdict> {
    if a.id == b.id {
        return Ok(Verdict::Skipped);
    }
    // Only two live artifacts have a question worth a queue slot, a call, or a
    // supersede. A retired artifact must not win, and a resolved pair has
    // nothing left to decide.
    if [a, b].iter().any(|c| c.status != ArtifactStatus::Active || c.superseded_by.is_some()) {
        return Ok(Verdict::Skipped);
    }

    // The free band. Filed rather than acted on: resolving pairwise leaves A
    // pointing at a B that is itself hidden, which is what the sweep's
    // union-find exists to prevent. The cluster pass reads these rows.
    if score >= core.consolidate.auto_supersede {
        let changed = core
            .store
            .record_settled_pair(&a.id, &b.id, score, PairState::NearIdentical)
            .await?;
        return Ok(if changed { Verdict::NearIdentical } else { Verdict::Unchanged });
    }

    // One synthesis call emitting the same passage twice. Same corpus is the
    // whole of the condition: two documents that share a sentence are two
    // sources, and hiding one of those on a 0.9 similarity is exactly what
    // `auto_supersede` refuses to do.
    if a.corpus_id.is_some() && a.corpus_id == b.corpus_id {
        let (long, short) = if a.text.len() >= b.text.len() { (a, b) } else { (b, a) };
        if contains_normalized(&long.text, &short.text) {
            if let Err(e) = core.supersede(&short.id, &long.id).await {
                tracing::warn!(superseded = %short.id, by = %long.id, error = %e,
                    "could not hide a duplicated passage; it stays active");
                return Ok(Verdict::Skipped);
            }
            tracing::info!(superseded = %short.id, by = %long.id,
                "hid a passage one synthesis call emitted twice");
            return Ok(Verdict::Contained);
        }
    }

    // Everything else is a question for the dedupe pass. `may_disagree` used to
    // gate this, filing a pair with no differing values as `NoConflict` — which
    // is backwards for deduplication: those are the cleanest merge candidates.
    // It survives as a prior in the prompt and as the input to the merge
    // verification, not as an admission gate. See the spec, §6.5.
    let changed = core.store.record_pair(&a.id, &b.id, score).await?;
    Ok(if changed { Verdict::Queued } else { Verdict::Unchanged })
}
```

- [ ] **Step 4: Rewrite the sweep's review band to call it**

In `src/jobs/consolidate.rs`, replace lines 317–419 with:

```rust
    for p in pairs.iter().filter(|p| p.score < cfg.auto_supersede) {
        if hidden.contains(&p.a) || hidden.contains(&p.b) {
            continue;
        }
        let (Ok(a), Ok(b)) = (
            core.store.get_artifact(&p.a).await,
            core.store.get_artifact(&p.b).await,
        ) else {
            continue;
        };
        // Warn and carry on: a pair is one row about two artifacts, and a
        // transient BUSY on it is no reason to abandon the rest of the band.
        // The sweep re-finds the pair next run.
        match crate::jobs::classify::classify_pair(core, &a, &b, p.score).await {
            Ok(crate::jobs::classify::Verdict::Queued) => out.queued += 1,
            Ok(crate::jobs::classify::Verdict::Contained) => out.superseded += 1,
            Ok(_) => {}
            Err(e) => tracing::warn!(a = %p.a, b = %p.b, error = %e,
                "could not classify a pair; it will be re-examined next sweep"),
        }
    }
```

Delete `contains_normalized` from `consolidate.rs` (it now lives in `classify.rs`). Make the test helpers `seed` and `seed_into_new_corpus` `pub(crate)` and the test module `pub(crate) mod tests` so `classify.rs` can use them. Add `pub mod classify;` to `src/jobs/mod.rs`.

- [ ] **Step 5: Rewrite the two tests the polarity change invalidates**

In `src/jobs/consolidate.rs`, replace `a_pair_with_nothing_to_disagree_about_never_reaches_the_queue` (`:1432`) and `a_pair_with_no_facts_to_disagree_about_never_reaches_the_model` (`:1285`) with:

```rust
    #[tokio::test]
    async fn a_pair_with_no_differing_values_reaches_the_queue() {
        // This used to assert the opposite. `may_disagree` admits a pair only
        // when both sides state values and those values differ — right for
        // "do these contradict?", backwards for "are these duplicates?". Two
        // artifacts at 0.93 saying the same thing in different words are the
        // best merge candidate there is, and the old rule filed them as
        // settled and left both in every result set.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;

        let out = run(&core).await.unwrap();
        assert_eq!(out.queued, 1, "{out:?}");
        // Queued is not hidden: nothing about either artifact changes until a
        // dedupe unit has ruled.
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().superseded_by.is_none());
        }
    }
```

Remove the `closed` field from `Outcome` and every reference to it; no path files `NoConflict` from the sweep any more.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/jobs/classify.rs src/jobs/consolidate.rs src/jobs/mod.rs
git commit -m "refactor(jobs): one place that classifies a scored pair

Detection is about to gain a second producer, and two copies of these
rules would diverge silently. Also inverts the prefilter: may_disagree
gated the queue on values *differing*, which discards exactly the pairs
that are cleanest to merge. It survives as a prompt prior and as the
merge verification, not as an admission gate.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: Per-artifact neighbour discovery

**Files:**
- Create: `src/jobs/relate.rs`
- Modify: `src/store/jobs.rs:13-53` (`Stage::Relate`), `src/jobs/mod.rs:40-52` (dispatch), `src/jobs/embed.rs:277-290` (`mark_indexed`)
- Test: `src/jobs/relate.rs` (in-module)

**Interfaces:**
- Consumes: `classify_pair` (Task 4), `VectorStore::neighbours(artifact_id, limit) -> Result<Vec<SearchHit>>`.
- Produces: `Stage::Relate` (`as_str` → `"relate"`); `pub async fn run(core: &Core, artifact_id: &str) -> Result<()>`; `pub async fn arm(core: &Core, artifact_id: &str, seq: i64) -> Result<()>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::jobs::consolidate::tests::seed;
    use crate::store::pairs::PairState;

    #[tokio::test]
    async fn an_artifact_finds_its_duplicate_the_moment_it_is_embedded() {
        // The sweep samples 2000 points and needs both members of a pair in the
        // same draw, so coverage decays as (sample/N)^2 — at 100k artifacts a
        // given pair waits years. Asking one artifact for its own neighbours
        // costs one Qdrant query, no embedding call, and is exact.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;

        run(&core, &ids[1]).await.unwrap();

        let pending = core.store.pairs_by_state(PairState::Pending, 10).await.unwrap();
        assert_eq!(pending.len(), 1, "the unit found no duplicate");
        assert!(
            [&pending[0].a_id, &pending[0].b_id].contains(&&ids[0])
                && [&pending[0].a_id, &pending[0].b_id].contains(&&ids[1])
        );
    }

    #[tokio::test]
    async fn a_near_identical_neighbour_never_costs_a_model_call() {
        // >= auto_supersede is settled for free by clustering. Filing it as an
        // ordinary pending pair would arm a dedupe unit and spend a call on the
        // one case where the cheap rule is already right.
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;

        run(&core, &ids[1]).await.unwrap();

        assert!(core.store.pairs_to_judge(10).await.unwrap().is_empty());
        assert_eq!(
            core.store.pairs_by_state(PairState::NearIdentical, 10).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn an_unrelated_neighbour_is_left_entirely_alone() {
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        run(&core, &ids[1]).await.unwrap();
        assert!(core.store.pairs_by_state(PairState::Pending, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_artifact_that_is_no_longer_active_asks_for_nothing() {
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;
        core.deprecate(&ids[1]).await.unwrap();

        run(&core, &ids[1]).await.unwrap();

        assert!(core.store.pairs_by_state(PairState::Pending, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_pair_is_found_by_whichever_member_is_embedded_second() {
        // The completeness argument. When X's unit runs, Y is either already
        // indexed — X finds it — or not, and Y finds X later. There is no
        // window in which a pair falls through.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;

        run(&core, &ids[0]).await.unwrap();
        let from_a = core.store.pairs_by_state(PairState::Pending, 10).await.unwrap().len();
        run(&core, &ids[1]).await.unwrap();
        let after_b = core.store.pairs_by_state(PairState::Pending, 10).await.unwrap().len();

        assert_eq!(from_a, 1);
        assert_eq!(after_b, 1, "the second member filed the same pair twice");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib relate`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Add the stage**

In `src/store/jobs.rs`, add to `Stage`:

```rust
    /// One artifact, one neighbour query. No inference: `neighbours` looks the
    /// vector up by point id, so this costs a round trip and nothing else.
    /// It is what makes duplicate detection complete rather than sampled.
    Relate,
```

with `"relate"` in `as_str` and `parse`.

- [ ] **Step 4: Implement the unit**

Create `src/jobs/relate.rs`:

```rust
//! What else is this artifact already saying?
//!
//! Duplicate detection used to be a sampled sweep: 2000 points drawn per run,
//! pairs computed only within the draw, so both members of a pair had to land
//! in the same sample. The probability of that is (sample/N)^2 and it decays
//! quadratically — at a hundred thousand artifacts a given duplicate pair waits
//! years for its turn.
//!
//! This asks the opposite question. One artifact, its own neighbours, one
//! query. `neighbours` addresses the point by id, so Qdrant looks the vector up
//! itself and no embedding call is paid, and it already excludes superseded and
//! deprecated points. Coverage becomes 1, independent of N.
//!
//! A separate unit rather than a tail on `embed_batch`: a failing Qdrant query
//! would otherwise fail the embed job, whose retry pays for the embedding
//! again. Two failure domains, two units.

use crate::core::Core;
use crate::error::Result;
use crate::store::artifacts::ArtifactStatus;
use crate::store::jobs::Stage;

pub async fn arm(core: &Core, artifact_id: &str, seq: i64) -> Result<()> {
    core.store
        .rearm_idle_seq(Stage::Relate, "artifact", artifact_id, seq)
        .await
}

pub async fn run(core: &Core, artifact_id: &str) -> Result<()> {
    let me = core.store.get_artifact(artifact_id).await?;
    // A retired artifact has no duplicates worth recording: every pair naming
    // it would be skipped by `classify_pair` anyway, and the query is wasted.
    if me.status != ArtifactStatus::Active || me.superseded_by.is_some() {
        return Ok(());
    }

    let hits = core
        .vectors
        .neighbours(artifact_id, core.consolidate.per_point)
        .await?;

    for h in hits {
        if h.score < core.consolidate.review_min {
            continue;
        }
        // Ordinary: the vector store can list a point SQLite has dropped, a
        // delete lags. Not an error.
        let Ok(other) = core.store.get_artifact(&h.payload.artifact_id).await else {
            tracing::debug!(artifact_id = %h.payload.artifact_id, "neighbour is gone");
            continue;
        };
        // Warn and carry on, as the sweep does: one unwritable pair row is no
        // reason to drop the other neighbours.
        if let Err(e) = crate::jobs::classify::classify_pair(core, &me, &other, h.score).await {
            tracing::warn!(a = %me.id, b = %other.id, error = %e, "could not classify a neighbour");
        }
    }
    Ok(())
}
```

`SearchHit`'s similarity field is `score`; confirm against `src/vector/mod.rs` and use the actual field name.

- [ ] **Step 5: Wire the dispatch and the arming**

In `src/jobs/mod.rs`, add `pub mod relate;` and a dispatch arm:

```rust
        (Stage::Relate, _) => relate::run(core, &job.target_id).await,
```

In `src/jobs/embed.rs`, in `mark_indexed`, after a successful mark:

```rust
    if !landed {
        tracing::info!(
            artifact_id = %chunk.id,
            "chunk was edited while it was being embedded; leaving it pending"
        );
        return Ok(());
    }
    // Only now: the vector is in the index, so the neighbour query has
    // something to find. Armed rather than run inline — a failing Qdrant query
    // must not fail the embed job, whose retry would pay for the embedding
    // again.
    crate::jobs::relate::arm(core, &chunk.id, 0).await?;
    Ok(())
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/jobs/relate.rs src/jobs/mod.rs src/jobs/embed.rs src/store/jobs.rs
git commit -m "feat(jobs): find duplicates when an artifact is embedded

near_pairs samples 2000 points and needs both members of a pair in the
same draw, so coverage decays as (sample/N)^2 -- at 100k artifacts a
given pair waits years. neighbours() asks one artifact for its own
neighbours by point id: no embedding call, lifecycle already filtered,
coverage 1 regardless of N. The sweep keeps the backlog and the backstop.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: The cluster pass reads stored pairs too

**Files:**
- Modify: `src/jobs/consolidate.rs:247-254` (cluster construction)
- Test: `src/jobs/consolidate.rs` (in-module)

**Interfaces:**
- Consumes: `PairState::NearIdentical` (Task 3), `Store::pairs_by_state`.
- Produces: no new API. The sweep's union-find input becomes `near_pairs(...) ∪ pairs_by_state(NearIdentical, ...)`.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn the_cluster_pass_settles_a_pair_the_relate_unit_filed() {
        // The relate unit files >= auto_supersede pairs rather than acting on
        // them, because resolving pairwise leaves A pointing at a B that is
        // itself hidden. Nothing would ever settle them if the cluster pass
        // read only what one sampled round trip happened to return.
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        core.store
            .record_settled_pair(&ids[0], &ids[1], 0.999, PairState::NearIdentical)
            .await
            .unwrap();
        // Empty the vector store's view so `near_pairs` cannot supply the pair
        // and only the stored row can.
        core.vectors.delete_artifacts(&ids).await.unwrap();

        let out = run(&core).await.unwrap();

        assert_eq!(out.superseded, 1, "{out:?}");
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().superseded_by.as_deref(),
            Some(ids[1].as_str())
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test the_cluster_pass_settles_a_pair_the_relate_unit_filed`
Expected: FAIL — `out.superseded` is 0.

- [ ] **Step 3: Implement**

In `src/jobs/consolidate.rs`, replace the cluster construction:

```rust
    // Group everything near-identical first, and only then decide who wins.
    //
    // Two inputs. `near_pairs` is one sampled round trip, which is all this had
    // when the sweep was the only producer; the stored `NearIdentical` rows are
    // what the per-artifact relate units have filed since, and they are exact
    // rather than sampled. Reading only the first would leave every pair a
    // relate unit found above `auto_supersede` unsettled forever, because
    // nothing else acts on that band.
    let mut clusters = Clusters::default();
    let mut in_a_cluster: HashSet<String> = HashSet::new();
    let filed = core
        .store
        .pairs_by_state(crate::store::pairs::PairState::NearIdentical, 500)
        .await?;
    let from_sweep = pairs
        .iter()
        .filter(|p| p.score >= cfg.auto_supersede)
        .map(|p| (p.a.clone(), p.b.clone()));
    let from_store = filed.iter().map(|p| (p.a_id.clone(), p.b_id.clone()));
    for (a, b) in from_sweep.chain(from_store) {
        clusters.union(&a, &b);
        in_a_cluster.insert(a);
        in_a_cluster.insert(b);
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/consolidate.rs
git commit -m "feat(consolidate): cluster from stored pairs as well as the sample

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 7: Lifecycle repair reads a marker instead of scanning

**Files:**
- Modify: `src/store/artifacts.rs:541-575` (`set_superseded_by`, `set_artifact_status`), add `dirty_lifecycle_artifacts`, `clear_lifecycle_dirty`
- Modify: `src/core/ingest.rs:274-366` (`supersede`, `unsupersede`, `deprecate`, `reactivate`)
- Modify: `src/jobs/consolidate.rs:121-190` (`repair_lifecycle_drift`)
- Test: `src/jobs/consolidate.rs` (in-module)

**Interfaces:**
- Produces: `Store::dirty_lifecycle_artifacts(&self, limit: usize) -> Result<Vec<Chunk>>`, `Store::clear_lifecycle_dirty(&self, ids: &[String]) -> Result<()>`.

**Why.** `DRIFT_SCAN = 5000` caps both scans. Autonomous merging makes hidden artifacts grow monotonically — every merge hides at least two, forever — so the cap is permanently reached and the repair degrades into sampling an ever-growing set.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn a_lifecycle_write_that_lost_its_payload_is_repaired_from_the_marker() {
        // The scan-based repair is capped at DRIFT_SCAN from both sides, and
        // merging makes hidden artifacts grow without bound, so past the cap it
        // repairs a random window of an ever-growing set. The marker is set in
        // the same SQLite write that hides an artifact and cleared once the
        // payload write lands, so the repair costs the open writes and nothing
        // else.
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;

        // Row written, payload not — exactly what a crash between the two
        // leaves behind.
        core.store.set_artifact_status(&ids[0], ArtifactStatus::Deprecated).await.unwrap();

        assert_eq!(
            core.store.dirty_lifecycle_artifacts(10).await.unwrap().len(),
            1,
            "the write left no marker"
        );
        assert_eq!(repair_lifecycle_drift(&core).await.unwrap(), 1);
        assert!(
            core.store.dirty_lifecycle_artifacts(10).await.unwrap().is_empty(),
            "the marker outlived the repair, so every sweep will redo it"
        );

        let hits = core
            .vectors
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert!(!hits.iter().any(|h| h.payload.artifact_id == ids[0]));
    }

    #[tokio::test]
    async fn a_completed_lifecycle_change_leaves_no_marker() {
        // If `Core::supersede` did not clear it, the repair would rewrite every
        // payload it ever touched, on every sweep, forever.
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        core.supersede(&ids[0], &ids[1]).await.unwrap();

        assert!(core.store.dirty_lifecycle_artifacts(10).await.unwrap().is_empty());
        assert_eq!(
            repair_lifecycle_drift(&core).await.unwrap(),
            0,
            "the repair fired on a base that agrees with itself"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test a_lifecycle_write_that_lost_its_payload_is_repaired_from_the_marker`
Expected: FAIL — `dirty_lifecycle_artifacts` does not exist.

- [ ] **Step 3: Set the marker where the row changes**

In `src/store/artifacts.rs`, add `lifecycle_dirty = 1` to both writes:

```rust
    pub async fn set_superseded_by(&self, artifact_id: &str, by: Option<&str>) -> Result<()> {
        // The marker rides the same statement as the change it describes: two
        // statements could be interrupted between, which is the exact failure
        // this is meant to catch.
        sqlx::query(
            "UPDATE artifacts SET superseded_by = ?, status = ?, lifecycle_dirty = 1 WHERE id = ?",
        )
        // ... existing binds
    }

    pub async fn set_artifact_status(&self, id: &str, status: ArtifactStatus) -> Result<()> {
        sqlx::query("UPDATE artifacts SET status = ?, lifecycle_dirty = 1 WHERE id = ?")
        // ... existing binds
    }
```

Add the reads:

```rust
    /// Artifacts whose lifecycle row has changed since the payload was last
    /// written. The repair's whole work list.
    pub async fn dirty_lifecycle_artifacts(&self, limit: usize) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT * FROM artifacts WHERE lifecycle_dirty = 1 ORDER BY id LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_artifact).collect())
    }

    /// Clear the marker once the payload write is acknowledged. Never before:
    /// clearing first turns a failed payload write into permanent drift that
    /// nothing is left to notice.
    pub async fn clear_lifecycle_dirty(&self, ids: &[String]) -> Result<()> {
        for id in ids {
            sqlx::query("UPDATE artifacts SET lifecycle_dirty = 0 WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }
```

- [ ] **Step 4: Clear the marker where the payload write succeeds**

In `src/core/ingest.rs`, at the end of each of `supersede`, `unsupersede`, `deprecate` and `reactivate` — after the `self.vectors.set_lifecycle(...)` call returns `Ok` — add:

```rust
        self.store.clear_lifecycle_dirty(std::slice::from_ref(&id.to_string())).await?;
```

using whichever id that function just wrote. Where a function writes two artifacts, clear both.

- [ ] **Step 5: Rewrite the repair**

In `src/jobs/consolidate.rs`, replace the body of `repair_lifecycle_drift`:

```rust
/// Make the vector store's lifecycle payloads agree with SQLite, which is the
/// source of truth for all of them.
///
/// Reads `lifecycle_dirty` rather than scanning. The scan version capped both
/// sides at `DRIFT_SCAN` and, because autonomous merging makes hidden artifacts
/// grow monotonically, was permanently past that cap — repairing a random
/// window of an ever-growing set while reporting success. It also had to
/// reconcile two differently-ordered truncated lists, which is what made
/// "absent from the other list" read as drift.
///
/// Returns how many artifacts it rewrote, which is worth asserting on: a repair
/// that fires on a base in agreement is a bug hiding behind a correct end state.
async fn repair_lifecycle_drift(core: &Core) -> Result<usize> {
    let dirty = core.store.dirty_lifecycle_artifacts(DRIFT_SCAN).await?;
    if dirty.is_empty() {
        return Ok(0);
    }
    let rows: Vec<crate::vector::LifecycleRow> = dirty.iter().map(lifecycle_row_of).collect();
    core.vectors.apply_lifecycle(&rows).await?;
    let ids: Vec<String> = dirty.iter().map(|c| c.id.clone()).collect();
    core.store.clear_lifecycle_dirty(&ids).await?;
    tracing::info!(repaired = rows.len(), "reconciled lifecycle state with the vector store");
    Ok(rows.len())
}
```

Keep `repair_lifecycle_drift_scanning` under a new name, `full_lifecycle_reconcile`, and call it from `Core::backfill_lifecycle` only — it still catches drift that arose with no SQLite write behind it, but it no longer runs every sweep. Update `the_drift_repair_rewrites_nothing_when_the_two_stores_agree` and `a_scan_cap_reached_from_both_sides_is_not_drift` to call `full_lifecycle_reconcile`.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/store/artifacts.rs src/core/ingest.rs src/jobs/consolidate.rs
git commit -m "fix(consolidate): repair lifecycle drift from a marker, not a scan

Both scans were capped at DRIFT_SCAN and autonomous merging makes hidden
artifacts grow without bound, so the repair was about to become a random
sample of an ever-growing set that reported success either way. The
marker rides the same UPDATE as the change it describes and is cleared
only once the payload write is acknowledged.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## Phase 3 — The dedupe contract, recording only

### Task 8: Rename judge to dedupe

**Files:**
- Rename: `src/jobs/judge.rs` → `src/jobs/dedupe.rs`
- Modify: `src/store/jobs.rs` (`Stage::Judge` → `Stage::Dedupe`), `src/jobs/mod.rs`, `src/jobs/consolidate.rs`, `src/config.rs`, `src/web/api.rs`, `src/web/ui.rs`, `config.example.toml`, `README.md`

**Interfaces:**
- Produces: `Stage::Dedupe` (`as_str` → `"dedupe"`), `consolidate.autonomous: bool`, `crate::jobs::dedupe::run`.

**Why.** `/ui/judge` and `src/web/judge.rs` are the relevance-feedback evaluation surface. Two unrelated things called "judge", one of which is being extended.

- [ ] **Step 1: Rename and rewire**

```bash
git mv src/jobs/judge.rs src/jobs/dedupe.rs
```

Then, mechanically: `Stage::Judge` → `Stage::Dedupe`; `"judge"` → `"dedupe"` in `as_str`/`parse`; `judge::run` → `dedupe::run`; `pub mod judge;` → `pub mod dedupe;` in `src/jobs/mod.rs`; `arm_judgements` → `arm_dedupe`; `ConsolidateConfig.judge` → `.autonomous`; `JUDGE_SYSTEM` → `DEDUPE_SYSTEM`; `judge_prompt` → `dedupe_prompt`. Leave `src/web/judge.rs` and `/ui/judge` untouched — they are the other thing.

Keep `pairs.judge_attempts`, `judge_unreadable` and `MAX_UNREADABLE_JUDGEMENTS` as they are: renaming a column means recreating the database, and the names are unambiguous in context.

- [ ] **Step 2: Run tests to verify nothing broke**

Run: `cargo test`
Expected: PASS — this task changes no behaviour.

- [ ] **Step 3: Commit**

```bash
git add -A src/ config.example.toml README.md
git commit -m "refactor: rename the consolidation judge to dedupe

/ui/judge is the relevance-feedback surface. Two unrelated things called
judge, one of them about to grow, is one too many.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 9: The four-verdict prompt and parser

**Files:**
- Modify: `src/infer/prompt.rs:124-238`
- Test: `src/infer/prompt.rs` (in-module)

**Interfaces:**
- Consumes: nothing.
- Produces:

```rust
pub enum Relation { Distinct, Conflict, Replaced, Duplicate }

pub struct MergedDraft {
    pub title: Option<String>,
    pub text: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub caveats: Vec<String>,
}

pub struct Dedupe {
    pub relation: Relation,
    pub detail: Option<String>,
    /// 'a' or 'b', only meaningful when `relation` is `Replaced`.
    pub supersedes: Option<char>,
    /// `Some` if and only if `relation` is `Duplicate`.
    pub merged: Option<MergedDraft>,
}

pub const DEDUPE_SYSTEM: &str;
pub fn dedupe_prompt(members: &[(&str, &str)], differing_values: &[String], attempt: i64) -> String;
pub fn parse_dedupe(body: &str) -> Result<Dedupe>;
```

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn a_duplicate_verdict_carries_a_merged_draft() {
        let d = parse_dedupe(
            r#"{"relation":"duplicate","detail":"same command, different detail",
                "merged":{"title":"Bind mounts","text":"Use mount --bind.","tags":["mount"],
                          "caveats":[],"category":"howto"}}"#,
        )
        .unwrap();
        assert_eq!(d.relation, Relation::Duplicate);
        assert_eq!(d.merged.as_ref().unwrap().text, "Use mount --bind.");
        assert_eq!(d.merged.as_ref().unwrap().title.as_deref(), Some("Bind mounts"));
    }

    #[test]
    fn a_merged_block_on_a_non_duplicate_verdict_is_unreadable() {
        // `merged` belongs to `duplicate` and to nothing else. Accepting it
        // elsewhere would let a reply that classified a pair as a conflict
        // still hand us text to write, which is the one outcome the conflict
        // verdict exists to prevent.
        for relation in ["conflict", "replaced", "distinct"] {
            let body = format!(
                r#"{{"relation":"{relation}","supersedes":"a",
                     "merged":{{"text":"x","tags":[],"caveats":[]}}}}"#
            );
            assert!(
                matches!(parse_dedupe(&body), Err(crate::error::Error::MalformedLlmOutput(_))),
                "a {relation} verdict was allowed to carry a merge"
            );
        }
    }

    #[test]
    fn a_duplicate_verdict_without_a_merged_block_is_unreadable() {
        assert!(matches!(
            parse_dedupe(r#"{"relation":"duplicate","detail":"x"}"#),
            Err(crate::error::Error::MalformedLlmOutput(_))
        ));
    }

    #[test]
    fn a_replaced_verdict_names_a_side() {
        let d = parse_dedupe(r#"{"relation":"replaced","supersedes":"B","detail":"old flag"}"#).unwrap();
        assert_eq!(d.relation, Relation::Replaced);
        assert_eq!(d.supersedes, Some('b'));
    }

    #[test]
    fn a_replaced_verdict_naming_no_side_is_a_conflict() {
        // A direction the model would not name is not a direction. Treating it
        // as one would pick a side by accident.
        let d = parse_dedupe(r#"{"relation":"replaced","detail":"one of them is old"}"#).unwrap();
        assert_eq!(d.relation, Relation::Conflict);
    }

    #[test]
    fn the_dedupe_prompt_keeps_the_subject_rule() {
        // FAT12, FAT16 and FAT32 are near-identical in form and deliberately
        // different in content: they sit at 0.91 and every number in them
        // differs. Without the subject-first rule the autonomous path merges a
        // reference document into mush.
        assert!(DEDUPE_SYSTEM.contains("same subject"));
        assert!(DEDUPE_SYSTEM.contains("different things"));
    }

    #[test]
    fn the_prompt_varies_with_the_attempt() {
        // The endpoint replays identical output for identical prompts, so an
        // unchanged retry returns the same unreadable bytes every time.
        let a = dedupe_prompt(&[("t", "one"), ("t", "two")], &[], 0);
        let b = dedupe_prompt(&[("t", "one"), ("t", "two")], &[], 1);
        assert_ne!(a, b);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib prompt`
Expected: FAIL — `parse_dedupe` does not exist.

- [ ] **Step 3: Write the system prompt**

```rust
pub const DEDUPE_SYSTEM: &str = r#"You compare two or more knowledge artifacts about possibly the same thing, and decide what should happen to them.

First decide whether they are about the same subject. Their titles say what each one is about, and the body may never repeat it — an artifact titled "FAT32 Specifications" can open with "32 Bit Clusternummern" and never name FAT32 again.

If the titles name different things — two versions, two variants, two products, two filesystems, two commands — then they are not duplicates and not in conflict, no matter how far apart their numbers are. Different things have different numbers; that is what makes them different things. Answer "distinct" and stop.

Only when they describe the same subject, choose one of:

- "replaced" — one artifact plainly supersedes another: a deprecated flag, step or default versus its current replacement. Prefer this whenever it applies. It keeps the original wording of the survivor, which is always better than rewriting.
- "duplicate" — they make the same claim, and each carries some detail the other lacks. Write one artifact that says everything all of them said.
- "conflict" — they give a different value for the same detail of the same subject, and you cannot tell which is current. Do not choose a side and do not merge; a person decides this one.
- "distinct" — different subjects, or one covers something the other simply does not.

When you answer "duplicate", the merged text must contain every number, version, date, path, flag, command and error string that appeared in any input. If you cannot write one that does, the answer is "conflict", not "duplicate".

Reply with JSON only, no commentary, in exactly this shape:

{"relation": "duplicate", "detail": "...", "supersedes": "a", "merged": {"title": "...", "text": "...", "category": "...", "tags": [], "caveats": []}}

- relation: one of "duplicate", "replaced", "conflict", "distinct".
- detail: one short sentence saying why. Always.
- supersedes: the letter of the artifact that is obsolete. Only with "replaced"; omit it otherwise.
- merged: only with "duplicate"; omit it entirely otherwise. `text` stands alone without its sources. `caveats` are the conditions under which it does not apply."#;
```

- [ ] **Step 4: Write the user prompt and the parser**

```rust
/// The artifacts, each under its title and a letter.
///
/// The title is not decoration, it is the subject: synthesis writes a body that
/// stands alone within its segment, which is not the same as naming what it is
/// about. Handed bodies alone, the model saw two anonymous spec lists with
/// different numbers and called them a contradiction — correctly, on the
/// evidence it was given.
///
/// `differing_values` is what `facts::fact_tokens` found on more than one side
/// with different values. It is a prior, not a verdict: it cannot tell a
/// conflict from two levels of detail about one subject, which is the whole
/// reason a model is asked.
///
/// `attempt` varies the wording. The endpoint replays identical output for
/// identical prompts, so without it a retry is the same unreadable bytes again.
pub fn dedupe_prompt(members: &[(&str, &str)], differing_values: &[String], attempt: i64) -> String {
    let mut s = String::new();
    for (i, (title, text)) in members.iter().enumerate() {
        let letter = (b'a' + i as u8) as char;
        s.push_str(&format!("Artifact {letter} — {title}\n{text}\n\n"));
    }
    if !differing_values.is_empty() {
        s.push_str(&format!(
            "These values appear with different readings across the artifacts: {}. \
             They may be a real disagreement, or the same subject described at \
             different levels of detail. Decide which.\n\n",
            differing_values.join(", ")
        ));
    }
    s.push_str(match attempt {
        0 => "Answer with the JSON object and nothing else.",
        1 => "Reply with only the JSON object described above. No prose.",
        2 => "Output the JSON object. Do not explain it.",
        3 => "Return the JSON object alone, starting with { and ending with }.",
        _ => "JSON object only. No commentary, no code fence.",
    });
    s
}

pub fn parse_dedupe(body: &str) -> Result<Dedupe> {
    #[derive(serde::Deserialize)]
    struct Raw {
        relation: String,
        #[serde(default)]
        detail: Option<String>,
        #[serde(default)]
        supersedes: Option<String>,
        #[serde(default)]
        merged: Option<RawMerged>,
    }
    #[derive(serde::Deserialize)]
    struct RawMerged {
        text: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        category: Option<String>,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        caveats: Vec<String>,
    }

    let r: Raw = serde_json::from_str(extract_json(body)).map_err(|e| {
        Error::MalformedLlmOutput(format!("dedupe reply was not the expected JSON: {e}"))
    })?;

    let side = r.supersedes.as_deref().and_then(|s| match s.trim() {
        "a" | "A" => Some('a'),
        "b" | "B" => Some('b'),
        _ => None,
    });

    let relation = match r.relation.trim().to_ascii_lowercase().as_str() {
        "duplicate" => Relation::Duplicate,
        // A direction the model would not name is not a direction. Falling back
        // to a conflict is what stops this picking a side by accident.
        "replaced" if side.is_some() => Relation::Replaced,
        "replaced" | "conflict" => Relation::Conflict,
        "distinct" => Relation::Distinct,
        other => {
            return Err(Error::MalformedLlmOutput(format!(
                "dedupe reply named an unknown relation {other:?}"
            )));
        }
    };

    // `merged` belongs to `duplicate` and to nothing else. A conflict verdict
    // that still handed us text to write would defeat the one outcome that
    // verdict exists to produce.
    match (&relation, &r.merged) {
        (Relation::Duplicate, None) => {
            return Err(Error::MalformedLlmOutput(
                "dedupe reply said duplicate but wrote no merged artifact".into(),
            ));
        }
        (rel, Some(_)) if *rel != Relation::Duplicate => {
            return Err(Error::MalformedLlmOutput(
                "dedupe reply carried a merged artifact on a non-duplicate verdict".into(),
            ));
        }
        _ => {}
    }

    Ok(Dedupe {
        relation,
        detail: r.detail.map(|d| d.trim().to_string()).filter(|d| !d.is_empty()),
        supersedes: side,
        merged: r.merged.map(|m| MergedDraft {
            title: m.title,
            text: m.text,
            category: m.category,
            tags: m.tags,
            caveats: m.caveats,
        }),
    })
}
```

Keep `JUDGE_SYSTEM`/`parse_judgement` deleted — Task 8 renamed them and nothing else calls them.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib prompt`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/infer/prompt.rs
git commit -m "feat(infer): four-verdict dedupe prompt and parser

replaced is preferred over duplicate: a superseded original keeps its
stored wording and its span, which is strictly better than a rewrite.
merged belongs to the duplicate verdict and to nothing else, so a
conflict cannot hand us text to write.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 10: Component expansion and the fan-in cap

**Files:**
- Modify: `src/store/pairs.rs` (add `open_component`)
- Modify: `src/config.rs` (`merge_max_roots`)
- Test: `src/store/pairs.rs` (in-module)

**Interfaces:**
- Consumes: `Clusters` from `src/jobs/consolidate.rs:42` — make it `pub(crate)`.
- Produces: `Store::open_component(&self, pair_id: i64) -> Result<Vec<ArtifactPair>>` — every `Pending` pair reachable from `pair_id` through shared artifact ids, including itself. `ConsolidateConfig.merge_max_roots: usize` (default 8).

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn a_component_gathers_every_pending_pair_that_shares_an_artifact() {
        // Merging a three-artifact group pairwise costs two calls and produces
        // a merged artifact that is superseded almost immediately. One
        // component, one call.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("four", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..4)
            .map(|i| NewArtifact {
                ordinal: i,
                text: format!("artifact {i}"),
                corpus_span: None, title: None, category: None,
                tags: vec![], segment_idx: None, caveats: vec![],
            })
            .collect();
        let m = s.insert_artifacts(&src.id, &new).await.unwrap();

        s.record_pair(&m[0].id, &m[1].id, 0.91).await.unwrap();
        s.record_pair(&m[1].id, &m[2].id, 0.90).await.unwrap();
        s.record_pair(&m[3].id, &m[0].id, 0.89).await.unwrap();
        let seed = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;

        let comp = s.open_component(seed).await.unwrap();
        assert_eq!(comp.len(), 3, "the component stopped short: {comp:?}");
    }

    #[tokio::test]
    async fn a_settled_pair_never_drags_an_answered_artifact_into_a_component() {
        // Pending only. A dismissed or near-identical pair carries a decision,
        // and following it would pull an already-answered artifact back into a
        // group that is about to be rewritten.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("three", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..3)
            .map(|i| NewArtifact {
                ordinal: i, text: format!("artifact {i}"),
                corpus_span: None, title: None, category: None,
                tags: vec![], segment_idx: None, caveats: vec![],
            })
            .collect();
        let m = s.insert_artifacts(&src.id, &new).await.unwrap();

        s.record_pair(&m[0].id, &m[1].id, 0.91).await.unwrap();
        s.record_pair(&m[1].id, &m[2].id, 0.90).await.unwrap();
        let all = s.pairs_by_state(PairState::Pending, 10).await.unwrap();
        let (seed, other) = (all[0].id, all[1].id);
        s.set_pair_state(other, PairState::Dismissed, None).await.unwrap();

        assert_eq!(s.open_component(seed).await.unwrap().len(), 1);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test a_component_gathers_every_pending_pair_that_shares_an_artifact`
Expected: FAIL — `open_component` does not exist.

- [ ] **Step 3: Implement**

In `src/store/pairs.rs`:

```rust
    /// Every `Pending` pair reachable from this one through a shared artifact.
    ///
    /// Computed at the moment of use rather than snapshotted when the unit was
    /// armed: membership changes while a unit waits out a backoff, and acting
    /// on a stale group would rewrite artifacts that have since been answered.
    ///
    /// `Pending` only. A dismissed or near-identical row carries a decision,
    /// and following it would pull an already-settled artifact into a group
    /// that is about to be superseded.
    pub async fn open_component(&self, pair_id: i64) -> Result<Vec<ArtifactPair>> {
        let open = self.pairs_by_state(PairState::Pending, 5_000).await?;
        let Some(seed) = open.iter().find(|p| p.id == pair_id) else {
            return Ok(vec![]);
        };

        let mut members: std::collections::HashSet<String> =
            [seed.a_id.clone(), seed.b_id.clone()].into_iter().collect();
        let mut picked: std::collections::HashSet<i64> = [seed.id].into_iter().collect();
        // Fixed point rather than one pass: a pair joins the component only
        // once one of its artifacts is already in it, and the pair that brings
        // that artifact in may come later in the list.
        loop {
            let mut grew = false;
            for p in &open {
                if picked.contains(&p.id) {
                    continue;
                }
                if members.contains(&p.a_id) || members.contains(&p.b_id) {
                    members.insert(p.a_id.clone());
                    members.insert(p.b_id.clone());
                    picked.insert(p.id);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        Ok(open.into_iter().filter(|p| picked.contains(&p.id)).collect())
    }
```

In `src/config.rs`, add to `ConsolidateConfig` and its `Default`:

```rust
    /// How many captured roots one merged artifact may draw on. Above this the
    /// component is left alone and surfaced instead: a merge of forty sources
    /// is no longer one atomic piece of knowledge, which is what an artifact is
    /// defined to be.
    pub merge_max_roots: usize,
```

with `merge_max_roots: 8`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib pairs && cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/store/pairs.rs src/config.rs
git commit -m "feat(store): expand a pair into its open component

Three related artifacts merged pairwise cost two calls and produce a
merged artifact that is superseded almost immediately. Computed fresh at
run time, not snapshotted at arming: membership changes while a unit
waits out a backoff.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 11: The two verification passes

**Files:**
- Create: the `verify` section of `src/jobs/merge.rs`
- Modify: `src/jobs/mod.rs`
- Test: `src/jobs/merge.rs` (in-module)

**Interfaces:**
- Consumes: `crate::infer::facts::fact_tokens`, `crate::infer::verify::missing_literals`, `MergedDraft` (Task 9).
- Produces:

```rust
/// What a merged draft would have lost. Empty means it may be written.
pub fn losses(roots: &[Chunk], draft: &MergedDraft) -> Vec<String>;
```

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::prompt::MergedDraft;

    fn draft(text: &str) -> MergedDraft {
        MergedDraft {
            title: None, text: text.into(), category: None,
            tags: vec![], caveats: vec![],
        }
    }

    fn root(text: &str) -> Chunk { /* build a Chunk with this text; helper */ }

    #[test]
    fn a_merge_that_keeps_both_values_is_allowed() {
        let roots = [root("The timeout is 30 seconds."), root("The timeout is 90 seconds.")];
        let d = draft("Sources differ on the timeout: an earlier capture gives 30 seconds, a later one 90 seconds.");
        assert!(losses(&roots, &d).is_empty(), "{:?}", losses(&roots, &d));
    }

    #[test]
    fn a_merge_that_drops_a_value_is_refused() {
        // The one way this feature can destroy knowledge without anyone
        // noticing: the model answers "duplicate" and quietly picks a side
        // while writing. Nothing downstream would ever see the missing number.
        let roots = [root("The timeout is 30 seconds."), root("The timeout is 90 seconds.")];
        let d = draft("The timeout is 90 seconds.");
        assert_eq!(losses(&roots, &d), vec!["30".to_string()]);
    }

    #[test]
    fn a_merge_that_paraphrases_a_command_is_refused() {
        // A paraphrased command is a command that later gets pasted into a
        // root shell.
        let roots = [
            root("Attach it with `mount --bind /src /dst`."),
            root("Bind mounts attach a directory elsewhere."),
        ];
        let d = draft("Bind mounts attach a directory elsewhere; use the bind mount option.");
        assert!(
            losses(&roots, &d).iter().any(|l| l.contains("mount --bind")),
            "the literal check let a paraphrased command through: {:?}",
            losses(&roots, &d)
        );
    }

    #[test]
    fn a_value_that_moved_into_the_caveats_is_not_lost() {
        // Caveats are stored and rendered. A value demoted there is still
        // recoverable, which is the whole test: this checks for loss, not for
        // prominence.
        let roots = [root("The timeout is 30 seconds."), root("The timeout is 90 seconds.")];
        let mut d = draft("The timeout is 90 seconds.");
        d.caveats = vec!["An earlier capture gave 30 seconds.".into()];
        assert!(losses(&roots, &d).is_empty());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib merge`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Implement**

Create `src/jobs/merge.rs` with:

```rust
//! Writing, verifying and undoing a merge.
//!
//! Merging is the one thing in this system that puts model-written text where
//! stored text used to be, so it is also the one thing that can lose knowledge
//! silently: a plausible paragraph reads exactly as well without the number it
//! dropped. `losses` is what stands between a verdict and a write, and it is
//! free — a scan of two token sets and one substring pass.

use crate::infer::prompt::MergedDraft;
use crate::store::artifacts::Chunk;

/// Every value and literal in `roots` that `draft` does not carry.
///
/// Both halves search the draft's text *and* its caveats: a caveat is stored,
/// rendered and recoverable, so a value demoted there has not been lost. This
/// checks for loss, not for prominence.
pub fn losses(roots: &[Chunk], draft: &MergedDraft) -> Vec<String> {
    let mut haystack = draft.text.clone();
    for c in &draft.caveats {
        haystack.push(' ');
        haystack.push_str(c);
    }

    let have = crate::infer::facts::fact_tokens(&haystack);
    let mut out: Vec<String> = Vec::new();

    for r in roots {
        for tok in crate::infer::facts::fact_tokens(&r.text) {
            if !have.contains(&tok) && !out.contains(&tok) {
                out.push(tok);
            }
        }
        // The existing check, with the merged text as the haystack instead of
        // the segment: `missing_literals(artifact_text, caveats, haystack)`
        // asks which literals of the first argument are absent from the third,
        // which is exactly the question here with the arguments in this order.
        for lit in crate::infer::verify::missing_literals(&r.text, &r.caveats, &haystack) {
            if !out.contains(&lit) {
                out.push(lit);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}
```

Add `pub mod merge;` to `src/jobs/mod.rs`. Write the `root` test helper to construct a `Chunk` with the given text and defaults for everything else.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib merge`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/merge.rs src/jobs/mod.rs
git commit -m "feat(jobs): refuse a merge that would lose a value or a literal

Merging is the one thing that puts model-written text where stored text
was, so it is the one thing that can lose knowledge silently: a plausible
paragraph reads just as well without the number it dropped. Both checks
are local and free, and both search the caveats too -- a value demoted
there is stored and recoverable, so it is not lost.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 12: The dedupe unit, recording verdicts only

**Files:**
- Modify: `src/jobs/dedupe.rs` (rewrite `run` and `apply`)
- Modify: `src/jobs/consolidate.rs` (`arm_dedupe` skips the `may_disagree` gate)
- Test: `src/jobs/dedupe.rs` (in-module)

**Interfaces:**
- Consumes: `open_component` (Task 10), `parse_dedupe`/`dedupe_prompt` (Task 9), `losses` (Task 11), `roots_of` (Task 2).
- Produces: `pub async fn run(core: &Core, pair_id: &str) -> Result<()>`; `pub struct Settlement { pub relation: Relation, pub merged: Option<MergedDraft>, pub roots: Vec<Chunk> }` returned by an inner `decide` so Task 14 can apply it.

With `autonomous = false` this task records `Contradiction`, `NoConflict`, `Superseded` (proposal) and `Oversized`, and for `duplicate` records `Contradiction` with a detail naming the merge it would have written. Task 14 turns that last branch into a real merge.

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn two_artifacts_about_different_subjects_are_never_merged() {
        // FAT12, FAT16 and FAT32 are near-identical in form and deliberately
        // different in content: 0.91 similarity, every number different. This
        // is the failure mode that turns a reference document into mush, and
        // it is the reason the prompt checks the subject before anything else.
        let mut core = test_core().await;
        core.completer = std::sync::Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"distinct","detail":"two different filesystems"}"#.into(),
        ]));
        let ids = seed_titled(
            &core,
            &[
                ("FAT16 Specifications", "65524 clusters, 16 bit numbers.", [1.0, 0.0]),
                ("FAT32 Specifications", "268435445 clusters, 28 bit numbers.", [0.93, 0.37]),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(
            core.store.pairs_by_state(PairState::NoConflict, 10).await.unwrap().len(),
            1
        );
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().superseded_by.is_none());
        }
    }

    #[tokio::test]
    async fn a_value_conflict_is_escalated_and_never_merged() {
        // Deciding which of two contradictory facts is true stays a person's
        // job. This is the one queue that expects a human, and autonomy does
        // not empty it.
        let mut core = test_core().await;
        core.completer = std::sync::Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"conflict","detail":"1.21.4 versus 1.30.0"}"#.into(),
        ]));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids).await;

        run(&core, &pair.to_string()).await.unwrap();

        let found = core.store.pairs_by_state(PairState::Contradiction, 10).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].detail.as_deref(), Some("1.21.4 versus 1.30.0"));
        for id in &ids {
            assert_eq!(
                core.store.get_artifact(id).await.unwrap().status,
                ArtifactStatus::Active,
                "a conflict hid an artifact"
            );
        }
    }

    #[tokio::test]
    async fn a_plain_replacement_supersedes_rather_than_merging() {
        // The survivor is a stored original with a valid span. That is
        // strictly better than a rewrite, and it is the path by which fidelity
        // keeps holding under autonomy — so the prompt prefers it and this
        // pins that the code does too.
        let mut core = test_core().await;
        core.consolidate.autonomous = true;
        core.completer = std::sync::Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"replaced","supersedes":"a","detail":"old flag vs new flag"}"#.into(),
        ]));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().superseded_by.as_deref(),
            Some(ids[1].as_str())
        );
        assert_eq!(
            core.store.get_artifact(&ids[1]).await.unwrap().provenance,
            Provenance::Captured,
            "a replacement wrote synthetic text"
        );
    }

    #[tokio::test]
    async fn a_replacement_naming_the_newer_artifact_is_not_trusted() {
        // A miscalibrated call proposing to hide the *newer* side disagrees
        // with the sweep's own newest-wins bias, so it falls back to a
        // conflict rather than being applied.
        let mut core = test_core().await;
        core.consolidate.autonomous = true;
        let ids = disagreeing(&core).await;
        sqlx::query("UPDATE artifacts SET created_at = created_at + 100 WHERE id = ?")
            .bind(&ids[1])
            .execute(&core.store.pool)
            .await
            .unwrap();
        core.completer = std::sync::Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"replaced","supersedes":"b","detail":"x"}"#.into(),
        ]));
        let pair = queue_pair(&core, &ids).await;

        run(&core, &pair.to_string()).await.unwrap();

        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().superseded_by.is_none());
        }
        assert_eq!(
            core.store.pairs_by_state(PairState::Contradiction, 10).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn a_component_past_the_fan_in_cap_is_surfaced_and_never_called_about() {
        let mut core = test_core().await;
        core.consolidate.merge_max_roots = 2;
        let completer = std::sync::Arc::new(ScriptedCompleter::new(vec![]));
        core.completer = completer.clone();
        let ids = seed(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.93, 0.37]),
                ("timeout is 90 seconds", [0.94, 0.34]),
            ],
        )
        .await;
        core.store.record_pair(&ids[0], &ids[1], 0.91).await.unwrap();
        core.store.record_pair(&ids[1], &ids[2], 0.90).await.unwrap();
        let pair = core.store.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(completer.calls(), 0, "an oversized component cost a call");
        assert_eq!(
            core.store.pairs_by_state(PairState::Oversized, 10).await.unwrap().len(),
            2,
            "every pair in the component must leave the pending queue"
        );
    }

    #[tokio::test]
    async fn a_failed_dedupe_leaves_the_component_pending() {
        // A dead endpoint must not silently clear a queue of real duplicates.
        let mut core = test_core().await;
        core.completer = std::sync::Arc::new(ScriptedCompleter::new(vec!["not json".into()]));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids).await;

        assert!(run(&core, &pair.to_string()).await.is_err());
        assert_eq!(
            core.store.pairs_by_state(PairState::Pending, 10).await.unwrap().len(),
            1
        );
        assert_eq!(core.store.get_pair(pair).await.unwrap().judge_unreadable, 1);
    }
```

Add the helpers `seed_titled` (like `seed` but taking a title) and `queue_pair` (records a pair for two ids and returns its row id) to `src/jobs/consolidate.rs`'s `pub(crate) mod tests`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib dedupe`
Expected: FAIL.

- [ ] **Step 3: Implement**

Rewrite `src/jobs/dedupe.rs`:

```rust
//! One component, one call.
//!
//! The sweep decides which pairs are worth asking about, which costs nothing,
//! and arms one unit each. The unit then expands its pair into the connected
//! component of still-open pairs around it — three related artifacts settled
//! pairwise cost two calls and produce a merged artifact that is superseded
//! almost immediately.

use crate::core::Core;
use crate::error::{Error, Result};
use crate::infer::prompt::{Dedupe, MergedDraft, Relation};
use crate::store::artifacts::{ArtifactStatus, Chunk};
use crate::store::pairs::{ArtifactPair, PairState};

/// What the model decided, with everything the write path needs already read.
pub struct Settlement {
    pub relation: Relation,
    pub detail: Option<String>,
    /// The artifact named obsolete, already checked against newest-wins.
    pub obsolete: Option<String>,
    pub merged: Option<MergedDraft>,
    /// The component's members, active at the time of the call.
    pub members: Vec<Chunk>,
    /// Their captured roots, flattened. What the model was actually shown.
    pub roots: Vec<Chunk>,
    pub pairs: Vec<ArtifactPair>,
}

pub async fn run(core: &Core, pair_id: &str) -> Result<()> {
    let id: i64 = pair_id.parse().map_err(|_| Error::NotFound)?;
    let p = core.store.get_pair(id).await?;
    if p.state != PairState::Pending {
        // Settled by an operator, by a later sweep, or by a sibling unit that
        // already merged this component while this one waited.
        return Ok(());
    }

    let pairs = core.store.open_component(id).await?;
    let mut member_ids: Vec<String> = pairs
        .iter()
        .flat_map(|p| [p.a_id.clone(), p.b_id.clone()])
        .collect();
    member_ids.sort();
    member_ids.dedup();

    let mut members = Vec::new();
    for mid in &member_ids {
        let c = core.store.get_artifact(mid).await?;
        // Re-checked here and not only at arming: a member can be superseded
        // or deprecated while the unit waits out a backoff.
        if c.status != ArtifactStatus::Active || c.superseded_by.is_some() {
            settle_all(core, &pairs, PairState::Dismissed, None).await?;
            return Ok(());
        }
        members.push(c);
    }
    if members.len() < 2 {
        settle_all(core, &pairs, PairState::Dismissed, None).await?;
        return Ok(());
    }

    // Flatten to captured roots before anything else. A merged member's own
    // text is never shown to the model: rewriting from a rewrite is how
    // generational drift starts, and the closure is stored precisely so this
    // costs one query.
    let root_map = core.store.roots_of(&member_ids).await?;
    let mut root_ids: Vec<String> = root_map.values().flatten().cloned().collect();
    root_ids.sort();
    root_ids.dedup();

    if root_ids.len() > core.consolidate.merge_max_roots {
        tracing::info!(
            pair = id,
            roots = root_ids.len(),
            cap = core.consolidate.merge_max_roots,
            "component is past the fan-in cap; surfacing instead of merging"
        );
        settle_all(core, &pairs, PairState::Oversized, None).await?;
        return Ok(());
    }

    let mut roots = Vec::new();
    for rid in &root_ids {
        roots.push(core.store.get_artifact(rid).await?);
    }

    for p in &pairs {
        core.store.record_judge_attempt(p.id).await?;
    }

    let shown: Vec<(&str, &str)> = roots
        .iter()
        .map(|c| (c.title.as_deref().unwrap_or("untitled"), c.text.as_str()))
        .collect();
    let differing = differing_values(&roots);

    let permit = core.gate.background().await;
    let reply = match core
        .completer
        .complete(
            crate::infer::prompt::DEDUPE_SYSTEM,
            &crate::infer::prompt::dedupe_prompt(&shown, &differing, p.judge_attempts),
        )
        .await
    {
        Ok(r) => {
            permit.succeeded();
            r
        }
        Err(e) => {
            permit.failed(&e);
            return Err(e);
        }
    };

    let verdict = match crate::infer::prompt::parse_dedupe(&reply) {
        Ok(v) => v,
        // A reply that cannot be read is an error, not a verdict: the component
        // stays pending and the unit retries under the queue's backoff.
        Err(e) => {
            for p in &pairs {
                core.store.record_unreadable_judgement(p.id).await?;
            }
            tracing::warn!(pair = id, attempt = p.judge_attempts, error = %e,
                "dedupe reply unreadable; component stays pending");
            return Err(e);
        }
    };

    let settlement = interpret(verdict, members, roots, pairs);
    apply(core, settlement).await
}

/// Values that appear with different readings across the roots. A prior for the
/// prompt, never a verdict: it cannot tell a conflict from two levels of detail
/// about one subject, which is the whole reason a model is asked.
fn differing_values(roots: &[Chunk]) -> Vec<String> {
    let sets: Vec<_> = roots
        .iter()
        .map(|r| crate::infer::facts::fact_tokens(&r.text))
        .collect();
    let mut all: std::collections::BTreeSet<String> = Default::default();
    for s in &sets {
        all.extend(s.iter().cloned());
    }
    all.into_iter()
        .filter(|t| !sets.iter().all(|s| s.contains(t)))
        .collect()
}

/// Turn a parsed reply into what the write path will do, applying the two
/// guards that do not need the store: newest-wins on a named direction, and the
/// loss check on a merged draft.
fn interpret(
    v: Dedupe,
    members: Vec<Chunk>,
    roots: Vec<Chunk>,
    pairs: Vec<ArtifactPair>,
) -> Settlement {
    let mut relation = v.relation;
    let mut detail = v.detail;
    let mut merged = v.merged;
    let mut obsolete = None;

    if relation == Relation::Replaced {
        // Trust a named direction only when it agrees with the newest-wins
        // bias: a call naming the *newer* artifact obsolete would propose
        // hiding the side more likely to be current.
        let named = match v.supersedes {
            Some('a') => members.first(),
            _ => members.get(1),
        };
        let other = match v.supersedes {
            Some('a') => members.get(1),
            _ => members.first(),
        };
        obsolete = match (named, other) {
            (Some(n), Some(o)) if n.created_at <= o.created_at => Some(n.id.clone()),
            _ => None,
        };
        if obsolete.is_none() {
            relation = Relation::Conflict;
        }
    }

    if relation == Relation::Duplicate {
        if let Some(d) = &merged {
            let lost = crate::jobs::merge::losses(&roots, d);
            if !lost.is_empty() {
                // Escalate rather than retry: the merge is the thing that was
                // wrong, and a person can read what it would have cost.
                detail = Some(format!("the merge would have lost {}", lost.join(", ")));
                relation = Relation::Conflict;
                merged = None;
            }
        }
    }

    Settlement { relation, detail, obsolete, merged, members, roots, pairs }
}

async fn apply(core: &Core, s: Settlement) -> Result<()> {
    match s.relation {
        Relation::Distinct => settle_all(core, &s.pairs, PairState::NoConflict, s.detail.as_deref()).await,
        Relation::Conflict => settle_all(core, &s.pairs, PairState::Contradiction, s.detail.as_deref()).await,
        Relation::Replaced => {
            let obsolete = s.obsolete.clone().expect("interpret guarantees it");
            let winner = s
                .members
                .iter()
                .find(|m| m.id != obsolete)
                .expect("a component has at least two members");
            for p in &s.pairs {
                core.store.set_pair_superseded(p.id, &obsolete, s.detail.as_deref()).await?;
            }
            if core.consolidate.autonomous {
                core.supersede(&obsolete, &winner.id).await?;
                tracing::info!(superseded = %obsolete, by = %winner.id, "applied a replacement");
            } else {
                tracing::info!(obsolete = %obsolete, "proposed a replacement, pending confirmation");
            }
            Ok(())
        }
        // Task 14 replaces this branch with the merge write path. Until then a
        // duplicate is recorded, not applied: the verdict is worth reading
        // before anything acts on it.
        Relation::Duplicate => {
            let d = s.detail.or_else(|| Some("would merge".into()));
            settle_all(core, &s.pairs, PairState::Contradiction, d.as_deref()).await
        }
    }
}

async fn settle_all(
    core: &Core,
    pairs: &[ArtifactPair],
    state: PairState,
    detail: Option<&str>,
) -> Result<()> {
    for p in pairs {
        core.store.set_pair_state(p.id, state, detail).await?;
    }
    Ok(())
}
```

In `src/jobs/consolidate.rs`'s `arm_dedupe`, delete the `may_disagree` block (lines 518–523): the gate is gone, and the prefilter now lives in the prompt and the verification.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/dedupe.rs src/jobs/consolidate.rs
git commit -m "feat(jobs): settle a whole component in one call

Four verdicts. A conflict is escalated and never merged; a replacement
keeps a stored original rather than writing synthetic text; a duplicate
is recorded but not yet applied. Members are flattened to captured roots
before the call, so a re-merge never rewrites from a rewrite.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## Phase 4 — The merge write path

### Task 13: Write a merge, embed it, then supersede its roots

**Files:**
- Modify: `src/jobs/merge.rs` (add `write`), `src/jobs/dedupe.rs` (`apply`'s `Duplicate` branch), `src/jobs/embed.rs` (`mark_indexed`)
- Test: `src/jobs/merge.rs` (in-module)

**Interfaces:**
- Consumes: `insert_merged_artifact` (Task 2), `Settlement` (Task 12).
- Produces: `pub async fn write(core: &Core, draft: &MergedDraft, roots: &[String]) -> Result<Chunk>`; `pub async fn finish(core: &Core, merged_id: &str) -> Result<()>` — supersedes the roots of an already-embedded merged artifact.

**Order matters.** `write` creates the artifact and its lineage and arms an embed job. `finish` runs when that embed completes. Superseding before the merged artifact is indexed would leave a window in which the roots are out of search and the merge is not yet in it — the knowledge temporarily unreachable, which is the failure class `heal_dangling_supersessions` exists to prevent.

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn knowledge_is_never_unreachable_during_a_merge() {
        // The write path is five steps over two stores that cannot be written
        // atomically. Embedding before superseding means the worst state a
        // crash can leave is redundancy -- the merge and its roots both in
        // search -- rather than a gap. Redundancy is the state the system is
        // coming from anyway; a gap is knowledge nobody can find.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;
        let draft = MergedDraft {
            title: Some("Mounting".into()),
            text: "Mount the filesystem, or attach the volume, before writing.".into(),
            category: None, tags: vec![], caveats: vec![],
        };

        // Step 1+2: the merge exists, nothing is hidden yet.
        let m = write(&core, &draft, &ids).await.unwrap();
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().superseded_by.is_none());
        }
        let hits = core
            .vectors
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert!(
            hits.iter().any(|h| h.payload.artifact_id == ids[0]),
            "the roots left search before the merge was indexed"
        );

        // Step 3: embed the merge. Step 4 follows from `mark_indexed`.
        crate::jobs::embed::run(&core, &m.id).await.unwrap();
        for id in &ids {
            assert_eq!(
                core.store.get_artifact(id).await.unwrap().superseded_by.as_deref(),
                Some(m.id.as_str()),
                "the roots were never superseded"
            );
        }
        let hits = core
            .vectors
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert!(
            hits.iter().any(|h| h.payload.artifact_id == m.id),
            "the merge never reached search"
        );
    }

    #[tokio::test]
    async fn a_merge_whose_roots_were_never_superseded_is_finished_by_the_next_sweep() {
        // Crash between embedding the merge and hiding its roots. The merge
        // looks complete from the artifact side and absent from the pair side,
        // so only a join over the lineage would ever notice.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;
        let draft = MergedDraft {
            title: None,
            text: "Mount the filesystem, or attach the volume, before writing.".into(),
            category: None, tags: vec![], caveats: vec![],
        };
        let m = write(&core, &draft, &ids).await.unwrap();
        // Embed without the arming hook, as an interrupted run would leave it.
        core.store.mark_embedded(&m.id, "fake-embed", 0).await.unwrap();

        crate::jobs::consolidate::run(&core).await.unwrap();

        for id in &ids {
            assert_eq!(
                core.store.get_artifact(id).await.unwrap().superseded_by.as_deref(),
                Some(m.id.as_str())
            );
        }
    }

    #[tokio::test]
    async fn a_merge_of_a_merge_is_written_from_the_captured_roots() {
        // The anti-drift rule. M1(a,b) merged with c must be written from a, b
        // and c -- never from M1's text, which is itself a rewrite. Otherwise
        // every generation paraphrases a paraphrase and the originals drift
        // further away with each one.
        let core = test_core().await;
        let ids = seed(&core, &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])]).await;
        let draft = MergedDraft {
            title: None, text: "a text and b text".into(),
            category: None, tags: vec![], caveats: vec![],
        };
        let m1 = write(&core, &draft, &ids).await.unwrap();

        let c = seed_into_new_corpus(&core, "c text", [0.94, 0.34]).await;
        let m2_draft = MergedDraft {
            title: None, text: "a text and b text and c text".into(),
            category: None, tags: vec![], caveats: vec![],
        };
        let m2 = write(&core, &m2_draft, &[m1.id.clone(), c.clone()]).await.unwrap();

        let roots = core.store.roots_of(std::slice::from_ref(&m2.id)).await.unwrap();
        let mut got = roots[&m2.id].clone();
        got.sort();
        let mut want = vec![ids[0].clone(), ids[1].clone(), c];
        want.sort();
        assert_eq!(got, want, "the second merge did not flatten to captured roots");
        assert!(
            !got.contains(&m1.id),
            "a merged artifact was recorded as a root of another merge"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib merge`
Expected: FAIL — `write` does not exist.

- [ ] **Step 3: Implement `write` and `finish`**

Append to `src/jobs/merge.rs`:

```rust
/// Create a merged artifact and arm its embedding. Its roots are **not**
/// superseded here.
///
/// Superseding before the merge is indexed opens a window in which the roots
/// are out of search and the merge is not yet in it — the knowledge temporarily
/// unreachable. In this order the worst a crash can leave is the merge and its
/// roots both in search, which is redundancy, which is the state the system was
/// already in. `finish` closes it once the embedding lands.
pub async fn write(core: &Core, draft: &MergedDraft, roots: &[String]) -> Result<Chunk> {
    let m = core
        .store
        .insert_merged_artifact(
            &crate::store::artifacts::NewMerged {
                text: draft.text.clone(),
                title: draft.title.clone(),
                category: draft.category.clone(),
                tags: draft.tags.clone(),
                caveats: draft.caveats.clone(),
            },
            roots,
        )
        .await?;
    core.store.enqueue(Stage::Embed, "artifact", &m.id).await?;
    tracing::info!(merged = %m.id, roots = roots.len(), "wrote a merged artifact");
    Ok(m)
}

/// Supersede the roots of an already-indexed merged artifact.
///
/// Called from `mark_indexed` when a `merged` artifact finishes embedding, and
/// again from the sweep for merges whose process died in between.
pub async fn finish(core: &Core, merged_id: &str) -> Result<()> {
    let m = core.store.get_artifact(merged_id).await?;
    if m.provenance != Provenance::Merged || m.status != ArtifactStatus::Active {
        return Ok(());
    }
    let roots = core.store.roots_of(std::slice::from_ref(&m.id.clone())).await?;
    for root in roots.get(&m.id).into_iter().flatten() {
        let Ok(r) = core.store.get_artifact(root).await else {
            continue;
        };
        if r.status != ArtifactStatus::Active || r.superseded_by.is_some() {
            continue;
        }
        // One failure does not abandon the rest: an operator deprecating a root
        // between the read and here is an ordinary race, and the sweep's repair
        // reaches whatever is left.
        if let Err(e) = core.supersede(root, &m.id).await {
            tracing::warn!(root = %root, merged = %m.id, error = %e,
                "could not hide a merged artifact's root; it stays active");
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Hook it into embedding and the sweep**

In `src/jobs/embed.rs`'s `mark_indexed`, after the `relate::arm` call from Task 5:

```rust
    // A merged artifact hides its roots only once it is indexed itself, so the
    // knowledge is never out of search on both sides at once.
    if chunk.provenance == crate::store::artifacts::Provenance::Merged {
        crate::jobs::merge::finish(core, &chunk.id).await?;
    }
```

In `src/jobs/consolidate.rs`'s `run`, beside the other repairs:

```rust
    // A merge whose process died between embedding and superseding. Invisible
    // to everything else: complete from the artifact side, absent from the pair
    // side. Warn and carry on, like the repairs above.
    match core.store.merged_with_active_roots(200).await {
        Ok(unfinished) => {
            for id in unfinished {
                if let Err(e) = crate::jobs::merge::finish(core, &id).await {
                    tracing::warn!(merged = %id, error = %e, "could not finish a merge");
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not look for unfinished merges"),
    }
```

- [ ] **Step 5: Turn the `Duplicate` branch into a real merge**

In `src/jobs/dedupe.rs`'s `apply`:

```rust
        Relation::Duplicate => {
            let draft = s.merged.as_ref().expect("interpret guarantees it");
            if !core.consolidate.autonomous {
                // Recorded, not applied. Reading the verdicts before letting
                // the system act on them is the cheapest evidence available
                // about whether the contract holds on real data.
                let d = s.detail.clone().or_else(|| Some("would merge".into()));
                return settle_all(core, &s.pairs, PairState::Contradiction, d.as_deref()).await;
            }
            let root_ids: Vec<String> = s.roots.iter().map(|c| c.id.clone()).collect();
            let m = crate::jobs::merge::write(core, draft, &root_ids).await?;
            settle_all(core, &s.pairs, PairState::NoConflict, Some(&format!("merged into {}", m.id))).await?;
            Ok(())
        }
```

**Subsumed merges.** A component's members may include a merged artifact, and `finish` as written above hides only *roots*. Merging M1(a,b) with c produces M2 with roots {a,b,c}; a, b and c are hidden, and M1 is left active — where it is a near-duplicate of M2, so the next relate unit files the pair again and the two churn against each other forever.

The rule that closes it needs no extra column and survives a crash, because it is derivable from what is already stored: **`finish` also supersedes any active merged artifact whose root set is a subset of this merge's root set.** If M2 was written from everything M1 was made of, M1 is subsumed by construction. Add to `src/store/lineage.rs`:

```rust
    /// Active merged artifacts, other than `child_id`, every root of which is
    /// also a root of `child_id`.
    ///
    /// A merge written from everything an earlier merge was made of subsumes
    /// it. Superseding the roots alone would leave that earlier merge active
    /// and near-identical to the new one, and the two would be re-paired on
    /// every sweep for as long as they both existed.
    pub async fn subsumed_merges(&self, child_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT other.id FROM artifacts other
              WHERE other.provenance = 'merged'
                AND other.status = 'active'
                AND other.superseded_by IS NULL
                AND other.id != ?1
                AND EXISTS (SELECT 1 FROM artifact_sources WHERE child_id = other.id)
                AND NOT EXISTS (
                      SELECT 1 FROM artifact_sources mine
                       WHERE mine.child_id = other.id
                         AND mine.root_id NOT IN (
                               SELECT root_id FROM artifact_sources WHERE child_id = ?1))",
        )
        .bind(child_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get("id")).collect())
    }
```

and in `finish`, after the root loop:

```rust
    for older in core.store.subsumed_merges(&m.id).await? {
        if let Err(e) = core.supersede(&older, &m.id).await {
            tracing::warn!(subsumed = %older, by = %m.id, error = %e,
                "could not hide a merge this one subsumes; it stays active");
        }
    }
```

The `EXISTS` guard matters: a merged artifact whose roots have all been deleted has no lineage rows left, and without it the `NOT EXISTS` clause would call it a subset of everything and hide it behind an unrelated merge. That artifact is `flag_orphans`' business (Task 14), not this function's.

Add the covering test to `src/jobs/merge.rs`:

```rust
    #[tokio::test]
    async fn a_merge_that_subsumes_an_earlier_one_hides_it() {
        // Superseding only the roots leaves the earlier merge active and
        // near-identical to the new one, so the relate unit re-pairs them on
        // every sweep and the two churn against each other forever.
        let core = test_core().await;
        let ids = seed(&core, &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])]).await;
        let m1 = write(
            &core,
            &MergedDraft {
                title: None, text: "a text and b text".into(),
                category: None, tags: vec![], caveats: vec![],
            },
            &ids,
        )
        .await
        .unwrap();
        crate::jobs::embed::run(&core, &m1.id).await.unwrap();

        let c = seed_into_new_corpus(&core, "c text", [0.94, 0.34]).await;
        let m2 = write(
            &core,
            &MergedDraft {
                title: None, text: "a text and b text and c text".into(),
                category: None, tags: vec![], caveats: vec![],
            },
            &[m1.id.clone(), c.clone()],
        )
        .await
        .unwrap();
        crate::jobs::embed::run(&core, &m2.id).await.unwrap();

        assert_eq!(
            core.store.get_artifact(&m1.id).await.unwrap().superseded_by.as_deref(),
            Some(m2.id.as_str()),
            "the subsumed merge is still active and will be re-paired forever"
        );
        for id in ids.iter().chain(std::iter::once(&c)) {
            assert_eq!(
                core.store.get_artifact(id).await.unwrap().superseded_by.as_deref(),
                Some(m2.id.as_str())
            );
        }
    }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/jobs/merge.rs src/jobs/dedupe.rs src/jobs/embed.rs src/jobs/consolidate.rs
git commit -m "feat(jobs): write a merge, index it, then hide its roots

Embedding before superseding means the worst a crash can leave is the
merge and its roots both in search. The other order leaves a window in
which neither is findable, which is the failure class
heal_dangling_supersessions exists to prevent. The sweep gains a repair
for merges whose process died in between -- a state complete from the
artifact side and absent from the pair side, so nothing else would see it.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 14: Undo, and the pairs that must stay dismissed

**Files:**
- Modify: `src/jobs/merge.rs` (add `undo`, `flag_orphans`)
- Modify: `src/jobs/consolidate.rs` (call `flag_orphans`)
- Test: `src/jobs/merge.rs` (in-module)

**Interfaces:**
- Produces: `pub async fn undo(core: &Core, merged_id: &str) -> Result<()>`; `pub async fn flag_orphans(core: &Core) -> Result<usize>`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn undoing_a_merge_survives_the_next_sweep() {
        // Reactivating the roots alone accomplishes nothing: the next sweep
        // re-finds them, the model says duplicate again, and the operator's
        // decision is silently undone. Literally the same bug as
        // reactivating_a_superseded_artifact_survives_the_next_sweep.
        let mut core = test_core().await;
        core.consolidate.autonomous = true;
        core.completer = std::sync::Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"duplicate","detail":"same claim",
                "merged":{"text":"Mount the filesystem, or attach the volume, before writing.",
                          "tags":[],"caveats":[]}}"#
                .into(),
        ]));
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids).await;
        crate::jobs::dedupe::run(&core, &pair.to_string()).await.unwrap();
        let m = core.store.merged_with_active_roots(10).await.unwrap();
        let merged_id = if m.is_empty() {
            // already finished
            core.store.get_artifact(&ids[0]).await.unwrap().superseded_by.unwrap()
        } else {
            m[0].clone()
        };
        crate::jobs::embed::run(&core, &merged_id).await.ok();

        undo(&core, &merged_id).await.unwrap();

        for id in &ids {
            let c = core.store.get_artifact(id).await.unwrap();
            assert_eq!(c.status, ArtifactStatus::Active);
            assert!(c.superseded_by.is_none());
        }
        assert_eq!(
            core.store.get_artifact(&merged_id).await.unwrap().status,
            ArtifactStatus::Deprecated,
            "the merged artifact was deleted, taking its lineage with it"
        );
        assert_eq!(
            core.store.pairs_by_state(PairState::Dismissed, 10).await.unwrap().len(),
            1,
            "the pair was left answerable, so the sweep will merge it again"
        );

        crate::jobs::consolidate::run(&core).await.unwrap();
        for id in &ids {
            assert!(
                core.store.get_artifact(id).await.unwrap().superseded_by.is_none(),
                "the sweep merged the pair again after an explicit undo"
            );
        }
    }

    #[tokio::test]
    async fn deleting_a_root_flags_the_merged_artifact_rather_than_hiding_the_loss() {
        // The cascade removes the lineage row while the merged text still
        // carries that root's content, so the artifact claims less provenance
        // than it has. Not data loss, but a silent untruth.
        let core = test_core().await;
        let ids = seed(&core, &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])]).await;
        let draft = MergedDraft {
            title: None, text: "a text and b text".into(),
            category: None, tags: vec![], caveats: vec![],
        };
        let m = write(&core, &draft, &ids).await.unwrap();

        core.store.delete_artifact(&ids[0]).await.unwrap();
        assert_eq!(flag_orphans(&core).await.unwrap(), 1);

        let flagged = core.store.get_artifact(&m.id).await.unwrap();
        assert!(flagged.flags.iter().any(|f| f == "orphaned_source"), "{flagged:?}");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test undoing_a_merge_survives_the_next_sweep`
Expected: FAIL — `undo` does not exist.

- [ ] **Step 3: Implement**

```rust
/// Take a merge back: the roots return, the merged artifact is retired, and the
/// pairs that produced it are dismissed.
///
/// The last of those three is the one that is easy to leave out and useless to
/// leave out. Reactivating the roots alone accomplishes nothing: the sweep
/// re-finds them, the model reaches the same verdict, and the decision is
/// silently undone. `record_pair` is INSERT OR IGNORE, so a dismissed row is
/// respected forever — the mechanism exists and only needs to be used.
///
/// The merged artifact is deprecated rather than deleted: `artifact_sources`
/// cascades away with a delete, taking the record of what was attempted.
///
/// This is for an *explicit* undo. A merged artifact that is simply deleted is
/// handled by `heal_dangling_supersessions`, which restores the roots — and a
/// fresh merge is then correct, because the duplication is genuinely back. A
/// decision may overrule the sweep; a deletion may not.
pub async fn undo(core: &Core, merged_id: &str) -> Result<()> {
    let m = core.store.get_artifact(merged_id).await?;
    if m.provenance != Provenance::Merged {
        return Err(Error::Validation(format!("{merged_id} is not a merged artifact")));
    }

    let roots = core.store.roots_of(std::slice::from_ref(&m.id.clone())).await?;
    let members: Vec<String> = roots.get(&m.id).cloned().unwrap_or_default();

    for r in &members {
        if let Err(e) = core.reactivate(r).await {
            tracing::warn!(root = %r, error = %e, "could not reactivate a merged artifact's root");
        }
    }

    // Deprecate only after the roots are back: the other order leaves a window
    // with nothing in search at all.
    core.deprecate(&m.id).await?;

    // Every pair among the restored members. Without this the undo lasts until
    // the next sweep and no longer.
    for pair in core.store.pairs_among(&members).await? {
        core.store.set_pair_state(pair.id, PairState::Dismissed, Some("merge undone")).await?;
    }
    tracing::info!(merged = %m.id, roots = members.len(), "undid a merge");
    Ok(())
}

/// Flag merged artifacts that have lost a source to a delete.
///
/// The text still carries what the deleted root said, so this is not data loss
/// — it is a claim of provenance the artifact can no longer support, and the
/// detail pane says so rather than quietly showing one fewer source.
pub async fn flag_orphans(core: &Core) -> Result<usize> {
    let mut n = 0;
    for id in core.store.merged_missing_a_source(500).await? {
        core.store
            .set_artifact_flags(
                &id,
                &["orphaned_source".to_string()],
                Some("one of this artifact's sources has been deleted"),
            )
            .await?;
        n += 1;
    }
    Ok(n)
}
```

Add two store reads in `src/store/lineage.rs`:

```rust
    /// Every pair, in any state, both of whose artifacts are in this set.
    pub async fn pairs_among(&self, ids: &[String]) -> Result<Vec<ArtifactPair>> { /* ... */ }

    /// Merged artifacts whose stored lineage is smaller than the number of
    /// sources they were written from. Recorded as a count on the artifact at
    /// write time, so this is a comparison rather than a guess.
    pub async fn merged_missing_a_source(&self, limit: i64) -> Result<Vec<String>> { /* ... */ }
```

`merged_missing_a_source` needs a `source_count` column on `artifacts`, written by `insert_merged_artifact` as `roots.len()`. Add it to `schema.sql` beside `provenance` (one column per line) and to `Chunk`.

Call `flag_orphans` from `consolidate::run` beside the other repairs, warn-and-carry-on.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/merge.rs src/store/lineage.rs src/store/schema.sql src/store/artifacts.rs src/jobs/consolidate.rs
git commit -m "feat(jobs): undo a merge, and make the undo stick

Reactivating the roots alone accomplishes nothing -- the sweep re-finds
them and reaches the same verdict, so the decision is silently undone.
Dismissing the pairs is what makes it permanent. The merged artifact is
deprecated rather than deleted, because a delete cascades the lineage
away with it.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## Phase 5 — Surface, pacing, measurement

### Task 15: Ops and the detail pane

**Files:**
- Modify: `src/web/ui.rs`, `src/web/templates/` (artifact detail, ops), `src/web/api.rs`
- Test: `src/web/ui.rs` (in-module)

**Interfaces:**
- Consumes: `roots_of`, `pairs_by_state`, `merge::undo`.
- Produces: routes `POST /ui/merge/{id}/undo`; Ops sections **Merged**, **Conflicts**, **Oversized**.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn a_merged_artifact_renders_its_roots_instead_of_corpus_lines() {
        // A captured artifact renders the corpus lines its span claims. A
        // merged one has no span and no corpus, so the pane must show what it
        // is actually made of -- and each root must link to its own corpus,
        // because that is where the wording it was built from still lives.
        let core = test_core().await;
        let ids = seed(&core, &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])]).await;
        let draft = MergedDraft {
            title: None, text: "a text and b text".into(),
            category: None, tags: vec![], caveats: vec![],
        };
        let m = crate::jobs::merge::write(&core, &draft, &ids).await.unwrap();

        let html = render_artifact_detail(&core, &m.id).await.unwrap();
        for id in &ids {
            assert!(html.contains(id.as_str()), "the pane does not name root {id}");
        }
        assert!(!html.contains("corpus lines"), "a merged artifact claimed a span");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test a_merged_artifact_renders_its_roots_instead_of_corpus_lines`
Expected: FAIL.

- [ ] **Step 3: Implement**

In the artifact detail handler, branch on `chunk.provenance`: `Captured` keeps the existing corpus-lines path; `Merged` loads `roots_of` and renders each root with its title, its corpus link, and its status. Add the **Undo merge** button, posting to `/ui/merge/{id}/undo`, which calls `crate::jobs::merge::undo`.

On the Ops page add three sections beside the existing ones: **Merged** (`artifacts_by_status` filtered to `provenance = 'merged'`), **Conflicts** (`pairs_by_state(Contradiction, ..)`), **Oversized** (`pairs_by_state(Oversized, ..)`), each with its `count_pairs_by_state` total so the page can say how many it is not showing.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/
git commit -m "feat(ui): render merged artifacts by their roots, and undo a merge

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 16: Pace dedupe by a rate, not by a sweep

**Files:**
- Modify: `src/core/background.rs` (add `spawn_dedupe_ticker`), `src/main.rs:286-296`, `src/jobs/consolidate.rs` (`arm_dedupe`), `src/config.rs`, `config.example.toml`, `README.md:158`
- Test: `src/core/background.rs`, `src/jobs/consolidate.rs`

**Interfaces:**
- Produces: `ConsolidateConfig.dedupe_interval_mins: u64` (15), `.max_dedupe_per_tick: usize` (5); `pub fn spawn_dedupe_ticker(core: Core, shutdown: watch::Receiver<bool>) -> JoinHandle<()>`. `max_judgements` is removed.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn dedupe_units_sort_behind_capture_work() {
        // A large ingest may delay hygiene; hygiene may not delay an ingest.
        // jobs.seq and idx_jobs_claim2 already order the queue this way -- this
        // pins that dedupe actually uses it.
        let core = test_core().await;
        core.store.enqueue(Stage::SegmentWindow, "segment", "w0").await.unwrap();
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids).await;
        crate::jobs::consolidate::arm_dedupe_for_test(&core, pair).await.unwrap();

        let first = core.store.claim_job().await.unwrap().unwrap();
        assert_eq!(first.stage, Stage::SegmentWindow, "dedupe ran ahead of capture");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test dedupe_units_sort_behind_capture_work`
Expected: FAIL.

- [ ] **Step 3: Implement**

In `src/config.rs`, replace `max_judgements` with the two new fields and defaults. In `arm_dedupe`, use `core.consolidate.max_dedupe_per_tick` as the budget and arm with a high `seq`:

```rust
/// Dedupe units sort behind synthesis and embedding of equal attempt count, so
/// hygiene consumes what capture leaves over rather than the other way round.
/// `claim_job` orders by (state, attempts, seq, id), and a window's seq is its
/// index within a document — a few hundred at most.
const DEDUPE_SEQ_BASE: i64 = 1_000_000;
```

`rearm_idle_seq(Stage::Dedupe, "pair", &target, DEDUPE_SEQ_BASE + armed as i64)`.

Add `spawn_dedupe_ticker` modelled on `spawn_retention_ticker`, firing every `dedupe_interval_mins`, calling `arm_dedupe(&core)`. Wire it in `src/main.rs` beside the other two and push its handle. Leave the in-flight rule alone: `live_job` still skips a pair whose unit is queued, and nothing counts units in flight — an unreachable unit must not be able to block every other pair.

Expose `arm_dedupe_for_test` as `#[cfg(test)] pub(crate)`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/core/background.rs src/main.rs src/jobs/consolidate.rs config.example.toml README.md
git commit -m "feat(consolidate): pace dedupe by a rate, not by a sweep

max_judgements bounded what one sweep armed, which was right while the
sweep was the only producer. Relate units now file pairs continuously, so
a number per 24-hour tick is a queue that only grows. The fixed quantity
is hardware throughput, so the budget becomes calls per hour. Units sort
behind capture work: a large ingest may delay hygiene, not the reverse.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 17: Teach the evaluation harness about merges

**Files:**
- Modify: `tests/eval.rs`, `src/eval/metrics.rs`
- Test: `tests/eval.rs`

**Interfaces:**
- Consumes: `roots_of`, `Chunk.superseded_by`.
- Produces: a graded pair whose target artifact has since been superseded by a merge scores as a hit on the merged artifact.

**Why.** Merging changes what is retrievable at all. Without this the score collapses for a reason that says nothing about retrieval, and the number that would tell us whether the feature helps becomes unreadable.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn a_graded_artifact_that_was_merged_still_counts_as_a_hit() {
    // A judged pair names the artifact that answered the query. When a merge
    // supersedes it, the knowledge lives in the merged artifact and search
    // correctly returns that instead. Scoring it as a miss would report a
    // retrieval regression that did not happen.
    let core = test_core().await;
    let ids = seed(&core, &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])]).await;
    let draft = MergedDraft {
        title: None, text: "a text and b text".into(),
        category: None, tags: vec![], caveats: vec![],
    };
    let m = crate::jobs::merge::write(&core, &draft, &ids).await.unwrap();
    engram::jobs::embed::run(&core, &m.id).await.unwrap();

    // The graded pair names ids[0], which is now superseded by m.
    assert!(
        resolve_expected(&core, &ids[0]).await.unwrap().contains(&m.id),
        "a merged artifact does not satisfy a grade against its root"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test eval a_graded_artifact_that_was_merged_still_counts_as_a_hit`
Expected: FAIL — `resolve_expected` does not exist.

- [ ] **Step 3: Implement**

Add to the eval harness:

```rust
/// The artifact ids that satisfy a grade naming `expected`.
///
/// Itself, plus anything that superseded it. A merge moves the knowledge into a
/// new artifact and search correctly returns that one; without this the harness
/// reports a retrieval regression that is really a bookkeeping change.
async fn resolve_expected(core: &Core, expected: &str) -> Result<Vec<String>> {
    let mut out = vec![expected.to_string()];
    let mut cursor = expected.to_string();
    // Chains are short by construction — a merge supersedes onto a live
    // artifact — but bound it anyway rather than trusting that.
    for _ in 0..8 {
        let c = core.store.get_artifact(&cursor).await?;
        match c.superseded_by {
            Some(next) => {
                out.push(next.clone());
                cursor = next;
            }
            None => break,
        }
    }
    Ok(out)
}
```

Use it wherever the harness compares a returned artifact id against a graded expectation.

- [ ] **Step 4: Run the harness and record the delta**

Run: `cargo test --test eval`
Expected: PASS. Record the scores before and after a merge run over the same corpus in the commit message. A drop is a finding about the feature, not about the harness.

- [ ] **Step 5: Commit**

```bash
git add tests/eval.rs src/eval/
git commit -m "test(eval): a merged artifact satisfies a grade against its root

Merging changes what is retrievable at all. Without this the score
collapses for a bookkeeping reason and the one number that says whether
the feature helps becomes unreadable.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 18: Turn it on

**Files:**
- Modify: `src/config.rs` (`autonomous` default), `config.example.toml`, `README.md`, `ROADMAP.md`, `src/store/schema.sql:51-53`, `src/jobs/consolidate.rs:1-13`

**Why last.** The gap between Task 12 (`autonomous = false`, verdicts recorded) and this task is the cheapest evidence available about whether the contract holds on real data. Do not close it in the same sitting.

- [ ] **Step 1: Read the recorded verdicts**

Run the instance with `autonomous = false` over a real corpus. On Ops, read the `Contradiction` rows whose detail begins `would merge` — those are the merges that would have been written. Confirm the subject rule held: no two artifacts about different subjects proposed as duplicates.

- [ ] **Step 2: Flip the default**

In `src/config.rs`, `autonomous: true`. In `config.example.toml`:

```toml
# Let consolidation settle duplicate groups by itself: superseding where one
# stored artifact plainly replaces another, and writing one merged artifact
# where each side carries something the other lacks. On by default.
#
# A disagreement about a value is never settled this way. It is escalated to
# the conflicts queue on Ops, because deciding which of two facts is current is
# the judgement a model is worst at.
#
# Every merge is undoable, and no merge is written that would drop a number,
# version, date, path, command or error string that appeared in any source.
autonomous = true
```

- [ ] **Step 3: Correct the four places that state the old invariant**

- `src/store/schema.sql:51-53`: replace "nothing is ever merged or rewritten in place" with a sentence that says a merged artifact is a distinct `provenance` kind with lineage rows, and that a captured artifact's text is still never rewritten in place.
- `src/jobs/consolidate.rs:1-13`: rewrite the module header to describe the four outcomes and name the two verification passes.
- `ROADMAP.md:23-24`: keep the fidelity principle and add the narrow exception with its four conditions, referencing the spec.
- `README.md:21` and the config table: same, plus the new keys.

- [ ] **Step 4: Run the full suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(consolidate): autonomous duplicate hygiene on by default

Four places said nothing is ever merged. They now say what is: merging is
permitted narrowly, and the four conditions that make it safe are part of
the fidelity thesis rather than exceptions to it -- superseding is
preferred wherever a stored original suffices, merged artifacts are a
distinct provenance kind with explicit lineage, originals are hidden and
never destroyed, and no merge may drop a value or a literal. A value
conflict is still a person's decision.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## Self-review notes

Checked against the spec, section by section.

- §3's four mechanisms: Tasks 12 (`replaced` preferred), 1+2 (provenance and lineage), 13 (`supersede`, never delete), 11 (`losses`).
- §4.1's `lifecycle_dirty` lands in Task 1's schema but is only *used* in Task 7 — deliberate, so the column exists before the one database recreation.
- §6.1's four verdicts: Task 9 parses them, Task 12 applies three, Task 13 the fourth.
- §8.3's `orphaned_source` needs a `source_count` column, which §4 did not name. Added in Task 14 with the reason: without it "missing a source" cannot be distinguished from "had two sources".
- **Found during self-review and fixed in Task 13:** the spec's §6.6 re-merge rule flattens a component to captured roots, and §8's write path supersedes those roots — which leaves an *earlier merged artifact* in the component active. It is then near-identical to the new merge, so the relate unit re-pairs them every sweep and the two churn indefinitely. `subsumed_merges` closes it from data already stored, so it survives a crash: a merge written from everything an earlier merge was made of subsumes it.
- **Known gap, deliberate:** a corpus filter does not match merged artifacts, because `VectorPayload.corpus_id` carries `""` for them. Making it optional ripples through every search filter for little gain, and an artifact belonging to no corpus arguably should not match a corpus filter. If it turns out to matter, the fix is a `root_corpus_ids` payload array — not in this plan.
- Type consistency: `Provenance`, `NewMerged`, `MergedDraft`, `Relation`, `Dedupe`, `Settlement`, `Verdict`, `losses`, `write`, `finish`, `undo`, `flag_orphans`, `classify_pair`, `open_component`, `roots_of`, `merged_with_active_roots` are each defined in exactly one task and referenced by name thereafter.
