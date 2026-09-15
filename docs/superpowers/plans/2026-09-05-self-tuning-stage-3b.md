# Self-tuning, stage 3b: the corpus jobs answer to the same evidence

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every corpus action the base takes is journaled; two rules read the journal against observations and take actions back; the graveyard is listed; and `review_min` moves on the ladder. After this plan the stage 3 spec is built in full.

**Architecture:** One new table, `corpus_actions`, written at every action site and stamped by every undo. One new job module, `jobs/retract.rs`, run inside the idle pass after the anchor check, holding rule 1 (a survivor must still be found: replay the subject's observations, read the survivor's rank) and rule 2 (a give-up a hidden artifact would have answered: replay the give-up with hidden hits included, and by cosine over buried vectors). Restores go through the `Core` methods the operator's buttons already call. Part C puts `review_min` on `RankingParams`, read by `relate.rs` off the lock, and moves it on two band records read from pairs and the journal.

**Tech Stack:** Rust 2024 edition, sqlx 0.9 over SQLite, tokio, serde. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-05-self-tuning-stage-3-design.md`, Parts B and C, plus "Error handling" and "Testing". Read them before task 1, and the handoff `docs/superpowers/plans/2026-09-05-self-tuning-handoff.md` for the corrections to Part 5.

**Depends on:** stage 3a, on `feat/observations`. Baseline: 2550 passing, 0 failing, 1 ignored.

## What this plan admits

Read against the tree, the spec's Part B and C are built with these readings:

- **There is no `stale` kind.** Nothing hides an artifact as stale on its own: `stale_after_days` / `stale_max_hits` build the "Worth a second look" list, and the "Hidden as stale" heading on Insights shows operator deprecations. The base never takes that action, so there is nothing to journal and nothing for rule 2 to restore. Kinds are `merge`, `supersede`, `discard`, `reap`, `promote`, `moment`.
- **Rule 2 compares within one replay, not against the captured pool.** The replay with hidden hits included returns live and hidden hits with a similarity from one search; "the best hidden hit is more similar than the best live hit" is read there. The captured pool holds the same quantity as of the search; the replay holds it as of now, and both sides of the comparison come from one vector.
- **Rescues are not journaled.** `reap::rescue_one` supersedes a retired artifact by a rewrite; the original stays as a superseded row with its text, and the spec's table does not name it. It joins when someone defines what taking a rescue back means.
- **A `review_min` move is watched by `lived`, like any generation.** The spec asks for a watch on the band records. The wrong signal at the next pass is that watch — it steps the rung back up — and `lived` ends the "under watch" state on observation count, so ranking moves resume. One watch mechanism, not two.
- **The band's "above" record runs to 1.0**, not to the next rung: pairs above `auto_supersede` are judged the same way, and the question is whether the lowest band acts like everything above it.
- **`DecidedBy` gains `Evidence`.** `merge::undo` writes `DecidedBy::Operator` on the pairs it dismisses; an undo the base takes on evidence must not claim a person.
- **The journal rows for merge, supersede and discard are written under the lifecycle lock inside the `Core` method, or beside the pair write where no lock is held (merge).** Reap's row rides `Store::bury`'s transaction. That is as close to "same transaction" as the tree allows.

## Global Constraints

- Gate, every task: `cargo fmt --all --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`. Adding a field to `RankingParams` (task 7) breaks initializers under `--all-targets`.
- No tuned constant. Gates read the evidence in front of them: `recommend` (two net, no aggregate loss) and the one-decision noise term `1/n_a + 1/n_b` on rates.
- `config.toml` is never written by the loop.
- New tables are free; every column on an existing table is on the `ADDITIVE` list in `src/store/mod.rs` with a written reason. `graveyard.vec` and `graveyard.embed_model` are nullable and qualify.
- Test names are sentences stating the rule.
- `gen` is reserved in edition 2024. `crate::error::Error` has no `From<serde_json::Error>`. sqlx 0.9 refuses `query(&format!(..))`; `sqlx::AssertSqlSafe(format!(..))` is the tree's escape where a constant fragment is spliced (see `Store::bury`). Ids are ULIDs; shorten by the tail. `search_candidates.rank` is 0-based; `observations.rank` is 1-based.
- The lifecycle lock is `Core::lifecycle_lock` and is taken *inside* `Core::supersede`, `Core::deprecate`, `Core::reactivate`, `Core::unsupersede`; never take it in a job and then call one of them.
- Commit after every task, on `feat/observations`.

---

### Task 1: The journal

**Files:**
- Create: `src/store/actions.rs`
- Modify: `src/store/mod.rs` (`mod actions;`), `src/store/schema.sql` (new table), `src/store/pairs.rs:151-180` (`DecidedBy::Evidence`)

**Interfaces:**
- Produces, in `src/store/actions.rs`:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum Job { Dedupe, Reap, Promote, Judgement }            // as_str: dedupe|reap|promote|judgement
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum Kind { Merge, Supersede, Discard, Reap, Promote, Moment } // as_str: merge|supersede|discard|reap|promote|moment
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum UndoneBy { Operator, Evidence }                     // as_str: operator|evidence
  #[derive(Debug, Clone)]
  pub struct NewAction {
      pub job: Job, pub kind: Kind,
      pub subject_id: String, pub survivor_id: Option<String>,
      pub detail: Option<String>, pub evidence: serde_json::Value,
      /// The pair's cosine, for dedupe kinds; read by Part C's bands.
      pub pair_score: Option<f32>,
  }
  #[derive(Debug, Clone)]
  pub struct Action {
      pub id: String, pub at: i64, pub job: Job, pub kind: Kind,
      pub subject_id: String, pub survivor_id: Option<String>,
      pub detail: Option<String>, pub evidence_json: String, pub pair_score: Option<f32>,
      pub undone_at: Option<i64>, pub undone_by: Option<UndoneBy>, pub undone_reason: Option<String>,
  }
  pub(crate) async fn insert<'e, E: sqlx::Executor<'e, Database = sqlx::Sqlite>>(ex: E, a: &NewAction) -> Result<String>;
  impl Store {
      pub async fn record_action(&self, a: &NewAction) -> Result<String>;
      /// Open (not undone) rows of these kinds, oldest first, at most `limit`.
      pub async fn open_actions(&self, kinds: &[Kind], limit: usize) -> Result<Vec<Action>>;
      /// The open row on this subject of this kind, if any.
      pub async fn open_action_on(&self, subject_id: &str, kind: Kind) -> Result<Option<Action>>;
      /// Whether an action of this kind on this subject was ever taken back — the memory.
      pub async fn action_was_undone(&self, subject_id: &str, kind: Kind) -> Result<bool>;
      /// Stamp the open rows on `subject_id` of `kind`. Rows stamped.
      pub async fn undo_action_on(&self, subject_id: &str, kind: Kind, by: UndoneBy, reason: &str) -> Result<u64>;
      /// Stamp every open row whose survivor is `survivor_id` (a merge's originals).
      pub async fn undo_actions_under(&self, survivor_id: &str, by: UndoneBy, reason: &str) -> Result<u64>;
      /// Newest first, for disclosure.
      pub async fn recent_actions(&self, limit: usize) -> Result<Vec<Action>>;
  }
  ```
- `DecidedBy::Evidence` with `as_str() == "evidence"`, parsed leniently like the others.

- [x] **Step 1: Write the failing tests in `src/store/actions.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn merge_of(subject: &str, survivor: &str) -> NewAction {
        NewAction {
            job: Job::Dedupe,
            kind: Kind::Merge,
            subject_id: subject.into(),
            survivor_id: Some(survivor.into()),
            detail: Some("the same note twice".into()),
            evidence: serde_json::json!({ "pair_id": 7 }),
            pair_score: Some(0.91),
        }
    }

    #[tokio::test]
    async fn a_recorded_action_is_open_until_it_is_taken_back_and_then_remembered() {
        let store = Store::memory().await.unwrap();
        store.record_action(&merge_of("a", "m")).await.unwrap();
        store.record_action(&merge_of("b", "m")).await.unwrap();
        assert_eq!(store.open_actions(&[Kind::Merge], 10).await.unwrap().len(), 2);
        assert!(store.open_action_on("a", Kind::Merge).await.unwrap().is_some());
        assert!(!store.action_was_undone("a", Kind::Merge).await.unwrap());

        assert_eq!(
            store.undo_actions_under("m", UndoneBy::Evidence, "the survivor was not found").await.unwrap(),
            2
        );
        assert!(store.open_actions(&[Kind::Merge], 10).await.unwrap().is_empty());
        assert!(store.action_was_undone("a", Kind::Merge).await.unwrap());
        let a = store.recent_actions(10).await.unwrap();
        assert_eq!(a[0].undone_by, Some(UndoneBy::Evidence));
        assert_eq!(a[0].undone_reason.as_deref(), Some("the survivor was not found"));
    }

    #[tokio::test]
    async fn an_undo_stamps_only_the_open_rows_of_its_kind_on_its_subject() {
        let store = Store::memory().await.unwrap();
        store.record_action(&merge_of("a", "m")).await.unwrap();
        store
            .record_action(&NewAction { kind: Kind::Discard, survivor_id: None, ..merge_of("a", "m") })
            .await
            .unwrap();
        assert_eq!(store.undo_action_on("a", Kind::Discard, UndoneBy::Operator, "button").await.unwrap(), 1);
        assert!(store.open_action_on("a", Kind::Merge).await.unwrap().is_some());
        assert_eq!(store.undo_action_on("a", Kind::Discard, UndoneBy::Operator, "again").await.unwrap(), 0);
    }

    #[test]
    fn the_names_round_trip() {
        for k in [Kind::Merge, Kind::Supersede, Kind::Discard, Kind::Reap, Kind::Promote, Kind::Moment] {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
        }
        assert_eq!(UndoneBy::parse("evidence"), Some(UndoneBy::Evidence));
        assert_eq!(crate::store::pairs::DecidedBy::parse("evidence"), Some(crate::store::pairs::DecidedBy::Evidence));
    }
}
```

