# PR #11 Review Fixes Implementation Plan

> **Executed 2026-08-13.** Three things diverged from the plan as written, each
> recorded at the task it affected: the breaker test could not use a paused
> clock (Task 1), `arm_seq` turned out to be dead and was removed outright
> (Task 5), and finding #5 was largely unreachable so Task 3 shrank to the part
> that was real. Final state: 659 tests passing, clippy clean.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the six defects the code review of PR #11 ("A job becomes one inference call") found, without weakening any of the invariants that PR established.

**Architecture:** Six independent fixes across the inference gate, the embed stage, the judge stage, the reconciliation sweep, the synthesis planner, and the consolidation sweep's reporting. Each is small and local. Two of them (Tasks 1 and 2) both touch `src/jobs/embed.rs`, so they are ordered to avoid edit conflicts; the rest are independent and could in principle be done in any order.

**Tech Stack:** Rust, tokio, sqlx over SQLite, `cargo test --lib` for the inline `#[cfg(test)] mod tests` blocks each file carries.

## Global Constraints

- Branch is `feat/independently-schedulable-inference`. Do not merge or push; commit only.
- Every test lives in the inline `#[cfg(test)] mod tests` block at the bottom of the file it tests. There is no separate `tests/` file to add to for any task here.
- Verification command for every task: `cargo test --lib` (currently 653 passing). Individual tests: `cargo test --lib <test_name>`.
- **Comment style is load-bearing in this codebase.** Comments explain *why*, in prose, and usually name the concrete failure the code prevents. Match that register — every comment written in this plan is the comment to use, verbatim. Do not summarise them down to one line.
- Do not add dependencies.
- `MAX_ATTEMPTS`, `BATCH`, `SAFETY` and the other existing constants keep their current values.

---

### Task 1: An oversize refusal must not open the circuit breaker

**Files:**
- Modify: `src/infer/gate.rs:138-168` (add `call_refused`), `src/infer/gate.rs:182-190` (add `BackgroundPermit::refused`)
- Modify: `src/jobs/embed.rs:74-92`, `src/jobs/embed.rs:141-145`, `src/jobs/embed.rs:320-344`
- Test: `src/infer/gate.rs` tests module, `src/jobs/embed.rs` tests module

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `InferenceGate::call_refused(&self)` and `BackgroundPermit::refused(self)`. Both take no error argument — a refusal carries no information the gate acts on. Task 2 does not use them.

**Background:** `gate.rs:151` counts every `Error::Inference` toward `consecutive_transport_failures`. `embed.rs`'s `input_too_large()` matches on `Error::Inference { role: "embed", .. }`, so a semantic refusal — the endpoint answered, saying the chunk exceeds its physical batch — is counted as a transport failure. Three unsplittable oversize chunks in one corpus trips the default `breaker_after = 3` and holds *all* background inference for `breaker_probe_secs` against a healthy endpoint. This is the same category as `MalformedLlmOutput`, which the gate already excludes.

- [ ] **Step 1: Write the failing gate test**

Add to the `mod tests` block in `src/infer/gate.rs`, after `unreadable_output_is_not_an_endpoint_failure`:

```rust
    #[tokio::test(start_paused = true)]
    async fn a_refused_input_is_not_an_endpoint_failure() {
        // The endpoint answered: this input is too big for it. That is a fact
        // about the input, not about the endpoint's health — the same
        // distinction `MalformedLlmOutput` gets, arriving here as an
        // `Error::Inference` only because a size refusal is transport-shaped.
        // Counted, three unsplittable oversize chunks in one document would
        // hold every background call in the system — synthesis and judging
        // included — against a server that is working perfectly.
        let g =
            Arc::new(InferenceGate::new(Duration::ZERO).with_breaker(3, Duration::from_secs(60)));
        for _ in 0..5 {
            g.background().await.refused();
        }
        let started = tokio::time::Instant::now();
        g.background().await;
        assert_eq!(started.elapsed(), Duration::ZERO);
    }
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib a_refused_input_is_not_an_endpoint_failure`
Expected: compile error — `no method named 'refused' found for struct 'BackgroundPermit'`.

- [ ] **Step 3: Add `call_refused` to the gate**

In `src/infer/gate.rs`, insert immediately after the `call_failed` method (which ends at line 168, before the closing `}` of `impl InferenceGate`):

```rust
    /// A call the endpoint answered by refusing the input.
    ///
    /// It occupied the GPU, so it starts the cooldown like any other call. It
    /// says nothing about whether the endpoint is well, so it must not count
    /// toward the breaker — the same distinction `MalformedLlmOutput` gets, and
    /// it has to be made by the caller because a size refusal arrives as an
    /// `Error::Inference` and is indistinguishable here from a dead server.
    pub fn call_refused(&self) {
        let mut st = self.state.lock().expect("gate state");
        st.last_finished = Some(Instant::now());
    }
```

- [ ] **Step 4: Add `refused` to the permit**

In `src/infer/gate.rs`, inside `impl BackgroundPermit<'_>`, after `failed`:

```rust
    /// The endpoint answered, refusing the input. Starts the cooldown and hands
    /// the turn on, without counting against the endpoint.
    pub fn refused(self) {
        self.gate.call_refused();
    }
```

- [ ] **Step 5: Run the gate test to verify it passes**

Run: `cargo test --lib a_refused_input_is_not_an_endpoint_failure`
Expected: PASS

- [ ] **Step 6: Commit the gate half**

```bash
git add src/infer/gate.rs
git commit -m "feat: a refusal is not the endpoint failing"
```

