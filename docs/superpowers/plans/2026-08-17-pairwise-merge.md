# Pairwise Merging Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the dedupe unit judge exactly two artifacts at a time, so that clusters converge by repeated pairwise merging instead of being refused whole when they flatten to more roots than a configured cap.

**Architecture:** `dedupe::run` stops expanding its seed pair into a connected component and asks about the pair's two members only. A member that is itself a merge contributes its own text as a lettered input and its captured roots as an unlettered, budget-trimmable context block. The fan-in cap disappears with the component, `PairState::Oversized` stops being written, the sixteen rows already in that state are reopened by the sweep, and pending siblings are re-pointed onto a merge once it is indexed so a cluster still converges without waiting on a re-scan.

**Tech Stack:** Rust, `sqlx` against SQLite, `tokio::test` for async unit tests, in-module `#[cfg(test)] mod tests`. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-17-pairwise-merge-design.md`

## Global Constraints

- No new dependencies. Everything needed is already in `Cargo.toml`.
- `PairState::Oversized` and its `"oversized"` string mapping stay in `src/store/pairs.rs` for one release so existing rows still parse. Nothing may *write* the state after this work.
- Pair rows are stored with `a_id <= b_id` — `record_pair` canonicalises with `let (a, b) = if a <= b { (a, b) } else { (b, a) };` (`src/store/pairs.rs:144`). Any code that writes `a_id`/`b_id` must preserve that ordering.
- `artifact_pairs` has a uniqueness constraint on `(a_id, b_id)` — that is what makes `INSERT OR IGNORE` meaningful. Any `UPDATE` that changes a side must check for an existing row first.
- Comments in this codebase carry the reasoning, not the restatement. When a comment's premise is deleted by this work, rewrite the comment; do not leave it describing code that no longer exists.
- Run `cargo test` (whole suite) before every commit. Run `cargo clippy --all-targets -- -D warnings` before the final commit of each task.

---

### Task 1: `repoint_open_pairs` in the pair store

Moves pending pairs from a merge's sources onto the merge. Pure store work with no callers yet, so it is testable on its own.

**Files:**
- Modify: `src/store/pairs.rs` (add method after `dismiss_pairs_merged_into`, around line 430; tests into the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `PairState`, `row_to_pair` (`src/store/pairs.rs:121`), `set_pair_state`, `pair_state_between` — all already in this file.
- Produces: `Store::repoint_open_pairs(&self, old: &[String], new_id: &str) -> Result<u64>`, returning how many rows were moved (rows dismissed instead are not counted). Task 6 calls it.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/store/pairs.rs`. The existing tests there build a store with `crate::store::test_support::test_store()` — check the top of that module and follow whatever helper the neighbouring tests use to make artifacts; the snippets below use `s` for the store and `a`/`b`/`c` for artifact ids the same way `a_component_is_every_pair_reachable_through_a_shared_artifact` does.

```rust
/// The whole point of re-pointing: C was a duplicate of B, B is now inside M,
/// so C is a duplicate of M and the question survives the merge.
#[tokio::test]
async fn an_open_pair_follows_its_member_into_the_merge() {
    let s = test_store().await;
    let (a, b, c, m) = (mk(&s).await, mk(&s).await, mk(&s).await, mk(&s).await);
    s.record_pair(&b, &c, 0.91).await.unwrap();
    let _ = a;

    let moved = s.repoint_open_pairs(&[b.clone()], &m).await.unwrap();

    assert_eq!(moved, 1);
    let state = s.pair_state_between(&c, &m).await.unwrap();
    assert_eq!(state, Some(PairState::Pending), "the pair did not follow B into M");
    assert_eq!(s.pair_state_between(&b, &c).await.unwrap(), None, "the old pair is still there");
}

/// A pair between the merge's two own sources becomes a pair of the merge with
/// itself. There is no question left in it.
#[tokio::test]
async fn a_pair_between_two_sources_of_the_same_merge_is_dismissed() {
    let s = test_store().await;
    let (a, b, m) = (mk(&s).await, mk(&s).await, mk(&s).await);
    s.record_pair(&a, &b, 0.91).await.unwrap();

    let moved = s.repoint_open_pairs(&[a.clone(), b.clone()], &m).await.unwrap();

    assert_eq!(moved, 0, "a self-pair was written");
    assert_eq!(s.pair_state_between(&a, &b).await.unwrap(), Some(PairState::Dismissed));
}

/// An operator's decision outlives the merge. Re-pointing onto a pair someone
/// already dismissed must not put that question back, which is the same
/// property `record_pair`'s INSERT OR IGNORE provides.
#[tokio::test]
async fn re_pointing_onto_an_existing_pair_leaves_the_existing_row_alone() {
    let s = test_store().await;
    let (b, c, m) = (mk(&s).await, mk(&s).await, mk(&s).await);
    s.record_pair(&c, &m, 0.80).await.unwrap();
    let existing = s.pair_state_between(&c, &m).await.unwrap();
    assert_eq!(existing, Some(PairState::Pending));
    let row = s.pairs_by_state(PairState::Pending, 10).await.unwrap();
    let existing_id = row[0].id;
    s.set_pair_state(existing_id, PairState::Dismissed, Some("operator")).await.unwrap();
    s.record_pair(&b, &c, 0.91).await.unwrap();

    let moved = s.repoint_open_pairs(&[b.clone()], &m).await.unwrap();

    assert_eq!(moved, 0);
    assert_eq!(
        s.pair_state_between(&c, &m).await.unwrap(),
        Some(PairState::Dismissed),
        "an operator's dismissal was overwritten"
    );
    assert_eq!(s.pair_state_between(&b, &c).await.unwrap(), Some(PairState::Dismissed));
}

/// The re-pointed row asks a different question than the one that earned the
/// counters, so it must not inherit a backoff from artifacts it no longer names.
#[tokio::test]
async fn a_re_pointed_pair_starts_its_attempts_over() {
    let s = test_store().await;
    let (b, c, m) = (mk(&s).await, mk(&s).await, mk(&s).await);
    s.record_pair(&b, &c, 0.91).await.unwrap();
    let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
    s.record_judge_attempt(id).await.unwrap();
    s.record_unreadable_judgement(id).await.unwrap();

    s.repoint_open_pairs(&[b.clone()], &m).await.unwrap();

    let moved = s.pairs_by_state(PairState::Pending, 10).await.unwrap();
    let moved = moved.iter().find(|p| p.a_id == m || p.b_id == m).expect("the row moved");
    assert_eq!(moved.judge_attempts, 0);
    assert_eq!(moved.judge_unreadable, 0);
}

/// Only pending rows move. A settled pair is an answered question and moving it
/// would re-file it against a different artifact under someone else's verdict.
#[tokio::test]
async fn a_settled_pair_is_not_re_pointed() {
    let s = test_store().await;
    let (b, c, m) = (mk(&s).await, mk(&s).await, mk(&s).await);
    s.record_pair(&b, &c, 0.91).await.unwrap();
    let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
    s.set_pair_state(id, PairState::NoConflict, None).await.unwrap();

    let moved = s.repoint_open_pairs(&[b.clone()], &m).await.unwrap();

    assert_eq!(moved, 0);
    assert_eq!(s.pair_state_between(&b, &c).await.unwrap(), Some(PairState::NoConflict));
}
```

If `mk` and `test_store` are not the helper names this module already uses, use whatever the neighbouring tests use rather than adding new helpers.

- [ ] **Step 2: Run the tests and watch them fail**

Run: `cargo test --lib store::pairs::tests::an_open_pair_follows_its_member_into_the_merge`
Expected: compile error, `no method named 'repoint_open_pairs'`.

- [ ] **Step 3: Implement the method**