(Check `DecidedBy`'s parse function name at `src/store/pairs.rs:151-180` and use it.)

- [x] **Step 2: Run them to see them fail**

Run: `cargo test --lib -- actions::tests 2>&1 | tail -5`
Expected: compile error, no module `actions`.

- [x] **Step 3: Schema**

In `src/store/schema.sql`, after `graveyard`:

```sql
-- ── The corpus journal ───────────────────────────────────────────────────────
-- Every action a corpus job takes on its own: what was hidden, buried or
-- created, in favour of what, on what evidence, and whether it was later taken
-- back — by a person, or by the base on what use showed. The record the
-- ranking side has in `generations`, for the corpus. Rows are never deleted;
-- retention leaves them alone, as it leaves observations alone.
CREATE TABLE IF NOT EXISTS corpus_actions (
  id            TEXT PRIMARY KEY,
  at            INTEGER NOT NULL,
  -- dedupe | reap | promote | judgement
  job           TEXT NOT NULL,
  -- merge | supersede | discard | reap | promote | moment
  kind          TEXT NOT NULL,
  -- The artifact hidden or buried, the window promoted (`corpus_id#idx`), or
  -- the moment written.
  subject_id    TEXT NOT NULL,
  -- For merge and supersede: what now answers for the subject.
  survivor_id   TEXT,
  detail        TEXT,
  evidence_json TEXT NOT NULL,
  -- The pair's cosine, for the dedupe kinds. A column rather than a JSON
  -- path, because the review threshold's bands read it in aggregate.
  pair_score    REAL,
  undone_at     INTEGER,
  -- operator | evidence
  undone_by     TEXT,
  undone_reason TEXT
);
CREATE INDEX IF NOT EXISTS idx_corpus_actions_open
  ON corpus_actions(kind, at) WHERE undone_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_corpus_actions_subject
  ON corpus_actions(subject_id, kind);
CREATE INDEX IF NOT EXISTS idx_corpus_actions_survivor
  ON corpus_actions(survivor_id) WHERE survivor_id IS NOT NULL;
```

- [x] **Step 4: The module**

`src/store/actions.rs`, modelled on `src/store/observations.rs` (the generic `insert`, `new_id()`, `now()`, `Row` reads). The enums carry `as_str()` and `parse(&str) -> Option<Self>`. `insert` binds every field, `evidence` as `serde_json::to_string(&a.evidence)` mapped to `Error::Store`. Queries are literals:

```rust
// open_actions: one query per kind is simpler than an IN over a dynamic list —
// there are at most six kinds; collect and sort by `at`.
"SELECT ... FROM corpus_actions WHERE kind = ? AND undone_at IS NULL ORDER BY at ASC LIMIT ?"
// open_action_on
"SELECT ... FROM corpus_actions WHERE subject_id = ? AND kind = ? AND undone_at IS NULL ORDER BY at DESC LIMIT 1"
// action_was_undone
"SELECT 1 FROM corpus_actions WHERE subject_id = ? AND kind = ? AND undone_at IS NOT NULL LIMIT 1"
// undo_action_on
"UPDATE corpus_actions SET undone_at = ?, undone_by = ?, undone_reason = ? WHERE subject_id = ? AND kind = ? AND undone_at IS NULL"
// undo_actions_under
"UPDATE corpus_actions SET undone_at = ?, undone_by = ?, undone_reason = ? WHERE survivor_id = ? AND undone_at IS NULL"
// recent_actions
"SELECT ... FROM corpus_actions ORDER BY at DESC, id DESC LIMIT ?"
```

One private `fn read(r: &SqliteRow) -> Result<Action>` shared by the readers. `DecidedBy` in `src/store/pairs.rs` gains `Evidence` / `"evidence"` with a doc line: "the base, on what use showed — never the judge and never a person".

- [x] **Step 5: Run the gate**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked 2>&1 | tail -3`
Expected: green.

- [x] **Step 6: Commit**

```bash
git add -A src
git commit -m "feat(evolve): a journal of what the corpus jobs did to the base"
```

---

### Task 2: Every action site writes its row, and remembers what was taken back

**Files:**
- Modify: `src/core/ingest.rs:1158-1200` (`supersede` → `supersede_with`), `:1240-1262` (`deprecate` → `deprecate_with`)
- Modify: `src/jobs/dedupe.rs:405-540` (the three arms), `:559-640` (`discard_both`)
- Modify: `src/jobs/reap.rs:340-372` (`reap_one`), the candidate loop in `run` (memory)
- Modify: `src/store/artifacts.rs:1699-1740` (`bury` takes the vector, the model and the row)
- Modify: `src/store/schema.sql` (`graveyard.vec`, `graveyard.embed_model`), `src/store/mod.rs` (`ADDITIVE`)
- Modify: `src/jobs/promote.rs:70-86`, `src/jobs/judgement.rs:132-140`, `:318-330`

**Interfaces:**
- Produces: `Core::supersede_with(&self, loser_id, winner_id, journal: Option<NewAction>) -> Result<()>` with `supersede(l, w)` = `supersede_with(l, w, None)`; `Core::deprecate_with(&self, id, journal: Option<NewAction>)` likewise; `Store::bury(&self, id, meta_json, min_age_secs, vec: Option<&[f32]>, embed_model: Option<&str>, journal: &NewAction) -> Result<()>`; `promote::window_key(corpus_id: &str, idx: i64) -> String` (`format!("{corpus_id}#{idx}")`).

- [x] **Step 1: Write the failing tests**

In `src/jobs/dedupe.rs` tests, beside the existing tests that drive `apply` through the fake judge (find the ones asserting `set_pair_merged` / `supersede` / `discard_both` outcomes and copy their fixtures):

```rust
#[tokio::test]
async fn a_merge_journals_one_row_per_original_naming_the_merge() {
    let (core, a, b, pair) = judged_as_duplicate().await; // the fixture the Duplicate test uses
    run(&core, pair).await.unwrap();
    let merged = core.store.get_pair(pair).await.unwrap().merged_into.unwrap();
    let rows = core.store.open_actions(&[Kind::Merge], 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.survivor_id.as_deref() == Some(merged.as_str())));
    assert_eq!(rows.iter().map(|r| r.subject_id.clone()).collect::<HashSet<_>>(), [a, b].into());
    assert!(rows[0].pair_score.is_some());
}

#[tokio::test]
async fn a_replacement_journals_the_loser_naming_the_winner() { /* Replaced fixture; one Kind::Supersede row, subject = obsolete, survivor = winner */ }

#[tokio::test]
async fn a_discard_by_the_judge_journals_both_sides_and_one_by_a_person_journals_nothing() {
    // Vacuous fixture → two Kind::Discard rows, job Dedupe, survivor None.
    // discard_both(.., DecidedBy::Operator) on another pair → no new rows.
}

#[tokio::test]
async fn an_action_taken_back_is_not_taken_again_on_the_same_subject() {
    let (core, a, _b, pair) = judged_as_replacement().await;
    core.store
        .record_action(&NewAction { job: Job::Dedupe, kind: Kind::Supersede, subject_id: a.clone(), survivor_id: None, detail: None, evidence: serde_json::json!({}), pair_score: None })
        .await.unwrap();
    core.store.undo_action_on(&a, Kind::Supersede, UndoneBy::Operator, "put back").await.unwrap();
    run(&core, pair).await.unwrap();
    let p = core.store.get_pair(pair).await.unwrap();
    assert_eq!(p.state, PairState::Contradiction, "handed to a person, not repeated");
    assert!(core.store.get_artifact(&a).await.unwrap().superseded_by.is_none());
}
```