**Executed note:** Steps 7-8 below specify `#[tokio::test(start_paused = true)]`
for the embed-level test. That does not work: building a `Core` opens an sqlx
pool, and a pool acquired under a paused clock waits on a timer that never fires,
so the test panics with `pool timed out while waiting for an open connection`.
The shipped test is a plain `#[tokio::test]` asserting through
`tokio::time::timeout(100ms, core.gate.background())`, the same real-time pattern
`the_per_chunk_path_is_paced_like_every_other_call` already uses. The gate-level
test in Steps 1-5 keeps `start_paused` — it builds no `Core`.

- [ ] **Step 7: Write the failing embed test**

Add to the `mod tests` block in `src/jobs/embed.rs`, after `a_refusal_during_the_as_is_attempt_still_ends_in_a_split`:

```rust
    #[tokio::test(start_paused = true)]
    async fn a_size_refusal_does_not_hold_the_rest_of_the_queue() {
        // Three unsplittable oversize chunks — code blocks, tables, the exact
        // thing `split_oversize`'s hard path exists for — used to be three
        // consecutive transport failures. At the default `breaker_after` that
        // opened the breaker and stopped synthesis, judging and embedding alike
        // for the whole probe window, against an endpoint that was answering
        // every request it was given.
        let mut core = crate::core::test_support::test_core().await;
        core.gate = std::sync::Arc::new(
            crate::infer::gate::InferenceGate::new(std::time::Duration::ZERO)
                .with_breaker(1, std::time::Duration::from_secs(60)),
        );
        core.embedder = std::sync::Arc::new(crate::infer::fake::StrictEmbedder::new(
            crate::core::test_support::TEST_DIM,
            200,
        ));

        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let body = (0..40)
            .map(|i| format!("paragraph {i} with a good deal of filler text in it"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: body,
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();

        // The configured limit is the lie; the endpoint's is what bites.
        run_with_limit(&core, &made[0].id, 8192).await.unwrap();

        let started = tokio::time::Instant::now();
        core.gate.background().await;
        assert_eq!(
            started.elapsed(),
            std::time::Duration::ZERO,
            "a chunk the endpoint refused opened the breaker"
        );
    }
```

- [ ] **Step 8: Run it to make sure it fails**

Run: `cargo test --lib a_size_refusal_does_not_hold_the_rest_of_the_queue`
Expected: FAIL — `assertion 'left == right' failed: a chunk the endpoint refused opened the breaker`, left `60s`, right `0ns`.

- [ ] **Step 9: Report the refusal as a refusal in `run_with_limit`**

In `src/jobs/embed.rs`, in `run_with_limit`, change the `input_too_large` arm's first line. Replace:

```rust
        Err(e) if input_too_large(&e) => {
            permit.failed(&e);
```

with:

```rust
        Err(e) if input_too_large(&e) => {
            // Refused, not failed: the endpoint answered. Counting this toward
            // the breaker let a handful of oversize chunks stop every
            // background call in the system.
            permit.refused();
```

- [ ] **Step 10: Report the refusal as a refusal in `run_corpus_with_limit`**

In the same file, in `run_corpus_with_limit`, replace:

```rust
        Err(e) if input_too_large(&e) => {
            permit.failed(&e);
            tracing::warn!(corpus_id, error = %e, "batch held a chunk the endpoint will not take; isolating");
```

with:

```rust
        Err(e) if input_too_large(&e) => {
            // Refused, not failed: the endpoint answered, and what it answered
            // is about one chunk in this batch rather than about the endpoint.
            permit.refused();
            tracing::warn!(corpus_id, error = %e, "batch held a chunk the endpoint will not take; isolating");
```

- [ ] **Step 11: Report the refusal as a refusal in `split_oversize`, and cover the no-budget case with it**

In `split_oversize`, the fall-through match currently has two error arms. Replace the whole `Err(e) if input_too_large(&e) && budget > 0 => { ... }` arm *and* leave the general `Err(e)` arm in place, so the two read:

```rust
        // Refused, not failed: the endpoint answered. This arm no longer tests
        // `budget`, because a refusal we cannot act on is still a refusal —
        // reporting it as a sick endpoint was how a single untouchable chunk
        // could hold the breaker open on every retry of it.
        Err(e) if input_too_large(&e) => {
            permit.refused();
            // Still nothing to cut with when the title alone fills the limit:
            // `split_by_lines` at a budget of zero puts every line in a part of
            // its own and then falls to the 64-character floor, which shreds the
            // text into fragments that are each still oversize once they inherit
            // the title. A refusal we cannot act on is reported as one.
            if budget > 0 {
                let parts = split_by_lines(&chunk.text, budget, &core.counter);
                if parts.len() > 1 {
                    tracing::warn!(artifact_id = %chunk.id, parts = parts.len(), "endpoint refused it whole; cutting on lines");
                    return replace_with_siblings(core, chunk, parts).await;
                }
            }
            return Err(e);
        }
        Err(e) => {
            permit.failed(&e);
            return Err(e);
        }
```

- [ ] **Step 12: Run the embed test to verify it passes**

Run: `cargo test --lib a_size_refusal_does_not_hold_the_rest_of_the_queue`
Expected: PASS

- [ ] **Step 13: Run the whole suite to check for regressions**

Run: `cargo test --lib`
Expected: PASS, 655 tests. `a_refusal_with_no_budget_left_is_reported_rather_than_shredded` must still pass — the restructured arm still returns `Err(e)` when `budget == 0`.

- [ ] **Step 14: Commit**

```bash
git add src/jobs/embed.rs
git commit -m "fix: a chunk the endpoint will not take is not a broken endpoint"
```

