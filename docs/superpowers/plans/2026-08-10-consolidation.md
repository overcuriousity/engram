# Consolidation and Hygiene Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop near-duplicate and stale artifacts from competing for the same queries, without adding an LLM call to the ingest path and without ever replacing stored artifact text with synthetic text.

**Architecture:** Three layers, cheapest first. (1) At capture, a shingle signature over the raw corpus catches a re-pasted document whose bytes changed slightly; the corpus is stored but parked in `needs_review` and synthesis is never paid for until a human chooses replace/keep/discard. (2) A periodic `Stage::Consolidate` job asks Qdrant's distance-matrix API for near pairs; pairs at or above `auto_supersede` mark the older artifact `superseded_by` the newer and hide it from search by a payload flag, pairs in the review band land in an `artifact_pairs` queue on Ops. (3) Only pairs in the review band whose fact-shaped tokens actually disagree — a zero-inference regex prefilter — are sent to the completer as a yes/no contradiction judge, capped per sweep. Separately, the existing synthesis call is asked for `caveats` alongside the artifact it already produces, which costs output tokens on a call already being made and no new call.

**Tech Stack:** Rust 1.94+, sqlx 0.9 (SQLite), axum, askama templates, Qdrant REST, `async_trait`, `tokio`.

## Global Constraints

- Rust edition and toolchain as already configured; do not touch `Cargo.toml` version floors. New dependencies are **not permitted** in this plan — everything here is standard library plus crates already in `Cargo.lock` (`serde`, `serde_json`, `sha2`, `hex`, `uuid`, `tracing`, `sqlx`, `axum`, `askama`, `async-trait`).
- **SQLite is the source of truth. Qdrant is derived.** Every consolidation decision is written to SQLite first and mirrored to Qdrant second. Never mutate a Qdrant point without a corresponding artifact row change.
- **Never rewrite artifact text with model output.** No merge, no summary, no regeneration of a stored artifact's `text`. Consolidation only ever sets `superseded_by`, writes a payload flag, or files a review row.
- **Never delete on a similarity signal.** Superseding is reversible; deletion is not.
- **No new inference call on the ingest path.** Capture stays instant and survives a dead endpoint.
- Existing artifact fields are additive-only: `superseded_by` and `caveats` are new nullable/defaulted columns; nothing existing changes type or meaning.
- Comment density and prose style must match the surrounding files: comments explain *why*, in full sentences, and name the failure mode being prevented.
- Every task ends with `cargo test` green and a commit. `cargo clippy --all-targets -- -D warnings` must pass before the final task's commit.

---

## File Structure

**Created:**
- `migrations/0009_consolidation.sql` — the schema for everything below.
- `src/store/shingle.rs` — pure bottom-k shingle signature and Jaccard estimate. No I/O, no DB.
- `src/store/pairs.rs` — `artifact_pairs` CRUD.
- `src/jobs/consolidate.rs` — the sweep: near pairs → supersede or queue → optional judge.
- `src/infer/facts.rs` — pure fact-token extraction and the zero-inference disagreement prefilter.

**Modified:**
- `src/store/mod.rs` — declare the two new store modules.
- `src/store/corpora.rs` — `CorpusStatus::NeedsReview`, shingle columns, near-dupe scan.
- `src/store/artifacts.rs` — `superseded_by`, `caveats` on `Chunk` and `NewArtifact`, plus writers.
- `src/store/jobs.rs` — `Stage::Consolidate`.
- `src/core/ingest.rs` — near-dupe detection at capture; resolve actions.
- `src/core/search.rs` — superseded artifacts excluded by default.
- `src/vector/mod.rs` — `superseded` on `VectorPayload`, `include_superseded` on `SearchFilter`, `NearPair`, two new trait methods.
- `src/vector/memory.rs` — brute-force implementations of the two new methods.
- `src/vector/qdrant.rs` — matrix-pairs implementation, superseded filter, payload flag write.
- `src/infer/mod.rs` — `caveats` on `ProposedArtifact`.
- `src/infer/prompt.rs` — caveats in the synthesizer contract and parser; the judge prompt and its parser.
- `src/infer/verify.rs` — literal check extended over caveats.
- `src/jobs/synthesize.rs` — carry `caveats` from proposal to row.
- `src/jobs/mod.rs` — dispatch `Stage::Consolidate`.
- `src/jobs/embed.rs` — carry `superseded` into the payload it builds.
- `src/core/background.rs` — periodic sweep enqueuer.
- `src/config.rs` — `[consolidate]` block.
- `src/web/api.rs`, `src/web/ui.rs`, `src/web/templates/ops.html`, `src/web/templates/_artifact_detail.html`, `src/web/templates/browse.html` — review queue, resolve buttons, caveats rendering.
- `config.example.toml`, `README.md`, `ROADMAP.md`.
- `src/infer/fake.rs` — a scriptable completer for judge tests.

---

## Task 1: Shingle signature and Jaccard estimate

Pure functions. No database, no network. This is the whole of the capture-time near-duplicate signal.

**Files:**
- Create: `src/store/shingle.rs`
- Modify: `src/store/mod.rs:1-5`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub const SIGNATURE_SIZE: usize = 128;`
  - `pub fn signature(text: &str) -> Vec<u64>` — bottom-k hashes of the text's word 5-grams, ascending, at most `SIGNATURE_SIZE` long.
  - `pub fn similarity(a: &[u64], b: &[u64]) -> f64` — estimated Jaccard in `0.0..=1.0`.
  - `pub fn encode(sig: &[u64]) -> String` / `pub fn decode(s: &str) -> Vec<u64>` — JSON round trip for the SQLite column.

- [ ] **Step 1: Write the failing test**

Create `src/store/shingle.rs` containing only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn doc(n: usize) -> String {
        (0..n)
            .map(|i| format!("line {i} of a reference document about filesystems"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn identical_text_is_perfectly_similar() {
        let a = signature(&doc(200));
        let b = signature(&doc(200));
        assert_eq!(a, b, "the signature must be deterministic");
        assert!((similarity(&a, &b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn one_changed_byte_still_reads_as_the_same_document() {
        // The case `content_hash` misses entirely: the same chapter pasted a
        // year later with one typo fixed. It must not become a second corpus
        // competing for the same queries.
        let original = doc(200);
        let edited = original.replacen("filesystems", "filesystem", 1);
        assert_ne!(original, edited);
        let s = similarity(&signature(&original), &signature(&edited));
        assert!(s > 0.95, "one edit dropped similarity to {s}");
    }

    #[test]
    fn unrelated_text_is_not_similar() {
        let a = signature(&doc(200));
        let b = signature(
            &(0..200)
                .map(|i| format!("entirely different sentence number {i} concerning pastry"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let s = similarity(&a, &b);
        assert!(s < 0.2, "unrelated documents scored {s}");
    }

    #[test]
    fn a_document_shorter_than_one_shingle_still_has_a_signature() {
        // A three-word capture is legitimate and must not panic or produce a
        // signature that matches everything.
        let sig = signature("just three words");
        assert!(!sig.is_empty());
        assert!(similarity(&sig, &signature("wholly other words here")) < 1.0);
    }

    #[test]
    fn an_empty_signature_is_never_similar_to_anything() {
        assert_eq!(similarity(&[], &signature("some text")), 0.0);
        assert_eq!(similarity(&[], &[]), 0.0);
    }

    #[test]
    fn signatures_round_trip_through_the_column_encoding() {
        let sig = signature(&doc(50));
        assert_eq!(decode(&encode(&sig)), sig);
        assert!(decode("not json").is_empty(), "a corrupt column must not panic");
    }

    #[test]
    fn the_signature_is_bounded_however_long_the_document() {
        assert!(signature(&doc(10_000)).len() <= SIGNATURE_SIZE);
    }
}
```

Add the module declaration to `src/store/mod.rs`, keeping the list alphabetical:

```rust
pub mod artifacts;
pub mod auth;
pub mod corpora;
pub mod jobs;
pub mod pairs;
pub mod segments;
pub mod shingle;
```

Note `pairs` is declared here too — Task 8 creates that file. To keep this task compiling on its own, add `pub mod pairs;` in Task 8 instead and only add `pub mod shingle;` now.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib store::shingle`
Expected: FAIL — compile errors, `cannot find function 'signature' in this scope`.

- [ ] **Step 3: Write minimal implementation**

Prepend to `src/store/shingle.rs`, above the test module:

```rust
//! Is this the same document we already have?
//!
//! `content_hash` answers that for byte-identical text and nothing else, so
//! re-pasting a chapter a year later with one typo fixed stores it a second
//! time, and the two copies then compete for the same queries forever. That is
//! the largest single source of duplication in the base, and it is detectable
//! without an embedding call, let alone a model call.
//!
//! The signature is bottom-k MinHash over word 5-grams: hash every shingle,
//! keep the `SIGNATURE_SIZE` smallest hashes. The fraction of a pair of
//! signatures' bottom-k that agree estimates the Jaccard similarity of the two
//! shingle sets, which is a number an operator can be shown — "94% of this
//! document's phrasing is already stored" — in a way a Hamming distance is not.

use std::collections::BTreeSet;

/// Hashes kept per document. Estimation error is roughly `1/sqrt(k)`, so 128
/// gives about nine points of slack — ample against a 0.90 decision threshold,
/// and small enough that the column stays a couple of kilobytes.
pub const SIGNATURE_SIZE: usize = 128;

/// Words per shingle. Five is long enough that ordinary English collides
/// rarely and short enough that a document has plenty of them.
const SHINGLE_WORDS: usize = 5;

/// FNV-1a. Chosen over `sha2`, which is already a dependency, because this runs
/// once per shingle over a whole document and the signature never leaves the
/// machine — there is nothing here for a cryptographic hash to defend.
fn hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// The bottom-k hashes of this text's word shingles, ascending.
///
/// Whitespace is normalised and case folded first, so reflowing a paragraph or
/// changing a heading's capitalisation does not read as a different document.
pub fn signature(text: &str) -> Vec<u64> {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .collect();
    if words.is_empty() {
        return Vec::new();
    }

    // A BTreeSet both dedupes — a repeated shingle must count once, or a
    // document that says the same sentence twice would skew its own bottom-k —
    // and keeps the smallest hashes reachable in order.
    let mut smallest: BTreeSet<u64> = BTreeSet::new();
    // A document shorter than one shingle still gets a signature, from the one
    // shingle its whole text forms. Returning nothing would make it match
    // every other short capture.
    let step = SHINGLE_WORDS.min(words.len());
    for w in words.windows(step) {
        smallest.insert(hash(&w.join(" ")));
        if smallest.len() > SIGNATURE_SIZE {
            let largest = *smallest.iter().next_back().expect("non-empty");
            smallest.remove(&largest);
        }
    }
    smallest.into_iter().collect()
}

/// Estimated Jaccard similarity of the two documents' shingle sets.
///
/// Both signatures are the bottom-k of their own set, so the estimate is the
/// agreement within the bottom-k of their union — taking the k smallest hashes
/// across both and asking how many of those appear in both.
pub fn similarity(a: &[u64], b: &[u64]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let k = SIGNATURE_SIZE.min(a.len()).min(b.len());
    let mut union: Vec<u64> = a.iter().chain(b.iter()).copied().collect();
    union.sort_unstable();
    union.dedup();
    union.truncate(k);

    let shared = union
        .iter()
        .filter(|h| a.binary_search(h).is_ok() && b.binary_search(h).is_ok())
        .count();
    shared as f64 / k as f64
}

/// The column holds JSON rather than a blob: it is a few kilobytes either way,
/// and a readable column is worth a great deal the first time someone has to
/// work out why two documents were called duplicates.
pub fn encode(sig: &[u64]) -> String {
    serde_json::to_string(sig).unwrap_or_else(|_| "[]".into())
}