In `src/jobs/reap.rs` tests, extend the test that buries (`:700-740`): after `run`, `open_actions(&[Kind::Reap], 10)` has one row with `subject_id == id` and `detail == Some(reason)`; `graveyard_row` still works; and a new reader `graveyard_vectors(embed_model)` (task 5 adds it — here assert through `sqlx::query_scalar("SELECT vec IS NOT NULL FROM graveyard WHERE id = ?")`) is `1`. Add: a candidate with an undone `Kind::Reap` row on it is skipped by `run` — `reaped_at` stays NULL and no new row is written.

In `src/jobs/promote.rs` tests: after a promotion is armed, one `Kind::Promote` row with `subject_id == window_key(corpus, idx)` and `evidence["passage"] == id`.

In `src/jobs/judgement.rs` tests: after `apply` writes a due moment, one `Kind::Moment` row whose `subject_id` is the moment's id (read it from `store.moments_for(anchor)` or whatever the module's tests use).

- [x] **Step 2: Run them to see them fail**

Run: `cargo test --lib -- dedupe::tests::a_merge_journals reap::tests promote::tests judgement::tests 2>&1 | grep -E '^test |error' | head`
Expected: compile errors on the new names.

- [x] **Step 3: `Core` methods take the row**

In `src/core/ingest.rs`:

```rust
pub async fn supersede(&self, loser_id: &str, winner_id: &str) -> Result<()> {
    self.supersede_with(loser_id, winner_id, None).await
}

/// `supersede`, journaling what a corpus job did. The row is written under
/// the lifecycle guard after the artifact is hidden, so nothing can read a
/// hidden artifact with no row, or a row for an artifact still live.
pub async fn supersede_with(
    &self,
    loser_id: &str,
    winner_id: &str,
    journal: Option<crate::store::actions::NewAction>,
) -> Result<()> {
    let _guard = self.lifecycle_lock.lock().await;
    // ...the existing body, unchanged...
    if let Some(a) = journal {
        self.store.record_action(&a).await?;
    }
    // ...the follow_supersession tail, unchanged...
    Ok(())
}
```

Place the `record_action` right after `clear_lifecycle_dirty`, before `follow_supersession` (whose failure is logged, not returned). Same shape for `deprecate` / `deprecate_with`.

- [x] **Step 4: The dedupe arms**

A helper at the top of `apply`'s module:

```rust
fn action(kind: Kind, pair: &ArtifactPair, subject: &str, survivor: Option<&str>, detail: Option<&str>) -> NewAction {
    NewAction {
        job: Job::Dedupe,
        kind,
        subject_id: subject.to_string(),
        survivor_id: survivor.map(str::to_string),
        detail: detail.map(str::to_string),
        evidence: serde_json::json!({ "pair_id": pair.id, "a": pair.a_id, "b": pair.b_id }),
        pair_score: Some(pair.score),
    }
}
```

`Replaced`: before `core.supersede(..)`, the memory check:

```rust
if core.store.action_was_undone(&obsolete, Kind::Supersede).await? {
    return settle(core, &s.pair, PairState::Contradiction,
        Some("This replacement was applied before and taken back. Resolve by hand.")).await;
}
core.supersede_with(&obsolete, &winner, Some(action(Kind::Supersede, &s.pair, &obsolete, Some(&winner), s.detail.as_deref()))).await?;
```

`Duplicate`: the same check over every id in `sources` for `Kind::Merge`, same Contradiction detail with "merge"; after `merge::write` succeeds, one `record_action` per source with `survivor = m.id`, written before `set_pair_merged`.

`Vacuous` → `discard_both`: the check over both sides for `Kind::Discard` when `by == DecidedBy::Model` (route the Contradiction through `settle_as(.., by)`); the retire loop becomes

```rust
for id in &retire {
    let journal = matches!(by, DecidedBy::Model)
        .then(|| action(Kind::Discard, pair, id, None, detail));
    core.deprecate_with(id, journal).await?;
}
```

- [x] **Step 5: Reap**

`graveyard` gains, in the schema and on `ADDITIVE` ("both nullable, no default; NULL is the truth about every row buried before the vector was kept"):

```sql
  -- The dense vector the point carried and the model that made it, kept so a
  -- give-up can be compared with what was buried without an embedding. NULL
  -- for rows buried before this column, and for a point the store no longer
  -- had.
  vec         BLOB,
  embed_model TEXT
```

`Store::bury` gains `vec: Option<&[f32]>`, `embed_model: Option<&str>`, `journal: &NewAction`; the INSERT selects `?, ?` for the two new columns (bind `vec.map(crate::store::feedback::vec_to_blob)` and the model) and, inside the same transaction after the UPDATE, `crate::store::actions::insert(&mut *tx, journal).await?`.

`reap_one`:

```rust
let dense = core.vectors.dense_of(&c.id).await.unwrap_or_else(|e| {
    tracing::warn!(id = %c.id, error = %e, "buried without its vector");
    None
});
let journal = NewAction {
    job: Job::Reap, kind: Kind::Reap,
    subject_id: c.id.clone(), survivor_id: None,
    detail: Some(reason.to_string()),
    evidence: serde_json::json!({ "status": c.status.as_str(), "retired_at": c.retired_at, "created_at": c.created_at }),
    pair_score: None,
};
let _guard = core.lifecycle_lock.lock().await;
core.store.bury(&c.id, &meta, min_age_secs, dense.as_deref(), c.embed_model.as_deref(), &journal).await?;
```

(`Chunk.embed_model` — check the field name on `Chunk` in `src/store/artifacts.rs`; the column exists because `exhume` sets it NULL.) The memory: in `reap::run`'s loop over nominees, before judging, `if core.store.action_was_undone(&c.id, Kind::Reap).await? { continue; }` — a judge call is the expensive step, so the check goes before it.

- [x] **Step 6: Promote and judgement**

`promote.rs`, after `rearm_idle_seq`:

```rust
pub fn window_key(corpus_id: &str, idx: i64) -> String { format!("{corpus_id}#{idx}") }
// ...
core.store.record_action(&NewAction {
    job: Job::Promote, kind: Kind::Promote,
    subject_id: window_key(corpus_id, idx), survivor_id: None,
    detail: None,
    evidence: serde_json::json!({ "passage": id, "activation": earned }),
    pair_score: None,
}).await?;
```

`judgement.rs`: both `insert_moment` calls capture the returned id and record `Kind::Moment` with `job: Job::Judgement`, `subject_id: moment_id`, `detail: Some("event")` / `Some("due")`, `evidence: json!({ "artifact": anchor_id })`. The event site is inside an `if let Err(err) = ...` best-effort block; keep it best-effort: on `Ok(id)`, record and `warn!` on failure rather than return.

- [x] **Step 7: Run the gate**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked 2>&1 | tail -3`
Expected: green. `bury`'s test callers need the three new arguments (`None, None, &journal`).

- [x] **Step 8: Commit**

```bash
git add -A src
git commit -m "feat(evolve): every corpus action writes its row, and what was taken back is not taken again"
```

---

### Task 3: The operator's undos stamp the journal

**Files:**
- Modify: `src/jobs/merge.rs:182-240` (`undo` takes `by: DecidedBy`)
- Modify: `src/web/ui.rs:2662` (`undo_merge_ui`), `:2984` (`unsupersede_ui`), `:3125` (`reactivate_ui`), `:1960` (`unpromote_ui`)
- Modify: `src/web/due.rs:488` (`not_a_reminder`)

**Interfaces:**
- Produces: `merge::undo(core, merged_id, by: DecidedBy) -> Result<()>`; every handler above stamps with `UndoneBy::Operator` and a reason naming the button.

- [x] **Step 1: Write the failing tests**

In `src/web/ui.rs` tests (the module has handler tests with a test tenant; find the one for `undo_merge_ui` or `reactivate_ui` and copy its setup):

```rust
#[tokio::test]
async fn pressing_undo_on_a_merge_stamps_its_rows_as_taken_back_by_the_operator() {
    // seed a merge with two journal rows (record_action ×2, survivor = merge id)
    // POST /ui/ops/merges/{merge}/undo
    // open_actions(&[Kind::Merge]) is empty; recent_actions()[0].undone_by == Some(UndoneBy::Operator)
}

#[tokio::test]
async fn reactivating_a_discarded_or_reaped_artifact_stamps_its_row() { /* Kind::Discard row; POST reactivate; stamped. Then a Kind::Reap row on a buried artifact; POST reactivate; stamped and exhumed. */ }