---

### Task 2: A batch job makes one inference call, oversize chunks included

**Files:**
- Modify: `src/jobs/embed.rs:110-125` (the scan loop in `run_corpus_with_limit`)
- Modify: `src/jobs/embed.rs:1189-1209` (the `an_oversize_chunk_does_not_block_its_siblings` test)
- Test: `src/jobs/embed.rs` tests module

**Interfaces:**
- Consumes: `Store::rearm_idle_seq(stage, target_kind, target_id, seq)` — already exists at `src/store/jobs.rs:197`.
- Produces: nothing new. `run_corpus_with_limit` keeps its signature.

**Background:** `run_corpus_with_limit` now takes at most `BATCH` chunks per job, but the loop above it still calls `split_oversize` for *every* oversize pending chunk, and that function's fall-through path makes a gated inference call apiece. A corpus with 50 unsplittable oversize chunks makes 50 sequential model calls inside one job — exactly the head-of-line blocking this PR set out to remove, and with `pacing.cooldown_secs > 0` it holds the turn for `50 × cooldown`.

The fix: the batch path stops splitting entirely. An oversize chunk gets its own per-artifact `Embed` unit, and `run_with_limit` — which already handles the oversize case at line 61 — splits it in a job of its own. This terminates: `rearm_if_more` stops re-arming the batch once `pending_artifacts_are_isolated` is true, and an exhausted per-artifact unit is marked embed-failed by `run_one` (`src/jobs/mod.rs:111-116`), which settles the corpus.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/jobs/embed.rs`, next to `one_run_embeds_at_most_one_batch`:

```rust
    #[tokio::test]
    async fn oversize_chunks_do_not_turn_one_batch_into_many_calls() {
        // `split_oversize` can cost a model call of its own — its fall-through
        // embeds the chunk whole to find out whether our estimate was wrong —
        // and the scan ran it once per oversize chunk. Fifty of them was fifty
        // sequential calls inside a job that is allowed exactly one, holding the
        // turn for fifty cooldowns before anything else could run.
        let (core, embedder) = crate::core::test_support::test_core_counting_embed_calls().await;
        let big = "alpha ".repeat(400);
        let (src_id, _) = seed(&core, &[&big, &big, &big, "small"]).await;

        run_corpus_with_limit(&core, &src_id, 200).await.unwrap();

        assert_eq!(
            embedder.calls(),
            1,
            "the batch job made more than one inference call"
        );
        let armed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs
              WHERE stage = 'embed' AND target_kind = 'artifact' AND state = 'pending'",
        )
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(armed, 3, "each oversize chunk should have got its own unit");
    }
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib oversize_chunks_do_not_turn_one_batch_into_many_calls`
Expected: FAIL — `embedder.calls()` is 4 (three fall-through splits plus the batch), and `armed` is 6 (the sibling units `replace_with_siblings` enqueued) rather than 3.

- [ ] **Step 3: Arm a unit instead of splitting inline**

In `src/jobs/embed.rs`, replace the scan loop in `run_corpus_with_limit`. The current body is:

```rust
    for chunk in pending {
        let text = embed_text(&chunk);
        if core.counter.count(&text) > limit {
            split_oversize(core, &chunk, limit, false).await?;
        } else {
            texts.push(text);
            batch.push(chunk);
        }
    }
```

Replace it with:

```rust
    for chunk in pending {
        let text = embed_text(&chunk);
        if core.counter.count(&text) > limit {
            // Splitting is not free: `split_oversize` falls through to embedding
            // the chunk whole when there is no boundary to cut on, and that is a
            // model call. Doing it here made a job that is allowed one call make
            // one per oversize chunk — fifty of them held the turn for fifty
            // cooldowns, which is the head-of-line blocking one-batch-per-run
            // exists to prevent. Its own unit instead, where `run_with_limit`
            // splits it paced like everything else.
            //
            // Idle-only: `rearm_if_more` brings this job back for every batch of
            // a long document, and `enqueue` would wind the attempts of a unit
            // already queued back to zero on each of them.
            core.store
                .rearm_idle_seq(Stage::Embed, "artifact", &chunk.id, 0)
                .await?;
        } else {
            texts.push(text);
            batch.push(chunk);
        }
    }
```

Also update the comment directly above the loop. Replace:

```rust
    // An oversize chunk becomes siblings instead of a vector, so it cannot ride
    // along in a batch. It leaves behind its own per-chunk jobs.
```

with:

```rust
    // An oversize chunk becomes siblings instead of a vector, so it cannot ride
    // along in a batch — and finding out how to cut it can itself cost a call.
    // It gets a unit of its own and this job goes on without it.