```rust
    /// Move every still-open pair that names one of `old` onto `new_id`.
    ///
    /// Called when a merge is finished, so that a duplicate of one of its
    /// sources becomes a duplicate of the merge rather than dying with the
    /// source. Without this a cluster only converges by waiting for the merge
    /// to embed and a later similarity sweep to re-file the same question,
    /// which is a whole tick per generation.
    ///
    /// Three rows never move. One whose other side is already `new_id` would
    /// become a pair of the merge with itself. One that would collide with an
    /// existing pair between the same two artifacts must leave that row alone,
    /// whatever state it is in — that is what keeps an operator's dismissal
    /// binding, the same property `record_pair`'s `INSERT OR IGNORE` provides.
    /// Both are dismissed instead, because the question they carried has been
    /// answered by the merge. And a row that is not `Pending` is an answered
    /// question already; moving it would re-file someone's verdict against an
    /// artifact it was never about.
    ///
    /// `judge_attempts` and `judge_unreadable` reset, because the moved row
    /// asks about a different pair of artifacts than the one that earned those
    /// counts. This cannot loop forever: every merge takes one artifact out of
    /// results, so the sequence of merges a cluster can produce is finite.
    ///
    /// `score` is deliberately left alone and is now stale — it was measured
    /// between the old member and the other side. It orders the judge queue and
    /// gates nothing at this point, so the staleness costs ordering accuracy
    /// and nothing else.
    pub async fn repoint_open_pairs(&self, old: &[String], new_id: &str) -> Result<u64> {
        let mut moved = 0u64;
        for o in old {
            if o == new_id {
                continue;
            }
            let rows = sqlx::query(
                "SELECT * FROM artifact_pairs
                  WHERE state = 'pending' AND (a_id = ? OR b_id = ?)",
            )
            .bind(o)
            .bind(o)
            .fetch_all(&self.pool)
            .await?;
            for p in rows.iter().map(row_to_pair) {
                let other = if p.a_id == *o { p.b_id.clone() } else { p.a_id.clone() };
                if other == new_id {
                    self.set_pair_state(
                        p.id,
                        PairState::Dismissed,
                        Some("its other side is now this merge"),
                    )
                    .await?;
                    continue;
                }
                if self.pair_state_between(&other, new_id).await?.is_some() {
                    self.set_pair_state(
                        p.id,
                        PairState::Dismissed,
                        Some("a pair between these two already exists"),
                    )
                    .await?;
                    continue;
                }
                let (a, b) = if other.as_str() <= new_id {
                    (other.as_str(), new_id)
                } else {
                    (new_id, other.as_str())
                };
                sqlx::query(
                    "UPDATE artifact_pairs
                        SET a_id = ?, b_id = ?, judge_attempts = 0, judge_unreadable = 0,
                            detail = NULL
                      WHERE id = ?",
                )
                .bind(a)
                .bind(b)
                .bind(p.id)
                .execute(&self.pool)
                .await?;
                moved += 1;
            }
        }
        Ok(moved)
    }
```

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test --lib store::pairs`
Expected: PASS, including the five new tests.

- [ ] **Step 5: Commit**

```bash
cargo test && cargo clippy --all-targets -- -D warnings
git add src/store/pairs.rs
git commit -m "feat(store): move an open pair onto the merge that swallowed its member"
```

---

### Task 2: Reopening what the cap already refused

**Files:**
- Modify: `src/store/pairs.rs` (method beside `repoint_open_pairs`; test into the same `mod tests`)

**Interfaces:**
- Produces: `Store::reopen_oversized(&self) -> Result<u64>`, returning how many rows were reopened. Task 7 calls it from the sweep.

- [ ] **Step 1: Write the failing test**

```rust
/// Sixteen of these exist in the field, every one with judge_attempts = 0: the
/// cap refused them before any call, and `pairs_to_judge` only ever looks at
/// pending rows, so nothing could reach them again.
#[tokio::test]
async fn an_oversized_pair_goes_back_into_the_queue() {
    let s = test_store().await;
    let (a, b) = (mk(&s).await, mk(&s).await);
    s.record_pair(&a, &b, 0.91).await.unwrap();
    let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
    s.set_pair_state(id, PairState::Oversized, Some("12 sources, cap is 8")).await.unwrap();

    assert_eq!(s.reopen_oversized().await.unwrap(), 1);

    let back = s.pairs_by_state(PairState::Pending, 10).await.unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].detail, None, "the cap's line is still on a pending row");
    assert_eq!(back[0].judge_attempts, 0);
    // Runs every sweep; once the queue is drained it must do nothing at all.
    assert_eq!(s.reopen_oversized().await.unwrap(), 0);
}
```

- [ ] **Step 2: Run the test and watch it fail**

Run: `cargo test --lib store::pairs::tests::an_oversized_pair_goes_back_into_the_queue`
Expected: compile error, `no method named 'reopen_oversized'`.

- [ ] **Step 3: Implement**

```rust
    /// Put every pair the fan-in cap refused back into the judge queue.
    ///
    /// `Oversized` was terminal and reached without a call ever being made: the
    /// component flattened to more roots than the cap and every pair in it was
    /// settled before the model saw anything. Pairwise merging removes the
    /// condition, so the rows left behind are simply unanswered questions.
    ///
    /// Run every sweep rather than once behind a guard. Nothing writes the
    /// state any more, so the first pass drains it and every later one matches
    /// no rows — which is cheaper than the machinery a one-shot would need.
    ///
    /// Safe to run with the queue in any state: the refused rows never spent a
    /// call, so `judge_attempts` is zero and no backoff is being reset.
    pub async fn reopen_oversized(&self) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE artifact_pairs SET state = 'pending', detail = NULL
              WHERE state = 'oversized'",
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }
```

- [ ] **Step 4: Run the test and watch it pass**

Run: `cargo test --lib store::pairs`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo test && cargo clippy --all-targets -- -D warnings
git add src/store/pairs.rs
git commit -m "feat(store): put the pairs the cap refused back in the queue"
```

---

### Task 3: The two-member prompt

**Files:**
- Modify: `src/infer/prompt.rs:239` (`dedupe_prompt`), `src/infer/prompt.rs:170` (`DEDUPE_SYSTEM`), tests in the same file's `mod tests`

**Interfaces:**
- Produces:
  ```rust
  pub struct DedupeMember<'a> {
      pub title: &'a str,
      pub text: &'a str,
      pub sources: Vec<(&'a str, &'a str)>,
  }
  pub fn dedupe_prompt(a: &DedupeMember<'_>, b: &DedupeMember<'_>, attempt: i64) -> String
  ```
  Task 4 and Task 5 call it. `sources` is `(title, text)` pairs, oldest first, and is empty for a member that was captured rather than merged.
- Consumes: nothing new.

- [ ] **Step 1: Write the failing tests**

```rust
/// A merged member's own wording is what is being judged, and its captured
/// roots are there so the model can put back a detail the earlier merge
/// dropped. Both appear; only the member is lettered.
#[test]
fn a_merged_member_is_shown_with_its_sources_beneath_it() {
    let a = DedupeMember {
        title: "Pool sizing",
        text: "the pool holds sixteen",
        sources: vec![("Pool sizing, 2024", "max_connections is 16"), ("Pool notes", "raise it for batch jobs")],
    };
    let b = DedupeMember { title: "Connections", text: "sixteen connections", sources: vec![] };

    let p = dedupe_prompt(&a, &b, 0);

    assert!(p.contains("ARTIFACT A"));
    assert!(p.contains("ARTIFACT B"));
    assert!(p.contains("the pool holds sixteen"));
    assert!(p.contains("max_connections is 16"), "a source was not shown");
    assert!(p.contains("SOURCES OF A"));
    assert!(!p.contains("SOURCES OF B"), "a captured member was given a sources block");
    assert!(!p.contains("ARTIFACT C"), "a source was lettered");
}

/// The endpoint caches by exact prompt text, so a retry of a reply the parser
/// could not read has to differ from the reply it is retrying.
#[test]
fn only_a_retry_carries_an_attempt_line() {
    let m = DedupeMember { title: "t", text: "x", sources: vec![] };
    assert!(!dedupe_prompt(&m, &m, 0).contains("attempt"));
    assert!(dedupe_prompt(&m, &m, 1).contains("(attempt 2)"));
}

/// The letters the verdict may name are exactly the two artifacts under
/// judgement, and the system prompt has to say so or a merged member's sources
/// are fair game for `supersedes`.
#[test]
fn the_system_prompt_rules_the_sources_out_of_the_verdict() {
    assert!(DEDUPE_SYSTEM.contains("SOURCES"));
    assert!(DEDUPE_SYSTEM.contains("never name a source"));
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --lib infer::prompt::tests::a_merged_member_is_shown_with_its_sources_beneath_it`
Expected: compile error, `cannot find struct 'DedupeMember'`.