/// A column written by an older version, or corrupted, yields no signature
/// rather than an error: the corpus simply is not compared, which is exactly
/// the behaviour before this existed.
pub fn decode(s: &str) -> Vec<u64> {
    serde_json::from_str(s).unwrap_or_default()
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib store::shingle`
Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
git add src/store/shingle.rs src/store/mod.rs
git commit -m "feat: shingle signatures for near-duplicate corpora"
```

---

## Task 2: Schema

Everything the rest of the plan writes to. One migration, so a half-applied consolidation schema is not a state that exists.

**Files:**
- Create: `migrations/0009_consolidation.sql`
- Test: `src/store/corpora.rs` (test module, appended)

**Interfaces:**
- Consumes: nothing.
- Produces: columns `corpora.shingles`, `corpora.near_dupe_of`, `corpora.near_dupe_score`, `artifacts.superseded_by`, `artifacts.caveats`; table `artifact_pairs(id, a_id, b_id, score, state, detail, created_at)`.

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `src/store/corpora.rs`:

```rust
    #[tokio::test]
    async fn the_consolidation_schema_is_present() {
        // Migrations run on connect, so this failing means 0009 did not apply.
        let s = Store::memory().await.unwrap();
        for sql in [
            "SELECT shingles, near_dupe_of, near_dupe_score FROM corpora LIMIT 1",
            "SELECT superseded_by, caveats FROM artifacts LIMIT 1",
            "SELECT id, a_id, b_id, score, state, detail, created_at FROM artifact_pairs LIMIT 1",
        ] {
            sqlx::query(sql)
                .fetch_optional(&s.pool)
                .await
                .unwrap_or_else(|e| panic!("{sql} failed: {e}"));
        }
    }

    #[tokio::test]
    async fn a_pair_is_recorded_once_whichever_order_it_is_found_in() {
        // The sweep sees (a,b) on one run and (b,a) on the next. Without a
        // canonical order the review queue fills with the same pair twice.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("x", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        ordinal: 0,
                        text: "one".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "two".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    },
                ],
            )
            .await
            .unwrap();

        let insert = |a: &str, b: &str| {
            let (a, b) = (a.to_string(), b.to_string());
            let pool = s.pool.clone();
            async move {
                sqlx::query(
                    "INSERT OR IGNORE INTO artifact_pairs (a_id, b_id, score, state, created_at)
                     VALUES (?, ?, 0.9, 'pending', 0)",
                )
                .bind(&a)
                .bind(&b)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        insert(&made[0].id, &made[1].id).await;
        insert(&made[0].id, &made[1].id).await;

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifact_pairs")
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(n, 1, "the unique constraint on (a_id, b_id) is missing");
    }
```

The `caveats: vec![]` field on `NewArtifact` does not exist yet — Task 12 adds it. Until then, **omit `caveats` from these two literals**; add it back as part of Task 12's compile fix.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib store::corpora::tests::the_consolidation_schema_is_present`
Expected: FAIL with `no such column: shingles`.

- [ ] **Step 3: Write minimal implementation**

Create `migrations/0009_consolidation.sql`:

```sql
-- Near-duplicate detection at capture.
--
-- `content_hash` is exact, so the same chapter re-pasted with one changed byte
-- becomes a second corpus, and the two then compete for the same queries for
-- as long as the base exists. The signature is a bottom-k MinHash over word
-- shingles (`src/store/shingle.rs`), compared against every other corpus at
-- capture time — a scan, because a single-operator base holds hundreds of
-- corpora, not millions.
ALTER TABLE corpora ADD COLUMN shingles TEXT;
-- Set when capture found a near-identical corpus. The capture is stored
-- regardless and parked in `needs_review`: nothing is ever discarded on a
-- similarity score, and synthesis is not paid for until a human decides.
ALTER TABLE corpora ADD COLUMN near_dupe_of TEXT;
ALTER TABLE corpora ADD COLUMN near_dupe_score REAL;

-- Consolidation, artifact side.
--
-- The artifact this one lost to. Set by the sweep when two artifacts are near
-- identical; the loser stays stored, readable and reversible, and is hidden
-- from search by a payload flag rather than deleted. A merged rewrite would
-- put synthetic text where a stored artifact used to be, which is the one
-- failure mode this design exists to avoid.
ALTER TABLE artifacts ADD COLUMN superseded_by TEXT;
CREATE INDEX idx_artifacts_superseded ON artifacts(superseded_by);

-- Conditions under which the artifact does not apply, as stated by the source.
-- Emitted by the same synthesis call that produces the artifact, so it costs
-- output tokens rather than another call.
ALTER TABLE artifacts ADD COLUMN caveats TEXT NOT NULL DEFAULT '[]';

-- The review queue.
--
-- Pairs similar enough to be worth a person's attention but not similar enough
-- to supersede automatically. `state` is 'pending' until something resolves
-- it: 'no_conflict' when the fact-token prefilter or the judge clears it,
-- 'contradiction' when the judge finds one, 'dismissed' when an operator does.
-- `a_id` < `b_id` by string order, so the same pair found in either direction
-- is one row.
CREATE TABLE artifact_pairs (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  a_id       TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  b_id       TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  score      REAL NOT NULL,
  state      TEXT NOT NULL DEFAULT 'pending',
  detail     TEXT,
  created_at INTEGER NOT NULL,
  UNIQUE(a_id, b_id)
);
CREATE INDEX idx_pairs_state ON artifact_pairs(state, created_at DESC);
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib store::corpora`
Expected: PASS. Then `cargo test` to confirm no existing test broke.

- [ ] **Step 5: Commit**

```bash
git add migrations/0009_consolidation.sql src/store/corpora.rs
git commit -m "feat: schema for consolidation and capture near-dupe review"
```

---

## Task 3: Corpus store — signature column, `needs_review`, near-dupe scan

**Files:**
- Modify: `src/store/corpora.rs:6-41` (status enum), `:43-56` (struct), `:62-74` (row mapper), `:77-109` (insert), and the `impl Store` block.

**Interfaces:**
- Consumes: `shingle::{signature, similarity, encode, decode}` from Task 1.
- Produces:
  - `CorpusStatus::NeedsReview` ⇄ `"needs_review"`.
  - `Corpus { shingles: Vec<u64>, near_dupe_of: Option<String>, near_dupe_score: Option<f64>, .. }`
  - `Store::insert_corpus` unchanged in signature; it now computes and stores the signature.
  - `pub struct NearDuplicate { pub corpus_id: String, pub title_hint: Option<String>, pub similarity: f64 }`
  - `pub async fn find_near_duplicate(&self, sig: &[u64], min: f64) -> Result<Option<NearDuplicate>>`
  - `pub async fn set_near_dupe(&self, corpus_id: &str, of: Option<&str>, score: Option<f64>) -> Result<()>`

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `src/store/corpora.rs`:

```rust
    #[tokio::test]
    async fn a_stored_corpus_carries_its_signature() {
        let s = Store::memory().await.unwrap();
        let src = s
            .insert_corpus("a document about mounting filesystems", "web", None)
            .await
            .unwrap();
        assert!(!src.shingles.is_empty());
        assert_eq!(s.get_corpus(&src.id).await.unwrap().shingles, src.shingles);
    }

    #[tokio::test]
    async fn a_near_identical_corpus_is_found_by_signature() {
        let s = Store::memory().await.unwrap();
        let body: String = (0..200)
            .map(|i| format!("step {i}: run the command and read the output"))
            .collect::<Vec<_>>()
            .join("\n");
        let first = s.insert_corpus(&body, "web", Some("manual")).await.unwrap();

        let edited = body.replacen("step 7", "step seven", 1);
        let hit = s
            .find_near_duplicate(&crate::store::shingle::signature(&edited), 0.90)
            .await
            .unwrap()
            .expect("the edited copy should have matched");
        assert_eq!(hit.corpus_id, first.id);
        assert_eq!(hit.title_hint.as_deref(), Some("manual"));
        assert!(hit.similarity > 0.90);
    }

    #[tokio::test]
    async fn an_unrelated_corpus_is_not_a_near_duplicate() {
        let s = Store::memory().await.unwrap();
        s.insert_corpus("a chapter about filesystems and mounting", "web", None)
            .await
            .unwrap();
        let other = crate::store::shingle::signature("a recipe for shortcrust pastry and jam");
        assert!(s.find_near_duplicate(&other, 0.90).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn needs_review_survives_a_round_trip() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("x", "web", None).await.unwrap();
        s.set_corpus_status(&src.id, CorpusStatus::NeedsReview)
            .await
            .unwrap();
        s.set_near_dupe(&src.id, Some("other-id"), Some(0.94))
            .await
            .unwrap();
        let got = s.get_corpus(&src.id).await.unwrap();
        assert_eq!(got.status, CorpusStatus::NeedsReview);
        assert_eq!(got.near_dupe_of.as_deref(), Some("other-id"));
        assert!((got.near_dupe_score.unwrap() - 0.94).abs() < 1e-9);

        // Clearing it is what "keep both" does, and it must actually clear.
        s.set_near_dupe(&src.id, None, None).await.unwrap();
        assert!(s.get_corpus(&src.id).await.unwrap().near_dupe_of.is_none());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib store::corpora`
Expected: FAIL — `no variant named NeedsReview`, `no field shingles`, `no method find_near_duplicate`.

- [ ] **Step 3: Write minimal implementation**

In `src/store/corpora.rs`, add the variant to `CorpusStatus` (enum body, `as_str`, and `parse`):

```rust
pub enum CorpusStatus {
    Raw,
    /// Captured, stored, and deliberately not queued for synthesis: something
    /// near-identical is already in the base, and segmenting it would pay a
    /// model to produce artifacts that compete with ones that already exist.
    /// An operator resolves it on Ops.
    NeedsReview,
    Segmenting,
    // ... unchanged
}
```

```rust
            CorpusStatus::NeedsReview => "needs_review",
```
```rust
            "needs_review" => CorpusStatus::NeedsReview,
```

Extend `Corpus`:

```rust
    /// Bottom-k shingle hashes of `raw_text`. Empty for corpora captured before
    /// the signature existed, which simply are not compared.
    #[serde(skip)]
    pub shingles: Vec<u64>,
    /// The corpus this one looked like at capture, and how alike they were.
    /// Both cleared when an operator chooses to keep both.
    pub near_dupe_of: Option<String>,
    pub near_dupe_score: Option<f64>,
```

Extend `row_to_corpus`:

```rust
        shingles: r
            .get::<Option<String>, _>("shingles")
            .map(|s| super::shingle::decode(&s))
            .unwrap_or_default(),
        near_dupe_of: r.get("near_dupe_of"),
        near_dupe_score: r.get("near_dupe_score"),
```

Extend `insert_corpus` — compute the signature and add it to the struct literal and the SQL:

```rust
        let shingles = super::shingle::signature(raw_text);
        let src = Corpus {
            // ... existing fields unchanged ...
            coverage: None,
            shingles,
            near_dupe_of: None,
            near_dupe_score: None,
        };
        sqlx::query(
            "INSERT INTO corpora (id, raw_text, origin, title_hint, content_hash, status, created_at, updated_at, shingles)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        // ... existing binds unchanged ...
        .bind(super::shingle::encode(&src.shingles))
        .execute(&self.pool)
        .await?;
```

Add to the `impl Store` block:

```rust
/// A stored corpus that a new capture looks like.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NearDuplicate {
    pub corpus_id: String,
    pub title_hint: Option<String>,
    pub similarity: f64,
}
```

```rust
    /// The stored corpus most like this signature, if any clears `min`.
    ///
    /// A full scan of the signature column. A single-operator base holds
    /// hundreds of corpora, each with a signature of a couple of kilobytes, so
    /// this is a few milliseconds of memory bandwidth on a path that already
    /// writes the whole document to disk. An index over MinHash bands is the
    /// answer at a scale this design does not target.
    pub async fn find_near_duplicate(
        &self,
        sig: &[u64],
        min: f64,
    ) -> Result<Option<NearDuplicate>> {
        if sig.is_empty() {
            return Ok(None);
        }
        let rows = sqlx::query(
            "SELECT id, title_hint, shingles FROM corpora WHERE shingles IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut best: Option<NearDuplicate> = None;
        for r in &rows {
            let stored: String = r.get("shingles");
            let s = super::shingle::similarity(sig, &super::shingle::decode(&stored));
            if s < min {
                continue;
            }
            if best.as_ref().is_none_or(|b| s > b.similarity) {
                best = Some(NearDuplicate {
                    corpus_id: r.get("id"),
                    title_hint: r.get("title_hint"),
                    similarity: s,
                });
            }
        }
        Ok(best)
    }

    pub async fn set_near_dupe(
        &self,
        corpus_id: &str,
        of: Option<&str>,
        score: Option<f64>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE corpora SET near_dupe_of = ?, near_dupe_score = ?, updated_at = ? WHERE id = ?",
        )
        .bind(of)
        .bind(score)
        .bind(now())
        .bind(corpus_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib store::corpora`
Expected: PASS. Then `cargo test` — `Corpus` gained fields, so any struct literal elsewhere must be updated; there should be none outside this file.

- [ ] **Step 5: Commit**

```bash
git add src/store/corpora.rs
git commit -m "feat: store corpus shingle signatures and near-dupe state"
```

---

## Task 4: Capture parks a near-duplicate instead of synthesising it

**Files:**
- Modify: `src/core/ingest.rs:6-46` (outcome + `ingest`)
- Modify: `src/config.rs` (new `[consolidate]` block)

**Interfaces:**
- Consumes: `Store::find_near_duplicate`, `Store::set_near_dupe`, `CorpusStatus::NeedsReview`, `shingle::signature`.
- Produces:
  - `IngestOutcome { id, status, duplicate, near_duplicate: Option<NearDuplicate> }`
  - `ConsolidateConfig { enabled: bool, near_dupe_min: f64, review_min: f32, auto_supersede: f32, sample: usize, per_point: usize, interval_hours: u64, judge: bool, max_judgements: usize }` on `Config` as `consolidate`.
  - `Core::resolve_near_duplicate(&self, corpus_id: &str, action: NearDupeAction) -> Result<()>` with `pub enum NearDupeAction { Replace, KeepBoth, Discard }`.

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `src/core/ingest.rs`:

```rust
    /// A body long enough to have a stable shingle signature.
    fn manual(marker: &str) -> String {
        (0..200)
            .map(|i| format!("step {i}: run the {marker} command and read its output"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn a_near_identical_capture_is_parked_rather_than_synthesised() {
        // The whole point: a re-pasted chapter must not cost a model call, and
        // must not become a second set of artifacts competing with the first.
        let core = test_core().await;
        let first = core.ingest(&manual("mount"), "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}

        let edited = manual("mount").replacen("step 7:", "step seven:", 1);
        let second = core.ingest(&edited, "web", None).await.unwrap();

        assert_ne!(second.id, first.id, "the capture must still be stored");
        assert!(!second.duplicate, "it is not a byte-identical duplicate");
        assert_eq!(second.status, CorpusStatus::NeedsReview);
        let near = second.near_duplicate.expect("no near-duplicate reported");
        assert_eq!(near.corpus_id, first.id);
        assert!(near.similarity > 0.90);
        assert!(
            core.store.claim_job().await.unwrap().is_none(),
            "a parked capture must not queue synthesis"
        );
    }

    #[tokio::test]
    async fn an_ordinary_capture_is_unaffected() {
        let core = test_core().await;
        core.ingest(&manual("mount"), "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}

        let out = core.ingest(&manual("pastry"), "web", None).await.unwrap();
        assert_eq!(out.status, CorpusStatus::Raw);
        assert!(out.near_duplicate.is_none());
        assert!(
            core.store.claim_job().await.unwrap().is_some(),
            "an unrelated capture must still queue synthesis"
        );
    }

    #[tokio::test]
    async fn keeping_both_releases_the_capture_into_the_pipeline() {
        let core = test_core().await;
        core.ingest(&manual("mount"), "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}
        let second = core
            .ingest(&manual("mount").replacen("step 7:", "step seven:", 1), "web", None)
            .await
            .unwrap();

        core.resolve_near_duplicate(&second.id, crate::core::ingest::NearDupeAction::KeepBoth)
            .await
            .unwrap();

        let got = core.store.get_corpus(&second.id).await.unwrap();
        assert_eq!(got.status, CorpusStatus::Raw);
        assert!(got.near_dupe_of.is_none(), "the flag must be cleared");
        assert!(core.store.claim_job().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn replacing_deletes_the_older_corpus_and_its_vectors() {
        let core = test_core().await;
        let first = core.ingest(&manual("mount"), "web", None).await.unwrap();
        while crate::jobs::run_one(&core).await.unwrap() {}
        assert!(core.vectors.count().await.unwrap() > 0);

        let second = core
            .ingest(&manual("mount").replacen("step 7:", "step seven:", 1), "web", None)
            .await
            .unwrap();
        core.resolve_near_duplicate(&second.id, crate::core::ingest::NearDupeAction::Replace)
            .await
            .unwrap();

        assert!(matches!(
            core.store.get_corpus(&first.id).await,
            Err(crate::error::Error::NotFound)
        ));
        assert_eq!(
            core.store.get_corpus(&second.id).await.unwrap().status,
            CorpusStatus::Raw
        );
        assert!(core.store.claim_job().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn discarding_removes_the_new_capture_only() {
        let core = test_core().await;
        let first = core.ingest(&manual("mount"), "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}
        let second = core
            .ingest(&manual("mount").replacen("step 7:", "step seven:", 1), "web", None)
            .await
            .unwrap();

        core.resolve_near_duplicate(&second.id, crate::core::ingest::NearDupeAction::Discard)
            .await
            .unwrap();

        assert!(matches!(
            core.store.get_corpus(&second.id).await,
            Err(crate::error::Error::NotFound)
        ));
        assert!(core.store.get_corpus(&first.id).await.is_ok());
    }

    #[tokio::test]
    async fn resolving_a_corpus_that_is_not_parked_is_rejected() {
        let core = test_core().await;
        let out = core.ingest("ordinary text", "web", None).await.unwrap();
        assert!(matches!(
            core.resolve_near_duplicate(&out.id, crate::core::ingest::NearDupeAction::Replace)
                .await,
            Err(crate::error::Error::Validation(_))
        ));
    }
```

Add `use crate::core::ingest::NearDupeAction;`-adjacent imports as the compiler asks. `test_core()` must expose the threshold; see Step 3 — `Core` gains a `consolidate: ConsolidateConfig` field, and `test_support::build` sets `ConsolidateConfig::default()`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib core::ingest`
Expected: FAIL — `no field near_duplicate`, `no method resolve_near_duplicate`.

- [ ] **Step 3: Write minimal implementation**

In `src/config.rs`, add the block and hang it off `Config`:

```rust
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConsolidateConfig {
    /// Whether the background sweep runs at all. Capture-time near-duplicate
    /// detection is separate and always on: it costs a hash, not a query.
    pub enabled: bool,
    /// Estimated Jaccard over word shingles above which a capture is parked as
    /// a near-duplicate of an existing corpus.
    pub near_dupe_min: f64,
    /// Cosine at or above which a pair is worth an operator's attention.
    pub review_min: f32,
    /// Cosine at or above which the older artifact is superseded without
    /// asking. Deliberately far above `review_min`: two genuinely distinct
    /// artifacts about one subsystem sit around 0.88 routinely, and superseding
    /// at that score destroys knowledge rather than duplication.
    pub auto_supersede: f32,
    /// Points sampled from the collection per sweep by the matrix API.
    pub sample: usize,
    /// Neighbours considered per sampled point.
    pub per_point: usize,
    /// How often the sweep is queued.
    pub interval_hours: u64,
    /// Whether pairs in the review band that survive the fact-token prefilter
    /// are sent to the completer. Off by default: it is the only part of
    /// consolidation that costs inference.
    pub judge: bool,
    /// Ceiling on judge calls per sweep, so one sweep cannot occupy the GPU.
    pub max_judgements: usize,
}

impl Default for ConsolidateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            near_dupe_min: 0.90,
            review_min: 0.88,
            auto_supersede: 0.95,
            sample: 2000,
            per_point: 5,
            interval_hours: 24,
            judge: false,
            max_judgements: 20,
        }
    }
}
```

Add `#[serde(default)] pub consolidate: ConsolidateConfig,` to `Config`, and `pub consolidate: ConsolidateConfig,` to `Core`, set from `cfg.consolidate.clone()` in `from_config` and from `ConsolidateConfig::default()` in `test_support::build`.

