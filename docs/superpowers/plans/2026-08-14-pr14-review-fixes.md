# PR 14 Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix every confirmed finding from the multi-agent review of PR 14 (feat/autonomous-consolidation), including the minor reuse/efficiency ones.

**Architecture:** All fixes are local repairs to the consolidation subsystem introduced by this branch: the dedupe judge unit (`src/jobs/dedupe.rs`), merge lifecycle (`src/jobs/merge.rs`), the consolidation sweep (`src/jobs/consolidate.rs`), the pair and lineage stores, and the Ops UI. No schema changes; two new store methods.

**Tech Stack:** Rust, sqlx/SQLite, tokio, axum. Tests are in-file `#[cfg(test)] mod tests` using `crate::core::test_support::test_core()`, `crate::infer::fake::ScriptedCompleter`, and `crate::jobs::consolidate::tests::{seed, seed_titled}`.

**Spec:** The findings come from the code review recorded in this plan's "Findings and decisions" section below (verifier transcripts under the session's task outputs). The branch's own design spec is `docs/superpowers/specs/2026-08-14-autonomous-consolidation-design.md`.

## Global Constraints

- Run `cargo test` after every implementation step; run `cargo fmt` before every commit.
- Comment style: comments state constraints the code cannot show, in the codebase's essay style. Never "fixed per review".
- Test names are full snake_case sentences (`a_retired_member_dismisses_only_its_own_pairs`), matching the existing suites.
- Commit after each task with a conventional-commit message.

## Findings and decisions

Confirmed findings fixed by this plan (task number in parentheses):

1. Stale `member_ids` in the dedupe unit leaks retired members' roots into the prompt, the fan-in cap, `losses()`, and the Replaced-verdict resolution (Task 2).
2. `merge::undo` pressed before the embed lands dismisses nothing and strands the merge's pairs settled forever — silent permanent duplication (Task 4).
3. Proposal-mode sibling pairs record "`X` was superseded" when the supersede was only proposed and may be rejected (Task 3).
4. `WouldMerge` doc promises re-judging on autonomy flip; no code path provides it (Task 5).
5. `flag_orphans` starves: no flag filter / ORDER BY in `merged_missing_a_source`, so 500 permanently-orphaned old merges block new ones forever, and "mark reviewed" is undone next sweep (Task 6).
6. `merge_max_roots = 0` or `1` silently settles every component `Oversized`; `normalize()` doesn't touch it (Task 7).
7. `full_lifecycle_reconcile_scanning` runs outside `lifecycle_lock`, races a concurrent reveal, and writes the corrupted state with no marker left to find it by (Task 8).
8. `heal_dangling_supersessions` takes no lock and never marks dirty; interleaved with `repair_lifecycle_drift` it produces row-Active/payload-Superseded/marker-clear (Task 9).
9. The liveness predicate `status == Active && superseded_by.is_none()` is hand-spelled at 12 production sites (Task 1).
10. `ops()` and `build_artifact_detail()` duplicate the source-rows loop verbatim, and `ops()` is an N+1 over a batch API (Task 10).
11. `open_component`'s `let Some(seed) ... else` arm is unreachable (Task 11).
12. `arm_dedupe` pays two full-row `get_artifact` fetches per pair before the cheap already-queued skip, re-checking the same in-flight pairs every tick (Task 12).

Declined by the user (2026-08-14, "prototype, only used by me — legacy doesn't exist"):

- **Legacy-DB startup refusal stays.** `migrate()`'s hard error on a pre-PR `corpus_id NOT NULL` schema is deliberate and keeps its test. No automated rebuild.
- **The autonomy default flip on upgrade stays.** No carry for configs that never wrote `judge`; fresh-install `autonomous: true` is the spec's choice.
- **Legacy `no_conflict` prefilter pairs stay settled.** No migration reopens them.

Deferred (not in this plan): the altitude finding proposing a single `Core::lifecycle_write` helper owning the lock/marker/ordering protocol. Tasks 8 and 9 fix the two concrete protocol violations; the full refactor of five working mutators is follow-up work, not a review fix.

---

### Task 1: `Chunk::in_results()` — one owner for the liveness predicate

**Files:**
- Modify: `src/store/artifacts.rs` (impl block for `Chunk`, after the struct around line 99)
- Modify: `src/jobs/dedupe.rs:79`, `src/jobs/dedupe.rs:345`
- Modify: `src/jobs/classify.rs:54`
- Modify: `src/jobs/relate.rs:50`
- Modify: `src/jobs/consolidate.rs:440`, `src/jobs/consolidate.rs:657-661`
- Modify: `src/jobs/merge.rs:61-64`, `src/jobs/merge.rs:76`, `src/jobs/merge.rs:184-187`
- Modify: `src/core/ingest.rs:314`
- Modify: `src/web/judge.rs:174`, `src/web/judge.rs:363`
- Test: `src/store/artifacts.rs` tests module

**Interfaces:**
- Produces: `impl Chunk { pub fn in_results(&self) -> bool }` — later tasks (2, 12) call it.

- [ ] **Step 1: Write the failing test** (in `src/store/artifacts.rs` tests)

```rust
#[tokio::test]
async fn in_results_means_active_and_not_superseded() {
    let s = Store::memory().await.unwrap();
    let src = s.insert_corpus("raw", "web", None).await.unwrap();
    let made = s
        .insert_artifacts(&src.id, &[nc(0, "one"), nc(1, "two")])
        .await
        .unwrap();

    assert!(s.get_artifact(&made[0].id).await.unwrap().in_results());
    s.set_superseded_by(&made[0].id, Some(&made[1].id)).await.unwrap();
    assert!(!s.get_artifact(&made[0].id).await.unwrap().in_results());
    s.set_artifact_status(&made[1].id, ArtifactStatus::Deprecated)
        .await
        .unwrap();
    assert!(!s.get_artifact(&made[1].id).await.unwrap().in_results());
}
```