- [ ] **Step 3: Implement**

Replace `dedupe_prompt` (`src/infer/prompt.rs:239`) and its doc comment with:

```rust
/// One of the two artifacts under judgement, and — when it is itself a merge —
/// the captured originals behind it.
///
/// `sources` is context and never an input. It exists so that a detail an
/// earlier merge dropped can be put back into this one, which is what keeps
/// repeated pairwise merging from walking away from the wording someone
/// actually captured. It is unlettered so that no verdict can name it: the
/// letters `a` and `b` are the two members and nothing else, and a letter that
/// could resolve to a source would supersede an artifact on the strength of a
/// text the model was shown as reference.
pub struct DedupeMember<'a> {
    pub title: &'a str,
    pub text: &'a str,
    /// `(title, text)`, oldest first. Empty for a captured artifact.
    pub sources: Vec<(&'a str, &'a str)>,
}

/// The two artifacts, each under its letter and its title, each followed by its
/// sources when it has any.
///
/// The title is not decoration here, it is the subject. Synthesis writes a body
/// that stands on its own within its segment, which is not the same as naming
/// what it is about: a section headed "FAT32" becomes an artifact whose text
/// opens "32 Bit Clusternummern" and never says FAT32 again. Handed the bodies
/// alone, the model saw two anonymous spec lists with different numbers and
/// called them a contradiction — correctly, on the evidence it was given.
///
/// `attempt` is in the prompt because the endpoint caches by exact prompt text:
/// without it, a retry of a reply the parser could not read would replay the
/// same unreadable bytes for every attempt. Zero adds nothing, so a first ask
/// stays byte-identical between runs.
pub fn dedupe_prompt(a: &DedupeMember<'_>, b: &DedupeMember<'_>, attempt: i64) -> String {
    let mut s = String::new();
    if attempt > 0 {
        s.push_str(&format!("(attempt {})\n", attempt + 1));
    }
    for (letter, m) in [('A', a), ('B', b)] {
        s.push_str(&format!(
            "----- ARTIFACT {letter} -----\nTitle: {}\n\n{}\n",
            m.title, m.text
        ));
        if !m.sources.is_empty() {
            s.push_str(&format!("----- SOURCES OF {letter} -----\n"));
            for (title, text) in &m.sources {
                s.push_str(&format!("Title: {title}\n\n{text}\n\n"));
            }
        }
    }
    s.push_str("----- END -----");
    s
}
```

Then add one paragraph to `DEDUPE_SYSTEM` (`src/infer/prompt.rs:170`), immediately before the "Reply with JSON only" line:

```
An artifact that was itself written by merging earlier ones is shown with those originals under "SOURCES OF A" or "SOURCES OF B". They are there for one reason: so that a detail an earlier merge dropped can go back into your answer. They are not under judgement. There are exactly two artifacts, A and B — never name a source in `supersedes`, and never treat a source as a third artifact.
```

- [ ] **Step 4: Run and watch them pass**

Run: `cargo test --lib infer::prompt`
Expected: PASS. Any other test in the file that calls `dedupe_prompt` with the old slice signature must be updated to the new one as part of this step.

- [ ] **Step 5: Commit**

```bash
cargo test --lib infer::prompt && cargo clippy --all-targets -- -D warnings
git add src/infer/prompt.rs
git commit -m "feat(infer): show a merged artifact's own words, with its sources beside them"
```

Note: `cargo test` as a whole will not pass yet — `src/jobs/dedupe.rs` still calls the old signature. That is Task 4, and the two are committed back to back.

---

### Task 4: The unit is one pair

The large one. `run`, `interpret`, `apply` and `Settlement` all lose the component, letters start indexing members, and the loss check moves onto the two member texts.

**Files:**
- Modify: `src/jobs/dedupe.rs` — module header (lines 1-25), `Settlement` (33-48), `run` (50-236), `interpret` (244-310), `apply` (311-425), `settle_all` (443-456), and the test module
- Test: `src/jobs/dedupe.rs` `mod tests`

**Interfaces:**
- Consumes: `DedupeMember` and the new `dedupe_prompt` from Task 3. `crate::jobs::merge::losses(&[Chunk], &MergedDraft) -> Vec<String>` (`src/jobs/merge.rs:273`) — unchanged signature, different argument.
- Produces: `Settlement { relation, detail, obsolete, merged, members: Vec<Chunk>, pair: ArtifactPair }` — the `roots` and `pairs` fields are gone. Task 5 edits `run` again.

- [ ] **Step 1: Write the failing tests**

These replace or join the existing tests. Delete outright, in the same step:

- `a_retired_members_roots_do_not_count_against_the_cap` (line 503) — there is no cap.
- `a_component_past_the_fan_in_cap_is_surfaced_and_never_called_about` (line 912) — replaced below.
- `one_call_settles_every_pair_in_the_component` (line 873) — replaced below.
- `a_retired_member_dismisses_only_its_own_pairs` (line 1061) — a unit owns one pair, so it has no siblings to mis-settle.
- `a_pair_of_two_survivors_is_not_closed_with_someone_elses_direction` (line 677) — the `survivors` branch it pins is deleted.

Rename `a_failed_dedupe_leaves_the_component_pending` (line 972) to `a_failed_dedupe_leaves_the_pair_pending` and keep its body, adjusting any component-shaped assertion to the single pair.

- `a_replacement_naming_a_root_already_out_of_results_is_applied_to_the_carrier` (line 1140) — its scenario is a letter resolving to a root whose carrier survives, which cannot occur once letters index the two members. The "already out of results" branch it also touched is kept in `apply` and covered by `a_component_whose_member_was_retired_is_dismissed_without_a_call` (line 993), which stays.

Rewrite `a_letter_names_the_root_it_was_shown_beside_not_the_nth_member` (line 727) as `a_letter_names_a_member_and_never_one_of_its_sources` below. Keep everything else.