Rewrite `src/core/ingest.rs`'s outcome and `ingest`:

```rust
use crate::store::corpora::{CorpusStatus, NearDuplicate, content_hash};

#[derive(Debug, Clone, serde::Serialize)]
pub struct IngestOutcome {
    pub id: String,
    pub status: CorpusStatus,
    /// True when the text was already stored byte for byte and no new source
    /// was created.
    pub duplicate: bool,
    /// Set when the text is not identical to anything stored but is close
    /// enough that segmenting it would produce artifacts competing with ones
    /// that already exist. The capture is stored and parked, never dropped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub near_duplicate: Option<NearDuplicate>,
}

/// What an operator decided about a parked capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NearDupeAction {
    /// The new capture is the better copy: delete the old corpus and its
    /// artifacts, then process this one.
    Replace,
    /// They are genuinely different despite the score. Process this one and
    /// leave the other alone.
    KeepBoth,
    /// The new capture adds nothing. Delete it.
    Discard,
}
```

```rust
    pub async fn ingest(
        &self,
        text: &str,
        origin: &str,
        title_hint: Option<&str>,
    ) -> Result<IngestOutcome> {
        if text.trim().is_empty() {
            return Err(Error::Validation("text is empty".into()));
        }

        if let Some(existing) = self.store.find_by_hash(&content_hash(text)).await? {
            tracing::info!(corpus_id = %existing.id, "duplicate ingest, returning existing source");
            return Ok(IngestOutcome {
                id: existing.id,
                status: existing.status,
                duplicate: true,
                near_duplicate: None,
            });
        }

        // Computed once, before the insert, so the same signature answers "is
        // this a near-duplicate" and becomes the row's stored column.
        let sig = crate::store::shingle::signature(text);
        let near = self
            .store
            .find_near_duplicate(&sig, self.consolidate.near_dupe_min)
            .await?;

        let src = self.store.insert_corpus(text, origin, title_hint).await?;

        match &near {
            // Parked. Synthesis is the expensive stage and this text may not
            // deserve it; an operator decides on Ops. Nothing is lost either
            // way — the corpus is stored verbatim like any other.
            Some(n) => {
                self.store
                    .set_near_dupe(&src.id, Some(&n.corpus_id), Some(n.similarity))
                    .await?;
                self.store
                    .set_corpus_status(&src.id, CorpusStatus::NeedsReview)
                    .await?;
                tracing::info!(
                    corpus_id = %src.id,
                    near = %n.corpus_id,
                    similarity = n.similarity,
                    "capture looks like an existing corpus; parked for review"
                );
            }
            None => {
                self.store
                    .enqueue(Stage::Synthesize, "corpus", &src.id)
                    .await?;
                tracing::info!(corpus_id = %src.id, origin, bytes = text.len(), "ingested");
            }
        }

        Ok(IngestOutcome {
            id: src.id,
            status: if near.is_some() {
                CorpusStatus::NeedsReview
            } else {
                CorpusStatus::Raw
            },
            duplicate: false,
            near_duplicate: near,
        })
    }

    /// Act on a parked capture. Every branch ends with a corpus that is either
    /// in the pipeline or gone; none of them leaves a corpus stuck in
    /// `needs_review` with no way out.
    pub async fn resolve_near_duplicate(
        &self,
        corpus_id: &str,
        action: NearDupeAction,
    ) -> Result<()> {
        let src = self.store.get_corpus(corpus_id).await?;
        let Some(other) = src.near_dupe_of.clone() else {
            return Err(Error::Validation(
                "this corpus is not parked as a near-duplicate".into(),
            ));
        };

        match action {
            NearDupeAction::Discard => {
                self.delete_corpus(&src.id).await?;
                tracing::info!(corpus_id = %src.id, "discarded a near-duplicate capture");
            }
            NearDupeAction::Replace | NearDupeAction::KeepBoth => {
                if action == NearDupeAction::Replace {
                    // The older corpus goes first. If this fails the new one is
                    // still parked, which is a state an operator can retry from;
                    // releasing it first would leave both live on a failure.
                    self.delete_corpus(&other).await?;
                    tracing::info!(corpus_id = %src.id, replaced = %other, "replaced an older corpus");
                }
                self.store.set_near_dupe(&src.id, None, None).await?;
                self.store
                    .set_corpus_status(&src.id, CorpusStatus::Raw)
                    .await?;
                self.store
                    .enqueue(Stage::Synthesize, "corpus", &src.id)
                    .await?;
            }
        }
        Ok(())
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib core::ingest` then `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core/ingest.rs src/config.rs src/core/mod.rs
git commit -m "feat: park near-duplicate captures instead of synthesising them"
```

---

## Task 5: Web surface for parked captures

**Files:**
- Modify: `src/web/api.rs:391-406` (routes), `src/web/ui.rs:557-606` (ops handler), `src/web/ui.rs:829-832` (routes)
- Modify: `src/web/templates/ops.html`

**Interfaces:**
- Consumes: `Core::resolve_near_duplicate`, `NearDupeAction`, `Store::list_corpora`.
- Produces:
  - `POST /api/v1/corpora/{id}/resolve` with body `{"action":"replace"|"keep_both"|"discard"}`.
  - `POST /ui/ops/corpora/{id}/resolve` form-encoded `action=...`, redirecting to `/ui/ops`.
  - `pub async fn parked_corpora(&self, limit: i64) -> Result<Vec<Corpus>>` on `Store`.

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `src/web/api.rs`:

```rust
    #[tokio::test]
    async fn a_parked_capture_is_resolved_over_the_api() {
        let (app, token, core) = app_token_and_core().await;
        let body: String = (0..200)
            .map(|i| format!("step {i}: run the mount command and read its output"))
            .collect::<Vec<_>>()
            .join("\n");
        core.ingest(&body, "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}
        let second = core
            .ingest(&body.replacen("step 7:", "step seven:", 1), "web", None)
            .await
            .unwrap();
        assert!(second.near_duplicate.is_some());

        let res = app
            .oneshot(post_json(
                &format!("/api/v1/corpora/{}/resolve", second.id),
                &token,
                serde_json::json!({ "action": "keep_both" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        assert_eq!(
            core.store.get_corpus(&second.id).await.unwrap().status,
            crate::store::corpora::CorpusStatus::Raw
        );
    }

    #[tokio::test]
    async fn resolving_a_corpus_that_is_not_parked_is_a_bad_request() {
        let (app, token, core) = app_token_and_core().await;
        let out = core.ingest("plain text", "web", None).await.unwrap();
        let res = app
            .oneshot(post_json(
                &format!("/api/v1/corpora/{}/resolve", out.id),
                &token,
                serde_json::json!({ "action": "discard" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::BAD_REQUEST);
    }
```

Reuse the existing `post_json` helper in that module; if it is named differently, use whatever the file already provides for an authenticated JSON POST.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib web::api`
Expected: FAIL — 404, no such route.

- [ ] **Step 3: Write minimal implementation**

In `src/web/api.rs`, beside the other corpus handlers:

```rust
#[derive(serde::Deserialize)]
struct ResolveBody {
    action: crate::core::ingest::NearDupeAction,
}

/// Act on a capture parked as a near-duplicate. The decision is an operator's:
/// nothing here compares the two documents again, it only carries out what was
/// chosen.
async fn resolve_near_dupe(
    State(st): State<AppState>,
    _id: Identity,
    Path(id): Path<String>,
    Json(body): Json<ResolveBody>,
) -> Result<Json<serde_json::Value>> {
    st.core.resolve_near_duplicate(&id, body.action).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
```

Register it:

```rust
        .route("/corpora/{id}/resolve", post(resolve_near_dupe))
```

In `src/store/corpora.rs`:

```rust
    /// Captures waiting on a near-duplicate decision, newest first. They are
    /// the one corpus state nothing else advances, so Ops has to show them or
    /// they sit unprocessed with no indication why.
    pub async fn parked_corpora(&self, limit: i64) -> Result<Vec<Corpus>> {
        let rows = sqlx::query(
            "SELECT * FROM corpora WHERE near_dupe_of IS NOT NULL
              ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_corpus).collect())
    }
```

In `src/web/ui.rs`, add a row type and thread it into the ops template struct and handler:

```rust
/// A parked capture, with enough of the corpus it resembles to decide without
/// opening both.
pub struct ParkedRow {
    pub id: String,
    pub title_hint: Option<String>,
    pub bytes: usize,
    pub other_id: String,
    pub other_title: Option<String>,
    pub percent: i64,
}
```

In the `ops` handler, before building the template struct:

```rust
    let mut parked = Vec::new();
    for c in st.core.store.parked_corpora(50).await? {
        let other = c.near_dupe_of.clone().unwrap_or_default();
        let other_title = st
            .core
            .store
            .get_corpus(&other)
            .await
            .ok()
            .and_then(|o| o.title_hint);
        parked.push(ParkedRow {
            other_id: other,
            other_title,
            percent: (c.near_dupe_score.unwrap_or(0.0) * 100.0).round() as i64,
            bytes: c.raw_text.len(),
            title_hint: c.title_hint.clone(),
            id: c.id,
        });
    }
```

Add the form handler and route:

```rust
#[derive(serde::Deserialize)]
struct ResolveForm {
    action: crate::core::ingest::NearDupeAction,
}

async fn resolve_near_dupe_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(id): Path<String>,
    Form(form): Form<ResolveForm>,
) -> Result<Response> {
    st.core.resolve_near_duplicate(&id, form.action).await?;
    Ok(Redirect::to("/ui/ops").into_response())
}
```

```rust
        .route("/ui/ops/corpora/{id}/resolve", post(resolve_near_dupe_ui))
```

In `src/web/templates/ops.html`, add a section above the flagged-artifacts table, matching the file's existing markup conventions:

```html
{% if !parked.is_empty() %}
<section>
  <h2>Captures waiting on a decision</h2>
  <p class="hint">
    These were stored but not synthesised: each looks like a document already
    in the base. Nothing is spent on them until you choose.
  </p>
  <table>
    <thead><tr><th>Capture</th><th>Looks like</th><th>Match</th><th></th></tr></thead>
    <tbody>
    {% for p in parked %}
      <tr>
        <td><a href="/ui/corpora/{{ p.id }}">{{ p.title_hint.as_deref().unwrap_or("untitled") }}</a> ({{ p.bytes }} bytes)</td>
        <td><a href="/ui/corpora/{{ p.other_id }}">{{ p.other_title.as_deref().unwrap_or("untitled") }}</a></td>
        <td>{{ p.percent }}%</td>
        <td>
          <form method="post" action="/ui/ops/corpora/{{ p.id }}/resolve">
            <button name="action" value="replace">Replace the old one</button>
            <button name="action" value="keep_both">Keep both</button>
            <button name="action" value="discard">Discard this</button>
          </form>
        </td>
      </tr>
    {% endfor %}
    </tbody>
  </table>
</section>
{% endif %}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib web`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/api.rs src/web/ui.rs src/web/templates/ops.html src/store/corpora.rs
git commit -m "feat: resolve parked captures from Ops and the API"
```

---

## Task 6: Superseded artifacts are hidden from search

The flag first, then the sweep that sets it. Doing it this way round means the sweep is never the thing under test when the filter is wrong.