(`Store::memory()` and the `nc` helper are the module's existing test fixtures.)

- [ ] **Step 2: Run it — expect FAIL** with "no method named `in_results`": `cargo test in_results_means`

- [ ] **Step 3: Implement** in `src/store/artifacts.rs`, next to the `Chunk` struct:

```rust
impl Chunk {
    /// Whether search may return this artifact: active and not hidden behind
    /// a winner. This is the predicate every consolidation decision gates on —
    /// what may win a cluster, be shown to the model, or be superseded — so it
    /// has exactly one spelling. A third lifecycle state changes this method,
    /// not twelve call sites.
    pub fn in_results(&self) -> bool {
        self.status == ArtifactStatus::Active && self.superseded_by.is_none()
    }
}
```

- [ ] **Step 4: Replace every inline spelling.** Positive form `c.status == ArtifactStatus::Active && c.superseded_by.is_none()` becomes `c.in_results()`; negated form becomes `!c.in_results()`. Exact sites:
  - `src/jobs/dedupe.rs:79` → `if !c.in_results() {`
  - `src/jobs/dedupe.rs:345` → delete the `live` closure; use `c.in_results()` at its two call sites (lines 346, 352)
  - `src/jobs/classify.rs:54` → `.any(|c| !c.in_results())`
  - `src/jobs/relate.rs:50` → `if !me.in_results() {`
  - `src/jobs/consolidate.rs:440` → negated call
  - `src/jobs/consolidate.rs:657-661` → `if !a.in_results() || !b.in_results() {`
  - `src/jobs/merge.rs:61-64` (`finish`) → `if m.provenance != Provenance::Merged || !m.in_results() { return Ok(()); }`
  - `src/jobs/merge.rs:76` → `if !r.in_results() { continue; }`
  - `src/jobs/merge.rs:184-187` (`reap_stranded`) → `if m.provenance != Provenance::Merged || !m.in_results() || m.embed_state == EmbedState::Embedded {`
  - `src/core/ingest.rs:314` (`repoint_supersession`) → `if !winner.in_results() {`
  - `src/web/judge.rs:174` → `usable: a.in_results(),`
  - `src/web/judge.rs:363` → negated call

  Leave the single-condition `superseded_by.is_none()` payload checks (vector/memory.rs, qdrant.rs, embed.rs, ingest.rs:389/427) alone — they ask a different question.

- [ ] **Step 5: Run the full suite** — `cargo test`. Expected: PASS (pure extraction, zero behavior change).

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "refactor(core): the liveness predicate has one owner, Chunk::in_results"
```

---

### Task 2: Recompute `member_ids` after retired members are dropped

**Files:**
- Modify: `src/jobs/dedupe.rs:120-122`
- Test: `src/jobs/dedupe.rs` tests module

**Interfaces:**
- Consumes: `Chunk::in_results()` from Task 1 (only incidentally; the fix itself is one line).

The bug: `member_ids` is built at line 60 from the pre-partition pairs and never rebuilt after lines 85–113 drop retired members, so `roots_of(&member_ids)` at line 122 resolves roots of artifacts no longer in the question — they reach the prompt, the `merge_max_roots` cap, `losses()`, and the Replaced-verdict resolution.

- [ ] **Step 1: Write the failing test** (in `src/jobs/dedupe.rs` tests):

```rust
#[tokio::test]
async fn a_retired_members_roots_do_not_count_against_the_cap() {
    // C is deprecated while the unit waits. Its pair is dismissed and it is
    // dropped from the component — but its root must also leave the question,
    // or a two-root component at the cap is settled Oversized for fan-in it
    // does not have, and C's text is shown to the model as an original.
    let mut core = test_core().await;
    core.consolidate.autonomous = true;
    core.consolidate.merge_max_roots = 2;
    core.completer = Arc::new(ScriptedCompleter::new(vec![
        r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
    ]));
    let ids = seed(
        &core,
        &[
            ("a text", [1.0, 0.0]),
            ("b text", [0.93, 0.37]),
            ("c text", [0.90, 0.44]),
        ],
    )
    .await;
    let seed_pair = queue_pair(&core, &ids[0], &ids[1]).await;
    queue_pair(&core, &ids[1], &ids[2]).await;
    core.deprecate(&ids[2]).await.unwrap();

    run(&core, &seed_pair.to_string()).await.unwrap();

    assert!(
        core.store
            .pairs_by_state(PairState::Oversized, 10)
            .await
            .unwrap()
            .is_empty(),
        "a retired member's root was counted against merge_max_roots"
    );
    // The live pair reached the model and took its verdict.
    assert_eq!(
        core.store
            .pairs_by_state(PairState::NoConflict, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}
```

- [ ] **Step 2: Run it — expect FAIL**: the pair settles `Oversized` (3 roots > cap 2). `cargo test a_retired_members_roots`

- [ ] **Step 3: Implement.** In `src/jobs/dedupe.rs`, immediately before line 122 (`let root_map = ...`), rebind `member_ids` from the surviving members so the two can never disagree:

```rust
    // Flatten before anything else, and never show the model a merged member's
    // own text.
    //
    // From `members`, not the list the component arrived with: the partition
    // above drops retired members and their pairs, and roots resolved from the
    // stale list would put a retired artifact back into the prompt, the fan-in
    // cap, and the loss check — the question this unit is no longer asking.
    let member_ids: Vec<String> = members.iter().map(|c| c.id.clone()).collect();
    let root_map = core.store.roots_of(&member_ids).await?;
```

Also change line 60's binding from `let mut member_ids` to `let member_ids` if the shadowing makes the `mut` unused (the compiler will say).

- [ ] **Step 4: Run** `cargo test` (whole dedupe suite — `a_retired_member_dismisses_only_its_own_pairs` must still pass). Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "fix(dedupe): roots are resolved from the surviving members, not the stale list"
```

---

### Task 3: Truthful survivor-pair detail, written once

**Files:**
- Modify: `src/jobs/dedupe.rs:372-438` (the `Relation::Replaced` arm of `apply`)
- Test: `src/jobs/dedupe.rs` tests module

Two findings, one edit: (a) in proposal mode the sibling pair's detail claims "`X` was superseded" before any supersede happened; (b) the survivor-Contradiction block is byte-identical in both branches, so a future edit lands in one copy. Partition the pairs once; branch on `autonomous` only where the modes actually differ.

- [ ] **Step 1: Write the failing test:**

```rust
#[tokio::test]
async fn a_proposed_supersede_is_not_recorded_as_a_done_one() {
    // Proposal mode: the supersede is a proposal an operator may reject. The
    // sibling pair's record must not assert an event that has not happened.
    let mut core = test_core().await;
    core.consolidate.autonomous = false;
    core.completer = Arc::new(ScriptedCompleter::new(vec![
        r#"{"relation":"replaced","supersedes":"a","detail":"a is stale"}"#.into(),
    ]));
    let ids = seed(
        &core,
        &[
            ("engram needs Rust 1.21.4 to build.", [1.0, 0.0]),
            ("engram needs Rust 1.30.0 to build.", [0.93, 0.37]),
            ("engram builds with stable Rust.", [0.90, 0.44]),
        ],
    )
    .await;
    let seed_pair = queue_pair(&core, &ids[0], &ids[1]).await;
    queue_pair(&core, &ids[1], &ids[2]).await;

    run(&core, &seed_pair.to_string()).await.unwrap();

    let contradictions = core
        .store
        .pairs_by_state(PairState::Contradiction, 10)
        .await
        .unwrap();
    assert_eq!(contradictions.len(), 1, "the survivor pair escalates");
    let detail = contradictions[0].detail.clone().unwrap_or_default();
    assert!(
        !detail.contains("was superseded"),
        "the record asserts a supersession that is only proposed: {detail}"
    );
    // Nothing was hidden in proposal mode.
    for id in &ids {
        assert!(core.store.get_artifact(id).await.unwrap().in_results());
    }
}
```

Note: `supersedes` letters index the *roots* list, ordered by sorted root id; if the scripted letter doesn't resolve to the oldest artifact the guard downgrades to Conflict and the test still sees a Contradiction — but then the pair naming the obsolete isn't `Superseded`. If the assertion setup proves fragile, pin creation order by seeding the "stale" artifact first (seed inserts in order, `created_at` ascending) and letter `"a"` = lexicographically first root id; adjust the letter after printing `roots` once in a debug run. The behavioral assertions above (no "was superseded" text; nothing hidden) are the contract.

- [ ] **Step 2: Run it — expect FAIL** on the detail assertion. `cargo test a_proposed_supersede_is_not_recorded`

- [ ] **Step 3: Implement.** In `apply`'s `Relation::Replaced` arm, after the `(Some(winner), true)` destructure, replace both per-mode loops with one partition:

```rust
            let (touching, survivors): (Vec<&ArtifactPair>, Vec<&ArtifactPair>) = s
                .pairs
                .iter()
                .partition(|pr| pr.a_id == obsolete || pr.b_id == obsolete);

            if core.consolidate.autonomous {
                // The side effect FIRST. A failure here leaves every pair
                // pending, so the unit retries under the queue's backoff — the
                // reverse order left the verdict recorded on the pairs but
                // never applied, permanently, because run() skips a component
                // whose seed is no longer Pending.
                core.supersede(&obsolete, &winner).await?;
                tracing::info!(superseded = %obsolete, by = %winner, "applied a replacement");
                for pr in &touching {
                    // As the manual apply settles it (`apply_pair_supersede_ui`):
                    // done, with the model's reasoning kept as the record of why.
                    core.store
                        .set_pair_state(pr.id, PairState::Dismissed, s.detail.as_deref())
                        .await?;
                }
            } else {
                // Proposal mode: nothing is hidden, the pair carries the
                // direction and an operator confirms via "apply supersede".
                for pr in &touching {
                    core.store
                        .set_pair_superseded(pr.id, &obsolete, s.detail.as_deref())
                        .await?;
                }
                tracing::info!(obsolete = %obsolete, "proposed a replacement, pending confirmation");
            }

            // Both sides survived these pairs. Writing the direction on them
            // named an artifact the pair does not contain. Not left pending
            // either: the roots this verdict was drawn from are unchanged, so
            // re-arming would build the identical prompt and receive the
            // identical answer forever. An unanswered question goes where the
            // others go: to a person.
            //
            // Past tense only where the supersede actually ran. In proposal
            // mode it may yet be rejected, and a record asserting it happened
            // would outlive the rejection.
            let survivor_detail = if core.consolidate.autonomous {
                format!("{obsolete} was superseded; these two were not separated")
            } else {
                format!("superseding {obsolete} was proposed; these two were not separated")
            };
            for pr in &survivors {
                core.store
                    .set_pair_state(pr.id, PairState::Contradiction, Some(&survivor_detail))
                    .await?;
            }
            Ok(())
```

Keep the surrounding rationale comments that still apply; the two deleted copies' comments are consolidated above.

- [ ] **Step 4: Run** `cargo test` (the whole dedupe suite — the autonomous Replaced tests must still pass). Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "fix(dedupe): a proposal-mode survivor pair records a proposal, not a fait accompli"
```

---

### Task 4: `merge::undo` before the embed lands reopens the pairs it settled

**Files:**
- Modify: `src/store/pairs.rs` (new method, next to `reopen_pairs_merged_into` at line 324)
- Modify: `src/jobs/merge.rs:156-165` (`undo`)
- Test: `src/jobs/merge.rs` tests module

The bug: pairs are settled `no_conflict, merged_into = M` at write time, but roots are superseded only at embed time. `undo` pressed in between finds `artifacts_superseded_by(M)` empty, dismisses nothing, deprecates M — and the pairs stay settled forever pointing at a deprecated merge, with both duplicates active side by side.

**Interfaces:**
- Produces: `Store::dismiss_pairs_merged_into(&self, merged_id: &str, detail: &str) -> Result<u64>`

- [ ] **Step 1: Write the failing test** (in `src/jobs/merge.rs` tests; `write` + `draft` helpers already exist there):

```rust
#[tokio::test]
async fn undoing_a_merge_before_its_embed_lands_still_releases_its_pairs() {
    // Pairs are settled at write time; roots are superseded at embed time.
    // An undo in between used to find nothing superseded, dismiss nothing,
    // and leave the pairs no_conflict behind a deprecated merge — both
    // duplicates active, and record_pair unable to ever re-file them.
    let core = crate::core::test_support::test_core().await;
    let ids = crate::jobs::consolidate::tests::seed(
        &core,
        &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])],
    )
    .await;
    core.store.record_pair(&ids[0], &ids[1], 0.91).await.unwrap();
    let pair = core
        .store
        .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
        .await
        .unwrap()[0]
        .id;

    let m = write(&core, &draft("a text and b text"), &ids).await.unwrap();
    core.store
        .set_pair_merged(pair, &m.id, Some("duplicate"))
        .await
        .unwrap();
    // No embed ran: nothing is superseded by m yet.

    undo(&core, &m.id).await.unwrap();

    let p = core.store.get_pair(pair).await.unwrap();
    assert_eq!(
        p.state,
        crate::store::pairs::PairState::Dismissed,
        "the pair stayed settled behind a deprecated merge: {p:?}"
    );
    assert_eq!(p.merged_into, None);
}
```

- [ ] **Step 2: Run it — expect FAIL** (state stays `NoConflict`). `cargo test undoing_a_merge_before_its_embed`

- [ ] **Step 3: Implement the store method** in `src/store/pairs.rs`, after `reopen_pairs_merged_into`:

```rust
    /// Dismiss every pair a merge being undone had settled, by the lineage the
    /// settlement recorded. `pairs_among` covers only what the merge had
    /// already hidden — before the embed lands that is nothing, while the
    /// pairs were settled the moment the merge was written. Dismissed, not
    /// Contradiction: an undo is an operator overruling the verdict, and
    /// `record_pair` respecting dismissed rows is what makes that last.
    pub async fn dismiss_pairs_merged_into(&self, merged_id: &str, detail: &str) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE artifact_pairs
                SET state = 'dismissed', detail = ?, merged_into = NULL
              WHERE merged_into = ?",
        )
        .bind(detail)
        .bind(merged_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }
```

- [ ] **Step 4: Call it from `undo`** in `src/jobs/merge.rs`, after the existing `pairs_among` loop (keep that loop — it also catches pairs between restored artifacts that never carried `merged_into`):

```rust
    // And by lineage, for an undo that outran the embed: before `finish` runs,
    // nothing is superseded by this merge, so `restored` above is empty — but
    // the pairs were settled the moment the merge was written, and leaving
    // them would keep the duplicates invisible to every later sweep.
    core.store
        .dismiss_pairs_merged_into(&m.id, "merge undone")
        .await?;
```

- [ ] **Step 5: Run** `cargo test` (the post-embed undo test in merge.rs must still pass). Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "fix(merge): undo before the embed lands releases the pairs the write settled"
```

---

### Task 5: Flipping autonomy on re-arms `WouldMerge` verdicts

**Files:**
- Modify: `src/store/pairs.rs` (new method)
- Modify: `src/jobs/consolidate.rs:606-613` (`arm_dedupe`)
- Test: `src/jobs/consolidate.rs` tests module (or pairs.rs for the store half)

The `WouldMerge` doc (pairs.rs:68-74) promises "flipping autonomy on lets a later unit re-judge and merge"; nothing implements it. User chose: implement the re-arm.

**Interfaces:**
- Produces: `Store::reopen_would_merge_pairs(&self) -> Result<u64>`