```rust
/// The unit answers its own pair and nothing else. A sibling pair is a
/// separate question and keeps its own turn in the queue.
#[tokio::test]
async fn a_unit_settles_only_its_own_pair() {
    let mut core = test_core().await;
    core.judge = Arc::new(ScriptedCompleter::new(vec![
        r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
    ]));
    let ids = seed(
        &core,
        &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37]), ("c text", [0.90, 0.44])],
    )
    .await;
    let seed_pair = queue_pair(&core, &ids[0], &ids[1]).await;
    queue_pair(&core, &ids[1], &ids[2]).await;

    run(&core, &seed_pair.to_string()).await.unwrap();

    assert_eq!(core.store.pairs_by_state(PairState::NoConflict, 10).await.unwrap().len(), 1);
    assert_eq!(
        core.store.pairs_by_state(PairState::Pending, 10).await.unwrap().len(),
        1,
        "the sibling pair was answered by a call that was not about it"
    );
}

/// A twelve-root cluster is exactly what the cap refused. Nothing about it is
/// oversized any more: it is a sequence of two-artifact questions.
#[tokio::test]
async fn a_cluster_past_the_old_cap_is_asked_about_rather_than_refused() {
    let mut core = test_core().await;
    core.judge = Arc::new(ScriptedCompleter::new(vec![
        r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
    ]));
    let rows: Vec<(&str, [f32; 2])> = vec![
        ("t0", [1.00, 0.00]), ("t1", [0.99, 0.01]), ("t2", [0.98, 0.02]),
        ("t3", [0.97, 0.03]), ("t4", [0.96, 0.04]), ("t5", [0.95, 0.05]),
        ("t6", [0.94, 0.06]), ("t7", [0.93, 0.07]), ("t8", [0.92, 0.08]),
        ("t9", [0.91, 0.09]), ("t10", [0.90, 0.10]), ("t11", [0.89, 0.11]),
    ];
    let ids = seed(&core, &rows).await;
    let seed_pair = queue_pair(&core, &ids[0], &ids[1]).await;
    for w in ids.windows(2).skip(1) {
        queue_pair(&core, &w[0], &w[1]).await;
    }

    run(&core, &seed_pair.to_string()).await.unwrap();

    assert!(
        core.store.pairs_by_state(PairState::Oversized, 10).await.unwrap().is_empty(),
        "a twelve-root cluster was refused instead of asked about"
    );
    assert_eq!(core.store.pairs_by_state(PairState::NoConflict, 10).await.unwrap().len(), 1);
}

/// The letter indexes the two members and can reach nothing else. This used to
/// be resolved against the flattened roots while the members were a different
/// list, and whenever a component held an earlier merge the mismatch superseded
/// an artifact the model had never been shown.
#[tokio::test]
async fn a_letter_names_a_member_and_never_one_of_its_sources() {
    let mut core = test_core().await;
    core.judge = Arc::new(ScriptedCompleter::new(vec![
        r#"{"relation":"replaced","detail":"a is stale","supersedes":"a"}"#.into(),
    ]));
    let ids = seed_titled(
        &core,
        &[
            ("Old", "the pool holds eight", [1.0, 0.0]),
            ("New", "the pool holds sixteen", [0.93, 0.37]),
            ("Third", "unrelated text", [0.60, 0.80]),
        ],
    )
    .await;
    // A merge whose roots are ids[0] and ids[2], so that "a" could resolve to a
    // source if letters ever reached past the members again.
    let m = crate::jobs::merge::write(
        &core,
        &crate::infer::prompt::MergedDraft {
            title: Some("Merged".into()),
            text: "the pool holds eight, and unrelated text".into(),
            category: None,
            tags: vec![],
            caveats: vec![],
        },
        &[ids[0].clone(), ids[2].clone()],
    )
    .await
    .unwrap();
    core.store.mark_indexed(&m.id).await.ok();
    let pair = queue_pair(&core, &m.id, &ids[1]).await;

    run(&core, &pair.to_string()).await.unwrap();

    let merged = core.store.get_artifact(&m.id).await.unwrap();
    assert!(!merged.in_results(), "the member named obsolete was not the one superseded");
    let survivor = core.store.get_artifact(&ids[1]).await.unwrap();
    assert!(survivor.in_results());
}

/// The whole point of merging two at a time: the result is mergeable again, and
/// it carries the flattened lineage of both sides rather than naming them.
#[tokio::test]
async fn a_merge_of_a_merge_names_every_original_behind_it() {
    let mut core = test_core().await;
    core.judge = Arc::new(ScriptedCompleter::new(vec![
        r#"{"relation":"duplicate","detail":"same thing","merged":{"title":"Pool","text":"max_connections is 16, raise it for batch jobs, sixteen connections","category":null,"tags":[],"caveats":[]}}"#.into(),
    ]));
    let ids = seed_titled(
        &core,
        &[
            ("Pool sizing", "max_connections is 16", [1.0, 0.0]),
            ("Pool notes", "raise it for batch jobs", [0.99, 0.05]),
            ("Connections", "sixteen connections", [0.93, 0.37]),
        ],
    )
    .await;
    let m1 = crate::jobs::merge::write(
        &core,
        &crate::infer::prompt::MergedDraft {
            title: Some("Pool".into()),
            text: "max_connections is 16, raise it for batch jobs".into(),
            category: None,
            tags: vec![],
            caveats: vec![],
        },
        &[ids[0].clone(), ids[1].clone()],
    )
    .await
    .unwrap();
    core.store.mark_indexed(&m1.id).await.ok();
    let pair = queue_pair(&core, &m1.id, &ids[2]).await;

    run(&core, &pair.to_string()).await.unwrap();

    let settled = core.store.get_pair(pair).await.unwrap();
    let m2 = settled.merged_into.expect("the second merge was not written");
    let roots = core.store.roots_of(&[m2]).await.unwrap();
    let roots: Vec<&String> = roots.values().flatten().collect();
    assert_eq!(roots.len(), 3, "the merge of a merge did not inherit both lineages");
    for id in &ids {
        assert!(roots.contains(&id), "an original is missing from the lineage");
    }
}

/// A member that is itself a merge is shown to the model as its own text, with
/// its captured roots beside it as reference.
#[tokio::test]
async fn a_merged_member_reaches_the_model_with_its_sources() {
    let mut core = test_core().await;
    let judge = Arc::new(ScriptedCompleter::new(vec![
        r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
    ]));
    core.judge = judge.clone();
    let ids = seed_titled(
        &core,
        &[
            ("Pool sizing", "max_connections is 16", [1.0, 0.0]),
            ("Pool notes", "raise it for batch jobs", [0.99, 0.05]),
            ("Connections", "sixteen connections", [0.93, 0.37]),
        ],
    )
    .await;
    let m = crate::jobs::merge::write(
        &core,
        &crate::infer::prompt::MergedDraft {
            title: Some("Pool".into()),
            text: "the pool holds sixteen".into(),
            category: None,
            tags: vec![],
            caveats: vec![],
        },
        &[ids[0].clone(), ids[1].clone()],
    )
    .await
    .unwrap();
    core.store.mark_indexed(&m.id).await.ok();
    let pair = queue_pair(&core, &m.id, &ids[2]).await;

    run(&core, &pair.to_string()).await.unwrap();

    let sent = judge.prompts();
    let sent = sent.first().expect("the judge was asked");
    assert!(sent.contains("the pool holds sixteen"), "the merge's own words were withheld");
    assert!(sent.contains("max_connections is 16"), "a source was not shown as context");
    assert!(sent.contains("SOURCES OF A") || sent.contains("SOURCES OF B"));
}

/// One member is a source of the other: a merge and one of the artifacts it was
/// written from. Comparing them asks whether an artifact matches itself.
#[tokio::test]
async fn a_merge_and_one_of_its_own_sources_is_dismissed_without_a_call() {
    let mut core = test_core().await;
    let judge = Arc::new(ScriptedCompleter::new(vec![]));
    core.judge = judge.clone();
    let ids = seed(&core, &[("a text", [1.0, 0.0]), ("b text", [0.99, 0.05])]).await;
    let m = crate::jobs::merge::write(
        &core,
        &crate::infer::prompt::MergedDraft {
            title: Some("Merged".into()),
            text: "a text and b text".into(),
            category: None,
            tags: vec![],
            caveats: vec![],
        },
        &[ids[0].clone(), ids[1].clone()],
    )
    .await
    .unwrap();
    let pair = queue_pair(&core, &m.id, &ids[0]).await;

    run(&core, &pair.to_string()).await.unwrap();

    assert_eq!(judge.calls(), 0, "a call was spent asking whether an artifact matches itself");
    assert_eq!(core.store.pairs_by_state(PairState::Dismissed, 10).await.unwrap().len(), 1);
}

/// The loss check runs against what was actually merged. A value that only ever
/// lived in a context source was already dropped a generation ago; failing on it
/// would freeze the lineage and no later merge in it could ever be written.
#[tokio::test]
async fn a_value_lost_by_an_earlier_merge_does_not_block_the_next_one() {
    let mut core = test_core().await;
    core.judge = Arc::new(ScriptedCompleter::new(vec![
        r#"{"relation":"duplicate","detail":"same thing","merged":{"title":"Pool","text":"the pool holds sixteen connections","category":null,"tags":[],"caveats":[]}}"#.into(),
    ]));
    let ids = seed_titled(
        &core,
        &[
            ("Pool sizing", "max_connections is 16 and the timeout is 30s", [1.0, 0.0]),
            ("Pool notes", "raise it for batch jobs", [0.99, 0.05]),
            ("Connections", "sixteen connections", [0.93, 0.37]),
        ],
    )
    .await;
    // This merge already dropped "30s". The next one must not be blamed for it.
    let m = crate::jobs::merge::write(
        &core,
        &crate::infer::prompt::MergedDraft {
            title: Some("Pool".into()),
            text: "the pool holds sixteen".into(),
            category: None,
            tags: vec![],
            caveats: vec![],
        },
        &[ids[0].clone(), ids[1].clone()],
    )
    .await
    .unwrap();
    core.store.mark_indexed(&m.id).await.ok();
    let pair = queue_pair(&core, &m.id, &ids[2]).await;

    run(&core, &pair.to_string()).await.unwrap();

    assert!(
        core.store.pairs_by_state(PairState::Contradiction, 10).await.unwrap().is_empty(),
        "the merge was blamed for a value an earlier one dropped"
    );
}
```

