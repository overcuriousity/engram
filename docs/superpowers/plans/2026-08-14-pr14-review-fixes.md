# PR #14 Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix all 14 confirmed findings from the multi-agent review of PR #14 (`feat/autonomous-consolidation`), on that branch.

**Architecture:** The load-bearing principle across the dedupe fixes: a pair-state write may only become terminal *after* the fallible side effect it describes (supersede, merge-embed) is durable — or the settled state must be recoverable by a sweep repair. Three product decisions are already made by the user: (1) observation-mode merge verdicts get a new honest `WouldMerge` pair state, draft still discarded; (2) unsuperseding a merge source is honored as a partial restore via a `restored` marker on lineage rows; (3) merges whose embed permanently fails are auto-undone by the sweep, reopening their pairs for a person.

**Tech Stack:** Rust (tokio, sqlx/SQLite, axum, askama templates), Qdrant + in-memory vector store. Tests are `#[tokio::test]` colocated in each module.

**Spec:** The 14 findings, restated one line each in "Findings Index" below. Full failure scenarios are in the PR #14 review report (delivered in-session); each task's rationale section repeats what its finding claims.

## Global Constraints

- Work on branch `feat/autonomous-consolidation` (PR #14). Do not merge or rebase onto master.
- Every task: `cargo test` must pass and `cargo fmt` must be clean before its commit.
- Commit style (from repo history): `fix(scope): lowercase clause describing the behavior`, e.g. `fix(dedupe): a retired member dismisses only its own pairs`. End every commit message with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Comment style: this codebase writes long "why" comments. When you change behavior a nearby comment describes, update the comment in the same edit — several findings are precisely stale comments.
- Schema changes go in BOTH places: the fresh-DB schema (`src/store/schema.sql` — locate the `artifact_pairs` / `artifact_sources` CREATE TABLE there) AND the `ADDED_COLUMNS` list in `src/store/mod.rs` (append-only, with a default; never reorder).
- SQLite `now()` helper and test idioms (`test_core()`, `seed`, `queue_pair`, `ScriptedCompleter`, direct `sqlx::query(...).execute(&core.store.pool)` for state surgery) already exist — use them, do not invent new harnesses.

## Findings Index

| # | Where | One-line claim |
|---|-------|----------------|
| F1 | `src/jobs/dedupe.rs:294-327` | Replaced verdict settles pairs terminally before the fallible `core.supersede`; picks obsolete/winner from roots with no status check; failure is unretryable (run() skips non-Pending). |
| F2 | `src/jobs/dedupe.rs:78-81` | One retired component member dismisses the entire component, killing sibling pairs between still-active duplicates forever. |
| F3 | `src/jobs/consolidate.rs:307` | Sweep's unfinished-merge repair silently reverts an operator's unsupersede of a merge source, every sweep. |
| F4 | `src/jobs/consolidate.rs:446-470` | Closing pass treats any `get_artifact` error (transient BUSY) as "artifact gone" and permanently closes a live pair as NoConflict. |
| F5 | `src/jobs/dedupe.rs:357-365` | Duplicate verdict settles pairs as NoConflict before the merge is embedded; a permanently failing embed strands an invisible merge, duplicates unmergeable forever. |
| F6 | `src/jobs/dedupe.rs:321` | Autonomous applied replacement leaves its pair in `Superseded` (= "awaiting confirmation"); its buttons then always return a validation error. |
| F7 | `src/jobs/dedupe.rs:334-350` | Autonomy-off duplicate verdict filed as `Contradiction`; UI lies ("These two disagree"), offers only lossy keep-one buttons, draft discarded. |
| F8 | `src/store/lineage.rs:43-55` | `roots_of` self-root fallback fires for an orphaned merge; its synthesized text is shown to the model as a captured original and can become a `root_id`. |
| F9 | `src/store/artifacts.rs:407` | `restore_artifact` loses `source_count`/lineage, so a merge restored from its vector payload silently defeats the anti-drift invariant. |
| F10 | `src/jobs/consolidate.rs:153-169` | `repair_lifecycle_drift` racing a payload-first reveal can re-hide the payload, then the reveal clears the marker: row Active, payload Superseded, no marker. |
| F11 | `src/store/jobs.rs:330` | Legacy `'judge'` job rows parse as `None` → `unwrap_or(Stage::Synthesize)` misroute. |
| F12 | `src/jobs/consolidate.rs:549-560` | `arm_dedupe` does a 200-row query + loop every tick even when `max_dedupe_per_tick == 0`. |
| F13 | `src/store/pairs.rs:350-366` | `open_component` fixed-point loop rescans the whole window per growth pass — quadratic on the 5 000-row window. |
| F14 | `src/jobs/consolidate.rs:148,175` | Comments claim `full_lifecycle_reconcile` "no longer runs every sweep" / is "behind the marker" while `run()` calls it unconditionally at line 298. |

---

### Task 1: Pair-state groundwork — `WouldMerge` state and `merged_into` column

**Files:**
- Modify: `src/store/pairs.rs` (PairState enum, `ArtifactPair`, `row_to_pair`, `set_pair_state`, `set_pair_superseded`; new fns `set_pair_merged`, `reopen_pairs_merged_into`)
- Modify: `src/store/mod.rs` (`ADDED_COLUMNS`)
- Modify: `src/store/schema.sql` (or wherever `CREATE TABLE artifact_pairs` lives — find with `grep -rn "CREATE TABLE artifact_pairs" src/`)
- Test: `src/store/pairs.rs` tests module

**Interfaces:**
- Consumes: existing `Store` pair API.
- Produces (later tasks rely on these exact names):
  - `PairState::WouldMerge` with `as_str() == "would_merge"`, `parse("would_merge")`.
  - `ArtifactPair.merged_into: Option<String>`.
  - `Store::set_pair_merged(&self, id: i64, merged_into: &str, detail: Option<&str>) -> Result<()>` — writes `state = 'no_conflict'`, `detail`, `merged_into`, `obsolete_id = NULL`; `Err(NotFound)` when 0 rows affected.
  - `Store::reopen_pairs_merged_into(&self, merged_id: &str, detail: &str) -> Result<u64>` — `state = 'contradiction'`, given detail, `merged_into = NULL` for all rows whose `merged_into = merged_id`; returns rows affected.
  - `set_pair_state` and `set_pair_superseded` both additionally set `merged_into = NULL` (same rule as `obsolete_id`: the column belongs to the merged-settlement and to nothing else).

- [ ] **Step 1: Write the failing tests** (append to `src/store/pairs.rs` tests)

```rust
#[tokio::test]
async fn a_merged_settlement_records_which_merge_answered_it() {
    let s = Store::memory().await.unwrap();
    let (a, b) = two_artifacts(&s).await;
    s.record_pair(&a, &b, 0.91).await.unwrap();
    let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;

    s.set_pair_merged(id, "merge-1", Some("same claim")).await.unwrap();

    let p = s.get_pair(id).await.unwrap();
    assert_eq!(p.state, PairState::NoConflict);
    assert_eq!(p.merged_into.as_deref(), Some("merge-1"));

    // Leaving the settlement drops the record, exactly as obsolete_id does.
    s.set_pair_state(id, PairState::Dismissed, None).await.unwrap();
    assert_eq!(s.get_pair(id).await.unwrap().merged_into, None);
}

#[tokio::test]
async fn reopening_a_stranded_merge_s_pairs_touches_only_its_own() {
    let s = Store::memory().await.unwrap();
    let (a, b) = two_artifacts(&s).await;
    s.record_pair(&a, &b, 0.91).await.unwrap();
    let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
    s.set_pair_merged(id, "merge-1", None).await.unwrap();

    assert_eq!(
        s.reopen_pairs_merged_into("merge-other", "unrelated").await.unwrap(),
        0,
        "another merge's undo reopened this pair"
    );
    assert_eq!(s.reopen_pairs_merged_into("merge-1", "the merged text could not be indexed").await.unwrap(), 1);
    let p = s.get_pair(id).await.unwrap();
    assert_eq!(p.state, PairState::Contradiction);
    assert_eq!(p.merged_into, None);
}

#[tokio::test]
async fn would_merge_is_a_state_of_its_own() {
    let s = Store::memory().await.unwrap();
    let (a, b) = two_artifacts(&s).await;
    s.record_pair(&a, &b, 0.91).await.unwrap();
    let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
    s.set_pair_state(id, PairState::WouldMerge, Some("same claim")).await.unwrap();
    assert_eq!(s.pairs_by_state(PairState::WouldMerge, 10).await.unwrap().len(), 1);
    assert_eq!(PairState::parse("would_merge"), PairState::WouldMerge);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p engram a_merged_settlement_records reopening_a_stranded would_merge_is_a_state 2>&1 | tail -20`
Expected: compile errors — `WouldMerge`, `merged_into`, `set_pair_merged` do not exist.

- [ ] **Step 3: Implement**

In `src/store/pairs.rs`:
- Add `WouldMerge` variant with doc comment: the model answered "duplicate" while autonomy is off — recorded so an operator can read the verdicts before letting the system act on them; the draft is not kept, and flipping autonomy on lets a later unit re-judge and merge.
- Extend `as_str`/`parse` with `"would_merge"`.
- Add `pub merged_into: Option<String>` to `ArtifactPair` (doc: which merged artifact answered this pair, when the settlement was an applied merge; what the stranded-merge reap uses to reopen exactly the pairs a failed merge closed). Read it in `row_to_pair`.
- In `set_pair_state` and `set_pair_superseded` SQL, add `, merged_into = NULL` next to the existing `obsolete_id` handling (extend `set_pair_state`'s doc comment to name both columns).
- New functions:

```rust
/// Settle a pair as answered by an applied merge. `merged_into` names the
/// merged artifact, which is what lets the stranded-merge reap reopen exactly
/// the pairs a merge that never embedded had closed.
pub async fn set_pair_merged(&self, id: i64, merged_into: &str, detail: Option<&str>) -> Result<()> {
    let res = sqlx::query(
        "UPDATE artifact_pairs
            SET state = 'no_conflict', detail = ?, merged_into = ?, obsolete_id = NULL
          WHERE id = ?",
    )
    .bind(detail)
    .bind(merged_into)
    .bind(id)
    .execute(&self.pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(crate::error::Error::NotFound);
    }
    Ok(())
}

/// Reopen every pair a now-dead merge had settled, handing them to a person.
/// Contradiction rather than Pending on purpose: re-arming the model would
/// regenerate the same unembeddable draft and loop forever.
pub async fn reopen_pairs_merged_into(&self, merged_id: &str, detail: &str) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE artifact_pairs
            SET state = 'contradiction', detail = ?, merged_into = NULL
          WHERE merged_into = ?",
    )
    .bind(detail)
    .bind(merged_id)
    .execute(&self.pool)
    .await?;
    Ok(res.rows_affected())
}
```

In `src/store/mod.rs` `ADDED_COLUMNS`, append (comment: arrived with the stranded-merge reap; NULL on every pair predating it, which is correct — those settlements predate the column and are not reopenable by merge id):

```rust
("artifact_pairs", "merged_into", "TEXT"),
```

In the schema file, add `merged_into TEXT` to the `artifact_pairs` CREATE TABLE.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p engram a_merged_settlement_records reopening_a_stranded would_merge_is_a_state` — Expected: PASS. Then full `cargo test` (the `PairState` match in `as_str` is exhaustive; the compiler will point at any consumer that needs the new arm — fix those `match`es by adding the arm, nothing else).

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(pairs): a would_merge state and a merged_into column for settlements"
```

---

### Task 2: F7 — observation-mode merge verdicts land as `WouldMerge`, rendered honestly

**Files:**
- Modify: `src/jobs/dedupe.rs` (Duplicate arm, non-autonomous branch, ~line 334)
- Modify: `src/web/ui.rs` (`PAIR_STATES`, `PairRow`, `pair_rows`)
- Modify: `templates/_decide.html`
- Modify: `src/web/api.rs` (`consolidation` handler)
- Test: `src/jobs/dedupe.rs` tests

**Interfaces:**
- Consumes: `PairState::WouldMerge` (Task 1).
- Produces: `PairRow.would_merge: bool`; API key `"merge_proposals"`.

- [ ] **Step 1: Write the failing test** (in `src/jobs/dedupe.rs` tests; copy the seeding idiom from the existing duplicate-verdict test around line 720)

```rust
#[tokio::test]
async fn with_autonomy_off_a_duplicate_verdict_is_filed_as_would_merge() {
    // It used to be filed as Contradiction, so the UI said "These two
    // disagree" about a pair the model judged complementary, and offered only
    // the lossy keep-one buttons for it.
    let mut core = test_core().await;
    core.consolidate.autonomous = false;
    core.completer = Arc::new(ScriptedCompleter::new(vec![
        r#"{"relation":"duplicate","detail":"same claim",
            "merged":{"text":"engram needs Rust 1.21.4 and 1.30.0 to build.","tags":[],"caveats":[]}}"#
            .into(),
    ]));
    let ids = disagreeing(&core).await;
    let pair = queue_pair(&core, &ids[0], &ids[1]).await;

    run(&core, &pair.to_string()).await.unwrap();

    let found = core.store.pairs_by_state(PairState::WouldMerge, 10).await.unwrap();
    assert_eq!(found.len(), 1, "the verdict must land as its own state");
    assert_eq!(found[0].detail.as_deref(), Some("same claim"));
    assert!(
        core.store.pairs_by_state(PairState::Contradiction, 10).await.unwrap().is_empty(),
        "a mergeable pair was filed among genuine conflicts"
    );
    // Recorded, not applied: no merge written, nothing hidden.
    for id in &ids {
        let c = core.store.get_artifact(id).await.unwrap();
        assert!(c.superseded_by.is_none());
    }
    assert!(core.store.merged_artifacts(10).await.unwrap().is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p engram with_autonomy_off_a_duplicate_verdict` — Expected: FAIL (pair lands in Contradiction).

- [ ] **Step 3: Implement**

`src/jobs/dedupe.rs`, Duplicate arm, non-autonomous branch: replace the `Contradiction` settle (and its `"would merge: {detail}"` prefix hack) with:

```rust
if !core.consolidate.autonomous {
    // Recorded, not applied. Reading the verdicts before letting the
    // system act on them is the cheapest evidence available about
    // whether the contract holds on real data. Its own state rather
    // than Contradiction: filing a mergeable pair among genuine
    // conflicts made the UI claim the two disagree, and steered the
    // operator toward hiding a side the model judged complementary.
    // The draft is discarded — once autonomy is on, the unit re-judges
    // and merges then.
    return settle_all(core, &s.pairs, PairState::WouldMerge, s.detail.as_deref()).await;
}
```

`src/web/ui.rs`:
- `PAIR_STATES` becomes 5 entries; insert `WouldMerge` after `Oversized` with a comment (a verdict recorded in observation mode: worth a person's read, less urgent than a contradiction):

```rust
const PAIR_STATES: [crate::store::pairs::PairState; 5] = [
    crate::store::pairs::PairState::Contradiction,
    crate::store::pairs::PairState::Superseded,
    crate::store::pairs::PairState::Oversized,
    crate::store::pairs::PairState::WouldMerge,
    crate::store::pairs::PairState::Pending,
];
```

- Add `pub would_merge: bool` to `PairRow` (doc: the model would merge these; rendered with its own header so the page never claims a disagreement it did not find). In `pair_rows`, next to the existing `contradiction:` field init, add `would_merge: state == crate::store::pairs::PairState::WouldMerge,`.

`templates/_decide.html` head line — three-way branch:

```html
{% if p.contradiction %}<b>These two disagree</b>{% else %}{% if p.would_merge %}<b>The model would merge these</b>{% else %}<b>These two cover the same ground</b>{% endif %}{% endif %}
```

(Keep the existing keep-one and Dismiss forms for all states — that is the chosen scope: honest header, existing actions.)

`src/web/api.rs` `consolidation` handler: after `"supersede_proposals"`, add:

```rust
// Merge verdicts recorded while autonomy is off. Their own key for the
// same reason they are their own state: an API consumer counting
// `contradictions` must not see pairs the model judged complementary.
"merge_proposals": st
    .core
    .store
    .pairs_by_state(PairState::WouldMerge, 100)
    .await?,
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p engram` — Expected: PASS, including the existing autonomy-off dedupe tests (any asserting Contradiction + `"would merge:"` prefix must be updated to assert `WouldMerge` — that assertion was pinning the bug).

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "fix(dedupe): an observation-mode merge verdict is not a contradiction"
```

---

### Task 3: F1 + F6 — Replaced verdict: validate, apply, then settle

**Files:**
- Modify: `src/jobs/dedupe.rs` (`apply`, `Relation::Replaced` arm, ~lines 281-328)
- Test: `src/jobs/dedupe.rs` tests

**Interfaces:**
- Consumes: `Core::supersede`, `ArtifactStatus`, `settle_all`.
- Produces: new arm behavior — later tasks do not depend on its internals.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn an_applied_replacement_does_not_wait_for_an_operator() {
    // F6: the pair used to stay in Superseded — the state every consumer
    // reads as "awaiting confirmation" — with buttons that could only
    // return a validation error.
    let mut core = test_core().await;
    core.consolidate.autonomous = true;
    core.completer = Arc::new(ScriptedCompleter::new(vec![
        r#"{"relation":"replaced","detail":"old flag vs new flag","supersedes":"a"}"#.into(),
    ]));
    let ids = disagreeing(&core).await;
    // Make ids[0] strictly older so the newest-wins guard accepts "a".
    sqlx::query("UPDATE artifacts SET created_at = created_at - 100 WHERE id = ?")
        .bind(&ids[0]).execute(&core.store.pool).await.unwrap();
    let pair = queue_pair(&core, &ids[0], &ids[1]).await;

    run(&core, &pair.to_string()).await.unwrap();

    assert!(
        core.store.get_artifact(&ids[0]).await.unwrap().superseded_by.is_some(),
        "the replacement was not applied"
    );
    assert!(
        core.store.pairs_by_state(PairState::Superseded, 10).await.unwrap().is_empty(),
        "an applied replacement is still listed as awaiting confirmation"
    );
    assert_eq!(
        core.store.pairs_by_state(PairState::Dismissed, 10).await.unwrap().len(),
        1,
        "the applied pair should be settled the way the manual apply settles it"
    );
}

#[tokio::test]
async fn a_replacement_naming_a_root_already_out_of_results_settles_cleanly() {
    // F1: a component holding a finished merge flattens to roots that are
    // already superseded. Applying blindly errored *after* the pairs were
    // settled, and run()'s Pending guard made the error unretryable.
    let mut core = test_core().await;
    core.consolidate.autonomous = true;
    // First call merges a+b; second call answers the (merge, c) pair.
    core.completer = Arc::new(ScriptedCompleter::new(vec![
        r#"{"relation":"duplicate","detail":"same claim",
            "merged":{"text":"engram needs Rust 1.21.4 to build.","tags":[],"caveats":[]}}"#.into(),
        r#"{"relation":"replaced","detail":"superseded by the merge","supersedes":"c"}"#.into(),
    ]));
    let ids = seed(
        &core,
        &[
            ("engram needs Rust 1.21.4 to build.", [1.0, 0.0]),
            ("engram needs Rust 1.21.4 to compile.", [0.999, 0.02]),
            ("engram wants Rust 1.20 or so to build.", [0.97, 0.2]),
        ],
    )
    .await;
    // Oldest so the newest-wins guard accepts it as obsolete. Its letter
    // among the sorted roots must be computed, not assumed — see step 3
    // note; if the sorted position of ids[2] is not 'c', adjust the
    // scripted letter accordingly by sorting [&ids[0], &ids[1], &ids[2]].
    sqlx::query("UPDATE artifacts SET created_at = created_at - 100 WHERE id = ?")
        .bind(&ids[2]).execute(&core.store.pool).await.unwrap();

    // Merge a and b, then drive the queue so finish() supersedes the roots.
    let p1 = queue_pair(&core, &ids[0], &ids[1]).await;
    run(&core, &p1.to_string()).await.unwrap();
    drive_queue(&core).await; // embed lands, merge::finish hides ids[0], ids[1]
    let m = &core.store.merged_artifacts(10).await.unwrap()[0].id;

    let p2 = queue_pair(&core, m, &ids[2]).await;
    run(&core, &p2.to_string()).await.unwrap();

    // The winner among the roots is superseded, so the live carrier (the
    // merge, a member) wins instead — and the settle happens after.
    assert_eq!(
        core.store.get_artifact(&ids[2]).await.unwrap().superseded_by.as_deref(),
        Some(m.as_str()),
        "the replacement was not applied against the live carrier"
    );
    assert!(core.store.pairs_by_state(PairState::Pending, 10).await.unwrap().is_empty());
}
```

Add the small queue driver next to the tests if none exists in this module (the consolidate tests have `sweep_and_judge`/`sweep_and_dedupe` — reuse the same body):

```rust
async fn drive_queue(core: &Core) {
    for _ in 0..100 {
        sqlx::query("UPDATE jobs SET run_after = 0")
            .execute(&core.store.pool).await.unwrap();
        if !crate::jobs::run_one(core).await.unwrap_or(false) { break; }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p engram an_applied_replacement_does_not_wait a_replacement_naming_a_root_already` — Expected: first FAILS on the `Superseded`-empty assertion; second FAILS (error propagates or pair left half-settled). If the second fails only because the letter `"c"` maps to a different sorted root, fix the test's scripted letter first (sort the three ids; the letter is the index of `ids[2]` in that order) — then confirm it fails for the real reason.

- [ ] **Step 3: Implement** — replace the whole `Relation::Replaced` arm of `apply`:

```rust
Relation::Replaced => {
    let obsolete = s
        .obsolete
        .clone()
        .expect("interpret sets this or downgrades to Conflict");
    // Fresh statuses, not the snapshot `interpret` saw. The roots of a
    // member that is itself a finished merge are already superseded, and
    // a component can change while the unit waits out a backoff.
    let mut fresh = Vec::new();
    for r in &s.roots {
        match core.store.get_artifact(&r.id).await {
            Ok(c) => fresh.push(c),
            Err(Error::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    let live = |c: &Chunk| c.status == ArtifactStatus::Active && c.superseded_by.is_none();
    let obsolete_live = fresh.iter().any(|c| c.id == obsolete && live(c));
    // A live root wins if one exists; otherwise the live member that
    // carries the surviving roots — a finished merge's own sources are
    // superseded, and the merge is the one thing still in results.
    let winner = fresh
        .iter()
        .find(|c| c.id != obsolete && live(c))
        .map(|c| c.id.clone())
        .or_else(|| s.members.iter().find(|m| m.id != obsolete).map(|m| m.id.clone()));
    let (Some(winner), true) = (winner, obsolete_live) else {
        // Nothing to apply: the named side is already out of results, so
        // the replacement has in effect already happened.
        return settle_all(
            core,
            &s.pairs,
            PairState::NoConflict,
            Some("the named replacement is already out of results"),
        )
        .await;
    };

    if core.consolidate.autonomous {
        // The side effect FIRST. A failure here leaves every pair
        // pending, so the unit retries under the queue's backoff — the
        // reverse order left the verdict recorded but never applied,
        // permanently, because run() skips non-Pending pairs.
        core.supersede(&obsolete, &winner).await?;
        tracing::info!(superseded = %obsolete, by = %winner, "applied a replacement");
        for pr in &s.pairs {
            if pr.a_id == obsolete || pr.b_id == obsolete {
                // As the manual apply settles it (`apply_pair_supersede_ui`):
                // done, with the model's reasoning kept as the record of why.
                core.store
                    .set_pair_state(pr.id, PairState::Dismissed, s.detail.as_deref())
                    .await?;
            } else {
                // Both sides of this pair survived; see the comment below.
                core.store
                    .set_pair_state(
                        pr.id,
                        PairState::Contradiction,
                        Some(&format!("{obsolete} was superseded; these two were not separated")),
                    )
                    .await?;
            }
        }
        return Ok(());
    }

    // Proposal mode: nothing is hidden, the pair carries the direction
    // and an operator confirms via "apply supersede".
    for pr in &s.pairs {
        if pr.a_id == obsolete || pr.b_id == obsolete {
            core.store
                .set_pair_superseded(pr.id, &obsolete, s.detail.as_deref())
                .await?;
        } else {
            core.store
                .set_pair_state(
                    pr.id,
                    PairState::Contradiction,
                    Some(&format!("{obsolete} was superseded; these two were not separated")),
                )
                .await?;
        }
    }
    Ok(())
}
```

Carry over (do not lose) the two existing long comments in that arm — the "letter indexes `roots`" comment stays on `interpret`, and the "Both sides survived…" comment moves onto the Contradiction branch. The `winner`-from-roots comment is superseded by the new "live root wins…" comment.

- [ ] **Step 4: Run tests**

Run: `cargo test -p engram` — Expected: PASS. The existing test `a_confident_direction_proposes_a_supersede_but_does_not_apply_it` must still pass unchanged (proposal path preserved).

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "fix(dedupe): a replacement is applied before its pairs are settled"
```

---

### Task 4: F2 — a retired member dismisses only the pairs that name it

**Files:**
- Modify: `src/jobs/dedupe.rs` (`run`, member-gathering loop, ~lines 67-87)
- Test: `src/jobs/dedupe.rs` tests

**Interfaces:** none new.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn a_retired_member_dismisses_only_its_own_pairs() {
    // Dismissing the whole component killed sibling pairs between
    // still-active duplicates permanently: record_pair is INSERT OR
    // IGNORE and Dismissed appears on no list, so the A/B duplication
    // became invisible forever.
    let mut core = test_core().await;
    core.consolidate.autonomous = true;
    core.completer = Arc::new(ScriptedCompleter::new(vec![
        r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
    ]));
    let ids = seed(
        &core,
        &[
            ("the timeout is 30 seconds", [1.0, 0.0]),
            ("the timeout is 90 seconds", [0.93, 0.37]),
            ("the timeout is 120 seconds", [0.94, 0.34]),
        ],
    )
    .await;
    let p_ab = queue_pair(&core, &ids[0], &ids[1]).await;
    let p_bc = queue_pair(&core, &ids[1], &ids[2]).await;
    core.deprecate(&ids[2]).await.unwrap();

    run(&core, &p_ab.to_string()).await.unwrap();

    assert_eq!(
        core.store.get_pair(p_bc).await.unwrap().state,
        PairState::Dismissed,
        "the pair naming the retired artifact should be dismissed"
    );
    assert_eq!(
        core.store.get_pair(p_ab).await.unwrap().state,
        PairState::NoConflict,
        "the pair between two live artifacts must still be judged, not dismissed"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p engram a_retired_member_dismisses_only_its_own_pairs` — Expected: FAIL — `p_ab` is Dismissed.

- [ ] **Step 3: Implement** — in `run`, replace the retired-member early-out. `pairs` must become `let mut pairs = …`:

```rust
let mut members = Vec::new();
let mut retired: std::collections::HashSet<String> = Default::default();
for mid in &member_ids {
    // (keep the existing "Reported, not swallowed" comment here)
    let c = core.store.get_artifact(mid).await?;
    if c.status != ArtifactStatus::Active || c.superseded_by.is_some() {
        // Only the pairs naming this member are answered by its
        // retirement. Dismissing the whole component killed sibling
        // pairs between still-active duplicates — record_pair is
        // INSERT OR IGNORE, so nothing could ever re-file them.
        retired.insert(c.id);
        continue;
    }
    members.push(c);
}
if !retired.is_empty() {
    let (dead, live): (Vec<_>, Vec<_>) = pairs
        .into_iter()
        .partition(|pr| retired.contains(&pr.a_id) || retired.contains(&pr.b_id));
    settle_all(core, &dead, PairState::Dismissed, Some("a member is no longer in results")).await?;
    pairs = live;
    // Dropping a member can strand others with no surviving pair; they
    // are simply not part of this unit's question any more.
    let named: std::collections::HashSet<&str> = pairs
        .iter()
        .flat_map(|pr| [pr.a_id.as_str(), pr.b_id.as_str()])
        .collect();
    members.retain(|c| named.contains(c.id.as_str()));
    // The seed pair itself may be among the dead; the survivors keep
    // their own units and nothing further is owed here.
    if !pairs.iter().any(|pr| pr.id == id) {
        return Ok(());
    }
}
if members.len() < 2 {
    settle_all(core, &pairs, PairState::Dismissed, None).await?;
    return Ok(());
}
```

Also move the existing re-check comment ("Re-checked here and not only when the unit was armed…") onto the retired-branch.

- [ ] **Step 4: Run tests** — `cargo test -p engram` — Expected: PASS (the existing dismissal tests for fully-retired components still pass: with every pair dead, the seed pair is dismissed and the unit returns).

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "fix(dedupe): a retired member dismisses only the pairs that name it"
```

---

### Task 5: F5 — stranded merges are reaped by the sweep

**Files:**
- Modify: `src/jobs/dedupe.rs` (Duplicate arm, autonomous settle, ~line 357)
- Modify: `src/store/lineage.rs` (new query `stranded_merges`)
- Modify: `src/jobs/merge.rs` (new fn `reap_stranded`)
- Modify: `src/jobs/consolidate.rs` (`run`, after the `merged_with_active_roots` block, ~line 316)
- Test: `src/jobs/consolidate.rs` tests

**Interfaces:**
- Consumes: `set_pair_merged`, `reopen_pairs_merged_into` (Task 1), `MAX_ATTEMPTS` (`crate::store::jobs`), `delete_job`.
- Produces:
  - `Store::stranded_merges(&self, limit: i64) -> Result<Vec<String>>`
  - `merge::reap_stranded(core: &Core, merged_id: &str) -> Result<()>`

- [ ] **Step 1: Write the failing test** (in `src/jobs/consolidate.rs` tests)

```rust
#[tokio::test]
async fn a_merge_that_can_never_embed_is_reaped_and_its_pairs_reopened() {
    // The pairs were settled "merged into m" the moment the merge was
    // written. If the embed then fails permanently the merge is stranded
    // active-but-unindexed, the roots are never superseded, and the
    // NoConflict pairs mean the duplicates can never be merged again.
    use crate::store::artifacts::NewMerged;
    let core = test_core().await;
    let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
    let m = core
        .store
        .insert_merged_artifact(
            &NewMerged {
                text: "both".into(), title: None, category: None,
                tags: vec![], caveats: vec![],
            },
            &[ids[0].clone(), ids[1].clone()],
        )
        .await
        .unwrap();
    core.store.enqueue(crate::store::jobs::Stage::Embed, "artifact", &m.id).await.unwrap();
    core.store.record_pair(&ids[0], &ids[1], 0.91).await.unwrap();
    let pid = core.store.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
    core.store.set_pair_merged(pid, &m.id, Some("same claim")).await.unwrap();
    // The embed job has exhausted its retries and cannot succeed.
    sqlx::query("UPDATE jobs SET attempts = ? WHERE target_id = ?")
        .bind(crate::store::jobs::MAX_ATTEMPTS)
        .bind(&m.id)
        .execute(&core.store.pool)
        .await
        .unwrap();

    run(&core).await.unwrap();

    assert_eq!(
        core.store.get_artifact(&m.id).await.unwrap().status,
        ArtifactStatus::Deprecated,
        "the stranded merge should be retired"
    );
    let p = core.store.get_pair(pid).await.unwrap();
    assert_eq!(p.state, crate::store::pairs::PairState::Contradiction, "the pair goes back to a person");
    for id in &ids {
        assert_eq!(core.store.get_artifact(id).await.unwrap().status, ArtifactStatus::Active);
    }
    // And a healthy in-flight merge is left alone.
    let m2 = core.store.insert_merged_artifact(
        &NewMerged { text: "x".into(), title: None, category: None, tags: vec![], caveats: vec![] },
        &[ids[0].clone(), ids[1].clone()],
    ).await.unwrap();
    core.store.enqueue(crate::store::jobs::Stage::Embed, "artifact", &m2.id).await.unwrap();
    run(&core).await.unwrap();
    assert_eq!(core.store.get_artifact(&m2.id).await.unwrap().status, ArtifactStatus::Active);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p engram a_merge_that_can_never_embed` — Expected: compile FAIL (`stranded_merges` missing) or assertion FAIL (merge still active).

- [ ] **Step 3: Implement**

`src/store/lineage.rs`:

```rust
/// Active merged artifacts whose embedding can no longer arrive: no live
/// embed job below the retry ceiling. The write path settles the pairs the
/// moment the merge is written, so a merge stuck here is invisible to
/// search, its roots were never superseded, and nothing else would notice.
pub async fn stranded_merges(&self, limit: i64) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT a.id FROM artifacts a
          WHERE a.provenance = 'merged'
            AND a.status = 'active'
            AND a.superseded_by IS NULL
            AND a.embed_state != 'embedded'
            AND NOT EXISTS (
                  SELECT 1 FROM jobs j
                   WHERE j.stage = 'embed'
                     AND j.target_id = a.id
                     AND j.state IN ('pending', 'running')
                     AND j.attempts < ?)
          LIMIT ?",
    )
    .bind(crate::store::jobs::MAX_ATTEMPTS)
    .bind(limit)
    .fetch_all(&self.pool)
    .await?;
    Ok(rows.iter().map(|r| r.get("id")).collect())
}
```

`src/jobs/merge.rs`:

```rust
/// Retire a merge whose embedding can never arrive, and hand its pairs back
/// to a person.
///
/// Safe by the write path's own ordering: the roots are superseded only
/// after the embed lands, so a stranded merge has hidden nothing — the base
/// is exactly what it was before the verdict, plus one unindexed artifact.
/// Deprecated rather than deleted for the same reason `undo` deprecates:
/// the lineage is the record of what was attempted.
///
/// Contradiction rather than Pending for the reopened pairs: re-arming the
/// model would regenerate the same unembeddable draft, at full price,
/// forever.
pub async fn reap_stranded(core: &Core, merged_id: &str) -> Result<()> {
    let m = core.store.get_artifact(merged_id).await?;
    if m.provenance != Provenance::Merged
        || m.status != ArtifactStatus::Active
        || m.superseded_by.is_some()
        || m.embed_state == crate::store::artifacts::EmbedState::Embedded
    {
        // The embed landed (or someone else acted) between the scan and
        // here. Nothing is stranded any more.
        return Ok(());
    }
    core.deprecate(&m.id).await?;
    let reopened = core
        .store
        .reopen_pairs_merged_into(&m.id, "the merged text could not be indexed; resolve by hand")
        .await?;
    // The forever-retrying job is the only signal this state used to have;
    // with the merge retired it is pure noise.
    core.store.delete_job(Stage::Embed, &m.id).await?;
    tracing::warn!(merged = %m.id, reopened, "reaped a merge that could not be embedded");
    Ok(())
}
```

`src/jobs/dedupe.rs` Duplicate arm, autonomous branch — replace the `settle_all(NoConflict, "merged into …")` with per-pair stamping so the reap can find them:

```rust
let sources: Vec<String> = s.members.iter().map(|m| m.id.clone()).collect();
let m = crate::jobs::merge::write(core, draft, &sources).await?;
// `merged_into` rather than a detail string: if the embed never lands,
// the sweep's reap has to find exactly these pairs and reopen them.
for pr in &s.pairs {
    core.store
        .set_pair_merged(pr.id, &m.id, s.detail.as_deref())
        .await?;
}
Ok(())
```

`src/jobs/consolidate.rs` `run`, directly after the `merged_with_active_roots` repair block:

```rust
// The opposite failure: a merge that will never be embedded. Its pairs
// are already settled, its roots were never superseded, and its only
// signal was a forever-retrying embed job.
match core.store.stranded_merges(50).await {
    Ok(stranded) => {
        for id in stranded {
            if let Err(e) = crate::jobs::merge::reap_stranded(core, &id).await {
                tracing::warn!(merged = %id, error = %e, "could not reap a stranded merge");
            }
        }
    }
    Err(e) => tracing::warn!(error = %e, "could not look for stranded merges"),
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p engram` — Expected: PASS. Any existing test asserting the `"merged into {id}"` detail on settled pairs should be updated to assert `merged_into == Some(m.id)` instead.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "fix(consolidate): a merge that can never embed is reaped, not stranded"
```

---

### Task 6: F4 — a transient read error must not close a filed pair

**Files:**
- Modify: `src/jobs/consolidate.rs` (member-read loop ~line 376, closing pass ~line 446; extract pure helper)
- Test: `src/jobs/consolidate.rs` tests

**Interfaces:**
- Produces: `fn close_filed_pair(a_known: bool, b_known: bool, a_live: bool, b_live: bool, a_id: &str, b_id: &str) -> Option<String>` — `None` = leave the pair filed; `Some(detail)` = close as NoConflict with that detail.

- [ ] **Step 1: Write the failing test** (pure-function test; store-level fault injection isn't feasible with SQLite here, so the decision is extracted and tested directly)

```rust
#[test]
fn an_unreadable_member_leaves_the_filed_pair_alone() {
    // A transient BUSY on one member used to read as "gone", closing the
    // pair as NoConflict while both artifacts were live — permanently,
    // because record_settled_pair only updates pending rows.
    assert_eq!(close_filed_pair(false, true, false, true, "a", "b"), None);
    assert_eq!(close_filed_pair(true, false, true, false, "a", "b"), None);
    // Both readable and live: genuinely unanswered, stays filed.
    assert_eq!(close_filed_pair(true, true, true, true, "a", "b"), None);
    // Known-gone or known-hidden sides do close.
    assert_eq!(
        close_filed_pair(true, true, true, false, "a", "b").as_deref(),
        Some("near-identical; a kept")
    );
    assert_eq!(
        close_filed_pair(true, true, false, false, "a", "b").as_deref(),
        Some("near-identical; neither side is in results any more")
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p engram an_unreadable_member_leaves` — Expected: compile FAIL — `close_filed_pair` does not exist.

- [ ] **Step 3: Implement**

Pure helper in `src/jobs/consolidate.rs`:

```rust
/// Whether the clustering pass answered a filed near-identical pair, and
/// with what record. `known` is false when the member's row could not be
/// read this sweep — an unreadable member is not a gone member, and closing
/// on a transient store error would settle a live pair permanently
/// (`record_settled_pair` only updates pending rows).
fn close_filed_pair(
    a_known: bool,
    b_known: bool,
    a_live: bool,
    b_live: bool,
    a_id: &str,
    b_id: &str,
) -> Option<String> {
    if !a_known || !b_known {
        return None;
    }
    match (a_live, b_live) {
        (true, true) => None,
        (true, false) => Some(format!("near-identical; {a_id} kept")),
        (false, true) => Some(format!("near-identical; {b_id} kept")),
        (false, false) => Some("near-identical; neither side is in results any more".to_string()),
    }
}
```

Member-read loop: add `let mut unknown: HashSet<String> = HashSet::new();` beside `live`, and split the error cases:

```rust
let c = match core.store.get_artifact(id).await {
    Ok(c) => c,
    Err(Error::NotFound) => {
        tracing::debug!(artifact_id = %id, "pair names an artifact that is gone");
        continue;
    }
    Err(e) => {
        // Unreadable is not gone. The closing pass must not settle a
        // pair on the strength of a store that was briefly unwell.
        tracing::warn!(artifact_id = %id, error = %e, "could not read a clustered artifact this sweep");
        unknown.insert(id.clone());
        continue;
    }
};
```

Closing pass body becomes:

```rust
for p in &filed {
    let alive = |id: &String| live.contains(id) && !hidden.contains(id);
    let Some(detail) = close_filed_pair(
        !unknown.contains(&p.a_id),
        !unknown.contains(&p.b_id),
        alive(&p.a_id),
        alive(&p.b_id),
        &p.a_id,
        &p.b_id,
    ) else {
        continue;
    };
    if let Err(e) = core
        .store
        .set_pair_state(p.id, crate::store::pairs::PairState::NoConflict, Some(&detail))
        .await
    {
        tracing::warn!(pair = p.id, error = %e, "could not close a settled near-identical pair");
    }
}
```

Keep the existing block comment above the pass; extend it with one line: an unreadable member leaves the pair filed for the next sweep.

- [ ] **Step 4: Run tests** — `cargo test -p engram` — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "fix(consolidate): an unreadable member does not close a filed pair"
```

---

### Task 7: F3 — an operator's partial restore survives the sweep

**Files:**
- Modify: `src/store/mod.rs` (`ADDED_COLUMNS`) + schema file (`artifact_sources` CREATE TABLE)
- Modify: `src/store/lineage.rs` (`merged_with_active_roots`; new fns `mark_source_restored`, `roots_to_hide`)
- Modify: `src/jobs/merge.rs` (`finish` uses `roots_to_hide`)
- Modify: `src/core/ingest.rs` (`unsupersede`)
- Test: `src/jobs/merge.rs` tests

**Interfaces:**
- Produces:
  - `artifact_sources.restored INTEGER NOT NULL DEFAULT 0` (schema + `ADDED_COLUMNS`).
  - `Store::mark_source_restored(&self, root_id: &str) -> Result<()>` — sets `restored = 1` on every lineage row naming that root.
  - `Store::roots_to_hide(&self, child_id: &str) -> Result<Vec<String>>` — `SELECT root_id FROM artifact_sources WHERE child_id = ? AND restored = 0 ORDER BY root_id`.
- `roots_of` is deliberately unchanged: flattening for the model must still see the true captured roots.

- [ ] **Step 1: Write the failing test** (in `src/jobs/merge.rs` tests; mirror the setup of the existing `undoing_a_merge_survives_the_next_sweep` test at ~line 596, which already builds a finished merge — reuse its seeding verbatim up to the point where the merge's roots are superseded)

```rust
#[tokio::test]
async fn restoring_one_merge_source_survives_the_next_sweep() {
    // "Put it back" on a merge-hidden artifact used to last exactly one
    // sweep: merged_with_active_roots cannot tell a crash-interrupted
    // merge from an operator's explicit restore, and finish re-hid it.
    // <seeding: as in undoing_a_merge_survives_the_next_sweep — build a
    //  finished merge m over roots [a, b], both superseded by m>
    core.unsupersede(&a).await.unwrap();

    crate::jobs::consolidate::run(&core).await.unwrap();

    let back = core.store.get_artifact(&a).await.unwrap();
    assert!(
        back.superseded_by.is_none(),
        "the sweep re-hid an artifact an operator had explicitly restored"
    );
    // The rest of the merge is untouched: b stays hidden, m stays active.
    assert!(core.store.get_artifact(&b).await.unwrap().superseded_by.is_some());
    assert_eq!(core.store.get_artifact(&m).await.unwrap().status, ArtifactStatus::Active);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p engram restoring_one_merge_source_survives` — Expected: FAIL — `a.superseded_by` is `Some(m)` again after the sweep.

- [ ] **Step 3: Implement**

Schema: `ADDED_COLUMNS` append (comment: the operator's partial restore; `0` on every existing row, which is correct — nothing predating the column was explicitly restored):

```rust
("artifact_sources", "restored", "INTEGER NOT NULL DEFAULT 0"),
```

plus `restored INTEGER NOT NULL DEFAULT 0` in the `artifact_sources` CREATE TABLE.

`src/store/lineage.rs`:

```rust
/// Record that an operator explicitly restored this captured root: no
/// repair may hide it behind a merge again. Every merge naming it, not one
/// — the operator's decision is about the root, not about a lineage edge.
/// A *new* merge decision may still hide it: `insert_merged_artifact`
/// writes fresh rows with `restored = 0`, and that is new evidence rather
/// than a repair of old state.
pub async fn mark_source_restored(&self, root_id: &str) -> Result<()> {
    sqlx::query("UPDATE artifact_sources SET restored = 1 WHERE root_id = ?")
        .bind(root_id)
        .execute(&self.pool)
        .await?;
    Ok(())
}

/// The roots `finish` is allowed to hide: the lineage minus every root an
/// operator explicitly restored. Distinct from `roots_of`, which answers
/// "what was this written from" and must keep seeing the true closure.
pub async fn roots_to_hide(&self, child_id: &str) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT root_id FROM artifact_sources
          WHERE child_id = ? AND restored = 0 ORDER BY root_id",
    )
    .bind(child_id)
    .fetch_all(&self.pool)
    .await?;
    Ok(rows.iter().map(|r| r.get("root_id")).collect())
}
```

`merged_with_active_roots`: add `AND s.restored = 0` to the WHERE clause, and extend its doc comment: a root an operator explicitly restored is not an unfinished merge, and the repair must not see it.

`src/jobs/merge.rs` `finish`: replace the `roots_of` read with:

```rust
for root in core.store.roots_to_hide(&m.id).await? {
    let Ok(r) = core.store.get_artifact(&root).await else {
        continue;
    };
    ...
    if let Err(e) = core.supersede(&root, &m.id).await {
```

(loop body otherwise unchanged; adjust for `root: String` instead of `&String`).

`src/core/ingest.rs` `unsupersede` — read the winner before clearing, mark after both stores agree:

```rust
pub async fn unsupersede(&self, artifact_id: &str) -> Result<()> {
    let winner = self.store.get_artifact(artifact_id).await?.superseded_by;
    // (existing body: mark dirty, payload, row, clear marker)
    ...
    // A restore out of a merge is an operator overruling the merge for
    // this one source. Recorded on the lineage, or the sweep's
    // unfinished-merge repair re-hides it on the next tick, every tick.
    if let Some(w) = winner
        && let Ok(wc) = self.store.get_artifact(&w).await
        && wc.provenance == crate::store::artifacts::Provenance::Merged
    {
        self.store.mark_source_restored(artifact_id).await?;
    }
    tracing::info!(artifact_id, "restored a superseded artifact to search");
    Ok(())
}
```

(`reactivate` routes merge-hidden artifacts through `unsupersede`, so both buttons are covered. `merge::undo` deprecates the merge itself, so stray `restored = 1` rows under a deprecated merge are inert.)

- [ ] **Step 4: Run tests** — `cargo test -p engram` — Expected: PASS, including `a_merge_whose_roots_were_never_superseded_is_finished_by_the_next_sweep` (fresh lineage rows have `restored = 0`, so `finish` still hides them).

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "fix(merge): an operator's restore of a merge source survives the sweep"
```

---

### Task 8: F8 — an orphaned merge is never its own root

**Files:**
- Modify: `src/store/lineage.rs` (`roots_of`)
- Modify: `src/jobs/dedupe.rs` (`run`, after `roots_of`, ~line 91)
- Test: `src/jobs/dedupe.rs` tests

**Interfaces:**
- Changed contract: `roots_of` returns an **empty** `Vec` for a merged artifact with no lineage rows (previously: the artifact itself). Captured artifacts keep the self-root fallback. Before relying on this, `grep -rn "roots_of" src/` and check every caller tolerates an empty entry (`merge::finish` no longer calls it after Task 7; `insert_merged_artifact`'s flattening treats "no rows" per its own logic — verify it maps a rootless merged member to nothing rather than to itself; if it self-roots there too, apply the same provenance check in that flattening).

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn a_merge_that_lost_its_sources_is_never_shown_as_an_original() {
    // The self-root fallback put a model-synthesized paraphrase in the
    // prompt as a captured original, and a Duplicate verdict would then
    // record a merged artifact as root_id — paraphrase drift, one
    // generation per merge, which the lineage design exists to prevent.
    let mut core = test_core().await;
    core.consolidate.autonomous = true;
    let completer = Arc::new(ScriptedCompleter::new(vec![]));
    core.completer = completer.clone();
    let ids = disagreeing(&core).await;
    let m = core
        .store
        .insert_merged_artifact(
            &crate::store::artifacts::NewMerged {
                text: "a paraphrase".into(), title: None, category: None,
                tags: vec![], caveats: vec![],
            },
            &[ids[0].clone()],
        )
        .await
        .unwrap();
    // Its every source cascades away, as a corpus deletion does.
    sqlx::query("DELETE FROM artifact_sources WHERE child_id = ?")
        .bind(&m.id).execute(&core.store.pool).await.unwrap();

    let pair = queue_pair(&core, &m.id, &ids[1]).await;
    run(&core, &pair.to_string()).await.unwrap();

    assert_eq!(completer.calls(), 0, "a rootless merge reached the model as an original");
    assert_eq!(
        core.store.get_pair(pair).await.unwrap().state,
        PairState::Contradiction,
        "the component goes to a person rather than being judged on a paraphrase"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p engram a_merge_that_lost_its_sources` — Expected: FAIL — the completer is called (empty script panics or call count is 1).

- [ ] **Step 3: Implement**

`roots_of` fallback branch:

```rust
let entry = if roots.is_empty() {
    // No lineage rows means a captured artifact — or a merged one every
    // root of which has since been deleted. A captured artifact is its
    // own root; a merged one is nobody's: its text is a synthesis, and
    // handing it back as a root is how a paraphrase of a paraphrase
    // ends up in a prompt as an original. The empty answer makes that
    // state visible to the caller instead.
    let provenance: String =
        sqlx::query_scalar("SELECT provenance FROM artifacts WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .unwrap_or_else(|| "captured".to_string());
    if provenance == "merged" { vec![] } else { vec![id.clone()] }
} else {
    roots
};
out.insert(id.clone(), entry);
```

`src/jobs/dedupe.rs` `run`, right after `let root_map = …`:

```rust
// A member with no roots at all is a merge whose sources were deleted
// out from under it. Its text is a paraphrase with nothing behind it —
// not something to show the model as an original, and not something a
// rule can settle. A person decides.
if member_ids.iter().any(|mid| root_map.get(mid).is_none_or(|r| r.is_empty())) {
    settle_all(
        core,
        &pairs,
        PairState::Contradiction,
        Some("a merged member has lost its sources; resolve by hand"),
    )
    .await?;
    return Ok(());
}
```

Update the `roots_of` doc comment (its current text explains the old fallback) and the module-header sentence in `lineage.rs` that describes it.

- [ ] **Step 4: Run tests** — `cargo test -p engram` — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "fix(lineage): a merge that lost every source is nobody's root"
```

---

### Task 9: F9 — a merge restored from the index is flagged as having lost its lineage

**Files:**
- Modify: `src/core/ingest.rs` (`heal_store_drift` restore loop, ~line 647)
- Test: `src/core/ingest.rs` tests (or wherever `heal_store_drift` tests live — `grep -rn "heal_store_drift" src/ --include=*.rs -l`)

**Interfaces:**
- Consumes: `set_artifact_flags` (exists), `Provenance::Merged`, flag string `"orphaned_source"` (the one `flag_orphans` and the UI already read).

- [ ] **Step 1: Write the failing test** (place beside the existing `heal_store_drift` tests, reusing their vector-point seeding idiom — the test around `ingest.rs:1580` restores an artifact from a surviving payload; copy its setup with `provenance: Some("merged".into())` in the payload)

```rust
#[tokio::test]
async fn a_merge_restored_from_the_index_is_flagged_as_orphaned() {
    // restore_artifact cannot recreate lineage rows and leaves
    // source_count at 0, so 0 > COUNT(*) never fires and nothing ever
    // flagged the restored merge — it became its own root silently.
    // <setup: as in the existing restore test, but the payload's
    //  provenance is "merged" and corpus_id handling follows the
    //  Provenance::Merged branch>
    core.heal_store_drift().await.unwrap();

    let c = core.store.get_artifact(&artifact_id).await.unwrap();
    assert!(
        c.flags.iter().any(|f| f == "orphaned_source"),
        "a restored merge must say it cannot support its provenance claim"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p engram a_merge_restored_from_the_index` — Expected: FAIL — no flag set.

- [ ] **Step 3: Implement** — in the restore loop, after `if self.store.restore_artifact(&restored).await? {`:

```rust
if self.store.restore_artifact(&restored).await? {
    out.rows_restored += 1;
    // A payload records neither source_count nor lineage rows, so a
    // restored merge definitionally cannot support its provenance
    // claim — and `merged_missing_a_source` (0 > 0) will never say
    // so. Said here instead, with the flag that pass would have set.
    if provenance == crate::store::artifacts::Provenance::Merged {
        self.store
            .set_artifact_flags(
                &p.artifact_id,
                &["orphaned_source".to_string()],
                Some("restored from the index; the record of its sources was lost"),
            )
            .await?;
    }
    self.store
        .enqueue(Stage::Embed, "artifact", &p.artifact_id)
        .await?;
}
```

(With Task 8 in place, such a merge also never self-roots — the flag is the operator-visible half, `roots_of` is the invariant half.)

- [ ] **Step 4: Run tests** — `cargo test -p engram` — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "fix(ingest): a merge restored from the index is flagged as orphaned"
```

---

### Task 10: F10 — lifecycle writes are serialized against the marker repair

**Files:**
- Modify: `src/core/mod.rs` (`Core` struct + its constructor(s))
- Modify: `src/core/ingest.rs` (`supersede`, `unsupersede`, `deprecate`, `reactivate`, `repoint_supersession`)
- Modify: `src/jobs/consolidate.rs` (`repair_lifecycle_drift`)

**Interfaces:**
- Produces: `Core.lifecycle_lock: Arc<tokio::sync::Mutex<()>>` (constructed as `Arc::new(tokio::sync::Mutex::new(()))` wherever `Core` is built — `grep -rn "Core {" src/` for every literal, including `test_support`).

No new deterministic test: the race needs two tasks interleaved between specific awaits, which nothing in this test harness can schedule reliably. The fix is structural (mutual exclusion); the existing lifecycle test suite guards against regressions in each path's behavior. State this in the commit message body.

- [ ] **Step 1: Implement**

`Core`:

```rust
/// Serializes every lifecycle transition against the sweep's marker
/// repair. Each transition is two writes to two stores plus a dirty
/// marker, none of it atomic; without mutual exclusion the repair can
/// read a stale row mid-reveal, write the old state back over the new
/// payload, and the reveal then clears the marker — row active, payload
/// hidden, and nothing left that would ever notice. Shared by every
/// clone, like the background queue.
pub lifecycle_lock: Arc<tokio::sync::Mutex<()>>,
```

In `ingest.rs`, split `unsupersede` into a public locking wrapper and a private body so `reactivate` never locks twice:

```rust
pub async fn unsupersede(&self, artifact_id: &str) -> Result<()> {
    let _guard = self.lifecycle_lock.lock().await;
    self.unsupersede_locked(artifact_id).await
}

async fn unsupersede_locked(&self, artifact_id: &str) -> Result<()> {
    // (entire current body, including Task 7's restored-marking)
}
```

`reactivate`:

```rust
pub async fn reactivate(&self, id: &str) -> Result<()> {
    let _guard = self.lifecycle_lock.lock().await;
    if self.store.get_artifact(id).await?.superseded_by.is_some() {
        return self.unsupersede_locked(id).await;
    }
    // (rest of current body)
}
```

`supersede`, `deprecate`, `repoint_supersession`: `let _guard = self.lifecycle_lock.lock().await;` as the first line (none of them calls another locked fn — verify by reading each body before adding the guard; `merge::finish` and the sweep call them from *outside*, which is fine, the guard is per-call).

`repair_lifecycle_drift` in `consolidate.rs`:

```rust
async fn repair_lifecycle_drift(core: &Core) -> Result<usize> {
    // Under the same lock as every lifecycle transition: the repair
    // reads rows, writes payloads and clears markers, and interleaving
    // that with a payload-first reveal is exactly the sequence that
    // hides an artifact with no marker left to find it by.
    let _guard = core.lifecycle_lock.lock().await;
    let dirty = core.store.dirty_lifecycle_artifacts(DRIFT_SCAN).await?;
    // (rest unchanged)
```

- [ ] **Step 2: Run tests** — `cargo test -p engram` — Expected: PASS (behavior unchanged single-threaded; watch for a deadlock-shaped test hang, which would mean a locked fn calls another locked fn — re-check call graphs).

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "fix(core): lifecycle transitions and the marker repair exclude each other"
```

Body: note that the interleaving (repair reads stale row → reveal writes payload → repair writes stale payload → reveal clears marker) is unreproducible deterministically in tests, so the fix is mutual exclusion with no new test; existing lifecycle tests cover each path.

---

### Task 11: F11 — legacy `'judge'` job rows

**Files:**
- Modify: `src/store/mod.rs` (migration section — near the existing one-off `ALTER TABLE`/cleanup statements, `grep -n "DROP COLUMN" src/store/mod.rs` for the spot)
- Test: `src/store/jobs.rs` tests

**Interfaces:** none new.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn a_legacy_judge_row_is_not_claimed_as_something_else() {
    // Stage::parse has no 'judge' arm and claim_job falls back to
    // Synthesize, so a leftover row from the branch this replaced was
    // claimed as a synthesize job aimed at a pair id.
    let s = Store::memory().await.unwrap();
    sqlx::query(
        "INSERT INTO jobs (stage, target_kind, target_id, state, run_after, attempts, seq)
         VALUES ('judge', 'pair', '17', 'pending', 0, 0, 0)",
    )
    .execute(&s.pool)
    .await
    .unwrap();

    s.migrate().await.unwrap();

    assert!(
        s.claim_job().await.unwrap().is_none(),
        "a legacy judge row survived migration and was claimed"
    );
}
```

(If the migration entry point is not named `migrate`, find the fn that runs `ADDED_COLUMNS` — the test calls that. If it is private, make the test a sibling in `store/mod.rs` tests instead.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p engram a_legacy_judge_row` — Expected: FAIL — the row is claimed (as Synthesize).

- [ ] **Step 3: Implement** — in the migration path, after the `ADDED_COLUMNS` loop:

```rust
// The judge became the dedupe unit on this branch and its stage name
// went with it. A leftover row would otherwise be claimed under
// `Stage::parse`'s Synthesize fallback and aimed at a pair id. Deleted
// rather than renamed: the pair is still pending, so the next sweep
// re-arms it under its real stage, and a rename could collide with a
// dedupe row the sweep has already written for the same pair.
sqlx::query("DELETE FROM jobs WHERE stage = 'judge'")
    .execute(&self.pool)
    .await?;
```

- [ ] **Step 4: Run tests** — `cargo test -p engram` — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "fix(store): a legacy judge job row is deleted, not misrouted"
```

---

### Task 12: F12 — `arm_dedupe` does nothing when its budget is zero

**Files:**
- Modify: `src/jobs/consolidate.rs` (`arm_dedupe`, ~line 549)
- Test: `src/jobs/consolidate.rs` tests

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn a_zero_dedupe_budget_arms_nothing_and_reads_nothing() {
    // With the budget at zero every tick still ran the 200-row query and
    // logged "budget spent" — dead work on the sweep's hot path, and a
    // misleading log line. The observable half: nothing is armed and no
    // pair is touched.
    let mut core = test_core().await;
    core.consolidate.max_dedupe_per_tick = 0;
    disagreeing(&core).await;
    let out = run(&core).await.unwrap();
    assert_eq!(out.judged, 0);
    assert_eq!(
        core.store.pairs_by_state(PairState::Pending, 10).await.unwrap().len(),
        1,
        "the pending pair must be left exactly as it was"
    );
}
```

- [ ] **Step 2: Run test to verify it fails or passes vacuously**

Run: `cargo test -p engram a_zero_dedupe_budget` — This may already PASS behaviorally (the waste is the query, not the outcome). Either way it pins the contract; proceed.

- [ ] **Step 3: Implement** — first lines of `arm_dedupe`:

```rust
// `max_dedupe_per_tick = 0` is the off switch for the model (see run's
// comment on the `autonomous` flag). Off means off: no 200-row read per
// tick, and no "budget spent" log line implying a budget was spent.
if core.consolidate.max_dedupe_per_tick == 0 {
    return Ok(0);
}
```

- [ ] **Step 4: Run tests** — `cargo test -p engram` — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "fix(consolidate): a zero dedupe budget arms nothing and reads nothing"
```

---

### Task 13: F13 — `open_component` grows by worklist, not by rescan

**Files:**
- Modify: `src/store/pairs.rs` (`open_component` fixed-point loop, ~lines 342-366)

Behavior is unchanged (same members, same picked set), so the existing `open_component` tests are the spec — no new test.

- [ ] **Step 1: Implement** — replace the loop with an adjacency-indexed worklist:

```rust
// Adjacency once, then a worklist. The fixed point rescanned the whole
// window per growth pass — quadratic at the 5 000-row window for a
// long chain — for an answer a plain flood fill gives in one pass over
// the edges.
let mut by_artifact: std::collections::HashMap<&str, Vec<usize>> = Default::default();
for (i, p) in open.iter().enumerate() {
    by_artifact.entry(p.a_id.as_str()).or_default().push(i);
    by_artifact.entry(p.b_id.as_str()).or_default().push(i);
}
let mut picked: std::collections::HashSet<i64> = [seed.id].into_iter().collect();
let mut queue: Vec<&str> = vec![seed.a_id.as_str(), seed.b_id.as_str()];
let mut seen: std::collections::HashSet<&str> = queue.iter().copied().collect();
while let Some(id) = queue.pop() {
    for &i in by_artifact.get(id).into_iter().flatten() {
        let p = &open[i];
        picked.insert(p.id);
        for other in [p.a_id.as_str(), p.b_id.as_str()] {
            if seen.insert(other) {
                queue.push(other);
            }
        }
    }
}
Ok(open.into_iter().filter(|p| picked.contains(&p.id)).collect())
```

(Delete the now-dead `members` set and the old loop; keep the surrounding comments about the seed always being in the component.)

- [ ] **Step 2: Run tests** — `cargo test -p engram open_component` then full `cargo test -p engram` — Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "perf(pairs): open_component flood-fills instead of rescanning the window"
```

---

### Task 14: F14 — comments stop claiming the full reconcile is gated

**Files:**
- Modify: `src/jobs/consolidate.rs` (doc comment of `repair_lifecycle_drift` ~line 143-148, doc comment of `full_lifecycle_reconcile` ~line 173-179)

The code is right (the reconcile's own rationale at lines 180-184 argues for keeping it); the comments are wrong.

- [ ] **Step 1: Implement** — two wording fixes:

In `repair_lifecycle_drift`'s doc, replace "`full_lifecycle_reconcile` keeps that scan for the drift that arises with no SQLite write behind it, but it no longer runs every sweep." with:

```
/// `full_lifecycle_reconcile` keeps that scan for the drift that arises with
/// no SQLite write behind it; it still runs every sweep, after this pass.
```

In `full_lifecycle_reconcile`'s doc, replace "Still on the sweep, behind the marker, and the division of labour is the point." with:

```
/// Still on every sweep, after the marker pass, and the division of labour
/// is the point.
```

- [ ] **Step 2: Verify and commit**

Run: `cargo build 2>&1 | tail -3` (comments only; build proves nothing broke) and `cargo fmt`.

```bash
git add -A && git commit -m "docs(consolidate): the full reconcile runs every sweep, and says so"
```

---

## Final verification (after Task 14)

- [ ] `cargo test` — full suite green.
- [ ] `cargo fmt --check` and (if the repo uses it — check CI config) `cargo clippy` — clean.
- [ ] Re-read the Findings Index: each of F1-F14 maps to a merged commit. F1→T3, F2→T4, F3→T7, F4→T6, F5→T1+T5, F6→T3, F7→T1+T2, F8→T8, F9→T9, F10→T10, F11→T11, F12→T12, F13→T13, F14→T14.
- [ ] `git push` to `feat/autonomous-consolidation` only when the user asks.