```

- [ ] **Step 4: Run the new test to verify it passes**

Run: `cargo test --lib oversize_chunks_do_not_turn_one_batch_into_many_calls`
Expected: PASS

- [ ] **Step 5: Run the suite and expect one existing test to fail**

Run: `cargo test --lib`
Expected: FAIL in `an_oversize_chunk_does_not_block_its_siblings` — it calls `run_corpus_with_limit` directly and asserts `chunks.len() > 3`, but the split now happens in a separate unit that this test never runs. This is the intended behaviour change, not a bug; the next step updates the test to assert the new seam.

- [ ] **Step 6: Update `an_oversize_chunk_does_not_block_its_siblings`**

Replace the body of that test (`src/jobs/embed.rs`, currently lines 1189-1209) with:

```rust
    #[tokio::test]
    async fn an_oversize_chunk_does_not_block_its_siblings() {
        // It becomes siblings rather than a vector, so it cannot ride along in
        // the batch. The rest of the source must still be embedded, and the
        // oversize one must be handed to a unit of its own rather than split
        // here — splitting can cost a call this job has already spent.
        let core = test_core().await;
        let big = format!("{}\n\n{}", "alpha ".repeat(400), "beta ".repeat(400));
        let (src_id, ids) = seed(&core, &["small one", &big, "small two"]).await;

        run_corpus_with_limit(&core, &src_id, 200).await.unwrap();

        assert_eq!(
            core.vectors.count().await.unwrap(),
            2,
            "the two small chunks should be embedded"
        );
        let armed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs
              WHERE stage = 'embed' AND target_kind = 'artifact' AND target_id = ?
                AND state = 'pending'",
        )
        .bind(&ids[1])
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(armed, 1, "the oversize chunk was not given its own unit");

        // And that unit does the split, so nothing is lost by deferring it.
        run_with_limit(&core, &ids[1], 200).await.unwrap();
        let chunks = core.store.artifacts_for_corpus(&src_id).await.unwrap();
        assert!(
            chunks.len() > 3,
            "the oversize chunk should have become siblings, got {}",
            chunks.len()
        );
    }
```

- [ ] **Step 7: Run the full suite**

Run: `cargo test --lib`
Expected: PASS, 656 tests. Watch in particular that `a_long_document_re_arms_itself_until_it_is_drained` and `isolating_a_batch_does_not_put_the_batch_straight_back` still pass — both exercise the `rearm_if_more` termination this change relies on.

- [ ] **Step 8: Commit**

```bash
git add src/jobs/embed.rs
git commit -m "fix: an oversize chunk costs its own unit, not this job's turn"
```

---

### Task 3: A pair whose artifact is gone is closed, not left pending

**Files:**
- Modify: `src/jobs/judge.rs:23-30`
- Modify: `src/jobs/consolidate.rs:473-478` (the matching `continue` in `arm_judgements`)
- Test: `src/jobs/judge.rs` tests module

**Interfaces:**
- Consumes: `Store::get_artifact` returns `Err(Error::NotFound)` for a missing row (`src/store/artifacts.rs:272`), `Store::set_pair_state(id, PairState, Option<&str>)`.
- Produces: nothing new.

**Executed note — this task shrank.** `artifact_pairs.a_id` and `b_id` are
`REFERENCES artifacts(id) ON DELETE CASCADE` (`src/store/schema.sql:160-161`) and
every pool sets `foreign_keys(true)` (`src/store/mod.rs:36,156,336`), so deleting
an artifact takes its pairs with it. A pair naming an artifact that is gone is a
state the schema forbids, which makes the permanent-pending leak this task was
written for unreachable — the test in Step 1 fails at `get_pair`, not at the
assertion. What was real is the error swallowing, so the shipped change is
narrower than the steps below: both call sites use `?` on `get_artifact` and the
deletion branch is deleted along with the comment claiming it happens. No
`NotFound` arm, and no test — injecting a store failure needs scaffolding that
does not exist. Steps 1-5 below are superseded; Step 5's `arm_judgements` edit
was applied in the same narrowed form.

**Background (as originally written):** The `let (Ok(a), Ok(b)) = … else` swallows *any* error from `get_artifact`, not just `NotFound`. It records an attempt and returns `Ok`, so `run_one` closes the unit while the pair stays `Pending`. `pairs_to_judge` (`src/store/pairs.rs:233`) has no attempts ceiling, and `arm_judgements` hits the same `continue` and never dismisses it either — so the pair sits in the review queue permanently. A transient store error is also miscategorised as a deletion.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/jobs/judge.rs`. If that file has no `mod tests` block yet, create one at the end of the file with `use super::*;` and `use crate::core::test_support::test_core;` at its top.

```rust
    #[tokio::test]
    async fn a_pair_whose_artifact_is_gone_is_closed_rather_than_left_pending() {
        // `pairs_to_judge` has no attempts ceiling, so a pair left pending here
        // is pending for good: this unit closes without settling it, and the
        // next sweep's `arm_judgements` skips it for exactly the same reason.
        // It sits in the operator's review queue naming an artifact that does
        // not exist, forever.
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        ordinal: 0,
                        text: "the mount point is /srv".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: Some(0),
                        caveats: vec![],
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "the mount point is /mnt".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: Some(0),
                        caveats: vec![],
                    },
                ],
            )
            .await
            .unwrap();
        core.store
            .record_pair(&made[0].id, &made[1].id, 0.9)
            .await
            .unwrap();
        let pair = core.store.pairs_to_judge(1).await.unwrap().remove(0);

        // The half the unit will not find when it is finally claimed.
        core.store.delete_artifact(&made[1].id).await.unwrap();

        run(&core, &pair.id.to_string()).await.unwrap();

        assert_eq!(
            core.store.get_pair(pair.id).await.unwrap().state,
            PairState::Dismissed,
            "the pair was left pending with nothing left that could settle it"
        );
    }
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib a_pair_whose_artifact_is_gone_is_closed_rather_than_left_pending`
Expected: FAIL — state is `Pending`, not `Dismissed`.

- [ ] **Step 3: Tell a deletion apart from a sick store in `judge::run`**

In `src/jobs/judge.rs`, replace:

```rust
    let (Ok(a), Ok(b)) = (
        core.store.get_artifact(&p.a_id).await,
        core.store.get_artifact(&p.b_id).await,
    ) else {
        // One side was deleted while the unit waited. Nothing to compare.
        core.store.record_judge_attempt(id).await?;
        return Ok(());
    };
```

with:

```rust
    let (a, b) = match (
        core.store.get_artifact(&p.a_id).await,
        core.store.get_artifact(&p.b_id).await,
    ) {
        (Ok(a), Ok(b)) => (a, b),
        // One side was deleted while the unit waited. Dismissed rather than
        // left pending: `pairs_to_judge` has no attempts ceiling, so a pending
        // pair nothing can compare stays at the head of every sweep and in the
        // operator's review queue for good — `arm_judgements` skips it for the
        // same reason this unit does, so neither of them would ever close it.
        (Err(Error::NotFound), _) | (_, Err(Error::NotFound)) => {
            core.store
                .set_pair_state(id, PairState::Dismissed, None)
                .await?;
            return Ok(());
        }
        // Any other error is the store being unwell, not an artifact being
        // gone. Reporting it keeps the unit retryable instead of settling a
        // pair on the strength of a failed read.
        (Err(e), _) | (_, Err(e)) => return Err(e),
    };
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib a_pair_whose_artifact_is_gone_is_closed_rather_than_left_pending`
Expected: PASS

- [ ] **Step 5: Make the same distinction in `arm_judgements`**

In `src/jobs/consolidate.rs`, in `arm_judgements`, replace:

```rust
        let (Ok(a), Ok(b)) = (
            core.store.get_artifact(&p.a_id).await,
            core.store.get_artifact(&p.b_id).await,
        ) else {
            continue;
        };
```

with:

```rust
        let (a, b) = match (
            core.store.get_artifact(&p.a_id).await,
            core.store.get_artifact(&p.b_id).await,
        ) {
            (Ok(a), Ok(b)) => (a, b),
            // Gone, so there is nothing to ask about and nothing a later sweep
            // could do either. Dismissed rather than skipped: skipping left the
            // pair pending, and a pending pair with no attempts against it
            // leads `pairs_to_judge`'s ordering every sweep from here on.
            (Err(Error::NotFound), _) | (_, Err(Error::NotFound)) => {
                core.store
                    .set_pair_state(p.id, crate::store::pairs::PairState::Dismissed, None)
                    .await?;
                continue;
            }
            (Err(e), _) | (_, Err(e)) => return Err(e),
        };
```

If `src/jobs/consolidate.rs` does not already import `Error`, add it: change the existing `use crate::error::Result;` to `use crate::error::{Error, Result};`.

- [ ] **Step 6: Run the full suite**

Run: `cargo test --lib`
Expected: PASS, 657 tests.

- [ ] **Step 7: Commit**

```bash
git add src/jobs/judge.rs src/jobs/consolidate.rs
git commit -m "fix: a pair with nothing left to compare stops waiting to be judged"
```

---

### Task 4: The sweep heals a document that resolved every window but never finished

**Files:**
- Modify: `src/jobs/reconcile.rs:45-75`
- Modify: `src/jobs/reconcile.rs:192-224` (the `a_corpus_whose_artifacts_never_embedded_gets_an_embed_job` test)
- Test: `src/jobs/reconcile.rs` tests module

**Interfaces:**
- Consumes: `Corpus.coverage: Option<f64>` (`src/store/corpora.rs:83`), `crate::jobs::window::settle(core, corpus_id)` (`src/jobs/window.rs:212`), `Store::artifacts_for_corpus`.
- Produces: nothing new.

**Background:** On master the sweep enqueued `Stage::Synthesize`, whose `run()` ended in `finish()`. Now it only arms `SegmentWindow` units for *unresolved* windows. A process killed between the last `set_segment_state(Done)` and `settle()` leaves a corpus with every window `done` and no `finish`: `unresolved` is empty, so that branch is skipped. The embed branch below still fires — artifacts are pending — and `settle_corpus` eventually gives the corpus a status, so nothing looks wrong. But `renumber_artifacts`, `recompute_coverage` and the `Title` unit never ran for that document, and nothing will ever run them.

`finish()` calls `recompute_coverage`, which always writes a value, on every path that produced artifacts — so a NULL `coverage` on a corpus that has artifacts and no unresolved windows is exactly the stuck state. The zero-chunk path in `finish()` sets `Failed` and writes no coverage, which is why the artifact check is part of the condition.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/jobs/reconcile.rs`, after `a_corpus_whose_artifacts_never_embedded_gets_an_embed_job`:

```rust
    #[tokio::test]
    async fn a_corpus_that_resolved_every_window_but_never_finished_is_healed() {
        // Killed between the last window's `Done` and the `settle` that
        // follows it. Every window has resolved, so the branch above arms
        // nothing, and the embed branch below gives the corpus a status anyway
        // — which is what makes this invisible. The document was never
        // renumbered, never measured and never named, and nothing else in the
        // system would ever do it.
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store
            .upsert_segments(&src.id, &[seg(1, 10, "the window")])
            .await
            .unwrap();
        core.store
            .set_segment_state(&src.id, 0, SegmentState::Done, None)
            .await
            .unwrap();
        core.store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "the window".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        assert!(
            core.store.get_corpus(&src.id).await.unwrap().coverage.is_none(),
            "the fixture must start unmeasured"
        );

        run(&core).await.unwrap();

        assert!(
            core.store.get_corpus(&src.id).await.unwrap().coverage.is_some(),
            "the document was never measured, and nothing was left that would"
        );
        assert!(
            core.store
                .has_job(Stage::Title, &src.id)
                .await
                .unwrap(),
            "the document was never handed to the namer"
        );
    }
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib a_corpus_that_resolved_every_window_but_never_finished_is_healed`
Expected: FAIL — `the document was never measured, and nothing was left that would`.

- [ ] **Step 3: Add the branch**

In `src/jobs/reconcile.rs`, insert between the `if !unresolved.is_empty() { … continue; }` block and the `if !core.store.pending_artifacts_for_corpus(...)` block:

```rust
            // Every window resolved and the document was still never finished.
            // `finish` measures coverage on every path that produced artifacts,
            // so a corpus with artifacts and none of it is one whose process
            // died between the last window's `Done` and the `settle` that
            // follows it. Nothing else would notice: the embed branch below
            // still fires and `settle_corpus` gives it a status, while the
            // renumbering, the coverage measure and the title unit never ran.
            //
            // `settle` rather than a job, because there is no inference here —
            // it is the same local work `finish` does, and re-running it on a
            // document that is fine is what the coverage test rules out.
            if c.coverage.is_none() && !core.store.artifacts_for_corpus(&c.id).await?.is_empty() {
                crate::jobs::window::settle(core, &c.id).await?;
                armed += 1;
                continue;
            }