#[tokio::test]
async fn unsuperseding_stamps_the_supersede_row_and_unpromoting_stamps_the_window() { /* two cases */ }
```

In `src/web/due.rs` tests: `not-a-reminder` on a moment with a `Kind::Moment` row stamps it.

- [x] **Step 2: Run them to see them fail**

Run: `cargo test --lib -- ui::tests::pressing_undo ui::tests::reactivating ui::tests::unsuperseding due::tests 2>&1 | grep -E '^test ' | head`
Expected: FAIL on the stamp assertions.

- [x] **Step 3: Stamp**

`merge::undo(core, merged_id, by)`: replace both `DecidedBy::Operator` literals with `by`. `undo_merge_ui` calls `undo(&core, &aid, DecidedBy::Operator)` then `store.undo_actions_under(&aid, UndoneBy::Operator, "undone on Insights")`. `unsupersede_ui`: after `core.unsupersede(&aid)`, `undo_action_on(&aid, Kind::Supersede, Operator, "unsuperseded on Insights")`. `reactivate_ui`: after `core.reactivate(&aid)`, stamp both `Kind::Discard` and `Kind::Reap` (the one that is open stamps 1, the other 0). `unpromote_ui`: `undo_action_on(&promote::window_key(&cid, idx), Kind::Promote, Operator, "unpromoted")`. `not_a_reminder`: `undo_action_on(&id, Kind::Moment, Operator, "not a reminder")` for the moment id in hand (the handler reads `m` first; stamp the rows of every moment on `m.artifact_id` that `set_reminder(false)` removes, which is what `store.moments_for(...)` lists — or, simpler and honest, the one moment `id` the button was on).

Stamp after the action succeeds, never before.

- [x] **Step 4: Run the gate**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked 2>&1 | tail -3`
Expected: green.

- [x] **Step 5: Commit**

```bash
git add -A src
git commit -m "feat(evolve): the operator's undos stamp the journal"
```

---

### Task 4: Rule 1 — a survivor must still be found

**Files:**
- Create: `src/jobs/retract.rs`
- Modify: `src/jobs/mod.rs` (`pub mod retract;`)
- Modify: `src/store/observations.rs` (`observations_naming`, `Observation.embed_model`)
- Modify: `src/jobs/tune.rs:88-150` (`pass` runs the corpus half after the anchor), `Pass` gains two counts
- Modify: `src/jobs/retention.rs:85-95` (`Report` carries them)
- Modify: `src/eval/sweep.rs` (`OBSERVATION_LIMIT`, `Pair`, `rank_of`, `recommend` become `pub(crate)` where they are not)

**Interfaces:**
- Produces:
  ```rust
  // src/store/observations.rs
  pub struct Observation { /* existing */ pub embed_model: String }
  impl Store {
      /// Positive observations naming `artifact_id`, made before `before`, under any
      /// generation of the era `(embed_recipe, chat_model)`. Newest first, at most `limit`.
      pub async fn observations_naming(&self, artifact_id: &str, before: i64, embed_recipe: &str, chat_model: &str, limit: usize) -> Result<Vec<Observation>>;
  }
  // src/jobs/retract.rs
  #[derive(Debug, Default, Clone, Copy, serde::Serialize)]
  pub struct Retracted { pub reconsidered: usize, pub undone: usize, pub restored: usize }
  pub async fn run(core: &Core, live: &Generation, started: i64) -> Result<Retracted>;
  pub(crate) async fn rule_one(core: &Core, live: &Generation, started: i64) -> Result<(usize, usize, bool)>; // (reconsidered, undone, stopped)
  // src/jobs/tune.rs
  pub struct Pass { pub adopted: Option<String>, pub reverted: Option<String>, pub undone: usize, pub restored: usize }
  ```

- [x] **Step 1: Write the failing tests in `src/jobs/retract.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::sweep::test_support::{QUERY, seeded};
    use crate::store::actions::{Job, Kind, NewAction, UndoneBy};
    use crate::store::observations::{NewObservation, Source};

    async fn generation_for(core: &Core) -> Generation { /* record_generation as tune's tests do, then live_generation().unwrap() */ }

    async fn observed(core: &Core, generation: &str, artifact: &str, rank: i64) {
        let query_vec = core.embedder.embed_query(QUERY).await.unwrap();
        core.store.record_observation(&NewObservation {
            generation_id: generation.into(), query: QUERY.into(), query_vec,
            embed_model: "fake".into(), artifact_id: Some(artifact.into()), rank: Some(rank),
            source: Source::Cited, event_id: None,
        }).await.unwrap();
    }

    /// The seeded base, with the top hit of the second source (order[3])
    /// superseded by the last hit of the first (order[2]) on the judge's word,
    /// after use had named order[3] at ranks 1 and 1.
    async fn superseded_after_use() -> (Core, Generation, String, String) {
        let (core, order) = seeded().await;
        let g = generation_for(&core).await;
        observed(&core, &g.id, &order[3], 1).await;
        observed(&core, &g.id, &order[3], 1).await;
        let (loser, winner) = (order[3].clone(), order[2].clone());
        core.supersede_with(&loser, &winner, Some(NewAction {
            job: Job::Dedupe, kind: Kind::Supersede, subject_id: loser.clone(),
            survivor_id: Some(winner.clone()), detail: None, evidence: serde_json::json!({}), pair_score: Some(0.9),
        })).await.unwrap();
        (core, g, loser, winner)
    }

    #[tokio::test]
    async fn a_supersession_whose_survivor_ranks_two_net_pairs_worse_is_taken_back_on_evidence() {
        let (core, g, loser, winner) = superseded_after_use().await;
        // Served at rank 1 twice; the survivor replays at wherever order[2] sits — worse than 0.
        let (reconsidered, undone, stopped) = rule_one(&core, &g, crate::store::now()).await.unwrap();
        assert_eq!((reconsidered, undone, stopped), (1, 1, false));
        assert!(core.store.get_artifact(&loser).await.unwrap().superseded_by.is_none(), "unsuperseded");
        let row = core.store.recent_actions(1).await.unwrap().remove(0);
        assert_eq!(row.undone_by, Some(UndoneBy::Evidence));
        assert_eq!(row.survivor_id.as_deref(), Some(winner.as_str()));
        assert!(core.store.action_was_undone(&loser, Kind::Supersede).await.unwrap());
    }

    #[tokio::test]
    async fn a_supersession_nobody_had_used_has_no_evidence_and_is_left_alone() {
        let (core, order) = seeded().await;
        let g = generation_for(&core).await;
        core.supersede_with(&order[3], &order[2], Some(/* as above */)).await.unwrap();
        assert_eq!(rule_one(&core, &g, crate::store::now()).await.unwrap(), (0, 0, false));
        assert!(core.store.get_artifact(&order[3]).await.unwrap().superseded_by.is_some());
    }

    #[tokio::test]
    async fn a_survivor_that_ranks_as_well_as_the_subject_did_holds() {
        // Observe order[3] at rank 4 twice (where it sat uncapped), supersede by order[0]
        // (rank 0 always): the survivor replays better, recommend refuses, nothing undone.
    }

    #[tokio::test]
    async fn observations_from_another_era_are_not_evidence() {
        // Same as the first test, but the observations are under a generation with
        // embed_recipe "other": rule_one sees none; (0, 0, false).
    }

    #[tokio::test]
    async fn a_merge_is_taken_back_whole_when_one_original_is_no_longer_found() {
        // Build a merge through jobs::merge::write over order[3], order[4] (see merge.rs tests
        // for a MergedDraft), finish it, record two Kind::Merge rows with survivor = merge id,
        // observe order[3] at rank 1 twice under the live era before `at`. rule_one undoes the
        // merge via merge::undo(.., DecidedBy::Evidence): both rows stamped, both originals active.
    }

    #[tokio::test]
    async fn the_rule_stops_between_subjects_when_somebody_comes_back() {
        // Two superseded-after-use subjects; record a search event after `started`;
        // rule_one returns stopped == true and undoes at most one.
    }
}
```

- [x] **Step 2: Run them to see them fail**

Run: `cargo test --lib -- retract::tests 2>&1 | tail -5`
Expected: no module `retract`.

- [x] **Step 3: The observation reader**

`Observation` gains `embed_model: String` (select it in both readers). New reader:

```rust
pub async fn observations_naming(&self, artifact_id: &str, before: i64, embed_recipe: &str, chat_model: &str, limit: usize) -> Result<Vec<Observation>> {
    sqlx::query(
        "SELECT o.id, o.created_at, o.generation_id, o.query, o.query_vec, o.artifact_id,
                o.rank, o.source, o.strength, o.event_id, o.embed_model
           FROM observations o
           JOIN generations g ON g.id = o.generation_id
          WHERE o.artifact_id = ? AND o.created_at < ? AND o.strength > 0
            AND o.excluded_at IS NULL
            AND g.embed_recipe = ? AND g.chat_model = ?
          ORDER BY o.created_at DESC, o.id DESC
          LIMIT ?",
    )
    // ...binds, map through the shared row reader...
}
```

Factor the existing row-to-`Observation` mapping into `fn read(r: &SqliteRow) -> Result<Observation>` so both readers share it.

- [x] **Step 4: The module**