**Files:**
- Modify: `src/vector/mod.rs:8-44` (payload, filter), `:69-110` (trait)
- Modify: `src/vector/memory.rs:109-140` (search), and add `set_superseded`
- Modify: `src/vector/qdrant.rs` (`build_filter`, and add `set_superseded`)
- Modify: `src/jobs/embed.rs:365-379` (`payload_of`)
- Modify: `src/store/artifacts.rs` (`Chunk.superseded_by`, `set_superseded_by`)

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `VectorPayload { superseded: Option<bool>, .. }` — `skip_serializing_if = "Option::is_none"`, so an absent value means "leave what is stored alone", exactly like `last_seen_at`.
  - `SearchFilter { include_superseded: bool, .. }` — `false` by default.
  - `VectorStore::set_superseded(&self, artifact_id: &str, superseded: bool) -> Result<()>`
  - `Chunk { superseded_by: Option<String>, .. }`
  - `Store::set_superseded_by(&self, artifact_id: &str, by: Option<&str>) -> Result<()>`

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `src/vector/memory.rs` (create the module if the file has none):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::VectorPayload;

    fn point(id: &str, v: f32) -> VectorPoint {
        VectorPoint {
            vector: vec![v, 1.0 - v],
            sparse: Default::default(),
            payload: VectorPayload {
                artifact_id: id.into(),
                corpus_id: "c".into(),
                text: id.into(),
                title: None,
                category: None,
                tags: vec![],
                created_at: 0,
                last_seen_at: None,
                superseded: None,
            },
        }
    }

    #[tokio::test]
    async fn a_superseded_artifact_drops_out_of_search() {
        let v = MemoryVectors::new();
        v.upsert(vec![point("a", 1.0), point("b", 0.99)]).await.unwrap();
        assert_eq!(v.search(&[1.0, 0.0], &Default::default(), 10, &Default::default()).await.unwrap().len(), 2);

        v.set_superseded("b", true).await.unwrap();
        let hits = v
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].payload.artifact_id, "a");
    }

    #[tokio::test]
    async fn a_superseded_artifact_is_still_reachable_when_asked_for() {
        // Superseding hides an artifact from ranking. It must not make it
        // unreadable: the review queue and the undo both need to see it.
        let v = MemoryVectors::new();
        v.upsert(vec![point("a", 1.0), point("b", 0.99)]).await.unwrap();
        v.set_superseded("b", true).await.unwrap();

        let filter = SearchFilter { include_superseded: true, ..Default::default() };
        assert_eq!(
            v.search(&[1.0, 0.0], &Default::default(), 10, &filter).await.unwrap().len(),
            2
        );
    }

    #[tokio::test]
    async fn un_superseding_puts_an_artifact_back_in_results() {
        let v = MemoryVectors::new();
        v.upsert(vec![point("a", 1.0)]).await.unwrap();
        v.set_superseded("a", true).await.unwrap();
        v.set_superseded("a", false).await.unwrap();
        assert_eq!(
            v.search(&[1.0, 0.0], &Default::default(), 10, &Default::default()).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn re_embedding_does_not_un_supersede_an_artifact() {
        // `payload_of` in the embed job knows nothing about consolidation, so
        // it leaves the field unset — and unset must mean "keep what is
        // stored", or every re-embed would silently revive a hidden artifact.
        let v = MemoryVectors::new();
        v.upsert(vec![point("a", 1.0)]).await.unwrap();
        v.set_superseded("a", true).await.unwrap();
        v.upsert(vec![point("a", 1.0)]).await.unwrap();
        assert!(
            v.search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
                .await
                .unwrap()
                .is_empty()
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib vector::memory`
Expected: FAIL — `no field superseded`, `no method set_superseded`.

- [ ] **Step 3: Write minimal implementation**

`src/vector/mod.rs` — extend the payload:

```rust
    /// Set when this artifact lost a near-identical pair to a newer one. Like
    /// `last_seen_at`, it is omitted when unset so that a writer which does not
    /// know the value — the embed job rebuilding a payload — leaves the stored
    /// one alone rather than reviving a hidden artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded: Option<bool>,
```

Extend the filter:

```rust
#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    pub tags: Vec<String>,
    pub category: Option<String>,
    /// Superseded artifacts are excluded by default. They are still stored and
    /// still readable by id — hiding them from ranking is the whole of what
    /// superseding does.
    pub include_superseded: bool,
}
```

`is_empty` must not report a filter as empty merely because it only excludes superseded points:

```rust
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty() && self.category.is_none() && self.include_superseded
    }
```

Add the trait method next to `set_payload`:

```rust
    /// Hide or unhide one artifact. A payload write, not a re-embed: which
    /// artifact won a near-identical pair changes nothing the embedding model
    /// saw.
    async fn set_superseded(&self, artifact_id: &str, superseded: bool) -> Result<()>;
```

`src/vector/memory.rs` — implement it and filter in `search`:

```rust
    async fn set_superseded(&self, artifact_id: &str, superseded: bool) -> Result<()> {
        let mut w = self.points.write().unwrap();
        if let Some(p) = w.get_mut(artifact_id) {
            p.payload.superseded = Some(superseded);
        }
        Ok(())
    }
```

In `search`, extend the `filter(...)` closure:

```rust
            .filter(|p| {
                (filter.include_superseded || p.payload.superseded != Some(true))
                    && filter.tags.iter().all(|t| p.payload.tags.contains(t))
                    && filter
                        .category
                        .as_ref()
                        .is_none_or(|c| p.payload.category.as_ref() == Some(c))
            })
```

In `upsert`, carry the flag forward exactly as `last_seen_at` is carried:

```rust
            if p.payload.superseded.is_none() {
                p.payload.superseded = w
                    .get(&p.payload.artifact_id)
                    .and_then(|old| old.payload.superseded);
            }
```

And in `set_payload`, the same preservation:

```rust
            let sup = payload.superseded.or(p.payload.superseded);
            p.payload = payload.clone();
            p.payload.last_seen_at = seen;
            p.payload.superseded = sup;
```

`src/vector/qdrant.rs` — in `build_filter`, add the exclusion. Because `build_filter` currently returns `Option<Value>` and may return `None`, it must now return a filter whenever superseded points are being excluded:

```rust
    // Superseded points are excluded with `must_not` rather than by matching
    // `false`: an artifact stored before consolidation existed has no
    // `superseded` key at all, and a `match: false` clause would drop every
    // one of them from search.
    if !filter.include_superseded {
        must_not.push(json!({ "key": "superseded", "match": { "value": true } }));
    }
```

Assemble the returned object from both `must` and `must_not`, returning `None` only when both are empty. Add the method:

```rust
    async fn set_superseded(&self, artifact_id: &str, superseded: bool) -> Result<()> {
        let _: Value = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/payload?wait=true", self.alias),
                Some(json!({
                    "payload": { "superseded": superseded },
                    "points": [ point_uuid(artifact_id) ],
                })),
            )
            .await?;
        Ok(())
    }
```

`src/jobs/embed.rs` — `payload_of` gains one field with the same comment shape as `last_seen_at`:

```rust
        // Unset for the same reason as `last_seen_at`: this job knows nothing
        // about consolidation, and writing `false` here would revive an
        // artifact the sweep hid on every re-embed.
        superseded: None,
```

`src/store/artifacts.rs` — add `pub superseded_by: Option<String>,` to `Chunk`, read it in `row_to_artifact` (`superseded_by: r.get("superseded_by"),`), set `superseded_by: None` in the `insert_artifacts` literal, and add:

```rust
    /// Record that this artifact lost a near-identical pair. `None` undoes it.
    pub async fn set_superseded_by(&self, artifact_id: &str, by: Option<&str>) -> Result<()> {
        let res = sqlx::query("UPDATE artifacts SET superseded_by = ? WHERE id = ?")
            .bind(by)
            .bind(artifact_id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Artifacts currently hidden by consolidation, newest first.
    pub async fn superseded_artifacts(&self, limit: i64) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT * FROM artifacts WHERE superseded_by IS NOT NULL
              ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_artifact).collect())
    }
```

Every `VectorPayload` literal in the test suites now needs `superseded: None`; the compiler names them all.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test`
Expected: PASS across the whole suite.

- [ ] **Step 5: Commit**

```bash
git add src/vector src/jobs/embed.rs src/store/artifacts.rs
git commit -m "feat: hide superseded artifacts from search behind a payload flag"
```

---

## Task 7: `near_pairs` on the vector store

**Files:**
- Modify: `src/vector/mod.rs` (`NearPair`, trait method)
- Modify: `src/vector/memory.rs`
- Modify: `src/vector/qdrant.rs`
- Test: `tests/` integration file for the Qdrant path if one exists; otherwise the unit test on the request body, matching how `qdrant.rs` tests its other bodies.

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `pub struct NearPair { pub a: String, pub b: String, pub score: f32 }` — `a` and `b` are artifact ids, ordered so `a < b`.
  - `VectorStore::near_pairs(&self, sample: usize, per_point: usize, min_score: f32) -> Result<Vec<NearPair>>` — best first, superseded points excluded.

- [ ] **Step 1: Write the failing test**

Append to `src/vector/memory.rs`'s test module:

```rust
    #[tokio::test]
    async fn near_pairs_finds_the_close_pair_and_not_the_far_one() {
        let v = MemoryVectors::new();
        v.upsert(vec![point("a", 1.0), point("b", 0.999), point("c", 0.0)])
            .await
            .unwrap();

        let pairs = v.near_pairs(100, 5, 0.9).await.unwrap();
        assert_eq!(pairs.len(), 1, "got {pairs:?}");
        assert_eq!((pairs[0].a.as_str(), pairs[0].b.as_str()), ("a", "b"));
        assert!(pairs[0].score >= 0.9);
    }

    #[tokio::test]
    async fn a_pair_is_reported_once_not_twice() {
        // (a,b) and (b,a) are the same pair. Reporting both doubles the review
        // queue and makes the sweep supersede an artifact twice.
        let v = MemoryVectors::new();
        v.upsert(vec![point("a", 1.0), point("b", 0.999)]).await.unwrap();
        assert_eq!(v.near_pairs(100, 5, 0.9).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_superseded_artifact_is_not_paired_again() {
        // Otherwise every sweep re-finds the pair it resolved last time and
        // the review queue never empties.
        let v = MemoryVectors::new();
        v.upsert(vec![point("a", 1.0), point("b", 0.999)]).await.unwrap();
        v.set_superseded("b", true).await.unwrap();
        assert!(v.near_pairs(100, 5, 0.9).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pairs_come_back_best_first() {
        let v = MemoryVectors::new();
        v.upsert(vec![point("a", 1.0), point("b", 0.999), point("c", 0.99)])
            .await
            .unwrap();
        let pairs = v.near_pairs(100, 5, 0.5).await.unwrap();
        for w in pairs.windows(2) {
            assert!(w[0].score >= w[1].score, "not sorted: {pairs:?}");
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib vector::memory`
Expected: FAIL — `no method near_pairs`.

- [ ] **Step 3: Write minimal implementation**

`src/vector/mod.rs`:

```rust
/// Two artifacts the index says are close, and how close.
///
/// `a` sorts before `b` so the same pair found from either end is one value —
/// the sweep would otherwise queue it twice and supersede the loser twice.
#[derive(Debug, Clone, PartialEq)]
pub struct NearPair {
    pub a: String,
    pub b: String,
    pub score: f32,
}

impl NearPair {
    pub fn new(x: &str, y: &str, score: f32) -> NearPair {
        let (a, b) = if x <= y { (x, y) } else { (y, x) };
        NearPair {
            a: a.to_string(),
            b: b.to_string(),
            score,
        }
    }
}
```

Trait method:

```rust
    /// Pairs of artifacts closer than `min_score`, best first, over a sample of
    /// the collection. Superseded artifacts are excluded — a resolved pair
    /// re-found every sweep is a review queue that never empties.
    ///
    /// This is one round trip, not one query per point: `sample` points are
    /// drawn and each contributes at most `per_point` neighbours. A sweep over
    /// a base of any size therefore costs a bounded amount rather than growing
    /// with the collection.
    async fn near_pairs(
        &self,
        sample: usize,
        per_point: usize,
        min_score: f32,
    ) -> Result<Vec<NearPair>>;
```

`src/vector/memory.rs`:

```rust
    async fn near_pairs(
        &self,
        sample: usize,
        per_point: usize,
        min_score: f32,
    ) -> Result<Vec<NearPair>> {
        let r = self.points.read().unwrap();
        let live: Vec<&VectorPoint> = r
            .values()
            .filter(|p| p.payload.superseded != Some(true))
            .take(sample)
            .collect();

        let mut out: Vec<NearPair> = Vec::new();
        for (i, a) in live.iter().enumerate() {
            let mut mine: Vec<NearPair> = live
                .iter()
                .skip(i + 1)
                .map(|b| {
                    NearPair::new(
                        &a.payload.artifact_id,
                        &b.payload.artifact_id,
                        cosine(&a.vector, &b.vector),
                    )
                })
                .filter(|p| p.score >= min_score)
                .collect();
            mine.sort_by(|x, y| y.score.total_cmp(&x.score));
            mine.truncate(per_point);
            out.extend(mine);
        }
        // Deterministic order, so a test never depends on HashMap iteration.
        out.sort_by(|x, y| {
            y.score
                .total_cmp(&x.score)
                .then_with(|| x.a.cmp(&y.a))
                .then_with(|| x.b.cmp(&y.b))
        });
        out.dedup_by(|x, y| x.a == y.a && x.b == y.b);
        Ok(out)
    }
```

`src/vector/qdrant.rs` — the matrix API, plus a uuid→artifact_id lookup because the response speaks in point ids:

```rust
#[derive(serde::Deserialize)]
struct MatrixPairs {
    pairs: Vec<MatrixPair>,
}

#[derive(serde::Deserialize)]
struct MatrixPair {
    a: Value,
    b: Value,
    score: f32,
}
```

```rust
    async fn near_pairs(
        &self,
        sample: usize,
        per_point: usize,
        min_score: f32,
    ) -> Result<Vec<NearPair>> {
        // Superseded points are excluded at the source. Including them would
        // hand the sweep pairs it has already resolved, every single run.
        let res: MatrixPairs = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/search/matrix/pairs", self.alias),
                Some(json!({
                    "sample": sample,
                    "limit": per_point,
                    "using": DENSE,
                    "filter": { "must_not": [
                        { "key": "superseded", "match": { "value": true } }
                    ] },
                })),
            )
            .await?;

        let mut ids: Vec<Value> = Vec::new();
        for p in &res.pairs {
            if p.score < min_score {
                continue;
            }
            ids.push(p.a.clone());
            ids.push(p.b.clone());
        }
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        ids.sort_by_key(|v| v.to_string());
        ids.dedup();

        // `point_uuid` is one-way, so the artifact id has to come back from the
        // payload. One retrieve for the whole sweep, asking for the single key
        // rather than dragging every candidate's text across the wire.
        let looked_up: Value = self
            .call(
                Method::POST,
                &format!("/collections/{}/points", self.alias),
                Some(json!({ "ids": ids, "with_payload": ["artifact_id"], "with_vector": false })),
            )
            .await?;
        let mut by_uuid: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if let Some(list) = looked_up.get("result").and_then(|r| r.as_array()) {
            for p in list {
                let (Some(id), Some(aid)) = (
                    p.get("id"),
                    p.get("payload")
                        .and_then(|pl| pl.get("artifact_id"))
                        .and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                by_uuid.insert(id.to_string(), aid.to_string());
            }
        }

        let mut out: Vec<NearPair> = res
            .pairs
            .iter()
            .filter(|p| p.score >= min_score)
            .filter_map(|p| {
                let a = by_uuid.get(&p.a.to_string())?;
                let b = by_uuid.get(&p.b.to_string())?;
                // A point can pair with itself across a re-embed boundary; a
                // pair of one artifact is not a duplicate of anything.
                (a != b).then(|| NearPair::new(a, b, p.score))
            })
            .collect();
        out.sort_by(|x, y| {
            y.score
                .total_cmp(&x.score)
                .then_with(|| x.a.cmp(&y.a))
                .then_with(|| x.b.cmp(&y.b))
        });
        out.dedup_by(|x, y| x.a == y.a && x.b == y.b);
        Ok(out)
    }
```

Add a unit test in `qdrant.rs`'s existing test module, in the style of the ones already there:

```rust
    #[test]
    fn matrix_pairs_deserialise_from_qdrant_shape() {
        let res: MatrixPairs = serde_json::from_value(json!({
            "pairs": [ { "a": 1, "b": 2, "score": 0.97 } ]
        }))
        .unwrap();
        assert_eq!(res.pairs.len(), 1);
        assert!((res.pairs[0].score - 0.97).abs() < 1e-6);
    }

    #[test]
    fn a_pair_is_canonically_ordered() {
        assert_eq!(NearPair::new("z", "a", 0.9), NearPair::new("a", "z", 0.9));
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib vector`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/vector
git commit -m "feat: near_pairs over the collection via the distance matrix"
```

---

## Task 8: The review queue store

**Files:**
- Create: `src/store/pairs.rs`
- Modify: `src/store/mod.rs` (add `pub mod pairs;`)

**Interfaces:**
- Consumes: the `artifact_pairs` table from Task 2.
- Produces:
  - `pub enum PairState { Pending, NoConflict, Contradiction, Dismissed }` with `as_str` / `parse`, mirroring `EmbedState`.
  - `pub struct ArtifactPair { pub id: i64, pub a_id: String, pub b_id: String, pub score: f32, pub state: PairState, pub detail: Option<String>, pub created_at: i64 }`
  - `Store::record_pair(&self, a: &str, b: &str, score: f32) -> Result<bool>` — true when a new row was written.
  - `Store::pairs_by_state(&self, state: PairState, limit: i64) -> Result<Vec<ArtifactPair>>`
  - `Store::set_pair_state(&self, id: i64, state: PairState, detail: Option<&str>) -> Result<()>`

- [ ] **Step 1: Write the failing test**

Create `src/store/pairs.rs` with only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::artifacts::NewArtifact;

    async fn two_artifacts(s: &Store) -> (String, String) {
        let src = s.insert_corpus("x", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(
                &src.id,
                &[
                    NewArtifact {
                        ordinal: 0,
                        text: "one".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                    },
                    NewArtifact {
                        ordinal: 1,
                        text: "two".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                    },
                ],
            )
            .await
            .unwrap();
        (made[0].id.clone(), made[1].id.clone())
    }

    #[tokio::test]
    async fn a_pair_is_recorded_once_and_only_once() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        assert!(s.record_pair(&a, &b, 0.91).await.unwrap());
        assert!(!s.record_pair(&a, &b, 0.91).await.unwrap(), "a repeat sweep duplicated the pair");
        assert!(!s.record_pair(&b, &a, 0.91).await.unwrap(), "the reversed pair duplicated it");
        assert_eq!(s.pairs_by_state(PairState::Pending, 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn resolving_a_pair_takes_it_off_the_pending_list() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let p = s.pairs_by_state(PairState::Pending, 10).await.unwrap().remove(0);

        s.set_pair_state(p.id, PairState::Contradiction, Some("version differs: 1.2 vs 1.4"))
            .await
            .unwrap();

        assert!(s.pairs_by_state(PairState::Pending, 10).await.unwrap().is_empty());
        let done = s.pairs_by_state(PairState::Contradiction, 10).await.unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].detail.as_deref(), Some("version differs: 1.2 vs 1.4"));
    }

    #[tokio::test]
    async fn a_resolved_pair_is_not_re_queued_by_the_next_sweep() {
        // The sweep re-finds the same pair every run. If `record_pair` reset a
        // dismissed row to pending, dismissing would achieve nothing.
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let p = s.pairs_by_state(PairState::Pending, 10).await.unwrap().remove(0);
        s.set_pair_state(p.id, PairState::Dismissed, None).await.unwrap();

        assert!(!s.record_pair(&a, &b, 0.91).await.unwrap());
        assert!(s.pairs_by_state(PairState::Pending, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleting_an_artifact_takes_its_pairs_with_it() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        s.delete_artifact(&a).await.unwrap();
        assert!(s.pairs_by_state(PairState::Pending, 10).await.unwrap().is_empty());
    }
}
```

Also add `pub mod pairs;` to `src/store/mod.rs`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib store::pairs`
Expected: FAIL — `cannot find type PairState`.

- [ ] **Step 3: Write minimal implementation**

Prepend to `src/store/pairs.rs`:

```rust
//! The consolidation review queue.
//!
//! Pairs similar enough to be worth attention but not similar enough to
//! supersede without asking. The sweep finds the same pair on every run, so a
//! row here is also the record that a decision was already made about it — a
//! dismissed pair must stay dismissed, or dismissing would achieve nothing.

use super::{Store, now};
use crate::error::Result;
use sqlx::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PairState {
    /// Found by the sweep, nothing has looked at it yet.
    Pending,
    /// The fact-token prefilter or the judge found nothing to disagree about.
    NoConflict,
    /// The judge found a detail the two artifacts state differently. Which one
    /// is current is a judgement only the reader can make.
    Contradiction,
    /// An operator looked and decided there is nothing here.
    Dismissed,
}

impl PairState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PairState::Pending => "pending",
            PairState::NoConflict => "no_conflict",
            PairState::Contradiction => "contradiction",
            PairState::Dismissed => "dismissed",
        }
    }
    pub fn parse(s: &str) -> PairState {
        match s {
            "no_conflict" => PairState::NoConflict,
            "contradiction" => PairState::Contradiction,
            "dismissed" => PairState::Dismissed,
            _ => PairState::Pending,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ArtifactPair {
    pub id: i64,
    pub a_id: String,
    pub b_id: String,
    pub score: f32,
    pub state: PairState,
    pub detail: Option<String>,
    pub created_at: i64,
}

fn row_to_pair(r: &sqlx::sqlite::SqliteRow) -> ArtifactPair {
    ArtifactPair {
        id: r.get("id"),
        a_id: r.get("a_id"),
        b_id: r.get("b_id"),
        score: r.get::<f64, _>("score") as f32,
        state: PairState::parse(r.get::<String, _>("state").as_str()),
        detail: r.get("detail"),
        created_at: r.get("created_at"),
    }
}

impl Store {
    /// File a pair for review. Returns whether this was new.
    ///
    /// `INSERT OR IGNORE` rather than an upsert, deliberately: the sweep finds
    /// the same pair every run, and re-arming a row an operator dismissed
    /// would make dismissing pointless.
    pub async fn record_pair(&self, a: &str, b: &str, score: f32) -> Result<bool> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let res = sqlx::query(
            "INSERT OR IGNORE INTO artifact_pairs (a_id, b_id, score, state, created_at)
             VALUES (?, ?, ?, 'pending', ?)",
        )
        .bind(a)
        .bind(b)
        .bind(score as f64)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn pairs_by_state(&self, state: PairState, limit: i64) -> Result<Vec<ArtifactPair>> {
        let rows = sqlx::query(
            "SELECT * FROM artifact_pairs WHERE state = ?
              ORDER BY score DESC, created_at DESC LIMIT ?",
        )
        .bind(state.as_str())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_pair).collect())
    }

    pub async fn set_pair_state(
        &self,
        id: i64,
        state: PairState,
        detail: Option<&str>,
    ) -> Result<()> {
        sqlx::query("UPDATE artifact_pairs SET state = ?, detail = ? WHERE id = ?")
            .bind(state.as_str())
            .bind(detail)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib store::pairs`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add src/store/pairs.rs src/store/mod.rs
git commit -m "feat: artifact pair review queue"
```

---

## Task 9: The sweep

**Files:**
- Create: `src/jobs/consolidate.rs`
- Modify: `src/jobs/mod.rs:1-2` (module), `:29-35` (dispatch)
- Modify: `src/store/jobs.rs:8-30` (`Stage::Consolidate`)

**Interfaces:**
- Consumes: `VectorStore::near_pairs`, `VectorStore::set_superseded`, `Store::set_superseded_by`, `Store::record_pair`, `Core::consolidate`.
- Produces:
  - `Stage::Consolidate` ⇄ `"consolidate"`.
  - `pub async fn run(core: &Core) -> Result<Outcome>` in `jobs::consolidate`, with `pub struct Outcome { pub examined: usize, pub superseded: usize, pub queued: usize }`.

- [ ] **Step 1: Write the failing test**

Create `src/jobs/consolidate.rs` with only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::artifacts::NewArtifact;
    use crate::store::pairs::PairState;
    use crate::vector::{VectorPayload, VectorPoint};

    /// Seed artifacts with hand-placed vectors, so the test controls the exact
    /// similarity rather than depending on what the fake embedder produces.
    async fn seed(core: &crate::core::Core, vectors: &[(&str, [f32; 2])]) -> Vec<String> {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = vectors
            .iter()
            .enumerate()
            .map(|(i, (text, _))| NewArtifact {
                ordinal: i as i64,
                text: (*text).to_string(),
                corpus_span: None,
                title: None,
                category: None,
                tags: vec![],
                segment_idx: None,
            })
            .collect();
        let made = core.store.insert_artifacts(&src.id, &new).await.unwrap();
        let points: Vec<VectorPoint> = made
            .iter()
            .zip(vectors)
            .map(|(c, (text, v))| VectorPoint {
                vector: v.to_vec(),
                sparse: Default::default(),
                payload: VectorPayload {
                    artifact_id: c.id.clone(),
                    corpus_id: c.corpus_id.clone(),
                    text: (*text).to_string(),
                    title: None,
                    category: None,
                    tags: vec![],
                    created_at: c.created_at,
                    last_seen_at: None,
                    superseded: None,
                },
            })
            .collect();
        core.vectors.upsert(points).await.unwrap();
        made.into_iter().map(|c| c.id).collect()
    }

    #[tokio::test]
    async fn a_near_identical_pair_supersedes_the_older_artifact() {
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;

        let out = run(&core).await.unwrap();
        assert_eq!(out.superseded, 1, "{out:?}");

        // The older one loses: ordinal 0 was inserted first.
        let older = core.store.get_artifact(&ids[0]).await.unwrap();
        let newer = core.store.get_artifact(&ids[1]).await.unwrap();
        assert_eq!(older.superseded_by.as_deref(), Some(ids[1].as_str()));
        assert!(newer.superseded_by.is_none());

        // And it is out of search, which is the whole point.
        let hits = core
            .vectors
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].payload.artifact_id, ids[1]);
    }

    #[tokio::test]
    async fn a_pair_in_the_review_band_is_queued_not_superseded() {
        // 0.88 is where two genuinely distinct artifacts about one subsystem
        // routinely sit. Acting on that score destroys knowledge.
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.93, 0.37])]).await;

        let out = run(&core).await.unwrap();
        assert_eq!(out.superseded, 0, "{out:?}");
        assert_eq!(out.queued, 1);
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().superseded_by.is_none());
        }
        assert_eq!(
            core.store.pairs_by_state(PairState::Pending, 10).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn an_unrelated_pair_is_left_entirely_alone() {
        let core = test_core().await;
        seed(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        let out = run(&core).await.unwrap();
        assert_eq!((out.superseded, out.queued), (0, 0), "{out:?}");
    }

    #[tokio::test]
    async fn a_second_sweep_changes_nothing() {
        // The sweep runs on a timer. If it were not idempotent it would churn
        // the queue and the payload flags on every tick.
        let core = test_core().await;
        seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        run(&core).await.unwrap();
        let second = run(&core).await.unwrap();
        assert_eq!((second.superseded, second.queued), (0, 0), "{second:?}");
    }

    #[tokio::test]
    async fn an_artifact_is_never_superseded_twice() {
        // Three near-identical artifacts. Whatever survives, exactly one must,
        // and no artifact may point at one that is itself superseded — that is
        // a chain the UI cannot resolve and the reader cannot follow.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("first", [1.0, 0.0]),
                ("second", [0.9999, 0.005]),
                ("third", [0.9998, 0.01]),
            ],
        )
        .await;

        run(&core).await.unwrap();

        let mut live = 0;
        for id in &ids {
            let c = core.store.get_artifact(id).await.unwrap();
            match &c.superseded_by {
                None => live += 1,
                Some(winner) => {
                    let w = core.store.get_artifact(winner).await.unwrap();
                    assert!(
                        w.superseded_by.is_none(),
                        "{id} was superseded by {winner}, which is itself superseded"
                    );
                }
            }
        }
        assert_eq!(live, 1, "exactly one artifact should have survived");
    }

    #[tokio::test]
    async fn the_sweep_is_off_when_configuration_says_so() {
        let mut core = test_core().await;
        core.consolidate.enabled = false;
        seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        let out = run(&core).await.unwrap();
        assert_eq!((out.examined, out.superseded, out.queued), (0, 0, 0));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib jobs::consolidate`
Expected: FAIL — `cannot find function run`.

- [ ] **Step 3: Write minimal implementation**

Add the stage in `src/store/jobs.rs` — enum body, `as_str`, `parse`:

```rust
    /// The periodic consolidation sweep. Its target is the collection rather
    /// than any one corpus, so there is exactly one of these in the queue at a
    /// time.
    Consolidate,
```
```rust
            Stage::Consolidate => "consolidate",
```
```rust
            "consolidate" => Some(Stage::Consolidate),
```

Prepend to `src/jobs/consolidate.rs`:

```rust
//! Consolidation: what to do about two artifacts the index says are the same.
//!
//! Three thresholds and three outcomes. At or above `auto_supersede` the pair
//! is near enough to identical that the older one is hidden — it is still
//! stored, still readable, and one write undoes it. Between `review_min` and
//! that, the pair goes on a queue for a person, because two genuinely distinct
//! artifacts about one subsystem sit at 0.88 routinely and acting on that score
//! destroys knowledge rather than duplication. Below `review_min`, nothing.
//!
//! Nothing here rewrites an artifact. A merged artifact would be synthetic text
//! standing where a stored passage used to, with no segment to verify it
//! against and no corpus lines to show beside it, which is the one failure mode
//! this design exists to avoid.

use crate::core::Core;
use crate::error::Result;
use crate::store::artifacts::Chunk;
use std::collections::HashMap;

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Outcome {
    pub examined: usize,
    pub superseded: usize,
    pub queued: usize,
}

/// Which of two artifacts survives.
///
/// The newer one, by capture time, with the id as a tie-break so the answer
/// does not depend on clock resolution. Newer is the right default because the
/// thing most often being re-captured is a document that has since been
/// updated.
fn winner<'a>(a: &'a Chunk, b: &'a Chunk) -> (&'a Chunk, &'a Chunk) {
    let a_first = (a.created_at, a.id.as_str()) < (b.created_at, b.id.as_str());
    if a_first { (b, a) } else { (a, b) }
}