```

- [ ] **Step 4: Run the new test**

Run: `cargo test --lib a_corpus_that_resolved_every_window_but_never_finished_is_healed`
Expected: PASS

- [ ] **Step 5: Run the suite and expect one existing test to fail**

Run: `cargo test --lib`
Expected: FAIL in `a_corpus_whose_artifacts_never_embedded_gets_an_embed_job` — its fixture is a corpus with a Done window, artifacts, and no coverage, so it now takes the new branch and `claim_job` may return the `Title` unit rather than the `Embed` one. The fixture, not the assertion, is what needs fixing: a corpus that reached the embed stage has been through `finish`, and so has a coverage value.

- [ ] **Step 6: Give that fixture the coverage a finished document has**

In `a_corpus_whose_artifacts_never_embedded_gets_an_embed_job`, immediately after the `insert_artifacts` call and before the `assert_eq!(run(&core).await.unwrap(), 1);`, add:

```rust
        // A corpus that got as far as embedding has been through `finish`, and
        // `finish` measures it. Without this the fixture is indistinguishable
        // from a document whose `finish` never ran, which is a different repair.
        core.store.set_corpus_coverage(&src.id, 0.9).await.unwrap();
```

- [ ] **Step 7: Run the full suite**

Run: `cargo test --lib`
Expected: PASS, 658 tests. `a_finished_corpus_is_left_alone` must still return 0 — that fixture has a Done window and no artifacts, so the new branch's artifact check excludes it.

- [ ] **Step 8: Commit**

```bash
git add src/jobs/reconcile.rs
git commit -m "fix: a document that resolved every window still gets finished"
```

---

### Task 5: Planning a document does not wind back a queued window's attempts

**Files:**
- Modify: `src/jobs/synthesize.rs:66-75` (`arm_seq` → `rearm_idle_seq`)
- Modify: `src/store/jobs.rs` (add `delete_window_jobs`)
- Modify: `src/core/ingest.rs:639` (call it from `reprocess`)
- Modify: `src/store/jobs.rs:175-177` (the `arm_seq` docstring, which still names planning as its caller)
- Test: `src/jobs/synthesize.rs` tests module, `src/core/ingest.rs` tests module

**Interfaces:**
- Consumes: `Store::rearm_idle_seq`, `crate::jobs::window::unit_target(corpus_id, idx) -> String` which formats `"{corpus_id}#{idx}"` (`src/jobs/window.rs:21`).
- Produces: `Store::delete_window_jobs(&self, corpus_id: &str) -> Result<()>`.

**Background:** `plan()` arms window units with `arm_seq` (`Guard::NotRunning`), which resets `attempts` and `run_after` on units that are merely queued. Everything else that arms units automatically was moved to `rearm_idle_seq` for exactly this reason. `plan()` is reachable more than once per corpus — the comment at `synthesize.rs:47-53` describes the case itself: a process killed after planning leaves the plan row pending, startup re-arms it, the units run and fail while the stale plan waits, and re-arming them from zero when it is finally claimed keeps a window the model will not read forever young, so `settle` never counts it as spent.

The one thing `arm_seq` was buying is the operator's reprocess, which today deletes the `Title` job and the window *rows* but not the window *units* — so with `rearm_idle_seq` alone, a rerun would inherit the previous run's attempt counts. `reprocess` deleting the units restores that, and is what it should have been doing anyway.

- [ ] **Step 1: Write the failing planner test**

Add to the `mod tests` block in `src/jobs/synthesize.rs`:

```rust
    #[tokio::test]
    async fn re_planning_does_not_wind_back_a_queued_windows_attempts() {
        // A plan job outlives the units it arms: killed after planning, its row
        // stays pending, startup re-arms it, the units sort ahead of it and run
        // and fail — and only then is the stale plan claimed. Re-arming them
        // from zero there keeps a window the model will not read forever young,
        // so `settle` never counts it as spent and the document never leaves
        // `segmenting`. It is the failure the reconciliation sweep was already
        // fixed for, reached by a second route.
        let core = test_core().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();
        plan(&core, &out.id).await.unwrap();
        sqlx::query("UPDATE jobs SET attempts = 4 WHERE stage = 'segment_window'")
            .execute(&core.store.pool)
            .await
            .unwrap();

        plan(&core, &out.id).await.unwrap();

        let attempts: Vec<i64> =
            sqlx::query_scalar("SELECT attempts FROM jobs WHERE stage = 'segment_window'")
                .fetch_all(&core.store.pool)
                .await
                .unwrap();
        assert!(
            attempts.iter().all(|&a| a == 4),
            "re-planning reset a unit that was already queued: {attempts:?}"
        );
    }
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib re_planning_does_not_wind_back_a_queued_windows_attempts`
Expected: FAIL — attempts are `[0, 0]` (or `[0]`, depending on how the fixture windows out).

- [ ] **Step 3: Switch `plan()` to idle-only arming**

In `src/jobs/synthesize.rs`, in the `for w in pending` loop, replace `arm_seq` with `rearm_idle_seq` and extend the comment above the loop. The loop's preceding comment currently reads:

```rust
    // One unit per window that has not resolved. `seq` is the window index, so
    // this document's window 0 is claimed before any document's window 1 and a
    // capture made during a long ingest does not wait for all of it.