```rust
//! The corpus jobs answer to the same evidence the ranking side answers to.
//!
//! Two rules, both read inside the idle pass after the anchor check, under
//! the same claim and the same switch. Rule 1 asks whether what a merge or a
//! supersession hid is still found through what now answers for it; rule 2
//! asks whether a search that was given up on would have been answered by
//! something the base hid or buried. Either way the base takes its own action
//! back through the same `Core` method the operator's button calls, and
//! stamps the journal row as taken back on evidence.

use crate::core::Core;
use crate::error::Result;
use crate::eval::sweep::{self, Pair};
use crate::store::actions::{Action, Kind, UndoneBy};
use crate::store::generations::Generation;
use crate::store::pairs::DecidedBy;

/// Rows one pass will reconsider. A bound on work, like `OBSERVATION_LIMIT`.
const ACTION_LIMIT: usize = 200;

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Retracted { pub reconsidered: usize, pub undone: usize, pub restored: usize }

pub async fn run(core: &Core, live: &Generation, started: i64) -> Result<Retracted> {
    let (reconsidered, undone, stopped) = rule_one(core, live, started).await?;
    let mut out = Retracted { reconsidered, undone, restored: 0 };
    if stopped { return Ok(out); }
    // rule_two arrives in the next task
    Ok(out)
}

/// Rule 1. Returns (subjects reconsidered, actions undone, stopped early).
pub(crate) async fn rule_one(core: &Core, live: &Generation, started: i64) -> Result<(usize, usize, bool)> {
    let current = *core.ranking.read().expect("ranking lock");
    let mut reconsidered = 0;
    let mut undone = 0;
    for a in core.store.open_actions(&[Kind::Merge, Kind::Supersede], ACTION_LIMIT).await? {
        if core.store.activity_since(started).await? {
            return Ok((reconsidered, undone, true));
        }
        // A merge's rows are stamped together by the first original that
        // fails; a row already stamped this pass is skipped by the read below.
        if core.store.open_action_on(&a.subject_id, a.kind).await?.is_none() {
            continue;
        }
        let named = core.store
            .observations_naming(&a.subject_id, a.at, &live.embed_recipe, &live.chat_model, sweep::OBSERVATION_LIMIT)
            .await?;
        if named.is_empty() {
            continue;
        }
        reconsidered += 1;
        let satisfies = crate::eval::satisfied_by(core, &a.subject_id).await;
        let mut observed = Vec::with_capacity(named.len());
        let mut replayed = Vec::with_capacity(named.len());
        for o in named {
            let pair = Pair {
                query: o.query, satisfies: satisfies.clone(), query_vec: Some(o.query_vec),
                priming: None, served: o.rank.map(|r| (r - 1).max(0) as usize),
            };
            observed.push(pair.served);
            replayed.push(sweep::rank_of(core, &pair, current, false).await?);
        }
        // `recommend` pointed the other way: the subject's record is the
        // candidate, the survivor's replay is the base. When the record clears
        // the gate, the survivor lost what the subject had.
        if !sweep::recommend(&replayed, &observed) {
            continue;
        }
        let reason = format!("what it hid was found better than it is, over {} observations", observed.len());
        match a.kind {
            Kind::Merge => {
                let survivor = a.survivor_id.clone().expect("a merge row names its merge");
                crate::jobs::merge::undo(core, &survivor, DecidedBy::Evidence).await?;
                undone += core.store.undo_actions_under(&survivor, UndoneBy::Evidence, &reason).await? as usize;
            }
            Kind::Supersede => {
                core.unsupersede(&a.subject_id).await?;
                undone += core.store.undo_action_on(&a.subject_id, Kind::Supersede, UndoneBy::Evidence, &reason).await? as usize;
            }
            _ => unreachable!("only merge and supersede are read"),
        }
        tracing::info!(subject = %a.subject_id, kind = a.kind.as_str(), "took a corpus action back on evidence");
    }
    Ok((reconsidered, undone, false))
}
```

`sweep::OBSERVATION_LIMIT`, `Pair`, `rank_of`, `recommend` need `pub(crate)` visibility (`recommend` is `pub` already).

- [x] **Step 5: The pass runs the corpus half**

In `src/jobs/tune.rs` `pass()`, right after the anchor check and before the params-mismatch check:

```rust
    // The corpus half, before the ranking half's own gates: a base under
    // watch, or one whose parameters drifted, still answers for what it hid.
    let started = crate::store::now();
    let retracted = crate::jobs::retract::run(core, &live, started).await?;
    let mut out = Pass { undone: retracted.undone, restored: retracted.restored, ..Default::default() };
```

and every later `return Ok(Pass::default())` becomes `return Ok(out)`; the `revert(..)` and `propose(..)` results are merged into `out` (`out.adopted = p.adopted; out.reverted = p.reverted;`). `Pass` gains `pub undone: usize, pub restored: usize`. `retention::Report` gains `undone` and `restored` (flat, so `did_work` sees them), filled from the pass.

Add to `src/jobs/tune.rs` tests:

```rust
#[tokio::test]
async fn the_corpus_half_runs_while_the_ranking_half_is_under_watch() {
    let (core, _) = test_support::adopted_and_watching().await;
    // superseded-after-use on this base (as retract's fixture), then:
    let p = run_pass(&core).await; // whatever returns Pass here — `pass(&core)`
    assert_eq!(p.undone, 1);
    assert!(p.adopted.is_none(), "still under watch");
}

#[tokio::test]
async fn an_untrustworthy_anchor_stops_the_corpus_rules_too() {
    let (core, _) = test_support::suspended().await;
    // superseded-after-use; pass(&core).undone == 0 and the artifact stays superseded.
}
```

- [x] **Step 6: Run the gate**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked 2>&1 | tail -3`
Expected: green.

- [x] **Step 7: Commit**

```bash
git add -A src
git commit -m "feat(evolve): rule 1 — a survivor must still be found, or the base takes the merge back"
```

---

### Task 5: Rule 2 — a give-up that a hidden artifact would have answered

**Files:**
- Modify: `src/jobs/retract.rs` (`rule_two`, wired into `run`)
- Modify: `src/store/observations.rs` (`gave_ups_since`)
- Modify: `src/store/artifacts.rs` (`graveyard_vectors`)
- Modify: `src/store/mod.rs` or wherever `meta_get`/`meta_set` live (cursor key)

**Interfaces:**
- Produces:
  ```rust
  impl Store {
      /// Give-ups recorded after `after` (a `created_at`), oldest first, at most `limit`.
      pub async fn gave_ups_since(&self, after: i64, limit: usize) -> Result<Vec<Observation>>;
      /// Every buried vector made by `embed_model`: `(artifact_id, vec)`.
      pub async fn graveyard_vectors(&self, embed_model: &str) -> Result<Vec<(String, Vec<f32>)>>;
  }
  pub(crate) async fn rule_two(core: &Core, started: i64) -> Result<(usize, bool)>; // (restored, stopped)
  const GAVE_UP_AFTER: &str = "evolve.retract.gave_up_after";
  ```

- [x] **Step 1: Write the failing tests in `src/jobs/retract.rs`**

```rust
/// A give-up on QUERY under the live generation.
async fn gave_up(core: &Core, generation: &str) { /* NewObservation with Source::GaveUp, artifact None, rank None, event_id None */ }

#[tokio::test]
async fn a_give_up_that_a_discarded_artifact_would_have_topped_restores_it() {
    let (core, order) = seeded().await;
    let g = generation_for(&core).await;
    // Discard the top hit for QUERY by the judge's word: it is the best answer there is.
    core.deprecate_with(&order[0], Some(NewAction { kind: Kind::Discard, subject_id: order[0].clone(), survivor_id: None, job: Job::Dedupe, detail: None, evidence: serde_json::json!({}), pair_score: None })).await.unwrap();
    gave_up(&core, &g.id).await;
    let (restored, stopped) = rule_two(&core, crate::store::now()).await.unwrap();
    assert_eq!((restored, stopped), (1, false));
    assert!(core.store.get_artifact(&order[0]).await.unwrap().in_results());
    assert!(core.store.action_was_undone(&order[0], Kind::Discard).await.unwrap());
}

#[tokio::test]
async fn a_give_up_the_live_list_answers_better_restores_nothing() {
    // Discard order[5] (the worst hit): the best hidden hit is less similar than the best live; nothing restored.
}

#[tokio::test]
async fn an_artifact_a_person_hid_is_not_the_base_s_to_restore() {
    // core.deprecate(&order[0]) with no journal row; a give-up; rule_two restores nothing.
}

#[tokio::test]
async fn a_buried_artifact_is_exhumed_by_cosine_and_re_embedded() {
    // Retire order[0], bury it through reap_one's path (or Store::bury with dense_of's vector,
    // embed_model "fake", and a Kind::Reap row); a give-up; rule_two → restored 1;
    // get_artifact(order[0]).reaped_at is None, text restored, an Embed job is queued
    // (store.pending_jobs or however reap's tests check it); row stamped Evidence.
}