pub async fn run(core: &Core) -> Result<Outcome> {
    let cfg = &core.consolidate;
    if !cfg.enabled {
        return Ok(Outcome::default());
    }

    let pairs = core
        .vectors
        .near_pairs(cfg.sample, cfg.per_point, cfg.review_min)
        .await?;
    let mut out = Outcome {
        examined: pairs.len(),
        ..Default::default()
    };

    // Artifacts superseded during this sweep. A three-way near-identical
    // cluster would otherwise have its middle member both supersede and be
    // superseded, leaving a chain the reader cannot follow.
    let mut lost: HashMap<String, ()> = HashMap::new();

    for pair in pairs {
        if lost.contains_key(&pair.a) || lost.contains_key(&pair.b) {
            continue;
        }
        // A pair whose artifacts have since been deleted is not an error; the
        // vector store can lag a delete by a sweep.
        let (Ok(a), Ok(b)) = (
            core.store.get_artifact(&pair.a).await,
            core.store.get_artifact(&pair.b).await,
        ) else {
            tracing::debug!(a = %pair.a, b = %pair.b, "pair names an artifact that is gone");
            continue;
        };
        if a.superseded_by.is_some() || b.superseded_by.is_some() {
            continue;
        }

        if pair.score >= cfg.auto_supersede {
            let (keep, drop) = winner(&a, &b);
            // SQLite first: it is the source of truth, and a payload flag with
            // no row behind it is a hidden artifact nothing can explain.
            core.store
                .set_superseded_by(&drop.id, Some(&keep.id))
                .await?;
            core.vectors.set_superseded(&drop.id, true).await?;
            lost.insert(drop.id.clone(), ());
            out.superseded += 1;
            tracing::info!(
                superseded = %drop.id,
                by = %keep.id,
                score = pair.score,
                "hid a near-identical artifact"
            );
        } else if core.store.record_pair(&pair.a, &pair.b, pair.score).await? {
            out.queued += 1;
            tracing::info!(a = %pair.a, b = %pair.b, score = pair.score, "queued a pair for review");
        }
    }

    if out.superseded > 0 || out.queued > 0 {
        tracing::info!(
            examined = out.examined,
            superseded = out.superseded,
            queued = out.queued,
            "consolidation sweep finished"
        );
    }
    Ok(out)
}
```

In `src/jobs/mod.rs`, declare the module and dispatch:

```rust
pub mod consolidate;
pub mod embed;
pub mod synthesize;
```
```rust
        (Stage::Consolidate, _) => consolidate::run(core).await.map(|_| ()),
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib jobs::consolidate` then `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/consolidate.rs src/jobs/mod.rs src/store/jobs.rs
git commit -m "feat: consolidation sweep supersedes near-identical artifacts"
```

---

## Task 10: Schedule the sweep

**Files:**
- Modify: `src/core/background.rs`
- Modify: `src/main.rs` (spawn the ticker beside the workers)

**Interfaces:**
- Consumes: `Store::enqueue`, `Core::consolidate`.
- Produces: `pub fn spawn_consolidation_ticker(core: Core, shutdown: tokio::sync::watch::Receiver<bool>) -> tokio::task::JoinHandle<()>` in `crate::core::background`.

- [ ] **Step 1: Write the failing test**

Append to `src/core/background.rs`'s test module:

```rust
    #[tokio::test]
    async fn the_ticker_queues_exactly_one_sweep() {
        // `jobs` is unique on (stage, target), so a ticker that fires while a
        // sweep is still queued must collapse onto the same row rather than
        // stacking sweeps behind a slow one.
        let core = crate::core::test_support::test_core().await;
        for _ in 0..3 {
            core.store
                .enqueue(crate::store::jobs::Stage::Consolidate, "collection", CONSOLIDATE_TARGET)
                .await
                .unwrap();
        }
        let mut seen = 0;
        while let Some(j) = core.store.claim_job().await.unwrap() {
            assert_eq!(j.stage, crate::store::jobs::Stage::Consolidate);
            seen += 1;
        }
        assert_eq!(seen, 1, "the sweep stacked up in the queue");
    }

    #[tokio::test]
    async fn a_disabled_sweep_is_never_queued() {
        let mut core = crate::core::test_support::test_core().await;
        core.consolidate.enabled = false;
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let h = spawn_consolidation_ticker(core.clone(), rx);
        // The ticker enqueues once on start when enabled; disabled it must not.
        tokio::task::yield_now().await;
        assert!(core.store.claim_job().await.unwrap().is_none());
        h.abort();
    }
```

`Stage` needs `PartialEq` for the first assertion; derive it if it is not already there.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib core::background`
Expected: FAIL — `cannot find value CONSOLIDATE_TARGET`.

- [ ] **Step 3: Write minimal implementation**

Append to `src/core/background.rs`:

```rust
/// The sweep's job target. A constant rather than a corpus id: consolidation
/// looks at the whole collection, and the `UNIQUE(stage, target_id)` on `jobs`
/// then guarantees at most one queued sweep however often the ticker fires.
pub const CONSOLIDATE_TARGET: &str = "collection";

/// Queue a consolidation sweep now and every `interval_hours` after.
///
/// A timer rather than a trigger on write: a sweep after every capture would
/// re-examine the whole collection for one new artifact, and the pairs it finds
/// do not become interesting the instant they are written.
pub fn spawn_consolidation_ticker(
    core: crate::core::Core,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !core.consolidate.enabled {
            tracing::info!("consolidation sweep disabled");
            return;
        }
        let period = std::time::Duration::from_secs(
            core.consolidate.interval_hours.max(1) * 3600,
        );
        let mut tick = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    if let Err(e) = core
                        .store
                        .enqueue(
                            crate::store::jobs::Stage::Consolidate,
                            "collection",
                            CONSOLIDATE_TARGET,
                        )
                        .await
                    {
                        tracing::warn!(error = %e, "could not queue the consolidation sweep");
                    }
                }
            }
        }
        tracing::info!("consolidation ticker stopped");
    })
}
```

In `src/main.rs`, beside the existing `Worker::spawn(...)` call, add:

```rust
    let ticker = crate::core::background::spawn_consolidation_ticker(core.clone(), shutdown_rx.clone());
```

and include `ticker` in whatever join or abort the shutdown path already performs on the worker handles.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib core::background` then `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core/background.rs src/main.rs
git commit -m "feat: queue the consolidation sweep on a timer"
```

---

## Task 11: Ops shows superseded artifacts and the review queue

**Files:**
- Modify: `src/web/ui.rs` (ops handler, two form handlers, routes)
- Modify: `src/web/templates/ops.html`
- Modify: `src/web/api.rs` (read-only JSON for the queue)
- Modify: `src/web/templates/_artifact_detail.html` (a superseded banner)

**Interfaces:**
- Consumes: `Store::superseded_artifacts`, `Store::pairs_by_state`, `Store::set_pair_state`, `Store::set_superseded_by`, `VectorStore::set_superseded`.
- Produces:
  - `Core::unsupersede(&self, artifact_id: &str) -> Result<()>`
  - `POST /ui/ops/artifacts/{id}/unsupersede`
  - `POST /ui/ops/pairs/{id}/dismiss`
  - `GET /api/v1/consolidation` → `{"superseded":[...],"pairs":[...]}`

- [ ] **Step 1: Write the failing test**

Append to `src/web/ui.rs`'s test module:

```rust
    #[tokio::test]
    async fn ops_lists_a_superseded_artifact_and_can_undo_it() {
        let (app, cookie, core) = ui_app_and_login().await;
        let src = core.store.insert_corpus("x", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "the losing artifact".into(),
                    corpus_span: None,
                    title: Some("loser".into()),
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                }],
            )
            .await
            .unwrap();
        core.store
            .set_superseded_by(&made[0].id, Some("some-winner"))
            .await
            .unwrap();

        let body = text_of(app.clone(), get("/ui/ops", &cookie)).await;
        assert!(body.contains("loser"), "the superseded artifact is not listed");

        app.clone()
            .oneshot(form(
                &format!("/ui/ops/artifacts/{}/unsupersede", made[0].id),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        assert!(
            core.store
                .get_artifact(&made[0].id)
                .await
                .unwrap()
                .superseded_by
                .is_none(),
            "undo did not clear the flag"
        );
    }

    #[tokio::test]
    async fn ops_lists_a_pending_pair_and_can_dismiss_it() {
        let (app, cookie, core) = ui_app_and_login().await;
        let src = core.store.insert_corpus("x", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        ordinal: 0,
                        text: "left".into(),
                        corpus_span: None,
                        title: Some("left one".into()),
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "right".into(),
                        corpus_span: None,
                        title: Some("right one".into()),
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                    },
                ],
            )
            .await
            .unwrap();
        core.store
            .record_pair(&made[0].id, &made[1].id, 0.9)
            .await
            .unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);

        let body = text_of(app.clone(), get("/ui/ops", &cookie)).await;
        assert!(body.contains("left one") && body.contains("right one"));

        app.clone()
            .oneshot(form(&format!("/ui/ops/pairs/{}/dismiss", pair.id), &cookie, ""))
            .await
            .unwrap();
        assert!(
            core.store
                .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }
```

Use whatever helpers the file already has for a logged-in UI client and for reading a response body; `ui_app_and_login`, `get`, `form` and `text_of` above stand for them — substitute the real names.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib web::ui`
Expected: FAIL — the ops body does not contain the artifact; the routes 404.

- [ ] **Step 3: Write minimal implementation**

In `src/core/ingest.rs` (or a small `impl Core` block in `src/core/mod.rs` — keep it beside `resolve_near_duplicate`):

```rust
    /// Put a superseded artifact back in search. The row first, then the
    /// payload, in the same order the sweep wrote them.
    pub async fn unsupersede(&self, artifact_id: &str) -> Result<()> {
        self.store.set_superseded_by(artifact_id, None).await?;
        self.vectors.set_superseded(artifact_id, false).await?;
        tracing::info!(artifact_id, "restored a superseded artifact to search");
        Ok(())
    }
```

In `src/web/ui.rs`, add row types and extend the ops template struct:

```rust
/// An artifact the sweep hid, with the one it lost to.
pub struct SupersededRow {
    pub id: String,
    pub title: String,
    pub winner_id: String,
    pub winner_title: String,
}

/// A pair waiting on a person.
pub struct PairRow {
    pub id: i64,
    pub percent: i64,
    pub a_id: String,
    pub a_title: String,
    pub b_id: String,
    pub b_title: String,
    pub detail: Option<String>,
    pub contradiction: bool,
}
```

In the `ops` handler:

```rust
    let title_of = |c: &crate::store::artifacts::Chunk| {
        c.title
            .clone()
            .unwrap_or_else(|| c.text.chars().take(60).collect())
    };

    let mut superseded = Vec::new();
    for c in st.core.store.superseded_artifacts(50).await? {
        let winner_id = c.superseded_by.clone().unwrap_or_default();
        let winner_title = match st.core.store.get_artifact(&winner_id).await {
            Ok(w) => title_of(&w),
            Err(_) => "(deleted)".to_string(),
        };
        superseded.push(SupersededRow {
            title: title_of(&c),
            id: c.id,
            winner_id,
            winner_title,
        });
    }

    let mut pairs = Vec::new();
    for state in [
        crate::store::pairs::PairState::Contradiction,
        crate::store::pairs::PairState::Pending,
    ] {
        for p in st.core.store.pairs_by_state(state, 50).await? {
            let (Ok(a), Ok(b)) = (
                st.core.store.get_artifact(&p.a_id).await,
                st.core.store.get_artifact(&p.b_id).await,
            ) else {
                continue;
            };
            pairs.push(PairRow {
                id: p.id,
                percent: (p.score * 100.0).round() as i64,
                a_title: title_of(&a),
                b_title: title_of(&b),
                a_id: p.a_id,
                b_id: p.b_id,
                detail: p.detail,
                contradiction: state == crate::store::pairs::PairState::Contradiction,
            });
        }
    }
```

Handlers and routes:

```rust
async fn unsupersede_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(id): Path<String>,
) -> Result<Response> {
    st.core.unsupersede(&id).await?;
    Ok(Redirect::to("/ui/ops").into_response())
}

async fn dismiss_pair_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(id): Path<i64>,
) -> Result<Response> {
    st.core
        .store
        .set_pair_state(id, crate::store::pairs::PairState::Dismissed, None)
        .await?;
    Ok(Redirect::to("/ui/ops").into_response())
}
```

```rust
        .route("/ui/ops/artifacts/{id}/unsupersede", post(unsupersede_ui))
        .route("/ui/ops/pairs/{id}/dismiss", post(dismiss_pair_ui))
```

In `src/web/templates/ops.html`, two sections in the established markup style:

```html
{% if !pairs.is_empty() %}
<section>
  <h2>Pairs worth a look</h2>
  <p class="hint">
    Two artifacts covering the same ground. Which one is current is a judgement
    only you can make, so nothing here has been changed.
  </p>
  <table>
    <thead><tr><th>Match</th><th>One</th><th>The other</th><th>Finding</th><th></th></tr></thead>
    <tbody>
    {% for p in pairs %}
      <tr{% if p.contradiction %} class="warn"{% endif %}>
        <td>{{ p.percent }}%</td>
        <td><a href="/ui/artifacts/{{ p.a_id }}">{{ p.a_title }}</a></td>
        <td><a href="/ui/artifacts/{{ p.b_id }}">{{ p.b_title }}</a></td>
        <td>{{ p.detail.as_deref().unwrap_or("") }}</td>
        <td>
          <form method="post" action="/ui/ops/pairs/{{ p.id }}/dismiss">
            <button type="submit">Dismiss</button>
          </form>
        </td>
      </tr>
    {% endfor %}
    </tbody>
  </table>
</section>
{% endif %}

{% if !superseded.is_empty() %}
<section>
  <h2>Hidden as near-identical</h2>
  <p class="hint">Still stored and still readable; kept out of results only.</p>
  <table>
    <thead><tr><th>Hidden</th><th>Kept</th><th></th></tr></thead>
    <tbody>
    {% for s in superseded %}
      <tr>
        <td><a href="/ui/artifacts/{{ s.id }}">{{ s.title }}</a></td>
        <td><a href="/ui/artifacts/{{ s.winner_id }}">{{ s.winner_title }}</a></td>
        <td>
          <form method="post" action="/ui/ops/artifacts/{{ s.id }}/unsupersede">
            <button type="submit">Put it back</button>
          </form>
        </td>
      </tr>
    {% endfor %}
    </tbody>
  </table>
</section>
{% endif %}
```

In `src/web/templates/_artifact_detail.html`, above the artifact body, so opening a hidden artifact by link says why it is not in results:

```html
{% if let Some(winner) = artifact.superseded_by %}
<p class="warn">
  Hidden from results: something near-identical and newer
  (<a href="/ui/artifacts/{{ winner }}">this one</a>) was kept instead.
</p>
{% endif %}
```

In `src/web/api.rs`, a read-only endpoint plus its route:

```rust
/// What consolidation has decided and what it is still asking about.
async fn consolidation(
    State(st): State<AppState>,
    _id: Identity,
) -> Result<Json<serde_json::Value>> {
    Ok(Json(serde_json::json!({
        "superseded": st.core.store.superseded_artifacts(100).await?,
        "pairs": st
            .core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 100)
            .await?,
    })))
}
```
```rust
        .route("/consolidation", get(consolidation))
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib web` then `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web src/core
git commit -m "feat: consolidation review surface on Ops"
```

---

## Task 12: Caveats from the synthesis call

Tier 1, and independent of everything above: it changes what one existing model call is asked to return.