`ScriptedCompleter::prompts()` may not exist. If it does not, add it to `src/infer/fake.rs` beside `calls()` — a `Mutex<Vec<String>>` of the user prompts it was handed, returned as a clone. That is part of this step.

`MergedDraft`'s exact field list is in `src/infer/prompt.rs`; match it rather than the snippet if they differ.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --lib jobs::dedupe`
Expected: compile errors — `dedupe_prompt` takes two members now, and `Settlement` has no `pair` field.

- [ ] **Step 3: Rewrite `Settlement`, `run`, `interpret`, `apply` and `settle_all`**

`Settlement` becomes:

```rust
/// What the model decided, with everything the write path needs already read.
pub struct Settlement {
    pub relation: Relation,
    pub detail: Option<String>,
    /// The member named obsolete, already checked against newest-wins. Only
    /// set for `Replaced`.
    pub obsolete: Option<String>,
    /// Only set for `Duplicate`, and only once the loss check has passed.
    pub merged: Option<MergedDraft>,
    /// The two artifacts the model was shown, in letter order.
    pub members: Vec<Chunk>,
    pub pair: ArtifactPair,
}
```

`run` becomes (Task 5 adds the trim loop where the comment says so):

```rust
pub async fn run(core: &Core, pair_id: &str) -> Result<()> {
    let id: i64 = pair_id.parse().map_err(|_| Error::NotFound)?;
    let p = core.store.get_pair(id).await?;
    if p.state != PairState::Pending {
        // Settled by an operator, by a later sweep, or by the unit that merged
        // one of its members while this one waited out a backoff.
        return Ok(());
    }

    let a = core.store.get_artifact(&p.a_id).await?;
    let b = core.store.get_artifact(&p.b_id).await?;
    // Re-checked here and not only when the unit was armed: a member can be
    // superseded by a later sweep or deprecated by an operator while this waits
    // out a backoff, and spending the scarcest thing in the system to rule on
    // an artifact no longer in results buys nothing.
    if !a.in_results() || !b.in_results() {
        return settle(
            core,
            &p,
            PairState::Dismissed,
            Some("a member is no longer in results"),
        )
        .await;
    }

    let members = vec![a, b];
    let member_ids: Vec<String> = members.iter().map(|c| c.id.clone()).collect();
    let root_map = core.store.roots_of(&member_ids).await?;
    // A member with no roots at all is a merge whose sources were deleted out
    // from under it. Its text is a paraphrase with nothing behind it — not
    // something a rule can settle. A person decides.
    if members
        .iter()
        .any(|c| root_map.get(&c.id).is_none_or(|r| r.is_empty()))
    {
        return settle(
            core,
            &p,
            PairState::Contradiction,
            Some("a merged member has lost its sources; resolve by hand"),
        )
        .await;
    }
    // A merge and one of its own sources are not two things to compare. Asking
    // would spend a call to be told an artifact matches itself.
    let one_contains_the_other = root_map[&members[0].id].contains(&members[1].id)
        || root_map[&members[1].id].contains(&members[0].id);
    if one_contains_the_other {
        return settle(
            core,
            &p,
            PairState::Dismissed,
            Some("one of these is a source of the other"),
        )
        .await;
    }

    // A merged member's captured roots, oldest first — context, never an input.
    // Read as whole artifacts because the prompt needs their titles: a body
    // that never names its own subject is the failure `dedupe_prompt` documents.
    let mut context: Vec<Vec<Chunk>> = Vec::new();
    for c in &members {
        let mut v = Vec::new();
        if c.provenance == crate::store::artifacts::Provenance::Merged {
            for rid in &root_map[&c.id] {
                match core.store.get_artifact(rid).await {
                    Ok(r) => v.push(r),
                    Err(Error::NotFound) => {}
                    Err(e) => return Err(e),
                }
            }
            v.sort_by(|x, y| x.created_at.cmp(&y.created_at));
        }
        context.push(v);
    }

    // Task 5 replaces this line with the budget trim loop.
    let user = build_prompt(&members, &context, p.judge_attempts);

    // Counted before the call and regardless of how it goes, so a pair the
    // model keeps failing on drops behind the rest of the queue rather than
    // absorbing the budget again on the next sweep.
    core.store.record_judge_attempt(p.id).await?;

    let permit = core.gate.background().await;
    let reply = core
        .judge
        .complete(crate::infer::prompt::DEDUPE_SYSTEM, &user)
        .await;
    permit.finished();
    let reply = reply?;

    let verdict = match crate::infer::prompt::parse_dedupe(&reply) {
        Ok(v) => v,
        // A reply that cannot be read is an error, not a verdict: the pair
        // stays pending and the unit retries under the queue's backoff.
        //
        // Retrying is only worth anything because `dedupe_prompt` carries the
        // attempt number. Against an endpoint that caches by exact prompt, an
        // unchanged prompt would replay the same unreadable bytes for every one
        // of `MAX_ATTEMPTS`.
        //
        // Counted here and not beside `record_judge_attempt`, because this is
        // the only failure that says anything about the pair. A call the
        // endpoint never answered says something about the endpoint, and
        // letting an outage count against every pending pair would take the
        // whole review queue out of reach on its way past.
        Err(e) => {
            core.store.record_unreadable_judgement(p.id).await?;
            tracing::warn!(
                pair = id,
                attempt = p.judge_attempts,
                reply_len = reply.len(),
                error = %e,
                "dedupe reply unreadable; pair stays pending"
            );
            return Err(e);
        }
    };

    apply(core, interpret(verdict, members, p)).await
}

/// Assemble the user prompt from the two members and whatever context survives
/// the budget.
fn build_prompt(members: &[Chunk], context: &[Vec<Chunk>], attempt: i64) -> String {
    let member = |i: usize| crate::infer::prompt::DedupeMember {
        title: members[i].title.as_deref().unwrap_or("untitled"),
        text: members[i].text.as_str(),
        sources: context[i]
            .iter()
            .map(|c| (c.title.as_deref().unwrap_or("untitled"), c.text.as_str()))
            .collect(),
    };
    crate::infer::prompt::dedupe_prompt(&member(0), &member(1), attempt)
}
```

`interpret` loses its `roots` parameter and resolves the letter against the members:

```rust
fn interpret(
    v: crate::infer::prompt::Dedupe,
    members: Vec<Chunk>,
    pair: ArtifactPair,
) -> Settlement {
    let mut relation = v.relation;
    let mut detail = v.detail;
    let mut merged = v.merged;
    let mut obsolete = None;

    if relation == Relation::Replaced {
        // Trust a named direction only when it agrees with the sweep's own
        // newest-wins bias (see `keeper`): a call naming the *newer* artifact
        // obsolete is exactly the failure mode worth guarding against, since it
        // would hide the side more likely to be current.
        //
        // The letter indexes the members, which are the only artifacts the
        // prompt letters. Context sources are unlettered by construction, so a
        // letter can no longer resolve to something the model was shown as
        // reference — the mismatch that used to supersede an artifact the model
        // had never been shown at all.
        let named = v
            .supersedes
            .map(|c| (c as u8 - b'a') as usize)
            .and_then(|i| members.get(i));
        obsolete = match named {
            Some(named)
                if members
                    .iter()
                    .all(|o| o.id == named.id || named.created_at <= o.created_at) =>
            {
                Some(named.id.clone())
            }
            _ => None,
        };
        if obsolete.is_none() {
            relation = Relation::Conflict;
        }
    }

    if relation == Relation::Duplicate
        && let Some(d) = &merged
    {
        // Against the members, which are what was actually merged — not against
        // every captured root behind them. A merged member's own text is
        // already a generation away from its sources, so checking the draft
        // against those sources would fail on a value dropped by an earlier
        // merge and freeze that lineage: no later merge in it could ever be
        // written. Loss stays one generation deep per step, and the sources go
        // into the prompt as context precisely so the model can undo the
        // earlier drift rather than compound it.
        let lost = crate::jobs::merge::losses(&members, d);
        if !lost.is_empty() {
            detail = Some(format!("the merge would have lost {}", lost.join(", ")));
            relation = Relation::Conflict;
            merged = None;
        }
    }

    Settlement { relation, detail, obsolete, merged, members, pair }
}
```

`apply`:

```rust
async fn apply(core: &Core, s: Settlement) -> Result<()> {
    match s.relation {
        Relation::Distinct => {
            settle(core, &s.pair, PairState::NoConflict, s.detail.as_deref()).await
        }
        Relation::Conflict => {
            tracing::info!("artifacts disagree; escalating rather than merging");
            settle(core, &s.pair, PairState::Contradiction, s.detail.as_deref()).await
        }
        Relation::Replaced => {
            let obsolete = s
                .obsolete
                .clone()
                .expect("interpret sets this or downgrades to Conflict");
            let winner = s
                .members
                .iter()
                .find(|m| m.id != obsolete)
                .map(|m| m.id.clone())
                .expect("a pair has two members and only one of them is obsolete");
            // A fresh status, not the snapshot `interpret` saw: an operator can
            // retire the named side while the unit waits out a backoff.
            let still_live = match core.store.get_artifact(&obsolete).await {
                Ok(c) => c.in_results(),
                Err(Error::NotFound) => false,
                Err(e) => return Err(e),
            };
            if !still_live {
                return settle(
                    core,
                    &s.pair,
                    PairState::NoConflict,
                    Some("the named replacement is already out of results"),
                )
                .await;
            }
            // The side effect FIRST. A failure here leaves the pair pending, so
            // the unit retries under the queue's backoff — the reverse order
            // left the verdict recorded on the pair but never applied, because
            // `run` skips a pair that is no longer Pending.
            core.supersede(&obsolete, &winner).await?;
            tracing::info!(superseded = %obsolete, by = %winner, "applied a replacement");
            // Done, with the model's reasoning kept as the record of why.
            // Leaving it Superseded listed the applied replacement as awaiting
            // confirmation forever.
            settle(core, &s.pair, PairState::Dismissed, s.detail.as_deref()).await
        }
        Relation::Duplicate => {
            let draft = s
                .merged
                .as_ref()
                .expect("interpret keeps this or downgrades to Conflict");
            // Both members, not their roots. A merged member is not its own
            // root, and `finish` hides what the lineage names — so passing only
            // roots would leave that earlier merge active and near-identical to
            // the new one. `insert_merged_artifact` flattens both to captured
            // roots, and `subsumed_merges` catches the merged member.
            let sources: Vec<String> = s.members.iter().map(|m| m.id.clone()).collect();
            let m = crate::jobs::merge::write(core, draft, &sources).await?;
            // `merged_into` rather than a detail string: if the embed never
            // lands, the sweep's reap has to find exactly this pair and reopen
            // it (`reap_stranded`).
            core.store
                .set_pair_merged(s.pair.id, &m.id, s.detail.as_deref())
                .await
        }
    }
}

/// One pair, one verdict.
async fn settle(
    core: &Core,
    pair: &ArtifactPair,
    state: PairState,
    detail: Option<&str>,
) -> Result<()> {
    core.store.set_pair_state(pair.id, state, detail).await
}
```

Finally, rewrite the module header (lines 1-25) to describe the unit that exists. It currently argues *for* the component and *against* pairwise settlement, which is now backwards. Say: one pair, one call; a merged member is shown its own text with its captured roots beside it as context; the four verdicts and which two touch an artifact; and that a cluster converges by repeated pairwise merging, each result inheriting the flattened roots of both sides.

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test --lib jobs::dedupe`
Expected: PASS. Then `cargo test` for the whole suite — `src/jobs/consolidate.rs` and `src/web/ui.rs` have tests that exercise the dedupe path and may need their expectations adjusted from "one call settles the component" to "one call settles a pair".

- [ ] **Step 5: Commit**

```bash
cargo test && cargo clippy --all-targets -- -D warnings
git add src/jobs/dedupe.rs src/infer/fake.rs
git commit -m "feat(dedupe): ask about two artifacts at a time, and let the answers stack"
```

---

### Task 5: Trimming context to the budget

**Files:**
- Modify: `src/jobs/dedupe.rs` (the `build_prompt` call site in `run`)
- Test: `src/jobs/dedupe.rs` `mod tests`

**Interfaces:**
- Consumes: `crate::infer::budget::{TokenCounter, checked_ceiling_for_prompt}` (`src/infer/budget.rs`), `Completer::context_tokens()` and `Completer::max_output_tokens()` (`src/infer/mod.rs:111`, `:119`), `build_prompt` from Task 4.
- Produces: no new public surface.

- [ ] **Step 1: Write the failing tests**

```rust
/// The context block is reference material, so a window too small to hold it
/// costs an answer quality, not the answer itself. The two artifacts under
/// judgement always survive the trim.
#[tokio::test]
async fn a_context_block_too_big_for_the_window_is_trimmed_not_refused() {
    let mut core = test_core().await;
    let judge = Arc::new(ScriptedCompleter::new(vec![
        r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
    ]));
    // A window that fits the two members and little else.
    judge.set_context_tokens(600);
    core.judge = judge.clone();
    let long = "x ".repeat(2000);
    let ids = seed_titled(
        &core,
        &[
            ("Root one", long.as_str(), [1.0, 0.0]),
            ("Root two", "raise it for batch jobs", [0.99, 0.05]),
            ("Other", "sixteen connections", [0.93, 0.37]),
        ],
    )
    .await;
    let m = crate::jobs::merge::write(
        &core,
        &crate::infer::prompt::MergedDraft {
            title: Some("Pool".into()),
            text: "the pool holds sixteen".into(),
            category: None,
            tags: vec![],
            caveats: vec![],
        },
        &[ids[0].clone(), ids[1].clone()],
    )
    .await
    .unwrap();
    core.store.mark_indexed(&m.id).await.ok();
    let pair = queue_pair(&core, &m.id, &ids[2]).await;

    run(&core, &pair.to_string()).await.unwrap();

    assert_eq!(judge.calls(), 1, "the call was refused instead of trimmed");
    let sent = judge.prompts().first().cloned().unwrap();
    assert!(sent.contains("the pool holds sixteen"), "a member was trimmed away");
    assert!(sent.contains("sixteen connections"), "a member was trimmed away");
    assert!(!sent.contains(long.as_str()), "the oversized source was not trimmed");
}