#[tokio::test]
async fn a_buried_vector_from_another_model_is_not_compared() {
    // Same, with embed_model "other" on the grave: nothing restored.
}

#[tokio::test]
async fn give_ups_are_read_once_and_the_cursor_moves() {
    // Two passes over one give-up: the second sees none (meta cursor).
}
```

- [x] **Step 2: Run them to see them fail**

Run: `cargo test --lib -- retract::tests::a_give_up retract::tests::a_buried retract::tests::give_ups 2>&1 | grep -E '^test |error' | head`
Expected: compile errors on `rule_two`.

- [x] **Step 3: The readers**

`gave_ups_since`: `WHERE source = 'gave_up' AND created_at > ? AND excluded_at IS NULL ORDER BY created_at ASC, id ASC LIMIT ?`, through the shared row reader.

`graveyard_vectors`: `SELECT id, vec FROM graveyard WHERE vec IS NOT NULL AND embed_model = ?`, `blob_to_vec` on each.

- [x] **Step 4: The rule**

```rust
const GAVE_UP_AFTER: &str = "evolve.retract.gave_up_after";

/// Rule 2. Returns (artifacts restored, stopped early).
pub(crate) async fn rule_two(core: &Core, started: i64) -> Result<(usize, bool)> {
    let current = *core.ranking.read().expect("ranking lock");
    let after: i64 = core.store.meta_get(GAVE_UP_AFTER).await?.and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut restored = 0;
    let mut cursor = after;
    for o in core.store.gave_ups_since(after, sweep::OBSERVATION_LIMIT).await? {
        if core.store.activity_since(started).await? {
            core.store.meta_set(GAVE_UP_AFTER, &cursor.to_string()).await?;
            return Ok((restored, true));
        }
        cursor = o.created_at;
        core.remember_query_vector(&o.query, o.query_vec.clone());
        let q = crate::core::search::SearchQuery {
            q: o.query.clone(), limit: sweep::LIMIT, tags: vec![], category: None,
            mark: false, rerank: false, explain: false,
            include_deprecated: true, include_superseded: true,
        };
        let (hits, _) = core.search_with_ranking(&q, current, crate::store::feedback::Door::Judge).await?;
        let best_live = hits.iter()
            .filter(|h| h.status.is_none_or(|s| s == ArtifactStatus::Active) && h.superseded_by.is_none())
            .filter_map(|h| h.similarity).fold(0.0f32, f32::max);
        // The best hidden hit the base itself hid, with the row that says so.
        let mut best_hidden: Option<(f32, Action)> = None;
        for h in hits.iter().filter(|h| h.similarity.is_some_and(|s| s > best_live)) {
            let hidden = h.superseded_by.is_some() || h.status.is_some_and(|s| s != ArtifactStatus::Active);
            if !hidden { continue; }
            for kind in [Kind::Discard, Kind::Supersede, Kind::Merge] {
                if let Some(a) = core.store.open_action_on(&h.artifact_id, kind).await? {
                    let s = h.similarity.expect("filtered");
                    if best_hidden.as_ref().is_none_or(|(b, _)| s > *b) {
                        best_hidden = Some((s, a));
                    }
                }
            }
        }
        // And the graveyard, by cosine over what was buried by the same model.
        for (id, vec) in core.store.graveyard_vectors(&o.embed_model).await? {
            let s = crate::vector::cosine(&o.query_vec, &vec);
            if s > best_live && best_hidden.as_ref().is_none_or(|(b, _)| s > *b)
                && let Some(a) = core.store.open_action_on(&id, Kind::Reap).await?
            {
                best_hidden = Some((s, a));
            }
        }
        let Some((sim, a)) = best_hidden else { continue; };
        let reason = format!("a search given up on would have been answered by it (cosine {sim:.2} against {best_live:.2} live)");
        match a.kind {
            Kind::Discard | Kind::Reap => {
                core.reactivate(&a.subject_id).await?;
                core.store.undo_action_on(&a.subject_id, a.kind, UndoneBy::Evidence, &reason).await?;
            }
            Kind::Supersede => {
                core.unsupersede(&a.subject_id).await?;
                core.store.undo_action_on(&a.subject_id, Kind::Supersede, UndoneBy::Evidence, &reason).await?;
            }
            Kind::Merge => {
                let survivor = a.survivor_id.clone().expect("a merge row names its merge");
                crate::jobs::merge::undo(core, &survivor, DecidedBy::Evidence).await?;
                core.store.undo_actions_under(&survivor, UndoneBy::Evidence, &reason).await?;
            }
            _ => continue,
        }
        restored += 1;
        tracing::info!(subject = %a.subject_id, kind = a.kind.as_str(), sim, best_live, "restored what a give-up would have been answered by");
    }
    core.store.meta_set(GAVE_UP_AFTER, &cursor.to_string()).await?;
    Ok((restored, false))
}
```

Check `SearchResult.status`'s type (`Option<ArtifactStatus>` per `src/core/search.rs:157-163`) and `meta_get`/`meta_set` names on `Store` (`jobs/promote.rs` and `jobs/observe.rs` use them). Wire `rule_two` into `run` after rule 1, honouring `stopped`. An artifact restored this pass is live for the next give-up's replay, which is the right order.

- [x] **Step 5: Run the gate**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked 2>&1 | tail -3`
Expected: green.

- [x] **Step 6: Commit**

```bash
git add -A src
git commit -m "feat(evolve): rule 2 — a give-up a hidden artifact would have answered restores it"
```

---

### Task 6: The graveyard is listed, and Insights says what the base did to itself

**Files:**
- Modify: `src/store/artifacts.rs` (`graveyard_list`)
- Modify: `src/web/insights.rs` (`GraveRow`, `reaped: Vec<GraveRow>`, `more_reaped`, `EvolveView.actions`, `EvolveView.rules`), `src/web/templates/insights.html` (Reaped section after "Hidden as stale"), `src/web/templates/_evolve.html` (two folded lists)
- Modify: `src/jobs/retract.rs` (`run` writes `evolve.retract.last` to meta)

**Interfaces:**
- Produces: `Store::graveyard_list(limit: i64) -> Result<Vec<Grave>>` with `pub struct Grave { pub id: String, pub title: Option<String>, pub reaped_at: i64, pub reason: Option<String> }` (reason from `meta_json.reason`); `EvolveView { /* existing */ pub actions: Vec<String>, pub rules: Option<String> }`; meta key `evolve.retract.last` holding `Retracted` plus `at` as JSON.

- [x] **Step 1: Write the failing tests**

In `src/web/insights.rs` tests (the module renders the page with a test tenant; find `evolve_view` tests from stage 2):

```rust
#[tokio::test]
async fn the_reaped_section_lists_what_is_buried_with_a_restore_button() {
    // bury one artifact with a reason; GET /ui/insights body contains its title,
    // "Reaped", and `action="/ui/ops/artifacts/{id}/reactivate"`.
}

#[tokio::test]
async fn the_evolve_section_tells_an_evidence_undo_from_an_operator_undo() {
    // two journal rows, one stamped Evidence, one Operator; evolve_view(core).actions has two
    // sentences, one containing "taken back on evidence", one "undone by you".
}

#[tokio::test]
async fn the_evolve_section_says_what_the_rules_last_did() {
    // meta evolve.retract.last = {"at":..,"reconsidered":3,"undone":1,"restored":0};
    // evolve_view(core).rules == Some("…reconsidered 3 …took back 1 …restored 0…") — assert on the numbers.
}
```

In `src/store/artifacts.rs` tests: `graveyard_list(10)` after a `bury` returns the row with its reason and title.

- [x] **Step 2: Run them to see them fail**

Run: `cargo test --lib -- insights::tests::the_reaped insights::tests::the_evolve artifacts::tests::graveyard_list 2>&1 | grep -E '^test |error' | head`
Expected: compile errors.

- [x] **Step 3: The listing**

```rust
pub struct Grave { pub id: String, pub title: Option<String>, pub reaped_at: i64, pub reason: Option<String> }

pub async fn graveyard_list(&self, limit: i64) -> Result<Vec<Grave>> {
    sqlx::query("SELECT id, title, meta_json, reaped_at FROM graveyard ORDER BY reaped_at DESC LIMIT ?")
        .bind(limit).fetch_all(&self.pool).await?
        .iter()
        .map(|r| {
            let meta: serde_json::Value = serde_json::from_str(&r.get::<String, _>("meta_json")).unwrap_or_default();
            Ok(Grave {
                id: r.get("id"), title: r.get("title"), reaped_at: r.get("reaped_at"),
                reason: meta.get("reason").and_then(|v| v.as_str()).map(str::to_string),
            })
        })
        .collect()
}
```

`graveyard_row`'s "for tests and nothing else today" comment is now wrong; fix it.