- [ ] **Step 1: Write the failing test** (in `src/jobs/consolidate.rs` tests):

```rust
#[tokio::test]
async fn would_merge_verdicts_are_re_armed_once_autonomy_is_on() {
    // Recorded while autonomy was off, the verdict's whole point is to be
    // acted on once the switch flips — the variant's own doc says so. The
    // ticker's arming pass is where the flip becomes visible.
    let mut core = test_core().await;
    core.consolidate.autonomous = true;
    core.consolidate.max_dedupe_per_tick = 5;
    let ids = seed(
        &core,
        &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])],
    )
    .await;
    core.store.record_pair(&ids[0], &ids[1], 0.91).await.unwrap();
    let pair = core
        .store
        .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
        .await
        .unwrap()[0]
        .id;
    core.store
        .set_pair_state(pair, crate::store::pairs::PairState::WouldMerge, Some("same claim"))
        .await
        .unwrap();

    arm_dedupe(&core).await.unwrap();

    assert_eq!(
        core.store.get_pair(pair).await.unwrap().state,
        crate::store::pairs::PairState::Pending,
        "a would_merge verdict stayed stranded after autonomy came on"
    );
}
```

(If `arm_dedupe` and `seed` aren't importable in that tests module, follow how neighbouring `arm_dedupe` tests there reference them.)

- [ ] **Step 2: Run it — expect FAIL** (state stays `WouldMerge`). `cargo test would_merge_verdicts_are_re_armed`

- [ ] **Step 3: Implement the store method** in `src/store/pairs.rs`:

```rust
    /// Hand every recorded would-merge verdict back to the judge queue.
    ///
    /// `would_merge` exists only as a note taken while autonomy is off — the
    /// draft was discarded, so there is nothing to apply directly; the unit
    /// re-judges and merges at the queue's own pace. The detail is kept: it is
    /// the model's recorded reasoning, and `pending` reads it never.
    pub async fn reopen_would_merge_pairs(&self) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE artifact_pairs SET state = 'pending' WHERE state = 'would_merge'",
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }
```

- [ ] **Step 4: Call it from `arm_dedupe`** in `src/jobs/consolidate.rs`, after the `max_dedupe_per_tick == 0` early return and before `pairs_to_judge`:

```rust
    // Verdicts recorded while autonomy was off go back into the queue now
    // that it is on. Idempotent and normally free: with autonomy on, no unit
    // writes would_merge, so this touches rows exactly once per flip.
    if core.consolidate.autonomous {
        let reopened = core.store.reopen_would_merge_pairs().await?;
        if reopened > 0 {
            tracing::info!(reopened, "re-armed would-merge verdicts recorded before autonomy");
        }
    }
```

- [ ] **Step 5: Run** `cargo test`. Expected: PASS (including `would_merge_is_a_state_of_its_own` in pairs.rs).

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat(consolidate): flipping autonomy on re-arms recorded would-merge verdicts"
```

---

### Task 6: `flag_orphans` stops starving, and "reviewed" sticks

**Files:**
- Modify: `src/store/lineage.rs:223-235` (`merged_missing_a_source`)
- Modify: `src/jobs/merge.rs:218-238` (`flag_orphans`)
- Modify: `src/store/artifacts.rs` (new method near `clear_artifact_flags`, line 935)
- Modify: `src/web/ui.rs:1529-1536` (`mark_artifact_reviewed`)
- Test: `src/jobs/merge.rs` tests module

Two halves. (a) Starvation: the SQL has no flag filter and no ORDER BY, so once 500 old merges are permanently orphaned the same rows fill the LIMIT every sweep and new orphans are never flagged; each sweep also burns one `get_artifact` per already-flagged row. (b) The operator's "reviewed" clears the flag, but the row still matches the query, so the next sweep re-flags it — the dismissal doesn't stick.

For (b): reviewing an orphaned merge means the operator accepts it as a merge of what remains, so the review also syncs `source_count` down to the surviving lineage count. The row then leaves the query's result set permanently.

**Interfaces:**
- Produces: `Store::accept_source_loss(&self, id: &str) -> Result<()>`

- [ ] **Step 1: Write the failing tests** (in `src/jobs/merge.rs` tests):

```rust
#[tokio::test]
async fn an_already_flagged_merge_does_not_occupy_the_orphan_scan() {
    let core = crate::core::test_support::test_core().await;
    let ids = crate::jobs::consolidate::tests::seed(
        &core,
        &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])],
    )
    .await;
    write(&core, &draft("a text and b text"), &ids).await.unwrap();
    core.store.delete_artifact(&ids[0]).await.unwrap();
    assert_eq!(flag_orphans(&core).await.unwrap(), 1);
    // Flagged rows leave the scan entirely — not fetched and skipped in Rust,
    // which is what let 500 of them starve every newer orphan out of the LIMIT.
    assert!(
        core.store.merged_missing_a_source(500).await.unwrap().is_empty(),
        "a flagged merge still occupies a scan slot"
    );
}