/// Two artifacts that alone do not fit is a different failure with a different
/// cause — an artifact no pair containing it can ever be judged against — and it
/// goes to a person rather than being counted as answered.
#[tokio::test]
async fn two_members_that_alone_do_not_fit_go_to_a_person() {
    let mut core = test_core().await;
    let judge = Arc::new(ScriptedCompleter::new(vec![]));
    judge.set_context_tokens(100);
    core.judge = judge.clone();
    let long = "x ".repeat(4000);
    let ids = seed_titled(
        &core,
        &[("One", long.as_str(), [1.0, 0.0]), ("Two", long.as_str(), [0.93, 0.37])],
    )
    .await;
    let pair = queue_pair(&core, &ids[0], &ids[1]).await;

    run(&core, &pair.to_string()).await.unwrap();

    assert_eq!(judge.calls(), 0, "a call that cannot fit the window was sent anyway");
    let stuck = core.store.pairs_by_state(PairState::Contradiction, 10).await.unwrap();
    assert_eq!(stuck.len(), 1);
    assert!(
        stuck[0].detail.as_deref().unwrap_or("").contains("do not fit"),
        "the reason a person is being asked was not recorded"
    );
    assert!(
        core.store.pairs_by_state(PairState::Oversized, 10).await.unwrap().is_empty(),
        "the refused state came back under the old name"
    );
}
```

`ScriptedCompleter` needs `set_context_tokens`. If it hard-codes `context_tokens()`, add an `AtomicUsize` field defaulting to whatever it returns today and a setter, in `src/infer/fake.rs`, as part of this step.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --lib jobs::dedupe::tests::a_context_block_too_big_for_the_window_is_trimmed_not_refused`
Expected: FAIL — the prompt goes out whole, or the endpoint-shaped assertion about the trim does not hold.

- [ ] **Step 3: Replace the `build_prompt` call in `run` with the trim loop**

```rust
    // The two artifacts under judgement are bounded by what capture bounds and
    // always go out. What can grow without limit is the context block behind a
    // long lineage — and context is reference, not input, so it is trimmed
    // rather than defended against. Oldest first: the roots furthest from the
    // present are the ones a later capture is most likely to have restated.
    //
    // No count-based cap. `merge_max_roots` was one, set to eight by a default
    // nobody typed, and it settled whole clusters before any call was made —
    // sixteen pairs sat refused with `judge_attempts = 0`, twelve roots against
    // a ceiling that could have held them many times over.
    let counter = crate::infer::budget::TokenCounter;
    let window = core.judge.context_tokens();
    let ceiling = core.judge.max_output_tokens();
    let system = counter.count(crate::infer::prompt::DEDUPE_SYSTEM);
    let user = loop {
        let user = build_prompt(&members, &context, p.judge_attempts);
        let cost = system + counter.count(&user);
        if crate::infer::budget::checked_ceiling_for_prompt(window, cost, ceiling).is_some() {
            break user;
        }
        // Whichever member still holds the oldest surviving source gives it up.
        let oldest = context
            .iter()
            .enumerate()
            .filter(|(_, v)| !v.is_empty())
            .min_by(|(_, x), (_, y)| x[0].created_at.cmp(&y[0].created_at))
            .map(|(i, _)| i);
        match oldest {
            Some(i) => {
                context[i].remove(0);
            }
            // Nothing left to give: the two artifacts alone do not fit one
            // call. That is a fact about an artifact's size and not about this
            // pair, and no rule here can settle it — so it goes to a person
            // rather than being recorded as answered.
            None => {
                return settle(
                    core,
                    &p,
                    PairState::Contradiction,
                    Some("these two artifacts do not fit one call; resolve by hand"),
                )
                .await;
            }
        }
    };
```

- [ ] **Step 4: Run and watch them pass**

Run: `cargo test --lib jobs::dedupe`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo test && cargo clippy --all-targets -- -D warnings
git add src/jobs/dedupe.rs src/infer/fake.rs
git commit -m "feat(dedupe): trim the context to the window instead of refusing the call"
```

---

### Task 6: A merge takes its members' open questions with it

**Files:**
- Modify: `src/jobs/merge.rs:88-131` (`finish`)
- Test: `src/jobs/merge.rs` `mod tests`

**Interfaces:**
- Consumes: `Store::repoint_open_pairs` from Task 1.
- Produces: no new public surface.

- [ ] **Step 1: Write the failing tests**

```rust
/// C was a duplicate of B; B is now inside M. Without this the question dies
/// with B and only comes back when M embeds and a later similarity sweep
/// re-files it — a whole tick per generation of a cluster.
#[tokio::test]
async fn finishing_a_merge_moves_its_members_open_pairs_onto_it() {
    let core = test_core().await;
    let ids = seed(
        &core,
        &[("a text", [1.0, 0.0]), ("b text", [0.99, 0.05]), ("c text", [0.93, 0.37])],
    )
    .await;
    core.store.record_pair(&ids[1], &ids[2], 0.91).await.unwrap();
    let m = write(
        &core,
        &crate::infer::prompt::MergedDraft {
            title: Some("Merged".into()),
            text: "a text and b text".into(),
            category: None,
            tags: vec![],
            caveats: vec![],
        },
        &[ids[0].clone(), ids[1].clone()],
    )
    .await
    .unwrap();
    core.store.mark_indexed(&m.id).await.unwrap();

    finish(&core, &m.id).await.unwrap();

    assert_eq!(
        core.store.pair_state_between(&ids[2], &m.id).await.unwrap(),
        Some(crate::store::pairs::PairState::Pending),
        "the surviving duplicate has no question against the merge"
    );
}

/// `finish` is re-run by the sweep for as long as the merge stands, so running
/// it twice must not put a pair back that the second run already answered.
#[tokio::test]
async fn re_pointing_is_safe_to_run_twice() {
    let core = test_core().await;
    let ids = seed(
        &core,
        &[("a text", [1.0, 0.0]), ("b text", [0.99, 0.05]), ("c text", [0.93, 0.37])],
    )
    .await;
    core.store.record_pair(&ids[1], &ids[2], 0.91).await.unwrap();
    let m = write(
        &core,
        &crate::infer::prompt::MergedDraft {
            title: Some("Merged".into()),
            text: "a text and b text".into(),
            category: None,
            tags: vec![],
            caveats: vec![],
        },
        &[ids[0].clone(), ids[1].clone()],
    )
    .await
    .unwrap();
    core.store.mark_indexed(&m.id).await.unwrap();

    finish(&core, &m.id).await.unwrap();
    finish(&core, &m.id).await.unwrap();

    let pending = core.store.pairs_by_state(crate::store::pairs::PairState::Pending, 10).await.unwrap();
    assert_eq!(pending.len(), 1, "a second finish duplicated the question");
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --lib jobs::merge::tests::finishing_a_merge_moves_its_members_open_pairs_onto_it`
Expected: FAIL — `pair_state_between` returns `None`, because the pair still names B.

- [ ] **Step 3: Implement**

At the end of `finish`, after the `subsumed_merges` loop and before `Ok(())`:

```rust
    // The open questions its members carried are now questions about the merge.
    //
    // Here and not in `write`: at this point the merge is indexed and its
    // sources are superseded. Re-pointing at write time would arm a unit that
    // could merge this artifact into a further one before it had ever
    // superseded its own sources, leaving them active underneath a chain whose
    // middle is out of results — the dead end `repoint_supersession` exists to
    // prevent on the other side.
    //
    // Warn and carry on, like the loops above: a pair that could not be moved
    // is a question filed against an artifact that is now hidden, which the
    // next sweep re-files against the merge. Losing the whole repair over it
    // would be the more expensive failure.
    let mut moved_from: Vec<String> = core.store.roots_to_hide(&m.id).await.unwrap_or_default();
    moved_from.extend(core.store.subsumed_merges(&m.id).await.unwrap_or_default());
    match core.store.repoint_open_pairs(&moved_from, &m.id).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(merged = %m.id, pairs = n, "moved open pairs onto a merge"),
        Err(e) => tracing::warn!(merged = %m.id, error = %e, "could not move open pairs onto a merge"),
    }
```

`roots_to_hide` is read a second time here rather than reusing the earlier binding because the first loop consumes it; hoist it into a `let` at the top of `finish` and use it in both places if that reads better.

- [ ] **Step 4: Run and watch them pass**

Run: `cargo test --lib jobs::merge`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo test && cargo clippy --all-targets -- -D warnings
git add src/jobs/merge.rs
git commit -m "feat(merge): carry a member's open questions onto the merge that swallowed it"
```