- [x] **Step 4: The page**

In `page`, fetch `graveyard_list(DEPRECATED_CAP + 1)`, set `more_reaped`, truncate, map to `GraveRow { id, title, ago: ago(reaped_at), reason }`. In `insights.html`, after the "Hidden as stale" block, the same shape:

```html
{% if !reaped.is_empty() %}
<h3>Reaped</h3>
<p class="muted hint">Retired for long enough that the judge called them worthless, and buried: out of search, text kept. Restore puts one back in results and re-embeds it. The base restores one itself when a search given up on would have been answered by it.</p>
<table>
  {% for g in reaped %}
  <tr>
    <td><a href="/ui/artifacts/{{ g.id }}">{{ g.title.as_deref().unwrap_or("(untitled)") }}</a></td>
    <td class="muted">{{ g.ago }}{% if let Some(r) = g.reason %} · {{ r }}{% endif %}</td>
    <td><form method="post" action="/ui/ops/artifacts/{{ g.id }}/reactivate"><button class="btn btn-sm" type="submit">Restore</button></form></td>
  </tr>
  {% endfor %}
</table>
{% if more_reaped %}<p class="muted">Showing {{ deprecated_cap }}.</p>{% endif %}
{% endif %}
```

(Copy the exact classes and the `ReturnTo` form fields from the "Hidden as stale" block so the redirect lands back on Insights.)

`evolve_view`: `actions` from `recent_actions(10)`, each a sentence:

```rust
let what = match a.kind {
    Kind::Merge => format!("merged {} into {}", short(&a.subject_id), short(a.survivor_id.as_deref().unwrap_or("?"))),
    Kind::Supersede => format!("hid {} in favour of {}", short(&a.subject_id), short(a.survivor_id.as_deref().unwrap_or("?"))),
    Kind::Discard => format!("discarded {}", short(&a.subject_id)),
    Kind::Reap => format!("buried {}", short(&a.subject_id)),
    Kind::Promote => format!("promoted window {}", a.subject_id),
    Kind::Moment => format!("filed a reminder on {}", short(&a.subject_id)),
};
let ended = match a.undone_by {
    Some(UndoneBy::Evidence) => " — taken back on evidence".to_string(),
    Some(UndoneBy::Operator) => " — undone by you".to_string(),
    None => String::new(),
};
format!("{} — {}{}", ago(a.at), what, ended)
```

`rules`: read `evolve.retract.last`; `Some(format!("Last quiet period the base reconsidered {} of what it hid, took back {}, and restored {} for a search given up on.", ..))`, `None` when the key is absent. `retract::run` writes the key at its end (`serde_json::json!({"at": now(), "reconsidered": .., "undone": .., "restored": ..})`).

`_evolve.html` gains, after the generations `<details>`:

```html
{% if let Some(r) = e.rules %}<p class="muted hint">{{ r }}</p>{% endif %}
{% if !e.actions.is_empty() %}
<details class="judge-tune-history"><summary>what the base did to the corpus ({{ e.actions.len() }})</summary>
<ul class="mono">{% for a in e.actions %}<li>{{ a }}</li>{% endfor %}</ul></details>
{% endif %}
```

- [x] **Step 5: Run the gate**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked 2>&1 | tail -3`
Expected: green.

- [x] **Step 6: Commit**

```bash
git add -A src
git commit -m "feat(evolve): the graveyard is listed, and Insights says what the base did to itself"
```

---

### Task 7: `review_min` on the ladder

**Files:**
- Modify: `src/core/ranking.rs` (`review_min: f32`, `REVIEW_MINS`, `from_config` takes `&ConsolidateConfig`), `src/store/generations.rs`, `src/store/eval_runs.rs` (serde default `crate::config::default_review_min`), `src/config.rs` (`default_review_min() -> f32` = 0.88, `ConsolidateConfig::default` reads it, `write_ranking` writes `consolidate.review_min`, `ranking_keys_in_env` names it), `src/core/mod.rs` (the `from_config` call and the test-core literal), `src/eval/sweep.rs` (`moved`), `src/web/insights.rs` (`params_str`)
- Modify: `src/jobs/relate.rs:77` (reads the lock)
- Modify: `src/store/pairs.rs` (`band_record`), `src/jobs/tune.rs` (`review_step`, after `spread_step`)

**Interfaces:**
- Produces: `pub const REVIEW_MINS: [f32; 4] = [0.80, 0.84, 0.88, 0.92]`; `RankingParams.review_min: f32`; `Store::band_record(lo: f32, hi: f32) -> Result<BandRecord>` with `pub struct BandRecord { pub judged: usize, pub acted: usize, pub undone: usize }`; `tune::next_review_min(current: f32, auto_supersede: f32, low: BandRecord, above: BandRecord) -> Option<f32>`.

- [x] **Step 1: Write the failing tests**

Pure rule, in `src/jobs/tune.rs`:

```rust
#[test]
fn the_review_threshold_steps_down_when_its_lowest_band_acts_like_the_band_above_and_up_when_its_actions_are_taken_back_more() {
    use crate::store::pairs::BandRecord;
    let b = |judged, acted, undone| BandRecord { judged, acted, undone };
    // short: low acts 8/10, above acts 9/10 — within one decision of each other → down a rung
    assert_eq!(next_review_min(0.88, 0.95, b(10, 8, 0), b(10, 9, 0)), Some(0.84));
    // low acts 2/10 against 9/10 → hold
    assert_eq!(next_review_min(0.88, 0.95, b(10, 2, 0), b(10, 9, 0)), None);
    // wrong: low's actions taken back 4/8, above's 0/9 → up a rung
    assert_eq!(next_review_min(0.88, 0.95, b(10, 8, 4), b(10, 9, 0)), Some(0.92));
    // wrong wins over short when both fire
    assert_eq!(next_review_min(0.84, 0.95, b(10, 9, 5), b(10, 9, 0)), Some(0.88));
    // a rung at or above auto_supersede is never offered
    assert_eq!(next_review_min(0.92, 0.93, b(10, 8, 4), b(10, 9, 0)), None);
    // nothing judged in a band is no evidence
    assert_eq!(next_review_min(0.88, 0.95, b(0, 0, 0), b(10, 9, 0)), None);
    // a hand-set value off the ladder holds
    assert_eq!(next_review_min(0.85, 0.95, b(10, 8, 0), b(10, 9, 0)), None);
    // the bottom rung cannot step down
    assert_eq!(next_review_min(0.80, 0.95, b(10, 9, 0), b(10, 9, 0)), None);
}
```

In `src/store/pairs.rs` tests: record pairs at scores 0.81, 0.86, 0.90 settled by the model (use `set_pair_state`/`set_pair_merged` with `DecidedBy::Model`), one still pending at 0.82; journal rows with `pair_score` 0.81 (undone) and 0.90; `band_record(0.80, 0.88)` is `{judged: 2, acted: 1, undone: 1}` and `band_record(0.88, 1.0)` is `{judged: 1, acted: 1, undone: 0}`.

In `src/jobs/relate.rs` tests: a neighbour at 0.86 is recorded as a pair when `core.ranking.write().review_min = 0.84` and not when it is 0.88 (find the existing test that seeds neighbours and asserts on `record_pair`).

Pass-level, in `src/jobs/tune.rs`:

```rust
#[tokio::test]
async fn a_base_whose_lowest_band_acts_like_the_one_above_lowers_the_review_threshold_when_nothing_else_moves() {
    let (mut core, parent) = seeded_with_nothing_to_gain().await;
    core.evolve.autonomous = true;
    // pairs and journal rows as the band_record test seeds them, ten a band, low acting like above
    let adopted = run(&core).await.unwrap().adopted.expect("review_min steps down");
    let live = core.store.live_generation().await.unwrap().unwrap();
    assert_eq!(live.id, adopted);
    assert_eq!(live.params.review_min, 0.84);
    assert!(live.run_id.is_none());
    assert_eq!(core.ranking.read().unwrap().review_min, 0.84, "relate reads it from here");
}
```

- [x] **Step 2: Run them to see them fail**

Run: `cargo test --lib -- tune::tests::the_review_threshold pairs::tests::band_record 2>&1 | grep -E '^test |error' | head`
Expected: compile errors.

- [x] **Step 3: The knob**

As task 1 of the 3a plan did for three knobs: field, serde default, `from_config(vector, associate, consolidate, reranker_configured)` copying `consolidate.review_min`, `moved` +1, `params_str` gains `review {:.2}`, `write_ranking` writes `doc["consolidate"]["review_min"]` (rounded to three decimals like `recency_weight`), `ranking_keys_in_env` gains `ENGRAM__CONSOLIDATE__REVIEW_MIN`. The file validation `auto_supersede > review_min` stays on the file. `relate.rs:77`:

```rust
let review_min = core.ranking.read().expect("ranking lock").review_min;
// ...
if similarity < review_min { continue; }
```

(Read the lock once before the loop; never hold it across an await.)

- [x] **Step 4: The band record**

```rust
pub struct BandRecord { pub judged: usize, pub acted: usize, pub undone: usize }