#[tokio::test]
async fn reviewing_an_orphaned_merge_is_not_undone_by_the_next_sweep() {
    let core = crate::core::test_support::test_core().await;
    let ids = crate::jobs::consolidate::tests::seed(
        &core,
        &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])],
    )
    .await;
    let m = write(&core, &draft("a text and b text"), &ids).await.unwrap();
    core.store.delete_artifact(&ids[0]).await.unwrap();
    assert_eq!(flag_orphans(&core).await.unwrap(), 1);

    // What mark_artifact_reviewed does for an orphaned merge.
    core.store.accept_source_loss(&m.id).await.unwrap();
    core.store.clear_artifact_flags(&m.id).await.unwrap();

    assert_eq!(
        flag_orphans(&core).await.unwrap(),
        0,
        "the sweep re-flagged a merge the operator had reviewed"
    );
    assert!(core.store.get_artifact(&m.id).await.unwrap().flags.is_empty());
}
```

- [ ] **Step 2: Run — expect FAIL** (first test: `merged_missing_a_source` still returns the flagged row; second: `flag_orphans` returns 1 again). `cargo test orphan`

- [ ] **Step 3: Fix the SQL** in `src/store/lineage.rs` (`merged_missing_a_source`). Flags are stored as a JSON array in the `flags` TEXT column, so a LIKE exclusion is exact enough (no other flag contains the substring):

```rust
    /// Merged artifacts holding fewer lineage rows than the number of sources
    /// they were written from — and not yet flagged for it. The exclusion is
    /// in the SQL, not the caller: membership in this set is permanent
    /// (deletes are hard), so without it the oldest flagged rows fill the
    /// LIMIT forever and a newly orphaned merge past the five-hundredth is
    /// never seen. Newest first for the same reason.
    ///
    /// A comparison rather than a guess, which is what `source_count` is for:
    /// without it, "lost a source" cannot be told from "only ever had two".
    pub async fn merged_missing_a_source(&self, limit: i64) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT a.id FROM artifacts a
              WHERE a.provenance = 'merged'
                AND a.source_count >
                    (SELECT COUNT(*) FROM artifact_sources WHERE child_id = a.id)
                AND (a.flags IS NULL OR a.flags NOT LIKE '%orphaned_source%')
              ORDER BY a.created_at DESC, a.id
              LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get("id")).collect())
    }