```

Replace it with:

```rust
    // One unit per window that has not resolved. `seq` is the window index, so
    // this document's window 0 is claimed before any document's window 1 and a
    // capture made during a long ingest does not wait for all of it.
    //
    // Idle-only, like every other automatic arming in the system. A plan job
    // outlives the units it arms — the case the comment above describes — so
    // this runs again while those units are queued with attempts against them,
    // and winding those back keeps a window the model will not read forever
    // young. An operator's reprocess still gets a clean slate: it deletes the
    // units outright, which is a decision a person made rather than a sweep.
```

and change the call itself to:

```rust
        core.store
            .rearm_idle_seq(
                Stage::SegmentWindow,
                "segment",
                &crate::jobs::window::unit_target(corpus_id, w.idx),
                w.idx,
            )
            .await?;
```

- [ ] **Step 4: Run the planner test to verify it passes**

Run: `cargo test --lib re_planning_does_not_wind_back_a_queued_windows_attempts`
Expected: PASS

- [ ] **Step 5: Write the failing reprocess test**

Add to the `mod tests` block in `src/core/ingest.rs`, next to `reprocessing_gives_a_corpus_that_was_never_named_another_chance`:

```rust
    #[tokio::test]
    async fn reprocessing_gives_every_window_its_attempts_back() {
        // Planning arms idle-only now, so the window units are the one piece of
        // the previous run that a reprocess would otherwise inherit: a rerun
        // asked for by a person would start its windows four attempts in and
        // give up on them almost at once. `clear_segments` drops the rows the
        // units name but not the units, which outlive them.
        let core = test_core().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        sqlx::query("UPDATE jobs SET attempts = 4 WHERE stage = 'segment_window'")
            .execute(&core.store.pool)
            .await
            .unwrap();

        core.reprocess(&out.id, Stage::Synthesize).await.unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();

        let attempts: Vec<i64> =
            sqlx::query_scalar("SELECT attempts FROM jobs WHERE stage = 'segment_window'")
                .fetch_all(&core.store.pool)
                .await
                .unwrap();
        assert!(
            !attempts.is_empty(),
            "the rerun should have armed its windows again"
        );
        assert!(
            attempts.iter().all(|&a| a == 0),
            "the rerun inherited the previous run's attempts: {attempts:?}"
        );
    }