**Files:**
- Modify: `src/infer/mod.rs:11-18` (`ProposedArtifact`)
- Modify: `src/infer/prompt.rs:4-29` (system prompt), `:53-64` (`RawArtifact`), and the mapping into `ProposedArtifact`
- Modify: `src/infer/verify.rs`
- Modify: `src/store/artifacts.rs` (`Chunk.caveats`, `NewArtifact.caveats`, insert, `update_artifact_caveats`)
- Modify: `src/jobs/synthesize.rs` (carry the field through)
- Modify: `src/web/templates/_artifact_detail.html`

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `ProposedArtifact { caveats: Vec<String>, .. }`
  - `NewArtifact { caveats: Vec<String>, .. }`, `Chunk { caveats: Vec<String>, .. }`
  - `verify::literals_missing_from(text_and_caveats, segment)` unchanged in name; it is fed the caveats as well.

**Design note the implementer must not change:** caveats are stored and rendered but **not** added to `embed_text` in `src/jobs/embed.rs`. Changing what every vector is built from is exactly the kind of index change the roadmap says needs evaluation pairs first, and there are none. Storage and display cost nothing and can be reversed; a re-embedded collection cannot.

- [ ] **Step 1: Write the failing test**

Append to `src/infer/prompt.rs`'s test module:

```rust
    #[test]
    fn caveats_are_parsed_when_the_model_supplies_them() {
        let body = r#"{"artifacts":[{
            "text":"Run `mkfs.ext4 /dev/sdb1` to format the partition.",
            "title":"Formatting a partition",
            "category":"procedure",
            "tags":["disk"],
            "corpus_lines":[3,9],
            "caveats":["Destroys every existing file on the device.",
                       "Requires root."]
        }]}"#;
        let got = parse_response(body).unwrap();
        assert_eq!(
            got[0].caveats,
            vec![
                "Destroys every existing file on the device.".to_string(),
                "Requires root.".to_string()
            ]
        );
    }

    #[test]
    fn an_artifact_without_caveats_parses_to_an_empty_list() {
        // Most models will omit the field most of the time, and a missing
        // field must never fail a segment that is otherwise fine.
        let body = r#"{"artifacts":[{"text":"plain","title":"t","category":"c","tags":[]}]}"#;
        assert!(parse_response(body).unwrap()[0].caveats.is_empty());
    }

    #[test]
    fn the_system_prompt_asks_for_caveats_and_forbids_inventing_them() {
        assert!(SYNTHESIZER_SYSTEM.contains("caveats"));
        assert!(
            SYNTHESIZER_SYSTEM.contains("stated"),
            "the prompt must tie caveats to what the source says"
        );
    }
```

Append to `src/infer/verify.rs`'s test module:

```rust
    #[test]
    fn a_command_invented_in_a_caveat_is_caught() {
        // A caveat is prose the model wrote, and it is exactly where an
        // invented "run `rm -rf /var/lib/thing` first" would appear. The
        // literal check has to reach it, or caveats become the one part of an
        // artifact nothing verifies.
        let segment = "Format with mkfs.ext4 /dev/sdb1 after unmounting.";
        let missing = missing_literals(
            "Run `mkfs.ext4 /dev/sdb1`.",
            &["First run `wipefs --all /dev/sdb1`.".to_string()],
            segment,
        );
        assert_eq!(missing, vec!["wipefs --all /dev/sdb1".to_string()]);
    }

    #[test]
    fn a_caveat_quoting_a_real_command_is_not_flagged() {
        let segment = "Format with mkfs.ext4 /dev/sdb1 after unmounting.";
        assert!(
            missing_literals(
                "Format the partition.",
                &["Unmount first: `mkfs.ext4 /dev/sdb1` fails on a mounted device.".to_string()],
                segment,
            )
            .is_empty()
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib infer`
Expected: FAIL — `no field caveats`, `cannot find function missing_literals`.

- [ ] **Step 3: Write minimal implementation**

`src/infer/mod.rs`:

```rust
pub struct ProposedArtifact {
    pub text: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub corpus_lines: Option<(i64, i64)>,
    /// Conditions under which the artifact does not apply, as the source states
    /// them. The model is already holding this segment, so asking for them
    /// costs output tokens rather than another call.
    pub caveats: Vec<String>,
}
```

`src/infer/prompt.rs` — extend the contract. Replace the shape line and the field list:

```rust
{"artifacts":[{"text":"...","title":"...","category":"...","tags":["..."],"corpus_lines":[start,end],"caveats":["..."]}]}

- title: a short noun phrase naming the artifact.
- category: one lowercase word, e.g. procedure, concept, reference, snippet.
- tags: 1-5 lowercase keywords for filtering.
- corpus_lines: the 1-based line range in the input this artifact came from.
- caveats: 0-3 short sentences for conditions under which this artifact does
  not hold — a prerequisite, a version or platform it is specific to, a
  destructive effect, a documented failure. Take these only from what the input
  states or plainly implies. Never invent a caveat, never add general advice,
  and never put a command in a caveat that is not in the input. Use an empty
  list when the input states none, which is the common case.
```

Add the field to `RawArtifact`:

```rust
    #[serde(default)]
    caveats: Vec<String>,
```

and to wherever `RawArtifact` becomes `ProposedArtifact`:

```rust
            caveats: raw
                .caveats
                .into_iter()
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .take(3)
                .collect(),
```

`src/infer/verify.rs` — add a function that runs the existing literal check over the artifact and its caveats together:

```rust
/// Literals in an artifact and its caveats that the segment does not contain.
///
/// Caveats are the newest place model prose can appear, and a caveat that says
/// to run something first is a command that gets pasted into a root shell just
/// like one in the body. They go through the same check rather than a weaker
/// one.
pub fn missing_literals(text: &str, caveats: &[String], segment: &str) -> Vec<String> {
    let haystack = normalize(segment);
    let mut all = extract_literals(text);
    for c in caveats {
        all.extend(extract_literals(c));
    }
    let mut missing: Vec<String> = all
        .into_iter()
        .filter(|l| !haystack.contains(&normalize(l)))
        .collect();
    missing.sort();
    missing.dedup();
    missing
}
```

Route the existing caller in `src/jobs/synthesize.rs` through `missing_literals`, passing the proposal's caveats, so flagging behaviour is unchanged except that it now sees caveats.

`src/store/artifacts.rs` — `caveats: Vec<String>` on both `Chunk` and `NewArtifact`; read it in `row_to_artifact` the same way `tags` is read; bind it in the `INSERT` alongside `tags`; and:

```rust
    pub async fn update_artifact_caveats(&self, id: &str, caveats: &[String]) -> Result<()> {
        sqlx::query("UPDATE artifacts SET caveats = ? WHERE id = ?")
            .bind(serde_json::to_string(caveats).unwrap_or_else(|_| "[]".into()))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
```

`src/jobs/synthesize.rs` — set `caveats: p.caveats.clone()` where `NewArtifact` is built from a `ProposedArtifact`. In `src/jobs/embed.rs`'s `replace_with_siblings`, siblings inherit `caveats: chunk.caveats.clone()`, as they inherit tags.

`src/web/templates/_artifact_detail.html` — render them under the body:

```html
{% if !artifact.caveats.is_empty() %}
<aside class="caveats">
  <h3>Before you rely on this</h3>
  <ul>
    {% for c in artifact.caveats %}<li>{{ c }}</li>{% endfor %}
  </ul>
</aside>
{% endif %}
```

Every `NewArtifact` and `ProposedArtifact` literal in the test suites now needs the new field; the compiler names them all, including the two literals deferred from Task 2.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/infer src/store/artifacts.rs src/jobs src/web/templates/_artifact_detail.html
git commit -m "feat: synthesis emits caveats alongside each artifact"
```

---

## Task 13: The fact-token prefilter

Zero inference. Its entire job is to keep the judge from being called on pairs that cannot disagree.

**Files:**
- Create: `src/infer/facts.rs`
- Modify: `src/infer/mod.rs` (add `pub mod facts;`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub fn fact_tokens(text: &str) -> std::collections::BTreeSet<String>` — numbers with units, version strings, dates, and flag/path tokens.
  - `pub fn may_disagree(a: &str, b: &str) -> bool` — true only when the two texts share some fact-shaped vocabulary *and* differ on some of it.

- [ ] **Step 1: Write the failing test**

Create `src/infer/facts.rs` with only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_numbers_and_dates_are_facts() {
        let f = fact_tokens("Requires 1.21.4 or later, 30 seconds, on 2024-03-01, at 8080.");
        assert!(f.contains("1.21.4"), "{f:?}");
        assert!(f.contains("2024-03-01"), "{f:?}");
        assert!(f.contains("30"), "{f:?}");
        assert!(f.contains("8080"), "{f:?}");
    }

    #[test]
    fn ordinary_prose_carries_no_facts() {
        assert!(fact_tokens("Mount the filesystem before writing to it.").is_empty());
    }

    #[test]
    fn two_artifacts_giving_a_different_version_may_disagree() {
        assert!(may_disagree(
            "engram requires Rust 1.21.4 to build.",
            "engram requires Rust 1.30.0 to build.",
        ));
    }

    #[test]
    fn the_same_fact_stated_twice_does_not_disagree() {
        assert!(!may_disagree(
            "engram requires Rust 1.21.4 to build.",
            "To build engram you need Rust 1.21.4.",
        ));
    }

    #[test]
    fn artifacts_with_no_facts_in_common_do_not_disagree() {
        // Two artifacts about different subjects that happen to embed close.
        // Sending these to the model is the waste this filter exists to stop.
        assert!(!may_disagree(
            "The mount command attaches a filesystem.",
            "Version 9.9.9 of the pastry compiler ships on 2030-01-01.",
        ));
    }

    #[test]
    fn one_artifact_with_no_facts_never_disagrees() {
        assert!(!may_disagree("Prose only.", "Requires 1.2.3."));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib infer::facts`
Expected: FAIL — `cannot find function fact_tokens`.

- [ ] **Step 3: Write minimal implementation**

Prepend to `src/infer/facts.rs`:

```rust
//! Could these two artifacts actually disagree?
//!
//! The similarity sweep says two artifacts cover the same ground. That is not
//! the interesting question — the interesting one is whether they state some
//! detail differently, because a wrong artifact ranks exactly as well as a
//! right one and nothing else in the system notices. Answering it properly
//! needs a model, and a model call is minutes on the hardware this is built
//! for, so this narrows the candidate set first at the cost of a scan.
//!
//! The rule is deliberately conservative in one direction only: it must never
//! discard a pair that might disagree, and it is free to pass through pairs
//! that turn out not to. A pair it passes costs one call; a pair it wrongly
//! drops costs a stale artifact nobody ever finds.

use std::collections::BTreeSet;

/// Is this token shaped like something two documents could state differently?
///
/// Bare numbers count: a timeout, a port, a count of retries. Words do not —
/// two artifacts using different prose for the same thing is what synthesis is
/// supposed to produce, not a contradiction.
fn is_fact(token: &str) -> bool {
    let t = token.trim_matches(|c: char| !c.is_alphanumeric());
    if t.is_empty() || !t.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return false;
    }
    // A leading digit plus only digits and separators: 30, 1.21.4, 2024-03-01,
    // 8080. Anything with letters in it — `3rd`, `x86_64` — is vocabulary
    // rather than a value.
    t.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-' || c == ':')
}

/// Every value-shaped token in the text, normalised of surrounding punctuation.
pub fn fact_tokens(text: &str) -> BTreeSet<String> {
    text.split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric())
                .to_string()
        })
        .filter(|t| is_fact(t))
        .collect()
}

/// Whether a pair is worth a model call.
///
/// Two conditions, and both are needed. Some shared value means the artifacts
/// are talking about the same measurable thing at all — without it, differing
/// numbers are two unrelated facts rather than a disagreement. Some differing
/// value is the disagreement itself: artifacts that state exactly the same
/// values have nothing for a judge to find, however much prose separates them.
pub fn may_disagree(a: &str, b: &str) -> bool {
    let (fa, fb) = (fact_tokens(a), fact_tokens(b));
    if fa.is_empty() || fb.is_empty() {
        return false;
    }
    let shared = fa.intersection(&fb).count();
    let differing = fa.symmetric_difference(&fb).count();
    shared > 0 && differing > 0
}
```

Add `pub mod facts;` to `src/infer/mod.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib infer::facts`
Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
git add src/infer/facts.rs src/infer/mod.rs
git commit -m "feat: fact-token prefilter for consolidation pairs"
```

---

## Task 14: The contradiction judge

**Files:**
- Modify: `src/infer/prompt.rs` (judge prompt and parser)
- Modify: `src/jobs/consolidate.rs` (judge stage after the sweep)
- Modify: `src/infer/fake.rs` (a scriptable completer)

**Interfaces:**
- Consumes: `Completer::complete`, `facts::may_disagree`, `Store::pairs_by_state`, `Store::set_pair_state`, `Core::consolidate`.
- Produces:
  - `pub const JUDGE_SYSTEM: &str`
  - `pub fn judge_prompt(a: &str, b: &str) -> String`
  - `pub fn parse_judgement(body: &str) -> Result<(bool, Option<String>)>`
  - `pub struct ScriptedCompleter { replies: Mutex<VecDeque<String>>, calls: AtomicUsize }` with `pub fn new(replies: Vec<String>) -> Self` and `pub fn calls(&self) -> usize`.
  - `Outcome { judged: usize, contradictions: usize, .. }`

- [ ] **Step 1: Write the failing test**

Append to `src/infer/prompt.rs`'s test module:

```rust
    #[test]
    fn a_judgement_parses() {
        let (yes, detail) =
            parse_judgement(r#"{"contradicts":true,"detail":"one says 1.2, the other 1.4"}"#)
                .unwrap();
        assert!(yes);
        assert_eq!(detail.as_deref(), Some("one says 1.2, the other 1.4"));
    }

    #[test]
    fn a_negative_judgement_carries_no_detail() {
        let (yes, detail) = parse_judgement(r#"{"contradicts":false}"#).unwrap();
        assert!(!yes);
        assert!(detail.is_none());
    }

    #[test]
    fn a_judgement_wrapped_in_prose_and_fences_still_parses() {
        // The same models that fence the synthesis reply fence this one.
        let (yes, _) = parse_judgement("Sure:\n```json\n{\"contradicts\": true}\n```")
            .unwrap();
        assert!(yes);
    }

    #[test]
    fn an_unparsable_judgement_is_an_error_not_a_yes() {
        // Defaulting to "contradicts" would fill the review queue with noise;
        // defaulting to "no" would hide real conflicts. Neither: it fails, the
        // pair stays pending, and the next sweep tries again.
        assert!(parse_judgement("I could not decide.").is_err());
    }