```

- [ ] **Step 4: Slim `flag_orphans`** in `src/jobs/merge.rs` — the per-row fetch-and-skip is now dead weight:

```rust
pub async fn flag_orphans(core: &Core) -> Result<usize> {
    let mut n = 0;
    // The scan already excludes flagged rows, so every id here is new work.
    for id in core.store.merged_missing_a_source(500).await? {
        core.store
            .set_artifact_flags(
                &id,
                &["orphaned_source".to_string()],
                Some("one of the artifacts this was written from has been deleted"),
            )
            .await?;
        n += 1;
    }
    if n > 0 {
        tracing::info!(flagged = n, "merged artifacts have lost a source");
    }
    Ok(n)
}
```

- [ ] **Step 5: Add `accept_source_loss`** in `src/store/artifacts.rs`, next to `clear_artifact_flags`:

```rust
    /// Record that an operator reviewed a merge's lost sources and accepted it
    /// as a merge of what remains. `source_count` comes down to the surviving
    /// lineage count, so the orphan scan's comparison goes quiet — without
    /// this, clearing the flag lasts exactly one sweep, because the row still
    /// answers "lost a source" and is flagged all over again.
    pub async fn accept_source_loss(&self, id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE artifacts
                SET source_count = (SELECT COUNT(*) FROM artifact_sources WHERE child_id = ?)
              WHERE id = ? AND provenance = 'merged'",
        )
        .bind(id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
```

- [ ] **Step 6: Wire the handler** in `src/web/ui.rs` (`mark_artifact_reviewed`):

```rust
async fn mark_artifact_reviewed(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
) -> Result<Response> {
    // For an orphaned merge, "reviewed" means accepted as a merge of what
    // remains — recorded on source_count, or the next sweep re-flags it and
    // the operator's judgement lasts one tick.
    let c = st.core.store.get_artifact(&cid).await?;
    if c.flags.iter().any(|f| f == "orphaned_source") {
        st.core.store.accept_source_loss(&cid).await?;
    }
    st.core.store.clear_artifact_flags(&cid).await?;
    Ok(axum::response::Html(String::new()).into_response())
}
```

- [ ] **Step 7: Run** `cargo test` — including `deleting_a_source_flags_the_merge_rather_than_hiding_the_loss`, whose "does not re-flag" assertion now holds via the SQL exclusion. Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add -A && git commit -m "fix(merge): the orphan scan skips flagged rows in SQL, and a review sticks"
```

---

### Task 7: `merge_max_roots` below 2 goes back to the default

**Files:**
- Modify: `src/config.rs:546-566` (`normalize`)
- Test: `src/config.rs` tests module

A cap of 0 or 1 settles every judgeable component `Oversized` before any model call — merging silently off from a number nobody types meaning that. The codebase's own pattern for this is `normalize()`'s put-back (see `feedback.candidates`).

- [ ] **Step 1: Write the failing test** (in `src/config.rs` tests, following how the existing `feedback.candidates` normalize tests build a `Config`):

```rust
#[test]
fn a_merge_cap_below_two_goes_back_to_the_default() {
    // The same put-back as feedback.candidates: every judgeable component
    // flattens to at least two roots, so a cap of 0 or 1 settles all of them
    // Oversized before any call — merging silently off.
    let dir = tempfile::tempdir().unwrap();
    let p = write(
        &dir,
        &format!("{MINIMAL}\n[consolidate]\nmerge_max_roots = 1\n"),
    );
    let cfg = Config::load(Some(&p)).unwrap();
    assert_eq!(
        cfg.consolidate.merge_max_roots,
        ConsolidateConfig::default().merge_max_roots
    );
}
```

(`write` and `MINIMAL` are the config test module's existing fixtures; check whether `MINIMAL` already contains a `[consolidate]` section — if it does, append the key inside that section instead of opening a second one, since TOML rejects duplicate tables.)

- [ ] **Step 2: Run — expect FAIL.** `cargo test a_merge_cap_below_two`

- [ ] **Step 3: Implement** in `normalize()`, after the `feedback.candidates` blocks:

```rust
        // Same argument as above: every judgeable component flattens to at
        // least two roots, so a cap of zero or one settles all of them
        // Oversized before any call — merging silently off from a number
        // nobody types meaning that.
        if self.consolidate.merge_max_roots < 2 {
            let d = ConsolidateConfig::default().merge_max_roots;
            tracing::warn!(
                configured = self.consolidate.merge_max_roots,
                using = d,
                "consolidate.merge_max_roots below 2 would settle every component \
                 as oversized; using the default"
            );
            self.consolidate.merge_max_roots = d;
        }
```

- [ ] **Step 4: Run** `cargo test`. Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "fix(config): a merge cap below two goes back to the default instead of disabling merges"
```

---

### Task 8: The full lifecycle reconcile scan runs under `lifecycle_lock` and marks what it repairs

**Files:**
- Modify: `src/jobs/consolidate.rs:223-286` (`full_lifecycle_reconcile_scanning`)
- Test: `src/jobs/consolidate.rs` tests module

The race: the scan reads the SQLite row snapshot and the payload at different times with no lock; a concurrent `unsupersede` completing in between makes the scan write the *stale hidden state* back over the fresh payload, and because the scan's write marks nothing dirty, the corrupted state (row Active, payload Superseded, marker clear) is invisible to the complete marker pass.

- [ ] **Step 1: Write the test** (deterministic protocol test — the race itself needs a scheduler; assert the protocol instead):

```rust
#[tokio::test]
async fn the_scan_repair_leaves_no_repaired_id_unmarked_midway() {
    // The scan writes payloads from row state. An interrupted write must be
    // findable by the complete marker pass, so the ids are marked dirty
    // before the payload write and cleared only after it returns — the same
    // contract every lifecycle mutator honours.
    let core = test_core().await;
    let ids = seed(&core, &[("a text", [1.0, 0.0])]).await;
    // Drift no SQLite write produced: the payload hides what the row shows.
    core.vectors
        .set_lifecycle(&ids[0], crate::store::artifacts::ArtifactStatus::Deprecated, None)
        .await
        .unwrap();

    let repaired = full_lifecycle_reconcile(&core).await.unwrap();

    assert_eq!(repaired, 1);
    // Repair complete: payload agrees with the row again and no marker is left.
    assert!(
        core.store.dirty_lifecycle_artifacts(10).await.unwrap().is_empty(),
        "the scan left markers standing on a base in agreement"
    );
}
```

(Adapt the drift seeding and assertions to the existing reconcile tests in that module — several already seed payload-side drift; follow their pattern. If `dirty_lifecycle_artifacts` has a different name/signature, use whatever `repair_lifecycle_drift` reads.)

- [ ] **Step 2: Run** — this passes or fails depending on current marker behavior; its job is to pin the contract. Note the result.

- [ ] **Step 3: Implement.** At the top of `full_lifecycle_reconcile_scanning`, before `list_non_active_artifacts`:

```rust
    // Under the same lock as every lifecycle transition. The scan reads the
    // row side and the payload side at different moments and then writes
    // payloads from the row snapshot; interleaved with a payload-first reveal
    // it would write the stale hidden state back over the fresh payload —
    // and, writing with no marker, leave nothing behind for the complete
    // marker pass to find it by. The scan is capped at `DRIFT_SCAN`, so the
    // hold is bounded.
    let _guard = core.lifecycle_lock.lock().await;
```

And wrap the write at the bottom in the marker protocol:

```rust
    if !rows.is_empty() {
        tracing::info!(
            repaired = rows.len(),
            "lifecycle state disagreed between sqlite and the vector store"
        );
        // Marked before the payload write, cleared only after it returns: an
        // interrupted repair is then ordinary marked drift the next marker
        // pass finishes, instead of a best-effort scan's private problem.
        let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
        for id in &ids {
            core.store.mark_lifecycle_dirty(id).await?;
        }
        core.vectors.apply_lifecycle(&rows).await?;
        core.store.clear_lifecycle_dirty(&ids).await?;
    }
    Ok(rows.len())
```

(Check the actual field name for the id on `LifecycleRow` — `lifecycle_row_of` builds it; adjust `r.id` accordingly. If `mark_lifecycle_dirty` only takes `&str` one at a time, the loop above is the call shape.)

- [ ] **Step 4: Verify no caller holds the lock** (tokio Mutex deadlocks silently):

Run: `grep -n "full_lifecycle_reconcile\|lifecycle_lock" src/jobs/consolidate.rs src/core/*.rs`
Expected: `run()` calls `repair_lifecycle_drift` (locks and releases) and then `full_lifecycle_reconcile` as a *separate* call; no call path enters the scan with the lock held. If any does, restructure as `_locked` variants the way `unsupersede`/`unsupersede_locked` do.

- [ ] **Step 5: Run** `cargo test` (the reconcile suite has several scan tests — all must pass). Expected: PASS.
- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "fix(consolidate): the reconcile scan takes the lifecycle lock and marks its writes"
```

---

### Task 9: `heal_dangling_supersessions` honours the lifecycle lock and marker

**Files:**
- Modify: `src/core/ingest.rs:756-793`
- Test: `src/core/ingest.rs` tests module (or wherever the heal's existing tests live — find with `grep -rn "heal_dangling" src --include=*.rs`)

Same failure class as Task 8: heal writes payload → row → clear-marker with no lock and no pre-mark; interleaved with `repair_lifecycle_drift` it produces row Active / payload Superseded / marker clear.

- [ ] **Step 1: Write the test** (protocol pin, same shape as Task 8's):

```rust
#[tokio::test]
async fn the_heal_reveals_under_the_lifecycle_lock_with_the_marker_raised_first() {
    // Payload-first direction, so the contract is unsupersede's: mark before
    // the payload write, clear only once both stores agree. Without the mark,
    // a crash between the two writes is drift no marker ever announced; and
    // without the lock, the sweep's repair can interleave and write the stale
    // hidden state back with nothing left to notice.
    let core = test_core().await;
    let ids = seed(&core, &[("a", [1.0, 0.0]), ("b", [0.93, 0.37])]).await;
    core.supersede(&ids[0], &ids[1]).await.unwrap();
    core.delete_artifact(&ids[1]).await.unwrap(); // runs the heal

    let a = core.store.get_artifact(&ids[0]).await.unwrap();
    assert!(a.in_results(), "the heal did not restore the dangling loser");
    assert!(
        core.store.dirty_lifecycle_artifacts(10).await.unwrap().is_empty(),
        "the heal left a marker on a base in agreement"
    );
}
```

(This may already pass end-state-wise; it pins the marker end state while Step 3 fixes the ordering and locking. Keep it regardless.)

- [ ] **Step 2: Run it**, note the result.

- [ ] **Step 3: Implement.** In `heal_dangling_supersessions`:

```rust
    pub(crate) async fn heal_dangling_supersessions(&self) -> Result<()> {
        // Under the lifecycle lock like every other transition: this path
        // reveals payload-first, and interleaving with the sweep's repair —
        // which reads rows and writes payloads — is exactly the sequence that
        // hides an artifact with no marker left to find it by.
        let _guard = self.lifecycle_lock.lock().await;
        let mut first_err = None;
        for id in self.store.dangling_superseded().await? {
            // Marked before the payload write, as `unsupersede` does and for
            // the same reason: this direction writes the payload first, so
            // without it a crash between the two stores would leave drift no
            // row write ever announced.
            self.store.mark_lifecycle_dirty(&id).await?;
            if let Err(e) = self
                .vectors
                .set_lifecycle(&id, ArtifactStatus::Active, None)
                .await
            {
                tracing::warn!(
                    artifact_id = %id,
                    error = %e,
                    "could not clear the hidden flag; the artifact stays listed on Ops"
                );
                first_err.get_or_insert(e);
                continue;
            }
            self.store.set_superseded_by(&id, None).await?;
            // ... keep the existing clear_lifecycle_dirty call and its comment ...
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
```

One subtlety the implementer must preserve: on the `continue` (payload write failed), the marker now stays raised — that is correct and is the point: the failed reveal is findable drift. But `repair_lifecycle_drift` repairs *row → payload*, and here the row still says superseded, so the marker pass would just rewrite "superseded" into the payload — harmless (that is the current true state) and the heal retries next sweep anyway.

- [ ] **Step 4: Verify no caller holds the lock:**

Run: `grep -rn "heal_dangling_supersessions" src --include=*.rs`
Expected callers: `delete_artifact`, `delete_corpus`, `reprocess` (ingest.rs), `embed.rs`, `consolidate::run` — read each call site and confirm none holds `lifecycle_lock` at the call. `delete_artifact`/`delete_corpus`/`reprocess` do not lock; check `embed.rs:582`'s enclosing function. If any holds it, add a `_locked` variant per the `unsupersede_locked` pattern.

- [ ] **Step 5: Run** `cargo test`. Expected: PASS.
- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "fix(core): the dangling-supersession heal takes the lifecycle lock and marks first"
```

---

### Task 10: One `source_rows` helper; Ops batches `roots_of`

**Files:**
- Modify: `src/web/ui.rs:1043-1075` (`ops()`), `src/web/ui.rs:1412-1435` (`build_artifact_detail`)
- Test: existing UI/store tests only (this is an extraction; `cargo test` guards it)

The loop `roots_of(one id)` → skip self → `get_artifact(rid)` → push `SourceRow` is copy-pasted verbatim in both functions, and `ops()` runs it once per merged artifact — an N+1 over an API whose signature `&[String]` exists for batching.

- [ ] **Step 1: Extract the helper** in `src/web/ui.rs`, near `SourceRow`:

```rust
/// The source list a merge renders: its lineage roots, fetched and titled.
/// One shape for Ops and the detail pane — the two must stay behaviorally
/// identical (same self-guard, same tolerance for deleted sources, same
/// corpus fallback), and a copy in each is how they come to disagree about
/// what a merge was made of.
async fn source_rows(store: &crate::store::Store, merged_id: &str, roots: &[String]) -> Vec<SourceRow> {
    let mut sources = Vec::new();
    for rid in roots {
        // `roots_of` answers an empty list for a merge that lost every source;
        // the self guard stays as defense against a base written before that
        // change.
        if rid == merged_id {
            continue;
        }
        if let Ok(r) = store.get_artifact(rid).await {
            sources.push(SourceRow {
                corpus_id: r.corpus_id.clone().unwrap_or_default(),
                title: title_of(&r),
                id: r.id,
            });
        }
    }
    sources
}
```

- [ ] **Step 2: Rewrite `ops()`'s merged loop** — one batched `roots_of` for all listed merges:

```rust
    let mut merged = Vec::new();
    let merged_chunks = st.core.store.merged_artifacts(50).await?;
    // One lineage query per page, not one per row: `roots_of` takes the batch.
    let merged_ids: Vec<String> = merged_chunks.iter().map(|c| c.id.clone()).collect();
    let roots = st.core.store.roots_of(&merged_ids).await.unwrap_or_default();
    for c in merged_chunks {
        let sources = source_rows(
            &st.core.store,
            &c.id,
            roots.get(&c.id).map(Vec::as_slice).unwrap_or_default(),
        )
        .await;
        merged.push(MergedRow {
            orphaned: c.flags.iter().any(|f| f == "orphaned_source"),
            title: title_of(&c),
            id: c.id,
            sources,
        });
    }
```

(If `st.core.store` is not a plain `crate::store::Store`, match the helper's parameter type to whatever both call sites can hand over — the detail pane has `core.store`, ops has `st.core.store`.)

- [ ] **Step 3: Rewrite `build_artifact_detail`'s block** to use the same helper:

```rust
    let mut sources = Vec::new();
    if c.provenance == crate::store::artifacts::Provenance::Merged {
        let roots = core
            .store
            .roots_of(std::slice::from_ref(&c.id))
            .await
            .unwrap_or_default();
        sources = source_rows(
            &core.store,
            &c.id,
            roots.get(&c.id).map(Vec::as_slice).unwrap_or_default(),
        )
        .await;
    }
```

- [ ] **Step 4: Run** `cargo test`. Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "refactor(ui): one source_rows helper, one batched roots_of per Ops page"
```

---

### Task 11: Remove `open_component`'s unreachable empty-component arm

**Files:**
- Modify: `src/store/pairs.rs:399-401`

The block above guarantees the seed is in `open` (pushed if Pending, early-returned if settled), so the `else` arm can never run — yet it returns the same empty vec that has a *documented meaning* ("settled while the unit waited"), forcing readers to reason about a phantom third case.

- [ ] **Step 1: Replace the let-else** at pairs.rs:399:

```rust
        let seed = open
            .iter()
            .find(|p| p.id == pair_id)
            .expect("the block above put the seed in the window or returned");
```

- [ ] **Step 2: Run** `cargo test` (pairs suite). Expected: PASS.
- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "refactor(pairs): open_component has two empty-component cases, not a phantom third"
```

---

### Task 12: `arm_dedupe` skips in-flight pairs before fetching their artifacts

**Files:**
- Modify: `src/jobs/consolidate.rs:636-688` (inside `arm_dedupe`'s loop)

Each tick reads up to 200 pending pairs and pays two full-row `get_artifact` fetches per pair *before* the cheap `live_job` skip — and in-flight pairs sort first (`judge_attempts = 0`), so the same rows are re-fetched every 15 minutes for nothing.

- [ ] **Step 1: Reorder.** Move the `live_job` check (the block at lines 680-688 beginning `// A pair whose unit is still queued from an earlier sweep...` through `if core.store.live_job(Stage::Dedupe, &target).await? { continue; }`) to directly *above* the `let (a, b) = match (...)` artifact fetch at line 636. Keep both comment blocks with their code. The retired-member dismissal stays where it is — it needs the artifacts.

- [ ] **Step 2: Check nothing between the two blocks depended on order.** The moved check reads only `p.id`; the fetch reads only `p.a_id`/`p.b_id`. Nothing in between mutates state. Confirm by reading the final loop top-to-bottom: budget check → live_job skip → artifact fetch → retired dismissal → re-arm.

- [ ] **Step 3: Run** `cargo test` (the arm_dedupe tests). Expected: PASS.
- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "perf(consolidate): arm_dedupe skips queued pairs before fetching their artifacts"
```

---

## Final verification

- [ ] `cargo test` — full suite green.
- [ ] `cargo fmt --check` and `cargo clippy` (fix anything the tasks introduced).
- [ ] Re-read the Findings list at the top: every numbered finding maps to a landed commit; the three declined ones and the deferred refactor are recorded there.