---

### Task 7: The sweep drains the refused queue, and the UI stops listing it

**Files:**
- Modify: `src/jobs/consolidate.rs:265-320` (`run`, beside the other repairs)
- Modify: `src/web/ui.rs:1373-1380` (`PAIR_STATES`)
- Test: `src/jobs/consolidate.rs` `mod tests`

**Interfaces:**
- Consumes: `Store::reopen_oversized` from Task 2.
- Produces: no new public surface. `PAIR_STATES` becomes a `[PairState; 3]`.

- [ ] **Step 1: Write the failing test**

```rust
/// The state was terminal and reached without a call. Every row in it is an
/// unanswered question, and the sweep is what puts them back.
#[tokio::test]
async fn the_sweep_puts_the_refused_pairs_back_in_the_queue() {
    let core = test_core().await;
    let ids = seed(&core, &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])]).await;
    core.store.record_pair(&ids[0], &ids[1], 0.91).await.unwrap();
    let id = core
        .store
        .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
        .await
        .unwrap()[0]
        .id;
    core.store
        .set_pair_state(id, crate::store::pairs::PairState::Oversized, Some("12 sources, cap is 8"))
        .await
        .unwrap();

    run(&core).await.unwrap();

    assert!(
        core.store
            .pairs_by_state(crate::store::pairs::PairState::Oversized, 10)
            .await
            .unwrap()
            .is_empty(),
        "a pair refused before any call is still refused"
    );
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test --lib jobs::consolidate::tests::the_sweep_puts_the_refused_pairs_back_in_the_queue`
Expected: FAIL — the row is still `Oversized`.

- [ ] **Step 3: Implement**

In `consolidate::run`, after the `flag_orphans` call and before `let mut out = Outcome::default();`:

```rust
    // Pairs the old fan-in cap refused before any call was made. There is no
    // cap now, so each one is simply an unanswered question — and every one of
    // them has `judge_attempts = 0`, so putting them back redoes no work and
    // resets no backoff. Runs every sweep and matches nothing once drained,
    // which is cheaper than the machinery a one-shot would need.
    match core.store.reopen_oversized().await {
        Ok(0) => {}
        Ok(n) => tracing::info!(pairs = n, "reopened pairs the fan-in cap had refused"),
        Err(e) => tracing::warn!(error = %e, "could not reopen the pairs the cap refused"),
    }
```

In `src/web/ui.rs`, drop `PairState::Oversized` and its comment from `PAIR_STATES`, and change the array's length to 3:

```rust
const PAIR_STATES: [crate::store::pairs::PairState; 3] = [
    crate::store::pairs::PairState::Contradiction,
    crate::store::pairs::PairState::Superseded,
    crate::store::pairs::PairState::Pending,
];
```

The `more` count at `src/web/ui.rs:1478` needs no change — it is computed from `PAIR_STATES` and already reports what `PAIR_LIMIT` truncates.

- [ ] **Step 4: Run and watch it pass**

Run: `cargo test --lib jobs::consolidate && cargo test --lib web::ui`
Expected: PASS. A UI test asserting an oversized row appears on the page must be deleted — the state is no longer written and its rows are reopened.

- [ ] **Step 5: Commit**

```bash
cargo test && cargo clippy --all-targets -- -D warnings
git add src/jobs/consolidate.rs src/web/ui.rs
git commit -m "fix(consolidate): put the pairs the cap refused back, and stop listing the state"
```

---

### Task 8: Retiring `merge_max_roots`

**Files:**
- Modify: `src/config.rs:239-245` (field), `:286` (default), `:830-850` (the clamp), `:1128-1143` (the clamp's test)
- Modify: `config.example.toml:260`, `README.md:185`
- Modify: comments only — `src/infer/budget.rs:74`, `src/infer/openai.rs:840`, `src/infer/openai.rs:1666`, `src/store/pairs.rs:64`
- Test: `src/config.rs` `mod tests`

**Interfaces:**
- Produces: `ConsolidateConfig::merge_max_roots: Option<usize>`, read by nothing.

- [ ] **Step 1: Write the failing test**

Replace `a_merge_cap_below_two_goes_back_to_the_default` (`src/config.rs:1128`) with:

```rust
/// The key stays parseable for one release so an existing config file still
/// loads, and says nothing about how the sweep behaves. The default of eight
/// was quietly switching merging off for any cluster past eight sources.
#[test]
fn the_merge_cap_is_accepted_and_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let p = write(&dir, &format!("{MINIMAL}\n[consolidate]\nmerge_max_roots = 1\n"));
    let cfg = Config::load(Some(&p)).unwrap();
    assert_eq!(cfg.consolidate.merge_max_roots, Some(1));
    assert_eq!(ConsolidateConfig::default().merge_max_roots, None);
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test --lib config::tests::the_merge_cap_is_accepted_and_ignored`
Expected: compile error — `expected usize, found Option<usize>`.

- [ ] **Step 3: Implement**

Field (`src/config.rs:239-245`):

```rust
    /// Deprecated and unread. Kept for one release so an existing config file
    /// still loads.
    ///
    /// It capped how many captured roots one merge could be written from, and
    /// a component past it was settled `Oversized` — terminal, and reached
    /// before any call. The default of eight was never typed by anyone and
    /// switched merging off for every cluster past eight sources; sixteen pairs
    /// sat refused with no attempt against them. The unit now merges two
    /// artifacts at a time and lets the results be merged again, so fan-in is
    /// not a thing one call has to survive.
    pub merge_max_roots: Option<usize>,
```

Default (`src/config.rs:286`): `merge_max_roots: None,`.

Delete the clamp at `src/config.rs:838-850` entirely, and in its place, in whatever `validate`-shaped function it lived in:

```rust
        if self.consolidate.merge_max_roots.is_some() {
            tracing::warn!(
                "consolidate.merge_max_roots is deprecated and ignored: merging is pairwise \
                 now, so there is no fan-in for one call to survive"
            );
        }
```

Remove `merge_max_roots = 8` and its comment from `config.example.toml:260`. Remove `merge_max_roots` from the `consolidate.*` row of `README.md:185`.

Rewrite the four comments that describe it as a live bound. `src/infer/budget.rs:74` and `src/infer/openai.rs:840`/`:1666` should say that the dedupe judge packs two artifacts plus a context block trimmed against this window, which is why the ceiling has to come off the prompt's own cost. `src/store/pairs.rs:64` documents `PairState::Oversized`: say that it is no longer written, that its rows are reopened by the sweep, and that the variant survives one release so old rows parse.

- [ ] **Step 4: Run and watch it pass**

Run: `cargo test --lib config`
Expected: PASS. Then `cargo test` — nothing else should reference the field.

- [ ] **Step 5: Verify no live references remain**

Run: `grep -rn "merge_max_roots" src/ config.example.toml README.md`
Expected: only the field, its default, the deprecation warning, and the comments that explain the deprecation. No reads.

- [ ] **Step 6: Commit**

```bash
cargo test && cargo clippy --all-targets -- -D warnings
git add src/config.rs src/infer/budget.rs src/infer/openai.rs src/store/pairs.rs config.example.toml README.md
git commit -m "feat(config): retire the fan-in cap that was refusing clusters before any call"
```

---

## Verification after the last task

- [ ] `cargo test` — whole suite green.
- [ ] `cargo clippy --all-targets -- -D warnings` — clean.
- [ ] `grep -rn "Oversized" src/` — only the enum variant, its string mapping, `reopen_oversized`, and the comments explaining the deprecation. Nothing writes it.
- [ ] Against a copy of the real database: count `SELECT state, count(*) FROM artifact_pairs GROUP BY state` before and after one sweep. The sixteen `oversized` rows should be `pending`, and their `judge_attempts` still zero.