```

Append to `src/jobs/consolidate.rs`'s test module:

```rust
    use crate::infer::fake::ScriptedCompleter;

    /// Two artifacts about the same thing that give a different version.
    async fn disagreeing(core: &crate::core::Core) -> Vec<String> {
        seed(
            core,
            &[
                ("engram needs Rust 1.21.4 to build.", [1.0, 0.0]),
                ("engram needs Rust 1.30.0 to build.", [0.93, 0.37]),
            ],
        )
        .await
    }

    #[tokio::test]
    async fn the_judge_is_off_by_default() {
        let core = test_core().await;
        disagreeing(&core).await;
        let out = run(&core).await.unwrap();
        assert_eq!(out.queued, 1);
        assert_eq!(out.judged, 0, "the judge ran without being asked for");
    }

    #[tokio::test]
    async fn an_enabled_judge_marks_a_real_contradiction() {
        let mut core = test_core().await;
        core.consolidate.judge = true;
        core.completer = std::sync::Arc::new(ScriptedCompleter::new(vec![
            r#"{"contradicts":true,"detail":"1.21.4 versus 1.30.0"}"#.into(),
        ]));
        disagreeing(&core).await;

        let out = run(&core).await.unwrap();
        assert_eq!((out.judged, out.contradictions), (1, 1), "{out:?}");
        let found = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Contradiction, 10)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].detail.as_deref(), Some("1.21.4 versus 1.30.0"));
    }

    #[tokio::test]
    async fn a_pair_with_no_facts_to_disagree_about_never_reaches_the_model() {
        // The prefilter is the whole economic argument for this feature: a
        // model call is minutes, and most near pairs have nothing to judge.
        let mut core = test_core().await;
        core.consolidate.judge = true;
        let completer = std::sync::Arc::new(ScriptedCompleter::new(vec![]));
        core.completer = completer.clone();
        seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;

        let out = run(&core).await.unwrap();
        assert_eq!(completer.calls(), 0, "the prefilter let a factless pair through");
        assert_eq!(out.judged, 0);
        assert_eq!(
            core.store
                .pairs_by_state(crate::store::pairs::PairState::NoConflict, 10)
                .await
                .unwrap()
                .len(),
            1,
            "a cleared pair must leave the pending queue"
        );
    }

    #[tokio::test]
    async fn the_judge_stops_at_its_budget() {
        // One sweep must not be able to occupy the GPU for an hour.
        let mut core = test_core().await;
        core.consolidate.judge = true;
        core.consolidate.max_judgements = 1;
        let completer = std::sync::Arc::new(ScriptedCompleter::new(vec![
            r#"{"contradicts":false}"#.into(),
            r#"{"contradicts":false}"#.into(),
            r#"{"contradicts":false}"#.into(),
        ]));
        core.completer = completer.clone();
        seed(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.93, 0.37]),
                ("timeout is 90 seconds", [0.94, 0.34]),
            ],
        )
        .await;

        run(&core).await.unwrap();
        assert_eq!(completer.calls(), 1, "the budget was ignored");
    }

    #[tokio::test]
    async fn a_failed_judgement_leaves_the_pair_pending() {
        // A dead endpoint must not silently clear a queue of real conflicts.
        let mut core = test_core().await;
        core.consolidate.judge = true;
        core.completer = std::sync::Arc::new(ScriptedCompleter::new(vec!["not json".into()]));
        disagreeing(&core).await;

        run(&core).await.unwrap();
        assert_eq!(
            core.store
                .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib`
Expected: FAIL — `cannot find function parse_judgement`, `no field judged`, `cannot find struct ScriptedCompleter`.

- [ ] **Step 3: Write minimal implementation**

`src/infer/fake.rs`:

```rust
/// A completer that answers from a script and counts how often it was asked.
///
/// The consolidation tests are largely about *not* calling the model, so what
/// they assert on is the call count as much as the reply.
pub struct ScriptedCompleter {
    replies: std::sync::Mutex<std::collections::VecDeque<String>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl ScriptedCompleter {
    pub fn new(replies: Vec<String>) -> Self {
        Self {
            replies: std::sync::Mutex::new(replies.into()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl Completer for ScriptedCompleter {
    async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| crate::error::Error::Inference {
                role: "ask",
                detail: "the script ran out of replies".into(),
            })
    }
    fn context_tokens(&self) -> usize {
        4096
    }
}
```

`src/infer/prompt.rs`:

```rust
/// The judge is asked one question and given no room to be helpful.
///
/// It is not asked which artifact is right, nor to merge them, nor to rewrite
/// anything. Deciding which of two contradictory artifacts is current needs
/// context the base does not hold — what the reader is actually running — and
/// is a judgement only they can make. All this call does is tell them there is
/// a judgement waiting.
pub const JUDGE_SYSTEM: &str = r#"You compare two knowledge artifacts and answer one question: do they state some specific detail differently?

A contradiction is a concrete disagreement about the same thing: a different version, number, date, path, flag, default, or step order for the same subject.

These are NOT contradictions:
- The same fact in different words.
- Different levels of detail about the same thing.
- Two different subjects that happen to use similar language.
- One artifact mentioning something the other simply does not cover.

Reply with JSON only, no commentary, in exactly this shape:

{"contradicts": true, "detail": "..."}

- contradicts: true only for a concrete disagreement, as above.
- detail: when true, one short sentence naming the two conflicting values. Omit it when false."#;

pub fn judge_prompt(a: &str, b: &str) -> String {
    format!(
        "----- ARTIFACT A -----\n{a}\n----- ARTIFACT B -----\n{b}\n----- END -----"
    )
}

#[derive(serde::Deserialize)]
struct Judgement {
    contradicts: bool,
    #[serde(default)]
    detail: Option<String>,
}

/// A reply that cannot be read is an error, not a verdict.
///
/// Defaulting to "contradicts" would fill the review queue with noise an
/// operator has to clear by hand; defaulting to "no" would quietly close real
/// conflicts. Failing leaves the pair pending, and the next sweep asks again.
pub fn parse_judgement(body: &str) -> Result<(bool, Option<String>)> {
    let j: Judgement = serde_json::from_str(extract_json(body)).map_err(|e| {
        Error::MalformedLlmOutput(format!("judge reply was not the expected JSON: {e}"))
    })?;
    let detail = j
        .detail
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty() && j.contradicts);
    Ok((j.contradicts, detail))
}
```

`src/jobs/consolidate.rs` — extend `Outcome` and append the judge stage to `run`:

```rust
pub struct Outcome {
    pub examined: usize,
    pub superseded: usize,
    pub queued: usize,
    pub judged: usize,
    pub contradictions: usize,
}
```

At the end of `run`, before the summary log:

```rust
    if core.consolidate.judge {
        let (judged, contradictions) = judge_pending(core).await?;
        out.judged = judged;
        out.contradictions = contradictions;
    }
```

```rust
/// Ask the model about pending pairs, but only the ones that could possibly
/// disagree, and only up to the sweep's budget.
///
/// Returns how many calls were made and how many found a contradiction. A
/// failed call leaves its pair pending on purpose: a dead endpoint must never
/// look like a clean bill of health.
async fn judge_pending(core: &Core) -> Result<(usize, usize)> {
    let pending = core
        .store
        .pairs_by_state(crate::store::pairs::PairState::Pending, 200)
        .await?;

    let (mut judged, mut contradictions) = (0usize, 0usize);
    for p in pending {
        if judged >= core.consolidate.max_judgements {
            tracing::info!(
                budget = core.consolidate.max_judgements,
                "judge budget spent; the rest wait for the next sweep"
            );
            break;
        }
        let (Ok(a), Ok(b)) = (
            core.store.get_artifact(&p.a_id).await,
            core.store.get_artifact(&p.b_id).await,
        ) else {
            continue;
        };

        // The whole economic argument: most near pairs have no value in common
        // to disagree about, and a model call is minutes on this hardware.
        if !crate::infer::facts::may_disagree(&a.text, &b.text) {
            core.store
                .set_pair_state(p.id, crate::store::pairs::PairState::NoConflict, None)
                .await?;
            continue;
        }

        judged += 1;
        let reply = match core
            .completer
            .complete(
                crate::infer::prompt::JUDGE_SYSTEM,
                &crate::infer::prompt::judge_prompt(&a.text, &b.text),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(pair = p.id, error = %e, "judge call failed; pair stays pending");
                continue;
            }
        };

        match crate::infer::prompt::parse_judgement(&reply) {
            Ok((true, detail)) => {
                contradictions += 1;
                core.store
                    .set_pair_state(
                        p.id,
                        crate::store::pairs::PairState::Contradiction,
                        detail.as_deref(),
                    )
                    .await?;
                tracing::info!(pair = p.id, a = %a.id, b = %b.id, "artifacts disagree");
            }
            Ok((false, _)) => {
                core.store
                    .set_pair_state(p.id, crate::store::pairs::PairState::NoConflict, None)
                    .await?;
            }
            Err(e) => {
                tracing::warn!(pair = p.id, error = %e, "judge reply unreadable; pair stays pending");
            }
        }
    }
    Ok((judged, contradictions))
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/infer src/jobs/consolidate.rs
git commit -m "feat: contradiction judge over the consolidation review band"
```

---

## Task 15: Configuration, documentation, and the final gate

**Files:**
- Modify: `config.example.toml`, `README.md`, `ROADMAP.md`

**Interfaces:**
- Consumes: everything above.
- Produces: no code.

- [ ] **Step 1: Write the failing test**

`src/core/mod.rs`'s `rerank_is_wired_only_when_configured` already loads `config.example.toml`, so an example file that does not deserialize fails the suite. Add one explicit test beside it:

```rust
    #[tokio::test]
    async fn the_example_config_carries_the_consolidation_defaults() {
        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert!(cfg.consolidate.enabled);
        assert!(
            cfg.consolidate.auto_supersede > cfg.consolidate.review_min,
            "superseding at or below the review threshold would hide distinct artifacts"
        );
        assert!(!cfg.consolidate.judge, "the only inference-costing stage must be opt-in");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib core::mod`
Expected: FAIL — `unknown field 'consolidate'` or the assertions fail, depending on whether the block was added yet.

- [ ] **Step 3: Write minimal implementation**

Append to `config.example.toml`:

```toml
[consolidate]
# The background sweep. Capture-time near-duplicate detection is separate and
# always on: it costs a hash of the text, not a query.
enabled = true
# Estimated overlap of word shingles above which a new capture is parked as a
# near-duplicate of an existing corpus rather than segmented. It is stored
# either way; parking only withholds the model call until you decide on Ops.
near_dupe_min = 0.90
# Cosine at or above which a pair of artifacts goes on the review queue.
review_min = 0.88
# Cosine at or above which the older artifact is hidden without asking. Well
# clear of `review_min` on purpose: two genuinely distinct artifacts about one
# subsystem sit around 0.88 routinely, and hiding at that score costs knowledge
# rather than duplication.
auto_supersede = 0.95
# Points drawn per sweep, and neighbours considered per point. The sweep is one
# round trip whose cost these two numbers bound, rather than a query per point.
sample = 2000
per_point = 5
interval_hours = 24
# Ask the completion model whether a queued pair actually disagrees. Off by
# default: it is the only part of consolidation that costs inference, and it
# only ever sees pairs whose numbers, versions or dates already differ.
judge = false
max_judgements = 20
```

Add to the README configuration table:

```markdown
| `consolidate.*` | Duplicate hygiene: `enabled`, `near_dupe_min`, `review_min`, `auto_supersede`, `sample`, `per_point`, `interval_hours`, `judge`, `max_judgements`. |
```

Add a README section after "Does the artifact still say what the corpus said?":

```markdown
## Duplicates, and what goes quietly out of date

Two failures look identical from a result list: the same thing stored twice,
and the same thing stored twice with one copy now wrong. Both are handled
without a model call in the ordinary case.

**At capture.** Corpora are deduplicated by an exact hash, so re-pasting a
chapter a year later with one changed byte used to store it twice, and the two
copies then competed for the same queries. A shingle signature over the raw
text catches that: the capture is stored like any other, and parked in
`needs_review` rather than segmented. Ops offers three answers — replace the
older corpus, keep both, or discard this one — and until one is chosen, no
model call has been spent on it.

**Afterwards.** A sweep asks Qdrant for near pairs across the collection, one
round trip, on a timer. Above `auto_supersede` the older artifact is marked
`superseded_by` the newer and hidden from results; it is still stored, still
readable by link, and Ops has a button that puts it back. Between `review_min`
and that, the pair goes on a queue instead, because two genuinely distinct
artifacts about one subsystem sit around 0.88 and hiding at that score would
cost knowledge rather than duplication.

**Nothing is ever merged.** A merged artifact is synthetic text standing where a
stored passage used to be, with no segment to verify it against and no corpus
lines to render beside it. Consolidation only ever hides, flags, or asks.

**The judge**, off by default, is the one part that costs inference. Queued
pairs are first filtered on fact-shaped tokens — versions, numbers, dates —
and only a pair that shares some and differs on others reaches the model, which
is asked one yes/no question and given a per-sweep budget. Which of two
contradictory artifacts is current stays a judgement for the reader.

Artifacts also carry **caveats**: the conditions under which they do not apply,
emitted by the same synthesis call that wrote them, so they cost output tokens
rather than another call. They are stored and shown, and deliberately not part
of what gets embedded — changing what every vector is built from is a decision
for the evaluation harness, not a hunch.
```

In `ROADMAP.md`, strike through the two delivered items — "Near-duplicate detection on capture" and "Consolidation and staleness sweep" — in the style already used for the FTS5 entry, naming the files that implement them. Leave the "Corpus map" item, which is not built.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test` then `cargo clippy --all-targets -- -D warnings`
Expected: PASS and clean.

- [ ] **Step 5: Commit**

```bash
git add config.example.toml README.md ROADMAP.md src/core/mod.rs
git commit -m "docs: describe consolidation and its thresholds"
```

---

## Self-Review

**Spec coverage.** Tier 0 hygiene: capture-time shingle detection (Tasks 1, 3, 4, 5), the supersede sweep with `superseded_by` and the review queue (Tasks 2, 6, 7, 8, 9, 10, 11). Tier 1 prompt enrichment: Task 12. Tier 2 contradiction judge: Tasks 13, 14. Configuration and documentation: Task 15. The dropped item — the speculative query index — appears nowhere, as agreed.

**Deferred-field note.** Task 2's tests reference `NewArtifact.caveats`, which Task 12 introduces. The task text says to omit the field until then. An implementer working strictly in order will not hit it; one working out of order will be told by the compiler.

**Type consistency.** `NearPair::new` orders its members, and `Store::record_pair` orders again — both are needed because the memory and Qdrant stores build pairs independently. `PairState` is used by name in Tasks 8, 11 and 14 with the same four variants. `superseded` (payload, `Option<bool>`) and `superseded_by` (row, `Option<String>`) are distinct on purpose and never interchanged. `ConsolidateConfig` field names are identical in `config.rs`, the example file, and every `core.consolidate.*` read.

**Known judgement calls, flagged rather than hidden:** superseding keeps the *newer* artifact, which is wrong for a base where the older capture was the better transcription — the undo button on Ops exists for exactly that. And the Qdrant `near_pairs` implementation depends on the matrix-pairs endpoint, which needs Qdrant 1.12 or newer; if the deployment is older, that call fails and the sweep retries as an ordinary job failure rather than corrupting anything.