```

- [ ] **Step 6: Run it to make sure it fails**

Run: `cargo test --lib reprocessing_gives_every_window_its_attempts_back`
Expected: FAIL — attempts are `[4, 4]`.

- [ ] **Step 7: Add `delete_window_jobs` to the store**

In `src/store/jobs.rs`, add after `delete_job`:

```rust
    /// Forget every window unit of one document.
    ///
    /// The units are keyed `corpus#idx` and outlive the window rows they name,
    /// so `clear_segments` on its own leaves a rerun sharing the attempt counts
    /// of the run it replaces — and since planning arms idle-only, it would keep
    /// them. Only an operator asking for the work again wants this, for the same
    /// reason `delete_job` exists: a rerun a person asked for is a clean slate
    /// or it is not one.
    ///
    /// Matched on the `corpus#` prefix rather than with `LIKE`, so an id
    /// carrying a wildcard character cannot widen the delete.
    pub async fn delete_window_jobs(&self, corpus_id: &str) -> Result<()> {
        sqlx::query(
            "DELETE FROM jobs
              WHERE stage = 'segment_window'
                AND substr(target_id, 1, length(?) + 1) = ? || '#'",
        )
        .bind(corpus_id)
        .bind(corpus_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
```

- [ ] **Step 8: Call it from `reprocess`**

In `src/core/ingest.rs`, in the `Stage::Synthesize | Stage::Enrich` arm, immediately after the `self.store.clear_segments(&src.id).await?;` line and its comment, add:

```rust
                // And the units that name those windows, which outlive them.
                // Planning arms idle-only, so a unit still queued from the run
                // being replaced would carry its attempts into the rerun — the
                // person who asked for another try would get a window that gives
                // up after one.
                self.store.delete_window_jobs(&src.id).await?;
```

- [ ] **Step 9: Run the reprocess test to verify it passes**

Run: `cargo test --lib reprocessing_gives_every_window_its_attempts_back`
Expected: PASS

**Executed note:** the `rg` in Step 10 found `arm_seq`'s only remaining caller
was its own test, so the delete branch was taken. That also removed
`Guard::NotRunning` and its statement arm, since nothing else constructs the
variant and it would have warned as dead. The test that covered it,
`arming_a_unit_a_worker_is_already_inside_leaves_it_alone`, was pointed at
`rearm_idle_seq` instead — `Guard::Closed` is strictly narrower than
`NotRunning`, so it still holds the property the test is about. The two
remaining tiers are `enqueue_seq` (an operator's reprocess) and `rearm_idle_seq`
(everything automatic).

- [ ] **Step 10: Correct the `arm_seq` docstring**

`arm_seq` no longer has any caller. Confirm that first:

Run: `rg 'arm_seq\(' src/ | rg -v 'rearm_idle_seq|enqueue_seq|fn arm_seq'`
Expected: no output.

If there are no callers, delete the `arm_seq` method from `src/store/jobs.rs` along with its docstring, since a method with no callers is a claim about the system that nothing checks. If `rg` shows callers, instead replace the misleading final paragraph of its docstring:

```rust
    /// Everything that arms units on its own — planning a document, a
    /// consolidation sweep arming a judgement — wants this rather than
    /// `enqueue_seq`.
```

with:

```rust
    /// Weaker than what automatic arming actually wants: a queued unit is
    /// already going to run, and re-arming it winds its attempts back. Planning
    /// a document and the consolidation sweep both use `rearm_idle_seq` below
    /// for that reason.
```

- [ ] **Step 11: Run the full suite**

Run: `cargo test --lib`
Expected: PASS, 660 tests. `a_sweep_does_not_wind_back_a_failing_units_attempts` in `reconcile.rs` and `reprocessing_gives_a_corpus_that_was_never_named_another_chance` in `ingest.rs` both exercise neighbouring behaviour and must still pass.

- [ ] **Step 12: Commit**

```bash
git add src/jobs/synthesize.rs src/store/jobs.rs src/core/ingest.rs
git commit -m "fix: planning a document leaves its queued windows alone"
```

---

### Task 6: Stop reporting a contradiction count the sweep cannot know

**Files:**
- Modify: `src/jobs/consolidate.rs:32` (remove the field), `src/jobs/consolidate.rs:436` (remove the log field)
- Test: none — this is the removal of a value that is never written.

**Interfaces:**
- Consumes: nothing.
- Produces: `Outcome` loses its `contradictions` field. Verified before writing this plan: nothing outside `consolidate.rs` reads it, and the `contradictions` key in `src/web/api.rs:235` comes from `pairs_by_state`, not from `Outcome`.

**Background:** `arm_judgements` makes no model call, so `out.contradictions` is always 0, yet the sweep's summary line still logs it. An operator reading the journal concludes the judge found nothing, when in fact no judging happened in that sweep at all. The comment at `consolidate.rs:423-426` already says the number "is no longer knowable here"; the field and the log line are what is left of it.

- [ ] **Step 1: Remove the field**

In `src/jobs/consolidate.rs`, delete this line from `struct Outcome`:

```rust
    pub contradictions: usize,
```

- [ ] **Step 2: Remove it from the summary line**

In the same file, delete this line from the `tracing::info!` call at the end of `run`:

```rust
            contradictions = out.contradictions,
```

- [ ] **Step 3: Extend the comment that explains the absence**

The comment above `out.judged = arm_judgements(core).await?;` already explains why. Replace its last clause so it describes the current state rather than a field that no longer exists. The comment currently reads:

```rust
        // Armed, not asked. `judged` counts the calls this sweep is responsible
        // for rather than the calls it made, since it now makes none;
        // `contradictions` is no longer knowable here at all, because the answer
        // arrives one unit at a time long after the sweep has returned.
```

Replace with:

```rust
        // Armed, not asked. `judged` counts the calls this sweep is responsible
        // for rather than the calls it made, since it now makes none. There is
        // deliberately no contradiction count beside it: the answers arrive one
        // unit at a time long after the sweep has returned, and a zero logged
        // here read as "the judge found nothing" rather than "the judge has not
        // been asked yet". `pairs_by_state(Contradiction, ..)` is where that
        // number actually lives, and the API and Ops both read it from there.
```

- [ ] **Step 4: Run the full suite**

Run: `cargo test --lib`
Expected: PASS, 660 tests. If any test constructs an `Outcome` literally with all fields named, the compiler will point at it; remove the `contradictions:` line there too. Tests that assert on `out.judged` and `out.queued` are unaffected.

- [ ] **Step 5: Check nothing else referenced it**

Run: `rg 'contradictions' src/`
Expected: hits only in `src/infer/prompt.rs` (prompt text), `src/web/api.rs` (the `pairs_by_state` key), `src/web/ui.rs` (a comment), and the new comment in `consolidate.rs`.

- [ ] **Step 6: Commit**

```bash
git add src/jobs/consolidate.rs
git commit -m "refactor: drop a count the sweep stopped being able to make"
```

---

## Final Verification

- [ ] Run `cargo test --lib` — expect 660 passing.
- [ ] Run `cargo check --all-targets` — expect clean.
- [ ] Run `cargo clippy --all-targets -- -D warnings` — expect clean.
- [ ] Run `git log --oneline origin/master..HEAD` and confirm the six fix commits are present on top of the PR's existing history.

## Self-Review Notes

Checked against the six review findings:

| Finding | Task |
|---|---|
| #1 oversize refusals trip the breaker | Task 1 |
| #2 oversize scan unbounded per run | Task 2 |
| #3 resolved-but-never-finished corpus | Task 4 |
| #4 `plan()` uses `arm_seq` | Task 5 |
| #5 store error treated as deletion | Task 3 |
| #6 dead `contradictions` counter | Task 6 |

Two behaviour changes deliberately break an existing test, and each is handled in the task that causes it: `an_oversize_chunk_does_not_block_its_siblings` (Task 2, Step 6) and `a_corpus_whose_artifacts_never_embedded_gets_an_embed_job` (Task 4, Step 6). Both are fixture corrections, not weakened assertions.

Test counts assume the suite is at 653 before Task 1 and that each task adds the tests it lists. If the running total differs, the number is the thing that is wrong, not the tasks.