/// Pairs the judge settled with a cosine in `[lo, hi)`, and, from the journal,
/// how many of the base's dedupe actions in that band there were and how
/// many were taken back.
pub async fn band_record(&self, lo: f32, hi: f32) -> Result<BandRecord> {
    let judged: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM artifact_pairs
          WHERE score >= ? AND score < ? AND state <> 'pending' AND decided_by = 'model'",
    ).bind(lo).bind(hi).fetch_one(&self.pool).await?;
    let r = sqlx::query(
        "SELECT COUNT(*) AS acted,
                COALESCE(SUM(CASE WHEN undone_at IS NOT NULL THEN 1 ELSE 0 END), 0) AS undone
           FROM corpus_actions
          WHERE job = 'dedupe' AND pair_score >= ? AND pair_score < ?",
    ).bind(lo).bind(hi).fetch_one(&self.pool).await?;
    Ok(BandRecord { judged: judged as usize, acted: r.get::<i64, _>("acted") as usize, undone: r.get::<i64, _>("undone") as usize })
}
```

- [x] **Step 5: The rule and the step**

```rust
/// The review threshold's rule. Two signals, each a rate over the lowest
/// recorded band against the band above it, compared with one-decision noise:
/// `wrong` — the lowest band's actions taken back more often — steps up;
/// `short` — the lowest band acting as often — steps down. Wrong first.
pub fn next_review_min(current: f32, auto_supersede: f32, low: BandRecord, above: BandRecord) -> Option<f32> {
    use crate::core::ranking::REVIEW_MINS;
    let at = REVIEW_MINS.iter().position(|r| (r - current).abs() < 1e-6)?;
    let rate = |n: usize, d: usize| (d > 0).then(|| n as f64 / d as f64);
    let noise = |a: usize, b: usize| 1.0 / a as f64 + 1.0 / b as f64;
    if let (Some(lw), Some(aw)) = (rate(low.undone, low.acted), rate(above.undone, above.acted))
        && lw - aw > noise(low.acted, above.acted)
    {
        return REVIEW_MINS.get(at + 1).copied().filter(|r| *r < auto_supersede);
    }
    if let (Some(ls), Some(as_)) = (rate(low.acted, low.judged), rate(above.acted, above.judged))
        && as_ - ls <= noise(low.judged, above.judged)
    {
        return at.checked_sub(1).map(|i| REVIEW_MINS[i]);
    }
    None
}

async fn review_step(core: &Core, live: &Generation, current: RankingParams, tried: &[GenerationParams]) -> Result<Pass> {
    use crate::core::ranking::REVIEW_MINS;
    let Some(at) = REVIEW_MINS.iter().position(|r| (r - current.review_min).abs() < 1e-6) else { return Ok(Pass::default()); };
    let hi = REVIEW_MINS.get(at + 1).copied().unwrap_or(1.0).min(core.consolidate.auto_supersede);
    let low = core.store.band_record(current.review_min, hi).await?;
    let above = core.store.band_record(hi, 1.0).await?;
    let Some(next) = next_review_min(current.review_min, core.consolidate.auto_supersede, low, above) else { return Ok(Pass::default()); };
    let candidate = RankingParams { review_min: next, ..current };
    if tried.contains(&GenerationParams::from(candidate)) { return Ok(Pass::default()); }
    let predicted = match low.judged { 0 => 0.0, n => low.acted as f64 / n as f64 };
    // adopt_generation_lived + write the lock + info!, as spread_step does
}
```

`propose` chains: ladder → flip → `spread_step` → `review_step` (each only when the previous proposed nothing; `spread_step` returns `Pass::default()` when it holds, so `review_step` runs on that).

- [x] **Step 6: Run the gate**

Run: `cargo fmt --all && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked 2>&1 | tail -3`
Expected: green; the `RankingParams` literal initializers from 3a need `review_min`.

- [x] **Step 7: Commit**

```bash
git add -A src
git commit -m "feat(evolve): review_min moves on what its lowest band earned and what was taken back"
```

---

### Task 8: The words catch up

**Files:**
- Modify: `config.example.toml` (`[consolidate] review_min` is the starting rung; `[evolve] autonomous` names the corpus half and the eight knobs; `[reap]` says the base restores on a give-up)
- Modify: `src/jobs/tune.rs:1-40` (module doc: the corpus half), `src/jobs/reap.rs:1-12` (header: "No operator queue" → the journal, the listing, rule 2)
- Modify: `docs/superpowers/plans/2026-09-05-self-tuning-handoff.md` (3b built; the stage is complete; what a stage 4 would be, if any)
- Modify: `docs/superpowers/specs/2026-09-05-self-tuning-stage-3-design.md` (a "Built" note under Part B naming the admissions, as the 2b note under stage 2 does in the original spec)

- [x] **Step 1: Rewrite**

`review_min` comment gains: "The starting rung. A base with `evolve.autonomous` on lowers it when the pairs just above it act as often as the pairs higher up, and raises it when its actions are taken back more often; the database holds what is live."

`autonomous` comment: add a paragraph — "The same switch lets the base take its own corpus actions back: a merge or a supersession whose survivor is no longer found where the original was, and anything it hid or buried that a search given up on would have been answered by. Every action and every undo is on /ui/insights."

`reap.rs` header: replace "No operator queue; the graveyard is the insurance a wrong verdict answers to" with the journal row, the Reaped listing and rule 2.

Handoff: "What exists" gains the journal, `jobs/retract.rs`, the listing, `review_min` on the ladder; "Plans, in order" marks 3b built and says the stage 3 spec is complete; the "What stage 3b has to build on" section is rewritten as "What a later stage has to build on": the thresholds that do not move yet (stale, promote, reap) and why, rescues, and the `ADDITIVE` list's length.

- [x] **Step 2: Run the gate**

Run: `cargo fmt --all --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked 2>&1 | tail -3`
Expected: green.

- [x] **Step 3: Commit**

```bash
git add -A config.example.toml src docs
git commit -m "docs(evolve): the corpus half, the journal, and the stage 3 spec is built"
```

---

## Self-review against the spec

- **The journal** (Part B): table with every named column plus `pair_score` (T1); one row per subject, a merge is one row per original sharing `survivor_id` (T2); written under the lifecycle lock inside `Core::supersede_with` / `deprecate_with`, inside `bury`'s transaction for reap, beside the pair write for merge (T2, admitted); every undo route stamps `operator` (T3); rows never deleted, retention untouched (T1 — `expire_feedback` never touched observations either).
- **Two rules, one pass**: after the ranking half's anchor check, same switch, claim and suspension (T4); rule 1 as written, `recommend` pointed the other way, era-filtered, no evidence → left alone (T4); rule 2 with hidden hits included and cosine over buried vectors, restoring through `reactivate` / `unsupersede` / `merge::undo` (T5); `graveyard.vec` and `embed_model`, another era skipped (T2, T5).
- **Memory**: `action_was_undone` read before dedupe acts and before reap judges (T2). Promote's memory is already `segment_no_promote`; judgement has no repeat path.
- **The anchor**: shared; the corpus half runs only past `trustworthy` (T4 test).
- **What joins**: merge, supersede, discard, reap on both rules where the table says; promote and moment journal only (T2). `stale` dropped — admitted.
- **Graveyard visible** (T6); **disclosure** with evidence and operator undos told apart and a line for the rules (T6).
- **Part C**: `review_min` on the generation, `Core` holds it behind the same lock, `relate.rs` reads it there (T7); ladder `[0.80, 0.84, 0.88, 0.92]`, rungs at or above `auto_supersede` never offered (T7); bands from pairs and the journal (T7); short and wrong with one-decision noise (T7); journaled as a generation with `predicted` = the band's action rate (T7); watched by `lived` — admitted; one change at a time holds because it is a generation under watch like any other (T7).
- **Error handling**: a rule that fails partway returns the error; rows not yet stamped are re-read next pass (T4, T5); a grave with no vector is skipped (T5); `reactivate` enqueues the embed (existing); stop-on-return between subjects and between give-ups (T4, T5).
- **Testing** sentences from the spec: every one has a test above except "an untrustworthy anchor stops both rules and the ladder alike", which is T4's second pass-level test.

Types across tasks: `NewAction` / `Action` / `Kind` / `UndoneBy` (T1) used by T2–T7; `merge::undo(.., DecidedBy)` (T3) used by T4 and T5; `Observation.embed_model` (T4) used by T5; `Retracted` (T4) extended by T5 and written to meta by T6; `BandRecord` (T7) consumed by `next_review_min` (T7); `Pass.undone` / `restored` (T4) read by retention (T4).
